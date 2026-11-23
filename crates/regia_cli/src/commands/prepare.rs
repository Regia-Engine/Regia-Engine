// SPDX-License-Identifier: MPL-2.0
//! `regia prepare` — headless warm of the shipped pipeline units, then a stamp.
//!
//! Stdout is one JSON object per line ([`regia_render::prepare::PrepareProgress::to_json`])
//! so a launcher can draw a progress bar. Human text goes to stderr. Exit `0` when the stamp
//! is current (a warm that finished, a skip, or `--check` that matched). Exit `2` from
//! `--check` when a prepare is still needed. Exit `1` on error, with one `failed` JSON line
//! if the warm had not already printed one.
//!
//! `--scene <level>` (repeatable, ADR-8453) adds a game's maps: each level (`.level.ron` or
//! `.level.d`) is spawned headless, its `MeshSource` entities get a stand-in quad with their
//! material (`.material.ron`, `.matgraph.ron` or `.matinst.ron`; textures are 1×1 stand-ins:
//! a pipeline depends on the material's kind and graph, not on its pixels), and the extracted
//! frame is warmed like the game-frame warm warms it. The map list (names and a hash of each
//! level's files and the material files it names) is in the stamp, so `--check` exits 2 when it
//! changes. `--target <format>[xN]` adds a render target the game draws into besides the window.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use regia_render::RenderProfilePreset;
use regia_render::pipeline_warm::WarmTargetKey;
use regia_render::prepare::{self, PrepareMap, PrepareProgress, PrepareRequest, PrepareState};

mod scene;

/// Arguments of `regia prepare`.
#[derive(Debug, Clone, Args)]
pub struct PrepareArgs {
    /// Pipeline-cache directory of the game (its `cache://pipelines`), not this tool's cache.
    ///
    /// `REGIA_PIPELINE_CACHE_DIR`, when set and non-empty, wins over this flag.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Shipped `.fpc` directory (`content/pipelines` in a package).
    #[arg(long, value_name = "DIR")]
    pub shipped_dir: Option<PathBuf>,

    /// Content salt folded into the cache key. A game update with new materials passes a new one.
    #[arg(long, default_value = "")]
    pub salt: String,

    /// Extra map-tier frames for this preset (`DesktopHigh`, `mobile-high`, …).
    #[arg(long, value_parser = parse_profile, value_name = "NAME")]
    pub profile: Option<RenderProfilePreset>,

    /// Warm workers. The default is the Boot budget (cores − 2, clamped to 1..=12).
    #[arg(long, value_parser = parse_threads)]
    pub threads: Option<usize>,

    /// Do not lower this thread or the warm workers to background priority.
    #[arg(long)]
    pub no_nice: bool,

    /// Warm even when the stamp matches. The stamp is deleted before the warm starts.
    #[arg(long, conflicts_with = "check")]
    pub force: bool,

    /// Print whether the stamp is current and exit. Does not warm. Exit 2 when a prepare is needed.
    #[arg(long, conflicts_with = "status")]
    pub check: bool,

    /// Print the status file, applying the stale-heartbeat rule. Does not open a device.
    #[arg(long, conflicts_with_all = ["check", "force"])]
    pub status: bool,

    /// Stop the warm after this many seconds, deferred variants included.
    #[arg(long, default_value_t = 480, value_parser = parse_timeout)]
    pub timeout_secs: u64,

    /// A map to prepare: a level source (`.level.ron` or `.level.d`). Repeatable, in order.
    #[arg(long = "scene", value_name = "LEVEL")]
    pub scenes: Vec<PathBuf>,

    /// Project root the levels' asset paths resolve under.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub project: PathBuf,

    /// A render target the game draws into besides the window: `<format>[x<samples>]`
    /// (`rgba8-srgb`, `bgra8-srgb`, `rgba8`, `bgra8`, `rgba16f`, `rgb10a2`; `rgba16fx4`).
    /// Repeatable.
    #[arg(long = "target", value_parser = parse_target, value_name = "FORMAT[xN]")]
    pub targets: Vec<WarmTargetKey>,
}

/// Run `regia prepare`.
pub fn run(args: &PrepareArgs) -> ExitCode {
    let cache_dir = match cache_dir_of(args) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("error: {err}");
            println!("{}", failed_line(&err));
            return ExitCode::FAILURE;
        }
    };
    if args.status {
        return print_status(&cache_dir);
    }
    let mut request = request_from(args, cache_dir);
    // A check needs each map's name and fingerprint only; a warm needs its frames too.
    match maps_of(args, !args.check) {
        Ok(maps) => request.maps = maps,
        Err(err) => {
            eprintln!("error: {err}");
            println!("{}", failed_line(&err));
            return ExitCode::FAILURE;
        }
    }
    if args.check {
        return check(&request);
    }
    let mut saw_failed = false;
    let result = prepare::run(&request, |progress| {
        if progress.state == PrepareState::Failed {
            saw_failed = true;
        }
        println!("{}", progress.to_json());
    });
    match result {
        Ok(report) => {
            eprintln!(
                "prepare: {} ({} workers, {}, {} pipelines, {} ms, worker time {} ms)",
                if report.skipped { "current" } else { "ready" },
                report.threads,
                report.backend,
                report.pipelines,
                report.elapsed_ms,
                report.worker_ms
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            if !saw_failed {
                println!("{}", failed_line(&err.to_string()));
            }
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn request_from(args: &PrepareArgs, cache_dir: PathBuf) -> PrepareRequest {
    PrepareRequest {
        cache_dir,
        shipped_dir: args.shipped_dir.clone(),
        content_salt: args.salt.clone(),
        profile: args.profile,
        threads: args.threads,
        nice: !args.no_nice && !args.check,
        skip_if_current: !args.force,
        timeout: Duration::from_secs(args.timeout_secs),
        maps: Vec::new(),
        targets: args.targets.clone(),
    }
}

/// The `--scene` maps, in order. With `frames`, each level is loaded and extracted; without,
/// only its name and fingerprint are read (no device, no spawn).
fn maps_of(args: &PrepareArgs, frames: bool) -> Result<Vec<PrepareMap>, String> {
    let mut maps = Vec::with_capacity(args.scenes.len());
    for path in &args.scenes {
        let path = if path.is_absolute() || path.exists() {
            path.clone()
        } else {
            args.project.join(path)
        };
        let name = scene::map_name(&args.project, &path);
        let fingerprint = scene::fingerprint(&args.project, &path)?;
        let mut map = PrepareMap::new(name, fingerprint);
        if frames {
            println!(
                "{}",
                progress_line(
                    PrepareState::Starting,
                    &format!("load:{}", map.name),
                    false,
                    &path.display().to_string(),
                    0,
                    "",
                    None,
                )
            );
            map.frames.push(scene::frame_of(&args.project, &path)?);
        }
        maps.push(map);
    }
    Ok(maps)
}

fn cache_dir_of(args: &PrepareArgs) -> Result<PathBuf, String> {
    let from_env =
        regia_core::brand::env_var_os("REGIA_PIPELINE_CACHE_DIR").filter(|value| !value.is_empty());
    if let Some(env) = from_env {
        let env = PathBuf::from(env);
        if let Some(flag) = &args.cache_dir
            && flag != &env
        {
            eprintln!(
                "warning: --cache-dir {} ignored; REGIA_PIPELINE_CACHE_DIR is {}",
                flag.display(),
                env.display()
            );
        }
        return Ok(env);
    }
    args.cache_dir.clone().ok_or_else(|| {
        "pass --cache-dir, the game's pipeline-cache directory (REGIA_PIPELINE_CACHE_DIR also works)"
            .to_string()
    })
}

fn print_status(dir: &Path) -> ExitCode {
    let path = prepare::status_path(dir);
    match prepare::read_status(&path) {
        Ok(None) => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "prepare",
                    "state": "absent",
                    "path": path.display().to_string(),
                })
            );
            ExitCode::SUCCESS
        }
        Ok(Some(status)) => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "prepare",
                    "state": status.state.name(),
                    "phase": status.phase,
                    "done": status.done,
                    "total": status.total,
                    "fraction": status.fraction,
                    "pipelines": status.pipelines,
                    "threads": status.threads,
                    "backend": status.backend,
                    "error": status.error,
                    "pid": status.pid,
                    "started_unix": status.started_unix,
                    "updated_unix": status.updated_unix,
                    "path": path.display().to_string(),
                })
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            println!("{}", failed_line(&err.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn check(request: &PrepareRequest) -> ExitCode {
    match prepare::stamp_is_current(request) {
        Ok(current) => {
            let stamp = prepare::read_stamp(&prepare::stamp_path(&request.cache_dir))
                .ok()
                .flatten();
            let threads = stamp.as_ref().map_or(0, |stamp| stamp.threads);
            let backend = stamp.map_or(String::new(), |stamp| stamp.backend);
            let (state, phase, message) = if current {
                (
                    PrepareState::Ready,
                    "current",
                    "stamp matches this GPU, driver, unit list and map list",
                )
            } else {
                (PrepareState::Starting, "check", "stamp is not current")
            };
            println!(
                "{}",
                progress_line(state, phase, current, message, threads, &backend, None)
            );
            if current {
                eprintln!("prepare: current");
                ExitCode::SUCCESS
            } else {
                eprintln!("prepare: needs prepare");
                ExitCode::from(2)
            }
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            println!("{}", failed_line(&err.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn failed_line(message: &str) -> String {
    progress_line(
        PrepareState::Failed,
        "failed",
        false,
        message,
        0,
        "",
        Some(message.to_string()),
    )
}

fn progress_line(
    state: PrepareState,
    phase: &str,
    skipped: bool,
    message: &str,
    threads: usize,
    backend: &str,
    error: Option<String>,
) -> String {
    PrepareProgress {
        state,
        phase: phase.to_string(),
        done: u32::from(skipped),
        total: u32::from(skipped),
        fraction: if skipped { 1.0 } else { 0.0 },
        pipelines: 0,
        threads,
        backend: backend.to_string(),
        message: message.to_string(),
        error,
        skipped,
    }
    .to_json()
}

fn parse_profile(name: &str) -> Result<RenderProfilePreset, String> {
    RenderProfilePreset::from_name(name).ok_or_else(|| {
        format!("unknown profile {name} (MobileLow, MobileHigh, DesktopLow, DesktopHigh, DesktopUltra, Deck)")
    })
}

fn parse_target(raw: &str) -> Result<WarmTargetKey, String> {
    let lower = raw.to_ascii_lowercase();
    let (name, samples) = match lower.rsplit_once('x') {
        Some((name, count)) if !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) => {
            let samples: u32 = count
                .parse()
                .map_err(|_| format!("target samples must be a number, got {count}"))?;
            (name.to_string(), samples)
        }
        _ => (lower.clone(), 1),
    };
    if !matches!(samples, 1 | 2 | 4 | 8) {
        return Err(format!("target samples must be 1, 2, 4 or 8, got {samples}"));
    }
    let format = match name.as_str() {
        "rgba8-srgb" => wgpu::TextureFormat::Rgba8UnormSrgb,
        "bgra8-srgb" => wgpu::TextureFormat::Bgra8UnormSrgb,
        "rgba8" => wgpu::TextureFormat::Rgba8Unorm,
        "bgra8" => wgpu::TextureFormat::Bgra8Unorm,
        "rgba16f" => wgpu::TextureFormat::Rgba16Float,
        "rgb10a2" => wgpu::TextureFormat::Rgb10a2Unorm,
        _ => {
            return Err(format!(
                "unknown target format {name} (rgba8-srgb, bgra8-srgb, rgba8, bgra8, rgba16f, rgb10a2)"
            ));
        }
    };
    Ok(WarmTargetKey { format, samples })
}

fn parse_threads(raw: &str) -> Result<usize, String> {
    let count: usize = raw
        .parse()
        .map_err(|_| format!("threads must be a number, got {raw}"))?;
    if count == 0 {
        return Err("threads must be at least 1".to_string());
    }
    Ok(count)
}

fn parse_timeout(raw: &str) -> Result<u64, String> {
    let seconds: u64 = raw
        .parse()
        .map_err(|_| format!("timeout-secs must be a number, got {raw}"))?;
    if seconds == 0 {
        return Err("timeout-secs must be at least 1".to_string());
    }
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: PrepareArgs,
    }

    fn args(extra: &[&str]) -> PrepareArgs {
        let mut argv = vec!["prepare", "--cache-dir", "/nowhere"];
        argv.extend_from_slice(extra);
        Cli::parse_from(argv).args
    }

    fn level(guid: u128, material: &str) -> String {
        format!(
            r#"(format_version: 13, entities: [
    (guid: "{}", name: "Prop", components: {{
        "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
        "regia_render::runtime::MeshSource": (mesh: "builtin:cube", material: "{material}"),
    }}),
])"#,
            regia_core::id::Guid::from_u128(guid)
        )
    }

    /// A two-level project: `a` with the standard material, `b` with a blended double-sided one.
    fn project() -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("prepare-scenes");
        std::fs::create_dir_all(dir.join("levels")).expect("dirs");
        std::fs::create_dir_all(dir.join("assets")).expect("dirs");
        let standard = regia_asset::MaterialAssetRon::default_standard();
        let mut blended = regia_asset::MaterialAssetRon::default_standard();
        blended.render_state.alpha_mode = regia_asset::AlphaMode::Blend;
        blended.render_state.cull_mode = Some(regia_asset::CullMode::None);
        for (name, asset) in [("stone", &standard), ("glass", &blended)] {
            regia_test_utils::write_atomic(
                &dir.join(format!("assets/{name}.material.ron")),
                asset.to_ron_string().expect("ron").as_bytes(),
            )
            .expect("material");
        }
        regia_test_utils::write_atomic(
            &dir.join("levels/a.level.ron"),
            level(0xa, "assets/stone.material.ron").as_bytes(),
        )
        .expect("level a");
        regia_test_utils::write_atomic(
            &dir.join("levels/b.level.ron"),
            level(0xb, "assets/glass.material.ron").as_bytes(),
        )
        .expect("level b");
        dir
    }

    /// (b) at the CLI: what `--check` compares follows the `--scene` list and the levels'
    /// files, including the materials they name.
    #[test]
    fn the_checked_map_list_follows_the_scenes_and_their_files() {
        let dir = project();
        let root = dir.display().to_string();
        let both = args(&[
            "--project",
            &root,
            "--scene",
            "levels/a.level.ron",
            "--scene",
            "levels/b.level.ron",
        ]);
        let request = |args: &PrepareArgs| {
            let mut request = request_from(args, PathBuf::from("/nowhere"));
            request.maps = maps_of(args, false).expect("maps");
            request
        };
        let first = request(&both);
        assert_eq!(
            first.maps.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["levels/a", "levels/b"]
        );
        assert!(
            first.maps.iter().all(|m| m.frames.is_empty()),
            "a check loads no frames"
        );
        let units = first.units();
        assert!(
            units.contains("map:levels/b:rgba8-srgb") && units.contains(",maps:"),
            "{units}"
        );
        assert_eq!(request(&both).units(), units, "the same files give the same list");

        let only_a = args(&["--project", &root, "--scene", "levels/a.level.ron"]);
        assert_ne!(request(&only_a).units(), units, "a map left out");

        // The material level b names changes: b's fingerprint and the list change.
        let mut changed = regia_asset::MaterialAssetRon::default_standard();
        changed.render_state.cull_mode = Some(regia_asset::CullMode::None);
        regia_test_utils::write_atomic(
            &dir.join("assets/glass.material.ron"),
            changed.to_ron_string().expect("ron").as_bytes(),
        )
        .expect("material");
        let after = request(&both);
        assert_eq!(after.maps[0].fingerprint, first.maps[0].fingerprint);
        assert_ne!(after.maps[1].fingerprint, first.maps[1].fingerprint);
        assert_ne!(after.units(), units, "a changed material makes the stamp stale");

        let missing = args(&["--project", &root, "--scene", "levels/none.level.ron"]);
        assert!(maps_of(&missing, false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each level becomes a frame with its materials (no device needed to load).
    #[test]
    fn a_scene_loads_into_a_frame_with_its_materials() {
        let dir = project();
        let frame = scene::frame_of(&dir, &dir.join("levels/b.level.ron")).expect("level b");
        assert_eq!(frame.window_views().len(), 1, "a level without a camera gets one");
        let material = &frame.meshes.first().expect("the level's mesh").material;
        assert_eq!(material.alpha_mode, regia_render::ALPHA_BLEND);
        assert!(material.double_sided);
        let a = scene::frame_of(&dir, &dir.join("levels/a.level.ron")).expect("level a");
        assert_eq!(a.meshes.len(), 1);
        assert_eq!(a.meshes[0].material.alpha_mode, regia_render::ALPHA_OPAQUE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn targets_parse_format_and_samples() {
        let key = parse_target("rgba16fx4").expect("target");
        assert_eq!(key.format, wgpu::TextureFormat::Rgba16Float);
        assert_eq!(key.samples, 4);
        assert_eq!(parse_target("BGRA8-SRGB").expect("target").samples, 1);
        assert!(parse_target("rgba16fx3").is_err());
        assert!(parse_target("r8").is_err());
    }
}
