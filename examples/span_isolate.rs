//! Isolate the L19 span-model vs sampled-model discrepancy.

use compress_estimate::backends::zstd::cost;
use compress_estimate::backends::zstd::levels::params_for;
use compress_estimate::backends::zstd::lzparse::{ParseStats, ParseTables, replay_blocks};

fn sql_dump(rows: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let names = ["alice", "bob", "carol", "dave", "erin", "frank", "grace"];
    for i in 0..rows {
        let name = names[i % names.len()];
        data.extend_from_slice(
            format!("INSERT INTO users (id, name, email) VALUES ({i}, '{name}{i}', '{name}{i}@example.com');\n").as_bytes(),
        );
    }
    data
}

fn model_ratio(chunk: &[u8], level: i32, tables: &mut ParseTables) -> f64 {
    let mut blocks: Vec<ParseStats> = Vec::new();
    replay_blocks(chunk, &params_for(level), &mut blocks, tables);
    1.0 / cost::chunk_fraction(&blocks)
}

fn main() {
    let data = sql_dump(200_000);
    let n = data.len();
    println!("input: {n} bytes");
    let mid = &data[(n - (8 << 20)) / 2..][..8 << 20];

    for level in [10, 19] {
        // (a) each 512KB tile of the middle-8MB span with its own fresh tables
        let mut fresh = Vec::new();
        for t in mid.chunks(512 << 10) {
            let mut tables = ParseTables::default();
            fresh.push(model_ratio(t, level, &mut tables));
        }
        // (b) same tiles, tables reused across tiles
        let mut reused = Vec::new();
        let mut tables = ParseTables::default();
        for t in mid.chunks(512 << 10) {
            reused.push(model_ratio(t, level, &mut tables));
        }
        // (c) one shared fraction over concatenated blocks, fresh tables
        let mut tables = ParseTables::default();
        let mut all: Vec<ParseStats> = Vec::new();
        let mut tile = Vec::new();
        for t in mid.chunks(512 << 10) {
            replay_blocks(t, &params_for(level), &mut tile, &mut tables);
            all.append(&mut tile);
        }
        let concat = 1.0 / cost::chunk_fraction(&all);
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        println!(
            "L{level:>2}: fresh-tables mean {:6.2} | reused-tables mean {:6.2} | concat {:6.2}",
            mean(&fresh),
            mean(&reused),
            concat
        );
        println!(
            "     fresh per-tile: {:.1?}",
            fresh
                .iter()
                .map(|r| (r * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        );
    }
}
