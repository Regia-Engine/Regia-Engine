// SPDX-License-Identifier: MPL-2.0
//! Skins and animation clips of the source meshes (`ExportOptions::skins`, `animations`).
//!
//! A source model with a skin (and `skins` on) or with clips (and `animations` on) cannot have its
//! node transforms baked into vertices: the joints are nodes and the clips drive them. Its
//! scene's node tree is therefore written as is under the entity's node (below one
//! `Regia_import_axis` node that carries the importer's half turn about +Y under
//! [`Forward::Convert`]), with raw vertices, `JOINTS_n`/`WEIGHTS_n`, skins with their inverse bind
//! matrices, and the clips whose channels drive written nodes (glTF 2.0 sections 3.7.3 and 3.11).

use std::collections::BTreeMap;

use regia_core::math::Mat4;

use super::{Exporter, Forward, GlbNode, MeshSourceRon, SourceModel, SrcPrim, read_prim};
use crate::gltf_write::{GlbAnimation, GlbChannel, GlbPath, GlbSkin};

/// Name of the node that carries the half turn above a source model's tree.
pub const IMPORT_AXIS_NODE_NAME: &str = "Regia_import_axis";

/// Whether `src` needs its tree written for the options.
fn needs_tree(ex: &Exporter<'_>, src: &SourceModel) -> bool {
    (ex.options.skins && src.document.skins().next().is_some())
        || (ex.options.animations && src.document.animations().next().is_some())
}

/// Writes the source tree of `s` under `node` when the options ask for skins or clips and the
/// model has some. Returns false (and writes nothing) when the plain baked mesh should be used.
pub(super) fn attach(ex: &mut Exporter<'_>, node: usize, s: &MeshSourceRon, entity: &str) -> bool {
    if s.mesh.is_empty() || s.mesh.starts_with("builtin:") || !(ex.options.skins || ex.options.animations) {
        return false;
    }
    let Some(src) = ex.take_source(&s.mesh) else {
        return false;
    };
    if !needs_tree(ex, &src) {
        ex.put_source(&s.mesh, src);
        return false;
    }
    let override_material = if s.material.is_empty() {
        None
    } else {
        ex.ron_material(&s.material)
    };
    write_tree(ex, node, &s.mesh, &src, override_material, entity);
    ex.put_source(&s.mesh, src);
    true
}

fn write_tree(
    ex: &mut Exporter<'_>,
    entity_node: usize,
    path: &str,
    src: &SourceModel,
    override_material: Option<usize>,
    entity: &str,
) {
    let Some(scene) = src
        .document
        .default_scene()
        .or_else(|| src.document.scenes().next())
    else {
        return;
    };
    let with_skins = ex.options.skins;
    // Pass 1: nodes in pre-order, so a parent always precedes its children.
    let mut mapped: BTreeMap<usize, usize> = BTreeMap::new();
    let mut order: Vec<gltf::Node> = Vec::new();
    let mut stack: Vec<gltf::Node> = scene.nodes().collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        let mut kids: Vec<gltf::Node> = n.children().collect();
        kids.reverse();
        stack.extend(kids);
        order.push(n);
    }
    for n in &order {
        let (t, r, sc) = n.transform().decomposed();
        let skinned_rigid = n.skin().is_some() && !with_skins;
        let index = ex.scene.nodes.len();
        ex.scene.nodes.push(GlbNode {
            name: n
                .name()
                .map_or_else(|| format!("node{}", n.index()), str::to_owned),
            // A skinned mesh's node transform is ignored by the specification; without the skin
            // the mesh must not pick it up.
            translation: if skinned_rigid { [0.0; 3] } else { t },
            rotation: if skinned_rigid { [0.0, 0.0, 0.0, 1.0] } else { r },
            scale: if skinned_rigid { [1.0; 3] } else { sc },
            ..GlbNode::default()
        });
        mapped.insert(n.index(), index);
    }
    // Pass 2: children, meshes.
    for n in &order {
        let Some(&index) = mapped.get(&n.index()) else {
            continue;
        };
        let kids: Vec<usize> = n
            .children()
            .filter_map(|c| mapped.get(&c.index()).copied())
            .collect();
        ex.scene.nodes[index].children = kids;
        let Some(mesh) = n.mesh() else { continue };
        let skinned = n.skin().is_some() && with_skins;
        let key = (path.to_owned(), mesh.index(), override_material, skinned);
        let mesh_index = match ex.tree_meshes.get(&key) {
            Some(i) => *i,
            None => {
                let mut parts: Vec<(SrcPrim, Option<usize>)> = Vec::new();
                for primitive in mesh.primitives() {
                    let Some(prim) = read_prim(
                        src,
                        mesh.name().unwrap_or(""),
                        &primitive,
                        Mat4::IDENTITY,
                        skinned,
                        &mut ex.warnings,
                    ) else {
                        continue;
                    };
                    let material = match override_material {
                        Some(m) => Some(m),
                        None => prim.material.and_then(|i| ex.gltf_material(src, i)),
                    };
                    parts.push((prim, material));
                }
                let name = mesh
                    .name()
                    .map_or_else(|| format!("mesh{}", mesh.index()), str::to_owned);
                let i = ex.push_mesh(name, parts);
                ex.tree_meshes.insert(key, i);
                i
            }
        };
        ex.scene.nodes[index].mesh = Some(mesh_index);
    }
    // Pass 3: skins (a joint may come after the node that uses it, so after all nodes exist).
    if with_skins {
        let mut skin_cache: BTreeMap<usize, usize> = BTreeMap::new();
        for n in &order {
            let Some(skin) = n.skin() else { continue };
            let Some(&index) = mapped.get(&n.index()) else {
                continue;
            };
            let skin_index = match skin_cache.get(&skin.index()) {
                Some(i) => Some(*i),
                None => {
                    let joints: Option<Vec<usize>> =
                        skin.joints().map(|j| mapped.get(&j.index()).copied()).collect();
                    match joints {
                        None => {
                            ex.warnings.push(format!(
                                "skin_skipped: `{entity}` skin {} has a joint outside the exported scene",
                                skin.index()
                            ));
                            None
                        }
                        Some(joints) => {
                            let reader = skin.reader(|b| src.buffers.get(b.index()).map(|d| d.0.as_slice()));
                            let inverse_bind: Vec<[f32; 16]> = match reader.read_inverse_bind_matrices() {
                                Some(m) => m.map(|m| Mat4::from_cols_array_2d(&m).to_cols_array()).collect(),
                                None => vec![Mat4::IDENTITY.to_cols_array(); joints.len()],
                            };
                            ex.scene.skins.push(GlbSkin {
                                name: skin.name().unwrap_or("").to_owned(),
                                joints,
                                inverse_bind,
                            });
                            let i = ex.scene.skins.len() - 1;
                            skin_cache.insert(skin.index(), i);
                            Some(i)
                        }
                    }
                }
            };
            ex.scene.nodes[index].skin = skin_index;
        }
    }
    // The tree's roots go under the entity's node, below the axis node when converting.
    let roots: Vec<usize> = scene
        .nodes()
        .filter_map(|n| mapped.get(&n.index()).copied())
        .collect();
    match ex.options.forward {
        Forward::Convert => {
            let axis = ex.scene.nodes.len();
            ex.scene.nodes.push(GlbNode {
                name: IMPORT_AXIS_NODE_NAME.to_owned(),
                rotation: [0.0, 1.0, 0.0, 0.0],
                children: roots,
                ..GlbNode::default()
            });
            ex.scene.nodes[entity_node].children.push(axis);
        }
        Forward::Keep => ex.scene.nodes[entity_node].children.extend(roots),
    }
    if ex.options.animations {
        write_animations(ex, src, &mapped, entity);
    }
}

fn write_animations(ex: &mut Exporter<'_>, src: &SourceModel, mapped: &BTreeMap<usize, usize>, entity: &str) {
    use gltf::animation::Interpolation;
    use gltf::animation::util::ReadOutputs;
    for (a, animation) in src.document.animations().enumerate() {
        let mut channels = Vec::new();
        for channel in animation.channels() {
            let Some(&node) = mapped.get(&channel.target().node().index()) else {
                continue;
            };
            let reader = channel.reader(|b| src.buffers.get(b.index()).map(|d| d.0.as_slice()));
            let (Some(inputs), Some(outputs)) = (reader.read_inputs(), reader.read_outputs()) else {
                continue;
            };
            let times: Vec<f32> = inputs.collect();
            if times.is_empty() {
                continue;
            }
            let (path, flat): (GlbPath, Vec<f32>) = match outputs {
                ReadOutputs::Translations(v) => (GlbPath::Translation, v.flatten().collect()),
                ReadOutputs::Scales(v) => (GlbPath::Scale, v.flatten().collect()),
                ReadOutputs::Rotations(v) => (GlbPath::Rotation, v.into_f32().flatten().collect()),
                ReadOutputs::MorphTargetWeights(v) => (GlbPath::Weights, v.into_f32().collect()),
            };
            let cubic = channel.sampler().interpolation() == Interpolation::CubicSpline;
            let step = channel.sampler().interpolation() == Interpolation::Step;
            let keys = times.len() * if cubic { 3 } else { 1 };
            if flat.is_empty() || !flat.len().is_multiple_of(keys) {
                ex.warnings.push(format!(
                    "animation_channel_skipped: `{entity}` clip {a} has {} output values for {} keys",
                    flat.len(),
                    times.len()
                ));
                continue;
            }
            let per_key = flat.len() / keys;
            let values: Vec<f32> = if cubic {
                // [in-tangent, value, out-tangent] per key: keep the values.
                flat.chunks(per_key * 3)
                    .flat_map(|k| k[per_key..per_key * 2].to_vec())
                    .collect()
            } else {
                flat
            };
            if cubic || step {
                ex.warnings.push(format!(
                    "animation_interpolation: `{entity}` clip {a} {} is written as LINEAR through its key values",
                    if cubic { "CUBICSPLINE" } else { "STEP" }
                ));
            }
            channels.push(GlbChannel {
                node,
                path,
                times,
                values,
            });
        }
        if channels.is_empty() {
            continue;
        }
        let name = animation.name().map_or_else(|| format!("clip{a}"), str::to_owned);
        ex.scene.animations.push(GlbAnimation {
            name: format!("{entity}/{name}"),
            channels,
        });
    }
}
