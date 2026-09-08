//! Accuracy integration tests: estimates vs. real zstd on synthetic corpora.

use compress_estimate::Estimator;
use compress_estimate::backends::zstd::Zstd;

/// Deterministic pseudo-random bytes (xorshift64).
fn random_bytes(n: usize, mut x: u64) -> Vec<u8> {
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

/// Synthetic SQL dump: long-range structure + local repetitiveness.
fn sql_dump(rows: usize) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(
        b"-- synthetic dump\nCREATE TABLE users (id INT, name TEXT, email TEXT);\n",
    );
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

/// Heterogeneous mix: SQL, then random, then zeros, then logs.
fn mixed() -> Vec<u8> {
    let mut data = sql_dump(2_000);
    data.extend_from_slice(&random_bytes(1 << 19, 0xABCD));
    data.extend_from_slice(&vec![0u8; 1 << 19]);
    for i in 0..1500 {
        data.extend_from_slice(
            format!(
                "2026-09-08T10:{:02}:{:02} INFO request handled in {}ms\n",
                i % 60,
                i % 60,
                i % 97
            )
            .as_bytes(),
        );
    }
    data
}

fn real_ratio(data: &[u8], level: i32) -> f64 {
    let out = zstd::bulk::compress(data, level).unwrap();
    data.len() as f64 / out.len() as f64
}

fn estimate(data: &[u8], levels: &[i32]) -> compress_estimate::Report<i32> {
    let src: &[u8] = data;
    Estimator::new(Zstd::levels(levels.iter().copied()))
        .threads(4)
        .budget(16 << 20)
        .estimate_source(&src)
        .unwrap()
}

/// Relative error of the estimated ratio at `level`.
fn rel_err(data: &[u8], level: i32) -> (f64, f64, f64) {
    let report = estimate(data, &[level]);
    let est = report.levels()[0].ratio;
    let real = real_ratio(data, level);
    (est, real, (est - real).abs() / real)
}

#[test]
fn sql_dump_level3_accurate() {
    let data = sql_dump(8_000); // ~600 KB
    let (est, real, err) = rel_err(&data, 3);
    assert!(
        err < 0.10,
        "level 3: est {est:.2} real {real:.2} err {err:.3}"
    );
}

#[test]
fn sql_dump_level10_accurate() {
    let data = sql_dump(8_000);
    let (est, real, err) = rel_err(&data, 10);
    assert!(
        err < 0.10,
        "level 10: est {est:.2} real {real:.2} err {err:.3}"
    );
}

#[test]
fn random_data_ratio_near_one() {
    let data = random_bytes(1 << 20, 0x1234);
    let (est, real, err) = rel_err(&data, 3);
    assert!(
        err < 0.02,
        "random: est {est:.3} real {real:.3} err {err:.3}"
    );
}

#[test]
fn zeros_ratio_huge() {
    let data = vec![0u8; 1 << 20];
    let (est, real, _) = rel_err(&data, 3);
    assert!(est > 50.0 && real > 50.0, "est {est} real {real}");
    // Order-of-magnitude agreement for extreme ratios.
    assert!((est.ln() - real.ln()).abs() < 1.0, "est {est} real {real}");
}

#[test]
fn mixed_data_accurate() {
    let data = mixed();
    for level in [3, 10, 15] {
        let (est, real, err) = rel_err(&data, level);
        assert!(
            err < 0.12,
            "mixed level {level}: est {est:.2} real {real:.2} err {err:.3}"
        );
    }
}

#[test]
fn anchored_estimates_track_real_across_levels() {
    // Real zstd is non-monotone on small inputs (higher levels can compress
    // worse), so instead of asserting ordering we assert per-level tracking.
    let data = sql_dump(8_000);
    let report = estimate(&data, &[1, 3, 7, 12, 19]);
    for row in report.levels() {
        let real = real_ratio(&data, row.level);
        let err = (row.ratio - real).abs() / real;
        assert!(
            err < 0.10,
            "level {}: est {:.2} real {real:.2} err {err:.3}",
            row.level,
            row.ratio
        );
    }
}

#[test]
fn streaming_matches_seekable() {
    let data = mixed();
    let seekable = estimate(&data, &[3]);
    let mut est = Estimator::new(Zstd::levels([3])).budget(16 << 20);
    for chunk in data.chunks(128 * 1024) {
        est.update(chunk).unwrap();
    }
    let streamed = est.finish().unwrap();
    let a = seekable.levels()[0].ratio;
    let b = streamed.levels()[0].ratio;
    assert!(
        (a - b).abs() / a < 0.1,
        "seekable {a:.2} vs streamed {b:.2}"
    );
}
