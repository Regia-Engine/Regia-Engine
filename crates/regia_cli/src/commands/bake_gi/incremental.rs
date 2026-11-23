// SPDX-License-Identifier: MPL-2.0
//! `regia bake-gi … --incremental <STATE>` (ADR-1094, `regia_pt::bake_incremental`): the
//! bake reads the state file the previous bake left, traces only the cells (detail bricks,
//! terrain tiles) near what changed, and writes the new state back. `--plan-incremental` prints
//! what a bake would reuse and trace, and why, without tracing. Works with a built-in, a
//! `.bake.ron`, and `--from-scene … --bake`.
//!
//! The summary is `regia bake-gi`'s with an `incremental` object (the
//! `regia_pt::bake_incremental::IncrementalReport`), a `state` object (`path`, `written`,
//! `bytes`) and `warnings` (`bake_state_unreadable`: the state could not be used, every cell was
//! traced; `bake_state_write_failed`: the volume is written, the state is not, so the next bake
//! traces everything). A bad `--influence` or `--tile-columns` is a usage error; options built
//! in code fail with `bake_incremental_invalid`. Codes are listed in
//! `book/agents/baked-gi-incremental.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args;
use regia_asset::baked_gi::{BakedGiVolume, ProbeMeta};
use regia_pt::PathTracer;
use regia_pt::bake_desc::{BakeDescError, GiBakeJob};
use regia_pt::bake_incremental::{
    BakeState, DEFAULT_TILE_COLUMNS, IncrementalSettings, bake_gi_volume_incremental, plan_incremental,
};
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::probe::ProbeTracer;
use serde_json::{Value, json};

use super::{BakeBackend, BakeGiArgs, bake_job, layer_summary};

/// The `--incremental` options of `regia bake-gi` (ADR-1094).
#[derive(Debug, Clone, Default, Args)]
pub struct IncrementalArgs {
    /// Bake incrementally through this state file: read the state the previous bake left (none
    /// on the first bake), trace only the cells near what changed, write the new state back. A
    /// build cache, not content: keep it out of `assets/` (ADR-1094).
    #[arg(long, value_name = "STATE", conflicts_with_all = ["dry_run", "sample", "check_volume", "check"])]
    pub incremental: Option<PathBuf>,
    /// With `--incremental`: print which cells a bake would reuse and trace, and why, without
    /// tracing or writing (no GPU with `--cpu`; the GPU adapter is opened to name the device).
    #[arg(long, requires = "incremental", conflicts_with = "bake")]
    pub plan_incremental: bool,
    /// With `--incremental`: scene content within this many meters of a cell's probes rebakes
    /// the cell (default: the layout's `max_distance`).
    #[arg(long, value_name = "METERS", requires = "incremental", value_parser = parse_influence)]
    pub influence: Option<f32>,
    /// With `--incremental`: terrain-layer columns per tile edge (default 8).
    #[arg(
        long,
        value_name = "COLUMNS",
        requires = "incremental",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub tile_columns: Option<u32>,
}

impl IncrementalArgs {
    /// `--incremental` was given.
    #[must_use]
    pub fn active(&self) -> bool {
        self.incremental.is_some()
    }

    /// The bake's settings for traces on `device`.
    ///
    /// # Errors
    ///
    /// `bake_incremental_invalid`: a negative or non-finite `influence`, or `tile_columns` 0
    /// (options built in code; on the command line the parser refuses them first).
    pub fn settings(&self, device: String) -> Result<IncrementalSettings, BakeDescError> {
        if let Some(m) = self.influence
            && !(m.is_finite() && m >= 0.0)
        {
            return Err(invalid(format!(
                "--influence {m}: a distance in meters, 0 or more"
            )));
        }
        if self.tile_columns == Some(0) {
            return Err(invalid("--tile-columns 0: at least one column per tile".into()));
        }
        Ok(IncrementalSettings {
            influence_margin: self.influence,
            tile_columns: self.tile_columns.unwrap_or(DEFAULT_TILE_COLUMNS),
            device,
        })
    }
}

/// `--influence`: a finite distance, 0 or more (a usage error otherwise).
fn parse_influence(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(m) if m.is_finite() && m >= 0.0 => Ok(m),
        _ => Err(format!("`{s}`: a distance in meters, 0 or more")),
    }
}

fn invalid(message: String) -> BakeDescError {
    BakeDescError {
        code: "bake_incremental_invalid",
        path: None,
        message,
    }
}

/// Bakes `job` to `out`: incrementally through `--incremental`'s state file when it is given
/// ([`bake`]), else in full ([`bake_job`]). Returns the summary.
///
/// # Errors
///
/// As [`bake`] or [`bake_job`].
pub fn bake_for(
    args: &BakeGiArgs,
    job: &GiBakeJob,
    out: &Path,
    scene: &str,
    started: Instant,
    built: f64,
) -> Result<Value> {
    if args.checkpoint.active() {
        if args.inc.active() {
            // An incremental bake retraces a few cells; a checkpoint is for the long full bake.
            anyhow::bail!(
                "--checkpoint and --resume do not combine with --incremental or --plan-incremental"
            );
        }
        let plan = match args.checkpoint.plan(out) {
            Ok(plan) => plan,
            Err((code, path, message)) => {
                return Err(super::CheckpointFailure { code, path, message }.into());
            }
        };
        return super::bake_job_checkpointed(args.backend(), job, out, scene, started, built, plan.as_ref());
    }
    match &args.inc.incremental {
        Some(state) => bake(args, state, job, out, scene, started, built),
        None => bake_job(args.backend(), job, out, scene, started, built),
    }
}

/// The traces' device: its name for the summary, its key for the state (two drivers may round
/// differently), and the tracer when one is needed.
struct Device {
    name: String,
    key: String,
    tracer: Option<Box<dyn ProbeTracer>>,
}

fn open(backend: BakeBackend, with_tracer: bool) -> Result<Device> {
    Ok(match backend {
        BakeBackend::Gpu => {
            let device = regia_render::RenderDevice::headless().context("no GPU adapter for the bake")?;
            let info = device.adapter.get_info();
            let tracer: Option<Box<dyn ProbeTracer>> = if with_tracer {
                Some(Box::new(PathTracer::new(&device)?))
            } else {
                None
            };
            Device {
                name: format!("{} ({:?})", info.name, info.backend),
                key: format!(
                    "{} ({:?}) {} {}",
                    info.name, info.backend, info.driver, info.driver_info
                ),
                tracer,
            }
        }
        BakeBackend::Cpu => {
            let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
            Device {
                name: format!("cpu ({threads} threads)"),
                // The CPU result does not depend on the thread count.
                key: "cpu".into(),
                tracer: with_tracer.then(|| Box::new(CpuProbeTracer::new()) as Box<dyn ProbeTracer>),
            }
        }
    })
}

/// Bakes `job` incrementally through the state file `state` and writes the volume to `out`
/// (or, with `--plan-incremental`, prints the plan and writes nothing). Returns the summary
/// (module docs).
///
/// # Errors
///
/// `bake_incremental_invalid`, no GPU (for the GPU backend), a scene the CPU tracer does not
/// take, a failed bake, or a failed volume write.
pub fn bake(
    args: &BakeGiArgs,
    state: &Path,
    job: &GiBakeJob,
    out: &Path,
    scene: &str,
    started: Instant,
    built: f64,
) -> Result<Value> {
    let backend = args.backend();
    // Bad options fail before a GPU is opened.
    let inc = args.inc.settings(String::new())?;
    let mut warnings = Vec::new();
    let previous = match BakeState::read(state) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(json!({ "code": "bake_state_unreadable", "message": e.to_string() }));
            None
        }
    };
    let unreadable = !warnings.is_empty();
    let device = open(backend, !args.inc.plan_incremental)?;
    let inc = IncrementalSettings {
        device: device.key.clone(),
        ..inc
    };
    if args.inc.plan_incremental {
        let plan = plan_incremental(
            &job.scene,
            &job.layout,
            &job.settings,
            previous.as_ref(),
            &inc,
            backend.name(),
        );
        let mut report = serde_json::to_value(&plan.report)?;
        if unreadable {
            report["full"] = json!("state_unreadable");
        }
        return Ok(json!({
            "ok": true,
            "plan_incremental": true,
            "scene": scene,
            "backend": backend.name(),
            "device": device.name,
            "state": { "path": state.display().to_string(), "exists": previous.is_some() },
            "incremental": report,
            "warnings": warnings,
            "layers": layer_summary(job, None),
            "seconds": started.elapsed().as_secs_f64(),
        }));
    }
    let tracer = device.tracer.as_deref().context("the bake has no tracer")?;
    let mut last = Instant::now();
    let result = bake_gi_volume_incremental(
        tracer,
        &job.scene,
        &job.layout,
        &job.settings,
        previous.as_ref(),
        &inc,
        |stage, done, total| {
            if last.elapsed().as_secs_f32() > 2.0 || done == total {
                eprintln!("  {stage}: {done}/{total} probes");
                last = Instant::now();
            }
            true
        },
    )?;
    let parts = write_volume(&result.bake.volume, out)?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    let (written, state_bytes) = match result.state.write(state) {
        Ok(n) => (true, n),
        Err(e) => {
            warnings.push(json!({ "code": "bake_state_write_failed", "message": e.to_string() }));
            (false, 0)
        }
    };
    let mut report = serde_json::to_value(&result.report)?;
    if unreadable {
        report["full"] = json!("state_unreadable");
    }
    let meta: Vec<ProbeMeta> = result.bake.raw.iter().map(|p| p.meta).collect();
    Ok(json!({
        "ok": true,
        "out": out.display().to_string(),
        "bytes": bytes,
        "scene": scene,
        "backend": backend.name(),
        "device": device.name,
        "seconds": started.elapsed().as_secs_f64(),
        "build_seconds": built,
        "report": result.bake.report,
        "layers": layer_summary(job, Some(&meta)),
        "plan": job.plan,
        "incremental": report,
        "state": { "path": state.display().to_string(), "written": written, "bytes": state_bytes },
        "warnings": warnings,
        "parts": super::part_summary(&parts),
    }))
}

/// Writes `volume` to `out`, split into portable parts when it is larger than one binding
/// (ADR-2618: part 0 at `out`, part `k` at `regia_asset::baked_gi_split::split_part_path`), each
/// through a temporary file in the same folder and a rename, so a failed write never leaves half
/// a volume where the runtime reads it. Part files of an earlier, larger split are removed.
/// Returns the files written.
pub(crate) fn write_volume(volume: &BakedGiVolume, out: &Path) -> Result<Vec<PathBuf>> {
    use regia_asset::baked_gi_split::{MAX_SPLIT_PARTS, split_part_path};
    let parts = volume.split_parts(regia_asset::baked_gi::PORTABLE_VOLUME_BYTES)?;
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    // Parts 1.. first and part 0 last: a reader (the editor's level sync, the live cook) that
    // sees the new part 0 finds the new parts beside it.
    let written: Vec<PathBuf> = (0..parts.len()).map(|k| split_part_path(out, k as u32)).collect();
    for (part, at) in parts.iter().zip(&written).rev() {
        write_one(part, at)?;
    }
    for k in (parts.len() as u32).max(1)..=MAX_SPLIT_PARTS {
        let stale = split_part_path(out, k);
        if stale.exists() {
            std::fs::remove_file(&stale).with_context(|| format!("cannot remove {}", stale.display()))?;
        }
    }
    Ok(written)
}

/// Writes one volume file atomically (temporary file and rename).
fn write_one(volume: &BakedGiVolume, out: &Path) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let data = volume.to_bytes()?;
    let mut tmp = out.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &data).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot write {}", out.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{job, run};
    use super::*;

    fn incremental_args(state: &Path, out: &Path) -> BakeGiArgs {
        let mut a = super::super::tests::args("furnace");
        a.cpu = true;
        a.out = Some(out.to_path_buf());
        a.inc.incremental = Some(state.to_path_buf());
        a
    }

    /// `--incremental` on the CPU: the first bake traces every cell and writes the state; the
    /// second traces nothing and writes the same volume, byte for byte; `--plan-incremental`
    /// says so without writing; another seed plans a full bake for `settings`.
    #[test]
    fn a_second_incremental_bake_of_the_same_scene_traces_nothing() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_incremental");
        let (state, out) = (dir.join("cache").join("furnace.fgis"), dir.join("furnace.fgiv"));
        let a = incremental_args(&state, &out);
        let j = job(&a).expect("furnace");
        let first = bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect("first bake");
        assert_eq!(first["ok"], true);
        assert_eq!(first["incremental"]["full"], "no_state");
        assert_eq!(first["incremental"]["probes_baked"], 27);
        assert_eq!(first["state"]["written"], true);
        assert!(state.is_file());
        let bytes = std::fs::read(&out).expect("volume");

        let mut plan = a.clone();
        plan.inc.plan_incremental = true;
        let planned = bake_for(&plan, &j, &out, "furnace", Instant::now(), 0.0).expect("plan");
        assert_eq!(planned["plan_incremental"], true);
        assert_eq!(planned["state"]["exists"], true);
        assert!(planned["incremental"]["full"].is_null());
        assert_eq!(planned["incremental"]["probes_baked"], 0);
        assert_eq!(planned["incremental"]["cells_reused"], 1);
        run(&plan).expect("plan through run");

        let second = bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect("second bake");
        assert_eq!(second["incremental"]["probes_baked"], 0);
        assert_eq!(second["incremental"]["probes_reused"], 27);
        assert_eq!(second["report"]["valid"], 27);
        assert_eq!(std::fs::read(&out).expect("volume"), bytes);

        let mut reseeded = plan.clone();
        reseeded.seed = Some(9);
        let rj = job(&reseeded).expect("furnace");
        let planned = bake_for(&reseeded, &rj, &out, "furnace", Instant::now(), 0.0).expect("plan");
        assert_eq!(planned["incremental"]["full"], "settings");
        assert_eq!(planned["incremental"]["probes_baked"], 27);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn parse(argv: &[&str]) -> Result<BakeGiArgs, clap::Error> {
        use clap::FromArgMatches;
        let cmd = BakeGiArgs::augment_args(clap::Command::new("bake-gi"));
        let matches = cmd.try_get_matches_from(std::iter::once("bake-gi").chain(argv.iter().copied()))?;
        BakeGiArgs::from_arg_matches(&matches)
    }

    /// The flags parse; the options need `--incremental`; `--incremental` does not combine with
    /// `--dry-run`, `--sample` or `--check-volume`, nor `--plan-incremental` with `--bake`; a bad
    /// margin or tile size is a usage error.
    #[test]
    fn the_flags_parse_and_refuse_bad_combinations() {
        let a = parse(&[
            "furnace",
            "--incremental",
            "bake/state/furnace.fgis",
            "--influence",
            "6.5",
            "--tile-columns",
            "4",
            "--plan-incremental",
        ])
        .expect("parse");
        assert_eq!(
            a.inc.incremental.as_deref(),
            Some(Path::new("bake/state/furnace.fgis"))
        );
        assert_eq!(
            (a.inc.influence, a.inc.tile_columns, a.inc.plan_incremental),
            (Some(6.5), Some(4), true)
        );
        assert!(a.inc.active());
        assert!(!parse(&["furnace"]).expect("plain").inc.active());
        for bad in [
            &["furnace", "--plan-incremental"][..],
            &["furnace", "--influence", "3"],
            &["furnace", "--tile-columns", "3"],
            &["furnace", "--incremental", "s.fgis", "--dry-run"],
            &["furnace", "--incremental", "s.fgis", "--check-volume", "v.fgiv"],
            &["--sample", "v.fgiv", "--at", "0,0,0", "--incremental", "s.fgis"],
            &[
                "--from-scene",
                "l.level.ron",
                "--bake",
                "--incremental",
                "s.fgis",
                "--plan-incremental",
            ],
            &["furnace", "--incremental", "s.fgis", "--influence", "-1"],
            &["furnace", "--incremental", "s.fgis", "--influence", "inf"],
            &["furnace", "--incremental", "s.fgis", "--tile-columns", "0"],
            &[
                "--from-scene",
                "l.level.ron",
                "--check",
                "--incremental",
                "s.fgis",
            ],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let from_scene =
            parse(&["--from-scene", "l.level.ron", "--bake", "--incremental", "s.fgis"]).expect("from scene");
        assert!(from_scene.inc.active() && from_scene.from_scene.bake);
        // An export without `--bake` would ignore the state: refused before anything is read.
        let export_only = parse(&["--from-scene", "l.level.ron", "--incremental", "s.fgis"]).expect("parses");
        let message = format!("{:#}", run(&export_only).expect_err("needs --bake"));
        assert!(message.contains("needs --bake"), "{message}");
    }

    /// A damaged state is a warning, not a failure: every cell is traced, the summary says why,
    /// and a good state replaces it. Bad options fail with `bake_incremental_invalid`.
    #[test]
    fn a_damaged_state_bakes_everything_and_bad_options_fail() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_incremental_damaged");
        let (state, out) = (dir.join("furnace.fgis"), dir.join("furnace.fgiv"));
        let tmp = dir.join("furnace.fgis.tmp");
        std::fs::write(&tmp, b"FGIS not a state").expect("write");
        std::fs::rename(&tmp, &state).expect("rename");
        let a = incremental_args(&state, &out);
        let j = job(&a).expect("furnace");
        let s = bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect("bake");
        assert_eq!(s["incremental"]["full"], "state_unreadable");
        assert_eq!(s["warnings"][0]["code"], "bake_state_unreadable");
        assert_eq!(s["incremental"]["probes_baked"], 27);
        assert!(BakeState::read(&state).expect("readable").is_some(), "replaced");

        let mut bad = a.clone();
        bad.inc.influence = Some(-1.0);
        let message = format!(
            "{:#}",
            bake_for(&bad, &j, &out, "furnace", Instant::now(), 0.0).expect_err("bad")
        );
        assert!(message.contains("bake_incremental_invalid"), "{message}");
        let mut bad = a.clone();
        bad.inc.tile_columns = Some(0);
        assert_eq!(
            bad.inc.settings("cpu".into()).expect_err("zero").code,
            "bake_incremental_invalid"
        );
        let ok = a.inc.settings("cpu".into()).expect("defaults");
        assert_eq!(
            (ok.tile_columns, ok.influence_margin),
            (DEFAULT_TILE_COLUMNS, None)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
