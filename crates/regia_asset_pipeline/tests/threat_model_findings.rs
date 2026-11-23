// SPDX-License-Identifier: MPL-2.0
//! Open findings of the threat-model review in code this lane does not own (ADR-3206). Each test
//! fails today and passes once the finding is fixed; the number names it in the lane report.

use regia_test_utils::unique_temp_dir;

/// A glTF with one triangle whose buffer lies at `uri`.
fn gltf_with_buffer(uri: &str, len: usize) -> String {
    format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
        "nodes":[{{"name":"sim","mesh":0}}],
        "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
        "accessors":[
          {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
          {{"bufferView":1,"componentType":5123,"count":3,"type":"SCALAR"}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":6}}],
        "buffers":[{{"uri":"{uri}","byteLength":{len}}}]}}"#
    )
}

/// Finding 19: the cloth cook hands a project's glTF to `gltf::import_buffers` with no URI check and
/// joins `sim_mesh.source` unconfined, where every other import refuses a URI that leaves the
/// project. A `.cloth.ron` can pull any file of the cook host into cooked output as positions.
#[test]
#[ignore = "finding 19: open, routed by the lead (regia_asset_pipeline::cloth_cook)"]
fn finding_19_the_cloth_cook_refuses_a_buffer_outside_the_project() {
    let base = unique_temp_dir("threat-cloth");
    let project = base.join("project");
    std::fs::create_dir_all(&project).expect("project");
    // 3 positions then 3 u16 indices: the "secret" decodes as a valid triangle.
    let mut secret: Vec<u8> = Vec::new();
    for v in [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for c in v {
            secret.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in [0_u16, 1, 2] {
        secret.extend_from_slice(&i.to_le_bytes());
    }
    std::fs::write(base.join("outside.bin"), &secret).expect("outside file");
    std::fs::write(
        project.join("sim.gltf"),
        gltf_with_buffer("../outside.bin", secret.len()),
    )
    .expect("gltf");
    let cloth = project.join("a.cloth.ron");
    std::fs::write(
        &cloth,
        r#"(format_version: 1, name: "c", sim_mesh: (source: "sim.gltf", node: "sim"))"#,
    )
    .expect("cloth");
    let result = regia_asset_pipeline::cloth_cook::cook_cloth(&cloth);
    let message = match &result {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    };
    // Whatever else a real cook needs, the buffer outside the project must not be what stopped or
    // fed it: only a refusal that names the URI counts.
    assert!(
        message.to_ascii_lowercase().contains("uri") || message.contains("outside"),
        "the buffer outside the project was read: {message:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Finding 20: `.tmx` has no cell cap (the `.tmj` reader has `MAX_TMJ_CELLS`), and the capacity is an
/// unchecked `u32` product: this map panics on overflow in a debug build and wraps in a release one.
#[test]
#[ignore = "finding 20: open, routed by the lead (regia_asset_pipeline::tiled_import)"]
fn finding_20_a_tmx_claiming_too_many_tiles_is_refused() {
    let dir = unique_temp_dir("threat-tmx");
    let map = dir.join("big.tmx");
    std::fs::write(
        &map,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.9" orientation="orthogonal" renderorder="right-down" width="65536" height="65536" tilewidth="8" tileheight="8" infinite="0">
 <layer id="1" name="ground" width="65536" height="65536"><data encoding="csv">0</data></layer>
</map>"#,
    )
    .expect("map");
    let result = std::panic::catch_unwind(|| regia_asset_pipeline::tiled_import::import_tiled(&map).is_err());
    assert_eq!(
        result.ok(),
        Some(true),
        "the map must be an error, not a panic or a success"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 25: a `command` generator inherits the whole environment of whoever runs
/// `regia gen run|verify` (a CI step holds its secrets there); a `[[tool]]` starts with an empty one.
#[cfg(unix)]
#[test]
#[ignore = "finding 25: open, routed by the lead (regia_asset_pipeline::generators)"]
fn finding_25_a_generator_does_not_inherit_the_callers_secrets() {
    let project = unique_temp_dir("threat-gen");
    let gens = project.join("generators");
    std::fs::create_dir_all(&gens).expect("generators");
    std::fs::write(project.join("Regia.toml"), "").expect("manifest");
    std::fs::write(
        gens.join("leak.toml"),
        r#"format_version = 1
[generator]
kind = "command"
seed = 1
command = ['/bin/sh', '-c', 'mkdir -p "$REGIA_GEN_OUT/assets/gen/leak"; env > "$REGIA_GEN_OUT/assets/gen/leak/env.txt"']
outputs = ["assets/gen/leak/"]
"#,
    )
    .expect("generator");
    regia_asset_pipeline::generators::run(&project, "leak").expect("the generator runs");
    let seen = std::fs::read_to_string(project.join("assets/gen/leak/env.txt")).expect("its output");
    assert!(
        seen.contains("REGIA_GEN_SEED="),
        "the generator's own variables: {seen}"
    );
    // `cargo test` sets this for the test process: a stand-in for a CI step's secret.
    assert!(
        !seen.contains("CARGO_MANIFEST_DIR="),
        "the generator saw the caller's environment"
    );
    let _ = std::fs::remove_dir_all(&project);
}
