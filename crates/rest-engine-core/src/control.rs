use std::time::{Duration, Instant};

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

fn parse_deadline(value: &str) -> Result<Instant, EngineError> {
    let deadline = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| EngineError::InvalidInput("deadline must be RFC 3339".to_owned()))?;
    let now = OffsetDateTime::now_utc();
    let remaining = deadline - now;
    if remaining.is_negative() || remaining.is_zero() {
        return Ok(Instant::now());
    }
    let duration = Duration::try_from(remaining)
        .map_err(|_| EngineError::InvalidInput("deadline is out of range".to_owned()))?;
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| EngineError::InvalidInput("deadline is out of range".to_owned()))
}
