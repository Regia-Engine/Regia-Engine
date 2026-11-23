// SPDX-License-Identifier: MPL-2.0
//! A scene that touches every path of the writer the FBX, OBJ, plant, procgen and cloth callers
//! use, so a test can pin its bytes: the additive `GlbExtras` work must not move them. The same
//! file is compiled against the writer as it was before that work (`include!`) to take the pin.

use serde_json::{Map, json};

use super::*;

/// FNV-1a 64 of `bytes`.
pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The scene.
pub(super) fn rich_scene() -> GlbScene {
    let quad = |z: f32| vec![[0.0, 0.0, z], [1.0, 0.0, z], [1.0, 1.0, z], [0.0, 1.0, z]];
    GlbScene {
        generator: "pin".into(),
        nodes: vec![
            GlbNode {
                name: "Root".into(),
                children: vec![1, 2],
                translation: [1.0, 2.0, 3.0],
                rotation: [
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                ],
                scale: [1.0, 2.0, 1.0],
                extras: Some(json!({ "k": 1 })),
                ..GlbNode::default()
            },
            GlbNode {
                name: "Skinned".into(),
                mesh: Some(0),
                skin: Some(0),
                ..GlbNode::default()
            },
            GlbNode {
                name: "Joint".into(),
                mesh: Some(1),
                ..GlbNode::default()
            },
        ],
        roots: vec![0],
        meshes: vec![
            GlbMesh {
                name: "A".into(),
                target_names: vec!["smile".into()],
                weights: vec![0.5],
                primitives: vec![GlbPrimitive {
                    positions: quad(0.0),
                    normals: vec![[0.0, 0.0, 1.0]; 4],
                    uvs: vec![
                        vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                        vec![[0.5, 0.5]; 4],
                    ],
                    colors: vec![[1.0, 0.5, 0.25, 1.0]; 4],
                    joints: vec![vec![[0, 1, 0, 0]; 4]],
                    weights: vec![vec![[0.75, 0.25, 0.0, 0.0]; 4]],
                    indices: vec![0, 1, 2, 0, 2, 3],
                    material: Some(0),
                    targets: vec![GlbMorphTarget {
                        positions: vec![[0.0, 0.1, 0.0]; 4],
                        normals: vec![[0.0, 0.0, 0.1]; 4],
                    }],
                    custom_vec3: vec![("_AO".into(), vec![[0.1, 0.2, 0.3]; 4])],
                    custom_vec4: vec![("_TINT".into(), vec![[0.1, 0.2, 0.3, 0.4]; 4])],
                }],
                extras: Map::new(),
            },
            GlbMesh {
                name: "B".into(),
                primitives: vec![GlbPrimitive {
                    positions: quad(1.0),
                    indices: vec![0, 1, 2],
                    material: Some(1),
                    ..GlbPrimitive::default()
                }],
                ..GlbMesh::default()
            },
        ],
        materials: vec![
            GlbMaterial {
                name: "Lamp".into(),
                base_color: [0.9, 0.8, 0.7, 0.6],
                metallic: 0.25,
                roughness: 0.75,
                emissive: [4.0, 2.0, 1.0],
                base_color_texture: Some(GlbTextureRef {
                    image: 0,
                    tex_coord: 1,
                }),
                metallic_roughness_texture: Some(GlbTextureRef {
                    image: 1,
                    tex_coord: 0,
                }),
                normal_texture: Some(GlbTextureRef {
                    image: 1,
                    tex_coord: 0,
                }),
                emissive_texture: Some(GlbTextureRef {
                    image: 0,
                    tex_coord: 0,
                }),
                alpha_blend: true,
                double_sided: true,
            },
            GlbMaterial {
                name: "Plain".into(),
                emissive: [0.2, 0.0, 0.0],
                ..GlbMaterial::default()
            },
        ],
        images: vec![
            GlbImage::Embedded {
                bytes: vec![1, 2, 3, 4, 5],
                mime: "image/png",
            },
            GlbImage::Uri("tex/a%20b.png".into()),
        ],
        skins: vec![GlbSkin {
            name: "Rig".into(),
            joints: vec![2, 0],
            inverse_bind: vec![
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                ],
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 2.0, 3.0, 1.0,
                ],
            ],
        }],
        animations: vec![GlbAnimation {
            name: "Wave".into(),
            channels: vec![
                GlbChannel {
                    node: 2,
                    path: GlbPath::Translation,
                    times: vec![0.0, 1.0],
                    values: vec![0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
                },
                GlbChannel {
                    node: 2,
                    path: GlbPath::Rotation,
                    times: vec![0.0, 1.0],
                    values: vec![
                        0.0,
                        0.0,
                        0.0,
                        1.0,
                        0.0,
                        std::f32::consts::FRAC_1_SQRT_2,
                        0.0,
                        std::f32::consts::FRAC_1_SQRT_2,
                    ],
                },
                GlbChannel {
                    node: 2,
                    path: GlbPath::Scale,
                    times: vec![0.0, 1.0],
                    values: vec![1.0, 1.0, 1.0, 2.0, 2.0, 2.0],
                },
                GlbChannel {
                    node: 1,
                    path: GlbPath::Weights,
                    times: vec![0.0, 1.0],
                    values: vec![0.0, 1.0],
                },
            ],
        }],
    }
}
