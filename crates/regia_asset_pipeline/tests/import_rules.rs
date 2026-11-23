// SPDX-License-Identifier: MPL-2.0
//! Import folder rules end to end (ADR-3321, `docs/specs/12-assets.md` § 10): a
//! `config/import_rules.ron` rule cooks matching textures uncompressed, an explicit `.meta` field
//! beats a rule, editing a rule recooks exactly the matching sources through the incremental
//! cook, and a malformed rules file fails the cook before anything cooks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use image::ImageEncoder;
use regia_asset::{AssetManifest, MetaSidecar, cooked_cache_dir, cooked_file_name, meta_path_for};
use regia_asset_pipeline::import_rules::IMPORT_RULES_FILE;
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};

/// A 64×64 RGBA PNG (large enough to block-compress) whose pixels depend on `seed`.
fn png(seed: u8) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&[((x * 4) as u8) ^ seed, (y * 4) as u8, seed, 255]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

/// A textured quad: `props/crate.gltf` with its buffer `crate.bin` and image `crate_albedo.png`
/// beside it, a standalone UI icon and a world texture.
fn write_project(root: &Path) {
    let assets = root.join("assets");
    std::fs::create_dir_all(assets.join("ui")).expect("ui dir");
    std::fs::create_dir_all(assets.join("world")).expect("world dir");
    std::fs::create_dir_all(assets.join("props")).expect("props dir");
    let positions: [[f32; 3]; 4] = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ];
    let uvs: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let indices: [u16; 6] = [0, 2, 1, 0, 3, 2];
    let mut bin = Vec::new();
    for p in positions {
        p.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for _ in 0..4 {
        [0.0f32, 1.0, 0.0]
            .iter()
            .for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    for uv in uvs {
        uv.iter().for_each(|v| bin.extend_from_slice(&v.to_le_bytes()));
    }
    indices
        .iter()
        .for_each(|i| bin.extend_from_slice(&i.to_le_bytes()));
    assert_eq!(bin.len(), 140);
    let json = r#"{
  "asset": {"version": "2.0"},
  "scene": 0,
  "scenes": [{"nodes": [0]}],
  "nodes": [{"mesh": 0, "name": "Crate"}],
  "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0, "mode": 4}]}],
  "materials": [{"name": "crate", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
  "textures": [{"source": 0}],
  "images": [{"uri": "crate_albedo.png"}],
  "buffers": [{"byteLength": 140, "uri": "crate.bin"}],
  "bufferViews": [
    {"buffer": 0, "byteOffset": 0, "byteLength": 48, "target": 34962},
    {"buffer": 0, "byteOffset": 48, "byteLength": 48, "target": 34962},
    {"buffer": 0, "byteOffset": 96, "byteLength": 32, "target": 34962},
    {"buffer": 0, "byteOffset": 128, "byteLength": 12, "target": 34963}
  ],
  "accessors": [
    {"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-1, 0, -1], "max": [1, 0, 1]},
    {"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3"},
    {"bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2"},
    {"bufferView": 3, "componentType": 5123, "count": 6, "type": "SCALAR"}
  ]
}"#;
    std::fs::write(assets.join("props/crate.gltf"), json).expect("gltf");
    std::fs::write(assets.join("props/crate.bin"), bin).expect("bin");
    std::fs::write(assets.join("props/crate_albedo.png"), png(1)).expect("albedo");
    std::fs::write(assets.join("ui/icon.png"), png(2)).expect("icon");
    std::fs::write(assets.join("world/ground.png"), png(3)).expect("ground");
}

fn write_rules(root: &Path, text: &str) {
    std::fs::create_dir_all(root.join("config")).expect("config dir");
    std::fs::write(root.join(IMPORT_RULES_FILE), text).expect("rules");
}

fn options(root: &Path) -> CookOptions {
    CookOptions::for_host(root.to_path_buf())
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(options(root)).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    report
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

/// Sets the modification time of every file directly under `assets/` an hour back (cook_skip's
/// pattern: aged files are stamped, not re-read, and a rewrite always moves the mtime).
fn age_assets(root: &Path) {
    let then = SystemTime::now() - Duration::from_secs(3600);
    for entry in std::fs::read_dir(root.join("assets")).expect("assets").flatten() {
        if !entry.path().is_file() {
            continue;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(entry.path())
            .expect("open");
        file.set_modified(then).expect("set mtime");
    }
}

/// Sets the modification time of every cooked file but the manifest `ago` back.
fn age_cooked(root: &Path, ago: Duration) {
    let then = SystemTime::now() - ago;
    for entry in std::fs::read_dir(cache(root)).expect("cache dir").flatten() {
        if entry.file_name() == "asset_manifest.bin" || !entry.path().is_file() {
            continue;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(entry.path())
            .expect("open");
        file.set_modified(then).expect("set mtime");
    }
}

/// Every cooked file but the manifest (rewritten by every cook): name → (bytes, modified).
fn cooked_files(root: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(cache(root)).expect("cache dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "asset_manifest.bin" {
            continue;
        }
        let meta = entry.metadata().expect("metadata");
        let modified = meta.modified().expect("mtime");
        out.insert(name, (std::fs::read(entry.path()).expect("read"), modified));
    }
    out
}

/// The cooked type path of one source's primary asset (`regia_asset::CookedImage` when a rule
/// kept the texture uncompressed, `regia_asset::CookedTexture` when it block-compressed).
fn primary_type(root: &Path, source_path: &str) -> String {
    let manifest = AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present");
    manifest
        .entries
        .iter()
        .find(|e| e.path == source_path)
        .unwrap_or_else(|| panic!("{source_path} in {entries:?}", entries = manifest.entries))
        .type_path
        .clone()
}

/// The per-asset cooked files (`.fckd`) whose bytes or mtime changed between two snapshots; the
/// manifest, registry and ledgers rewrite on every cook and are not per-asset outputs.
fn changed(
    before: &BTreeMap<String, (Vec<u8>, SystemTime)>,
    after: &BTreeMap<String, (Vec<u8>, SystemTime)>,
) -> Vec<String> {
    before
        .iter()
        .filter(|(name, _)| name.ends_with(".fckd"))
        .filter(|(name, (bytes, mtime))| after.get(*name).is_none_or(|(b, m)| b != bytes || m != mtime))
        .map(|(name, _)| name.clone())
        .collect()
}

#[test]
fn rules_cook_matching_textures_uncompressed_and_meta_beats_rules() {
    let root = regia_test_utils::unique_temp_dir("import-rules-cook");
    write_project(&root);
    age_assets(&root);
    write_rules(
        &root,
        r#"(rules: [
            (glob: "assets/ui/**", settings: "(compress: Some(false))"),
        ])"#,
    );

    cook(&root);
    assert_eq!(
        primary_type(&root, "assets/ui/icon.png"),
        "regia_asset::CookedImage",
        "the rule keeps the UI texture uncompressed"
    );
    assert_eq!(
        primary_type(&root, "assets/world/ground.png"),
        "regia_asset::CookedTexture",
        "no rule reaches the world texture"
    );

    // An explicit .meta field beats the rule: icon.meta asks for compression again.
    let icon = root.join("assets/ui/icon.png");
    let mut meta = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&icon)).expect("meta bytes"))
        .expect("meta parses");
    meta.settings = Some("(compress: Some(true))".to_string());
    std::fs::write(meta_path_for(&icon), meta.to_ron_string().expect("meta ron")).expect("meta write");
    age_assets(&root);
    cook(&root);
    assert_eq!(
        primary_type(&root, "assets/ui/icon.png"),
        "regia_asset::CookedTexture",
        "the explicit .meta field wins over the rule"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn editing_a_rule_recooks_exactly_the_matching_sources() {
    let root = regia_test_utils::unique_temp_dir("import-rules-recook");
    write_project(&root);
    age_assets(&root);
    write_rules(
        &root,
        r#"(rules: [
            (glob: "assets/ui/**", settings: "(compress: Some(false), max_size: Some(64))"),
            (glob: "assets/**", importer: "gltf", settings: "(drop_double_sided_on_closed: true)"),
        ])"#,
    );

    // Settle the stamps and mtimes: an unchanged recook must rewrite nothing.
    cook(&root);
    age_assets(&root);
    let restamp = cook(&root);
    assert!(
        restamp.imports_skipped >= 3,
        "the second cook skips every source: {restamp:?}"
    );
    age_cooked(&root, Duration::from_secs(1800));
    let settled = cooked_files(&root);
    let again = cook(&root);
    assert_eq!(
        again.imports_skipped, restamp.imports_skipped,
        "still all skipped"
    );
    assert!(
        changed(&settled, &cooked_files(&root)).is_empty(),
        "an unchanged recook is fully skipped"
    );

    // Edit the texture rule (a field that does not change this 64-px output): only the UI
    // texture re-cooks; the glTF, its albedo and the world texture stay skipped.
    write_rules(
        &root,
        r#"(rules: [
            (glob: "assets/ui/**", settings: "(compress: Some(false), max_size: Some(32))"),
            (glob: "assets/**", importer: "gltf", settings: "(drop_double_sided_on_closed: true)"),
        ])"#,
    );
    let before = cooked_files(&root);
    let report = cook(&root);
    let rewritten = changed(&before, &cooked_files(&root));
    let manifest = AssetManifest::read_from_cache(&cache(&root))
        .expect("manifest")
        .expect("present");
    let icon_id = manifest
        .entries
        .iter()
        .find(|e| e.path == "assets/ui/icon.png")
        .expect("icon entry")
        .id;
    assert_eq!(
        rewritten,
        [cooked_file_name(icon_id)],
        "only the UI texture recooked"
    );
    assert!(
        report.imports_skipped >= 2,
        "the incremental skip still covered the untouched sources: {report:?}"
    );

    // Editing the glTF rule recooks exactly the crate's outputs, including its shared image.
    write_rules(
        &root,
        r#"(rules: [
            (glob: "assets/ui/**", settings: "(compress: Some(false), max_size: Some(32))"),
            (glob: "assets/**", importer: "gltf", settings: "(drop_double_sided_on_closed: false)"),
        ])"#,
    );
    let before = cooked_files(&root);
    cook(&root);
    let rewritten = changed(&before, &cooked_files(&root));
    let crate_files: Vec<String> = manifest
        .entries
        .iter()
        .filter(|e| {
            e.path.starts_with("assets/props/crate.gltf") || e.path == "assets/props/crate_albedo.png"
        })
        .map(|e| cooked_file_name(e.id))
        .collect();
    assert!(!crate_files.is_empty());
    let mut rewritten_sorted = rewritten.clone();
    rewritten_sorted.sort();
    let mut crate_sorted = crate_files.clone();
    crate_sorted.sort();
    assert_eq!(
        rewritten_sorted, crate_sorted,
        "exactly the crate's cooked files changed: {rewritten:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_malformed_rules_file_fails_the_cook_before_any_source_cooks() {
    let root = regia_test_utils::unique_temp_dir("import-rules-bad");
    write_project(&root);
    age_assets(&root);
    write_rules(&root, "(rules: [(glob: \"\", settings: \"()\")])");
    let report = cook_project(options(&root)).expect("the cook reports errors, it does not panic");
    assert!(
        report
            .errors
            .iter()
            .any(|(path, err)| path == IMPORT_RULES_FILE && err.contains("rule 0")),
        "the cook names the rules file and the rule: {:?}",
        report.errors
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A rule matching an image a glTF references reaches that image's cooked texture (the glTF cook
/// read only the image's `.meta`, while `import-rules check` reported the rule as applied), and
/// editing the rule recooks the glTF (its key hashes the rules its images match).
#[test]
fn rules_reach_the_images_a_gltf_references() {
    let root = regia_test_utils::unique_temp_dir("import-rules-gltf-images");
    write_project(&root);
    age_assets(&root);
    let rules = |compress: bool| {
        format!(r#"(rules: [(glob: "assets/props/*.png", settings: "(compress: Some({compress}))")])"#)
    };
    write_rules(&root, &rules(false));
    cook(&root);
    assert_eq!(
        primary_type(&root, "assets/props/crate_albedo.png"),
        "regia_asset::CookedImage",
        "the rule keeps the glTF's albedo uncompressed"
    );
    write_rules(&root, &rules(true));
    cook(&root);
    assert_eq!(
        primary_type(&root, "assets/props/crate_albedo.png"),
        "regia_asset::CookedTexture",
        "editing the rule recooks the glTF"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The live cooker (`cook_sources`) fails each source on a malformed rules file too: it read the
/// bad file as "no rules" and cooked every source without its rules, reporting success.
#[test]
fn a_malformed_rules_file_fails_each_live_cooked_source() {
    let root = regia_test_utils::unique_temp_dir("import-rules-bad-live");
    write_project(&root);
    age_assets(&root);
    write_rules(&root, "(rules: [(glob: \"\", settings: \"()\")])");
    let icon = root.join("assets/ui/icon.png");
    let targets: std::collections::BTreeSet<PathBuf> = [icon].into_iter().collect();
    let cooked = regia_asset_pipeline::cook_sources(&options(&root), &targets).expect("the live cook runs");
    assert_eq!(cooked.len(), 1);
    let error = cooked[0].error.as_deref().unwrap_or_default();
    assert!(
        error.contains(IMPORT_RULES_FILE) && error.contains("rule 0"),
        "the source fails naming the rules file and the rule: {:?}",
        cooked[0]
    );
    assert!(cooked[0].ids.is_empty(), "nothing cooked: {:?}", cooked[0]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Folder settings must reach both the direct glTF and converted-model voxel cook paths,
/// including the source-aware cooker introduced by the HLOD lane (ADR-4853).
#[test]
fn folder_rules_enable_voxel_proxies_for_direct_and_converted_models() {
    let root = regia_test_utils::unique_temp_dir("import-rules-voxel-models");
    write_project(&root);
    std::fs::write(
        root.join("assets/props/converted.obj"),
        "v -1 0 -1\nv 1 0 -1\nv 1 0 1\nv -1 0 1\n\
         vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvn 0 1 0\n\
         f 1/1/1 3/3/1 2/2/1\nf 1/1/1 4/4/1 3/3/1\n",
    )
    .expect("OBJ fixture");
    write_rules(
        &root,
        r#"(rules: [
            (glob: "assets/props/*.gltf", settings: "(voxel_far: Some((resolution: 4)))"),
            (glob: "assets/props/*.obj", settings: "(voxel_far: Some((resolution: 4)))"),
        ])"#,
    );
    cook(&root);
    let manifest = AssetManifest::read_from_cache(&cache(&root))
        .expect("manifest")
        .expect("present");
    for source in ["assets/props/crate.gltf", "assets/props/converted.obj"] {
        assert!(
            manifest.entries.iter().any(|entry| {
                entry.path.starts_with(source)
                    && entry.type_path == regia_render::visbuffer::VOXEL_FAR_TYPE_PATH
            }),
            "folder rule must produce a voxel proxy for {source}: {:?}",
            manifest.entries
        );
        let meta =
            MetaSidecar::from_ron(&std::fs::read(meta_path_for(&root.join(source))).expect("meta bytes"))
                .expect("meta parses");
        assert!(
            meta.settings.is_none(),
            "inherited settings stay out of {source}'s meta"
        );
    }
    std::fs::remove_dir_all(&root).expect("fixture cleanup");
}
