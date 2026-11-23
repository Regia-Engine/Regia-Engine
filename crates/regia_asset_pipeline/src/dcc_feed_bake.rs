// SPDX-License-Identifier: MPL-2.0
//! The bake of the DCC scene feed (`docs/specs/60-dcc-scene-feed.md` § 3.5): feed-owned mesh data
//! becomes ordinary `.glb` assets, which then go through the same importer as any other glTF.
//!
//! The input is a [`FeedSnapshot`], a plain-data copy taken on the main thread, so planning and
//! writing run off-thread. Output is deterministic: items in key order, a fixed writer and no
//! timestamps, so baking one snapshot twice gives identical bytes.
//!
//! Each mesh item becomes one file with its vertices in object space and the root node at
//! identity. The entity keeps its placement, so the bake lands exactly where the live mesh was.

use std::collections::BTreeMap;
use std::io::Write;

use regia_asset::AssetId;
use regia_core::id::Guid;

use crate::gltf_write::{
    GlbAnimation, GlbChannel, GlbImage, GlbMaterial, GlbMesh, GlbNode, GlbPath, GlbPrimitive, GlbScene,
    GlbSkin, GlbTextureRef,
};

/// The `asset.generator` of baked files; fixed, so bakes stay byte-identical across versions.
const GENERATOR: &str = "regia dcc_feed_bake";

/// One material slot of a mesh.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotSlot {
    /// Slot name.
    pub name: String,
    /// Linear base color and alpha.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Roughness factor.
    pub roughness: f32,
    /// Linear emissive color.
    pub emissive: [f32; 3],
    /// Texture names: base color, normal, metal-rough, emissive (empty = none).
    pub textures: [String; 4],
}

/// The skeleton a skinned mesh binds to.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotSkin {
    /// The skeleton item.
    pub guid: Guid,
    /// Joint names.
    pub names: Vec<String>,
    /// Parent joint per joint.
    pub parents: Vec<Option<u32>>,
    /// Local bind translation, rotation (x, y, z, w) and scale per joint.
    pub binds: Vec<([f32; 3], [f32; 4], [f32; 3])>,
    /// Inverse bind matrices, column-major.
    pub inverse_bind: Vec<[f32; 16]>,
}

/// One item's geometry, in object space.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotMesh {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals.
    pub normals: Vec<[f32; 3]>,
    /// UV set 0 (empty when absent).
    pub uv0: Vec<[f32; 2]>,
    /// Vertex colors (empty when absent).
    pub colors: Vec<[f32; 4]>,
    /// Skin joint indices (empty on a rigid mesh).
    pub joints: Vec<[u16; 4]>,
    /// Skin weights (empty on a rigid mesh).
    pub weights: Vec<[f32; 4]>,
    /// The skeleton of a skinned mesh.
    pub skin: Option<SnapshotSkin>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// Draw ranges `[slot, first_index, index_count]`.
    pub submeshes: Vec<[u32; 3]>,
    /// Material slots.
    pub slots: Vec<SnapshotSlot>,
}

/// One item of a [`FeedSnapshot`].
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotItem {
    /// The sender's key.
    pub key: String,
    /// The stable id.
    pub guid: Guid,
    /// `transform`, `mesh`, `camera`, `light`, `skeleton` or `generic`.
    pub kind: String,
    /// The geometry of a mesh item.
    pub mesh: Option<SnapshotMesh>,
}

/// A transient texture a material may name.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SnapshotTexture {
    /// PNG or JPEG bytes.
    pub bytes: Vec<u8>,
    /// `image/png` or `image/jpeg`.
    pub mime: &'static str,
}

/// One track of a pushed clip.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotTrack {
    /// The node or joint name it drives.
    pub target: String,
    /// 0 translation, 1 rotation, 2 scale.
    pub property: u8,
    /// True for linear keys, false for stepped ones.
    pub linear: bool,
    /// Key times in seconds.
    pub times: Vec<f32>,
    /// 3 or 4 floats per key.
    pub values: Vec<f32>,
}

/// A pushed clip.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotClip {
    /// Clip name.
    pub name: String,
    /// The mesh or skeleton item it binds to.
    pub item: Option<Guid>,
    /// Tracks.
    pub tracks: Vec<SnapshotTrack>,
}

/// A plain-data copy of feed items and the textures and clips they use.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FeedSnapshot {
    /// Items, in any order.
    pub items: Vec<SnapshotItem>,
    /// Textures by name.
    pub textures: BTreeMap<String, SnapshotTexture>,
    /// Clips, in name order.
    pub clips: Vec<SnapshotClip>,
}

/// Import settings of a bake (spec § 5 `import`).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ImportOverrides {
    /// Multiplies vertices; the caller divides the entity's scale by it, so the world pose stays.
    pub scale: Option<f32>,
    /// Total LOD levels to generate when the file cooks (`.meta` importer settings).
    pub lods: Option<u32>,
    /// Also writes the mesh under a `collision_` node, which the importer takes as its collider.
    pub collision: bool,
}

/// What to bake.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct BakeRequest {
    /// The items (empty: every bakeable item, skipping the rest with a warning).
    pub items: Vec<Guid>,
    /// Destination folder under `assets/`.
    pub folder: String,
    /// Import settings.
    pub import: ImportOverrides,
    /// Make the entities ordinary content using the baked assets.
    pub replace_live: bool,
    /// Plan only; nothing is written.
    pub dry_run: bool,
}

/// One file of a plan.
#[derive(Clone, Debug)]
pub struct PlannedGlb {
    /// The item.
    pub guid: Guid,
    /// The item's key.
    pub key: String,
    /// Project-relative path, `assets/<folder>/<name>.glb`.
    pub path: String,
    /// The scale the vertices took ([`ImportOverrides::scale`], default 1).
    pub scale: f32,
    scene: GlbScene,
}

/// A planned bake: the files to write and what else converts.
#[derive(Clone, Debug, Default)]
pub struct BakePlan {
    /// One file per mesh item, in key order.
    pub files: Vec<PlannedGlb>,
    /// Items with no geometry that become ordinary entities (no file).
    pub plain: Vec<Guid>,
    /// What the plan skipped or ignored.
    pub warnings: Vec<String>,
}

/// What a finished bake produced.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct BakeReport {
    /// The assets the import produced.
    pub assets: Vec<AssetId>,
    /// Project-relative paths of the written files.
    pub glb_paths: Vec<String>,
    /// The items that became ordinary content.
    pub converted: Vec<Guid>,
    /// What the bake skipped or ignored.
    pub warnings: Vec<String>,
}

/// Why a bake was refused.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BakeError {
    /// The request selected nothing.
    #[error("nothing to bake")]
    NothingToBake,
    /// An explicitly requested item cannot become an asset.
    #[error("a {0} item cannot be baked")]
    NotBakeable(String),
    /// A file already exists at the target.
    #[error("{0} already exists")]
    TargetExists(String),
    /// The import failed.
    #[error("import failed: {0}")]
    Import(String),
    /// The feed data is inconsistent: an attribute array shorter than `positions`, an index or
    /// draw range past the arrays, mismatched skin arrays, or a non-finite import scale.
    #[error("invalid data: {0}")]
    InvalidData(String),
    /// A write failed.
    #[error("{0}")]
    Io(String),
}

impl BakeError {
    /// The agent-protocol reason (`specs/60` § 5).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NothingToBake => "nothing_to_bake",
            Self::NotBakeable(_) => "not_bakeable",
            Self::TargetExists(_) => "target_exists",
            Self::Import(_) => "import_failed",
            Self::InvalidData(_) => "invalid_value",
            Self::Io(_) => "internal",
        }
    }
}

/// A file-name form of `key`: letters, digits, `.`, `-` and `_`; the rest becomes `_`.
fn file_stem(key: &str) -> String {
    let stem: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches('.').to_owned();
    if stem.is_empty() { "item".to_owned() } else { stem }
}

/// Plans a bake. Pure: reads nothing and writes nothing.
///
/// # Errors
///
/// [`BakeError::NothingToBake`], [`BakeError::NotBakeable`] for a requested skeleton or generic
/// item (spec § 3.5), or [`BakeError::InvalidData`] for inconsistent feed geometry or import
/// settings (a short attribute array, an index or draw range past the arrays, mismatched skin
/// arrays, a non-finite import scale).
pub fn plan_bake(snapshot: &FeedSnapshot, req: &BakeRequest) -> Result<BakePlan, BakeError> {
    let mut items: Vec<&SnapshotItem> = snapshot
        .items
        .iter()
        .filter(|i| req.items.is_empty() || req.items.contains(&i.guid))
        .collect();
    items.sort_by(|a, b| a.key.cmp(&b.key));
    let folder = req
        .folder
        .trim_matches('/')
        .trim_start_matches("assets/")
        .trim_matches('/');
    let scale = req.import.scale.unwrap_or(1.0);
    if !scale.is_finite() || scale <= 0.0 {
        return Err(BakeError::InvalidData(format!(
            "import.scale {scale} is not a finite number above 0"
        )));
    }
    let mut plan = BakePlan::default();
    let mut stems: BTreeMap<String, u32> = BTreeMap::new();
    for item in items {
        match item.kind.as_str() {
            "transform" | "camera" | "light" => {
                plan.plain.push(item.guid);
                continue;
            }
            "mesh" => {}
            other if req.items.is_empty() => {
                plan.warnings
                    .push(format!("{:?}: a {other} item has no asset form", item.key));
                continue;
            }
            other => return Err(BakeError::NotBakeable(other.to_owned())),
        }
        let Some(mesh) = item.mesh.as_ref().filter(|m| !m.indices.is_empty()) else {
            plan.warnings
                .push(format!("{:?}: no geometry arrived yet; skipped", item.key));
            continue;
        };
        let stem = file_stem(&item.key);
        let seen = stems.entry(stem.clone()).or_insert(0);
        *seen += 1;
        // Two keys can reduce to one stem; the later one (in key order) takes a number.
        let name = if *seen == 1 {
            stem
        } else {
            format!("{stem}-{seen}")
        };
        let path = if folder.is_empty() {
            format!("assets/{name}.glb")
        } else {
            format!("assets/{folder}/{name}.glb")
        };
        plan.files.push(PlannedGlb {
            guid: item.guid,
            key: item.key.clone(),
            path,
            scale,
            scene: scene_of(item, mesh, snapshot, &req.import, &mut plan.warnings)?,
        });
    }
    if plan.files.is_empty() && plan.plain.is_empty() {
        return Err(BakeError::NothingToBake);
    }
    Ok(plan)
}

/// The image of a named texture, added once.
fn image_of(
    name: &str,
    key: &str,
    textures: &BTreeMap<String, SnapshotTexture>,
    scene: &mut GlbScene,
    names: &mut BTreeMap<String, usize>,
    warnings: &mut Vec<String>,
) -> Option<GlbTextureRef> {
    if name.is_empty() {
        return None;
    }
    if let Some(image) = names.get(name) {
        return Some(GlbTextureRef {
            image: *image,
            tex_coord: 0,
        });
    }
    let Some(texture) = textures.get(name).filter(|t| !t.bytes.is_empty()) else {
        warnings.push(format!(
            "{key:?}: texture {name:?} has no embeddable bytes; left out"
        ));
        return None;
    };
    let image = scene.images.len();
    scene.images.push(GlbImage::Embedded {
        bytes: texture.bytes.clone(),
        mime: texture.mime,
    });
    names.insert(name.to_owned(), image);
    Some(GlbTextureRef { image, tex_coord: 0 })
}

/// The primitive of one draw range, with its vertices in first-use order.
///
/// # Errors
///
/// [`BakeError::InvalidData`] when a draw range runs past `indices`, an index runs past
/// `positions`, or a non-empty attribute array is shorter than `positions` (the importer rejects
/// a GLB whose NORMAL/TEXCOORD/COLOR/JOINTS accessor count differs from POSITION).
fn primitive_of(
    key: &str,
    mesh: &SnapshotMesh,
    range: &[u32; 3],
    scale: f32,
    material: Option<usize>,
) -> Result<GlbPrimitive, BakeError> {
    let first = range[1] as usize;
    let count = range[2] as usize;
    let indices = mesh.indices.get(first..first + count).ok_or_else(|| {
        BakeError::InvalidData(format!(
            "{key:?}: draw range [{}, {}] runs past {} indices",
            range[1],
            range[2],
            mesh.indices.len()
        ))
    })?;
    for (name, len) in [
        ("normals", mesh.normals.len()),
        ("uv0", mesh.uv0.len()),
        ("colors", mesh.colors.len()),
        ("joints", mesh.joints.len()),
        ("weights", mesh.weights.len()),
    ] {
        if len != 0 && len < mesh.positions.len() {
            return Err(BakeError::InvalidData(format!(
                "{key:?}: {name} has {len} entries for {} positions",
                mesh.positions.len()
            )));
        }
    }
    let mut remap: BTreeMap<u32, u32> = BTreeMap::new();
    let mut out = GlbPrimitive {
        material,
        ..GlbPrimitive::default()
    };
    let mut uv = Vec::new();
    let mut joints = Vec::new();
    let mut weights = Vec::new();
    for index in indices {
        let next = u32::try_from(remap.len())
            .map_err(|_| BakeError::InvalidData(format!("{key:?}: more than u32::MAX distinct vertices")))?;
        let mapped = *remap.entry(*index).or_insert(next);
        if mapped == next {
            let i = usize::try_from(*index).map_err(|_| {
                BakeError::InvalidData(format!("{key:?}: index {index} does not fit a vertex slot"))
            })?;
            let p = mesh.positions.get(i).ok_or_else(|| {
                BakeError::InvalidData(format!(
                    "{key:?}: index {i} runs past {} positions",
                    mesh.positions.len()
                ))
            })?;
            out.positions.push([p[0] * scale, p[1] * scale, p[2] * scale]);
            if let Some(n) = mesh.normals.get(i) {
                out.normals.push(*n);
            }
            if let Some(t) = mesh.uv0.get(i) {
                uv.push(*t);
            }
            if let Some(c) = mesh.colors.get(i) {
                out.colors.push(*c);
            }
            if let (Some(j), Some(w)) = (mesh.joints.get(i), mesh.weights.get(i)) {
                joints.push(*j);
                weights.push(*w);
            }
        }
        out.indices.push(mapped);
    }
    if !uv.is_empty() {
        out.uvs.push(uv);
    }
    if !joints.is_empty() {
        out.joints.push(joints);
        out.weights.push(weights);
    }
    Ok(out)
}

/// Adds the joint nodes and the skin; the mesh node is node 0.
///
/// # Errors
///
/// [`BakeError::InvalidData`] when the skin's `names`, `parents`, `binds` and `inverse_bind`
/// arrays do not all have the same length.
fn add_skin(scene: &mut GlbScene, key: &str, skin: &SnapshotSkin) -> Result<(), BakeError> {
    let joints = skin.names.len();
    for (name, len) in [
        ("parents", skin.parents.len()),
        ("binds", skin.binds.len()),
        ("inverse_bind", skin.inverse_bind.len()),
    ] {
        if len != joints {
            return Err(BakeError::InvalidData(format!(
                "{key:?}: skin {name} has {len} entries for {joints} joint names"
            )));
        }
    }
    let first = scene.nodes.len();
    for (i, (name, bind)) in skin.names.iter().zip(&skin.binds).enumerate() {
        let children = skin
            .parents
            .iter()
            .enumerate()
            .filter(|(_, p)| **p == Some(i as u32))
            .map(|(child, _)| first + child)
            .collect();
        scene.nodes.push(GlbNode {
            name: name.clone(),
            children,
            translation: bind.0,
            rotation: bind.1,
            scale: bind.2,
            ..GlbNode::default()
        });
        if skin.parents.get(i).copied().flatten().is_none() {
            scene.roots.push(first + i);
        }
    }
    scene.skins.push(GlbSkin {
        joints: (first..first + joints).collect(),
        inverse_bind: skin.inverse_bind.clone(),
        ..GlbSkin::default()
    });
    scene.nodes[0].skin = Some(0);
    Ok(())
}

/// The clips bound to this item, or to its skeleton. glTF `LINEAR` channels hold them, so a
/// stepped track gets a warning.
fn add_clips(
    scene: &mut GlbScene,
    item: &SnapshotItem,
    mesh: &SnapshotMesh,
    clips: &[SnapshotClip],
    warnings: &mut Vec<String>,
) {
    let skeleton = mesh.skin.as_ref().map(|s| s.guid);
    let bound = clips
        .iter()
        .filter(|c| c.item.is_some() && (c.item == Some(item.guid) || c.item == skeleton));
    for clip in bound {
        let mut channels = Vec::new();
        for track in &clip.tracks {
            let node = scene.nodes.iter().position(|n| n.name == track.target);
            let (Some(node), Some(path)) = (node, path_of(track.property)) else {
                warnings.push(format!(
                    "clip {:?}: no node {:?}; track left out",
                    clip.name, track.target
                ));
                continue;
            };
            if !track.linear {
                warnings.push(format!(
                    "clip {:?}: stepped track {:?} written as linear",
                    clip.name, track.target
                ));
            }
            channels.push(GlbChannel {
                node,
                path,
                times: track.times.clone(),
                values: track.values.clone(),
            });
        }
        if !channels.is_empty() {
            scene.animations.push(GlbAnimation {
                name: clip.name.clone(),
                channels,
            });
        }
    }
}

fn path_of(property: u8) -> Option<GlbPath> {
    match property {
        0 => Some(GlbPath::Translation),
        1 => Some(GlbPath::Rotation),
        2 => Some(GlbPath::Scale),
        _ => None,
    }
}

fn scene_of(
    item: &SnapshotItem,
    mesh: &SnapshotMesh,
    snapshot: &FeedSnapshot,
    import: &ImportOverrides,
    warnings: &mut Vec<String>,
) -> Result<GlbScene, BakeError> {
    let textures = &snapshot.textures;
    let mut scene = GlbScene {
        generator: GENERATOR.to_owned(),
        ..GlbScene::default()
    };
    let mut names = BTreeMap::new();
    for slot in &mesh.slots {
        let mut texture = |index: usize, scene: &mut GlbScene| {
            image_of(
                &slot.textures[index],
                &item.key,
                textures,
                scene,
                &mut names,
                warnings,
            )
        };
        let material = GlbMaterial {
            name: slot.name.clone(),
            base_color: slot.base_color,
            metallic: slot.metallic,
            roughness: slot.roughness,
            emissive: slot.emissive,
            base_color_texture: texture(0, &mut scene),
            normal_texture: texture(1, &mut scene),
            metallic_roughness_texture: texture(2, &mut scene),
            emissive_texture: texture(3, &mut scene),
            ..GlbMaterial::default()
        };
        scene.materials.push(material);
    }
    let scale = import.scale.unwrap_or(1.0);
    let primitives: Vec<GlbPrimitive> = mesh
        .submeshes
        .iter()
        .map(|range| {
            let material = (range[0] as usize) < mesh.slots.len();
            primitive_of(
                &item.key,
                mesh,
                range,
                scale,
                material.then_some(range[0] as usize),
            )
        })
        .collect::<Result<_, _>>()?;
    scene.meshes.push(GlbMesh {
        name: item.key.clone(),
        primitives,
        ..GlbMesh::default()
    });
    scene.nodes.push(GlbNode {
        name: item.key.clone(),
        mesh: Some(0),
        ..GlbNode::default()
    });
    scene.roots.push(0);
    if import.collision {
        scene.nodes.push(GlbNode {
            name: format!("collision_{}", file_stem(&item.key)),
            mesh: Some(0),
            ..GlbNode::default()
        });
        scene.roots.push(1);
    }
    if let Some(skin) = &mesh.skin {
        add_skin(&mut scene, &item.key, skin)?;
    }
    add_clips(&mut scene, item, mesh, &snapshot.clips, warnings);
    Ok(scene)
}

/// Writes one planned file. The same plan gives the same bytes.
///
/// # Errors
///
/// [`BakeError::Io`].
pub fn write_glb(file: &PlannedGlb, out: &mut dyn Write) -> Result<(), BakeError> {
    let bytes = file
        .scene
        .to_glb()
        .map_err(|e| BakeError::Import(e.to_string()))?;
    out.write_all(&bytes).map_err(|e| BakeError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture_compress::TextureCookQuality;
    use crate::{Forward, import_gltf_glb_bytes};

    fn slot(name: &str) -> SnapshotSlot {
        SnapshotSlot {
            name: name.to_owned(),
            base_color: [0.5, 0.25, 1.0, 1.0],
            metallic: 0.25,
            roughness: 0.75,
            emissive: [0.0; 3],
            textures: Default::default(),
        }
    }

    fn quad_snapshot() -> FeedSnapshot {
        FeedSnapshot {
            items: vec![
                SnapshotItem {
                    key: "Hull/Wing 1".to_owned(),
                    guid: Guid::from_u128(1),
                    kind: "mesh".to_owned(),
                    mesh: Some(SnapshotMesh {
                        positions: vec![[0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
                        normals: vec![[0.0, 1.0, 0.0]; 4],
                        uv0: vec![[0.0, 0.0]; 4],
                        indices: vec![0, 1, 2, 0, 2, 3],
                        submeshes: vec![[0, 0, 3], [1, 3, 3]],
                        slots: vec![slot("Paint"), slot("Trim")],
                        ..SnapshotMesh::default()
                    }),
                },
                SnapshotItem {
                    key: "Lamp".to_owned(),
                    guid: Guid::from_u128(2),
                    kind: "light".to_owned(),
                    mesh: None,
                },
                SnapshotItem {
                    key: "Rig".to_owned(),
                    guid: Guid::from_u128(3),
                    kind: "skeleton".to_owned(),
                    mesh: None,
                },
            ],
            ..FeedSnapshot::default()
        }
    }

    fn bytes(file: &PlannedGlb) -> Vec<u8> {
        let mut out = Vec::new();
        write_glb(file, &mut out).expect("write");
        out
    }

    /// Spec § 8 test 12, the pipeline half: byte-identical bakes, the normal importer reads the
    /// file, positions are the live object-space positions and slot names survive.
    #[test]
    fn a_bake_round_trips_through_the_importer_and_repeats_byte_for_byte() {
        let snapshot = quad_snapshot();
        let req = BakeRequest {
            folder: "live".to_owned(),
            ..BakeRequest::default()
        };
        let plan = plan_bake(&snapshot, &req).expect("plan");
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].path, "assets/live/Hull_Wing_1.glb");
        assert_eq!(plan.plain, vec![Guid::from_u128(2)]);
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        let first = bytes(&plan.files[0]);
        let again = plan_bake(&snapshot, &req).expect("plan");
        assert_eq!(first, bytes(&again.files[0]), "same snapshot, same bytes");

        import_gltf_glb_bytes(
            &first,
            AssetId::default(),
            Forward::Keep,
            "linux",
            TextureCookQuality::default(),
        )
        .expect("the normal importer reads it");
        let (document, buffers, _) = gltf::import_slice(&first).expect("parses");
        let names: Vec<&str> = document.materials().filter_map(|m| m.name()).collect();
        assert_eq!(names, vec!["Paint", "Trim"]);
        // Both draw ranges, with the live object-space positions.
        let mut positions = Vec::new();
        for primitive in document.meshes().flat_map(|m| m.primitives().collect::<Vec<_>>()) {
            let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
            positions.extend(reader.read_positions().expect("positions"));
        }
        assert_eq!(positions.len(), 6);
        assert!(positions.contains(&[1.0, 0.0, 1.0]) && positions.contains(&[1.0, 0.0, 0.0]));
    }

    /// A skinned mesh keeps its skin and joints, and a clip bound to its skeleton becomes an
    /// animation of those joints.
    #[test]
    fn skin_and_bound_clips_reach_the_file() {
        let mut snapshot = quad_snapshot();
        let skeleton = Guid::from_u128(3);
        if let Some(mesh) = &mut snapshot.items[0].mesh {
            mesh.joints = vec![[0, 1, 0, 0]; 4];
            mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 4];
            mesh.submeshes = vec![[0, 0, 6]];
            mesh.skin = Some(SnapshotSkin {
                guid: skeleton,
                names: vec!["root".to_owned(), "tip".to_owned()],
                parents: vec![None, Some(0)],
                binds: vec![([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]); 2],
                inverse_bind: vec![
                    [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0
                    ];
                    2
                ],
            });
        }
        snapshot.clips.push(SnapshotClip {
            name: "Wave".to_owned(),
            item: Some(skeleton),
            tracks: vec![SnapshotTrack {
                target: "tip".to_owned(),
                property: 0,
                linear: true,
                times: vec![0.0, 1.0],
                values: vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            }],
        });
        let plan = plan_bake(&snapshot, &BakeRequest::default()).expect("plan");
        let (document, _, _) = gltf::import_slice(bytes(&plan.files[0])).expect("parses");
        let skin = document.skins().next().expect("skin");
        assert_eq!(skin.joints().count(), 2);
        let animation = document.animations().next().expect("animation");
        assert_eq!(animation.name(), Some("Wave"));
        let target = animation.channels().next().expect("channel").target().node();
        assert_eq!(target.name(), Some("tip"));
    }

    #[test]
    fn explicit_requests_and_empty_selections_are_refused_with_their_codes() {
        let snapshot = quad_snapshot();
        let rig = BakeRequest {
            items: vec![Guid::from_u128(3)],
            ..BakeRequest::default()
        };
        assert_eq!(
            plan_bake(&snapshot, &rig).expect_err("rig").code(),
            "not_bakeable"
        );
        let none = BakeRequest {
            items: vec![Guid::from_u128(99)],
            ..BakeRequest::default()
        };
        assert_eq!(
            plan_bake(&snapshot, &none).expect_err("none").code(),
            "nothing_to_bake"
        );
    }

    #[test]
    fn scale_multiplies_vertices_and_keys_with_one_stem_stay_apart() {
        let mut snapshot = quad_snapshot();
        let mut twin = snapshot.items[0].clone();
        twin.key = "Hull Wing 1".to_owned();
        twin.guid = Guid::from_u128(9);
        // Sorted, "Hull Wing 1" comes before "Hull/Wing 1".
        snapshot.items.push(twin);
        let req = BakeRequest {
            import: ImportOverrides {
                scale: Some(2.0),
                ..ImportOverrides::default()
            },
            ..BakeRequest::default()
        };
        let plan = plan_bake(&snapshot, &req).expect("plan");
        assert_eq!(plan.files[0].path, "assets/Hull_Wing_1.glb");
        assert_eq!(plan.files[1].path, "assets/Hull_Wing_1-2.glb");
        assert_eq!(
            plan.files[0].scene.meshes[0].primitives[1].positions[1],
            [2.0, 0.0, 2.0]
        );
    }

    #[test]
    fn a_short_attribute_array_fails_the_bake() {
        // 4 positions but 3 normals: the importer rejects a NORMAL accessor shorter than the
        // position one, so the bake must refuse instead of writing a file that cannot cook.
        let mut snapshot = quad_snapshot();
        let mesh = snapshot.items[0].mesh.as_mut().expect("mesh");
        mesh.normals.pop();
        let err = plan_bake(&snapshot, &BakeRequest::default()).expect_err("3 normals");
        assert_eq!(err.code(), "invalid_value");
        assert!(err.to_string().contains("normals"), "{err}");
        // Equal lengths still bake (the round-trip test above uses 4 and 4).
    }

    #[test]
    fn an_index_past_the_positions_fails_the_bake() {
        let mut snapshot = quad_snapshot();
        let mesh = snapshot.items[0].mesh.as_mut().expect("mesh");
        mesh.positions = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.0, 1.0]];
        mesh.normals = Vec::new();
        mesh.uv0 = Vec::new();
        mesh.indices = vec![0, 1, 99];
        mesh.submeshes = vec![[0, 0, 3]];
        let err = plan_bake(&snapshot, &BakeRequest::default()).expect_err("index 99");
        assert_eq!(err.code(), "invalid_value");
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn a_bad_index_in_one_of_two_submeshes_fails_the_bake() {
        // The second draw range holds the bad index; the bake must not silently keep only the
        // first range.
        let mut snapshot = quad_snapshot();
        let mesh = snapshot.items[0].mesh.as_mut().expect("mesh");
        mesh.indices = vec![0, 1, 2, 0, 1, 99];
        mesh.submeshes = vec![[0, 0, 3], [0, 3, 3]];
        let err = plan_bake(&snapshot, &BakeRequest::default()).expect_err("second range");
        assert_eq!(err.code(), "invalid_value");
    }

    #[test]
    fn a_skin_with_mismatched_arrays_fails_the_bake() {
        let mut snapshot = quad_snapshot();
        let mesh = snapshot.items[0].mesh.as_mut().expect("mesh");
        mesh.joints = vec![[0, 1, 0, 0]; 4];
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 4];
        mesh.submeshes = vec![[0, 0, 6]];
        mesh.skin = Some(SnapshotSkin {
            guid: Guid::from_u128(3),
            names: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            parents: vec![None, Some(0), Some(1)],
            binds: vec![([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]); 2],
            inverse_bind: vec![
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0
                ];
                3
            ],
        });
        let err = plan_bake(&snapshot, &BakeRequest::default()).expect_err("2 binds for 3 names");
        assert_eq!(err.code(), "invalid_value");
    }

    #[test]
    fn a_non_finite_or_non_positive_scale_fails_the_bake() {
        for scale in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 0.0, -2.0] {
            let req = BakeRequest {
                import: ImportOverrides {
                    scale: Some(scale),
                    ..ImportOverrides::default()
                },
                ..BakeRequest::default()
            };
            let err = plan_bake(&quad_snapshot(), &req).expect_err("scale {scale}");
            assert_eq!(err.code(), "invalid_value", "scale {scale}");
        }
        let req = BakeRequest {
            import: ImportOverrides {
                scale: Some(1.0),
                ..ImportOverrides::default()
            },
            ..BakeRequest::default()
        };
        let plan = plan_bake(&quad_snapshot(), &req).expect("scale 1 bakes");
        assert!(
            plan.files[0].scene.meshes[0].primitives[0]
                .positions
                .iter()
                .flatten()
                .all(|c| c.is_finite())
        );
    }
}
