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

/// Whether `value` is spelled `YYYY-MM-DDTHH:MM:SS[.f{1,9}]Z`.
///
/// The runtime binding names the deadline an absolute RFC 3339 timestamp in
/// UTC. RFC 3339 also admits offsets, `-00:00` ("UTC unknown"), lowercase
/// `t`/`z` and leap seconds; one spelling is accepted, so a deadline in local
/// time cannot be read as a different instant.
pub(crate) fn is_canonical_deadline(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let fixed = bytes.len() >= 20
        && digits(0..4)
        && bytes.get(4) == Some(&b'-')
        && digits(5..7)
        && bytes.get(7) == Some(&b'-')
        && digits(8..10)
        && bytes.get(10) == Some(&b'T')
        && digits(11..13)
        && bytes.get(13) == Some(&b':')
        && digits(14..16)
        && bytes.get(16) == Some(&b':')
        && digits(17..19)
        && bytes.last() == Some(&b'Z');
    if !fixed {
        return false;
    }
    // Leap seconds are not accepted: `:60` would need a table to place.
    if bytes.get(17..19) == Some(b"60".as_slice()) {
        return false;
    }
    match bytes.get(19..bytes.len() - 1) {
        Some([]) => true,
        Some([b'.', fraction @ ..]) => {
            (1..=9).contains(&fraction.len()) && fraction.iter().all(u8::is_ascii_digit)
        }
        _ => false,
    }
}

fn parse_deadline(value: &str) -> Result<Instant, EngineError> {
    if !is_canonical_deadline(value) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "deadline must be RFC 3339 in UTC with a Z suffix",
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
