// SPDX-License-Identifier: MPL-2.0
//! The glTF data-layout extensions in the editor's own readers (ADR-3016): the merged model and the named
//! hierarchy read quantized, compressed and instanced files, and `asset.gltf_extensions` reports them
//! through the registered `AgentExtension` handler, as dispatch calls it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::prelude::ChildOf;
use regia_editor::gltf_hierarchy::{GltfSpawnOptions, spawn_gltf_hierarchy};
use regia_render::{CpuMaterial, DirectionalLight, MeshRenderer, PointLight, SpotLight};
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

fn glb(json: &Value, bin: &[u8]) -> Vec<u8> {
    let mut json = serde_json::to_vec(json).expect("json");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

/// A triangle in normalized shorts, compressed, drawn three times by a node at x = 10.
fn instanced_file() -> Vec<u8> {
    let positions: [[i16; 4]; 3] = [[0, 0, 0, 0], [32767, 0, 0, 0], [0, 32767, 0, 0]];
    let encoded = meshopt::encoding::encode_vertex_buffer(&positions).expect("encode");
    let mut bin = encoded.clone();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let translations_at = bin.len();
    for v in [0.0f32, 0.0, 0.0, 2.0, 0.0, 0.0, 4.0, 0.0, 0.0] {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_mesh_quantization", "EXT_meshopt_compression", "EXT_mesh_gpu_instancing", "VENDOR_extra"],
        "extensionsRequired": ["KHR_mesh_quantization", "EXT_meshopt_compression"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "Trees", "translation": [10, 0, 0],
                   "extensions": {"EXT_mesh_gpu_instancing": {"attributes": {"TRANSLATION": 1}}}}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
        "buffers": [
            {"byteLength": bin.len()},
            {"byteLength": 24, "extensions": {"EXT_meshopt_compression": {"fallback": true}}},
        ],
        "bufferViews": [
            {"buffer": 1, "byteLength": 24, "byteStride": 8, "extensions": {
                "EXT_meshopt_compression": {"buffer": 0, "byteLength": encoded.len(), "byteStride": 8,
                    "mode": "ATTRIBUTES", "count": 3}}},
            {"buffer": 0, "byteOffset": translations_at, "byteLength": 36},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5122, "normalized": true, "count": 3, "type": "VEC3",
             "min": [0, 0, 0], "max": [32767, 32767, 0]},
            {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"},
        ],
    });
    glb(&document, &bin)
}

fn write_fixture(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    let path = root.join("assets").join(name);
    std::fs::write(&path, bytes).expect("write");
    path
}

#[test]
fn the_merged_model_bakes_every_instance_of_a_compressed_quantized_file() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_merged");
    let path = write_fixture(&root, "trees.glb", &instanced_file());
    let material = Arc::new(CpuMaterial::default());
    let model = regia_editor::mesh_source::load_gltf_model(&path, &material).expect("loads");
    assert_eq!(model.triangles(), 3, "one triangle per instance");
    assert_eq!(model.vertices(), 9);
    // Node at x = 10, instances at 0, 2 and 4, a triangle one unit wide.
    assert!((model.aabb_min.x - 10.0).abs() < 1e-3, "{:?}", model.aabb_min);
    assert!((model.aabb_max.x - 15.0).abs() < 1e-3, "{:?}", model.aabb_max);
    assert!((model.aabb_max.y - 1.0).abs() < 1e-3, "{:?}", model.aabb_max);
}

#[test]
fn the_hierarchy_spawns_one_child_per_instance_sharing_the_mesh() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_hierarchy");
    let path = write_fixture(&root, "trees.glb", &instanced_file());
    let mut world = World::new();
    let spawn = spawn_gltf_hierarchy(&mut world, &path, &GltfSpawnOptions::default()).expect("spawns");
    assert_eq!(spawn.report.instances, 3);
    let node = spawn.entity("Trees").expect("the node");
    assert!(
        world.get::<MeshRenderer>(node).is_none(),
        "the node itself no longer draws"
    );
    let mut children = Vec::new();
    let mut query = world.query::<(Entity, &ChildOf, &Name, &Transform, &MeshRenderer)>();
    for (_, child_of, name, transform, renderer) in query.iter(&world) {
        if child_of.0 == node {
            children.push((
                name.as_str().to_string(),
                transform.translation.x,
                renderer.mesh.clone(),
            ));
        }
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(children.len(), 3);
    assert_eq!(children[1].0, "Trees#instance_1");
    assert_eq!(children[2].1, 4.0);
    assert!(
        Arc::ptr_eq(&children[0].2, &children[2].2),
        "the instances share one mesh, so the renderer batches them"
    );
}

/// Point, spot, directional and an unbounded point light on nodes, with no geometry.
fn lights_file() -> Vec<u8> {
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_lights_punctual"],
        "extensions": {"KHR_lights_punctual": {"lights": [
            {"type": "point", "name": "bulb", "color": [1.0, 0.5, 0.25], "intensity": 100.0, "range": 8.0},
            {"type": "spot", "intensity": 40.0, "range": 12.0,
             "spot": {"innerConeAngle": 0.25, "outerConeAngle": 0.5}},
            {"type": "directional", "intensity": 90000.0, "color": [1.0, 0.9, 0.8]},
            {"type": "point", "intensity": 100.0}
        ]}},
        "scene": 0, "scenes": [{"nodes": [0, 1, 2, 3, 4]}],
        "nodes": [
            {"name": "Lamp", "translation": [1, 2, 3], "extensions": {"KHR_lights_punctual": {"light": 0}}},
            {"name": "Cone", "extensions": {"KHR_lights_punctual": {"light": 1}}},
            // A quarter turn about X sends the node's -Z to world -Y.
            {"name": "Sun", "rotation": [-half, 0.0, 0.0, half],
             "extensions": {"KHR_lights_punctual": {"light": 2}}},
            {"name": "Open", "extensions": {"KHR_lights_punctual": {"light": 3}}},
            {"name": "Dark"}
        ],
    });
    serde_json::to_vec(&document).expect("json")
}

#[test]
fn light_nodes_carry_the_engine_light_in_engine_units() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_lights");
    let path = write_fixture(&root, "lights.gltf", &lights_file());
    let mut world = World::new();
    let spawn = spawn_gltf_hierarchy(&mut world, &path, &GltfSpawnOptions::default()).expect("spawns");
    assert_eq!(spawn.report.lights, 4);
    let entity = |name: &str| spawn.entity(name).expect("node");

    let lamp = world.get::<PointLight>(entity("Lamp")).expect("point");
    assert_eq!(lamp.color, [1.0, 0.5, 0.25]);
    // The reverse of what scene export writes: cd = lm / 4 pi.
    assert!((lamp.intensity / (4.0 * std::f32::consts::PI) - 100.0).abs() < 1e-3);
    assert_eq!(lamp.radius, 8.0);
    let placed = world.get::<Transform>(entity("Lamp")).expect("transform");
    assert_eq!(placed.translation, Vec3::new(1.0, 2.0, 3.0), "the node places it");

    let cone = world.get::<SpotLight>(entity("Cone")).expect("spot");
    assert_eq!(cone.intensity, 40.0, "both are candela");
    assert_eq!((cone.inner_angle, cone.outer_angle), (0.25, 0.5));
    assert_eq!(cone.range, 12.0);

    let sun = world.get::<DirectionalLight>(entity("Sun")).expect("directional");
    assert_eq!(sun.illuminance, 90000.0);
    assert_eq!(sun.color, [1.0, 0.9, 0.8]);
    let direction = Vec3::from_array(sun.direction);
    assert!(
        direction.distance(Vec3::NEG_Y) < 1e-5,
        "the node turns its -Z onto the light: {direction:?}"
    );

    // No `range`: 100 cd falls to one lux at 10 m by the inverse-square law.
    let open = world.get::<PointLight>(entity("Open")).expect("point");
    assert!((open.radius - 10.0).abs() < 1e-3, "{}", open.radius);

    assert!(world.get::<PointLight>(entity("Dark")).is_none());
}

#[test]
fn a_model_does_not_take_the_place_of_the_scene_sun() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_second_sun");
    let path = write_fixture(&root, "lights.gltf", &lights_file());
    let mut world = World::new();
    let scene_sun = world.spawn(DirectionalLight::default()).id();
    let before = world.get::<DirectionalLight>(scene_sun).cloned();
    let spawn = spawn_gltf_hierarchy(&mut world, &path, &GltfSpawnOptions::default()).expect("spawns");
    assert_eq!(spawn.report.lights, 3, "the local lights still join");
    assert!(
        world
            .get::<DirectionalLight>(spawn.entity("Sun").expect("node"))
            .is_none(),
        "the renderer draws one directional light"
    );
    assert!(
        spawn
            .report
            .warnings
            .iter()
            .any(|w| w.contains("`Sun`") && w.contains("directional")),
        "{:?}",
        spawn.report.warnings
    );
    assert_eq!(world.get::<DirectionalLight>(scene_sun).cloned(), before);
}

#[test]
fn the_hierarchy_can_leave_the_lights_out() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_no_lights");
    let path = write_fixture(&root, "lights.gltf", &lights_file());
    let mut world = World::new();
    let opts = GltfSpawnOptions {
        lights: false,
        ..GltfSpawnOptions::default()
    };
    let spawn = spawn_gltf_hierarchy(&mut world, &path, &opts).expect("spawns");
    assert_eq!(spawn.report.lights, 0);
    assert!(
        world
            .get::<PointLight>(spawn.entity("Lamp").expect("node"))
            .is_none()
    );
}

#[test]
fn the_agent_method_reports_lights() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_agent_lights");
    write_fixture(&root, "lights.gltf", &lights_file());
    let answer = call(&root, &json!({"path": "assets/lights.gltf"})).expect("report");
    assert_eq!(answer["lights"], 4);
    assert_eq!(answer["light_nodes"], 4);
    assert_eq!(answer["extensions"][0]["support"], "side_table");
}

/// `asset.gltf_extensions` through the handler dispatch uses.
fn call(root: &Path, params: &Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    regia_editor::agent_ext::install(&mut world);
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension.method("asset.gltf_extensions").expect("registered");
    handler(&mut world, params)
}

#[test]
fn the_agent_method_reports_a_file_and_its_fallbacks() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_agent");
    write_fixture(&root, "trees.glb", &instanced_file());
    let answer = call(&root, &json!({"path": "assets/trees.glb"})).expect("report");
    assert_eq!(answer["ok"], true);
    assert_eq!(answer["meshopt_views"], 1);
    assert_eq!(answer["quantized_accessors"], 1);
    assert_eq!(answer["instanced_nodes"], json!([[0, 3]]));
    assert_eq!(answer["fallbacks"], json!(["VENDOR_extra"]));
    let extensions = answer["extensions"].as_array().expect("list");
    let entry = |name: &str| {
        extensions
            .iter()
            .find(|e| e["name"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(
        entry("KHR_mesh_quantization"),
        json!({"name": "KHR_mesh_quantization", "required": true, "support": "cooked"})
    );
    assert_eq!(
        entry("VENDOR_extra"),
        json!({"name": "VENDOR_extra", "required": false, "support": null})
    );
}

#[test]
fn the_agent_method_names_the_extension_it_refuses() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_agent_refused");
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_draco_mesh_compression"],
        "extensionsRequired": ["KHR_draco_mesh_compression"],
    });
    write_fixture(&root, "draco.glb", &glb(&document, &[]));
    let error = call(&root, &json!({"path": "assets/draco.glb"})).expect_err("refused");
    let data = error.data.expect("data");
    assert_eq!(data["reason"], "gltf_ext_unsupported");
    assert_eq!(data["extension"], "KHR_draco_mesh_compression");
    assert!(
        error.message.contains("KHR_draco_mesh_compression"),
        "{}",
        error.message
    );
}

#[test]
fn the_agent_method_refuses_bad_parameters_and_paths() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_agent_params");
    let error = call(&root, &json!({})).expect_err("no path");
    assert_eq!(error.data.expect("data")["reason"], "gltf_ext_params");
    let error = call(&root, &json!({"path": "assets/none.glb"})).expect_err("no file");
    assert!(error.message.contains("none.glb"), "{}", error.message);
    let error = call(&root, &json!({"path": "../outside.glb"})).expect_err("outside");
    assert_eq!(error.data.expect("data")["reason"], "path_outside_project");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    std::fs::write(root.join("assets/broken.glb"), b"not a glb").expect("file");
    let error = call(&root, &json!({"path": "assets/broken.glb"})).expect_err("broken");
    assert_eq!(error.data.expect("data")["reason"], "gltf_unreadable");
}

#[test]
fn the_importer_fuzz_targets_cover_the_extensions() {
    let targets = regia_editor::import_fuzz::targets();
    for name in ["import.gltf_ext", "import.meshopt"] {
        let target = targets.iter().find(|t| t.name == name).expect(name);
        let report = regia_agent::fuzz_targets::run(target, None, 5, 200).expect("built-in fixture");
        assert!(report.is_clean(), "{name}: {report:?}");
        assert!(report.accepted > 0, "{name}: nothing loaded: {report:?}");
    }
}
