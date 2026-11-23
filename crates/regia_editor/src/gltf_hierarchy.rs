// SPDX-License-Identifier: MPL-2.0
//! Spawns a glTF/GLB as a **named entity hierarchy** (Tincal engine request F1,
//! `docs/specs/19-editor.md`, `book/user-guide/gltf-hierarchy.md`).
//!
//! [`load_gltf_model`](crate::mesh_source::load_gltf_model) bakes every node into one merged mesh
//! per material, which is right for a placed prop and wrong for a tool that must address the
//! parts of a model by name (a turret, a socket, a hand bone). [`spawn_gltf_hierarchy`] keeps the
//! file's structure instead:
//!
//! - one entity per scene node, [`Name`]d after the node (`node_<index>` when unnamed), parented
//!   with [`ChildOf`] and carrying the node's **local** [`Transform`];
//! - meshes stay in **node-local space** (nothing is baked), one [`MeshRenderer`] per node and
//!   material (extra materials become named child entities), sharing one material `Arc` per
//!   glTF material;
//! - empty nodes, including the `socket_*` attachment nodes, are plain empty entities
//!   ([`GltfSpawnOptions::keep_sockets`]);
//! - `collision_*` nodes (collision hulls, [`regia_asset::is_collision_node_name`]) keep their
//!   entity, name and transform and get a [`GltfCollisionNode`] marker, but no
//!   [`MeshRenderer`]: they are not drawn and their triangles are not counted (ADR-1622).
//!   [`GltfSpawnOptions::show_collision`] draws them for debugging;
//! - each glTF skin becomes a [`Skeleton`] (through
//!   [`regia_asset_pipeline::gltf_anim::import_gltf_animation`]) bound by a
//!   [`SkinnedMesh`] (spec 17 § 2) whose joint entities **are** the joint nodes' entities;
//!   skinned meshes also get a bind-pose [`SkinJoints`].
//! - a mesh with morph targets gets `regia_render::MeshMorphTargets` (its targets, sparse, per
//!   material part), [`MorphWeights`] at the mesh's default weights and [`MorphTargetNames`]
//!   (`extras.targetNames`, else `target_<i>`), so it draws deformed (Tincal B1/C2).
//! - a node with `EXT_mesh_gpu_instancing` draws through one child entity per instance
//!   (`<node>#instance_<i>`), its local [`Transform`] the instance's, every child sharing the
//!   node's mesh and material `Arc`s, so the renderer instances them (ADR-3016);
//! - a node with `KHR_lights_punctual` carries the engine's [`PointLight`], [`SpotLight`] or
//!   [`DirectionalLight`] ([`GltfSpawnOptions::lights`], ADR-3016). A directional light's direction is
//!   a world-space field, set from the node at spawn, so it does not follow a moved root. The renderer
//!   draws one directional light, so one joins only a world that has none (a warning says otherwise);
//! - a [`GltfSpawn`] with name → entity and entity → node lookups and a [`GltfSpawnReport`].
//!
//! # Axis convention
//!
//! The mesh path ([`load_gltf_model`](crate::mesh_source::load_gltf_model)) applies no axis
//! conversion, so the skin import runs with [`Forward::Keep`]: using the importer's default
//! ([`Forward::Convert`]) would bake a 180° yaw into the root joints and the inverse binds and
//! turn skinned vertices around relative to the rigid ones.
//!
//! # Limits
//!
//! - Skins are read from the file bytes: a `.gltf` whose buffers are external gets bind poses
//!   computed from the joint transforms (no stored inverse binds); a GLB uses the stored ones.
//! - Skeleton space is the (first) skinned mesh node's local space, as in the animation crate,
//!   so a skeleton's root joint is expressed relative to that node. Playing animation through
//!   `regia_animation` rewrites joint transforms in that convention.
//! - Only `JOINTS_0`/`WEIGHTS_0`, triangle lists and the default scene are read, and at most
//!   [`MAX_JOINTS_PER_SKIN`] joints are uploaded.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use regia_animation::{MorphTargetNames, MorphWeights, Skeleton, SkinnedMesh};
use regia_asset_pipeline::Forward;
use regia_asset_pipeline::gltf_anim::{
    GltfAnimSettings, ImportedSkin, import_gltf_animation, skin_slot_joints,
};
use regia_asset_pipeline::gltf_extensions::{GltfLightKind, GltfPunctualLight};
use regia_core::math::{Mat4, Quat, Vec3, Vec4};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_render::{
    CpuMaterial, CpuMesh, DirectionalLight, JointMatrix, LabVertex, MAX_JOINTS_PER_SKIN, MeshRenderer,
    PointLight, SkinJoints, SpotLight,
};
use regia_scene::components::Name;
use regia_scene::prelude::Transform;

use crate::mesh_source::{PrimitiveData, gltf_lab_material, read_primitive};

/// Normalizes a glTF node rotation; a zero-length, non-finite or too-short-to-normalize
/// quaternion becomes identity.
fn safe_rotation(rotation: [f32; 4]) -> Quat {
    Vec4::from_array(rotation)
        .try_normalize()
        .map_or(Quat::IDENTITY, Quat::from_vec4)
}

/// Marks every entity [`spawn_gltf_hierarchy`] created when [`GltfSpawnOptions::editor_only`] is
/// set: tool-owned scaffolding that a level save or a game export should skip.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GltfEditorOnly;

/// Marks the entity of a `collision_*` node ([`regia_asset::is_collision_node_name`]):
/// collision geometry that [`spawn_gltf_hierarchy`] does not draw (ADR-1622), unless
/// [`GltfSpawnOptions::show_collision`] is set.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GltfCollisionNode;

/// Options for [`spawn_gltf_hierarchy`].
#[derive(Debug, Clone)]
pub struct GltfSpawnOptions {
    /// Material for primitives without one (default: a plain lab material).
    pub default_material: Option<Arc<CpuMaterial>>,
    /// Tag every spawned entity with [`GltfEditorOnly`].
    pub editor_only: bool,
    /// Keep empty `socket_*` nodes as empty entities (default true). When false they, and their
    /// descendants, are not spawned.
    pub keep_sockets: bool,
    /// Import skins into [`Skeleton`] + [`SkinnedMesh`] + [`SkinJoints`] (default true). When
    /// false, skinned meshes spawn as rigid meshes at their bind pose.
    pub skins: bool,
    /// Name of the root entity (default: the file stem).
    pub root_name: Option<String>,
    /// Draw the meshes of `collision_*` nodes too (default false: collision hulls are hidden,
    /// as in the cook). A debugging aid; the nodes keep [`GltfCollisionNode`] either way.
    pub show_collision: bool,
    /// Give `KHR_lights_punctual` nodes their light component (default true). When false they are
    /// plain empty entities, for a tool that only wants the geometry.
    pub lights: bool,
}

impl Default for GltfSpawnOptions {
    fn default() -> Self {
        Self {
            default_material: None,
            editor_only: false,
            keep_sockets: true,
            skins: true,
            root_name: None,
            show_collision: false,
            lights: true,
        }
    }
}

/// One spawned glTF node.
#[derive(Debug, Clone)]
pub struct SpawnedNode {
    /// The node's name (`node_<index>` when the file gives none). Not necessarily unique.
    pub name: String,
    /// The node's index in the glTF document.
    pub gltf_index: usize,
    /// The entity carrying the node.
    pub entity: Entity,
    /// The parent entity (the root entity for top-level nodes).
    pub parent: Option<Entity>,
    /// Triangles over every primitive of the node's mesh (0 for an empty node).
    pub triangles: u32,
    /// Names of the glTF materials the node's mesh uses (`default` for none).
    pub materials: Vec<String>,
    /// An empty `socket_*` attachment node.
    pub is_socket: bool,
    /// A `collision_*` node ([`GltfCollisionNode`]); its mesh is drawn only with
    /// [`GltfSpawnOptions::show_collision`], and its triangles count only then.
    pub is_collision: bool,
    /// The glTF skin index the node's mesh is bound to.
    pub skin: Option<usize>,
}

/// What [`spawn_gltf_hierarchy`] built, for a tool's status line or an agent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GltfSpawnReport {
    /// Spawned node entities (not counting the root or material-part children).
    pub nodes: usize,
    /// Entities carrying a mesh (including material-part children).
    pub mesh_entities: usize,
    /// Of those, skinned.
    pub skinned_meshes: usize,
    /// `socket_*` nodes kept.
    pub sockets: usize,
    /// `collision_*` nodes spawned ([`GltfCollisionNode`]).
    pub collision_nodes: usize,
    /// Of those, nodes with a mesh that was left undrawn (0 with
    /// [`GltfSpawnOptions::show_collision`]).
    pub hidden_collision_meshes: usize,
    /// Skeletons imported.
    pub skeletons: usize,
    /// Joints over every skeleton.
    pub joints: usize,
    /// Total triangles.
    pub triangles: u32,
    /// `EXT_mesh_gpu_instancing` instances spawned.
    pub instances: usize,
    /// `KHR_lights_punctual` lights spawned.
    pub lights: usize,
    /// Node names used by more than one node (name lookups return the first).
    pub duplicate_names: Vec<String>,
    /// Non-fatal problems (a skin that could not be imported, a truncated palette).
    pub warnings: Vec<String>,
}

/// A skeleton imported from the file and its joint entities.
#[derive(Debug, Clone)]
pub struct SpawnedSkin {
    /// The glTF skin index.
    pub gltf_index: usize,
    /// The skeleton (joint ids index [`Self::joints`]).
    pub skeleton: Arc<Skeleton>,
    /// Joint entities by skeleton joint id: the joint nodes' own entities.
    pub joints: Vec<Entity>,
}

/// The result of [`spawn_gltf_hierarchy`].
#[derive(Debug, Clone)]
pub struct GltfSpawn {
    /// The root entity every top-level node hangs from (identity transform).
    pub root: Entity,
    /// One record per spawned node, parents before children.
    pub nodes: Vec<SpawnedNode>,
    /// Imported skeletons.
    pub skins: Vec<SpawnedSkin>,
    /// Counts and warnings.
    pub report: GltfSpawnReport,
    by_name: HashMap<String, usize>,
    by_entity: HashMap<Entity, usize>,
}

impl GltfSpawn {
    /// The first node named `name`.
    #[must_use]
    pub fn node(&self, name: &str) -> Option<&SpawnedNode> {
        self.by_name.get(name).map(|&i| &self.nodes[i])
    }

    /// The entity of the first node named `name`.
    #[must_use]
    pub fn entity(&self, name: &str) -> Option<Entity> {
        self.node(name).map(|n| n.entity)
    }

    /// The node an entity belongs to (a material-part child maps to its owner node).
    #[must_use]
    pub fn node_of(&self, entity: Entity) -> Option<&SpawnedNode> {
        self.by_entity.get(&entity).map(|&i| &self.nodes[i])
    }

    /// The node name of an entity.
    #[must_use]
    pub fn name_of(&self, entity: Entity) -> Option<&str> {
        self.node_of(entity).map(|n| n.name.as_str())
    }

    /// Name → entity for every node (the first of equal names).
    #[must_use]
    pub fn name_map(&self) -> BTreeMap<String, Entity> {
        let mut map = BTreeMap::new();
        for node in &self.nodes {
            map.entry(node.name.clone()).or_insert(node.entity);
        }
        map
    }
}

/// One mesh group of a node: the triangles of one material.
struct Group {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    /// Sparse morph targets over [`Self::vertices`] (the mesh's targets, in order).
    morphs: Vec<regia_asset::CpuMorphTarget>,
}

/// Most instance entities one node spawns. A forest of a million trees belongs in the cooked instance
/// set, not in one entity each.
const MAX_SPAWNED_INSTANCES: usize = 1 << 16;

/// Moves each mesh entity's draw onto one child per instance. The children share the `Arc`s of the mesh
/// and material, so the renderer batches them; the node keeps its place in the hierarchy.
fn spawn_instances(
    world: &mut World,
    name: &str,
    mesh_entities: &[Entity],
    locals: &[[f32; 16]],
    opts: &GltfSpawnOptions,
    report: &mut GltfSpawnReport,
) {
    if locals.len() > MAX_SPAWNED_INSTANCES {
        report.warnings.push(format!(
            "node `{name}`: {} instances, {MAX_SPAWNED_INSTANCES} spawned",
            locals.len()
        ));
    }
    for &target in mesh_entities {
        let Some(renderer) = world.get::<MeshRenderer>(target).cloned() else {
            continue;
        };
        let label = world.get::<regia_render::MaterialLabel>(target).cloned();
        for (i, local) in locals.iter().take(MAX_SPAWNED_INSTANCES).enumerate() {
            let (scale, rotation, translation) = Mat4::from_cols_array(local).to_scale_rotation_translation();
            let child = world
                .spawn((
                    Name::new(format!("{name}#instance_{i}")),
                    Transform {
                        translation,
                        rotation,
                        scale,
                    },
                    ChildOf(target),
                    renderer.clone(),
                ))
                .id();
            if let Some(label) = label.clone() {
                world.entity_mut(child).insert(label);
            }
            if opts.editor_only {
                world.entity_mut(child).insert(GltfEditorOnly);
            }
            report.instances += 1;
        }
        world
            .entity_mut(target)
            .remove::<(MeshRenderer, regia_render::MaterialLabel)>();
    }
}

/// Illuminance where a light the file gives no `range` ends, in lux. The engine's point and spot lights
/// need a finite cutoff; this one puts the engine's default point light (800 lm, 10 m) at about 8 m.
const UNBOUNDED_LIGHT_CUTOFF_LUX: f32 = 1.0;

/// The cutoff distance of a light of `candela` the file gives no `range`, by the inverse-square law:
/// `E = I / d²`, so `d = sqrt(I / E)`.
fn unbounded_light_range(candela: f32) -> f32 {
    (candela / UNBOUNDED_LIGHT_CUTOFF_LUX).sqrt().clamp(1.0, 1000.0)
}

/// The world direction a light on a node shines: the node's -Z, or -Z itself for a matrix that has none.
fn light_direction(world: Mat4) -> [f32; 3] {
    world
        .transform_vector3(Vec3::NEG_Z)
        .try_normalize()
        .unwrap_or(Vec3::NEG_Z)
        .to_array()
}

/// Puts a file's light on its node's entity, converting to the engine's units: a point light is in
/// lumens here and candela in the file (`cd = lm / 4π`, `KHR_lights_punctual`); spot candela and
/// directional lux carry over. `position` is unused on a component; the entity's transform places it.
///
/// The renderer draws one directional light, whichever its query meets first, so a model must not take
/// that place from the scene's own sun: a directional light joins only a world with none. Returns whether
/// a light was added.
fn insert_light(world: &mut World, entity: Entity, light: &GltfPunctualLight, node_world: Mat4) -> bool {
    let range = light
        .range
        .unwrap_or_else(|| unbounded_light_range(light.intensity));
    match light.kind {
        GltfLightKind::Directional => {
            if world.query::<&DirectionalLight>().iter(world).next().is_some() {
                return false;
            }
            world.entity_mut(entity).insert(DirectionalLight {
                direction: light_direction(node_world),
                color: light.color,
                illuminance: light.intensity,
                ..DirectionalLight::default()
            });
        }
        GltfLightKind::Point => {
            world.entity_mut(entity).insert(PointLight {
                color: light.color,
                intensity: light.intensity * 4.0 * std::f32::consts::PI,
                radius: range,
                ..PointLight::default()
            });
        }
        GltfLightKind::Spot {
            inner_cone,
            outer_cone,
        } => {
            world.entity_mut(entity).insert(SpotLight {
                color: light.color,
                intensity: light.intensity,
                inner_angle: inner_cone,
                outer_angle: outer_cone,
                range,
                ..SpotLight::default()
            });
        }
    }
    true
}

/// Spawns `path` (glTF or GLB) as a named entity hierarchy under a new root entity. See the
/// module docs for the layout.
///
/// # Errors
///
/// Not a readable glTF, or no scene.
pub fn spawn_gltf_hierarchy(
    world: &mut World,
    path: &Path,
    opts: &GltfSpawnOptions,
) -> Result<GltfSpawn, String> {
    let _span = tracing::info_span!("spawn_gltf_hierarchy").entered();
    let regia_asset_pipeline::GltfOpened {
        document,
        buffers,
        images,
        load_data,
    } = regia_asset_pipeline::open_gltf(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let authored = regia_asset_pipeline::gltf_materials(path).map_err(|err| err.to_string())?;
    let scene = document
        .default_scene()
        .or_else(|| document.scenes().next())
        .ok_or_else(|| format!("{}: no scene", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mesh")
        .to_string();
    let default_material = opts.default_material.clone().unwrap_or_default();
    let source_bytes = std::fs::read(path).unwrap_or_default();
    // Morph target names (`meshes[i].extras.targetNames`); the parsed document drops extras.
    let target_names = regia_asset_pipeline::morph_target_names(&source_bytes);
    let lights = opts
        .lights
        .then(|| regia_asset_pipeline::gltf_extensions::punctual_lights(&source_bytes));

    let mut report = GltfSpawnReport::default();
    let imported_skins = if opts.skins && document.skins().len() > 0 {
        import_skins(path, &mut report)
    } else {
        Vec::new()
    };
    // Per glTF skin: slot -> skeleton joint id, as the importer laid the skeleton out.
    let slot_maps: Vec<Vec<u32>> = if imported_skins.is_empty() {
        Vec::new()
    } else {
        document
            .skins()
            .map(|skin| joint_slots(&document, &skin))
            .collect()
    };

    // Materials shared per glTF material index.
    let mut materials: HashMap<Option<usize>, (Arc<CpuMaterial>, String)> = HashMap::new();
    let mut material_for = |index: Option<usize>| -> (Arc<CpuMaterial>, String) {
        materials
            .entry(index)
            .or_insert_with(|| match index.and_then(|i| authored.get(i).map(|m| (i, m))) {
                Some((i, asset)) => {
                    let gltf_name = document
                        .materials()
                        .nth(i)
                        .and_then(|m| m.name().map(str::to_string))
                        .unwrap_or_else(|| format!("material {i}"));
                    let material =
                        gltf_lab_material(&format!("{file_name}#{gltf_name}"), asset, &document, i, &images);
                    (Arc::new(material), gltf_name)
                }
                None => (Arc::clone(&default_material), "default".to_string()),
            })
            .clone()
    };

    let root_name = opts.root_name.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("gltf")
            .to_string()
    });
    let root = world.spawn((Name::new(root_name), Transform::IDENTITY)).id();
    if opts.editor_only {
        world.entity_mut(root).insert(GltfEditorOnly);
    }

    let mut nodes: Vec<SpawnedNode> = Vec::new();
    let mut node_entity: HashMap<usize, Entity> = HashMap::new();
    let mut extra_owner: Vec<(Entity, usize)> = Vec::new();
    // Skinned mesh entities waiting for their skeleton binding (node index, skin index).
    let mut skinned_nodes: Vec<(usize, usize, Vec<Entity>)> = Vec::new();

    let mut stack: Vec<(gltf::Node<'_>, Entity, Mat4)> = scene
        .nodes()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|node| (node, root, Mat4::IDENTITY))
        .collect();
    while let Some((node, parent, parent_world)) = stack.pop() {
        let index = node.index();
        let name = node
            .name()
            .filter(|n| !n.is_empty())
            .map_or_else(|| format!("node_{index}"), str::to_string);
        let is_socket = node.mesh().is_none() && name.starts_with("socket_");
        if is_socket && !opts.keep_sockets {
            continue;
        }
        let (translation, rotation, scale) = node.transform().decomposed();
        let local_matrix = Mat4::from_cols_array_2d(&node.transform().matrix());
        let world_matrix = parent_world * local_matrix;
        // A zero or non-finite quaternion in the file cannot be normalized: keep the node, drop the rotation.
        if Vec4::from_array(rotation).try_normalize().is_none() {
            report.warnings.push(format!(
                "node `{name}`: invalid rotation quaternion, using identity"
            ));
        }
        let rotation = safe_rotation(rotation);
        let entity = world
            .spawn((
                Name::new(name.clone()),
                Transform {
                    translation: Vec3::from_array(translation),
                    rotation,
                    scale: Vec3::from_array(scale),
                },
            ))
            .id();
        world.entity_mut(entity).insert(ChildOf(parent));
        node_entity.insert(index, entity);
        if is_socket {
            report.sockets += 1;
        }
        let is_collision = regia_asset::is_collision_node_name(&name);
        if is_collision {
            world.entity_mut(entity).insert(GltfCollisionNode);
            report.collision_nodes += 1;
        }
        let draw_mesh = !is_collision || opts.show_collision;
        if !draw_mesh && node.mesh().is_some() {
            report.hidden_collision_meshes += 1;
        }

        let skin_index = if opts.skins {
            node.skin().map(|s| s.index())
        } else {
            None
        };
        let imported = skin_index.and_then(|i| imported_skins.get(i).and_then(Option::as_ref));
        let mut triangles = 0u32;
        let mut material_names: Vec<String> = Vec::new();
        if let Some(mesh) = node.mesh().filter(|_| draw_mesh) {
            let skinned = imported.is_some();
            let joint_remap: &[u32] = match (imported, node.skin()) {
                (Some(_), Some(gltf_skin)) => slot_maps.get(gltf_skin.index()).map_or(&[], Vec::as_slice),
                _ => &[],
            };
            let flip = !skinned && world_matrix.determinant() < 0.0;
            let mut groups: BTreeMap<Option<usize>, Group> = BTreeMap::new();
            for primitive in mesh.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                let Some(PrimitiveData {
                    positions,
                    normals,
                    uvs,
                    colors,
                    tangents,
                    indices,
                    joints,
                    weights,
                }) = read_primitive(&primitive, &buffers, skinned)
                else {
                    continue;
                };
                let group = groups
                    .entry(primitive.material().index())
                    .or_insert_with(|| Group {
                        vertices: Vec::new(),
                        indices: Vec::new(),
                        morphs: Vec::new(),
                    });
                let base = group.vertices.len() as u32;
                let count = positions.len();
                add_morph_targets(
                    &mut group.morphs,
                    &primitive,
                    &buffers,
                    base,
                    count,
                    target_names.get(mesh.index()).map_or(&[][..], Vec::as_slice),
                );
                for i in 0..count {
                    let (joint_ids, joint_weights) = if skinned && !joints.is_empty() {
                        remap_influences(joints[i], weights[i], joint_remap)
                    } else {
                        ([0; 4], [0.0; 4])
                    };
                    group.vertices.push(LabVertex {
                        position: positions[i],
                        normal: normals[i],
                        uv: uvs[i],
                        tangent: tangents[i],
                        color: colors.get(i).copied().unwrap_or([1.0; 4]),
                        joints: joint_ids,
                        weights: joint_weights,
                    });
                }
                for tri in indices.as_chunks::<3>().0 {
                    if tri.iter().any(|&i| i as usize >= count) {
                        continue;
                    }
                    if flip {
                        group
                            .indices
                            .extend_from_slice(&[base + tri[0], base + tri[2], base + tri[1]]);
                    } else {
                        group
                            .indices
                            .extend_from_slice(&[base + tri[0], base + tri[1], base + tri[2]]);
                    }
                }
            }
            let mut first = true;
            let mut mesh_entities = Vec::new();
            for (material_index, group) in groups {
                if group.indices.is_empty() {
                    continue;
                }
                triangles += (group.indices.len() / 3) as u32;
                let (material, material_name) = material_for(material_index);
                if !material_names.contains(&material_name) {
                    material_names.push(material_name.clone());
                }
                let morphs = morph_components(&group, mesh.weights(), &mut report);
                let renderer = MeshRenderer::new(
                    Arc::new(CpuMesh {
                        vertices: group.vertices,
                        indices: group.indices,
                    }),
                    material,
                );
                let target = if first {
                    first = false;
                    entity
                } else {
                    let part = world
                        .spawn((
                            Name::new(format!("{name}#{material_name}")),
                            Transform::IDENTITY,
                            ChildOf(entity),
                        ))
                        .id();
                    extra_owner.push((part, index));
                    part
                };
                world
                    .entity_mut(target)
                    .insert((renderer, regia_render::MaterialLabel(material_name.clone())));
                if let Some(morphs) = morphs {
                    // The source node, so per-node morph tracks find this entity (ADR-297).
                    let source = regia_animation::SourceNode {
                        index: index as u32,
                        name: node.name().unwrap_or_default().to_string(),
                    };
                    world.entity_mut(target).insert((morphs, source));
                }
                mesh_entities.push(target);
                report.mesh_entities += 1;
                if opts.editor_only {
                    world.entity_mut(target).insert(GltfEditorOnly);
                }
            }
            if let (Some(skin), Some(skin_index)) = (imported, skin_index)
                && !mesh_entities.is_empty()
            {
                report.skinned_meshes += mesh_entities.len();
                let palette = bind_palette(&skin.skeleton, &mut report);
                for &target in &mesh_entities {
                    world.entity_mut(target).insert(SkinJoints {
                        palette: palette.clone(),
                    });
                }
                skinned_nodes.push((index, skin_index, mesh_entities));
            } else if let Some(locals) = load_data.instances.get(&index) {
                triangles = triangles.saturating_mul(u32::try_from(locals.len()).unwrap_or(u32::MAX));
                spawn_instances(world, &name, &mesh_entities, locals, opts, &mut report);
            }
        }
        if let Some((lights, node_lights)) = &lights
            && let Some(light) = node_lights
                .iter()
                .find(|link| link.node == index)
                .and_then(|link| lights.get(link.light))
        {
            if insert_light(world, entity, light, world_matrix) {
                report.lights += 1;
            } else {
                report.warnings.push(format!(
                    "node `{name}`: the scene already has a directional light and the renderer draws one; this one was left out"
                ));
            }
        }
        report.triangles += triangles;
        report.nodes += 1;
        if opts.editor_only {
            world.entity_mut(entity).insert(GltfEditorOnly);
        }
        nodes.push(SpawnedNode {
            name,
            gltf_index: index,
            entity,
            parent: Some(parent),
            triangles,
            materials: material_names,
            is_socket,
            is_collision,
            skin: skin_index,
        });
        for child in node.children().collect::<Vec<_>>().into_iter().rev() {
            stack.push((child, entity, world_matrix));
        }
    }

    // Skeleton bindings: joint nodes' entities are the joints.
    let mut skins = Vec::new();
    for (skin_index, imported) in imported_skins.iter().enumerate() {
        let Some(imported) = imported else { continue };
        let Some(gltf_skin) = document.skins().nth(skin_index) else {
            continue;
        };
        let Some(slots) = slot_maps.get(skin_index) else {
            continue;
        };
        let mut joints = vec![None; imported.skeleton.len()];
        for (slot, joint_node) in gltf_skin.joints().enumerate() {
            let (Some(&id), Some(&entity)) = (slots.get(slot), node_entity.get(&joint_node.index())) else {
                continue;
            };
            if let Some(entry) = joints.get_mut(id as usize) {
                *entry = Some(entity);
            }
        }
        if joints.iter().any(Option::is_none) {
            report.warnings.push(format!(
                "skin {skin_index}: some joint nodes are outside the default scene; the skeleton binding was skipped"
            ));
            continue;
        }
        let joints: Vec<Entity> = joints.into_iter().flatten().collect();
        let skeleton = Arc::new(imported.skeleton.clone());
        report.skeletons += 1;
        report.joints += joints.len();
        for (_, mesh_skin, targets) in &skinned_nodes {
            if *mesh_skin != skin_index {
                continue;
            }
            for &target in targets {
                world.entity_mut(target).insert(SkinnedMesh {
                    skeleton: Arc::clone(&skeleton),
                    joints: joints.clone(),
                });
            }
        }
        skins.push(SpawnedSkin {
            gltf_index: skin_index,
            skeleton,
            joints,
        });
    }

    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut by_entity: HashMap<Entity, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if by_name.contains_key(&node.name) {
            if !report.duplicate_names.contains(&node.name) {
                report.duplicate_names.push(node.name.clone());
            }
        } else {
            by_name.insert(node.name.clone(), i);
        }
        by_entity.insert(node.entity, i);
    }
    for (part, owner_index) in extra_owner {
        if let Some(&i) = nodes.iter().position(|n| n.gltf_index == owner_index).as_ref() {
            by_entity.insert(part, i);
        }
    }
    regia_scene::prelude::propagate_transforms(world);
    Ok(GltfSpawn {
        root,
        nodes,
        skins,
        report,
        by_name,
        by_entity,
    })
}

/// Imports the file's skins, one `Option` per glTF skin (`None` when it could not be read).
fn import_skins(path: &Path, report: &mut GltfSpawnReport) -> Vec<Option<ImportedSkin>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            report
                .warnings
                .push(format!("skins skipped: {}: {err}", path.display()));
            return Vec::new();
        }
    };
    let settings = GltfAnimSettings {
        forward: Forward::Keep,
        ..GltfAnimSettings::default()
    };
    match import_gltf_animation(&bytes, &settings) {
        Ok(import) => import.skins.into_iter().map(Some).collect(),
        Err(err) => {
            report.warnings.push(format!("skins skipped: {err}"));
            Vec::new()
        }
    }
}

/// Maps each slot of a glTF skin's joint list to the skeleton joint id the importer gave that
/// node. It asks the importer for its own slot map ([`skin_slot_joints`]) rather than guessing
/// from names: a joint literally called `{name}_{node}` would otherwise steal the slot of the
/// node the importer renamed. Empty when the skin has no layout (its import failed as well).
fn joint_slots(document: &gltf::Document, skin: &gltf::Skin<'_>) -> Vec<u32> {
    skin_slot_joints(document, skin).unwrap_or_default()
}

/// Renumbers one vertex's four glTF joint slots to skeleton joint ids.
fn remap_influences(joints: [u16; 4], weights: [f32; 4], remap: &[u32]) -> ([u32; 4], [f32; 4]) {
    let ids = joints.map(|joint| remap.get(usize::from(joint)).copied().unwrap_or(0));
    (ids, weights)
}

/// The bind-pose palette (`global · inverse bind`), capped at [`MAX_JOINTS_PER_SKIN`]. The
/// skeleton lives in the skinned node's local space and the renderer multiplies the palette
/// by the entity's model matrix afterwards, so the node's own world transform must not be
/// folded in here (it would apply twice).
fn bind_palette(skeleton: &Skeleton, report: &mut GltfSpawnReport) -> Vec<JointMatrix> {
    let globals = skeleton.globals(&skeleton.bind_pose_pose(), None);
    let mut palette: Vec<JointMatrix> = skeleton
        .skin_palette(&globals)
        .into_iter()
        .map(JointMatrix::from_mat4)
        .collect();
    if palette.len() > MAX_JOINTS_PER_SKIN {
        report.warnings.push(format!(
            "skeleton has {} joints; only the first {MAX_JOINTS_PER_SKIN} are uploaded",
            palette.len()
        ));
        palette.truncate(MAX_JOINTS_PER_SKIN);
    }
    palette
}
/// Appends one primitive's glTF morph targets (dense deltas) to a part's sparse targets, its
/// vertices starting at `base`. Every primitive of a glTF mesh has the same targets.
fn add_morph_targets(
    morphs: &mut Vec<regia_asset::CpuMorphTarget>,
    primitive: &gltf::Primitive<'_>,
    buffers: &[gltf::buffer::Data],
    base: u32,
    count: usize,
    names: &[String],
) {
    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data.0[..]));
    for (index, (positions, normals, tangents)) in reader.read_morph_targets().enumerate() {
        if morphs.len() <= index {
            morphs.resize_with(index + 1, || regia_asset::CpuMorphTarget {
                name: String::new(),
                vertex_indices: Vec::new(),
                position_deltas: Vec::new(),
                normal_deltas: Vec::new(),
                tangent_deltas: Vec::new(),
            });
        }
        let target = &mut morphs[index];
        if target.name.is_empty() {
            target.name = names
                .get(index)
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("target_{index}"));
        }
        let (positions, normals, tangents) = (dense(positions), dense(normals), dense(tangents));
        let at = |deltas: &[[f32; 3]], i: usize| deltas.get(i).copied().unwrap_or([0.0; 3]);
        for i in 0..count {
            let (p, n, t) = (at(&positions, i), at(&normals, i), at(&tangents, i));
            if p.iter().chain(&n).chain(&t).all(|v| *v == 0.0) {
                continue;
            }
            target.vertex_indices.push(base + i as u32);
            target.position_deltas.push(p);
            target.normal_deltas.push(n);
            target.tangent_deltas.push(t);
        }
    }
}

/// A glTF displacement stream as a vector (empty when absent).
fn dense(deltas: Option<impl Iterator<Item = [f32; 3]>>) -> Vec<[f32; 3]> {
    deltas.map(Iterator::collect).unwrap_or_default()
}

/// The morph components of a part with targets: [`regia_render::MeshMorphTargets`],
/// [`MorphWeights`] at the mesh's default weights (glTF `mesh.weights`, else zero) and
/// [`MorphTargetNames`]. `None` without targets, or (with a warning) when they do not pack.
fn morph_components(
    group: &Group,
    defaults: Option<&[f32]>,
    report: &mut GltfSpawnReport,
) -> Option<(regia_render::MeshMorphTargets, MorphWeights, MorphTargetNames)> {
    if group.morphs.is_empty() {
        return None;
    }
    match regia_render::MorphTargets::new(&group.vertices, &group.morphs) {
        Ok(targets) => {
            let count = targets.len();
            let names = MorphTargetNames::new(targets.names().iter().cloned());
            let mut weights = vec![0.0; count];
            for (w, d) in weights.iter_mut().zip(defaults.unwrap_or(&[])) {
                *w = *d;
            }
            Some((
                regia_render::MeshMorphTargets::new(Arc::new(targets)),
                MorphWeights(weights),
                names,
            ))
        }
        Err(err) => {
            report.warnings.push(format!("morph targets skipped: {err}"));
            None
        }
    }
}

#[cfg(test)]
mod safe_rotation_tests {
    use super::*;

    #[test]
    fn degenerate_rotations_become_identity() {
        assert_eq!(safe_rotation([0.0; 4]), Quat::IDENTITY);
        assert_eq!(safe_rotation([f32::NAN, 0.0, 0.0, 1.0]), Quat::IDENTITY);
        let q = safe_rotation([0.0, 0.0, 0.0, 2.0]);
        assert!((q.length() - 1.0).abs() < 1e-6);
    }
}
