// SPDX-License-Identifier: MPL-2.0
//! `regia bake-lightmaps --scene <level>` end to end through the real binary (ADR-2024): the fixture
//! project `tests/fixtures/bake_e2e` (a room and a small copy of it under the Earth sky) bakes with
//! no `.bake.ron`, entries keyed by entity, chart seams measured, and the file reads back. CPU only;
//! the fixture is copied to a temporary directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regia_asset::lightmap::{LIGHTMAP_FORMAT_VERSION, Lightmap, LightmapEntryKey, lightmap_version};
use serde_json::Value;

const LEVEL: &str = "assets/levels/courtyard.level.ron";
const OUT: &str = "assets/maps/courtyard.flmap";
const ROOM: &str = "00000000-0000-0000-0000-0000000000a1";
const MARKER: &str = "00000000-0000-0000-0000-0000000000b2";
/// Quick on the CPU in a debug build.
const QUICK: [&str; 8] = [
    "--density",
    "2",
    "--samples",
    "8",
    "--bounces",
    "2",
    "--threads",
    "2",
];

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dirs");
    for entry in std::fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).expect("copy");
        }
    }
}

fn project(tag: &str) -> Cleanup {
    let dir = regia_test_utils::unique_temp_dir(tag);
    copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bake_e2e"),
        &dir,
    );
    Cleanup(dir)
}

fn regia(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run regia")
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn a_level_bakes_with_no_description_and_keys_its_entries() {
    let tmp = project("lightmap_scene_e2e");
    let dir = &tmp.0;
    let mut args = vec!["bake-lightmaps", "--scene", LEVEL];
    args.extend(QUICK);
    let out = regia(dir, &args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let v = json_of(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["atlas"]["generator"], "atlas");
    assert_eq!(v["settings"]["backend"], "cpu");
    // Room and Marker are static; the dynamic Rover only shadows.
    assert_eq!(v["level"]["statics"], 2);
    assert_eq!(v["level"]["skipped_by"]["dynamic"], 1);
    assert!(v["level"]["input_hash"].is_string());
    let entries = v["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(entries[0]["entity"], ROOM);
    assert_eq!(entries[0]["label"], format!("Room ({ROOM})"));
    assert_eq!(entries[1]["entity"], MARKER);
    assert!(v["seams"]["entries"].is_array(), "{v}");
    assert!(!dir.join("bake").exists(), "no description is left behind");
    assert!(!dir.join("lightmap.bake.ron").exists());

    // The file: the current version, keyed and lit by the level's sky and sun.
    let file = dir.join(OUT);
    let bytes = std::fs::read(&file).expect("the default output");
    assert_eq!(
        lightmap_version(&bytes).expect("version"),
        LIGHTMAP_FORMAT_VERSION
    );
    let map = Lightmap::from_bytes(&bytes).expect("reads back");
    let key = LightmapEntryKey {
        entity: MARKER.parse().expect("guid"),
        part: 0,
    };
    assert_eq!(map.entry_for_key(key), Some(1));
    assert!(
        (0..map.height).any(|y| (0..map.width).any(|x| map.irradiance_at(x, y)[1] > 0.0)),
        "the sky reaches the open surfaces"
    );

    // `--inspect` reads the key back, and a second bake writes the same bytes.
    let inspected = regia(dir, &["bake-lightmaps", OUT, "--inspect"]);
    assert!(inspected.status.success());
    let i = json_of(&inspected);
    assert_eq!(i["version"], LIGHTMAP_FORMAT_VERSION);
    assert_eq!(i["entries"][0]["entity"], ROOM);
    assert_eq!(i["entries"][1]["part"], 0);
    let out = regia(dir, &args);
    assert!(out.status.success());
    assert_eq!(
        std::fs::read(&file).expect("read"),
        bytes,
        "a re-bake is byte-identical"
    );
}

/// `--meshlets` through the real binary (ADR-2107): every entry of a level gets clusters that decode,
/// fit its unwrap and keep each triangle inside one chart, `--inspect` shows them, and a bake
/// without the flag stores none.
#[test]
fn meshlets_bake_clusters_that_fit_each_entry() {
    let tmp = project("lightmap_scene_meshlets_e2e");
    let dir = &tmp.0;
    let mut args = vec!["bake-lightmaps", "--scene", LEVEL];
    args.extend(QUICK);
    let plain = regia(dir, &args);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stdout)
    );
    assert_eq!(json_of(&plain)["meshlets"], Value::Null);
    assert_eq!(
        Lightmap::read(&dir.join(OUT)).expect("reads").meshlet_entries(),
        0
    );

    args.push("--meshlets");
    let out = regia(dir, &args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let v = json_of(&out);
    assert_eq!(v["meshlets"]["entries"], 2, "{v}");
    assert_eq!(v["meshlets"]["refused"], serde_json::json!([]));
    assert!(v["meshlets"]["bytes"].as_u64().expect("bytes") > 0);
    let map = Lightmap::read(&dir.join(OUT)).expect("reads");
    assert_eq!(map.meshlet_entries(), 2);
    for k in 0..map.entries.len() {
        let blob = map.entry_meshlets(k).expect("a blob");
        let mesh = regia_render::MeshletMesh::from_bytes(blob).expect("decodes");
        let unwrap = &map.unwraps[k];
        assert_eq!(mesh.source_vertex_count as usize, unwrap.xref.len());
        assert_eq!(mesh.source_index_count as usize, unwrap.indices.len());
        let charts =
            regia_asset_pipeline::lightmap_uv::chart_of_each_vertex(unwrap.xref.len(), &unwrap.indices);
        assert_eq!(
            regia_asset_pipeline::lightmap_uv::cluster_triangles_across_charts(&mesh, &charts),
            0,
            "entry {k}"
        );
    }
    let inspected = regia(dir, &["bake-lightmaps", OUT, "--inspect"]);
    assert_eq!(json_of(&inspected)["meshlet_entries"], 2);
}

/// `--dry-run` writes nothing and needs no bake; `--unwrap charts` keeps the planar generator; a level
/// that is missing is a coded error on stdout, and `--scene` refuses a positional scene.
#[test]
fn dry_runs_generators_and_errors() {
    let tmp = project("lightmap_scene_e2e_dry");
    let dir = &tmp.0;
    let out = regia(
        dir,
        &[
            "bake-lightmaps",
            "--scene",
            LEVEL,
            "--density",
            "2",
            "--dry-run",
            "--unwrap",
            "charts",
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let v = json_of(&out);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["atlas"]["generator"], "charts");
    assert!(!dir.join(OUT).exists(), "a dry run writes nothing");

    let missing = regia(
        dir,
        &["bake-lightmaps", "--scene", "assets/levels/nope.level.ron"],
    );
    assert!(!missing.status.success());
    assert_eq!(json_of(&missing)["code"], "export_read_failed");

    let both = regia(dir, &["bake-lightmaps", "cornell", "--scene", LEVEL]);
    assert!(!both.status.success(), "a scene and a level at once");
    let neither = regia(dir, &["bake-lightmaps"]);
    assert!(!neither.status.success(), "a scene or a level is required");
}

/// `--write-level` through the real binary (ADR-2313): the level names the lightmap it baked
/// (`look.lightmap.file`, scene format v11), a second run finds it named, and the flag is refused
/// without `--scene` and with `--dry-run`.
#[test]
fn write_level_names_the_lightmap_in_the_level() {
    let tmp = project("lightmap_scene_write_level");
    let dir = &tmp.0;
    let mut args = vec!["bake-lightmaps", "--scene", LEVEL, "--write-level"];
    args.extend(QUICK);
    let out = regia(dir, &args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let v = json_of(&out);
    assert_eq!(v["level_written"], true, "{v}");
    let text = std::fs::read_to_string(dir.join(LEVEL)).expect("level");
    assert!(
        text.contains(&format!(
            "format_version: {}",
            regia_scene::format::SCENE_FORMAT_VERSION
        )),
        "{text}"
    );
    let level = regia_scene::level::Level::from_ron_str(regia_scene::level::LevelId::default(), &text)
        .expect("the rewritten level reads");
    assert_eq!(
        level.settings.look.lightmap.map(|l| l.file),
        Some("maps/courtyard.flmap".to_string())
    );
    let again = json_of(&regia(dir, &args));
    assert_eq!(again["level_written"], false, "{again}");

    for refused in [
        vec!["bake-lightmaps", "quad-box", "--write-level"],
        vec!["bake-lightmaps", "--scene", LEVEL, "--write-level", "--dry-run"],
    ] {
        let out = regia(dir, &refused);
        assert!(!out.status.success(), "{refused:?} is refused");
    }
}
