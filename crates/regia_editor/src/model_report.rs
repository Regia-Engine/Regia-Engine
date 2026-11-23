// SPDX-License-Identifier: MPL-2.0
//! Model map and node report (Tincal engine request F14, `docs/specs/19-editor.md`,
//! `book/user-guide/model-report.md`).
//!
//! [`ModelReport::from_file`] reads a glTF/GLB and answers the questions an export check or a
//! "final asset" view asks: the node tree with per-node triangle, vertex and material counts,
//! skin and joint counts, which texture slot of which material is bound and how large it is,
//! duplicate node names, nodes the default scene never reaches, and which budget limits of the
//! chosen [`ModelClass`] the model breaks. [`ModelReport::check_hierarchy`] (and
//! [`ModelReport::from_spawn`]) additionally compare the report with a spawned entity hierarchy
//! ([`crate::gltf_hierarchy`]) so a node that was despawned, or an entity that is not in the
//! file, shows up as an orphan.
//!
//! The report is pure CPU: it reads accessor counts and image dimensions (`from_file` decodes the images through `gltf::import`), never the GPU. It
//! serializes to JSON ([`ModelReport::to_json`]) and has a compact log rendering
//! ([`ModelReport::to_text`]).
//!
//! # Conventions
//!
//! - A node without a name is `node_<index>`, as in [`crate::gltf_hierarchy`]; a material without
//!   one is `material <index>`; a primitive without a material uses `default`.
//! - Triangles count triangle lists, strips and fans of the node's own mesh (not its children).
//!   A mesh instanced by two nodes counts once per node.
//! - `Orm` is the glTF metallic-roughness texture; `Occlusion` is reported separately because
//!   exporters may point both at one packed image.
//! - Texture memory is an estimate: width × height × 4 bytes × 4/3 for the mip chain, once per
//!   distinct image the materials in use reference.
//! - The numbers in [`ModelClass::limits`] are engine defaults (ADR-234), not a standard; hosts
//!   override any field of [`BudgetLimits`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use crate::viewport_agent::Handler;
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_scene::components::Name;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gltf_hierarchy::GltfSpawn;

/// The kind of model, which picks the default [`BudgetLimits`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelClass {
    /// A placed prop: crates, furniture, hand-held items.
    #[default]
    Prop,
    /// A skinned character or creature.
    Character,
    /// A vehicle or other large articulated machine.
    Vehicle,
    /// A building or other large static structure.
    Building,
    /// A hero hull: a heavy machine the player looks at close up (ADR-511). LOD0 up to 350k
    /// triangles, LOD2 under 15k and LOD3 under 4k, at most 4 materials and one glass material.
    /// Light skimmers (120-180k at LOD0) also fit this class; see `book/user-guide/model-report.md`.
    Hero,
}

impl ModelClass {
    /// Every class.
    pub const ALL: [Self; 5] = [
        Self::Prop,
        Self::Character,
        Self::Vehicle,
        Self::Building,
        Self::Hero,
    ];

    /// The snake_case name used in JSON and by the agent method.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Prop => "prop",
            Self::Character => "character",
            Self::Vehicle => "vehicle",
            Self::Building => "building",
            Self::Hero => "hero",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.name() == name)
    }

    /// The default limits of the class.
    #[must_use]
    pub const fn limits(self) -> BudgetLimits {
        const MIB: u64 = 1024 * 1024;
        // The LOD and glass checks are off for the classes that predate them.
        const NO_LOD_LIMITS: BudgetLimits = BudgetLimits {
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
        };
        match self {
            Self::Prop => BudgetLimits {
                max_triangles: 20_000,
                max_node_triangles: 10_000,
                max_nodes: 64,
                max_materials: 4,
                max_materials_per_node: 2,
                max_joints_per_skin: 256,
                max_texture_dimension: 2048,
                max_texture_bytes: 32 * MIB,
                require_normal_maps: false,
                ..NO_LOD_LIMITS
            },
            Self::Character => BudgetLimits {
                max_triangles: 60_000,
                max_node_triangles: 40_000,
                max_nodes: 256,
                max_materials: 8,
                max_materials_per_node: 3,
                max_joints_per_skin: 256,
                max_texture_dimension: 4096,
                max_texture_bytes: 128 * MIB,
                require_normal_maps: false,
                ..NO_LOD_LIMITS
            },
            Self::Vehicle => BudgetLimits {
                max_triangles: 150_000,
                max_node_triangles: 60_000,
                max_nodes: 512,
                max_materials: 12,
                max_materials_per_node: 4,
                max_joints_per_skin: 256,
                max_texture_dimension: 4096,
                max_texture_bytes: 192 * MIB,
                require_normal_maps: false,
                ..NO_LOD_LIMITS
            },
            Self::Building => BudgetLimits {
                max_triangles: 250_000,
                max_node_triangles: 100_000,
                max_nodes: 1024,
                max_materials: 16,
                max_materials_per_node: 6,
                max_joints_per_skin: 256,
                max_texture_dimension: 4096,
                max_texture_bytes: 256 * MIB,
                require_normal_maps: false,
                ..NO_LOD_LIMITS
            },
            Self::Hero => BudgetLimits {
                max_triangles: 350_000,
                max_node_triangles: 350_000,
                max_nodes: 512,
                max_materials: 4,
                max_materials_per_node: 4,
                max_joints_per_skin: 256,
                max_texture_dimension: 4096,
                max_texture_bytes: 256 * MIB,
                require_normal_maps: false,
                lod1_permille: 200,
                lod2_permille: 50,
                lod3_permille: 12,
                lod_ratio_tolerance_percent: 25,
                max_lod2_triangles: 14_999,
                max_lod3_triangles: 3_999,
                max_glass_materials: 1,
            },
        }
    }
}

/// Per-class budget limits. A limit of 0 turns that check off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetLimits {
    /// Triangles over the whole model.
    pub max_triangles: u64,
    /// Triangles on any one node.
    pub max_node_triangles: u64,
    /// Nodes in the glTF document.
    pub max_nodes: u64,
    /// Materials in use.
    pub max_materials: u64,
    /// Distinct materials on one node.
    pub max_materials_per_node: u64,
    /// Joints in any one skin (the runtime caps a skin at 256).
    pub max_joints_per_skin: u64,
    /// Largest texture side in pixels.
    pub max_texture_dimension: u64,
    /// Estimated texture memory with mips, in bytes.
    pub max_texture_bytes: u64,
    /// Flag a material in use that has no normal map.
    pub require_normal_maps: bool,
    /// Expected LOD1 triangles as parts per thousand of LOD0 (200 = 20 %). With the tolerance
    /// below this is a ceiling; 0 turns the check off.
    pub lod1_permille: u64,
    /// Expected LOD2 triangles as parts per thousand of LOD0 (50 = 5 %).
    pub lod2_permille: u64,
    /// Expected LOD3 triangles as parts per thousand of LOD0 (12 = 1.2 %).
    pub lod3_permille: u64,
    /// How far above the expected ratio a LOD may be before it is flagged, in percent.
    pub lod_ratio_tolerance_percent: u64,
    /// Hard ceiling for LOD2 triangles (the rule "LOD2 under 15k" is a limit of 14 999).
    pub max_lod2_triangles: u64,
    /// Hard ceiling for LOD3 triangles (the rule "LOD3 under 4k" is a limit of 3 999).
    pub max_lod3_triangles: u64,
    /// Glass materials in use (transmission extension or a name containing `glass`).
    pub max_glass_materials: u64,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        ModelClass::Prop.limits()
    }
}

impl BudgetLimits {
    /// Applies the fields present in a JSON object over `self` (unknown keys are errors).
    ///
    /// # Errors
    ///
    /// A key that is not a limit name, or a value of the wrong type.
    pub fn with_overrides(self, overrides: &Value) -> Result<Self, String> {
        let Some(map) = overrides.as_object() else {
            return Err("`limits` must be an object".to_string());
        };
        let mut merged = serde_json::to_value(self).map_err(|err| err.to_string())?;
        let target = merged.as_object_mut().ok_or("limits are not an object")?;
        for (key, value) in map {
            if !target.contains_key(key) {
                return Err(format!("unknown limit `{key}`"));
            }
            target.insert(key.clone(), value.clone());
        }
        serde_json::from_value(merged).map_err(|err| format!("limits: {err}"))
    }
}

/// A texture slot of a material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapSlot {
    /// Base color (albedo).
    BaseColor,
    /// Tangent-space normal map.
    Normal,
    /// The glTF metallic-roughness texture (the packed ORM map when occlusion shares it).
    Orm,
    /// The occlusion texture.
    Occlusion,
    /// Emissive color.
    Emissive,
}

impl MapSlot {
    /// Every slot, in report order.
    pub const ALL: [Self; 5] = [
        Self::BaseColor,
        Self::Normal,
        Self::Orm,
        Self::Occlusion,
        Self::Emissive,
    ];

    /// The snake_case name used in JSON and text.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BaseColor => "base_color",
            Self::Normal => "normal",
            Self::Orm => "orm",
            Self::Occlusion => "occlusion",
            Self::Emissive => "emissive",
        }
    }
}

/// One node of the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    /// The node's index in the glTF document.
    pub gltf_index: usize,
    /// The node's name (`node_<index>` when unnamed).
    pub name: String,
    /// The parent node's index, `None` for a root.
    pub parent: Option<usize>,
    /// Child node indices, in file order.
    pub children: Vec<usize>,
    /// Depth below the node's root (0 for a root).
    pub depth: u32,
    /// Triangles of the node's own mesh.
    pub triangles: u64,
    /// Vertices of the node's own mesh (position accessor counts).
    pub vertices: u64,
    /// Names of the materials the node's mesh uses, in first-use order.
    pub materials: Vec<String>,
    /// Indices of those materials in [`ModelReport::materials`] (`None` for `default`).
    pub material_indices: Vec<Option<usize>>,
    /// The glTF skin index the node is bound to.
    pub skin: Option<usize>,
    /// Joints of that skin.
    pub joints: u64,
    /// The node carries no mesh and is named `socket_*`.
    pub is_socket: bool,
    /// The node is a `collision_*` physics proxy (`regia_asset::is_collision_node_name`): its
    /// triangles and vertices stay out of the model totals and the budget flags and appear in
    /// [`ModelTotals::collision_triangles`] instead.
    #[serde(default)]
    pub is_collision: bool,
    /// The node is a joint of some skin.
    pub is_joint: bool,
    /// The default scene reaches the node.
    pub in_scene: bool,
}

/// One skin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkinReport {
    /// The glTF skin index.
    pub gltf_index: usize,
    /// The skin's name (`skin_<index>` when unnamed).
    pub name: String,
    /// Joints in the skin.
    pub joints: u64,
    /// Nodes (indices) whose mesh is bound to the skin.
    pub meshes: Vec<usize>,
}

/// One material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialReport {
    /// The glTF material index.
    pub gltf_index: usize,
    /// The material's name (`material <index>` when unnamed).
    pub name: String,
    /// Nodes (indices) whose mesh uses it.
    pub nodes: Vec<usize>,
    /// Triangles drawn with it.
    pub triangles: u64,
    /// The material is glass: it uses `KHR_materials_transmission` or its name contains
    /// `glass` (any case).
    #[serde(default)]
    pub glass: bool,
}

/// One level of detail, found by the `_LOD<n>` node-name suffix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LodReport {
    /// The level (0 is the full-detail mesh).
    pub level: u32,
    /// Triangles of every node tagged with the level, their descendants, and (for level 0) every
    /// node that no `_LOD<n>` node contains.
    pub triangles: u64,
    /// Mesh nodes counted into the level.
    pub mesh_nodes: u64,
}

/// One texture slot of one material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapReport {
    /// The material's name.
    pub material: String,
    /// The glTF material index.
    pub material_index: usize,
    /// The slot.
    pub slot: MapSlot,
    /// A texture is assigned to the slot.
    pub bound: bool,
    /// The image size in pixels, when bound and decodable.
    pub size: Option<(u32, u32)>,
    /// The glTF image index, when bound.
    pub image: Option<usize>,
    /// A node's mesh uses the material.
    pub used: bool,
}

/// Which budget a [`BudgetFlag`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKind {
    /// Total triangles.
    Triangles,
    /// Triangles on one node.
    NodeTriangles,
    /// Node count.
    Nodes,
    /// Materials in use.
    Materials,
    /// Distinct materials on one node.
    MaterialsPerNode,
    /// Joints in one skin.
    Joints,
    /// A texture side.
    TextureSize,
    /// Estimated texture memory.
    TextureMemory,
    /// A material in use without a normal map.
    MissingNormalMap,
    /// A LOD over its hard triangle ceiling.
    LodTriangles,
    /// A LOD well above its expected share of LOD0.
    LodRatio,
    /// More glass materials in use than the class allows.
    GlassMaterials,
}

/// A budget limit the model breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetFlag {
    /// The budget.
    pub kind: FlagKind,
    /// What exceeded it: `model`, a node name, a skin name, `material/slot`.
    pub subject: String,
    /// The measured value.
    pub value: u64,
    /// The limit (0 for a presence check).
    pub limit: u64,
    /// A sentence for a log.
    pub message: String,
}

/// A node that is not where the file puts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanNode {
    /// The glTF node index (`None` for an entity that is not in the file).
    pub gltf_index: Option<usize>,
    /// The node or entity name.
    pub name: String,
    /// Why it is an orphan.
    pub reason: String,
}

/// Node names used by more than one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateName {
    /// The name.
    pub name: String,
    /// The nodes (indices) that carry it.
    pub nodes: Vec<usize>,
}

/// Whole-model totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTotals {
    /// Nodes in the document.
    pub nodes: u64,
    /// Nodes the default scene reaches.
    pub nodes_in_scene: u64,
    /// Nodes with a mesh.
    pub mesh_nodes: u64,
    /// Triangles drawn at once: every node, or only LOD level 0 when the model has LODs
    /// (ADR-511).
    pub triangles: u64,
    /// Vertices over every node.
    pub vertices: u64,
    /// Materials in use.
    pub materials_in_use: u64,
    /// Skins.
    pub skins: u64,
    /// Joints over every skin.
    pub joints: u64,
    /// Distinct images used by materials in use.
    pub textures: u64,
    /// Estimated texture memory with mips, in bytes.
    pub texture_bytes: u64,
    /// Largest texture side in pixels.
    pub max_texture_dimension: u64,
    /// Glass materials in use.
    #[serde(default)]
    pub glass_materials: u64,
    /// `collision_*` nodes (physics proxies, not drawn): counted here and not in `nodes`-based
    /// mesh totals.
    #[serde(default)]
    pub collision_nodes: u64,
    /// Triangles of the `collision_*` nodes, excluded from [`Self::triangles`].
    #[serde(default)]
    pub collision_triangles: u64,
    /// Vertices of the `collision_*` nodes, excluded from [`Self::vertices`].
    #[serde(default)]
    pub collision_vertices: u64,
}

/// The model map and node report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReport {
    /// Where the model came from (the path as given).
    pub source: String,
    /// The class whose limits were applied.
    pub class: ModelClass,
    /// The limits applied.
    pub limits: BudgetLimits,
    /// Every node of the document, by glTF index.
    pub nodes: Vec<NodeReport>,
    /// Root nodes of the default scene.
    pub roots: Vec<usize>,
    /// Skins.
    pub skins: Vec<SkinReport>,
    /// Materials.
    pub materials: Vec<MaterialReport>,
    /// Levels of detail, empty when no node carries a `_LOD<n>` suffix.
    #[serde(default)]
    pub lods: Vec<LodReport>,
    /// Every slot of every material.
    pub maps: Vec<MapReport>,
    /// Totals.
    pub totals: ModelTotals,
    /// Budget limits the model breaks.
    pub flags: Vec<BudgetFlag>,
    /// Names shared by more than one node.
    pub duplicate_names: Vec<DuplicateName>,
    /// Nodes outside the default scene, and (after a hierarchy check) missing or extra entities.
    pub orphans: Vec<OrphanNode>,
}

impl ModelReport {
    /// Reads `path` (glTF or GLB) and reports it against `class`'s default limits.
    ///
    /// # Errors
    ///
    /// Not a readable glTF.
    pub fn from_file(path: &Path, class: ModelClass) -> Result<Self, String> {
        Self::from_file_with(path, class, class.limits())
    }

    /// [`Self::from_file`] with explicit limits.
    ///
    /// # Errors
    ///
    /// Not a readable glTF.
    pub fn from_file_with(path: &Path, class: ModelClass, limits: BudgetLimits) -> Result<Self, String> {
        let _span = tracing::info_span!("model_report").entered();
        let regia_asset_pipeline::GltfOpened { document, images, .. } =
            regia_asset_pipeline::open_gltf(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let sizes: Vec<(u32, u32)> = images.iter().map(|image| (image.width, image.height)).collect();
        Ok(Self::from_document(
            &path.display().to_string(),
            &document,
            &sizes,
            class,
            limits,
        ))
    }

    /// Reports an already parsed document. `image_sizes[i]` is the size of glTF image `i`.
    #[must_use]
    pub fn from_document(
        source: &str,
        document: &gltf::Document,
        image_sizes: &[(u32, u32)],
        class: ModelClass,
        limits: BudgetLimits,
    ) -> Self {
        let node_count = document.nodes().len();
        let node_names: Vec<String> = document
            .nodes()
            .map(|node| {
                node.name()
                    .filter(|n| !n.is_empty())
                    .map_or_else(|| format!("node_{}", node.index()), str::to_string)
            })
            .collect();
        let material_names: Vec<String> = document
            .materials()
            .map(|m| {
                m.name()
                    .map_or_else(|| format!("material {}", m.index().unwrap_or(0)), str::to_string)
            })
            .collect();

        // Tree structure.
        let mut parent: Vec<Option<usize>> = vec![None; node_count];
        for node in document.nodes() {
            for child in node.children() {
                parent[child.index()] = Some(node.index());
            }
        }
        let scene = document.default_scene().or_else(|| document.scenes().next());
        let roots: Vec<usize> = scene
            .as_ref()
            .map(|s| s.nodes().map(|n| n.index()).collect())
            .unwrap_or_default();
        let mut depth = vec![0u32; node_count];
        let mut in_scene = vec![false; node_count];
        let mut stack: Vec<(usize, u32)> = roots.iter().rev().map(|&r| (r, 0)).collect();
        while let Some((index, d)) = stack.pop() {
            if in_scene[index] {
                continue; // a malformed shared child: visit once
            }
            in_scene[index] = true;
            depth[index] = d;
            if let Some(node) = document.nodes().nth(index) {
                for child in node.children().collect::<Vec<_>>().into_iter().rev() {
                    stack.push((child.index(), d + 1));
                }
            }
        }
        // Depth of nodes outside the scene, from their own root.
        for index in 0..node_count {
            if !in_scene[index] {
                let (mut d, mut cursor) = (0u32, index);
                while let Some(p) = parent[cursor] {
                    d += 1;
                    cursor = p;
                    if d as usize > node_count {
                        break; // a cycle in a malformed file
                    }
                }
                depth[index] = d;
            }
        }

        let joint_nodes: BTreeSet<usize> = document
            .skins()
            .flat_map(|skin| skin.joints().map(|j| j.index()).collect::<Vec<_>>())
            .collect();

        // Per node and per material.
        let mut material_nodes: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); material_names.len()];
        let mut material_triangles = vec![0u64; material_names.len()];
        // Primitives with no material index use the glTF default material.
        let mut default_nodes: BTreeSet<usize> = BTreeSet::new();
        let mut default_triangles = 0u64;
        let mut skin_meshes: Vec<Vec<usize>> = vec![Vec::new(); document.skins().len()];
        let mut nodes = Vec::with_capacity(node_count);
        for node in document.nodes() {
            let index = node.index();
            let (mut triangles, mut vertices) = (0u64, 0u64);
            let mut names: Vec<String> = Vec::new();
            let mut indices: Vec<Option<usize>> = Vec::new();
            let is_collision = regia_asset::is_collision_node_name(&node_names[index]);
            if let Some(mesh) = node.mesh() {
                for primitive in mesh.primitives() {
                    let tris = primitive_triangles(&primitive);
                    triangles += tris;
                    vertices += primitive
                        .get(&gltf::Semantic::Positions)
                        .map_or(0, |a| a.count() as u64);
                    let material = primitive.material().index();
                    if is_collision {
                        // A physics proxy has no drawn material: it stays out of the material tables.
                        continue;
                    }
                    if !indices.contains(&material) {
                        indices.push(material);
                        names.push(
                            material
                                .and_then(|i| material_names.get(i).cloned())
                                .unwrap_or_else(|| "default".to_string()),
                        );
                    }
                    if let Some(i) = material {
                        material_nodes[i].insert(index);
                        material_triangles[i] += tris;
                    } else {
                        default_nodes.insert(index);
                        default_triangles += tris;
                    }
                }
            }
            let skin = node.skin().map(|s| s.index());
            if let (Some(skin), true) = (skin, node.mesh().is_some()) {
                skin_meshes[skin].push(index);
            }
            nodes.push(NodeReport {
                gltf_index: index,
                name: node_names[index].clone(),
                parent: parent[index],
                children: node.children().map(|c| c.index()).collect(),
                depth: depth[index],
                triangles,
                vertices,
                materials: names,
                material_indices: indices,
                skin,
                joints: node.skin().map_or(0, |s| s.joints().len() as u64),
                is_socket: node.mesh().is_none() && node_names[index].starts_with("socket_"),
                is_collision,
                is_joint: joint_nodes.contains(&index),
                in_scene: in_scene[index],
            });
        }

        let skins: Vec<SkinReport> = document
            .skins()
            .map(|skin| SkinReport {
                gltf_index: skin.index(),
                name: skin
                    .name()
                    .map_or_else(|| format!("skin_{}", skin.index()), str::to_string),
                joints: skin.joints().len() as u64,
                meshes: skin_meshes[skin.index()].clone(),
            })
            .collect();

        let glass_flags: Vec<bool> = document
            .materials()
            .map(|m| m.transmission().is_some() || is_glass_name(m.name().unwrap_or_default()))
            .collect();
        let mut materials: Vec<MaterialReport> = material_names
            .iter()
            .enumerate()
            .map(|(i, name)| MaterialReport {
                gltf_index: i,
                name: name.clone(),
                nodes: material_nodes[i].iter().copied().collect(),
                triangles: material_triangles[i],
                glass: glass_flags.get(i).copied().unwrap_or(false),
            })
            .collect();
        if !default_nodes.is_empty() {
            // The implicit default material counts like any other (`gltf_index` is one past the
            // document's materials).
            materials.push(MaterialReport {
                gltf_index: materials.len(),
                name: "default".to_string(),
                nodes: default_nodes.into_iter().collect(),
                triangles: default_triangles,
                glass: false,
            });
        }

        // Texture slots.
        let mut maps = Vec::new();
        for material in document.materials() {
            let Some(index) = material.index() else {
                continue; // the implicit default material has no textures
            };
            let pbr = material.pbr_metallic_roughness();
            let slots: [(MapSlot, Option<usize>); 5] = [
                (
                    MapSlot::BaseColor,
                    pbr.base_color_texture().map(|t| t.texture().source().index()),
                ),
                (
                    MapSlot::Normal,
                    material.normal_texture().map(|t| t.texture().source().index()),
                ),
                (
                    MapSlot::Orm,
                    pbr.metallic_roughness_texture()
                        .map(|t| t.texture().source().index()),
                ),
                (
                    MapSlot::Occlusion,
                    material.occlusion_texture().map(|t| t.texture().source().index()),
                ),
                (
                    MapSlot::Emissive,
                    material.emissive_texture().map(|t| t.texture().source().index()),
                ),
            ];
            for (slot, image) in slots {
                maps.push(MapReport {
                    material: material_names[index].clone(),
                    material_index: index,
                    slot,
                    bound: image.is_some(),
                    size: image.and_then(|i| image_sizes.get(i).copied()),
                    image,
                    used: !material_nodes[index].is_empty(),
                });
            }
        }

        // Duplicate names, in first-use order.
        let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for node in &nodes {
            by_name
                .entry(node.name.as_str())
                .or_default()
                .push(node.gltf_index);
        }
        let duplicate_names: Vec<DuplicateName> = by_name
            .into_iter()
            .filter(|(_, list)| list.len() > 1)
            .map(|(name, list)| DuplicateName {
                name: name.to_string(),
                nodes: list,
            })
            .collect();

        let orphans: Vec<OrphanNode> = nodes
            .iter()
            .filter(|node| !node.in_scene)
            .map(|node| OrphanNode {
                gltf_index: Some(node.gltf_index),
                name: node.name.clone(),
                reason: if node.is_joint {
                    "joint node outside the default scene".to_string()
                } else {
                    "not reachable from the default scene".to_string()
                },
            })
            .collect();

        let lods = lod_levels(&nodes);

        // Totals.
        let mut used_images: BTreeMap<usize, (u32, u32)> = BTreeMap::new();
        for map in maps.iter().filter(|m| m.used) {
            if let (Some(image), Some(size)) = (map.image, map.size) {
                used_images.insert(image, size);
            }
        }
        let texture_bytes: u64 = used_images.values().map(|&(w, h)| mip_chain_bytes(w, h)).sum();
        let totals = ModelTotals {
            nodes: node_count as u64,
            nodes_in_scene: in_scene.iter().filter(|&&b| b).count() as u64,
            mesh_nodes: nodes.iter().filter(|n| !n.material_indices.is_empty()).count() as u64,
            triangles: drawn_triangles(&nodes, &lods),
            vertices: nodes.iter().filter(|n| !n.is_collision).map(|n| n.vertices).sum(),
            collision_nodes: nodes.iter().filter(|n| n.is_collision).count() as u64,
            collision_triangles: nodes.iter().filter(|n| n.is_collision).map(|n| n.triangles).sum(),
            collision_vertices: nodes.iter().filter(|n| n.is_collision).map(|n| n.vertices).sum(),
            materials_in_use: materials.iter().filter(|m| !m.nodes.is_empty()).count() as u64,
            skins: skins.len() as u64,
            joints: skins.iter().map(|s| s.joints).sum(),
            glass_materials: materials
                .iter()
                .filter(|m| m.glass && !m.nodes.is_empty())
                .count() as u64,
            textures: used_images.len() as u64,
            texture_bytes,
            max_texture_dimension: used_images
                .values()
                .map(|&(w, h)| u64::from(w.max(h)))
                .max()
                .unwrap_or(0),
        };

        let mut report = Self {
            source: source.to_string(),
            class,
            limits,
            nodes,
            roots,
            skins,
            materials,
            lods,
            maps,
            totals,
            flags: Vec::new(),
            duplicate_names,
            orphans,
        };
        report.flags = report.budget_flags();
        report
    }

    /// Reports `path` and compares it with a spawned hierarchy (see [`Self::check_hierarchy`]).
    ///
    /// # Errors
    ///
    /// Not a readable glTF.
    pub fn from_spawn(
        world: &World,
        spawn: &GltfSpawn,
        path: &Path,
        class: ModelClass,
    ) -> Result<Self, String> {
        let mut report = Self::from_file(path, class)?;
        report.check_hierarchy(world, spawn.root);
        Ok(report)
    }

    /// Compares the nodes of the scene with the entities below `root`, matched by name: a node
    /// with no entity left is an orphan ("missing"), and so is an entity below `root` that the
    /// file has no node for (names with a `#` are the material-part children
    /// [`crate::gltf_hierarchy`] adds and are ignored, as are missing `socket_*` nodes, which
    /// `keep_sockets: false` leaves out). Duplicate names are matched one to one.
    pub fn check_hierarchy(&mut self, world: &World, root: Entity) {
        let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
        for entity in world.iter_entities() {
            if let Some(child_of) = world.get::<ChildOf>(entity.id()) {
                children.entry(child_of.parent()).or_default().push(entity.id());
            }
        }
        let mut live: BTreeMap<String, usize> = BTreeMap::new();
        let mut stack = vec![root];
        while let Some(entity) = stack.pop() {
            for &child in children.get(&entity).map_or(&[][..], Vec::as_slice) {
                if let Some(name) = world.get::<Name>(child) {
                    *live.entry(name.as_str().to_string()).or_insert(0) += 1;
                }
                stack.push(child);
            }
        }
        self.orphans
            .retain(|o| o.gltf_index.is_some() && o.reason != HIERARCHY_MISSING);
        for node in self.nodes.iter().filter(|n| n.in_scene) {
            match live.get_mut(&node.name) {
                Some(count) if *count > 0 => *count -= 1,
                _ if node.is_socket => {}
                _ => self.orphans.push(OrphanNode {
                    gltf_index: Some(node.gltf_index),
                    name: node.name.clone(),
                    reason: HIERARCHY_MISSING.to_string(),
                }),
            }
        }
        for (name, count) in live {
            if count > 0 && !name.contains('#') {
                for _ in 0..count {
                    self.orphans.push(OrphanNode {
                        gltf_index: None,
                        name: name.clone(),
                        reason: "entity below the model root that the file has no node for".to_string(),
                    });
                }
            }
        }
    }

    /// Recomputes [`Self::flags`] against `limits` (call after changing [`Self::limits`]).
    #[must_use]
    pub fn with_limits(mut self, class: ModelClass, limits: BudgetLimits) -> Self {
        self.class = class;
        self.limits = limits;
        self.flags = self.budget_flags();
        self
    }

    fn budget_flags(&self) -> Vec<BudgetFlag> {
        let limits = &self.limits;
        let mut flags = Vec::new();
        let mut over = |kind: FlagKind, subject: &str, value: u64, limit: u64, what: &str| {
            if limit > 0 && value > limit {
                flags.push(BudgetFlag {
                    kind,
                    subject: subject.to_string(),
                    value,
                    limit,
                    message: format!(
                        "{subject}: {what} {value} exceeds the {} limit {limit}",
                        self.class.name()
                    ),
                });
            }
        };
        over(
            FlagKind::Triangles,
            "model",
            self.totals.triangles,
            limits.max_triangles,
            "triangles",
        );
        over(
            FlagKind::Nodes,
            "model",
            self.totals.nodes,
            limits.max_nodes,
            "nodes",
        );
        over(
            FlagKind::Materials,
            "model",
            self.totals.materials_in_use,
            limits.max_materials,
            "materials",
        );
        over(
            FlagKind::TextureMemory,
            "model",
            self.totals.texture_bytes,
            limits.max_texture_bytes,
            "texture bytes",
        );
        over(
            FlagKind::GlassMaterials,
            "model",
            self.totals.glass_materials,
            limits.max_glass_materials,
            "glass materials",
        );
        self.lod_flags(&mut over);
        for node in self.nodes.iter().filter(|n| !n.is_collision) {
            over(
                FlagKind::NodeTriangles,
                &node.name,
                node.triangles,
                limits.max_node_triangles,
                "triangles",
            );
            over(
                FlagKind::MaterialsPerNode,
                &node.name,
                node.materials.len() as u64,
                limits.max_materials_per_node,
                "materials",
            );
        }
        for skin in &self.skins {
            over(
                FlagKind::Joints,
                &skin.name,
                skin.joints,
                limits.max_joints_per_skin,
                "joints",
            );
        }
        for map in self.maps.iter().filter(|m| m.used) {
            let subject = format!("{}/{}", map.material, map.slot.name());
            if let Some((w, h)) = map.size {
                over(
                    FlagKind::TextureSize,
                    &subject,
                    u64::from(w.max(h)),
                    limits.max_texture_dimension,
                    "texture side",
                );
            }
        }
        if limits.require_normal_maps {
            for map in self
                .maps
                .iter()
                .filter(|m| m.used && m.slot == MapSlot::Normal && !m.bound)
            {
                flags.push(BudgetFlag {
                    kind: FlagKind::MissingNormalMap,
                    subject: format!("{}/normal", map.material),
                    value: 0,
                    limit: 0,
                    message: format!("{}: normal map not bound", map.material),
                });
            }
        }
        flags
    }

    /// The LOD checks: the hard ceilings of LOD2 and LOD3, and each level against its expected
    /// share of LOD0 (the ratio plus the tolerance).
    fn lod_flags(&self, over: &mut impl FnMut(FlagKind, &str, u64, u64, &str)) {
        let limits = &self.limits;
        let Some(lod0) = self.lods.iter().find(|l| l.level == 0).map(|l| l.triangles) else {
            return;
        };
        for lod in &self.lods {
            let subject = format!("LOD{}", lod.level);
            let (ceiling, permille) = match lod.level {
                1 => (0, limits.lod1_permille),
                2 => (limits.max_lod2_triangles, limits.lod2_permille),
                3 => (limits.max_lod3_triangles, limits.lod3_permille),
                _ => continue,
            };
            over(
                FlagKind::LodTriangles,
                &subject,
                lod.triangles,
                ceiling,
                "triangles",
            );
            if permille > 0 {
                // Expected share of LOD0 plus the tolerance.
                let expected = lod0 * permille / 1000;
                let allowed = expected + expected * limits.lod_ratio_tolerance_percent / 100;
                over(FlagKind::LodRatio, &subject, lod.triangles, allowed, "triangles");
            }
        }
    }

    /// No budget flag, duplicate name or orphan.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.flags.is_empty() && self.duplicate_names.is_empty() && self.orphans.is_empty()
    }

    /// The report as JSON (the shape `editor.model_report` returns under `report`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// A compact multi-line rendering for logs: totals, the node tree, bound maps, flags,
    /// duplicates and orphans.
    #[must_use]
    pub fn to_text(&self) -> String {
        let t = &self.totals;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "model {} [{}]: {} nodes ({} in scene), {} tris, {} verts, {} materials, {} skins / {} joints, {} textures ~{} (max side {})",
            self.source,
            self.class.name(),
            t.nodes,
            t.nodes_in_scene,
            t.triangles,
            t.vertices,
            t.materials_in_use,
            t.skins,
            t.joints,
            t.textures,
            human_bytes(t.texture_bytes),
            t.max_texture_dimension,
        );
        if t.collision_nodes > 0 {
            let _ = writeln!(
                out,
                "collision: {} nodes, {} tris, {} verts (not counted above)",
                t.collision_nodes, t.collision_triangles, t.collision_vertices
            );
        }
        if !self.lods.is_empty() {
            let levels: Vec<String> = self
                .lods
                .iter()
                .map(|l| format!("LOD{} {}", l.level, l.triangles))
                .collect();
            let _ = writeln!(out, "lods: {}", levels.join(", "));
        }
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        let mut seen = BTreeSet::new();
        while let Some(index) = stack.pop() {
            if !seen.insert(index) {
                continue;
            }
            self.write_node_line(&mut out, index);
            stack.extend(self.nodes[index].children.iter().rev());
        }
        for node in self.nodes.iter().filter(|n| !n.in_scene && n.parent.is_none()) {
            let _ = writeln!(out, "  (outside scene)");
            let mut stack = vec![node.gltf_index];
            while let Some(index) = stack.pop() {
                if !seen.insert(index) {
                    continue;
                }
                self.write_node_line(&mut out, index);
                stack.extend(self.nodes[index].children.iter().rev());
            }
        }
        if !self.maps.is_empty() {
            let _ = writeln!(out, "maps:");
            for material in &self.materials {
                let slots: Vec<String> = self
                    .maps
                    .iter()
                    .filter(|m| m.material_index == material.gltf_index)
                    .map(|m| match (m.bound, m.size) {
                        (true, Some((w, h))) => format!("{} {w}x{h}", m.slot.name()),
                        (true, None) => format!("{} bound", m.slot.name()),
                        (false, _) => format!("{} -", m.slot.name()),
                    })
                    .collect();
                let used = if material.nodes.is_empty() {
                    " (unused)"
                } else {
                    ""
                };
                let _ = writeln!(out, "  {}{used}: {}", material.name, slots.join(", "));
            }
        }
        for flag in &self.flags {
            let _ = writeln!(out, "FLAG {}", flag.message);
        }
        for dup in &self.duplicate_names {
            let _ = writeln!(
                out,
                "DUPLICATE {} x{} (nodes {:?})",
                dup.name,
                dup.nodes.len(),
                dup.nodes
            );
        }
        for orphan in &self.orphans {
            let _ = writeln!(out, "ORPHAN {}: {}", orphan.name, orphan.reason);
        }
        if self.is_clean() {
            let _ = writeln!(out, "ok: within the {} budget", self.class.name());
        }
        out
    }

    fn write_node_line(&self, out: &mut String, index: usize) {
        let node = &self.nodes[index];
        let indent = "  ".repeat(node.depth as usize + 1);
        let mut line = format!("{indent}{} #{}", node.name, node.gltf_index);
        if node.triangles > 0 {
            let _ = write!(line, " tris {}", node.triangles);
        }
        if !node.materials.is_empty() {
            let _ = write!(line, " mat [{}]", node.materials.join(", "));
        }
        if let Some(skin) = node.skin {
            let _ = write!(line, " skin {skin} ({} joints)", node.joints);
        }
        if node.is_socket {
            line.push_str(" socket");
        }
        if node.is_joint {
            line.push_str(" joint");
        }
        out.push_str(&line);
        out.push('\n');
    }
}

/// The `_LOD<n>` suffix of a node name (any case), as the level.
fn lod_level_of(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let at = lower.rfind("_lod")?;
    let digits = &lower[at + 4..];
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A material name that marks glass.
fn is_glass_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("glass")
}

/// The level each node belongs to: its own `_LOD<n>` suffix, else the nearest tagged ancestor's.
fn node_levels(nodes: &[NodeReport]) -> Vec<Option<u32>> {
    nodes
        .iter()
        .map(|node| {
            let mut cursor = Some(node.gltf_index);
            let mut steps = 0;
            while let Some(index) = cursor {
                if let Some(level) = lod_level_of(&nodes[index].name) {
                    return Some(level);
                }
                steps += 1;
                if steps > nodes.len() {
                    break; // a cycle in a malformed file
                }
                cursor = nodes[index].parent;
            }
            None
        })
        .collect()
}

/// Levels of detail from the `_LOD<n>` node-name suffix; empty when no node has one.
fn lod_levels(nodes: &[NodeReport]) -> Vec<LodReport> {
    let levels = node_levels(nodes);
    if levels.iter().all(Option::is_none) {
        return Vec::new();
    }
    let mut by_level: BTreeMap<u32, LodReport> = BTreeMap::new();
    for (node, level) in nodes.iter().zip(&levels) {
        if node.is_collision {
            continue;
        }
        let level = level.unwrap_or(0);
        let entry = by_level.entry(level).or_insert(LodReport {
            level,
            triangles: 0,
            mesh_nodes: 0,
        });
        entry.triangles += node.triangles;
        entry.mesh_nodes += u64::from(!node.material_indices.is_empty());
    }
    by_level.into_values().collect()
}

/// Triangles that are drawn at once: every node, except that with LODs only level 0 counts
/// (the other levels are alternatives to it, not additions).
fn drawn_triangles(nodes: &[NodeReport], lods: &[LodReport]) -> u64 {
    if lods.is_empty() {
        nodes
            .iter()
            .filter(|n| !n.is_collision)
            .map(|n| n.triangles)
            .sum()
    } else {
        lods.iter().find(|l| l.level == 0).map_or(0, |l| l.triangles)
    }
}

const HIERARCHY_MISSING: &str = "no entity of that name below the model root";

/// Triangles of one primitive from its accessor counts (lists, strips and fans).
fn primitive_triangles(primitive: &gltf::Primitive<'_>) -> u64 {
    let count = primitive.indices().map_or_else(
        || primitive.get(&gltf::Semantic::Positions).map_or(0, |a| a.count()),
        |a| a.count(),
    ) as u64;
    match primitive.mode() {
        gltf::mesh::Mode::Triangles => count / 3,
        gltf::mesh::Mode::TriangleStrip | gltf::mesh::Mode::TriangleFan => count.saturating_sub(2),
        _ => 0,
    }
}

/// Bytes of an RGBA8 image with a full mip chain (the 4/3 geometric sum).
fn mip_chain_bytes(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 4 * 4 / 3
}

fn human_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

/// `editor.model_report`.
fn model_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let class = match params.get("class").and_then(Value::as_str) {
        Some(name) => ModelClass::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown class `{name}` (prop, character, vehicle, building, hero)"
            ))
        })?,
        None => ModelClass::default(),
    };
    let mut limits = class.limits();
    if let Some(overrides) = params.get("limits") {
        limits = limits
            .with_overrides(overrides)
            .map_err(RpcError::invalid_params)?;
    }
    let full = crate::mesh_source::agent_asset_path(world, "path", path)?;
    let mut report = ModelReport::from_file_with(&full, class, limits)
        .map_err(|err| RpcError::not_found(err, "check the path; glTF/GLB only"))?;
    report.source = path.to_string();
    if let Some(bits) = params.get("root").and_then(Value::as_u64) {
        let root = crate::command::entity_from_bits(bits)
            .filter(|&e| world.get_entity(e).is_ok())
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not a live entity")))?;
        report.check_hierarchy(world, root);
    }
    let mut result =
        serde_json::json!({ "path": path, "report": report.to_json(), "clean": report.is_clean() });
    if params.get("text").and_then(Value::as_bool).unwrap_or(false) {
        result["text"] = Value::String(report.to_text());
    }
    Ok(result)
}

/// `editor.model_extensions`: the glTF extension side table of a file.
fn model_extensions(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let full = crate::mesh_source::agent_asset_path(world, "path", path)?;
    let bytes = std::fs::read(&full)
        .map_err(|err| RpcError::not_found(err.to_string(), "check the path; glTF/GLB only"))?;
    let table = regia_asset_pipeline::gltf_extensions::GltfExtensionTable::from_bytes(
        &bytes,
        &mut regia_asset_pipeline::gltf_extensions::GltfExtensionRegistry::new(),
    );
    let include_json = params
        .get("include_json")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut value = serde_json::to_value(&table)
        .map_err(|err| RpcError::invalid_params(format!("extension table: {err}")))?;
    if !include_json {
        // Raw unknown blocks can be large; `include_json` asks for them.
        if let Some(unknown) = value.get_mut("unknown").and_then(Value::as_array_mut) {
            for entry in unknown {
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("json");
                }
            }
        }
    }
    Ok(serde_json::json!({ "path": path, "extensions": value }))
}

/// The model-report agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.model_report",
                tool: "model_report",
                description: "Model map and node report of a glTF/GLB (CPU only): node tree with per-node \
                triangles, vertices, materials and skin/joint counts, every texture slot of every \
                material (base_color, normal, orm, occlusion, emissive: bound, size), totals, budget \
                flags against the model class's limits, duplicate node names and orphan nodes. \
                Pass `root` (from editor.spawn_gltf_hierarchy) to also compare with the spawned \
                entities.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path of the .glb/.gltf.",
                    },
                    ParamSchema {
                        name: "class",
                        kind: "string",
                        required: false,
                        description: "prop (default), character, vehicle, building or hero: picks the default limits.",
                    },
                    ParamSchema {
                        name: "limits",
                        kind: "object",
                        required: false,
                        description: "Overrides over the class limits: max_triangles, max_node_triangles, max_nodes, \
                        max_materials, max_materials_per_node, max_joints_per_skin, max_texture_dimension, \
                        max_texture_bytes, require_normal_maps, lod1_permille, lod2_permille, lod3_permille, \
                        lod_ratio_tolerance_percent, max_lod2_triangles, max_lod3_triangles, \
                        max_glass_materials. 0 turns a check off.",
                    },
                    ParamSchema {
                        name: "root",
                        kind: "number",
                        required: false,
                        description: "Entity id of a spawned hierarchy root to compare the file with.",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "boolean",
                        required: false,
                        description: "Also return the compact text rendering. Default false.",
                    },
                ],
                result: "{ path, report: { source, class, limits, nodes, roots, skins, materials, lods, maps, totals, flags, duplicate_names, orphans }, clean, text? }",
                mutating: false,
                permission: Permission::Read,
            },
            model_report as Handler,
        ),
        (
            MethodSchema {
                name: "editor.model_extensions",
                tool: "model_extensions",
                description: "glTF extension table of a glTF/GLB (CPU only): extensionsUsed/Required, per \
                material the clearcoat, sheen, specular, anisotropy, iridescence and emissive-strength values and \
                the textures (with KHR_texture_transform) they use, and the extensions the importer \
                does not read (scope, index, name; the raw JSON with include_json).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path of the .glb/.gltf.",
                    },
                    ParamSchema {
                        name: "include_json",
                        kind: "boolean",
                        required: false,
                        description: "Include the raw JSON of unknown extension blocks. Default false.",
                    },
                ],
                result: "{ path, extensions: { used, required, materials, unknown, dropped_unknown, unknown_names } }",
                mutating: false,
                permission: Permission::Read,
            },
            model_extensions as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_names_round_trip_and_limits_grow() {
        for class in ModelClass::ALL {
            assert_eq!(ModelClass::from_name(class.name()), Some(class));
        }
        assert!(ModelClass::Prop.limits().max_triangles < ModelClass::Building.limits().max_triangles);
        assert_eq!(ModelClass::from_name("tank"), None);
    }

    #[test]
    fn lod_suffix_parses_any_case_and_rejects_other_names() {
        assert_eq!(lod_level_of("Hull_LOD0"), Some(0));
        assert_eq!(lod_level_of("hull_lod3"), Some(3));
        assert_eq!(lod_level_of("Hull_LOD12"), Some(12));
        assert_eq!(lod_level_of("Hull_LOD"), None);
        assert_eq!(lod_level_of("Hull_LODx"), None);
        assert_eq!(lod_level_of("Hull_LOD123"), None);
        assert_eq!(lod_level_of("Hull"), None);
        assert!(is_glass_name("Cockpit GLASS"));
        assert!(!is_glass_name("Paint"));
    }

    #[test]
    fn limit_overrides_apply_and_reject_unknown_keys() {
        let limits = ModelClass::Prop
            .limits()
            .with_overrides(&serde_json::json!({ "max_triangles": 5, "require_normal_maps": true }))
            .expect("overrides");
        assert_eq!(limits.max_triangles, 5);
        assert!(limits.require_normal_maps);
        assert_eq!(limits.max_nodes, ModelClass::Prop.limits().max_nodes);
        assert!(
            ModelClass::Prop
                .limits()
                .with_overrides(&serde_json::json!({ "max_tris": 5 }))
                .is_err()
        );
        assert!(
            ModelClass::Prop
                .limits()
                .with_overrides(&serde_json::json!({ "max_nodes": "x" }))
                .is_err()
        );
    }

    #[test]
    fn mip_chain_is_four_thirds() {
        assert_eq!(mip_chain_bytes(1024, 1024), 1024 * 1024 * 4 * 4 / 3);
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    /// A glTF with one drawn mesh (2 triangles) and one `collision_hull` mesh (4 triangles).
    const COLLISION_GLTF: &str = r#"{
      "asset": { "version": "2.0" },
      "scene": 0,
      "scenes": [ { "nodes": [0, 1] } ],
      "nodes": [ { "name": "body", "mesh": 0 }, { "name": "collision_hull", "mesh": 1 } ],
      "meshes": [
        { "primitives": [ { "attributes": { "POSITION": 0 }, "indices": 1 } ] },
        { "primitives": [ { "attributes": { "POSITION": 2 }, "indices": 3 } ] }
      ],
      "accessors": [
        { "bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [0,0,0], "max": [1,1,1] },
        { "bufferView": 0, "componentType": 5123, "count": 6, "type": "SCALAR" },
        { "bufferView": 0, "componentType": 5126, "count": 8, "type": "VEC3", "min": [0,0,0], "max": [1,1,1] },
        { "bufferView": 0, "componentType": 5123, "count": 12, "type": "SCALAR" }
      ],
      "bufferViews": [ { "buffer": 0, "byteLength": 4 } ],
      "buffers": [ { "byteLength": 4, "uri": "data:application/octet-stream;base64,AAAAAA==" } ]
    }"#;

    #[test]
    fn collision_nodes_stay_out_of_the_totals() {
        let gltf = gltf::Gltf::from_slice(COLLISION_GLTF.as_bytes()).expect("gltf");
        let report = ModelReport::from_document(
            "collision.gltf",
            &gltf.document,
            &[],
            ModelClass::Prop,
            ModelClass::Prop.limits(),
        );
        assert_eq!(report.nodes.len(), 2);
        assert!(!report.nodes[0].is_collision && report.nodes[1].is_collision);
        // The drawn mesh only: 2 triangles, 4 vertices.
        assert_eq!(report.totals.triangles, 2);
        assert_eq!(report.totals.vertices, 4);
        assert_eq!(report.totals.mesh_nodes, 1);
        // The proxy has its own fields.
        assert_eq!(report.totals.collision_nodes, 1);
        assert_eq!(report.totals.collision_triangles, 4);
        assert_eq!(report.totals.collision_vertices, 8);
        assert!(report.to_text().contains("collision: 1 nodes, 4 tris, 8 verts"));
    }
}
