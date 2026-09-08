//! # compress-estimate
//!
//! Estimate the compression ratio and speed of compressing a data source —
//! without actually compressing it — by sampling the source *strategically*
//! and simulating the compressor's cost model on the samples.
//!
//! The crate is built around the [`Backend`] trait; the flagship backend is
//! [`backends::zstd::Zstd`], which implements the SCOPE algorithm: faithful
//! chunk-local ports of zstd's parse loops, an RFC 8878 cost model, and
//! self-calibration against a small contiguous span of real compression
//! (which captures whole-file history effects that sampled chunks cannot).
//!
//! ```no_run
//! use compress_estimate::{Estimator, backends::zstd::Zstd};
//!
//! // Seekable fast path: stratified sampling with parallel positioned reads.
//! let report = Estimator::new(Zstd::levels([3, 10, 15]))
//!     .estimate_path("database_dump.sql")?;
//! for row in report.levels() {
//!     println!("level {}: {:.2}x", row.level, row.ratio);
//! }
//!
//! // Streaming, Hash-style:
//! let mut est = Estimator::new(Zstd::default());
//! est.update(b"some bytes")?;
//! let report = est.finish()?;
//! # Ok::<(), compress_estimate::Error>(())
//! ```

pub mod backend;
pub mod backends;
pub mod sampler;
pub mod source;
pub mod stats;

use std::io;
use std::path::Path;

use backend::{Backend, Calibration, LevelEstimate, SpanSample, StratumSamples};
use sampler::{Read as PlannedRead, Reservoir, Sampler};
use source::Source;
use stats::Histogram;

/// Errors from estimation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("input is empty")]
    EmptyInput,
    #[error("estimator already used in streaming mode (update() was called)")]
    ModeConflict,
}

/// Default number of strata for seekable sampling.
const DEFAULT_STRATA: usize = 64;
/// Default sampled-chunk size.
const DEFAULT_CHUNK: usize = 512 * 1024;
/// Hard bounds for the adaptive sampling budget.
const MIN_BUDGET: u64 = 8 << 20;
const MAX_BUDGET: u64 = 64 << 20;
/// How much of a stream's prefix is retained (span fallback + short streams).
const STREAM_PREFIX_CAP: usize = 8 << 20;
/// How much of a stream's tail is retained: the anchor span for long streams,
/// since the tail is mature, representative data while the prefix holds the
/// cold-start region (headers, atypical beginnings).
const STREAM_TAIL_CAP: usize = 32 << 20;

/// The estimator front-end. Drives sampling, feeds the backend, assembles
/// the [`Report`].
pub struct Estimator<B: Backend> {
    backend: B,
    threads: usize,
    budget: Option<u64>,
    chunk_size: usize,
    n_strata: usize,
    accuracy: f64,
    anchor: bool,
    stream: Option<StreamState>,
}

struct StreamState {
    reservoir: Reservoir<Vec<u8>>,
    pending: Vec<u8>,
    /// Retained stream prefix: span for short streams, fallback otherwise.
    prefix: Vec<u8>,
    /// Retained stream tail (last ≤ STREAM_TAIL_CAP bytes): the anchor span
    /// for long streams, since it is mature, representative data.
    tail: Vec<u8>,
    hist: Histogram,
    total: u64,
}

impl<B: Backend> Estimator<B> {
    pub fn new(backend: B) -> Self {
        Estimator {
            backend,
            threads: 0,
            budget: None,
            chunk_size: DEFAULT_CHUNK,
            n_strata: DEFAULT_STRATA,
            accuracy: 0.02,
            anchor: true,
            stream: None,
        }
    }

    /// Worker threads for sampling/analysis (0 = all cores).
    pub fn threads(mut self, n: usize) -> Self {
        self.threads = n;
        self
    }

    /// Total sampling budget in bytes (default: ~size/1024, clamped to
    /// 8–64 MiB).
    pub fn budget(mut self, bytes: u64) -> Self {
        self.budget = Some(bytes);
        self
    }

    /// Sampled chunk size (default 512 KiB).
    pub fn chunk_size(mut self, bytes: usize) -> Self {
        self.chunk_size = bytes.max(4 * 1024);
        self
    }

    /// Number of strata for seekable sampling (default 64).
    pub fn strata(mut self, n: usize) -> Self {
        self.n_strata = n.clamp(1, 256);
        self
    }

    /// Early-stop target: relative 95% CI half-width on the primary
    /// quantity (default 0.02 = 2%).
    pub fn accuracy(mut self, rel_ci: f64) -> Self {
        self.accuracy = rel_ci.clamp(0.001, 0.5);
        self
    }

    /// Enable/disable real-compression anchor calibration (default on).
    pub fn anchor(mut self, on: bool) -> Self {
        self.anchor = on;
        self
    }

    fn budget_for(&self, total: u64) -> u64 {
        self.budget
            .unwrap_or((total / 1024).clamp(MIN_BUDGET, MAX_BUDGET))
    }

    fn pool(&self) -> rayon::ThreadPool {
        let mut b = rayon::ThreadPoolBuilder::new();
        if self.threads > 0 {
            b = b.num_threads(self.threads);
        }
        b.build().expect("failed to build thread pool")
    }

    /// Estimate from any seekable [`Source`] (stratified sampling).
    pub fn estimate_source<S: Source + ?Sized>(&self, src: &S) -> Result<Report<B::Level>, Error> {
        if self.stream.is_some() {
            return Err(Error::ModeConflict);
        }
        let total = src.len();
        if total == 0 {
            return Err(Error::EmptyInput);
        }
        let budget = self.budget_for(total);
        let pool = self.pool();

        // Inputs that fit the budget are covered exactly: deterministic
        // tiling chunks, no randomization, no early stop.
        let covering = total <= budget;
        let mut sampler = if covering {
            Sampler::cover(total, self.chunk_size)
        } else {
            Sampler::plan(total, budget, self.chunk_size, self.n_strata)
        };

        let mut collected: Vec<(PlannedRead, Vec<u8>, B::Stats)> = Vec::new();
        let mut round = if covering {
            sampler.cover_reads()
        } else {
            sampler.probe_reads()
        };
        loop {
            let results: Vec<(PlannedRead, Vec<u8>, B::Stats)> = pool.install(|| {
                use rayon::prelude::*;
                round
                    .par_iter()
                    .map(|read| -> io::Result<_> {
                        let mut buf = vec![0u8; read.len];
                        src.read_exact_at(read.offset, &mut buf)?;
                        let stats = self.backend.analyze_chunk(&buf);
                        Ok((*read, buf, stats))
                    })
                    .collect::<io::Result<Vec<_>>>()
            })?;
            for (read, buf, stats) in results {
                sampler.observe(read, self.backend.primary(&stats));
                collected.push((read, buf, stats));
            }
            if covering || sampler.converged(self.accuracy) || sampler.budget_exhausted() {
                break;
            }
            round = sampler.next_reads();
            if round.is_empty() {
                break;
            }
        }

        let span = self.acquire_span(src, total);
        self.assemble(total, &sampler, collected, &pool, span.as_deref())
    }

    /// Read one contiguous span of the source for anchor calibration,
    /// centered in the source. `None` when anchoring is off or the backend
    /// does not want a span.
    fn acquire_span<S: Source + ?Sized>(&self, src: &S, total: u64) -> Option<Vec<u8>> {
        if !self.anchor {
            return None;
        }
        let len = self.backend.anchor_span_len(total) as u64;
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        src.read_exact_at((total - len) / 2, &mut buf).ok()?;
        Some(buf)
    }

    /// Estimate from a file path (parallel positioned reads on one fd).
    pub fn estimate_path(&self, path: impl AsRef<Path>) -> Result<Report<B::Level>, Error> {
        let file = source::open_path(path.as_ref())?;
        self.estimate_source(&file)
    }

    /// Feed bytes in streaming mode (stdin, pipes, iterators…).
    ///
    /// Maintains a reservoir of chunks (uniform sample of the whole stream)
    /// plus an exact order-0 byte histogram over *all* bytes seen.
    pub fn update(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.stream.is_none() {
            let cap = (self
                .budget
                .unwrap_or(MAX_BUDGET)
                .div_ceil(self.chunk_size as u64)) as usize;
            self.stream = Some(StreamState {
                reservoir: Reservoir::new(cap.max(8)),
                pending: Vec::with_capacity(self.chunk_size),
                prefix: Vec::new(),
                tail: Vec::new(),
                hist: Histogram::default(),
                total: 0,
            });
        }
        let chunk = self.chunk_size;
        let s = self.stream.as_mut().unwrap();
        s.hist.add(bytes);
        s.total += bytes.len() as u64;
        if s.prefix.len() < STREAM_PREFIX_CAP {
            let take = (STREAM_PREFIX_CAP - s.prefix.len()).min(bytes.len());
            s.prefix.extend_from_slice(&bytes[..take]);
        }
        // Amortized trailing window: grow, then halve when over 2x the cap.
        s.tail.extend_from_slice(bytes);
        if s.tail.len() > 2 * STREAM_TAIL_CAP {
            let excess = s.tail.len() - STREAM_TAIL_CAP;
            s.tail.drain(..excess);
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let want = chunk - s.pending.len();
            let take = want.min(rest.len());
            s.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if s.pending.len() == chunk {
                s.reservoir.offer(std::mem::take(&mut s.pending));
            }
        }
        Ok(())
    }

    /// Finish streaming mode and produce the report.
    pub fn finish(mut self) -> Result<Report<B::Level>, Error> {
        let Some(s) = self.stream.take() else {
            return Err(Error::EmptyInput);
        };
        if s.total == 0 {
            return Err(Error::EmptyInput);
        }
        let pool = self.pool();
        // The partial tail chunk is offered to the reservoir like any other
        // item (Algorithm R), keeping the sample uniform over the stream.
        let mut s = s;
        if !s.pending.is_empty() {
            s.reservoir.offer(std::mem::take(&mut s.pending));
        }
        let chunks: Vec<Vec<u8>> = s.reservoir.slots;
        let stats: Vec<B::Stats> = pool.install(|| {
            use rayon::prelude::*;
            chunks
                .par_iter()
                .map(|c| self.backend.analyze_chunk(c))
                .collect()
        });

        // One stratum per retained chunk, weighted by byte length, so a
        // short tail chunk is not over-weighted next to full chunks.
        let total = s.total;
        let collected: Vec<(PlannedRead, Vec<u8>, B::Stats)> = chunks
            .into_iter()
            .zip(stats)
            .enumerate()
            .map(|(i, (buf, st))| {
                (
                    PlannedRead {
                        stratum: i,
                        offset: 0,
                        len: buf.len(),
                    },
                    buf,
                    st,
                )
            })
            .collect();
        let lens: Vec<usize> = collected.iter().map(|(r, ..)| r.len).collect();
        let sampler = Sampler::weighted(self.chunk_size, &lens);
        // Anchor span: the retained tail when it holds more than the prefix
        // (long streams: mature data beats the cold-start region); otherwise
        // the prefix, which for short streams is the whole input.
        let span_bytes = if s.tail.len() > s.prefix.len() {
            s.tail.as_slice()
        } else {
            s.prefix.as_slice()
        };
        let span = if self.anchor && !span_bytes.is_empty() {
            Some(span_bytes)
        } else {
            None
        };
        let mut report = self.assemble(total, &sampler, collected, &pool, span)?;
        // Exact streaming histogram beats the sampled one.
        report.hist = s.hist;
        Ok(report)
    }

    /// Aggregate collected samples into a report.
    fn assemble(
        &self,
        total: u64,
        sampler: &Sampler,
        collected: Vec<(PlannedRead, Vec<u8>, B::Stats)>,
        pool: &rayon::ThreadPool,
        span: Option<&[u8]>,
    ) -> Result<Report<B::Level>, Error> {
        // Group chunk refs per stratum; `collected` outlives the borrows.
        let mut per_stratum: Vec<Vec<(&[u8], &B::Stats)>> =
            (0..sampler.strata.len()).map(|_| Vec::new()).collect();
        let mut sampled_bytes = 0u64;
        let mut hist = Histogram::default();
        for (read, buf, st) in &collected {
            sampled_bytes += read.len as u64;
            per_stratum[read.stratum].push((buf.as_slice(), st));
            match self.backend.chunk_histogram(st) {
                Some(h) => hist.merge(h),
                None => hist.add(buf),
            }
        }

        let strata: Vec<StratumSamples<'_, B::Stats>> = per_stratum
            .into_iter()
            .zip(sampler.strata.iter())
            .map(|(samples, st)| StratumSamples {
                weight: st.weight,
                samples,
            })
            .collect();

        let cal = Calibration {
            enabled: self.anchor,
            span: span.map(|bytes| SpanSample {
                bytes,
                chunk_len: self.chunk_size,
            }),
        };
        // Throughput microbenchmarks are independent of the ratio estimates
        // (they need only the level list and resident chunks), so they run
        // concurrently with level estimation, whose span-anchor calibration
        // is the slowest phase.
        let level_ids = self.backend.levels();
        // Benchmark inputs: the contiguous anchor span first (honest window
        // and table sizes for levels ≤ 15), then the sampled chunks.
        let mut bench_inputs: Vec<&[u8]> = Vec::new();
        if let Some(bytes) = span {
            bench_inputs.push(bytes);
        }
        bench_inputs.extend(
            strata
                .iter()
                .flat_map(|st| st.samples.iter().map(|(c, _)| *c)),
        );
        let (mut levels, throughputs) = pool.install(|| {
            use rayon::prelude::*;
            rayon::join(
                || self.backend.estimate_levels(&strata, total, cal),
                || {
                    level_ids
                        .par_iter()
                        .map(|&l| self.backend.measure_throughput(&bench_inputs, l))
                        .collect::<Vec<_>>()
                },
            )
        });
        for (row, t) in levels.iter_mut().zip(throughputs) {
            row.throughput = t;
        }

        Ok(Report {
            total_len: total,
            sampled_bytes,
            hist,
            levels,
        })
    }
}

/// The final estimation report.
pub struct Report<L> {
    total_len: u64,
    sampled_bytes: u64,
    hist: Histogram,
    levels: Vec<LevelEstimate<L>>,
}

impl<L: Copy + Ord + std::fmt::Display> Report<L> {
    /// Source size in bytes.
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Bytes actually read from the source.
    pub fn sampled_bytes(&self) -> u64 {
        self.sampled_bytes
    }

    /// Order-0 entropy of the source, bits per byte (0..=8).
    pub fn entropy(&self) -> f64 {
        self.hist.entropy()
    }

    /// Human-readable data classification.
    pub fn data_class(&self) -> &'static str {
        classify(&self.hist)
    }

    /// Per-level estimates, ascending by level.
    pub fn levels(&self) -> &[LevelEstimate<L>] {
        &self.levels
    }

    /// Overall compressibility verdict, from the median requested level.
    pub fn verdict(&self) -> Verdict {
        let mid = &self.levels[self.levels.len() / 2];
        Verdict::from_ratio(mid.ratio)
    }

    /// The "best value" level: the knee of the cost/reduction curve —
    /// the point furthest above the straight line between the cheapest
    /// and the most expensive level (cost = relative wall time).
    pub fn best_value(&self) -> Option<&LevelEstimate<L>> {
        let rows = &self.levels;
        match rows.len() {
            0 => None,
            1 => rows.first(),
            2 => {
                let gain = rows[1].reduction - rows[0].reduction;
                Some(if gain >= 0.02 { &rows[1] } else { &rows[0] })
            }
            _ => {
                let tput = |r: &LevelEstimate<L>| r.throughput.unwrap_or(1.0);
                let t_max = rows.iter().map(&tput).fold(f64::MIN, f64::max);
                let cost: Vec<f64> = rows
                    .iter()
                    .map(|r| (t_max / tput(r).max(1.0)).ln().max(0.0))
                    .collect();
                let c_min = cost[0];
                let c_span = (cost[cost.len() - 1] - c_min).max(1e-9);
                let r_min = rows[0].reduction;
                let r_span = (rows[rows.len() - 1].reduction - r_min).abs().max(1e-9);
                let mut best = 0;
                let mut best_d = f64::MIN;
                for i in 0..rows.len() {
                    let x = (cost[i] - c_min) / c_span;
                    let y = (rows[i].reduction - r_min) / r_span;
                    // Distance above the diagonal line y = x.
                    let d = y - x;
                    if d > best_d {
                        best_d = d;
                        best = i;
                    }
                }
                Some(&rows[best])
            }
        }
    }
}

/// Overall compressibility verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Ratio ≥ 3.
    High,
    /// Ratio 1.75–3.
    Good,
    /// Ratio 1.15–1.75.
    Moderate,
    /// Ratio < 1.15.
    Low,
}

impl Verdict {
    pub fn from_ratio(ratio: f64) -> Self {
        if ratio >= 3.0 {
            Verdict::High
        } else if ratio >= 1.75 {
            Verdict::Good
        } else if ratio >= 1.15 {
            Verdict::Moderate
        } else {
            Verdict::Low
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verdict::High => "💡 High Compressibility",
            Verdict::Good => "✅ Good Compressibility",
            Verdict::Moderate => "➖ Moderate Compressibility",
            Verdict::Low => "⚠️  Low Compressibility — likely already compressed",
        }
    }
}

/// Data classification from the byte histogram.
fn classify(hist: &Histogram) -> &'static str {
    let h = hist.entropy();
    if h >= 7.7 {
        "High-entropy data (encrypted or already compressed)"
    } else if h <= 2.0 {
        "Highly repetitive data"
    } else if hist.nul_fraction() > 0.05 {
        "Binary data"
    } else if hist.printable_fraction() > 0.9 {
        "Structured text/data"
    } else {
        "Mixed binary data"
    }
}
