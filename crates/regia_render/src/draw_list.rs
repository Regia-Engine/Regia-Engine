// SPDX-License-Identifier: MPL-2.0
//! Keyed draws without one entity per draw (ADR-8087).
//!
//! A host that keeps its own scene (a lockstep sim mirror, a crowd, thousands of scatter rocks)
//! used to spawn one ECS entity per drawn part and write `Transform`, `GlobalTransform` and
//! `MeshRenderer` into it every frame, keeping a spare-entity pool so a despawn did not mint a
//! new object-cache key. [`DrawList`] replaces that mirror: the host keys each draw with a
//! [`DrawKey`] it already has, and [`crate::extract_frame`] appends every visible item to
//! [`crate::ExtractedFrame::meshes`] after the entity meshes. No entity is spawned, no command
//! is queued, and nothing is written per frame for a draw that did not move.
//!
//! - **Identity.** A draw's object key (the renderer's per-object cache slot and the temporal
//!   history key that motion vectors and TAA read) is [`DrawKey::object_key`]: a fixed function
//!   of the key, so the same key keeps its history across frames, across removal and re-insertion,
//!   and across runs. The extracted mesh carries `Entity::PLACEHOLDER` (never a live entity) and
//!   `object_key_xor` so that `entity.to_bits() ^ object_key_xor` is that key, the convention the
//!   scatter field's entity-less draws already use.
//! - **Mesh cache.** The GPU mesh and material caches key on `Arc` identity: keep one `Arc` per
//!   asset and hand the same clone to every draw of it, or every frame uploads again.
//! - **Order.** Items are stored and extracted in ascending key order, so the frame is the same
//!   whatever order the host inserted them in.
//! - **Two ways to feed it.** Retained: [`DrawList::insert`], [`DrawList::set_transform`],
//!   [`DrawList::remove`] when something changes. Per frame (the mirror pattern): wrap the frame's
//!   draws in [`DrawList::begin_sync`] / [`DrawList::submit`] / [`DrawList::end_sync`], and every
//!   key not submitted this frame is dropped ([`DrawList::sync`] does all three).
//!
//! Draws from the list are not entities, so entity-keyed features (`Visibility`, layer parking,
//! render overrides, glass, morph targets, `ViewVisibility`) do not apply to them;
//! [`KeyedDraw::visible`] and [`KeyedDraw::layer`] are the list's own visibility. Frustum and
//! occlusion culling, shadows, skinning from a palette and decals work as for any extracted mesh.
//!
//! - **LOD** (ADR-8457). A draw with [`KeyedDraw::with_lod_chain`] is selected by
//!   [`crate::select_mesh_lods`] exactly as an entity with a [`crate::MeshLodChain`]: projected
//!   size times [`KeyedDraw::lod_bias`] and the tier scales, the same hysteresis band, the same
//!   dither cross-fade and shadow LOD ([`KeyedDraw::shadow_lod_hint`]). Its hysteresis memory is
//!   keyed by the [`DrawKey`] in a map of its own, so a draw keeps its level while the host
//!   re-submits it every frame, and a key never shares memory with an entity. The outgoing LOD
//!   of a fade and a separate shadow LOD draw carry the key's object key with the same tags an
//!   entity's do. [`DrawList::lod_stats`] counts the last selection.
//!
//! Agent: `render.draw_list` (`book/agents/draw-list.md`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use regia_app::prelude::*;
use regia_core::math::Mat4;

use crate::runtime::{CpuMaterial, CpuMesh, ExtractedMesh, counted_mesh_id};
use crate::{JointMatrix, ShadowCast};

/// Salt of [`DrawKey::object_key`], so a draw-list key and the same number used as an object key
/// elsewhere (an entity's bits, a scatter instance) do not land on one cache slot.
const OBJECT_KEY_SALT: u64 = 0xD1A7_C0DE_5EED_0B1E;

/// The SplitMix64 output function (Steele, Lea and Flood 2014; Vigna's public-domain
/// reference): a bijection on `u64` that spreads nearby inputs over the whole range.
#[must_use]
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The host's name for one draw, stable across frames: a sim id with a part index, a prop
/// instance, a terrain tile. Two items with the same key are the same draw; inserting under a
/// key that exists replaces it.
///
/// # Example
///
/// ```
/// use regia_render::DrawKey;
///
/// let hull_part = DrawKey::pair(42, 3);
/// assert_eq!(hull_part, DrawKey::pair(42, 3));
/// assert_ne!(hull_part, DrawKey::pair(42, 4));
/// assert_eq!(hull_part.object_key(), DrawKey::pair(42, 3).object_key());
/// assert_ne!(DrawKey(7).object_key(), 7);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DrawKey(pub u64);

impl DrawKey {
    /// A key from two numbers (an owner id and a part index, a kind and an instance). The pair is
    /// mixed, not packed, so any two `u64`s fit; distinct pairs collide with odds of about
    /// `n² / 2⁶⁵` for `n` keys (about 2 × 10⁻¹² for 9,000).
    #[must_use]
    pub const fn pair(a: u64, b: u64) -> Self {
        Self(mix64(mix64(a ^ OBJECT_KEY_SALT).wrapping_add(b)))
    }

    /// The object key the renderer gives this draw (`FrameMesh::object_key`): its per-object
    /// cache slot and temporal history. A fixed function of the key with the top bit set, so it is
    /// never 0 and does not meet a live entity's bits (an entity's generation would need 2³¹
    /// reuses of one slot to reach it).
    #[must_use]
    pub const fn object_key(self) -> u64 {
        mix64(self.0 ^ OBJECT_KEY_SALT) | (1 << 63)
    }
}

/// One draw: a shared mesh and material at a world transform.
///
/// Built with [`KeyedDraw::new`] and the `with_*` methods; the fields are public to read and
/// change in place ([`DrawList::get_mut`]). The struct is `#[non_exhaustive]` so later fields
/// (LOD chains, cross-fades) are not breaking changes.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use regia_core::math::{Mat4, Vec3};
/// use regia_render::{CpuMaterial, CpuMesh, KeyedDraw, ShadowCast};
///
/// let mesh = Arc::new(CpuMesh { vertices: vec![], indices: vec![] });
/// let material = Arc::new(CpuMaterial::default());
/// let item = KeyedDraw::new(mesh, material, Mat4::from_translation(Vec3::new(1.0, 0.0, 0.0)))
///     .with_layer(2)
///     .with_shadow(ShadowCast::Off);
/// assert_eq!(item.layer, 2);
/// assert!(item.visible);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct KeyedDraw {
    /// Shared mesh (the GPU upload is cached by `Arc` identity: reuse the clone).
    pub mesh: Arc<CpuMesh>,
    /// Shared material (cached by `Arc` identity).
    pub material: Arc<CpuMaterial>,
    /// Object-to-world matrix.
    pub model: Mat4,
    /// Render layer (`0..=31`, as [`crate::MeshRenderer::layer`]).
    pub layer: u32,
    /// False keeps the item (and its key's history) in the list but draws nothing.
    pub visible: bool,
    /// Which shadow views it is drawn into (default [`ShadowCast::All`]).
    pub shadow: ShadowCast,
    /// Skin palette in the space of [`crate::SkinJoints::palette`]; empty draws rigid.
    pub joints: Vec<JointMatrix>,
    /// Decal receiver layer (as [`crate::MeshRenderer::decal_layer`]).
    pub decal_layer: u32,
    /// GI proxy (as [`crate::MeshRenderer::gi_proxy`], default true). Turn it off for thousands
    /// of small instances: a brick is stored per mesh and any instance opting out opts the mesh out.
    pub gi_proxy: bool,
    /// Static geometry (as the `Static` marker on an entity).
    pub is_static: bool,
    /// Second UV set, one entry per vertex (as [`crate::MeshUv1`]).
    pub uv1: Option<Arc<Vec<[f32; 2]>>>,
    /// LOD chain (as a [`crate::MeshLodChain`] next to an entity's `MeshRenderer`): the
    /// selection picks a level per frame by projected size, with the same hysteresis and dither
    /// cross-fade, its memory keyed by the [`DrawKey`] (ADR-8457). Share one `Arc` per asset.
    /// `None` draws the mesh's own index buffer.
    pub lod_chain: Option<Arc<crate::MeshLodChain>>,
    /// Multiplier on the projected size (as [`crate::MeshLodBias`]; 1 = none, above 1 keeps
    /// detail longer).
    pub lod_bias: f32,
    /// Bounds on the shadow draw's LOD (as a [`crate::ShadowLodHint`] on an entity); `None`
    /// keeps the selected shadow LOD.
    pub shadow_lod_hint: Option<crate::ShadowLodHint>,
}

impl KeyedDraw {
    /// A visible rigid draw on layer 0 that casts every shadow and is a GI proxy: what a
    /// [`crate::MeshRenderer::new`] entity with the same transform draws.
    #[must_use]
    pub fn new(mesh: Arc<CpuMesh>, material: Arc<CpuMaterial>, model: Mat4) -> Self {
        Self {
            mesh,
            material,
            model,
            layer: 0,
            visible: true,
            shadow: ShadowCast::All,
            joints: Vec::new(),
            decal_layer: 0,
            gi_proxy: true,
            is_static: false,
            uv1: None,
            lod_chain: None,
            lod_bias: 1.0,
            shadow_lod_hint: None,
        }
    }

    /// With render layer `layer`.
    #[must_use]
    pub fn with_layer(mut self, layer: u32) -> Self {
        self.layer = layer;
        self
    }

    /// Drawn (`true`) or kept but hidden.
    #[must_use]
    pub fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// With shadow policy `shadow`.
    #[must_use]
    pub fn with_shadow(mut self, shadow: ShadowCast) -> Self {
        self.shadow = shadow;
        self
    }

    /// Skinned with palette `joints`.
    #[must_use]
    pub fn with_joints(mut self, joints: Vec<JointMatrix>) -> Self {
        self.joints = joints;
        self
    }

    /// With decal receiver layer `decal_layer`.
    #[must_use]
    pub fn with_decal_layer(mut self, decal_layer: u32) -> Self {
        self.decal_layer = decal_layer;
        self
    }

    /// A GI proxy or not.
    #[must_use]
    pub fn with_gi_proxy(mut self, gi_proxy: bool) -> Self {
        self.gi_proxy = gi_proxy;
        self
    }

    /// Static geometry or not.
    #[must_use]
    pub fn with_static(mut self, is_static: bool) -> Self {
        self.is_static = is_static;
        self
    }

    /// With second UV set `uv1`.
    #[must_use]
    pub fn with_uv1(mut self, uv1: Arc<Vec<[f32; 2]>>) -> Self {
        self.uv1 = Some(uv1);
        self
    }

    /// With LOD chain `chain` (a chain of fewer than two levels draws the mesh as is).
    #[must_use]
    pub fn with_lod_chain(mut self, chain: Arc<crate::MeshLodChain>) -> Self {
        self.lod_chain = Some(chain);
        self
    }

    /// With LOD bias `bias` (multiplier on the projected size).
    #[must_use]
    pub fn with_lod_bias(mut self, bias: f32) -> Self {
        self.lod_bias = bias;
        self
    }

    /// With shadow LOD bounds `hint`.
    #[must_use]
    pub fn with_shadow_lod_hint(mut self, hint: crate::ShadowLodHint) -> Self {
        self.shadow_lod_hint = Some(hint);
        self
    }

    /// Whether extraction draws it: visible, and a mesh with triangles (or a counted asset
    /// stand-in, which draws its upload).
    fn drawable(&self) -> bool {
        self.visible
            && ((!self.mesh.vertices.is_empty() && !self.mesh.indices.is_empty())
                || counted_mesh_id(&self.mesh).is_some())
    }
}

/// One stored item and the sync pass that last submitted it.
#[derive(Clone, Debug)]
struct Slot {
    item: KeyedDraw,
    seen: u64,
}

/// What one [`DrawList::sync`] (or `end_sync`) pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawSyncReport {
    /// Keys that were not in the list before this pass.
    pub inserted: usize,
    /// Keys that were in the list and were submitted again.
    pub updated: usize,
    /// Keys in the list that this pass did not submit, now removed.
    pub removed: usize,
}

/// Counts of a [`DrawList`] (agent `render.draw_list`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawListStats {
    /// Items in the list.
    pub items: usize,
    /// Items with [`KeyedDraw::visible`] false.
    pub hidden: usize,
    /// Items with a skin palette.
    pub skinned: usize,
    /// Items with a [`KeyedDraw::lod_chain`] of two or more levels (ADR-8457).
    pub chained: usize,
    /// Items that cast a [`ShadowCast::Local`] shadow.
    pub local_casters: usize,
    /// Items that cast no shadow ([`ShadowCast::Off`]).
    pub shadow_off: usize,
    /// Meshes the last extraction appended to the frame.
    pub extracted: usize,
    /// Extractions that read the list since it was created.
    pub extractions: u64,
}

/// What the last [`crate::select_mesh_lods`] run did with the list's chained draws (ADR-8457,
/// agent `render.draw_list` `lod`).
///
/// Only drawn rigid items with a chain of two or more levels are counted, and only while a camera
/// is extracted (the selection needs a view). Skinned items keep their mesh, as skinned entities do.
///
/// # Example
///
/// ```
/// use regia_render::DrawList;
///
/// let list = DrawList::default();
/// let lod = list.lod_stats();
/// assert_eq!((lod.selected, lod.crossfading, lod.selections), (0, 0, 0));
/// assert!(lod.color_lods.is_empty());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrawLodStats {
    /// Chained draws the selection placed.
    pub selected: usize,
    /// Of those, how many draw each colour LOD (index 0 = LOD 0, the finest), the incoming level
    /// of a cross-fade included.
    pub color_lods: Vec<usize>,
    /// Of those that cast a shadow, how many cast it from each LOD (after
    /// [`KeyedDraw::shadow_lod_hint`]).
    pub shadow_lods: Vec<usize>,
    /// Draws cross-fading between two colour LODs (each then has two colour draws).
    pub crossfading: usize,
    /// Selection runs that have written these counts since the list was created.
    pub selections: u64,
}

impl DrawLodStats {
    /// Empties the counts for a new selection run (keeps the allocations).
    pub(crate) fn clear(&mut self) {
        self.selected = 0;
        self.color_lods.clear();
        self.shadow_lods.clear();
        self.crossfading = 0;
    }

    /// Counts one placed draw: colour LOD `color`, the outgoing LOD of a fade, and the shadow LOD
    /// when it casts.
    pub(crate) fn note(&mut self, color: usize, fading_from: Option<usize>, shadow: Option<usize>) {
        fn bump(counts: &mut Vec<usize>, index: usize) {
            if counts.len() <= index {
                counts.resize(index + 1, 0);
            }
            counts[index] += 1;
        }
        self.selected += 1;
        bump(&mut self.color_lods, color);
        if fading_from.is_some() {
            self.crossfading += 1;
        }
        if let Some(shadow) = shadow {
            bump(&mut self.shadow_lods, shadow);
        }
    }
}

/// Keyed draws the renderer extracts without an entity per draw (see the module docs). The
/// render plugin inserts an empty one; a host fills it from any system that runs before
/// `Render`.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use regia_core::math::Mat4;
/// use regia_render::{CpuMaterial, CpuMesh, KeyedDraw, DrawKey, DrawList};
///
/// let mesh = Arc::new(CpuMesh { vertices: vec![], indices: vec![] });
/// let material = Arc::new(CpuMaterial::default());
/// let mut list = DrawList::default();
/// // Per frame: submit what is drawn now; keys not submitted are dropped.
/// let report = list.sync((0..3u64).map(|i| {
///     (DrawKey::pair(7, i), KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&material), Mat4::IDENTITY))
/// }));
/// assert_eq!((report.inserted, list.len()), (3, 3));
/// let report = list.sync([(DrawKey::pair(7, 1), KeyedDraw::new(mesh, material, Mat4::IDENTITY))]);
/// assert_eq!((report.updated, report.removed, list.len()), (1, 2, 1));
/// ```
#[derive(Resource, Debug, Default)]
pub struct DrawList {
    items: BTreeMap<DrawKey, Slot>,
    /// The current sync pass (bumped by [`Self::begin_sync`]).
    pass: u64,
    /// Inserted / updated counts of the open sync pass.
    open: DrawSyncReport,
    /// Meshes the last extraction appended.
    extracted: AtomicUsize,
    /// Extractions that read the list.
    extractions: AtomicU64,
    /// What the last LOD selection did with the chained items.
    lod: Mutex<DrawLodStats>,
}

impl DrawList {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `item` under `key` and returns the item it replaced.
    pub fn insert(&mut self, key: DrawKey, item: KeyedDraw) -> Option<KeyedDraw> {
        let pass = self.pass;
        self.items
            .insert(key, Slot { item, seen: pass })
            .map(|slot| slot.item)
    }

    /// Removes the item under `key`.
    pub fn remove(&mut self, key: DrawKey) -> Option<KeyedDraw> {
        self.items.remove(&key).map(|slot| slot.item)
    }

    /// The item under `key`.
    #[must_use]
    pub fn get(&self, key: DrawKey) -> Option<&KeyedDraw> {
        self.items.get(&key).map(|slot| &slot.item)
    }

    /// The item under `key`, to change in place.
    pub fn get_mut(&mut self, key: DrawKey) -> Option<&mut KeyedDraw> {
        self.items.get_mut(&key).map(|slot| &mut slot.item)
    }

    /// Whether `key` is in the list.
    #[must_use]
    pub fn contains(&self, key: DrawKey) -> bool {
        self.items.contains_key(&key)
    }

    /// Moves the item under `key`; false when there is none.
    pub fn set_transform(&mut self, key: DrawKey, model: Mat4) -> bool {
        self.get_mut(key).map(|item| item.model = model).is_some()
    }

    /// Shows or hides the item under `key` (it keeps its key and history); false when there is
    /// none.
    pub fn set_visible(&mut self, key: DrawKey, visible: bool) -> bool {
        self.get_mut(key).map(|item| item.visible = visible).is_some()
    }

    /// Items in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Removes every item.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Keeps the items `keep` answers true for.
    pub fn retain(&mut self, mut keep: impl FnMut(DrawKey, &KeyedDraw) -> bool) {
        self.items.retain(|key, slot| keep(*key, &slot.item));
    }

    /// Every item, ascending by key (the extraction order).
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (DrawKey, &KeyedDraw)> + '_ {
        self.items.iter().map(|(key, slot)| (*key, &slot.item))
    }

    /// Every key, ascending.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = DrawKey> + '_ {
        self.items.keys().copied()
    }

    /// Opens a sync pass: from here to [`Self::end_sync`], [`Self::submit`] marks each key it
    /// stores, and `end_sync` drops every key it did not mark.
    pub fn begin_sync(&mut self) {
        self.pass = self.pass.wrapping_add(1);
        self.open = DrawSyncReport::default();
    }

    /// Stores `item` under `key` in the open sync pass (replacing any item there).
    pub fn submit(&mut self, key: DrawKey, item: KeyedDraw) {
        let pass = self.pass;
        match self.items.get_mut(&key) {
            Some(slot) => {
                slot.item = item;
                slot.seen = pass;
                self.open.updated += 1;
            }
            None => {
                self.items.insert(key, Slot { item, seen: pass });
                self.open.inserted += 1;
            }
        }
    }

    /// Closes the sync pass: removes every key [`Self::submit`] did not store since
    /// [`Self::begin_sync`], and reports the pass.
    pub fn end_sync(&mut self) -> DrawSyncReport {
        let pass = self.pass;
        let before = self.items.len();
        self.items.retain(|_, slot| slot.seen == pass);
        let mut report = std::mem::take(&mut self.open);
        report.removed = before - self.items.len();
        report
    }

    /// One whole sync pass: the list afterwards holds exactly `draws` (a key given twice keeps
    /// the last item).
    pub fn sync(&mut self, draws: impl IntoIterator<Item = (DrawKey, KeyedDraw)>) -> DrawSyncReport {
        self.begin_sync();
        for (key, item) in draws {
            self.submit(key, item);
        }
        self.end_sync()
    }

    /// The list's counts.
    #[must_use]
    pub fn stats(&self) -> DrawListStats {
        let mut stats = DrawListStats {
            items: self.items.len(),
            extracted: self.extracted.load(Ordering::Relaxed),
            extractions: self.extractions.load(Ordering::Relaxed),
            ..DrawListStats::default()
        };
        for slot in self.items.values() {
            let item = &slot.item;
            stats.hidden += usize::from(!item.visible);
            stats.skinned += usize::from(!item.joints.is_empty());
            stats.chained += usize::from(item.lod_chain.as_ref().is_some_and(|c| c.levels.len() >= 2));
            stats.local_casters += usize::from(matches!(item.shadow, ShadowCast::Local { .. }));
            stats.shadow_off += usize::from(item.shadow == ShadowCast::Off);
        }
        stats
    }

    /// What the last [`crate::select_mesh_lods`] run did with the chained items (all zero before
    /// the first run, or in an app without the selection).
    #[must_use]
    pub fn lod_stats(&self) -> DrawLodStats {
        self.lod.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Stores one selection run's counts (the selection system calls it).
    pub(crate) fn record_lod(&self, run: &DrawLodStats) {
        let mut stored = self.lod.lock().unwrap_or_else(PoisonError::into_inner);
        stored.selected = run.selected;
        stored.color_lods.clone_from(&run.color_lods);
        stored.shadow_lods.clone_from(&run.shadow_lods);
        stored.crossfading = run.crossfading;
        stored.selections = stored.selections.saturating_add(1);
    }

    /// Appends every drawable item to `out` in key order, as [`crate::extract_frame`] does after
    /// the entity meshes, and returns how many it appended.
    pub fn extend_extracted(&self, out: &mut Vec<ExtractedMesh>) -> usize {
        let _span = tracing::info_span!("draw_list_extract", items = self.items.len()).entered();
        let anchor = Entity::PLACEHOLDER;
        let before = out.len();
        out.reserve(self.items.len());
        for (key, slot) in &self.items {
            let item = &slot.item;
            if !item.drawable() {
                continue;
            }
            out.push(ExtractedMesh {
                entity: anchor,
                mesh: Arc::clone(&item.mesh),
                material: Arc::clone(&item.material),
                layer: item.layer,
                model: item.model.to_cols_array_2d(),
                joints: item.joints.clone(),
                decal_layer: item.decal_layer,
                uv1: item.uv1.clone(),
                shadow: item.shadow,
                draw_indices: None,
                lod_key: 0,
                object_key_xor: key.object_key() ^ anchor.to_bits(),
                gi_proxy: item.gi_proxy,
                lod_dither: 0.0,
                is_static: item.is_static,
            });
        }
        let appended = out.len() - before;
        self.extracted.store(appended, Ordering::Relaxed);
        self.extractions.fetch_add(1, Ordering::Relaxed);
        appended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle() -> Arc<CpuMesh> {
        let v = crate::LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]);
        Arc::new(CpuMesh {
            vertices: vec![v; 3],
            indices: vec![0, 1, 2],
        })
    }

    fn item(mesh: &Arc<CpuMesh>, material: &Arc<CpuMaterial>) -> KeyedDraw {
        KeyedDraw::new(Arc::clone(mesh), Arc::clone(material), Mat4::IDENTITY)
    }

    #[test]
    fn mix64_matches_the_splitmix64_reference_output() {
        // SplitMix64 seeded with 0 yields mix64(0x9E37_79B9_7F4A_7C15) first: 0xE220_A839_7B1D_CDAF.
        assert_eq!(mix64(0x9E37_79B9_7F4A_7C15), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn object_keys_are_stable_nonzero_and_outside_entity_bits() {
        for raw in [0, 1, 2, u64::MAX, 0x1_0000_0001] {
            let key = DrawKey(raw);
            assert_eq!(key.object_key(), DrawKey(raw).object_key());
            assert_ne!(key.object_key(), 0);
            assert!(key.object_key() >> 63 == 1, "top bit set");
        }
        let mut keys: Vec<u64> = (0..10_000u64).map(|i| DrawKey(i).object_key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 10_000, "consecutive keys get distinct object keys");
    }

    #[test]
    fn extraction_is_in_key_order_whatever_the_insert_order() {
        let (mesh, material) = (triangle(), Arc::new(CpuMaterial::default()));
        let mut forward = DrawList::new();
        let mut backward = DrawList::new();
        for i in 0..50u64 {
            forward.insert(DrawKey(i), item(&mesh, &material));
            backward.insert(DrawKey(49 - i), item(&mesh, &material));
        }
        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert_eq!(forward.extend_extracted(&mut a), 50);
        assert_eq!(backward.extend_extracted(&mut b), 50);
        let keys = |v: &[ExtractedMesh]| v.iter().map(|m| m.object_key_xor).collect::<Vec<_>>();
        assert_eq!(keys(&a), keys(&b));
    }

    #[test]
    fn hidden_and_empty_items_are_not_extracted() {
        let (mesh, material) = (triangle(), Arc::new(CpuMaterial::default()));
        let empty = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let mut list = DrawList::new();
        list.insert(DrawKey(1), item(&mesh, &material));
        list.insert(DrawKey(2), item(&mesh, &material).with_visible(false));
        list.insert(DrawKey(3), item(&empty, &material));
        let mut out = Vec::new();
        assert_eq!(list.extend_extracted(&mut out), 1);
        assert_eq!(
            out[0].entity.to_bits() ^ out[0].object_key_xor,
            DrawKey(1).object_key()
        );
        let stats = list.stats();
        assert_eq!(
            (stats.items, stats.hidden, stats.extracted, stats.extractions),
            (3, 1, 1, 1)
        );
        assert!(list.set_visible(DrawKey(2), true));
        assert_eq!(list.extend_extracted(&mut Vec::new()), 2);
    }

    #[test]
    fn a_sync_pass_drops_the_keys_it_did_not_submit() {
        let (mesh, material) = (triangle(), Arc::new(CpuMaterial::default()));
        let mut list = DrawList::new();
        let report = list.sync((0..10u64).map(|i| (DrawKey(i), item(&mesh, &material))));
        assert_eq!(
            report,
            DrawSyncReport {
                inserted: 10,
                updated: 0,
                removed: 0
            }
        );
        list.begin_sync();
        for i in 5..12u64 {
            list.submit(DrawKey(i), item(&mesh, &material));
        }
        let report = list.end_sync();
        assert_eq!(
            report,
            DrawSyncReport {
                inserted: 2,
                updated: 5,
                removed: 5
            }
        );
        assert_eq!(
            list.keys().map(|k| k.0).collect::<Vec<_>>(),
            (5..12).collect::<Vec<_>>()
        );
        // A retained insert between passes survives only if the next pass submits it.
        list.insert(DrawKey(99), item(&mesh, &material));
        let report = list.sync(std::iter::empty());
        assert_eq!(report.removed, 8);
        assert!(list.is_empty());
    }

    #[test]
    fn stats_count_chained_local_and_shadowless_items() {
        let (mesh, material) = (triangle(), Arc::new(CpuMaterial::default()));
        let level = |n: usize, screen_size: f32| crate::MeshLodLevel {
            indices: Arc::new(vec![0; n * 3]),
            screen_size,
        };
        let chain = |levels: Vec<crate::MeshLodLevel>| {
            Arc::new(crate::MeshLodChain {
                levels,
                bounds_center: [0.0; 3],
                bounds_radius: 1.0,
            })
        };
        let local = ShadowCast::Local {
            reach: 1.0,
            max_cascades: 1,
            shadow_scale: 1.0,
            shadow_lift: 0.0,
            anchor: None,
            lod_switch_keeps_pages: false,
        };
        let mut list = DrawList::new();
        list.insert(
            DrawKey(1),
            item(&mesh, &material).with_lod_chain(chain(vec![level(2, 0.3), level(1, 0.0)])),
        );
        // A one-level chain selects nothing: not counted as chained.
        list.insert(
            DrawKey(2),
            item(&mesh, &material).with_lod_chain(chain(vec![level(1, 0.0)])),
        );
        list.insert(DrawKey(3), item(&mesh, &material).with_shadow(local));
        list.insert(DrawKey(4), item(&mesh, &material).with_shadow(ShadowCast::Off));
        let stats = list.stats();
        assert_eq!((stats.chained, stats.local_casters, stats.shadow_off), (1, 1, 1));
        let fields = item(&mesh, &material)
            .with_lod_bias(2.0)
            .with_shadow_lod_hint(crate::ShadowLodHint::cap(1));
        assert_eq!(fields.lod_bias, 2.0);
        assert_eq!(fields.shadow_lod_hint, Some(crate::ShadowLodHint::cap(1)));
        assert!(fields.lod_chain.is_none());
    }

    #[test]
    fn lod_stats_count_levels_fades_and_runs() {
        let list = DrawList::new();
        let mut run = DrawLodStats::default();
        run.note(0, None, Some(1));
        run.note(2, Some(1), None);
        run.note(2, None, Some(2));
        list.record_lod(&run);
        let stored = list.lod_stats();
        assert_eq!(stored.selected, 3);
        assert_eq!(stored.color_lods, vec![1, 0, 2]);
        assert_eq!(stored.shadow_lods, vec![0, 1, 1]);
        assert_eq!(stored.crossfading, 1);
        assert_eq!(stored.selections, 1);
        run.clear();
        list.record_lod(&run);
        let stored = list.lod_stats();
        assert_eq!(
            (stored.selected, stored.crossfading, stored.selections),
            (0, 0, 2)
        );
        assert!(stored.color_lods.is_empty() && stored.shadow_lods.is_empty());
    }

    #[test]
    fn pair_keys_differ_by_either_half() {
        assert_ne!(DrawKey::pair(1, 2), DrawKey::pair(2, 1));
        assert_ne!(DrawKey::pair(0, 0), DrawKey::pair(0, 1));
        assert_ne!(DrawKey::pair(0, 0), DrawKey::pair(1, 0));
    }

    #[test]
    fn every_item_field_reaches_the_extracted_mesh() {
        let (mesh, material) = (triangle(), Arc::new(CpuMaterial::default()));
        let uv1 = Arc::new(vec![[0.25, 0.75]; 3]);
        let shadow = ShadowCast::Off;
        let model = Mat4::from_translation(regia_core::math::Vec3::new(1.0, 2.0, 3.0));
        let mut list = DrawList::new();
        list.insert(
            DrawKey(5),
            KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&material), model)
                .with_layer(3)
                .with_shadow(shadow)
                .with_joints(vec![JointMatrix::translation(0.0, 1.0, 0.0)])
                .with_decal_layer(4)
                .with_gi_proxy(false)
                .with_static(true)
                .with_uv1(Arc::clone(&uv1)),
        );
        let mut out = Vec::new();
        assert_eq!(list.extend_extracted(&mut out), 1);
        let m = &out[0];
        assert!(Arc::ptr_eq(&m.mesh, &mesh) && Arc::ptr_eq(&m.material, &material));
        assert_eq!(m.layer, 3);
        assert_eq!(m.model, model.to_cols_array_2d());
        assert_eq!(m.shadow, shadow);
        assert_eq!(m.joints.len(), 1);
        assert_eq!(m.joints[0].m, JointMatrix::translation(0.0, 1.0, 0.0).m);
        assert_eq!(m.decal_layer, 4);
        assert!(!m.gi_proxy && m.is_static);
        assert!(m.uv1.as_ref().is_some_and(|u| Arc::ptr_eq(u, &uv1)));
        assert_eq!(
            (m.lod_key, m.lod_dither, m.draw_indices.is_none()),
            (0, 0.0, true)
        );
        assert_eq!(list.stats().skinned, 1);
    }
}
