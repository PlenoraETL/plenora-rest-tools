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
/// The default has a fresh token and no deadline. The deadline set here is
/// the only one that applies: `execute_with_control` does not read
/// `options.deadline` from the request.
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

    /// Sets the deadline from an RFC 3339 timestamp with offset, such as
    /// `2026-01-31T12:00:00Z`.
    ///
    /// When the deadline is reached the execution fails with `TIMEOUT`; a
    /// deadline already in the past fails it before any network activity. A
    /// value that is not RFC 3339, or too far in the future to be represented,
    /// is rejected with `INVALID_INPUT`.
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
}

fn parse_deadline(value: &str) -> Result<Instant, EngineError> {
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
