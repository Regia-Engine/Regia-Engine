// SPDX-License-Identifier: MPL-2.0
//! Baked light end to end, without a GPU (ADR-365; `cargo xtask bake-check` runs this file).
//!
//! The fixture project `tests/fixtures/bake_e2e` (a sealed room and a small copy of it under the
//! Earth sky, `README.md` there) goes through the real `regia` binary and the runtime:
//!
//! 1. `regia bake-gi --from-scene <level> --bake --cpu --write-level`: the level's statics
//!    export to `bake/courtyard.bake.ron`, the CPU probe tracer bakes
//!    `assets/maps/courtyard.fgiv`, and the level now names it.
//! 2. `--check` says the bake input is current (exit 0); moving a static entity makes it stale
//!    (exit 2).
//! 3. `--sample` reads the volume back: an open cell gets the exported sky's irradiance, the
//!    sealed room's centre gets none.
//! 4. The level opens in a headless app: `BakedGiLevelPlugin` installs the volume through the
//!    asset server, and the renderer's CPU lookup on the installed volume gives the same numbers.
//!
//! The tests copy the fixture to a temporary directory; nothing is written into the tree.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use regia::app::App;
use regia::framework::baked_gi_level::{BakedGiLevelPlugin, LevelBakedGiState};
use regia_asset::baked_gi::BakedGiVolume;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_core::id::Guid;
use regia_pt::bake_desc::GiBakeDescription;
use regia_render::{BakedGiFrameLights, RuntimeRenderer, component_scales, sample_static_gi};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::prelude::ScenePlugin;
use serde_json::Value;

/// An open cell 12 m east of the room, between the room and its copy.
const LIT: [f32; 3] = [14.8, 0.0, 0.0];
/// The sealed room's centre.
const SEALED: [f32; 3] = [0.0, 0.0, 0.0];
const UP: [f32; 3] = [0.0, 1.0, 0.0];
const LEVEL: &str = "assets/levels/courtyard.level.ron";
const VOLUME: &str = "assets/maps/courtyard.fgiv";
const DESC: &str = "bake/courtyard.bake.ron";
/// Light enough to stay quick on the CPU in a debug build, and the open cell's sky is exact
/// whatever the ray count (every open ray returns the uniform sky).
const BAKE_ARGS: [&str; 6] = ["--rays", "64", "--samples", "2", "--bounces", "2"];

fn regia_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_regia"))
}

/// The fixture, copied.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bake_e2e")
}

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for entry in std::fs::read_dir(from).expect("read the fixture") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy a fixture file");
        }
    }
}

fn project(tag: &str) -> (PathBuf, Cleanup) {
    let dir = regia_test_utils::unique_temp_dir(tag);
    copy_tree(&fixture(), &dir);
    (dir.clone(), Cleanup(dir))
}

fn regia(dir: &Path, args: &[&str]) -> Output {
    Command::new(regia_cli())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run regia")
}

/// The last JSON line on stdout.
fn json_line(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| {
            panic!(
                "no JSON line; stdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))
}

fn f(v: &Value) -> f32 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}")) as f32
}

fn rgb(v: &Value) -> [f32; 3] {
    [f(&v[0]), f(&v[1]), f(&v[2])]
}

/// The uniform sky radiance the export wrote (Earth at noon).
fn exported_sky(dir: &Path) -> [f32; 3] {
    let text = std::fs::read_to_string(dir.join(DESC)).expect("the description");
    let (desc, version) = GiBakeDescription::parse_versioned(&text).expect("parses");
    assert_eq!(version, 1);
    let sky = desc.sky.expect("Earth exports a sky");
    assert!(sky.iter().all(|c| *c > 0.0), "{sky:?}");
    sky
}

/// Asserts the open cell's sky against `sky` and the sealed room's darkness, from one point's
/// irradiance per component (lux) and coverage. `what` names the source.
fn check_expectations(what: &str, sky: [f32; 3], lit: ([f32; 3], [f32; 3], f32), sealed: ([f32; 3], f32)) {
    let (lit_total, lit_sky, lit_cov) = lit;
    assert_eq!(lit_cov, 1.0, "{what}: the open cell is inside the volume");
    for c in 0..3 {
        let ratio = lit_sky[c] / (std::f32::consts::PI * sky[c]);
        // The two rooms hide about 3 % of the open cell's sky; nothing adds sky.
        assert!((0.94..=1.02).contains(&ratio), "{what}: sky channel {c}: {ratio}");
        assert!(
            lit_total[c] >= lit_sky[c] * 0.999,
            "{what}: total below its sky part"
        );
    }
    let (sealed_total, sealed_cov) = sealed;
    assert_eq!(sealed_cov, 1.0, "{what}: the room's centre is inside the volume");
    assert_eq!(sealed_total, [0.0; 3], "{what}: no light enters the sealed room");
}

/// The full chain: export, CPU bake, level rewrite, check, sample, runtime install, lookup.
#[test]
fn the_fixture_bakes_installs_and_lights_an_open_and_a_sealed_cell() {
    let (dir, _cleanup) = project("regia_bake_e2e");
    let level = dir.join(LEVEL);

    // 1. Export + CPU bake + name the volume in the level.
    let mut args = vec![
        "bake-gi",
        "--from-scene",
        LEVEL,
        "--bake",
        "--cpu",
        "--write-level",
    ];
    args.extend(BAKE_ARGS);
    let out = regia(&dir, &args);
    let line = json_line(&out);
    assert!(
        out.status.success(),
        "{line}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(line["ok"], true, "{line}");
    assert_eq!(line["statics"], 2, "Room and Marker: {line}");
    assert_eq!(line["skipped_by"]["dynamic"], 1, "Rover: {line}");
    assert_eq!(line["sun_source"], "time_of_day");
    assert_eq!(line["sky"], true);
    assert_eq!(line["level_written"], true);
    assert_eq!(line["volume"], "maps/courtyard.fgiv");
    let bake = &line["bake"];
    assert_eq!(bake["backend"], "cpu", "{bake}");
    let report = &bake["report"];
    assert!(report["probes"].as_u64().is_some_and(|p| p > 1000), "{report}");
    assert_eq!(report["rejected_rays"], 0, "{report}");
    assert!(report["valid"].as_u64().is_some_and(|n| n > 1000), "{report}");
    assert_eq!(report["under_terrain"], 0, "no terrain: {report}");
    assert!(dir.join(VOLUME).is_file());
    let rewritten = std::fs::read_to_string(&level).expect("level");
    assert!(rewritten.contains("maps/courtyard.fgiv"), "{rewritten}");
    let sky = exported_sky(&dir);

    // 2. The bake input is current; a second write-level changes nothing.
    let check = regia(&dir, &["bake-gi", "--from-scene", LEVEL, "--check"]);
    assert_eq!(check.status.code(), Some(0), "{}", json_line(&check));
    assert_eq!(json_line(&check)["stale"], false);

    // 3. `--sample` reads the volume back.
    let sampled = regia(
        &dir,
        &[
            "bake-gi", "--sample", VOLUME, "--at", "14.8,0,0", "--at", "0,0,0", "--normal", "0,1,0",
        ],
    );
    let s = json_line(&sampled);
    assert!(sampled.status.success(), "{s}");
    assert_eq!(s["ok"], true);
    assert_eq!(
        s["bricks"], 3,
        "the volume brick and one automatic brick per room: {s}"
    );
    let point = |k: usize| &s["samples"][k];
    check_expectations(
        "--sample",
        sky,
        (
            rgb(&point(0)["irradiance"]),
            rgb(&point(0)["components"]["sky"]),
            f(&point(0)["coverage"]),
        ),
        (rgb(&point(1)["irradiance"]), f(&point(1)["coverage"])),
    );

    // 4. The runtime installs the level's volume and lights the same cells.
    let server = AssetServer::open(AssetServerSettings {
        project_root: dir.clone(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .expect("asset server");
    let mut app = App::new();
    app.add_plugins((ScenePlugin, BakedGiLevelPlugin));
    app.insert_resource(server);
    app.insert_resource(RuntimeRenderer::default());
    let id = LevelId(Guid::new_random());
    let parsed = Level::from_ron_str(id, &rewritten).expect("the rewritten level loads");
    app.world_mut().resource_mut::<LevelLibrary>().insert(parsed);
    open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
    app.update();
    let state = app.world().resource::<LevelBakedGiState>();
    assert!(state.installed, "{:?} {:?}", state.error_code, state.error);
    assert_eq!(state.volume.as_deref(), Some("maps/courtyard.fgiv"));
    let renderer = &app.world().resource::<RuntimeRenderer>().0;
    let installed: Arc<BakedGiVolume> = renderer.baked_gi().cloned().expect("a volume is installed");
    assert!(state.owns(Some(&installed)));
    assert_eq!(state.probes, installed.probe_count());
    let on_disk = BakedGiVolume::read(&dir.join(VOLUME)).expect("read");
    assert_eq!(
        installed.to_bytes().ok(),
        on_disk.to_bytes().ok(),
        "the file, unchanged"
    );

    // The frame's lights equal the bake's, so every component is at its baked light.
    let baked = installed.lights;
    let lights = BakedGiFrameLights {
        sun: Some(baked.sun_color.map(|c| c * baked.sun_illuminance)),
        to_sun: Some(baked.sun_direction),
        fill: None,
    };
    let settings = renderer.baked_gi_settings();
    let scales = component_scales(&installed, &settings, &lights);
    let lookup = |p: [f32; 3]| {
        let s = sample_static_gi(&installed, &settings, &scales, p, UP, UP);
        let lux = |v: [f32; 3]| v.map(|x| x * std::f32::consts::PI);
        (lux(s.irradiance_over_pi), lux(s.components[1]), s.coverage)
    };
    let (lit_total, lit_sky, lit_cov) = lookup(LIT);
    let (sealed_total, _, sealed_cov) = lookup(SEALED);
    check_expectations(
        "runtime",
        sky,
        (lit_total, lit_sky, lit_cov),
        (sealed_total, sealed_cov),
    );
    let cli_lit = rgb(&point(0)["irradiance"]);
    for c in 0..3 {
        assert!(
            (cli_lit[c] - lit_total[c]).abs() <= 1e-4 * lit_total[c].max(1.0),
            "the CLI and the installed volume agree: {cli_lit:?} vs {lit_total:?}"
        );
    }

    // 2b. Moving a static entity makes the bake input stale (exit 2). The fixture's own text is
    // edited (the volume it names is not part of the bake input).
    let original = std::fs::read_to_string(fixture().join(LEVEL)).expect("fixture level");
    let moved = original.replace("(24.0, 0.0, 0.0)", "(25.0, 0.0, 0.0)");
    assert_ne!(
        moved, original,
        "the Marker's translation is in the fixture level"
    );
    regia_test_utils::write_atomic(&level, moved.as_bytes()).expect("move the marker");
    let stale = regia(&dir, &["bake-gi", "--from-scene", LEVEL, "--check"]);
    assert_eq!(stale.status.code(), Some(2), "{}", json_line(&stale));
    assert_eq!(json_line(&stale)["reason"], "inputs_changed");
}

/// `--sample` errors are one JSON line with a code and a failing exit.
#[test]
fn sample_errors_have_codes() {
    let (dir, _cleanup) = project("regia_bake_e2e_errors");
    let missing = regia(
        &dir,
        &["bake-gi", "--sample", "assets/maps/none.fgiv", "--at", "0,0,0"],
    );
    assert!(!missing.status.success());
    assert_eq!(json_line(&missing)["code"], "volume_not_found");
    std::fs::create_dir_all(dir.join("assets/maps")).expect("maps dir");
    regia_test_utils::write_atomic(&dir.join("assets/maps/bad.fgiv"), b"FGIV but not really").expect("write");
    let bad = regia(
        &dir,
        &["bake-gi", "--sample", "assets/maps/bad.fgiv", "--at", "0,0,0"],
    );
    assert!(!bad.status.success());
    assert_eq!(json_line(&bad)["code"], "volume_unreadable");
    // `--at` without `--sample` is a usage error (clap), not a bake.
    let usage = regia(&dir, &["bake-gi", "furnace", "--at", "0,0,0"]);
    assert!(!usage.status.success());
}

/// `--dry-run` on a fresh project (no `bake/` directory yet) lays out the exported description
/// without writing anything. The description's mesh paths are `../assets/...` relative to
/// `bake/`, which the OS cannot walk through before the directory exists (found by the v0.2.0
/// look review, ADR-1326).
#[test]
fn a_dry_run_on_a_fresh_project_writes_nothing_and_lays_out() {
    let (dir, _cleanup) = project("regia_bake_e2e_dry_run");
    assert!(!dir.join("bake").exists(), "the fixture has no bake directory");
    let out = regia(&dir, &["bake-gi", "--from-scene", LEVEL, "--dry-run"]);
    let line = json_line(&out);
    assert!(out.status.success(), "{line}");
    assert_eq!(line["dry_run"], true);
    assert_eq!(line["statics"], 2);
    assert!(line["layers"]["probes"].as_u64().is_some_and(|p| p > 0), "{line}");
    assert!(!dir.join("bake").exists(), "a dry run writes no description");
    assert!(!dir.join(VOLUME).exists(), "and no volume");
}
