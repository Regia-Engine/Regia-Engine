// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask nightly-sweep` — the player sweep scenes at the standard profiles (sprint C29).
//!
//! Where `player-sweep` answers "does a player's session survive on each machine", this runs the
//! scenes (cold first launch, a replay, a clean quit) once per quality profile on the card the
//! caller already selected, and writes one summary row per profile and scene to `sweep.csv` and
//! `sweep.json` under `--out`. `.claude/orchestration/nightly-sweep.sh` wraps it with the farm's
//! card lock convention. Each scene is a child `do27_chase`, so a crash fails one row only.

use anyhow::{Context, Result, bail};
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Arguments for `cargo xtask nightly-sweep`.
#[derive(Args, Debug)]
pub struct NightlyArgs {
    /// Directory for `sweep.csv`, `sweep.json`, per-scene `summary.json` files and logs.
    #[arg(long)]
    pub out: PathBuf,
    /// Comma list of profiles: `high`, `mid`, `low` (2560x1440) and `high-1080p`, `mid-1080p`,
    /// `low-1080p` (1920x1080). `1080p` stands for the three 1080p profiles.
    #[arg(long, default_value = "high,mid,low")]
    pub profiles: String,
    /// Measured seconds of each replay scene.
    #[arg(long, default_value_t = 120.0)]
    pub replay_seconds: f64,
    /// Date stamped into the JSON. Default is `date +%F`.
    #[arg(long)]
    pub date: Option<String>,
    /// Write the planned rows without building or launching anything.
    #[arg(long)]
    pub dry_run: bool,
    /// `do27_chase` binary. When omitted, this builds `-p lookdev --bin do27_chase --release`.
    #[arg(long)]
    pub bin: Option<PathBuf>,
    /// DO27 content directory. When omitted, `REGIA_DO27_CONTENT` is used.
    #[arg(long)]
    pub content: Option<PathBuf>,
    /// Per-launch ceiling in seconds (`timeout`).
    #[arg(long, default_value_t = 900)]
    pub timeout_seconds: u64,
}

/// One standard quality profile: a name and the extra `do27_chase` arguments it adds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Name in the CSV (`high`).
    pub name: &'static str,
    /// Arguments appended to every scene of this profile.
    pub args: &'static [&'static str],
}

/// Every standard profile, in report order. `high` is the host defaults at native resolution,
/// `mid` adds the mid-range terrain budget, `low` adds the budget and renders at 67 % with TSR.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: "high",
        args: &[],
    },
    Profile {
        name: "mid",
        args: &["--tier", "mid"],
    },
    Profile {
        name: "low",
        args: &["--tier", "mid", "--scale", "0.67", "--upscaler", "tsr"],
    },
];

/// The 1080p profiles, for cards that are judged at 1080p (GTX 1080, Arc B65 class). Same tier and
/// scale as `high`, `mid` and `low`, only the output size differs. Listed separately from
/// `PROFILES` so the default sweep stays the three 1440p rows.
pub const PROFILES_1080P: &[Profile] = &[
    Profile {
        name: "high-1080p",
        args: &["--size", "1920x1080"],
    },
    Profile {
        name: "mid-1080p",
        args: &["--tier", "mid", "--size", "1920x1080"],
    },
    Profile {
        name: "low-1080p",
        args: &[
            "--tier",
            "mid",
            "--scale",
            "0.67",
            "--upscaler",
            "tsr",
            "--size",
            "1920x1080",
        ],
    },
];

/// Metres of the Switchbacks chase route (`do27_chase` measures it from the map; this is the
/// shipped map's length, 1728 m).
pub const SWITCHBACKS_ROUTE_M: f64 = 1728.0;

/// Vehicle speed of the chase, m/s (`lookdev::chase::SPEED`).
pub const CHASE_SPEED_MPS: f64 = 15.0;

/// Whole seconds a replay can drive before the vehicle parks at the route end: 115 s on
/// Switchbacks. Beyond it the camera stands still and held-view reuse starts, which is not driving.
#[must_use]
pub fn driving_seconds() -> f64 {
    (SWITCHBACKS_ROUTE_M / CHASE_SPEED_MPS).floor()
}

/// Replay length to run and the note for the row: `requested` itself when the route lasts that
/// long, else `driving_seconds` with a note naming the shortening.
#[must_use]
pub fn clamp_replay(requested: f64) -> (f64, String) {
    let limit = driving_seconds();
    if requested > limit {
        (
            limit,
            format!(
                "replay shortened from {requested} s to {limit} s: the route ends at {} s and the parked tail is not driving",
                SWITCHBACKS_ROUTE_M / CHASE_SPEED_MPS
            ),
        )
    } else {
        (requested, String::new())
    }
}

/// Whether a scene runs no warm-up frames (`--warmup 0`), so the spec 27 § 2.4 bind-group and
/// pipeline verdict does not apply to it (the first frames create everything).
#[must_use]
pub fn skips_creates_verdict(args: &[String]) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == "--warmup" && pair[1] == "0")
}

/// One scene of the sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    /// First launch with an empty pipeline cache, two frames.
    Cold,
    /// Long run on the warm cache.
    Replay,
    /// One frame on the warm cache, then a clean exit.
    Quit,
}

impl Scene {
    /// Every scene in run order. `Cold` runs first so it seeds the warm cache.
    pub const ALL: [Scene; 3] = [Scene::Cold, Scene::Replay, Scene::Quit];

    /// Name in the CSV and the summary directory.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Scene::Cold => "cold",
            Scene::Replay => "replay",
            Scene::Quit => "quit",
        }
    }
}

/// One planned launch.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// Profile name.
    pub profile: &'static str,
    /// Scene.
    pub scene: Scene,
    /// Arguments after the binary, including `--summary`.
    pub args: Vec<String>,
    /// Extra text for the row's note (a shortened replay), empty when there is none.
    pub note: String,
}

/// Numbers read from a scene's `summary.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metrics {
    /// Median frame time, ms.
    pub p50_ms: Option<f64>,
    /// 95th percentile frame time, ms.
    pub p95_ms: Option<f64>,
    /// 99th percentile frame time, ms.
    pub p99_ms: Option<f64>,
    /// Hitch count.
    pub hitches: Option<u64>,
    /// Session length, seconds.
    pub seconds: Option<f64>,
    /// Pipeline cache state the shaders line reported (`hit`, `cold`, ...).
    pub cache: Option<String>,
    /// `exit.clean`.
    pub clean: Option<bool>,
}

/// One CSV/JSON row.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepRow {
    /// Profile name.
    pub profile: String,
    /// Scene name.
    pub scene: String,
    /// `pass`, `fail` or `planned`.
    pub status: String,
    /// Process exit code, when the scene launched.
    pub exit_code: Option<i32>,
    /// Metrics from the summary, when it parsed.
    pub metrics: Metrics,
    /// Why the row has this status.
    pub note: String,
}

/// Parses the `--profiles` list. `1080p` expands to the three 1080p profiles.
///
/// # Errors
///
/// The list is empty or names a profile that is not in `PROFILES` or `PROFILES_1080P`.
pub fn parse_profiles(list: &str) -> Result<Vec<Profile>> {
    let mut out: Vec<Profile> = Vec::new();
    for name in list.split(',').map(str::trim).filter(|name| !name.is_empty()) {
        let found: Vec<&Profile> = if name == "1080p" {
            PROFILES_1080P.iter().collect()
        } else {
            PROFILES
                .iter()
                .chain(PROFILES_1080P)
                .filter(|profile| profile.name == name)
                .collect()
        };
        if found.is_empty() {
            bail!(
                "unknown profile `{name}` (known: high, mid, low, high-1080p, mid-1080p, low-1080p, 1080p)"
            );
        }
        for profile in found {
            if !out.contains(profile) {
                out.push(profile.clone());
            }
        }
    }
    if out.is_empty() {
        bail!("--profiles names no profile");
    }
    Ok(out)
}

/// The launches for `profiles`, scenes in order within each profile.
#[must_use]
pub fn plan(profiles: &[Profile], replay_seconds: f64, out: &Path) -> Vec<Job> {
    let (replay_seconds, clamp_note) = clamp_replay(replay_seconds);
    let mut jobs = Vec::new();
    for profile in profiles {
        for scene in Scene::ALL {
            let summary = out
                .join(profile.name)
                .join(scene.name())
                .join("summary.json")
                .display()
                .to_string();
            let mut args = vec![
                "--scenario".to_owned(),
                format!("{}-{}", profile.name, scene.name()),
                "--map".to_owned(),
                "switchbacks".to_owned(),
                "--summary".to_owned(),
                summary,
            ];
            let extra: Vec<String> = match scene {
                Scene::Cold => ["--frames", "2", "--warmup", "0"]
                    .iter()
                    .map(|arg| (*arg).to_owned())
                    .collect(),
                Scene::Replay => {
                    let warmup = if replay_seconds < 30.0 { "2" } else { "30" };
                    vec![
                        "--seconds".to_owned(),
                        format!("{replay_seconds}"),
                        "--warmup".to_owned(),
                        warmup.to_owned(),
                    ]
                }
                Scene::Quit => ["--frames", "1", "--warmup", "0"]
                    .iter()
                    .map(|arg| (*arg).to_owned())
                    .collect(),
            };
            args.extend(extra);
            args.extend(profile.args.iter().map(|arg| (*arg).to_owned()));
            let note = if scene == Scene::Replay {
                clamp_note.clone()
            } else {
                String::new()
            };
            jobs.push(Job {
                profile: profile.name,
                scene,
                args,
                note,
            });
        }
    }
    jobs
}

/// Reads the numbers out of a spec 28 `summary.json`.
///
/// # Errors
///
/// The text is not JSON.
pub fn parse_metrics(text: &str) -> Result<Metrics> {
    let value: serde_json::Value = serde_json::from_str(text).context("summary.json is not JSON")?;
    let num = |pointer: &str| value.pointer(pointer).and_then(serde_json::Value::as_f64);
    Ok(Metrics {
        p50_ms: num("/frames/p50_ms"),
        p95_ms: num("/frames/p95_ms"),
        p99_ms: num("/frames/p99_ms"),
        hitches: value
            .pointer("/hitches/count")
            .and_then(serde_json::Value::as_u64),
        seconds: num("/session/seconds"),
        cache: value
            .pointer("/shaders/cache")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        clean: value.pointer("/exit/clean").and_then(serde_json::Value::as_bool),
    })
}

/// Status and note for a finished scene.
#[must_use]
pub fn judge(exit_code: Option<i32>, metrics: Option<&Metrics>) -> (String, String) {
    match (exit_code, metrics) {
        (Some(124), _) => ("fail".to_owned(), "timed out".to_owned()),
        (None, _) => ("fail".to_owned(), "did not launch".to_owned()),
        (Some(code), _) if code != 0 => ("fail".to_owned(), format!("exit {code}")),
        (Some(_), None) => ("fail".to_owned(), "no readable summary.json".to_owned()),
        (Some(_), Some(metrics)) if metrics.clean == Some(true) => {
            ("pass".to_owned(), "exit.clean".to_owned())
        }
        (Some(_), Some(metrics)) => ("fail".to_owned(), format!("exit.clean is {:?}", metrics.clean)),
    }
}

/// Column names of `sweep.csv`, in order.
pub const CSV_HEADER: &str = "profile,scene,status,exit_code,p50_ms,p95_ms,p99_ms,hitches,seconds,cache,note";

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

fn opt<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// The CSV text: a header and one line per row (RFC 4180 quoting, `\n` line ends).
#[must_use]
pub fn csv(rows: &[SweepRow]) -> String {
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for row in rows {
        let fields = [
            csv_field(&row.profile),
            csv_field(&row.scene),
            csv_field(&row.status),
            opt(row.exit_code),
            opt(row.metrics.p50_ms),
            opt(row.metrics.p95_ms),
            opt(row.metrics.p99_ms),
            opt(row.metrics.hitches),
            opt(row.metrics.seconds),
            csv_field(row.metrics.cache.as_deref().unwrap_or("")),
            csv_field(&row.note),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// The JSON text: `{schema, date, passed, failed, rows[]}`, pretty-printed with a final newline.
#[must_use]
pub fn json(date: &str, rows: &[SweepRow]) -> String {
    let passed = rows.iter().filter(|row| row.status == "pass").count();
    let failed = rows.iter().filter(|row| row.status == "fail").count();
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "profile": row.profile,
                "scene": row.scene,
                "status": row.status,
                "exit_code": row.exit_code,
                "p50_ms": row.metrics.p50_ms,
                "p95_ms": row.metrics.p95_ms,
                "p99_ms": row.metrics.p99_ms,
                "hitches": row.metrics.hitches,
                "seconds": row.metrics.seconds,
                "cache": row.metrics.cache,
                "note": row.note,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "schema": 1,
        "date": date,
        "passed": passed,
        "failed": failed,
        "rows": rows,
    });
    let mut text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

/// Writes `text` to `path` through a sibling temp file and a rename.
///
/// # Errors
///
/// The directory, the write or the rename failed.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

fn iso_date_from_clock() -> String {
    Command::new("date")
        .arg("+%F")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown-date".to_owned())
}

fn ensure_bin(workspace: &Path, bin: Option<&Path>) -> Result<PathBuf> {
    if let Some(bin) = bin {
        if bin.is_file() {
            return Ok(bin.to_path_buf());
        }
        bail!("{} is not a do27_chase binary", bin.display());
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace)
        .args(["build", "--release", "-p", "lookdev", "--bin", "do27_chase"])
        .status()
        .context("building do27_chase")?;
    if !status.success() {
        bail!("building do27_chase failed");
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let path = target.join("release/do27_chase");
    if !path.is_file() {
        bail!("{} was not produced", path.display());
    }
    Ok(path)
}

/// Runs the sweep and writes `sweep.csv` and `sweep.json` under `--out`.
///
/// The card is whatever the caller selected through the environment (`REGIA_GPU_UUID`,
/// `REGIA_GPU_INDEX`, ...); this does not pick or lock one.
///
/// # Errors
///
/// A profile name is unknown, `--replay-seconds` is not positive, or an output file could not be
/// written. A failed scene is a `fail` row and a failing exit code, not an error.
pub fn run(args: &NightlyArgs, workspace: &Path) -> Result<ExitCode> {
    let _span = tracing::info_span!("nightly_sweep").entered();
    if !args.replay_seconds.is_finite() || args.replay_seconds <= 0.0 {
        bail!("--replay-seconds must be a positive duration");
    }
    let profiles = parse_profiles(&args.profiles)?;
    let date = args
        .date
        .clone()
        .filter(|date| !date.is_empty())
        .unwrap_or_else(iso_date_from_clock);
    let jobs = plan(&profiles, args.replay_seconds, &args.out);
    let mut rows = Vec::new();
    if args.dry_run {
        for job in &jobs {
            rows.push(SweepRow {
                profile: job.profile.to_owned(),
                scene: job.scene.name().to_owned(),
                status: "planned".to_owned(),
                exit_code: None,
                metrics: Metrics::default(),
                note: join_note("dry-run", &job.note),
            });
        }
    } else {
        let content = args
            .content
            .clone()
            .or_else(|| std::env::var_os("REGIA_DO27_CONTENT").map(PathBuf::from))
            .filter(|path| path.is_dir());
        let Some(content) = content else {
            bail!("no DO27 content directory: pass --content or set REGIA_DO27_CONTENT");
        };
        let bin = ensure_bin(workspace, args.bin.as_deref())?;
        rows = execute(args, workspace, &bin, &content, &jobs)?;
    }
    write_atomic(&args.out.join("sweep.csv"), &csv(&rows))?;
    write_atomic(&args.out.join("sweep.json"), &json(&date, &rows))?;
    let failed = rows.iter().filter(|row| row.status == "fail").count();
    println!(
        "nightly-sweep: {} rows, {failed} failed, wrote {}",
        rows.len(),
        args.out.display()
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `note` with the job's extra text after it, when there is any.
fn join_note(note: &str, extra: &str) -> String {
    if extra.is_empty() {
        note.to_owned()
    } else {
        format!("{note}; {extra}")
    }
}

/// The directory each child `do27_chase` runs in: the workspace, or `out` when the workspace is
/// not on this machine (a copied `xtask` binary on a test box without the source tree, whose
/// compile-time workspace path does not exist there).
#[must_use]
pub fn child_dir<'a>(workspace: &'a Path, out: &'a Path) -> &'a Path {
    if workspace.is_dir() { workspace } else { out }
}

/// The directory a pass command (`showcase-table`, `gpu-bench`, `player-sweep`) starts its host
/// in: the workspace when it exists, or `None` (the child inherits this process's working
/// directory, where a relative `--scene` or `--out` was resolved) when the compile-time
/// workspace is not on this machine. A copied `xtask` on a test box would otherwise fail every
/// launch with "No such file or directory" for the missing directory (GPU sweep v0.2.16).
#[must_use]
pub fn launch_dir(workspace: &Path) -> Option<&Path> {
    workspace.is_dir().then_some(workspace)
}

fn execute(
    args: &NightlyArgs,
    workspace: &Path,
    bin: &Path,
    content: &Path,
    jobs: &[Job],
) -> Result<Vec<SweepRow>> {
    let timeout = args.timeout_seconds.max(1).to_string();
    let cwd = child_dir(workspace, &args.out);
    let mut rows = Vec::new();
    let mut last_profile = "";
    let mut cache_dir = args.out.join("cache");
    for job in jobs {
        let profile_dir = args.out.join(job.profile);
        if job.profile != last_profile {
            // Each profile starts from an empty cache: the cold scene seeds it for the rest.
            last_profile = job.profile;
            cache_dir = profile_dir.join("cache");
            let _ = fs::remove_dir_all(&cache_dir);
            fs::create_dir_all(&cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
        }
        let scene_dir = profile_dir.join(job.scene.name());
        fs::create_dir_all(&scene_dir).with_context(|| format!("creating {}", scene_dir.display()))?;
        let summary = scene_dir.join("summary.json");
        let _ = fs::remove_file(&summary);
        let log = fs::File::create(scene_dir.join("run.log"))
            .with_context(|| format!("creating the log in {}", scene_dir.display()))?;
        let err = log.try_clone()?;
        let status = Command::new("timeout")
            .arg("--signal=TERM")
            .arg(&timeout)
            .arg(bin)
            .args(&job.args)
            .current_dir(cwd)
            .env("REGIA_DO27_CONTENT", content)
            .env("REGIA_PIPELINE_CACHE_DIR", &cache_dir)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .status();
        let exit_code = status.ok().and_then(|status| status.code());
        let metrics = fs::read_to_string(&summary)
            .ok()
            .and_then(|text| parse_metrics(&text).ok());
        let (status, note) = judge(exit_code, metrics.as_ref());
        rows.push(SweepRow {
            profile: job.profile.to_owned(),
            scene: job.scene.name().to_owned(),
            status,
            exit_code,
            metrics: metrics.unwrap_or_default(),
            note: join_note(&note, &job.note),
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn row(profile: &str, scene: &str, status: &str, note: &str) -> SweepRow {
        SweepRow {
            profile: profile.to_owned(),
            scene: scene.to_owned(),
            status: status.to_owned(),
            exit_code: Some(0),
            metrics: Metrics {
                p50_ms: Some(8.5),
                p95_ms: Some(12.25),
                p99_ms: Some(20.0),
                hitches: Some(3),
                seconds: Some(120.0),
                cache: Some("warm".to_owned()),
                clean: Some(true),
            },
            note: note.to_owned(),
        }
    }

    #[test]
    fn parse_profiles_orders_dedups_and_rejects_unknown() {
        let got = parse_profiles("low, high,low").expect("valid list");
        let names: Vec<_> = got.iter().map(|profile| profile.name).collect();
        assert_eq!(names, ["low", "high"]);
        assert!(parse_profiles("ultra").is_err());
        assert!(parse_profiles(" , ").is_err());
    }

    #[test]
    fn plan_has_every_scene_per_profile_with_profile_args() {
        let profiles = parse_profiles("high,low").expect("valid list");
        let jobs = plan(&profiles, 90.0, Path::new("/out"));
        assert_eq!(jobs.len(), 6);
        let low_replay = jobs
            .iter()
            .find(|job| job.profile == "low" && job.scene == Scene::Replay)
            .expect("low replay");
        assert!(low_replay.args.windows(2).any(|pair| pair == ["--scale", "0.67"]));
        assert!(low_replay.args.windows(2).any(|pair| pair == ["--seconds", "90"]));
        assert!(low_replay.args.windows(2).any(|pair| pair == ["--warmup", "30"]));
        let high_cold = &jobs[0];
        assert_eq!(high_cold.scene, Scene::Cold);
        assert!(!high_cold.args.iter().any(|arg| arg == "--scale"));
        // The summary path goes to the child process as the host's native path.
        let summary = Path::new("high").join("cold").join("summary.json");
        assert!(
            high_cold
                .args
                .iter()
                .any(|arg| arg.ends_with(&summary.display().to_string()))
        );
    }

    #[test]
    fn the_1080p_profiles_add_the_size_and_keep_the_tier_and_scale() {
        let got = parse_profiles("1080p").expect("alias");
        let names: Vec<_> = got.iter().map(|profile| profile.name).collect();
        assert_eq!(names, ["high-1080p", "mid-1080p", "low-1080p"]);
        let jobs = plan(&got, 60.0, Path::new("/out"));
        assert_eq!(jobs.len(), 9);
        for job in &jobs {
            assert!(job.args.windows(2).any(|pair| pair == ["--size", "1920x1080"]));
        }
        let low = jobs
            .iter()
            .find(|job| job.profile == "low-1080p" && job.scene == Scene::Replay)
            .expect("low replay");
        assert!(low.args.windows(2).any(|pair| pair == ["--scale", "0.67"]));
        assert!(low.args.windows(2).any(|pair| pair == ["--tier", "mid"]));
        let mid = jobs.iter().find(|job| job.profile == "mid-1080p").expect("mid");
        assert!(!mid.args.iter().any(|arg| arg == "--scale"));
        let summary = Path::new("mid-1080p").join("cold").join("summary.json");
        assert!(
            mid.args
                .iter()
                .any(|arg| arg.ends_with(&summary.display().to_string()))
        );
    }

    #[test]
    fn the_default_profiles_stay_at_native_size_and_names_mix() {
        let jobs = plan(&parse_profiles("high").expect("valid"), 60.0, Path::new("/out"));
        assert!(!jobs.iter().any(|job| job.args.iter().any(|arg| arg == "--size")));
        let mixed = parse_profiles("high,high-1080p,1080p").expect("valid");
        let names: Vec<_> = mixed.iter().map(|profile| profile.name).collect();
        assert_eq!(names, ["high", "high-1080p", "mid-1080p", "low-1080p"]);
    }

    #[test]
    fn a_replay_longer_than_the_route_is_shortened_and_noted() {
        assert_eq!(driving_seconds(), 115.0);
        let jobs = plan(&parse_profiles("high").expect("valid"), 120.0, Path::new("/out"));
        let replay = &jobs[1];
        assert!(replay.args.windows(2).any(|pair| pair == ["--seconds", "115"]));
        assert!(replay.note.contains("shortened from 120 s to 115 s"));
        assert!(jobs[0].note.is_empty() && jobs[2].note.is_empty());
        let (kept, note) = clamp_replay(100.0);
        assert_eq!(kept, 100.0);
        assert!(note.is_empty());
        assert_eq!(clamp_replay(115.0).0, 115.0);
    }

    #[test]
    fn the_shortened_note_reaches_the_dry_run_row() {
        assert_eq!(join_note("exit.clean", ""), "exit.clean");
        assert_eq!(join_note("exit.clean", "x"), "exit.clean; x");
    }

    #[test]
    fn warmup_zero_scenes_skip_the_creates_verdict() {
        let jobs = plan(&parse_profiles("high").expect("valid"), 120.0, Path::new("/out"));
        assert!(skips_creates_verdict(&jobs[0].args), "cold");
        assert!(!skips_creates_verdict(&jobs[1].args), "replay");
        assert!(skips_creates_verdict(&jobs[2].args), "quit");
        assert!(!skips_creates_verdict(&[]));
    }

    #[test]
    fn short_replay_uses_the_short_warmup() {
        let profiles = parse_profiles("high").expect("valid list");
        let jobs = plan(&profiles, 10.0, Path::new("/out"));
        assert!(jobs[1].args.windows(2).any(|pair| pair == ["--warmup", "2"]));
    }

    #[test]
    fn parse_metrics_reads_the_spec_28_fields() {
        let text = r#"{"schema":1,"session":{"seconds":12.5},
            "frames":{"p50_ms":8.0,"p95_ms":11.0,"p99_ms":15.5},
            "hitches":{"count":4},"shaders":{"cache":"hit"},"exit":{"clean":true}}"#;
        let metrics = parse_metrics(text).expect("parses");
        assert_eq!(metrics.p50_ms, Some(8.0));
        assert_eq!(metrics.p99_ms, Some(15.5));
        assert_eq!(metrics.hitches, Some(4));
        assert_eq!(metrics.seconds, Some(12.5));
        assert_eq!(metrics.cache.as_deref(), Some("hit"));
        assert_eq!(metrics.clean, Some(true));
        assert!(parse_metrics("not json").is_err());
        assert_eq!(parse_metrics("{}").expect("empty object"), Metrics::default());
    }

    #[test]
    fn judge_covers_timeout_exit_and_missing_summary() {
        let clean = Metrics {
            clean: Some(true),
            ..Metrics::default()
        };
        assert_eq!(judge(Some(0), Some(&clean)).0, "pass");
        assert_eq!(judge(Some(124), Some(&clean)).1, "timed out");
        assert_eq!(judge(Some(3), Some(&clean)).1, "exit 3");
        assert_eq!(judge(Some(0), None).0, "fail");
        assert_eq!(judge(None, None).1, "did not launch");
        assert_eq!(judge(Some(0), Some(&Metrics::default())).0, "fail");
    }

    #[test]
    fn csv_has_header_and_quotes_awkward_fields() {
        let rows = vec![
            row("high", "cold", "pass", "exit.clean"),
            row("mid", "replay", "fail", "said \"no\", twice\nnewline"),
        ];
        let text = csv(&rows);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(CSV_HEADER));
        assert_eq!(
            lines.next(),
            Some("high,cold,pass,0,8.5,12.25,20,3,120,warm,exit.clean")
        );
        assert!(text.contains("\"said \"\"no\"\", twice\nnewline\""));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn csv_leaves_missing_numbers_empty() {
        let planned = SweepRow {
            profile: "low".to_owned(),
            scene: "quit".to_owned(),
            status: "planned".to_owned(),
            exit_code: None,
            metrics: Metrics::default(),
            note: "dry-run".to_owned(),
        };
        assert_eq!(
            csv(&[planned]).lines().nth(1),
            Some("low,quit,planned,,,,,,,,dry-run")
        );
    }

    #[test]
    fn json_round_trips_counts_and_values() {
        let rows = vec![
            row("high", "cold", "pass", "ok"),
            row("high", "replay", "fail", "exit 1"),
        ];
        let value: serde_json::Value = serde_json::from_str(&json("2026-10-21", &rows)).expect("valid JSON");
        assert_eq!(value["schema"], 1);
        assert_eq!(value["date"], "2026-10-21");
        assert_eq!(value["passed"], 1);
        assert_eq!(value["failed"], 1);
        assert_eq!(value["rows"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["rows"][0]["p95_ms"], 12.25);
        assert_eq!(value["rows"][1]["status"], "fail");
        let planned = SweepRow {
            exit_code: None,
            metrics: Metrics::default(),
            ..row("low", "quit", "planned", "dry-run")
        };
        let value: serde_json::Value = serde_json::from_str(&json("d", &[planned])).expect("valid JSON");
        assert!(value["rows"][0]["exit_code"].is_null());
        assert!(value["rows"][0]["p50_ms"].is_null());
    }

    #[test]
    fn write_atomic_creates_parents_and_leaves_no_temp_file() {
        let dir = temp_dir("nightly-sweep-write");
        let path = dir.join("nested/sweep.csv");
        write_atomic(&path, "a\n").expect("first write");
        write_atomic(&path, "b\n").expect("overwrite");
        assert_eq!(fs::read_to_string(&path).expect("read back"), "b\n");
        assert!(!dir.join("nested/sweep.csv.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_writes_planned_rows_for_every_job() {
        let dir = temp_dir("nightly-sweep-dry");
        let args = NightlyArgs {
            out: dir.join("out"),
            profiles: "high,mid".to_owned(),
            replay_seconds: 60.0,
            date: Some("2026-10-21".to_owned()),
            dry_run: true,
            bin: None,
            content: None,
            timeout_seconds: 10,
        };
        let code = run(&args, &dir).expect("dry run");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        let text = fs::read_to_string(dir.join("out/sweep.csv")).expect("csv written");
        assert_eq!(text.lines().count(), 1 + 6);
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("out/sweep.json")).expect("json written"))
                .expect("valid JSON");
        assert_eq!(value["rows"].as_array().map(Vec::len), Some(6));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn children_run_in_out_when_the_workspace_is_not_on_this_machine() {
        let dir = temp_dir("nightly-sweep-cwd");
        let out = dir.join("out");
        assert_eq!(child_dir(&dir, &out), dir.as_path());
        let gone = dir.join("never-created-workspace");
        assert_eq!(child_dir(&gone, &out), out.as_path());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosts_inherit_the_working_directory_when_the_workspace_is_not_on_this_machine() {
        let dir = temp_dir("launch-dir");
        assert_eq!(launch_dir(&dir), Some(dir.as_path()));
        assert_eq!(launch_dir(&dir.join("never-created-workspace")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_replay_seconds_is_an_error() {
        let dir = temp_dir("nightly-sweep-bad");
        let args = NightlyArgs {
            out: dir.join("out"),
            profiles: "high".to_owned(),
            replay_seconds: 0.0,
            date: None,
            dry_run: true,
            bin: None,
            content: None,
            timeout_seconds: 10,
        };
        assert!(run(&args, &dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
