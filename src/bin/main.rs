//! `compress-estimate` CLI.

use std::io::{self, Read as _, Write as _};
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use compress_estimate::backends::zstd::Zstd;
use compress_estimate::{Estimator, Report, Verdict};

/// Estimate compression ratio and speed for a data source — without
/// compressing it — via strategic sampling.
#[derive(Parser)]
#[command(name = "compress-estimate", version, about, long_about = None)]
struct Cli {
    /// Target file, or '-' to read stdin as a stream.
    target: String,

    /// Compression levels to estimate (comma-separated).
    #[arg(short, long, value_delimiter = ',', default_value = "3,10,15")]
    levels: Vec<i32>,

    /// Estimate every level, 1 through 22.
    #[arg(long, conflicts_with = "levels")]
    all_levels: bool,

    /// Sampling budget: bytes ('64MB', '500K'), or a fraction of the input
    /// ('0.1%'). Default: ~0.1% of input, clamped to 8–64MB.
    #[arg(short, long)]
    budget: Option<String>,

    /// Early-stop accuracy target: relative 95%% CI half-width on the ratio,
    /// in percent.
    #[arg(long, default_value = "2")]
    accuracy: f64,

    /// Worker threads for sampling and analysis (0 = all cores).
    #[arg(short, long, default_value = "0")]
    threads: usize,

    /// Disable the real-compression anchor: ratios come from the pure cost
    /// model and no data is compressed at all for ratio estimation.
    #[arg(long)]
    no_anchor: bool,

    /// Force streaming mode (reservoir sampling while reading linearly).
    #[arg(long)]
    stream: bool,

    /// Emit JSON instead of the human report.
    #[arg(long)]
    json: bool,

    /// Show sampling detail and confidence intervals.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let backend = if cli.all_levels {
        Zstd::all_levels()
    } else {
        Zstd::levels(cli.levels.iter().copied())
    };

    let mut est = Estimator::new(backend)
        .threads(cli.threads)
        .accuracy(cli.accuracy / 100.0)
        .anchor(!cli.no_anchor);
    if let Some(b) = &cli.budget {
        est = est.budget(parse_budget(b)?);
    }

    let streaming = cli.stream || cli.target == "-";
    let started = Instant::now();
    let (name, report) = if streaming {
        let mut est = est;
        let stdin = io::stdin();
        let mut lock = stdin.lock();
        let mut buf = vec![0u8; 1 << 18];
        loop {
            let n = lock.read(&mut buf)?;
            if n == 0 {
                break;
            }
            est.update(&buf[..n])?;
        }
        ("<stdin>".to_string(), est.finish()?)
    } else {
        (cli.target.clone(), est.estimate_path(&cli.target)?)
    };
    let elapsed = started.elapsed();

    if cli.json {
        print_json(&name, &report, elapsed)?;
    } else {
        print_report(&name, &report, elapsed, cli.verbose);
    }
    Ok(())
}

fn print_report(name: &str, report: &Report<i32>, elapsed: std::time::Duration, verbose: bool) {
    let out = io::stdout();
    let mut w = out.lock();

    let total = report.total_len();
    writeln!(w, "Target:       {name} ({})", human_size(total)).unwrap();
    writeln!(
        w,
        "Entropy:      {:.2} / 8.00 bits/byte ({})",
        report.entropy(),
        report.data_class()
    )
    .unwrap();
    let verdict = report.verdict();
    writeln!(w, "Verdict:      {}", verdict.label()).unwrap();
    if verbose {
        writeln!(
            w,
            "Sampled:      {} of {} ({:.3}%) in {:.2?}",
            human_size(report.sampled_bytes()),
            human_size(total),
            report.sampled_bytes() as f64 / total.max(1) as f64 * 100.0,
            elapsed
        )
        .unwrap();
    }

    writeln!(
        w,
        "Level    Ratio     Projected Size     Reduction    Throughput"
    )
    .unwrap();
    writeln!(
        w,
        "─────────────────────────────────────────────────────────────"
    )
    .unwrap();
    let best = report.best_value().map(|r| r.level);
    for row in report.levels() {
        let tput = match row.throughput {
            Some(t) => format_throughput(t),
            None => "n/a".to_string(),
        };
        let marker = if Some(row.level) == best && report.levels().len() > 1 {
            "  ★ Best Value"
        } else {
            ""
        };
        writeln!(
            w,
            "{:>4}    {:>5.2}×    {:>10}        {:>5.1}%    {:>11}{}",
            row.level,
            row.ratio,
            human_size(row.projected_size),
            row.reduction * 100.0,
            tput,
            marker
        )
        .unwrap();
        if verbose && row.ratio_ci > 0.0 {
            writeln!(
                w,
                "         └─ 95% CI: {:.2}×–{:.2}×",
                row.ratio - row.ratio_ci,
                row.ratio + row.ratio_ci
            )
            .unwrap();
        }
    }
    if let Some(best) = report.best_value() {
        let saved = total.saturating_sub(best.projected_size);
        writeln!(
            w,
            "Projected Space Savings (Level {}): +{}",
            best.level,
            human_size(saved)
        )
        .unwrap();
    }
    if verdict == Verdict::Low {
        writeln!(
            w,
            "Note: this data looks incompressible; consider skipping compression."
        )
        .unwrap();
    }
}

fn print_json(
    name: &str,
    report: &Report<i32>,
    elapsed: std::time::Duration,
) -> serde_json::Result<()> {
    let levels: Vec<serde_json::Value> = report
        .levels()
        .iter()
        .map(|r| {
            serde_json::json!({
                "level": r.level,
                "ratio": r.ratio,
                "ratio_ci95": r.ratio_ci,
                "projected_size_bytes": r.projected_size,
                "reduction": r.reduction,
                "throughput_bytes_per_sec": r.throughput,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "target": name,
        "size_bytes": report.total_len(),
        "sampled_bytes": report.sampled_bytes(),
        "entropy_bits_per_byte": report.entropy(),
        "data_class": report.data_class(),
        "verdict": format!("{:?}", report.verdict()),
        "best_value_level": report.best_value().map(|r| r.level),
        "elapsed_ms": elapsed.as_millis() as u64,
        "levels": levels,
    });
    let out = io::stdout();
    serde_json::to_writer_pretty(out.lock(), &doc)?;
    println!();
    Ok(())
}

/// 12345678 → "11.77 MB" (1024-based, two decimals).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.2} {}", UNITS[u])
}

/// 490_000_000.0 → "~490 MB/s" (3 significant digits).
fn format_throughput(bytes_per_sec: f64) -> String {
    let (v, unit) = if bytes_per_sec >= 1e9 {
        (bytes_per_sec / 1e9, "GB/s")
    } else if bytes_per_sec >= 1e6 {
        (bytes_per_sec / 1e6, "MB/s")
    } else {
        (bytes_per_sec / 1e3, "KB/s")
    };
    if v >= 10.0 {
        format!("~{v:.0} {unit}")
    } else {
        format!("~{v:.1} {unit}")
    }
}

fn parse_budget(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        return pct
            .parse::<f64>()
            .map_err(|e| format!("bad percentage {s:?}: {e}"))
            .map(|p| (p / 100.0 * (1u64 << 40) as f64) as u64); // % of 1 TiB baseline
    }
    let (num, mult) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => {
            let (n, u) = s.split_at(i);
            let m = match u.to_ascii_uppercase().as_str() {
                "K" | "KB" => 1 << 10,
                "M" | "MB" => 1 << 20,
                "G" | "GB" => 1 << 30,
                _ => return Err(format!("unknown unit in {s:?}")),
            };
            (n, m)
        }
        None => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .map_err(|e| format!("bad budget {s:?}: {e}"))
        .map(|n| (n * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_parsing() {
        assert_eq!(parse_budget("64MB").unwrap(), 64 << 20);
        assert_eq!(parse_budget("1024").unwrap(), 1024);
        assert!(parse_budget("bogus").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1536), "1.50 KB");
        assert_eq!(human_size(18 << 30), "18.00 GB");
    }
}
