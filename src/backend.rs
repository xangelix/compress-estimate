//! The backend extension point: one implementation per compression algorithm.

use std::fmt::Display;

/// Samples belonging to one stratum of the source, with the stratum's weight
/// (fraction of the total source it covers).
pub struct StratumSamples<'a, S> {
    pub weight: f64,
    /// `(chunk bytes, per-chunk analysis)` pairs.
    pub samples: Vec<(&'a [u8], &'a S)>,
}

/// Per-level estimate produced by a backend.
#[derive(Debug, Clone)]
pub struct LevelEstimate<L> {
    pub level: L,
    /// Estimated compression ratio (uncompressed / compressed), > 1.
    pub ratio: f64,
    /// 95% CI half-width on `ratio` (relative sampling error only).
    pub ratio_ci: f64,
    /// Projected compressed size of the whole source, bytes.
    pub projected_size: u64,
    /// Fraction of source removed by compression, 0..1.
    pub reduction: f64,
    /// Estimated single-thread compression throughput, bytes/sec.
    pub throughput: Option<f64>,
}

/// A contiguous span of the source, really compressed by the backend to
/// measure whole-file history effects that chunk-local analysis cannot see.
pub struct SpanSample<'a> {
    pub bytes: &'a [u8],
    /// Tile size for the backend's cold replay of the span; should match the
    /// sampled-chunk size so the span delta compares like regimes.
    pub chunk_len: usize,
}

/// Calibration inputs handed to [`Backend::estimate_levels`].
pub struct Calibration<'a> {
    /// Whether the backend may really compress some bytes to calibrate.
    pub enabled: bool,
    /// A contiguous span of the source, when one could be acquired.
    pub span: Option<SpanSample<'a>>,
}

impl Calibration<'_> {
    pub fn disabled() -> Self {
        Calibration {
            enabled: false,
            span: None,
        }
    }
}

/// A compression-algorithm backend.
///
/// The design contract: [`Backend::analyze_chunk`] must be *cheap* — it may
/// not compress the chunk with the real algorithm. All expensive machinery
/// (match finding simulation, cost modeling) lives behind this interface so
/// the sampling/aggregation driver stays algorithm-agnostic.
pub trait Backend: Send + Sync {
    /// Level type (e.g. `i32` for zstd levels 1..=22).
    type Level: Copy + Ord + Display + Send + Sync;
    /// Per-chunk analysis output.
    type Stats: Send + Sync + Clone;

    /// Levels to estimate, ascending.
    fn levels(&self) -> Vec<Self::Level>;

    /// Analyze one sampled chunk without compressing it.
    fn analyze_chunk(&self, chunk: &[u8]) -> Self::Stats;

    /// The single scalar used to drive sample allocation and early stopping
    /// (typically the model ratio at the median requested level).
    fn primary(&self, stats: &Self::Stats) -> f64;

    /// Combine per-chunk stats into per-level estimates for a source of
    /// `total_len` bytes. `cal` carries the real-compression calibration
    /// inputs: whether calibration is allowed, and a contiguous span of the
    /// source to compress if one could be acquired.
    fn estimate_levels(
        &self,
        strata: &[StratumSamples<'_, Self::Stats>],
        total_len: u64,
        cal: Calibration<'_>,
    ) -> Vec<LevelEstimate<Self::Level>>;

    /// How many contiguous source bytes the backend wants for span
    /// calibration (0 = no span anchoring). The estimator clamps this to the
    /// source size and, for streams, to the retained prefix.
    fn anchor_span_len(&self, _total_len: u64) -> usize {
        0
    }

    /// Estimate single-thread compression throughput (bytes/sec) at `level`,
    /// e.g. via a microbenchmark on resident chunks. `None` = unknown.
    fn measure_throughput(&self, _chunks: &[&[u8]], _level: Self::Level) -> Option<f64> {
        None
    }

    /// Byte histogram of an analyzed chunk, if the backend tracks one; the
    /// estimator uses it for source-wide entropy. `None` = recompute.
    fn chunk_histogram<'a>(
        &'a self,
        _stats: &'a Self::Stats,
    ) -> Option<&'a crate::stats::Histogram> {
        None
    }
}
