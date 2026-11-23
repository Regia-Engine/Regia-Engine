// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 2: deterministic tests for public `regia_asset_pipeline` functions no test
//! called before (verdict builders, validation context lookups, registry selection and report
//! exit rules, source-kind detection, terrain input discovery and hashing, LOD defaults).

use std::path::{Path, PathBuf};

use regia_asset_pipeline::asset_graph::{AssetEntry, AssetKind, AssetSet};
use regia_asset_pipeline::data_cook::is_data_table;
use regia_asset_pipeline::texture_stats::TextureFamily;
use regia_asset_pipeline::tiled_import::{is_tiled_map, is_tiled_tileset};
use regia_asset_pipeline::validators::{
    Finding, Outcome, RunSettings, Severity, ValidationContext, Validator, ValidatorError, ValidatorRegistry,
    Verdict,
};
use regia_asset_pipeline::{
    default_lod_screen_size, discover_terrains, terrain_input_files, terrain_is_cookable,
    terrain_settings_hash,
};
use regia_test_utils::unique_temp_dir;

// ---------------------------------------------------------------------------------------------
// Verdicts and findings
// ---------------------------------------------------------------------------------------------

/// `with_hint` attaches a hint and keeps the rest of the finding.
#[test]
fn finding_with_hint_keeps_code_severity_and_text() {
    let f = Finding::new("tex_too_big", Severity::Warning, "4096 px").with_hint("cap at 2048");
    assert_eq!(f.code, "tex_too_big");
    assert_eq!(f.severity, Severity::Warning);
    assert_eq!(f.text, "4096 px");
    assert_eq!(f.hint.as_deref(), Some("cap at 2048"));
    assert_eq!(Finding::error("x", "y").hint, None);
}

/// `fail_with` and `pass_with` copy each finding's text into the messages; `from_findings` fails
/// exactly when an `Error` is present; `from_problems` passes on an empty list.
#[test]
fn verdict_builders_follow_the_contract() {
    let warn = Finding::new("w", Severity::Warning, "warned");
    let info = Finding::new("i", Severity::Info, "noted");
    let err = Finding::error("e", "broken");

    let v = Verdict::pass_with(vec![warn.clone(), info.clone()]);
    assert_eq!(v.outcome, Outcome::Pass);
    assert_eq!(v.messages, vec!["warned", "noted"]);
    let v = Verdict::fail_with(vec![err.clone()]);
    assert_eq!(v.outcome, Outcome::Fail);
    assert_eq!(v.messages, vec!["broken"]);

    assert_eq!(Verdict::from_findings(Vec::new()), Verdict::pass_with(Vec::new()));
    assert_eq!(
        Verdict::from_findings(vec![warn.clone(), info.clone()]).outcome,
        Outcome::Pass
    );
    let mixed = Verdict::from_findings(vec![warn, err, info]);
    assert_eq!(mixed.outcome, Outcome::Fail);
    assert_eq!(mixed.findings.len(), 3, "every finding kept");

    assert_eq!(Verdict::from_problems(Vec::new()), Verdict::pass());
    let v = Verdict::from_problems(vec!["a".into(), "b".into()]);
    assert_eq!(v.outcome, Outcome::Fail);
    assert_eq!(v.findings.len(), 2);
    assert!(
        v.findings
            .iter()
            .all(|f| f.severity == Severity::Error && f.code.is_empty())
    );
}

// ---------------------------------------------------------------------------------------------
// Validation context and registry
// ---------------------------------------------------------------------------------------------

fn sample_set() -> AssetSet {
    let mut set = AssetSet::new();
    set.roots = vec!["scenes/main.scene".into(), "scenes/typo.scene".into()];
    set.push(AssetEntry::new("scenes/main.scene", AssetKind::Scene).with_references(["meshes/rock.mesh"]));
    set.push(
        AssetEntry::new("meshes/rock.mesh", AssetKind::Mesh)
            .with_id("id-rock")
            .with_references(["textures/rock.png"])
            .with_triangles(100),
    );
    set.push(
        AssetEntry::new("textures/rock.png", AssetKind::Texture)
            .with_id("id-shared")
            .with_cooked_bytes(500),
    );
    set.push(
        AssetEntry::new("textures/orphan.png", AssetKind::Texture)
            .with_id("id-shared")
            .with_cooked_bytes(50),
    );
    set.push(AssetEntry::new("textures/orphan.png", AssetKind::Texture));
    set
}

/// The context indexes ids (in set order), duplicate paths, unknown roots and reachability (roots
/// included, references followed, orphans not).
#[test]
fn validation_context_lookups() {
    let set = sample_set();
    let ctx = ValidationContext::new(&set);
    assert_eq!(
        ctx.owners_of_id("id-shared"),
        ["textures/rock.png", "textures/orphan.png"]
    );
    assert_eq!(ctx.owners_of_id("id-rock"), ["meshes/rock.mesh"]);
    assert!(ctx.owners_of_id("id-missing").is_empty());
    assert_eq!(ctx.path_count("textures/orphan.png"), 2);
    assert_eq!(ctx.path_count("meshes/rock.mesh"), 1);
    assert_eq!(ctx.path_count("nothing"), 0);
    assert_eq!(ctx.unknown_roots(), ["scenes/typo.scene"]);
    assert!(ctx.is_reachable("scenes/main.scene"));
    assert!(ctx.is_reachable("textures/rock.png"));
    assert!(!ctx.is_reachable("textures/orphan.png"));
    assert!(!ctx.is_reachable("scenes/typo.scene"));
}

/// Fails meshes without a triangle count; warns on cooked assets over 100 bytes.
struct Probe {
    name: &'static str,
}

impl Validator for Probe {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "test probe"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        match self.name {
            "mesh_tris" if asset.kind == AssetKind::Mesh && asset.triangles.is_none() => {
                Verdict::fail(vec!["no triangle count".into()])
            }
            "mesh_tris" if asset.kind != AssetKind::Mesh => Verdict::not_validated("not a mesh"),
            "size_warn" if asset.cooked_bytes > 100 => Verdict::from_findings(vec![Finding::new(
                "",
                Severity::Warning,
                format!("{} bytes", asset.cooked_bytes),
            )]),
            _ => Verdict::pass(),
        }
    }
}

fn registry() -> ValidatorRegistry {
    let mut r = ValidatorRegistry::new();
    r.register(Box::new(Probe { name: "size_warn" }))
        .expect("register");
    r.register(Box::new(Probe { name: "mesh_tris" }))
        .expect("register");
    r
}

fn probe_set() -> AssetSet {
    let mut set = AssetSet::new();
    set.push(AssetEntry::new("a.mesh", AssetKind::Mesh).with_triangles(3));
    set.push(AssetEntry::new("b.mesh", AssetKind::Mesh));
    set.push(AssetEntry::new("c.png", AssetKind::Texture).with_cooked_bytes(400));
    set
}

/// `describe` lists validators in registration order; `run_checked` refuses an unknown name in
/// `only` (while `run_with` selects nothing for it) and runs only the named validators.
#[test]
fn registry_describe_and_run_checked() {
    let r = registry();
    assert_eq!(
        r.describe(),
        vec![
            ("size_warn".to_owned(), "test probe".to_owned()),
            ("mesh_tris".to_owned(), "test probe".to_owned())
        ]
    );
    let set = probe_set();
    let unknown = RunSettings {
        only: Some(vec!["mesh_tris".into(), "nope".into()]),
        ..RunSettings::default()
    };
    assert!(matches!(
        r.run_checked(&set, &unknown),
        Err(ValidatorError::UnknownValidator(name)) if name == "nope"
    ));
    let lenient = r.run_with(&set, &unknown);
    assert!(lenient.results.iter().all(|row| row.validator == "mesh_tris"));

    let only = RunSettings {
        only: Some(vec!["mesh_tris".into()]),
        ..RunSettings::default()
    };
    let report = r.run_checked(&set, &only).expect("known names");
    assert_eq!(report.results.len(), 3);
    assert!(report.results.iter().all(|row| row.validator == "mesh_tris"));
    assert_eq!(report.summary.failed, 1);
    assert_eq!(report.summary.not_validated, 1);
}

/// `all_findings` walks rows in (asset, validator) order with the runner-filled codes; a warning
/// fails a CI run only under `strict`, a failure always does.
#[test]
fn report_all_findings_and_exit_rules() {
    let r = registry();
    let set = probe_set();
    let report = r.run(&set);
    let found: Vec<(String, String, String)> = report
        .all_findings()
        .map(|(row, f)| (row.asset.clone(), row.validator.clone(), f.code.clone()))
        .collect();
    assert_eq!(
        found,
        vec![
            ("b.mesh".into(), "mesh_tris".into(), "mesh_tris".into()),
            ("c.png".into(), "size_warn".into(), "size_warn".into()),
        ]
    );
    assert!(report.exit_failed(false));
    assert!(report.exit_failed(true));

    let mut clean = AssetSet::new();
    clean.push(AssetEntry::new("c.png", AssetKind::Texture).with_cooked_bytes(400));
    let report = r.run(&clean);
    assert_eq!(report.summary.failed, 0);
    assert_eq!(report.summary.warnings, 1);
    assert!(!report.exit_failed(false), "warnings pass a lenient run");
    assert!(report.exit_failed(true), "warnings fail a strict run");

    let empty = r.run(&AssetSet::new());
    assert!(!empty.exit_failed(true));
    assert_eq!(empty.all_findings().count(), 0);
}

// ---------------------------------------------------------------------------------------------
// Source detection
// ---------------------------------------------------------------------------------------------

/// Tiled maps are `tmx`/`tmj`, tilesets `tsx`/`tsj`, in any case; the two sets never overlap.
#[test]
fn tiled_extensions_are_case_insensitive_and_disjoint() {
    for map in ["a.tmx", "dir/b.TMJ", "c.Tmx"] {
        assert!(is_tiled_map(Path::new(map)), "{map}");
        assert!(!is_tiled_tileset(Path::new(map)), "{map}");
    }
    for tileset in ["a.tsx", "dir/b.TSJ"] {
        assert!(is_tiled_tileset(Path::new(tileset)), "{tileset}");
        assert!(!is_tiled_map(Path::new(tileset)), "{tileset}");
    }
    for neither in ["a.json", "tmx", "a.tmx.bak", "a", "", "a.tm"] {
        assert!(!is_tiled_map(Path::new(neither)), "{neither}");
        assert!(!is_tiled_tileset(Path::new(neither)), "{neither}");
    }
}

/// Data tables are matched on the file name's double suffix in any case, never on a directory.
#[test]
fn data_table_detection_uses_the_file_name() {
    for yes in [
        "loot.curves.ron",
        "a/b/LOOT.CURVES.CSV",
        "boss.chooser.ron",
        "x/door.smartobject.ron",
    ] {
        assert!(is_data_table(Path::new(yes)), "{yes}");
    }
    for no in [
        "loot.ron",
        "curves.ron.bak",
        "loot.curves.ron/inner.txt",
        "a.chooser",
        "",
    ] {
        assert!(!is_data_table(Path::new(no)), "{no}");
    }
}

/// Android expects ASTC, desktop targets BC, anything else has no expectation.
#[test]
fn texture_family_expected_per_platform() {
    assert_eq!(TextureFamily::expected_for("android"), Some(TextureFamily::Astc));
    for desktop in ["linux", "windows", "macos"] {
        assert_eq!(TextureFamily::expected_for(desktop), Some(TextureFamily::Bc));
    }
    for other in ["", "Android", "web", "ios"] {
        assert_eq!(TextureFamily::expected_for(other), None, "{other}");
    }
}

// ---------------------------------------------------------------------------------------------
// Terrain sources
// ---------------------------------------------------------------------------------------------

const LAYERS: &str = r#"TerrainLayers(
    version: 1,
    wetness_default: 0.0,
    layers: [
        LayerDesc(name: "grass", albedo: (0.2, 0.3, 0.1), roughness: 0.9, tiling: 8.0, slope_min: 0.0,
            slope_max: 0.6, altitude_min: -1000.0, altitude_max: 1000.0, blend_sharpness: 0.15,
            triplanar: false),
        LayerDesc(name: "rock", albedo: (0.4, 0.4, 0.4), roughness: 0.8, tiling: 4.0, slope_min: 0.0,
            slope_max: 1.0, altitude_min: -1000.0, altitude_max: 1000.0, blend_sharpness: 0.2,
            triplanar: true),
    ],
)"#;

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// A terrain directory `root/map/terrain/` with the layer file and nothing else.
fn terrain_dir() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir("tqs2_terrain");
    let layer_file = root.join("map").join("terrain").join("ground.terrain.layers.ron");
    write(&layer_file, LAYERS.as_bytes());
    (root, layer_file)
}

/// Discovery keeps only `*terrain.layers.ron` files (any case), sorted by path.
#[test]
fn discover_terrains_filters_and_sorts() {
    let sources: Vec<PathBuf> = [
        "z/b.terrain.layers.ron",
        "a/ground.TERRAIN.LAYERS.RON",
        "a/ground.terrain.ron",
        "a/terrain.layers.ron.bak",
        "a/heightfield.png",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let found: Vec<PathBuf> = discover_terrains(&sources)
        .into_iter()
        .map(|t| t.source)
        .collect();
    assert_eq!(
        found,
        vec![
            PathBuf::from("a/ground.TERRAIN.LAYERS.RON"),
            PathBuf::from("z/b.terrain.layers.ron")
        ]
    );
    assert!(discover_terrains(&[]).is_empty());
}

/// The candidate inputs are both heightfield locations with their extents and raster descriptors
/// (ADR-8542), the source tile-set descriptor beside the layer file (ADR-8595), the macro slots
/// and four textures per named layer; a broken layer file is an error.
#[test]
fn terrain_input_files_lists_every_candidate() {
    let (root, layer_file) = terrain_dir();
    let dir = layer_file.parent().expect("dir").to_path_buf();
    let files = terrain_input_files(&layer_file).expect("inputs");
    // Heightfield and extents at both locations (4), a raster descriptor at both (2, ADR-8542),
    // the tile-set descriptor (1, ADR-8595; no tile files without it), 5 macro slots, 4 per layer.
    assert_eq!(files.len(), 4 + 2 + 1 + 5 + 2 * 4, "{files:#?}");
    for expected in [
        dir.join("heightfield.png"),
        dir.join("heightfield.json"),
        dir.join("heightfield.raster.json"),
        dir.join("heightfield.tiles.json"),
        root.join("map").join("visual").join("heightfield.png"),
        root.join("map").join("visual").join("heightfield.raster.json"),
        dir.join("macro_albedo.png"),
        dir.join("macro").join("normal.png"),
        dir.join("detail").join("grass").join("albedo.png"),
        dir.join("detail").join("rock").join("height.png"),
    ] {
        assert!(files.contains(&expected), "{}", expected.display());
    }
    assert!(
        !files.contains(&layer_file),
        "the layer file itself is not an input"
    );

    write(&layer_file, b"TerrainLayers(version: 1, layers: [");
    assert!(terrain_input_files(&layer_file).is_err());
    assert!(terrain_is_cookable(&layer_file).is_err());
    assert!(terrain_input_files(&root.join("missing.terrain.layers.ron")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// A layer file alone is not cookable; one layer texture or a heightfield (either location)
/// makes it cookable.
#[test]
fn terrain_is_cookable_needs_a_heightfield_or_a_layer_texture() {
    let (root, layer_file) = terrain_dir();
    let dir = layer_file.parent().expect("dir").to_path_buf();
    assert!(!terrain_is_cookable(&layer_file).expect("readable"));
    // Extents and macro maps alone do not count.
    write(&dir.join("heightfield.json"), b"{}");
    write(&dir.join("macro_albedo.png"), b"x");
    assert!(!terrain_is_cookable(&layer_file).expect("readable"));
    // A texture of an undeclared layer does not count either.
    write(&dir.join("detail").join("sand").join("albedo.png"), b"x");
    assert!(!terrain_is_cookable(&layer_file).expect("readable"));
    write(&dir.join("detail").join("rock").join("orm.png"), b"x");
    assert!(terrain_is_cookable(&layer_file).expect("readable"));
    std::fs::remove_file(dir.join("detail").join("rock").join("orm.png")).expect("rm");
    write(&root.join("map").join("visual").join("heightfield.png"), b"x");
    assert!(terrain_is_cookable(&layer_file).expect("readable"));
    let _ = std::fs::remove_dir_all(&root);
}

/// The settings hash is stable, ignores absent candidates and files that are not inputs, and
/// changes with any input's content or with an input appearing.
#[test]
fn terrain_settings_hash_tracks_inputs_only() {
    let (root, layer_file) = terrain_dir();
    let dir = layer_file.parent().expect("dir").to_path_buf();
    let h0 = terrain_settings_hash(&layer_file).expect("hash");
    assert_eq!(terrain_settings_hash(&layer_file).expect("hash"), h0, "stable");
    write(&dir.join("notes.txt"), b"not an input");
    write(
        &dir.join("detail").join("sand").join("albedo.png"),
        b"undeclared layer",
    );
    assert_eq!(terrain_settings_hash(&layer_file).expect("hash"), h0);

    write(&dir.join("heightfield.png"), b"one");
    let h1 = terrain_settings_hash(&layer_file).expect("hash");
    assert_ne!(h1, h0);
    write(&dir.join("heightfield.png"), b"two");
    let h2 = terrain_settings_hash(&layer_file).expect("hash");
    assert_ne!(h2, h1);
    write(&dir.join("heightfield.png"), b"one");
    assert_eq!(
        terrain_settings_hash(&layer_file).expect("hash"),
        h1,
        "content, not mtime"
    );
    write(&dir.join("detail").join("grass").join("normal.png"), b"n");
    assert_ne!(terrain_settings_hash(&layer_file).expect("hash"), h1);
    let _ = std::fs::remove_dir_all(&root);
}

/// The layer file with one layer named `name` (otherwise the first fixture layer).
fn layers_named(name: &str) -> String {
    LAYERS.replacen("\"grass\"", &format!("{name:?}"), 1)
}

/// Regression (sweep 2): a layer name that is not one folder (`..`, a separator) is refused by
/// `terrain_is_cookable` and by the cook's emptiness test, as `terrain_input_files` and the cook
/// already refused it. Before, the probe joined the raw name (looking outside `detail/`): a file
/// there made the terrain "cookable", and without one the cook returned an empty terrain instead
/// of the error.
#[test]
fn bad_layer_names_are_refused_before_the_emptiness_probe() {
    for bad in ["../outside", "a/b", "a\\b", ".."] {
        let (root, layer_file) = terrain_dir();
        let dir = layer_file.parent().expect("dir").to_path_buf();
        write(&layer_file, layers_named(bad).as_bytes());
        assert!(terrain_input_files(&layer_file).is_err(), "{bad}");
        assert!(terrain_is_cookable(&layer_file).is_err(), "{bad}: no file");
        // The texture the raw name would reach (`terrain/outside/albedo.png` for `../outside`).
        write(&dir.join("outside").join("albedo.png"), b"x");
        assert!(
            terrain_is_cookable(&layer_file).is_err(),
            "{bad}: with a file there"
        );
        std::fs::remove_dir_all(dir.join("outside")).expect("rm");
        let cache = root.join("cache");
        let db = regia_asset_pipeline::AssetDatabase::default();
        let cooked = regia_asset_pipeline::cook_terrain(
            &layer_file,
            "assets/ground.terrain.layers.ron",
            &cache,
            "linux",
            &db,
            false,
        );
        assert!(cooked.is_err(), "{bad}: the cook refuses the name");
        let _ = std::fs::remove_dir_all(&root);
    }
    // A good name with nothing to cook is still an empty terrain, not an error.
    let (root, layer_file) = terrain_dir();
    let db = regia_asset_pipeline::AssetDatabase::default();
    let cooked = regia_asset_pipeline::cook_terrain(
        &layer_file,
        "assets/ground.terrain.layers.ron",
        &root.join("cache"),
        "linux",
        &db,
        false,
    )
    .expect("empty terrain cooks to nothing");
    assert!(cooked.empty);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// LOD defaults
// ---------------------------------------------------------------------------------------------

/// Every representable halving is strictly smaller, including LOD3 (ADR-7869).
/// Beyond the last positive f32 power of two, the threshold is zero, never a repeated floor.
#[test]
fn default_lod_screen_sizes_never_increase() {
    assert_eq!(default_lod_screen_size(0), 1.0);
    assert_eq!(default_lod_screen_size(1), 0.5);
    assert_eq!(default_lod_screen_size(2), 0.25);
    assert_eq!(default_lod_screen_size(3), 0.125);
    assert_eq!(default_lod_screen_size(4), 0.0625);
    assert_eq!(default_lod_screen_size(19), 1.0 / 524_288.0);
    assert_eq!(default_lod_screen_size(149), f32::from_bits(1));
    assert_eq!(default_lod_screen_size(150), 0.0);
    assert_eq!(default_lod_screen_size(u32::MAX), 0.0);
    let mut prev = f32::INFINITY;
    for level in 0..=150 {
        let size = default_lod_screen_size(level);
        assert!(
            size.is_finite() && (0.0..=1.0).contains(&size),
            "LOD{level}: {size}"
        );
        assert!(size < prev, "LOD{level}: {size} is not below {prev}");
        prev = size;
    }
}

/// No intermediate level of a five-to-sixteen-level default chain is skipped by the runtime
/// selector. In particular LOD3 needs a nonempty interval below LOD2 (ADR-7869).
#[test]
fn every_default_lod_has_a_selection_interval() {
    for count in 5..=regia_asset_pipeline::lod_classes::MAX_LOD_LEVELS {
        let entries: Vec<_> = (0..count)
            .map(|level| {
                let name = format!("Hull_LOD{level}");
                let id = regia_asset::AssetId::sub_asset(regia_asset::AssetId::NIL, &name);
                (name, id)
            })
            .collect();
        let groups = regia_asset_pipeline::build_lod_groups(&entries);
        let sizes: Vec<_> = groups[0]
            .levels
            .iter()
            .map(|level| level.screen_relative_size)
            .collect();
        assert_eq!(sizes.len(), count);
        for (index, pair) in sizes.windows(2).enumerate() {
            let level = index + 1;
            let midpoint = (pair[0] + pair[1]) * 0.5;
            assert_eq!(
                regia_asset::lod_at(&sizes, midpoint),
                level,
                "{count} levels: LOD{level} has no interval"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Content chunks (second batch)
// ---------------------------------------------------------------------------------------------

mod chunks {
    use std::collections::BTreeSet;

    use regia_asset_pipeline::chunks::{ChunkRule, ChunkRules};

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    /// A chunk may reference itself, `base` and its requirements transitively; an unknown chunk
    /// only itself and `base`; a hand-built cycle terminates.
    #[test]
    fn allowed_targets_follow_requirements_transitively() {
        let rules = ChunkRules::from_manifest_toml(
            "[cook.chunks.a]\nrequires = [\"b\"]\n[cook.chunks.b]\nrequires = [\"c\"]\n[cook.chunks.c]\ninclude = []\n",
        )
        .expect("rules");
        assert_eq!(rules.allowed_targets("a"), set(&["a", "b", "c", "base"]));
        assert_eq!(rules.allowed_targets("b"), set(&["b", "c", "base"]));
        assert_eq!(rules.allowed_targets("c"), set(&["c", "base"]));
        assert_eq!(rules.allowed_targets("zz"), set(&["zz", "base"]));
        assert_eq!(rules.allowed_targets("base"), set(&["base"]));

        let mut cyclic = ChunkRules::default();
        let mut ra = ChunkRule::new("a");
        ra.requires = vec!["b".into()];
        let mut rb = ChunkRule::new("b");
        rb.requires = vec!["a".into()];
        cyclic.rules.insert("a".into(), ra);
        cyclic.rules.insert("b".into(), rb);
        assert_eq!(cyclic.allowed_targets("a"), set(&["a", "b", "base"]));
    }

    /// `from_table` reads the same rules as the TOML text entry point, defaults with no
    /// `[cook]` section, and refuses a `chunks` key that is not a table.
    #[test]
    fn from_table_matches_the_text_entry_point() {
        let text = "[cook.chunks.forest]\ninclude = [\"levels/forest/**\"]\npriority = 5\n";
        let table: toml::Table = text.parse().expect("toml");
        let a = ChunkRules::from_table(&table).expect("table");
        let b = ChunkRules::from_manifest_toml(text).expect("text");
        assert_eq!(a.allowed_targets("forest"), b.allowed_targets("forest"));
        assert_eq!(a, b);

        let empty = ChunkRules::from_table(&toml::Table::new()).expect("empty");
        let defaults = ChunkRules::from_manifest_toml("").expect("empty text");
        assert_eq!(empty.android_pack_limit, defaults.android_pack_limit);
        assert!(empty.android_pack_limit > 0);

        let bad: toml::Table = "[cook]\nchunks = 3\n".parse().expect("toml");
        assert_eq!(
            ChunkRules::from_table(&bad).map_err(|e| e.reason()).err(),
            Some("bad_params")
        );
    }
}
