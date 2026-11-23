// SPDX-License-Identifier: MPL-2.0
//! Per-mesh LOD selection for ordinary [`crate::MeshRenderer`]s (ADR-208).
//!
//! The cook stores a screen-size threshold per generated LOD. This system, chained after
//! extraction, projects the mesh bounds through the primary camera and picks a level with the
//! same hysteresis and dither cross-fade terrain scatter uses. Shadow draws may use a coarser
//! level. A mesh with no [`MeshLodChain`], a single level, [`DisableMeshLod`], or a skin palette
//! is submitted exactly as before.
//!
//! The projected size is multiplied by the profile's bias and tier distance scale
//! ([`MeshLodScales`], ADR-260): the colour LOD uses that size, the shadow LOD that size times
//! the tier shadow scale. Switch distances scale linearly with both.
//!
//! **Cluster LOD hand-off** (ADR-1148): while the runtime renderer's last frame drew through the
//! visibility buffer, a chained mesh whose LOD 0 has meshlets
//! ([`crate::FrameRenderer::visbuffer_cluster_lod`]) is not switched: it draws LOD 0, whose
//! cluster hierarchy the visibility buffer cuts per pixel at every distance, instead of its
//! coarser levels on the forward path. Its shadow keeps the chain's shadow LOD unless the
//! clusters cast too ([`crate::visbuffer::VisBufferSettings::cluster_shadows`]).
//! [`ClusterLodHandoff`] counts the meshes handed over.
//!
//! **Shadow LOD hint** (ADR-1622, DO27 request DO-6): a [`ShadowLodHint`] on a chained entity
//! bounds the LOD its shadow draws use: `max_lod` caps how coarse the shadow may get (a hero
//! object whose coarse shadow would pop or self-shadow badly), `min_lod` keeps the shadow at or
//! below a level of detail (a busy prop whose fine shadow costs more than it shows). The colour
//! LOD and the hysteresis memory are unchanged; an entity without the hint selects exactly as
//! before.

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{LodPick, LodSwitch, lod_threshold_scale, projected_screen_size, step_lod};
use regia_core::math::{Mat3, Mat4, Vec3};

use crate::ShadowCast;
use crate::frame::{Frustum, SHADOW_LOD_MESH_TAG};
use crate::runtime::{
    ExtractedFrame, ExtractedMesh, ExtractedTarget, RenderSettings, RuntimeRenderer, SkinJoints, mesh_key,
};

/// ± band around a switch size. The current LOD is kept while the projected size stays inside it.
pub const MESH_LOD_HYSTERESIS: f32 = 0.15;

/// Frames a colour LOD switch spends cross-fading (complementary interleaved-gradient-noise dither).
pub const MESH_LOD_FADE_FRAMES: u64 = 8;

/// Mixed into the outgoing draw's object key so the pair does not share one object uniform.
pub const MESH_LOD_FADE_OBJECT_TAG: u64 = 0x4D0D_FADE_0000_0001;

/// Mixed into the shadow-LOD draw's object key. Distinct from [`MESH_LOD_FADE_OBJECT_TAG`] and
/// from terrain scatter's fade tag.
pub const MESH_LOD_SHADOW_OBJECT_TAG: u64 = 0x5A40_10D0_0000_0001;

/// Default projected-size multiplier from the colour size to the shadow size (a shadow draw
/// steps to a coarser LOD earlier). [`crate::RenderProfile::mesh_lod_shadow_scale`] replaces it
/// per tier; a non-finite profile value falls back to this.
pub const MESH_LOD_SHADOW_SCALE_DEFAULT: f32 = 0.5;

/// Clamp of `mesh_lod_bias × mesh_lod_distance_scale`, the colour-size multiplier (ADR-260).
pub const MESH_LOD_DISTANCE_SCALE_RANGE: (f32, f32) = (0.25, 16.0);

/// Clamp of [`crate::RenderProfile::mesh_lod_shadow_scale`] (ADR-260).
pub const MESH_LOD_SHADOW_SCALE_RANGE: (f32, f32) = (0.05, 1.0);

/// LOD bias is clamped here so a bad preset or component cannot collapse every mesh to one level.
const BIAS_MIN: f32 = 0.25;
const BIAS_MAX: f32 = 4.0;

/// Drop selector memory for entities unseen this long (they come back as a fresh switch).
const MEMORY_FRAMES: u64 = 120;

/// One generated or authored LOD: indices into the [`crate::MeshRenderer`] vertex buffer, and
/// the minimum projected screen size at which this level is chosen. The last level is 0.
#[derive(Clone, Debug)]
pub struct MeshLodLevel {
    /// Triangle list for this level.
    pub indices: Arc<Vec<u32>>,
    /// Minimum projected size (1 = the bounding diameter fills the viewport height).
    pub screen_size: f32,
}

/// LOD chain for one rigid mesh. Hosts attach it next to [`crate::MeshRenderer`].
///
/// Built from a cooked mesh with [`Self::from_cooked`]. The editor's raw glTF loader does not
/// attach one; a mesh without this component draws LOD 0.
#[derive(Component, Clone, Debug)]
pub struct MeshLodChain {
    /// Coarse-to-fine order is finest first (LOD 0 at index 0). At least two levels are selected.
    pub levels: Vec<MeshLodLevel>,
    /// Object-space bounding-sphere centre (metres).
    pub bounds_center: [f32; 3],
    /// Object-space bounding-sphere radius (metres).
    pub bounds_radius: f32,
}

impl MeshLodChain {
    /// Decode every LOD index range. The sphere is the cooked bounds, so selection does not
    /// walk vertices.
    pub fn from_cooked(mesh: &regia_asset::CookedMesh) -> Result<Self, regia_asset::CookedMeshLoadError> {
        mesh.validate()?;
        let mut levels = Vec::with_capacity(mesh.lods.len().max(1));
        if mesh.lods.is_empty() {
            levels.push(MeshLodLevel {
                indices: Arc::new(mesh.lod_indices(0)?),
                screen_size: 0.0,
            });
        } else {
            for (index, lod) in mesh.lods.iter().enumerate() {
                levels.push(MeshLodLevel {
                    indices: Arc::new(mesh.lod_indices(index)?),
                    screen_size: lod.screen_relative_size,
                });
            }
        }
        Ok(Self {
            levels,
            bounds_center: [
                mesh.bounding_sphere[0],
                mesh.bounding_sphere[1],
                mesh.bounding_sphere[2],
            ],
            bounds_radius: mesh.bounding_sphere[3].max(0.0),
        })
    }

    /// The chain of counted mesh `id` (ADR-7428), read from its cooked payload through `reader`
    /// (`AssetServer::payload_reader`): a compact cooked mesh's or a GPU-ready mesh's LOD index
    /// ranges, screen sizes and bounding sphere. The payload is dropped once the chain is built;
    /// no vertex is kept. `None` when the payload holds fewer than two levels or is the legacy CPU
    /// form. Put it next to a [`crate::MeshRenderer::counted`] of the same asset: each level draws
    /// the counted upload's vertices with its own index list, uploaded once per level.
    ///
    /// # Errors
    ///
    /// The payload does not read, or does not decode as a mesh.
    pub fn from_cooked_asset(
        reader: &regia_asset::PayloadReader,
        id: regia_asset::AssetId,
    ) -> Result<Option<Self>, regia_asset::AssetLoadError> {
        let (bytes, _) = reader.read(id)?;
        let (header, payload) = regia_asset::decode_cooked_file(&bytes)?;
        let tag = regia_asset::AssetTypeTag(header.type_tag);
        let chain = if tag == regia_asset::AssetTypeTag::mesh() {
            Self::from_cooked(&regia_asset::CookedMesh::from_postcard(payload)?)?
        } else if tag == regia_asset::AssetTypeTag::gpu_mesh() {
            let view = regia_asset::GpuMeshView::parse(payload)?;
            let mut levels = Vec::with_capacity(view.info.lods.len());
            for (index, lod) in view.info.lods.iter().enumerate() {
                levels.push(MeshLodLevel {
                    indices: Arc::new(view.lod_indices(index)?),
                    screen_size: lod.screen_relative_size,
                });
            }
            let sphere = view.info.bounding_sphere;
            Self {
                levels,
                bounds_center: [sphere[0], sphere[1], sphere[2]],
                bounds_radius: sphere[3].max(0.0),
            }
        } else {
            return Ok(None);
        };
        Ok((chain.levels.len() >= 2).then_some(chain))
    }
}

/// Per-mesh multiplier on the projected size. Above 1 keeps detail longer. Absent = 1.
#[derive(Component, Clone, Copy, Debug)]
pub struct MeshLodBias(pub f32);

/// Leave this entity on the [`crate::MeshRenderer`] index buffer (no selection, no fade).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct DisableMeshLod;

/// Per-object bounds on the LOD index a shadow draw of a [`MeshLodChain`] uses (ADR-1622).
/// Absent = the chain's shadow LOD as selected (the default; nothing changes).
///
/// The selector picks the shadow LOD as usual (projected size times the tier shadow scale, with
/// hysteresis), then clamps it to `min_lod..=max_lod` and to the chain's last level. When
/// `min_lod` exceeds `max_lod`, `min_lod` wins. When the clamped shadow LOD equals the colour LOD
/// and no cross-fade runs, the mesh is drawn once (colour and shadow from one draw), as without
/// a hint. Shadows cast by the visibility buffer's clusters
/// ([`crate::visbuffer::VisBufferSettings::cluster_shadows`]) pick their detail per texel and
/// ignore the hint.
///
/// ```
/// use regia_render::ShadowLodHint;
///
/// let cap = ShadowLodHint::cap(1);
/// assert_eq!(cap.apply(3, 4), 1); // never coarser than LOD 1
/// assert_eq!(cap.apply(0, 4), 0);
/// assert_eq!(ShadowLodHint::fixed(2).apply(0, 4), 2);
/// assert_eq!(ShadowLodHint::fixed(9).apply(0, 3), 2); // clamped to the last level
/// assert_eq!(ShadowLodHint::default().apply(3, 4), 3); // the default changes nothing
/// ```
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShadowLodHint {
    /// Coarsest LOD index shadow draws may use (0 = always LOD 0's detail). [`u8::MAX`]: no cap.
    pub max_lod: u8,
    /// Finest LOD index shadow draws may use (0: no floor).
    pub min_lod: u8,
}

impl Default for ShadowLodHint {
    fn default() -> Self {
        Self {
            max_lod: u8::MAX,
            min_lod: 0,
        }
    }
}

impl ShadowLodHint {
    /// Shadows never coarser than LOD `max_lod`.
    #[must_use]
    pub fn cap(max_lod: u8) -> Self {
        Self {
            max_lod,
            ..Self::default()
        }
    }

    /// Shadows always at LOD `lod` (clamped to the chain).
    #[must_use]
    pub fn fixed(lod: u8) -> Self {
        Self {
            max_lod: lod,
            min_lod: lod,
        }
    }

    /// Whether this hint changes no selection (no cap, no floor).
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        self.max_lod == u8::MAX && self.min_lod == 0
    }

    /// The shadow LOD for a selected `lod` of a chain with `levels` levels: clamped to
    /// `min_lod..=max_lod` (`min_lod` wins a conflict), then to the last level.
    #[must_use]
    pub fn apply(&self, lod: usize, levels: usize) -> usize {
        let min = usize::from(self.min_lod);
        let max = usize::from(self.max_lod).max(min);
        lod.clamp(min, max).min(levels.saturating_sub(1))
    }
}

/// When absent, switches cross-fade. `MeshLodCrossfade(false)` hard-switches (tests, and a host
/// that wants the spec's optional hard switch).
#[derive(Resource, Clone, Copy, Debug)]
pub struct MeshLodCrossfade(pub bool);

impl Default for MeshLodCrossfade {
    fn default() -> Self {
        Self(true)
    }
}

/// Last presented window size, in pixels. Selection scales stored thresholds by `1080 / height`.
///
/// The window pass publishes it while drawing. Extraction runs first, so the height is the
/// previous frame's (1080 until the first present). Tests and headless hosts insert this
/// themselves; the default is 1920×1080.
#[derive(Resource, Clone, Copy, Debug)]
pub struct LodViewport {
    /// Window width in pixels.
    pub width: u32,
    /// Window height in pixels.
    pub height: u32,
}

impl Default for LodViewport {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

/// Mesh-cache key xor. LOD 0 that is not shadow-only keeps the mesh key (xor 0), so a scene
/// without a chain hashes like today. Bits 1–2 are the LOD index. The top byte `0xFF` marks a shadow-only draw.
///
/// ```
/// use regia_render::mesh_lod_cache_key;
///
/// assert_eq!(mesh_lod_cache_key(0, false), 0); // LOD 0 keeps the plain mesh key
/// assert_eq!(mesh_lod_cache_key(1, false), 2); // LOD index in bits 1-2
/// assert_eq!(mesh_lod_cache_key(9, false), mesh_lod_cache_key(3, false)); // clamped to LOD 3
/// assert_ne!(mesh_lod_cache_key(0, true), 0); // shadow-only draws get their own key
/// ```
#[must_use]
pub fn mesh_lod_cache_key(lod: usize, shadow_only: bool) -> u64 {
    if lod == 0 && !shadow_only {
        return 0;
    }
    let mut key = (lod.min(3) as u64) << 1;
    if shadow_only {
        key |= SHADOW_LOD_MESH_TAG;
    }
    key
}

/// The mesh key a renderer sees for `base` drawn as the variant `lod_key`
/// ([`mesh_lod_cache_key`]): their xor, with the top byte forced to `0xFF` for a shadow-only
/// draw. The xor alone keeps the tag only for a key whose top byte is clear (an `Arc` address);
/// a [`crate::mesh_key`] carries `'M'` there, and `'M' ^ 0xFF` is not the tag, so its shadow
/// LOD would be drawn in the colour pass too (ADR-1384).
#[must_use]
pub(crate) fn lod_variant_key(base: u64, lod_key: u64) -> u64 {
    let key = base ^ lod_key;
    if crate::frame::is_shadow_lod_key(lod_key) {
        key | SHADOW_LOD_MESH_TAG
    } else {
        key
    }
}

/// What the last [`select_mesh_lods`] run did with the visibility buffer's cluster LOD
/// (ADR-1148, `render.get_visbuffer` `lod_handoff`).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClusterLodHandoff {
    /// The runtime renderer's last main-view frame drew through the visibility buffer
    /// ([`crate::FrameRenderer::visbuffer_active`]), so meshes with meshlets could be handed over.
    pub active: bool,
    /// Entities with a [`MeshLodChain`] and keyed draws with a [`crate::KeyedDraw::lod_chain`]
    /// the selector placed: every extracted rigid chained mesh, in view or not ([`DisableMeshLod`],
    /// single-level and skinned ones excluded, and none without a camera).
    pub chained: u32,
    /// Of those, the meshes left at LOD 0 because their clusters pick the detail (opaque, not
    /// transmissive, no material graph, no morph targets).
    pub cluster_lod: u32,
}

/// Insert to make the next LOD selection forget every mesh's level, hysteresis and fade, as on the
/// first frame of a fresh app: a render-out job starts this way, so its first frame does not depend
/// on the levels the frames before it left (ADR-1881). The selection removes it.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct MeshLodRestart;

/// Per-entity hysteresis the selector stores in a [`Local`]. Hosts do not read it.
///
/// It is public because [`select_mesh_lods`] names it in its signature. A fresh value is the
/// first time an entity is seen.
#[derive(Clone, Copy, Debug, Default)]
pub struct MeshLodMemory {
    color: LodSwitch,
    shadow: LodSwitch,
    seen: u64,
}

thread_local! {
    static LEVEL_REBUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_level_rebuild() {
    LEVEL_REBUILDS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
fn level_rebuilds() -> u64 {
    LEVEL_REBUILDS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_level_rebuilds() {
    LEVEL_REBUILDS.with(|count| count.set(0));
}

struct Snap {
    levels: Vec<(Arc<Vec<u32>>, f32)>,
    center: Vec3,
    radius: f32,
    /// Colour-size multiplier: the clamped bias times the tier distance scale, clamped again.
    bias: f32,
    skinned: bool,
    /// Has morph targets: the visibility buffer never draws it, so it keeps its chain.
    morphing: bool,
    /// The entity's [`ShadowLodHint`], when present.
    shadow_hint: Option<ShadowLodHint>,
    /// Frame that last refreshed this chain. A chain missing that frame is dropped.
    seen: u64,
}

/// Recycled chain snapshots for [`select_mesh_lods`].
///
/// One level list per chained entity and per chained keyed draw of the [`crate::DrawList`]. A
/// frame rebuilds a list only when a level's index allocation (`Arc` pointer) or its screen-size
/// bits change. The centre, radius, bias and the skin, morph and shadow flags are copied every
/// frame. Hosts do not read this value; it is public because the system names it.
///
/// Keyed draws keep their snapshots and their hysteresis memory in maps keyed by
/// [`crate::DrawKey`], apart from the entity maps (ADR-8457): a draw key whose number equals an
/// entity's bits shares nothing with that entity.
#[derive(Default)]
pub struct MeshLodSnaps {
    snaps: HashMap<Entity, Snap>,
    /// Chained keyed draws, by key.
    draws: HashMap<crate::DrawKey, Snap>,
    /// Hysteresis and fade of chained keyed draws, by key.
    draw_memory: HashMap<crate::DrawKey, MeshLodMemory>,
    /// This frame's chained keyed draws by object key (how an extracted mesh names its draw).
    draw_objects: HashMap<u64, crate::DrawKey>,
    /// This frame's per-draw LOD counts, handed to the list at the end of the run.
    draw_stats: crate::DrawLodStats,
}

/// The per-frame inputs of one chained mesh's snapshot besides its chain.
struct SnapInputs {
    /// Colour-size multiplier ([`MeshLodScales::color_factor`] of the mesh's bias).
    bias: f32,
    skinned: bool,
    morphing: bool,
    shadow_hint: Option<ShadowLodHint>,
}

/// Refreshes (or inserts) the snapshot of one chained mesh under `key` for `frame`.
fn refresh_snap<K: std::hash::Hash + Eq>(
    snaps: &mut HashMap<K, Snap>,
    key: K,
    chain: &MeshLodChain,
    inputs: SnapInputs,
    frame: u64,
) {
    let SnapInputs {
        bias,
        skinned,
        morphing,
        shadow_hint: hint,
    } = inputs;
    let center = Vec3::from_array(chain.bounds_center);
    let radius = chain.bounds_radius.max(0.0);
    if let Some(snap) = snaps.get_mut(&key) {
        let levels_same = snap.levels.len() == chain.levels.len()
            && snap.levels.iter().zip(chain.levels.iter()).all(|(kept, level)| {
                Arc::ptr_eq(&kept.0, &level.indices) && kept.1.to_bits() == level.screen_size.to_bits()
            });
        snap.center = center;
        snap.radius = radius;
        snap.bias = bias;
        snap.skinned = skinned;
        snap.morphing = morphing;
        snap.shadow_hint = hint;
        snap.seen = frame;
        if !levels_same {
            note_level_rebuild();
            snap.levels = chain_levels(chain);
        }
    } else {
        note_level_rebuild();
        snaps.insert(
            key,
            Snap {
                levels: chain_levels(chain),
                center,
                radius,
                bias,
                skinned,
                morphing,
                shadow_hint: hint,
                seen: frame,
            },
        );
    }
}

/// The tier's LOD multipliers, read once per frame from the profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshLodScales {
    /// Global [`crate::RenderProfile::mesh_lod_bias`], clamped to 0.25..=4.
    pub bias: f32,
    /// [`crate::RenderProfile::mesh_lod_distance_scale`]; non-finite or non-positive reads as 1.
    pub distance: f32,
    /// [`crate::RenderProfile::mesh_lod_shadow_scale`], clamped to
    /// [`MESH_LOD_SHADOW_SCALE_RANGE`]; non-finite reads as [`MESH_LOD_SHADOW_SCALE_DEFAULT`].
    pub shadow: f32,
}

impl Default for MeshLodScales {
    fn default() -> Self {
        Self {
            bias: 1.0,
            distance: 1.0,
            shadow: MESH_LOD_SHADOW_SCALE_DEFAULT,
        }
    }
}

impl MeshLodScales {
    /// The multipliers `profile` asks for, sanitised.
    #[must_use]
    pub fn of_profile(profile: &crate::RenderProfile) -> Self {
        Self {
            bias: clamp_bias(profile.mesh_lod_bias),
            distance: if profile.mesh_lod_distance_scale.is_finite() && profile.mesh_lod_distance_scale > 0.0
            {
                profile.mesh_lod_distance_scale
            } else {
                1.0
            },
            shadow: if profile.mesh_lod_shadow_scale.is_finite() {
                profile
                    .mesh_lod_shadow_scale
                    .clamp(MESH_LOD_SHADOW_SCALE_RANGE.0, MESH_LOD_SHADOW_SCALE_RANGE.1)
            } else {
                MESH_LOD_SHADOW_SCALE_DEFAULT
            },
        }
    }

    /// Colour-size multiplier for a mesh with per-entity bias `mesh_bias`:
    /// `clamp(clamp(bias × mesh_bias, 0.25, 4) × distance, 0.25, 16)`. With `distance` 1 it is
    /// the clamped bias, bit for bit.
    #[must_use]
    pub fn color_factor(&self, mesh_bias: f32) -> f32 {
        let bias = clamp_bias(self.bias * clamp_bias(mesh_bias));
        let product = bias * self.distance;
        if product.is_finite() {
            product.clamp(MESH_LOD_DISTANCE_SCALE_RANGE.0, MESH_LOD_DISTANCE_SCALE_RANGE.1)
        } else {
            bias
        }
    }

    /// Distance (metres) at which a sphere of `radius` crosses a stored `threshold` for the colour
    /// LOD (`shadow` false) or the shadow LOD, at viewport `height` and vertical FOV `fov_y`.
    /// Distances scale linearly with the colour factor (and the shadow scale). `None` for a
    /// threshold of 0 or less (the last level never switches out).
    #[must_use]
    pub fn switch_distance(
        &self,
        radius: f32,
        threshold: f32,
        height: u32,
        fov_y: f32,
        mesh_bias: f32,
        shadow: bool,
    ) -> Option<f32> {
        let stored = threshold * lod_threshold_scale(height);
        if stored.is_nan() || stored <= 0.0 || !radius.is_finite() || radius <= 0.0 {
            return None;
        }
        let factor = self.color_factor(mesh_bias) * if shadow { self.shadow } else { 1.0 };
        let tan_half = (fov_y * 0.5).tan().max(1.0e-4);
        let distance = radius * factor / (stored * tan_half);
        distance.is_finite().then_some(distance)
    }
}

/// Largest singular value of the 3×3 linear part of `m`: the most any unit
/// direction stretches, i.e. the radius multiplier that keeps the transformed
/// bounds sphere enclosing. The longest matrix *column* only lower-bounds it;
/// a sheared chain (a parent's non-uniform scale times a child's rotation —
/// `GlobalTransform` is parent × child) underestimates the sphere by that gap
/// and switches LOD early, or drops a grazing mesh off screen.
///
/// Computed as the largest eigenvalue of the symmetric `MᵀM` in closed form
/// (Golub & Van Loan, *Matrix Computations*, 4th ed., §8.4.2: the cubic's
/// trigonometric solution), in `f64` for a stable root.
fn linear_operator_norm(m: &Mat4) -> f32 {
    let g = Mat3::from_mat4(*m);
    let a = g.transpose() * g;
    let (m00, m11, m22) = (
        f64::from(a.x_axis.x),
        f64::from(a.y_axis.y),
        f64::from(a.z_axis.z),
    );
    let (m01, m02, m12) = (
        f64::from(a.x_axis.y),
        f64::from(a.x_axis.z),
        f64::from(a.y_axis.z),
    );
    let p1 = m00 + m11 + m22;
    let p2 = m00 * m11 + m00 * m22 + m11 * m22 - m01 * m01 - m02 * m02 - m12 * m12;
    let p3 = m00 * (m11 * m22 - m12 * m12) - m01 * (m01 * m22 - m12 * m02) + m02 * (m01 * m12 - m11 * m02);
    // Depress λ³ − p1 λ² + p2 λ − p3 = 0 with λ = x + p1/3: x³ + q x + r = 0.
    // MᵀM is positive semidefinite, so all three roots are real (q ≤ 0).
    let q = p2 - p1 * p1 / 3.0;
    let r = 2.0 * p1 * p1 * p1 / 27.0 - p1 * p2 / 3.0 + p3;
    let x = if q > -1.0e-30 {
        // Triple (or nearly) root: q = 0 gives x³ = −r.
        -r.cbrt()
    } else {
        let arg = (-(3.0 * r / (2.0 * q)) * (-3.0_f64 / q).sqrt()).clamp(-1.0, 1.0);
        2.0 * (-q / 3.0).sqrt() * (arg.acos() / 3.0).cos()
    };
    let largest = (p1 / 3.0 + x).max(0.0);
    (largest.sqrt()) as f32
}

/// `Render`, after [`crate::extract_frame`]: rewrite chained rigid meshes into one or more draws.
/// Meshes whose clusters pick their detail on the visibility buffer stay at LOD 0 (see the
/// module docs); [`ClusterLodHandoff`], when present, records how many.
///
/// Keyed draws of the [`crate::DrawList`] with a [`crate::KeyedDraw::lod_chain`] are selected by
/// the same rule, hysteresis and cross-fade as a chained entity, their memory keyed by
/// [`crate::DrawKey`] (ADR-8457); the list's [`crate::DrawList::lod_stats`] records the result.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn select_mesh_lods(
    mut extracted: ResMut<ExtractedFrame>,
    chains: Query<(
        Entity,
        &MeshLodChain,
        Option<&MeshLodBias>,
        Option<&DisableMeshLod>,
        Option<&SkinJoints>,
        Has<crate::MeshMorphTargets>,
        Option<&ShadowLodHint>,
    )>,
    settings: Option<Res<RenderSettings>>,
    viewport: Option<Res<LodViewport>>,
    crossfade: Option<Res<MeshLodCrossfade>>,
    runtime: Option<Res<RuntimeRenderer>>,
    mut handoff: Option<ResMut<ClusterLodHandoff>>,
    restart: Option<Res<MeshLodRestart>>,
    mut commands: Commands,
    mut memory: Local<HashMap<Entity, MeshLodMemory>>,
    mut frame_counter: Local<u64>,
    mut spare: Local<Vec<ExtractedMesh>>,
    mut lod_snaps: Local<MeshLodSnaps>,
    draw_list: Option<Res<crate::DrawList>>,
) {
    let _span = tracing::info_span!("mesh_lod_select").entered();
    // Meshes with clusters are handed to the visibility buffer only while it took the last
    // frame and the profile still has it on (the first frame after switching it off keeps the
    // chains).
    let path_on = settings.as_deref().is_some_and(|s| s.profile.visibility_buffer);
    let clusters = runtime
        .as_deref()
        .map(|r| &r.0)
        .filter(|r| path_on && r.visbuffer_active());
    let mut counts = ClusterLodHandoff {
        active: clusters.is_some(),
        ..ClusterLodHandoff::default()
    };
    let MeshLodSnaps {
        snaps,
        draws,
        draw_memory,
        draw_objects,
        draw_stats,
    } = &mut *lod_snaps;
    if restart.is_some() {
        memory.clear();
        draw_memory.clear();
        commands.remove_resource::<MeshLodRestart>();
    }
    *frame_counter = frame_counter.saturating_add(1);
    let frame = *frame_counter;
    let scales = settings
        .as_deref()
        .map(|s| MeshLodScales::of_profile(&s.profile))
        .unwrap_or_default();
    for (entity, chain, bias, disabled, skin, morphing, shadow_hint) in &chains {
        if disabled.is_some() || chain.levels.len() < 2 {
            continue;
        }
        let mesh_bias = bias.map(|b| b.0).unwrap_or(1.0);
        let skinned = skin.is_some_and(|joints| !joints.palette.is_empty());
        let inputs = SnapInputs {
            bias: scales.color_factor(mesh_bias),
            skinned,
            morphing,
            shadow_hint: shadow_hint.copied(),
        };
        refresh_snap(snaps, entity, chain, inputs, frame);
    }
    // Chained keyed draws, by key, apart from the entities (ADR-8457).
    draw_objects.clear();
    if let Some(list) = draw_list.as_deref() {
        for (key, item) in list.iter() {
            let Some(chain) = item.lod_chain.as_deref() else {
                continue;
            };
            if chain.levels.len() < 2 {
                continue;
            }
            let inputs = SnapInputs {
                bias: scales.color_factor(item.lod_bias),
                skinned: !item.joints.is_empty(),
                morphing: false,
                shadow_hint: item.shadow_lod_hint,
            };
            refresh_snap(draws, key, chain, inputs, frame);
            draw_objects.insert(key.object_key(), key);
        }
    }
    snaps.retain(|_, snap| snap.seen == frame);
    draws.retain(|_, snap| snap.seen == frame);
    draw_stats.clear();
    if snaps.is_empty() {
        memory.clear();
    }
    if draws.is_empty() {
        draw_memory.clear();
    }
    if snaps.is_empty() && draws.is_empty() {
        if let Some(list) = draw_list.as_deref() {
            list.record_lod(draw_stats);
        }
        publish(&mut handoff, counts);
        return;
    }
    let (width, height) = viewport.map(|vp| (vp.width, vp.height)).unwrap_or((1920, 1080));
    let threshold_scale = lod_threshold_scale(height);
    let fade = crossfade.map(|c| c.0).unwrap_or(true);
    let view = primary_view(&extracted);
    let cluster_shadows = clusters.is_some_and(|r| r.visbuffer_settings().cluster_shadows);
    // One mesh-key lookup per distinct mesh per frame (scatter shares a few meshes).
    let mut cluster_lod: HashMap<usize, bool> = HashMap::new();
    // Built once per frame, not once per mesh (ADR-385).
    let frustum = view.as_ref().map(|view| view_frustum(view, width, height));
    // The list is rebuilt into last frame's (emptied) buffer, and this frame's goes spare
    // (ADR-385): a steady frame allocates neither.
    let mut next = std::mem::take(&mut *spare);
    next.clear();
    next.reserve(extracted.meshes.len());
    let mut thresholds: Vec<f32> = Vec::new();
    for mesh in extracted.meshes.drain(..) {
        // An entity's chain by entity; a keyed draw's by the key its object key names.
        let draw_key = if mesh.entity == Entity::PLACEHOLDER && !draws.is_empty() {
            draw_objects
                .get(&(mesh.entity.to_bits() ^ mesh.object_key_xor))
                .copied()
        } else {
            None
        };
        let snap = match draw_key {
            Some(key) => draws.get(&key),
            None => snaps.get(&mesh.entity),
        };
        let Some(snap) = snap else {
            next.push(mesh);
            continue;
        };
        if snap.skinned || !mesh.joints.is_empty() {
            next.push(mesh);
            continue;
        }
        let (Some(view), Some(frustum)) = (view.as_ref(), frustum.as_ref()) else {
            next.push(mesh);
            continue;
        };
        let model = Mat4::from_cols_array_2d(&mesh.model);
        let world_center = model.transform_point3(snap.center);
        let world_radius = snap.radius * linear_operator_norm(&model);
        let eye = Vec3::from_array(view.eye);
        let distance = (world_center - eye).length();
        let size = projected_screen_size(world_radius, distance, view.fov_y) * snap.bias;
        let on_screen = frustum.intersects_sphere(world_center.to_array(), world_radius);
        thresholds.clear();
        thresholds.extend(snap.levels.iter().map(|(_, screen)| screen * threshold_scale));
        let mem = match draw_key {
            Some(key) => draw_memory.entry(key).or_default(),
            None => memory.entry(mesh.entity).or_default(),
        };
        mem.seen = frame;
        counts.chained = counts.chained.saturating_add(1);
        // Meshlets are per mesh, eligibility per entity: a translucent, transmissive,
        // material-graph or morphing user of a mesh with meshlets draws forward, so it keeps
        // its chain (the renderer's own per-draw gate, as far as the selector can see it).
        let eligible = !snap.morphing
            && crate::hair::effective_alpha_mode(&mesh.material) == crate::ALPHA_OPAQUE
            && mesh.material.transmission <= 0.0
            && mesh.material.surface_graph.is_none();
        let handed = eligible
            && clusters.is_some_and(|renderer| {
                *cluster_lod
                    .entry(Arc::as_ptr(&mesh.mesh) as usize)
                    .or_insert_with(|| renderer.visbuffer_cluster_lod(mesh_key(&mesh.mesh)))
            });
        if handed {
            counts.cluster_lod = counts.cluster_lod.saturating_add(1);
            // A later return to the chain (the path turned off, the meshlets removed) picks its
            // level at once, without a fade from a level last drawn frames ago.
            mem.color = LodSwitch::default();
            let shadow_lod = if cluster_shadows {
                // The clusters cast with their own per-texel cut (one draw, LOD 0's key).
                mem.shadow = LodSwitch::default();
                0
            } else {
                let lod = step_lod(
                    &thresholds,
                    size * scales.shadow,
                    on_screen,
                    false,
                    MESH_LOD_HYSTERESIS,
                    MESH_LOD_FADE_FRAMES,
                    frame,
                    &mut mem.shadow,
                )
                .lod;
                hinted_shadow_lod(snap, lod)
            };
            if draw_key.is_some() {
                draw_stats.note(0, None, (mesh.shadow != ShadowCast::Off).then_some(shadow_lod));
            }
            push_draws(&mut next, mesh, snap, LodPick { lod: 0, fade: None }, shadow_lod);
            continue;
        }
        let color = step_lod(
            &thresholds,
            size,
            on_screen,
            fade,
            MESH_LOD_HYSTERESIS,
            MESH_LOD_FADE_FRAMES,
            frame,
            &mut mem.color,
        );
        let shadow = step_lod(
            &thresholds,
            size * scales.shadow,
            on_screen,
            false,
            MESH_LOD_HYSTERESIS,
            MESH_LOD_FADE_FRAMES,
            frame,
            &mut mem.shadow,
        );
        let shadow_lod = hinted_shadow_lod(snap, shadow.lod);
        if draw_key.is_some() {
            draw_stats.note(
                color.lod,
                color.fade.map(|(prev, _)| prev),
                (mesh.shadow != ShadowCast::Off).then_some(shadow_lod),
            );
        }
        push_draws(&mut next, mesh, snap, color, shadow_lod);
    }
    *spare = std::mem::replace(&mut extracted.meshes, next);
    memory.retain(|_, mem| frame.saturating_sub(mem.seen) < MEMORY_FRAMES);
    draw_memory.retain(|_, mem| frame.saturating_sub(mem.seen) < MEMORY_FRAMES);
    if let Some(list) = draw_list.as_deref() {
        list.record_lod(draw_stats);
    }
    publish(&mut handoff, counts);
}

fn chain_levels(chain: &MeshLodChain) -> Vec<(Arc<Vec<u32>>, f32)> {
    chain
        .levels
        .iter()
        .map(|level| (Arc::clone(&level.indices), level.screen_size))
        .collect()
}

/// The shadow LOD after the entity's [`ShadowLodHint`] (the selected one without a hint). The
/// hysteresis memory keeps the unclamped pick, so removing the hint resumes without a jump.
fn hinted_shadow_lod(snap: &Snap, lod: usize) -> usize {
    match snap.shadow_hint {
        Some(hint) => hint.apply(lod, snap.levels.len()),
        None => lod,
    }
}

/// Writes this run's counters, marking the resource changed only when they differ.
fn publish(handoff: &mut Option<ResMut<ClusterLodHandoff>>, counts: ClusterLodHandoff) {
    if let Some(handoff) = handoff.as_mut()
        && **handoff != counts
    {
        **handoff = counts;
    }
}

fn clamp_bias(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(BIAS_MIN, BIAS_MAX)
    } else {
        1.0
    }
}

/// Camera fields copied out of the extracted frame so selection can drain the mesh list.
struct ViewSnap {
    eye: [f32; 3],
    target: [f32; 3],
    fov_y: f32,
    near: f32,
    far: f32,
}

fn primary_view(frame: &ExtractedFrame) -> Option<ViewSnap> {
    let view = frame
        .views
        .iter()
        .filter(|view| matches!(view.target, ExtractedTarget::Window))
        .min_by_key(|view| view.order)
        .or(frame.views.first())?;
    Some(ViewSnap {
        eye: view.camera.eye,
        target: view.camera.target,
        fov_y: view.camera.fov_y,
        near: view.camera.near,
        far: view.camera.far,
    })
}

/// The primary view's frustum at `width`×`height`, for the on-screen test.
fn view_frustum(view: &ViewSnap, width: u32, height: u32) -> Frustum {
    let aspect = width.max(1) as f32 / height.max(1) as f32;
    let eye = Vec3::from_array(view.eye);
    let target = Vec3::from_array(view.target);
    let view_m = crate::view_math::look_at_view(eye, target, Vec3::Y);
    // The renderer's frustum planes are the DirectX 0..1 clip range (Gribb–Hartmann).
    // `Mat4::perspective_rh` is OpenGL −1..1 and would mark an on-screen mesh as outside.
    // The frame's own repair of a degenerate camera (ADR-869), so LOD selection sees the frustum
    // the frame draws. Below 16:9 a horizontal-plus profile widens the drawn FOV; the test takes
    // that wider frustum whatever the profile (only conservative: an edge mesh cross-fades
    // instead of switching at once), so a mesh at the top or bottom of a 4:3 picture is not
    // treated as off screen.
    let fov_y = crate::output::horizontal_plus_fov_y(view.fov_y, aspect);
    let (fov, near, far) = crate::view_math::usable_projection(fov_y, view.near, view.far);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov, aspect.max(1.0e-3), near, far);
    Frustum::from_view_proj(&(proj * view_m))
}

fn push_draws(
    out: &mut Vec<ExtractedMesh>,
    mesh: ExtractedMesh,
    snap: &Snap,
    color: LodPick,
    shadow_lod: usize,
) {
    let casts = mesh.shadow != ShadowCast::Off;
    let separate_shadow = casts && (color.fade.is_some() || shadow_lod != color.lod);
    let color_shadow = if separate_shadow {
        ShadowCast::Off
    } else {
        mesh.shadow
    };
    let incoming = color.fade.map(|(_, coverage)| coverage).unwrap_or(0.0);
    out.push(draw_of(&mesh, snap, color.lod, incoming, color_shadow, 0, false));
    if let Some((prev, coverage)) = color.fade {
        out.push(draw_of(
            &mesh,
            snap,
            prev,
            -coverage,
            ShadowCast::Off,
            MESH_LOD_FADE_OBJECT_TAG,
            false,
        ));
    }
    if separate_shadow {
        out.push(draw_of(
            &mesh,
            snap,
            shadow_lod,
            0.0,
            mesh.shadow,
            MESH_LOD_SHADOW_OBJECT_TAG,
            true,
        ));
    }
}

fn draw_of(
    mesh: &ExtractedMesh,
    snap: &Snap,
    lod: usize,
    dither: f32,
    shadow: ShadowCast,
    object_xor: u64,
    shadow_only: bool,
) -> ExtractedMesh {
    let mut draw = mesh.clone();
    if let Some((indices, _)) = snap.levels.get(lod).or_else(|| snap.levels.last()) {
        draw.draw_indices = Some(Arc::clone(indices));
    }
    draw.lod_dither = dither;
    draw.lod_key = mesh_lod_cache_key(lod, shadow_only);
    // Keeps the mesh's own object key (an entity's bits, a keyed draw's key) and tags the
    // outgoing and shadow-only draws apart from it.
    draw.object_key_xor = mesh.object_key_xor ^ object_xor;
    draw.shadow = shadow;
    draw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::is_shadow_lod_key;
    use regia_core::math::Vec3;
    use regia_scene::{GlobalTransform, Transform};

    use crate::Camera;
    use crate::ShadowCast;
    use crate::lab_frame::LabVertex;
    use crate::lights::DirectionalLight;
    use crate::profile::RenderProfilePreset;
    use crate::runtime::{CpuMaterial, CpuMesh, MeshRenderer, extract_frame};

    fn place(pos: Vec3) -> (Transform, GlobalTransform) {
        let transform = Transform::from_translation(pos);
        (transform, GlobalTransform(transform.compute_affine()))
    }

    fn chain(thresholds: &[(u32, f32)]) -> MeshLodChain {
        MeshLodChain {
            levels: thresholds
                .iter()
                .map(|(tris, screen)| MeshLodLevel {
                    indices: Arc::new(vec![0; (*tris * 3) as usize]),
                    screen_size: *screen,
                })
                .collect(),
            bounds_center: [0.0; 3],
            bounds_radius: 1.0,
        }
    }

    fn single_thread(app: &mut App) {
        // Installed at the start of `update`, before Render, so the thread-local rebuild
        // counter is written on this thread. `App` has no `edit_schedule`.
        app.set_schedule_executor(regia_app::ScheduleExecutor::SingleThreaded);
    }

    fn index_ptrs(app: &App) -> Vec<usize> {
        app.world()
            .resource::<ExtractedFrame>()
            .meshes
            .iter()
            .filter_map(|mesh| {
                mesh.draw_indices
                    .as_ref()
                    .map(|indices| Arc::as_ptr(indices) as usize)
            })
            .collect()
    }

    #[test]
    fn a_steady_frame_does_not_rebuild_lod_level_vecs() {
        let levels = [(3, 0.4), (2, 0.1), (1, 0.0)];
        let (mut app, _, camera, _) = app_with(Some(chain(&levels)), false);
        single_thread(&mut app);
        set_distance(&mut app, camera, distance_for(0.5));
        reset_level_rebuilds();
        app.update();
        let first = index_ptrs(&app);
        assert!(!first.is_empty(), "the chain draws index buffers");
        assert_eq!(level_rebuilds(), 1, "the first frame builds the level vec");
        app.update();
        assert_eq!(
            level_rebuilds(),
            1,
            "a second frame with the same chain rebuilt its level vec"
        );
        assert_eq!(index_ptrs(&app), first, "the drawn index allocations changed");
    }

    fn app_with(lod: Option<MeshLodChain>, skinned: bool) -> (App, Entity, Entity, u64) {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        app.insert_resource(LodViewport {
            width: 1920,
            height: 1080,
        });
        app.add_systems(Render, (extract_frame, select_mesh_lods).chain());
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0, 0, 0, 0],
        });
        let key = crate::runtime::mesh_key(&mesh);
        let mut entity = app.world_mut().spawn((
            place(Vec3::ZERO),
            MeshRenderer::new(mesh, Arc::new(CpuMaterial::default())),
        ));
        if let Some(lod) = lod {
            entity.insert(lod);
        }
        if skinned {
            entity.insert(SkinJoints {
                palette: vec![crate::skin::JointMatrix::identity()],
            });
        }
        let mesh_entity = entity.id();
        let cam = app
            .world_mut()
            .spawn((
                place(Vec3::new(0.0, 0.0, 4.0)),
                Camera {
                    fov_y: 0.9,
                    exposure_ev: Some(14.0),
                    near: 0.1,
                    far: 500.0,
                    ..Camera::default()
                },
            ))
            .id();
        app.world_mut().spawn((
            place(Vec3::ZERO),
            DirectionalLight {
                direction: [0.0, -1.0, 0.0],
                color: [1.0; 3],
                illuminance: 80_000.0,
                cast_shadows: true,
                static_shadow: false,
            },
        ));
        (app, mesh_entity, cam, key)
    }

    fn set_distance(app: &mut App, camera: Entity, distance: f32) {
        let (transform, global) = place(Vec3::new(0.0, 0.0, distance));
        app.world_mut().entity_mut(camera).insert((transform, global));
    }

    #[derive(Debug)]
    struct Drawn {
        mesh_key: u64,
        lod_dither: f32,
        shadow: ShadowCast,
        indices: usize,
    }

    fn drawn(app: &App) -> Vec<Drawn> {
        let keys: Vec<(u64, f32, ShadowCast)> = {
            let extracted = app.world().resource::<ExtractedFrame>();
            let scene = extracted.frame_scene(0, 1920, 1080);
            scene
                .meshes
                .iter()
                .map(|mesh| (mesh.mesh_key, mesh.lod_dither, mesh.shadow))
                .collect()
        };
        let lens: Vec<usize> = app
            .world()
            .resource::<ExtractedFrame>()
            .meshes
            .iter()
            .map(|mesh| {
                mesh.draw_indices
                    .as_ref()
                    .map_or(mesh.mesh.indices.len(), |indices| indices.len())
            })
            .collect();
        keys.into_iter()
            .zip(lens)
            .map(|((mesh_key, lod_dither, shadow), indices)| Drawn {
                mesh_key,
                lod_dither,
                shadow,
                indices,
            })
            .collect()
    }

    fn distance_for(size: f32) -> f32 {
        // projected_screen_size(1, d, 0.9) = size → d = 1 / (size * tan(0.45)), solved by search.
        let mut lo = 0.2f32;
        let mut hi = 400.0f32;
        for _ in 0..40 {
            let mid = 0.5 * (lo + hi);
            if projected_screen_size(1.0, mid, 0.9) > size {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }

    #[test]
    fn a_mesh_without_a_chain_keeps_todays_draw() {
        let (mut app, _, _, key) = app_with(None, false);
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mesh_key, key);
        assert_eq!(rows[0].lod_dither, 0.0);
        assert_eq!(rows[0].shadow, ShadowCast::All);
        assert_eq!(rows[0].indices, 6);
    }

    #[test]
    fn distance_selects_the_coarse_lod_and_a_near_camera_keeps_lod0() {
        let (mut app, _, camera, key) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        set_distance(&mut app, camera, distance_for(1.0));
        app.update();
        let near = drawn(&app);
        assert_eq!(near.len(), 1, "near: colour and shadow agree on LOD 0");
        assert_eq!(near[0].indices, 6);
        assert_eq!(near[0].mesh_key, key);
        assert_eq!(near[0].lod_dither, 0.0);
        set_distance(&mut app, camera, distance_for(0.2));
        app.update();
        let far = drawn(&app);
        let incoming = far.iter().find(|row| row.lod_dither > 0.0).expect("fade in");
        assert_eq!(incoming.indices, 3, "far camera draws the 1-triangle LOD");
    }

    #[test]
    fn hysteresis_does_not_flicker_and_the_cross_fade_covers_every_pixel() {
        let (mut app, _, camera, _) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        set_distance(&mut app, camera, distance_for(1.0));
        app.update();
        set_distance(&mut app, camera, distance_for(0.46));
        app.update();
        let held = drawn(&app);
        let color = held
            .iter()
            .find(|row| !is_shadow_lod_key(row.mesh_key))
            .expect("colour");
        assert_eq!(color.indices, 6, "inside ±15 % the colour LOD stays");
        assert_eq!(color.lod_dither, 0.0, "holding is not a fade");
        set_distance(&mut app, camera, distance_for(0.2));
        app.update();
        let fading: Vec<f32> = drawn(&app)
            .iter()
            .map(|row| row.lod_dither)
            .filter(|d| *d != 0.0)
            .collect();
        assert_eq!(fading.len(), 2, "a switch cross-fades as a complementary pair");
        let sum: f32 = fading.iter().sum();
        assert!(
            sum.abs() < 1.0e-5,
            "complementary coverage sums to 0, got {fading:?}"
        );
    }

    #[test]
    fn a_restart_picks_as_on_the_first_frame_without_a_fade() {
        let (mut app, _, camera, _) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        set_scales(&mut app, 1.0, 1.0, MESH_LOD_SHADOW_SCALE_DEFAULT);
        set_distance(&mut app, camera, distance_for(1.0));
        app.update();
        set_distance(&mut app, camera, distance_for(0.46));
        app.update();
        let colour = |app: &App| {
            drawn(app)
                .iter()
                .find(|row| !is_shadow_lod_key(row.mesh_key))
                .map(|row| (row.indices, row.lod_dither))
        };
        assert_eq!(colour(&app), Some((6, 0.0)), "hysteresis holds the fine LOD here");
        app.insert_resource(MeshLodRestart);
        app.update();
        assert_eq!(colour(&app), Some((3, 0.0)), "the size alone picks, at once");
        assert!(
            !app.world().contains_resource::<MeshLodRestart>(),
            "one frame only"
        );
    }

    #[test]
    fn eight_fade_frames_then_one_draw() {
        let (mut app, _, camera, _) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        set_distance(&mut app, camera, distance_for(1.0));
        app.update();
        set_distance(&mut app, camera, distance_for(0.2));
        for _ in 0..8 {
            app.update();
            assert_eq!(
                app.world().resource::<ExtractedFrame>().meshes.len(),
                3,
                "incoming, outgoing, and the shadow LOD"
            );
        }
        app.update();
        let settled = drawn(&app);
        assert_eq!(settled.len(), 1, "both selectors agree on the coarse LOD");
        assert_eq!(settled[0].lod_dither, 0.0);
        assert_eq!(settled[0].indices, 3);
        assert_eq!(settled[0].shadow, ShadowCast::All);
    }

    /// scale(4, 1, 1) × rotation_y(45°): the longest column is √8.5 ≈ 2.915
    /// while the operator norm (largest singular value) is 4. A radius-1
    /// sphere must size as radius 4.
    fn shear() -> Mat4 {
        Mat4::from_scale(Vec3::new(4.0, 1.0, 1.0)) * Mat4::from_rotation_y(std::f32::consts::FRAC_PI_4)
    }

    #[test]
    fn the_operator_norm_bounds_the_sheared_radius() {
        assert!((linear_operator_norm(&Mat4::IDENTITY) - 1.0).abs() < 1.0e-6);
        assert!((linear_operator_norm(&Mat4::from_scale(Vec3::splat(3.0))) - 3.0).abs() < 1.0e-6);
        // Plain TRS: the longest column is exact.
        let trs = Mat4::from_rotation_z(0.4) * Mat4::from_scale(Vec3::new(2.0, 3.0, 1.0));
        let longest = trs
            .x_axis
            .length()
            .max(trs.y_axis.length())
            .max(trs.z_axis.length());
        assert!((linear_operator_norm(&trs) - longest).abs() < 1.0e-5);
        // The sheared chain: 4, not 2.915.
        assert!(
            (linear_operator_norm(&shear()) - 4.0).abs() < 1.0e-4,
            "{}",
            linear_operator_norm(&shear())
        );
    }

    #[test]
    fn a_sheared_chain_switches_by_the_operator_norm() {
        let (mut app, entity, camera, _) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        set_scales(&mut app, 1.0, 1.0, MESH_LOD_SHADOW_SCALE_DEFAULT);
        app.world_mut()
            .entity_mut(entity)
            .insert(GlobalTransform(regia_core::math::Affine3A::from_mat4(shear())));
        // True size 0.6 (fine band); the longest-column size 0.6·2.915/4 ≈ 0.44
        // would sit in the coarse band and switch a frame early. The shadow
        // sizes at 0.6 × 0.5 = 0.3 and takes the coarse LOD (as it should).
        let size = 0.6;
        let d = distance_for(size) * 4.0;
        set_distance(&mut app, camera, d);
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let color = extracted
            .meshes
            .iter()
            .find(|m| !is_shadow_lod_key(m.lod_key))
            .expect("colour draw");
        assert_eq!(
            color.draw_indices.as_ref().map(|i| i.len()),
            Some(6),
            "the fine LOD is kept at true size {size}"
        );
        assert_eq!(color.lod_dither, 0.0);
    }

    #[test]
    fn a_grazing_sheared_sphere_cross_fades_when_it_switches() {
        let (mut app, entity, camera, _) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        set_scales(&mut app, 1.0, 1.0, MESH_LOD_SHADOW_SCALE_DEFAULT);
        app.update();
        // A position where the true sphere (radius 4) meets the frustum and
        // the longest-column sphere (≈ 2.915) does not, far enough out that
        // the fine LOD has switched. Found by a deterministic scan.
        let (snap, frustum) = {
            let extracted = app.world().resource::<ExtractedFrame>();
            let snap = primary_view(extracted).expect("camera");
            let frustum = view_frustum(&snap, 1920, 1080);
            (snap, frustum)
        };
        let eye = Vec3::from_array(snap.eye);
        let mut found = None;
        'scan: for zi in 0..300 {
            let z = -6.0 - zi as f32 * 0.1;
            for xi in 0..300 {
                let x = 3.0 + xi as f32 * 0.05;
                let d = Vec3::new(x, 0.0, z).distance(eye);
                // Past the hysteresis hold: a held LOD 0 switches only when
                // size × 1.15 < 0.5, i.e. d > 4·(1/tan(0.45))·1.15/0.5 ≈ 19.0.
                if d < 19.6 {
                    continue;
                }
                if frustum.intersects_sphere([x, 0.0, z], 4.0)
                    && !frustum.intersects_sphere([x, 0.0, z], 2.915_2)
                {
                    found = Some((x, z));
                    break 'scan;
                }
            }
        }
        let (x, z) = found.expect("a grazing position");
        // Near: LOD 0, on screen.
        app.world_mut()
            .entity_mut(entity)
            .insert(GlobalTransform(regia_core::math::Affine3A::from_mat4(shear())));
        set_distance(&mut app, camera, 4.0);
        app.update();
        assert_eq!(drawn(&app)[0].indices, 6, "near keeps LOD 0");
        // Far at the grazing position: on screen with the true radius, so the
        // switch cross-fades; the underestimated sphere would switch at once.
        let model = Mat4::from_translation(Vec3::new(x, 0.0, z)) * shear();
        app.world_mut()
            .entity_mut(entity)
            .insert(GlobalTransform(regia_core::math::Affine3A::from_mat4(model)));
        app.update();
        let fading: Vec<f32> = drawn(&app)
            .iter()
            .map(|row| row.lod_dither)
            .filter(|dither| *dither != 0.0)
            .collect();
        assert_eq!(fading.len(), 2, "an on-screen switch cross-fades: {fading:?}");
        assert!(
            fading.iter().sum::<f32>().abs() < 1.0e-5,
            "complementary coverage sums to 0, got {fading:?}"
        );
    }

    #[test]
    fn disable_and_skin_stay_on_the_authored_indices() {
        let (mut app, entity, camera, key) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), false);
        app.world_mut().entity_mut(entity).insert(DisableMeshLod);
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mesh_key, key);
        assert_eq!(rows[0].indices, 6);

        let (mut app, _, camera, key) = app_with(Some(chain(&[(2, 0.5), (1, 0.0)])), true);
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mesh_key, key);
    }

    #[test]
    fn shadow_uses_a_coarser_lod_without_dither() {
        let (mut app, _, camera, _) = app_with(Some(chain(&[(3, 0.4), (2, 0.1), (1, 0.0)])), false);
        set_distance(&mut app, camera, distance_for(0.5));
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        assert_eq!(extracted.meshes.len(), 2);
        let color = extracted
            .meshes
            .iter()
            .find(|m| !is_shadow_lod_key(m.lod_key))
            .expect("colour draw");
        let shadow = extracted
            .meshes
            .iter()
            .find(|m| is_shadow_lod_key(m.lod_key))
            .expect("shadow draw");
        assert_eq!(color.draw_indices.as_ref().map(|i| i.len()), Some(9));
        assert_eq!(color.shadow, ShadowCast::Off);
        assert_eq!(color.lod_dither, 0.0);
        assert_eq!(shadow.draw_indices.as_ref().map(|i| i.len()), Some(6));
        assert_eq!(shadow.lod_dither, 0.0);
        assert_eq!(shadow.shadow, ShadowCast::All);
        assert_ne!(color.object_key_xor, shadow.object_key_xor);
    }

    /// (indices drawn, lod_key, object_key_xor, shadow, dither) of every extracted draw.
    fn draw_rows(app: &App) -> Vec<(Option<usize>, u64, u64, ShadowCast, f32)> {
        app.world()
            .resource::<ExtractedFrame>()
            .meshes
            .iter()
            .map(|m| {
                (
                    m.draw_indices.as_ref().map(|i| i.len()),
                    m.lod_key,
                    m.object_key_xor,
                    m.shadow,
                    m.lod_dither,
                )
            })
            .collect()
    }

    fn shadow_draw_len(app: &App) -> Option<usize> {
        let extracted = app.world().resource::<ExtractedFrame>();
        let shadow = extracted.meshes.iter().find(|m| m.shadow != ShadowCast::Off)?;
        shadow.draw_indices.as_ref().map(|i| i.len())
    }

    #[test]
    fn shadow_lod_hint_cap_keeps_the_shadow_on_the_colour_lod() {
        let levels = [(3, 0.4), (2, 0.1), (1, 0.0)];
        let (mut app, entity, camera, _) = app_with(Some(chain(&levels)), false);
        set_distance(&mut app, camera, distance_for(0.5));
        app.update();
        assert_eq!(draw_rows(&app).len(), 2, "without a hint the shadow takes LOD 1");
        assert_eq!(shadow_draw_len(&app), Some(6));

        app.world_mut().entity_mut(entity).insert(ShadowLodHint::cap(0));
        app.update();
        let rows = draw_rows(&app);
        assert_eq!(rows.len(), 1, "capped at LOD 0: one draw casts and shades");
        assert_eq!(rows[0].0, Some(9));
        assert_eq!(rows[0].3, ShadowCast::All);
        assert_eq!(rows[0].1, 0, "LOD 0 colour key: no shadow-only tag");
    }

    #[test]
    fn shadow_lod_hint_floor_and_fixed_level_coarsen_a_near_shadow() {
        let levels = [(3, 0.4), (2, 0.1), (1, 0.0)];
        let (mut app, entity, camera, _) = app_with(Some(chain(&levels)), false);
        set_distance(&mut app, camera, distance_for(1.0));
        app.update();
        assert_eq!(draw_rows(&app).len(), 1, "near: colour and shadow on LOD 0");

        app.world_mut().entity_mut(entity).insert(ShadowLodHint::fixed(2));
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let color = extracted
            .meshes
            .iter()
            .find(|m| !is_shadow_lod_key(m.lod_key))
            .expect("colour draw");
        let shadow = extracted
            .meshes
            .iter()
            .find(|m| is_shadow_lod_key(m.lod_key))
            .expect("shadow draw");
        assert_eq!(color.draw_indices.as_ref().map(|i| i.len()), Some(9));
        assert_eq!(color.shadow, ShadowCast::Off);
        assert_eq!(
            shadow.draw_indices.as_ref().map(|i| i.len()),
            Some(3),
            "LOD 2 shadow"
        );
        assert_eq!(shadow.lod_dither, 0.0);

        // A floor past the chain clamps to its last level; a conflicting cap loses to the floor.
        app.world_mut().entity_mut(entity).insert(ShadowLodHint {
            max_lod: 0,
            min_lod: 7,
        });
        app.update();
        assert_eq!(shadow_draw_len(&app), Some(3));
    }

    #[test]
    fn a_neutral_shadow_lod_hint_reproduces_the_draw_list_bit_for_bit() {
        let levels = [(4, 0.6), (3, 0.3), (2, 0.1), (1, 0.0)];
        let (mut plain, _, plain_camera, _) = app_with(Some(chain(&levels)), false);
        let (mut hinted, entity, hinted_camera, _) = app_with(Some(chain(&levels)), false);
        hinted
            .world_mut()
            .entity_mut(entity)
            .insert(ShadowLodHint::default());
        for size in [1.2, 0.9, 0.5, 0.31, 0.2, 0.12, 0.05, 0.02, 0.2, 0.7, 1.5] {
            set_distance(&mut plain, plain_camera, distance_for(size));
            set_distance(&mut hinted, hinted_camera, distance_for(size));
            plain.update();
            hinted.update();
            let a = draw_rows(&plain);
            let b = draw_rows(&hinted);
            assert_eq!(a.len(), b.len(), "size {size}");
            for (x, y) in a.iter().zip(&b) {
                assert_eq!((x.0, x.1, x.2, x.3), (y.0, y.1, y.2, y.3), "size {size}");
                assert_eq!(x.4.to_bits(), y.4.to_bits(), "size {size}: dither");
            }
        }
    }

    #[test]
    fn removing_the_hint_resumes_the_selected_shadow_lod_at_once() {
        let levels = [(3, 0.4), (2, 0.1), (1, 0.0)];
        let (mut app, entity, camera, _) = app_with(Some(chain(&levels)), false);
        app.world_mut().entity_mut(entity).insert(ShadowLodHint::cap(0));
        set_distance(&mut app, camera, distance_for(0.5));
        app.update();
        assert_eq!(shadow_draw_len(&app), Some(9));
        app.world_mut().entity_mut(entity).remove::<ShadowLodHint>();
        app.update();
        assert_eq!(
            shadow_draw_len(&app),
            Some(6),
            "the unclamped pick was kept in memory"
        );
    }

    #[test]
    fn shadow_lod_hint_apply_clamps() {
        assert!(ShadowLodHint::default().is_neutral());
        assert!(!ShadowLodHint::cap(3).is_neutral());
        for lod in 0..6 {
            assert_eq!(ShadowLodHint::default().apply(lod, 4), lod.min(3));
        }
        assert_eq!(ShadowLodHint::cap(1).apply(3, 4), 1);
        assert_eq!(ShadowLodHint::cap(1).apply(0, 4), 0);
        assert_eq!(
            ShadowLodHint {
                max_lod: 1,
                min_lod: 2
            }
            .apply(0, 4),
            2
        );
        assert_eq!(
            ShadowLodHint::fixed(3).apply(0, 0),
            0,
            "an empty chain never underflows"
        );
    }

    fn set_scales(app: &mut App, bias: f32, distance: f32, shadow: f32) {
        let mut settings = app.world_mut().resource_mut::<RenderSettings>();
        settings.profile.mesh_lod_bias = bias;
        settings.profile.mesh_lod_distance_scale = distance;
        settings.profile.mesh_lod_shadow_scale = shadow;
    }

    /// The colour LOD and the shadow LOD a fresh selector picks at camera distance `d`.
    fn picks_at(levels: &[(u32, f32)], bias: f32, distance: f32, shadow: f32, d: f32) -> (usize, usize) {
        let (mut app, _, camera, _) = app_with(Some(chain(levels)), false);
        set_scales(&mut app, bias, distance, shadow);
        set_distance(&mut app, camera, d);
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let lod = |mesh: &ExtractedMesh| ((mesh.lod_key >> 1) & 3) as usize;
        let color = extracted
            .meshes
            .iter()
            .find(|m| !is_shadow_lod_key(m.lod_key))
            .map_or(usize::MAX, lod);
        let shadow = extracted
            .meshes
            .iter()
            .find(|m| is_shadow_lod_key(m.lod_key))
            .map_or(color, lod);
        (color, shadow)
    }

    /// The selection before the tier scales (v0.1.19), frozen: size × clamp(global × clamp(mesh
    /// bias)), shadows at half that size.
    struct Legacy {
        color: LodSwitch,
        shadow: LodSwitch,
        frame: u64,
    }

    impl Legacy {
        fn step(
            &mut self,
            mesh: &ExtractedMesh,
            snap_of: &MeshLodChain,
            global: f32,
            mesh_bias: f32,
            view: &ViewSnap,
        ) -> Vec<ExtractedMesh> {
            self.frame += 1;
            let bias = clamp_bias(clamp_bias(global) * clamp_bias(mesh_bias));
            let snap = Snap {
                levels: snap_of
                    .levels
                    .iter()
                    .map(|l| (Arc::clone(&l.indices), l.screen_size))
                    .collect(),
                center: Vec3::from_array(snap_of.bounds_center),
                radius: snap_of.bounds_radius,
                bias,
                skinned: false,
                morphing: false,
                shadow_hint: None,
                seen: 0,
            };
            let model = Mat4::from_cols_array_2d(&mesh.model);
            let center = model.transform_point3(snap.center);
            let radius = snap.radius * linear_operator_norm(&model);
            let distance = (center - Vec3::from_array(view.eye)).length();
            let size = projected_screen_size(radius, distance, view.fov_y) * bias;
            let visible = view_frustum(view, 1920, 1080).intersects_sphere(center.to_array(), radius);
            let thresholds: Vec<f32> = snap
                .levels
                .iter()
                .map(|(_, t)| t * lod_threshold_scale(1080))
                .collect();
            let color = step_lod(
                &thresholds,
                size,
                visible,
                true,
                MESH_LOD_HYSTERESIS,
                MESH_LOD_FADE_FRAMES,
                self.frame,
                &mut self.color,
            );
            let shadow = step_lod(
                &thresholds,
                size * 0.5,
                visible,
                false,
                MESH_LOD_HYSTERESIS,
                MESH_LOD_FADE_FRAMES,
                self.frame,
                &mut self.shadow,
            );
            let mut out = Vec::new();
            push_draws(&mut out, mesh.clone(), &snap, color, shadow.lod);
            out
        }
    }

    #[test]
    fn distance_scale_one_reproduces_the_legacy_draw_list_bit_for_bit() {
        let levels = [(8, 0.9), (5, 0.4), (3, 0.15), (1, 0.0)];
        let lod_chain = chain(&levels);
        let (mut app, entity, camera, _) = app_with(Some(lod_chain.clone()), false);
        app.world_mut().entity_mut(entity).insert(MeshLodBias(1.3));
        // Desktop Ultra's bias (1.15) with the scales at their identity values.
        set_scales(&mut app, 1.15, 1.0, 0.5);
        // The template is the entity's unselected draw (what extraction produced).
        let template = {
            let (mut plain, _, plain_camera, _) = app_with(None, false);
            set_distance(&mut plain, plain_camera, 4.0);
            plain.update();
            plain.world().resource::<ExtractedFrame>().meshes[0].clone()
        };
        let mut legacy = Legacy {
            color: LodSwitch::default(),
            shadow: LodSwitch::default(),
            frame: 0,
        };
        // Dolly out through every switch, back in, and out again (fades, hysteresis, re-entry).
        let mut distances: Vec<f32> = (0..60).map(|i| 1.5 + i as f32 * 0.4).collect();
        distances.extend((0..60).rev().map(|i| 1.5 + i as f32 * 0.4));
        distances.extend((0..30).map(|i| 1.5 + i as f32 * 1.1));
        let mut compared = 0usize;
        for d in distances {
            set_distance(&mut app, camera, d);
            app.update();
            let extracted = app.world().resource::<ExtractedFrame>();
            let view = primary_view(extracted).expect("camera");
            let mut source = template.clone();
            source.entity = entity;
            let expected = legacy.step(&source, &lod_chain, 1.15, 1.3, &view);
            let actual = &extracted.meshes;
            assert_eq!(actual.len(), expected.len(), "draw count at {d} m");
            for (a, e) in actual.iter().zip(&expected) {
                let ptr = |m: &ExtractedMesh| m.draw_indices.as_ref().map(Arc::as_ptr);
                assert_eq!(ptr(a), ptr(e), "indices at {d} m");
                assert_eq!(a.lod_dither.to_bits(), e.lod_dither.to_bits(), "dither at {d} m");
                assert_eq!(a.lod_key, e.lod_key, "lod key at {d} m");
                assert_eq!(a.object_key_xor, e.object_key_xor, "object key at {d} m");
                assert_eq!(a.shadow, e.shadow, "shadow cast at {d} m");
                compared += 1;
            }
        }
        assert!(compared > 150, "the sweep compared {compared} draws");
    }

    #[test]
    fn distance_scale_moves_the_colour_switch_linearly() {
        let levels = [(2, 0.5), (1, 0.0)];
        let one = MeshLodScales {
            bias: 1.0,
            distance: 1.0,
            shadow: 0.5,
        };
        let six = MeshLodScales { distance: 6.0, ..one };
        let d1 = one.switch_distance(1.0, 0.5, 1080, 0.9, 1.0, false).expect("d1");
        let d6 = six.switch_distance(1.0, 0.5, 1080, 0.9, 1.0, false).expect("d6");
        assert!((d6 / d1 - 6.0).abs() < 1.0e-4, "{d1} → {d6}");
        assert!(
            (d1 - distance_for(0.5)).abs() < 1.0e-3,
            "{d1} vs {}",
            distance_for(0.5)
        );
        // A fresh selector at 0.9× / 1.2× the switch distance (outside the ±15 % band).
        assert_eq!(picks_at(&levels, 1.0, 1.0, 0.5, d1 * 0.9).0, 0);
        assert_eq!(picks_at(&levels, 1.0, 1.0, 0.5, d1 * 1.2).0, 1);
        assert_eq!(picks_at(&levels, 1.0, 6.0, 0.5, d6 * 0.9).0, 0);
        assert_eq!(picks_at(&levels, 1.0, 6.0, 0.5, d6 * 1.2).0, 1);
        // Scale 6 keeps LOD 0 well past scale 1's switch.
        assert_eq!(picks_at(&levels, 1.0, 6.0, 0.5, d1 * 4.0).0, 0);
    }

    #[test]
    fn shadow_scale_drops_the_shadow_lod_earlier_than_colour() {
        let levels = [(3, 0.4), (2, 0.1), (1, 0.0)];
        let d = distance_for(1.0);
        assert_eq!(
            picks_at(&levels, 1.0, 1.0, 0.5, d),
            (0, 0),
            "0.5: shadow size 0.5 ≥ 0.4"
        );
        assert_eq!(
            picks_at(&levels, 1.0, 1.0, 0.3, d),
            (0, 1),
            "0.3: shadow size 0.3"
        );
        assert_eq!(
            picks_at(&levels, 1.0, 1.0, 0.05, d),
            (0, 2),
            "0.05: shadow size 0.05"
        );
        // The distance scale lifts colour and shadow together; the shadow scale applies after it.
        assert_eq!(
            picks_at(&levels, 1.0, 1.0, 0.5, distance_for(0.3)),
            (1, 1),
            "colour 0.3, shadow 0.15"
        );
        assert_eq!(
            picks_at(&levels, 1.0, 6.0, 0.3, distance_for(0.2)),
            (0, 1),
            "colour 1.2, shadow 0.36"
        );
        let ultra = MeshLodScales::of_profile(&crate::RenderProfile::from_preset(
            RenderProfilePreset::DesktopUltra,
        ));
        let colour = ultra
            .switch_distance(1.0, 0.4, 1080, 0.9, 1.0, false)
            .expect("colour");
        let shadow = ultra
            .switch_distance(1.0, 0.4, 1080, 0.9, 1.0, true)
            .expect("shadow");
        assert!((shadow / colour - 0.15).abs() < 1.0e-5);
    }

    #[test]
    fn tier_scales_clamp_and_sanitise() {
        let with = |bias: f32, distance: f32, shadow: f32| {
            let mut profile = crate::RenderProfile::from_preset(RenderProfilePreset::DesktopLow);
            profile.mesh_lod_bias = bias;
            profile.mesh_lod_distance_scale = distance;
            profile.mesh_lod_shadow_scale = shadow;
            MeshLodScales::of_profile(&profile)
        };
        // The bias clamp is unchanged (0.25..=4).
        assert_eq!(with(10.0, 1.0, 0.5).bias, 4.0);
        assert_eq!(with(0.01, 1.0, 0.5).bias, 0.25);
        assert_eq!(with(f32::NAN, 1.0, 0.5).bias, 1.0);
        // The product clamps to 0.25..=16.
        assert_eq!(with(4.0, 16.0, 0.5).color_factor(1.0), 16.0);
        assert_eq!(with(4.0, 1.0e30, 0.5).color_factor(4.0), 16.0);
        assert_eq!(with(0.25, 0.01, 0.5).color_factor(1.0), 0.25);
        assert_eq!(with(1.0, 6.0, 0.5).color_factor(1.0), 6.0);
        assert_eq!(with(1.15, 6.0, 0.5).color_factor(1.0), 1.15 * 6.0);
        // A per-mesh bias still clamps to 4 before the tier scale.
        assert_eq!(with(1.0, 2.0, 0.5).color_factor(100.0), 8.0);
        // Non-finite or non-positive distance scales read as 1.
        for bad in [f32::NAN, f32::NEG_INFINITY, -3.0, 0.0] {
            assert_eq!(with(1.0, bad, 0.5).distance, 1.0, "{bad}");
            assert_eq!(with(1.0, bad, 0.5).color_factor(1.0), 1.0, "{bad}");
        }
        assert_eq!(with(1.0, f32::INFINITY, 0.5).color_factor(1.0), 1.0);
        // The shadow scale clamps to 0.05..=1; NaN reads as the old 0.5.
        assert_eq!(with(1.0, 1.0, 2.0).shadow, 1.0);
        assert_eq!(with(1.0, 1.0, 0.0).shadow, 0.05);
        assert_eq!(with(1.0, 1.0, -1.0).shadow, 0.05);
        assert_eq!(with(1.0, 1.0, f32::NAN).shadow, 0.5);
        assert_eq!(with(1.0, 1.0, f32::INFINITY).shadow, 0.5);
        // Hostile values still select a level (no panic, no NaN size).
        let levels = [(2, 0.5), (1, 0.0)];
        for (bias, distance, shadow) in [
            (f32::NAN, f32::NAN, f32::NAN),
            (1.0e30, 1.0e30, -1.0e30),
            (-1.0, -1.0, -1.0),
        ] {
            let (color, shadow_lod) = picks_at(&levels, bias, distance, shadow, distance_for(0.3));
            assert!(color < 2 && shadow_lod < 2);
        }
        // No switch distance for the last level's 0 or a degenerate radius.
        let one = MeshLodScales::default();
        assert_eq!(one.switch_distance(1.0, 0.0, 1080, 0.9, 1.0, false), None);
        assert_eq!(one.switch_distance(0.0, 0.5, 1080, 0.9, 1.0, false), None);
        assert_eq!(one.switch_distance(f32::NAN, 0.5, 1080, 0.9, 1.0, false), None);
        assert_eq!(one.switch_distance(1.0, f32::NAN, 1080, 0.9, 1.0, false), None);
    }

    /// Turns the profile's visibility buffer on or off.
    fn set_profile_path(app: &mut App, on: bool) {
        app.world_mut()
            .resource_mut::<RenderSettings>()
            .profile
            .visibility_buffer = on;
    }

    /// `app_with` plus the profile's visibility buffer on, a device-less runtime renderer
    /// holding meshlets for the mesh, its path marked as drawing (`active`), and the hand-off
    /// counters.
    fn app_with_clusters(lod: MeshLodChain, active: bool, cluster_shadows: bool) -> (App, Entity, u64) {
        let (mut app, _, camera, key) = app_with(Some(lod), false);
        set_profile_path(&mut app, true);
        let mut renderer = crate::FrameRenderer::new();
        renderer.insert_meshlets(
            key,
            Arc::new(crate::visbuffer::MeshletMesh::single_level(&[[0.0; 3]], &[0; 6])),
        );
        renderer.set_visbuffer_settings(crate::visbuffer::VisBufferSettings {
            cluster_shadows,
            ..crate::visbuffer::VisBufferSettings::default()
        });
        renderer.force_visbuffer_active(active);
        app.insert_resource(RuntimeRenderer(renderer));
        app.init_resource::<ClusterLodHandoff>();
        (app, camera, key)
    }

    fn set_active(app: &mut App, active: bool) {
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .force_visbuffer_active(active);
    }

    #[test]
    fn meshes_with_clusters_stay_at_lod0_while_the_visibility_buffer_draws() {
        let (mut app, camera, key) = app_with_clusters(chain(&[(2, 0.5), (1, 0.0)]), true, true);
        // Desktop High (distance scale 2, shadow scale 0.35): colour size 0.1, shadow 0.035.
        // Far enough that the chain alone would draw its 1-triangle level.
        set_distance(&mut app, camera, distance_for(0.05));
        for frame in 0..3 {
            app.update();
            let rows = drawn(&app);
            assert_eq!(
                rows.len(),
                1,
                "frame {frame}: one draw, no fade pair, no shadow LOD"
            );
            assert_eq!(rows[0].mesh_key, key, "LOD 0's key: the meshlets are under it");
            assert_eq!(rows[0].indices, 6, "LOD 0's index list");
            assert_eq!(rows[0].lod_dither, 0.0);
            assert_eq!(rows[0].shadow, ShadowCast::All, "the clusters cast");
            assert_eq!(
                *app.world().resource::<ClusterLodHandoff>(),
                ClusterLodHandoff {
                    active: true,
                    chained: 1,
                    cluster_lod: 1,
                }
            );
        }
        // The path stops drawing (profile off, device lost): the chain picks the far level at
        // once, with no cross-fade from LOD 0.
        set_active(&mut app, false);
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1, "a fresh pick is immediate");
        assert_eq!(rows[0].indices, 3);
        assert_eq!(rows[0].lod_dither, 0.0);
        assert_ne!(rows[0].mesh_key, key);
        assert_eq!(
            *app.world().resource::<ClusterLodHandoff>(),
            ClusterLodHandoff {
                active: false,
                chained: 1,
                cluster_lod: 0,
            }
        );
        // And back: LOD 0 again from the next frame, no fade.
        set_active(&mut app, true);
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mesh_key, key);
        assert_eq!(rows[0].lod_dither, 0.0);
    }

    #[test]
    fn without_cluster_shadows_the_shadow_keeps_the_chains_lod() {
        let (mut app, camera, _) = app_with_clusters(chain(&[(3, 0.4), (2, 0.1), (1, 0.0)]), true, false);
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        assert_eq!(
            extracted.meshes.len(),
            2,
            "colour at LOD 0 plus a shadow-only draw"
        );
        let color = extracted
            .meshes
            .iter()
            .find(|m| !is_shadow_lod_key(m.lod_key))
            .expect("colour draw");
        let shadow = extracted
            .meshes
            .iter()
            .find(|m| is_shadow_lod_key(m.lod_key))
            .expect("shadow draw");
        assert_eq!(color.lod_key, 0, "LOD 0's key");
        assert_eq!(color.draw_indices.as_ref().map(|i| i.len()), Some(9));
        assert_eq!(
            color.shadow,
            ShadowCast::Off,
            "the forward shadow passes draw the shadow LOD"
        );
        assert_eq!(color.lod_dither, 0.0);
        assert_eq!(
            shadow.draw_indices.as_ref().map(|i| i.len()),
            Some(3),
            "the coarsest level"
        );
        assert_eq!(shadow.shadow, ShadowCast::All);
        assert_eq!(app.world().resource::<ClusterLodHandoff>().cluster_lod, 1);
    }

    #[test]
    fn a_shadow_lod_hint_applies_to_handed_over_meshes_unless_the_clusters_cast() {
        let chained = |app: &mut App| {
            let mut query = app.world_mut().query::<(Entity, &MeshLodChain)>();
            query
                .iter(app.world())
                .next()
                .map(|(entity, _)| entity)
                .expect("chained entity")
        };
        // Forward shadows of a handed-over mesh: the cap pulls the shadow up to LOD 0, so the
        // one LOD 0 draw casts too.
        let (mut app, camera, key) = app_with_clusters(chain(&[(3, 0.4), (2, 0.1), (1, 0.0)]), true, false);
        let entity = chained(&mut app);
        app.world_mut().entity_mut(entity).insert(ShadowLodHint::cap(0));
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1, "capped: no shadow-only draw");
        assert_eq!(rows[0].mesh_key, key);
        assert_eq!(rows[0].indices, 9);
        assert_eq!(rows[0].shadow, ShadowCast::All);

        // The clusters cast: they pick their own detail per texel and the hint changes nothing.
        let (mut app, camera, key) = app_with_clusters(chain(&[(3, 0.4), (2, 0.1), (1, 0.0)]), true, true);
        let entity = chained(&mut app);
        app.world_mut().entity_mut(entity).insert(ShadowLodHint::fixed(2));
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1, "one LOD 0 draw; the clusters cast");
        assert_eq!(rows[0].mesh_key, key);
        assert_eq!(rows[0].shadow, ShadowCast::All);
    }

    #[test]
    fn only_meshes_with_meshlets_are_handed_over() {
        let lods = chain(&[(2, 0.5), (1, 0.0)]);
        let (mut app, camera, key) = app_with_clusters(lods.clone(), true, true);
        // A second mesh without meshlets, drawn by two entities, and a second entity on the first
        // mesh: the lookup is per mesh, so both of the first mesh's entities are handed over and
        // neither of the second's.
        let other = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0, 0, 0, 0],
        });
        let other_key = crate::runtime::mesh_key(&other);
        let first_mesh = app
            .world_mut()
            .query::<&MeshRenderer>()
            .iter(app.world())
            .next()
            .map(|r| Arc::clone(&r.mesh))
            .expect("the first mesh");
        for (mesh, x) in [(first_mesh, 0.5), (Arc::clone(&other), -0.5), (other, 1.0)] {
            app.world_mut().spawn((
                place(Vec3::new(x, 0.0, 0.0)),
                MeshRenderer::new(mesh, Arc::new(CpuMaterial::default())),
                lods.clone(),
            ));
        }
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 4, "one draw per entity");
        assert_eq!(
            rows.iter()
                .filter(|r| r.mesh_key == key && r.indices == 6)
                .count(),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.mesh_key == other_key ^ mesh_lod_cache_key(1, false) && r.indices == 3)
                .count(),
            2,
            "the mesh without meshlets draws its chain's far level"
        );
        assert_eq!(
            *app.world().resource::<ClusterLodHandoff>(),
            ClusterLodHandoff {
                active: true,
                chained: 4,
                cluster_lod: 2,
            }
        );
    }

    #[test]
    fn switching_the_profile_off_returns_the_chain_on_the_same_frame() {
        // The renderer still says the path drew (that was last frame), but the profile is off
        // now: this frame draws forward, so the chain picks.
        let (mut app, camera, key) = app_with_clusters(chain(&[(2, 0.5), (1, 0.0)]), true, true);
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        assert_eq!(drawn(&app)[0].mesh_key, key);
        set_profile_path(&mut app, false);
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].indices, 3,
            "the chain's far level, no frame at LOD 0 forward"
        );
        assert_eq!(rows[0].lod_dither, 0.0);
        assert!(!app.world().resource::<ClusterLodHandoff>().active);
    }

    #[test]
    fn entities_the_visibility_buffer_cannot_draw_keep_their_chain() {
        // One mesh with meshlets, drawn by the opaque entity and by three that draw forward:
        // blended, transmissive and morphing.
        let lods = chain(&[(2, 0.5), (1, 0.0)]);
        let (mut app, camera, _) = app_with_clusters(lods.clone(), true, true);
        let mesh = app
            .world_mut()
            .query::<&MeshRenderer>()
            .iter(app.world())
            .next()
            .map(|r| Arc::clone(&r.mesh))
            .expect("the mesh");
        let blended = CpuMaterial {
            alpha_mode: crate::ALPHA_BLEND,
            ..CpuMaterial::default()
        };
        let glass = CpuMaterial {
            transmission: 1.0,
            ..CpuMaterial::default()
        };
        for material in [blended, glass] {
            app.world_mut().spawn((
                place(Vec3::new(0.5, 0.0, 0.0)),
                MeshRenderer::new(Arc::clone(&mesh), Arc::new(material)),
                lods.clone(),
            ));
        }
        let targets = crate::morph::MorphTargets::new(&mesh.vertices, &[]).expect("no targets");
        app.world_mut().spawn((
            place(Vec3::new(-0.5, 0.0, 0.0)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::new(CpuMaterial::default())),
            lods,
            crate::MeshMorphTargets::new(Arc::new(targets)),
        ));
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let lod0 = extracted.meshes.iter().filter(|m| m.lod_key == 0).count();
        assert_eq!(lod0, 1, "only the opaque entity is handed over");
        assert_eq!(
            extracted
                .meshes
                .iter()
                .filter(|m| m.lod_key == mesh_lod_cache_key(1, false))
                .count(),
            3,
            "the blended, transmissive and morphing entities draw the far level"
        );
        assert_eq!(
            *app.world().resource::<ClusterLodHandoff>(),
            ClusterLodHandoff {
                active: true,
                chained: 4,
                cluster_lod: 1,
            }
        );
    }

    #[test]
    fn meshes_without_clusters_or_an_idle_path_follow_their_chain() {
        // Meshlets, but the path did not draw the last frame.
        let (mut app, camera, _) = app_with_clusters(chain(&[(2, 0.5), (1, 0.0)]), false, true);
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].indices, 3, "the chain's far level");
        let handoff = *app.world().resource::<ClusterLodHandoff>();
        assert!(!handoff.active);
        assert_eq!((handoff.chained, handoff.cluster_lod), (1, 0));

        // The path draws, but this mesh has no meshlets.
        let (mut app, camera, key) = app_with_clusters(chain(&[(2, 0.5), (1, 0.0)]), true, true);
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .remove_meshlets(key);
        set_distance(&mut app, camera, distance_for(0.05));
        app.update();
        let rows = drawn(&app);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].indices, 3);
        let handoff = *app.world().resource::<ClusterLodHandoff>();
        assert!(handoff.active);
        assert_eq!((handoff.chained, handoff.cluster_lod), (1, 0));

        // A scene with no chain at all reports zeros (and keeps `active`).
        let (mut app, _, _, _) = app_with(None, false);
        set_profile_path(&mut app, true);
        let mut renderer = crate::FrameRenderer::new();
        renderer.force_visbuffer_active(true);
        app.insert_resource(RuntimeRenderer(renderer));
        app.insert_resource(ClusterLodHandoff {
            active: false,
            chained: 7,
            cluster_lod: 7,
        });
        app.update();
        assert_eq!(
            *app.world().resource::<ClusterLodHandoff>(),
            ClusterLodHandoff {
                active: true,
                chained: 0,
                cluster_lod: 0,
            }
        );
    }

    #[test]
    fn cache_key_of_lod0_is_the_arc_address() {
        assert_eq!(mesh_lod_cache_key(0, false), 0);
        assert_eq!(mesh_lod_cache_key(0, true), SHADOW_LOD_MESH_TAG);
        assert_eq!(mesh_lod_cache_key(1, false), 1 << 1);
        assert!(is_shadow_lod_key(mesh_lod_cache_key(2, true) ^ 0x7f12_3456_7890)); // an Arc address
        // A host-hashed key such as the editor's terrain key is never mistaken for the tag.
        assert!(!is_shadow_lod_key(0xed17_2000_0000_0000));
        assert!(!is_shadow_lod_key(crate::EPHEMERAL_KEY | 5));
    }

    #[test]
    fn a_registry_key_keeps_the_shadow_tag_on_its_shadow_lod() {
        let base = crate::runtime::MESH_ID_KEY_TAG | (7 << 3);
        for lod in 0..4 {
            let shadow = lod_variant_key(base, mesh_lod_cache_key(lod, true));
            assert!(is_shadow_lod_key(shadow), "LOD {lod}: {shadow:#x}");
            let color = lod_variant_key(base, mesh_lod_cache_key(lod, false));
            assert!(!is_shadow_lod_key(color), "LOD {lod}: {color:#x}");
            assert_eq!(color, base ^ mesh_lod_cache_key(lod, false));
            // The id and the LOD bits survive, so variants of two meshes never meet.
            let other = lod_variant_key(base + (1 << 3), mesh_lod_cache_key(lod, true));
            assert_ne!(shadow, other);
            assert_ne!(shadow, color);
        }
        // An `Arc` address key: the xor already carried the tag.
        let addr = 0x7f12_3456_7890;
        assert_eq!(
            lod_variant_key(addr, mesh_lod_cache_key(2, true)),
            addr ^ mesh_lod_cache_key(2, true)
        );
    }

    /// Whether a sphere is inside the frustum `view_frustum` builds (the per-mesh test the
    /// frame loop runs against the frustum it builds once).
    fn on_screen(view: &ViewSnap, width: u32, height: u32, center: Vec3, radius: f32) -> bool {
        view_frustum(view, width, height).intersects_sphere(center.to_array(), radius)
    }

    /// ADR-869: at 4:3 the frame draws the horizontal-plus FOV, wider than the authored one; a
    /// mesh in that extra band is on screen. A degenerate camera (`far = ∞`, NaN fov) still
    /// gives a usable frustum instead of rejecting everything.
    #[test]
    fn on_screen_uses_the_drawn_frustum() {
        let view = ViewSnap {
            eye: [0.0, 0.0, 0.0],
            target: [0.0, 0.0, -1.0],
            fov_y: 0.8,
            near: 0.1,
            far: 1000.0,
        };
        // 10 m ahead, just above the authored frustum's top (tan 0.4 × 10 ≈ 4.23 m) but inside the
        // 4:3 horizontal-plus one (≈ 5.6 m).
        let center = Vec3::new(0.0, 4.9, -10.0);
        assert!(on_screen(&view, 1440, 1080, center, 0.2));
        // At 16:9 the drawn FOV is the authored one: the same mesh is off screen.
        assert!(!on_screen(&view, 1920, 1080, center, 0.2));
        let degenerate = ViewSnap {
            fov_y: f32::NAN,
            far: f32::INFINITY,
            ..view
        };
        assert!(on_screen(
            &degenerate,
            1920,
            1080,
            Vec3::new(0.0, 0.0, -50.0),
            1.0
        ));
        assert!(!on_screen(
            &degenerate,
            1920,
            1080,
            Vec3::new(0.0, 0.0, 50.0),
            1.0
        ));
    }
}
