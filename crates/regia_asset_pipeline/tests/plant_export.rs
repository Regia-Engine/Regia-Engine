// SPDX-License-Identifier: MPL-2.0
//! Spec 68 § 10 test 9: a grown plant exported by `plant_export::write_job` loads through the glTF
//! import with the tree conventions: the requested LOD count, decreasing triangle counts, a
//! billboard part on the last LOD and both wind attributes on every vertex.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_asset::AssetId;
use regia_asset_pipeline::plant_export::{glb_bytes, write_job_to};
use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_glb_bytes};
use regia_procgen::exec::StaticSnapshot;
use regia_procgen::plant::{ExportJob, starter};
use regia_procgen::{CancelToken, Evaluator, ProcGraph, ProcNodeRegistry, RunInputs, SettingValue};

fn broadleaf_job(seed: u32) -> ExportJob {
    let text = starter("starter/plant_broadleaf.pgraph.ron").expect("starter ships");
    let g = ProcGraph::from_ron(text).expect("parses");
    let overrides = BTreeMap::from([("growth_steps".to_string(), SettingValue::Int(7))]);
    let c = g
        .compile_with(&ProcNodeRegistry::with_builtins(), &overrides)
        .expect("compiles");
    let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), seed);
    let r = Evaluator::new()
        .run(&c, &inputs, &CancelToken::new())
        .expect("runs");
    r.exports().first().cloned().expect("one export job")
}

#[test]
fn t09_export_loads_with_lods_billboard_and_wind() {
    let job = broadleaf_job(7);
    let dir = regia_test_utils::unique_temp_dir("plant_export");
    let path = dir.join("plant_broadleaf.glb");
    let report = write_job_to(&job, &path).expect("writes");
    let bytes = std::fs::read(&path).expect("reads back");
    assert_eq!(report.bytes, bytes.len());
    assert_eq!(report.triangles_per_lod.len(), 4);
    assert!(
        report.triangles_per_lod.windows(2).all(|w| w[1] < w[0]),
        "decreasing: {:?}",
        report.triangles_per_lod
    );

    // The importer groups the `_LOD<n>` nodes.
    let imported = import_gltf_glb_bytes(
        &bytes,
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
    )
    .expect("imports");
    let group = imported
        .lod_groups
        .iter()
        .find(|g| g.name == "plant_broadleaf")
        .expect("an LOD group named after the plant");
    // A level with several primitives (bark and leaves; the billboard on the last) is listed
    // once per primitive, so the requested count is the distinct levels.
    let levels: std::collections::BTreeSet<u32> = group.levels.iter().map(|l| l.level).collect();
    assert_eq!(
        levels.into_iter().collect::<Vec<_>>(),
        [0, 1, 2, 3],
        "requested LOD count"
    );

    // The JSON chunk records the wind layout on every LOD mesh.
    let json_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let json = String::from_utf8_lossy(&bytes[20..20 + json_len]);
    assert_eq!(
        json.matches("\"regia_wind_layout\":1").count(),
        4,
        "regia_wind_layout on each LOD"
    );

    // Wind attributes on every vertex; the billboard part on the last LOD (read from the JSON).
    gltf::Gltf::from_slice(&bytes).expect("valid glTF");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("JSON chunk");
    let accessors = doc["accessors"].as_array().expect("accessors");
    let materials = doc["materials"].as_array().expect("materials");
    let count = |a: &serde_json::Value| {
        a.as_u64()
            .and_then(|i| accessors.get(i as usize))
            .and_then(|x| x["count"].as_u64())
    };
    let mut lods = 0;
    for mesh in doc["meshes"].as_array().expect("meshes") {
        let lod = mesh["name"].as_str().unwrap_or_default().to_string();
        if !lod.contains("_LOD") {
            continue;
        }
        lods += 1;
        let mut has_billboard = false;
        for prim in mesh["primitives"].as_array().expect("primitives") {
            let attrs = &prim["attributes"];
            let n = count(&attrs["POSITION"]);
            assert!(n.is_some_and(|n| n > 0));
            assert_eq!(count(&attrs["_WIND"]), n, "{lod}: _WIND on every vertex");
            assert_eq!(
                count(&attrs["_WIND_PIVOT"]),
                n,
                "{lod}: _WIND_PIVOT on every vertex"
            );
            let mat = prim["material"]
                .as_u64()
                .and_then(|m| materials.get(m as usize))
                .and_then(|m| m["name"].as_str())
                .unwrap_or_default();
            has_billboard |= mat.ends_with("_billboard");
        }
        assert_eq!(
            has_billboard,
            lod.ends_with("_LOD3"),
            "{lod}: billboard only on the last LOD"
        );
    }
    assert_eq!(lods, 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn same_graph_and_seed_give_the_same_bytes() {
    let a = glb_bytes(&broadleaf_job(7)).expect("writes").0;
    let b = glb_bytes(&broadleaf_job(7)).expect("writes").0;
    assert_eq!(a, b, "byte-stable export");
    let c = glb_bytes(&broadleaf_job(8)).expect("writes").0;
    assert_ne!(a, c, "another seed, another plant");
}

/// The billboard part is the impostor bake (spec 68 § 6.3, ADR-2731): two quads that read side captures
/// of an embedded atlas, with coverage where the plant is and none in the corners.
#[test]
fn the_billboard_reads_the_baked_impostor_atlas() {
    let job = broadleaf_job(7);
    let (bytes, tris) = glb_bytes(&job).expect("writes");
    let (again, _) = glb_bytes(&job).expect("writes");
    assert!(bytes == again, "the bake is byte-stable");
    assert_eq!(*tris.last().expect("LODs"), tris[3]);
    let (doc, buffers, images) = gltf::import_slice(&bytes).expect("valid glTF");
    assert_eq!(images.len(), 2, "albedo and normal-depth atlases");
    let (atlas, grid, cell_px, radius, centre_y) = {
        let node = doc
            .nodes()
            .find(|n| n.name().is_some_and(|x| x.ends_with("_LOD3")))
            .expect("last LOD");
        let extras: serde_json::Value =
            serde_json::from_str(node.extras().as_ref().expect("extras").get()).expect("JSON");
        let imp = &extras["regia_impostor"];
        (
            &images[imp["albedo_image"].as_u64().expect("image") as usize],
            imp["grid"].as_u64().expect("grid") as u32,
            imp["cell"].as_u64().expect("cell") as u32,
            imp["radius"].as_f64().expect("radius") as f32,
            imp["centre_y"].as_f64().expect("centre") as f32,
        )
    };
    assert_eq!(atlas.format, gltf::image::Format::R8G8B8A8);
    let side = atlas.width as usize;
    assert_eq!(side, (grid * cell_px) as usize);
    let leaves: Vec<[f32; 3]> = doc
        .meshes()
        .find(|m| m.name().is_some_and(|n| n.ends_with("_LOD0")))
        .expect("LOD 0")
        .primitives()
        .flat_map(|p| {
            let reader = p.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            reader.read_positions().expect("positions").collect::<Vec<_>>()
        })
        .collect();
    let prim = doc
        .meshes()
        .flat_map(|m| m.primitives().collect::<Vec<_>>())
        .find(|p| p.material().name().is_some_and(|m| m.ends_with("_billboard")))
        .expect("billboard primitive");
    assert_eq!(prim.material().alpha_mode(), gltf::material::AlphaMode::Blend);
    assert!(
        prim.material()
            .pbr_metallic_roughness()
            .base_color_texture()
            .is_some()
    );
    let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
    let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uvs").into_f32().collect();
    assert_eq!(positions.len(), 8);
    for quad in [0..4, 4..8] {
        // Each quad spans the framed sphere and faces nearly its axis.
        let center: [f32; 3] =
            std::array::from_fn(|k| positions[quad.clone()].iter().map(|p| p[k]).sum::<f32>() / 4.0);
        assert!(center[0].abs() < 1e-3 && center[2].abs() < 1e-3 && (center[1] - centre_y).abs() < 1e-3);
        for p in &positions[quad.clone()] {
            let d: f32 = (0..3).map(|k| (p[k] - center[k]).powi(2)).sum::<f32>().sqrt();
            assert!(
                (d - radius * std::f32::consts::SQRT_2).abs() < radius * 1e-3,
                "{p:?}"
            );
        }
        // The quad's UVs fill one capture's cell of the atlas.
        let (lo, hi) = uvs[quad.clone()]
            .iter()
            .fold(([1.0f32; 2], [0.0f32; 2]), |(lo, hi), uv| {
                (
                    [lo[0].min(uv[0]), lo[1].min(uv[1])],
                    [hi[0].max(uv[0]), hi[1].max(uv[1])],
                )
            });
        let cell = 1.0 / grid as f32;
        assert!((hi[0] - lo[0] - cell).abs() < 1e-5 && (hi[1] - lo[1] - cell).abs() < 1e-5);
        assert!(lo[0] >= 0.0 && lo[1] >= 0.0 && hi[0] <= 1.0 && hi[1] <= 1.0);
        // Coverage: the capture holds the plant, and its top corners are clear.
        let alpha = |u: f32, v: f32| {
            let x = ((u.max(0.0) * side as f32) as usize).min(side - 1);
            let y = ((v.max(0.0) * side as f32) as usize).min(side - 1);
            atlas.pixels[(y * side + x) * 4 + 3]
        };
        let (mut covered, mut samples) = (0, 0);
        for j in 0..32 {
            for i in 0..32 {
                let (u, v) = (
                    lo[0] + (i as f32 + 0.5) * cell / 32.0,
                    lo[1] + (j as f32 + 0.5) * cell / 32.0,
                );
                samples += 1;
                covered += usize::from(alpha(u, v) > 0);
            }
        }
        assert!(
            covered > 8 && covered * 2 < samples,
            "coverage {covered} of {samples}"
        );
        // The plant's own vertices, projected through the quad's frame, land on covered texels; with
        // the frame mirrored fewer do. The plant is asymmetric, so a flipped mapping cannot pass.
        let q = &positions[quad.clone()];
        let t = &uvs[quad];
        let axis = |a: [f32; 3], b: [f32; 3]| -> [f32; 3] { std::array::from_fn(|k| b[k] - a[k]) };
        let dot = |a: [f32; 3], b: [f32; 3]| (0..3).map(|k| a[k] * b[k]).sum::<f32>();
        let (ex, ey) = (axis(q[0], q[1]), axis(q[0], q[3]));
        let uv3 = |v: [f32; 2]| [v[0], v[1], 0.0];
        let (dx, dy) = (axis(uv3(t[0]), uv3(t[1])), axis(uv3(t[0]), uv3(t[3])));
        let hits = |mirror: bool| {
            leaves
                .iter()
                .filter(|p| {
                    let d = axis(q[0], **p);
                    let mut fx = dot(d, ex) / dot(ex, ex);
                    let fy = dot(d, ey) / dot(ey, ey);
                    if mirror {
                        fx = 1.0 - fx;
                    }
                    let (u, v) = (
                        t[0][0] + dx[0] * fx + dy[0] * fy,
                        t[0][1] + dx[1] * fx + dy[1] * fy,
                    );
                    // A vertex is on a triangle's edge: the texel beside it may hold the coverage.
                    let texel = 1.0 / side as f32;
                    (-1..=1).any(|i| (-1..=1).any(|j| alpha(u + i as f32 * texel, v + j as f32 * texel) > 0))
                })
                .count()
        };
        let (straight, mirrored) = (hits(false), hits(true));
        assert!(
            straight * 10 > leaves.len() * 7 && straight > mirrored,
            "{straight} of {} vertices on covered texels, {mirrored} mirrored",
            leaves.len()
        );
    }
    // Colors come from the plant's slots, not the atlas clear color alone.
    assert!(
        atlas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[3] > 127 && p[1] > p[2]),
        "tinted texels"
    );
}

/// Audit finding 3: a non-finite `lod_screen_size` used to be written as `null` node extras by
/// serde_json 1.0.151 — a silently invalid glTF, not a panic. It is now an export error.
#[test]
fn a_non_finite_lod_screen_size_is_an_export_error_not_a_panic() {
    for bad in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
        let mut job = broadleaf_job(7);
        job.settings.lod_screen_size = vec![bad];
        let err = glb_bytes(&job).expect_err("a non-finite lod_screen_size is refused");
        assert_eq!(err.code(), "export_invalid", "{err}");
    }
}

#[test]
fn no_billboard_means_no_atlas() {
    let mut job = broadleaf_job(7);
    job.settings.billboard = false;
    let (bytes, _) = glb_bytes(&job).expect("writes");
    let (doc, _, images) = gltf::import_slice(&bytes).expect("valid glTF");
    assert!(images.is_empty());
    assert!(
        !doc.materials()
            .any(|m| m.name().is_some_and(|n| n.ends_with("_billboard")))
    );
}

/// `host_load_mesh`: what an envelope reads back out of a file, transforms applied.
#[test]
fn an_exported_glb_loads_back_as_triangles() {
    let job = broadleaf_job(7);
    let dir = regia_test_utils::unique_temp_dir("plant_export_mesh");
    let path = dir.join("p.glb");
    write_job_to(&job, &path).expect("writes");
    let mesh = regia_asset_pipeline::plant_export::host_load_mesh(&path, "p.glb").expect("loads");
    assert_eq!(mesh.asset, "p.glb");
    assert!(mesh.indices.len() >= 3 && mesh.indices.len().is_multiple_of(3));
    assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.positions.len()));
    assert!(regia_asset_pipeline::plant_export::host_load_mesh(&dir.join("none.glb"), "x").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// An exported plant reads back as its LOD 0 only: the lower LODs overlap it.
#[test]
fn an_exported_glb_reads_back_its_lod0_only() {
    let job = broadleaf_job(7);
    let dir = regia_test_utils::unique_temp_dir("plant_export_lod0");
    let path = dir.join("p.glb");
    let report = write_job_to(&job, &path).expect("writes");
    assert!(report.triangles_per_lod.len() > 1);
    let mesh = regia_asset_pipeline::plant_export::host_load_mesh(&path, "p.glb").expect("loads");
    assert_eq!(mesh.indices.len(), 3 * report.triangles_per_lod[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal `.gltf` plus `.bin`: one triangle (3 positions, 3 `u32` indices) on each named node.
fn tiny_gltf(dir: &std::path::Path, indices: [u32; 3], nodes: &[&str]) -> std::path::PathBuf {
    let mut bin = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for c in p {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    std::fs::write(dir.join("t.bin"), &bin).expect("bin");
    let node_list: Vec<String> = nodes
        .iter()
        .map(|n| format!("{{\"name\":\"{n}\",\"mesh\":0}}"))
        .collect();
    let scene: Vec<String> = (0..nodes.len()).map(|i| i.to_string()).collect();
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[{}]}}],"nodes":[{}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
"buffers":[{{"uri":"t.bin","byteLength":48}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":12}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
{{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}]}}"#,
        scene.join(","),
        node_list.join(",")
    );
    let path = dir.join("t.gltf");
    std::fs::write(&path, json).expect("gltf");
    path
}

/// Node names: `_LOD0` and plain names load, `_LOD<n>` with n ≥ 1 is skipped, look-alikes load.
#[test]
fn lower_lod_nodes_are_skipped_by_name() {
    let dir = regia_test_utils::unique_temp_dir("plant_export_lod_names");
    let load = |nodes: &[&str]| {
        let path = tiny_gltf(&dir, [0, 1, 2], nodes);
        regia_asset_pipeline::plant_export::host_load_mesh(&path, "t.gltf")
            .expect("loads")
            .indices
            .len()
            / 3
    };
    assert_eq!(load(&["box"]), 1);
    assert_eq!(load(&["box_LOD0", "box_LOD1", "box_LOD12"]), 1);
    assert_eq!(load(&["box_LOD00", "box_LOD", "box_LODx", "box_lod1"]), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An index past its primitive's vertices is refused, never wrapped onto another primitive.
#[test]
fn an_index_past_its_vertices_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("plant_export_bad_index");
    for bad in [3, u32::MAX] {
        let path = tiny_gltf(&dir, [0, 1, bad], &["a", "b"]);
        let e = regia_asset_pipeline::plant_export::host_load_mesh(&path, "t.gltf").expect_err("refused");
        assert!(e.contains("past its 3 vertices"), "{e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tiny_gltf`'s buffer with `nodes_json` as the node list, `uri` as the buffer URI, `positions`
/// as the POSITION accessor and `index_count` indices in the index accessor (the buffer holds 3).
fn custom_gltf(
    dir: &std::path::Path,
    nodes_json: &str,
    uri: &str,
    positions: u32,
    index_count: u32,
) -> std::path::PathBuf {
    let path = tiny_gltf(dir, [0, 1, 2], &["seed"]);
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":{nodes_json},
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":{positions}}},"indices":1}}]}}],
"buffers":[{{"uri":"{uri}","byteLength":48}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":12}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
{{"bufferView":1,"componentType":5125,"count":{index_count},"type":"SCALAR"}}]}}"#
    );
    std::fs::write(&path, json).expect("gltf");
    path
}

/// A node that is its own child (or a deep chain) is an error, not a stack overflow that aborts
/// the procgen host (`regia procgen`, the editor, the agent): the checked open refuses a node
/// graph that is not a forest (ADR-5869), and the walk stops a legitimate but too-deep chain at
/// the importer's depth limit. Before the fix the self-cycle recursed until the stack overflowed.
#[test]
fn a_node_cycle_is_refused_not_a_stack_overflow() {
    let dir = regia_test_utils::unique_temp_dir("plant_export_node_cycle");
    let cycle = custom_gltf(&dir, r#"[{"mesh":0,"children":[0]}]"#, "t.bin", 0, 3);
    let e = regia_asset_pipeline::plant_export::host_load_mesh(&cycle, "t.gltf").expect_err("cycle refused");
    assert!(e.contains("own child"), "{e}");
    // Two nodes that are each other's child.
    let pair = custom_gltf(
        &dir,
        r#"[{"mesh":0,"children":[1]},{"mesh":0,"children":[0]}]"#,
        "t.bin",
        0,
        3,
    );
    let e = regia_asset_pipeline::plant_export::host_load_mesh(&pair, "t.gltf").expect_err("cycle refused");
    assert!(e.contains("own ancestor"), "{e}");
    // A tree chain deeper than the importer's depth limit (a forest, so past the checked open)
    // stops at the walk's guard.
    let deep: Vec<String> = (0..300)
        .map(|i| {
            if i < 299 {
                format!(r#"{{"mesh":0,"children":[{}]}}"#, i + 1)
            } else {
                r#"{"mesh":0}"#.to_string()
            }
        })
        .collect();
    let deep = custom_gltf(&dir, &format!("[{}]", deep.join(",")), "t.bin", 0, 3);
    let e = regia_asset_pipeline::plant_export::host_load_mesh(&deep, "t.gltf").expect_err("too deep");
    assert!(e.contains("nested deeper than"), "{e}");
    // A legitimate chain of 8 still loads every node's triangle.
    let chain: Vec<String> = (0..8)
        .map(|i| {
            if i < 7 {
                format!(r#"{{"mesh":0,"children":[{}]}}"#, i + 1)
            } else {
                r#"{"mesh":0}"#.to_string()
            }
        })
        .collect();
    let chain = custom_gltf(&dir, &format!("[{}]", chain.join(",")), "t.bin", 0, 3);
    let mesh = regia_asset_pipeline::plant_export::host_load_mesh(&chain, "t.gltf").expect("chain loads");
    assert_eq!(mesh.indices.len(), 8 * 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Files the raw `gltf::import` panics on (a POSITION accessor that does not exist, a buffer URI
/// that percent-decodes to invalid UTF-8) and a GLB header shorter than itself are errors: the
/// envelope loader opens through the importer's checks.
#[test]
fn hostile_envelope_files_are_errors_not_panics() {
    let dir = regia_test_utils::unique_temp_dir("plant_export_hostile");
    let node = r#"[{"mesh":0}]"#;
    for (uri, positions) in [("t.bin", 5), ("%FF.bin", 0)] {
        let path = custom_gltf(&dir, node, uri, positions, 3);
        let result =
            std::panic::catch_unwind(|| regia_asset_pipeline::plant_export::host_load_mesh(&path, "t.gltf"));
        assert!(
            matches!(result, Ok(Err(_))),
            "uri {uri} POSITION {positions}: {:?}",
            result.as_ref().map(|r| r.as_ref().map(|m| m.indices.len()))
        );
    }
    // A GLB whose header declares a length under its own 12 bytes.
    let glb = dir.join("short.glb");
    let mut bytes = b"glTF".to_vec();
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&4u32.to_le_bytes());
    std::fs::write(&glb, &bytes).expect("glb");
    let result =
        std::panic::catch_unwind(|| regia_asset_pipeline::plant_export::host_load_mesh(&glb, "s.glb"));
    assert!(matches!(result, Ok(Err(_))), "short GLB header");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A triangle primitive whose index count is not a multiple of 3 is refused: appended as is, it
/// would shift every later triangle of the envelope onto the wrong vertices.
#[test]
fn a_partial_triangle_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("plant_export_partial_triangle");
    let path = custom_gltf(&dir, r#"[{"mesh":0}]"#, "t.bin", 0, 2);
    let e = regia_asset_pipeline::plant_export::host_load_mesh(&path, "t.gltf").expect_err("refused");
    assert!(e.contains("not a whole number of triangles"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 68 § 6.3 extras and collision (lane spec-28-68): the root node carries `shadow_lod`, each
/// `_LOD<n>` node its `lod_screen_size`, and `collision: capsules` writes one capsule per trunk and
/// order-1 branch thicker than 5 cm (and none for thinner ones).
#[test]
fn shadow_lod_screen_sizes_and_capsules_follow_the_settings() {
    use regia_procgen::plant::ExportCollision;

    let mut job = broadleaf_job(7);
    job.settings.shadow_lod = 2;
    job.settings.lod_screen_size = vec![0.9, 0.4, 0.15, 0.03];
    job.settings.collision = ExportCollision::Capsules;
    // Thicken the skeleton so several axes pass the 5 cm rule (capsules read the skeleton only).
    {
        let mesh = Arc::make_mut(&mut job.mesh);
        let sk = Arc::make_mut(&mut mesh.skeleton);
        for p in &mut sk.plants {
            for n in &mut p.nodes {
                n.radius *= 10.0;
            }
        }
    }
    let expected: usize = job
        .mesh
        .skeleton
        .plants
        .iter()
        .map(|p| {
            p.axes()
                .iter()
                .filter(|a| {
                    let first = &p.nodes[a.nodes[0]];
                    first.order <= 1 && first.radius > 0.05
                })
                .count()
        })
        .sum();
    assert!(expected >= 2, "the thickened tree has {expected} thick axes");
    let bytes = glb_bytes(&job).expect("writes").0;
    let json_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let doc: serde_json::Value = serde_json::from_slice(&bytes[20..20 + json_len]).expect("JSON chunk");
    let nodes = doc["nodes"].as_array().expect("nodes");
    let root = nodes
        .iter()
        .find(|n| n["name"] == "plant_broadleaf")
        .expect("root node named after the plant");
    assert_eq!(root["extras"]["shadow_lod"], 2, "shadow_lod on the root");
    for (k, want) in [0.9f64, 0.4, 0.15, 0.03].into_iter().enumerate() {
        let lod = nodes
            .iter()
            .find(|n| n["name"] == format!("plant_broadleaf_LOD{k}").as_str())
            .expect("LOD node");
        let got = lod["extras"]["lod_screen_size"]
            .as_f64()
            .expect("lod_screen_size");
        assert!((got - want).abs() < 1e-6, "LOD{k} screen size {got} vs {want}");
    }
    let capsules: Vec<&serde_json::Value> = nodes
        .iter()
        .filter(|n| n["extras"]["regia_capsule"].is_array())
        .collect();
    assert_eq!(
        capsules.len(),
        expected,
        "one capsule per thick trunk or order-1 axis"
    );
    for c in capsules {
        let r = c["extras"]["regia_capsule"][6].as_f64().expect("radius");
        assert!(r > 0.05, "capsule radius {r} over 5 cm");
    }
    // Without the setting, no capsule.
    job.settings.collision = ExportCollision::None;
    let bytes = glb_bytes(&job).expect("writes").0;
    let json_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let json = String::from_utf8_lossy(&bytes[20..20 + json_len]);
    assert!(
        !json.contains("regia_capsule"),
        "collision: none writes no capsule"
    );
}

/// Spec 68 § 11 budgets, hand-run (`cargo test -p regia_asset_pipeline --release --test plant_export
/// -- --ignored --nocapture plant_budget_hand_run`): prints the grow, mesh and leaves node times of
/// the run report and the export time of each starter at 12 growth steps. It asserts no times (the
/// gate never runs it; `docs/06-testing-and-ci.md` § 9 hand-run benchmarks).
#[test]
#[ignore = "hand-run timing; prints numbers only"]
fn plant_budget_hand_run() {
    for name in ["plant_broadleaf", "plant_conifer", "plant_bush"] {
        let text = starter(&format!("starter/{name}.pgraph.ron")).expect("starter ships");
        let g = ProcGraph::from_ron(text).expect("parses");
        let c = g.compile(&ProcNodeRegistry::with_builtins()).expect("compiles");
        let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 7);
        let r = Evaluator::new()
            .run(&c, &inputs, &CancelToken::new())
            .expect("runs");
        let ms = |ty: &str| -> f64 { r.report.nodes.iter().filter(|n| n.ty == ty).map(|n| n.ms).sum() };
        let job = r.exports().first().cloned().expect("one export job");
        let nodes = job.mesh.skeleton.node_count();
        let t = std::time::Instant::now();
        let (bytes, tris) = glb_bytes(&job).expect("writes");
        let export_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{name}: {nodes} nodes, grow {:.1} ms, mesh {:.1} ms, leaves {:.1} ms, export {export_ms:.1} ms \
             ({} LODs {tris:?} triangles, {} bytes)",
            ms("plant.grow"),
            ms("plant.mesh"),
            ms("plant.leaves"),
            tris.len(),
            bytes.len()
        );
    }
}

/// sec-review-assets-agent (ADR-5869): `host_load_mesh` (reached by `plant.envelope_mesh` from the
/// agent, the editor and the CLI) parsed with a plain `gltf::import`. A buffer URI naming any
/// absolute path was read into the envelope, and a node cycle recursed until the stack overflowed
/// (an abort, not an error). It now opens through the importer's checks.
#[test]
fn a_hostile_envelope_gltf_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("plant_export_hostile");
    let path = tiny_gltf(&dir, [0, 1, 2], &["a"]);
    let text = std::fs::read_to_string(&path).expect("read");
    // The same buffer by absolute path: outside any confinement.
    let absolute = dir.join("t.bin").to_string_lossy().replace('\\', "/");
    let uri = if absolute.starts_with('/') {
        absolute
    } else {
        format!("/{absolute}")
    };
    let escaped = dir.join("abs.gltf");
    std::fs::write(
        &escaped,
        text.replace("\"uri\":\"t.bin\"", &format!("\"uri\":\"{uri}\"")),
    )
    .expect("write");
    let e =
        regia_asset_pipeline::plant_export::host_load_mesh(&escaped, "abs.gltf").expect_err("absolute URI");
    assert!(e.contains("absolute") || e.contains("not a relative path"), "{e}");
    // A node that is its own child.
    let cyclic = dir.join("cycle.gltf");
    std::fs::write(
        &cyclic,
        text.replace(
            "{\"name\":\"a\",\"mesh\":0}",
            "{\"name\":\"a\",\"mesh\":0,\"children\":[0]}",
        ),
    )
    .expect("write");
    let e = regia_asset_pipeline::plant_export::host_load_mesh(&cyclic, "cycle.gltf").expect_err("cycle");
    assert!(e.contains("own child"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
