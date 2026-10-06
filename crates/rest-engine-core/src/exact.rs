//! Exact arithmetic for the numeric limits of the engine.
//!
//! Rates, backoff factors and polling backoff are configured as `f64`. Every
//! finite `f64` is exactly a dyadic rational `m · 2^e`, so the quantities the
//! engine derives from them (an interval in nanoseconds, a delay in
//! milliseconds) can be computed exactly in integer arithmetic. Nothing here
//! rounds through floating point, saturates a cast or meets a NaN: a value
//! with no exact representation in the target unit is refused by the caller
//! before any request, and a value that genuinely exceeds a configured cap is
//! the cap.

use std::time::Duration;

/// `x = m · 2^e` exactly, for a finite `x > 0`; `None` otherwise.
pub(crate) fn dyadic(x: f64) -> Option<(u64, i32)> {
    if !x.is_finite() || x <= 0.0 {
        return None;
    }
    let bits = x.to_bits();
    let exponent = i32::try_from((bits >> 52) & 0x7ff).ok()?;
    let fraction = bits & ((1_u64 << 52) - 1);
    if exponent == 0 {
        Some((fraction, -1074))
    } else {
        Some((fraction | (1_u64 << 52), exponent - 1075))
    }
}

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// The interval between two requests at `rate` requests per second.
///
/// The interval is `10^9 / rate` nanoseconds, rounded up to a whole
/// nanosecond so the engine never sends faster than the rate allows (the
/// rounding is below one nanosecond and always on the slow side). `None` when
/// the rate is not finite and positive, when the interval is shorter than one
/// nanosecond (a rate above 10^9 per second), or when it does not fit a
/// `Duration` of `u64` nanoseconds (a rate below about 5.4 · 10^-11).
pub(crate) fn rate_interval(rate: f64) -> Option<Duration> {
    let (mantissa, exponent) = dyadic(rate)?;
    let mantissa = u128::from(mantissa);
    // interval = 10^9 / (m · 2^e) = numerator / denominator, both integers.
    let (numerator, denominator) = if exponent >= 0 {
        // A rate of at least 2^e: beyond 2^30 > 10^9 it is shorter than 1 ns.
        let shift = u32::try_from(exponent).ok()?;
        if shift > 30 {
            return None;
        }
        (NANOS_PER_SECOND, mantissa << shift)
    } else {
        let shift = exponent.unsigned_abs();
        // 10^9 < 2^30, so 10^9 · 2^shift fits a u128 up to shift 97; beyond
        // that the interval is at least 10^9 · 2^98 / 2^53 > 2^64 ns.
        if shift > 97 {
            return None;
        }
        (NANOS_PER_SECOND << shift, mantissa)
    };
    if numerator < denominator {
        return None;
    }
    let nanos = numerator.div_ceil(denominator);
    u64::try_from(nanos).ok().map(Duration::from_nanos)
}

/// `min(cap, floor(value · factor))`, exactly, for a finite `factor >= 1`.
///
/// `None` when the factor is not finite or is below one: callers validate it
/// before any request, so `None` is an internal invariant failure, never a
/// value to replace. A product above the cap is the cap because the true
/// value exceeds it, not because of a saturating conversion.
pub(crate) fn scale_floor(value: u64, factor: f64, cap: u64) -> Option<u64> {
    if !factor.is_finite() || factor < 1.0 {
        return None;
    }
    let (mantissa, exponent) = dyadic(factor)?;
    if value == 0 {
        return Some(0);
    }
    // value < 2^64 and mantissa < 2^53, so the product fits in 117 bits.
    let product = u128::from(value) * u128::from(mantissa);
    let scaled = if exponent >= 0 {
        let shift = exponent.unsigned_abs();
        // value >= 1 and mantissa >= 1: a shift past the free high bits is
        // a product above 2^128, far above any u64 cap.
        if shift >= product.leading_zeros() {
            return Some(cap);
        }
        product << shift
    } else {
        // factor >= 1 is a normal number with m >= 2^k, so k <= 52.
        product >> exponent.unsigned_abs()
    };
    Some(u64::try_from(scaled).map_or(cap, |scaled| scaled.min(cap)))
}

/// The backoff delays of one retry loop, in milliseconds.
///
/// The first retry waits `backoff_base_ms`; each later one waits the previous
/// delay times `backoff_factor`, rounded down to a whole millisecond, and
/// never more than `max_backoff_ms`. Each step is exact (`scale_floor`), so a
/// base of zero always waits zero and a factor whose powers overflow reaches
/// the cap because the true delay exceeds it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Backoff {
    next_ms: u64,
    factor: f64,
    max_ms: u64,
}

impl Backoff {
    pub(crate) fn new(base_ms: u64, factor: f64, max_ms: u64) -> Option<Self> {
        // Validates the factor once: `scale_floor` fails on the same inputs.
        scale_floor(0, factor, max_ms)?;
        Some(Self {
            next_ms: base_ms.min(max_ms),
            factor,
            max_ms,
        })
    }

    /// The delay before the next retry, in milliseconds.
    pub(crate) fn next_ms(&mut self) -> u64 {
        let current = self.next_ms;
        // `new` validated the factor, so the fallback is never taken; it is
        // the cap rather than a panic in library code.
        self.next_ms = scale_floor(current, self.factor, self.max_ms).unwrap_or(self.max_ms);
        current
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Backoff, dyadic, rate_interval, scale_floor};

    #[test]
    fn rate_intervals_are_exact_or_refused() {
        assert_eq!(rate_interval(1.0), Some(Duration::from_secs(1)));
        assert_eq!(rate_interval(1e9), Some(Duration::from_nanos(1)));
        // 10^9 / 3 = 333 333 333.3... ns, rounded up: never faster.
        assert_eq!(rate_interval(3.0), Some(Duration::from_nanos(333_333_334)));
        assert_eq!(rate_interval(0.5), Some(Duration::from_secs(2)));
        // Shorter than one nanosecond.
        assert_eq!(rate_interval(2e9), None);
        assert_eq!(rate_interval(1e9 + 1.0), None);
        assert_eq!(rate_interval(f64::MAX), None);
        // Longer than a Duration of u64 nanoseconds.
        assert_eq!(rate_interval(f64::MIN_POSITIVE), None);
        assert_eq!(rate_interval(5e-324), None);
        assert_eq!(rate_interval(5e-11), None);
        assert!(rate_interval(6e-11).is_some());
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(rate_interval(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn scaling_is_exact_and_capped_only_above_the_cap() {
        assert_eq!(scale_floor(0, f64::MAX, 30_000), Some(0));
        assert_eq!(scale_floor(1, f64::MAX, 30_000), Some(30_000));
        assert_eq!(scale_floor(500, 2.0, 30_000), Some(1_000));
        assert_eq!(scale_floor(3, 1.5, u64::MAX), Some(4));
        assert_eq!(scale_floor(u64::MAX, 1.0, u64::MAX), Some(u64::MAX));
        assert_eq!(scale_floor(u64::MAX, 2.0, u64::MAX), Some(u64::MAX));
        assert_eq!(scale_floor(1, 0.5, 10), None);
        assert_eq!(scale_floor(1, f64::NAN, 10), None);
        assert_eq!(scale_floor(1, f64::INFINITY, 10), None);
    }

    #[test]
    fn backoff_with_a_zero_base_never_waits() {
        let mut backoff = Backoff::new(0, f64::MAX, 30_000).unwrap();
        for _ in 0..10 {
            assert_eq!(backoff.next_ms(), 0);
        }
        let mut backoff = Backoff::new(1, f64::MAX, 30_000).unwrap();
        assert_eq!(backoff.next_ms(), 1);
        assert_eq!(backoff.next_ms(), 30_000);
        assert_eq!(backoff.next_ms(), 30_000);
    }

    #[test]
    fn dyadic_decomposition_is_exact() {
        for value in [
            1.0,
            0.5,
            3.0,
            1e9,
            2.5e-11,
            f64::MIN_POSITIVE,
            5e-324,
            f64::MAX,
        ] {
            let (mantissa, exponent) = dyadic(value).unwrap();
            #[allow(clippy::cast_precision_loss)]
            let rebuilt = (mantissa as f64) * 2_f64.powi(exponent);
            if exponent > -1022 && exponent < 970 {
                assert_eq!(rebuilt, value);
            }
        }
    }
}
