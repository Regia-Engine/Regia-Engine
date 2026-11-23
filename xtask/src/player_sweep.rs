// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask player-sweep` — the nightly DO27 player sweep (roadmap item 8).
//!
//! Four scenarios, each a child `do27_chase` so a crash does not kill the sweep:
//! a cold first launch (empty pipeline cache), a replay (default 10 minutes),
//! five clean quits, and a crash test. Each child writes spec 28 `summary.json`.
//!
//! The roster is this box's RTX 5080, the build server card 5, the GTX 1080 (only when
//! `REGIA_156_STABLE=1`) and the Arc. A machine that cannot be reached is a
//! skipped row, not a green run. The table is `.claude/orchestration/reports/sweep-<date>.md`.

use anyhow::{Context, Result, bail};
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Arguments for `cargo xtask player-sweep`.
#[derive(Args, Debug)]
pub struct SweepArgs {
    /// Directory for per-scenario `summary.json` files. Defaults to `<workspace>/target/player-sweep`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Measured seconds of the replay. 600 is the nightly 10-minute replay.
    #[arg(long, default_value_t = 600.0)]
    pub replay_seconds: f64,
    /// Output size `WxH` for the cold, replay and quit launches (`1920x1080` for cards judged at
    /// 1080p). Omitted: `do27_chase` uses 2560x1440.
    #[arg(long)]
    pub size: Option<String>,
    /// Clean short launches after the replay.
    #[arg(long, default_value_t = 5)]
    pub quits: u32,
    /// Do not probe or launch on remote machines.
    #[arg(long)]
    pub local_only: bool,
    /// Write the table without building or launching `do27_chase`.
    #[arg(long)]
    pub dry_run: bool,
    /// `do27_chase` binary. When omitted, this builds `-p lookdev --bin do27_chase --release`.
    #[arg(long)]
    pub bin: Option<PathBuf>,
    /// DO27 content directory. When omitted, `REGIA_DO27_CONTENT` is used.
    #[arg(long)]
    pub content: Option<PathBuf>,
    /// Per-launch ceiling, seconds (`timeout`). The replay needs more than its measured seconds.
    #[arg(long, default_value_t = 1_200)]
    pub timeout_seconds: u64,
    /// Present cold, replay and quits in a window. Uses `xvfb-run -a` when no display is set.
    #[arg(long)]
    pub window: bool,
    /// Date in the report name (`sweep-<date>.md`). Default is `date +%F`.
    #[arg(long)]
    pub date: Option<String>,
}

/// One machine on the roster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineSpec {
    /// Stable id in the table (`rtx-5080`).
    pub id: &'static str,
    /// Where it runs.
    pub place: Place,
}

/// Local adapter match, or a remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// This process's machine. Run when an adapter name contains `name_contains`.
    Local {
        /// Substring of the adapter name (`5080`).
        name_contains: &'static str,
    },
    /// Another box, over ssh.
    Remote {
        /// Env var that overrides the host. Empty when unset.
        host_env: Option<&'static str>,
        /// Host when the env var is unset.
        host_default: &'static str,
        /// `REGIA_GPU_INDEX` on that box.
        card: Option<u32>,
        /// Skip unless the caller says the box is stable (the .156 GTX 1080).
        requires_stable: bool,
    },
}

/// What a probe learned about a remote, or nothing for a local row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    /// ssh printed `ok` and the remote root and content env vars are set.
    RemoteOk,
    /// ssh did not succeed.
    Unreachable(String),
    /// The GTX 1080 gate is closed.
    NotStable,
    /// ssh worked and `REGIA_SWEEP_REMOTE_ROOT` or `REGIA_SWEEP_REMOTE_CONTENT` is unset.
    NoRemoteRoot(String),
}

/// Whether a machine's scenarios launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Launch. `gpu_index` becomes `REGIA_GPU_INDEX` when set.
    Run {
        /// Adapter index, when the roster names one.
        gpu_index: Option<u32>,
    },
    /// One note, copied onto every scenario row.
    Skip(String),
}

/// An adapter name the local match looks at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterBrief {
    /// `REGIA_GPU_INDEX`.
    pub index: usize,
    /// Adapter name.
    pub name: String,
}

/// Cold cache or the directory seeded by the cold launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    /// `REGIA_PIPELINE_CACHE_DIR` starts empty.
    Empty,
    /// The directory may already hold a cache.
    Warm,
}

/// One child launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// Directory name (`cold`, `replay`, `quit-1`, `crash`).
    pub scenario: String,
    /// Table row (`cold`, `replay`, `quits`, `crash`).
    pub group: &'static str,
    /// Arguments after the binary.
    pub args: Vec<String>,
    /// Which cache directory the child gets.
    pub cache: CacheMode,
    /// The child loads a map.
    pub needs_content: bool,
}

/// One cell of the posted table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepRow {
    /// Machine id.
    pub machine: String,
    /// Scenario group.
    pub scenario: String,
    /// `pass`, `fail`, `skip` or `planned`.
    pub status: String,
    /// Process exit code, or empty when the row did not launch.
    pub exit_code: String,
    /// `summary.json` path, or empty.
    pub summary: String,
    /// Why, or what the summary said.
    pub note: String,
}

/// The four machines, in table order.
#[must_use]
pub fn roster() -> Vec<MachineSpec> {
    vec![
        MachineSpec {
            id: "rtx-5080",
            place: Place::Local {
                name_contains: "5080",
            },
        },
        MachineSpec {
            id: "buildhost-5090",
            place: Place::Remote {
                host_env: Some("REGIA_BUILDHOST"),
                host_default: "buildhost",
                card: Some(5),
                requires_stable: false,
            },
        },
        MachineSpec {
            id: "gtx-1080",
            place: Place::Remote {
                host_env: None,
                host_default: "192.0.2.156",
                card: None,
                requires_stable: true,
            },
        },
        MachineSpec {
            id: "arc",
            place: Place::Remote {
                host_env: Some("REGIA_ARC_HOST"),
                host_default: "arc",
                card: None,
                requires_stable: false,
            },
        },
    ]
}

/// `true` when the caller set the .156 gate (`REGIA_156_STABLE=1`).
#[must_use]
pub fn gtx_1080_allowed(stable: bool) -> bool {
    stable
}

/// Index of the first adapter whose name contains `needle`.
#[must_use]
pub fn matching_index(adapters: &[AdapterBrief], needle: &str) -> Option<usize> {
    adapters
        .iter()
        .find(|adapter| adapter.name.contains(needle))
        .map(|adapter| adapter.index)
}

/// Host string for a machine. Remote env overrides win when they are non-empty.
#[must_use]
pub fn host_of(machine: &MachineSpec, env_host: Option<&str>) -> String {
    match &machine.place {
        Place::Local { .. } => "local".to_owned(),
        Place::Remote { host_default, .. } => env_host
            .filter(|host| !host.is_empty())
            .unwrap_or(*host_default)
            .to_owned(),
    }
}

/// Launch or skip. `stable` is the .156 gate. `local_only` skips every remote without a probe.
#[must_use]
pub fn decide(
    machine: &MachineSpec,
    adapters: &[AdapterBrief],
    reach: Reach,
    stable: bool,
    local_only: bool,
) -> Intent {
    match &machine.place {
        Place::Local { name_contains } => match matching_index(adapters, name_contains) {
            Some(index) => Intent::Run {
                gpu_index: u32::try_from(index).ok(),
            },
            None => Intent::Skip(format!("local adapter name does not contain {name_contains}")),
        },
        Place::Remote {
            requires_stable,
            card,
            ..
        } => {
            if local_only {
                return Intent::Skip("local-only".to_owned());
            }
            if *requires_stable && !gtx_1080_allowed(stable) {
                return Intent::Skip(
                    "skipped: .156 is not marked stable (set REGIA_156_STABLE=1)".to_owned(),
                );
            }
            match reach {
                Reach::RemoteOk => Intent::Run { gpu_index: *card },
                Reach::Unreachable(reason) => Intent::Skip(format!("unreachable: {reason}")),
                Reach::NotStable => {
                    Intent::Skip("skipped: .156 is not marked stable (set REGIA_156_STABLE=1)".to_owned())
                }
                Reach::NoRemoteRoot(reason) => Intent::Skip(reason),
            }
        }
    }
}

/// Child launches for one machine directory.
#[must_use]
pub fn plan_launches(replay_seconds: f64, quits: u32, window: bool, out_dir: &Path) -> Vec<Launch> {
    let mut launches = Vec::new();
    launches.push(Launch {
        scenario: "cold".to_owned(),
        group: "cold",
        args: chase_args("cold", out_dir, window, &["--frames", "2", "--warmup", "0"]),
        cache: CacheMode::Empty,
        needs_content: true,
    });
    let seconds = format!("{replay_seconds}");
    let warmup = if replay_seconds < 30.0 { "2" } else { "30" };
    launches.push(Launch {
        scenario: "replay".to_owned(),
        group: "replay",
        args: chase_args(
            "replay",
            out_dir,
            window,
            &["--seconds", &seconds, "--warmup", warmup],
        ),
        cache: CacheMode::Warm,
        needs_content: true,
    });
    for index in 1..=quits {
        let scenario = format!("quit-{index}");
        launches.push(Launch {
            scenario: scenario.clone(),
            group: "quits",
            args: chase_args(&scenario, out_dir, window, &["--frames", "1", "--warmup", "0"]),
            cache: CacheMode::Warm,
            needs_content: true,
        });
    }
    if quits == 0 {
        launches.push(Launch {
            scenario: "quits".to_owned(),
            group: "quits",
            args: Vec::new(),
            cache: CacheMode::Warm,
            needs_content: false,
        });
    }
    let mut crash = chase_args("crash", out_dir, false, &["--crash-test"]);
    crash.retain(|arg| arg != "--map" && arg != "switchbacks");
    launches.push(Launch {
        scenario: "crash".to_owned(),
        group: "crash",
        args: crash,
        cache: CacheMode::Empty,
        needs_content: false,
    });
    launches
}

/// Appends `--size <size>` to every launch that renders the map (not the crash row, which loads
/// no map). A `size` that is not `WxH` with positive integers is an error.
///
/// # Errors
///
/// `size` is not `<width>x<height>`.
pub fn with_size(launches: &mut [Launch], size: &str) -> Result<()> {
    let valid = size
        .split_once('x')
        .is_some_and(|(w, h)| w.parse::<u32>().is_ok_and(|w| w > 0) && h.parse::<u32>().is_ok_and(|h| h > 0));
    if !valid {
        bail!("--size must look like 1920x1080, got `{size}`");
    }
    for launch in launches
        .iter_mut()
        .filter(|launch| launch.needs_content && !launch.args.is_empty())
    {
        launch.args.push("--size".to_owned());
        launch.args.push(size.to_owned());
    }
    Ok(())
}

/// Seconds of a `replay_seconds` replay spent parked at the route end (zero when the route lasts
/// that long). Those frames are a standing camera, not driving.
#[must_use]
pub fn parked_tail_seconds(replay_seconds: f64) -> f64 {
    (replay_seconds - crate::sweep::SWITCHBACKS_ROUTE_M / crate::sweep::CHASE_SPEED_MPS).max(0.0)
}

fn chase_args(scenario: &str, out_dir: &Path, window: bool, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--scenario".to_owned(),
        scenario.to_owned(),
        "--map".to_owned(),
        "switchbacks".to_owned(),
        "--summary".to_owned(),
        out_dir.join(scenario).join("summary.json").display().to_string(),
    ];
    if window && scenario != "crash" {
        args.push("--window".to_owned());
    }
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    args
}

/// Warm-up frames the replay passes. A short evidence replay does not spend a second warming.
#[must_use]
pub fn replay_warmup(seconds: f64) -> u32 {
    if seconds < 30.0 { 2 } else { 30 }
}

/// Markdown table. Every machine id and scenario group in `rows` is printed as given.
#[must_use]
pub fn markdown(date: &str, rows: &[SweepRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Player sweep {date}\n\n"));
    out.push_str(
        "Cold first launch, replay, quits, and a crash test. Each launch writes spec 28 `summary.json`.\n\n",
    );
    out.push_str("| Machine | Scenario | Status | Exit | Summary | Note |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            cell(&row.machine),
            cell(&row.scenario),
            cell(&row.status),
            cell(&row.exit_code),
            cell(&row.summary),
            cell(&row.note)
        ));
    }
    out.push('\n');
    out
}

fn cell(text: &str) -> String {
    text.replace('|', "/").replace('\n', " ")
}

/// `.claude/orchestration/reports/sweep-<date>.md` under the workspace (gitignored).
#[must_use]
pub fn report_path(workspace: &Path, date: &str) -> PathBuf {
    workspace
        .join(".claude/orchestration/reports")
        .join(format!("sweep-{date}.md"))
}

/// `date +%F`, or `unknown-date` when the command cannot run.
#[must_use]
pub fn iso_date_from_clock() -> String {
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

/// Runs the sweep and writes the table.
///
/// # Errors
///
/// The report file could not be written, or a requested binary/content path is unusable.
/// A failed scenario is an exit code, not an error. Skipped machines are not failures.
pub fn run(args: &SweepArgs, workspace: &Path) -> Result<ExitCode> {
    let _span = tracing::info_span!("player_sweep").entered();
    if !args.replay_seconds.is_finite() || args.replay_seconds <= 0.0 {
        bail!("--replay-seconds must be a positive duration");
    }
    let parked = parked_tail_seconds(args.replay_seconds);
    if parked > 0.0 {
        println!(
            "player-sweep: the last {parked:.1} s of the {} s replay are parked at the route end, not driving",
            args.replay_seconds
        );
    }
    let date = args
        .date
        .clone()
        .filter(|date| !date.is_empty())
        .unwrap_or_else(iso_date_from_clock);
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| workspace.join("target/player-sweep"));
    let stable = std::env::var("REGIA_156_STABLE").ok().as_deref() == Some("1");
    let adapters = if args.dry_run {
        Vec::new()
    } else {
        local_adapters()
    };
    let mut rows = Vec::new();
    let mut failed = false;
    for machine in roster() {
        let reach = if args.dry_run {
            Reach::Unreachable("dry-run".to_owned())
        } else {
            probe(&machine, args.local_only)
        };
        let intent = if args.dry_run {
            dry_intent(&machine, args.local_only, stable)
        } else {
            decide(&machine, &adapters, reach, stable, args.local_only)
        };
        let machine_rows = match &intent {
            Intent::Skip(note) => skipped_rows(&machine, note),
            Intent::Run { gpu_index: _ } if args.dry_run => planned_rows(&machine),
            Intent::Run { gpu_index } => {
                let (ran, any_fail) = execute_machine(args, workspace, &out, &machine, *gpu_index)?;
                failed |= any_fail;
                ran
            }
        };
        rows.extend(machine_rows);
    }
    let text = markdown(&date, &rows);
    print!("{text}");
    let report = report_path(workspace, &date);
    write_report(&report, &text)?;
    write_report(&out.join(format!("sweep-{date}.md")), &text)?;
    println!("wrote {}", report.display());
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn dry_intent(machine: &MachineSpec, local_only: bool, stable: bool) -> Intent {
    match &machine.place {
        Place::Local { .. } => Intent::Run { gpu_index: None },
        Place::Remote { requires_stable, .. } if *requires_stable && !gtx_1080_allowed(stable) => {
            Intent::Skip("skipped: .156 is not marked stable (set REGIA_156_STABLE=1)".to_owned())
        }
        Place::Remote { .. } if local_only => Intent::Skip("local-only".to_owned()),
        Place::Remote { .. } => Intent::Run { gpu_index: None },
    }
}

fn skipped_rows(machine: &MachineSpec, note: &str) -> Vec<SweepRow> {
    groups()
        .into_iter()
        .map(|scenario| SweepRow {
            machine: machine.id.to_owned(),
            scenario: scenario.to_owned(),
            status: "skip".to_owned(),
            exit_code: String::new(),
            summary: String::new(),
            note: note.to_owned(),
        })
        .collect()
}

fn planned_rows(machine: &MachineSpec) -> Vec<SweepRow> {
    groups()
        .into_iter()
        .map(|scenario| SweepRow {
            machine: machine.id.to_owned(),
            scenario: scenario.to_owned(),
            status: "planned".to_owned(),
            exit_code: String::new(),
            summary: String::new(),
            note: "dry-run".to_owned(),
        })
        .collect()
}

fn groups() -> [&'static str; 4] {
    ["cold", "replay", "quits", "crash"]
}

fn local_adapters() -> Vec<AdapterBrief> {
    regia_render::list_adapters()
        .into_iter()
        .map(|adapter| AdapterBrief {
            index: adapter.index,
            name: adapter.name,
        })
        .collect()
}

fn probe(machine: &MachineSpec, local_only: bool) -> Reach {
    let Place::Remote {
        host_env,
        requires_stable,
        ..
    } = &machine.place
    else {
        return Reach::RemoteOk;
    };
    if local_only {
        return Reach::Unreachable("local-only".to_owned());
    }
    if *requires_stable && std::env::var("REGIA_156_STABLE").ok().as_deref() != Some("1") {
        return Reach::NotStable;
    }
    let host = host_of(
        machine,
        host_env.and_then(|key| std::env::var(key).ok()).as_deref(),
    );
    if !safe_token(&host) {
        return Reach::Unreachable(format!("host `{host}` is not a plain name"));
    }
    match Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=3",
            &host,
            "echo",
            "ok",
        ])
        .output()
    {
        Ok(out) if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("ok") => {
            if std::env::var_os("REGIA_SWEEP_REMOTE_ROOT").is_none() {
                Reach::NoRemoteRoot("reachable but REGIA_SWEEP_REMOTE_ROOT is unset".to_owned())
            } else if std::env::var_os("REGIA_SWEEP_REMOTE_CONTENT").is_none() {
                Reach::NoRemoteRoot("reachable but REGIA_SWEEP_REMOTE_CONTENT is unset".to_owned())
            } else {
                Reach::RemoteOk
            }
        }
        Ok(out) => Reach::Unreachable(format!("ssh exit {}", out.status)),
        Err(err) => Reach::Unreachable(err.to_string()),
    }
}

fn safe_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

fn execute_machine(
    args: &SweepArgs,
    workspace: &Path,
    out: &Path,
    machine: &MachineSpec,
    gpu_index: Option<u32>,
) -> Result<(Vec<SweepRow>, bool)> {
    let content = resolve_content(args.content.as_deref());
    let machine_dir = out.join(machine.id);
    fs::create_dir_all(&machine_dir).with_context(|| format!("creating {}", machine_dir.display()))?;
    let bin = if matches!(machine.place, Place::Local { .. }) {
        Some(ensure_bin(workspace, args.bin.as_deref())?)
    } else {
        None
    };
    let mut launches = plan_launches(args.replay_seconds, args.quits, args.window, &machine_dir);
    if let Some(size) = args.size.as_deref() {
        with_size(&mut launches, size)?;
    }
    let cold_cache = machine_dir.join("cache-cold");
    let warm_cache = machine_dir.join("cache-warm");
    fs::create_dir_all(&cold_cache)?;
    fs::create_dir_all(&warm_cache)?;
    let mut grouped: Vec<Vec<SweepRow>> = groups().iter().map(|_| Vec::new()).collect();
    let mut any_fail = false;
    for launch in &launches {
        if launch.group == "quits" && launch.args.is_empty() {
            push_group(
                &mut grouped,
                "quits",
                SweepRow {
                    machine: machine.id.to_owned(),
                    scenario: "quits".to_owned(),
                    status: "skip".to_owned(),
                    exit_code: String::new(),
                    summary: String::new(),
                    note: "quits is 0".to_owned(),
                },
            );
            continue;
        }
        if launch.needs_content && content.is_none() {
            push_group(
                &mut grouped,
                launch.group,
                SweepRow {
                    machine: machine.id.to_owned(),
                    scenario: launch.group.to_owned(),
                    status: "fail".to_owned(),
                    exit_code: String::new(),
                    summary: String::new(),
                    note: "REGIA_DO27_CONTENT is not a directory".to_owned(),
                },
            );
            any_fail = true;
            continue;
        }
        let cache_dir = match launch.cache {
            CacheMode::Empty => &cold_cache,
            CacheMode::Warm => &warm_cache,
        };
        let summary = summary_path(&launch.args);
        // A summary left by an earlier sweep must not stand in for this launch's (a child that
        // dies before writing one would otherwise be judged on last night's file).
        clear_summary(summary.as_deref());
        let log_path = machine_dir.join(format!("{}.log", launch.scenario));
        let code = launch_child(&ChildLaunch {
            args,
            workspace,
            machine,
            bin: bin.as_deref(),
            launch,
            cache_dir,
            log_path: &log_path,
            gpu_index,
        })?;
        if launch.scenario == "cold" && code == 0 {
            let _ = copy_cache_files(&cold_cache, &warm_cache);
        }
        let expect_panic = launch.group == "crash";
        let (status, note) = judge(code, summary.as_deref(), expect_panic);
        if status != "pass" {
            any_fail = true;
        }
        push_group(
            &mut grouped,
            launch.group,
            SweepRow {
                machine: machine.id.to_owned(),
                scenario: launch.group.to_owned(),
                status,
                exit_code: code.to_string(),
                summary: summary.unwrap_or_default(),
                note,
            },
        );
    }
    let rows = grouped
        .into_iter()
        .map(|group| collapse_group(machine.id, group))
        .collect();
    Ok((rows, any_fail))
}

fn push_group(grouped: &mut [Vec<SweepRow>], group: &str, row: SweepRow) {
    let index = groups().iter().position(|name| *name == group).unwrap_or(0);
    grouped[index].push(row);
}

fn collapse_group(machine: &str, rows: Vec<SweepRow>) -> SweepRow {
    if rows.is_empty() {
        return SweepRow {
            machine: machine.to_owned(),
            scenario: "quits".to_owned(),
            status: "skip".to_owned(),
            exit_code: String::new(),
            summary: String::new(),
            note: "no launches".to_owned(),
        };
    }
    if rows.len() == 1 {
        return rows.into_iter().next().unwrap_or_else(|| SweepRow {
            machine: machine.to_owned(),
            scenario: String::new(),
            status: "fail".to_owned(),
            exit_code: String::new(),
            summary: String::new(),
            note: "empty group".to_owned(),
        });
    }
    let pass = rows.iter().filter(|row| row.status == "pass").count();
    let status = if pass == rows.len() { "pass" } else { "fail" };
    let note = format!("{pass}/{} clean", rows.len());
    SweepRow {
        machine: machine.to_owned(),
        scenario: rows
            .first()
            .map(|row| row.scenario.clone())
            .unwrap_or_else(|| "quits".to_owned()),
        status: status.to_owned(),
        exit_code: rows
            .iter()
            .map(|row| row.exit_code.as_str())
            .collect::<Vec<_>>()
            .join(","),
        summary: rows.last().map(|row| row.summary.clone()).unwrap_or_default(),
        note,
    }
}

fn summary_path(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "--summary")
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// Deletes a launch's summary file before the launch. A missing file is fine.
fn clear_summary(path: Option<&str>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

fn judge(code: i32, summary: Option<&str>, expect_panic: bool) -> (String, String) {
    if code == 124 {
        return ("fail".to_owned(), "timed out".to_owned());
    }
    let Some(path) = summary else {
        return ("fail".to_owned(), "no summary path".to_owned());
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => return ("fail".to_owned(), format!("no summary.json: {err}")),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => return ("fail".to_owned(), format!("summary.json: {err}")),
    };
    if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return ("fail".to_owned(), "summary schema is not 1".to_owned());
    }
    let clean = value.pointer("/exit/clean").and_then(serde_json::Value::as_bool);
    let panic = value.pointer("/exit/panic").and_then(serde_json::Value::as_bool);
    if expect_panic {
        if code == 0 {
            return ("fail".to_owned(), "crash test exited 0".to_owned());
        }
        if panic == Some(true) && clean == Some(false) {
            return ("pass".to_owned(), "exit.panic true, exit.clean false".to_owned());
        }
        return (
            "fail".to_owned(),
            format!("crash summary clean={clean:?} panic={panic:?}"),
        );
    }
    if code != 0 {
        return ("fail".to_owned(), format!("exit {code}"));
    }
    if clean == Some(true) && panic != Some(true) {
        let cache = value
            .pointer("/shaders/cache")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        return ("pass".to_owned(), format!("exit.clean, cache {cache}"));
    }
    ("fail".to_owned(), format!("exit clean={clean:?} panic={panic:?}"))
}

/// Inputs for one `do27_chase` child. One struct so the launcher stays under the argument cap.
#[derive(Copy, Clone)]
struct ChildLaunch<'a> {
    args: &'a SweepArgs,
    workspace: &'a Path,
    machine: &'a MachineSpec,
    bin: Option<&'a Path>,
    launch: &'a Launch,
    cache_dir: &'a Path,
    log_path: &'a Path,
    gpu_index: Option<u32>,
}

fn launch_child(job: &ChildLaunch<'_>) -> Result<i32> {
    let ChildLaunch {
        args,
        workspace,
        machine,
        bin,
        launch,
        cache_dir,
        log_path,
        gpu_index,
    } = *job;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let log = fs::File::create(log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let err = log.try_clone()?;
    let timeout = args.timeout_seconds.max(1).to_string();
    let mut cmd = match &machine.place {
        Place::Local { .. } => {
            let bin = bin.context("local sweep has no do27_chase binary")?;
            let mut cmd = Command::new("timeout");
            cmd.arg("--signal=TERM").arg(&timeout).arg(bin);
            if let Some(dir) = crate::sweep::launch_dir(workspace) {
                cmd.current_dir(dir);
            }
            if args.window && launch.args.iter().any(|arg| arg == "--window") && !display_present() {
                // timeout wraps xvfb, which wraps the binary. Rebuild the command.
                cmd = Command::new("timeout");
                cmd.arg("--signal=TERM")
                    .arg(&timeout)
                    .arg("xvfb-run")
                    .arg("-a")
                    .arg(bin);
                if let Some(dir) = crate::sweep::launch_dir(workspace) {
                    cmd.current_dir(dir);
                }
            }
            cmd
        }
        Place::Remote { host_env, .. } => {
            let host = host_of(
                machine,
                host_env.and_then(|key| std::env::var(key).ok()).as_deref(),
            );
            let root = std::env::var("REGIA_SWEEP_REMOTE_ROOT").context("REGIA_SWEEP_REMOTE_ROOT")?;
            let remote_content =
                std::env::var("REGIA_SWEEP_REMOTE_CONTENT").context("REGIA_SWEEP_REMOTE_CONTENT")?;
            let remote_bin = format!("{root}/target/release/do27_chase");
            let remote_cache = format!(
                "target/player-sweep/{}/{}",
                machine.id,
                cache_dir.file_name().and_then(|n| n.to_str()).unwrap_or("cache")
            );
            let remote_summary = format!(
                "target/player-sweep/{}/{}/summary.json",
                machine.id, launch.scenario
            );
            let gpu = gpu_index
                .map(|index| format!("REGIA_GPU_INDEX={index} "))
                .unwrap_or_default();
            let mut remote = format!(
                "cd {} && mkdir -p {} && rm -f {} && env REGIA_PIPELINE_CACHE_DIR={} REGIA_DO27_CONTENT={} {gpu}VK_LOADER_DRIVERS_SELECT='*nvidia*' timeout --signal=TERM {} {}",
                shell_quote(&root),
                shell_quote(remote_summary.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(".")),
                shell_quote(&remote_summary),
                shell_quote(&remote_cache),
                shell_quote(&remote_content),
                shell_quote(&timeout),
                shell_quote(&remote_bin)
            );
            let mut args_iter = launch.args.iter();
            while let Some(arg) = args_iter.next() {
                remote.push(' ');
                if arg == "--summary" {
                    remote.push_str(&shell_quote("--summary"));
                    let _ = args_iter.next();
                    remote.push(' ');
                    remote.push_str(&shell_quote(&remote_summary));
                } else {
                    remote.push_str(&shell_quote(arg));
                }
            }
            let mut cmd = Command::new("ssh");
            cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", &host, &remote]);
            cmd
        }
    };
    if matches!(machine.place, Place::Local { .. }) {
        cmd.args(&launch.args);
        if let Some(content) = resolve_content(args.content.as_deref()) {
            cmd.env("REGIA_DO27_CONTENT", content);
        }
        cmd.env("REGIA_PIPELINE_CACHE_DIR", cache_dir);
        if let Some(index) = gpu_index {
            cmd.env("REGIA_GPU_INDEX", index.to_string());
        }
    }
    cmd.stdout(Stdio::from(log)).stderr(Stdio::from(err));
    let status = cmd
        .status()
        .with_context(|| format!("launching {}", launch.scenario))?;
    if matches!(machine.place, Place::Remote { .. })
        && let Some(local) = summary_path(&launch.args)
    {
        // The child wrote the summary on the remote. A missing copy fails the row in `judge`.
        let _ = fetch_remote_summary(machine, &launch.scenario, Path::new(&local));
    }
    Ok(status.code().unwrap_or(1))
}

fn fetch_remote_summary(machine: &MachineSpec, scenario: &str, local: &Path) -> Result<()> {
    let Place::Remote { host_env, .. } = &machine.place else {
        return Ok(());
    };
    let host = host_of(
        machine,
        host_env.and_then(|key| std::env::var(key).ok()).as_deref(),
    );
    let root = std::env::var("REGIA_SWEEP_REMOTE_ROOT")?;
    let remote = format!(
        "{root}/target/player-sweep/{}/{scenario}/summary.json",
        machine.id
    );
    if let Some(parent) = local.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("scp")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            &format!("{host}:{remote}"),
            &local.display().to_string(),
        ])
        .status()
        .context("scp summary.json")?;
    if status.success() {
        Ok(())
    } else {
        bail!("scp {remote} exited {status}");
    }
}

fn display_present() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn resolve_content(flag: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = flag {
        return path.is_dir().then(|| path.to_path_buf());
    }
    std::env::var_os("REGIA_DO27_CONTENT")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
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
    let path = workspace.join("target/release/do27_chase");
    if !path.is_file() {
        bail!("{} was not produced", path.display());
    }
    Ok(path)
}

fn copy_cache_files(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn write_report(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn a_crash_test_that_writes_no_summary_fails_even_after_a_passing_sweep() {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-sweep-clear-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("summary.json");
        let path_str = path.display().to_string();
        // Last night's crash test passed and left its summary behind.
        fs::write(&path, r#"{"schema":1,"exit":{"clean":false,"panic":true}}"#).expect("write");
        assert_eq!(judge(101, Some(&path_str), true).0, "pass");
        // Tonight's child dies (non-zero exit) before it writes a summary.
        clear_summary(Some(&path_str));
        let (status, note) = judge(139, Some(&path_str), true);
        clear_summary(Some(&path_str));
        clear_summary(None);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(status, "fail");
        assert!(note.starts_with("no summary.json"), "{note}");
    }
}
