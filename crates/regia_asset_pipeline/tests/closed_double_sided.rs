// SPDX-License-Identifier: MPL-2.0
//! The opt-in `drop_double_sided_on_closed` glTF cook hint end to end: a `doubleSided` material
//! used only by a closed opaque cube cooks single-sided with the hint on; one used by an open fin,
//! or by a closed mesh that is see-through, keeps the flag; without the hint nothing changes.

use base64::Engine;
use regia_asset::{AssetId, CookedStandardMaterial, CullMode};
use regia_asset_pipeline::closed_mesh::{MaterialSidedness, closed_mesh_report_path};
use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_path};

/// Unit cube with split face vertices, outward counter-clockwise faces.
fn cube(offset: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let (mut positions, mut indices) = (Vec::new(), Vec::new());
    for (n, u, v) in faces {
        let base = positions.len() as u32;
        for (a, b) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            positions.push([0, 1, 2].map(|k| offset + 0.5 * n[k] + 0.5 * a * u[k] + 0.5 * b * v[k]));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (positions, indices)
}

/// Meshes: closed cube (material 0), open fin (material 1), closed cube in a see-through
/// material (2, alpha blend). All three materials are `doubleSided`; material 3 is not.
fn write_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let (cube_p, cube_i) = cube(0.0);
    let fin_p: Vec<[f32; 3]> = vec![[0.0, 0.0, 3.0], [1.0, 0.0, 3.0], [1.0, 1.0, 3.0], [0.0, 1.0, 3.0]];
    let fin_i: Vec<u32> = vec![0, 1, 2, 0, 2, 3];
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut push = |positions: &[[f32; 3]], indices: &[u32]| -> (usize, usize) {
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        let p_off = bin.len();
        for p in positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
                bin.extend(p[k].to_le_bytes());
            }
        }
        views.push(format!(
            r#"{{"buffer":0,"byteOffset":{p_off},"byteLength":{}}}"#,
            positions.len() * 12
        ));
        accessors.push(format!(
            r#"{{"bufferView":{},"componentType":5126,"count":{},"type":"VEC3","min":[{},{},{}],"max":[{},{},{}]}}"#,
            views.len() - 1,
            positions.len(),
            lo[0],
            lo[1],
            lo[2],
            hi[0],
            hi[1],
            hi[2]
        ));
        let i_off = bin.len();
        for i in indices {
            bin.extend(i.to_le_bytes());
        }
        views.push(format!(
            r#"{{"buffer":0,"byteOffset":{i_off},"byteLength":{}}}"#,
            indices.len() * 4
        ));
        accessors.push(format!(
            r#"{{"bufferView":{},"componentType":5125,"count":{},"type":"SCALAR"}}"#,
            views.len() - 1,
            indices.len()
        ));
        (accessors.len() - 2, accessors.len() - 1)
    };
    let cube_a = push(&cube_p, &cube_i);
    let fin_a = push(&fin_p, &fin_i);
    let prim = |(p, i): (usize, usize), material: usize| {
        format!(r#"{{"attributes":{{"POSITION":{p}}},"indices":{i},"material":{material}}}"#)
    };
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0,1,2]}}],
        "nodes":[{{"mesh":0}},{{"mesh":1}},{{"mesh":2,"translation":[4,0,0]}}],
        "meshes":[{{"primitives":[{}]}},{{"primitives":[{}]}},{{"primitives":[{}]}}],
        "materials":[{{"name":"Hull","doubleSided":true}},{{"name":"Fin","doubleSided":true}},
                     {{"name":"Bubble","doubleSided":true,"alphaMode":"BLEND"}},{{"name":"Plain"}}],
        "buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,{}"}}],
        "bufferViews":[{}],"accessors":[{}]}}"#,
        prim(cube_a, 0),
        prim(fin_a, 1),
        prim(cube_a, 2),
        bin.len(),
        base64::engine::general_purpose::STANDARD.encode(&bin),
        views.join(","),
        accessors.join(",")
    );
    let path = dir.join("closed_double_sided.gltf");
    std::fs::write(&path, json).expect("write fixture");
    path
}

fn cooked_cull(path: &std::path::Path, settings: Option<&str>, name: &str) -> Option<CullMode> {
    let result = import_gltf_path(
        path,
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        settings,
        TextureCookQuality::Low,
    )
    .expect("import");
    let sub = result
        .sub_assets
        .iter()
        .find(|s| s.path == format!("material/{name}"))
        .unwrap_or_else(|| panic!("no material/{name}"));
    CookedStandardMaterial::from_postcard(&sub.payload)
        .expect("material")
        .authored
        .render_state
        .cull_mode
}

#[test]
fn closed_opaque_meshes_drop_double_sided_only_with_the_hint() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("closed_double_sided");
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let path = write_fixture(&dir);

    let report = closed_mesh_report_path(&path).expect("report");
    assert_eq!(report.meshes, 3);
    assert_eq!(report.closed_meshes, 2, "{report:?}");
    assert_eq!(report.closed_see_through, 1, "{report:?}");
    assert_eq!(report.meshes_double_sided, 3);
    assert_eq!(report.meshes_dropped, 1, "{report:?}");
    assert_eq!(
        report.materials,
        vec![
            MaterialSidedness::DoubleSidedClosed,
            MaterialSidedness::DoubleSidedVisible,
            MaterialSidedness::DoubleSidedVisible,
            MaterialSidedness::SingleSided,
        ]
    );

    // Off by default: every doubleSided material keeps cull none.
    for name in ["Hull", "Fin", "Bubble"] {
        assert_eq!(cooked_cull(&path, None, name), Some(CullMode::None), "{name}");
    }
    let hint = Some("(drop_double_sided_on_closed: true)");
    assert_eq!(
        cooked_cull(&path, hint, "Hull"),
        None,
        "the closed cube's material culls back faces"
    );
    assert_eq!(
        cooked_cull(&path, hint, "Fin"),
        Some(CullMode::None),
        "an open fin stays double-sided"
    );
    assert_eq!(
        cooked_cull(&path, hint, "Bubble"),
        Some(CullMode::None),
        "a see-through closed mesh shows its inside"
    );
    assert_eq!(cooked_cull(&path, hint, "Plain"), None);
}
