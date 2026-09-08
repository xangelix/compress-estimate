//! Dump the first N real sequences zstd chooses on the SQL corpus.

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

fn main() {
    let data = sql_dump(2_000);
    let level: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
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
        let real = data.len() as f64 / zstd::bulk::compress(&data, level).unwrap().len() as f64;
        println!("level {level}: {} sequences, real ratio {real:.2}", n);

        let mut pos = 0usize;
        let mut shown = 0;
        for s in &seqs[..n.min(400)] {
            let (ll, ml, off, rep) = (
                s.litLength as usize,
                s.matchLength as usize,
                s.offset as usize,
                s.rep as usize,
            );
            if ml == 0 {
                continue;
            }
            let lit_text: String = data[pos..pos + ll.min(40)]
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            println!(
                "pos {:>7} | lit {:>3} {:?} | match {:>4} off {:>7} rep {}",
                pos, ll, lit_text, ml, off, rep
            );
            pos += ll + ml;
            shown += 1;
            if shown >= 60 {
                break;
            }
        }
    }
}
