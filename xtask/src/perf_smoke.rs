// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask perf-smoke`: five CPU microbenchmarks compared against a checked-in baseline with a
//! generous tolerance (`docs/06-testing-and-ci.md` § 10).
//!
//! The benchmarks are small deterministic kernels over the libraries the engine's hot paths lean on
//! (content hashing, cooked-payload and scene-text codecs) plus two numeric kernels (a matrix chain and
//! a sort). They call no engine API on purpose, so the check keeps working while the engine changes.
//! The number is the median of several samples. A bench fails only when it is more than `tolerance`
//! times slower than its baseline (default 3x), which catches an accidental order-of-magnitude
//! regression and not machine noise. Times are only comparable on the machine that recorded the
//! baseline; `--update` re-records it.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};

/// Default baseline file, relative to the repository root.
pub const DEFAULT_BASELINE: &str = "docs/perf/smoke-baseline.json";

/// Arguments of `cargo xtask perf-smoke`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Baseline file (default `docs/perf/smoke-baseline.json` under the repository root).
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Re-record the baseline from this run instead of comparing.
    #[arg(long)]
    pub update: bool,
    /// A bench fails when its median is more than this many times its baseline.
    #[arg(long, default_value_t = 3.0)]
    pub tolerance: f64,
    /// Timed samples per bench (the median is reported).
    #[arg(long, default_value_t = 15)]
    pub samples: usize,
}

/// The checked-in baseline: median nanoseconds per bench.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// File format version (1).
    pub version: u32,
    /// Where the numbers were recorded (free text: machine, date).
    #[serde(default)]
    pub recorded_on: String,
    /// Median nanoseconds by bench name; `null` means not recorded yet.
    pub entries: BTreeMap<String, Option<u64>>,
}

/// Verdict for one bench.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Within tolerance of the baseline.
    Pass,
    /// Slower than `tolerance` times the baseline.
    Fail,
    /// Faster than `1 / tolerance` of the baseline: not a failure, but the baseline is stale.
    Faster,
    /// The baseline has no number for this bench.
    NoBaseline,
}

/// One comparison row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Bench name.
    pub name: String,
    /// Measured median, nanoseconds.
    pub now_ns: u64,
    /// Baseline median, nanoseconds, when recorded.
    pub base_ns: Option<u64>,
    /// The verdict.
    pub verdict: Verdict,
}

/// Compares measured medians with a baseline. Pure, so it is unit-tested.
pub fn compare(baseline: &Baseline, measured: &[(String, u64)], tolerance: f64) -> Vec<Row> {
    measured
        .iter()
        .map(|(name, now)| {
            let base = baseline.entries.get(name).copied().flatten();
            let verdict = match base {
                None | Some(0) => Verdict::NoBaseline,
                Some(b) => {
                    let ratio = *now as f64 / b as f64;
                    if ratio > tolerance {
                        Verdict::Fail
                    } else if ratio < 1.0 / tolerance {
                        Verdict::Faster
                    } else {
                        Verdict::Pass
                    }
                }
            };
            Row {
                name: name.clone(),
                now_ns: *now,
                base_ns: base,
                verdict,
            }
        })
        .collect()
}

/// A prepared bench: calling it does one iteration and returns a checksum of the work.
type Iteration = Box<dyn FnMut() -> u64>;

/// A named bench with its setup (untimed).
struct Bench {
    name: &'static str,
    setup: fn() -> Iteration,
}

/// xorshift64 step: deterministic input data without a dependency.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn setup_blake3() -> Iteration {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let data: Vec<u8> = (0..1 << 20).map(|_| xorshift(&mut state) as u8).collect();
    Box::new(move || {
        let hash = blake3::hash(black_box(&data));
        let (head, _) = hash.as_bytes().split_at(8);
        let mut word = [0u8; 8];
        word.copy_from_slice(head);
        u64::from_le_bytes(word)
    })
}

fn setup_matrix_chain() -> Iteration {
    let mut state = 0x1234_5678_9ABC_DEF1;
    let mut make = || {
        let mut m = [[0.0f32; 4]; 4];
        for row in &mut m {
            for v in row {
                // Small values around zero keep the product finite.
                *v = (xorshift(&mut state) % 2001) as f32 / 1000.0 - 1.0;
            }
        }
        m
    };
    let a = make();
    let b = make();
    Box::new(move || {
        let mut acc = a;
        for _ in 0..20_000 {
            let mut out = [[0.0f32; 4]; 4];
            for (i, out_row) in out.iter_mut().enumerate() {
                for (j, cell) in out_row.iter_mut().enumerate() {
                    let mut sum = 0.0;
                    for (k, b_row) in b.iter().enumerate() {
                        sum += acc[i][k] * b_row[j];
                    }
                    *cell = sum;
                }
            }
            // Renormalize so values neither overflow nor vanish.
            let scale: f32 = out.iter().flatten().map(|v| v.abs()).fold(1e-6, f32::max);
            for v in out.iter_mut().flatten() {
                *v /= scale;
            }
            acc = black_box(out);
        }
        acc.iter()
            .flatten()
            .fold(0u64, |h, v| h.rotate_left(5) ^ u64::from(v.to_bits()))
    })
}

fn setup_sort() -> Iteration {
    let mut state = 0xDEAD_BEEF_CAFE_F00D;
    let data: Vec<u64> = (0..100_000).map(|_| xorshift(&mut state)).collect();
    Box::new(move || {
        let mut work = data.clone();
        work.sort_unstable();
        black_box(&work);
        work[0] ^ work[work.len() / 2] ^ work[work.len() - 1]
    })
}

/// A record shaped like a small cooked scene entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Record {
    id: u32,
    position: [f32; 3],
    name: String,
    tags: Vec<u16>,
}

fn records() -> Vec<Record> {
    let mut state = 0x0F0F_1234_ABCD_9876;
    (0..2000u32)
        .map(|id| Record {
            id,
            position: [
                (xorshift(&mut state) % 1000) as f32 * 0.5,
                (xorshift(&mut state) % 100) as f32 * 0.25,
                (xorshift(&mut state) % 1000) as f32 * 0.5,
            ],
            name: format!("entity_{id}"),
            tags: (0..(xorshift(&mut state) % 6) as u16).collect(),
        })
        .collect()
}

fn checksum_records(list: &[Record]) -> u64 {
    list.iter().fold(0u64, |h, r| {
        h.wrapping_mul(31)
            .wrapping_add(u64::from(r.id) + r.name.len() as u64 + r.tags.len() as u64)
    })
}

fn setup_postcard() -> Iteration {
    let list = records();
    Box::new(move || match postcard::to_stdvec(black_box(&list)) {
        Ok(bytes) => match postcard::from_bytes::<Vec<Record>>(&bytes) {
            Ok(back) => checksum_records(&back) ^ bytes.len() as u64,
            Err(_) => 0,
        },
        Err(_) => 0,
    })
}

fn setup_ron() -> Iteration {
    let list = records();
    let text = ron::to_string(&list).unwrap_or_default();
    Box::new(move || match ron::from_str::<Vec<Record>>(black_box(&text)) {
        Ok(back) => checksum_records(&back) ^ text.len() as u64,
        Err(_) => 0,
    })
}

const BENCHES: [Bench; 5] = [
    Bench {
        name: "blake3_1mib",
        setup: setup_blake3,
    },
    Bench {
        name: "mat4_chain_20k",
        setup: setup_matrix_chain,
    },
    Bench {
        name: "sort_u64_100k",
        setup: setup_sort,
    },
    Bench {
        name: "postcard_roundtrip_2k",
        setup: setup_postcard,
    },
    Bench {
        name: "ron_parse_2k",
        setup: setup_ron,
    },
];

/// Names of the five benches, in run order.
pub fn bench_names() -> Vec<&'static str> {
    BENCHES.iter().map(|b| b.name).collect()
}

/// Runs one bench once untimed and returns its checksum (used by the determinism test).
pub fn checksum(name: &str) -> Option<u64> {
    let bench = BENCHES.iter().find(|b| b.name == name)?;
    let mut iteration = (bench.setup)();
    Some(iteration())
}

/// Median of the timed samples, nanoseconds. Two untimed warm-up iterations come first.
fn measure(bench: &Bench, samples: usize) -> u64 {
    let mut iteration = (bench.setup)();
    for _ in 0..2 {
        black_box(iteration());
    }
    let mut times: Vec<u64> = (0..samples.max(1))
        .map(|_| {
            let start = Instant::now();
            black_box(iteration());
            start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
        })
        .collect();
    times.sort_unstable();
    times[times.len() / 2]
}

/// Reads a baseline file.
pub fn read_baseline(path: &Path) -> Result<Baseline> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let baseline: Baseline =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if baseline.version != 1 {
        bail!(
            "{}: unsupported baseline version {}",
            path.display(),
            baseline.version
        );
    }
    Ok(baseline)
}

fn write_baseline(path: &Path, baseline: &Baseline) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(baseline)?;
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Runs the benches in release mode (re-launching itself when this is a debug build) and compares.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    if cfg!(debug_assertions) {
        println!("perf-smoke: debug build, re-running through `cargo run --release -p xtask`");
        let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cmd.current_dir(root)
            .args(["run", "--release", "-p", "xtask", "--", "perf-smoke"]);
        if let Some(path) = &args.baseline {
            cmd.arg("--baseline").arg(path);
        }
        if args.update {
            cmd.arg("--update");
        }
        cmd.args([
            "--tolerance",
            &args.tolerance.to_string(),
            "--samples",
            &args.samples.to_string(),
        ]);
        let status = cmd.status().context("running cargo for perf-smoke")?;
        return Ok(if status.success() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if args.tolerance <= 1.0 {
        bail!("--tolerance must be greater than 1");
    }
    let path = args
        .baseline
        .clone()
        .unwrap_or_else(|| root.join(DEFAULT_BASELINE));
    let measured: Vec<(String, u64)> = BENCHES
        .iter()
        .map(|b| {
            let ns = measure(b, args.samples);
            println!("perf-smoke {}: median {} us", b.name, ns / 1000);
            (b.name.to_string(), ns)
        })
        .collect();
    if args.update {
        let baseline = Baseline {
            version: 1,
            recorded_on: std::env::var("REGIA_PERF_HOST").unwrap_or_default(),
            entries: measured.iter().map(|(n, ns)| (n.clone(), Some(*ns))).collect(),
        };
        write_baseline(&path, &baseline)?;
        println!("perf-smoke: baseline written to {}", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    let baseline = read_baseline(&path)?;
    let rows = compare(&baseline, &measured, args.tolerance);
    let mut failed = false;
    for row in &rows {
        let base = row
            .base_ns
            .map_or_else(|| "-".to_string(), |b| format!("{} us", b / 1000));
        let tag = match row.verdict {
            Verdict::Pass => "PASS",
            Verdict::Fail => {
                failed = true;
                "FAIL"
            }
            Verdict::Faster => "FASTER (baseline is stale; consider --update)",
            Verdict::NoBaseline => "NO-BASELINE (run --update on the reference machine)",
        };
        println!(
            "{:<24} now {:>8} us  base {:>10}  {tag}",
            row.name,
            row.now_ns / 1000,
            base
        );
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A fresh directory per call (pid + counter). xtask does not depend on `regia_test_utils`.
    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn baseline(entries: &[(&str, Option<u64>)]) -> Baseline {
        Baseline {
            version: 1,
            recorded_on: String::new(),
            entries: entries.iter().map(|(n, v)| ((*n).to_string(), *v)).collect(),
        }
    }

    fn verdict(base: Option<u64>, now: u64, tolerance: f64) -> Verdict {
        let rows = compare(&baseline(&[("a", base)]), &[("a".to_string(), now)], tolerance);
        rows[0].verdict
    }

    #[test]
    fn within_tolerance_passes_and_beyond_fails() {
        assert_eq!(verdict(Some(1000), 1000, 3.0), Verdict::Pass);
        assert_eq!(verdict(Some(1000), 3000, 3.0), Verdict::Pass);
        assert_eq!(verdict(Some(1000), 3001, 3.0), Verdict::Fail);
    }

    #[test]
    fn much_faster_is_flagged_not_failed() {
        assert_eq!(verdict(Some(1000), 333, 3.0), Verdict::Faster);
        assert_eq!(verdict(Some(1000), 400, 3.0), Verdict::Pass);
    }

    #[test]
    fn missing_or_zero_baseline_is_not_a_failure() {
        assert_eq!(verdict(None, 5, 3.0), Verdict::NoBaseline);
        assert_eq!(verdict(Some(0), 5, 3.0), Verdict::NoBaseline);
        let rows = compare(&baseline(&[]), &[("b".to_string(), 5)], 3.0);
        assert_eq!(rows[0].verdict, Verdict::NoBaseline);
    }

    #[test]
    fn there_are_five_uniquely_named_benches() {
        let names = bench_names();
        assert_eq!(names.len(), 5);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5);
    }

    #[test]
    fn every_bench_is_deterministic_and_does_real_work() {
        for name in bench_names() {
            let first = checksum(name);
            assert_eq!(first, checksum(name), "{name} checksum changed between runs");
            assert_ne!(first, Some(0), "{name} produced the failure checksum");
        }
    }

    #[test]
    fn baseline_round_trips_through_a_file() {
        let dir = temp_dir("perf_smoke");
        let path = dir.join("baseline.json");
        let original = baseline(&[("x", Some(7)), ("y", None)]);
        write_baseline(&path, &original).expect("write");
        assert_eq!(read_baseline(&path).expect("read"), original);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_baseline_version_is_refused() {
        let dir = temp_dir("perf_smoke_version");
        let path = dir.join("baseline.json");
        std::fs::write(&path, r#"{"version":2,"entries":{}}"#).expect("write");
        assert!(read_baseline(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_checked_in_baseline_names_exactly_the_benches() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let baseline = read_baseline(&root.join(DEFAULT_BASELINE)).expect("checked-in baseline");
        let mut have: Vec<&str> = baseline.entries.keys().map(String::as_str).collect();
        have.sort_unstable();
        let mut want = bench_names();
        want.sort_unstable();
        assert_eq!(have, want);
    }
}
