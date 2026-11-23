// SPDX-License-Identifier: MPL-2.0
//! glTF 2.0 skin, socket and animation import.
//!
//! Specification: `docs/specs/12-assets.md` § 7 and `docs/specs/17-animation.md` § 1.
//! Buffers, skins, inverse bind matrices and animation samplers follow the glTF 2.0
//! specification (Khronos). Cubic-spline channels are baked to linear keys with the
//! Appendix C Hermite formula. `Forward::Convert` bakes the 180° yaw from
//! `docs/formats/gltf-conventions.md` into root-joint rest poses, inverse bind matrices
//! and root-joint tracks. [`GltfAnimSettings::scale`] other than 1 multiplies that yaw
//! (or the identity, for `Keep`) by a uniform scale.
//!
//! The skeleton's object space is the import's scene space (after the yaw), the space the mesh
//! importer bakes vertices into: root joints carry their parent nodes' transform, and each
//! inverse bind matrix is followed by the inverse of the matrix the mesh importer baked into
//! the skinned vertices (`skin_bake_matrix`). The skinned result is then the glTF 2.0
//! § 3.7.3 result (`Σ w · G_j · IBM_j · p`, mesh node transform ignored) for any node
//! hierarchy (ADR-226).
//!
//! [`import_gltf_animation`] reads only the binary payload embedded in a `.glb` (buffer 0);
//! [`import_gltf_animation_with_base`] also reads external `.bin` buffers, as the mesh importer does.

use gltf::animation::util::ReadOutputs;
use gltf::animation::{Interpolation, Property};
use regia_animation::clip::{
    AnimationClip, JointTrack, MorphNode, MorphTrack, RootMotionMode, RootMotionTrack, TrackCurve,
};
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_core::math::{Mat4, Quat, Vec3, Vec4};
use thiserror::Error;

use crate::ImportError;

/// One authored sub-range of a glTF animation (`specs/12` § 7, clip splitting).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipRange {
    /// Clip name suffix.
    pub name: String,
    /// Start time in seconds.
    pub start: f32,
    /// End time in seconds.
    pub end: f32,
}

/// Import options for skins and clips.
#[derive(Clone, Debug)]
pub struct GltfAnimSettings {
    /// Axis conversion. [`crate::Forward::Convert`] is the project default.
    pub forward: crate::Forward,
    /// Uniform scale baked with the axis conversion. `1.0` leaves lengths unchanged.
    /// A non-finite or non-positive value is treated as `1.0`.
    pub scale: f32,
    /// Root-motion extraction tagged onto every imported clip.
    pub root_motion: RootMotionMode,
    /// When non-empty, each glTF animation is split into these ranges.
    pub ranges: Vec<ClipRange>,
    /// Per-animation options (the `.meta` `clips` key, ADR-7904). An animation an entry names
    /// follows it: skipped, renamed, or split by the entry's own ranges (which replace
    /// [`Self::ranges`] for that animation).
    pub clips: Vec<ClipOptions>,
}

/// One glTF animation's clip options ([`GltfAnimSettings::clips`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipOptions {
    /// The animation's name in the file (`anim_<index>` for an unnamed one).
    pub animation: String,
    /// Import no clip from this animation.
    pub skip: bool,
    /// The clip's name (`None` keeps the animation's).
    pub name: Option<String>,
    /// Split into these clips, each named by its [`ClipRange::name`] as given (`<clip>_<index>`
    /// when empty). Empty imports one clip (or [`GltfAnimSettings::ranges`] when that is set).
    pub ranges: Vec<ClipRange>,
}

/// Turns one imported animation into the clips `settings` asks for (ADR-7904): skipped, renamed,
/// split by the animation's own ranges, or by the file-wide [`GltfAnimSettings::ranges`].
/// `keep_unsplit` decides whether an unsplit clip is kept (the skin path drops a clip with no
/// tracks).
fn emit_clips(
    clips: &mut Vec<AnimationClip>,
    mut clip: AnimationClip,
    anim_name: &str,
    settings: &GltfAnimSettings,
    keep_unsplit: bool,
) {
    let options = settings.clips.iter().find(|c| c.animation == anim_name);
    if options.is_some_and(|o| o.skip) {
        return;
    }
    let base = options
        .and_then(|o| o.name.clone())
        .unwrap_or_else(|| anim_name.to_string());
    match options.filter(|o| !o.ranges.is_empty()) {
        Some(options) => {
            for (index, range) in options.ranges.iter().enumerate() {
                let name = if range.name.trim().is_empty() {
                    format!("{base}_{index}")
                } else {
                    range.name.clone()
                };
                clips.push(clip.slice(name, range.start, range.end));
            }
        }
        None if settings.ranges.is_empty() => {
            if keep_unsplit {
                clip.name = base;
                clips.push(clip);
            }
        }
        None => {
            for range in &settings.ranges {
                let name = format!("{base}_{}", range.name);
                clips.push(clip.slice(name, range.start, range.end));
            }
        }
    }
}

impl Default for GltfAnimSettings {
    fn default() -> Self {
        Self {
            forward: crate::Forward::Convert,
            scale: 1.0,
            root_motion: RootMotionMode::None,
            ranges: Vec::new(),
            clips: Vec::new(),
        }
    }
}

/// A socket authored as an empty `socket_<name>` node under a joint.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedSocket {
    /// Socket id (`socket_turret` → `"turret"`).
    pub name: String,
    /// Joint the socket hangs from.
    pub joint: String,
    /// Offset from that joint, in the joint's local frame.
    pub offset: LocalBind,
}

/// One imported skin: its skeleton, the clips that animate it, and its sockets.
#[derive(Clone, Debug)]
pub struct ImportedSkin {
    /// Joint hierarchy, topologically sorted.
    pub skeleton: Skeleton,
    /// Clips targeting this skin's joints.
    pub clips: Vec<AnimationClip>,
    /// `socket_*` nodes parented under the skin.
    pub sockets: Vec<ImportedSocket>,
}

/// Everything [`import_gltf_animation`] produced from one file.
#[derive(Clone, Debug)]
pub struct GltfAnimationImport {
    /// One entry per glTF skin. Empty when the file has no skin.
    pub skins: Vec<ImportedSkin>,
    /// Clips with plain-node TRS or per-node morph channels, independent of skins.
    /// TRS hierarchies retain source-node indices, bind defaults and interpolation (ADR-8157).
    pub node_clips: Vec<AnimationClip>,
    /// Import problems that did not stop the import (a `weights` channel on a node without a mesh
    /// or without morph targets).
    pub warnings: Vec<String>,
}

/// Failures while reading a skin or an animation channel.
#[derive(Debug, Error)]
pub enum GltfAnimError {
    /// The bytes are not a glTF 2.0 asset.
    #[error("glTF parse error: {0}")]
    Parse(String),
    /// A skin's joints, parents or inverse bind matrices are unusable.
    #[error("skin {index}: {reason}")]
    Skin {
        /// Skin index in the file.
        index: usize,
        /// What was wrong.
        reason: String,
    },
    /// An animation channel could not be read.
    #[error("animation {animation} channel {channel}: {reason}")]
    Channel {
        /// Animation name or index.
        animation: String,
        /// Channel index.
        channel: usize,
        /// What was wrong.
        reason: String,
    },
}

/// Import every skin, its clips and its `socket_*` nodes from a glTF 2.0 file.
///
/// # Errors
///
/// Returns [`GltfAnimError`] when the file cannot be parsed or a skin is structurally
/// invalid. A file with no skins returns an empty [`GltfAnimationImport`].
pub fn import_gltf_animation(
    bytes: &[u8],
    settings: &GltfAnimSettings,
) -> Result<GltfAnimationImport, GltfAnimError> {
    import_gltf_animation_with_base(bytes, None, settings)
}

/// [`import_gltf_animation`] that also reads the file's external buffers (`.bin` files and data
/// URIs) relative to `base`, the directory of the `.gltf`, as the mesh importer does. With
/// `base: None` only the binary payload embedded in a `.glb` is available, so a `.gltf` whose
/// accessors live in `hero.bin` needs the directory.
///
/// # Errors
///
/// As [`import_gltf_animation`], plus [`GltfAnimError::Parse`] when an external buffer cannot be
/// read.
pub fn import_gltf_animation_with_base(
    bytes: &[u8],
    base: Option<&std::path::Path>,
    settings: &GltfAnimSettings,
) -> Result<GltfAnimationImport, GltfAnimError> {
    // Reject a hostile GLB header (declared length < 12) up front: the `gltf` crate reads it as
    // `length - 12` and underflow-panics in debug, and a `catch_unwind` cannot contain that under
    // `panic = "abort"` (shipping).
    crate::gltf_import::reject_hostile_glb_header(bytes).map_err(GltfAnimError::Parse)?;
    let parse_error = |e: ImportError| {
        GltfAnimError::Parse(match e {
            ImportError::Gltf(message) => message,
            other => other.to_string(),
        })
    };
    let (mut gltf, raw) = crate::gltf_import::checked_parse(bytes).map_err(parse_error)?;
    // A static mesh has nothing to read here: skip loading its external buffers, so a missing
    // `.bin` stays the mesh importer's error rather than a second one from this pass.
    let animated = gltf.skins().next().is_some() || gltf.animations().next().is_some();
    let on_disk = base.filter(|_| animated);
    let loaded: Vec<Vec<u8>> = match on_disk {
        Some(base) => crate::gltf_import::disk_buffers(&gltf, &raw, base).map_err(parse_error)?,
        None => gltf.blob.take().into_iter().collect(),
    };
    // Compressed or quantized data reads like any other from here on. Without a directory a `.gltf`
    // has no external buffers to decode from, and only the bind poses are computed.
    let loaded = if animated && (on_disk.is_some() || loaded.len() == gltf.document.buffers().count()) {
        let document = std::mem::replace(
            &mut gltf.document,
            gltf::Document::from_json_without_validation(gltf::json::Root::default()),
        );
        let normalized = crate::gltf_normalize::normalize(document, &raw, loaded).map_err(parse_error)?;
        gltf.document = normalized.document;
        normalized.buffers
    } else {
        loaded
    };
    let buffers: Vec<&[u8]> = loaded.iter().map(Vec::as_slice).collect();
    let worlds = node_worlds(&gltf)?;
    let parents = node_parents(&gltf, worlds.len());
    let mut skins = Vec::new();
    for skin in gltf.skins() {
        let imported = import_skin(&gltf, skin, &worlds, &parents, &buffers, settings)?;
        skins.push(imported);
    }
    let mut warnings = Vec::new();
    let node_clips = node_morph_clips(&gltf, &buffers, settings, &mut warnings)?;
    Ok(GltfAnimationImport {
        skins,
        node_clips,
        warnings,
    })
}

pub(crate) fn node_parents(gltf: &gltf::Document, len: usize) -> Vec<Option<usize>> {
    let mut parents = vec![None; len];
    for node in gltf.nodes() {
        for child in node.children() {
            if child.index() < len {
                parents[child.index()] = Some(node.index());
            }
        }
    }
    parents
}

/// World matrix of every node from the file's static node transforms (no axis conversion).
pub(crate) fn node_worlds(gltf: &gltf::Document) -> Result<Vec<Mat4>, GltfAnimError> {
    let len = gltf.nodes().map(|node| node.index() + 1).max().unwrap_or(0);
    let mut local = vec![Mat4::IDENTITY; len];
    let parents = node_parents(gltf, len);
    for node in gltf.nodes() {
        if node.index() < len {
            local[node.index()] = matrix_of(node.transform());
        }
    }
    // Iterative, so a hostile file with a node chain thousands deep returns instead of
    // overflowing the stack: walk up to the first ancestor with a known world (or a root), then
    // compose back down. A walk longer than the node count revisits a node: a cycle.
    let mut world: Vec<Option<Mat4>> = vec![None; len];
    let mut chain = Vec::new();
    for start in 0..len {
        if world[start].is_some() {
            continue;
        }
        chain.clear();
        let mut at = start;
        let mut base = Mat4::IDENTITY;
        loop {
            if let Some(done) = world[at] {
                base = done;
                break;
            }
            if chain.len() >= len {
                return Err(GltfAnimError::Parse(format!("node {at} is in a transform cycle")));
            }
            chain.push(at);
            match parents[at] {
                Some(parent) => at = parent,
                None => break,
            }
        }
        for &node in chain.iter().rev() {
            base *= local[node];
            world[node] = Some(base);
        }
    }
    Ok(world.into_iter().map(|m| m.unwrap_or(Mat4::IDENTITY)).collect())
}

fn matrix_of(transform: gltf::scene::Transform) -> Mat4 {
    let m = transform.matrix();
    Mat4::from_cols(
        Vec4::from_array(m[0]),
        Vec4::from_array(m[1]),
        Vec4::from_array(m[2]),
        Vec4::from_array(m[3]),
    )
}

fn trs_of(matrix: Mat4) -> LocalBind {
    let (scale, rotation, translation) = matrix.to_scale_rotation_translation();
    LocalBind {
        translation,
        rotation: rotation.normalize(),
        scale,
    }
}

/// `matrix` is finite and has an inverse (no zero scale axis).
fn invertible(matrix: &Mat4) -> bool {
    let det = matrix.determinant();
    matrix.is_finite() && det.is_finite() && det != 0.0
}

/// [`trs_of`] for a matrix that decomposes: `None` for a singular or non-finite one, whose
/// rotation glam would return as NaN, and for any result that is not finite.
fn checked_trs(matrix: Mat4) -> Option<LocalBind> {
    if !invertible(&matrix) {
        return None;
    }
    let bind = trs_of(matrix);
    (bind.translation.is_finite() && bind.rotation.is_finite() && bind.scale.is_finite()).then_some(bind)
}

struct BuiltSkin {
    skeleton: Skeleton,
    /// glTF node index → joint id.
    node_to_joint: Vec<Option<u32>>,
    root_joints: Vec<u32>,
    /// Per root joint: the frame its glTF tracks are turned into (axis yaw × parent-node world),
    /// split for separate translation, rotation and scale channels.
    root_frames: Vec<(u32, LocalBind)>,
    mesh_node: Option<usize>,
}

/// The matrix the mesh importer bakes into a skinned primitive of `skin_index` (cooked mesh v2,
/// ADR-226): the axis yaw (for [`crate::Forward::Convert`], times `scale`) times the world matrix
/// of the first node that uses the skin. Every primitive of the skin uses this one matrix (glTF
/// 2.0 ignores a skinned mesh node's own transform), and the skin's inverse bind matrices are
/// followed by its inverse, so the bake cancels out of the skinned result. A degenerate
/// (non-invertible) node world is replaced by the yaw alone.
pub(crate) fn skin_bake_matrix(
    document: &gltf::Document,
    skin_index: usize,
    worlds: &[Mat4],
    forward: crate::Forward,
    scale: f32,
) -> Mat4 {
    let axis = axis_matrix(forward, scale);
    let node = document
        .nodes()
        .find(|node| node.skin().is_some_and(|s| s.index() == skin_index))
        .map(|n| n.index());
    match node.and_then(|i| worlds.get(i)) {
        Some(world) if world.determinant().is_normal() => axis * *world,
        _ => axis,
    }
}

/// The 180° yaw for [`crate::Forward::Convert`] (exact ±1 entries when `scale` is 1, the matrix
/// the mesh importer bakes into rigid meshes), identity for [`crate::Forward::Keep`]. A `scale`
/// other than 1 multiplies the diagonal. A non-finite or non-positive scale is 1.
fn axis_matrix(forward: crate::Forward, scale: f32) -> Mat4 {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    if s == 1.0 {
        if forward == crate::Forward::Convert {
            Mat4::from_diagonal(Vec4::new(-1.0, 1.0, -1.0, 1.0))
        } else {
            Mat4::IDENTITY
        }
    } else if forward == crate::Forward::Convert {
        Mat4::from_diagonal(Vec4::new(-s, s, -s, 1.0))
    } else {
        Mat4::from_diagonal(Vec4::new(s, s, s, 1.0))
    }
}

fn import_skin(
    gltf: &gltf::Gltf,
    skin: gltf::Skin<'_>,
    worlds: &[Mat4],
    parents: &[Option<usize>],
    buffers: &[&[u8]],
    settings: &GltfAnimSettings,
) -> Result<ImportedSkin, GltfAnimError> {
    let built = build_skeleton(gltf, skin.clone(), worlds, parents, buffers, settings)?;
    let sockets = collect_sockets(gltf, worlds, parents, &built);
    let clips = import_clips(gltf, buffers, &built, settings)?;
    Ok(ImportedSkin {
        skeleton: built.skeleton,
        clips,
        sockets,
    })
}

/// The joint order and names the cooked skeleton gives a glTF skin. The mesh importer uses the
/// same layout to write cooked joint indices as skeleton joint ids (cooked mesh v2).
pub(crate) struct SkinJointLayout {
    /// Skin joint slot (glTF `JOINTS_n` value) → skeleton joint id.
    pub(crate) slot_to_joint: Vec<u32>,
    /// Joint names in skeleton order (unique; unnamed joints are `joint_<node>`).
    pub(crate) names: Vec<String>,
    /// glTF node of each skin slot.
    joint_nodes: Vec<usize>,
    /// Nearest ancestor node that is also a joint of the skin, per slot.
    joint_parent_node: Vec<Option<usize>>,
    /// Parent slot per slot.
    parent_index: Vec<Option<usize>>,
    /// Skeleton order: skeleton id → slot.
    order: Vec<usize>,
}

/// Skin joint slot (glTF `JOINTS_n` value and position in `skin.joints`) → the id of that joint
/// in the skeleton [`import_gltf_animation`] builds for `skin`. The same map the mesh importer
/// writes cooked joint indices with, so a caller binding a skin to the file's nodes does not have
/// to guess which deduplicated joint name belongs to which node.
///
/// # Errors
/// When the skin has no joints or its joints form a parent cycle.
pub fn skin_slot_joints(document: &gltf::Document, skin: &gltf::Skin<'_>) -> Result<Vec<u32>, String> {
    let parents = node_parents(document, document.nodes().len());
    skin_joint_layout(document, skin, &parents).map(|layout| layout.slot_to_joint)
}

/// Computes [`SkinJointLayout`] for `skin` (topological joint order, deduplicated names).
pub(crate) fn skin_joint_layout(
    document: &gltf::Document,
    skin: &gltf::Skin<'_>,
    parents: &[Option<usize>],
) -> Result<SkinJointLayout, String> {
    let joint_nodes: Vec<usize> = skin.joints().map(|node| node.index()).collect();
    if joint_nodes.is_empty() {
        return Err("skin has no joints".into());
    }
    let member: std::collections::HashSet<usize> = joint_nodes.iter().copied().collect();
    let mut joint_parent_node = vec![None; joint_nodes.len()];
    for (i, &node) in joint_nodes.iter().enumerate() {
        let mut cursor = parents.get(node).copied().flatten();
        let mut guard = 0;
        while let Some(parent) = cursor {
            if member.contains(&parent) {
                joint_parent_node[i] = Some(parent);
                break;
            }
            cursor = parents.get(parent).copied().flatten();
            guard += 1;
            if guard > parents.len() {
                break;
            }
        }
    }
    // Node → its first slot, so a skin of many joints is not quadratic.
    let mut slot_of_node: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (slot, &node) in joint_nodes.iter().enumerate() {
        slot_of_node.entry(node).or_insert(slot);
    }
    let parent_index: Vec<Option<usize>> = joint_parent_node
        .iter()
        .map(|node| node.and_then(|n| slot_of_node.get(&n).copied()))
        .collect();
    let order = topo_order(&parent_index)?;
    let mut names = Vec::with_capacity(order.len());
    let mut seen_names = std::collections::HashSet::new();
    let mut slot_to_joint = vec![0u32; joint_nodes.len()];
    for (new_id, &old) in order.iter().enumerate() {
        let node = joint_nodes[old];
        let mut name = document
            .nodes()
            .nth(node)
            .and_then(|n| n.name().map(str::to_string))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("joint_{node}"));
        if seen_names.contains(&name) {
            // A later joint with a taken name carries its node index (and a counter past it when
            // another joint is already called that), so every joint name stays unique.
            let base = name.clone();
            name = format!("{base}_{node}");
            let mut n = 1usize;
            while seen_names.contains(&name) {
                name = format!("{base}_{node}_{n}");
                n += 1;
            }
        }
        seen_names.insert(name.clone());
        names.push(name);
        slot_to_joint[old] = new_id as u32;
    }
    Ok(SkinJointLayout {
        slot_to_joint,
        names,
        joint_nodes,
        joint_parent_node,
        parent_index,
        order,
    })
}

fn build_skeleton(
    gltf: &gltf::Gltf,
    skin: gltf::Skin<'_>,
    worlds: &[Mat4],
    parents: &[Option<usize>],
    buffers: &[&[u8]],
    settings: &GltfAnimSettings,
) -> Result<BuiltSkin, GltfAnimError> {
    let skin_index = skin.index();
    let layout = skin_joint_layout(gltf, &skin, parents).map_err(|reason| GltfAnimError::Skin {
        index: skin_index,
        reason,
    })?;
    let SkinJointLayout {
        names: layout_names,
        joint_nodes,
        joint_parent_node,
        parent_index,
        order,
        ..
    } = layout;

    let mesh_node = gltf
        .nodes()
        .find(|node| node.skin().is_some_and(|s| s.index() == skin_index))
        .map(|n| n.index());
    let axis = axis_matrix(settings.forward, settings.scale);
    let bake = skin_bake_matrix(gltf, skin_index, worlds, settings.forward, settings.scale);
    if gltf
        .nodes()
        .filter(|node| node.skin().is_some_and(|s| s.index() == skin_index))
        .filter_map(|node| worlds.get(node.index()))
        .any(|w| mesh_node.is_some_and(|m| !w.abs_diff_eq(worlds[m], 1e-5)))
    {
        tracing::debug!(
            target: "regia_asset_pipeline",
            skin = skin_index,
            "several nodes use the skin with different transforms; skinned vertices bake with the first"
        );
    }

    let mut names = Vec::with_capacity(order.len());
    let mut skel_parents = Vec::with_capacity(order.len());
    let mut bind = Vec::with_capacity(order.len());
    let mut root_frames = Vec::new();
    let mut node_to_joint = vec![None; worlds.len()];
    for (new_id, &old) in order.iter().enumerate() {
        let node = joint_nodes[old];
        let name = layout_names[new_id].clone();
        let parent_new = parent_index[old].map(|p| order.iter().position(|o| *o == p).unwrap_or(0) as u32);
        // A zero scale axis on the joint or on a node above it leaves no rotation to decompose and
        // no inverse: glam returns NaN there (its asserts are off), and that rest pose would skin
        // every vertex to NaN. Refuse the skin instead (audit asset_pipeline1 #2).
        let unusable = || GltfAnimError::Skin {
            index: skin_index,
            reason: format!(
                "joint `{name}` (node {node}) has no usable rest pose: it or a node above it has a zero \
                 or non-finite scale"
            ),
        };
        let local_matrix = match joint_parent_node[old] {
            Some(parent_node) => {
                if !invertible(&worlds[parent_node]) {
                    return Err(unusable());
                }
                worlds[parent_node].inverse() * worlds[node]
            }
            None => {
                // A root joint lives in scene space: its glTF parent chain (armature node,
                // Z-up or unit-scale nodes) and the axis yaw move onto its rest pose and tracks.
                let parent_world = parents
                    .get(node)
                    .copied()
                    .flatten()
                    .and_then(|p| worlds.get(p).copied())
                    .unwrap_or(Mat4::IDENTITY);
                let frame = checked_trs(axis * parent_world).ok_or_else(unusable)?;
                if frame.scale.max_element() - frame.scale.min_element()
                    > 1e-4 * frame.scale.abs().max_element()
                {
                    tracing::warn!(
                        target: "regia_asset_pipeline",
                        skin = skin_index,
                        joint = name.as_str(),
                        "root joint's parent nodes scale non-uniformly; its animated tracks are approximate"
                    );
                }
                root_frames.push((new_id as u32, frame));
                axis * worlds[node]
            }
        };
        let local = checked_trs(local_matrix).ok_or_else(unusable)?;
        names.push(name);
        skel_parents.push(parent_new);
        bind.push(local);
        if node < node_to_joint.len() {
            node_to_joint[node] = Some(new_id as u32);
        }
    }

    let file_ibms = read_ibms(&skin, buffers);
    let mut ibms = remap_ibms(&file_ibms, &order, joint_nodes.len());
    // The cooked vertices are `bake · p`; the file's inverse bind matrices expect `p`.
    let unbake = bake.inverse();
    for ibm in &mut ibms {
        *ibm *= unbake;
    }

    let mut skeleton = Skeleton {
        joint_names: names,
        parents: skel_parents.clone(),
        bind_pose: bind,
        inverse_bind_matrices: ibms,
        sockets: Vec::new(),
    };
    if skeleton.inverse_bind_matrices.len() != skeleton.len() {
        skeleton = Skeleton::from_local_bind(skeleton.joint_names, skeleton.parents, skeleton.bind_pose)
            .map_err(|e| GltfAnimError::Skin {
                index: skin_index,
                reason: e.to_string(),
            })?;
    } else {
        skeleton.validate().map_err(|e| GltfAnimError::Skin {
            index: skin_index,
            reason: e.to_string(),
        })?;
    }
    let root_joints = skeleton
        .parents
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.is_none().then_some(i as u32))
        .collect();
    Ok(BuiltSkin {
        skeleton,
        node_to_joint,
        root_joints,
        root_frames,
        mesh_node,
    })
}

/// Parents before children, always taking the lowest-numbered slot whose parent is placed (the
/// skeleton's joint ids, so this order is part of the cooked output). A min-heap of the ready
/// slots gives that order in `O(n log n)`; the former scan was quadratic, which a hostile skin of
/// tens of thousands of joints turned into a hang. A parent out of range counts as none.
fn topo_order(parents: &[Option<usize>]) -> Result<Vec<usize>, String> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let n = parents.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut ready = BinaryHeap::new();
    for (slot, parent) in parents.iter().enumerate() {
        match parent {
            Some(p) if *p < n => children[*p].push(slot),
            _ => ready.push(Reverse(slot)),
        }
    }
    let mut order = Vec::with_capacity(n);
    while let Some(Reverse(slot)) = ready.pop() {
        order.push(slot);
        ready.extend(children[slot].iter().map(|&child| Reverse(child)));
    }
    if order.len() < n {
        return Err("joint parents contain a cycle".into());
    }
    Ok(order)
}

fn read_ibms(skin: &gltf::Skin<'_>, buffers: &[&[u8]]) -> Vec<Mat4> {
    let Some(reader) = skin
        .reader(|buffer| buffers.get(buffer.index()).copied())
        .read_inverse_bind_matrices()
    else {
        return Vec::new();
    };
    reader
        .map(|m| {
            Mat4::from_cols(
                Vec4::from_array(m[0]),
                Vec4::from_array(m[1]),
                Vec4::from_array(m[2]),
                Vec4::from_array(m[3]),
            )
        })
        .collect()
}

fn remap_ibms(file_ibms: &[Mat4], order: &[usize], joint_count: usize) -> Vec<Mat4> {
    if file_ibms.len() != joint_count {
        return Vec::new();
    }
    order.iter().map(|&old| file_ibms[old]).collect()
}

fn collect_sockets(
    gltf: &gltf::Gltf,
    worlds: &[Mat4],
    parents: &[Option<usize>],
    built: &BuiltSkin,
) -> Vec<ImportedSocket> {
    let mut sockets = Vec::new();
    for node in gltf.nodes() {
        let Some(raw_name) = node.name() else {
            continue;
        };
        let Some(name) = raw_name.strip_prefix("socket_") else {
            continue;
        };
        if name.is_empty() || built.node_to_joint.get(node.index()).and_then(|j| *j).is_some() {
            continue;
        }
        let Some(joint) = nearest_joint(node.index(), parents, &built.node_to_joint) else {
            continue;
        };
        let joint_node = built
            .node_to_joint
            .iter()
            .position(|id| *id == Some(joint))
            .unwrap_or(node.index());
        let offset = if joint_node < worlds.len() && node.index() < worlds.len() {
            trs_of(worlds[joint_node].inverse() * worlds[node.index()])
        } else {
            LocalBind::default()
        };
        let joint_name = built
            .skeleton
            .joint_names
            .get(joint as usize)
            .cloned()
            .unwrap_or_else(|| format!("joint_{joint}"));
        sockets.push(ImportedSocket {
            name: name.to_string(),
            joint: joint_name,
            offset,
        });
    }
    sockets
}

fn nearest_joint(node: usize, parents: &[Option<usize>], node_to_joint: &[Option<u32>]) -> Option<u32> {
    let mut cursor = parents.get(node).copied().flatten();
    let mut guard = 0;
    while let Some(parent) = cursor {
        if let Some(Some(joint)) = node_to_joint.get(parent) {
            return Some(*joint);
        }
        cursor = parents.get(parent).copied().flatten();
        guard += 1;
        if guard > parents.len() {
            break;
        }
    }
    None
}

fn import_clips(
    gltf: &gltf::Gltf,
    buffers: &[&[u8]],
    built: &BuiltSkin,
    settings: &GltfAnimSettings,
) -> Result<Vec<AnimationClip>, GltfAnimError> {
    let mut clips = Vec::new();
    for animation in gltf.animations() {
        let anim_name = animation
            .name()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("anim_{}", animation.index()));
        let mut clip = AnimationClip::empty(anim_name.clone(), 0.0);
        let mut tracks: Vec<Option<JointTrack>> = (0..built.skeleton.len()).map(|_| None).collect();
        for (channel_index, channel) in animation.channels().enumerate() {
            let node = channel.target().node().index();
            let property = channel.target().property();
            let joint = built.node_to_joint.get(node).and_then(|j| *j);
            let on_mesh = built.mesh_node == Some(node);
            if joint.is_none() && !(on_mesh && property == Property::MorphTargetWeights) {
                continue;
            }
            let reader = channel.reader(|buffer| buffers.get(buffer.index()).copied());
            let times: Vec<f32> = reader
                .read_inputs()
                .map(|inputs| inputs.collect())
                .unwrap_or_default();
            let outputs = reader.read_outputs().ok_or_else(|| GltfAnimError::Channel {
                animation: anim_name.clone(),
                channel: channel_index,
                reason: "missing output accessor".into(),
            })?;
            let interpolation = channel.sampler().interpolation();
            match (property, outputs) {
                (Property::Translation, ReadOutputs::Translations(values)) => {
                    let joint = joint.unwrap_or(0);
                    let sampled =
                        bake_vec3(&times, &values.collect::<Vec<_>>(), interpolation).map_err(|reason| {
                            GltfAnimError::Channel {
                                animation: anim_name.clone(),
                                channel: channel_index,
                                reason,
                            }
                        })?;
                    let track =
                        tracks[joint as usize].get_or_insert_with(|| rest_track(joint, &built.skeleton));
                    track.translations = maybe_convert_vec3(sampled, joint, built);
                }
                (Property::Scale, ReadOutputs::Scales(values)) => {
                    let joint = joint.unwrap_or(0);
                    let sampled =
                        bake_vec3(&times, &values.collect::<Vec<_>>(), interpolation).map_err(|reason| {
                            GltfAnimError::Channel {
                                animation: anim_name.clone(),
                                channel: channel_index,
                                reason,
                            }
                        })?;
                    let track =
                        tracks[joint as usize].get_or_insert_with(|| rest_track(joint, &built.skeleton));
                    track.scales = maybe_convert_scale(sampled, joint, built);
                }
                (Property::Rotation, ReadOutputs::Rotations(values)) => {
                    let joint = joint.unwrap_or(0);
                    let sampled = bake_quat(&times, &values.into_f32().collect::<Vec<_>>(), interpolation)
                        .map_err(|reason| GltfAnimError::Channel {
                            animation: anim_name.clone(),
                            channel: channel_index,
                            reason,
                        })?;
                    let track =
                        tracks[joint as usize].get_or_insert_with(|| rest_track(joint, &built.skeleton));
                    track.rotations = maybe_convert_quat(sampled, joint, built);
                }
                (Property::MorphTargetWeights, ReadOutputs::MorphTargetWeights(values)) => {
                    let flat: Vec<f32> = values.into_f32().collect();
                    clip.morph_tracks.extend(
                        morph_tracks(&times, &flat, interpolation, None, None).map_err(|reason| {
                            GltfAnimError::Channel {
                                animation: anim_name.clone(),
                                channel: channel_index,
                                reason,
                            }
                        })?,
                    );
                }
                _ => {}
            }
        }
        clip.tracks = tracks.into_iter().flatten().collect();
        let mut end = 0.0f32;
        for track in &clip.tracks {
            end = end
                .max(track.translations.end_time())
                .max(track.rotations.end_time())
                .max(track.scales.end_time());
        }
        // Morph weights on the same animation count too (a blink on a skinned mesh).
        for track in &clip.morph_tracks {
            end = end.max(track.weights.end_time());
        }
        clip.duration = end;
        clip.sample_rate = 30.0;
        if let Some(&root) = built.root_joints.first()
            && settings.root_motion != RootMotionMode::None
        {
            clip.root_motion = Some(RootMotionTrack {
                joint: root,
                mode: settings.root_motion,
            });
        }
        let keep = !clip.tracks.is_empty() || !clip.morph_tracks.is_empty();
        emit_clips(&mut clips, clip, &anim_name, settings, keep);
    }
    Ok(clips)
}

fn rest_track(joint: u32, skeleton: &Skeleton) -> JointTrack {
    let bind = skeleton
        .bind_pose
        .get(joint as usize)
        .copied()
        .unwrap_or_default();
    JointTrack {
        joint,
        translations: TrackCurve::Static(bind.translation),
        rotations: TrackCurve::Static(bind.rotation),
        scales: TrackCurve::Static(bind.scale),
    }
}

/// The scene-space frame of root joint `joint` (`None` for other joints or an identity frame).
fn root_frame(joint: u32, built: &BuiltSkin) -> Option<LocalBind> {
    built
        .root_frames
        .iter()
        .find(|(j, _)| *j == joint)
        .map(|(_, frame)| *frame)
        .filter(|f| {
            !(f.translation.abs_diff_eq(Vec3::ZERO, 0.0)
                && f.rotation.abs_diff_eq(Quat::IDENTITY, 0.0)
                && f.scale.abs_diff_eq(Vec3::ONE, 0.0))
        })
}

/// Turns a root joint's translation keys into scene space (`frame · t` as a point).
fn maybe_convert_vec3(curve: TrackCurve<Vec3>, joint: u32, built: &BuiltSkin) -> TrackCurve<Vec3> {
    let Some(frame) = root_frame(joint, built) else {
        return curve;
    };
    map_curve(curve, |v| frame.translation + frame.rotation * (frame.scale * v))
}

/// Turns a root joint's rotation keys into scene space (`frame.rotation · r`).
fn maybe_convert_quat(curve: TrackCurve<Quat>, joint: u32, built: &BuiltSkin) -> TrackCurve<Quat> {
    let Some(frame) = root_frame(joint, built) else {
        return curve;
    };
    map_curve(curve, |v| (frame.rotation * v).normalize())
}

/// Turns a root joint's scale keys into scene space (`frame.scale · s`; exact for the uniform
/// scales armature and unit nodes use).
fn maybe_convert_scale(curve: TrackCurve<Vec3>, joint: u32, built: &BuiltSkin) -> TrackCurve<Vec3> {
    let Some(frame) = root_frame(joint, built) else {
        return curve;
    };
    map_curve(curve, |v| frame.scale * v)
}

/// Applies `f` to every value of `curve`, keeping its keys.
fn map_curve<T>(curve: TrackCurve<T>, f: impl Fn(T) -> T) -> TrackCurve<T> {
    match curve {
        TrackCurve::Static(v) => TrackCurve::Static(f(v)),
        TrackCurve::Animated {
            times,
            values,
            exact_rotation,
        } => TrackCurve::Animated {
            times,
            values: values.into_iter().map(f).collect(),
            exact_rotation,
        },
    }
}

fn bake_vec3(
    times: &[f32],
    outputs: &[[f32; 3]],
    interpolation: Interpolation,
) -> Result<TrackCurve<Vec3>, String> {
    let values = match interpolation {
        Interpolation::CubicSpline => bake_cubic_vec3(times, outputs)?,
        Interpolation::Step => {
            let linear: Vec<Vec3> = outputs.iter().map(|v| Vec3::from_array(*v)).collect();
            let (t, v) = hold_step(times, &linear)?;
            return Ok(curve_vec3(t, v));
        }
        Interpolation::Linear => {
            if outputs.len() != times.len() {
                return Err(format!(
                    "translation key count {} != time count {}",
                    outputs.len(),
                    times.len()
                ));
            }
            outputs.iter().map(|v| Vec3::from_array(*v)).collect()
        }
    };
    let times = if interpolation == Interpolation::CubicSpline {
        cubic_times(times)?
    } else {
        times.to_vec()
    };
    Ok(curve_vec3(times, values))
}

fn bake_quat(
    times: &[f32],
    outputs: &[[f32; 4]],
    interpolation: Interpolation,
) -> Result<TrackCurve<Quat>, String> {
    let to_quat = |v: [f32; 4]| Quat::from_xyzw(v[0], v[1], v[2], v[3]).normalize();
    let values = match interpolation {
        Interpolation::CubicSpline => bake_cubic_quat(times, outputs)?,
        Interpolation::Step => {
            let linear: Vec<Quat> = outputs.iter().copied().map(to_quat).collect();
            let (t, v) = hold_step(times, &linear)?;
            return Ok(curve_quat(t, v));
        }
        Interpolation::Linear => {
            if outputs.len() != times.len() {
                return Err(format!(
                    "rotation key count {} != time count {}",
                    outputs.len(),
                    times.len()
                ));
            }
            outputs.iter().copied().map(to_quat).collect()
        }
    };
    let times = if interpolation == Interpolation::CubicSpline {
        cubic_times(times)?
    } else {
        times.to_vec()
    };
    Ok(curve_quat(times, values))
}

fn curve_vec3(times: Vec<f32>, values: Vec<Vec3>) -> TrackCurve<Vec3> {
    if values.len() <= 1 || values.iter().all(|v| (*v - values[0]).length() <= 1e-8) {
        return TrackCurve::Static(values.first().copied().unwrap_or(Vec3::ZERO));
    }
    TrackCurve::Animated {
        times,
        values,
        exact_rotation: false,
    }
}

fn curve_quat(times: Vec<f32>, values: Vec<Quat>) -> TrackCurve<Quat> {
    if values.len() <= 1 || values.iter().all(|q| q.angle_between(values[0]) <= 1e-6) {
        return TrackCurve::Static(values.first().copied().unwrap_or(Quat::IDENTITY));
    }
    TrackCurve::Animated {
        times,
        values,
        exact_rotation: false,
    }
}

fn hold_step<T: Copy>(times: &[f32], values: &[T]) -> Result<(Vec<f32>, Vec<T>), String> {
    if times.len() != values.len() || times.is_empty() {
        return Err("step channel key counts disagree".into());
    }
    let mut out_t = Vec::new();
    let mut out_v = Vec::new();
    for i in 0..times.len() {
        out_t.push(times[i]);
        out_v.push(values[i]);
        if let Some(&next) = times.get(i + 1) {
            let hold = (next - 1.0e-4).max(times[i]);
            if hold > times[i] && hold < next {
                out_t.push(hold);
                out_v.push(values[i]);
            }
        }
    }
    Ok((out_t, out_v))
}

/// Most samples one cubic-spline channel bakes to: about 9.7 hours at 30 Hz. A longer span is a
/// channel error, so two keys far apart cannot make the cook allocate without bound.
const MAX_CUBIC_SAMPLES: usize = 1 << 20;

/// Sample times used when baking a cubic-spline channel (glTF Appendix C) at 30 Hz.
///
/// # Errors
///
/// A key time so large that adding the 30 Hz step no longer changes it in `f32` (from 2^20 s on)
/// or an infinite one: the walk would never reach the next key (audit asset_pipeline1 #1). Also a
/// channel that needs more than `MAX_CUBIC_SAMPLES` samples.
fn cubic_times(keys: &[f32]) -> Result<Vec<f32>, String> {
    if keys.len() < 2 {
        return Ok(keys.to_vec());
    }
    let mut times = Vec::new();
    let step = 1.0 / 30.0;
    let too_many = || {
        format!("cubic-spline channel needs more than {MAX_CUBIC_SAMPLES} samples at 30 Hz; split the clip")
    };
    for pair in keys.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        // `t < b` is false when either key is NaN, and when both are infinite, so the walk
        // below never runs and would otherwise return those keys as a successful bake.
        if !a.is_finite() || !b.is_finite() {
            return Err(format!(
                "cubic-spline key time {a} is too large to sample at 30 Hz in f32 seconds"
            ));
        }
        times.push(a);
        let mut t = a + step;
        while t < b - 1e-5 {
            // `t` stops moving once half its ulp exceeds the step: the loop would never end.
            if t <= times.last().copied().unwrap_or(a) || !t.is_finite() {
                return Err(format!(
                    "cubic-spline key time {a} is too large to sample at 30 Hz in f32 seconds"
                ));
            }
            if times.len() >= MAX_CUBIC_SAMPLES {
                return Err(too_many());
            }
            times.push(t);
            t += step;
        }
    }
    if let Some(&last) = keys.last() {
        times.push(last);
    }
    if times.len() > MAX_CUBIC_SAMPLES {
        return Err(too_many());
    }
    Ok(times)
}

fn bake_cubic_vec3(times: &[f32], outputs: &[[f32; 3]]) -> Result<Vec<Vec3>, String> {
    if times.len() < 2 || outputs.len() != times.len() * 3 {
        return Err(format!(
            "cubic translation needs 3 outputs per key, got {} outputs for {} keys",
            outputs.len(),
            times.len()
        ));
    }
    let sample_at = |t: f32| -> Vec3 {
        let k = cubic_span(times, t);
        let dt = (times[k + 1] - times[k]).max(1e-8);
        let u = ((t - times[k]) / dt).clamp(0.0, 1.0);
        let p0 = Vec3::from_array(outputs[k * 3 + 1]);
        let m0 = Vec3::from_array(outputs[k * 3 + 2]) * dt;
        let p1 = Vec3::from_array(outputs[(k + 1) * 3 + 1]);
        let m1 = Vec3::from_array(outputs[(k + 1) * 3]) * dt;
        hermite_vec3(p0, m0, p1, m1, u)
    };
    Ok(cubic_times(times)?.into_iter().map(sample_at).collect())
}

fn bake_cubic_quat(times: &[f32], outputs: &[[f32; 4]]) -> Result<Vec<Quat>, String> {
    if times.len() < 2 || outputs.len() != times.len() * 3 {
        return Err(format!(
            "cubic rotation needs 3 outputs per key, got {} outputs for {} keys",
            outputs.len(),
            times.len()
        ));
    }
    let q = |v: [f32; 4]| Quat::from_xyzw(v[0], v[1], v[2], v[3]);
    let sample_at = |t: f32| -> Quat {
        let k = cubic_span(times, t);
        let dt = (times[k + 1] - times[k]).max(1e-8);
        let u = ((t - times[k]) / dt).clamp(0.0, 1.0);
        let p0 = q(outputs[k * 3 + 1]);
        let m0 = q(outputs[k * 3 + 2]) * dt;
        let p1 = q(outputs[(k + 1) * 3 + 1]);
        let m1 = q(outputs[(k + 1) * 3]) * dt;
        hermite_quat(p0, m0, p1, m1, u)
    };
    Ok(cubic_times(times)?.into_iter().map(sample_at).collect())
}

fn cubic_span(times: &[f32], t: f32) -> usize {
    let mut k = 0;
    while k + 2 < times.len() && t >= times[k + 1] {
        k += 1;
    }
    k
}

fn hermite_vec3(p0: Vec3, m0: Vec3, p1: Vec3, m1: Vec3, t: f32) -> Vec3 {
    Vec3::new(
        hermite(p0.x, m0.x, p1.x, m1.x, t),
        hermite(p0.y, m0.y, p1.y, m1.y, t),
        hermite(p0.z, m0.z, p1.z, m1.z, t),
    )
}

fn hermite_quat(p0: Quat, m0: Quat, p1: Quat, m1: Quat, t: f32) -> Quat {
    Quat::from_xyzw(
        hermite(p0.x, m0.x, p1.x, m1.x, t),
        hermite(p0.y, m0.y, p1.y, m1.y, t),
        hermite(p0.z, m0.z, p1.z, m1.z, t),
        hermite(p0.w, m0.w, p1.w, m1.w, t),
    )
    .normalize()
}

/// glTF Appendix C cubic Hermite basis. `m0`/`m1` are already scaled by the key delta.
fn hermite(p0: f32, m0: f32, p1: f32, m1: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    (2.0 * t3 - 3.0 * t2 + 1.0) * p0 + (t3 - 2.0 * t2 + t) * m0 + (-2.0 * t3 + 3.0 * t2) * p1 + (t3 - t2) * m1
}

/// Morph targets of the mesh on `node` (`None` without a mesh or without targets).
fn node_morph_target_count(node: &gltf::Node<'_>) -> Option<usize> {
    let mesh = node.mesh()?;
    let from_primitives = mesh
        .primitives()
        .map(|primitive| primitive.morph_targets().count())
        .max()
        .unwrap_or(0);
    let count = from_primitives.max(mesh.weights().map_or(0, <[f32]>::len));
    (count > 0).then_some(count)
}

/// Every animation's `weights` channels as per-node morph tracks (ADR-297). A channel's output
/// count must be keys x the node's mesh target count (x3 for cubic spline); a node without a mesh
/// or targets is a warning and the channel is skipped.
fn node_morph_clips(
    gltf: &gltf::Gltf,
    buffers: &[&[u8]],
    settings: &GltfAnimSettings,
    warnings: &mut Vec<String>,
) -> Result<Vec<AnimationClip>, GltfAnimError> {
    let mut clips = Vec::new();
    for animation in gltf.animations() {
        let anim_name = animation
            .name()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("anim_{}", animation.index()));
        let mut clip = AnimationClip::empty(anim_name.clone(), 0.0);
        for (channel_index, channel) in animation.channels().enumerate() {
            if channel.target().property() != Property::MorphTargetWeights {
                continue;
            }
            let target_node = channel.target().node();
            let Some(targets) = node_morph_target_count(&target_node) else {
                warnings.push(format!(
                    "animation {anim_name} channel {channel_index}: node {} ({}) has no mesh with morph targets; the weights channel is skipped",
                    target_node.index(),
                    target_node.name().unwrap_or("unnamed"),
                ));
                continue;
            };
            let reader = channel.reader(|buffer| buffers.get(buffer.index()).copied());
            let times: Vec<f32> = reader
                .read_inputs()
                .map(|inputs| inputs.collect())
                .unwrap_or_default();
            let outputs = reader.read_outputs().ok_or_else(|| GltfAnimError::Channel {
                animation: anim_name.clone(),
                channel: channel_index,
                reason: "missing output accessor".into(),
            })?;
            let ReadOutputs::MorphTargetWeights(values) = outputs else {
                continue;
            };
            let flat: Vec<f32> = values.into_f32().collect();
            let node = MorphNode {
                index: target_node.index() as u32,
                name: target_node.name().unwrap_or_default().to_string(),
            };
            let tracks = morph_tracks(
                &times,
                &flat,
                channel.sampler().interpolation(),
                Some(&node),
                Some(targets),
            )
            .map_err(|reason| GltfAnimError::Channel {
                animation: anim_name.clone(),
                channel: channel_index,
                reason,
            })?;
            clip.morph_tracks.extend(tracks);
        }
        clip.node_animation = import_node_tracks(gltf, &animation, buffers, settings)?;
        if clip.morph_tracks.is_empty() && clip.node_animation.is_none() {
            continue;
        }
        clip.duration = clip.morph_tracks.iter().fold(
            clip.node_animation.as_ref().map_or(0.0, |nodes| nodes.end),
            |end, track| end.max(track.weights.end_time()),
        );
        if let Some(nodes) = &mut clip.node_animation {
            nodes.end = clip.duration;
        }
        clip.sample_rate = 30.0;
        emit_clips(&mut clips, clip, &anim_name, settings, true);
    }
    Ok(clips)
}

fn import_node_tracks(
    gltf: &gltf::Gltf,
    animation: &gltf::Animation<'_>,
    buffers: &[&[u8]],
    settings: &GltfAnimSettings,
) -> Result<Option<regia_animation::node_tracks::NodeAnimation>, GltfAnimError> {
    use regia_animation::node_tracks::{
        NODE_TRACKS_VERSION, NodeAnimation, NodeCurve, NodeInterpolation, NodeTrack,
    };
    let joint_nodes: std::collections::BTreeSet<_> = gltf
        .skins()
        .flat_map(|skin| skin.joints().map(|node| node.index()))
        .collect();
    if !animation.channels().any(|c| {
        c.target().property() != Property::MorphTargetWeights
            && !joint_nodes.contains(&c.target().node().index())
    }) {
        return Ok(None);
    }
    let source_nodes: Vec<_> = gltf.nodes().collect();
    let parents = node_parents(gltf, source_nodes.len());
    let order = topo_order(&parents).map_err(GltfAnimError::Parse)?;
    let mut slots = vec![0u32; order.len()];
    for (slot, &index) in order.iter().enumerate() {
        slots[index] = slot as u32;
    }
    let mut nodes: Vec<_> = order
        .iter()
        .map(|&index| {
            let node = &source_nodes[index];
            let (bind, matrix) = match node.transform() {
                gltf::scene::Transform::Matrix { .. } => {
                    (LocalBind::default(), Some(matrix_of(node.transform())))
                }
                gltf::scene::Transform::Decomposed {
                    translation,
                    rotation,
                    scale,
                } => (
                    LocalBind {
                        translation: Vec3::from_array(translation),
                        rotation: Quat::from_array(rotation),
                        scale: Vec3::from_array(scale),
                    },
                    None,
                ),
            };
            NodeTrack {
                node: MorphNode {
                    index: index as u32,
                    name: node.name().unwrap_or_default().into(),
                },
                parent: parents[index].map(|p| slots[p]),
                is_joint: joint_nodes.contains(&index),
                bind,
                matrix,
                translations: None,
                rotations: None,
                scales: None,
            }
        })
        .collect();
    let mut end = 0.0f32;
    for (channel_index, channel) in animation.channels().enumerate() {
        let property = channel.target().property();
        if property == Property::MorphTargetWeights {
            continue;
        }
        let error = |reason| GltfAnimError::Channel {
            animation: animation
                .name()
                .map(str::to_string)
                .unwrap_or_else(|| format!("anim_{}", animation.index())),
            channel: channel_index,
            reason,
        };
        let slot = slots[channel.target().node().index()] as usize;
        let node = &mut nodes[slot];
        if node.matrix.is_some() {
            return Err(error("an animated node must use TRS, not a matrix".into()));
        }
        let reader = channel.reader(|buffer| buffers.get(buffer.index()).copied());
        let times: Vec<_> = reader
            .read_inputs()
            .ok_or_else(|| error("missing input accessor".into()))?
            .collect();
        let interpolation = match channel.sampler().interpolation() {
            Interpolation::Step => NodeInterpolation::Step,
            Interpolation::Linear => NodeInterpolation::Linear,
            Interpolation::CubicSpline => NodeInterpolation::CubicSpline,
        };
        end = end.max(times.last().copied().unwrap_or(0.0));
        match (property, reader.read_outputs()) {
            (Property::Translation, Some(ReadOutputs::Translations(values)))
            | (Property::Scale, Some(ReadOutputs::Scales(values))) => {
                let curve = NodeCurve {
                    times,
                    values: values.map(Vec3::from_array).collect(),
                    interpolation,
                };
                curve.validate().map_err(error)?;
                let target = if property == Property::Translation {
                    &mut node.translations
                } else {
                    &mut node.scales
                };
                if target.replace(curve).is_some() {
                    return Err(error("duplicate node channel".into()));
                }
            }
            (Property::Rotation, Some(ReadOutputs::Rotations(values))) => {
                let curve = NodeCurve {
                    times,
                    values: values.into_f32().map(Quat::from_array).collect(),
                    interpolation,
                };
                curve.validate().map_err(error)?;
                if node.rotations.replace(curve).is_some() {
                    return Err(error("duplicate node channel".into()));
                }
            }
            _ => return Err(error("missing or mismatched output accessor".into())),
        }
    }
    let animation = NodeAnimation {
        version: NODE_TRACKS_VERSION,
        frame: axis_matrix(settings.forward, settings.scale),
        start: 0.0,
        end,
        nodes,
    };
    animation.validate().map_err(GltfAnimError::Parse)?;
    Ok(Some(animation))
}

fn morph_tracks(
    times: &[f32],
    outputs: &[f32],
    interpolation: Interpolation,
    node: Option<&MorphNode>,
    expected_targets: Option<usize>,
) -> Result<Vec<MorphTrack>, String> {
    let per_key = match interpolation {
        Interpolation::CubicSpline => 3,
        _ => 1,
    };
    if times.is_empty() || !outputs.len().is_multiple_of(times.len() * per_key) {
        return Err("morph weight output count is not a multiple of the key count".into());
    }
    if let Some(expected) = expected_targets
        && outputs.len() != times.len() * per_key * expected
    {
        let cubic = if per_key == 3 { " x 3 (cubic spline)" } else { "" };
        return Err(format!(
            "morph weight output count {} does not match {} keys x {expected} morph targets{cubic}",
            outputs.len(),
            times.len(),
        ));
    }
    let targets = outputs.len() / (times.len() * per_key);
    // The baked sample times are the same for every target.
    let key_times = if interpolation == Interpolation::CubicSpline {
        cubic_times(times)?
    } else {
        times.to_vec()
    };
    let mut tracks = Vec::with_capacity(targets);
    for target in 0..targets {
        let series: Vec<f32> = match interpolation {
            Interpolation::CubicSpline => {
                let flat: Vec<f32> = (0..times.len() * 3)
                    .map(|k| outputs[k * targets + target])
                    .collect();
                // Reuse the vec3 baker's timing by treating weights as a 1-component hermite.
                key_times
                    .iter()
                    .map(|&t| {
                        let k = cubic_span(times, t);
                        let dt = (times[k + 1] - times[k]).max(1e-8);
                        let u = ((t - times[k]) / dt).clamp(0.0, 1.0);
                        let p0 = flat[k * 3 + 1];
                        let m0 = flat[k * 3 + 2] * dt;
                        let p1 = flat[(k + 1) * 3 + 1];
                        let m1 = flat[(k + 1) * 3] * dt;
                        hermite(p0, m0, p1, m1, u)
                    })
                    .collect()
            }
            _ => (0..times.len()).map(|k| outputs[k * targets + target]).collect(),
        };
        let (key_times, series) = if interpolation == Interpolation::Step {
            hold_step(times, &series)?
        } else {
            (key_times.clone(), series)
        };
        tracks.push(MorphTrack {
            target: target as u32,
            node: node.cloned(),
            weights: if series.iter().all(|v| (v - series[0]).abs() <= 1e-6) {
                TrackCurve::Static(series[0])
            } else {
                TrackCurve::Animated {
                    times: key_times,
                    values: series,
                    exact_rotation: false,
                }
            },
        });
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::pose::Pose;

    fn f32_bytes(values: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(values.len() * 4);
        for v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// sec-review-assets-agent (ADR-5869): with a base folder, an animated glTF's buffers were
    /// loaded from any URI (`import_buffers` opens an absolute path as given), so a BVH retarget
    /// target read a host file into the cooked skeleton. The URIs are checked first now.
    #[test]
    fn an_animated_gltf_with_an_absolute_buffer_uri_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("gltf_anim_abs_uri");
        let secret = dir.join("secret.bin");
        let mut bin = f32_bytes(&[0.0, 1.0]);
        bin.extend(f32_bytes(&[0.0, 0.0, 0.0, 1.0, 2.0, 3.0]));
        std::fs::write(&secret, &bin).expect("secret");
        let absolute = secret.to_string_lossy().replace('\\', "/");
        let absolute = if absolute.starts_with('/') {
            absolute
        } else {
            format!("/{absolute}")
        };
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"nodes":[{{"name":"n"}}],
"animations":[{{"channels":[{{"sampler":0,"target":{{"node":0,"path":"translation"}}}}],"samplers":[{{"input":0,"output":1}}]}}],
"buffers":[{{"uri":"{absolute}","byteLength":32}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":8}},{{"buffer":0,"byteOffset":8,"byteLength":24}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]}},
{{"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"}}]}}"#
        );
        let result =
            import_gltf_animation_with_base(json.as_bytes(), Some(&dir), &GltfAnimSettings::default());
        match result {
            Err(GltfAnimError::Parse(why)) => {
                assert!(
                    why.contains("absolute") || why.contains("not a relative path"),
                    "{why}"
                );
            }
            Err(other) => panic!("expected the URI check, got {other}"),
            Ok(_) => panic!("an absolute buffer URI must be refused"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mat4_translation(x: f32, y: f32, z: f32) -> [f32; 16] {
        [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, x, y, z, 1.0,
        ]
    }

    /// Two-joint skin, a `socket_muzzle`, and linear translation clips on the root and the bone.
    fn sample_glb() -> Vec<u8> {
        let (json, bin) = sample_json_and_bin();
        glb(&json, &bin)
    }

    /// The JSON (buffer 0 declared without a `uri`) and binary payload of [`sample_glb`].
    fn sample_json_and_bin() -> (String, Vec<u8>) {
        let mut bin = Vec::new();
        bin.extend(f32_bytes(&mat4_translation(-1.0, 0.0, 0.0)));
        bin.extend(f32_bytes(&mat4_translation(-1.0, -1.0, 0.0)));
        bin.extend(f32_bytes(&[0.0, 1.0]));
        bin.extend(f32_bytes(&[0.0, 1.0, 0.0, 0.0, 2.0, 0.0]));
        bin.extend(f32_bytes(&[1.0, 0.0, 0.0, 3.0, 0.0, 0.0]));
        assert_eq!(bin.len(), 128 + 8 + 24 + 24);

        let json = r#"{
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [
                {"name": "root", "translation": [1.0, 0.0, 0.0], "children": [1]},
                {"name": "bone", "translation": [0.0, 1.0, 0.0], "children": [2]},
                {"name": "socket_muzzle", "translation": [0.0, 0.0, -0.2]}
            ],
            "skins": [{"joints": [0, 1], "inverseBindMatrices": 0, "skeleton": 0}],
            "animations": [{
                "name": "lift",
                "channels": [
                    {"sampler": 0, "target": {"node": 1, "path": "translation"}},
                    {"sampler": 1, "target": {"node": 0, "path": "translation"}}
                ],
                "samplers": [
                    {"input": 1, "output": 2, "interpolation": "LINEAR"},
                    {"input": 1, "output": 3, "interpolation": "LINEAR"}
                ]
            }],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 2, "type": "MAT4"},
                {"bufferView": 1, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0]},
                {"bufferView": 2, "componentType": 5126, "count": 2, "type": "VEC3"},
                {"bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC3"}
            ],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 128},
                {"buffer": 0, "byteOffset": 128, "byteLength": 8},
                {"buffer": 0, "byteOffset": 136, "byteLength": 24},
                {"buffer": 0, "byteOffset": 160, "byteLength": 24}
            ],
            "buffers": [{"byteLength": 184}]
        }"#;
        (json.to_string(), bin)
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
        out.extend(std::iter::repeat_n(0, bin_pad));
        out
    }

    fn palette_near_identity(skeleton: &Skeleton) {
        let pose = skeleton.bind_pose_pose();
        let globals = skeleton.globals(&pose, None);
        let palette = skeleton.skin_palette(&globals);
        for (i, matrix) in palette.iter().enumerate() {
            assert!(
                matrix.abs_diff_eq(Mat4::IDENTITY, 1e-4),
                "joint {i} bind palette is not identity:\n{matrix:?}"
            );
        }
    }

    /// A `.gltf` whose accessors live in an external `.bin` imports its skin and clip when the
    /// importer is given the directory; the bytes-only entry point has no buffer 0 and says so
    /// (the cook used to hit that error for every external-buffer animated model).
    #[test]
    fn external_buffers_are_read_from_the_base_directory() {
        let dir = regia_test_utils::unique_temp_dir("gltf-anim-ext");
        let (json, bin) = sample_json_and_bin();
        let json = json.replace(
            r#""buffers": [{"byteLength": 184}]"#,
            r#""buffers": [{"uri": "hero.bin", "byteLength": 184}]"#,
        );
        std::fs::write(dir.join("hero.bin"), &bin).expect("bin");
        let settings = GltfAnimSettings {
            forward: crate::Forward::Keep,
            ..GltfAnimSettings::default()
        };
        let with_base =
            import_gltf_animation_with_base(json.as_bytes(), Some(&dir), &settings).expect("import");
        let embedded = import_gltf_animation(&sample_glb(), &settings).expect("embedded");
        assert_eq!(with_base.skins.len(), 1);
        assert_eq!(with_base.skins[0].clips.len(), 1);
        assert_eq!(with_base.skins[0].clips[0].name, "lift");
        assert_eq!(with_base.skins[0].skeleton, embedded.skins[0].skeleton);
        assert!(import_gltf_animation(json.as_bytes(), &settings).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_skin_clip_and_socket_keep() {
        let settings = GltfAnimSettings {
            forward: crate::Forward::Keep,
            ..GltfAnimSettings::default()
        };
        let imported = import_gltf_animation(&sample_glb(), &settings).expect("import");
        assert_eq!(imported.skins.len(), 1);
        let skin = &imported.skins[0];
        assert_eq!(skin.skeleton.joint_names, ["root", "bone"]);
        assert_eq!(skin.skeleton.parents, vec![None, Some(0)]);
        assert!((skin.skeleton.bind_pose[0].translation.x - 1.0).abs() < 1e-5);
        assert!((skin.skeleton.bind_pose[1].translation.y - 1.0).abs() < 1e-5);
        palette_near_identity(&skin.skeleton);
        assert_eq!(skin.sockets.len(), 1);
        assert_eq!(skin.sockets[0].name, "muzzle");
        assert_eq!(skin.sockets[0].joint, "bone");
        assert!((skin.sockets[0].offset.translation.z + 0.2).abs() < 1e-4);
        assert_eq!(skin.clips.len(), 1);
        assert_eq!(skin.clips[0].name, "lift");
        let mut pose = Pose::from_identity(2);
        skin.clips[0].sample_pose(1.0, &mut pose);
        assert!(
            (pose.translations[1].y - 2.0).abs() < 1e-4,
            "bone y {}",
            pose.translations[1].y
        );
        assert!(
            (pose.translations[0].x - 3.0).abs() < 1e-4,
            "root x {}",
            pose.translations[0].x
        );
    }

    #[test]
    fn scale_multiplies_the_keep_root_and_a_non_positive_scale_does_not() {
        let scaled = GltfAnimSettings {
            forward: crate::Forward::Keep,
            scale: 2.0,
            ..GltfAnimSettings::default()
        };
        let imported = import_gltf_animation(&sample_glb(), &scaled).expect("import");
        let x = imported.skins[0].skeleton.bind_pose[0].translation.x;
        assert!((x - 2.0).abs() < 1e-4, "scaled root x {x}");
        let ignored = GltfAnimSettings {
            forward: crate::Forward::Keep,
            scale: 0.0,
            ..GltfAnimSettings::default()
        };
        let imported = import_gltf_animation(&sample_glb(), &ignored).expect("import");
        let x = imported.skins[0].skeleton.bind_pose[0].translation.x;
        assert!((x - 1.0).abs() < 1e-4, "non-positive scale stays 1, root x {x}");
    }

    #[test]
    fn forward_convert_yaws_root_and_keeps_bind_palette() {
        let imported = import_gltf_animation(&sample_glb(), &GltfAnimSettings::default()).expect("import");
        let skin = &imported.skins[0];
        assert!(
            (skin.skeleton.bind_pose[0].translation.x + 1.0).abs() < 1e-4,
            "converted root x {}",
            skin.skeleton.bind_pose[0].translation.x
        );
        assert!((skin.skeleton.bind_pose[1].translation.y - 1.0).abs() < 1e-4);
        palette_near_identity(&skin.skeleton);
        let mut pose = Pose::from_identity(2);
        skin.clips[0].sample_pose(1.0, &mut pose);
        assert!(
            (pose.translations[0].x + 3.0).abs() < 1e-4,
            "converted root key x {}",
            pose.translations[0].x
        );
        assert!((pose.translations[1].y - 2.0).abs() < 1e-4);
    }

    #[test]
    fn splits_clips_by_time_range() {
        let settings = GltfAnimSettings {
            forward: crate::Forward::Keep,
            ranges: vec![ClipRange {
                name: "end".into(),
                start: 0.5,
                end: 1.0,
            }],
            ..GltfAnimSettings::default()
        };
        let imported = import_gltf_animation(&sample_glb(), &settings).expect("import");
        let clip = &imported.skins[0].clips[0];
        assert_eq!(clip.name, "lift_end");
        assert!((clip.duration - 0.5).abs() < 1e-4);
        let mut pose = Pose::from_identity(2);
        clip.sample_pose(0.5, &mut pose);
        assert!((pose.translations[1].y - 2.0).abs() < 1e-3);
    }

    /// ADR-7904: one take split into named clips by its own ranges, a rename, a skip, and the
    /// file-wide ranges still applying to an animation no entry names.
    #[test]
    fn per_animation_clip_options_split_rename_and_skip() {
        let split = GltfAnimSettings {
            forward: crate::Forward::Keep,
            clips: vec![ClipOptions {
                animation: "lift".into(),
                ranges: vec![
                    ClipRange {
                        name: "up".into(),
                        start: 0.0,
                        end: 0.5,
                    },
                    ClipRange {
                        name: String::new(),
                        start: 0.5,
                        end: 1.0,
                    },
                ],
                ..ClipOptions::default()
            }],
            // An entry's own ranges replace the file-wide ones for its animation.
            ranges: vec![ClipRange {
                name: "ignored".into(),
                start: 0.0,
                end: 0.1,
            }],
            ..GltfAnimSettings::default()
        };
        let clips = import_gltf_animation(&sample_glb(), &split)
            .expect("import")
            .skins[0]
            .clips
            .clone();
        let names: Vec<&str> = clips.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["up", "lift_1"]);
        assert!((clips[0].duration - 0.5).abs() < 1e-4);
        assert!((clips[1].duration - 0.5).abs() < 1e-4);

        let renamed = GltfAnimSettings {
            forward: crate::Forward::Keep,
            clips: vec![ClipOptions {
                animation: "lift".into(),
                name: Some("raise".into()),
                ..ClipOptions::default()
            }],
            ..GltfAnimSettings::default()
        };
        let clips = import_gltf_animation(&sample_glb(), &renamed)
            .expect("import")
            .skins[0]
            .clips
            .clone();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "raise");
        assert!((clips[0].duration - 1.0).abs() < 1e-4);

        let skipped = GltfAnimSettings {
            forward: crate::Forward::Keep,
            clips: vec![ClipOptions {
                animation: "lift".into(),
                skip: true,
                ..ClipOptions::default()
            }],
            ..GltfAnimSettings::default()
        };
        let imported = import_gltf_animation(&sample_glb(), &skipped).expect("import");
        assert!(imported.skins[0].clips.is_empty(), "a skipped take cooks no clip");

        // An entry for another animation leaves this one as it was.
        let other = GltfAnimSettings {
            forward: crate::Forward::Keep,
            clips: vec![ClipOptions {
                animation: "walk".into(),
                skip: true,
                ..ClipOptions::default()
            }],
            ..GltfAnimSettings::default()
        };
        let clips = import_gltf_animation(&sample_glb(), &other)
            .expect("import")
            .skins[0]
            .clips
            .clone();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "lift");
    }

    #[test]
    fn skin_clip_duration_covers_morph_weight_keys() {
        // Add a 3-second weights channel (one target) to the 1-second "lift" animation.
        let (json, mut bin) = sample_json_and_bin();
        bin.extend(f32_bytes(&[0.0, 3.0]));
        bin.extend(f32_bytes(&[0.0, 1.0]));
        let json = json
            .replace(
                r#"{"sampler": 1, "target": {"node": 0, "path": "translation"}}"#,
                r#"{"sampler": 1, "target": {"node": 0, "path": "translation"}},
                    {"sampler": 2, "target": {"node": 0, "path": "weights"}}"#,
            )
            .replace(
                r#"{"input": 1, "output": 3, "interpolation": "LINEAR"}"#,
                r#"{"input": 1, "output": 3, "interpolation": "LINEAR"},
                    {"input": 4, "output": 5, "interpolation": "LINEAR"}"#,
            )
            .replace(
                r#"{"bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC3"}"#,
                r#"{"bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC3"},
                {"bufferView": 4, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [3.0]},
                {"bufferView": 5, "componentType": 5126, "count": 2, "type": "SCALAR"}"#,
            )
            .replace(
                r#"{"buffer": 0, "byteOffset": 160, "byteLength": 24}"#,
                r#"{"buffer": 0, "byteOffset": 160, "byteLength": 24},
                {"buffer": 0, "byteOffset": 184, "byteLength": 8},
                {"buffer": 0, "byteOffset": 192, "byteLength": 8}"#,
            )
            .replace(r#""buffers": [{"byteLength": 184}]"#, r#""buffers": [{"byteLength": 200}]"#);
        let settings = GltfAnimSettings {
            forward: crate::Forward::Keep,
            ..GltfAnimSettings::default()
        };
        let imported = import_gltf_animation(&glb(&json, &bin), &settings).expect("import");
        let clip = &imported.skins[0].clips[0];
        assert_eq!(clip.morph_tracks.len(), 1);
        assert!((clip.duration - 3.0).abs() < 1e-5, "duration {}", clip.duration);
    }

    #[test]
    fn hostile_glb_length_is_rejected_not_panicked() {
        // A GLB header declaring a length below 12 makes the `gltf` crate compute `length - 12`
        // and underflow-panic in debug. The pre-check must return an error instead (and holds under
        // `panic = "abort"`, where a catch_unwind cannot).
        let mut glb = Vec::new();
        glb.extend_from_slice(&0x4654_6C67u32.to_le_bytes()); // magic "glTF"
        glb.extend_from_slice(&2u32.to_le_bytes()); // version 2
        glb.extend_from_slice(&0u32.to_le_bytes()); // length 0 (< 12)
        let err = import_gltf_animation(&glb, &GltfAnimSettings::default())
            .expect_err("a hostile GLB length must be rejected, not panic");
        assert!(err.to_string().contains("12-byte header"), "{err}");
    }

    /// Two nodes, each with a mesh of `a_targets` / `b_targets` morph targets (`None`: node B has
    /// no mesh), both animated over keys 0, 1, 2 with the given interpolation and outputs.
    fn morph_glb(
        a_targets: usize,
        a_interp: &str,
        a_out: &[f32],
        b_targets: Option<usize>,
        b_out: &[f32],
    ) -> Vec<u8> {
        let mut bin = Vec::new();
        bin.extend(f32_bytes(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]));
        bin.extend(f32_bytes(&[0.0, 0.0, 0.5, 0.0, 0.0, 0.5, 0.0, 0.0, 0.5]));
        bin.extend(f32_bytes(&[0.0, 1.0, 2.0]));
        bin.extend(f32_bytes(a_out));
        bin.extend(f32_bytes(b_out));
        let primitive = |targets: usize| {
            let list = vec![r#"{"POSITION": 1}"#; targets].join(",");
            format!(r#"{{"primitives": [{{"attributes": {{"POSITION": 0}}, "targets": [{list}]}}]}}"#)
        };
        let mut meshes = vec![primitive(a_targets)];
        let node_b = match b_targets {
            Some(n) => {
                meshes.push(primitive(n));
                r#"{"name": "node_b", "mesh": 1}"#
            }
            None => r#"{"name": "node_b"}"#,
        };
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scene": 0,
            "scenes": [{{"nodes": [0, 1]}}],
            "nodes": [{{"name": "node_a", "mesh": 0}}, {node_b}],
            "meshes": [{}],
            "animations": [{{
                "name": "both",
                "channels": [
                    {{"sampler": 0, "target": {{"node": 0, "path": "weights"}}}},
                    {{"sampler": 1, "target": {{"node": 1, "path": "weights"}}}}
                ],
                "samplers": [
                    {{"input": 2, "output": 3, "interpolation": "{a_interp}"}},
                    {{"input": 2, "output": 4, "interpolation": "LINEAR"}}
                ]
            }}],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]}},
                {{"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0.5], "max": [0, 0, 0.5]}},
                {{"bufferView": 2, "componentType": 5126, "count": 3, "type": "SCALAR", "min": [0.0], "max": [2.0]}},
                {{"bufferView": 3, "componentType": 5126, "count": {}, "type": "SCALAR"}},
                {{"bufferView": 4, "componentType": 5126, "count": {}, "type": "SCALAR"}}
            ],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 36}},
                {{"buffer": 0, "byteOffset": 72, "byteLength": 12}},
                {{"buffer": 0, "byteOffset": 84, "byteLength": {}}},
                {{"buffer": 0, "byteOffset": {}, "byteLength": {}}}
            ],
            "buffers": [{{"byteLength": {}}}]
        }}"#,
            meshes.join(","),
            a_out.len(),
            b_out.len(),
            a_out.len() * 4,
            84 + a_out.len() * 4,
            b_out.len() * 4,
            bin.len(),
        );
        glb(&json, &bin)
    }

    fn keep() -> GltfAnimSettings {
        GltfAnimSettings {
            forward: crate::Forward::Keep,
            ..GltfAnimSettings::default()
        }
    }

    /// The weight of `target` on glTF node `node` at `time`.
    fn node_weight(clip: &AnimationClip, node: u32, target: u32, time: f32) -> f32 {
        clip.morph_tracks
            .iter()
            .find(|t| t.target == target && t.node.as_ref().is_some_and(|n| n.index == node))
            .unwrap_or_else(|| panic!("no track for node {node} target {target}"))
            .weights
            .sample(time)
    }

    #[test]
    fn two_morph_meshes_get_their_own_linear_weights() {
        // Node A: two targets (key-major: t0, t1 per key). Node B: one target.
        let a = [0.0, 1.0, 0.5, 0.5, 1.0, 0.0];
        let b = [1.0, 0.5, 0.0];
        let imported =
            import_gltf_animation(&morph_glb(2, "LINEAR", &a, Some(1), &b), &keep()).expect("import");
        assert!(imported.skins.is_empty());
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
        assert_eq!(imported.node_clips.len(), 1);
        let clip = &imported.node_clips[0];
        assert_eq!(clip.name, "both");
        assert!((clip.duration - 2.0).abs() < 1e-6);
        assert_eq!(clip.morph_tracks.len(), 3);
        for track in &clip.morph_tracks {
            let node = track.node.as_ref().expect("bound");
            assert_eq!(node.name, if node.index == 0 { "node_a" } else { "node_b" });
        }
        for (time, a0, a1, b0) in [(0.0, 0.0, 1.0, 1.0), (1.0, 0.5, 0.5, 0.5), (2.0, 1.0, 0.0, 0.0)] {
            assert!((node_weight(clip, 0, 0, time) - a0).abs() < 1e-5, "A0 at {time}");
            assert!((node_weight(clip, 0, 1, time) - a1).abs() < 1e-5, "A1 at {time}");
            assert!((node_weight(clip, 1, 0, time) - b0).abs() < 1e-5, "B0 at {time}");
        }
        // Mid-key: A0 runs 0 -> 0.5 over the first second, B0 runs 1 -> 0.5.
        assert!((node_weight(clip, 0, 0, 0.5) - 0.25).abs() < 1e-5);
        assert!((node_weight(clip, 1, 0, 0.5) - 0.75).abs() < 1e-5);
        // The per-node tracks do not leak into the all-meshes sample.
        let mut plain = Vec::new();
        clip.sample_morphs(0.5, &mut plain);
        assert!(plain.is_empty());
    }

    #[test]
    fn cubic_spline_weights_keep_their_node() {
        // One target on each node: per key [in-tangent, value, out-tangent], zero tangents.
        let a = [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.5, 0.0];
        let b = [0.0, 0.5, 1.0];
        let imported =
            import_gltf_animation(&morph_glb(1, "CUBICSPLINE", &a, Some(1), &b), &keep()).expect("import");
        let clip = &imported.node_clips[0];
        assert!((node_weight(clip, 0, 0, 0.0) - 0.0).abs() < 1e-5);
        assert!((node_weight(clip, 0, 0, 1.0) - 1.0).abs() < 1e-5);
        assert!((node_weight(clip, 0, 0, 2.0) - 0.5).abs() < 1e-5);
        // Zero tangents: the Hermite midpoint of 0 and 1 is 0.5.
        assert!((node_weight(clip, 0, 0, 0.5) - 0.5).abs() < 0.05);
        // Node B (linear) is untouched by A's interpolation mode.
        assert!((node_weight(clip, 1, 0, 1.0) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn step_weights_hold_until_the_next_key() {
        let a = [0.0, 1.0, 0.25];
        let b = [0.0, 0.0, 0.0];
        let imported =
            import_gltf_animation(&morph_glb(1, "STEP", &a, Some(1), &b), &keep()).expect("import");
        let clip = &imported.node_clips[0];
        assert!((node_weight(clip, 0, 0, 0.0) - 0.0).abs() < 1e-5);
        assert!((node_weight(clip, 0, 0, 0.99) - 0.0).abs() < 1e-5);
        assert!((node_weight(clip, 0, 0, 1.0) - 1.0).abs() < 1e-5);
        assert!((node_weight(clip, 0, 0, 2.0) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn mismatched_output_count_is_a_channel_error() {
        // Node A has two targets but the sampler outputs three values (one target's worth).
        let result = import_gltf_animation(
            &morph_glb(2, "LINEAR", &[0.0, 0.5, 1.0], Some(1), &[0.0, 0.5, 1.0]),
            &keep(),
        );
        match result {
            Err(GltfAnimError::Channel { channel, reason, .. }) => {
                assert_eq!(channel, 0);
                assert!(
                    reason.contains("does not match 3 keys x 2 morph targets"),
                    "{reason}"
                );
            }
            other => panic!("expected a channel error, got {other:?}"),
        }
    }

    #[test]
    fn weights_on_a_node_without_targets_warn_and_skip() {
        let imported = import_gltf_animation(
            &morph_glb(1, "LINEAR", &[0.0, 0.5, 1.0], None, &[0.0, 0.5, 1.0]),
            &keep(),
        )
        .expect("a node without targets is not an error");
        assert_eq!(imported.warnings.len(), 1, "{:?}", imported.warnings);
        assert!(
            imported.warnings[0].contains("node 1 (node_b)"),
            "{}",
            imported.warnings[0]
        );
        let clip = &imported.node_clips[0];
        assert_eq!(clip.morph_tracks.len(), 1);
        assert_eq!(clip.morph_tracks[0].node.as_ref().map(|n| n.index), Some(0));
    }

    /// Audit asset_pipeline1 #1: from 2^20 s on, adding the 30 Hz step to an `f32` time no longer
    /// changes it, and the cubic-spline bake looped forever. Every cubic path (translation,
    /// rotation, morph weights) now returns a channel error instead; an infinite key too.
    #[test]
    fn a_cubic_key_past_the_f32_30hz_resolution_is_an_error_not_a_hang() {
        let times = [1_048_576.0f32, 1_048_577.0];
        let vec3 = vec![[0.0f32, 1.0, 0.0]; 6];
        let quat = vec![[0.0f32, 0.0, 0.0, 1.0]; 6];
        let weights = vec![0.5f32; 6];
        let err = bake_vec3(&times, &vec3, Interpolation::CubicSpline).expect_err("vec3");
        assert!(err.contains("too large to sample"), "{err}");
        assert!(bake_quat(&times, &quat, Interpolation::CubicSpline).is_err());
        assert!(morph_tracks(&times, &weights, Interpolation::CubicSpline, None, Some(1)).is_err());
        for keys in [[0.0, f32::INFINITY], [f32::NEG_INFINITY, 0.0]] {
            assert!(
                bake_vec3(&keys, &vec3, Interpolation::CubicSpline).is_err(),
                "{keys:?}"
            );
        }
        // Two keys 10 h apart need more samples than one channel may bake.
        let err = bake_vec3(&[0.0, 36_000.0], &vec3, Interpolation::CubicSpline).expect_err("long");
        assert!(err.contains("more than"), "{err}");
    }

    /// The guards of [`a_cubic_key_past_the_f32_30hz_resolution_is_an_error_not_a_hang`] leave
    /// ordinary spans alone: below 2^20 s the walk still advances (by whole ulps), and a
    /// ten-minute clip bakes one sample per 1/30 s plus the last key.
    #[test]
    fn cubic_times_still_walk_ordinary_spans() {
        let near = cubic_times(&[524_288.0, 524_289.0]).expect("2^19 still advances");
        assert!(near.len() > 2 && near.windows(2).all(|w| w[1] > w[0]), "{near:?}");
        let ten_minutes = cubic_times(&[0.0, 600.0]).expect("ten minutes");
        assert!(ten_minutes.len().abs_diff(18_001) <= 2, "{}", ten_minutes.len());
        assert_eq!(ten_minutes.last(), Some(&600.0));
    }

    /// `[inf, inf]` and a NaN key never enter the `while t < b` walk (`NaN < x` and `inf < inf`
    /// are false), so the finite-step guard did not see them and the bake returned `Ok`.
    #[test]
    fn audfix_cubic_keys_that_skip_the_sample_loop_are_still_errors() {
        let vec3 = vec![[0.0f32, 1.0, 0.0]; 6];
        for keys in [[f32::INFINITY, f32::INFINITY], [f32::NAN, 1.0], [0.0, f32::NAN]] {
            assert!(
                bake_vec3(&keys, &vec3, Interpolation::CubicSpline).is_err(),
                "{keys:?}"
            );
        }
    }

    /// Audit asset_pipeline1 #2: a zero scale axis on a joint, on the root joint (every other
    /// joint's ancestor), or on a non-joint node above the root joint decomposed to a NaN rest rotation that cooked as `Ok`.
    /// Each is a skin error now.
    #[test]
    fn a_zero_scale_joint_or_parent_is_a_skin_error_not_a_nan_rest_pose() {
        let (json, bin) = sample_json_and_bin();
        let swap = |from: &str, to: &str| {
            let out = json.replace(from, to);
            assert_ne!(out, json, "fixture text `{from}` not found");
            out
        };
        let zero_bone = swap(
            r#"{"name": "bone", "translation": [0.0, 1.0, 0.0], "children": [2]}"#,
            r#"{"name": "bone", "translation": [0.0, 1.0, 0.0], "scale": [0.0, 1.0, 1.0], "children": [2]}"#,
        );
        // The root joint, which is also the bone's parent joint.
        let zero_root = swap(
            r#"{"name": "root", "translation": [1.0, 0.0, 0.0], "children": [1]}"#,
            r#"{"name": "root", "translation": [1.0, 0.0, 0.0], "scale": [1.0, 0.0, 1.0], "children": [1]}"#,
        );
        let zero_armature = swap(
            r#"{"name": "socket_muzzle", "translation": [0.0, 0.0, -0.2]}"#,
            r#"{"name": "socket_muzzle", "translation": [0.0, 0.0, -0.2]},
                {"name": "armature", "scale": [0.0, 0.0, 0.0], "children": [0]}"#,
        )
        .replace(r#""scenes": [{"nodes": [0]}]"#, r#""scenes": [{"nodes": [3]}]"#);
        for (what, text) in [
            ("joint", zero_bone),
            ("root joint", zero_root),
            ("armature", zero_armature),
        ] {
            for settings in [keep(), GltfAnimSettings::default()] {
                match import_gltf_animation(&glb(&text, &bin), &settings) {
                    Err(GltfAnimError::Skin { reason, .. }) => {
                        assert!(reason.contains("no usable rest pose"), "{what}: {reason}");
                    }
                    Ok(import) => panic!(
                        "{what}: a zero scale imported as {:?}",
                        import.skins[0].skeleton.bind_pose
                    ),
                    Err(other) => panic!("{what}: expected a skin error, got {other}"),
                }
            }
        }
        // The unmodified fixture still imports.
        import_gltf_animation(&glb(&json, &bin), &keep()).expect("sample");
    }
}
