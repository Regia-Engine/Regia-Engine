// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask showcase-table` — the repeatable showcase performance table (ADR-2444).
//!
//! Runs the prebuilt `showcase` host headless on one pinned card, once per quality tier, and
//! plays each camera *view* of the scene's views file through `render.out_start { timings: true }`
//! at the table's resolution (default 2560x1440). The host records the wall milliseconds of each
//! delivered frame's render and readback (`render_ms` in the render-out manifest); this command
//! turns them into per-view min / average / max fps and p99 frame time and writes
//! `showcase-table.md` and `showcase-table.json`.
//!
//! The run follows the card rules of `gpu-bench`: `REGIA_GPU_INDEX` names the card (6 is refused),
//! the host runs under the card's two locks (`gpu_bench::flock_args`), and `REGIA_GPU_UUID` (when
//! the caller sets it) is passed through to pin the physical card. With `--cpu` no card is selected; the run
//! must land on a software adapter and its rows are marked as CPU fallback numbers. A row that was
//! not measured says `not run`: nothing here is estimated.
//!
//! The views file (`perf_views.json`, default beside the scene) is
//! `{ "version": 1, "views": [ { "name": "canopy", "path": <camera.set_path "path">, "fov": [...] } ] }`:
//! each view's object (minus `name`) is the params of one `camera.set_path` call with `play: true`.

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::gpu_bench::{LOCK_HELD_ENV, flock_args, launch_takes_lock, validate_gpu_index};
use crate::sweep::write_atomic;

/// Arguments for `cargo xtask showcase-table`.
#[derive(Args, Debug)]
pub struct TableArgs {
    /// Scene directory the showcase host loads.
    #[arg(long, default_value = "samples/showcase/example_scene")]
    pub scene: PathBuf,
    /// Views file. Default: `perf_views.json` in the scene directory.
    #[arg(long)]
    pub views: Option<PathBuf>,
    /// Prebuilt `showcase` binary. Default: `target/release/showcase` (honors `CARGO_TARGET_DIR`).
    #[arg(long)]
    pub bin: Option<PathBuf>,
    /// Directory for `showcase-table.md`, `showcase-table.json` and the host logs.
    #[arg(long, default_value = "docs/perf")]
    pub out: PathBuf,
    /// Comma list of tiers: `ultra`, `high`, `low`, `deck`, `mobile-high`, `mobile-low`,
    /// `mobile-low-lite` and `mobile-high-lite` (the Mobile presets with the lite forward path
    /// forced on), or `all`.
    #[arg(long, default_value = "all")]
    pub tiers: String,
    /// Output size, `WIDTHxHEIGHT`.
    #[arg(long, default_value = "2560x1440")]
    pub size: String,
    /// Measured frames per view.
    #[arg(long, default_value_t = 240)]
    pub frames: u32,
    /// Warm-up frames per view (rendered, not measured).
    #[arg(long, default_value_t = 30)]
    pub warmup: u32,
    /// Software adapter run (lavapipe): no card, no lock; the rows are marked CPU fallback and the
    /// run fails if the host lands on a hardware adapter.
    #[arg(long)]
    pub cpu: bool,
    /// Ceiling for one tier's host in seconds (`timeout`).
    #[arg(long, default_value_t = 1800)]
    pub timeout_seconds: u64,
    /// Date stamped into the files. Default is `date +%F`.
    #[arg(long)]
    pub date: Option<String>,
    /// Write the planned table (every row `not run`) without launching anything or needing a card.
    #[arg(long)]
    pub dry_run: bool,
    /// Two-phase occlusion override sent to each tier's host before it measures
    /// (`render.set_occlusion { two_phase }`): `on` or `off`. Omitted: each tier's preset decides.
    /// Every measured row's note names the override (ADR-3286).
    #[arg(long, value_parser = parse_on_off)]
    pub two_phase: Option<bool>,
}

/// `on` / `off` (also `true` / `false`) for a switch flag.
///
/// # Errors
///
/// Any other word.
pub fn parse_on_off(text: &str) -> Result<bool, String> {
    match text.trim() {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        other => Err(format!("`{other}` is not `on` or `off`")),
    }
}

// The table's shared part (tiers, views file, statistics, Markdown and JSON) lives in the showcase library, so
// the interactive demo's benchmark writes the same files (ADR-3013).
pub use showcase::perf_table::{
    Machine, Row, Stamp, TIERS, TableSource, Tier, VIEW_FPS, VISBUFFER_REQUIREMENTS, View, ViewStats,
    is_software_adapter, json, markdown, not_run, view_stats,
};

/// Resolves a `--tiers` list ([`showcase::perf_table::select_tiers`]).
///
/// # Errors
///
/// A name is unknown or the list is empty.
pub fn select_tiers(list: &str) -> Result<Vec<Tier>> {
    showcase::perf_table::select_tiers(list).map_err(anyhow::Error::msg)
}

/// Reads a views file ([`showcase::perf_table::parse_views`]).
///
/// # Errors
///
/// Invalid JSON, a wrong `version`, no views, a duplicate or empty name, or a view without a
/// `path.keys` list of timed keys.
pub fn parse_views(text: &str) -> Result<Vec<View>> {
    showcase::perf_table::parse_views(text).map_err(anyhow::Error::msg)
}

/// The length check of a view ([`showcase::perf_table::check_view_length`]).
///
/// # Errors
///
/// The path ends before the last frame (`view_too_short`).
pub fn check_view_length(view: &View, warmup: u32, frames: u32) -> Result<()> {
    showcase::perf_table::check_view_length(view, warmup, frames).map_err(anyhow::Error::msg)
}

/// Parses `1440x2560`-style sizes.
///
/// # Errors
///
/// The text is not `WIDTHxHEIGHT` with both in `64..=16384`.
pub fn parse_size(text: &str) -> Result<(u32, u32)> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .with_context(|| format!("size `{text}` is not WIDTHxHEIGHT"))?;
    let w: u32 = w.trim().parse().with_context(|| format!("width in `{text}`"))?;
    let h: u32 = h.trim().parse().with_context(|| format!("height in `{text}`"))?;
    if !(64..=16384).contains(&w) || !(64..=16384).contains(&h) {
        bail!("size `{text}` is outside 64..=16384 a side");
    }
    Ok((w, h))
}

/// The driver stamp for the measured adapter: `nvidia_smi` (`NVIDIA <version>`, read from `nvidia-smi`) only
/// when the adapter the host reported is an NVIDIA one. On a host that also has NVIDIA cards, an Intel
/// or AMD row was stamped with the NVIDIA driver (the Arc on the build server, ADR-3957); it is then "not
/// recorded" (empty) instead.
#[must_use]
pub fn driver_for_adapter(adapter: &str, nvidia_smi: String) -> String {
    if adapter.to_ascii_lowercase().contains("nvidia") {
        nvidia_smi
    } else {
        String::new()
    }
}

/// Reads `render_ms` from a render-out manifest.
///
/// # Errors
///
/// The manifest is not JSON, the job did not finish `done`, or `render_ms` is missing, has the
/// wrong length, or contains a non-numeric, non-finite, or non-positive sample.
pub fn render_ms_from_manifest(text: &str, expected: usize) -> Result<Vec<f64>> {
    let doc: Value = serde_json::from_str(text).context("render-out manifest is not JSON")?;
    let status = doc.get("status").and_then(Value::as_str).unwrap_or("?");
    if status != "done" {
        bail!(
            "render-out job ended `{status}`: {}",
            doc.get("error")
                .and_then(Value::as_str)
                .unwrap_or("no error text")
        );
    }
    let list = doc
        .get("render_ms")
        .and_then(Value::as_array)
        .context("manifest has no `render_ms` (a host without `timings`?)")?;
    let ms: Vec<f64> = list.iter().filter_map(Value::as_f64).collect();
    if ms.len() != expected || ms.len() != list.len() {
        bail!("manifest has {} render_ms numbers, expected {expected}", ms.len());
    }
    if ms.iter().any(|value| !value.is_finite() || *value <= 0.0) {
        bail!("manifest `render_ms` values must be finite and positive");
    }
    Ok(ms)
}

fn default_bin(workspace: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"))
        .join("release/showcase")
}

/// Runs the table and writes the files under `--out`.
///
/// # Errors
///
/// Bad arguments, a missing binary or views file, an unset or forbidden card (without `--cpu`),
/// or an unwritable output. A tier or view that fails is a `not run` row with the reason in its
/// note, and the exit code is 1.
pub fn run(args: &TableArgs, workspace: &Path) -> Result<ExitCode> {
    let size = parse_size(&args.size)?;
    if args.frames == 0 {
        bail!("--frames must be above 0");
    }
    let tiers = select_tiers(&args.tiers)?;
    let views_path = args
        .views
        .clone()
        .unwrap_or_else(|| args.scene.join("perf_views.json"));
    let views = parse_views(
        &fs::read_to_string(&views_path)
            .with_context(|| format!("cannot read views file {}", views_path.display()))?,
    )?;
    for view in &views {
        check_view_length(view, args.warmup, args.frames)?;
    }
    let gpu_uuid = std::env::var("REGIA_GPU_UUID")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let gpu_index = if args.cpu || args.dry_run {
        std::env::var("REGIA_GPU_INDEX")
            .ok()
            .and_then(|v| validate_gpu_index(Some(&v)).ok())
    } else {
        Some(validate_gpu_index(
            std::env::var("REGIA_GPU_INDEX").ok().as_deref(),
        )?)
    };
    let date = args
        .date
        .clone()
        .or_else(|| crate::gpu_bench::command_text("date", &["+%F"], workspace))
        .unwrap_or_default();
    let mut stamp = Stamp {
        source: TableSource::Xtask,
        date,
        commit: crate::gpu_bench::command_text("git", &["rev-parse", "--short=12", "HEAD"], workspace)
            .or_else(|| {
                std::env::var("SHOWCASE_TABLE_COMMIT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "unknown".to_owned()),
        driver: String::new(),
        gpu_index: if args.cpu { None } else { gpu_index },
        gpu_uuid: if args.cpu { None } else { gpu_uuid },
        size,
        frames: args.frames,
        warmup: args.warmup,
        cpu: args.cpu,
        scene: args.scene.display().to_string(),
        machine: Machine::default(),
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut failed = false;
    if args.dry_run {
        for tier in &tiers {
            for view in &views {
                rows.push(not_run(tier, view, "dry run"));
            }
        }
    } else {
        let bin = args.bin.clone().unwrap_or_else(|| default_bin(workspace));
        if !bin.is_file() {
            bail!(
                "showcase binary {} not found (build `cargo build --release -p showcase`)",
                bin.display()
            );
        }
        if !args.cpu {
            stamp.driver = crate::gpu_bench::command_text(
                "nvidia-smi",
                &[
                    "--query-gpu=driver_version",
                    "--format=csv,noheader",
                    "-i",
                    &gpu_index.unwrap_or(0).to_string(),
                ],
                workspace,
            )
            .map(|v| format!("NVIDIA {v}"))
            .unwrap_or_default();
        }
        let ctx = Ctx {
            args,
            workspace,
            bin: &bin,
            views: &views,
            size,
            gpu_index,
        };
        for tier in &tiers {
            match measure_tier(&ctx, tier, &mut stamp) {
                Ok(tier_rows) => {
                    failed |= tier_rows.iter().any(|r| r.stats.is_none());
                    rows.extend(tier_rows);
                }
                Err(error) => {
                    failed = true;
                    let note = format!("{error:#}");
                    for view in &views {
                        rows.push(not_run(tier, view, &note));
                    }
                }
            }
        }
    }
    stamp.driver = driver_for_adapter(&stamp.machine.adapter, std::mem::take(&mut stamp.driver));
    fs::create_dir_all(&args.out).with_context(|| format!("cannot create {}", args.out.display()))?;
    write_atomic(
        &args.out.join("showcase-table.md"),
        &markdown(&stamp, &tiers, &views, &rows),
    )?;
    write_atomic(
        &args.out.join("showcase-table.json"),
        &json(&stamp, &tiers, &views, &rows),
    )?;
    println!("wrote {}", args.out.join("showcase-table.md").display());
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Stops the host when dropped, whatever happened.
///
/// The child is the `flock` / `timeout` wrapper, started in its own process group (see
/// `host_command`); the drop kills that whole group, so the showcase under the wrappers ends
/// with them and never keeps rendering on the card after its lock is released.
struct Host(Child);

impl Drop for Host {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-s", "KILL", "--", &format!("-{}", self.0.id())])
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The answer deadline of a tier's first agent call, in milliseconds: the tier's own time
/// limit, at least the agent's 30 s default, and under the client's 600 s read timeout.
fn first_answer_ms(timeout_seconds: u64) -> u64 {
    timeout_seconds.clamp(30, 570) * 1000
}

/// What every tier's run shares.
struct Ctx<'a> {
    args: &'a TableArgs,
    workspace: &'a Path,
    bin: &'a Path,
    views: &'a [View],
    size: (u32, u32),
    gpu_index: Option<u32>,
}

fn measure_tier(ctx: &Ctx<'_>, tier: &Tier, stamp: &mut Stamp) -> Result<Vec<Row>> {
    let project = std::env::temp_dir().join(format!("showcase-table-{}-{}", std::process::id(), tier.name));
    let _ = fs::remove_dir_all(&project);
    fs::create_dir_all(&project).with_context(|| format!("cannot create {}", project.display()))?;
    let result = measure_tier_in(ctx, tier, stamp, &project);
    let _ = fs::remove_dir_all(&project);
    result
}

/// The showcase host's command line for one tier: `flock <card lock> timeout --foreground <s>
/// <bin> <args>` (or without `flock` when the caller holds the card's lock), the tier's
/// environment, started in the workspace, or in this process's working directory when the
/// compile-time workspace is not on this machine (a copied `xtask` on a test box).
///
/// On Unix the wrapper starts a new process group, and `--foreground` keeps `timeout` (which
/// otherwise makes itself a group leader) and the showcase in that group: `Host`'s drop ends
/// all of them with one group kill. `timeout` still ends the showcase when its time is up.
fn host_command(
    bin: &Path,
    host_args: &[String],
    workspace: &Path,
    gpu_index: Option<u32>,
    lock_held: Option<&str>,
    timeout_seconds: u64,
    tier: &Tier,
) -> Command {
    let mut command = match gpu_index {
        Some(index) if launch_takes_lock(index, lock_held) => {
            let mut c = Command::new("flock");
            c.args(flock_args(index))
                .arg("timeout")
                .arg("--foreground")
                .arg(timeout_seconds.to_string())
                .arg(bin);
            c
        }
        _ => {
            let mut c = Command::new("timeout");
            c.arg("--foreground").arg(timeout_seconds.to_string()).arg(bin);
            c
        }
    };
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.args(host_args);
    if let Some(dir) = crate::sweep::launch_dir(workspace) {
        command.current_dir(dir);
    }
    if let Some(lite) = tier.forward_lite {
        command.env("REGIA_FORWARD_LITE", lite);
    }
    if let Some(index) = gpu_index {
        command.env("REGIA_GPU_INDEX", index.to_string());
    }
    command
}

fn measure_tier_in(ctx: &Ctx<'_>, tier: &Tier, stamp: &mut Stamp, project: &Path) -> Result<Vec<Row>> {
    let Ctx {
        args,
        workspace,
        bin,
        views,
        size,
        gpu_index,
    } = *ctx;
    let log_dir = args.out.join("logs");
    fs::create_dir_all(&log_dir)?;
    let log = fs::File::create(log_dir.join(format!("showcase-{}.log", tier.name)))?;
    let log_err = log.try_clone()?;
    let host_args: Vec<String> = vec![
        args.scene.display().to_string(),
        "--project-dir".into(),
        project.display().to_string(),
        "--render-profile".into(),
        tier.preset.into(),
        "--size".into(),
        format!("{}x{}", size.0, size.1),
    ];
    let held = std::env::var(LOCK_HELD_ENV).ok();
    let mut command = host_command(
        bin,
        &host_args,
        workspace,
        gpu_index,
        held.as_deref(),
        args.timeout_seconds,
        tier,
    );
    command.stdout(Stdio::from(log)).stderr(Stdio::from(log_err));
    let child = command.spawn().with_context(|| {
        format!(
            "cannot launch the showcase host {} (is {} installed?)",
            bin.display(),
            command.get_program().to_string_lossy()
        )
    })?;
    let mut host = Host(child);
    let mut client = connect(
        project,
        &mut host.0,
        Duration::from_secs(args.timeout_seconds.min(900)),
    )?;
    client.set_read_timeout(Some(Duration::from_secs(600))).ok();

    // The first call waits for the host's first frames: under load a Mesa driver can compile
    // pipelines for minutes (the Arc Pro B65 needed about 150 s at the build server load 400), longer
    // than the agent's 30 s default answer deadline.
    let caps = client
        .call(
            "render.get_capabilities",
            json!({ "timeout_ms": first_answer_ms(args.timeout_seconds) }),
        )
        .map_err(|e| anyhow::anyhow!("render.get_capabilities: {e}"))?;
    let machine = machine_from_caps(&caps);
    let software = is_software_adapter(&machine.adapter);
    if args.cpu && !software {
        bail!(
            "--cpu run landed on hardware adapter `{}` (point VK_ICD_FILENAMES at the lavapipe ICD)",
            machine.adapter
        );
    }
    if !args.cpu && software {
        bail!(
            "GPU run landed on software adapter `{}`; pin the card (REGIA_GPU_INDEX / REGIA_GPU_UUID)",
            machine.adapter
        );
    }
    if stamp.machine.adapter.is_empty() {
        stamp.machine = machine.clone();
    }
    // The host's profile must be the tier's preset: a silent fallback would label the wrong row.
    let profile = client
        .call("render.get_profile", json!({}))
        .map_err(|e| anyhow::anyhow!("render.get_profile: {e}"))?;
    let reported = profile.to_string();
    if !reported.contains(tier.preset) {
        bail!("host reports profile {reported}, not {}", tier.preset);
    }
    let mut override_note = String::new();
    if let Some(two_phase) = args.two_phase {
        client
            .call("render.set_occlusion", json!({ "two_phase": two_phase }))
            .map_err(|e| anyhow::anyhow!("render.set_occlusion: {e}"))?;
        override_note = format!(
            "two-phase occlusion {} (override)",
            if two_phase { "on" } else { "off" }
        );
    }
    wait_warm(&mut client, Duration::from_secs(900))?;

    let mut rows = Vec::new();
    for view in views {
        let note_row = |note: String| not_run(tier, view, &note);
        match measure_view(&mut client, project, view, args, size, &mut host.0) {
            Ok(ms) => match view_stats(&ms) {
                Some(stats) => rows.push(Row {
                    tier: tier.name.to_owned(),
                    view: view.name.clone(),
                    stats: Some(stats),
                    note: override_note.clone(),
                }),
                None => rows.push(note_row("no usable frame times".into())),
            },
            Err(error) => {
                // A job that is still running would refuse the next view.
                let _ = client.call("render.out_cancel", json!({}));
                rows.push(note_row(format!("{error:#}")));
            }
        }
    }
    let _ = client.call("quit.request", json!({}));
    Ok(rows)
}

/// Calls `method`, retrying while the busy host answers `F0006` (no answer in time: a slow first
/// frame or a long pipeline compile), up to `tries` attempts.
fn call_patient(
    client: &mut regia_agent::client::Client,
    method: &str,
    params: &Value,
    tries: u32,
) -> Result<Value> {
    let mut last = String::new();
    for _ in 0..tries.max(1) {
        match client.call(method, params.clone()) {
            Ok(value) => return Ok(value),
            Err(error) => {
                last = error.to_string();
                if !last.contains("F0006") && !matches!(error, regia_agent::client::ClientError::TimedOut) {
                    break;
                }
            }
        }
    }
    bail!("{method}: {last}")
}

fn machine_from_caps(caps: &Value) -> Machine {
    Machine {
        adapter: caps
            .get("adapter")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        backend: caps
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        storage_buffers: caps
            .pointer("/limits/max_storage_buffers_per_shader_stage")
            .and_then(Value::as_u64),
        multi_draw_indirect_count: caps
            .pointer("/features/multi_draw_indirect_count")
            .and_then(Value::as_bool),
        timestamp_query: caps.pointer("/features/timestamp_query").and_then(Value::as_bool),
        profile: String::new(),
    }
}

fn connect(project: &Path, child: &mut Child, limit: Duration) -> Result<regia_agent::client::Client> {
    let start = Instant::now();
    loop {
        if let Ok(client) = regia_agent::client::Client::connect_project(project) {
            return Ok(client);
        }
        if let Some(status) = child.try_wait()? {
            bail!("the showcase host exited ({status}) before its agent endpoint appeared; see the tier log");
        }
        if start.elapsed() > limit {
            bail!("no agent endpoint after {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn wait_warm(client: &mut regia_agent::client::Client, limit: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let warm = client
            .call("render.pipeline_warm", json!({}))
            .map_err(|e| anyhow::anyhow!("render.pipeline_warm: {e}"))?;
        if warm.pointer("/boot_background/idle").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if start.elapsed() > limit {
            bail!("the pipeline warm did not go idle in {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn measure_view(
    client: &mut regia_agent::client::Client,
    project: &Path,
    view: &View,
    args: &TableArgs,
    size: (u32, u32),
    child: &mut Child,
) -> Result<Vec<f64>> {
    let out = project.join(format!("view-{}", view.name));
    let mut path_params = view.params.clone();
    if let Some(object) = path_params.as_object_mut() {
        object.insert("play".into(), json!(true));
    }
    call_patient(client, "camera.set_path", &path_params, 20)?;
    call_patient(
        client,
        "render.out_start",
        &json!({
            "out": out.display().to_string(), "name": "v", "frames": args.frames,
            "width": size.0, "height": size.1, "fps": VIEW_FPS,
            "warmup_frames": args.warmup, "timings": true, "hud": false,
        }),
        20,
    )?;
    let start = Instant::now();
    loop {
        let progress = call_patient(client, "render.out_progress", &json!({}), 20)?;
        if matches!(
            progress.get("status").and_then(Value::as_str),
            Some("done" | "failed" | "cancelled")
        ) {
            break;
        }
        if let Some(status) = child.try_wait()? {
            bail!("the host exited ({status}) during view `{}`", view.name);
        }
        if start.elapsed() > Duration::from_secs(args.timeout_seconds) {
            bail!(
                "view `{}` did not finish in {} s",
                view.name,
                args.timeout_seconds
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let manifest =
        fs::read_to_string(out.join("v_render_out.json")).context("cannot read the render-out manifest")?;
    let ms = render_ms_from_manifest(&manifest, args.frames as usize);
    let _ = fs::remove_dir_all(&out);
    ms
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWS: &str = r#"{"version":1,"views":[
        {"name":"canopy","path":{"interp":"Linear","keys":[{"time":0,"position":[0,10,0]},{"time":6,"position":[5,10,0]}]},"look_at":{"Point":[0,5,10]}},
        {"name":"ground-1","path":{"interp":"Linear","keys":[{"time":0,"position":[0,2,0]},{"time":1,"position":[1,2,0]}]}}
    ]}"#;

    #[test]
    fn a_copied_xtask_starts_the_host_where_it_runs_when_the_workspace_is_absent() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let gone = here.join("never-created-workspace-dir");
        let bin = Path::new("/opt/regia/showcase");
        let args = vec!["scene".to_owned()];
        let high = TIERS.iter().find(|t| t.name == "high").expect("high tier");
        let absent = host_command(bin, &args, &gone, Some(3), Some("3"), 60, high);
        assert_eq!(
            absent.get_current_dir(),
            None,
            "a missing workspace is not the host's cwd"
        );
        assert_eq!(absent.get_program(), "timeout", "the caller holds card 3's lock");
        let present = host_command(bin, &args, here, Some(3), None, 60, high);
        assert_eq!(present.get_current_dir(), Some(here));
        assert_eq!(present.get_program(), "flock");
        let argv: Vec<_> = present
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            [
                "/tmp/regia-gpu-3.lock",
                "flock",
                "/tmp/ferrite-gpu-3.lock",
                "timeout",
                "--foreground",
                "60",
                "/opt/regia/showcase",
                "scene"
            ]
        );
    }

    /// `true` while `pid` is a live process (not gone, not a zombie waiting for its reaper).
    #[cfg(target_os = "linux")]
    fn alive(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next()))
            .is_some_and(|state| state != 'Z' && state != 'X')
    }

    /// A failed tier drops its `Host`. Dropping it must end the showcase and whatever it
    /// started, not only the `timeout` wrapper it runs under: a host left behind keeps rendering
    /// on the card after the card lock is gone (three were left on the build server card 5 by failed
    /// tables, 11-10).
    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_the_host_ends_every_process_under_the_timeout_wrapper() {
        let dir = regia_test_utils::unique_temp_dir("showcase-table-host-drop");
        let pid_file = dir.join("pid");
        let high = TIERS.iter().find(|t| t.name == "high").expect("high tier");
        // Stands in for the showcase and a process it started (a shader compiler, a worker): the
        // shell starts a child that lives far longer than the test and writes the child's PID.
        // The child is what the test watches: newer coreutils `timeout` ends its own direct child
        // when it is killed, but nothing ends a grandchild, nor anything under `flock`, except
        // the group kill.
        let script = format!(
            "sleep 600 & echo $! > {}.tmp && mv {0}.tmp {0} && wait",
            pid_file.display()
        );
        let args = vec!["-c".to_owned(), script];
        let mut command = host_command(Path::new("/bin/sh"), &args, &dir, None, None, 600, high);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let host = Host(command.spawn().expect("spawn the stand-in host"));
        let mut pid = None;
        for _ in 0..400 {
            if let Some(p) = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                pid = Some(p);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let pid = pid.expect("the stand-in host wrote its PID");
        assert_ne!(pid, host.0.id(), "the stand-in runs under the timeout wrapper");
        assert!(alive(pid), "the stand-in's child runs before the drop");
        drop(host);
        // The kill is synchronous; the orphaned process only needs its reaper, so poll a bounded
        // number of times for it to be gone.
        let mut gone = false;
        for _ in 0..400 {
            if !alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if !gone {
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(
            gone,
            "process {pid} under the timeout wrapper survived the Host drop"
        );
    }

    /// A slow first frame (pipeline compiles under load) must not fail the tier at the agent's
    /// 30 s default: the first call waits as long as the tier may run, within the client's read
    /// timeout.
    #[test]
    fn the_first_call_waits_for_the_tier_limit_within_the_read_timeout() {
        assert_eq!(first_answer_ms(240), 240_000);
        assert_eq!(
            first_answer_ms(5),
            30_000,
            "never shorter than the agent's default"
        );
        assert_eq!(
            first_answer_ms(3600),
            570_000,
            "under the client's 600 s read timeout"
        );
    }

    #[test]
    fn the_two_phase_switch_reads_on_and_off_only() {
        assert_eq!(parse_on_off("on"), Ok(true));
        assert_eq!(parse_on_off("false"), Ok(false));
        assert_eq!(parse_on_off(" off "), Ok(false));
        assert!(parse_on_off("maybe").is_err());
        assert!(parse_on_off("").is_err());
    }

    #[test]
    fn sizes_parse_and_reject_nonsense() {
        assert_eq!(parse_size("2560x1440").expect("size"), (2560, 1440));
        assert!(parse_size("2560").is_err());
        assert!(parse_size("10x10").is_err());
        assert!(parse_size("axb").is_err());
    }

    #[test]
    fn short_views_are_refused_with_a_code() {
        let views = parse_views(VIEWS).expect("views");
        assert!(
            check_view_length(&views[0], 30, 240).is_ok(),
            "6 s covers 270 frames at 60 Hz"
        );
        let error = check_view_length(&views[1], 30, 240).expect_err("1 s is short");
        assert!(error.to_string().starts_with("view_too_short"), "{error}");
    }

    #[test]
    fn stats_use_harmonic_average_and_nearest_rank() {
        let mut ms = vec![10.0; 98];
        ms.extend([40.0, 40.0]);
        let s = view_stats(&ms).expect("stats");
        assert_eq!(s.frames, 100);
        assert!((s.min_fps - 25.0).abs() < 1e-9);
        assert!((s.max_fps - 100.0).abs() < 1e-9);
        assert!((s.avg_fps - 1000.0 * 100.0 / 1060.0).abs() < 1e-9);
        assert!((s.p50_ms - 10.0).abs() < 1e-9);
        assert!(
            (s.p99_ms - 40.0).abs() < 1e-9,
            "rank 98.01 rounds to index 98, one of the two slow frames"
        );
        assert!(view_stats(&[]).is_none());
        assert!(view_stats(&[10.0, 0.0]).is_none());
        assert!(view_stats(&[10.0, f64::NAN]).is_none());
    }

    #[test]
    fn stats_match_a_sorted_rank_oracle_across_extreme_populations() {
        fn oracle(values: &[f64]) -> (f64, f64, f64, f64, f64) {
            let mut sorted = values.to_vec();
            sorted.sort_by(f64::total_cmp);
            let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
            let rank = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
            (
                1000.0 / sorted[sorted.len() - 1],
                1000.0 / mean,
                1000.0 / sorted[0],
                rank(0.5),
                rank(0.99),
            )
        }

        let populations = [
            vec![4.0],
            vec![2.0, 8.0],
            (1..=100).map(f64::from).collect(),
            vec![1.0, 1.0, 1.0, 500.0],
            vec![10.0, 10.0, 10_000.0, 10.0],
        ];
        for values in populations {
            let expected = oracle(&values);
            for candidate in [&values[..], &values.iter().rev().copied().collect::<Vec<_>>()[..]] {
                let stats = view_stats(candidate).expect("valid population");
                assert_eq!(stats.frames, candidate.len());
                for (actual, expected) in [
                    (stats.min_fps, expected.0),
                    (stats.avg_fps, expected.1),
                    (stats.max_fps, expected.2),
                    (stats.p50_ms, expected.3),
                    (stats.p99_ms, expected.4),
                ] {
                    assert!((actual - expected).abs() <= expected.abs() * 1e-12 + 1e-12);
                }
            }
        }

        let frames = [2.0, 5.0, 11.0, 20.0];
        let base = view_stats(&frames).expect("base");
        let scaled = view_stats(&frames.map(|ms| ms * 7.0)).expect("scaled");
        assert!((scaled.avg_fps - base.avg_fps / 7.0).abs() < 1e-12);
        assert!((scaled.min_fps - base.min_fps / 7.0).abs() < 1e-12);
        assert!((scaled.max_fps - base.max_fps / 7.0).abs() < 1e-12);
        assert!((scaled.p50_ms - base.p50_ms * 7.0).abs() < 1e-12);
        assert!((scaled.p99_ms - base.p99_ms * 7.0).abs() < 1e-12);
    }

    #[test]
    fn stats_reject_non_finite_published_values_and_invalid_samples() {
        for values in [
            vec![f64::MAX, f64::MAX],
            vec![f64::from_bits(1)],
            vec![1.0, 0.0],
            vec![1.0, -1.0],
            vec![1.0, f64::NAN],
            vec![1.0, f64::INFINITY],
            vec![1.0, f64::NEG_INFINITY],
        ] {
            let stats = view_stats(&values);
            assert!(
                stats.as_ref().is_none_or(|stats| {
                    [
                        stats.min_fps,
                        stats.avg_fps,
                        stats.max_fps,
                        stats.p50_ms,
                        stats.p99_ms,
                    ]
                    .into_iter()
                    .all(|value| value.is_finite() && value > 0.0)
                }),
                "non-finite or non-positive published statistic for {values:?}: {stats:?}"
            );
        }
        assert_eq!(view_stats(&[]), None);
        let huge = view_stats(&[f64::MAX / 2.0, f64::MAX / 2.0]).expect("finite statistics");
        assert!(huge.avg_fps.is_finite() && huge.avg_fps > 0.0);
    }

    #[test]
    fn manifest_times_need_a_done_job_and_the_right_count() {
        let ok = json!({"status": "done", "render_ms": [1.0, 2.0]}).to_string();
        assert_eq!(render_ms_from_manifest(&ok, 2).expect("ms"), vec![1.0, 2.0]);
        assert!(render_ms_from_manifest(&ok, 3).is_err());
        let failed = json!({"status": "failed", "error": "boom", "render_ms": [1.0]}).to_string();
        assert!(
            render_ms_from_manifest(&failed, 1)
                .expect_err("failed")
                .to_string()
                .contains("boom")
        );
        let missing = json!({"status": "done", "render_ms": null}).to_string();
        assert!(render_ms_from_manifest(&missing, 1).is_err());
        assert!(render_ms_from_manifest("not json", 1).is_err());
    }

    #[test]
    fn manifest_times_reject_invalid_status_shape_count_and_values() {
        for status in ["incomplete", "error"] {
            let doc = json!({"status": status, "render_ms": [1.0]}).to_string();
            assert!(render_ms_from_manifest(&doc, 1).is_err(), "status {status}");
        }
        for render_ms in [
            json!([1.0, 2.0]),
            json!([null]),
            json!(["1.0"]),
            json!([0.0]),
            json!([-1.0]),
        ] {
            let doc = json!({"status": "done", "render_ms": render_ms}).to_string();
            assert!(render_ms_from_manifest(&doc, 1).is_err(), "{doc}");
        }
        let valid = json!({"status": "done", "render_ms": [0.125, 4.5]}).to_string();
        assert_eq!(
            render_ms_from_manifest(&valid, 2).expect("valid samples"),
            [0.125, 4.5]
        );
    }

    #[test]
    fn the_shipped_example_views_parse_and_cover_the_default_run() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../samples/showcase/example_scene/perf_views.json");
        let views = parse_views(&fs::read_to_string(path).expect("perf_views.json")).expect("views");
        assert!(views.len() >= 3);
        for view in &views {
            check_view_length(view, 30, 240).expect("covers the default 30 + 240 frames");
        }
    }

    #[test]
    fn software_adapters_are_recognised() {
        assert!(is_software_adapter("llvmpipe (LLVM 18.1.3, 256 bits)"));
        assert!(is_software_adapter("Lavapipe"));
        assert!(!is_software_adapter("NVIDIA GeForce RTX 5080"));
    }

    fn stamp() -> Stamp {
        Stamp {
            date: "2026-11-04".into(),
            commit: "abc".into(),
            size: (2560, 1440),
            frames: 240,
            warmup: 30,
            scene: "scene".into(),
            ..Stamp::default()
        }
    }

    #[test]
    fn unmeasured_rows_say_not_run_and_no_number_appears() {
        let views = parse_views(VIEWS).expect("views");
        let tiers = select_tiers("ultra,low").expect("tiers");
        let rows: Vec<Row> = tiers
            .iter()
            .flat_map(|t| views.iter().map(move |v| not_run(t, v, "dry run")))
            .collect();
        let md = markdown(&stamp(), &tiers, &views, &rows);
        assert!(
            md.contains("| canopy | not run | not run | not run | not run | not run | not run | dry run |")
        );
        assert!(md.contains("- GPU: not run"));
        assert!(md.contains("Driver: not recorded"));
        assert!(md.contains("No. From the code"));
        assert!(md.contains("Mac and Android rows"));
        let parsed: Value = serde_json::from_str(&json(&stamp(), &tiers, &views, &rows)).expect("json");
        let list = parsed["rows"].as_array().expect("rows");
        assert_eq!(list.len(), 4);
        assert!(
            list.iter()
                .all(|r| r["status"] == "not run" && r["avg_fps"].is_null())
        );
    }

    #[test]
    fn measured_rows_print_their_numbers_and_cpu_runs_are_marked() {
        let views = parse_views(VIEWS).expect("views");
        let tiers = select_tiers("high").expect("tiers");
        let stats = view_stats(&[8.0, 10.0, 12.5]).expect("stats");
        let rows = vec![Row {
            tier: "high".into(),
            view: "canopy".into(),
            stats: Some(stats),
            note: String::new(),
        }];
        let mut s = stamp();
        s.cpu = true;
        s.machine.adapter = "llvmpipe".into();
        s.machine.backend = "Vulkan".into();
        let md = markdown(&s, &tiers, &views, &rows);
        assert!(md.contains("CPU fallback numbers"));
        assert!(md.contains("| canopy | 3 | 80.0 |"), "{md}");
        assert!(md.contains("| ground-1 | not run |"));
    }

    #[test]
    fn the_nvidia_driver_stamps_only_nvidia_adapters() {
        let smi = || "NVIDIA 615.71.09".to_owned();
        assert_eq!(
            driver_for_adapter("NVIDIA GeForce RTX 5090", smi()),
            "NVIDIA 615.71.09"
        );
        assert_eq!(driver_for_adapter("Intel(R) Graphics (BMG G31)", smi()), "");
        assert_eq!(
            driver_for_adapter("AMD Radeon RX 7900 XTX (RADV NAVI31)", smi()),
            ""
        );
        assert_eq!(driver_for_adapter("", smi()), "");
        assert_eq!(driver_for_adapter("NVIDIA GeForce GTX 1080", String::new()), "");
    }
}
