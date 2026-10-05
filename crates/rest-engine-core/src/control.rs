use std::time::{Duration, Instant};

use crate::error::ErrorDetail;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::EngineError;

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: tokio_util::sync::CancellationToken,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    pub(crate) async fn cancelled(&self) {
        self.inner.cancelled().await;
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionControl {
    pub cancellation: CancellationToken,
    pub(crate) deadline: Option<Instant>,
}

impl ExecutionControl {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline: None,
        }
    }

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
