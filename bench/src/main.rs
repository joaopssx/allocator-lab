//! Benchmark harness.
//!
//! Every (allocator, scenario) pair runs in its own child process, started
//! by this same binary with `--child`. A fresh process gives every run a
//! fresh arena. It also turns a failure into a table entry instead of a
//! crash: the bump allocator running out of memory, or the slab allocator
//! refusing a request above 2048 bytes, make `Vec` abort the child, and the
//! parent records that and moves on.
//!
//! ```text
//! cargo run --release -p bench             # print the tables
//! cargo run --release -p bench -- --write  # also update docs/
//! ```

mod report;
mod scenarios;
mod switch;

use scenarios::Ctx;
use std::process::Command;
use std::time::Instant;
use switch::{Kind, Switch};

#[global_allocator]
static GLOBAL: Switch = Switch;

/// Timed runs per child; the report uses the median.
const RUNS: usize = 7;

/// What one child reported.
#[derive(Debug, Clone)]
pub enum Cell {
    /// The child finished.
    Done {
        /// Median nanoseconds per operation.
        ns: f64,
        /// Usage at the scenario's peak, for lab allocators.
        peak: Option<switch::Usage>,
    },
    /// The child died; the reason is a short label.
    Failed(&'static str),
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        child(&args[2], &args[3]);
        return;
    }
    let write = args.iter().any(|a| a == "--write");

    let mut rows = Vec::new();
    for &(name, _, _) in scenarios::ALL {
        let cells: Vec<Cell> = Kind::ALL.iter().map(|&k| spawn(k, name)).collect();
        eprintln!("{name}: done");
        rows.push((name, cells));
    }

    let md = report::markdown(&rows);
    println!("{md}");
    if write {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
        std::fs::write(format!("{root}/docs/bench-results.md"), md).expect("write results");
        std::fs::create_dir_all(format!("{root}/docs/img")).expect("create docs/img");
        std::fs::write(
            format!("{root}/docs/img/time.svg"),
            report::time_chart(&rows),
        )
        .expect("write chart");
        std::fs::write(
            format!("{root}/docs/img/overhead.svg"),
            report::overhead_chart(&rows),
        )
        .expect("write chart");
        eprintln!("wrote docs/bench-results.md and docs/img/*.svg");
    }
}

/// Runs one pair in a child process and parses what it prints.
fn spawn(kind: Kind, scenario: &str) -> Cell {
    let exe = std::env::current_exe().expect("current exe");
    let out = Command::new(exe)
        .args(["--child", kind.name(), scenario])
        .output()
        .expect("spawn child");
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Out of memory and "request not served" look the same from here:
        // a null pointer that `Vec` or `handle_alloc_error` turned into an
        // abort.
        return Cell::Failed(if err.contains("memory allocation") {
            "alloc failed"
        } else {
            "crashed"
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("RESULT "))
        .expect("child result line");
    let f: Vec<&str> = line.split_whitespace().collect();
    let num = |s: &str| s.parse::<usize>().ok();
    let peak = num(f[1]).map(|requested| switch::Usage {
        requested,
        used: num(f[2]).unwrap_or(0),
        largest: num(f[3]),
        free: num(f[4]).unwrap_or(0),
    });
    Cell::Done {
        ns: f[0].parse().expect("ns per op"),
        peak,
    }
}

/// Body of a child process: run one scenario `RUNS` times on one allocator.
fn child(kind: &str, scenario: &str) {
    let kind = Kind::parse(kind).expect("unknown allocator");
    let &(_, _, run) = scenarios::ALL
        .iter()
        .find(|s| s.0 == scenario)
        .expect("unknown scenario");

    let mut ctx = Ctx::new(kind);
    let mut times = [0f64; RUNS];
    switch::activate(kind);
    for t in &mut times {
        let start = Instant::now();
        let ops = run(&mut ctx);
        *t = start.elapsed().as_nanos() as f64 / ops as f64;
        ctx.end_run();
    }
    switch::activate(Kind::System);

    times.sort_by(f64::total_cmp);
    let opt = |v: Option<usize>| v.map_or("-".to_string(), |x| x.to_string());
    let p = ctx.peak;
    println!(
        "RESULT {} {} {} {} {}",
        times[RUNS / 2],
        opt(p.map(|u| u.requested)),
        opt(p.map(|u| u.used)),
        opt(p.and_then(|u| u.largest)),
        opt(p.map(|u| u.free)),
    );
}
