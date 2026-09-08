//! Dump offset-code histograms: real zstd parse vs our replay, per level.

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

fn real_offcodes(data: &[u8], level: i32) -> ([u32; 32], u64, usize) {
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
        assert_eq!(ZSTD_isError(n), 0);
        ZSTD_freeCCtx(cctx);
        let mut codes = [0u32; 32];
        let mut extra = 0u64;
        let mut nb = 0usize;
        let trace = std::env::var_os("CE_TRACE").is_some();
        let mut litlen_acc = 0usize;
        for s in &seqs[..n] {
            if s.matchLength == 0 {
                litlen_acc = 0;
                continue;
            }
            if trace && nb < 60 {
                eprintln!(
                    "real seq lit={} ml={} off={} rep={}",
                    s.litLength, s.matchLength, s.offset, s.rep
                );
            }
            let _ = litlen_acc;
            let ov = if s.rep != 0 { s.rep } else { s.offset + 3 };
            let oc = 31 - ov.leading_zeros();
            codes[oc as usize] += 1;
            let (_, lle) = ll_code(s.litLength);
            let (_, mle) = ml_code(s.matchLength);
            extra += (lle + mle) as u64 + oc as u64;
            nb += 1;
        }
        (codes, extra, nb)
    }
}

fn main() {
    let data = sql_dump(8_000);
    let mut tables = ParseTables::default();
    let mut blocks: Vec<ParseStats> = Vec::new();

    for level in [1, 3, 7] {
        replay_blocks(&data, &params_for(level), &mut blocks, &mut tables);
        let mut our_codes = [0u32; 32];
        let mut our_extra = 0u64;
        for b in &blocks {
            for (i, &c) in b.off_codes.iter().enumerate() {
                our_codes[i] += c;
            }
            our_extra += b.extra_bits;
        }
        let (rcodes, rextra, rnb) = real_offcodes(&data, level);
        let our_nb: u32 = blocks.iter().map(|b| b.nb_seq).sum();
        println!("L{level}: real nb={rnb} extra={rextra} | ours nb={our_nb} extra={our_extra}");
        print!("  real off_codes:");
        for (i, &c) in rcodes.iter().enumerate() {
            if c > 0 {
                print!(" [{i}]={c}");
            }
        }
        println!();
        print!("  our  off_codes:");
        for (i, &c) in our_codes.iter().enumerate() {
            if c > 0 {
                print!(" [{i}]={c}");
            }
        }
        println!();
    }
}
