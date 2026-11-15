//! Circuit-breaker admission, probe leases and outcome recording.

use super::{
    CircuitBreakerPolicy, Duration, EngineError, ErrorDetail, Hash, Instant, MAX_PROBE_LEASE,
    PreparedRequest, ResponseData, Transport, is_retryable_transport_error,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct CircuitKey {
    pub(super) origin: String,
    pub(super) group: String,
}

/// Outcome of admitting a request past the circuit breaker. `probe` is set only
/// when this request is the half-open probe, and carries the generation that
/// entitles it to record the result. `epoch` is the state generation the
/// admission decision was based on.
pub(super) struct CircuitAdmission {
    pub(super) key: CircuitKey,
    pub(super) probe: Option<u64>,
    pub(super) epoch: u64,
}

#[derive(Default)]
pub(super) struct CircuitState {
    pub(super) consecutive_failures: u32,
    pub(super) opened_at: Option<Instant>,
    /// Current half-open probe, if any: its generation and its start.
    ///
    /// The start is a lease, so a cancelled or abandoned probe cannot keep the
    /// circuit open forever. The generation makes the lease safe: once it
    /// expires and a new probe is admitted, the abandoned one may still finish,
    /// and only the probe owning the current generation may record an outcome.
    pub(super) half_open_probe: Option<(u64, Instant)>,
    /// Bumped whenever the circuit opens or a probe starts.
    ///
    /// A request admitted while the circuit was closed carries no probe
    /// generation, yet it can still finish after the circuit opened and a probe
    /// began. Recording its outcome would reset `opened_at` and let ordinary
    /// traffic through while the probe is still in flight, so an admission from
    /// a superseded epoch is ignored.
    pub(super) epoch: u64,
    pub(super) sequence: u64,
}

impl Transport {
    pub(super) async fn execute_with_circuit(
        &self,
        request: &PreparedRequest,
    ) -> Result<ResponseData, EngineError> {
        let admission = self.admit_circuit(request).await?;
        let result = self.execute_authenticated(request).await;
        if let Some(admission) = admission {
            let failed = match &result {
                Ok(response) => request
                    .circuit_breaker
                    .failure_statuses
                    .contains(&response.status),
                Err(error) => is_retryable_transport_error(error),
            };
            self.record_circuit(&admission, &request.circuit_breaker, failed)
                .await;
        }
        result
    }

    pub(super) async fn admit_circuit(
        &self,
        request: &PreparedRequest,
    ) -> Result<Option<CircuitAdmission>, EngineError> {
        let policy = &request.circuit_breaker;
        if !policy.enabled {
            return Ok(None);
        }
        #[cfg(test)]
        crate::injection::visit(crate::injection::Point::Circuit);
        if policy.failure_threshold == 0 {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "circuit breaker failure_threshold must be greater than zero",
            )));
        }
        if policy.group.trim().is_empty() {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "circuit breaker group cannot be empty",
            )));
        }
        if self.config.max_circuit_origins == 0 {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "circuit breaker state is disabled by this engine",
            )));
        }
        let key = CircuitKey {
            origin: request.url.origin().ascii_serialization(),
            group: policy.group.clone(),
        };
        let sequence = self.next_sequence();
        let recovery = Duration::from_millis(policy.recovery_timeout_ms);
        // A probe whose future was cancelled never reaches `record_circuit`, so
        // the in-flight marker is a lease that expires instead of a sticky flag.
        //
        // The lease is an upper-bound *estimate*, not a guarantee: a probe runs a
        // whole retry loop, and its real duration also includes `Retry-After`
        // waits, redirects, rate limiting, and name resolution. Correctness does
        // not rest on it — the epoch and the probe generation already stop a
        // superseded probe from recording anything. The lease exists only so an
        // abandoned probe cannot wedge the circuit open forever, so it is
        // estimated generously and then clamped: too short would admit a second
        // probe next to a healthy one, too long would bring back the wedge it is
        // meant to prevent.
        let attempts = request.retry.max_attempts.max(1);
        let per_attempt = request.timeout.saturating_add(Duration::from_millis(
            request
                .retry
                .max_backoff_ms
                .max(request.retry.backoff_base_ms)
                .max(if request.retry.respect_retry_after {
                    request.retry.max_retry_after_ms
                } else {
                    0
                }),
        ));
        let probe_lease = recovery
            .max(per_attempt.saturating_mul(attempts))
            .min(MAX_PROBE_LEASE);
        let mut circuits = self.circuits.lock().await;
        if !circuits.contains_key(&key) && circuits.len() >= self.config.max_circuit_origins {
            let oldest = circuits
                .iter()
                .min_by_key(|(_, state)| state.sequence)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                circuits.remove(&oldest);
            }
        }
        let state = circuits.entry(key.clone()).or_default();
        if state.sequence == 0 {
            state.sequence = sequence;
        }
        let mut probe = None;
        if let Some(opened_at) = state.opened_at {
            let probing = state
                .half_open_probe
                .is_some_and(|(_, started)| started.elapsed() < probe_lease);
            if opened_at.elapsed() < recovery || probing {
                #[cfg(test)]
                crate::injection::visit(crate::injection::Point::CircuitReject);
                return Err(EngineError::CircuitOpen);
            }
            #[cfg(test)]
            crate::injection::visit(crate::injection::Point::CircuitProbe);
            let generation = self.next_sequence();
            state.half_open_probe = Some((generation, Instant::now()));
            state.epoch = state.epoch.saturating_add(1);
            probe = Some(generation);
        }
        Ok(Some(CircuitAdmission {
            key,
            probe,
            epoch: state.epoch,
        }))
    }

    pub(super) async fn record_circuit(
        &self,
        admission: &CircuitAdmission,
        policy: &CircuitBreakerPolicy,
        failed: bool,
    ) {
        let sequence = self.next_sequence();
        let mut circuits = self.circuits.lock().await;
        let state = circuits.entry(admission.key.clone()).or_default();
        if state.sequence == 0 {
            state.sequence = sequence;
        }
        // An admission from a superseded epoch describes a decision that no
        // longer holds: the circuit has opened, or a probe has started, since it
        // was let through. Recording it would close or reopen the circuit on
        // behalf of a request nobody is waiting for.
        if admission.epoch != state.epoch {
            return;
        }
        // A probe whose lease expired has likewise been superseded.
        let current_probe = state.half_open_probe.map(|(generation, _)| generation);
        if let Some(generation) = admission.probe
            && current_probe != Some(generation)
        {
            return;
        }
        if failed {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            if admission.probe.is_some() || state.consecutive_failures >= policy.failure_threshold {
                if state.opened_at.is_none() {
                    state.epoch = state.epoch.saturating_add(1);
                }
                state.opened_at = Some(Instant::now());
            }
        } else {
            state.consecutive_failures = 0;
            state.opened_at = None;
        }
        if admission.probe.is_some() {
            state.half_open_probe = None;
        }
    }
}
