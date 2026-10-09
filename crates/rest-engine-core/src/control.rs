use std::time::{Duration, Instant};

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

    pub(crate) fn with_optional_deadline(
        mut self,
        deadline: Option<&str>,
    ) -> Result<Self, EngineError> {
        self.deadline = deadline.map(parse_deadline).transpose()?;
        Ok(self)
    }

    /// Whether the deadline, if any, has already passed.
    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| deadline <= Instant::now())
    }
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
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| EngineError::InvalidInput(ErrorDetail::from("deadline is out of range")))
}
