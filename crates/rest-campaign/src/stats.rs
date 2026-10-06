//! Statistiche della campagna a memoria costante.
//!
//! Le latenze vanno in un istogramma log-lineare a dimensione fissa: un soak
//! di ore non deve far crescere la memoria dell'harness, altrimenti la misura
//! dell'RSS del processo confonderebbe l'harness con il motore. La risoluzione
//! è dichiarata: fino a 127 µs ogni valore è esatto, oltre ogni bucket copre al
//! più 1/64 del suo limite inferiore (circa 1,6%) e il percentile riportato è
//! il **limite superiore** del bucket, quindi un confronto con una soglia è
//! conservativo (mai più ottimista del valore reale).

use serde::{Deserialize, Serialize};

const SUB_BUCKET_BITS: u32 = 6;
const SUB_BUCKETS: u64 = 1 << SUB_BUCKET_BITS;
/// 64 valori esatti più 58 ottave da 64 sotto-bucket (esponenti da 6 a 63).
const BUCKETS: usize = (SUB_BUCKETS + (64 - SUB_BUCKET_BITS as u64) * SUB_BUCKETS) as usize;

#[derive(Clone, Debug)]
pub struct Histogram {
    counts: Vec<u64>,
    count: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: vec![0; BUCKETS],
            count: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }
}

fn bucket_index(value: u64) -> usize {
    if value < SUB_BUCKETS {
        return value as usize;
    }
    let exponent = 63 - value.leading_zeros();
    let shift = exponent - SUB_BUCKET_BITS;
    let sub = (value >> shift) - SUB_BUCKETS;
    (SUB_BUCKETS + u64::from(shift) * SUB_BUCKETS + sub) as usize
}

fn bucket_upper_bound(index: usize) -> u64 {
    let index = index as u64;
    if index < SUB_BUCKETS {
        return index;
    }
    let shift = (index - SUB_BUCKETS) / SUB_BUCKETS;
    let sub = (index - SUB_BUCKETS) % SUB_BUCKETS;
    let lower = (SUB_BUCKETS + sub) << shift;
    lower.saturating_add((1_u64 << shift) - 1)
}

impl Histogram {
    pub fn record(&mut self, value: u64) {
        let index = bucket_index(value);
        if let Some(slot) = self.counts.get_mut(index) {
            *slot += 1;
        }
        self.count += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += u128::from(value);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Percentile nearest-rank espresso in millesimi (`990` = p99): limite
    /// superiore del bucket che contiene il valore di rango
    /// `ceil(count * per_mille / 1000)`, mai oltre il massimo esatto.
    pub fn percentile(&self, per_mille: u64) -> Option<u64> {
        if self.count == 0 || per_mille == 0 || per_mille > 1_000 {
            return None;
        }
        let product = u128::from(self.count) * u128::from(per_mille);
        let rank = product.div_ceil(1_000).max(1);
        let mut cumulative = 0_u128;
        for (index, count) in self.counts.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                return Some(bucket_upper_bound(index).min(self.max));
            }
        }
        Some(self.max)
    }

    pub fn summary(&self) -> LatencySummary {
        let micros_to_ms = |value: u64| value.div_ceil(1_000);
        LatencySummary {
            count: self.count,
            min_ms: (self.count > 0).then_some(self.min / 1_000),
            p50_ms: self.percentile(500).map(micros_to_ms),
            p95_ms: self.percentile(950).map(micros_to_ms),
            p99_ms: self.percentile(990).map(micros_to_ms),
            max_ms: (self.count > 0).then(|| micros_to_ms(self.max)),
            mean_ms: (self.count > 0).then(|| {
                let mean = self.sum / u128::from(self.count);
                u64::try_from(mean.div_ceil(1_000)).unwrap_or(u64::MAX)
            }),
        }
    }
}

/// Latenze in millisecondi, arrotondate per eccesso (il minimo per difetto).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LatencySummary {
    pub count: u64,
    pub min_ms: Option<u64>,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub p99_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub mean_ms: Option<u64>,
}

/// Mediana inferiore esatta di una serie intera (`None` se vuota).
pub fn median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get((sorted.len() - 1) / 2).copied()
}

/// Pendenza ai minimi quadrati di `values` rispetto a `times_s`, in unità
/// all'ora. `None` con meno di due punti o tempi tutti uguali.
pub fn slope_per_hour(times_s: &[f64], values: &[u64]) -> Option<f64> {
    if times_s.len() != values.len() || times_s.len() < 2 {
        return None;
    }
    let count = times_s.len() as f64;
    let mean_t = times_s.iter().sum::<f64>() / count;
    let mean_v = values.iter().map(|value| *value as f64).sum::<f64>() / count;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (time, value) in times_s.iter().zip(values) {
        let dt = time - mean_t;
        numerator += dt * (*value as f64 - mean_v);
        denominator += dt * dt;
    }
    if denominator <= 0.0 {
        return None;
    }
    Some(numerator / denominator * 3_600.0)
}

/// Andamento di una risorsa dopo il warm-up.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Trend {
    /// Campioni usati dopo il warm-up.
    pub samples: usize,
    /// Mediana della finestra iniziale e di quella finale (stesse unità della
    /// risorsa).
    pub first_window_median: Option<u64>,
    pub last_window_median: Option<u64>,
    /// `last - first`, con segno.
    pub growth: Option<i64>,
    /// Pendenza ai minimi quadrati, unità all'ora.
    pub slope_per_hour: Option<f64>,
    pub peak: Option<u64>,
}

/// Calcola l'andamento di una serie `(t_s, valore)` scartando i campioni con
/// `t_s < warmup_s`. Le finestre iniziale e finale contengono ciascuna
/// `window_fraction` dei campioni rimasti (almeno uno).
pub fn trend(points: &[(f64, u64)], warmup_s: f64, window_fraction: f64) -> Trend {
    let kept: Vec<(f64, u64)> = points
        .iter()
        .copied()
        .filter(|(time, _)| *time >= warmup_s)
        .collect();
    let peak = points.iter().map(|(_, value)| *value).max();
    if kept.is_empty() {
        return Trend {
            peak,
            ..Trend::default()
        };
    }
    let window = ((kept.len() as f64 * window_fraction).floor() as usize).clamp(1, kept.len());
    let values: Vec<u64> = kept.iter().map(|(_, value)| *value).collect();
    let times: Vec<f64> = kept.iter().map(|(time, _)| *time).collect();
    let first = median(&values[..window]);
    let last = median(&values[values.len() - window..]);
    let growth = match (first, last) {
        (Some(first), Some(last)) => {
            let difference = i128::from(last) - i128::from(first);
            i64::try_from(difference).ok()
        }
        _ => None,
    };
    Trend {
        samples: kept.len(),
        first_window_median: first,
        last_window_median: last,
        growth,
        slope_per_hour: slope_per_hour(&times, &values),
        peak,
    }
}

#[cfg(test)]
mod tests {
    use super::{BUCKETS, Histogram, bucket_index, bucket_upper_bound, median, trend};

    #[test]
    fn buckets_cover_every_value_and_bound_it_from_above() {
        let probes = [
            0_u64,
            1,
            63,
            64,
            65,
            127,
            128,
            1_000,
            65_535,
            1 << 40,
            u64::MAX - 1,
            u64::MAX,
        ];
        for value in probes {
            let index = bucket_index(value);
            assert!(index < BUCKETS, "indice fuori dall'istogramma");
            let upper = bucket_upper_bound(index);
            assert!(upper >= value);
            if value >= 64 {
                // Larghezza relativa del bucket al più 1/64.
                assert!(upper - value <= value / 64, "bucket troppo largo");
            } else {
                assert_eq!(upper, value);
            }
        }
        assert_eq!(bucket_index(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn percentiles_follow_the_nearest_rank_rule() {
        let mut histogram = Histogram::default();
        for value in 1..=100_u64 {
            histogram.record(value);
        }
        // Fino a 127 i bucket sono larghi 1, quindi i percentili sono esatti
        // e mai oltre il massimo osservato.
        assert_eq!(histogram.percentile(500), Some(50));
        assert_eq!(histogram.percentile(990), Some(99));
        assert_eq!(histogram.percentile(1_000), Some(100));
        assert_eq!(Histogram::default().percentile(500), None);
        assert_eq!(histogram.percentile(0), None);
    }

    #[test]
    fn a_linear_leak_is_seen_as_growth_and_a_plateau_is_not() {
        let leak: Vec<(f64, u64)> = (0..100_u32)
            .map(|step| (f64::from(step) * 60.0, 1_000 + u64::from(step) * 10))
            .collect();
        let leaking = trend(&leak, 600.0, 0.2);
        assert!(leaking.growth.unwrap_or_default() > 500);
        assert!(leaking.slope_per_hour.unwrap_or_default() > 500.0);

        let plateau: Vec<(f64, u64)> = (0..100_u32)
            .map(|step| (f64::from(step) * 60.0, 5_000 + u64::from(step % 3)))
            .collect();
        let flat = trend(&plateau, 600.0, 0.2);
        assert!(flat.growth.unwrap_or_default().abs() <= 2);
        assert!(flat.slope_per_hour.unwrap_or_default().abs() < 5.0);
        assert_eq!(median(&[3, 1, 2, 4]), Some(2));
    }
}
