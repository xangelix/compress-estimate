//! Ground-truth probe: dump zstd's real parse (ZSTD_generateSequences) on
//! the SQL corpus and compare with our replayed parse statistics.

use compress_estimate::backends::zstd::cost;
use compress_estimate::backends::zstd::levels::params_for;
use compress_estimate::backends::zstd::lzparse::{
    ParseStats, ParseTables, ll_code, ml_code, replay_blocks,
};
use zstd_sys::*;

fn sql_dump(rows: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let names = ["alice", "bob", "carol", "dave", "erin", "frank", "grace"];
    for i in 0..rows {
        let name = names[i % names.len()];
        data.extend_from_slice(
            format!(
                "INSERT INTO users (id, name, email) VALUES ({i}, '{name}{i}', '{name}{i}@example.com');\n"
            )
            .as_bytes(),
        );
    }
    data
}

struct RealParse {
    nb_seq: usize,
    lit_bytes: usize,
    match_bytes: usize,
    /// Per-block ParseStats-shaped accumulation for cost-model comparison.
    blocks: Vec<ParseStats>,
}

/// Run zstd's real match finder at `level` over `data`.
fn real_parse(data: &[u8], level: i32) -> RealParse {
    unsafe {
        let cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_cParameter::ZSTD_c_compressionLevel, level);
        let bound = ZSTD_sequenceBound(data.len());
        let mut seqs: Vec<ZSTD_Sequence> = vec![std::mem::zeroed(); bound];
        let n = ZSTD_generateSequences(
            cctx,
            seqs.as_mut_ptr(),
            bound,
            data.as_ptr() as *const _,
            data.len(),
        );
        assert_eq!(ZSTD_isError(n), 0, "generateSequences failed");
        ZSTD_freeCCtx(cctx);

        let mut blocks = vec![ParseStats::new(0)];
        let mut lit_pos = 0usize;
        let mut lit_bytes = 0usize;
        let mut match_bytes = 0usize;
        let mut block_start = 0usize;
        let mut nb = 0usize;
        for s in &seqs[..n] {
            let ll = s.litLength as usize;
            let ml = s.matchLength as usize;
            if ml == 0 {
                // Block delimiter: ll = trailing literals of this block.
                let cur = blocks.last_mut().unwrap();
                cur.literals.add(&data[lit_pos..lit_pos + ll]);
                cur.total_lit_len += ll as u64;
                lit_pos += ll;
                lit_bytes += ll;
                cur.chunk_len = lit_pos - block_start;
                block_start = lit_pos;
                if lit_pos < data.len() {
                    blocks.push(ParseStats::new(0));
                }
                continue;
            }
            let off = s.offset;
            let ov = if s.rep != 0 { s.rep } else { off + 3 };
            let cur = blocks.last_mut().unwrap();
            cur.literals.add(&data[lit_pos..lit_pos + ll]);
            cur.total_lit_len += ll as u64;
            lit_pos += ll + ml;
            let (llc, lle) = ll_code(ll as u32);
            let (mlc, mle) = ml_code(ml as u32);
            let oc = (31 - ov.leading_zeros()) as usize;
            cur.ll_codes[llc] += 1;
            cur.ml_codes[mlc] += 1;
            cur.off_codes[oc.min(31)] += 1;
            cur.extra_bits += (lle + mle) as u64 + oc.min(31) as u64;
            cur.nb_seq += 1;
            cur.total_match_len += ml as u64;
            lit_bytes += ll;
            match_bytes += ml;
            nb += 1;
        }
        debug_assert_eq!(lit_pos, data.len());
        RealParse {
            nb_seq: nb,
            lit_bytes,
            match_bytes,
            blocks,
        }
    }
}

fn main() {
    let data = sql_dump(8_000);
    let n = data.len();
    println!("input: {n} bytes");
    let mut tables = ParseTables::default();
    let mut our_blocks: Vec<ParseStats> = Vec::new();

    println!(
        "lvl | real: seq, lit, match, ratio | ours: seq, lit, match, ratio | model-on-real-parse ratio"
    );
    for level in [1, 3, 7, 10, 15, 19] {
        let rp = real_parse(&data, level);
        let real_size = zstd::bulk::compress(&data, level).unwrap().len();
        let real_ratio = n as f64 / real_size as f64;
        let (rf, rl, rll, rml, rof, rextra) = rp.blocks.iter().map(cost::cost_breakdown_full).fold(
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
            |a, b| {
                (
                    a.0 + b.0,
                    a.1 + b.1,
                    a.2 + b.2,
                    a.3 + b.3,
                    a.4 + b.4,
                    a.5 + b.5,
                )
            },
        );
        let lit_h: f64 = rp
            .blocks
            .iter()
            .map(|b| b.literals.entropy() * b.literals.total as f64)
            .sum::<f64>()
            / rp.lit_bytes.max(1) as f64;
        println!(
            "  L{level} real-parse breakdown: framing {rf:.0} | lit {rl:.0} ({:.1}/B H={:.2}) | ll {rll:.0} ml {rml:.0} of {rof:.0} extra+hdr {rextra:.0} | real total {} bits",
            rl / rp.lit_bytes.max(1) as f64,
            lit_h,
            real_size * 8,
        );

        replay_blocks(&data, &params_for(level), &mut our_blocks, &mut tables);
        let our_frac = cost::chunk_fraction(&our_blocks);
        let our_seq: u32 = our_blocks.iter().map(|b| b.nb_seq).sum();
        let our_lit: u64 = our_blocks.iter().map(|b| b.total_lit_len).sum();
        let our_match: u64 = our_blocks.iter().map(|b| b.total_match_len).sum();
        let (of_, ol, oll, oml, oof, oextra) = our_blocks
            .iter()
            .map(cost::cost_breakdown_full)
            .fold((0.0, 0.0, 0.0, 0.0, 0.0, 0.0), |a, b| {
                (
                    a.0 + b.0,
                    a.1 + b.1,
                    a.2 + b.2,
                    a.3 + b.3,
                    a.4 + b.4,
                    a.5 + b.5,
                )
            });
        println!(
            "  L{level} our-parse breakdown:   framing {of_:.0} | lit {ol:.0} | ll {oll:.0} ml {oml:.0} of {oof:.0} extra+hdr {oextra:.0}"
        );

        // Our cost model applied to the REAL parse.
        let model_on_real = 1.0 / cost::chunk_fraction(&rp.blocks);

        println!(
            "L{level:>2} | {:>6} {:>7} {:>8} {:>6.2}× | {:>6} {:>7} {:>8} {:>6.2}× | {:>6.2}×",
            rp.nb_seq,
            rp.lit_bytes,
            rp.match_bytes,
            real_ratio,
            our_seq,
            our_lit,
            our_match,
            1.0 / our_frac,
            model_on_real,
        );
    }
}
