//! Retry policy shared by buffered and streaming attempts.
//!
//! The execution's absolute deadline and cancellation surround the entire loop
//! in ExecutionControl::run, including the chosen wait. Do not clamp that wait
//! to the deadline or return the last HTTP response early: expiration must keep
//! producing the execution's timeout, with its send tally and failure context.
use crate::{EngineError, HttpMethod, RetryPolicy, error::ErrorDetail};
use std::time::Duration;

pub(super) enum AttemptOutcome<'a> {
    Response {
        status: u16,
        retry_after_ms: Option<u64>,
    },
    Failed(&'a EngineError),
}

pub(super) enum RetryDecision {
    Stop,
    After(Duration),
}

pub(super) struct RetrySession<'a> {
    policy: &'a RetryPolicy,
    permitted: bool,
    backoff: crate::exact::Backoff,
}

impl<'a> RetrySession<'a> {
    pub(super) fn new(policy: &'a RetryPolicy, method: &HttpMethod) -> Result<Self, EngineError> {
        Ok(Self {
            policy,
            permitted: method.is_idempotent() || policy.retry_non_idempotent,
            backoff: backoff_for(policy)?,
        })
    }

    pub(super) fn attempts(&self) -> std::ops::RangeInclusive<u32> {
        1..=self.policy.max_attempts.max(1)
    }

    /// Only an admitted retry advances the backoff. A status refused here is
    /// still a response for the caller to decode; a failure remains that error.
    pub(super) fn decide(&mut self, attempt: u32, outcome: AttemptOutcome<'_>) -> RetryDecision {
        if !self.permitted || attempt >= self.policy.max_attempts.max(1) {
            return RetryDecision::Stop;
        }
        let retry_after = match outcome {
            AttemptOutcome::Response {
                status,
                retry_after_ms,
            } if self.policy.retry_on_status.contains(&status) => retry_after_ms,
            AttemptOutcome::Failed(error) if is_retryable_transport_error(error) => None,
            _ => return RetryDecision::Stop,
        };
        match retry_wait(self.policy, &mut self.backoff, retry_after) {
            Some(delay) => RetryDecision::After(delay),
            None => RetryDecision::Stop,
        }
    }
}

pub(super) fn is_retryable_transport_error(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Timeout | EngineError::Transport(_) | EngineError::DnsResolution(_)
    )
}

/// Wait before retrying a response the server asked to retry later, or
/// `None` when the retry must not happen.
///
/// A `Retry-After` longer than `max_retry_after_ms` is not shortened to fit:
/// retrying before the time the server named would ignore its instruction and
/// add load where it asked for less. The response is returned as it is, so
/// the operation fails with its HTTP status instead of retrying early.
pub(super) fn retry_wait(
    policy: &RetryPolicy,
    backoff: &mut crate::exact::Backoff,
    retry_after_ms: Option<u64>,
) -> Option<Duration> {
    match retry_after_ms {
        Some(delay) if policy.respect_retry_after && delay > policy.max_retry_after_ms => None,
        _ => Some(retry_delay(policy, backoff, retry_after_ms)),
    }
}

/// The backoff of a retry loop. The factor is validated before any request,
/// so a refusal here is an internal invariant failure.
pub(super) fn backoff_for(policy: &RetryPolicy) -> Result<crate::exact::Backoff, EngineError> {
    crate::exact::Backoff::new(
        policy.backoff_base_ms,
        policy.backoff_factor,
        policy.max_backoff_ms,
    )
    .ok_or_else(|| EngineError::Runtime(ErrorDetail::from("retry backoff factor is invalid")))
}

/// Wait before the next retry: the server's `Retry-After` when honoured,
/// otherwise the next exact backoff delay. The backoff advances on every
/// retry either way, so the n-th retry always uses the n-th delay.
pub(super) fn retry_delay(
    policy: &RetryPolicy,
    backoff: &mut crate::exact::Backoff,
    retry_after_ms: Option<u64>,
) -> Duration {
    let backoff_ms = backoff.next_ms();
    if policy.respect_retry_after
        && let Some(delay) = retry_after_ms
    {
        return Duration::from_millis(delay.min(policy.max_retry_after_ms));
    }
    Duration::from_millis(backoff_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_preserve_method_budget_status_and_failure_boundaries() {
        for (method, opt_in, allowed) in [
            (HttpMethod::Get, false, true),
            (HttpMethod::Post, false, false),
            (HttpMethod::Post, true, true),
        ] {
            let policy = RetryPolicy {
                max_attempts: 2,
                retry_non_idempotent: opt_in,
                retry_on_status: vec![503],
                backoff_base_ms: 7,
                ..RetryPolicy::default()
            };
            for attempt in [1, 2] {
                for (outcome, eligible) in [
                    (
                        AttemptOutcome::Response {
                            status: 503,
                            retry_after_ms: None,
                        },
                        true,
                    ),
                    (
                        AttemptOutcome::Response {
                            status: 200,
                            retry_after_ms: None,
                        },
                        false,
                    ),
                    (AttemptOutcome::Failed(&EngineError::Timeout), true),
                    (AttemptOutcome::Failed(&EngineError::Cancelled), false),
                ] {
                    let mut session = RetrySession::new(&policy, &method).unwrap();
                    let decision = session.decide(attempt, outcome);
                    assert_eq!(
                        matches!(decision, RetryDecision::After(_)),
                        allowed && attempt == 1 && eligible
                    );
                    if let RetryDecision::After(delay) = decision {
                        assert_eq!(delay, Duration::from_millis(7));
                    }
                }
            }
        }
    }

    #[test]
    fn refused_outcomes_do_not_consume_backoff_and_zero_budget_still_sends_once() {
        let policy = RetryPolicy {
            max_attempts: 4,
            backoff_base_ms: 7,
            backoff_factor: 2.0,
            max_retry_after_ms: 20,
            respect_retry_after: true,
            retry_on_status: vec![503],
            ..RetryPolicy::default()
        };
        let mut session = RetrySession::new(&policy, &HttpMethod::Get).unwrap();
        for (status, header, expected) in [
            (200, None, None),
            (503, Some(21), None),
            (503, Some(3), Some(3)),
            (503, None, Some(14)),
        ] {
            let actual = match session.decide(
                1,
                AttemptOutcome::Response {
                    status,
                    retry_after_ms: header,
                },
            ) {
                RetryDecision::Stop => None,
                RetryDecision::After(delay) => Some(delay.as_millis()),
            };
            assert_eq!(actual, expected);
        }
        let zero = RetryPolicy {
            max_attempts: 0,
            ..policy
        };
        let mut session = RetrySession::new(&zero, &HttpMethod::Get).unwrap();
        assert_eq!(session.attempts().collect::<Vec<_>>(), vec![1]);
        assert!(matches!(
            session.decide(1, AttemptOutcome::Failed(&EngineError::Timeout)),
            RetryDecision::Stop
        ));
    }
}
