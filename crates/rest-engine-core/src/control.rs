use std::{
    future::{Future, pending},
    time::{Duration, Instant},
};

use crate::error::ErrorDetail;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::EngineError;

/// Cooperative cancellation signal for an execution.
///
/// Clones share the same state: cancelling any clone cancels them all, and a
/// cancelled token stays cancelled. An execution observing the token stops at
/// the next await point and fails with `CANCELLED`; asynchronous jobs it was
/// polling are cancelled remotely when their polling configuration asks for
/// it.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: tokio_util::sync::CancellationToken,
}

impl CancellationToken {
    /// Creates a token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the token and every clone of it. Idempotent.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Whether [`cancel`](Self::cancel) has been called on this token or a
    /// clone of it.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    pub(crate) async fn cancelled(&self) {
        self.inner.cancelled().await;
    }
}

/// Cancellation and deadline that bound one execution, passed to
/// [`Engine::execute_with_control`](crate::Engine::execute_with_control).
///
/// The default has a fresh token and no deadline. When the request also
/// carries `options.deadline`, the earlier of the two applies.
#[derive(Clone, Debug, Default)]
pub struct ExecutionControl {
    /// Token that cancels the execution; keep a clone to cancel it from
    /// another task.
    pub cancellation: CancellationToken,
    pub(crate) deadline: Option<Instant>,
}

impl ExecutionControl {
    /// Control with the given token and no deadline.
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline: None,
        }
    }

    /// Sets the deadline from an RFC 3339 timestamp in UTC, such as
    /// `2026-01-31T12:00:00Z` (`z`, `+00:00` and a lowercase `t` are accepted
    /// too).
    ///
    /// When the deadline is reached the execution fails with `TIMEOUT`; a
    /// deadline already in the past fails it with `DEADLINE_EXPIRED` before
    /// anything runs. A non-zero offset, `-00:00`, text that is not RFC 3339
    /// or a value too far in the future to be represented is rejected with
    /// `INVALID_INPUT`.
    pub fn with_deadline(mut self, deadline: &str) -> Result<Self, EngineError> {
        self.deadline = Some(parse_deadline(deadline)?);
        Ok(self)
    }

    /// Admission deliberately checks only the deadline: cancellation is observed
    /// after validation/resource resolution, when the execution is first polled.
    /// Preserve this precedence for every Rust/JSON/runtime/SDK entry point.
    pub(crate) fn check_admission(&self) -> Result<(), EngineError> {
        if self
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            Err(EngineError::DeadlineExpired)
        } else {
            Ok(())
        }
    }

    /// One poll order for all executions: cancellation, elapsed deadline, work.
    /// A missing deadline is a pending future, not a second execution path.
    pub(crate) async fn run<F: Future>(&self, execution: F) -> ControlOutcome<F::Output> {
        let deadline = self.deadline.map(tokio::time::Instant::from_std);
        let elapsed = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => ControlOutcome::Cancelled,
            _ = elapsed => ControlOutcome::Deadline,
            result = execution => ControlOutcome::Finished(result),
        }
    }
}

pub(crate) enum ControlOutcome<T> {
    Finished(T),
    Cancelled,
    Deadline,
}

/// Whether `value` is an RFC 3339 timestamp in UTC.
///
/// The runtime binding names the deadline an absolute RFC 3339 timestamp in
/// UTC (Runtime Binding 1.0 RT-021, plenora-contracts v1.1.0). Every RFC 3339
/// spelling of UTC is accepted, as decision 0010 keeps it in 1.0: `Z` or `z`,
/// `+00:00`, a lowercase
/// `t`, a fraction of a second. A non-zero offset is local time, and `-00:00`
/// means "offset unknown" in RFC 3339, so neither names a UTC instant and both
/// are refused, as is anything that is not RFC 3339.
pub(crate) fn is_utc_deadline(value: &str) -> bool {
    if value.ends_with("-00:00") {
        return false;
    }
    OffsetDateTime::parse(value, &Rfc3339).is_ok_and(|parsed| parsed.offset().is_utc())
}

impl ExecutionControl {
    /// Adds the request's own deadline: the earlier of the two applies.
    ///
    /// `options.deadline` is part of the execution request contract, so it
    /// binds whichever entry point runs the request, not only
    /// `Engine::execute`; a caller-supplied control can shorten it but never
    /// lift it.
    pub(crate) fn with_request_deadline(
        mut self,
        deadline: Option<&str>,
    ) -> Result<Self, EngineError> {
        if let Some(requested) = deadline.map(parse_deadline).transpose()? {
            self.deadline = Some(match self.deadline {
                Some(existing) => existing.min(requested),
                None => requested,
            });
        }
        Ok(self)
    }
}

fn parse_deadline(value: &str) -> Result<Instant, EngineError> {
    if !is_utc_deadline(value) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "deadline must be an RFC 3339 timestamp in UTC",
        )));
    }
    let deadline = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("deadline must be RFC 3339")))?;
    let now = OffsetDateTime::now_utc();
    let remaining = deadline - now;
    if remaining.is_negative() || remaining.is_zero() {
        return Ok(Instant::now());
    }
    let duration = Duration::try_from(remaining)
        .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("deadline is out of range")))?;
    crate::temporal::instant_after(Instant::now(), duration)
        .ok_or_else(|| EngineError::InvalidInput(ErrorDetail::from("deadline is out of range")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn remote_time_deadline_calendar_boundaries() {
        for value in ["1970-01-01T00:00:00Z", "9999-12-31T23:59:59Z"] {
            assert!(parse_deadline(value).is_ok());
        }
        for value in ["10000-01-01T00:00:00Z", "NaN", "-1", "infinity"] {
            assert!(matches!(
                parse_deadline(value),
                Err(EngineError::InvalidInput(_))
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn admission_and_running_control_have_explicit_precedence() {
        let token = CancellationToken::new();
        token.cancel();
        let control = ExecutionControl {
            cancellation: token,
            deadline: Some(Instant::now() - Duration::from_secs(1)),
        };
        assert!(matches!(
            control.check_admission(),
            Err(EngineError::DeadlineExpired)
        ));
        let polled = Cell::new(false);
        let outcome = control
            .run(async {
                polled.set(true);
            })
            .await;
        assert!(matches!(outcome, ControlOutcome::Cancelled));
        assert!(
            !polled.get(),
            "cancellation must win even over ready work and deadline"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_race_handles_absent_and_elapsed_deadlines() {
        let control = ExecutionControl::default();
        assert!(matches!(
            control.run(async { 7 }).await,
            ControlOutcome::Finished(7)
        ));
        let control = ExecutionControl {
            deadline: Some((tokio::time::Instant::now() + Duration::from_secs(10)).into_std()),
            ..ExecutionControl::default()
        };
        assert!(matches!(
            control.run(pending::<()>()).await,
            ControlOutcome::Deadline
        ));
    }

    #[tokio::test]
    async fn cancellation_during_execution_drops_the_inflight_future() {
        struct Dropped<'a>(&'a Cell<bool>);
        impl Drop for Dropped<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Cell::new(false);
        let control = ExecutionControl::default();
        let work = async {
            let _guard = Dropped(&dropped);
            control.cancellation.cancel();
            pending::<()>().await;
        };
        assert!(matches!(control.run(work).await, ControlOutcome::Cancelled));
        assert!(dropped.get());
    }
}
