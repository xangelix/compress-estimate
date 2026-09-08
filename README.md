# compress-estimate

[![Crates.io](https://img.shields.io/crates/v/compress-estimate)](https://crates.io/crates/compress-estimate)
[![Docs.rs](https://docs.rs/compress-estimate/badge.svg)](https://docs.rs/compress-estimate)
[![MIT License](https://img.shields.io/crates/l/compress-estimate)](https://spdx.org/licenses/MIT)
[![Apache 2.0 License](https://img.shields.io/crates/l/compress-estimate)](https://spdx.org/licenses/Apache-2.0)

**Near-instant compression benchmarking.** Estimate the compression ratio and
speed of [zstd](https://github.com/facebook/zstd) at every level for a file or
stream — by strategically sampling ~0.1% of it, emulating the compressor's
parse on the samples, and self-calibrating against a few megabytes of real
compression. Constant time at any input scale: **~0.65 s whether the source is
2 GB or 200 GB**.

```
Target:       database_dump.sql (18.40 GB)
Entropy:      5.27 / 8.00 bits/byte (Structured text/data)
Verdict:      💡 High Compressibility
Level    Ratio     Projected Size     Reduction    Throughput
─────────────────────────────────────────────────────────────
   3    33.30×     553.47 MB         97.0%      ~2.5 GB/s  ★ Best Value
  10    40.01×     460.67 MB         97.5%      ~230 MB/s
  15    40.55×     454.54 MB         97.5%       ~95 MB/s
Projected Space Savings (Level 3): +17.4 GB
```

## Why not just compress a sample?

The naive approach — `head -c 1G dump.sql | zstd -15 | wc -c` — has three
problems, all measured during this project's development:

1. **Prefix bias.** Real files are not stationary. On our synthetic SQL
   corpus, the first 8 MB compressed 24.5× at level 3 while the middle
   compressed 32.5× — a ~30% error from sampling the wrong region.
   `compress-estimate` samples the *whole* source and calibrates out the
   regional structure.
2. **It costs real time per level.** Really compressing a 1 GB sample at level
   15 takes ~10 s (level 19: ~3.5 minutes); a multi-level table multiplies
   that. Here, every additional level is nearly free.
3. **It needs the whole prefix again** — impossible on a stream you can't
   replay. `compress-estimate` works on streams in one pass (reservoir
   sampling + a retained tail window).

## How it works: SCOPE

**SCOPE** (Sampled COst-model Parse Emulation) estimates, for each requested
level $\ell$, the compressed fraction $f_\ell$ of the source in three stages.

### 1. Strategic sampling

A seekable source of $N$ bytes is divided into strata; chunks of 512 KiB are
read with positioned reads (`pread`) in parallel, allocated across strata with
Neyman-style adaptive rounds until the 95% confidence interval on the primary
estimate is narrow (default: half-width 2%), subject to a sampling budget of
$\approx N/1024$ bytes clamped to 8–64 MiB. Sources smaller than the budget
are instead tiled deterministically for full coverage.

Per level, the modeled fractions aggregate as a stratified mean with per
stratum weights $w_s$, means $\bar{f}_s$ and variances $\hat\sigma_s^2$ over
$n_s$ chunks:

$$
\hat{f}_\ell = \sum_s w_s \bar{f}_s, \qquad
\operatorname{SE}\big(\hat{f}_\ell\big)^2 = \sum_s w_s^2 \frac{\hat\sigma_s^2}{n_s}.
$$

Streams get a uniform sample instead: reservoir sampling over fixed-size
chunks (each chunk weighted by its byte length, so a short tail chunk is not
over-weighted) plus an exact order-0 histogram over every byte seen, used for
the entropy readout $H = -\sum_b p_b \log_2 p_b$.

### 2. Parse emulation and an RFC 8878 cost model

Each sampled chunk is replayed through **faithful chunk-local ports of zstd's
own parse loops** — `fast`, `dfast`, and the `lazy` family (greedy through
btultra2, the latter approximated by the lazy loop with $2^{\text{searchLog}}$
chain attempts) — using the exact per-level parameters from zstd 1.5.7's
`clevels.h` (window/chain/hash logs, search log, min match, target length) and
the RFC 8878 three-slot repeat-offset machine. Nothing is entropy-coded; the
replay only *records* the sequences $(\text{litLen}, \text{offBase},
\text{matchLen})$ zstd would emit.

The cost model then prices each 128 KiB block exactly as the format would:
Huffman coding for literals (length-limited code lengths with Kraft repair),
FSE-coded literal-length/match-length/offset streams (per-code bits plus extra
bits), block and frame headers, and the raw-block fallback for incompressible
data:

$$
B_{\text{block}} = \min\Big(8n,\; B_{\text{lit}} + B_{\text{seq}} + B_{\text{hdr}}\Big),
\qquad
\tilde{f}_\ell = \frac{\sum_{\text{blocks}} B_{\text{block}}}{8n}.
$$

Given the *real* sequence stream (via `ZSTD_generateSequences`), this model is
within 0.2–0.9% of the true compressed size at every level; given the emulated
parse on a fixed history regime, sequence counts match zstd's to <1%.

### 3. Span anchoring

Chunk-local parsing cannot see whole-file history effects: deep windows find
far matches, and block statistics correlate across the file. Measured on real
zstd, these effects are large and — surprisingly — *strategy-dependent*: on
SQL-like data, deep history *helps* `fast` and `lazy` levels (+6–25%) but
*hurts* `dfast` (−30%: it takes far matches blindly at high offset-code
cost), and `btultra2` is position-sensitive and non-monotone.

So the estimator really compresses one contiguous **span** of the source
(depth $\approx 8 \times 2^{w_\ell}$ bytes, i.e. 8 window lengths, capped at
32 MiB — 4 MiB for the slow bt family), once per strategy family $F$ present
in the requested levels, and compares it to the tiled-cold model replay of the
same bytes:

$$
\delta_\ell = \ln \frac{f_\ell^{\text{real}}(\text{span})}{f_\ell^{\text{model}}(\text{span})},
\qquad
\hat{f}_\ell \;=\; \tilde{f}_\ell \cdot \exp\!\big(\hat\delta(\ell)\big),
$$

where $\hat\delta$ is piecewise-linear in $\ell$ *within each strategy family*
and flat outside the family's anchor range. This absorbs both residual model
bias and the warm-history regime the samples never see — and it reproduces
real zstd's non-monotonicity on small inputs (where, really, level 15 can be
worse than level 10) instead of smoothing it away.

### Throughput

Per level, a microbenchmark compresses long contiguous slices of the anchor
span (not cache-hot 512 KiB chunks, which make zstd clamp its match tables and
read up to 2× fast): one warm-regime pass, ~75 ms budget per level, all levels
in parallel with the anchor phase.

## Measured accuracy and speed

Against real single-threaded zstd 1.5.7 (`-T1`) on a synthetic SQL dump,
estimating levels 3/10/15 in one pass:

| Source | Level | Real ratio | Estimated | Error | Real time | Speedup |
|---|---|---|---|---|---|---|
| 700 KB | 1–19 | — | — | 0.0% at every level | — | — |
| 18 MB | 1–15 | — | — | ≤ 1.4% | — | — |
| 2 GB | 3 | 31.59× | 33.07× | +4.7% | 0.98 s | 1.5× |
| 2 GB | 10 | 40.72× | 40.07× | −1.6% | 7.98 s | 12× |
| 2 GB | 15 | 40.93× | 41.05× | +0.3% | 21.52 s | 32× |
| 18 GB | 3 | 32.17× | 33.30× | +3.5% | 9.31 s | 14× |
| 18 GB | 10 | 41.47× | 40.01× | −3.5% | 66.87 s | 103× |
| 18 GB | 15 | 41.71× | 40.55× | −2.8% | 190.94 s | 294× |

The point is the scaling law: real compression costs $T_{\text{real}}(N,
\ell) \approx N / v_\ell$, while estimation is $T_{\text{est}} \approx
\text{const}$ (~0.65 s, dominated by the anchor spans). Speedup therefore
grows linearly with source size — ~45× at 2 GB, ~410× at 18 GB for the
three-level benchmark, and >300× at any size once bt levels (≈5 MB/s in
reality) are included. Throughput estimates land within ~15% of measured
(e.g. level 15: estimated ~95 MB/s vs measured 100 MB/s).

## Usage

### CLI

```
compress-estimate [OPTIONS] <TARGET>   # TARGET = file path, or '-' for stdin
```

| Option | Meaning |
|---|---|
| `-l, --levels <LIST>` | Levels to estimate (comma-separated) [default: `3,10,15`] |
| `--all-levels` | Every level, 1 through 22 |
| `-b, --budget <B>` | Sampling budget: bytes (`64MB`, `500K`) or fraction of input (`0.1%`) |
| `--accuracy <PCT>` | Early-stop CI target, percent [default: 2] |
| `-t, --threads <N>` | Worker threads (0 = all cores) |
| `--no-anchor` | Pure cost model, no real compression at all (fastest, ~10–15% error) |
| `--stream` | Force streaming mode (reservoir sampling) |
| `--json` | Machine-readable report |
| `-v, --verbose` | Sampling detail and confidence intervals |

### Library

Seekable fast path (parallel positioned reads):

```rust
use compress_estimate::{Estimator, backends::zstd::Zstd};

let report = Estimator::new(Zstd::levels([3, 10, 15]))
    .threads(0)                      // 0 = all cores
    .budget(64 << 20)                // sampling budget, bytes
    .estimate_path("database_dump.sql")?;

println!("entropy: {:.2} bits/byte — {}", report.entropy(), report.data_class());
for row in report.levels() {
    println!("level {}: {:.2}x, ~{:.0} MB/s", row.level, row.ratio,
             row.throughput.unwrap_or(0.0) / 1e6);
}
println!("best value: level {}", report.best_value().unwrap().level);
```

Streaming, `Hash`-style (`update` / `finish`):

```rust
use compress_estimate::{Estimator, backends::zstd::Zstd};

let mut est = Estimator::new(Zstd::default());
while let Some(chunk) = next_chunk() {
    est.update(&chunk)?;
}
let report = est.finish()?;
```

### Writing a backend

The sampling/aggregation driver is algorithm-agnostic. A backend implements
`Backend` (`src/backend.rs`):

- `analyze_chunk(&[u8]) -> Stats` — the *cheap* per-chunk analysis (may not
  really compress);
- `estimate_levels(strata, total_len, Calibration)` — combine chunk stats
  into per-level estimates, optionally self-calibrating against
  `Calibration.span`, a contiguous span of the source;
- `measure_throughput(chunks, level)` — e.g. a microbenchmark;
- `anchor_span_len(total_len)` — how many contiguous bytes the backend wants
  for calibration.

The zstd backend lives in `src/backends/zstd/` (`lzparse.rs` parser ports,
`cost.rs` bit-cost model, `anchor.rs` span calibration, `levels.rs` the
`clevels.h` table). lz4 would be an easy second backend; brotli/xz are
possible but their context modeling makes the cost model a much bigger lift.

## Limitations

- **Levels 19–22 (btultra2)** are the weak spot: ~15–25% error on
  adversarial data. That strategy is position-sensitive and genuinely
  non-monotone on small inputs, and its 8–128 MiB windows make anchor spans
  expensive, so they're hard-capped at 4 MiB.
- **dfast levels (3–4)** carry ±5–10% noise on data whose compressibility
  drifts with position (a single centered span can't fully de-bias that).
- **Small files / fast levels**: below a few GB at levels 1–4, real
  compression is so fast that estimating is near-parity. Just compress.
- The parser ports track **zstd 1.5.7** internals (`clevels.h`); other zstd
  versions may shift level parameters.
- Ratio CIs cover *sampling* error only, not model bias.

## Development

```
cargo test                      # unit + accuracy tests
cargo clippy --all-targets      # clean
```

`examples/` contains the verification lab: `debug` (estimates vs real zstd on
synthetic SQL, with row-count argument), `warmth` (tiled vs whole-file
regimes), `real_parse` (replay vs `ZSTD_generateSequences` ground truth),
`span_isolate`, `stream_cmp`, `size_sweep`, `depth_sweep`, `parse_dump`,
`seq_dump`, and `genfile` (regenerate the test corpora used above).

Debug knobs: `CE_TRACE=1` dumps every emulated sequence during replay;
`CE_DEBUG_ANCHOR=1` prints per-anchor span fractions.
