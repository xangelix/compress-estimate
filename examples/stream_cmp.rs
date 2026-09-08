use compress_estimate::Estimator;
use compress_estimate::backends::zstd::Zstd;

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

fn main() {
    let names = ["alice", "bob", "carol", "dave", "erin", "frank", "grace"];
    let mut data = Vec::new();
    data.extend_from_slice(
        b"-- synthetic dump\nCREATE TABLE users (id INT, name TEXT, email TEXT);\n",
    );
    for i in 0..2000usize {
        let name = names[i % names.len()];
        data.extend_from_slice(format!("INSERT INTO users (id, name, email) VALUES ({i}, '{name}{i}', '{name}{i}@example.com');\n").as_bytes());
    }
    data.extend_from_slice(&random_bytes(1 << 19, 0xABCD));
    data.extend_from_slice(&vec![0u8; 1 << 19]);
    for i in 0..1500usize {
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
    println!("total {} bytes", data.len());
    let src: &[u8] = &data;
    let seek = Estimator::new(Zstd::levels([3]))
        .threads(4)
        .budget(16 << 20)
        .estimate_source(&src)
        .unwrap();
    eprintln!("--- seekable done");
    let mut est = Estimator::new(Zstd::levels([3])).budget(16 << 20);
    for chunk in data.chunks(128 * 1024) {
        est.update(chunk).unwrap();
    }
    let stream = est.finish().unwrap();
    eprintln!("--- stream done");
    println!(
        "seekable L3: {:.3} (sampled {})",
        seek.levels()[0].ratio,
        seek.sampled_bytes()
    );
    println!(
        "streamed L3: {:.3} (sampled {})",
        stream.levels()[0].ratio,
        stream.sampled_bytes()
    );
    let real = data.len() as f64 / zstd::bulk::compress(&data, 3).unwrap().len() as f64;
    println!("real     L3: {real:.3}");
}
