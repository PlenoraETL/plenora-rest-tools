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

    /// Whether the deadline, if any, has already passed.
    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| deadline <= Instant::now())
    }
}

/// Whether `value` is an RFC 3339 timestamp in UTC.
///
/// The runtime binding names the deadline an absolute RFC 3339 timestamp in
/// UTC (Runtime Binding 1.0 RT-021, proposed in plenora-contracts #21). Every
/// RFC 3339 spelling of UTC is accepted: `Z` or `z`, `+00:00`, a lowercase
/// `t`, a fraction of a second. A non-zero offset is local time, and `-00:00`
/// means "offset unknown" in RFC 3339, so neither names a UTC instant and both
/// are refused, as is anything that is not RFC 3339.
pub(crate) fn is_utc_deadline(value: &str) -> bool {
    if value.ends_with("-00:00") {
        return false;
    }
    OffsetDateTime::parse(value, &Rfc3339).is_ok_and(|parsed| parsed.offset().is_utc())
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
