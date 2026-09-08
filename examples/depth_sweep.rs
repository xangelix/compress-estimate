//! Depth-sweep experiment: how much does search depth buy on SQL data?

use compress_estimate::backends::zstd::cost;
use compress_estimate::backends::zstd::levels::{Strategy, params_for};
use compress_estimate::backends::zstd::lzparse::{ParseStats, ParseTables, replay_blocks};

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
    let data = sql_dump(8_000);
    let mut tables = ParseTables::default();
    let mut blocks: Vec<ParseStats> = Vec::new();

    println!("== depth sweep (greedy, min_match 4, window 2^22) ==");
    for slog in 0..7u32 {
        let mut p = params_for(5);
        p.strategy = Strategy::Greedy;
        p.min_match = 4;
        p.window_log = 22;
        p.search_log = slog; // 2^search_log attempts
        replay_blocks(&data, &p, &mut blocks, &mut tables);
        let frac = cost::chunk_fraction(&blocks);
        let seq: u32 = blocks.iter().map(|b| b.nb_seq).sum();
        let lit: u64 = blocks.iter().map(|b| b.total_lit_len).sum();
        let m: u64 = blocks.iter().map(|b| b.total_match_len).sum();
        println!(
            "  depth {:>3}: ratio {:>7.2}×  seq {seq}  lit {lit}  match {m}",
            1 << slog,
            1.0 / frac,
        );
    }

    println!("== real level parse params replayed ==");
    for level in [1, 3, 7, 10, 15, 19] {
        let p = params_for(level);
        replay_blocks(&data, &p, &mut blocks, &mut tables);
        let frac = cost::chunk_fraction(&blocks);
        let real = data.len() as f64 / zstd::bulk::compress(&data, level).unwrap().len() as f64;
        let seq: u32 = blocks.iter().map(|b| b.nb_seq).sum();
        let lit: u64 = blocks.iter().map(|b| b.total_lit_len).sum();
        println!(
            "  L{level:>2} ({:?}): model {:>7.2}×  real {:>7.2}×  [seq {seq} lit {lit}]",
            p.strategy,
            1.0 / frac,
            real,
        );
    }
}
