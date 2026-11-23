// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask bake-check [--gpu] [--release] [--keep]`: baked light end to end on the fixture
//! project `crates/regia_cli/tests/fixtures/bake_e2e` (ADR-365, `book/agents/baked-gi-bake-check.md`).
//!
//! Steps, each through the real `regia` binary (`cargo run -p regia_cli`) on a copy of the
//! fixture in a fresh temporary directory:
//!
//! 1. `bake-gi --from-scene <level> --bake --cpu --write-level` (or the GPU with `--gpu`): the
//!    level's statics export to a `.bake.ron`, the bake writes the `.fgiv`, the level names it.
//! 2. `bake-gi --from-scene <level> --check` exits 0 (current).
//! 3. `bake-gi --sample <fgiv> --at <open cell> --at <sealed room>`: the open cell's sky component
//!    is the exported sky's irradiance (π·L, within the 3 % the rooms hide), the sealed room's
//!    centre is exactly black ([`verdict`]).
//! 4. The level opens in a headless app: `BakedGiLevelPlugin` installs the volume through the
//!    asset server, and the renderer's lookup on the installed volume equals step 3's.
//! 5. Moving a static entity makes `--check` exit 2 (stale).
//!
//! Prints one line per step and a JSON summary; exits non-zero on the first failure. With `--gpu`
//! the same expectations hold for the GPU path tracer (needs a GPU; the CPU run needs none).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regia_app::App;
use regia_asset::baked_gi::BakedGiVolume;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_core::id::Guid;
use regia_framework::baked_gi_level::{BakedGiLevelPlugin, LevelBakedGiState};
use regia_pt::bake_desc::GiBakeDescription;
use regia_render::{BakedGiFrameLights, RuntimeRenderer, component_scales, sample_static_gi};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::prelude::ScenePlugin;
use serde_json::{Value, json};

/// The fixture project, relative to the repository root.
pub const FIXTURE: &str = "crates/regia_cli/tests/fixtures/bake_e2e";
/// The fixture's level, relative to the project.
pub const LEVEL: &str = "assets/levels/courtyard.level.ron";
/// Where the bake writes the volume, relative to the project.
pub const VOLUME: &str = "assets/maps/courtyard.fgiv";
/// The exported description, relative to the project.
pub const DESC: &str = "bake/courtyard.bake.ron";
/// The open cell between the two rooms.
pub const LIT: [f32; 3] = [14.8, 0.0, 0.0];
/// The sealed room's centre.
pub const SEALED: [f32; 3] = [0.0, 0.0, 0.0];
/// The sky the open cell sees, as a fraction of the exported sky's π·L: the rooms hide about
/// 3 % of it and nothing adds sky.
pub const LIT_SKY_RANGE: (f32, f32) = (0.94, 1.02);

/// Arguments of `cargo xtask bake-check`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Bake on the GPU path tracer instead of the CPU probe tracer.
    #[arg(long)]
    gpu: bool,
    /// Build and run `regia` in release mode (faster bake, slower build).
    #[arg(long)]
    release: bool,
    /// Keep the temporary project (its path is printed).
    #[arg(long)]
    keep: bool,
    /// Rays per probe per sample.
    #[arg(long, default_value_t = 64)]
    rays: u32,
    /// Samples per probe.
    #[arg(long, default_value_t = 2)]
    samples: u32,
}

fn temp_project(root: &Path) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("regia-bake-check-{}-{nanos}", std::process::id()));
    copy_tree(&root.join(FIXTURE), &dir)?;
    Ok(dir)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Runs `regia <args>` through `cargo run` from the repository root (so the repository's
/// `.cargo/config.toml` applies and the build is the usual one); paths in `args` are absolute.
fn regia(root: &Path, release: bool, args: &[&str]) -> Result<Output> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root)
        .arg("run")
        .arg("-q")
        .args(["-p", "regia_cli", "--bin", "regia"]);
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--").args(args);
    cmd.output().context("running cargo run -p regia_cli")
}

/// The last JSON line of a run's stdout.
///
/// # Errors
///
/// No line parses as JSON.
pub fn json_line(out: &Output) -> Result<Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some(line) = stdout.lines().rev().find(|l| l.trim_start().starts_with('{')) else {
        bail!(
            "no JSON line on stdout (exit {:?}); stderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    serde_json::from_str(line).with_context(|| format!("parsing {line}"))
}

fn num(v: &Value) -> f32 {
    v.as_f64().map_or(f32::NAN, |x| x as f32)
}

fn rgb(v: &Value) -> [f32; 3] {
    [num(&v[0]), num(&v[1]), num(&v[2])]
}

/// The failures of one lookup of the open cell and the sealed room against the exported `sky`:
/// `lit` and `sealed` are `(total irradiance, sky component, coverage)` in lux. Empty when it
/// passes.
#[must_use]
pub fn verdict(
    sky: [f32; 3],
    lit: ([f32; 3], [f32; 3], f32),
    sealed: ([f32; 3], [f32; 3], f32),
) -> Vec<String> {
    let mut failures = Vec::new();
    let (lo, hi) = LIT_SKY_RANGE;
    let (lit_total, lit_sky, lit_coverage) = lit;
    if lit_coverage != 1.0 {
        failures.push(format!("open cell coverage {lit_coverage} (want 1)"));
    }
    for (c, ((total, sky_part), l)) in lit_total.iter().zip(lit_sky).zip(sky).enumerate() {
        let ratio = sky_part / (std::f32::consts::PI * l);
        if !(lo..=hi).contains(&ratio) {
            failures.push(format!(
                "open cell sky channel {c}: {ratio:.4} of pi * L (want {lo}..={hi})"
            ));
        }
        if *total < sky_part * 0.999 {
            failures.push(format!(
                "open cell channel {c}: total {total} below its sky {sky_part}"
            ));
        }
    }
    if sealed.2 != 1.0 {
        failures.push(format!("sealed room coverage {} (want 1)", sealed.2));
    }
    if sealed.0 != [0.0; 3] {
        failures.push(format!("sealed room irradiance {:?} (want exactly 0)", sealed.0));
    }
    failures
}

/// `(total, sky, coverage)` of `--sample` point `k`.
/// One sampled cell: (irradiance, sky component, coverage).
type CellSample = ([f32; 3], [f32; 3], f32);

fn sampled(line: &Value, k: usize) -> CellSample {
    let p = &line["samples"][k];
    (
        rgb(&p["irradiance"]),
        rgb(&p["components"]["sky"]),
        num(&p["coverage"]),
    )
}

/// Opens the (rewritten) level in a headless app, lets `BakedGiLevelPlugin` install its volume,
/// and looks the two cells up on the installed volume at the baked light.
fn runtime_lookup(project: &Path) -> Result<[CellSample; 2]> {
    let server = AssetServer::open(AssetServerSettings {
        project_root: project.to_path_buf(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .context("asset server")?;
    let text = std::fs::read_to_string(project.join(LEVEL)).context("reading the level")?;
    let id = LevelId(Guid::new_random());
    let level = Level::from_ron_str(id, &text).context("the rewritten level loads")?;
    let mut app = App::new();
    app.add_plugins((ScenePlugin, BakedGiLevelPlugin));
    app.insert_resource(server);
    app.insert_resource(RuntimeRenderer::default());
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), id, LoadMode::Replace).context("opening the level")?;
    app.update();
    let state = app.world().resource::<LevelBakedGiState>();
    if !state.installed {
        bail!(
            "the level's volume is not installed: {:?} {:?}",
            state.error_code,
            state.error
        );
    }
    let renderer = &app.world().resource::<RuntimeRenderer>().0;
    let Some(volume) = renderer.baked_gi().cloned() else {
        bail!("the renderer has no volume");
    };
    let on_disk = BakedGiVolume::read(&project.join(VOLUME)).context("reading the volume")?;
    if volume.to_bytes().ok() != on_disk.to_bytes().ok() {
        bail!("the installed volume differs from the file");
    }
    let baked = volume.lights;
    let lights = BakedGiFrameLights {
        sun: Some(baked.sun_color.map(|c| c * baked.sun_illuminance)),
        to_sun: Some(baked.sun_direction),
        fill: None,
    };
    let settings = renderer.baked_gi_settings();
    let scales = component_scales(&volume, &settings, &lights);
    let up = [0.0, 1.0, 0.0];
    let look = |p: [f32; 3]| {
        let s = sample_static_gi(&volume, &settings, &scales, p, up, up);
        let lux = |v: [f32; 3]| v.map(|x| x * std::f32::consts::PI);
        (lux(s.irradiance_over_pi), lux(s.components[1]), s.coverage)
    };
    Ok([look(LIT), look(SEALED)])
}

fn step(name: &str, failures: &[String]) -> bool {
    if failures.is_empty() {
        println!("bake-check: {name}: ok");
        true
    } else {
        for f in failures {
            println!("bake-check: {name}: FAIL: {f}");
        }
        false
    }
}

/// Runs `cargo xtask bake-check`.
///
/// # Errors
///
/// A step that cannot run (cargo, the fixture copy, a missing output).
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    let project = temp_project(root)?;
    let result = run_in(args, root, &project);
    if args.keep {
        println!("bake-check: project kept at {}", project.display());
    } else {
        let _ = std::fs::remove_dir_all(&project);
    }
    result
}

fn run_in(args: &Args, root: &Path, project: &Path) -> Result<ExitCode> {
    let fail = |summary: Value| -> Result<ExitCode> {
        println!("{summary}");
        Ok(ExitCode::FAILURE)
    };
    let backend = if args.gpu { "gpu" } else { "cpu" };
    let level = project.join(LEVEL).display().to_string();
    let volume = project.join(VOLUME).display().to_string();
    let (rays, samples) = (args.rays.to_string(), args.samples.to_string());
    let mut bake_args = vec![
        "bake-gi",
        "--from-scene",
        &level,
        "--bake",
        "--write-level",
        "--rays",
        &rays,
        "--samples",
        &samples,
        "--bounces",
        "2",
    ];
    if !args.gpu {
        bake_args.push("--cpu");
    }

    // 1. Export, bake, name the volume.
    let out = regia(root, args.release, &bake_args)?;
    let line = json_line(&out)?;
    let mut failures = Vec::new();
    if !out.status.success() || line["ok"] != true {
        failures.push(format!("the bake failed: {line}"));
    }
    if line["statics"] != 2 || line["skipped_by"]["dynamic"] != 1 {
        failures.push(format!("the export counts are off: {line}"));
    }
    if line["bake"]["backend"] != backend {
        failures.push(format!("backend {} (want {backend})", line["bake"]["backend"]));
    }
    if line["level_written"] != true {
        failures.push("the level was not rewritten".into());
    }
    let report = line["bake"]["report"].clone();
    if !step("export + bake + write-level", &failures) {
        return fail(json!({"ok": false, "step": "bake", "line": line}));
    }
    let text = std::fs::read_to_string(project.join(DESC)).context("reading the description")?;
    let (desc, _) = GiBakeDescription::parse_versioned(&text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(sky) = desc.sky else {
        bail!("the description has no sky (Earth at noon exports one)");
    };

    // 2. Current.
    let check = regia(
        root,
        args.release,
        &["bake-gi", "--from-scene", &level, "--check"],
    )?;
    let current = check.status.code() == Some(0);
    if !step(
        "check (current)",
        &if current {
            Vec::new()
        } else {
            vec![format!("exit {:?}: {}", check.status.code(), json_line(&check)?)]
        },
    ) {
        return fail(json!({"ok": false, "step": "check"}));
    }

    // 3. Sample through the CLI.
    let lit_at = format!("{},{},{}", LIT[0], LIT[1], LIT[2]);
    let sealed_at = format!("{},{},{}", SEALED[0], SEALED[1], SEALED[2]);
    let s = regia(
        root,
        args.release,
        &[
            "bake-gi", "--sample", &volume, "--at", &lit_at, "--at", &sealed_at,
        ],
    )?;
    let sample = json_line(&s)?;
    let (cli_lit, cli_sealed) = (sampled(&sample, 0), sampled(&sample, 1));
    if !step("sample (cli)", &verdict(sky, cli_lit, cli_sealed)) {
        return fail(json!({"ok": false, "step": "sample", "sky": sky, "sample": sample}));
    }

    // 4. Runtime install and lookup.
    let [rt_lit, rt_sealed] = runtime_lookup(project)?;
    let mut failures = verdict(sky, rt_lit, rt_sealed);
    if rt_lit
        .0
        .iter()
        .zip(cli_lit.0)
        .any(|(rt, cli)| (rt - cli).abs() > 1e-4 * rt.max(1.0))
    {
        failures.push(format!("runtime {:?} and cli {:?} differ", rt_lit.0, cli_lit.0));
    }
    if !step("runtime install + lookup", &failures) {
        return fail(json!({"ok": false, "step": "runtime"}));
    }

    // 5. Moving a static entity makes the input stale.
    let original = std::fs::read_to_string(root.join(FIXTURE).join(LEVEL)).context("fixture level")?;
    let moved = original.replace("(24.0, 0.0, 0.0)", "(25.0, 0.0, 0.0)");
    if moved == original {
        bail!("the fixture's Marker translation was not found");
    }
    std::fs::write(project.join(LEVEL), moved).context("moving the marker")?;
    let stale = regia(
        root,
        args.release,
        &["bake-gi", "--from-scene", &level, "--check"],
    )?;
    if !step(
        "check (stale after a move)",
        &if stale.status.code() == Some(2) {
            Vec::new()
        } else {
            vec![format!("exit {:?} (want 2)", stale.status.code())]
        },
    ) {
        return fail(json!({"ok": false, "step": "stale"}));
    }

    let ratio =
        |v: [f32; 3]| -> Vec<f32> { (0..3).map(|c| v[c] / (std::f32::consts::PI * sky[c])).collect() };
    println!(
        "{}",
        json!({
            "ok": true,
            "backend": backend,
            "device": line["bake"]["device"],
            "report": report,
            "sky": sky,
            "lit": {"at": LIT, "irradiance": cli_lit.0, "sky": cli_lit.1, "sky_ratio": ratio(cli_lit.1)},
            "sealed": {"at": SEALED, "irradiance": cli_sealed.0},
            "bake_seconds": line["bake"]["seconds"],
        })
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKY: [f32; 3] = [1000.0, 2000.0, 4000.0];

    fn pi_l(k: f32) -> [f32; 3] {
        SKY.map(|c| c * std::f32::consts::PI * k)
    }

    #[test]
    fn the_verdict_accepts_the_expected_cells_and_names_each_miss() {
        let lit = (pi_l(1.05), pi_l(0.97), 1.0);
        let sealed = ([0.0; 3], [0.0; 3], 1.0);
        assert!(verdict(SKY, lit, sealed).is_empty());
        // Too little sky (a leak of the rooms into the open), light in the sealed room, a cell
        // outside the volume: each is named.
        let dim = verdict(SKY, (pi_l(0.9), pi_l(0.9), 1.0), sealed);
        assert_eq!(dim.len(), 3, "{dim:?}");
        let leak = verdict(SKY, lit, ([0.0, 1e-3, 0.0], [0.0; 3], 1.0));
        assert!(leak[0].contains("sealed room irradiance"), "{leak:?}");
        let outside = verdict(SKY, (lit.0, lit.1, 0.0), sealed);
        assert!(outside[0].contains("coverage"), "{outside:?}");
    }

    #[test]
    fn the_fixture_is_in_the_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let level = std::fs::read_to_string(root.join(FIXTURE).join(LEVEL)).expect("fixture level");
        assert!(level.contains("(24.0, 0.0, 0.0)"), "step 5 moves the Marker");
        assert!(root.join(FIXTURE).join("assets/meshes/room.gltf").is_file());
        assert!(root.join(FIXTURE).join("Regia.toml").is_file());
    }
}
