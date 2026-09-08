//! Debug harness: compare estimate vs real zstd on synthetic data.

use compress_estimate::Estimator;
use compress_estimate::backends::zstd::Zstd;

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
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(8_000);
    let data = sql_dump(rows);
    println!("input: {} bytes", data.len());
    let src: &[u8] = &data;

    // Real ratios: whole file vs first 512KB chunk.
    let chunk = &data[..data.len().min(512 * 1024)];
    for level in [1, 3, 7, 10, 15, 19] {
        let whole = data.len() as f64 / zstd::bulk::compress(&data, level).unwrap().len() as f64;
        let part = chunk.len() as f64 / zstd::bulk::compress(chunk, level).unwrap().len() as f64;
        println!("real L{level:>2}: whole {whole:>7.2}×   first-512K {part:>7.2}×");
    }

    for anchor in [true, false] {
        let report = Estimator::new(Zstd::levels([1, 3, 7, 10, 15, 19]))
            .threads(4)
            .budget(16 << 20)
            .anchor(anchor)
            .estimate_source(&src)
            .unwrap();
        println!(
            "--- anchor={anchor} (sampled {} bytes)",
            report.sampled_bytes()
        );
        for row in report.levels() {
            let real_out = zstd::bulk::compress(&data, row.level).unwrap();
            let real = data.len() as f64 / real_out.len() as f64;
            println!(
                "  L{:>2}: est {:>7.2}×  real {:>7.2}×  err {:>6.1}%",
                row.level,
                row.ratio,
                real,
                (row.ratio - real).abs() / real * 100.0
            );
        }
    }
}
