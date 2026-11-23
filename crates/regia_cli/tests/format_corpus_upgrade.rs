// SPDX-License-Identifier: MPL-2.0
//! `regia upgrade-project` over the old-bytes format corpus (ADR-2743, `docs/specs/12-assets.md` § 4a).
//!
//! Copies every older-version fixture of `crates/regia_agent/tests/fixtures/format-corpus/` into one project and
//! checks that the dry run reports what the real run does, that every file reaches its format's current version,
//! and that a second run changes no byte. A corpus file the command does not recognize is listed in
//! `NOT_HANDLED` with the reason; an unlisted one fails the test.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use format_corpus_support::{corpus_dir, load_all_indexes};
use regia_test_utils::unique_temp_dir;
use serde_json::Value;

/// Corpus formats `upgrade-project` does not read, with the reason (each is a finding in the lane report).
const NOT_HANDLED: &[(&str, &str)] = &[
    (
        "post-volume",
        "a reflected component inside .level.ron / .prefab.ron: the scene loader runs its migrate(); upgrade-project rewrites only the scene's own version",
    ),
    (
        "procgen-generator",
        "a reflected component inside .level.ron / .prefab.ron: the scene loader runs its migrate(); upgrade-project rewrites only the scene's own version",
    ),
    (
        "procgen-painted-points",
        "a reflected component stored inside .level.ron / .prefab.ron and level dirs, never a file of its own: upgrade-project migrates it there (upgrade_components, ADR-7215; unit test painted_points_v1_upgrade_to_v2_with_yaw_zero); the corpus fixture is the bare component value",
    ),
    (
        "lobby-keys",
        "lobby platform data (the Steam or hub lobby's key-value map), never a project file, so the ADR-2316 sweep gave it no upgrade-project entry; a v1 lobby reads unchanged under v2 (two optional keys, ADR-8897; integ-v07b ADR-10076)",
    ),
    (
        "nav-cell",
        "the cooked nav cell is not a standalone file (bytes inside a cell blob), so the ADR-2316 sweep gave it no upgrade-project entry",
    ),
    (
        "gpu-mesh",
        "a pak entry only (desktop paks; the cooked cache keeps the compact mesh), never a project file: v1 and v2 entries still load (GpuMeshView::parse, gpu_mesh_migration_sources) and the next cook writes v3; the loader's migrate-and-reencode is checked by format_corpus (ADR-10080)",
    ),
    (
        "test-report",
        "a run output, not project content: `regia test --report <path>` writes it where the caller points (commands/test.rs write_reports), nothing reads one back from a project, and it has no suffix of its own (`.json`); TestReport::from_json migrates v1 (ADR-7980, ADR-8580), checked by historical_test_reports_migrate_through_the_report_loader",
    ),
];

/// Formats upgrade-project owns by whole file name, not by suffix (`"/input_overrides.ron"`): the fixture goes into
/// its own folder under that name.
const WHOLE_NAME: &[(&str, &str)] = &[
    ("input-overrides", "input_overrides.ron"),
    ("server-toml", "server.toml"),
    ("bans", "bans.ron"),
    ("net-settings", "net.ron"),
    ("launch-profiles", "launch_profiles.ron"),
    // `config/audio.ron` (ADR-8354): the versionless v0 shape v0.5.0 shipped upgrades to v1.
    ("audio-buses", "audio.ron"),
    ("release-chunks", "release-chunks.json"),
    ("hub-toml", "hub.toml"),
];

/// Formats upgrade-project classifies by a compound suffix that a blessed fixture name (`v<N>.ron`, as the corpus
/// bless step writes it) does not carry: every fixture of the format is placed as `<format>_v<N>.<suffix>`.
const SUFFIX: &[(&str, &str)] = &[
    ("camera", "camera.ron"),
    ("terrain-layers", "terrain.layers.ron"),
    ("bt", "bt.ron"),
    ("decal", "decal.ron"),
    // `v3.ron` was the current fixture until v5 (ADR-2948); an old one keeps the `.seq.ron` the command reads.
    ("sequence", "seq.ron"),
    // The current fixtures were blessed as `v<N>.ron` / `v<N>.json`; the command reads `.ui.ron` and `.rprof.json`
    // (the v11 and profile-capture v2 bumps made `ui/v10.ron` and `profile-capture/v1.json` old fixtures; ADR-7422).
    ("ui", "ui.ron"),
    ("profile-capture", "rprof.json"),
    // Fixtures named `vN.ron` / `vN.bin` (ADR-7346 (3)): the project files keep their real suffixes.
    ("vfx", "vfx.ron"),
    ("audiopatch", "audiopatch.ron"),
    ("pgout", "pgout"),
    // The editor's dock layouts (ADR-7902): `v1.ron` is placed as `editor-layout_v1.editor_layout.ron`, a named
    // layout's suffix.
    ("editor-layout", "editor_layout.ron"),
    // `.formation.ron` (ADR-8358): the corpus file is `v1.ron`.
    ("ai-formation", "formation.ron"),
    // ADR-7819 made `meta/v1.ron` an old fixture; the command reads sidecars by `.meta` and needs no sibling source
    // file (ADR-8580).
    ("meta", "meta"),
    // The v0.6 bumps of matfunc (v4), matgraph (v5) and theme (v2) made `matfunc/v3.ron`, `matgraph/v4.ron` and
    // `theme/v1.ron` old fixtures (ADR-8580).
    ("matfunc", "matfunc.ron"),
    ("matgraph", "matgraph.ron"),
    ("theme", "theme.ron"),
    // Rows for the v0.6 bumps still on lanes (ADR-8580): each one's old fixture is `v<N>.ron` / `v<N>.bin`. Harmless
    // while every fixture of the format is current (populate places only older ones).
    ("shots", "shots.ron"),
    ("uisettings", "uisettings.ron"),
    ("terrain-editlayers", "terrain.editlayers"),
    ("retarget", "retarget.ron"),
    ("netprofile", "netprofile.ron"),
    // `.detutility.ron` (gap AI-7): the corpus file is `v1.ron`.
    ("ai-det-utility", "detutility.ron"),
    // The det line's v4 (chain IK, stride warp) made `animgraph/v3.ron` an old fixture.
    ("animgraph", "animgraph.ron"),
    // The cooked mesh layers (ADR-8816): `v1.bin` (v0.6.0) is placed as `<name>.meshlayers`, the suffix the
    // command reads; v2 stores fixed-width counts behind a hashed head.
    ("terrain-mesh-layers-cooked", "meshlayers"),
    // Format batch 2 (ADR-8900): `.logic.ron` v2 and `.uianim.ron` v2 made `logic/v1.ron` and `uianim/v1.ron` old
    // fixtures.
    ("logic", "logic.ron"),
    ("uianim", "uianim.ron"),
    // `.gait.ron` (gap G7): the corpus file is `v1.ron`.
    ("gait", "gait.ron"),
    // Scene 14 (TerrainMap spacing, ADR-10079) made `scene/v13.ron`, blessed under the bare `.ron` name, an old
    // fixture; the command reads levels by `.level.ron` (ADR-10183). The older `v<N>.level.ron` fixtures keep it.
    ("scene", "level.ron"),
    // `.playlist.ron` v2 (format batch 2, ADR-10064) made `playlist-ron/v1.ron` an old fixture (ADR-10183).
    ("playlist-ron", "playlist.ron"),
];

/// Formats whose corpus fixture is the bare payload of a cooked `.fckd` file (the cook writes no standalone file):
/// the fixture is wrapped in a cooked header of this type path and placed as `<format>_v<N>.fckd`.
const COOKED_PAYLOAD: &[(&str, &str)] = &[
    ("voxel-far", regia::render::visbuffer::VOXEL_FAR_TYPE_PATH),
    // ADR-7817: cooked motion databases v1 and v2 upgrade to v3.
    (
        "motiondb-cooked",
        regia_animation::motion_matching::MOTION_DATABASE_TYPE_PATH,
    ),
    // ADR-8024: cooked terrain indexes v1 upgrade to v2.
    ("terrain-index", regia_terrain::stream::TERRAIN_INDEX_TYPE_PATH),
];

/// `payload` in a cooked file of `type_path`, with a fixed asset id so every run writes the same bytes.
fn cooked_file(type_path: &str, payload: &[u8]) -> Vec<u8> {
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(type_path),
        asset_id: regia_asset::AssetId(regia_core::id::Guid::from_u128(
            0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100,
        )),
        importer_version: 1,
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    regia_asset::encode_cooked_file(&header, payload).expect("wrap the payload in a cooked file")
}

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("spawn the regia binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn plan(project: &Path, write: bool) -> Value {
    let mut args = vec![
        "upgrade-project",
        "--project",
        project.to_str().expect("UTF-8"),
        "--json",
    ];
    if write {
        args.push("--write");
    }
    let out = regia(&args);
    assert!(out.status.success(), "upgrade-project failed: {}", stderr(&out));
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

/// Every non-backup file of the project: relative path → bytes.
fn snapshot(project: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut all = BTreeMap::new();
    let mut stack = vec![project.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the project").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_none_or(|e| e != "bak") {
                let rel = path.strip_prefix(project).expect("inside the project");
                all.insert(
                    rel.to_string_lossy().replace('\\', "/"),
                    std::fs::read(&path).expect("read"),
                );
            }
        }
    }
    all
}

/// The version each format is at now (`docs/formats/format-hashes.ron`).
fn current_versions() -> BTreeMap<String, u32> {
    #[derive(serde::Deserialize)]
    struct Hashes {
        formats: BTreeMap<String, Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        version: u32,
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats/format-hashes.ron");
    let text = std::fs::read_to_string(&path).expect("read format-hashes.ron");
    let hashes: Hashes = ron::from_str(&text).expect("parse format-hashes.ron");
    hashes.formats.into_iter().map(|(k, v)| (k, v.version)).collect()
}

/// Places one corpus fixture of `format` in `project` under the name upgrade-project classifies it by
/// (`<format>_v<N>.<suffix>`, or `<format>_v<N>/<whole name>`); returns that project path.
fn place(project: &Path, format: &str, entry: &format_corpus_support::Entry) -> String {
    // `v3.material.ron` → `material_v3.material.ron`: keep the suffix the command classifies by.
    let cooked = COOKED_PAYLOAD.iter().find(|(f, _)| *f == format);
    let suffix = match (SUFFIX.iter().find(|(f, _)| *f == format), cooked) {
        (Some((_, suffix)), _) => suffix,
        (None, Some(_)) => "fckd",
        (None, None) => entry.file.split_once('.').map_or("", |(_, rest)| rest),
    };
    let name = match WHOLE_NAME.iter().find(|(f, _)| *f == format) {
        Some((_, whole)) => format!("{format}_v{}/{whole}", entry.version),
        None => format!("{format}_v{}.{suffix}", entry.version),
    };
    let from: PathBuf = corpus_dir().join(format).join(&entry.file);
    let to = project.join(&name);
    std::fs::create_dir_all(to.parent().expect("a parent")).expect("create the folder");
    let bytes = std::fs::read(&from).unwrap_or_else(|e| panic!("{}: {e}", from.display()));
    let bytes = match cooked {
        Some((_, type_path)) => cooked_file(type_path, &bytes),
        None => bytes,
    };
    std::fs::write(&to, bytes).unwrap_or_else(|e| panic!("{}: {e}", to.display()));
    name
}

/// Copies the older fixtures into `project` as `<format>_v<N>.<suffix>`; returns project path → (format, version).
fn populate(project: &Path) -> BTreeMap<String, (String, u32)> {
    let current = current_versions();
    let mut placed = BTreeMap::new();
    for (format, entries) in load_all_indexes() {
        let now = current
            .get(&format)
            .copied()
            .unwrap_or_else(|| panic!("{format}: not a registered format in format-hashes.ron"));
        for entry in entries.iter().filter(|e| e.version < now) {
            let name = place(project, &format, entry);
            placed.insert(name, (format.clone(), entry.version));
        }
    }
    placed
}

#[test]
fn upgrade_project_brings_every_old_fixture_to_the_current_version() {
    let project = unique_temp_dir("format_corpus_upgrade");
    let placed = populate(&project);
    assert!(!placed.is_empty(), "no older fixtures in the corpus");

    // The dry run: what the command would do, and nothing written.
    let before = snapshot(&project);
    let dry = plan(&project, false);
    assert_eq!(snapshot(&project), before, "the dry run wrote something");
    let files = dry["files"].as_array().expect("files");
    let mut would_migrate = Vec::new();
    let mut unhandled = Vec::new();
    for (name, (format, version)) in &placed {
        match files
            .iter()
            .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with(name.as_str())))
        {
            None => unhandled.push(format!("{name} ({format} v{version})")),
            Some(f) => {
                assert_eq!(f["status"], "migrate", "{name}: dry run says {f}");
                assert_eq!(
                    f["found"], *version,
                    "{name}: dry run read the wrong version: {f}"
                );
                would_migrate.push(name.clone());
            }
        }
    }
    let unexpected: Vec<&String> = unhandled
        .iter()
        .filter(|u| !NOT_HANDLED.iter().any(|(f, _)| u.contains(&format!("({f} v"))))
        .collect();
    assert!(
        unexpected.is_empty(),
        "upgrade-project does not recognize these corpus files (fix it or list them in NOT_HANDLED): {unexpected:?}"
    );

    // The real run changes exactly the files the dry run named.
    let written = plan(&project, true);
    assert_eq!(written["ok"], true);
    let after = snapshot(&project);
    let mut changed: Vec<String> = after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .collect();
    changed.sort();
    would_migrate.sort();
    assert_eq!(
        changed, would_migrate,
        "the real run changed other files than the dry run reported"
    );

    // Every file is at its current version now, and the originals are kept as backups.
    let check = plan(&project, false);
    for f in check["files"].as_array().expect("files") {
        assert_eq!(f["status"], "current", "after --write: {f}");
    }
    for name in &would_migrate {
        let bak = project.join(format!("{name}.bak"));
        assert_eq!(
            std::fs::read(&bak).expect("a backup"),
            before[name],
            "{name}: the backup is not the original"
        );
    }

    // A second run changes no byte.
    let again = plan(&project, true);
    assert_eq!(again["ok"], true);
    assert_eq!(snapshot(&project), after, "a second --write changed bytes");
    let _ = std::fs::remove_dir_all(&project);
}

/// The gap audit named `.uianim.ron`: upgrade-project reads it, and a current file is left alone. Since format
/// batch 2 (ADR-8900) the current fixture is `uianim/v2.ron`; the v1 one is planned for migration.
#[test]
fn upgrade_project_reads_uianim_files() {
    let project = unique_temp_dir("format_corpus_uianim");
    std::fs::copy(
        corpus_dir().join("uianim/v2.ron"),
        project.join("menu.uianim.ron"),
    )
    .expect("copy the fixture");
    let before = snapshot(&project);
    let dry = plan(&project, false);
    let files = dry["files"].as_array().expect("files");
    let entry = files
        .iter()
        .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with("menu.uianim.ron")))
        .expect("upgrade-project lists the .uianim.ron file");
    assert_eq!(entry["format"], "uianim");
    assert_eq!(entry["status"], "current", "{entry}");
    let _ = plan(&project, true);
    assert_eq!(snapshot(&project), before, "a current .uianim.ron was rewritten");
    std::fs::copy(corpus_dir().join("uianim/v1.ron"), project.join("old.uianim.ron"))
        .expect("copy the v1 fixture");
    let dry = plan(&project, false);
    let old = dry["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with("old.uianim.ron")))
        .cloned()
        .expect("upgrade-project lists the v1 file");
    assert_eq!(old["status"], "migrate", "{old}");
    let _ = std::fs::remove_dir_all(&project);
}

/// DO27 CONTENT_050 (ADR-7978): every older terrain-layers corpus fixture stored under the plain per-map name
/// `maps/<map>/textures/terrain.layers.ron` (and a mixed-case `Terrain.Layers.ron`), the names the cook and the live
/// loader take as a layer stack, is planned as `terrain-layers` at its version, reaches the current version under
/// `--write` with a `.bak` of the original bytes, and a second run changes no byte.
#[test]
fn upgrade_project_migrates_terrain_layers_under_the_names_the_cook_accepts() {
    let project = unique_temp_dir("format_corpus_terrain_plain_name");
    let now = current_versions()["terrain-layers"];
    let entries = load_all_indexes()
        .remove("terrain-layers")
        .expect("terrain-layers is in the corpus");
    let mut placed = BTreeMap::new();
    for entry in entries.iter().filter(|e| e.version < now) {
        let bytes =
            std::fs::read(corpus_dir().join("terrain-layers").join(&entry.file)).expect("read the fixture");
        for name in [
            format!("maps/map_v{}/textures/terrain.layers.ron", entry.version),
            format!("maps/map_v{}/Terrain.Layers.ron", entry.version),
        ] {
            let to = project.join(&name);
            // The engine's own readers take this name as a terrain layer stack.
            assert!(
                !regia_asset_pipeline::discover_terrains(std::slice::from_ref(&to)).is_empty(),
                "{name}: the cook does not discover it"
            );
            assert_eq!(
                regia_asset::SourceKind::of(&to),
                regia_asset::SourceKind::TerrainLayers,
                "{name}: the live loader does not classify it as terrain layers"
            );
            std::fs::create_dir_all(to.parent().expect("a parent")).expect("create the folder");
            std::fs::write(&to, &bytes).expect("place the fixture");
            placed.insert(name, entry.version);
        }
    }
    assert!(
        placed.len() >= 2,
        "no older terrain-layers fixtures in the corpus"
    );

    let before = snapshot(&project);
    let dry = plan(&project, false);
    let files = dry["files"].as_array().expect("files");
    for (name, version) in &placed {
        let f = files
            .iter()
            .find(|f| {
                f["path"]
                    .as_str()
                    .is_some_and(|p| p.replace('\\', "/").ends_with(name.as_str()))
            })
            .unwrap_or_else(|| panic!("upgrade-project does not list {name} (v{version}): {dry}"));
        assert_eq!(f["format"], "terrain-layers", "{name}: {f}");
        assert_eq!(f["status"], "migrate", "{name}: {f}");
        assert_eq!(f["found"], *version, "{name}: {f}");
    }

    let written = plan(&project, true);
    assert_eq!(written["ok"], true);
    for name in placed.keys() {
        let text = std::fs::read_to_string(project.join(name)).expect("read the migrated file");
        let layers = regia_terrain::TerrainLayers::from_ron(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            !layers.layers.is_empty(),
            "{name}: the migrated stack lost its layers"
        );
        assert_eq!(
            std::fs::read(project.join(format!("{name}.bak"))).expect("a backup"),
            before[name],
            "{name}: the backup is not the original"
        );
    }
    let check = plan(&project, false);
    for f in check["files"].as_array().expect("files") {
        assert_eq!(f["status"], "current", "after --write: {f}");
    }
    let after = snapshot(&project);
    let again = plan(&project, true);
    assert_eq!(again["ok"], true);
    assert_eq!(snapshot(&project), after, "a second --write changed bytes");
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-7819 / ADR-8580: the corpus's v1 `.meta` sidecar (no `lossless_clips` key) upgrades to v2 with only its
/// version number rewritten, every other byte kept; no sibling source file is needed.
#[test]
fn the_v1_meta_fixture_upgrades_to_v2_by_bytes() {
    let project = unique_temp_dir("format_corpus_meta_v1");
    let v1 = std::fs::read(corpus_dir().join("meta/v1.ron")).expect("read the v1 fixture");
    let meta = project.join("DamagedHelmet.glb.meta");
    std::fs::write(&meta, &v1).expect("place the fixture");
    let dry = plan(&project, false);
    let entry = dry["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| {
            f["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("DamagedHelmet.glb.meta"))
        })
        .cloned()
        .expect("upgrade-project lists the .meta sidecar");
    assert_eq!(entry["format"], "meta", "{entry}");
    assert_eq!(entry["status"], "migrate", "{entry}");
    assert_eq!(entry["found"], 1, "{entry}");
    assert_eq!(entry["current"], 2, "{entry}");
    assert_eq!(plan(&project, true)["ok"], true);
    let expected = String::from_utf8(v1.clone()).expect("UTF-8 fixture").replacen(
        "format_version: 1,",
        "format_version: 2,",
        1,
    );
    assert_eq!(
        String::from_utf8(std::fs::read(&meta).expect("read back")).expect("UTF-8"),
        expected,
        "only the version moves"
    );
    assert_eq!(
        std::fs::read(project.join("DamagedHelmet.glb.meta.bak")).expect("a backup"),
        v1
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-7980: report outputs migrate through the report reader, outside project rewriting.
#[test]
fn historical_test_reports_migrate_through_the_report_loader() {
    use regia_framework::functional::report::TestReport;

    let current = current_versions()["test-report"];
    let indexes = load_all_indexes();
    let old: Vec<_> = indexes["test-report"]
        .iter()
        .filter(|entry| entry.version < current)
        .collect();
    assert!(!old.is_empty(), "the corpus must retain historical reports");
    for entry in old {
        let path = corpus_dir().join("test-report").join(&entry.file);
        let text = std::fs::read_to_string(path).expect("historical report");
        let report = TestReport::from_json(&text).expect("report migrator");
        assert_eq!(report.format_version, current);
        let before: Value = serde_json::from_str(&text).expect("original JSON");
        let after = serde_json::to_value(&report).expect("migrated JSON");
        assert_eq!(after["build"], before["build"]);
        assert_eq!(after["seed"], before["seed"]);
        let before_tests = before["tests"].as_array().expect("original test records");
        let after_tests = after["tests"].as_array().expect("migrated test records");
        assert_eq!(after_tests.len(), before_tests.len());
        for (before_test, after_test) in before_tests.iter().zip(after_tests) {
            for (key, value) in before_test.as_object().expect("test record") {
                assert_eq!(&after_test[key], value, "report field {key} must survive");
            }
        }
        if entry.version == 1 {
            assert!(report.tests.iter().all(|test| test.notes.is_empty()));
        }
    }
}

/// The version each format had in a release's frozen table (`docs/formats/format-hashes-<release>.ron`, the
/// one-step baselines of `cargo xtask check-formats`, ADR-7724).
fn release_versions(release: &str) -> BTreeMap<String, u32> {
    #[derive(serde::Deserialize)]
    struct Hashes {
        formats: BTreeMap<String, Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        version: u32,
    }
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/formats/format-hashes-{release}.ron"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let hashes: Hashes = ron::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    hashes.formats.into_iter().map(|(k, v)| (k, v.version)).collect()
}

/// The corpus versions that cannot have a fixture (`docs/formats/format-corpus-exceptions.ron`): (format, version),
/// version 0 meaning every version.
fn corpus_exceptions() -> Vec<(String, u32)> {
    #[derive(serde::Deserialize)]
    struct Exceptions {
        exceptions: Vec<Exception>,
    }
    #[derive(serde::Deserialize)]
    struct Exception {
        format: String,
        version: u32,
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats/format-corpus-exceptions.ron");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let parsed: Exceptions = ron::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    parsed
        .exceptions
        .into_iter()
        .map(|x| (x.format, x.version))
        .collect()
}

/// Format batch 2's exit bar (ADR-10183, plan v0.7 exit bar 6): a project as `release` wrote it upgrades to the
/// current versions. For every format whose version moved since that release, the corpus fixtures of the version
/// the release wrote (a missing one is a failure unless the corpus exceptions excuse it) go into one project; the
/// dry run plans each as `migrate` from exactly that version (formats upgrade-project never reads are `NOT_HANDLED`),
/// `--write` brings every one to its current version, and a second run changes no byte. Returns the problems.
fn upgrade_from_release(release: &str) -> Vec<String> {
    let shipped = release_versions(release);
    let current = current_versions();
    let indexes = load_all_indexes();
    let exceptions = corpus_exceptions();
    let project = unique_temp_dir(&format!("format_corpus_upgrade_from_{release}"));
    let mut problems = Vec::new();
    let mut placed = BTreeMap::new();
    for (format, then) in &shipped {
        let Some(&now) = current.get(format) else {
            problems.push(format!(
                "{format}: v{then} in {release} but gone from format-hashes.ron"
            ));
            continue;
        };
        if *then >= now {
            continue;
        }
        let entries: Vec<_> = indexes
            .get(format)
            .map(|es| es.iter().filter(|e| e.version == *then).collect())
            .unwrap_or_default();
        if entries.is_empty() {
            let excused = exceptions
                .iter()
                .any(|(f, v)| f == format && (*v == *then || *v == 0));
            if !excused {
                problems.push(format!(
                    "{format}: {release} wrote v{then} (now v{now}) but the corpus has no v{then} fixture"
                ));
            }
            continue;
        }
        for entry in entries {
            placed.insert(place(&project, format, entry), (format.clone(), *then, now));
        }
    }
    if placed.is_empty() {
        problems.push(format!(
            "{release}: no format moved since the release, nothing to upgrade"
        ));
        return problems;
    }
    let before = snapshot(&project);
    let dry = plan(&project, false);
    if snapshot(&project) != before {
        problems.push(format!("{release}: the dry run wrote something"));
    }
    let files = dry["files"].as_array().cloned().unwrap_or_default();
    let listed = |files: &[Value], name: &str| {
        files
            .iter()
            .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with(name)))
            .cloned()
    };
    let mut would_migrate = Vec::new();
    for (name, (format, then, now)) in &placed {
        match listed(&files, name) {
            None if NOT_HANDLED.iter().any(|(f, _)| *f == format.as_str()) => {}
            None => problems.push(format!(
                "{name} ({format} v{then} -> v{now}): upgrade-project does not recognize a {release} file"
            )),
            Some(f) if f["status"] != "migrate" || f["found"] != *then => problems.push(format!(
                "{name} ({format} v{then} -> v{now}): the dry run says {f}"
            )),
            Some(_) => would_migrate.push(name.clone()),
        }
    }
    let written = plan(&project, true);
    if written["ok"] != true {
        problems.push(format!("{release}: --write did not succeed: {written}"));
    }
    let after = snapshot(&project);
    let mut changed: Vec<String> = after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .collect();
    changed.sort();
    would_migrate.sort();
    if changed != would_migrate {
        problems.push(format!(
            "{release}: --write changed {changed:?}, the dry run planned {would_migrate:?}"
        ));
    }
    let check = plan(&project, false);
    for f in check["files"].as_array().cloned().unwrap_or_default() {
        if f["status"] != "current" {
            problems.push(format!("{release}: after --write: {f}"));
        }
    }
    let again = plan(&project, true);
    if again["ok"] != true || snapshot(&project) != after {
        problems.push(format!("{release}: a second --write changed bytes"));
    }
    let _ = std::fs::remove_dir_all(&project);
    problems
}

/// A v0.5.0 project upgrades to v0.7 (format batch 2's exit bar, ADR-10183).
#[test]
fn a_v050_project_upgrades_to_the_current_formats() {
    let problems = upgrade_from_release("v0.5.0");
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// A v0.6.0 project upgrades to v0.7 (format batch 2's exit bar, ADR-10183).
#[test]
fn a_v060_project_upgrades_to_the_current_formats() {
    let problems = upgrade_from_release("v0.6.0");
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The `gait` v1 freeze (format batch 2, ADR-10266): every gait corpus file, placed under the `.gait.ron` name the
/// command reads, is listed as `gait` v1 and current, a v2 file is listed as newer and skipped, and `--write` run
/// twice changes no byte and writes no backup.
#[test]
fn upgrade_project_leaves_gait_v1_files_and_skips_a_newer_one() {
    let project = unique_temp_dir("format_corpus_gait");
    let mut placed = Vec::new();
    for (format, entries) in load_all_indexes() {
        if format != "gait" {
            continue;
        }
        for entry in &entries {
            let stem = entry.file.trim_end_matches(".ron");
            let name = format!("units/{stem}.gait.ron");
            let to = project.join(&name);
            std::fs::create_dir_all(to.parent().expect("a parent")).expect("create the folder");
            std::fs::copy(corpus_dir().join("gait").join(&entry.file), &to).expect("copy the fixture");
            placed.push((name, entry.version));
        }
    }
    assert!(placed.len() >= 4, "the gait corpus lost files: {placed:?}");
    let v1 = std::fs::read_to_string(corpus_dir().join("gait/v1.ron")).expect("read gait/v1.ron");
    let future = v1.replacen("format_version: 1,", "format_version: 2,", 1);
    assert_ne!(future, v1);
    std::fs::write(project.join("units/future.gait.ron"), future).expect("write the v2 file");

    let before = snapshot(&project);
    let dry = plan(&project, false);
    let files = dry["files"].as_array().expect("files");
    let entry = |name: &str| {
        files
            .iter()
            .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with(name)))
            .unwrap_or_else(|| panic!("upgrade-project does not list {name}: {dry}"))
    };
    for (name, version) in &placed {
        let f = entry(name);
        assert_eq!(f["format"], "gait", "{f}");
        assert_eq!(f["found"], *version, "{f}");
        assert_eq!(f["status"], "current", "{f}");
    }
    let f = entry("units/future.gait.ron");
    assert_eq!(f["format"], "gait", "{f}");
    assert_eq!(f["found"], 2, "{f}");
    assert_eq!(f["status"], "newer", "{f}");
    assert_eq!(snapshot(&project), before, "the dry run wrote something");

    for run in 1..=2 {
        let _ = plan(&project, true);
        assert_eq!(
            snapshot(&project),
            before,
            "--write run {run} changed a gait file"
        );
    }
    let backups: Vec<String> = std::fs::read_dir(project.join("units"))
        .expect("read units")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".bak"))
        .collect();
    assert!(
        backups.is_empty(),
        "upgrade-project backed up gait files it did not change: {backups:?}"
    );
    let _ = std::fs::remove_dir_all(&project);
}
