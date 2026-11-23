// SPDX-License-Identifier: MPL-2.0
//! C-06 read-only glTF lint acceptance, including hostile data and rig diagnostics.

#[path = "support/gltf_lint.rs"]
mod fixture;

use fixture::Fixture;
use regia_asset_pipeline::gltf_lint::{GltfLintReport, GltfLintSettings, lint_gltf};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

fn lint(fixture: &Fixture) -> GltfLintReport {
    let dir = unique_temp_dir("gltf-lint");
    let path = dir.join("model.glb");
    let bytes = fixture.glb();
    std::fs::write(&path, &bytes).expect("fixture");
    let report = lint_gltf(&path, &GltfLintSettings::default()).expect("lint");
    assert_eq!(
        std::fs::read(&path).expect("still exists"),
        bytes,
        "lint must not mutate source"
    );
    assert_eq!(
        report,
        lint_gltf(&path, &GltfLintSettings::default()).expect("repeat")
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
    report
}

#[test]
fn stretched_uv_coincident_pair_flagged() {
    let report = lint(&Fixture::stretched());
    assert_eq!(report.triangles, 2);
    assert_eq!(report.coincident_triangles.len(), 1);
    let row = &report.coincident_triangles[0];
    assert_eq!(
        (row.mesh, row.primitive, row.pair.keeper, row.pair.suspect),
        (0, 0, 0, 1)
    );
}

#[test]
fn duplicate_meshes_are_geometry_definitions_not_instances() {
    let mut f = Fixture::stretched();
    f.json["nodes"] = json!([{"mesh": 0}, {"mesh": 0, "translation": [2, 0, 0]}]);
    f.json["scenes"][0]["nodes"] = json!([0, 1]);
    assert!(lint(&f).duplicate_meshes.is_empty());
    let copy = f.json["meshes"][0].clone();
    f.json["meshes"].as_array_mut().expect("meshes").push(copy);
    let report = lint(&f);
    assert_eq!(report.duplicate_meshes.len(), 1);
    assert_eq!(
        (
            report.duplicate_meshes[0].original,
            report.duplicate_meshes[0].duplicate
        ),
        (0, 1)
    );
    // A separately packed mesh with the same geometry is still a duplicate.
    let mut positions = f.bin[..72].to_vec();
    for word in positions.as_chunks_mut::<4>().0 {
        let value = f32::from_le_bytes(*word);
        *word = (if value == 0.0 { -0.0 } else { value }).to_le_bytes();
    }
    let values = positions
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect::<Vec<_>>();
    let position = f.accessor(&values, 3);
    f.json["accessors"][position]["min"] = json!([0, 0, 0]);
    f.json["accessors"][position]["max"] = json!([1, 1, 0.00002]);
    f.json["meshes"][1]["primitives"][0]["attributes"]["POSITION"] = json!(position);
    assert_eq!(lint(&f).duplicate_meshes.len(), 1);
    let uv = f.accessor(&[0.0, 0.0, 3.0, 0.0, 0.0, 3.0, 0.0, 0.0, 3.0, 0.0, 0.0, 3.0], 2);
    f.json["meshes"][1]["primitives"][0]["attributes"]["TEXCOORD_0"] = json!(uv);
    assert_eq!(
        lint(&f).duplicate_meshes.len(),
        1,
        "different shading is advisory, still equal geometry"
    );
    let view = f.json["accessors"][position]["bufferView"]
        .as_u64()
        .expect("view") as usize;
    let offset = f.json["bufferViews"][view]["byteOffset"]
        .as_u64()
        .expect("offset") as usize;
    f.bin[offset..offset + 4].copy_from_slice(&0.5_f32.to_le_bytes());
    assert!(
        lint(&f).duplicate_meshes.is_empty(),
        "a geometry change is not an exact duplicate"
    );
}

fn animated(node: usize, times: &[f32]) -> Fixture {
    let mut f = Fixture::stretched();
    let input = f.accessor(times, 1);
    f.json["accessors"][input]["min"] = json!([times.iter().copied().reduce(f32::min).unwrap_or(0.0)]);
    f.json["accessors"][input]["max"] = json!([times.iter().copied().reduce(f32::max).unwrap_or(0.0)]);
    let output = f.accessor(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], 3);
    f.json["nodes"] = json!([{"mesh": 0}, {"name": "joint"}]);
    f.json["scenes"][0]["nodes"] = json!([0, 1]);
    f.json["skins"] = json!([{"joints": [1]}]);
    f.json["animations"] = json!([{"samplers": [{"input": input, "output": output}],
        "channels": [{"sampler": 0, "target": {"node": node, "path": "translation"}}]}]);
    f
}

#[test]
fn rig_channels_name_ignored_targets_and_duplicate_writes() {
    assert!(lint(&animated(1, &[0.0, 1.0])).rig_channels.is_empty());
    let mut f = animated(0, &[0.0, 1.0]);
    let copy = f.json["animations"][0]["channels"][0].clone();
    f.json["animations"][0]["channels"]
        .as_array_mut()
        .expect("channels")
        .push(copy);
    let report = lint(&f);
    let rows = report
        .rig_channels
        .iter()
        .map(|w| (w.channel, w.node, w.code.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            (0, 0, "gltf_rig_non_joint_channel"),
            (1, 0, "gltf_rig_duplicate_channel"),
            (1, 0, "gltf_rig_non_joint_channel")
        ]
    );
}

#[test]
fn rig_channels_validate_times_counts_and_finite_output() {
    let f = animated(1, &[0.0, 0.0]);
    assert_eq!(lint(&f).rig_channels[0].code, "gltf_rig_input_times");
    let mut f = animated(1, &[0.0, 1.0]);
    f.json["animations"][0]["samplers"][0]["interpolation"] = json!("CUBICSPLINE");
    assert_eq!(lint(&f).rig_channels[0].code, "gltf_rig_output_count");
    let len = f.bin.len();
    f.bin[len - 4..].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(
        lint(&f)
            .rig_channels
            .iter()
            .any(|w| w.code == "gltf_rig_nonfinite_output")
    );
}

#[test]
fn morph_channels_without_targets_warn() {
    let mut f = animated(0, &[0.0, 1.0]);
    let output = f.accessor(&[0.0, 1.0], 1);
    f.json["animations"][0]["samplers"][0]["output"] = json!(output);
    f.json["animations"][0]["channels"][0]["target"]["path"] = json!("weights");
    assert!(
        lint(&f)
            .rig_channels
            .iter()
            .any(|w| w.code == "gltf_rig_morph_target_missing")
    );
}

#[test]
fn unsupported_topology_is_explicit_and_never_a_partial_duplicate() {
    let mut f = Fixture::stretched();
    f.json["meshes"][0]["primitives"][0]["mode"] = json!(5);
    let copy = f.json["meshes"][0].clone();
    f.json["meshes"].as_array_mut().expect("meshes").push(copy);
    let report = lint(&f);
    assert_eq!(report.skipped_primitives.len(), 2);
    assert!(report.duplicate_meshes.is_empty());
    assert_eq!(report.triangles, 0);
}

#[test]
fn budgets_and_bad_geometry_fail_explicitly() {
    let dir = unique_temp_dir("gltf-lint-budget");
    let path = dir.join("model.glb");
    let mut f = Fixture::stretched();
    std::fs::write(&path, f.glb()).expect("write");
    for settings in [
        GltfLintSettings {
            max_triangles: 1,
            ..Default::default()
        },
        GltfLintSettings {
            max_vertices: 5,
            ..Default::default()
        },
    ] {
        assert_eq!(
            lint_gltf(&path, &settings).expect_err("budget").code(),
            "gltf_lint_limit"
        );
    }
    f.bin[..4].copy_from_slice(&f32::INFINITY.to_le_bytes());
    std::fs::write(&path, f.glb()).expect("write");
    assert_eq!(
        lint_gltf(&path, &Default::default()).expect_err("finite").code(),
        "gltf_lint_geometry"
    );
    std::fs::write(&path, b"glTF\x02\0\0\0\0\0\0\0").expect("hostile header");
    assert_eq!(
        lint_gltf(&path, &Default::default())
            .expect_err("bad file")
            .code(),
        "gltf_unreadable"
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn external_gltf_buffer_has_the_same_report() {
    let dir = unique_temp_dir("gltf-lint-external");
    let mut f = Fixture::stretched();
    let expected = lint(&f);
    f.json["buffers"][0]["uri"] = json!("mesh.bin");
    std::fs::write(dir.join("mesh.bin"), &f.bin).expect("bin");
    let path = dir.join("mesh.gltf");
    std::fs::write(&path, serde_json::to_vec(&f.json).expect("json")).expect("gltf");
    let actual = lint_gltf(&path, &Default::default()).expect("lint");
    // Diagnostics remain identical; newly reported IO telemetry records the
    // intentionally different source packaging (ADR-8226).
    assert_eq!(expected.input_usage.external_bytes, 0);
    assert_eq!(actual.input_usage.external_bytes, f.bin.len() as u64);
    assert_eq!(
        actual.input_usage.buffer_bytes_allocated,
        expected.input_usage.buffer_bytes_allocated
    );
    assert_eq!(
        actual,
        GltfLintReport {
            input_usage: actual.input_usage.clone(),
            ..expected
        }
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn indexed_triangles_validate_the_indices_and_match_reversed_winding() {
    let mut f = Fixture::stretched();
    let offset = f.bin.len();
    f.bin
        .extend([0_u32, 1, 2, 5, 4, 3].into_iter().flat_map(u32::to_le_bytes));
    let view = f.json["bufferViews"].as_array().expect("views").len();
    f.json["bufferViews"].as_array_mut().expect("views").push(json!({
        "buffer": 0, "byteOffset": offset, "byteLength": 24,
    }));
    let accessor = f.json["accessors"].as_array().expect("accessors").len();
    f.json["accessors"]
        .as_array_mut()
        .expect("accessors")
        .push(json!({
            "bufferView": view, "componentType": 5125, "count": 6, "type": "SCALAR",
        }));
    f.json["meshes"][0]["primitives"][0]["indices"] = json!(accessor);
    f.json["buffers"][0]["byteLength"] = json!(f.bin.len());
    assert_eq!(lint(&f).coincident_triangles.len(), 1);
    f.bin[offset..offset + 4].copy_from_slice(&999_u32.to_le_bytes());
    let dir = unique_temp_dir("gltf-lint-indices");
    let path = dir.join("model.glb");
    std::fs::write(&path, f.glb()).expect("write");
    assert_eq!(
        lint_gltf(&path, &Default::default())
            .expect_err("out of bounds")
            .code(),
        "gltf_lint_geometry"
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn rig_and_report_budgets_do_not_silently_truncate() {
    let dir = unique_temp_dir("gltf-lint-rig-budget");
    let path = dir.join("model.glb");
    let mut f = animated(0, &[0.0, 1.0]);
    let copy = f.json["animations"][0]["channels"][0].clone();
    f.json["animations"][0]["channels"]
        .as_array_mut()
        .expect("channels")
        .push(copy);
    std::fs::write(&path, f.glb()).expect("write");
    for settings in [
        GltfLintSettings {
            max_channels: 1,
            ..Default::default()
        },
        GltfLintSettings {
            max_animation_elements: 3,
            ..Default::default()
        },
        GltfLintSettings {
            max_findings: 1,
            ..Default::default()
        },
    ] {
        assert_eq!(
            lint_gltf(&path, &settings).expect_err("budget").code(),
            "gltf_lint_limit"
        );
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn valid_morph_and_cubic_channels_do_not_warn() {
    let mut f = animated(0, &[0.0, 1.0]);
    f.json["meshes"][0]["primitives"][0]["targets"] = json!([{"POSITION": 0}]);
    let output = f.accessor(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], 1);
    f.json["animations"][0]["samplers"][0]["output"] = json!(output);
    f.json["animations"][0]["samplers"][0]["interpolation"] = json!("CUBICSPLINE");
    f.json["animations"][0]["channels"][0]["target"]["path"] = json!("weights");
    assert!(lint(&f).rig_channels.is_empty());
}

#[test]
fn bad_uv_count_and_truncated_triangle_are_errors() {
    let dir = unique_temp_dir("gltf-lint-counts");
    let path = dir.join("model.glb");
    let mut f = Fixture::stretched();
    f.json["accessors"][1]["count"] = json!(5);
    std::fs::write(&path, f.glb()).expect("write");
    assert_eq!(
        lint_gltf(&path, &Default::default())
            .expect_err("UV count")
            .code(),
        "gltf_lint_geometry"
    );
    f.json["accessors"][0]["count"] = json!(5);
    std::fs::write(&path, f.glb()).expect("write");
    assert_eq!(
        lint_gltf(&path, &Default::default())
            .expect_err("triangle tail")
            .code(),
        "gltf_lint_geometry"
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}
