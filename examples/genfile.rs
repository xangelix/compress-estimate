use std::io::Write;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let target_mb: usize = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2048);
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let names = [
        "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy",
    ];
    let mut written = 0usize;
    let mut i = 0usize;
    while written < target_mb << 20 {
        let name = names[i % names.len()];
        let line = format!(
            "INSERT INTO users (id, name, email, balance, created) VALUES ({i}, '{name}{i}', '{name}{i}@example.com', {}.{:02}, '2026-09-{:02}T{:02}:{:02}:{:02}');\n",
            (i * 7) % 10000,
            (i * 13) % 100,
            (i % 28) + 1,
            i % 24,
            (i * 7) % 60,
            (i * 31) % 60
        );
        written += line.len();
        i += 1;
        f.write_all(line.as_bytes()).unwrap();
    }
    println!("wrote {written} bytes to {path}");
}
