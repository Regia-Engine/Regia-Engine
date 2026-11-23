// SPDX-License-Identifier: MPL-2.0
//! Cooked mesh format v2 (ADR-226): glTF skin, `COLOR_0` and morph targets survive the cook and
//! load, joint indices land on cooked skeleton ids, and weights are bit-exact after the documented
//! `unorm16` quantisation.
//!
//! A skinned mesh under a rotated, 0.01-scaled armature skins to the glTF 2.0 reference at bind
//! and in an animated pose, for `Forward::Keep` and `Forward::Convert` (ADR-226).
//!
//! Generated fixtures always run (67 joints like DO27's pilots, 82 like the helmet-off pilots,
//! 256 at the renderer's cap, 300 for `u16` joints). CesiumMan and Fox run when
//! `REGIA_KHRONOS_ASSETS` (or a `.regia/test-assets/khronos-gltf` above the crate) has them.

use std::collections::HashMap;
use std::path::PathBuf;

use regia_asset::{
    AssetId, CookedJointWidth, CookedMesh, CpuMesh, normalize_unorm16_weights, quantize_skin_weights,
};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_glb_bytes};
use regia_core::math::{Mat4, Quat, Vec3};

/// Weight encoding of a generated fixture.
#[derive(Clone, Copy, Debug, PartialEq)]
enum WeightType {
    Float,
    Unorm8,
    Unorm16,
}

/// A generated skinned strip: `joint_count` joints in chains whose skin lists them leaf first
/// (so the cooked skeleton order differs from the skin's slot order), `quads` quads, optional
/// second influence set, `COLOR_0`, and two named morph targets.
struct Fixture {
    glb: Vec<u8>,
    positions: Vec<[f32; 3]>,
    /// Skin slots per vertex (set 0, then set 1).
    slots: Vec<[u16; 8]>,
    /// Source weights as floats (per set as authored).
    weights: Vec<[f32; 8]>,
    /// Source weights as integers when [`WeightType`] is an integer type.
    weights_int: Vec<[u16; 8]>,
    colors: Vec<[f32; 4]>,
    /// Node index of each skin slot.
    slot_nodes: Vec<usize>,
    /// (vertex, delta) per target.
    morphs: Vec<Vec<(usize, [f32; 3])>>,
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn glb(json: &str, bin: &[u8]) -> Vec<u8> {
    let json_bytes = json.as_bytes();
    let json_pad = (4 - json_bytes.len() % 4) % 4;
    let bin_pad = (4 - bin.len() % 4) % 4;
    let json_len = json_bytes.len() + json_pad;
    let bin_len = bin.len() + bin_pad;
    let total = 12 + 8 + json_len + 8 + bin_len;
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_len as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(json_bytes);
    out.extend(std::iter::repeat_n(b' ', json_pad));
    out.extend_from_slice(&(bin_len as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(bin);
    out.extend(std::iter::repeat_n(0u8, bin_pad));
    out
}

fn fixture(joint_count: usize, eight: bool, weight_type: WeightType) -> Fixture {
    let quads = 40usize;
    let vertex_count = 2 * (quads + 1);
    let mut positions = Vec::new();
    for i in 0..=quads {
        positions.push([i as f32 * 0.125, 0.0, 0.0]);
        positions.push([i as f32 * 0.125, 0.5, 0.25]);
    }
    let mut indices: Vec<u16> = Vec::new();
    for q in 0..quads as u16 {
        let a = q * 2;
        indices.extend_from_slice(&[a, a + 2, a + 1, a + 1, a + 2, a + 3]);
    }
    let sets = if eight { 2 } else { 1 };
    let mut slots = Vec::new();
    let mut weights = Vec::new();
    let mut weights_int = Vec::new();
    for v in 0..vertex_count {
        let mut s = [0u16; 8];
        let mut w = [0f32; 8];
        let mut wi = [0u16; 8];
        for (k, slot) in s.iter_mut().enumerate().take(4 * sets) {
            *slot = ((v * 7 + k * 13) % joint_count) as u16;
        }
        // Uneven weights; the last slot of each vertex is sometimes zero.
        let raw: Vec<f32> = (0..4 * sets)
            .map(|k| {
                if k == 3 && v.is_multiple_of(3) {
                    0.0
                } else {
                    1.0 + ((v + k * 5) % 7) as f32
                }
            })
            .collect();
        let sum: f32 = raw.iter().sum();
        match weight_type {
            WeightType::Float => {
                for (dst, r) in w.iter_mut().zip(&raw) {
                    *dst = r / sum;
                }
            }
            WeightType::Unorm8 | WeightType::Unorm16 => {
                let max = if weight_type == WeightType::Unorm8 {
                    255u32
                } else {
                    65535
                };
                for (dst, r) in wi.iter_mut().zip(&raw) {
                    *dst = ((r / sum) * max as f32).floor() as u16;
                }
                // Exact integer sum, as glTF requires for normalised integer weights.
                let total: u32 = wi.iter().map(|x| u32::from(*x)).sum();
                wi[0] += (max - total) as u16;
                for (dst, x) in w.iter_mut().zip(&wi) {
                    *dst = f32::from(*x) / max as f32;
                }
            }
        }
        slots.push(s);
        weights.push(w);
        weights_int.push(wi);
    }
    let colors: Vec<[f32; 4]> = (0..vertex_count)
        .map(|v| [(v % 4) as f32 / 4.0, 0.5, 1.0 - (v % 8) as f32 / 8.0, 1.0])
        .collect();
    // Target 0 moves every third vertex; target 1 moves vertex 0 only. Values are exact in f16.
    let mut smile = vec![[0.0f32; 3]; vertex_count];
    let mut morph0 = Vec::new();
    for v in (0..vertex_count).step_by(3) {
        smile[v] = [0.0, 0.25 + v as f32 * 0.0625, -0.5];
        morph0.push((v, smile[v]));
    }
    let mut blink = vec![[0.0f32; 3]; vertex_count];
    blink[0] = [0.125, 0.0, 0.0];
    let morph1 = vec![(0usize, blink[0])];

    let mut bin = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut push = |bin: &mut Vec<u8>, bytes: Vec<u8>, accessor: serde_json::Value| {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        views.push(serde_json::json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len()}));
        bin.extend(bytes);
        let mut accessor = accessor;
        accessor["bufferView"] = serde_json::json!(views.len() - 1);
        accessors.push(accessor);
        accessors.len() - 1
    };
    let (min, max) = ([0.0, 0.0, 0.0], [quads as f32 * 0.125, 0.5, 0.25]);
    let pos = push(
        &mut bin,
        f32_bytes(&positions.concat()),
        serde_json::json!({"componentType": 5126, "count": vertex_count, "type": "VEC3", "min": min, "max": max}),
    );
    let idx = push(
        &mut bin,
        indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
        serde_json::json!({"componentType": 5123, "count": indices.len(), "type": "SCALAR"}),
    );
    let color = push(
        &mut bin,
        f32_bytes(&colors.concat()),
        serde_json::json!({"componentType": 5126, "count": vertex_count, "type": "VEC4"}),
    );
    let mut attributes = serde_json::json!({"POSITION": pos, "COLOR_0": color});
    for set in 0..sets {
        let joints: Vec<u8> = slots
            .iter()
            .flat_map(|s| s[set * 4..set * 4 + 4].to_vec())
            .flat_map(|j| j.to_le_bytes())
            .collect();
        let j = push(
            &mut bin,
            joints,
            serde_json::json!({"componentType": 5123, "count": vertex_count, "type": "VEC4"}),
        );
        let (bytes, component) = match weight_type {
            WeightType::Float => (
                f32_bytes(
                    &weights
                        .iter()
                        .flat_map(|w| w[set * 4..set * 4 + 4].to_vec())
                        .collect::<Vec<_>>(),
                ),
                5126,
            ),
            WeightType::Unorm8 => (
                weights_int
                    .iter()
                    .flat_map(|w| {
                        w[set * 4..set * 4 + 4]
                            .iter()
                            .map(|x| *x as u8)
                            .collect::<Vec<_>>()
                    })
                    .collect(),
                5121,
            ),
            WeightType::Unorm16 => (
                weights_int
                    .iter()
                    .flat_map(|w| w[set * 4..set * 4 + 4].to_vec())
                    .flat_map(|x| x.to_le_bytes())
                    .collect(),
                5123,
            ),
        };
        let mut accessor =
            serde_json::json!({"componentType": component, "count": vertex_count, "type": "VEC4"});
        if weight_type != WeightType::Float {
            accessor["normalized"] = serde_json::json!(true);
        }
        let w = push(&mut bin, bytes, accessor);
        attributes[format!("JOINTS_{set}")] = serde_json::json!(j);
        attributes[format!("WEIGHTS_{set}")] = serde_json::json!(w);
    }
    let mut targets = Vec::new();
    for deltas in [&smile, &blink] {
        let (lo, hi) = deltas.iter().fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), d| {
            (
                [lo[0].min(d[0]), lo[1].min(d[1]), lo[2].min(d[2])],
                [hi[0].max(d[0]), hi[1].max(d[1]), hi[2].max(d[2])],
            )
        });
        let a = push(
            &mut bin,
            f32_bytes(&deltas.concat()),
            serde_json::json!({"componentType": 5126, "count": vertex_count, "type": "VEC3", "min": lo, "max": hi}),
        );
        targets.push(serde_json::json!({"POSITION": a}));
    }

    // Joint nodes 0..J form chains of up to CHAIN joints (node i+1 is a child of node i), every
    // later chain hanging off node 0, so a skin past 256 joints (u16 ids) stays inside the
    // importer's 256-level nesting limit. The mesh node is J.
    const CHAIN: usize = 128;
    let mut nodes = Vec::new();
    for i in 0..joint_count {
        let mut node = serde_json::json!({"name": format!("j{i}"), "translation": [0.0, 0.05, 0.0]});
        let mut children = Vec::new();
        if i + 1 < joint_count && !(i + 1).is_multiple_of(CHAIN) {
            children.push(i + 1);
        }
        if i == 0 {
            children.extend((CHAIN..joint_count).step_by(CHAIN));
        }
        if !children.is_empty() {
            node["children"] = serde_json::json!(children);
        }
        nodes.push(node);
    }
    nodes.push(serde_json::json!({"name": "body", "mesh": 0, "skin": 0}));
    // Leaf first: skin slot s is node J-1-s.
    let slot_nodes: Vec<usize> = (0..joint_count).rev().collect();
    let doc = serde_json::json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, joint_count]}],
        "nodes": nodes,
        "skins": [{"joints": slot_nodes, "skeleton": 0}],
        "meshes": [{
            "name": "body",
            "primitives": [{"attributes": attributes, "indices": idx, "targets": targets}],
            "extras": {"targetNames": ["smile", "blink"]}
        }],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{"byteLength": bin.len()}]
    });
    Fixture {
        glb: glb(&doc.to_string(), &bin),
        positions,
        slots,
        weights,
        weights_int,
        colors,
        slot_nodes,
        morphs: vec![morph0, morph1],
    }
}

/// Cook (`Forward::Keep` so positions match the source), encode, decode, unpack.
fn cook_and_load(glb: &[u8], forward: Forward) -> (CookedMesh, CpuMesh) {
    let imported = import_gltf_glb_bytes(
        glb,
        AssetId::new_random(),
        forward,
        "linux",
        TextureCookQuality::Low,
    )
    .expect("import");
    let bytes = imported.primary_mesh.to_postcard().expect("encode");
    let cooked = CookedMesh::from_postcard(&bytes).expect("decode");
    let cpu = cooked.to_cpu_mesh().expect("unpack");
    (cooked, cpu)
}

#[test]
fn generated_skins_cook_bit_exact() {
    let cases = [
        (67, false, WeightType::Float),
        (82, false, WeightType::Float),
        (82, true, WeightType::Float),
        (256, false, WeightType::Unorm16),
        (256, true, WeightType::Unorm8),
        (300, false, WeightType::Float),
    ];
    for (joint_count, eight, weight_type) in cases {
        let case = format!("{joint_count} joints, eight={eight}, {weight_type:?}");
        let fx = fixture(joint_count, eight, weight_type);
        let skeleton = import_gltf_animation(
            &fx.glb,
            &GltfAnimSettings {
                forward: Forward::Keep,
                ..GltfAnimSettings::default()
            },
        )
        .expect("skeleton")
        .skins
        .remove(0)
        .skeleton;
        let (cooked, cpu) = cook_and_load(&fx.glb, Forward::Keep);
        let packed = cooked.skin.as_ref().expect("skin block");
        let width = if joint_count > 256 {
            CookedJointWidth::U16
        } else {
            CookedJointWidth::U8
        };
        assert_eq!(packed.joint_width, width, "{case}");
        let skin = cpu.skin.as_ref().expect("skin");
        assert_eq!(
            skin.joint_names, skeleton.joint_names,
            "{case}: names in skeleton order"
        );
        assert_eq!(skin.joints1.is_empty(), !eight, "{case}");
        // The skin lists joints leaf first; the skeleton is topological, so ids differ from slots.
        assert_ne!(
            skeleton.joint_names[0],
            format!("j{}", fx.slot_nodes[0]),
            "{case}: reordered"
        );

        let by_position: HashMap<[u32; 3], usize> = fx
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| (p.map(f32::to_bits), i))
            .collect();
        assert_eq!(cpu.positions.len(), fx.positions.len(), "{case}");
        let mut cooked_of_source = vec![usize::MAX; fx.positions.len()];
        for (v, p) in cpu.positions.iter().enumerate() {
            let s = by_position[&p.map(f32::to_bits)];
            cooked_of_source[s] = v;
            let want: [u16; 8] = match (weight_type, eight) {
                (WeightType::Float, false) => {
                    let q = quantize_skin_weights([
                        fx.weights[s][0],
                        fx.weights[s][1],
                        fx.weights[s][2],
                        fx.weights[s][3],
                    ]);
                    [q[0], q[1], q[2], q[3], 0, 0, 0, 0]
                }
                (WeightType::Float, true) => quantize_skin_weights(fx.weights[s]),
                (_, _) => {
                    let widen = |x: u16| {
                        if weight_type == WeightType::Unorm8 {
                            x * 257
                        } else {
                            x
                        }
                    };
                    let wide = fx.weights_int[s].map(widen);
                    if eight {
                        normalize_unorm16_weights(wide)
                    } else {
                        let q = normalize_unorm16_weights([wide[0], wide[1], wide[2], wide[3]]);
                        [q[0], q[1], q[2], q[3], 0, 0, 0, 0]
                    }
                }
            };
            let got: Vec<u16> = skin.weights[v]
                .iter()
                .chain(skin.weights1.get(v).into_iter().flatten())
                .copied()
                .collect();
            assert_eq!(
                &got[..],
                &want[..got.len()],
                "{case}: vertex {s} weights bit-exact"
            );
            if weight_type != WeightType::Float {
                // Integer sources that sum to one are not requantised.
                let widen = |x: u16| {
                    if weight_type == WeightType::Unorm8 {
                        x * 257
                    } else {
                        x
                    }
                };
                assert_eq!(
                    &got[..],
                    &fx.weights_int[s].map(widen)[..got.len()],
                    "{case}: kept verbatim"
                );
            }
            let joints: Vec<u16> = skin.joints[v]
                .iter()
                .chain(skin.joints1.get(v).into_iter().flatten())
                .copied()
                .collect();
            for (k, joint) in joints.iter().enumerate() {
                let node = fx.slot_nodes[usize::from(fx.slots[s][k])];
                assert_eq!(
                    skeleton.joint_names[usize::from(*joint)],
                    format!("j{node}"),
                    "{case}: vertex {s} slot {k} (influence order kept)"
                );
            }
            for (got, want) in cpu.colors[v].iter().zip(fx.colors[s]) {
                assert!((got - want).abs() <= 0.5 / 65535.0, "{case}: colour");
            }
        }

        assert_eq!(cpu.morph_targets.len(), 2, "{case}");
        for (target, (name, source)) in cpu
            .morph_targets
            .iter()
            .zip([("smile", &fx.morphs[0]), ("blink", &fx.morphs[1])])
        {
            assert_eq!(target.name, name, "{case}");
            let mut want: Vec<(u32, [f32; 3])> = source
                .iter()
                .map(|(s, d)| (cooked_of_source[*s] as u32, *d))
                .collect();
            want.sort_by_key(|(v, _)| *v);
            assert_eq!(
                target.vertex_indices,
                want.iter().map(|w| w.0).collect::<Vec<_>>(),
                "{case}: sparse vertices"
            );
            assert_eq!(
                target.position_deltas,
                want.iter().map(|w| w.1).collect::<Vec<_>>(),
                "{case}: deltas exact in f16"
            );
            assert!(target.normal_deltas.is_empty() && target.tangent_deltas.is_empty());
        }
        let bounds = &cooked.morph_targets[0];
        let ys: Vec<f32> = fx.morphs[0].iter().map(|m| m.1[1]).collect();
        assert_eq!(
            bounds.delta_min[1],
            ys.iter().copied().fold(f32::MAX, f32::min),
            "{case}"
        );
        assert_eq!(
            bounds.delta_max[1],
            ys.iter().copied().fold(f32::MIN, f32::max),
            "{case}"
        );
        assert_eq!((bounds.delta_min[2], bounds.delta_max[2]), (-0.5, -0.5), "{case}");
    }
}

#[test]
fn forward_convert_turns_morph_deltas_with_the_positions() {
    let fx = fixture(67, false, WeightType::Float);
    let (_, cpu) = cook_and_load(&fx.glb, Forward::Convert);
    let target = &cpu.morph_targets[1];
    assert_eq!(target.name, "blink");
    // The cooker's 180° yaw maps +X to −X.
    assert_eq!(target.position_deltas, vec![[-0.125, 0.0, 0.0]]);
    assert!(cpu.skin.is_some());
}

#[test]
fn rigid_gltf_cooks_without_v2_blocks() {
    let fx = fixture(67, false, WeightType::Float);
    // Drop the skin from the mesh node: the same primitive cooks rigid.
    let json_len = u32::from_le_bytes(fx.glb[12..16].try_into().expect("len")) as usize;
    let json = std::str::from_utf8(&fx.glb[20..20 + json_len])
        .expect("json")
        .trim_end();
    let mut doc: serde_json::Value = serde_json::from_str(json).expect("parse");
    let mesh_node = doc["nodes"].as_array().expect("nodes").len() - 1;
    doc["nodes"][mesh_node]
        .as_object_mut()
        .expect("node")
        .remove("skin");
    let bin_start = 20 + json_len + 8;
    let rigid = glb(&doc.to_string(), &fx.glb[bin_start..]);
    let (cooked, cpu) = cook_and_load(&rigid, Forward::Keep);
    assert!(cooked.skin.is_none() && cpu.skin.is_none());
    // Colour and morph targets do not depend on a skin.
    assert_eq!(cpu.colors.len(), cpu.positions.len());
    assert_eq!(cpu.morph_targets.len(), 2);
}

/// The Khronos sample cache: `REGIA_KHRONOS_ASSETS`, else a `.regia/test-assets/khronos-gltf`
/// in an ancestor of this crate (the main checkout when this runs from a worktree).
fn khronos(name: &str) -> Option<Vec<u8>> {
    let mut dirs: Vec<PathBuf> = regia_core::brand::env_var("REGIA_KHRONOS_ASSETS")
        .ok()
        .map(PathBuf::from)
        .into_iter()
        .collect();
    let mut dir = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    while let Some(d) = dir {
        dirs.push(d.join(".regia/test-assets/khronos-gltf"));
        dir = d.parent().map(PathBuf::from);
    }
    dirs.iter().find_map(|d| std::fs::read(d.join(name)).ok())
}

/// glTF → cook → load keeps every authored influence: the cooked (joint names, `unorm16`
/// weights) tuples are a sub-multiset of the source's, quantised with
/// [`quantize_skin_weights`] (the cook reorders vertices, so tuples are compared as a multiset).
fn khronos_skin_is_bit_exact(name: &str, expected_joints: usize) {
    let Some(bytes) = khronos(name) else {
        assert!(!assets_required(), "{name} is missing and REGIA_REQUIRE_ASSETS=1");
        eprintln!("skip {name}: set REGIA_KHRONOS_ASSETS (cargo xtask fetch-test-assets)");
        return;
    };
    let skeleton = import_gltf_animation(&bytes, &GltfAnimSettings::default())
        .expect("skeleton")
        .skins
        .remove(0)
        .skeleton;
    assert_eq!(skeleton.len(), expected_joints, "{name}");
    let (_, cpu) = cook_and_load(&bytes, Forward::Convert);
    let skin = cpu.skin.as_ref().expect("skin");
    assert_eq!(skin.joint_names, skeleton.joint_names, "{name}");

    let gltf = gltf::Gltf::from_slice(&bytes).expect("parse");
    let blob = gltf.blob.as_deref().expect("blob");
    let skin_names: Vec<String> = gltf
        .skins()
        .next()
        .expect("skin")
        .joints()
        .map(|j| j.name().unwrap_or_default().to_string())
        .collect();
    let prim = gltf
        .meshes()
        .next()
        .expect("mesh")
        .primitives()
        .next()
        .expect("prim");
    let reader = prim.reader(|b| (b.index() == 0).then_some(blob));
    let joints: Vec<[u16; 4]> = reader.read_joints(0).expect("joints").into_u16().collect();
    let weights: Vec<[f32; 4]> = reader.read_weights(0).expect("weights").into_f32().collect();
    // Every cooked vertex's (joint names, weights) must be a source vertex's, as a multiset.
    let key = |names: [&str; 4], w: [u16; 4]| -> String { format!("{names:?}{w:?}") };
    let mut source: HashMap<String, usize> = HashMap::new();
    for (j, w) in joints.iter().zip(&weights) {
        let q = quantize_skin_weights(*w);
        let names = std::array::from_fn(|k| {
            if q[k] == 0 {
                ""
            } else {
                skin_names[usize::from(j[k])].as_str()
            }
        });
        *source.entry(key(names, q)).or_default() += 1;
    }
    // The cook may drop vertices only a removed duplicate triangle used.
    assert!(
        cpu.positions.len() <= joints.len() && cpu.positions.len() * 100 >= joints.len() * 99,
        "{name}: {} cooked of {} source vertices",
        cpu.positions.len(),
        joints.len()
    );
    for v in 0..cpu.positions.len() {
        let w = skin.weights[v];
        let names = std::array::from_fn(|k| {
            if w[k] == 0 {
                ""
            } else {
                skin.joint_names[usize::from(skin.joints[v][k])].as_str()
            }
        });
        let k = key(names, w);
        let left = source
            .get_mut(&k)
            .unwrap_or_else(|| panic!("{name}: vertex {v} {k} is not a source influence"));
        assert!(
            *left > 0,
            "{name}: vertex {v} {k} occurs more often than in the source"
        );
        *left -= 1;
    }
}

#[test]
fn cesium_man_skin_survives_the_cook() {
    khronos_skin_is_bit_exact("CesiumMan.glb", 19);
}

#[test]
fn fox_skin_survives_the_cook() {
    khronos_skin_is_bit_exact("Fox.glb", 24);
}

/// A skinned strip in Z-up centimetres under a rotated, 0.01-scaled armature node (the usual
/// Blender/Mixamo export), with the mesh node itself moved and turned relative to the armature.
/// Two joints (`hips` → `spine`), file inverse bind matrices as exporters write them
/// (`inverse(joint world) · mesh world`), one morph target with position, normal and tangent
/// deltas, and a clip `bend` whose end key turns and moves both joints.
struct ArmatureFixture {
    glb: Vec<u8>,
    /// Source positions (mesh space, centimetres).
    positions: Vec<Vec3>,
    /// Skin slots (= joint index: hips 0, spine 1) and float weights per vertex.
    influences: Vec<([usize; 2], [f32; 2])>,
    /// Armature node world.
    armature: Mat4,
    /// Mesh node world.
    mesh_world: Mat4,
    /// Joint rest locals (relative to the armature / to hips) and their `bend` end keys.
    hips_rest: (Vec3, Quat),
    spine_rest: (Vec3, Quat),
    hips_end: (Vec3, Quat),
    spine_end: (Vec3, Quat),
    /// The joints' file inverse bind matrices.
    ibms: [Mat4; 2],
}

fn armature_fixture() -> ArmatureFixture {
    let armature = Mat4::from_scale_rotation_translation(
        Vec3::splat(0.01),
        Quat::from_rotation_y(0.35) * Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        Vec3::new(1.0, 2.0, -3.0),
    );
    let mesh_local = Mat4::from_rotation_translation(Quat::from_rotation_z(0.5), Vec3::new(0.0, 5.0, 2.0));
    let mesh_world = armature * mesh_local;
    let hips_rest = (
        Vec3::new(0.0, 0.0, 100.0),
        Quat::from_rotation_x(std::f32::consts::FRAC_PI_2),
    );
    let spine_rest = (Vec3::new(0.0, 40.0, 0.0), Quat::from_rotation_z(0.2));
    let hips_end = (
        Vec3::new(10.0, -5.0, 95.0),
        hips_rest.1 * Quat::from_rotation_y(0.8),
    );
    let spine_end = (spine_rest.0, spine_rest.1 * Quat::from_rotation_x(1.2));
    let trs = |(t, r): (Vec3, Quat)| Mat4::from_rotation_translation(r, t);
    let hips_world = armature * trs(hips_rest);
    let spine_world = hips_world * trs(spine_rest);
    let ibms = [
        hips_world.inverse() * mesh_world,
        spine_world.inverse() * mesh_world,
    ];

    let positions: Vec<Vec3> = vec![
        Vec3::new(-10.0, 90.0, 0.0),
        Vec3::new(10.0, 90.0, 0.0),
        Vec3::new(-10.0, 120.0, 5.0),
        Vec3::new(10.0, 120.0, 5.0),
        Vec3::new(-10.0, 160.0, 0.0),
        Vec3::new(10.0, 160.0, 0.0),
    ];
    let influences = vec![
        ([0, 1], [1.0, 0.0]),
        ([0, 1], [1.0, 0.0]),
        ([0, 1], [0.5, 0.5]),
        ([1, 0], [0.75, 0.25]),
        ([1, 0], [1.0, 0.0]),
        ([1, 0], [1.0, 0.0]),
    ];
    let indices: Vec<u16> = vec![0, 1, 2, 2, 1, 3, 2, 3, 4, 4, 3, 5];
    let n = positions.len();

    let mut bin = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut push = |bytes: Vec<u8>, accessor: serde_json::Value| {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        views.push(serde_json::json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len()}));
        bin.extend(bytes);
        let mut accessor = accessor;
        accessor["bufferView"] = serde_json::json!(views.len() - 1);
        accessors.push(accessor);
        accessors.len() - 1
    };
    let vec3s = |v: &[Vec3]| f32_bytes(&v.iter().flat_map(|p| p.to_array()).collect::<Vec<_>>());
    let pos = push(
        vec3s(&positions),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC3",
            "min": [-10.0, 90.0, 0.0], "max": [10.0, 160.0, 5.0]}),
    );
    let nrm = push(
        vec3s(&std::iter::repeat_n(Vec3::Z, n).collect::<Vec<_>>()),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC3"}),
    );
    // COLOR_0 tags each vertex with its source index (survives the cook's reorder).
    let colors: Vec<f32> = (0..n).flat_map(|i| [i as f32 / 8.0, 0.0, 0.0, 1.0]).collect();
    let col = push(
        f32_bytes(&colors),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC4"}),
    );
    let joints: Vec<u8> = influences
        .iter()
        .flat_map(|(j, _)| [j[0] as u8, j[1] as u8, 0, 0])
        .collect();
    let jnt = push(
        joints,
        serde_json::json!({"componentType": 5121, "count": n, "type": "VEC4"}),
    );
    let weights: Vec<f32> = influences
        .iter()
        .flat_map(|(_, w)| [w[0], w[1], 0.0, 0.0])
        .collect();
    let wgt = push(
        f32_bytes(&weights),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC4"}),
    );
    let idx = push(
        indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
        serde_json::json!({"componentType": 5123, "count": indices.len(), "type": "SCALAR"}),
    );
    let mut morph_pos = vec![Vec3::ZERO; n];
    let mut morph_nrm = vec![Vec3::ZERO; n];
    let mut morph_tan = vec![Vec3::ZERO; n];
    morph_pos[0] = Vec3::new(0.0, 10.0, 0.0);
    morph_nrm[0] = Vec3::new(0.5, 0.0, 0.0);
    morph_tan[0] = Vec3::new(0.0, 0.5, 0.0);
    let mpos = push(
        vec3s(&morph_pos),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC3",
            "min": [0.0, 0.0, 0.0], "max": [0.0, 10.0, 0.0]}),
    );
    let mnrm = push(
        vec3s(&morph_nrm),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC3"}),
    );
    let mtan = push(
        vec3s(&morph_tan),
        serde_json::json!({"componentType": 5126, "count": n, "type": "VEC3"}),
    );
    let ibm = push(
        f32_bytes(&ibms.iter().flat_map(|m| m.to_cols_array()).collect::<Vec<_>>()),
        serde_json::json!({"componentType": 5126, "count": 2, "type": "MAT4"}),
    );
    let times = push(
        f32_bytes(&[0.0, 1.0]),
        serde_json::json!({"componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0]}),
    );
    let quats = |a: Quat, b: Quat| f32_bytes(&[a.to_array(), b.to_array()].concat());
    let hips_t = push(
        vec3s(&[hips_rest.0, hips_end.0]),
        serde_json::json!({"componentType": 5126, "count": 2, "type": "VEC3"}),
    );
    let hips_r = push(
        quats(hips_rest.1, hips_end.1),
        serde_json::json!({"componentType": 5126, "count": 2, "type": "VEC4"}),
    );
    let spine_r = push(
        quats(spine_rest.1, spine_end.1),
        serde_json::json!({"componentType": 5126, "count": 2, "type": "VEC4"}),
    );

    let (ms, mr, mt) = armature.to_scale_rotation_translation();
    let doc = serde_json::json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [
            {"name": "Armature", "translation": mt.to_array(), "rotation": mr.to_array(),
             "scale": ms.to_array(), "children": [1, 2]},
            {"name": "body", "mesh": 0, "skin": 0, "translation": [0.0, 5.0, 2.0],
             "rotation": Quat::from_rotation_z(0.5).to_array()},
            {"name": "hips", "translation": hips_rest.0.to_array(), "rotation": hips_rest.1.to_array(),
             "children": [3]},
            {"name": "spine", "translation": spine_rest.0.to_array(), "rotation": spine_rest.1.to_array()}
        ],
        "skins": [{"joints": [2, 3], "inverseBindMatrices": ibm, "skeleton": 2}],
        "meshes": [{
            "name": "body",
            "primitives": [{
                "attributes": {"POSITION": pos, "NORMAL": nrm, "COLOR_0": col,
                               "JOINTS_0": jnt, "WEIGHTS_0": wgt},
                "indices": idx,
                "targets": [{"POSITION": mpos, "NORMAL": mnrm, "TANGENT": mtan}]
            }],
            "extras": {"targetNames": ["flex"]}
        }],
        "animations": [{
            "name": "bend",
            "channels": [
                {"sampler": 0, "target": {"node": 2, "path": "translation"}},
                {"sampler": 1, "target": {"node": 2, "path": "rotation"}},
                {"sampler": 2, "target": {"node": 3, "path": "rotation"}}
            ],
            "samplers": [
                {"input": times, "output": hips_t, "interpolation": "LINEAR"},
                {"input": times, "output": hips_r, "interpolation": "LINEAR"},
                {"input": times, "output": spine_r, "interpolation": "LINEAR"}
            ]
        }],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{"byteLength": bin.len()}]
    });
    ArmatureFixture {
        glb: glb(&doc.to_string(), &bin),
        positions,
        influences,
        armature,
        mesh_world,
        hips_rest,
        spine_rest,
        hips_end,
        spine_end,
        ibms,
    }
}

/// glTF 2.0 § 3.7.3 skinning of source vertex `v` with joint locals `hips`/`spine`: world
/// position `Σ w_j · G_j · IBM_j · p` (the mesh node's own transform does not apply).
fn gltf_reference(fx: &ArmatureFixture, v: usize, hips: (Vec3, Quat), spine: (Vec3, Quat)) -> Vec3 {
    let trs = |(t, r): (Vec3, Quat)| Mat4::from_rotation_translation(r, t);
    let g_hips = fx.armature * trs(hips);
    let g_spine = g_hips * trs(spine);
    let joint = [g_hips * fx.ibms[0], g_spine * fx.ibms[1]];
    let (slots, weights) = fx.influences[v];
    let p = fx.positions[v];
    joint[slots[0]].transform_point3(p) * weights[0] + joint[slots[1]].transform_point3(p) * weights[1]
}

/// Regia Engine's skinning of every cooked vertex at clip time `time` (bind pose plus the sampled
/// clip, `palette = global · inverse_bind`, `unorm16` weights), keyed by source vertex.
fn regia_skinned(glb: &[u8], forward: Forward, time: f32) -> (Vec<(usize, Vec3)>, CpuMesh) {
    let skin = import_gltf_animation(
        glb,
        &GltfAnimSettings {
            forward,
            ..GltfAnimSettings::default()
        },
    )
    .expect("skeleton")
    .skins
    .remove(0);
    let skeleton = &skin.skeleton;
    let mut pose = skeleton.bind_pose_pose();
    skin.clips[0].sample_pose(time, &mut pose);
    let palette = skeleton.skin_palette(&skeleton.globals(&pose, None));
    let (_, cpu) = cook_and_load(glb, forward);
    let cooked_skin = cpu.skin.as_ref().expect("skin");
    assert_eq!(cooked_skin.joint_names, skeleton.joint_names);
    let skinned = (0..cpu.positions.len())
        .map(|v| {
            let source = (cpu.colors[v][0] * 8.0).round() as usize;
            let p = Vec3::from_array(cpu.positions[v]);
            let mut out = Vec3::ZERO;
            for k in 0..4 {
                let w = f32::from(cooked_skin.weights[v][k]) / 65535.0;
                out += palette[usize::from(cooked_skin.joints[v][k])].transform_point3(p) * w;
            }
            (source, out)
        })
        .collect();
    (skinned, cpu)
}

#[test]
fn skinned_mesh_under_a_rotated_scaled_parent_matches_the_gltf_reference() {
    let fx = armature_fixture();
    let yaw = Mat4::from_rotation_y(std::f32::consts::PI);
    for (forward, axis) in [(Forward::Keep, Mat4::IDENTITY), (Forward::Convert, yaw)] {
        for (time, hips, spine) in [
            (0.0, fx.hips_rest, fx.spine_rest),
            (1.0, fx.hips_end, fx.spine_end),
        ] {
            let (skinned, cpu) = regia_skinned(&fx.glb, forward, time);
            assert_eq!(
                skinned.len(),
                fx.positions.len(),
                "{forward:?}: every vertex cooked"
            );
            for (v, (source, got)) in skinned.iter().enumerate() {
                let want = axis.transform_point3(gltf_reference(&fx, *source, hips, spine));
                assert!(
                    got.abs_diff_eq(want, 1e-4),
                    "{forward:?} t={time}: vertex {source} skins to {got:?}, glTF gives {want:?}"
                );
                if time == 0.0 {
                    // Bind pose: the skinned mesh sits where its cooked (rigid) vertices are.
                    let cooked = Vec3::from_array(cpu.positions[v]);
                    assert!(
                        cooked.abs_diff_eq(*got, 1e-4),
                        "{forward:?}: bind {cooked:?} vs {got:?}"
                    );
                    let rigid = (axis * fx.mesh_world).transform_point3(fx.positions[*source]);
                    assert!(
                        cooked.abs_diff_eq(rigid, 1e-4),
                        "{forward:?}: baked {cooked:?} vs {rigid:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn morph_deltas_follow_the_base_attributes_under_a_scaled_parent() {
    let fx = armature_fixture();
    let (_, cpu) = cook_and_load(&fx.glb, Forward::Keep);
    let target = &cpu.morph_targets[0];
    assert_eq!(target.name, "flex");
    assert_eq!(target.vertex_indices.len(), 1, "only source vertex 0 moves");
    let v = target.vertex_indices[0] as usize;
    assert_eq!((cpu.colors[v][0] * 8.0).round() as usize, 0);
    let (scale, rotation, _) = fx.mesh_world.to_scale_rotation_translation();
    let close = |got: [f32; 3], want: Vec3, what: &str| {
        let got = Vec3::from_array(got);
        // f16 deltas: 11-bit mantissa.
        assert!(
            got.abs_diff_eq(want, want.length() / 512.0),
            "{what}: {got:?} vs {want:?}"
        );
    };
    // Positions shrink with the 0.01 bake, like the base positions.
    close(
        target.position_deltas[0],
        rotation * (scale * Vec3::new(0.0, 10.0, 0.0)),
        "position",
    );
    // Normals and tangents keep their size: the base normals are renormalised after the bake.
    close(
        target.normal_deltas[0],
        rotation * Vec3::new(0.5, 0.0, 0.0),
        "normal",
    );
    close(
        target.tangent_deltas[0],
        rotation * Vec3::new(0.0, 0.5, 0.0),
        "tangent",
    );
    // The base normal turns with the same rotation.
    close(cpu.normals[v], rotation * Vec3::Z, "base normal");
}

/// The `skin_influences: 8` importer opt-in (ADR-1622): a skin with a second influence set
/// cooks with `skin_influences` 8 and unpacks with `eight_influences`; without the opt-in the
/// cooked mesh is the default (4) and differs from the opt-in cook only in that trailing byte.
/// A skin with four influences stays at 4 even when it asks for eight.
#[test]
fn eight_influences_are_an_importer_opt_in_that_changes_one_byte() {
    let dir = regia_test_utils::unique_temp_dir("skin-influences-opt-in");
    let cook = |eight_source: bool, meta: Option<&str>| {
        let fx = fixture(82, eight_source, WeightType::Float);
        let path = dir.join(format!("strip_{eight_source}.glb"));
        let tmp = dir.join(format!("strip_{eight_source}.glb.tmp"));
        std::fs::write(&tmp, &fx.glb).expect("write fixture");
        std::fs::rename(&tmp, &path).expect("rename fixture");
        regia_asset_pipeline::import_gltf_path(
            &path,
            AssetId::NIL,
            Forward::Keep,
            "linux",
            meta,
            TextureCookQuality::Low,
        )
        .expect("import")
        .primary_mesh
    };
    let default = cook(true, None);
    let explicit_four = cook(true, Some("(skin_influences: 4)"));
    let eight = cook(true, Some("(skin_influences: 8)"));
    assert_eq!(default.skin_influences, regia_asset::DEFAULT_SKIN_INFLUENCES);
    assert_eq!(explicit_four, default);
    assert_eq!(eight.skin_influences, regia_asset::EIGHT_SKIN_INFLUENCES);
    assert_eq!(eight.skin, default.skin, "the influence streams do not change");
    let default_bytes = default.to_postcard().expect("encode");
    let eight_bytes = eight.to_postcard().expect("encode");
    assert_eq!(default_bytes.len(), eight_bytes.len());
    let differ: Vec<usize> = (0..default_bytes.len())
        .filter(|&i| default_bytes[i] != eight_bytes[i])
        .collect();
    assert_eq!(
        differ,
        vec![default_bytes.len() - 1],
        "only skin_influences differs"
    );

    let loaded = CookedMesh::from_postcard(&eight_bytes).expect("decode");
    let cpu = loaded.to_cpu_mesh().expect("unpack");
    assert!(cpu.skin.as_ref().is_some_and(|s| s.eight_influences));
    let cpu_default = CookedMesh::from_postcard(&default_bytes)
        .expect("decode")
        .to_cpu_mesh()
        .expect("unpack");
    assert!(cpu_default.skin.as_ref().is_some_and(|s| !s.eight_influences));

    let four_source = cook(false, Some("(skin_influences: 8)"));
    assert_eq!(
        four_source.skin_influences,
        regia_asset::DEFAULT_SKIN_INFLUENCES,
        "no JOINTS_1: the opt-in cannot apply"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `REGIA_REQUIRE_ASSETS=1` (set by a job that must cover the Khronos samples): a missing sample is
/// a failure, not a skip, so lost assets cannot turn these tests vacuous (audit-v035-flaky M1).
fn assets_required() -> bool {
    std::env::var_os("REGIA_REQUIRE_ASSETS").is_some_and(|v| v == "1")
}
