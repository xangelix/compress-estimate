//! The SCOPE parse emulator: faithful, chunk-local ports of zstd's three
//! parser loops (`zstd_fast.c`, `zstd_double_fast.c`, `zstd_lazy.c` hash-chain
//! mode, with bt* levels mapped onto the lazy loop at `1 << searchLog`
//! attempts). Each level's parse is replayed over the sample with zstd's own
//! control flow — same hash functions, same insertion points, same repcode
//! state machine, same gain heuristics — but with all entropy coding replaced
//! by per-128KiB-block statistics that feed the RFC 8878 cost model. No data
//! is actually compressed.
//!
//! The one approximation: bt* strategies really use binary-tree match finding
//! (and btopt/btultra* an optimal parser); the lazy loop with deep chains
//! approximates them, and the anchor calibration absorbs residual bias.

use super::levels::{LevelParams, Strategy};
use crate::stats::Histogram;

/// zstd's block size: the compressor entropy-codes each 128 KiB block with
/// fresh Huffman/FSE tables (block-local distributions are far more
/// concentrated than chunk-global ones), so we accumulate parse statistics
/// per block. The *parse itself* runs continuously across block boundaries —
/// window, tables and repcodes persist — exactly like real zstd.
pub const ZSTD_BLOCK: usize = 128 * 1024;

/// zstd stops searching 8 bytes before the end (`ilimit = iend - 8`).
const TAIL_MARGIN: usize = 8;
/// kSearchStrength: lazy miss step is `litrun >> 8`; fast's step increment is
/// `1 << (8-1)`, dfast's `1 << 8`.
const SEARCH_STRENGTH: u32 = 8;
/// Once the lazy miss step exceeds this, insertion switches to sparse mode
/// ("lazy skipping"): only searched positions enter the tables.
const LAZY_SKIPPING_STEP: usize = 8;
/// Match lengths saturate in the cost tables; past 16 KiB the marginal cost
/// is ~1 bit per doubling, so we cap recorded lengths (the anchor absorbs
/// the residual error on pathological data).
const MAX_RECORDED_LEN: usize = 16384;

const PRIME4: u32 = 2654435761;
const PRIME5: u64 = 889523592379;
const PRIME6: u64 = 227718039650203;
const PRIME7: u64 = 58295818150454627;
const PRIME8: u64 = 0xCF1B_BCDC_B7A5_6463;

#[inline(always)]
fn read32(chunk: &[u8], p: usize) -> u32 {
    debug_assert!(p + 4 <= chunk.len());
    u32::from_le_bytes(chunk[p..p + 4].try_into().unwrap())
}

#[inline(always)]
fn read64(chunk: &[u8], p: usize) -> u64 {
    debug_assert!(p + 8 <= chunk.len());
    u64::from_le_bytes(chunk[p..p + 8].try_into().unwrap())
}

/// zstd's `ZSTD_hashPtr`: hash the `mls` leading bytes at `pos`.
/// Requires `pos + 8 <= chunk.len()` (guaranteed by the ilimit margins).
#[inline(always)]
fn zhash(chunk: &[u8], pos: usize, mls: u32, hlog: u32) -> usize {
    match mls {
        5 => ((read64(chunk, pos).wrapping_shl(24).wrapping_mul(PRIME5)) >> (64 - hlog)) as usize,
        6 => ((read64(chunk, pos).wrapping_shl(16).wrapping_mul(PRIME6)) >> (64 - hlog)) as usize,
        7 => ((read64(chunk, pos).wrapping_shl(8).wrapping_mul(PRIME7)) >> (64 - hlog)) as usize,
        8 => ((read64(chunk, pos).wrapping_mul(PRIME8)) >> (64 - hlog)) as usize,
        _ => (read32(chunk, pos).wrapping_mul(PRIME4) >> (32 - hlog)) as usize,
    }
}

/// Common prefix length of `chunk[a..]` and `chunk[b..]`, capped so that
/// callers may always `read32(b + len)` afterwards without leaving the
/// buffer. Loses at most one byte of match at the absolute chunk end.
#[inline(always)]
fn count(chunk: &[u8], a: usize, b: usize) -> usize {
    debug_assert!(a < b && b < chunk.len());
    let max = (chunk.len() - b).saturating_sub(1).min(MAX_RECORDED_LEN);
    let mut n = 0;
    while n + 8 <= max {
        let x = read64(chunk, a + n);
        let y = read64(chunk, b + n);
        let d = x ^ y;
        if d != 0 {
            return n + (d.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    while n < max && chunk[a + n] == chunk[b + n] {
        n += 1;
    }
    n
}

/// `ZSTD_highbit32` (0 for x ≤ 1).
#[inline(always)]
fn hb32(x: u32) -> u32 {
    31 - x.leading_zeros().min(31)
}

/// Cheap data features from a single dense head-only probe pass.
#[derive(Clone)]
pub struct Features {
    pub hist: Histogram,
    /// Fraction of positions whose 4-byte hash hits a verifying match.
    pub match_density: f64,
    pub len: usize,
}

/// Reusable buffer for [`FeatureScratch::scan_features`].
#[derive(Default)]
pub struct FeatureScratch {
    head: Vec<i32>,
}

impl FeatureScratch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scan_features(&mut self, chunk: &[u8]) -> Features {
        const FLOG: u32 = 17;
        let n = chunk.len();
        let mut hist = Histogram::default();
        hist.add(chunk);
        let mut hits = 0u64;
        if n >= 8 {
            self.head.clear();
            self.head.resize(1 << FLOG, -1);
            let last = n - 8;
            for i in 0..=last {
                let h = zhash(chunk, i, 4, FLOG);
                let cand = self.head[h];
                self.head[h] = i as i32;
                if cand >= 0 && read32(chunk, cand as usize) == read32(chunk, i) {
                    hits += 1;
                }
            }
        }
        Features {
            hist,
            match_density: if n >= 8 {
                hits as f64 / (n - 7) as f64
            } else {
                0.0
            },
            len: n,
        }
    }
}

/// Raw parse statistics for one level, accumulated during replay. All
/// histograms feed the cost model; nothing here is entropy-coded.
#[derive(Clone)]
pub struct ParseStats {
    /// Literal byte histogram (literals only, per this level's parse).
    pub literals: Histogram,
    /// Literal-length code counts (codes 0..=35).
    pub ll_codes: [u32; 36],
    /// Match-length code counts (codes 0..=52).
    pub ml_codes: [u32; 53],
    /// Offset code counts (codes 0..=31).
    pub off_codes: [u32; 32],
    /// Sum of extra bits carried by litlen/matchlen/offset codes.
    pub extra_bits: u64,
    pub nb_seq: u32,
    pub total_lit_len: u64,
    pub total_match_len: u64,
    pub chunk_len: usize,
}

impl ParseStats {
    pub fn new(chunk_len: usize) -> Self {
        ParseStats {
            literals: Histogram::default(),
            ll_codes: [0; 36],
            ml_codes: [0; 53],
            off_codes: [0; 32],
            extra_bits: 0,
            nb_seq: 0,
            total_lit_len: 0,
            total_match_len: 0,
            chunk_len,
        }
    }
}

/// Reusable per-level parse tables. Sizes adapt to the level's hash/chain
/// logs, clamped to the chunk size the way zstd clamps its tables to the
/// input size; only the head tables need clearing between replays.
#[derive(Default)]
pub struct ParseTables {
    /// Head table (fast: only table; dfast: small; lazy: hash heads).
    head: Vec<i32>,
    /// dfast's long (8-byte) table.
    head2: Vec<i32>,
    /// Lazy chain ring (indexed by `pos & (2^chain_log - 1)`).
    prev: Vec<i32>,
    /// Lazy catch-up insertion cursor.
    next_to_update: usize,
}

impl ParseTables {
    fn reset_head(head: &mut Vec<i32>, log: u32, n: usize) {
        // Like zstd's cParam clamping for small inputs, the table never
        // needs to be much larger than the chunk itself.
        let cap = (usize::BITS - n.max(64).leading_zeros()) + 1;
        let log = log.min(cap);
        head.clear();
        head.resize(1 << log, -1);
    }
}

/// Replay one level's parse over `chunk`, emitting one [`ParseStats`] per
/// 128 KiB block.
pub fn replay_blocks(
    chunk: &[u8],
    params: &LevelParams,
    blocks: &mut Vec<ParseStats>,
    tables: &mut ParseTables,
) {
    blocks.clear();
    blocks.push(ParseStats::new(0));
    let mut p = Parser {
        chunk,
        params,
        rep: [1, 4, 8], // RFC 8878 repeat-offset initial history
        lit_start: 0,
        block_start: 0,
        blocks,
        tables,
    };
    let n = chunk.len();
    if n > TAIL_MARGIN + 8 {
        match params.strategy {
            Strategy::Fast => p.replay_fast(),
            Strategy::Dfast => p.replay_dfast(),
            _ => p.replay_lazy(),
        }
    }
    p.finish();
}

/// The parse emulator state: chunk, repcode history, block-splitting stats,
/// and the level's match-finding tables.
struct Parser<'a> {
    chunk: &'a [u8],
    params: &'a LevelParams,
    rep: [u32; 3],
    /// Anchor: start of the pending literal run (zstd's `anchor`).
    lit_start: usize,
    block_start: usize,
    blocks: &'a mut Vec<ParseStats>,
    tables: &'a mut ParseTables,
}

impl Parser<'_> {
    #[inline(always)]
    fn n(&self) -> usize {
        self.chunk.len()
    }

    /// `ilimit = iend - 8`: no new sequence may start past this.
    #[inline(always)]
    fn ilimit(&self) -> usize {
        self.n() - TAIL_MARGIN
    }

    /// Effective hash log, clamped to the chunk size.
    #[inline(always)]
    fn hlog(&self) -> u32 {
        let cap = (usize::BITS - self.n().max(64).leading_zeros()) + 1;
        self.params.hash_log.min(cap)
    }

    /// Effective chain log, clamped to the chunk size.
    #[inline(always)]
    fn clog(&self) -> u32 {
        let cap = usize::BITS - self.n().max(64).leading_zeros();
        self.params.chain_log.min(cap)
    }

    /// Effective window: never needs to exceed the chunk length.
    #[inline(always)]
    fn max_dist(&self) -> usize {
        (1usize << self.params.window_log).min(self.n())
    }

    /// Close the current block's stats at its 128 KiB boundary. The parse
    /// itself (tables, repcodes) continues unaffected. The final block (which
    /// ends at `n`) is left open; [`Parser::finish`] accounts for it.
    fn close_blocks_up_to(&mut self, pos: usize) {
        loop {
            let boundary = (self.block_start + ZSTD_BLOCK).min(self.n());
            if pos < boundary || boundary == self.n() {
                return;
            }
            let cur = self.blocks.last_mut().unwrap();
            if self.lit_start < boundary {
                cur.literals.add(&self.chunk[self.lit_start..boundary]);
                cur.total_lit_len += (boundary - self.lit_start) as u64;
            }
            cur.chunk_len = boundary - self.block_start;
            self.lit_start = self.lit_start.max(boundary);
            self.block_start = boundary;
            self.blocks.push(ParseStats::new(0));
        }
    }

    /// Store one sequence: literals `[lit_start, start)` plus a match of
    /// `mlen` at offset `off`. Runs the RFC 8878 repeat-offset state machine
    /// (mirroring how zstd's parser-maintained `rep[0..1]` interact with the
    /// wire format's 3-slot history) and accumulates code statistics.
    fn store(&mut self, start: usize, mlen: usize, off: u32) {
        debug_assert!(
            start >= self.lit_start,
            "start={start} lit_start={}",
            self.lit_start
        );
        let litlen = start - self.lit_start;
        if std::env::var_os("CE_TRACE").is_some() {
            eprintln!(
                "seq @{start} lit={litlen} ml={mlen} off={off} rep={:?}",
                self.rep
            );
        }
        let stats = self.blocks.last_mut().unwrap();
        stats.literals.add(&self.chunk[self.lit_start..start]);
        stats.total_lit_len += litlen as u64;

        let rep = &mut self.rep;
        let ov: u32;
        if litlen != 0 {
            if off == rep[0] {
                ov = 1;
            } else if off == rep[1] {
                ov = 2;
                rep.swap(0, 1);
            } else if off == rep[2] {
                ov = 3;
                *rep = [rep[2], rep[0], rep[1]];
            } else {
                ov = off + 3;
                *rep = [off, rep[0], rep[1]];
            }
        } else if off == rep[1] {
            ov = 1;
            rep.swap(0, 1);
        } else if off == rep[2] {
            ov = 2;
            *rep = [rep[2], rep[0], rep[1]];
        } else if off == rep[0].saturating_sub(1) && off != 0 {
            ov = 3;
            rep[0] = rep[0].saturating_sub(1);
        } else {
            ov = off + 3;
            *rep = [off, rep[0], rep[1]];
        }

        let (llc, lle) = ll_code(litlen as u32);
        let (mlc, mle) = ml_code(mlen as u32);
        let oc = (31 - ov.leading_zeros()) as usize; // highbit of offset value
        stats.ll_codes[llc] += 1;
        stats.ml_codes[mlc] += 1;
        stats.off_codes[oc.min(31)] += 1;
        stats.extra_bits += (lle + mle) as u64 + oc.min(31) as u64;
        stats.nb_seq += 1;
        stats.total_match_len += mlen as u64;

        self.lit_start = start + mlen;
    }

    /// Flush trailing literals and fix block lengths at end of input.
    fn finish(&mut self) {
        let n = self.n();
        self.close_blocks_up_to(n);
        let cur = self.blocks.last_mut().unwrap();
        if self.lit_start < n {
            cur.literals.add(&self.chunk[self.lit_start..n]);
            cur.total_lit_len += (n - self.lit_start) as u64;
        }
        cur.chunk_len = n - self.block_start;
    }

    /// Repcode match length at `pos` for offset `r` (0 if none). Repcodes
    /// bypass the tables entirely, but must respect the window: zstd zeroes
    /// rep offsets beyond `windowLow` at each block start; probing here is
    /// equivalent for a continuous parse.
    #[inline(always)]
    fn rep_probe(&self, pos: usize, r: u32) -> usize {
        let r = r as usize;
        if r > 0 && r <= pos && r <= self.max_dist() && pos + 4 <= self.n() {
            if read32(self.chunk, pos - r) == read32(self.chunk, pos) {
                count(self.chunk, pos - r, pos)
            } else {
                0
            }
        } else {
            0
        }
    }

    // ------------------------------------------------------------------
    // zstd_fast.c
    // ------------------------------------------------------------------
    fn replay_fast(&mut self) {
        let n = self.n();
        let ilimit = self.ilimit();
        let step_size =
            (self.params.target_len as usize) + (self.params.target_len == 0) as usize + 1;
        let k_step_incr = 1usize << (SEARCH_STRENGTH - 1);
        let mls = self.params.min_match.clamp(4, 8);
        let hlog = self.hlog();
        ParseTables::reset_head(&mut self.tables.head, hlog, n);
        let max_dist = self.max_dist();

        let mut ip0 = 1usize; // ip0 += (ip0 == prefixStart)
        'start: loop {
            let mut step = step_size;
            let mut next_step = ip0 + k_step_incr;
            let mut ip1 = ip0 + 1;
            let mut ip2 = ip0 + step;
            let mut ip3 = ip2 + 1;
            if ip3 >= ilimit {
                break;
            }
            let mut hash0 = zhash(self.chunk, ip0, mls, hlog);
            let mut hash1 = zhash(self.chunk, ip1, mls, hlog);
            let mut match_idx = self.tables.head[hash0];
            // Search loop: yields (match_pos, match_len, offset, matched_ip0).
            let (mpos0, mlen0, off0, cur0) = 'search: loop {
                // Repcode check at ip2 (rep[0]).
                let r0 = self.rep[0];
                if self.rep_probe(ip2, r0) >= 4 {
                    let cur = ip0;
                    self.tables.head[hash1] = ip1 as i32;
                    let back = (ip2 > r0 as usize
                        && self.chunk[ip2 - 1] == self.chunk[ip2 - r0 as usize - 1])
                        as usize;
                    let s = ip2 - back;
                    let ml = back + 4 + count(self.chunk, ip2 - r0 as usize + 4, ip2 + 4);
                    self.close_blocks_up_to(s);
                    self.store(s, ml, r0);
                    ip0 = s + ml;
                    self.fast_after_match(&mut ip0, cur, ilimit, mls, hlog);
                    continue 'start;
                }
                // Insert ip0 and test its candidate.
                self.tables.head[hash0] = ip0 as i32;
                if match_idx >= 0 {
                    let m = match_idx as usize;
                    if ip0 - m <= max_dist && read32(self.chunk, m) == read32(self.chunk, ip0) {
                        self.tables.head[hash1] = ip1 as i32;
                        let ml = 4 + count(self.chunk, m + 4, ip0 + 4);
                        break 'search (m, ml, (ip0 - m) as u32, ip0);
                    }
                }
                match_idx = self.tables.head[hash1];
                hash0 = hash1;
                hash1 = zhash(self.chunk, ip2, mls, hlog);
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip3;

                self.tables.head[hash0] = ip0 as i32;
                if match_idx >= 0 {
                    let m = match_idx as usize;
                    if ip0 - m <= max_dist && read32(self.chunk, m) == read32(self.chunk, ip0) {
                        if step <= 4 {
                            self.tables.head[hash1] = ip1 as i32;
                        }
                        let ml = 4 + count(self.chunk, m + 4, ip0 + 4);
                        break 'search (m, ml, (ip0 - m) as u32, ip0);
                    }
                }
                match_idx = self.tables.head[hash1];
                hash0 = hash1;
                hash1 = zhash(self.chunk, ip2, mls, hlog);
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip0 + step;
                ip3 = ip1 + step;
                if ip2 >= next_step {
                    step += 1;
                    next_step += k_step_incr;
                }
                if ip3 >= ilimit {
                    break 'start;
                }
            };
            // _offset: full backward extension for offset matches.
            let mut start = cur0;
            let mut mpos = mpos0;
            let mut mlen = mlen0;
            while start > self.lit_start
                && mpos >= 1
                && self.chunk[start - 1] == self.chunk[mpos - 1]
            {
                start -= 1;
                mpos -= 1;
                mlen += 1;
            }
            self.close_blocks_up_to(start);
            self.store(start, mlen, off0);
            ip0 = start + mlen;
            self.fast_after_match(&mut ip0, cur0, ilimit, mls, hlog);
        }
    }

    /// fast.c's post-match tail: complementary insertions (`current+2`,
    /// `ip0-2`), then immediate repcode chaining on rep[1] with insertion.
    fn fast_after_match(
        &mut self,
        ip0: &mut usize,
        current0: usize,
        ilimit: usize,
        mls: u32,
        hlog: u32,
    ) {
        let n = self.n();
        let mut ip = *ip0;
        if ip <= ilimit {
            let i1 = (current0 + 2).min(n - TAIL_MARGIN);
            let h1 = zhash(self.chunk, i1, mls, hlog);
            self.tables.head[h1] = i1 as i32;
            let i2 = ip.saturating_sub(2).min(n - TAIL_MARGIN);
            let h2 = zhash(self.chunk, i2, mls, hlog);
            self.tables.head[h2] = i2 as i32;
            loop {
                if ip > ilimit {
                    break;
                }
                let r1 = self.rep[1];
                let rl = self.rep_probe(ip, r1);
                if rl < 4 {
                    break;
                }
                let h = zhash(self.chunk, ip, mls, hlog);
                self.tables.head[h] = ip as i32;
                self.close_blocks_up_to(ip);
                self.store(ip, rl, r1);
                ip += rl;
            }
        }
        *ip0 = ip;
    }

    // ------------------------------------------------------------------
    // zstd_double_fast.c (noDict)
    // ------------------------------------------------------------------
    fn replay_dfast(&mut self) {
        let n = self.n();
        let ilimit = self.ilimit();
        let k_step_incr = 1usize << SEARCH_STRENGTH;
        let mls = self.params.min_match.clamp(4, 8);
        let hbits_s = self.hlog();
        let hbits_l = self.clog();
        ParseTables::reset_head(&mut self.tables.head, hbits_s, n);
        ParseTables::reset_head(&mut self.tables.head2, hbits_l, n);
        let max_dist = self.max_dist();

        let mut ip = 1usize; // ip += (ip == prefixLowest)
        'outer: loop {
            let mut step = 1usize;
            let mut next_step = ip + k_step_incr;
            let mut ip1 = ip + 1;
            if ip1 > ilimit {
                break;
            }
            let mut hl0 = zhash(self.chunk, ip, 8, hbits_l);
            let mut idxl0 = self.tables.head2[hl0];

            // Inner search loop: one iteration per searched position. Yields
            // (match_len, offset, curr, matched_at_ip1, step).
            let (mlen, off, curr, at_ip1, step_now) = loop {
                let hs0 = zhash(self.chunk, ip, mls, hbits_s);
                let idxs0 = self.tables.head[hs0];
                let curr = ip;
                self.tables.head2[hl0] = ip as i32;
                self.tables.head[hs0] = ip as i32;

                // Repcode check at ip+1 (rep[0]) preempts everything.
                let r0 = self.rep[0];
                let rl = self.rep_probe(ip + 1, r0);
                if rl >= 4 {
                    let probe = ip + 1;
                    self.close_blocks_up_to(probe);
                    self.store(probe, rl, r0);
                    ip = probe + rl;
                    self.dfast_after_match(&mut ip, curr, ilimit, mls, hbits_s, hbits_l);
                    continue 'outer;
                }

                let hl1 = zhash(self.chunk, ip1, 8, hbits_l);
                let idxl1 = self.tables.head2[hl1];

                // Long (8-byte) match first.
                if idxl0 >= 0 {
                    let m = idxl0 as usize;
                    if ip - m <= max_dist
                        && m + 8 <= n
                        && read64(self.chunk, m) == read64(self.chunk, ip)
                    {
                        let ml = count(self.chunk, m + 8, ip + 8) + 8;
                        break (ml, (ip - m) as u32, curr, false, step);
                    }
                }
                // Short match, possibly superseded by a long match at ip+1.
                if idxs0 >= 0 {
                    let ms = idxs0 as usize;
                    if ip - ms <= max_dist && read32(self.chunk, ms) == read32(self.chunk, ip) {
                        let mut ml = count(self.chunk, ms + 4, ip + 4) + 4;
                        let mut off = (ip - ms) as u32;
                        let mut at_ip1 = false;
                        if idxl1 >= 0 {
                            let m1 = idxl1 as usize;
                            if ip1 - m1 <= max_dist
                                && m1 + 8 <= n
                                && read64(self.chunk, m1) == read64(self.chunk, ip1)
                            {
                                let l1len = count(self.chunk, m1 + 8, ip1 + 8) + 8;
                                if l1len > ml {
                                    ml = l1len;
                                    off = (ip1 - m1) as u32;
                                    at_ip1 = true;
                                }
                            }
                        }
                        break (ml, off, curr, at_ip1, step);
                    }
                }
                if ip1 >= next_step {
                    step += 1;
                    next_step += k_step_incr;
                }
                ip = ip1;
                ip1 += step;
                hl0 = hl1;
                idxl0 = idxl1;
                if ip1 > ilimit {
                    break 'outer;
                }
            };

            let mut start = if at_ip1 { ip1 } else { ip };
            let mut mpos = start - off as usize;
            let mut mlen = mlen;
            if step_now < 4 {
                self.tables.head2[zhash(self.chunk, ip1, 8, hbits_l)] = ip1 as i32;
            }
            // Complete backward extension.
            while start > self.lit_start
                && mpos >= 1
                && self.chunk[start - 1] == self.chunk[mpos - 1]
            {
                start -= 1;
                mpos -= 1;
                mlen += 1;
            }
            self.close_blocks_up_to(start);
            self.store(start, mlen, off);
            ip = start + mlen;
            self.dfast_after_match(&mut ip, curr, ilimit, mls, hbits_s, hbits_l);
        }
    }

    /// dfast's post-match tail: complementary insertions into both tables,
    /// then immediate repcode chaining on rep[1] with insertion.
    fn dfast_after_match(
        &mut self,
        ip: &mut usize,
        curr: usize,
        ilimit: usize,
        mls: u32,
        hbits_s: u32,
        hbits_l: u32,
    ) {
        let n = self.n();
        let mut pos = *ip;
        if pos <= ilimit {
            let idx_ins = (curr + 2).min(n - TAIL_MARGIN);
            let h = zhash(self.chunk, idx_ins, 8, hbits_l);
            self.tables.head2[h] = idx_ins as i32;
            let h = zhash(self.chunk, idx_ins, mls, hbits_s);
            self.tables.head[h] = idx_ins as i32;
            let i2 = pos.saturating_sub(2).min(n - TAIL_MARGIN);
            let h = zhash(self.chunk, i2, 8, hbits_l);
            self.tables.head2[h] = i2 as i32;
            let i3 = pos.saturating_sub(1).min(n - TAIL_MARGIN);
            let h = zhash(self.chunk, i3, mls, hbits_s);
            self.tables.head[h] = i3 as i32;
            loop {
                if pos > ilimit {
                    break;
                }
                let r1 = self.rep[1];
                let rl = self.rep_probe(pos, r1);
                if rl < 4 {
                    break;
                }
                let h = zhash(self.chunk, pos, mls, hbits_s);
                self.tables.head[h] = pos as i32;
                let h = zhash(self.chunk, pos, 8, hbits_l);
                self.tables.head2[h] = pos as i32;
                self.close_blocks_up_to(pos);
                self.store(pos, rl, r1);
                pos += rl;
            }
        }
        *ip = pos;
    }

    // ------------------------------------------------------------------
    // zstd_lazy.c (hash-chain mode; bt* levels approximated with deep chains)
    // ------------------------------------------------------------------
    fn replay_lazy(&mut self) {
        let n = self.n();
        let ilimit = self.ilimit();
        let depth = match self.params.strategy {
            Strategy::Greedy => 0,
            Strategy::Lazy => 1,
            _ => 2,
        };
        let nb_attempts = 1usize << self.params.search_log;
        let hlog = self.hlog();
        let clog = self.clog();
        ParseTables::reset_head(&mut self.tables.head, hlog, n);
        self.tables.prev.clear();
        self.tables.prev.resize(1 << clog, -1);
        self.tables.next_to_update = 0;

        let mut ip = 0usize;
        let mut lazy_skipping = false;
        while ip < ilimit {
            self.close_blocks_up_to(ip);
            let mut mlen = 0usize;
            // offbase: 1..=3 = repcode k, off+3 = real offset (zstd encoding).
            let mut offbase = 1u32; // REPCODE1_TO_OFFBASE
            let mut start = ip + 1;

            // Repcode check at ip+1 (offset_1).
            if ip < ilimit {
                let r0 = self.rep[0];
                let rl = self.rep_probe(ip + 1, r0);
                if rl >= 4 {
                    mlen = rl;
                    if depth == 0 {
                        self.store(start, mlen, r0);
                        ip = start + mlen;
                        self.lazy_after_match(&mut ip, ilimit);
                        continue;
                    }
                }
            }

            // First search (depth 0).
            {
                let (ml2, ofb) = self.search_max(ip, nb_attempts, lazy_skipping);
                if ml2 > mlen {
                    mlen = ml2;
                    start = ip;
                    offbase = ofb;
                }
            }

            if mlen < 4 {
                let step = ((ip - self.lit_start) >> SEARCH_STRENGTH) + 1;
                ip += step;
                lazy_skipping = step > LAZY_SKIPPING_STEP;
                continue;
            }

            // Try to find a better solution one (or two) bytes later.
            if depth >= 1 {
                while ip < ilimit {
                    ip += 1;
                    // Repcode at ip (gain weight 3).
                    let r0 = self.rep[0];
                    let rl = self.rep_probe(ip, r0);
                    if rl >= 4 {
                        let gain2 = rl as i64 * 3;
                        let gain1 = mlen as i64 * 3 - hb32(offbase) as i64 + 1;
                        if gain2 > gain1 {
                            mlen = rl;
                            offbase = 1;
                            start = ip;
                        }
                    }
                    let (ml2, ofb) = self.search_max(ip, nb_attempts, lazy_skipping);
                    let gain2 = ml2 as i64 * 4 - hb32(ofb) as i64;
                    let gain1 = mlen as i64 * 4 - hb32(offbase) as i64 + 4;
                    if ml2 >= 4 && gain2 > gain1 {
                        mlen = ml2;
                        offbase = ofb;
                        start = ip;
                        continue;
                    }
                    if depth == 2 && ip < ilimit {
                        ip += 1;
                        let r0 = self.rep[0];
                        let rl = self.rep_probe(ip, r0);
                        if rl >= 4 {
                            let gain2 = rl as i64 * 4;
                            let gain1 = mlen as i64 * 4 - hb32(offbase) as i64 + 1;
                            if gain2 > gain1 {
                                mlen = rl;
                                offbase = 1;
                                start = ip;
                            }
                        }
                        let (ml2, ofb) = self.search_max(ip, nb_attempts, lazy_skipping);
                        let gain2 = ml2 as i64 * 4 - hb32(ofb) as i64;
                        let gain1 = mlen as i64 * 4 - hb32(offbase) as i64 + 7;
                        if ml2 >= 4 && gain2 > gain1 {
                            mlen = ml2;
                            offbase = ofb;
                            start = ip;
                            continue;
                        }
                    }
                    break;
                }
            }

            // Catch up (backward extension) for real-offset matches only.
            if offbase > 3 {
                let off = offbase - 3;
                let offu = off as usize;
                while start > self.lit_start
                    && start - offu >= 1
                    && self.chunk[start - 1] == self.chunk[start - offu - 1]
                {
                    start -= 1;
                    mlen += 1;
                }
                self.close_blocks_up_to(start);
                self.store(start, mlen, off);
            } else {
                // Repcode match: the probed offset is rep[0].
                let r0 = self.rep[0];
                self.close_blocks_up_to(start);
                self.store(start, mlen, r0);
            }
            ip = start + mlen;
            lazy_skipping = false;
            self.lazy_after_match(&mut ip, ilimit);
        }
    }

    /// Immediate repcode chaining on rep[1] (the lazy family inserts nothing
    /// here in noDict mode).
    fn lazy_after_match(&mut self, ip: &mut usize, ilimit: usize) {
        let mut pos = *ip;
        loop {
            if pos > ilimit {
                break;
            }
            let r1 = self.rep[1];
            let rl = self.rep_probe(pos, r1);
            if rl < 4 {
                break;
            }
            self.close_blocks_up_to(pos);
            self.store(pos, rl, r1);
            pos += rl;
        }
        *ip = pos;
    }

    /// `ZSTD_HcFindBestMatch` (noDict): catch-up insertion up to `ip`, then
    /// walk the hash chain for the longest match within window/chain bounds.
    /// Returns `(match_len, offbase)`; `match_len == 0` when nothing ≥ 4 was
    /// found.
    fn search_max(&mut self, ip: usize, nb_attempts: usize, lazy_skipping: bool) -> (usize, u32) {
        let n = self.n();
        let hlog = self.hlog();
        let clog = self.clog();
        let mls = self.params.min_match.clamp(4, 8);
        let mask = (1usize << clog) - 1;
        // Catch-up insertion: everything between next_to_update and ip
        // enters the tables (dense), except in lazy-skipping mode, where the
        // cursor jumps ahead leaving the skipped positions uninserted.
        while self.tables.next_to_update < ip {
            let idx = self.tables.next_to_update;
            let h = zhash(self.chunk, idx, mls, hlog);
            self.tables.prev[idx & mask] = self.tables.head[h];
            self.tables.head[h] = idx as i32;
            self.tables.next_to_update = idx + 1;
            if lazy_skipping {
                break;
            }
        }
        if lazy_skipping {
            self.tables.next_to_update = ip;
        }

        let h = zhash(self.chunk, ip, mls, hlog);
        let mut cand = self.tables.head[h];
        let low_limit = ip.saturating_sub(self.max_dist()) as i32;
        let min_chain = ip.saturating_sub(1usize << clog) as i32;
        let mut ml = 3usize; // 4 - 1
        let mut best_off = 0u32;
        let mut attempts = nb_attempts;
        while cand >= low_limit && attempts > 0 {
            let c = cand as usize;
            // "potentially better" prune: the candidate can only beat ml if
            // it still matches at ml.
            if read32(self.chunk, c + ml - 3) == read32(self.chunk, ip + ml - 3) {
                let l = count(self.chunk, c, ip);
                if l > ml {
                    ml = l;
                    best_off = (ip - c) as u32 + 3;
                    if ip + l >= n - 1 {
                        break; // best possible
                    }
                }
            }
            attempts -= 1;
            if cand <= min_chain {
                break;
            }
            let next = self.tables.prev[c & mask];
            if next >= cand {
                break; // ring-slot overwrite anomaly; real terminates too
            }
            cand = next;
        }
        if ml >= 4 {
            (ml, best_off)
        } else {
            (0, 999_999_999)
        }
    }
}

/// Map a literal length to (code, extra bits) — RFC 8878 Table 2.
pub fn ll_code(litlen: u32) -> (usize, u32) {
    const BASE: [u32; 36] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ];
    const EXTRA: [u32; 36] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10,
        11, 12, 13, 14, 15, 16,
    ];
    let code = BASE.partition_point(|&b| b <= litlen) - 1;
    (code.min(35), EXTRA[code.min(35)])
}

/// Map a match length to (code, extra bits) — RFC 8878 Table 3.
pub fn ml_code(matchlen: u32) -> (usize, u32) {
    const BASE: [u32; 53] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515,
        1027, 2051, 4099, 8195, 16387, 32771, 65539,
    ];
    const EXTRA: [u32; 53] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ];
    let code = BASE.partition_point(|&b| b <= matchlen) - 1;
    (code.min(52), EXTRA[code.min(52)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::zstd::levels::params_for;

    #[test]
    fn replay_repetitive_data() {
        let chunk: Vec<u8> = b"INSERT INTO table VALUES (1, 'foo');\n".repeat(4096);
        let mut tables = ParseTables::default();
        let mut blocks = Vec::new();
        for level in [1, 3, 6, 10, 19] {
            replay_blocks(&chunk, &params_for(level), &mut blocks, &mut tables);
            let total_match: u64 = blocks.iter().map(|b| b.total_match_len).sum();
            let nb_seq: u32 = blocks.iter().map(|b| b.nb_seq).sum();
            // Almost everything should be matches after the first occurrence.
            assert!(total_match as usize > chunk.len() / 2, "level {level}");
            assert!(nb_seq > 0, "level {level}");
            // Lit + match bytes must cover the chunk exactly.
            let total_lit: u64 = blocks.iter().map(|b| b.total_lit_len).sum();
            assert_eq!(total_lit + total_match, chunk.len() as u64, "level {level}");
            let block_lens: usize = blocks.iter().map(|b| b.chunk_len).sum();
            assert_eq!(block_lens, chunk.len(), "level {level}");
        }
    }

    #[test]
    fn replay_random_data_is_all_literals() {
        // Deterministic pseudo-random bytes.
        let mut x = 0x1234_5678_9abc_def0u64;
        let chunk: Vec<u8> = (0..16384)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u8
            })
            .collect();
        let mut tables = ParseTables::default();
        let mut blocks = Vec::new();
        replay_blocks(&chunk, &params_for(3), &mut blocks, &mut tables);
        let total_lit: u64 = blocks.iter().map(|b| b.total_lit_len).sum();
        assert!(total_lit as usize >= chunk.len() - 64);
    }

    #[test]
    fn code_tables_boundaries() {
        assert_eq!(ll_code(0), (0, 0));
        assert_eq!(ll_code(15), (15, 0));
        assert_eq!(ll_code(16), (16, 1));
        assert_eq!(ll_code(65536), (35, 16));
        assert_eq!(ml_code(3), (0, 0));
        assert_eq!(ml_code(34), (31, 0));
        assert_eq!(ml_code(35), (32, 1));
        assert_eq!(ml_code(65539), (52, 16));
        assert_eq!(ml_code(1 << 20), (52, 16));
    }
}
