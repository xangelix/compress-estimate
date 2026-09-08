//! Measure the per-chunk cold-start penalty: real zstd ratio on the whole
//! input vs weighted mean of per-tile ratios (and the same for our model).

use compress_estimate::backends::zstd::cost;
use compress_estimate::backends::zstd::levels::params_for;
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
    let data = sql_dump(200_000); // ~18 MB
    println!("input: {} bytes", data.len());
    let mut tables = ParseTables::default();
    let mut blocks: Vec<ParseStats> = Vec::new();

    for level in [1, 3, 10, 19] {
        let whole_real =
            data.len() as f64 / zstd::bulk::compress(&data, level).unwrap().len() as f64;
        for tile_kb in [128, 512, 2048] {
            let tile = tile_kb << 10;
            // Real per-tile.
            let mut real_frac_sum = 0.0;
            let mut w_sum = 0.0;
            // Model per-tile.
            let mut model_frac_sum = 0.0;
            for chunk in data.chunks(tile) {
                let real = zstd::bulk::compress(chunk, level).unwrap();
                real_frac_sum += real.len() as f64;
                replay_blocks(chunk, &params_for(level), &mut blocks, &mut tables);
                model_frac_sum += cost::chunk_fraction(&blocks) * chunk.len() as f64;
                w_sum += chunk.len() as f64;
            }
            let tiled_real = w_sum / real_frac_sum;
            let tiled_model = w_sum / model_frac_sum;
            println!(
                "L{level:>2} tile {tile_kb:>4}KB: whole-real {whole_real:6.2}× | tiled-real {tiled_real:6.2}× ({:+.1}%) | tiled-model {tiled_model:6.2}× ({:+.1}%)",
                (tiled_real / whole_real - 1.0) * 100.0,
                (tiled_model / whole_real - 1.0) * 100.0,
            );
        }
    }
}
