//! Generatore pseudo-casuale deterministico (SplitMix64).
//!
//! Ogni scelta casuale della campagna parte da un seed esplicito: stesso seed,
//! stessa sequenza di scenari e di parametri. Non dipende da hash, thread o
//! tempo.

#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Intero uniforme in `0..bound`, senza distorsione modulare (rigetto).
    /// `bound` pari a zero restituisce zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let zone = u64::MAX - (u64::MAX % bound);
        loop {
            let value = self.next_u64();
            if value < zone {
                return value % bound;
            }
        }
    }

    /// Intero uniforme nell'intervallo chiuso `low..=high`; se `high < low`
    /// restituisce `low`.
    pub fn between(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        let span = high - low;
        if span == u64::MAX {
            return self.next_u64();
        }
        low + self.below(span + 1)
    }
}

/// Seed derivato per un flusso indipendente (per esempio un biglietto di
/// carico): la funzione è pura, quindi lo stesso `(seed, stream)` produce
/// sempre lo stesso valore.
pub fn derive(seed: u64, stream: u64) -> u64 {
    SplitMix64::new(seed ^ stream.wrapping_mul(0xD6E8_FEB8_6659_FD93)).next_u64()
}

#[cfg(test)]
mod tests {
    use super::{SplitMix64, derive};

    #[test]
    fn the_sequence_is_fixed_by_the_seed() {
        let mut first = SplitMix64::new(42);
        let mut second = SplitMix64::new(42);
        for _ in 0..1_000 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
        // Valore di riferimento di SplitMix64 per seed 0.
        assert_eq!(SplitMix64::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(derive(7, 3), derive(7, 3));
        assert_ne!(derive(7, 3), derive(7, 4));
    }

    #[test]
    fn bounded_values_stay_in_range() {
        let mut rng = SplitMix64::new(9);
        for _ in 0..10_000 {
            assert!(rng.below(7) < 7);
            let value = rng.between(3, 5);
            assert!((3..=5).contains(&value));
        }
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.between(5, 5), 5);
        assert_eq!(rng.between(6, 2), 6);
    }
}
