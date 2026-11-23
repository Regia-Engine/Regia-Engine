// SPDX-License-Identifier: MPL-2.0
//! Authored LOD screen sizes from data (ADR-260, lane v0119d): `_LODn` node extras, then the
//! project's `lod_classes.toml`, then the importer default, end to end through `cook_project`.

use std::path::PathBuf;

use regia_asset::{AssetId, LodGroup, MetaSidecar, cooked_cache_dir, cooked_file_name, decode_cooked_file};
use regia_asset_pipeline::lod_classes::LOD_CLASSES_FILE;
use regia_asset_pipeline::{
    CookOptions, Forward, TextureCookQuality, cook_project, host_platform, import_gltf_glb_bytes,
};

struct TempProject(PathBuf);

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_project(label: &str) -> TempProject {
    let path = regia_test_utils::unique_temp_dir(&format!("regia_lod_{label}"));
    std::fs::create_dir_all(path.join("assets/units/battle")).expect("project dir");
    TempProject(path)
}

/// A GLB with a root `Hull` node (`root_extras`) and children `Hull_LOD0..N-1`, each one triangle,
/// with per-node extras from `lod_extras` (`""` = none).
fn hull_glb<const N: usize>(root_extras: &str, lod_extras: [&str; N]) -> Vec<u8> {
    let positions = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let mut bin = Vec::new();
    for p in &positions {
        for c in p {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in [0u32, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let extras = |e: &str| {
        if e.is_empty() {
            String::new()
        } else {
            format!(r#","extras":{e}"#)
        }
    };
    let mesh = r#"{"primitives":[{"attributes":{"POSITION":0},"indices":1,"mode":4}]}"#;
    let lods: Vec<String> = (0..N)
        .map(|i| format!(r#"{{"mesh":{i},"name":"Hull_LOD{i}"{}}}"#, extras(lod_extras[i])))
        .collect();
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"name":"Hull","children":[{children}]{root}}},{lods}],"meshes":[{meshes}],"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]}},{{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":12}}],"buffers":[{{"byteLength":{bl}}}]}}"#,
        root = extras(root_extras),
        lods = lods.join(","),
        children = (1..=N).map(|i| i.to_string()).collect::<Vec<_>>().join(","),
        meshes = vec![mesh; N].join(","),
        bl = bin.len(),
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut glb = Vec::new();
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

fn sizes(group: &LodGroup) -> Vec<f32> {
    group.levels.iter().map(|l| l.screen_relative_size).collect()
}

fn import_sizes(glb: &[u8]) -> Vec<f32> {
    let imported = import_gltf_glb_bytes(
        glb,
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
    )
    .expect("import");
    assert_eq!(imported.lod_groups.len(), 1);
    sizes(&imported.lod_groups[0])
}

/// The cooked `LodGroup` sub-asset of `assets/<rel>`.
fn cooked_group(project: &TempProject, rel: &str) -> LodGroup {
    let meta = std::fs::read(project.0.join(format!("assets/{rel}.meta"))).expect("meta");
    let meta = MetaSidecar::from_ron(&meta).expect("meta ron");
    let id = AssetId::sub_asset(meta.id, "lod_group/Hull");
    let cache = cooked_cache_dir(&project.0, host_platform());
    let bytes = std::fs::read(cache.join(cooked_file_name(id))).expect("cooked lod group");
    let (_, payload) = decode_cooked_file(&bytes).expect("header");
    postcard::from_bytes(payload).expect("LodGroup")
}

#[test]
fn import_without_data_halves_defaults_and_node_extras_win() {
    assert_eq!(
        import_sizes(&hull_glb("", ["", "", "", ""])),
        vec![1.0, 0.5, 0.25, 0.125]
    );
    let glb = hull_glb(
        "",
        [
            r#"{"lod_screen_size":1.6}"#,
            r#"{"lod_screen_size":0.7}"#,
            r#"{"lod_screen_size":0.2}"#,
            r#"{"lod_screen_size":0.06}"#,
        ],
    );
    assert_eq!(import_sizes(&glb), vec![1.6, 0.7, 0.2, 0.06]);
    // Hostile extras: ignored or repaired, never a panic, always strictly decreasing.
    let glb = hull_glb(
        r#"{"lod_class":42}"#,
        [
            r#"{"lod_screen_size":"huge"}"#,
            r#"{"lod_screen_size":1e300}"#,
            r#"{"lod_screen_size":2.0}"#,
            r#"{"lod_screen_size":-1}"#,
        ],
    );
    let got = import_sizes(&glb);
    assert_eq!(
        got,
        vec![1.0, 0.5, 0.25, 0.125],
        "LOD2's 2.0 repairs to half of LOD1"
    );
}

#[test]
fn cook_takes_the_class_table_and_node_extras_beat_it() {
    let project = temp_project("classes");
    std::fs::write(
        project.0.join(LOD_CLASSES_FILE),
        r#"
version = 1

[classes]
battle = [1.2, 0.45, 0.16, 0.05]
heavy = [1.4, 0.5, 0.18, 0.06]
broken = [0.1, 0.9]

[[assign]]
glob = "assets/units/battle/**"
class = "battle"
"#,
    )
    .expect("table");
    let dir = project.0.join("assets/units/battle");
    // By glob; by the root's extras (beats the glob); a node value beats the class; a class the
    // table lacks keeps the defaults.
    std::fs::write(dir.join("glob.glb"), hull_glb("", ["", "", "", ""])).expect("glb");
    std::fs::write(
        dir.join("root.glb"),
        hull_glb(r#"{"lod_class":"heavy"}"#, ["", "", "", ""]),
    )
    .expect("glb");
    std::fs::write(
        dir.join("node.glb"),
        hull_glb("", ["", r#"{"lod_screen_size":0.6}"#, "", ""]),
    )
    .expect("glb");
    std::fs::write(
        dir.join("unknown.glb"),
        hull_glb(r#"{"lod_class":"dropship"}"#, ["", "", "", ""]),
    )
    .expect("glb");
    std::fs::write(project.0.join("assets/plain.glb"), hull_glb("", ["", "", "", ""])).expect("glb");

    let report = cook_project(CookOptions::for_host(project.0.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        report.warnings.iter().any(|w| w.contains("broken")),
        "the bad class is reported once: {:?}",
        report.warnings
    );
    assert_eq!(
        sizes(&cooked_group(&project, "units/battle/glob.glb")),
        vec![1.2, 0.45, 0.16, 0.05]
    );
    assert_eq!(
        sizes(&cooked_group(&project, "units/battle/root.glb")),
        vec![1.4, 0.5, 0.18, 0.06]
    );
    assert_eq!(
        sizes(&cooked_group(&project, "units/battle/node.glb")),
        vec![1.2, 0.6, 0.16, 0.05]
    );
    assert_eq!(
        sizes(&cooked_group(&project, "units/battle/unknown.glb")),
        vec![1.0, 0.5, 0.25, 0.125]
    );
    assert_eq!(
        sizes(&cooked_group(&project, "plain.glb")),
        vec![1.0, 0.5, 0.25, 0.125]
    );

    // Editing the table recooks the files it reaches.
    std::fs::write(
        project.0.join(LOD_CLASSES_FILE),
        "version = 1\n[classes]\nbattle = [2.0, 1.0, 0.5, 0.25]\n[[assign]]\nglob = \"assets/units/battle/**\"\nclass = \"battle\"\n",
    )
    .expect("table");
    let report = cook_project(CookOptions::for_host(project.0.clone())).expect("recook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        sizes(&cooked_group(&project, "units/battle/glob.glb")),
        vec![2.0, 1.0, 0.5, 0.25]
    );
}

/// The five-level cooked chain must actually select LOD3 between LOD2 and LOD4.
#[test]
fn five_level_cook_preserves_a_selectable_lod3() {
    let project = temp_project("five_levels");
    std::fs::write(project.0.join("assets/five.glb"), hull_glb("", [""; 5])).expect("glb");
    let report = cook_project(CookOptions::for_host(project.0.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let thresholds = sizes(&cooked_group(&project, "five.glb"));
    assert_eq!(thresholds, vec![1.0, 0.5, 0.25, 0.125, 0.0625]);
    assert_eq!(regia_asset::lod_at(&thresholds, 0.1875), 3);
    assert_eq!(regia_asset::lod_at(&thresholds, 0.09375), 4);
    assert_eq!(regia_asset::lod_at(&thresholds, 0.375), 2);
    let again = cook_project(CookOptions::for_host(project.0.clone())).expect("warm cook");
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(sizes(&cooked_group(&project, "five.glb")), thresholds);
}

#[test]
fn lod_class_table_reaches_cook() {
    let project = temp_project("stem_classes");
    std::fs::write(
        project.0.join(LOD_CLASSES_FILE),
        "version = 1\n[classes]\nprobe = [1.3, 0.6, 0.2, 0.04]\nexplicit = [1.8, 0.8, 0.3, 0.08]\n",
    )
    .expect("table");
    std::fs::write(project.0.join("assets/probe.glb"), hull_glb("", [""; 4])).expect("glb");
    std::fs::write(
        project.0.join("assets/units/battle/probe.glb"),
        hull_glb(r#"{"lod_class":"explicit"}"#, [""; 4]),
    )
    .expect("glb");
    let report = cook_project(CookOptions::for_host(project.0.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        sizes(&cooked_group(&project, "probe.glb")),
        vec![1.3, 0.6, 0.2, 0.04]
    );
    assert_eq!(
        sizes(&cooked_group(&project, "units/battle/probe.glb")),
        vec![1.8, 0.8, 0.3, 0.08]
    );
}

#[test]
fn stem_class_precedence_and_exact_names() {
    use regia_asset_pipeline::lod_classes::LodClassTable;
    let (table, warnings) = LodClassTable::parse(
        "version = 1\n[classes]\nprobe = [1.3, 0.6]\nassigned = [1.5, 0.7]\n\
         [[assign]]\nglob = \"assets/assigned/**\"\nclass = \"assigned\"\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let table = table.expect("table");
    assert_eq!(table.class_for("assets/probe.glb", None), Some("probe"));
    assert_eq!(table.class_for("assets/probe.gltf", None), Some("probe"));
    assert_eq!(
        table.class_for("assets/assigned/probe.glb", None),
        Some("assigned")
    );
    assert_eq!(
        table.class_for("assets/assigned/probe.glb", Some("explicit")),
        Some("explicit")
    );
    assert_eq!(
        table.class_for("assets/Probe.glb", None),
        None,
        "class names are exact"
    );
    assert_eq!(
        table.class_for("assets/probe_variant.glb", None),
        None,
        "no prefix inference"
    );
}

#[test]
fn stem_class_table_edits_and_removal_reach_incremental_cook() {
    let project = temp_project("stem_recook");
    // Exercise a JSON glTF as well as the GLB in lod_class_table_reaches_cook.
    let glb = hull_glb("", [""; 4]);
    let json_len = u32::from_le_bytes(glb[12..16].try_into().expect("length")) as usize;
    let mut json: serde_json::Value = serde_json::from_slice(&glb[20..20 + json_len]).expect("json");
    json["buffers"][0]["uri"] = serde_json::json!("probe.bin");
    regia_test_utils::write_atomic(&project.0.join("assets/probe.bin"), &glb[28 + json_len..])
        .expect("buffer");
    regia_test_utils::write_atomic(
        &project.0.join("assets/probe.gltf"),
        &serde_json::to_vec(&json).expect("json"),
    )
    .expect("gltf");
    let options = CookOptions::for_host(project.0.clone());
    let initial = cook_project(options.clone()).expect("cook without table");
    assert!(initial.errors.is_empty(), "{initial:?}");
    assert_eq!(
        sizes(&cooked_group(&project, "probe.gltf")),
        [1.0, 0.5, 0.25, 0.125]
    );
    for values in [[1.3, 0.6, 0.2, 0.04], [1.8, 0.8, 0.3, 0.08]] {
        let table = format!("version = 1\n[classes]\nprobe = {values:?}\n");
        regia_test_utils::write_atomic(&project.0.join(LOD_CLASSES_FILE), table.as_bytes()).expect("table");
        let report = cook_project(options.clone()).expect("cook with table");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(sizes(&cooked_group(&project, "probe.gltf")), values);
        let again = cook_project(options.clone()).expect("unchanged cook");
        assert!(again.errors.is_empty(), "{again:?}");
        assert_eq!(
            again.imports_skipped, 1,
            "metadata and class inputs settle after one cook"
        );
    }
    std::fs::remove_file(project.0.join(LOD_CLASSES_FILE)).expect("remove table");
    let report = cook_project(options).expect("cook after removing table");
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        sizes(&cooked_group(&project, "probe.gltf")),
        [1.0, 0.5, 0.25, 0.125]
    );
}
