//! zstd bit-cost model: given replayed parse statistics, compute how many
//! bits zstd would actually emit, following RFC 8878 framing semantics —
//! without entropy-coding anything.

use super::lzparse::ParseStats;

/// zstd block size upper bound.
/// (Framing overhead is accounted per block + frame header in
/// [`chunk_compressed_bits`].)
///
/// Length-limited (≤ `limit`) Huffman code lengths for a histogram, via the
/// classic clamp-then-Kraft-repair heuristic (what zstd's HUF builder
/// effectively achieves with its own optimizer).
pub fn huffman_lengths(counts: &[u64], limit: u32) -> Vec<u32> {
    let syms: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect();
    let mut lengths = vec![0u32; counts.len()];
    match syms.len() {
        0 => return lengths,
        1 => {
            lengths[syms[0]] = 1;
            return lengths;
        }
        _ => {}
    }

    // Standard Huffman: repeatedly merge two lightest nodes.
    // nodes: (weight, min index for tie-break stability, symbol or -1)
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Node {
        w: u64,
        order: u64,
        sym: i32,
    }
    impl Ord for Node {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            // Reverse for a min-heap.
            other
                .w
                .cmp(&self.w)
                .then_with(|| other.order.cmp(&self.order))
        }
    }
    impl PartialOrd for Node {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut heap = std::collections::BinaryHeap::with_capacity(syms.len() * 2);
    let mut children: Vec<(i32, i32)> = Vec::with_capacity(syms.len());
    let mut order = 0u64;
    for &s in &syms {
        heap.push(Node {
            w: counts[s],
            order,
            sym: s as i32,
        });
        order += 1;
    }
    while heap.len() > 1 {
        let a = heap.pop().unwrap();
        let b = heap.pop().unwrap();
        let idx = children.len() as i32;
        children.push((a.sym, b.sym));
        heap.push(Node {
            w: a.w + b.w,
            order,
            sym: !idx, // internal node: bitwise-NOT index
        });
        order += 1;
    }
    // Walk the tree assigning depths (root at depth 0).
    let root = heap.pop().unwrap();
    let mut stack = vec![(root.sym, 0u32)];
    while let Some((sym, depth)) = stack.pop() {
        if sym >= 0 {
            lengths[sym as usize] = depth;
        } else {
            let (a, b) = children[(!sym) as usize];
            stack.push((a, depth + 1));
            stack.push((b, depth + 1));
        }
    }

    // Enforce the length limit: clamp, then repair the Kraft sum by
    // lengthening the cheapest (most frequent) short codes.
    let mut over = false;
    for &s in &syms {
        if lengths[s] > limit {
            lengths[s] = limit;
            over = true;
        }
    }
    if over {
        // Kraft sum in units of 2^-limit: Σ 2^(limit - len).
        let kraft =
            |lengths: &[u32]| -> i64 { syms.iter().map(|&s| 1i64 << (limit - lengths[s])).sum() };
        let mut k = kraft(&lengths);
        let target = 1i64 << limit;
        while k > target {
            // Find the shortest length below the limit that can be lengthened.
            let mut best: Option<usize> = None;
            for &s in &syms {
                let l = lengths[s];
                if l < limit && best.is_none_or(|b| lengths[b] > l) {
                    best = Some(s);
                }
            }
            match best {
                Some(s) => {
                    lengths[s] += 1;
                    k -= 1i64 << (limit - lengths[s]);
                }
                None => break,
            }
        }
    }
    lengths
}

/// Cost in bits of Huffman-coding `total` symbols with the given histogram,
/// including an estimate of the table-description overhead.
fn huffman_cost(counts: &[u64; 256], _total: u64) -> f64 {
    let lengths = huffman_lengths(counts, 11);
    let mut bits = 0.0;
    let mut distinct = 0u32;
    for (s, &c) in counts.iter().enumerate() {
        if c > 0 {
            bits += c as f64 * lengths[s] as f64;
            distinct += 1;
        }
    }
    if distinct <= 1 {
        return 0.0; // handled as RLE by the caller
    }
    // Table description: header byte + ~4.5 bits per symbol weight.
    bits + 8.0 + 4.5 * distinct as f64
}

/// Entropy (bits) of an empirical distribution, 0 for empty.
fn entropy_of(counts: &[u32]) -> f64 {
    let total: u64 = counts.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let mut h = 0.0;
    for &c in counts {
        if c > 0 {
            let p = c as f64 / n;
            h -= p * p.log2();
        }
    }
    h
}

/// Predefined FSE distributions from RFC 8878 (normalized, `-1` = "less
/// than one" ≈ half a slot). Accuracy logs: LL 6, ML 6, OF 5.
static LL_PREDEF: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
static ML_PREDEF: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
static OF_PREDEF: [i32; 32] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1,
    -1, -1,
];

/// Cost per code under a predefined distribution, in bits.
fn predefined_costs(table: &[i32], acc_log: u32) -> Vec<f64> {
    let slots = (1u64 << acc_log) as f64;
    table
        .iter()
        .map(|&v| {
            let p = if v < 0 { 0.5 } else { v.max(1) as f64 } / slots;
            -p.log2()
        })
        .collect()
}

/// Cost of coding one sequence-code stream (one of litlen / matchlen /
/// offset), taking the minimum over zstd's per-stream modes:
/// predefined table, built FSE table, RLE.
fn sequence_stream_cost(counts: &[u32], predef: &[i32], acc_log: u32, nb_seq: u32) -> f64 {
    let total: u64 = counts.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return 0.0;
    }
    let distinct = counts.iter().filter(|&&c| c > 0).count();

    // Predefined table.
    let costs = predefined_costs(predef, acc_log);
    let mut predefined = 0.0;
    for (c, &n) in counts.iter().enumerate() {
        if n > 0 {
            predefined += n as f64 * costs[c.min(costs.len() - 1)];
        }
    }

    // Built FSE table: entropy + normalization loss + table description.
    let norm_loss = 0.03 * total as f64; // typical H(t||p) gap after normalization
    let table_desc = 16.0 + 6.0 * distinct as f64;
    let built = entropy_of(counts) * total as f64 + norm_loss + table_desc;

    // RLE: one symbol only.
    let rle = if distinct == 1 { 8.0 } else { f64::INFINITY };

    let _ = nb_seq;
    predefined.min(built).min(rle)
}

/// One 128 KiB block's compressed size in bits (block header + literals +
/// sequences), with zstd's stored-block fallback for incompressible data.
fn block_bits(stats: &ParseStats) -> f64 {
    let lit_total = stats.total_lit_len;

    // Literals section: min over Raw / RLE / Huffman(1 stream) like zstd.
    let lit_bits = if lit_total == 0 {
        8.0 // empty literals header
    } else {
        let raw = lit_total as f64 * 8.0 + 16.0;
        let rle = if stats.literals.distinct() == 1 {
            24.0
        } else {
            f64::INFINITY
        };
        let huff = huffman_cost(&stats.literals.counts, lit_total) + 24.0;
        raw.min(rle).min(huff)
    };

    // Sequences section.
    let seq_bits = if stats.nb_seq == 0 {
        8.0 // one-byte nbSeq=0 header, no section
    } else {
        let header = 32.0; // nbSeq (≤3B) + mode byte
        let ll = sequence_stream_cost(&stats.ll_codes, &LL_PREDEF, 6, stats.nb_seq);
        let ml = sequence_stream_cost(&stats.ml_codes, &ML_PREDEF, 6, stats.nb_seq);
        let of = sequence_stream_cost(&stats.off_codes, &OF_PREDEF, 5, stats.nb_seq);
        header + ll + ml + of + stats.extra_bits as f64
    };

    let modeled = lit_bits + seq_bits;
    // zstd stores incompressible blocks raw.
    modeled.min(stats.chunk_len as f64 * 8.0)
}

/// Total modeled compressed size of a chunk (frame header + blocks), in bits.
pub fn chunk_compressed_bits(blocks: &[ParseStats]) -> f64 {
    let chunk_len: usize = blocks.iter().map(|b| b.chunk_len).sum();
    if chunk_len == 0 {
        return 0.0;
    }
    let frame_header = 14.0 * 8.0;
    let mut total = frame_header;
    for b in blocks {
        total += 3.0 * 8.0 + block_bits(b);
    }
    total
}

/// Modeled compressed fraction of a chunk (compressed / original), ≤ ~1.
pub fn chunk_fraction(blocks: &[ParseStats]) -> f64 {
    let chunk_len: usize = blocks.iter().map(|b| b.chunk_len).sum();
    if chunk_len == 0 {
        return 1.0;
    }
    chunk_compressed_bits(blocks) / (chunk_len as f64 * 8.0)
}

/// Debug breakdown: summed (framing, literals, sequences) in bits.
pub fn cost_breakdown(blocks: &[ParseStats]) -> (f64, f64, f64) {
    let (f, l, s) = blocks
        .iter()
        .map(cost_breakdown_full)
        .fold((0.0, 0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2));
    (f, l, s)
}

/// Full debug breakdown for one block: (framing, lit, ll, ml, of, extra) bits.
pub fn cost_breakdown_full(stats: &ParseStats) -> (f64, f64, f64, f64, f64, f64) {
    let lit_total = stats.total_lit_len;
    let lit_bits = if lit_total == 0 {
        8.0
    } else {
        let raw = lit_total as f64 * 8.0 + 16.0;
        let rle = if stats.literals.distinct() == 1 {
            24.0
        } else {
            f64::INFINITY
        };
        let huff = huffman_cost(&stats.literals.counts, lit_total) + 24.0;
        raw.min(rle).min(huff)
    };
    if stats.nb_seq == 0 {
        return (24.0, lit_bits, 0.0, 0.0, 0.0, 8.0);
    }
    let header = 32.0;
    let ll = sequence_stream_cost(&stats.ll_codes, &LL_PREDEF, 6, stats.nb_seq);
    let ml = sequence_stream_cost(&stats.ml_codes, &ML_PREDEF, 6, stats.nb_seq);
    let of = sequence_stream_cost(&stats.off_codes, &OF_PREDEF, 5, stats.nb_seq);
    (24.0, lit_bits, ll, ml, of, stats.extra_bits as f64 + header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::zstd::levels::params_for;
    use crate::backends::zstd::lzparse::{ParseStats, ParseTables, replay_blocks};

    fn model_ratio(data: &[u8], level: i32) -> f64 {
        let mut tables = ParseTables::default();
        let mut blocks: Vec<ParseStats> = Vec::new();
        replay_blocks(data, &params_for(level), &mut blocks, &mut tables);
        1.0 / chunk_fraction(&blocks)
    }

    #[test]
    fn zeros_compress_hugely() {
        let data = vec![0u8; 256 * 1024];
        let r = model_ratio(&data, 3);
        assert!(r > 100.0, "ratio {r}");
    }

    #[test]
    fn random_is_stored_raw() {
        let mut x = 0xdead_beef_cafe_f00du64;
        let data: Vec<u8> = (0..256 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u8
            })
            .collect();
        let r = model_ratio(&data, 3);
        assert!((0.99..=1.001).contains(&r), "ratio {r}");
    }

    #[test]
    fn repetitive_text_compresses_well() {
        let data: Vec<u8> = b"the quick brown fox jumps over the lazy dog. ".repeat(8192);
        let r = model_ratio(&data, 3);
        assert!(r > 5.0, "ratio {r}");
    }

    #[test]
    fn huffman_respects_limit() {
        // Highly skewed distribution that would exceed 11 bits without limit.
        let mut counts = [0u64; 256];
        counts[0] = 1 << 20;
        for c in counts.iter_mut().skip(1) {
            *c = 1;
        }
        let lengths = huffman_lengths(&counts, 11);
        assert!(*lengths.iter().max().unwrap() <= 11);
        // Kraft sum must not exceed 1.
        let kraft: f64 = (0..256)
            .filter(|&i| counts[i] > 0)
            .map(|i| 2f64.powi(-(lengths[i] as i32)))
            .sum();
        assert!(kraft <= 1.0 + 1e-9, "kraft {kraft}");
    }
}
