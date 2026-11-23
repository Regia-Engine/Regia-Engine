// SPDX-License-Identifier: MPL-2.0
//! `ModelReport` (Tincal engine request F14): a synthetic GLB with a node tree, two meshes, a
//! duplicate name, a node outside the scene, a skin, and two embedded PNG textures is reported
//! (counts, bound maps and sizes, budget flags, duplicates, orphans, JSON, text, the agent
//! method and a spawned-hierarchy comparison); the Khronos Fox is reported when the test
//! assets are fetched (`cargo xtask fetch-test-assets`).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_editor::gltf_hierarchy::{GltfSpawnOptions, spawn_gltf_hierarchy};
use regia_editor::model_report::{BudgetLimits, FlagKind, MapSlot, ModelClass, ModelReport, methods};
use regia_scene::prelude::Name;
use serde_json::json;

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer
            .write_image_data(&vec![200u8; (width * height * 4) as usize])
            .expect("png data");
    }
    out
}

/// ```text
/// Root ─ Body (2 prims: Paint 2 tris + Trim 1 tri) ─ Dup (Paint, 1 tri) ─ socket_top
///      └ Dup (empty) ─ Hip (joint) ─ Hand (joint)           Skinny (scene root, Trim, skin 0)
/// Stray (not in the scene)
/// ```
fn fixture_glb() -> Vec<u8> {
    let positions = f32s(&[
        0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // tri A
        0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0, // tri B
        0.0, 0.0, 2.0, 1.0, 0.0, 2.0, 0.0, 1.0, 2.0, // tri C
    ]);
    let color_png = png_bytes(8, 4);
    let normal_png = png_bytes(4, 4);
    let mut bin = Vec::new();
    fn view(bytes: &[u8], bin: &mut Vec<u8>) -> (usize, usize) {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let range = (bin.len(), bytes.len());
        bin.extend(bytes);
        range
    }
    let (pos_off, pos_len) = view(&positions, &mut bin);
    let (color_off, color_len) = view(&color_png, &mut bin);
    let (normal_off, normal_len) = view(&normal_png, &mut bin);
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 6] }],
        "nodes": [
            { "name": "Root", "children": [1, 2] },
            { "name": "Body", "mesh": 0, "children": [3] },
            { "name": "Dup", "children": [4] },
            { "name": "Dup", "mesh": 1, "children": [7] },
            { "name": "Hip", "children": [5] },
            { "name": "Hand" },
            { "name": "Skinny", "mesh": 1, "skin": 0 },
            { "name": "socket_top" },
            { "name": "Stray", "mesh": 1 }
        ],
        "meshes": [
            { "primitives": [
                { "attributes": { "POSITION": 0 }, "material": 0 },
                { "attributes": { "POSITION": 1 }, "material": 1 }
            ] },
            { "primitives": [{ "attributes": { "POSITION": 2 }, "material": 0 }] }
        ],
        "materials": [
            { "name": "Paint", "pbrMetallicRoughness": { "baseColorTexture": { "index": 0 } }, "normalTexture": { "index": 1 } },
            { "name": "Trim", "pbrMetallicRoughness": { "baseColorFactor": [0.5, 0.5, 0.5, 1.0] } }
        ],
        "textures": [{ "source": 0 }, { "source": 1 }],
        "images": [
            { "bufferView": 1, "mimeType": "image/png" },
            { "bufferView": 2, "mimeType": "image/png" }
        ],
        "skins": [{ "joints": [4, 5] }],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": pos_off, "byteLength": pos_len },
            { "buffer": 0, "byteOffset": color_off, "byteLength": color_len },
            { "buffer": 0, "byteOffset": normal_off, "byteLength": normal_len }
        ],
        "accessors": [
            { "bufferView": 0, "byteOffset": 0, "componentType": 5126, "count": 6, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0] },
            { "bufferView": 0, "byteOffset": 72, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 2.0], "max": [1.0, 1.0, 2.0] },
            { "bufferView": 0, "byteOffset": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] }
        ]
    });
    let mut json_bytes = serde_json::to_vec(&json).expect("fixture json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut glb = Vec::with_capacity(total);
    glb.extend(b"glTF");
    glb.extend(2u32.to_le_bytes());
    glb.extend((total as u32).to_le_bytes());
    glb.extend((json_bytes.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(&json_bytes);
    glb.extend((bin.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(&bin);
    glb
}

fn write_fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-model-report-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, fixture_glb()).expect("write fixture");
    path
}

#[test]
fn counts_tree_maps_duplicates_and_orphans_match_the_file() {
    let path = write_fixture("report.glb");
    let report = ModelReport::from_file(&path, ModelClass::Prop).expect("report");

    // Node tree.
    assert_eq!(report.totals.nodes, 9);
    assert_eq!(report.totals.nodes_in_scene, 8);
    assert_eq!(report.roots, vec![0, 6]);
    let node = |name: &str| report.nodes.iter().find(|n| n.name == name).expect(name);
    assert_eq!(node("Root").depth, 0);
    assert_eq!(node("Body").parent, Some(0));
    assert_eq!(node("Body").depth, 1);
    assert_eq!(node("Hand").depth, 3);
    assert_eq!(node("Body").triangles, 3);
    assert_eq!(node("Body").vertices, 9);
    assert_eq!(node("Body").materials, vec!["Paint", "Trim"]);
    assert_eq!(node("Hip").triangles, 0);
    assert!(node("socket_top").is_socket);
    assert!(node("Hip").is_joint && node("Hand").is_joint && !node("Body").is_joint);

    // Skin.
    assert_eq!(report.skins.len(), 1);
    assert_eq!(report.skins[0].joints, 2);
    assert_eq!(report.skins[0].meshes, vec![6]);
    assert_eq!(node("Skinny").skin, Some(0));
    assert_eq!(node("Skinny").joints, 2);
    assert_eq!(report.totals.joints, 2);

    // Totals: Body 3 + Dup(mesh) 1 + Skinny 1 + Stray 1.
    assert_eq!(report.totals.triangles, 6);
    assert_eq!(report.totals.materials_in_use, 2);
    assert_eq!(report.materials[0].nodes, vec![1, 3, 6, 8]);

    // Maps: Paint binds base color 8x4 and normal 4x4; Trim binds nothing.
    let map = |material: &str, slot: MapSlot| {
        report
            .maps
            .iter()
            .find(|m| m.material == material && m.slot == slot)
            .expect("map row")
    };
    assert_eq!(report.maps.len(), 10);
    assert!(map("Paint", MapSlot::BaseColor).bound);
    assert_eq!(map("Paint", MapSlot::BaseColor).size, Some((8, 4)));
    assert!(map("Paint", MapSlot::Normal).bound);
    assert_eq!(map("Paint", MapSlot::Normal).size, Some((4, 4)));
    assert!(!map("Paint", MapSlot::Orm).bound);
    assert_eq!(map("Paint", MapSlot::Orm).size, None);
    assert!(!map("Trim", MapSlot::Normal).bound);
    assert!(map("Trim", MapSlot::Normal).used);
    assert_eq!(report.totals.textures, 2);
    assert_eq!(report.totals.max_texture_dimension, 8);
    assert_eq!(
        report.totals.texture_bytes,
        (8 * 4 * 4 * 4 / 3) + (4 * 4 * 4 * 4 / 3)
    );

    // Duplicates and orphans.
    assert_eq!(report.duplicate_names.len(), 1);
    assert_eq!(report.duplicate_names[0].name, "Dup");
    assert_eq!(report.duplicate_names[0].nodes, vec![2, 3]);
    assert_eq!(report.orphans.len(), 1);
    assert_eq!(report.orphans[0].name, "Stray");
    assert_eq!(report.orphans[0].gltf_index, Some(8));
    assert!(!report.is_clean());
    // The defaults are generous for this small model.
    assert!(report.flags.is_empty(), "{:?}", report.flags);
}

#[test]
fn primitives_without_a_material_count_as_the_default_material() {
    let positions = f32s(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": "Plain", "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 } }] }],
        "buffers": [{ "byteLength": positions.len() }],
        "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": positions.len() }],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] }
        ]
    });
    let mut json_bytes = serde_json::to_vec(&json).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + 8 + positions.len();
    let mut glb = Vec::new();
    glb.extend(b"glTF");
    glb.extend(2u32.to_le_bytes());
    glb.extend((total as u32).to_le_bytes());
    glb.extend((json_bytes.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(&json_bytes);
    glb.extend((positions.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(&positions);
    let dir = std::env::temp_dir().join(format!("regia-model-report-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("default_material.glb");
    std::fs::write(&path, glb).expect("write");
    let report = ModelReport::from_file(&path, ModelClass::Prop).expect("report");
    assert_eq!(report.totals.triangles, 1);
    assert_eq!(report.totals.materials_in_use, 1);
    let default = report
        .materials
        .iter()
        .find(|m| m.name == "default")
        .expect("default entry");
    assert_eq!(default.triangles, 1);
    assert_eq!(default.nodes, vec![0]);
}

#[test]
fn budget_flags_follow_the_class_and_the_overrides() {
    let path = write_fixture("budget.glb");
    let limits = ModelClass::Prop
        .limits()
        .with_overrides(&json!({
            "max_triangles": 5,
            "max_node_triangles": 2,
            "max_materials_per_node": 1,
            "max_texture_dimension": 4,
            "max_joints_per_skin": 1,
            "max_nodes": 8,
            "require_normal_maps": true
        }))
        .expect("limits");
    let report = ModelReport::from_file_with(&path, ModelClass::Prop, limits).expect("report");
    let kinds: Vec<FlagKind> = report.flags.iter().map(|f| f.kind).collect();
    for kind in [
        FlagKind::Triangles,
        FlagKind::NodeTriangles,
        FlagKind::MaterialsPerNode,
        FlagKind::TextureSize,
        FlagKind::Joints,
        FlagKind::Nodes,
        FlagKind::MissingNormalMap,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} missing from {kinds:?}");
    }
    let find = |kind| report.flags.iter().find(|f| f.kind == kind).expect("flag");
    assert_eq!(
        (find(FlagKind::Triangles).value, find(FlagKind::Triangles).limit),
        (6, 5)
    );
    assert_eq!(find(FlagKind::NodeTriangles).subject, "Body");
    assert_eq!(find(FlagKind::TextureSize).subject, "Paint/base_color");
    assert_eq!(find(FlagKind::MissingNormalMap).subject, "Trim/normal");
    assert_eq!(find(FlagKind::Joints).value, 2);
    // A zero limit switches a check off.
    let relaxed = report.clone().with_limits(
        ModelClass::Vehicle,
        BudgetLimits {
            max_triangles: 0,
            max_node_triangles: 0,
            max_nodes: 0,
            max_materials: 0,
            max_materials_per_node: 0,
            max_joints_per_skin: 0,
            max_texture_dimension: 0,
            max_texture_bytes: 0,
            require_normal_maps: false,
            lod1_permille: 0,
            lod2_permille: 0,
            lod3_permille: 0,
            lod_ratio_tolerance_percent: 0,
            max_lod2_triangles: 0,
            max_lod3_triangles: 0,
            max_glass_materials: 0,
        },
    );
    assert!(relaxed.flags.is_empty());
    assert_eq!(relaxed.class, ModelClass::Vehicle);
}

#[test]
fn json_and_text_renderings_carry_the_findings() {
    let path = write_fixture("render.glb");
    let report = ModelReport::from_file(&path, ModelClass::Character).expect("report");
    let json = report.to_json();
    assert_eq!(json["class"], "character");
    assert_eq!(json["totals"]["triangles"], 6);
    assert_eq!(json["nodes"][1]["name"], "Body");
    assert_eq!(json["maps"][0]["slot"], "base_color");
    assert_eq!(json["maps"][0]["size"], json!([8, 4]));
    let back: ModelReport = serde_json::from_value(json).expect("round trip");
    assert_eq!(back, report);

    let text = report.to_text();
    assert!(text.starts_with("model "), "{text}");
    assert!(text.contains("[character]"), "{text}");
    assert!(text.contains("    Body #1 tris 3 mat [Paint, Trim]"), "{text}");
    assert!(text.contains("Hip #4 joint"), "{text}");
    assert!(text.contains("socket"), "{text}");
    assert!(
        text.contains("Paint: base_color 8x4, normal 4x4, orm -"),
        "{text}"
    );
    assert!(text.contains("DUPLICATE Dup x2"), "{text}");
    assert!(text.contains("ORPHAN Stray"), "{text}");
    assert!(text.contains("(outside scene)"), "{text}");
}

fn new_world() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app
}

#[test]
fn a_spawned_hierarchy_is_compared_by_name() {
    let path = write_fixture("spawned.glb");
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn_gltf_hierarchy(world, &path, &GltfSpawnOptions::default()).expect("spawn");

    // Intact: only the file's own orphan (Stray) remains.
    let report = ModelReport::from_spawn(world, &spawned, &path, ModelClass::Prop).expect("report");
    let names: Vec<&str> = report.orphans.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, vec!["Stray"], "{:?}", report.orphans);

    // A despawned part and a foreign entity show up.
    let hand = spawned.entity("Hand").expect("Hand");
    world.despawn(hand);
    let intruder = world.spawn((Name::new("Intruder"), ChildOf(spawned.root))).id();
    let _ = intruder;
    let report = ModelReport::from_spawn(world, &spawned, &path, ModelClass::Prop).expect("report");
    let orphan = |name: &str| report.orphans.iter().find(|o| o.name == name);
    assert_eq!(orphan("Hand").and_then(|o| o.gltf_index), Some(5));
    assert!(orphan("Hand").is_some_and(|o| o.reason.contains("no entity")));
    assert_eq!(orphan("Intruder").and_then(|o| o.gltf_index), None);
    assert!(orphan("Stray").is_some());
    // The duplicate `Dup` nodes are matched one to one: both entities exist.
    assert!(orphan("Dup").is_none());

    // A socket left out by `keep_sockets: false` is not an orphan.
    let mut app = new_world();
    let world = app.world_mut();
    let opts = GltfSpawnOptions {
        keep_sockets: false,
        ..GltfSpawnOptions::default()
    };
    let spawned = spawn_gltf_hierarchy(world, &path, &opts).expect("spawn");
    let report = ModelReport::from_spawn(world, &spawned, &path, ModelClass::Prop).expect("report");
    assert!(
        report.orphans.iter().all(|o| o.name != "socket_top"),
        "{:?}",
        report.orphans
    );
}

#[test]
fn the_agent_method_reports_and_validates() {
    let path = write_fixture("agent.glb");
    let (schema, handler) = methods().remove(0);
    assert_eq!(schema.name, "editor.model_report");
    assert!(!schema.mutating);

    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn_gltf_hierarchy(world, &path, &GltfSpawnOptions::default()).expect("spawn");
    let result = handler(
        world,
        &json!({
            "path": path.to_str().expect("utf-8 path"),
            "class": "vehicle",
            "limits": { "max_triangles": 2 },
            "root": spawned.root.to_bits(),
            "text": true
        }),
    )
    .expect("report");
    assert_eq!(result["report"]["class"], "vehicle");
    assert_eq!(result["report"]["limits"]["max_triangles"], 2);
    assert_eq!(result["report"]["flags"][0]["kind"], "triangles");
    assert_eq!(result["clean"], false);
    assert!(result["text"].as_str().is_some_and(|t| t.contains("FLAG")));
    assert_eq!(result["report"]["orphans"][0]["name"], "Stray");

    let mut bad = |params: serde_json::Value| handler(world, &params).expect_err("invalid");
    let p = path.to_str().expect("utf-8 path");
    assert!(bad(json!({})).message.contains("path"));
    assert!(
        bad(json!({ "path": p, "class": "tank" }))
            .message
            .contains("class")
    );
    assert!(
        bad(json!({ "path": p, "limits": { "nope": 1 } }))
            .message
            .contains("nope")
    );
    assert!(
        bad(json!({ "path": p, "root": 12345 }))
            .message
            .contains("entity")
    );
    assert!(handler(world, &json!({ "path": "/nonexistent/x.glb" })).is_err());
}

#[test]
fn the_extensions_method_lists_known_and_unknown_extensions() {
    let dir = std::env::temp_dir().join(format!("regia_model_ext_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("ext.gltf");
    std::fs::write(
        &path,
        r#"{"asset":{"version":"2.0"},"extensionsUsed":["KHR_materials_clearcoat","VENDOR_x"],
        "materials":[{"extensions":{"KHR_materials_clearcoat":{"clearcoatFactor":0.5},"VENDOR_x":{"k":1}}}]}"#,
    )
    .expect("write");
    let (schema, handler) = methods().remove(1);
    assert_eq!(schema.name, "editor.model_extensions");
    assert!(!schema.mutating);
    let mut app = new_world();
    let world = app.world_mut();
    let p = path.to_str().expect("utf-8 path");
    let out = handler(world, &json!({ "path": p })).expect("extensions");
    let ext = &out["extensions"];
    assert_eq!(ext["materials"][0]["clearcoat"]["factor"], 0.5);
    assert_eq!(ext["unknown_names"][0], "VENDOR_x");
    assert!(
        ext["unknown"][0].get("json").is_none(),
        "raw JSON only on request"
    );
    let out = handler(world, &json!({ "path": p, "include_json": true })).expect("extensions");
    assert_eq!(out["extensions"]["unknown"][0]["json"]["k"], 1);
    assert!(
        handler(world, &json!({}))
            .expect_err("path")
            .message
            .contains("path")
    );
    assert!(handler(world, &json!({ "path": "/nonexistent/x.glb" })).is_err());
}

/// The Khronos Fox, when `cargo xtask fetch-test-assets` has run (or `REGIA_KHRONOS_ASSETS`
/// points at the cache).
fn fox() -> Option<PathBuf> {
    if let Ok(dir) = regia_core::brand::env_var("REGIA_KHRONOS_ASSETS") {
        let path = Path::new(&dir).join("Fox.glb");
        return path.exists().then_some(path);
    }
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    while dir.pop() {
        let path = dir.join(".regia/test-assets/khronos-gltf").join("Fox.glb");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

#[test]
fn the_fox_reports_its_skin_nodes_and_texture() {
    let Some(path) = fox() else {
        // CI fetches the assets first, so a missing Fox there is a failure, not a skip.
        assert!(
            regia_core::brand::env_var_os("CI").is_none(),
            "Fox.glb missing in CI: run `cargo xtask fetch-test-assets`"
        );
        eprintln!("skip the Fox: run `cargo xtask fetch-test-assets` or set REGIA_KHRONOS_ASSETS");
        return;
    };
    let report = ModelReport::from_file(&path, ModelClass::Character).expect("report");
    // The Khronos Fox: one skin of 24 joints, one mesh of 576 triangles, one 1024x1024 texture.
    assert_eq!(report.totals.skins, 1);
    assert_eq!(report.skins[0].joints, 24);
    assert_eq!(report.totals.joints, 24);
    assert_eq!(report.totals.triangles, 576);
    assert_eq!(report.totals.materials_in_use, 1);
    assert_eq!(report.totals.textures, 1);
    assert_eq!(report.totals.max_texture_dimension, 1024);
    let base = report
        .maps
        .iter()
        .find(|m| m.slot == MapSlot::BaseColor)
        .expect("base color row");
    assert!(base.bound && base.used);
    assert_eq!(base.size, Some((1024, 1024)));
    assert!(
        report
            .maps
            .iter()
            .filter(|m| m.slot == MapSlot::Normal)
            .all(|m| !m.bound)
    );
    // Every joint node is in the scene and flagged as a joint; the skinned mesh node has the skin.
    assert_eq!(report.nodes.iter().filter(|n| n.is_joint).count(), 24);
    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    assert!(
        report
            .nodes
            .iter()
            .any(|n| n.skin == Some(0) && n.triangles == 576)
    );
    assert!(report.flags.is_empty(), "{:?}", report.flags);
    // The 1024-pixel texture fits the character budget; tighten the limit to see the flag.
    let tight = ModelClass::Character
        .limits()
        .with_overrides(&json!({ "max_texture_dimension": 512 }))
        .expect("limits");
    let report = report.with_limits(ModelClass::Character, tight);
    assert_eq!(report.flags.len(), 1);
    assert_eq!(report.flags[0].kind, FlagKind::TextureSize);
    let text = report.to_text();
    assert!(text.contains("24 joints"), "{text}");
}

/// Builds a GLB with one mesh node per `(name, triangles)` and one material per name in
/// `materials` (each node uses all of them, as separate primitives).
fn lod_glb(nodes: &[(&str, u32)], materials: &[(&str, bool)]) -> Vec<u8> {
    let max_tris = nodes.iter().map(|n| n.1).max().unwrap_or(1).max(1) as usize;
    // One shared position buffer of max_tris triangles; each node's accessor counts a prefix.
    let positions = f32s(&vec![0.5; max_tris * 9]);
    let node_json: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(i, (name, _))| json!({ "name": name, "mesh": i }))
        .collect();
    let mesh_json: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let primitives: Vec<_> = (0..materials.len().max(1))
                .map(|m| {
                    let mut p = json!({ "attributes": { "POSITION": i } });
                    if !materials.is_empty() {
                        p["material"] = json!(m);
                    }
                    p
                })
                .collect();
            json!({ "primitives": primitives })
        })
        .collect();
    let accessors: Vec<_> = nodes
        .iter()
        .map(|(_, tris)| {
            json!({ "bufferView": 0, "componentType": 5126, "count": tris * 3, "type": "VEC3",
                    "min": [0.5, 0.5, 0.5], "max": [0.5, 0.5, 0.5] })
        })
        .collect();
    let material_json: Vec<_> = materials
        .iter()
        .map(|(name, transmissive)| {
            let mut m = json!({ "name": name });
            if *transmissive {
                m["extensions"] = json!({ "KHR_materials_transmission": { "transmissionFactor": 1.0 } });
            }
            m
        })
        .collect();
    let mut doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": (0..nodes.len()).collect::<Vec<_>>() }],
        "nodes": node_json,
        "meshes": mesh_json,
        "buffers": [{ "byteLength": positions.len() }],
        "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": positions.len() }],
        "accessors": accessors,
    });
    if !materials.is_empty() {
        doc["materials"] = json!(material_json);
        if materials.iter().any(|m| m.1) {
            doc["extensionsUsed"] = json!(["KHR_materials_transmission"]);
        }
    }
    let mut json_bytes = serde_json::to_vec(&doc).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + 8 + positions.len();
    let mut glb = Vec::new();
    glb.extend(b"glTF");
    glb.extend(2u32.to_le_bytes());
    glb.extend((total as u32).to_le_bytes());
    glb.extend((json_bytes.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(&json_bytes);
    glb.extend((positions.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(&positions);
    glb
}

fn write_glb(name: &str, glb: &[u8]) -> PathBuf {
    // Process id plus a counter: unique per test, never a fixed name. Written to a sibling file
    // and renamed so a reader never sees a partial file.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-model-report-hero-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, glb).expect("write glb");
    std::fs::rename(&tmp, &path).expect("rename glb");
    path
}

#[test]
fn hero_class_has_the_do27_hull_budget() {
    assert_eq!(ModelClass::from_name("hero"), Some(ModelClass::Hero));
    assert_eq!(ModelClass::Hero.name(), "hero");
    let limits = ModelClass::Hero.limits();
    assert_eq!(limits.max_triangles, 350_000);
    assert_eq!(limits.max_materials, 4);
    assert_eq!(limits.max_glass_materials, 1);
    assert_eq!(limits.max_lod2_triangles, 14_999);
    assert_eq!(limits.max_lod3_triangles, 3_999);
    assert_eq!(
        (limits.lod1_permille, limits.lod2_permille, limits.lod3_permille),
        (200, 50, 12)
    );
    // The other classes keep the LOD and glass checks off, and Character is unchanged.
    for class in [
        ModelClass::Prop,
        ModelClass::Character,
        ModelClass::Vehicle,
        ModelClass::Building,
    ] {
        let l = class.limits();
        assert_eq!(
            (l.max_lod2_triangles, l.max_glass_materials, l.lod1_permille),
            (0, 0, 0)
        );
    }
    assert_eq!(ModelClass::Character.limits().max_materials, 8);
}

#[test]
fn hero_lods_within_ratio_and_ceilings_are_clean() {
    // One transmissive material: the single glass material a hero object may have.
    let glb = lod_glb(
        &[
            ("Hull_LOD0", 300_000),
            ("Hull_LOD1", 60_000),
            ("Hull_LOD2", 14_000),
            ("Hull_LOD3", 3_600),
        ],
        &[("Canopy", true)],
    );
    let path = write_glb("hero_ok.glb", &glb);
    let report = ModelReport::from_file(&path, ModelClass::Hero).expect("report");
    let levels: Vec<(u32, u64)> = report.lods.iter().map(|l| (l.level, l.triangles)).collect();
    assert_eq!(levels, vec![(0, 300_000), (1, 60_000), (2, 14_000), (3, 3_600)]);
    assert_eq!(report.totals.triangles, 300_000);
    assert_eq!(report.totals.glass_materials, 1);
    assert!(report.materials.iter().any(|m| m.name == "Canopy" && m.glass));
    assert!(report.flags.is_empty(), "{:?}", report.flags);
}

#[test]
fn hero_lod_ceilings_ratios_and_glass_are_flagged() {
    // One material per node keeps the counts exact.
    let glb = lod_glb(
        &[
            ("Hull_LOD0", 100_000),
            ("Hull_LOD1", 60_000),
            ("Hull_LOD2", 16_000),
            ("Hull_LOD3", 4_000),
        ],
        &[("Glass", false)],
    );
    let path = write_glb("hero_bad.glb", &glb);
    let report = ModelReport::from_file(&path, ModelClass::Hero).expect("report");
    let levels: Vec<(u32, u64)> = report.lods.iter().map(|l| (l.level, l.triangles)).collect();
    assert_eq!(levels, vec![(0, 100_000), (1, 60_000), (2, 16_000), (3, 4_000)]);
    // Only LOD0 counts toward the model total.
    assert_eq!(report.totals.triangles, 100_000);
    let flag = |kind: FlagKind, subject: &str| {
        report
            .flags
            .iter()
            .find(|f| f.kind == kind && f.subject == subject)
    };
    assert_eq!(
        flag(FlagKind::LodTriangles, "LOD2").map(|f| f.limit),
        Some(14_999)
    );
    assert_eq!(flag(FlagKind::LodTriangles, "LOD3").map(|f| f.limit), Some(3_999));
    // 20 % of 100k is 20k, +25 % tolerance is 25k: 60k is over.
    assert_eq!(flag(FlagKind::LodRatio, "LOD1").map(|f| f.limit), Some(25_000));
    assert!(flag(FlagKind::LodRatio, "LOD2").is_some());
    // A glass-named material counts as glass.
    assert_eq!(report.totals.glass_materials, 1);

    // Two glass materials exceed the one allowed.
    let glb = lod_glb(&[("Hull", 10)], &[("Cockpit Glass", false), ("Visor", true)]);
    let path = write_glb("hero_glass.glb", &glb);
    let report = ModelReport::from_file(&path, ModelClass::Hero).expect("report");
    assert_eq!(report.totals.glass_materials, 2);
    let glass = report
        .flags
        .iter()
        .find(|f| f.kind == FlagKind::GlassMaterials)
        .expect("glass flag");
    assert_eq!((glass.value, glass.limit), (2, 1));
    // No LOD suffix, no LOD report.
    assert!(report.lods.is_empty());
}

#[test]
fn hero_material_cap_and_triangle_cap_flag() {
    let glb = lod_glb(
        &[("Hull", 360_000)],
        &[
            ("A", false),
            ("B", false),
            ("C", false),
            ("D", false),
            ("E", false),
        ],
    );
    let path = write_glb("hero_caps.glb", &glb);
    let report = ModelReport::from_file(&path, ModelClass::Hero).expect("report");
    assert!(
        report
            .flags
            .iter()
            .any(|f| f.kind == FlagKind::Materials && f.limit == 4)
    );
    assert!(report.flags.iter().any(|f| f.kind == FlagKind::Triangles));
}

#[test]
fn the_agent_method_accepts_hero_and_the_new_limits() {
    let glb = lod_glb(&[("Hull_LOD0", 1_000), ("Hull_LOD2", 900)], &[("Paint", false)]);
    let path = write_glb("hero_agent.glb", &glb);
    let (_, handler) = methods().remove(0);
    let mut app = new_world();
    let result = handler(
        app.world_mut(),
        &json!({ "path": path.to_str().expect("utf-8"), "class": "hero",
                 "limits": { "max_lod2_triangles": 500 } }),
    )
    .expect("report");
    assert_eq!(result["report"]["class"], "hero");
    assert_eq!(result["report"]["limits"]["max_lod2_triangles"], 500);
    assert_eq!(result["report"]["lods"][1]["level"], 2);
    assert!(
        result["report"]["flags"]
            .as_array()
            .expect("flags")
            .iter()
            .any(|f| f["kind"] == "lod_triangles")
    );
}
