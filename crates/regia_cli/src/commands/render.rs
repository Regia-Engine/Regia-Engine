// SPDX-License-Identifier: MPL-2.0
//! `regia render [--reference] [--project DIR] ...`: one image of a running app's current view
//! (`docs/specs/27-renderer-excellence.md` § 5).
//!
//! Without `--reference` it is the real-time frame (`seeing.screenshot`, PNG). With it, the
//! reference path tracer traces the same extracted frame in the app's process
//! (`seeing.reference_trace`, a background job polled here until it finishes) and writes the
//! layered EXR, the AgX preview and the metadata JSON into `--out-dir`. The app must be built
//! with `regia_agent`'s `reference-trace` feature; without it the method answers F0013.
//!
//! With `--job FILE` it runs a `.render_job.ron` (ADR-1616, book `agents/render-jobs.md`): the
//! file is checked here, sent to the running app (`render.out_job`, a headless host on a farm)
//! with today's UTC date for `{date}`, and polled until it ends. It prints the manifest path and
//! exits 0 (done), 2 (the job file or the job was refused), 3 (failed) or 4 (cancelled); 1 is no
//! app or a failed call. A job with a `product` block (ADR-2757, book `agents/capture-hooks.md`) needs no
//! app: it draws with the product viewer's raster renderer, in this process.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_agent::client::{Client, ClientError};
use regia_agent::render_job::{RenderJob, is_valid_date, utc_date_from_unix_days};
use serde_json::{Value, json};

/// Arguments of `regia render`.
#[derive(Debug, Clone, Args)]
pub struct RenderArgs {
    /// Trace the frame with the reference path tracer instead of grabbing the real-time image.
    #[arg(long)]
    pub reference: bool,
    /// Project directory of the running app (default current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// What to trace: `current` (the app's view) or a validation scene (`cornell`, `furnace`).
    #[arg(long, default_value = "current")]
    pub scene: String,
    /// Image width (reference and `--offscreen`; the window image is the window's size).
    #[arg(long, default_value_t = 1280)]
    pub width: u32,
    /// Image height (reference and `--offscreen`).
    #[arg(long, default_value_t = 720)]
    pub height: u32,
    /// Samples per pixel (reference; default 256, or the `--preset`'s own).
    #[arg(long)]
    pub spp: Option<u32>,
    /// Maximum path length (reference; default 12, or the `--preset`'s own).
    #[arg(long)]
    pub bounces: Option<u32>,
    /// Quality preset of the reference tracer: `draft`, `preview`, `final` or `reference`
    /// (book/agents/pt-presets.md). `draft` and `preview` are biased, denoised previews.
    #[arg(long)]
    pub preset: Option<String>,
    /// With `--preset`: caustics mode, `suppress`, `clamped` or `full`.
    #[arg(long)]
    pub caustics: Option<String>,
    /// With `--preset`: denoiser, `none`, `builtin` or `oidn`.
    #[arg(long)]
    pub denoiser: Option<String>,
    /// Seed (reference).
    #[arg(long, default_value_t = 1)]
    pub seed: u32,
    /// Output directory (reference) — default `regia_render_<pid>` in the temp directory.
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
    /// Render the real-time frame headless at `--width`×`--height` with the game HUD laid out for
    /// that size, instead of grabbing the window.
    #[arg(long)]
    pub offscreen: bool,
    /// HUD layout of `--offscreen`: `auto` (by orientation), `landscape` (16:9) or `portrait` (9:16).
    #[arg(long, default_value = "auto")]
    pub layout: String,
    /// PNG path of the real-time image (without `--reference`).
    #[arg(long, default_value = "render.png")]
    pub out: PathBuf,
    /// Give up after this many seconds (reference).
    #[arg(long, default_value_t = 3600)]
    pub timeout_s: u64,
    /// Run this `.render_job.ron` in the running app (takes, cut list, one folder per take) and
    /// wait for it.
    #[arg(long)]
    pub job: Option<PathBuf>,
    /// `{date}` of the job's paths, YYYY-MM-DD (default: today, UTC).
    #[arg(long)]
    pub date: Option<String>,
    /// With `--job`: print the resolved plan (takes, frames, cuts, paths, pre-roll) and render
    /// nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// With `--job`: cancel the job after this many seconds (0, the default: wait until it ends).
    #[arg(long, default_value_t = 0)]
    pub job_timeout_s: u64,
}

/// `regia render --job` exit status: the job is done.
pub const EXIT_DONE: u8 = 0;
/// `regia render --job` exit status: no app, or a call failed.
pub const EXIT_ERROR: u8 = 1;
/// `regia render --job` exit status: the job file or the job was refused.
pub const EXIT_REFUSED: u8 = 2;
/// `regia render --job` exit status: the job failed.
pub const EXIT_FAILED: u8 = 3;
/// `regia render --job` exit status: the job was cancelled.
pub const EXIT_CANCELLED: u8 = 4;

/// The exit status of a finished job's `render.out_progress` status.
#[must_use]
pub fn job_exit_code(status: &str) -> u8 {
    match status {
        "done" => EXIT_DONE,
        "cancelled" => EXIT_CANCELLED,
        _ => EXIT_FAILED,
    }
}

/// Today's UTC civil date, `YYYY-MM-DD`, from this process's clock (the engine reads none).
#[must_use]
pub fn today_utc() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400);
    utc_date_from_unix_days(i64::try_from(days).unwrap_or(0))
}

/// The `render.out_job` parameters: the job's text (its `shots` and `sequence` paths resolve in
/// the app's project), the date and the dry-run flag.
#[must_use]
pub fn job_params(text: &str, date: &str, dry_run: bool) -> Value {
    json!({ "job": text, "date": date, "dry_run": dry_run })
}

/// True when a failed call is the app refusing the job (a `render_job_*` code, or bad params),
/// not a broken connection.
fn is_refusal(error: &ClientError) -> bool {
    match error {
        ClientError::Rpc(rpc) => rpc
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .is_some_and(|code| code.starts_with("render_job_") || code == "F0002" || code == "F0018"),
        _ => false,
    }
}

/// Runs `regia render --job`, mapping the outcome to the exit status.
pub fn run_job(args: &RenderArgs) -> ExitCode {
    match job(args) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn job(args: &RenderArgs) -> Result<u8> {
    let path = args.job.as_deref().context("--job needs a file")?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("error: cannot read {}: {err}", path.display());
            return Ok(EXIT_REFUSED);
        }
    };
    let parsed = match RenderJob::from_ron(&text) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("error: {}: {err} ({})", path.display(), err.code());
            return Ok(EXIT_REFUSED);
        }
    };
    let date = args.date.clone().unwrap_or_else(today_utc);
    if !is_valid_date(&date) {
        eprintln!("error: --date {date:?} is not YYYY-MM-DD");
        return Ok(EXIT_REFUSED);
    }
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    if parsed.product.is_some() {
        return product_job(args, &project_dir, &text, &date);
    }
    let mut client =
        Client::connect_project(&project_dir).context("cannot connect to a running Regia Engine app")?;
    let started = match client.call("render.out_job", job_params(&text, &date, args.dry_run)) {
        Ok(answer) => answer,
        Err(err) if is_refusal(&err) => {
            eprintln!("error: the app refused the job: {err}");
            return Ok(EXIT_REFUSED);
        }
        Err(err) => return Err(err).context("render.out_job failed"),
    };
    if args.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&started["plan"]).context("cannot serialize the plan")?
        );
        return Ok(EXIT_DONE);
    }
    let id = started
        .get("job")
        .and_then(Value::as_u64)
        .context("render.out_job returned no job id")?;
    let frames = started["plan"]["frames"].as_u64().unwrap_or(0);
    let deadline = (args.job_timeout_s > 0).then(|| Instant::now() + Duration::from_secs(args.job_timeout_s));
    let mut reported = 0_u64;
    let mut cancelling = false;
    loop {
        let progress = client
            .call("render.out_progress", json!({ "job": id }))
            .context("polling the render job failed")?;
        let status = progress.get("status").and_then(Value::as_str).unwrap_or("");
        if !matches!(status, "running" | "cancelling") {
            let manifest = progress.get("manifest").and_then(Value::as_str);
            match manifest {
                Some(manifest) => println!("{}", shown_path(&project_dir, manifest).display()),
                None => eprintln!("warning: the job wrote no manifest"),
            }
            if let Some(error) = progress.get("error").and_then(Value::as_str) {
                eprintln!("error: render job {id} {status}: {error}");
            }
            return Ok(job_exit_code(status));
        }
        let frame = progress.get("frame").and_then(Value::as_u64).unwrap_or(0);
        if frames > 0 && frame >= reported + (frames / 20).max(1) {
            eprintln!("  {frame}/{frames} frames");
            reported = frame;
        }
        if !cancelling && deadline.is_some_and(|deadline| Instant::now() > deadline) {
            client.call("render.out_cancel", json!({ "job": id })).ok();
            eprintln!(
                "render job {id} did not finish in {} s; cancelling",
                args.job_timeout_s
            );
            cancelling = true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A job with a `product` block, in this process: it has no world to ask a running app for.
fn product_job(args: &RenderArgs, project_dir: &Path, text: &str, date: &str) -> Result<u8> {
    let deadline = (args.job_timeout_s > 0).then(|| Instant::now() + Duration::from_secs(args.job_timeout_s));
    let mut reported = 0_u64;
    let mut on_progress = |progress: &Value| {
        let frame = progress.get("frame").and_then(Value::as_u64).unwrap_or(0);
        let frames = progress.get("frames").and_then(Value::as_u64).unwrap_or(0);
        if frames > 0 && frame >= reported + (frames / 20).max(1) {
            eprintln!("  {frame}/{frames} frames");
            reported = frame;
        }
        deadline.is_none_or(|deadline| Instant::now() <= deadline)
    };
    let answer =
        match regia_agent::run_product_job(project_dir, text, Some(date), args.dry_run, &mut on_progress) {
            Ok(answer) => answer,
            Err(err) => {
                let code = err
                    .data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(Value::as_str);
                eprintln!(
                    "error: the job was refused: {} ({})",
                    err.message,
                    code.unwrap_or("error")
                );
                return Ok(if code == Some("F0003") {
                    EXIT_FAILED
                } else {
                    EXIT_REFUSED
                });
            }
        };
    if args.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&answer["plan"]).context("cannot serialize the plan")?
        );
        return Ok(EXIT_DONE);
    }
    let status = answer.get("status").and_then(Value::as_str).unwrap_or("");
    match answer.get("manifest").and_then(Value::as_str) {
        Some(manifest) => println!("{}", shown_path(project_dir, manifest).display()),
        None => eprintln!("warning: the job wrote no manifest"),
    }
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        eprintln!("error: render job {status}: {error}");
    }
    Ok(job_exit_code(status))
}

/// A manifest path as the app shows it (relative to its project root) on this machine.
fn shown_path(project_dir: &Path, shown: &str) -> PathBuf {
    let path = Path::new(shown);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_dir.join(path)
    }
}

/// The `seeing.reference_trace` parameters for `args`.
#[must_use]
pub fn reference_params(args: &RenderArgs, out_dir: &std::path::Path) -> Value {
    let mut params = json!({
        "scene": args.scene,
        "width": args.width,
        "height": args.height,
        "seed": args.seed,
        "out_dir": out_dir.display().to_string(),
    });
    if let Some(o) = params.as_object_mut() {
        // Without a preset the defaults are the base tracer's; with one, only what was given
        // overrides the preset's own table.
        if args.preset.is_none() {
            o.insert("spp".into(), json!(args.spp.unwrap_or(DEFAULT_SPP)));
            o.insert(
                "max_bounces".into(),
                json!(args.bounces.unwrap_or(DEFAULT_BOUNCES)),
            );
        } else {
            if let Some(spp) = args.spp {
                o.insert("spp".into(), json!(spp));
            }
            if let Some(b) = args.bounces {
                o.insert("max_bounces".into(), json!(b));
            }
        }
        for (key, value) in [
            ("preset", &args.preset),
            ("caustics", &args.caustics),
            ("denoiser", &args.denoiser),
        ] {
            if let Some(v) = value {
                o.insert(key.into(), json!(v));
            }
        }
    }
    params
}

/// Samples per pixel of a reference trace without `--spp` or `--preset`.
const DEFAULT_SPP: u32 = 256;
/// Path length of a reference trace without `--bounces` or `--preset`.
const DEFAULT_BOUNCES: u32 = 12;

/// The `seeing.screenshot` parameters for the real-time path of `args`.
#[must_use]
pub fn screenshot_params(args: &RenderArgs) -> Value {
    if args.offscreen {
        json!({
            "target": "offscreen",
            "width": args.width,
            "height": args.height,
            "layout": args.layout,
            "hud": true,
        })
    } else {
        json!({})
    }
}

/// Runs `regia render`.
///
/// # Errors
///
/// No running app, a failed call, a failed or timed-out trace, or a file write.
pub fn run(args: &RenderArgs) -> Result<()> {
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    let mut client =
        Client::connect_project(&project_dir).context("cannot connect to a running Regia Engine app")?;
    if !args.reference {
        let shot = client
            .call("seeing.screenshot", screenshot_params(args))
            .context("seeing.screenshot failed")?;
        let b64 = shot
            .get("png_base64")
            .and_then(Value::as_str)
            .context("seeing.screenshot returned no png_base64")?;
        let png = crate::commands::agent::decode_base64_png(b64)?;
        std::fs::write(&args.out, png).with_context(|| format!("cannot write {}", args.out.display()))?;
        println!("{}", args.out.display());
        return Ok(());
    }
    let out_dir = args
        .out_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("regia_render_{}", std::process::id())));
    let started = client
        .call("seeing.reference_trace", reference_params(args, &out_dir))
        .context(
            "seeing.reference_trace failed (is the app built with regia_agent's `reference-trace` feature?)",
        )?;
    let job = started
        .get("job")
        .and_then(Value::as_u64)
        .context("seeing.reference_trace returned no job id")?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_s);
    let mut last_report = 0u64;
    loop {
        let status = client
            .call("seeing.reference_trace", json!({ "job": job }))
            .context("polling the trace job failed")?;
        match status.get("status").and_then(Value::as_str) {
            Some("running") => {
                let done = status.get("spp_done").and_then(Value::as_u64).unwrap_or(0);
                let total = status
                    .get("spp")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| u64::from(args.spp.unwrap_or(DEFAULT_SPP)));
                if done >= last_report + (total / 8).max(1) {
                    eprintln!("  {done}/{total} spp");
                    last_report = done;
                }
            }
            Some("done") => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status).context("cannot serialize the result")?
                );
                return Ok(());
            }
            Some(other) => bail!(
                "trace job {job} {other}: {}",
                status.get("error").and_then(Value::as_str).unwrap_or("")
            ),
            None => bail!("unexpected job status {status}"),
        }
        if Instant::now() > deadline {
            client
                .call("seeing.reference_trace", json!({ "job": job, "cancel": true }))
                .ok();
            bail!(
                "trace job {job} did not finish in {} s (canceled)",
                args.timeout_s
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_params_carry_every_setting() {
        let args = RenderArgs {
            reference: true,
            project: PathBuf::from("."),
            scene: "cornell".into(),
            width: 64,
            height: 32,
            spp: Some(16),
            preset: None,
            caustics: None,
            denoiser: None,
            bounces: Some(5),
            seed: 9,
            out_dir: None,
            out: PathBuf::from("x.png"),
            offscreen: false,
            layout: "auto".into(),
            timeout_s: 10,
            job: None,
            date: None,
            dry_run: false,
            job_timeout_s: 0,
        };
        let p = reference_params(&args, std::path::Path::new("/tmp/pt"));
        assert_eq!(p["scene"], "cornell");
        assert_eq!(p["width"], 64);
        assert_eq!(p["height"], 32);
        assert_eq!(p["spp"], 16);
        assert_eq!(p["max_bounces"], 5);
        assert_eq!(p["seed"], 9);
        assert_eq!(p["out_dir"], "/tmp/pt");
        assert_eq!(screenshot_params(&args), json!({}));
        let portrait = RenderArgs {
            offscreen: true,
            layout: "portrait".into(),
            width: 1080,
            height: 1920,
            ..args
        };
        let shot = screenshot_params(&portrait);
        assert_eq!(shot["target"], "offscreen");
        assert_eq!(shot["layout"], "portrait");
        assert_eq!(shot["height"], 1920);
    }

    #[test]
    fn job_exit_codes_and_params() {
        assert_eq!(job_exit_code("done"), 0);
        assert_eq!(job_exit_code("cancelled"), 4);
        assert_eq!(job_exit_code("failed"), 3);
        assert_eq!(job_exit_code("anything else"), 3);
        let params = job_params("(version: 1)", "2026-10-28", true);
        assert_eq!(params["job"], "(version: 1)");
        assert_eq!(params["date"], "2026-10-28");
        assert_eq!(params["dry_run"], true);
        assert!(is_valid_date(&today_utc()), "{}", today_utc());
        assert_eq!(
            shown_path(Path::new("/p"), "renders/job_render_out.json"),
            Path::new("/p/renders/job_render_out.json")
        );
    }

    #[test]
    fn a_preset_sends_only_what_was_given() {
        let base = RenderArgs {
            reference: true,
            project: PathBuf::from("."),
            scene: "cornell".into(),
            width: 64,
            height: 32,
            spp: None,
            bounces: None,
            preset: None,
            caustics: None,
            denoiser: None,
            seed: 9,
            out_dir: None,
            out: PathBuf::from("x.png"),
            offscreen: false,
            layout: "auto".into(),
            timeout_s: 10,
            job: None,
            date: None,
            dry_run: false,
            job_timeout_s: 0,
        };
        let plain = reference_params(&base, std::path::Path::new("/tmp/pt"));
        assert_eq!(
            (plain["spp"].clone(), plain["max_bounces"].clone()),
            (json!(256), json!(12))
        );
        assert!(plain.get("preset").is_none());
        let draft = RenderArgs {
            preset: Some("draft".into()),
            caustics: Some("suppress".into()),
            ..base.clone()
        };
        let p = reference_params(&draft, std::path::Path::new("/tmp/pt"));
        assert_eq!(p["preset"], "draft");
        assert_eq!(p["caustics"], "suppress");
        assert!(
            p.get("spp").is_none() && p.get("max_bounces").is_none(),
            "the preset's own numbers"
        );
        let p = reference_params(
            &RenderArgs {
                spp: Some(2),
                ..draft
            },
            std::path::Path::new("/tmp/pt"),
        );
        assert_eq!(p["spp"], 2);
    }

    #[test]
    fn a_bad_job_file_exits_refused_before_any_connection() {
        let dir = std::env::temp_dir().join(format!("regia-cli-render-job-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("bad.render_job.ron");
        std::fs::write(&file, "(version: 9, out: \"r\", takes: [])").expect("write");
        let args = RenderArgs {
            reference: false,
            project: dir.join("no-such-project"),
            scene: "current".into(),
            width: 1,
            height: 1,
            spp: Some(1),
            bounces: Some(1),
            preset: None,
            caustics: None,
            denoiser: None,
            seed: 1,
            out_dir: None,
            out: PathBuf::from("x.png"),
            offscreen: false,
            layout: "auto".into(),
            timeout_s: 1,
            job: Some(file.clone()),
            date: None,
            dry_run: false,
            job_timeout_s: 0,
        };
        assert_eq!(
            job(&args).expect("no error path"),
            EXIT_REFUSED,
            "a newer version"
        );
        let missing = RenderArgs {
            job: Some(dir.join("missing.render_job.ron")),
            ..args.clone()
        };
        assert_eq!(job(&missing).expect("no error path"), EXIT_REFUSED, "no file");
        std::fs::write(
            &file,
            "(version: 1, out: \"r\", takes: [(name: \"a\", range: Frames(start: 0, end: 2))])",
        )
        .expect("write");
        let bad_date = RenderArgs {
            date: Some("28-10-2026".into()),
            ..args.clone()
        };
        assert_eq!(job(&bad_date).expect("no error path"), EXIT_REFUSED, "bad date");
        assert!(job(&args).is_err(), "a valid job and no app is an error (exit 1)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
