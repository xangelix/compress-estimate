//! The zstd backend: SCOPE (Sampled COst-model Parse Emulation) — faithful
//! chunk-local ports of zstd's fast/dfast/lazy parse loops, priced per 128
//! KiB block with an RFC 8878 Huffman/FSE cost model, and calibrated by
//! span anchors.

pub mod anchor;
pub mod cost;
pub mod levels;
pub mod lzparse;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::backend::{Backend, Calibration, LevelEstimate, SpanSample, StratumSamples};
use crate::stats::{Running, ci95_half, stratified_mean};

use anchor::AnchorFit;
use levels::{MAX_LEVEL, MIN_LEVEL};

thread_local! {
    static FEATURE_SCRATCH: RefCell<lzparse::FeatureScratch> =
        RefCell::new(lzparse::FeatureScratch::new());
    static PARSE_TABLES: RefCell<lzparse::ParseTables> =
        RefCell::new(lzparse::ParseTables::default());
}

/// The zstd backend.
///
/// ```
/// use compress_estimate::backends::zstd::Zstd;
/// let backend = Zstd::levels([3, 10, 15]);
/// ```
pub struct Zstd {
    levels: Vec<i32>,
    /// Max span bytes to really compress per anchor level during calibration.
    anchor_budget: usize,
    /// Time budget for each throughput microbenchmark.
    bench_time: Duration,
}

impl Default for Zstd {
    fn default() -> Self {
        Self::levels([3, 10, 15])
    }
}

impl Zstd {
    /// Estimate the given levels (clamped to 1..=22, sorted, deduplicated).
    pub fn levels(levels: impl IntoIterator<Item = i32>) -> Self {
        let mut levels: Vec<i32> = levels
            .into_iter()
            .map(|l| l.clamp(MIN_LEVEL, MAX_LEVEL))
            .collect();
        levels.sort_unstable();
        levels.dedup();
        if levels.is_empty() {
            levels = vec![3, 10, 15];
        }
        Zstd {
            levels,
            anchor_budget: 32 << 20,
            bench_time: Duration::from_millis(75),
        }
    }

    /// All levels 1..=22.
    pub fn all_levels() -> Self {
        Self::levels(MIN_LEVEL..=MAX_LEVEL)
    }

    /// Override the anchor span budget in bytes (0 disables calibration).
    /// This caps the contiguous span really compressed per anchor level.
    pub fn anchor_budget(mut self, bytes: usize) -> Self {
        self.anchor_budget = bytes;
        self
    }

    /// Override the per-level throughput-benchmark time budget.
    pub fn bench_time(mut self, t: Duration) -> Self {
        self.bench_time = t;
        self
    }
}

/// Per-chunk analysis: data features + modeled compressed fraction per
/// requested level.
#[derive(Clone)]
pub struct ChunkStats {
    pub features: lzparse::Features,
    /// Modeled compressed fraction (compressed / original) per level,
    /// aligned with [`Backend::levels`].
    pub fractions: Vec<f64>,
}

impl Backend for Zstd {
    type Level = i32;
    type Stats = ChunkStats;

    fn levels(&self) -> Vec<i32> {
        self.levels.clone()
    }

    fn analyze_chunk(&self, chunk: &[u8]) -> ChunkStats {
        let features = FEATURE_SCRATCH.with(|cell| cell.borrow_mut().scan_features(chunk));
        let fractions = PARSE_TABLES.with(|cell| {
            let mut tables = cell.borrow_mut();
            let mut blocks: Vec<lzparse::ParseStats> = Vec::new();
            self.levels
                .iter()
                .map(|&level| replay_and_cost(chunk, level, &mut blocks, &mut tables))
                .collect()
        });
        ChunkStats {
            features,
            fractions,
        }
    }

    fn primary(&self, stats: &ChunkStats) -> f64 {
        stats.fractions[stats.fractions.len() / 2]
    }

    fn estimate_levels(
        &self,
        strata: &[StratumSamples<'_, ChunkStats>],
        total_len: u64,
        cal: Calibration<'_>,
    ) -> Vec<LevelEstimate<i32>> {
        let n_levels = self.levels.len();

        // Stratified mean + SE of the modeled fraction per level.
        let mut mean_frac = vec![0.0; n_levels];
        let mut se_frac = vec![0.0; n_levels];
        for (li, (m, se)) in mean_frac.iter_mut().zip(se_frac.iter_mut()).enumerate() {
            let parts: Vec<(f64, Running)> = strata
                .iter()
                .map(|st| {
                    let mut r = Running::default();
                    for (_, stats) in &st.samples {
                        r.push(stats.fractions[li]);
                    }
                    (st.weight, r)
                })
                .collect();
            let (mean, s) = stratified_mean(&parts);
            *m = mean;
            *se = s;
        }

        // Anchoring: fit log-space correction from a really compressed
        // contiguous span (captures whole-file history effects).
        let fit = match (cal.enabled && self.anchor_budget > 0, cal.span) {
            (true, Some(span)) if span.bytes.len() >= 16 * 1024 => self.anchor_fit(&span),
            _ => AnchorFit::identity(),
        };

        self.levels
            .iter()
            .enumerate()
            .map(|(li, &level)| {
                let frac = fit.correct(level, mean_frac[li]);
                let frac_lo =
                    fit.correct(level, (mean_frac[li] - ci95_half(se_frac[li])).max(1e-9));
                let frac_hi = fit.correct(level, mean_frac[li] + ci95_half(se_frac[li]));
                let ratio = 1.0 / frac;
                // CI on ratio from CI on fraction (monotone transform).
                let r_hi = 1.0 / frac_lo;
                let r_lo = 1.0 / frac_hi.max(1e-9);
                LevelEstimate {
                    level,
                    ratio,
                    ratio_ci: ((r_hi - r_lo) / 2.0).max(0.0),
                    projected_size: (frac * total_len as f64).round() as u64,
                    reduction: (1.0 - frac).max(0.0),
                    throughput: None,
                }
            })
            .collect()
    }

    fn anchor_span_len(&self, total_len: u64) -> usize {
        if self.anchor_budget == 0 {
            return 0;
        }
        let want = self
            .anchor_levels()
            .iter()
            .map(|&l| self.span_want(l))
            .max()
            .unwrap_or(0) as u64;
        want.min(total_len) as usize
    }

    fn measure_throughput(&self, chunks: &[&[u8]], level: i32) -> Option<f64> {
        if chunks.is_empty() {
            return None;
        }
        // Byte cap scaled to expected slowness so slow levels stay cheap.
        let byte_cap: usize = if level >= 19 {
            1 << 20
        } else if level >= 13 {
            4 << 20
        } else if level >= 7 {
            12 << 20
        } else {
            24 << 20
        };
        // Per-iteration input cap. Levels ≤ 15 use long contiguous slices
        // (the anchor span, when present): small inputs make zstd clamp its
        // match tables and stay cache-hot, skewing measured speed by up to
        // 2x. bt levels are so slow that a 1 MiB cold slice already reflects
        // their whole-file speed.
        let slice_cap: usize = if level >= 16 { 1 << 20 } else { 32 << 20 };
        let slice = |i: usize| {
            let c = chunks[i % chunks.len()];
            &c[..c.len().min(slice_cap)]
        };
        let max_in = chunks
            .iter()
            .map(|c| c.len().min(slice_cap))
            .max()
            .unwrap_or(0);
        let mut compressor = zstd::bulk::Compressor::new(level).ok()?;
        let mut out = vec![0u8; zstd::zstd_safe::compress_bound(max_in)];

        // Warmup (not timed): lazy init, cache warming — on a small slice so
        // it stays cheap when the inputs are long.
        let warm = slice(0);
        compressor
            .compress_to_buffer(&warm[..warm.len().min(1 << 20)], &mut out)
            .ok()?;

        let mut total_bytes = 0u64;
        let mut total_time = Duration::ZERO;
        let start = Instant::now();
        let mut idx = 0;
        loop {
            let chunk = slice(idx);
            let t0 = Instant::now();
            let n = compressor.compress_to_buffer(chunk, &mut out).ok()?;
            let dt = t0.elapsed();
            let _ = n;
            total_bytes += chunk.len() as u64;
            total_time += dt;
            idx += 1;
            if total_bytes as usize >= byte_cap
                || start.elapsed() >= self.bench_time
                || (total_time >= Duration::from_millis(500) && level >= 19)
            {
                break;
            }
        }
        if total_time.is_zero() {
            return None;
        }
        Some(total_bytes as f64 / total_time.as_secs_f64())
    }

    fn chunk_histogram<'a>(&'a self, stats: &'a ChunkStats) -> Option<&'a crate::stats::Histogram> {
        Some(&stats.features.hist)
    }
}

impl Zstd {
    /// Levels at which the span is really compressed: the lowest and highest
    /// requested level of each strategy family (history effects are shared
    /// within a family but can differ in sign across families).
    fn anchor_levels(&self) -> Vec<i32> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.levels.len() {
            let fam = levels::family_of(self.levels[i]);
            let mut j = i + 1;
            while j < self.levels.len() && levels::family_of(self.levels[j]) == fam {
                j += 1;
            }
            out.push(self.levels[i]);
            if self.levels[j - 1] != self.levels[i] {
                out.push(self.levels[j - 1]);
            }
            i = j;
        }
        out
    }

    /// Span bytes wanted for anchoring one level: ~8x the level's window so
    /// the real span compression reaches the warm steady-state regime (the
    /// whole-file history residual roughly halves per doubling of span depth;
    /// 8x leaves only a few percent), hard capped for the bt family
    /// (btultra2 runs at ~5 MB/s) and by the budget.
    fn span_want(&self, level: i32) -> usize {
        let params = levels::params_for(level);
        let mult: u64 = match levels::family_of(level) {
            levels::Family::Bt => 4,
            _ => 8,
        };
        let mut want = (mult << params.window_log).min(self.anchor_budget as u64);
        if levels::family_of(level) == levels::Family::Bt {
            want = want.min(4 << 20);
        }
        want as usize
    }

    /// Really compress the span at the anchor levels and fit the log-space
    /// correction against a tiled-cold model replay of the same span bytes.
    /// Each anchor level uses a span prefix sized to its own window, so cheap
    /// levels stay cheap. Anchors run in parallel.
    fn anchor_fit(&self, span: &SpanSample) -> AnchorFit {
        use rayon::prelude::*;
        let chunk_len = span.chunk_len.max(1);
        let points: Vec<(i32, f64)> = self
            .anchor_levels()
            .par_iter()
            .filter_map(|&al| {
                let params = levels::params_for(al);
                let want = self.span_want(al).min(span.bytes.len());
                let bytes = &span.bytes[..want];
                let real = anchor::real_fraction(bytes, al)?;
                // Tiled-cold model replay of the same regime — but the model
                // fraction is statistical, so a deterministic subset of the
                // span's tiles (spread across it) is as informative as all
                // of them and much cheaper.
                let n_tiles = bytes.len().div_ceil(chunk_len);
                let step = (n_tiles / 16).max(1);
                let mut blocks: Vec<lzparse::ParseStats> = Vec::new();
                let mut tables = lzparse::ParseTables::default();
                let mut tile = Vec::new();
                for (i, t) in bytes.chunks(chunk_len).enumerate() {
                    if i % step != 0 {
                        continue;
                    }
                    lzparse::replay_blocks(t, &params, &mut tile, &mut tables);
                    blocks.append(&mut tile);
                }
                let modeled = cost::chunk_fraction(&blocks);
                if std::env::var_os("CE_DEBUG_ANCHOR").is_some() {
                    eprintln!(
                        "[anchor] L{al}: span {} bytes, real {:.4}, modeled {:.4}",
                        bytes.len(),
                        real,
                        modeled
                    );
                }
                if modeled > 0.0 {
                    Some((al, (real / modeled).ln()))
                } else {
                    None
                }
            })
            .collect();
        AnchorFit::fit(points)
    }
}

/// Replay + cost-model convenience for one level.
fn replay_and_cost(
    chunk: &[u8],
    level: i32,
    blocks: &mut Vec<lzparse::ParseStats>,
    tables: &mut lzparse::ParseTables,
) -> f64 {
    lzparse::replay_blocks(chunk, &levels::params_for(level), blocks, tables);
    cost::chunk_fraction(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn estimate(data: &[u8], levels: &[i32]) -> Vec<LevelEstimate<i32>> {
        let backend = Zstd::levels(levels.iter().copied());
        let stats = backend.analyze_chunk(data);
        let strata = vec![StratumSamples {
            weight: 1.0,
            samples: vec![(data, &stats)],
        }];
        backend.estimate_levels(
            &strata,
            data.len() as u64,
            Calibration {
                enabled: true,
                span: Some(SpanSample {
                    bytes: data,
                    chunk_len: data.len().max(1),
                }),
            },
        )
    }

    #[test]
    fn estimates_track_real_zstd() {
        // Semi-compressible text-ish data.
        let mut data = Vec::new();
        for i in 0..2000 {
            data.extend_from_slice(
                format!("INSERT INTO users (id, name, email) VALUES ({i}, 'user{i}', 'user{i}@example.com');\n").as_bytes(),
            );
        }
        let est = estimate(&data, &[3, 10]);
        for (row, level) in est.iter().zip([3, 10]) {
            let real = 1.0 / anchor::real_fraction(&data, level).unwrap();
            let err = (row.ratio - real).abs() / real;
            assert!(
                err < 0.15,
                "L{level}: model {:.2} vs real {real:.2} (err {err:.2})",
                row.ratio
            );
        }
        // No monotonicity assertion: real zstd is genuinely non-monotone on
        // small inputs (here L3 > L10), and the anchored estimate should
        // track that rather than smooth it over.
    }

    #[test]
    fn throughput_measurable() {
        let data = vec![b'x'; 256 * 1024];
        let backend = Zstd::levels([3]);
        let t = backend.measure_throughput(&[&data], 3);
        assert!(t.unwrap() > 1e6);
    }
}
