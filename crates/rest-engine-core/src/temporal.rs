//! Checked time arithmetic shared by the engine and workspace adapters.
//!
//! Active timers have a portable, explicit horizon. Passive elapsed-time
//! comparisons may retain the full u64 millisecond domain.

use std::time::{Duration, Instant, SystemTime};

pub(crate) use crate::exact::{Backoff, rate_interval};

/// Maximum active timer delay, in milliseconds (ten millennia).
pub const MAX_TIMER_MS: u64 = 315_537_897_599_000;

/// Lossless conversion of any u64 millisecond count, including passive ages.
pub const fn millis(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// A portable timer delay; out-of-horizon values are refused, never shortened.
pub fn timer_millis(value: u64) -> Option<Duration> {
    (value <= MAX_TIMER_MS).then(|| millis(value))
}

/// Checked monotonic addition, also enforcing the portable timer horizon.
pub fn instant_after(now: Instant, delay: Duration) -> Option<Instant> {
    if delay > millis(MAX_TIMER_MS) {
        return None;
    }
    now.checked_add(delay)
}

/// Checked wall-clock addition, also enforcing the portable timer horizon.
pub fn system_after(now: SystemTime, delay: Duration) -> Option<SystemTime> {
    if delay > millis(MAX_TIMER_MS) {
        return None;
    }
    now.checked_add(delay)
}

/// Checked sum of active timer budgets.
pub fn timer_sum(left: Duration, right: Duration) -> Option<Duration> {
    left.checked_add(right)
        .filter(|sum| *sum <= millis(MAX_TIMER_MS))
}

/// Checked floating-point seconds for the campaign ticker; zero is refused.
pub fn positive_seconds(value: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(value)
        .ok()
        .filter(|duration| !duration.is_zero() && *duration <= millis(MAX_TIMER_MS))
}

/// Decimal remote seconds as milliseconds. Overflow is an explicit sentinel.
pub(crate) fn remote_seconds_ms(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match value.parse::<u64>() {
        Ok(seconds) => Some(seconds.saturating_mul(1_000)),
        Err(error) if *error.kind() == std::num::IntErrorKind::PosOverflow => Some(u64::MAX),
        Err(_) => None,
    }
}

/// Saturating millisecond ceiling, so an HTTP-date retry never starts early.
pub(crate) fn ceil_millis(delay: Duration) -> u64 {
    let millis = delay
        .as_millis()
        .saturating_add(u128::from(!delay.subsec_nanos().is_multiple_of(1_000_000)));
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// ArcGIS absolute epoch milliseconds as whole remaining seconds.
pub(crate) fn epoch_ttl(now: SystemTime, expires_ms: u64) -> Option<u64> {
    let now_ms =
        u64::try_from(now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_millis()).ok()?;
    Some(expires_ms.saturating_sub(now_ms) / 1_000)
}

/// Reserve a rate slot without allowing the queue to exceed the timer horizon.
pub(crate) fn rate_slot(
    next: Instant,
    now: Instant,
    interval: Duration,
) -> Option<(Instant, Duration)> {
    let wait = next.saturating_duration_since(now);
    timer_sum(wait, interval)?;
    Some((instant_after(next.max(now), interval)?, wait))
}

/// Bounded circuit probe lease, an estimate rather than a remote deadline.
pub(crate) fn probe_lease(
    recovery: Duration,
    timeout: Duration,
    retry: Duration,
    attempts: u32,
    cap: Duration,
) -> Duration {
    recovery
        .max(timeout.saturating_add(retry).saturating_mul(attempts))
        .min(cap)
}

/// OAuth TTL including the refresh margin; never cache beyond the remote TTL.
pub(crate) fn token_expiry(now: Instant, seconds: u64) -> Option<Instant> {
    let delay = Duration::from_secs(seconds);
    if delay > millis(MAX_TIMER_MS) {
        return None;
    }
    let margin = (seconds / 10).clamp(1, 30);
    instant_after(now, Duration::from_secs(seconds.saturating_sub(margin)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_time_queue_epoch_and_lease_boundaries() {
        let now = Instant::now();
        let edge = instant_after(now, millis(MAX_TIMER_MS)).unwrap();
        assert!(rate_slot(now, now, Duration::ZERO).is_some());
        assert!(rate_slot(edge, now, Duration::ZERO).is_some());
        assert!(rate_slot(edge, now, millis(1)).is_none());
        assert!(rate_slot(now, now, Duration::MAX).is_none());
        assert_eq!(epoch_ttl(SystemTime::UNIX_EPOCH, 0), Some(0));
        assert_eq!(
            epoch_ttl(SystemTime::UNIX_EPOCH, u64::MAX),
            Some(u64::MAX / 1_000)
        );
        let later = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(1))
            .unwrap();
        assert_eq!(epoch_ttl(later, 0), Some(0));
        let cap = Duration::from_secs(900);
        for value in [0, 1, u64::MAX] {
            let duration = millis(value);
            let expected = duration
                .saturating_add(duration)
                .saturating_mul(u32::MAX)
                .max(duration)
                .min(cap);
            assert_eq!(
                probe_lease(duration, duration, duration, u32::MAX, cap),
                expected
            );
        }
        assert_eq!(ceil_millis(Duration::from_nanos(1)), 1);
        assert_eq!(ceil_millis(Duration::from_millis(1)), 1);
        assert_eq!(ceil_millis(Duration::MAX), u64::MAX);
    }

    #[test]
    fn remote_time_boundaries() {
        let now = Instant::now();
        for value in [0, 1, MAX_TIMER_MS, MAX_TIMER_MS + 1, u64::MAX] {
            assert_eq!(timer_millis(value).is_some(), value <= MAX_TIMER_MS);
            assert_eq!(
                instant_after(now, millis(value)).is_some(),
                value <= MAX_TIMER_MS
            );
            assert_eq!(
                system_after(SystemTime::UNIX_EPOCH, millis(value)).is_some(),
                value <= MAX_TIMER_MS
            );
        }
        assert_eq!(token_expiry(now, 0), Some(now));
        assert_eq!(token_expiry(now, 1), Some(now));
        assert!(token_expiry(now, MAX_TIMER_MS / 1_000).is_some());
        assert!(token_expiry(now, MAX_TIMER_MS / 1_000 + 1).is_none());
        assert!(token_expiry(now, u64::MAX).is_none());
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            0.0,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            assert!(positive_seconds(value).is_none());
        }
        assert_eq!(positive_seconds(1.0), Some(Duration::from_secs(1)));
        assert!(timer_sum(millis(MAX_TIMER_MS), millis(1)).is_none());
        assert!(timer_sum(Duration::MAX, Duration::MAX).is_none());
        for (value, expected) in [
            ("0", Some(0)),
            ("18446744073709551615", Some(u64::MAX)),
            ("18446744073709551616", Some(u64::MAX)),
            ("NaN", None),
            ("-1", None),
        ] {
            assert_eq!(remote_seconds_ms(value), expected);
        }
    }
}
