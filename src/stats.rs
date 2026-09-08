//! Statistical primitives: byte histograms, entropy, running variance, and
//! stratified-sampling estimators.

/// Order-0 byte histogram over observed data.
#[derive(Clone)]
pub struct Histogram {
    pub counts: [u64; 256],
    pub total: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            counts: [0; 256],
            total: 0,
        }
    }
}

impl Histogram {
    pub fn add(&mut self, data: &[u8]) {
        for &b in data {
            self.counts[b as usize] += 1;
        }
        self.total += data.len() as u64;
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a += *b;
        }
        self.total += other.total;
    }

    /// Shannon entropy in bits per byte (0..=8).
    pub fn entropy(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let n = self.total as f64;
        let mut h = 0.0;
        for &c in &self.counts {
            if c > 0 {
                let p = c as f64 / n;
                h -= p * p.log2();
            }
        }
        h
    }

    /// Number of byte values observed at least once.
    pub fn distinct(&self) -> u32 {
        self.counts.iter().filter(|&&c| c > 0).count() as u32
    }

    /// Fraction of NUL bytes, 0.0..=1.0.
    pub fn nul_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.counts[0] as f64 / self.total as f64
        }
    }

    /// Fraction of bytes that are printable ASCII or common whitespace.
    pub fn printable_fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let mut n = 0u64;
        for (b, &c) in self.counts.iter().enumerate() {
            let b = b as u8;
            if (0x20..=0x7e).contains(&b) || matches!(b, b'\t' | b'\n' | b'\r' | 0x0c) {
                n += c;
            }
        }
        n as f64 / self.total as f64
    }
}

/// Welford running mean/variance.
#[derive(Clone, Default)]
pub struct Running {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Running {
    pub fn push(&mut self, x: f64) {
        self.n += 1;
        let d = x - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (x - self.mean);
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// Population variance (ddof=0); 0 when fewer than 2 samples.
    pub fn variance(&self) -> f64 {
        if self.n < 2 {
            0.0
        } else {
            self.m2 / self.n as f64
        }
    }

    pub fn stddev(&self) -> f64 {
        self.variance().sqrt()
    }
}

/// Stratified mean over per-stratum estimators.
///
/// Each stratum covers `weight` of the total population (weights sum to 1) and
/// contributes a `Running` over per-chunk observations. Returns
/// `(mean, standard_error_of_mean)`.
pub fn stratified_mean(strata: &[(f64, Running)]) -> (f64, f64) {
    let mut mean = 0.0;
    let mut var_mean = 0.0;
    for (w, r) in strata {
        if r.count() == 0 {
            continue;
        }
        mean += w * r.mean();
        if r.count() >= 2 {
            // Var of stratum mean = w^2 * s^2 / n (finite-population correction
            // ignored: sampled fractions are tiny by design).
            var_mean += w * w * r.variance() / r.count() as f64;
        }
    }
    (mean, var_mean.sqrt())
}

/// Half-width of a 95% confidence interval given a standard error.
pub fn ci95_half(se: f64) -> f64 {
    1.96 * se
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_bounds() {
        let mut h = Histogram::default();
        h.add(&[0u8; 1024]);
        assert_eq!(h.entropy(), 0.0);
        assert_eq!(h.distinct(), 1);

        let mut h = Histogram::default();
        let data: Vec<u8> = (0..=255).collect();
        h.add(&data);
        assert!((h.entropy() - 8.0).abs() < 1e-9);
        assert_eq!(h.distinct(), 256);
    }

    #[test]
    fn running_stats() {
        let mut r = Running::default();
        for x in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
            r.push(x);
        }
        assert!((r.mean() - 5.0).abs() < 1e-9);
        assert!((r.stddev() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn stratified_combination() {
        let mut a = Running::default();
        let mut b = Running::default();
        for _ in 0..10 {
            a.push(2.0);
        }
        for _ in 0..10 {
            b.push(4.0);
        }
        let (m, se) = stratified_mean(&[(0.5, a), (0.5, b)]);
        assert!((m - 3.0).abs() < 1e-9);
        assert_eq!(se, 0.0);
    }
}
