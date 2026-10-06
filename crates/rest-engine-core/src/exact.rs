//! Exact arithmetic for the numeric limits of the engine.
//!
//! Rates, backoff factors and polling backoff are configured as `f64`. Every
//! finite `f64` is exactly a dyadic rational `m · 2^e`, so the quantities the
//! engine derives from them (an interval in nanoseconds, a delay in
//! milliseconds) are computed in integer arithmetic. Nothing here rounds
//! through floating point, saturates a cast or meets a NaN: a value with no
//! representation in the target unit is refused by the caller before any
//! request, and a value that genuinely exceeds a configured cap is the cap.

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
/// rounding is below one nanosecond and always on the slow side).
///
/// The declared domain: the exact interval is at least one nanosecond (a
/// rate of at most 10^9 per second) and, rounded up, at most `u64::MAX`
/// nanoseconds (about 584 years; a rate of at least about 5.4 · 10^-11).
/// Outside it, and for a rate that is not finite and positive, `None`.
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

/// The delays of one backoff sequence, in milliseconds.
///
/// The definition: `D_0 = min(base, max)` and
/// `D_{n+1} = min(max, trunc(D_n · factor))`, where `trunc` truncates to a
/// multiple of `2^-128` ms. The n-th delay is `floor(D_n)`. Every step is
/// integer arithmetic on the exact value of the factor (`m · 2^e`), in 256
/// bits: no floating point, no NaN, no saturating cast.
///
/// Against the ideal `min(max, floor(base · factor^n))` the delay is never
/// longer and at most one millisecond shorter: the truncation error after n
/// steps is below `2^-128 · factor^n / (factor - 1)`, and `factor^n` stays
/// below `2^64` until the cap, while `factor - 1 >= 2^-52` for any factor
/// above one, so the error is below `2^-12` ms. It is exactly the ideal as
/// long as no truncation happens (an integer factor, or `factor = p / 2^k`
/// for the first `128 / k` steps) and whenever the cap is reached. The
/// sequence never stalls: a delay of at least 1 ms grows by at least
/// `2^-52` ms per step, far above the truncation. A base of zero is zero
/// forever.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Backoff {
    /// Whole milliseconds of `D_n`; never above `max_ms`.
    whole: u128,
    /// Fraction of `D_n`, in units of `2^-128` ms.
    fraction: u128,
    mantissa: u64,
    exponent: i32,
    max_ms: u64,
}

impl Backoff {
    /// `None` when the factor is not finite or is below one: callers validate
    /// it before any request, so `None` is an internal invariant failure.
    pub(crate) fn new(base_ms: u64, factor: f64, max_ms: u64) -> Option<Self> {
        if !factor.is_finite() || factor < 1.0 {
            return None;
        }
        let (mantissa, exponent) = dyadic(factor)?;
        Some(Self {
            whole: u128::from(base_ms.min(max_ms)),
            fraction: 0,
            mantissa,
            exponent,
            max_ms,
        })
    }

    /// The current delay, in milliseconds; the sequence then advances.
    pub(crate) fn next_ms(&mut self) -> u64 {
        // `whole` never exceeds `max_ms`, so the conversion cannot fail.
        let current = u64::try_from(self.whole).unwrap_or(self.max_ms);
        self.advance();
        current
    }

    fn saturate(&mut self) {
        self.whole = u128::from(self.max_ms);
        self.fraction = 0;
    }

    /// `D_{n+1} = min(max, trunc(D_n · m · 2^e))`.
    fn advance(&mut self) {
        // D_n < 2^64 ms, so D_n · 2^128 < 2^192, and times m < 2^53 the
        // product is below 2^245: it fits the 256 bits (high, low).
        let mantissa = u128::from(self.mantissa);
        let low_low = (self.fraction & u128::from(u64::MAX)) * mantissa;
        let low_high = (self.fraction >> 64) * mantissa;
        let (low, carry) = low_low.overflowing_add(low_high << 64);
        let high = self.whole * mantissa + (low_high >> 64) + u128::from(carry);
        if high == 0 && low == 0 {
            // A base of zero stays zero whatever the factor.
            return;
        }
        let (high, low) = if self.exponent >= 0 {
            let shift = self.exponent.unsigned_abs();
            let free = if high == 0 {
                128 + low.leading_zeros()
            } else {
                high.leading_zeros()
            };
            if shift > free {
                // Above 2^256 units: far above any u64 cap.
                self.saturate();
                return;
            }
            shift_left(high, low, shift)
        } else {
            shift_right(high, low, self.exponent.unsigned_abs())
        };
        if high >= u128::from(self.max_ms) {
            self.saturate();
        } else {
            self.whole = high;
            self.fraction = low;
        }
    }
}

/// `(high, low) << shift`, for a value whose top `shift` bits are zero.
fn shift_left(high: u128, low: u128, shift: u32) -> (u128, u128) {
    match shift {
        0 => (high, low),
        1..128 => ((high << shift) | (low >> (128 - shift)), low << shift),
        _ => (low.checked_shl(shift - 128).unwrap_or(0), 0),
    }
}

/// `(high, low) >> shift`, truncating.
fn shift_right(high: u128, low: u128, shift: u32) -> (u128, u128) {
    match shift {
        0 => (high, low),
        1..128 => (high >> shift, (low >> shift) | (high << (128 - shift))),
        _ => (0, high.checked_shr(shift - 128).unwrap_or(0)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Backoff, dyadic, rate_interval};

    #[test]
    fn rate_intervals_are_exact_or_refused() {
        assert_eq!(rate_interval(1.0), Some(Duration::from_secs(1)));
        assert_eq!(rate_interval(1e9), Some(Duration::from_nanos(1)));
        // 10^9 / 3 = 333 333 333.3... ns, rounded up: never faster.
        assert_eq!(rate_interval(3.0), Some(Duration::from_nanos(333_333_334)));
        assert_eq!(rate_interval(0.5), Some(Duration::from_secs(2)));
        // Shorter than one nanosecond.
        assert_eq!(rate_interval(2e9), None);
        assert_eq!(rate_interval(1e9_f64.next_up()), None);
        assert_eq!(rate_interval(f64::MAX), None);
        // Longer than u64::MAX nanoseconds: 2^-35 per second is an interval
        // of 10^9 · 2^35 ns, about 1 089 years, which a Duration could hold
        // but the declared domain does not.
        assert_eq!(rate_interval(2_f64.powi(-35)), None);
        assert_eq!(rate_interval(f64::MIN_POSITIVE), None);
        assert_eq!(rate_interval(5e-324), None);
        assert_eq!(rate_interval(5e-11), None);
        assert!(rate_interval(6e-11).is_some());
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(rate_interval(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn the_longest_accepted_interval_is_at_most_u64_max_nanoseconds() {
        // The smallest accepted rate r satisfies 10^9 / r <= u64::MAX, i.e.
        // 10^9 · 2^k <= u64::MAX · m for r = m / 2^k: found from the declared
        // limit, not from the implementation.
        let accepted = |rate: f64| {
            let (mantissa, exponent) = dyadic(rate).unwrap();
            assert!(exponent < 0);
            (1_000_000_000_u128 << exponent.unsigned_abs())
                <= u128::from(u64::MAX) * u128::from(mantissa)
        };
        #[allow(clippy::cast_precision_loss)]
        let mut rate = 1e9 / u64::MAX as f64;
        while accepted(rate) {
            rate = rate.next_down();
        }
        while !accepted(rate) {
            rate = rate.next_up();
        }
        let longest = rate_interval(rate).unwrap();
        assert!(longest.as_nanos() <= u128::from(u64::MAX));
        // One ulp of the rate is about 2^-87 per second: the interval it
        // spans is a few thousand nanoseconds at this scale.
        assert!(longest.as_nanos() > u128::from(u64::MAX) - (1 << 20));
        assert_eq!(rate_interval(rate.next_down()), None);
    }

    fn sequence(base: u64, factor: f64, max: u64, steps: usize) -> Vec<u64> {
        let mut backoff = Backoff::new(base, factor, max).unwrap();
        (0..steps).map(|_| backoff.next_ms()).collect()
    }

    #[test]
    fn a_small_base_with_a_fractional_factor_reaches_the_cap() {
        // floor(1.5^n): 1, 1, 2, 3, 5, 7, 11, ... and the cap at n = 26.
        // 1.5^n is exact in f64 for these n (3^n < 2^53).
        let delays = sequence(1, 1.5, 30_000, 30);
        let mut ideal = 1.0_f64;
        for (step, delay) in delays.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let expected = (ideal.floor() as u64).min(30_000);
            assert_eq!(*delay, expected, "step {step}");
            ideal *= 1.5;
        }
        assert_eq!(delays.get(25), Some(&25_251));
        assert_eq!(delays.get(26), Some(&30_000));
    }

    #[test]
    fn the_smallest_factor_above_one_still_grows() {
        let base = 1_u64 << 62;
        let delays = sequence(base, 1.0 + f64::EPSILON, u64::MAX, 4);
        // base · 2^-52 = 1 024 ms more per step.
        assert_eq!(delays, vec![base, base + 1_024, base + 2_048, base + 3_072]);
    }

    #[test]
    fn backoff_with_a_zero_base_never_waits() {
        assert_eq!(sequence(0, f64::MAX, 30_000, 10), vec![0; 10]);
        assert_eq!(sequence(1, f64::MAX, 30_000, 3), vec![1, 30_000, 30_000]);
        assert_eq!(
            sequence(500, 2.0, 30_000, 8),
            vec![500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]
        );
        assert_eq!(sequence(u64::MAX, 2.0, u64::MAX, 2), vec![u64::MAX; 2]);
        assert_eq!(sequence(7, 1.0, 30_000, 3), vec![7; 3]);
        assert_eq!(sequence(40_000, 1.0, 30_000, 2), vec![30_000; 2]);
        assert!(Backoff::new(1, 0.5, 10).is_none());
        assert!(Backoff::new(1, f64::NAN, 10).is_none());
        assert!(Backoff::new(1, f64::INFINITY, 10).is_none());
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
