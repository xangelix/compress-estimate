fn sql_dump(rows: usize, fixed: bool) -> Vec<u8> {
    let mut data = Vec::new();
    let names = ["alice", "bob", "carol", "dave", "erin", "frank", "grace"];
    for i in 0..rows {
        let name = names[i % names.len()];
        if fixed {
            data.extend_from_slice(
                format!("INSERT INTO users (id, name, email) VALUES ({i:06}, '{name}{i:06}', '{name}{i:06}@example.com');\n").as_bytes(),
            );
        } else {
            data.extend_from_slice(
                format!("INSERT INTO users (id, name, email) VALUES ({i}, '{name}{i}', '{name}{i}@example.com');\n").as_bytes(),
            );
        }
    }
    data
}
fn main() {
    for (label, fixed) in [("variable-digits", false), ("fixed-digits", true)] {
        let data = sql_dump(200_000, fixed);
        print!("{label:>16}:");
        for mb in [1, 4, 18] {
            let end = (mb * 1024 * 1024).min(data.len());
            let d = &data[..end];
            print!(" {mb:>2}MB[");
            for level in [1, 3, 10] {
                let c = zstd::bulk::compress(d, level).unwrap();
                print!(" L{level}={:5.2}", d.len() as f64 / c.len() as f64);
            }
            print!(" ]");
        }
        println!();
    }
}
