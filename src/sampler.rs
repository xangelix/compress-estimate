//! Strategic sampling: stratified random sampling with Neyman allocation for
//! seekable sources, reservoir sampling for streams.

use crate::stats::{Running, ci95_half, stratified_mean};

/// Small deterministic RNG (SplitMix64) — sampling must be reproducible for a
/// given input size, and we avoid a dependency for this.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `[0, n)` for n > 0.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// One equal-sized slice of the source.
pub struct Stratum {
    pub start: u64,
    pub len: u64,
    /// Fraction of the total source this stratum covers (sums to 1).
    pub weight: f64,
    /// Primary-quantity observations, one per sampled chunk.
    pub stat: Running,
    /// Offsets already sampled within this stratum.
    pub sampled: Vec<u64>,
}

impl Stratum {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// A planned read: which stratum, at what absolute offset, how many bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Read {
    pub stratum: usize,
    pub offset: u64,
    pub len: usize,
}

/// Stratified sampling plan over a seekable source.
pub struct Sampler {
    pub chunk_size: usize,
    pub strata: Vec<Stratum>,
    /// Total chunks the budget allows across all strata.
    pub chunk_budget: usize,
    sampled_total: usize,
    rng: Rng,
}

impl Sampler {
    /// Build a plan. `budget_bytes` is the total number of bytes we may read.
    pub fn plan(total_len: u64, budget_bytes: u64, chunk_size: usize, n_strata: usize) -> Self {
        let n_strata = n_strata
            .max(1)
            .min((total_len / chunk_size.max(1) as u64).max(1) as usize);
        let base = total_len / n_strata as u64;
        let mut strata = Vec::with_capacity(n_strata);
        let mut start = 0u64;
        for i in 0..n_strata {
            let len = if i + 1 == n_strata {
                total_len - start
            } else {
                base
            };
            strata.push(Stratum {
                start,
                len,
                weight: len as f64 / total_len.max(1) as f64,
                stat: Running::default(),
                sampled: Vec::new(),
            });
            start += len;
        }
        let chunk_budget = (budget_bytes as usize / chunk_size.max(1)).max(n_strata);
        Sampler {
            chunk_size,
            strata,
            chunk_budget,
            sampled_total: 0,
            rng: Rng::new(0xC0FF_EE11_2233_4455 ^ total_len),
        }
    }

    fn make_read(&mut self, s: usize) -> Read {
        let st = &self.strata[s];
        let max_off = st.len.saturating_sub(self.chunk_size as u64);
        // Rejection-sample a few times to avoid overlapping previous reads.
        let mut off = self.rng.below(max_off.max(1));
        for _ in 0..8 {
            let abs = st.start + off;
            if st.sampled.iter().all(|&p| {
                let a = p.max(abs);
                let b = p.min(abs);
                a - b >= self.chunk_size as u64
            }) {
                break;
            }
            off = self.rng.below(max_off.max(1));
        }
        let len = self.chunk_size.min(st.len as usize);
        Read {
            stratum: s,
            offset: st.start + off,
            len,
        }
    }

    /// A sampler-shaped view over already-collected chunks (stream reservoir
    /// output): one stratum per chunk, weighted by relative byte length so a
    /// short tail chunk does not get the weight of a full one.
    pub fn weighted(chunk_size: usize, chunk_lens: &[usize]) -> Self {
        let total: u64 = chunk_lens.iter().map(|&l| l as u64).sum();
        let strata = chunk_lens
            .iter()
            .map(|&l| Stratum {
                start: 0,
                len: l as u64,
                weight: l as f64 / total.max(1) as f64,
                stat: Running::default(),
                sampled: Vec::new(),
            })
            .collect();
        Sampler {
            chunk_size,
            strata,
            chunk_budget: chunk_lens.len(),
            sampled_total: chunk_lens.len(),
            rng: Rng::new(0xC0FF_EE11_2233_4455 ^ total),
        }
    }

    /// Deterministic full-coverage plan for inputs that fit the budget: one
    /// stratum per tile, each read exactly covering its tile, in order.
    pub fn cover(total_len: u64, chunk_size: usize) -> Self {
        let cs = chunk_size.max(1) as u64;
        let n = total_len.div_ceil(cs).max(1) as usize;
        let mut strata = Vec::with_capacity(n);
        for i in 0..n {
            let start = i as u64 * cs;
            let len = (total_len - start).min(cs);
            strata.push(Stratum {
                start,
                len,
                weight: len as f64 / total_len.max(1) as f64,
                stat: Running::default(),
                sampled: Vec::new(),
            });
        }
        Sampler {
            chunk_size,
            strata,
            chunk_budget: n,
            sampled_total: 0,
            rng: Rng::new(0xC0FF_EE11_2233_4455 ^ total_len),
        }
    }

    /// The reads that tile the whole source, one per stratum.
    pub fn cover_reads(&mut self) -> Vec<Read> {
        (0..self.strata.len())
            .map(|s| {
                let st = &self.strata[s];
                Read {
                    stratum: s,
                    offset: st.start,
                    len: st.len as usize,
                }
            })
            .collect()
    }

    /// Probe phase: one chunk per stratum (or fewer if a stratum is tiny).
    pub fn probe_reads(&mut self) -> Vec<Read> {
        (0..self.strata.len()).map(|s| self.make_read(s)).collect()
    }

    /// Record a chunk observation.
    pub fn observe(&mut self, read: Read, primary: f64) {
        let st = &mut self.strata[read.stratum];
        st.stat.push(primary);
        st.sampled.push(read.offset);
        self.sampled_total += 1;
    }

    /// Stratified mean of the primary quantity and its standard error.
    pub fn estimate(&self) -> (f64, f64) {
        let parts: Vec<(f64, Running)> = self
            .strata
            .iter()
            .map(|s| (s.weight, s.stat.clone()))
            .collect();
        stratified_mean(&parts)
    }

    /// Converged when every stratum has a sample, at least one refinement
    /// round happened, and the relative 95% CI half-width is under `target`.
    pub fn converged(&self, target: f64) -> bool {
        if self.strata.iter().any(|s| s.stat.count() == 0) {
            return false;
        }
        if self.sampled_total < self.strata.len() * 2 {
            return false;
        }
        let (mean, se) = self.estimate();
        if mean <= 0.0 {
            return true;
        }
        ci95_half(se) / mean < target
    }

    pub fn budget_exhausted(&self) -> bool {
        self.sampled_total >= self.chunk_budget
    }

    /// Next round of reads, Neyman-allocated: strata with more variance (and
    /// more weight) get more of the remaining budget.
    pub fn next_reads(&mut self) -> Vec<Read> {
        let remaining = self.chunk_budget.saturating_sub(self.sampled_total);
        if remaining == 0 {
            return Vec::new();
        }
        // This round: up to one more chunk per selected stratum, at most
        // `remaining` reads total; allocate proportionally to w_h * sigma_h.
        let n = self.strata.len();
        let mut wants: Vec<f64> = (0..n)
            .map(|s| {
                let st = &self.strata[s];
                let capacity_left =
                    st.len / self.chunk_size.max(1) as u64 > st.sampled.len() as u64;
                if capacity_left {
                    st.weight * (st.stat.stddev() + 1e-6)
                } else {
                    0.0
                }
            })
            .collect();
        let total: f64 = wants.iter().sum();
        if total <= 0.0 {
            // All identical: spread evenly.
            wants = (0..n)
                .map(|s| {
                    let st = &self.strata[s];
                    if st.len / self.chunk_size.max(1) as u64 > st.sampled.len() as u64 {
                        1.0
                    } else {
                        0.0
                    }
                })
                .collect();
        }
        let total: f64 = wants.iter().sum();
        let take = remaining.min(n);
        // Largest-remainder rounding of take reads across strata.
        let mut quota: Vec<(usize, f64)> = wants
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 0.0)
            .map(|(s, &w)| (s, w / total * take as f64))
            .collect();
        quota.sort_by(|a, b| {
            b.1.fract()
                .partial_cmp(&a.1.fract())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut reads = Vec::with_capacity(take);
        for (i, (s, q)) in quota.iter().enumerate() {
            let give = *q as usize
                + usize::from(i < take - quota.iter().map(|(_, q)| *q as usize).sum::<usize>());
            for _ in 0..give.max(1).min(1 + *q as usize) {
                if reads.len() >= take {
                    break;
                }
                reads.push(self.make_read(*s));
            }
        }
        reads
    }
}

/// Fixed-capacity reservoir for uniform sampling of a stream of unknown
/// length (Algorithm R).
pub struct Reservoir<T> {
    pub cap: usize,
    pub seen: u64,
    pub slots: Vec<T>,
    rng: Rng,
}

impl<T> Default for Reservoir<T> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<T> Reservoir<T> {
    pub fn new(cap: usize) -> Self {
        Reservoir {
            cap,
            seen: 0,
            slots: Vec::with_capacity(cap.min(1 << 16)),
            rng: Rng::new(0x5EED_5EED_5EED_5EED),
        }
    }

    pub fn offer(&mut self, item: T) {
        self.seen += 1;
        if self.slots.len() < self.cap {
            self.slots.push(item);
        } else if self.cap > 0 {
            let j = self.rng.below(self.seen);
            if j < self.cap as u64 {
                self.slots[j as usize] = item;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_covers_source() {
        let s = Sampler::plan(1_000_000, 100_000, 10_000, 8);
        assert_eq!(s.strata.len(), 8);
        let covered: u64 = s.strata.iter().map(|s| s.len).sum();
        assert_eq!(covered, 1_000_000);
        assert!((s.strata.iter().map(|s| s.weight).sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn probe_then_refine() {
        let mut s = Sampler::plan(10_000_000, 2_000_000, 100_000, 10);
        for r in s.probe_reads() {
            assert!(r.offset + r.len as u64 <= 10_000_000);
            let st = &s.strata[r.stratum];
            assert!(r.offset >= st.start && r.offset < st.end());
            s.observe(r, if r.stratum % 2 == 0 { 2.0 } else { 4.0 });
        }
        // Not converged after only one round.
        assert!(!s.converged(0.02));
        // Second round with identical values → zero variance → converges.
        let reads = s.next_reads();
        assert!(!reads.is_empty());
        for r in reads {
            s.observe(r, if r.stratum % 2 == 0 { 2.0 } else { 4.0 });
        }
        assert!(s.converged(0.02));
        let (mean, _se) = s.estimate();
        assert!((mean - 3.0).abs() < 1e-9);
    }

    #[test]
    fn neyman_favors_variance() {
        let mut s = Sampler::plan(10_000_000, 5_000_000, 100_000, 10);
        for r in s.probe_reads() {
            s.observe(r, 3.0);
        }
        // Inject variance only into stratum 3.
        for _ in 0..3 {
            let st = &mut s.strata[3];
            st.stat.push(1.0);
            st.stat.push(5.0);
        }
        let reads = s.next_reads();
        let count3 = reads.iter().filter(|r| r.stratum == 3).count();
        let count_any_other = (0..10)
            .filter(|&i| i != 3)
            .map(|i| reads.iter().filter(|r| r.stratum == i).count())
            .max()
            .unwrap_or(0);
        assert!(count3 >= count_any_other);
    }

    #[test]
    fn reservoir_uniform_slots() {
        let mut r: Reservoir<u32> = Reservoir::new(10);
        for i in 0..1000 {
            r.offer(i);
        }
        assert_eq!(r.slots.len(), 10);
        assert_eq!(r.seen, 1000);
    }
}
