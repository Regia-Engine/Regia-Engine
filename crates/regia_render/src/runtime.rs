// SPDX-License-Identifier: MPL-2.0
//! ECS runtime path (`specs/13` §§ 4–5, 9): extract render components each frame
//! into an [`ExtractedFrame`], and render the same pass sequence as the golden
//! harness (prepass → clusters → shadows → forward → IBL → post) into window
//! surfaces and offscreen targets.
//!
//! Components:
//! - [`Camera`] (+ `regia_scene::GlobalTransform`) — every camera entity is a
//!   view: [`RenderTarget::Window`] cameras draw into the primary window's
//!   viewport rect (split-screen), [`RenderTarget::Texture`] cameras draw into a
//!   persistent offscreen texture owned by the renderer. `exposure_ev` pins the
//!   view's exposure; `order` sorts views on the same target.
//! - [`MeshRenderer`] — shared CPU mesh + material; the GPU upload is cached by
//!   the mesh's generation id ([`mesh_key`]) and the material `Arc`, so meshes and
//!   materials upload once. A same-sized mesh edited through [`with_unique_mesh_mut`]
//!   keeps that id; [`note_mesh_content_changed`] rewrites the resident buffers.
//! - [`SkinJoints`] — the joint palette for a skinned [`MeshRenderer`]. A game
//!   system copies `regia_animation::SkinPalette` into it each frame (the
//!   layering rules keep `regia_animation` from depending on
//!   `regia_render`, so the renderer reads this render-owned mirror instead).
//! - [`DirectionalLight`], [`PointLight`], [`SpotLight`] (+ `GlobalTransform`)
//!   — punctual lights follow the engine conventions: positions come from the
//!   entity transform, the spot's aim and the sun's travel direction come from
//!   the component's direction fields.
//! - [`InteriorVolume`] — an enclosed region. A camera whose eye is inside one
//!   drops sky IBL and uses that volume's neutral ambient illuminance.
//! - [`crate::EnclosureScene`] — an SDF shell. When no volume contains the eye,
//!   [`crate::detect_enclosure`] on this shell selects the same shade (ADR-034).
//!
//! [`Environment`] is a resource; when absent, rendering uses its default.
//! [`RenderSettings`] carries the active [`RenderProfile`] (`specs/13` § 3):
//! CSM resolution/cascades, shadow atlas size, MSAA and the post toggles follow
//! the profile, with the desktop preset as the default. The golden/lab harness
//! pins its own baseline through [`spawn_lab_scene`], so blessed images stay
//! reproducible while games get the profile table's values.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_scene::layers::LayerParked;
use regia_scene::{GlobalTransform, Transform};

use crate::frame::{FrameMesh, FrameRenderer, FrameScene, FrameTarget};
use crate::interior::{EnclosureScene, InteriorVolume, shade_for_view};
use crate::lab_frame::{LabLocalLight, LabMaterial, LabPointLight, LabSpotLight, LabVertex};
use crate::lights::{DirectionalLight, PointLight, SpotLight};
use crate::profile::{FrameQuality, RenderProfile, RenderProfilePreset};
use crate::sdf::SdfScene;
use crate::skin::JointMatrix;
use crate::surface_weather::SurfaceWeatherFrame;
use crate::{Camera, PostSettings, RenderDevice, RenderError, RenderTarget};

/// CPU-side mesh shared by any number of [`MeshRenderer`] components.
///
/// Held in an `Arc`: the renderer caches GPU uploads by a generation id given to each
/// distinct `Arc` the first time it is drawn ([`mesh_key`]), so identical `Arc`s mean
/// "already uploaded". Treat the contents as immutable while the `Arc` is shared. The
/// registry's weak handle blocks [`Arc::get_mut`] after [`mesh_key`]; [`with_unique_mesh_mut`]
/// edits the unique strong owner in place and keeps the id. That edit must call
/// [`note_mesh_content_changed`] so a same-sized resident mesh is rewritten. A mesh that
/// rewrites every frame while the extracted frame can still hold the previous edit (a cloth
/// section) keeps two `Arc`s and gives the new one the same id with [`alias_mesh_key`]
/// before writing it through [`with_unique_mesh_mut`]; both edits land in one resident
/// buffer. Replacing the `Arc` (asset reload) still uploads as a new mesh. Once the last
/// clone of an `Arc` drops, its id is retired and the renderer frees the mesh's GPU range at
/// the next frame; a new `Arc` never takes an old id, even at the same address, unless
/// [`alias_mesh_key`] said it shares one.
#[derive(Debug)]
pub struct CpuMesh {
    /// Vertices (object space).
    pub vertices: Vec<LabVertex>,
    /// Triangle-list indices.
    pub indices: Vec<u32>,
}

/// CPU-side material shared by any number of [`MeshRenderer`] components.
pub type CpuMaterial = LabMaterial;

/// Top byte of the mesh cache keys [`mesh_key`] gives out (`'M'`): clear of
/// [`crate::EPHEMERAL_KEY`] (`0xFE`) and [`crate::SHADOW_LOD_MESH_TAG`] (`0xFF`). The id sits in
/// bits 3..56, so the LOD bits (1–2) of [`crate::mesh_lod_cache_key`] never collide with
/// another mesh's key.
pub const MESH_ID_KEY_TAG: u64 = 0x4D00_0000_0000_0000;

/// Ids above this would reach the tag byte (2^53 meshes: never in practice).
const MESH_ID_MAX: u64 = (1 << 53) - 1;

/// Retired keys kept for [`ExtractedFrame::released_mesh_keys`] when no runtime frame drains
/// them (a host that draws through [`ExtractedFrame::frame_scene`] alone). Beyond this the
/// oldest are dropped: the renderer's idle eviction frees those entries instead.
const RELEASED_KEYS_CAP: usize = 1 << 16;

/// Process-wide mesh identity: one generation id per live `Arc<CpuMesh>`, keyed by the `Arc`
/// address. Each entry holds a `Weak`, which keeps the `Arc`'s allocation (not its vertex
/// data) alive, so no other `Arc` can be allocated at that address while the entry exists:
/// an address can never name two meshes (the ABA hole of address keys, ADR-179's class).
/// [`MeshRegistry::alias`] lets a second address share an id (a cloth section's alternating
/// render buffers); the id is retired only when every aliased address is gone.
#[derive(Default)]
struct MeshRegistry {
    by_addr: std::collections::HashMap<usize, (u64, std::sync::Weak<CpuMesh>)>,
    /// Generation ids whose registry `Weak` is parked so a unique owner can edit in place.
    /// [`with_unique_mesh_mut`] drops the weak (it blocks [`Arc::get_mut`]) and records the
    /// id here until the edit returns the same `Arc`.
    parked: std::collections::HashMap<usize, u64>,
    next_id: u64,
    /// Live entries after the last sweep (the next sweep runs once the map doubles).
    swept_len: usize,
    /// Keys of meshes whose last `Arc` dropped, not yet handed to a frame.
    released: Vec<u64>,
    /// Stand-ins of counted meshes drawn from their upload ([`counted_mesh`], ADR-7330), by `Arc`
    /// address: the frame key is the asset's ([`crate::asset_mesh_key`]), and the entry keeps the
    /// asset loaded while a clone of the stand-in lives.
    counted: std::collections::HashMap<usize, CountedStandIn>,
    /// The live stand-in of each counted asset (its `Arc` address in [`Self::counted`]).
    counted_ids: std::collections::HashMap<regia_asset::AssetId, usize>,
    /// Handles of stand-ins that died, dropped by [`take_released_mesh_keys`] once the registry
    /// lock is released (a handle drop reaches the asset server's tables).
    dead_handles: Vec<regia_asset::StrongHandle<regia_asset::CpuMesh>>,
}

/// One [`counted_mesh`] stand-in.
struct CountedStandIn {
    key: u64,
    weak: std::sync::Weak<CpuMesh>,
    handle: regia_asset::StrongHandle<regia_asset::CpuMesh>,
}

impl MeshRegistry {
    fn key(&mut self, mesh: &Arc<CpuMesh>) -> u64 {
        let addr = Arc::as_ptr(mesh) as usize;
        if !self.counted.is_empty()
            && let Some(stand_in) = self.counted.get(&addr)
            && stand_in.weak.strong_count() > 0
        {
            return stand_in.key;
        }
        if let Some(&id) = self.parked.get(&addr) {
            return MESH_ID_KEY_TAG | (id << 3);
        }
        if let Some((id, weak)) = self.by_addr.get(&addr)
            && weak.strong_count() > 0
        {
            return MESH_ID_KEY_TAG | (id << 3);
        }
        // A dead entry cannot share an address with a live `Arc` (its `Weak` pins the
        // allocation); retire it anyway rather than hand out its id. An id another address
        // still aliases stays.
        if let Some((id, _)) = self.by_addr.remove(&addr) {
            self.retire_if_unaliased(id);
        }
        if self.by_addr.len() >= (self.swept_len * 2).max(1024) {
            self.sweep();
        }
        self.next_id = (self.next_id + 1).min(MESH_ID_MAX);
        let id = self.next_id;
        self.by_addr.insert(addr, (id, Arc::downgrade(mesh)));
        MESH_ID_KEY_TAG | (id << 3)
    }

    /// Points `mesh` at an existing id (`key` from `mesh_key`). Call this before `key` on
    /// `mesh`, or that call has already assigned a different id. Idempotent when `mesh`
    /// already carries `key`. Returns false when `key` is not a mesh id, or when `mesh`
    /// already has a different live id.
    fn alias(&mut self, mesh: &Arc<CpuMesh>, key: u64) -> bool {
        let Some(id) = mesh_id_of_key(key) else {
            return false;
        };
        let addr = Arc::as_ptr(mesh) as usize;
        if let Some(&parked) = self.parked.get(&addr) {
            return parked == id;
        }
        if let Some((existing, weak)) = self.by_addr.get(&addr)
            && weak.strong_count() > 0
        {
            return *existing == id;
        }
        if let Some((old, _)) = self.by_addr.remove(&addr) {
            self.retire_if_unaliased(old);
        }
        self.by_addr.insert(addr, (id, Arc::downgrade(mesh)));
        true
    }

    fn retire(&mut self, id: u64) {
        let key = MESH_ID_KEY_TAG | (id << 3);
        content_revs().remove(&key);
        if self.released.len() >= RELEASED_KEYS_CAP {
            self.released.drain(..RELEASED_KEYS_CAP / 2);
        }
        self.released.push(key);
    }

    /// Retires `id` when no live address still carries it.
    fn retire_if_unaliased(&mut self, id: u64) {
        let live = self
            .by_addr
            .values()
            .any(|(other, weak)| *other == id && weak.strong_count() > 0);
        if !live {
            self.retire(id);
        }
    }

    /// Drops the entries whose last `Arc` is gone and queues their keys.
    fn sweep(&mut self) {
        let mut dead = Vec::new();
        self.by_addr.retain(|_, (id, weak)| {
            let live = weak.strong_count() > 0;
            if !live {
                dead.push(*id);
            }
            live
        });
        dead.sort_unstable();
        dead.dedup();
        for id in dead {
            self.retire_if_unaliased(id);
        }
        self.swept_len = self.by_addr.len();
        self.sweep_counted();
    }

    /// Drops the stand-ins whose last `Arc` is gone: their handles wait in
    /// [`Self::dead_handles`] and their keys are released (the renderer drops the adopted entry).
    fn sweep_counted(&mut self) {
        if self.counted.is_empty() {
            return;
        }
        let mut gone: Vec<usize> = Vec::new();
        for (addr, stand_in) in &self.counted {
            if stand_in.weak.strong_count() == 0 {
                gone.push(*addr);
            }
        }
        for addr in gone {
            let Some(stand_in) = self.counted.remove(&addr) else {
                continue;
            };
            let id = stand_in.handle.id();
            if self.counted_ids.get(&id) == Some(&addr) {
                self.counted_ids.remove(&id);
            } else if self.counted_ids.contains_key(&id) {
                // A newer stand-in of the same asset draws under the same key (ADR-7428):
                // releasing it would drop that stand-in's adopted entry.
                self.dead_handles.push(stand_in.handle);
                continue;
            }
            if self.released.len() >= RELEASED_KEYS_CAP {
                self.released.drain(..RELEASED_KEYS_CAP / 2);
            }
            self.released.push(stand_in.key);
            self.dead_handles.push(stand_in.handle);
        }
    }

    /// The live stand-in of counted mesh `handle`, made now when there is none.
    fn counted(&mut self, handle: regia_asset::StrongHandle<regia_asset::CpuMesh>) -> Arc<CpuMesh> {
        let id = handle.id();
        if let Some(&addr) = self.counted_ids.get(&id) {
            if let Some(live) = self.counted.get(&addr).and_then(|s| s.weak.upgrade()) {
                // `handle` drops with the caller's copy; the stand-in keeps its own.
                return live;
            }
            // The asset's last stand-in died and was not swept yet (despawned and respawned
            // between two extractions, ADR-7428): sweep it here without releasing its key, which
            // the new stand-in takes over, so the renderer keeps its adopted entry (no new mesh
            // generation, no frame without the mesh). Its handle drops with the next sweep's.
            if let Some(dead) = self.counted.remove(&addr) {
                self.dead_handles.push(dead.handle);
            }
            self.counted_ids.remove(&id);
        }
        let stand_in = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let addr = Arc::as_ptr(&stand_in) as usize;
        if let Some(old) = self.counted.insert(
            addr,
            CountedStandIn {
                key: crate::asset_mesh_key(id),
                weak: Arc::downgrade(&stand_in),
                handle,
            },
        ) {
            // A dead stand-in pins its allocation with its `Weak`, so no live `Arc` can share the
            // address; keep its handle for the deferred drop anyway.
            self.dead_handles.push(old.handle);
        }
        self.counted_ids.insert(id, addr);
        stand_in
    }

    /// The asset a counted stand-in draws.
    fn counted_id(&self, mesh: &Arc<CpuMesh>) -> Option<regia_asset::AssetId> {
        if self.counted.is_empty() {
            return None;
        }
        let stand_in = self.counted.get(&(Arc::as_ptr(mesh) as usize))?;
        (stand_in.weak.strong_count() > 0).then(|| stand_in.handle.id())
    }
}

/// The id packed in a [`mesh_key`], when `key` carries [`MESH_ID_KEY_TAG`].
fn mesh_id_of_key(key: u64) -> Option<u64> {
    if key >> 56 != MESH_ID_KEY_TAG >> 56 {
        return None;
    }
    let id = (key ^ MESH_ID_KEY_TAG) >> 3;
    (id > 0 && id <= MESH_ID_MAX).then_some(id)
}

/// Top byte of the material cache keys [`material_key`] gives out (`'L'`, for `LabMaterial`):
/// clear of the small integers and `Arc` addresses hosts key their own materials with.
pub(crate) const MATERIAL_ID_KEY_TAG: u64 = 0x4C00_0000_0000_0000;

/// Ids above this would reach the tag byte.
const MATERIAL_ID_MAX: u64 = (1 << 56) - 1;

/// Process-wide material identity (audit G render5 #1): one generation id per live
/// `Arc<CpuMaterial>`, keyed by the `Arc` address, as [`MeshRegistry`] does for meshes. The
/// renderer keeps a material's textures by its key, so an address key let a new material
/// allocated where a dropped one lived draw with the old one's maps. Each entry's `Weak` pins the
/// allocation, so no other material can take the address while the entry exists, and ids are
/// never reused. A material's GPU entry is freed by the renderer's idle eviction, so no released
/// keys are queued.
#[derive(Default)]
struct MaterialRegistry {
    by_addr: std::collections::HashMap<usize, (u64, std::sync::Weak<CpuMaterial>)>,
    next_id: u64,
    /// Live entries after the last sweep (the next sweep runs once the map doubles).
    swept_len: usize,
}

impl MaterialRegistry {
    fn key(&mut self, material: &Arc<CpuMaterial>) -> u64 {
        let addr = Arc::as_ptr(material) as usize;
        if let Some((id, weak)) = self.by_addr.get(&addr)
            && weak.strong_count() > 0
        {
            return MATERIAL_ID_KEY_TAG | id;
        }
        if self.by_addr.len() >= (self.swept_len * 2).max(1024) {
            self.by_addr.retain(|_, (_, weak)| weak.strong_count() > 0);
            self.swept_len = self.by_addr.len();
        }
        self.next_id = (self.next_id + 1).min(MATERIAL_ID_MAX);
        let id = self.next_id;
        self.by_addr.insert(addr, (id, Arc::downgrade(material)));
        MATERIAL_ID_KEY_TAG | id
    }
}

fn material_registry() -> std::sync::MutexGuard<'static, MaterialRegistry> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<MaterialRegistry>> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The renderer's cache key for `material` ([`crate::FrameMesh::material_key`]) on the ECS path:
/// a generation id given to this `Arc` the first time it is asked for, the same for every clone
/// of it, never reused by another `Arc` (`crate::shading_model::material_key` is this key).
pub(crate) fn material_key(material: &Arc<CpuMaterial>) -> u64 {
    material_registry().key(material)
}

/// Weak ownership proof for a registered material generation (ADR-7869). A borrowed address
/// or a copied host key alone cannot promise that public pixel vectors remain immutable.
pub(crate) fn material_identity(key: u64, material: &CpuMaterial) -> Option<std::sync::Weak<CpuMaterial>> {
    if key & !MATERIAL_ID_MAX != MATERIAL_ID_KEY_TAG {
        return None;
    }
    let registry = material_registry();
    let (id, owner) = registry.by_addr.get(&(material as *const CpuMaterial as usize))?;
    ((MATERIAL_ID_KEY_TAG | *id) == key && owner.strong_count() > 0).then(|| owner.clone())
}

/// True when a mesh on `layer` is drawn by a camera with `layer_mask`; layers above 31 have
/// no mask bit and are never visible.
fn layer_visible(layer_mask: u32, layer: u32) -> bool {
    layer < 32 && layer_mask & (1 << layer) != 0
}

fn mesh_registry() -> std::sync::MutexGuard<'static, MeshRegistry> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<MeshRegistry>> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The renderer's cache key for `mesh` ([`crate::FrameMesh::mesh_key`]): a generation id given
/// to this `Arc` the first time it is asked for (with [`MESH_ID_KEY_TAG`] on top), the same for
/// every clone of it, never reused by another `Arc`. Hosts that build [`crate::FrameMesh`]es
/// from `Arc<CpuMesh>` themselves can use it instead of the `Arc` address; [`ExtractedFrame`]
/// uses it for every [`MeshRenderer`].
#[must_use]
pub fn mesh_key(mesh: &Arc<CpuMesh>) -> u64 {
    mesh_registry().key(mesh)
}

/// Makes `mesh` share the cache id in `key` (from [`mesh_key`] of another `Arc`).
///
/// Call it before [`mesh_key`] on `mesh`. After that, both addresses draw as one mesh: one
/// resident upload, and [`note_mesh_content_changed`] on either bumps the one content
/// revision, so the cache rewrites the resident buffers in place while the byte size holds.
/// The id is retired when every aliased `Arc` has dropped. Returns false when `key` is not a
/// mesh id, or when `mesh` already has a different live id.
#[must_use]
pub fn alias_mesh_key(mesh: &Arc<CpuMesh>, key: u64) -> bool {
    mesh_registry().alias(mesh, key)
}

fn content_revs() -> std::sync::MutexGuard<'static, std::collections::HashMap<u64, u64>> {
    static REVS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, u64>>> =
        std::sync::OnceLock::new();
    REVS.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How a resident mesh should pick up bytes that changed under a stable [`mesh_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshRefresh {
    /// The byte size changed, or the mesh is not resident: allocate and upload.
    Upload,
    /// The size matches and the content revision moved: rewrite the resident buffers.
    Rewrite,
    /// Resident, and the content revision matches what those buffers hold.
    Keep,
}

/// Decides an upload from the size change and the content revision.
///
/// `size_changed` is true when the mesh is not resident or its vertex or index byte count
/// differs. `content_rev` is [`mesh_content_revision`]; `applied_rev` is the revision last
/// written into the resident buffers.
#[must_use]
pub fn mesh_refresh(size_changed: bool, content_rev: u64, applied_rev: u64) -> MeshRefresh {
    if size_changed {
        MeshRefresh::Upload
    } else if content_rev != applied_rev {
        MeshRefresh::Rewrite
    } else {
        MeshRefresh::Keep
    }
}

/// Records that `mesh`'s bytes changed while this `Arc` stayed put.
///
/// [`mesh_key`] does not change. A same-sized resident mesh is rewritten in place on the next
/// draw; the rewrite does not count as a new upload.
pub fn note_mesh_content_changed(mesh: &std::sync::Arc<CpuMesh>) {
    let key = mesh_key(mesh);
    let mut revs = content_revs();
    let rev = revs.entry(key).or_insert(0);
    *rev = rev.saturating_add(1);
}

/// Edits `mesh` in place when this task holds the only strong [`Arc`].
///
/// The mesh registry keeps a [`std::sync::Weak`] so an address cannot name two meshes. That
/// weak makes [`Arc::get_mut`] return `None` even when the component is the unique strong
/// owner. Parking the weak for the edit, then putting the same generation id back, keeps
/// [`mesh_key`] stable. [`note_mesh_content_changed`] still has to run so a resident GPU
/// buffer is rewritten.
///
/// Returns `None` when another strong [`Arc`], or a weak this registry does not own, still
/// shares the allocation. The caller allocates a new mesh in that case.
pub fn with_unique_mesh_mut<R>(mesh: &mut Arc<CpuMesh>, edit: impl FnOnce(&mut CpuMesh) -> R) -> Option<R> {
    if Arc::strong_count(mesh) != 1 {
        return None;
    }
    let addr = Arc::as_ptr(mesh) as usize;
    let parked_id = {
        let mut registry = mesh_registry();
        let found = match registry.by_addr.get(&addr) {
            Some((id, weak)) if weak.upgrade().is_some_and(|live| Arc::ptr_eq(&live, mesh)) => Some(*id),
            Some(_) => return None,
            None => None,
        };
        if let Some(id) = found {
            registry.by_addr.remove(&addr);
            registry.parked.insert(addr, id);
            Some(id)
        } else {
            None
        }
    };
    let edited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Arc::get_mut(mesh).map(edit)));
    if let Some(id) = parked_id {
        let mut registry = mesh_registry();
        let addr = Arc::as_ptr(mesh) as usize;
        registry.parked.remove(&addr);
        registry.by_addr.insert(addr, (id, Arc::downgrade(mesh)));
    }
    match edited {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Content revision of `key` (`0` until [`note_mesh_content_changed`] runs for that mesh).
/// A LOD, shadow-LOD or dithered variant of a mesh's key (`mesh_lod::lod_variant_key`) reads
/// its mesh's revision: those draws upload the same edited mesh (corr-render-rest LOW 17).
#[must_use]
pub fn mesh_content_revision(key: u64) -> u64 {
    content_revs().get(&content_base_key(key)).copied().unwrap_or(0)
}

/// The mesh key a variant key was derived from. A dithered copy xors
/// `frame::LOD_DITHER_MESH_TAG` (bit 0 set; mesh keys keep bit 0 clear); a shadow-LOD variant
/// overwrites the tag byte with `0xFF`; a LOD variant sets bits 1-2, which a generation key
/// ([`MESH_ID_KEY_TAG`]) keeps clear. Any other key is its own base.
fn content_base_key(key: u64) -> u64 {
    let key = if key & 1 == 1 {
        key ^ crate::frame::LOD_DITHER_MESH_TAG
    } else {
        key
    };
    let key = if crate::frame::is_shadow_lod_key(key) {
        (key & !(0xFF << 56)) | MESH_ID_KEY_TAG
    } else {
        key
    };
    if key >> 56 == MESH_ID_KEY_TAG >> 56 {
        key & !0b110
    } else {
        key
    }
}

/// Retires the ids of meshes whose last `Arc` dropped and returns their keys (each once,
/// process-wide). [`extract_frame`] calls it into [`ExtractedFrame::released_mesh_keys`].
#[must_use]
pub fn take_released_mesh_keys() -> Vec<u64> {
    let mut registry = mesh_registry();
    registry.sweep();
    let released = std::mem::take(&mut registry.released);
    let dead = if registry.dead_handles.is_empty() {
        Vec::new()
    } else {
        std::mem::take(&mut registry.dead_handles)
    };
    drop(registry);
    // Outside the lock: the last handle of an asset starts its unload grace.
    drop(dead);
    released
}

/// A stand-in `Arc<CpuMesh>` for counted mesh `handle` (ADR-7330): it holds no vertices, its
/// [`mesh_key`] is the asset's [`crate::asset_mesh_key`], and a [`MeshRenderer`] that holds it
/// draws the GPU buffers [`crate::AssetMeshUploads`] uploaded for the asset (no CPU copy; the
/// asset server owns and counts them). The stand-in keeps the asset loaded while any clone of it
/// lives; once the last clone drops, the next [`extract_frame`] releases its key and the handle,
/// and the asset unloads after the server's grace frames. Every call for the same live asset
/// returns the same `Arc`.
///
/// The App needs [`crate::CountedMeshPlugin`] (or a [`crate::FrameRenderer`] given the uploader
/// with [`crate::FrameRenderer::set_asset_meshes`] and an `AssetServer` it is registered with)
/// before the asset loads. A skinned upload skins from it (ADR-8610), and a
/// [`crate::MeshLodChain`] (`MeshLodChain::from_cooked_asset`) draws each level from it
/// (ADR-7428); morph targets deform a GPU copy of it with the targets its upload packed
/// ([`crate::AssetMeshUploads::morph_targets`], ADR-7721); a
/// [`MeshUv1`] second UV set with one entry per uploaded vertex is drawn (lightmap receivers,
/// graphs that read `TexCoord1`).
#[must_use]
pub fn counted_mesh(handle: regia_asset::StrongHandle<regia_asset::CpuMesh>) -> Arc<CpuMesh> {
    mesh_registry().counted(handle)
}

/// The counted asset a [`counted_mesh`] stand-in draws (`None` for any other mesh).
#[must_use]
pub fn counted_mesh_id(mesh: &Arc<CpuMesh>) -> Option<regia_asset::AssetId> {
    mesh_registry().counted_id(mesh)
}

/// Renders a shared mesh with a shared material at the entity's transform.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use regia_render::{CpuMaterial, CpuMesh, MeshRenderer};
///
/// let mesh = Arc::new(CpuMesh { vertices: vec![], indices: vec![] });
/// let material = Arc::new(CpuMaterial::default());
/// let renderer = MeshRenderer::new(mesh, material);
/// assert_eq!(renderer.layer, 0); // visible to every camera
/// assert!(renderer.gi_proxy); // a GI proxy unless the host opts out
/// ```
#[derive(Component, Clone, Debug)]
#[require(
    regia_scene::components::InheritedVisibility,
    regia_scene::components::ViewVisibility
)]
pub struct MeshRenderer {
    /// Shared mesh (GPU upload cached by `Arc` identity).
    pub mesh: Arc<CpuMesh>,
    /// Shared material (GPU upload cached by `Arc` identity).
    pub material: Arc<CpuMaterial>,
    /// Render layer (`0..=31`): visible to cameras whose `layer_mask` has this bit set. Values
    /// above 31 are visible to no camera.
    pub layer: u32,
    /// Decal receiver layer. `0` stamps a rigid mesh as static and a skinned mesh as a
    /// character. Any other value is the layer index, so a rigid mesh can be masked.
    pub decal_layer: u32,
    /// When false, this mesh is not a GI proxy: no signed-distance brick is baked for it
    /// and the probes do not trace it. Shadows stay as they are ([`NotShadowCaster`] still
    /// owns those). Default true, including [`Self::new`]. A brick is stored per mesh, so
    /// any instance with this false opts that mesh out.
    pub gi_proxy: bool,
}

impl MeshRenderer {
    /// A mesh + material visible to every camera (layer 0, the default mask).
    #[must_use]
    pub fn new(mesh: Arc<CpuMesh>, material: Arc<CpuMaterial>) -> Self {
        Self {
            mesh,
            material,
            layer: 0,
            decal_layer: 0,
            gi_proxy: true,
        }
    }

    /// A counted mesh drawn from its GPU upload ([`counted_mesh`], ADR-7330): no CPU vertices
    /// in the component or the extracted frame.
    #[must_use]
    pub fn counted(
        mesh: regia_asset::StrongHandle<regia_asset::CpuMesh>,
        material: Arc<CpuMaterial>,
    ) -> Self {
        Self::new(counted_mesh(mesh), material)
    }
}

/// Marks a [`MeshRenderer`] entity that casts no shadow. Extraction maps it to
/// [`crate::ShadowCast::Off`]: the mesh is drawn into no shadow view (no sun cascade, no
/// local-light tile) but still receives shadows. Use it for a huge opaque mesh around the camera
/// (a host sky dome or backdrop shell) that would otherwise shadow the whole map. Without it a
/// mesh casts [`crate::ShadowCast::All`], exactly as before this component existed.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct NotShadowCaster;

/// Which shadow views a [`MeshRenderer`] entity is drawn into, when it is not the default
/// [`crate::ShadowCast::All`] (ADR-3253): HLOD proxies cast with [`crate::ShadowCast::Local`] so they
/// reach only the far sun cascades their distance picks. [`NotShadowCaster`] wins over it, and an
/// outgoing LOD of a cross-fade still casts nothing. Without it a mesh casts as before.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct MeshShadowCast(pub crate::ShadowCast);

/// Dithered cross-fade of one LOD switch, for a host that picks its own LODs. Optional and
/// additive: an entity without it is drawn whole, bit-identical to before the component existed.
///
/// A switch uses two entities, one per LOD, with opposite signs of the same `c` in `(0, 1)`:
/// the **incoming** LOD gets `MeshLodFade(c)` and keeps the share `c` of the screen pixels,
/// the **outgoing** LOD gets `MeshLodFade(-c)` and keeps the other `1 - c`, so together they
/// cover every pixel exactly once. Ramp `c` from 0 to 1 over the fade and then drop the
/// component (and the outgoing entity). `0` is drawn whole. TAA averages the dither into a
/// blend.
///
/// Shadows: the incoming LOD (`c > 0`) casts the shadow for the whole fade and the outgoing
/// LOD (`c < 0`) casts none, so the shadow never doubles or pops. [`NotShadowCaster`] still
/// turns the shadow off for both.
///
/// A dithered mesh draws through the alpha-test pipeline under its own batch.
///
/// # Example
///
/// ```
/// use regia_render::MeshLodFade;
///
/// let c = 0.25;
/// let (incoming, outgoing) = (MeshLodFade(c), MeshLodFade(-c));
/// assert_eq!(incoming.0, -outgoing.0);
/// ```
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct MeshLodFade(pub f32);

/// Second UV set of the entity's [`MeshRenderer`] mesh, one per vertex. Material graphs that
/// read `TexCoord1` sample it; without it they get the first set.
#[derive(Component, Clone, Debug)]
pub struct MeshUv1(pub Arc<Vec<[f32; 2]>>);

/// A level-serializable mesh reference: what a `.level.ron` / `.prefab.ron` stores for a
/// placed mesh (`specs/14` A2). [`MeshRenderer`] holds the loaded data and is never saved;
/// a host resolves this component into one (the editor's resolver, its `mesh_source` module, loads
/// source assets). Paths are project-relative.
///
/// # Example
///
/// ```
/// use regia_render::MeshSource;
///
/// let crate_mesh = MeshSource::new("assets/props/crate.gltf", "assets/materials/crate.material.ron");
/// assert!(!crate_mesh.is_builtin());
/// assert!(MeshSource::new(MeshSource::BUILTIN_CUBE, "").is_builtin());
/// ```
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default, Clone)]
pub struct MeshSource {
    /// The mesh: a glTF/GLB path under `assets/`, or a built-in primitive
    /// ([`Self::BUILTIN_CUBE`], [`Self::BUILTIN_PLANE`], [`Self::BUILTIN_SPHERE`]).
    #[reflect(@regia_ecs::attrs::AssetFilter("mesh"))]
    pub mesh: String,
    /// The material: a `.material.ron`, `.matgraph.ron` or `.matinst.ron` path. Empty keeps
    /// the mesh's own materials (glTF) or the default material (built-ins).
    #[reflect(@regia_ecs::attrs::AssetFilter("material"))]
    pub material: String,
}

impl MeshSource {
    /// A unit cube (±0.5 m), scaled by the entity transform.
    pub const BUILTIN_CUBE: &'static str = "builtin:cube";
    /// A 1 m × 1 m plane on XZ facing +Y.
    pub const BUILTIN_PLANE: &'static str = "builtin:plane";
    /// A sphere of radius 0.5 m.
    pub const BUILTIN_SPHERE: &'static str = "builtin:sphere";

    /// A mesh + material reference.
    #[must_use]
    pub fn new(mesh: impl Into<String>, material: impl Into<String>) -> Self {
        Self {
            mesh: mesh.into(),
            material: material.into(),
        }
    }

    /// Whether [`Self::mesh`] names a built-in primitive rather than a file.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.mesh.starts_with("builtin:")
    }
}

/// Registers the render components that levels and prefabs store (cameras, lights,
/// [`MeshSource`]) with the app's type registry, so scene files can save and load them.
/// [`crate::RenderPlugin`] calls it; hosts without the render plugin (the editor's edit world,
/// play-in-editor) call it directly.
pub fn register_render_types(app: &mut App) {
    app.register_type::<MeshSource>()
        .register_type::<MeshLodFade>()
        // Lightmap receivers are level content (ADR-2313, scene format v11).
        .register_type::<crate::lightmap::LightmapReceiver>()
        .register_type::<crate::Camera>()
        .register_type::<crate::RenderTarget>()
        .register_type::<crate::ClearMode>()
        .register_type::<DirectionalLight>()
        // Ambient fill boxes (ADR-7211).
        .register_type::<crate::AmbientVolume>()
        .register_type::<PointLight>()
        .register_type::<SpotLight>()
        .register_type::<crate::LightSourceSize>()
        // IES profiles and authored area lights (ADR-2592).
        .register_type::<crate::LightProfile>()
        .register_type::<crate::LightSourceLtc>()
        .register_type::<crate::RectLight>()
        .register_type::<crate::TubeLight>()
        // Post volumes (ADR-1464) and the types they hold (`post_volumes::register_post_volume_types`).
        .register_type::<crate::PostVolume>()
        .register_type::<crate::PostVolumeShape>()
        .register_type::<crate::PostOverrides>()
        .register_type::<crate::EyeRange>()
        .register_type::<crate::TonemapOperator>()
        // Post-volume format v3 (ADR-2323): hull planes and the LUT path.
        .register_type::<Vec<[f32; 4]>>()
        .register_type::<[f32; 4]>()
        .register_type::<Option<String>>()
        // The physical camera in scene files (ADR-2323).
        .register_type::<crate::PhysicalCamera>()
        .register_type::<crate::CameraExposureMode>()
        .register_type::<crate::DepthOfFieldMode>()
        // Lens effects in scene files (ADR-3196).
        .register_type::<crate::LensEffects>()
        // Local reflection captures are level content (ADR-2860); their bake is the level's `.fcap`.
        .register_type::<crate::LocalCapture>()
        .register_type::<[f32; 3]>();
}

/// Joint palette of a skinned mesh in world space, written each frame by the
/// animation bridge (see the module docs).
#[derive(Component, Clone, Debug, Default)]
pub struct SkinJoints {
    /// One matrix per joint, joint-id order.
    pub palette: Vec<JointMatrix>,
}

/// Scene environment: ambient light, IBL and winding behaviour for the frame.
///
/// # Example
///
/// ```
/// use regia_render::Environment;
///
/// let env = Environment::default();
/// assert!(env.ibl); // procedural sky IBL on by default
/// assert_eq!(env.ambient_lux, 12_000.0); // flat ambient when IBL is off
/// ```
#[derive(Resource, Debug, Clone)]
pub struct Environment {
    /// Image-based lighting from the procedural sky.
    pub ibl: bool,
    /// Flat ambient illuminance in lux (used only when `ibl` is off). The default (12 000) is an
    /// open-daylight lux scale — clear-sky daylight is roughly 10 000–15 000 lux — chosen so the
    /// flat fallback visually matches the procedural sky's irradiance.
    pub ambient_lux: f32,
    /// Render both triangle faces (glTF doubleSided).
    pub double_sided: bool,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            ibl: true,
            ambient_lux: 12_000.0,
            double_sided: false,
        }
    }
}

/// Size of `RenderTarget::Texture` cameras' textures (they have no asset-backed
/// extent until texture handles are wired, `specs/13` § 9).
pub const TEXTURE_TARGET_SIZE: [u32; 2] = [512, 512];

/// Global render settings for the runtime path.
///
/// The profile (`specs/13` § 3) drives CSM resolution/cascade count, the local
/// shadow atlas, MSAA, the depth prepass and the post toggles (bloom, AA,
/// auto exposure); `post` carries the remaining user-facing settings.
#[derive(Resource, Debug, Clone)]
pub struct RenderSettings {
    /// Active render profile ([`RenderProfilePreset::runtime_default`] by default: Deck on a
    /// Steam Deck, Mobile High on Android, Desktop High elsewhere).
    pub profile: RenderProfile,
    /// Post stack base settings; the profile toggles bloom/AA/auto exposure.
    pub post: PostSettings,
    /// Distance + exponential height fog.
    pub fog: crate::FogSettings,
    /// The player's lighting option ([`crate::GiLook`]); `None` keeps the profile's default.
    pub gi_look: Option<crate::GiLook>,
    /// The player's dynamic GI tier ([`crate::DynamicGiTier`], `specs/79`): how large and how
    /// fast the probes are when the lighting option runs them. `None` keeps the profile's
    /// [`RenderProfile::gi_tier`].
    pub dynamic_gi_tier: Option<crate::DynamicGiTier>,
    /// The world-space radiance cache behind screen-space GI (`specs/79` § 12, ADR-2085): `Some`
    /// forces it on or off, `None` keeps the profile's [`RenderProfile::radiance_cache`].
    pub radiance_cache: Option<bool>,
    /// The two-phase occlusion cull of the depth prepass (`specs/98`, ADR-1887): `Some` forces
    /// it on or off, `None` keeps the profile's [`RenderProfile::two_phase_occlusion`].
    pub two_phase_occlusion: Option<bool>,
    /// The two-phase occlusion cull of the shadow casters (ADR-2072, ADR-2482): `Some` forces it
    /// on or off, `None` keeps the profile's [`RenderProfile::shadow_occlusion`] (off in every
    /// preset).
    pub shadow_occlusion: Option<bool>,
    /// The dense-scene default (ADR-3149): [`crate::VisibilityDefault::Dense`] routes dense
    /// scenes of a Desktop High or Ultra profile to the visibility buffer
    /// ([`crate::VisibilityPath::DENSE_DEFAULT`]). Off in every preset. Change it with
    /// [`Self::set_visibility_default`], which writes the profile's path columns; writing this
    /// field alone takes effect at the next [`Self::apply_preset`].
    pub visibility_default: crate::VisibilityDefault,
}

impl Default for RenderSettings {
    /// [`RenderProfilePreset::runtime_default`]: `Deck` on a Steam Deck, else the platform default.
    fn default() -> Self {
        Self::from_profile(RenderProfilePreset::runtime_default())
    }
}

impl RenderSettings {
    /// Settings for a named preset; the post stack is seeded from the profile's
    /// bloom/AA/auto-exposure columns. Fog is the default look's aerial perspective
    /// ([`crate::FogSettings::aerial`]); set `fog.enabled = false` for none.
    #[must_use]
    pub fn from_profile(preset: RenderProfilePreset) -> Self {
        let profile = RenderProfile::from_preset(preset);
        let post = PostSettings {
            bloom: profile.bloom,
            bloom_mips: profile.bloom_mip_count.clamp(1, 5),
            auto_exposure: profile.auto_exposure,
            fxaa: matches!(profile.anti_aliasing, crate::profile::AntiAliasing::Fxaa),
            taa: matches!(profile.anti_aliasing, crate::profile::AntiAliasing::Taa),
            ..PostSettings::default()
        };
        Self {
            profile,
            post,
            fog: crate::FogSettings::aerial(),
            gi_look: None,
            dynamic_gi_tier: None,
            radiance_cache: None,
            two_phase_occlusion: None,
            shadow_occlusion: None,
            visibility_default: crate::VisibilityDefault::Preset,
        }
    }

    /// The profile's post toggles applied over the user's post settings: the
    /// profile is authoritative for what runs, the user settings keep the rest.
    #[must_use]
    pub fn resolve_post(&self) -> PostSettings {
        let mut post = self.post.clone();
        if !self.profile.bloom {
            post.bloom = false;
        } else if post.bloom {
            post.bloom_mips = post.bloom_mips.clamp(1, self.profile.bloom_mip_count.max(1));
        }
        post.fxaa = matches!(self.profile.anti_aliasing, crate::profile::AntiAliasing::Fxaa);
        post.taa = matches!(self.profile.anti_aliasing, crate::profile::AntiAliasing::Taa);
        post.auto_exposure &= self.profile.auto_exposure;
        post
    }

    /// The lighting option in effect: the player's choice, else the profile's default.
    #[must_use]
    pub fn effective_gi_look(&self) -> crate::GiLook {
        self.gi_look
            .unwrap_or_else(|| crate::GiLook::of_profile(&self.profile))
    }

    /// The dynamic GI tier in effect: the player's choice, else the profile's column.
    #[must_use]
    pub fn effective_dynamic_gi_tier(&self) -> crate::DynamicGiTier {
        self.dynamic_gi_tier.unwrap_or(self.profile.gi_tier)
    }

    /// Whether screen-space GI reads the radiance cache: the override, else the profile's column.
    #[must_use]
    pub fn effective_radiance_cache(&self) -> bool {
        self.radiance_cache.unwrap_or(self.profile.radiance_cache)
    }

    /// Whether the shadow casters cull in two phases: the override, else the profile's column.
    #[must_use]
    pub fn effective_shadow_occlusion(&self) -> bool {
        self.shadow_occlusion.unwrap_or(self.profile.shadow_occlusion)
    }

    /// Whether the depth prepass culls in two phases: the override, else the profile's column.
    #[must_use]
    pub fn effective_two_phase_occlusion(&self) -> bool {
        self.two_phase_occlusion
            .unwrap_or(self.profile.two_phase_occlusion)
    }

    /// Switches to `preset` live: the profile, and the post toggles it seeds (bloom, AA, auto
    /// exposure) as [`RenderSettings::from_profile`] does. The rest of the post stack (enabled,
    /// exposure, grading) and the fog keep their values.
    pub fn apply_preset(&mut self, preset: RenderProfilePreset) {
        let seeded = Self::from_profile(preset);
        self.profile = seeded.profile;
        self.post.bloom = seeded.post.bloom;
        self.post.bloom_mips = seeded.post.bloom_mips;
        self.post.auto_exposure = seeded.post.auto_exposure;
        self.post.fxaa = seeded.post.fxaa;
        self.post.taa = seeded.post.taa;
        // The new row starts at its preset's path columns: the switch applies to it again.
        self.set_visibility_default(self.visibility_default);
    }

    /// Turns the dense-scene default (ADR-3149) on or off and returns what holds the path now.
    /// On, a Desktop High or Ultra profile whose path columns are still its preset's (the path
    /// off, [`crate::VisibilityPath::DEFAULT`]) takes [`crate::VisibilityPath::DENSE_DEFAULT`]
    /// with the path on. Off, a profile holding exactly what the default wrote goes back to the
    /// preset's columns. A path the game chose itself is never touched either way, and
    /// [`Self::preset`] answers the same before and after, so no other system changes tier.
    pub fn set_visibility_default(
        &mut self,
        default: crate::VisibilityDefault,
    ) -> crate::VisibilityDefaultState {
        let had_default = self.holds_visibility_default();
        self.visibility_default = default;
        match default {
            crate::VisibilityDefault::Dense => {
                if self.dense_default_measured() && self.at_preset_path() {
                    self.profile.visibility_buffer = true;
                    self.profile.visibility_path = crate::VisibilityPath::DENSE_DEFAULT;
                }
            }
            crate::VisibilityDefault::Preset => {
                if had_default {
                    self.profile.visibility_buffer = false;
                    self.profile.visibility_path = crate::VisibilityPath::DEFAULT;
                }
            }
        }
        self.visibility_default_state()
    }

    /// Takes back the path the dense-scene default wrote once the profile is no longer a row it
    /// was measured on: a tier that writes its own columns over a preset ([`crate::quality`]'s
    /// Medium) after [`Self::apply_preset`] ran the switch (audit G render5 #7). Only the exact
    /// columns the default writes are cleared, while the switch is on.
    pub(crate) fn retract_unmeasured_visibility_default(&mut self) {
        if self.visibility_default == crate::VisibilityDefault::Dense
            && self.holds_visibility_default()
            && !self.dense_default_measured()
        {
            self.profile.visibility_buffer = false;
            self.profile.visibility_path = crate::VisibilityPath::DEFAULT;
        }
    }

    /// Whether the dense-scene default holds the profile's path, and why not when it does not.
    #[must_use]
    pub fn visibility_default_state(&self) -> crate::VisibilityDefaultState {
        use crate::VisibilityDefaultState as S;
        match self.visibility_default {
            crate::VisibilityDefault::Preset => S::SwitchOff,
            crate::VisibilityDefault::Dense if !self.dense_default_measured() => S::PresetUnmeasured,
            crate::VisibilityDefault::Dense if self.holds_visibility_default() => S::Applied,
            crate::VisibilityDefault::Dense => S::ProfileChoice,
        }
    }

    /// The profile's path columns are what the default writes.
    fn holds_visibility_default(&self) -> bool {
        self.profile.visibility_buffer && self.profile.visibility_path == crate::VisibilityPath::DENSE_DEFAULT
    }

    /// The profile's path columns are its preset's (every preset has the path off).
    fn at_preset_path(&self) -> bool {
        !self.profile.visibility_buffer && self.profile.visibility_path == crate::VisibilityPath::DEFAULT
    }

    /// The profile is a preset row the default has been measured on.
    fn dense_default_measured(&self) -> bool {
        matches!(
            self.preset(),
            Some(RenderProfilePreset::DesktopHigh | RenderProfilePreset::DesktopUltra)
        )
    }

    /// The preset whose table row the active profile is, or `None` for a customised profile.
    /// The quality detector's terrain virtual texture lever
    /// ([`crate::quality::QualityTier::apply_for_gpu`]) does not make a profile customised, and
    /// neither does [`RenderProfile::frame_pipelining`], which leaves the frames as they are, or a
    /// feature no preset turns on (subsurface scattering, blue noise, order-independent
    /// transparency, the radiance cache, the visibility path selection, ADR-2106; the visibility
    /// buffer switch and the sun's shadow trace, audit v0.2.12; the subsurface kernel and the grass's
    /// foliage transmission, ADR-3170): other systems choose their tier
    /// by this preset, and opting in to one of these must not move water or foliage to another
    /// tier ([`RenderProfile::dense_scenes`] is still its preset's row). The contact calibration
    /// switches ([`RenderProfile::horizon_ao`], [`RenderProfile::angular_sun_penumbra`], ADR-3202)
    /// are runtime opt-ins too. Runtime [`RenderProfile::temporal_upscaler`] and
    /// [`RenderProfile::render_scale`] changes also preserve the preset and its gameplay caps.
    /// The quality ladder's Very Low profile
    /// ([`crate::quality::QualityTier::VeryLow`], Desktop Low drawn cheaper, ADR-7882) answers
    /// Desktop Low.
    #[must_use]
    pub fn preset(&self) -> Option<RenderProfilePreset> {
        let mut profile = self.profile.clone();
        profile.terrain_budget.virtual_texture = false;
        profile.frame_pipelining = crate::FramePipelining::OFF;
        profile.subsurface_scattering = false;
        profile.blue_noise = false;
        profile.order_independent_transparency = false;
        profile.radiance_cache = false;
        profile.shadow_occlusion = false;
        profile.visibility_path = crate::visbuffer::VisibilityPath::DEFAULT;
        profile.visibility_buffer = false;
        profile.sun_shadow_trace = crate::hwrt::RtShadowRequest::Off;
        profile.terrain_budget.vt_late_depth = false;
        // Set at runtime by `render.set_many_light_sampling` (ADR-2652).
        profile.many_light_sampling = false;
        // ADR-3294: a runtime switch until the owner has looked at the plates.
        profile.aerial_from_tables = false;
        // Set at runtime by `render.set_contact_calibration` (ADR-3202).
        profile.horizon_ao = false;
        profile.angular_sun_penumbra = false;
        profile.restir_gi = false;
        profile.subsurface_kernel = crate::subsurface::SubsurfaceKernel::Screen;
        profile.terrain_budget.foliage_subsurface = false;

        // Set at runtime by a host or `render.set_visbuffer_residency` (ADR-3118).
        profile.cluster_residency = crate::visbuffer::ClusterResidency::OFF;
        // The BRDF energy model is a look choice no preset makes (ADR-2632).
        profile.brdf_energy = crate::BrdfEnergy::Legacy;
        // The forward-decal path is a look switch no preset turns on (ADR-3255).
        profile.forward_decals = false;
        // Set at runtime by `render.set_sun_specular_disc` (ADR-3190).
        profile.sun_specular_disc = false;
        // Set at runtime by `render.set_area_light_fallback` (ADR-3247).
        profile.area_light_fallback = false;
        // Set at runtime by `render.set_sized_light_ltc` (ADR-3247).
        profile.sized_light_ltc = false;
        // The host memory budget changes no pixel; a host may set its own (ADR-7891). Each
        // row is compared with its own budget.
        // Runtime upscaling must not move gameplay tiers. Mask both sides because the preset
        // rows have different default scales, including the Very Low fallback below.
        profile.temporal_upscaler = crate::TemporalUpscaler::Taau;
        profile.render_scale = 1.0;
        let matches_quality = |mut candidate: RenderProfile| {
            candidate.temporal_upscaler = profile.temporal_upscaler;
            candidate.render_scale = profile.render_scale;
            candidate.host_memory = profile.host_memory;
            // CPU masked occlusion changes no pixel (ADR-10072); `render.set_occlusion` sets it at
            // runtime, so each row is compared with the profile's own value.
            candidate.masked_occlusion = profile.masked_occlusion;
            candidate == profile
        };
        RenderProfilePreset::WITH_OFFLINE
            .into_iter()
            .find(|p| matches_quality(RenderProfile::from_preset(*p)))
            // The quality ladder's Very Low is Desktop Low drawn cheaper (ADR-7882): systems that
            // pick their tier by preset stay on Low's (profile-field-breaks-preset).
            .or_else(|| {
                matches_quality(crate::quality::QualityTier::VeryLow.profile())
                    .then_some(RenderProfilePreset::DesktopLow)
            })
    }

    /// The frame quality the profile prescribes, with the lighting option applied.
    #[must_use]
    pub fn quality(&self) -> FrameQuality {
        let mut quality = FrameQuality::from_profile(&self.profile);
        quality.gi_probes = self.effective_gi_look().runs_probes();
        // A map's bake lights every preset (Low included) unless the player chose Balanced.
        quality.baked_gi = self.gi_look.is_none_or(crate::GiLook::uses_bake);
        quality
    }
}

/// The angular radius the runtime sun's specular integrates over (ADR-3190): the sun's 0.27°
/// (the radius the sun shadows' PCSS and the path tracer use) when the profile opts in, else 0
/// (the point sun every preset ships).
#[must_use]
pub(crate) fn sun_specular_disc(profile: &RenderProfile) -> f32 {
    if profile.sun_specular_disc {
        crate::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN
    } else {
        0.0
    }
}

/// `PreUpdate`: copy a profile frame cap into pacing once.
///
/// A profile with no cap does not clear one. An existing cap (the user's, or
/// the framework's) is left alone. [`crate::PresentPolicy::Vrr`] is not overwritten.
pub(crate) fn sync_profile_frame_cap(
    settings: Option<Res<RenderSettings>>,
    mut output: Option<ResMut<crate::OutputSettings>>,
    mut pacing: Option<ResMut<regia_app::FramePacing>>,
) {
    let (Some(settings), Some(output)) = (settings, output.as_deref_mut()) else {
        return;
    };
    if let Some(pacing) = pacing.as_deref_mut() {
        crate::apply_profile_frame_cap(&settings.profile, pacing, output);
    } else {
        let mut scratch = regia_app::FramePacing::default();
        crate::apply_profile_frame_cap(&settings.profile, &mut scratch, output);
    }
}

/// The persistent frame renderer, as an app resource.
///
/// Material-graph pipelines build on worker threads
/// ([`FrameRenderer::set_async_material_graphs`]): a game frame never compiles one.
/// A game frame never waits on a sky rebuild: [`FrameRenderer::set_async_sky`] is on.
///
/// Per-instance buffers start sized for [`RUNTIME_INSTANCE_RESERVE`] draws: a game spawning
/// objects never grows them (and rebuilds their bind groups) on a frame; a game that draws
/// more calls [`RuntimeRenderer::reserve_instances`] at level load.
#[derive(Resource)]
pub struct RuntimeRenderer(pub FrameRenderer);

/// Draws the runtime renderer's per-instance buffers hold before they grow (≈ 0.3 MiB).
pub const RUNTIME_INSTANCE_RESERVE: u32 = 1024;

impl Default for RuntimeRenderer {
    fn default() -> Self {
        let mut renderer = FrameRenderer::new();
        renderer.set_async_material_graphs(true);
        renderer.set_async_sky(true);
        renderer.reserve_instances(RUNTIME_INSTANCE_RESERVE);
        // The lens effects pass (ADR-1863) records only on views whose post settings ask for it.
        renderer.enable_lens_effects();
        Self(renderer)
    }
}

impl RuntimeRenderer {
    /// Sizes the per-instance buffers for at least `max_instances` draws (level load: the most
    /// objects a level will draw at once, spawns included).
    pub fn reserve_instances(&mut self, max_instances: u32) {
        self.0.reserve_instances(max_instances);
    }

    /// Hands over every material graph the level can draw
    /// ([`FrameRenderer::set_level_material_graphs`]): the game-frame warm starts their sets, so a
    /// graph first drawn mid-play finds them built. Call at level load, before the warm.
    pub fn set_level_material_graphs(&mut self, graphs: Vec<std::sync::Arc<crate::SurfaceGraph>>) {
        self.0.set_level_material_graphs(graphs);
    }

    /// Frame warms also cover reflection-probe captures. See
    /// [`FrameRenderer::set_warm_probe_capture`](crate::FrameRenderer::set_warm_probe_capture).
    pub fn set_warm_probe_capture(&mut self, enabled: bool) {
        self.0.set_warm_probe_capture(enabled);
    }

    /// Installs the irradiance field sampled by the forward pass.
    pub fn set_gi_field(&mut self, field: Option<crate::GiField>) {
        self.0.set_gi_field(field);
    }

    /// Installs the map's baked GI volume (`None` removes it). See
    /// [`FrameRenderer::set_baked_gi`](crate::FrameRenderer::set_baked_gi).
    pub fn set_baked_gi(&mut self, volume: Option<std::sync::Arc<regia_asset::baked_gi::BakedGiVolume>>) {
        self.0.set_baked_gi(volume);
    }

    /// Replaces the capsule occluders of dynamic units over the baked GI volume; returns how
    /// many are used. See
    /// [`FrameRenderer::set_gi_occluders`](crate::FrameRenderer::set_gi_occluders).
    pub fn set_gi_occluders(&mut self, capsules: &[crate::baked_gi::GiCapsule]) -> usize {
        self.0.set_gi_occluders(capsules)
    }

    /// Installs reflection probes for the SSR / probe pass.
    pub fn set_reflection_probes(&mut self, probes: Vec<crate::ReflectionProbe>) {
        self.0.set_reflection_probes(probes);
    }

    /// Captures one probe by rendering six faces. See
    /// [`FrameRenderer::capture_reflection_probe`](crate::FrameRenderer::capture_reflection_probe).
    pub fn capture_reflection_probe(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        id: u64,
        center: [f32; 3],
        half_extent: [f32; 3],
    ) -> Result<crate::ReflectionProbe, RenderError> {
        self.0
            .capture_reflection_probe(device, scene, id, center, half_extent)
    }

    /// Records the CPU probe-trace time for the render lab.
    pub fn set_gi_cpu_ms(&mut self, ms: f32) {
        self.0.set_gi_cpu_ms(ms);
    }

    /// Sets the main view's exposure-zone target (`None` clears it). See
    /// [`FrameRenderer::set_exposure_target`](crate::FrameRenderer::set_exposure_target).
    pub fn set_exposure_target(&mut self, target: Option<crate::EvTarget>) {
        self.0.set_exposure_target(target);
    }
}

/// Extracted camera of a view.
#[derive(Debug, Clone, Copy)]
pub struct ExtractedCamera {
    /// World-space eye.
    pub eye: [f32; 3],
    /// A point on the camera's forward ray, [`crate::view_math::look_reach`] ahead of the eye (at
    /// least one metre, and at least four times the eye's largest coordinate): far enough that
    /// `normalize(target − eye)` gives the forward direction to `f32` precision anywhere in the
    /// world (ADR-1488). It is not one unit ahead: normalize `target − eye` for the direction.
    pub target: [f32; 3],
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

/// What a view renders into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExtractedTarget {
    /// The window surface, inside the view's viewport rect.
    Window,
    /// The persistent texture owned for this camera (`asset_id`).
    Texture {
        /// The camera's texture asset id.
        asset_id: u64,
    },
}

/// One extracted view: a camera with its viewport, order and clear behaviour.
#[derive(Debug, Clone)]
pub struct ExtractedView {
    /// Camera placement and projection.
    pub camera: ExtractedCamera,
    /// Normalized viewport rect (x, y, width, height) on the target.
    pub viewport: [f32; 4],
    /// Sort order on the same target (lower draws first).
    pub order: i32,
    /// Clear the colour target before drawing (the first view of a target),
    /// or preserve what is there.
    pub clear: bool,
    /// Where this view renders.
    pub target: ExtractedTarget,
    /// Layer mask: bit `i` set ⇒ layer `i` is visible.
    pub layer_mask: u32,
    /// This view's post settings (profile toggles + the camera's EV pin).
    pub post: PostSettings,
}

/// One extracted mesh instance.
#[derive(Clone)]
pub struct ExtractedMesh {
    /// Entity (object GPU cache identity).
    pub entity: Entity,
    /// Shared mesh.
    pub mesh: Arc<CpuMesh>,
    /// Shared material.
    pub material: Arc<CpuMaterial>,
    /// Render layer (visible when `layer_mask` has the bit set).
    pub layer: u32,
    /// Column-major object-to-world matrix.
    pub model: [[f32; 4]; 4],
    /// World-space joint palette (empty = rigid).
    pub joints: Vec<JointMatrix>,
    /// Explicit decal receiver layer (`0` = infer from the skin palette).
    pub decal_layer: u32,
    /// Second UV set ([`MeshUv1`]).
    pub uv1: Option<Arc<Vec<[f32; 2]>>>,
    /// Which shadow views the mesh is drawn into: [`crate::ShadowCast::All`], or
    /// [`crate::ShadowCast::Off`] when the entity carries [`NotShadowCaster`].
    pub shadow: crate::ShadowCast,
    /// Indices to draw. `None` draws [`CpuMesh::indices`] (every mesh without a LOD chain).
    pub draw_indices: Option<Arc<Vec<u32>>>,
    /// Xored into the mesh cache key. `0` keeps the `Arc` address (LOD 0, not shadow-only).
    pub lod_key: u64,
    /// Xored into the object key so a fade pair and a shadow LOD do not share one uniform.
    pub object_key_xor: u64,
    /// Copied from [`MeshRenderer::gi_proxy`]. False skips the GI brick. Shadows stay
    /// [`Self::shadow`].
    pub gi_proxy: bool,
    /// Copied from [`MeshLodFade`] (`0.0` = drawn whole); becomes [`crate::FrameMesh::lod_dither`].
    pub lod_dither: f32,
    /// Copied from the entity's `regia_scene::components::Static` marker (`specs/14` A1);
    /// becomes [`crate::FrameMesh::is_static`].
    pub is_static: bool,
}

/// The render world: everything the renderer needs for one frame, extracted
/// from the ECS by [`extract_frame`].
#[derive(Default, Resource, Clone)]
pub struct ExtractedFrame {
    /// Every extracted camera view (window views first by order, then texture views).
    pub views: Vec<ExtractedView>,
    /// Mesh instances in draw order.
    pub meshes: Vec<ExtractedMesh>,
    /// Directional light.
    pub directional: Option<crate::DirectionalLightUniform>,
    /// Draw the sun's cascaded shadow map.
    pub directional_shadows: bool,
    /// The directional lights besides the sun, the ambient volumes and the forward shader's debug
    /// view (`specs/13` § 6, § 10, § 11; ADR-7211). The render systems hand it to
    /// [`crate::FrameRenderer::set_lights_views`].
    pub lights_views: crate::LightsViews,
    /// Point and spot lights.
    pub local_lights: Vec<LabLocalLight>,
    /// The entity of each point and spot light [`extract_frame`] put in [`Self::local_lights`],
    /// index for index (the lights appended later, such as area-light stand-ins and particle
    /// lights, have none: this list is shorter). The cell cull reads it to leave out the lights
    /// of unreached cells ([`crate::viscells`], stage 2).
    pub local_light_entities: Vec<Entity>,
    /// The IES profiles of [`Self::local_lights`] and the authored rect and tube lights (ADR-2592); empty
    /// without either.
    pub light_extras: crate::light_profiles::LightExtras,
    /// Effective environment (copy of the [`Environment`] resource or default).
    pub environment: Environment,
    /// Run the depth prepass.
    pub depth_prepass: bool,
    /// Distance + exponential height fog.
    pub fog: crate::FogSettings,
    /// Quality knobs (from [`RenderSettings`] or the lab baseline).
    pub quality: FrameQuality,
    /// The dynamic GI tier in effect ([`RenderSettings::effective_dynamic_gi_tier`]); the
    /// render systems hand it to [`crate::FrameRenderer::set_dynamic_gi_tier`].
    pub dynamic_gi_tier: crate::DynamicGiTier,
    /// Screen-space GI reads the radiance cache ([`RenderSettings::effective_radiance_cache`]); the
    /// render systems hand it to [`crate::FrameRenderer::set_radiance_cache`].
    pub radiance_cache: bool,
    /// The depth prepass culls in two phases ([`RenderSettings::effective_two_phase_occlusion`]);
    /// the render systems hand it to [`crate::FrameRenderer::set_two_phase_occlusion`].
    pub two_phase_occlusion: bool,
    /// The shadow casters cull in two phases ([`RenderSettings::effective_shadow_occlusion`]); the
    /// render systems hand it to [`crate::FrameRenderer::set_shadow_occlusion`].
    pub shadow_occlusion: bool,
    /// The forward draw list culls hidden draws on the CPU ([`RenderProfile::masked_occlusion`],
    /// ADR-10072); the render systems hand it to [`crate::FrameRenderer::set_masked_occlusion`].
    pub masked_occlusion: bool,
    /// Atmosphere, froxel fog and cloud inputs copied from optional resources.
    pub outdoor: crate::OutdoorParams,
    /// The valley mist ([`crate::ValleyMist`]) copied from its resource, or `None`.
    pub valley_mist: Option<crate::ValleyMist>,
    /// Interior volumes. A camera eye inside one drops sky IBL for that view.
    pub interiors: Vec<InteriorVolume>,
    /// Shell tested with [`crate::detect_enclosure`] when no volume contains the eye.
    pub enclosure: Option<SdfScene>,
    /// Cardinal reach for [`Self::enclosure`], metres. Non-positive disables the test.
    pub enclosure_reach: f32,
    /// Temporal upscaler for TAA views (the profile's [`crate::RenderProfile::temporal_upscaler`]).
    pub temporal_upscaler: crate::TemporalUpscaler,
    /// Gizmo lines for the window views (filled by [`crate::gizmos::GizmoPlugin`]).
    pub gizmos: std::sync::Arc<crate::gizmos::GizmoFrame>,
    /// Per-instance glass state ([`extract_glass`]; empty without glass components).
    pub glass: Vec<crate::glass::GlassInstance>,
    /// Large-mesh occlusion proxies ([`crate::mesh_occluders::extract_mesh_occluders`]; empty
    /// without [`crate::OcclusionProxy`] components).
    pub mesh_occluders: Vec<crate::MeshOccluder>,
    /// Morphed instances ([`crate::morph::extract_morphs`]; empty without
    /// [`crate::MeshMorphTargets`] + `MorphWeights` entities). Pass them to
    /// [`crate::FrameRenderer::set_morph_instances`].
    pub morphs: Vec<crate::MorphInstance>,
    /// [`crate::TextureViewSettings`] of the texture cameras that carry one, by asset id (the
    /// others use the default).
    pub texture_view_settings: std::collections::HashMap<u64, crate::TextureViewSettings>,
    /// Mesh keys ([`mesh_key`]) whose last `Arc<CpuMesh>` dropped since the previous
    /// [`extract_frame`]. Pass them to [`crate::FrameRenderer::release_meshes`] so their GPU
    /// ranges are freed now instead of at the idle eviction.
    pub released_mesh_keys: Vec<u64>,
    /// Masked instances ([`crate::mask_decals::extract_mask_decals`]; empty without
    /// [`crate::MaskDecals`] components). Pass them to [`crate::FrameRenderer::set_mask_decals`].
    pub mask_decals: Vec<crate::MaskDecalInstance>,
    /// The camera entity of each of [`Self::views`], index for index, as [`extract_frame`] found
    /// them (ADR-7217: the agent's `camera` screenshot target renders one named camera). A host
    /// that pushes views of its own leaves this shorter than `views`; [`Self::view_index_of`]
    /// then answers `None`.
    pub view_entities: Vec<Entity>,
    /// The outdoors the window views see from inside a cell (plan row W-22, ADR-8413): the union
    /// of [`Self::outdoor_portal_views`] (ADR-8843), restricted only when every window view is,
    /// with the first window view's eye. Filled by [`crate::viscells::cull_unreachable_cells`];
    /// unrestricted without cells. The scatter field reads it (the frame has one draw list), and
    /// a host that draws one view hands it to [`crate::FrameRenderer::set_outdoor_portals`].
    pub outdoor_portals: crate::viscells::OutdoorPortals,
    /// One outdoor list per window view (split-screen, ADR-8843); a view without an entry is
    /// unrestricted. The window paths hand each view its own entry
    /// ([`Self::outdoor_portals_for`]) before drawing it.
    pub outdoor_portal_views: crate::viscells::OutdoorPortalViews,
}

impl ExtractedFrame {
    /// Mesh keys ([`crate::FrameMesh::mesh_key`], [`mesh_key`] of the mesh) whose
    /// [`ExtractedMesh::gi_proxy`] is false.
    ///
    /// Any instance opts the whole mesh out. Pass this to
    /// [`crate::FrameRenderer::set_gi_proxy_disabled`] every frame, including when it is
    /// empty, so a mesh that opted back in is traced again.
    pub fn gi_proxy_disabled_keys(&self) -> impl Iterator<Item = u64> + '_ {
        // Collected under one lock, released before the caller iterates.
        let keys: Vec<u64> = if self.meshes.iter().all(|mesh| mesh.gi_proxy) {
            Vec::new()
        } else {
            let mut registry = mesh_registry();
            self.meshes
                .iter()
                .filter(|mesh| !mesh.gi_proxy)
                .map(|mesh| registry.key(&mesh.mesh))
                .collect()
        };
        keys.into_iter()
    }

    /// The rigid meshes with an emitting material ([`crate::emissive_lights::material_emits`]):
    /// what [`crate::FrameRenderer::hold_mesh_identities`] takes each frame, so the emissive
    /// fit cache knows a mesh key still names the same mesh (v0.1.18).
    pub fn emissive_meshes(&self) -> impl Iterator<Item = &Arc<CpuMesh>> + '_ {
        self.meshes
            .iter()
            .filter(|mesh| mesh.joints.is_empty() && crate::emissive_lights::material_emits(&mesh.material))
            .map(|mesh| &mesh.mesh)
    }

    /// Builds the renderer's frame input for the view at `view_index`,
    /// rendered at a `width`×`height` target.
    ///
    /// The same builder serves window surfaces and offscreen screenshots, so
    /// goldens (via [`crate::LabScene::as_frame_scene`]) and games submit the
    /// same shape of data to the same renderer.
    #[must_use]
    pub fn frame_scene(&self, view_index: usize, width: u32, height: u32) -> FrameScene<'_> {
        let view = self.views.get(view_index);
        let mut registry = mesh_registry();
        let mut materials = material_registry();
        let meshes = self
            .meshes
            .iter()
            .filter(|mesh| view.is_none_or(|view| layer_visible(view.layer_mask, mesh.layer)))
            .map(|mesh| {
                let indices = match mesh.draw_indices.as_deref() {
                    Some(indices) => indices.as_slice(),
                    None => mesh.mesh.indices.as_slice(),
                };
                FrameMesh {
                    mesh_key: crate::mesh_lod::lod_variant_key(registry.key(&mesh.mesh), mesh.lod_key),
                    material_key: materials.key(&mesh.material),
                    object_key: mesh.entity.to_bits() ^ mesh.object_key_xor,
                    vertices: &mesh.mesh.vertices,
                    indices,
                    material: &mesh.material,
                    model: mesh.model,
                    joints: &mesh.joints,
                    receiver_layer: mesh.decal_layer,
                    shadow: mesh.shadow,
                    uv1: mesh.uv1.as_deref().map_or(&[], Vec::as_slice),
                    lod_dither: mesh.lod_dither,
                    is_static: mesh.is_static,
                }
            })
            .collect::<Vec<_>>();
        drop(materials);
        drop(registry);
        let eye = view.map_or([0.0, 0.0, 0.0], |v| v.camera.eye);
        let shade = shade_for_view(
            eye,
            &self.interiors,
            self.enclosure.as_ref(),
            self.enclosure_reach,
            self.environment.ambient_lux,
            self.environment.ibl,
        );
        let mut quality = self.quality;
        if shade.interior {
            quality.interior_ambient = true;
        }
        FrameScene {
            width,
            height,
            eye,
            target: view.map_or([0.0, 0.0, -1.0], |v| v.camera.target),
            fov_y: view.map_or(std::f32::consts::FRAC_PI_4, |v| v.camera.fov_y),
            near: view.map_or(0.1, |v| v.camera.near),
            far: view.map_or(1000.0, |v| v.camera.far),
            depth_prepass: self.depth_prepass,
            directional_shadows: self.directional_shadows,
            ibl: shade.ibl,
            exposure_ev: view.map_or(0.0, |v| v.post.exposure_ev),
            ambient_lux: shade.ambient_lux,
            double_sided: self.environment.double_sided,
            post: view.map(|v| &v.post),
            // A post volume's fog tint is per view (ADR-1464); the neutral tint is bit-identical.
            fog: view.map_or(self.fog, |v| v.post.tinted_fog(self.fog)),
            directional: self.directional.unwrap_or(crate::DirectionalLightUniform {
                direction: [0.0, -1.0, 0.0],
                _pad_dir: 0,
                color: [1.0; 3],
                illuminance: 0.0,
            }),
            local_lights: &self.local_lights,
            quality,
            meshes,
        }
    }

    /// The window views of this frame, in draw order (lowest `order` first).
    /// Each entry carries the view's index in [`Self::views`] (for [`Self::frame_scene`]).
    #[must_use]
    pub fn window_views(&self) -> Vec<(usize, ExtractedView)> {
        let mut views: Vec<(usize, ExtractedView)> = self
            .views
            .iter()
            .enumerate()
            .filter(|(_, view)| matches!(view.target, ExtractedTarget::Window))
            .map(|(index, view)| (index, view.clone()))
            .collect();
        views.sort_by_key(|(_, view)| view.order);
        views
    }

    /// The render-to-texture views of this frame, in draw order.
    #[must_use]
    pub fn texture_views(&self) -> Vec<(usize, ExtractedView)> {
        let mut views: Vec<(usize, ExtractedView)> = self
            .views
            .iter()
            .enumerate()
            .filter(|(_, view)| matches!(view.target, ExtractedTarget::Texture { .. }))
            .map(|(index, view)| (index, view.clone()))
            .collect();
        views.sort_by_key(|(_, view)| view.order);
        views
    }

    /// The outdoor list of view `view` (its index in [`Self::views`]; ADR-8843): its entry in
    /// [`Self::outdoor_portal_views`], or `None` (unrestricted) when it has none. What the window
    /// paths hand [`crate::FrameRenderer::set_outdoor_portals`] before drawing that view.
    #[must_use]
    pub fn outdoor_portals_for(&self, view: usize) -> Option<&crate::viscells::OutdoorPortals> {
        self.outdoor_portal_views.get(view)
    }

    /// The first window view (the primary camera), if any.
    #[must_use]
    pub fn primary_view(&self) -> Option<ExtractedView> {
        self.window_views().into_iter().next().map(|(_, view)| view)
    }

    /// Removes the window views: a host that drew the frame elsewhere (XR eyes) leaves the window
    /// to its overlay layers.
    pub fn drop_window_views(&mut self) {
        if self.view_entities.len() == self.views.len() {
            let views = std::mem::take(&mut self.views);
            let entities = std::mem::take(&mut self.view_entities);
            for (view, entity) in views.into_iter().zip(entities) {
                if !matches!(view.target, ExtractedTarget::Window) {
                    self.views.push(view);
                    self.view_entities.push(entity);
                }
            }
        } else {
            self.view_entities.clear();
            self.views
                .retain(|view| !matches!(view.target, ExtractedTarget::Window));
        }
    }

    /// The index in [`Self::views`] of the view `camera` (a camera entity) rendered this frame,
    /// for [`Self::frame_scene`]. `None` when the camera was not extracted (no [`Camera`], no
    /// forward direction, not extracted yet) or the views were not built by [`extract_frame`].
    #[must_use]
    pub fn view_index_of(&self, camera: Entity) -> Option<usize> {
        if self.view_entities.len() != self.views.len() {
            return None;
        }
        self.view_entities.iter().position(|entity| *entity == camera)
    }

    /// True when extraction found at least one view and mesh.
    #[must_use]
    pub fn is_renderable(&self) -> bool {
        !self.views.is_empty() && !self.meshes.is_empty()
    }
}

/// Logs once per process when a camera asks for something the 3D frame does not draw yet: an
/// orthographic projection or a solid clear color (ADR-869). Each is a warning, not an error,
/// so the view still renders (in perspective, cleared as for a skybox).
fn warn_unrendered_camera_fields(camera: &Camera) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ORTHO: AtomicBool = AtomicBool::new(false);
    static CLEAR: AtomicBool = AtomicBool::new(false);
    if camera.orthographic && !ORTHO.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "regia_render",
            "Camera::orthographic is not rendered yet: the frame draws this camera in perspective \
             (fov_y {}); picking uses parallel rays (logged once)",
            camera.fov_y
        );
    }
    if let crate::ClearMode::Color(color) = camera.clear
        && !CLEAR.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(
            target: "regia_render",
            "ClearMode::Color({color:?}) is not used yet: the frame clears as for ClearMode::Skybox \
             (logged once)"
        );
    }
}

/// Converts a camera entity's `Camera` + `GlobalTransform` into an
/// [`ExtractedView`] (None when the transform has no forward direction).
pub(crate) fn extract_view(
    camera: &Camera,
    transform: &GlobalTransform,
    target: ExtractedTarget,
    mut post: PostSettings,
) -> Option<ExtractedView> {
    let eye: regia_core::math::Vec3 = transform.0.translation.into();
    let forward = transform
        .0
        .transform_vector3(-regia_core::math::Vec3::Z)
        .normalize_or_zero();
    if forward.length_squared() < 1.0e-6 {
        return None;
    }
    warn_unrendered_camera_fields(camera);
    if let Some(ev) = camera.exposure_ev {
        // A camera EV pins the exposure: auto exposure adapts around scene
        // luminance, which would make screenshots non-deterministic.
        post.auto_exposure = false;
        post.exposure_ev = ev;
        post.use_physical_camera = false;
    }
    Some(ExtractedView {
        camera: ExtractedCamera {
            eye: eye.to_array(),
            // Not `eye + forward`: one unit ahead of an eye far from the origin, the target's f32
            // rounding quantized the direction every pass rebuilds from it (ADR-1488).
            target: crate::view_math::look_target(eye, forward).to_array(),
            fov_y: camera.fov_y,
            near: camera.near,
            far: camera.far,
        },
        viewport: camera.viewport,
        order: camera.order,
        clear: matches!(
            camera.clear,
            crate::ClearMode::Color(_) | crate::ClearMode::Skybox
        ),
        target,
        layer_mask: camera.layer_mask,
        post,
    })
}

/// What [`extract_frame`] reads from a mesh entity.
type ExtractedMeshData = (
    Entity,
    &'static MeshRenderer,
    &'static GlobalTransform,
    Option<&'static SkinJoints>,
    Option<&'static MeshUv1>,
    Option<&'static NotShadowCaster>,
    Option<&'static MeshLodFade>,
    Option<&'static MeshShadowCast>,
    Option<&'static regia_scene::components::Static>,
    crate::visibility::DrawVisibility,
);

/// The draws [`extract_frame`] reads: the mesh entities, then the host's [`crate::DrawList`]
/// (ADR-8087). One parameter because the system is at the 16-parameter limit.
type ExtractedMeshSources<'w, 's> = (
    Query<
        'w,
        's,
        ExtractedMeshData,
        (
            Without<LayerParked>,
            Without<regia_scene::streaming::CellRenderHidden>,
        ),
    >,
    Option<Res<'w, crate::draw_list::DrawList>>,
);

/// The directional lights and ambient volumes [`extract_frame`] reads (ADR-7211). Both read
/// `DrawVisibility` and skip entities that are not drawn (a hidden light or volume lights
/// nothing, like a hidden mesh draws nothing).
type ExtractedDirectionalAndAmbient<'w, 's> = (
    Query<
        'w,
        's,
        (
            Entity,
            &'static DirectionalLight,
            crate::visibility::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
    Query<
        'w,
        's,
        (
            Entity,
            &'static crate::AmbientVolume,
            &'static GlobalTransform,
            crate::visibility::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
);

/// What [`extract_frame`] reads for local lights: points, spots (with their entity, the key of the
/// fog's light pick) and the pick itself (absent: no hysteresis, the pick still runs). One
/// parameter because the system is at the 16-parameter limit.
type ExtractedLocalLights<'w, 's> = (
    Query<
        'w,
        's,
        (
            Entity,
            &'static PointLight,
            &'static GlobalTransform,
            Option<&'static crate::LightSourceSize>,
            Option<&'static crate::LightProfile>,
            Has<crate::LightSourceLtc>,
            crate::visibility::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
    Query<
        'w,
        's,
        (
            Entity,
            &'static SpotLight,
            &'static GlobalTransform,
            Option<&'static crate::LightSourceSize>,
            Option<&'static crate::LightProfile>,
            Has<crate::LightSourceLtc>,
            crate::visibility::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
    Option<ResMut<'w, crate::fog_lights::FogLightPick>>,
    ExtractedAreaLights<'w, 's>,
);

/// What [`extract_frame`] reads for IES profiles and authored area lights (ADR-2592): the profile library and
/// every rect and tube light with its placement.
type ExtractedAreaLights<'w, 's> = (
    Option<Res<'w, crate::LightProfileLibrary>>,
    Query<
        'w,
        's,
        (
            &'static crate::RectLight,
            &'static GlobalTransform,
            crate::visibility::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
    Query<
        'w,
        's,
        (
            &'static crate::TubeLight,
            &'static GlobalTransform,
            crate::visibility::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
);

/// What [`extract_frame`] reads for the level LUT and the post volumes (ADR-1464): the LUT, every
/// volume with its placement, the scratch list of the volumes at one camera (reused, so a steady
/// frame allocates nothing) and the per-volume LUT blend (ADR-2323, off by default). One parameter
/// because the system is at the 16-parameter limit.
type ExtractedLookInputs<'w, 's> = (
    Option<Res<'w, LevelColorLut>>,
    Query<'w, 's, (Entity, &'static crate::PostVolume, &'static GlobalTransform)>,
    Local<'s, Vec<crate::post_volumes::VolumeContribution>>,
    Option<ResMut<'w, crate::post_volume_luts::PostVolumeLuts>>,
);

/// What [`extract_frame`] reads per camera: the camera, its placement, its lens, its texture-view
/// settings and its lens effects (ADR-1863). `Entity` is part of the read so equal-`order`
/// cameras tie-break by entity id, never by query (archetype) order (audit G, render5
/// determinism).
type ExtractedCameras<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Camera,
        &'static GlobalTransform,
        Option<&'static crate::PhysicalCamera>,
        Option<&'static crate::TextureViewSettings>,
        Option<&'static crate::LensEffects>,
    ),
>;

/// What [`extract_frame`] reads for the level and the weather (spec 99). One parameter because the
/// system is at the 16-parameter limit.
type ExtractedAirInputs<'w> = (
    Option<Res<'w, regia_scene::level::LevelSettings>>,
    Option<Res<'w, crate::AerosolSpectrum>>,
    Option<Res<'w, regia_scene::weather::SceneWind>>,
    Option<Res<'w, regia_scene::weather::SurfaceWeather>>,
    Option<Res<'w, regia_scene::weather::WeatherAir>>,
    Option<Res<'w, crate::fog::FogSunShadowProbe>>,
);

/// Records the profile of the light about to be pushed onto `extracted.local_lights` (ADR-2592). `lumens` is a
/// point light's power (its peak then keeps the flux); a spot passes `None` (its candela is the peak).
fn assign_light_profile(
    extracted: &mut ExtractedFrame,
    library: Option<&crate::LightProfileLibrary>,
    profile: Option<&crate::LightProfile>,
    (nadir, reference): (regia_core::glam::Vec3, regia_core::glam::Vec3),
    lumens: Option<f32>,
) {
    let Some(profile) = profile else { return };
    let Some(cooked) = library.and_then(|l| l.get(&profile.profile)) else {
        return;
    };
    let peak = if profile.file_intensity {
        Some(cooked.peak_candela * profile.scale.max(0.0))
    } else {
        lumens.map(|lm| crate::light_profiles::point_peak_for_lumens(cooked, lm))
    };
    let uses = crate::light_profiles::profile_use(nadir, reference, profile.rotation, peak);
    let index = extracted.local_lights.len();
    let cooked = cooked.clone();
    if !extracted.light_extras.assign(index, &cooked, uses) {
        tracing::warn!(
            "light profile {}: more than {} profiles in one frame; the light shades without it",
            profile.profile,
            crate::light_profiles::MAX_PROFILE_SLOTS
        );
    }
}

/// `Render`: extract cameras, meshes, materials, lights and environment into
/// [`ExtractedFrame`] (`specs/13` § 5 step 1). Mesh entities come first, then the keyed draws of
/// the [`crate::DrawList`] resource in key order (ADR-8087).
#[allow(clippy::too_many_arguments)]
pub fn extract_frame(
    mut extracted: ResMut<ExtractedFrame>,
    cameras: ExtractedCameras<'_, '_>,
    (meshes, draw_list): ExtractedMeshSources<'_, '_>,
    (directional, ambient_volumes): ExtractedDirectionalAndAmbient<'_, '_>,
    (points, spots, fog_pick, area_inputs): ExtractedLocalLights<'_, '_>,
    interiors: Query<&InteriorVolume, Without<LayerParked>>,
    enclosure: Option<Res<EnclosureScene>>,
    environment: Option<Res<Environment>>,
    settings: Option<Res<RenderSettings>>,
    time: Option<Res<crate::TimeOfDay>>,
    volumetric_fog: Option<Res<crate::VolumetricFogSettings>>,
    fog_volumes: Option<Res<crate::FogVolumes>>,
    valley_mist: Option<Res<crate::ValleyMist>>,
    clouds: Option<Res<crate::CloudSettings>>,
    (level, aerosol, scene_wind, surface_weather, weather_air, fog_probe): ExtractedAirInputs<'_>,
    (level_lut, post_volumes, mut volume_scratch, mut volume_luts): ExtractedLookInputs<'_, '_>,
) {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::EXTRACT);
    extracted.views.clear();
    extracted.view_entities.clear();
    extracted.texture_view_settings.clear();
    extracted.meshes.clear();
    extracted.morphs.clear();
    extracted.local_lights.clear();
    extracted.local_light_entities.clear();
    extracted.outdoor_portals.clear();
    extracted.outdoor_portal_views.clear();
    extracted.light_extras = crate::light_profiles::LightExtras::default();
    extracted.interiors.clear();
    extracted.enclosure = None;
    extracted.enclosure_reach = 0.0;
    extracted.directional = None;
    extracted.directional_shadows = false;
    extracted.lights_views = crate::LightsViews::default();
    extracted.released_mesh_keys = take_released_mesh_keys();

    let env = environment.map_or_else(Environment::default, |e| e.clone());
    extracted.environment = env;
    let settings = settings.map_or_else(RenderSettings::default, |s| s.clone());
    extracted.depth_prepass = settings.profile.depth_prepass;
    extracted.quality = settings.quality();
    extracted.dynamic_gi_tier = settings.effective_dynamic_gi_tier();
    extracted.radiance_cache = settings.effective_radiance_cache();
    extracted.two_phase_occlusion = settings.effective_two_phase_occlusion();
    extracted.shadow_occlusion = settings.effective_shadow_occlusion();
    extracted.masked_occlusion = settings.profile.masked_occlusion;
    extracted.temporal_upscaler = settings.profile.temporal_upscaler;
    extracted.fog = settings.fog;
    let mut post = settings.resolve_post();
    // The map's meter and grade (scene format v3) apply to every view.
    if let Some(level) = level.as_deref() {
        crate::post::apply_level_look(&mut post, &level.look);
    }
    if let Some(lut) = level_lut.as_deref().and_then(|l| l.lut.clone()) {
        post.lut = Some(lut);
    }

    // Cameras: every camera entity is a view. Window cameras render into the
    // window (viewport rects give split-screen); texture cameras render into
    // the renderer's persistent textures. Views are pushed in entity order so
    // equal-`order` tie-breaks (the primary view, split-screen draw order) never
    // depend on query (archetype) order (audit G, render5 determinism).
    let mut extracted_views: Vec<(Entity, ExtractedView)> = Vec::new();
    for (entity, camera, transform, lens, texture_view, lens_effects) in &cameras {
        // A lens from a newer engine is ignored, not misread (ADR-2323).
        let lens = lens.filter(|lens| {
            let supported = lens.is_supported();
            if !supported {
                crate::lens::warn_newer_lens_once(lens.format_version);
            }
            supported
        });
        let target = match &camera.target {
            RenderTarget::Window => ExtractedTarget::Window,
            RenderTarget::Texture { asset_id } => {
                if let Some(settings) = texture_view {
                    extracted
                        .texture_view_settings
                        .insert(*asset_id, settings.clone());
                }
                ExtractedTarget::Texture { asset_id: *asset_id }
            }
        };
        let mut view_post = post.clone();
        // A camera's lens effects replace the project's; volumes then override fields (ADR-1863).
        // One from a newer engine is ignored, not misread (ADR-3196).
        if let Some(effects) = lens_effects {
            if effects.is_supported() {
                effects.apply_camera(&mut view_post);
            } else {
                crate::lens_effects::warn_newer_lens_effects_once(effects.format_version);
            }
        }
        // Post volumes at this camera's eye (ADR-1464); none leaves the post untouched.
        if !post_volumes.is_empty() {
            let _span = tracing::info_span!("post_volumes").entered();
            crate::post_volumes::gather_contributions(
                post_volumes.iter(),
                transform.0.translation.to_array(),
                &mut volume_scratch,
            );
            crate::post_volumes::blend_post_volumes(&mut view_post, &mut volume_scratch);
            // Volume LUTs over the level's, only when the host turned them on (ADR-2323).
            if let Some(luts) = volume_luts.as_deref_mut() {
                luts.apply(
                    post_volumes.iter(),
                    transform.0.translation.to_array(),
                    &mut view_post,
                );
            }
        }
        // The camera's lens, where the preset allows each effect; no lens leaves both off.
        if let Some(lens) = lens {
            lens.apply(&mut view_post, &settings.profile);
        }
        // Lens effects only where the preset allows them, whatever the volumes asked for.
        crate::gate_lens_effects(&mut view_post, &settings.profile);
        if let Some(mut view) = extract_view(camera, transform, target, view_post) {
            // The lens's exposure mode and focal length come after the camera's EV pin, so a
            // manual or physical lens wins over it (ADR-1464).
            if let Some(lens) = lens {
                lens.apply_exposure(&mut view.post);
                if let Some(fov_y) = lens.vertical_fov() {
                    view.camera.fov_y = fov_y;
                }
            }
            extracted_views.push((entity, view));
        }
    }
    // Spawn index, not `Entity::to_bits`. bevy_ecs stores the index xored with `u32::MAX`
    // (`NonMaxU32`), so `to_bits` reverses spawn order. The stream origin uses `index_u32`
    // (`pick_primary_view`); sorting views the other way drew the other equal-order camera.
    extracted_views.sort_by_key(|(entity, _)| entity.index_u32());
    for (entity, view) in extracted_views {
        extracted.views.push(view);
        extracted.view_entities.push(entity);
    }

    // Meshes: world matrix from the transform; palette from SkinJoints.
    for (
        entity,
        mesh,
        transform,
        joints,
        uv1,
        not_shadow_caster,
        lod_fade,
        shadow_cast,
        is_static,
        inherited,
    ) in &meshes
    {
        // A hidden entity (its own or an ancestor's `Visibility::Hidden`) draws nothing: no colour,
        // shadow, cluster or decal draw follows it (ADR-7200).
        if !crate::visibility::is_drawn(inherited) {
            continue;
        }
        // A counted stand-in has no CPU vertices: it draws its upload (ADR-7330).
        if (mesh.mesh.vertices.is_empty() || mesh.mesh.indices.is_empty())
            && counted_mesh_id(&mesh.mesh).is_none()
        {
            continue;
        }
        let model = regia_core::math::Mat4::from(transform.0);
        // A non-finite fade is drawn whole; an outgoing LOD (negative) casts no shadow, the
        // incoming one casts it for both (`MeshLodFade`).
        let lod_dither = lod_fade.map_or(0.0, |f| f.0).clamp(-1.0, 1.0);
        let lod_dither = if lod_dither.is_finite() { lod_dither } else { 0.0 };
        let shadow = if not_shadow_caster.is_some() || lod_dither < 0.0 {
            crate::ShadowCast::Off
        } else {
            shadow_cast.map_or(crate::ShadowCast::All, |cast| cast.0)
        };
        extracted.meshes.push(ExtractedMesh {
            entity,
            mesh: Arc::clone(&mesh.mesh),
            material: Arc::clone(&mesh.material),
            layer: mesh.layer,
            model: model.to_cols_array_2d(),
            joints: joints.map_or_else(Vec::new, |j| j.palette.clone()),
            decal_layer: mesh.decal_layer,
            uv1: uv1.map(|uv1| Arc::clone(&uv1.0)),
            shadow,
            draw_indices: None,
            lod_key: 0,
            object_key_xor: 0,
            gi_proxy: mesh.gi_proxy,
            lod_dither,
            is_static: is_static.is_some(),
        });
    }
    // Keyed draws without entities, after the entity meshes, in key order (ADR-8087).
    if let Some(draw_list) = draw_list.as_deref() {
        draw_list.extend_extracted(&mut extracted.meshes);
    }

    // Lights.
    // Up to four directional lights (`specs/13` § 6, ADR-7211): the sun (with the cascades when it
    // casts shadows) is the lowest-entity light, the next three in entity order shade unshadowed, any
    // more are refused. Entity order, not the query's: query order is archetype order, which a save
    // respawn or an added component changes without an edit to the light itself (audit G, render5
    // determinism).
    let rows: Vec<(Entity, &DirectionalLight)> = directional
        .iter()
        .filter(|(.., inherited)| crate::visibility::is_drawn(*inherited))
        .map(|(entity, light, _)| (entity, light))
        .collect();
    let keys: Vec<u64> = rows.iter().map(|(e, _)| e.to_bits()).collect();
    let pick = crate::lights_views::pick_directional(&keys);
    if let Some(light) = pick.sun.map(|i| rows[i].1) {
        // The sun alone carries the specular disc (ADR-3190); the extra lights stay points.
        extracted.directional = Some(
            crate::DirectionalLightUniform::new(light.direction, light.color, light.illuminance)
                .with_specular_disc(sun_specular_disc(&settings.profile)),
        );
        extracted.directional_shadows = light.cast_shadows;
    }
    extracted.lights_views.extra_directional = pick
        .extra
        .iter()
        .map(|&i| {
            let light = rows[i].1;
            crate::DirectionalLightUniform::new(light.direction, light.color, light.illuminance)
        })
        .collect();
    extracted.lights_views.refused_directional = pick.refused;
    crate::lights_views::warn_refused_directional(pick.refused);
    // Ambient volumes (`specs/13` § 10, ADR-7211), in entity order, up to four; a volume that is
    // not drawn (hidden itself or under a hidden parent) lights nothing and is not extracted.
    let mut volumes: Vec<(u64, crate::FrameAmbientVolume)> = ambient_volumes
        .iter()
        .filter(|(_, _, _, inherited)| crate::visibility::is_drawn(*inherited))
        .map(|(entity, volume, transform, _)| {
            let (scale, rotation, translation) = transform.0.to_scale_rotation_translation();
            (
                entity.to_bits(),
                volume.frame_volume_turned(translation.to_array(), rotation.to_array(), scale.to_array()),
            )
        })
        .collect();
    volumes.sort_by_key(|(bits, _)| *bits);
    let keep = volumes.len().min(crate::MAX_AMBIENT_VOLUMES);
    extracted.lights_views.refused_ambient_volumes = volumes.len() - keep;
    crate::lights_views::warn_refused_volumes(volumes.len() - keep);
    extracted.lights_views.ambient_volumes = volumes.into_iter().take(keep).map(|(_, v)| v).collect();
    let mut fog_candidates = Vec::new();
    let (profile_library, rects, tubes) = area_inputs;
    // Points then spots, each in entity order: the index a light lands at is
    // what the budget tie-breaks and the froxel cap keep, so extraction order
    // must be entity order (audit G).
    // A hidden light (its own `Visibility::Hidden` or a hidden ancestor) is not extracted: no
    // light-list entry, no shadow, no fog pick (ADR-7670).
    let mut point_rows: Vec<_> = points
        .iter()
        .filter(|(.., inherited)| crate::visibility::is_drawn(*inherited))
        .collect();
    point_rows.sort_by_key(|(entity, ..)| entity.to_bits());
    // ADR-3247: the profile's opt-in shades every sized light as if it carried the marker (a light without a size
    // ignores the mark).
    let ltc_all = settings.profile.sized_light_ltc;
    for (entity, light, transform, source, profile, ltc, _) in point_rows {
        if ltc || (ltc_all && source.is_some()) {
            let index = extracted.local_lights.len();
            extracted.light_extras.mark_ltc_sphere(index);
        }
        assign_light_profile(
            &mut extracted,
            profile_library.as_deref(),
            profile,
            crate::light_profiles::point_light_frame(transform.0.to_scale_rotation_translation().1),
            Some(light.intensity),
        );
        extracted.local_lights.push(LabLocalLight::Point(LabPointLight {
            position: transform.0.translation.to_array(),
            color: light.color,
            intensity: light.intensity,
            radius: light.radius,
            cast_shadows: light.cast_shadows,
            source_radius: source.and_then(|s| s.sanitized()),
        }));
        extracted.local_light_entities.push(entity);
        push_fog_candidate(&mut fog_candidates, entity, extracted.local_lights.last());
    }
    let mut spot_rows: Vec<_> = spots
        .iter()
        .filter(|(.., inherited)| crate::visibility::is_drawn(*inherited))
        .collect();
    spot_rows.sort_by_key(|(entity, ..)| entity.to_bits());
    for (entity, light, transform, source, profile, ltc, _) in spot_rows {
        if ltc || (ltc_all && source.is_some()) {
            let index = extracted.local_lights.len();
            extracted.light_extras.mark_ltc_sphere(index);
        }
        assign_light_profile(
            &mut extracted,
            profile_library.as_deref(),
            profile,
            crate::light_profiles::spot_light_frame(transform.0.to_scale_rotation_translation().1),
            None,
        );
        let dir = transform
            .0
            .transform_vector3(-regia_core::math::Vec3::Z)
            .normalize_or_zero();
        extracted.local_lights.push(LabLocalLight::Spot(LabSpotLight {
            position: transform.0.translation.to_array(),
            direction: dir.to_array(),
            color: light.color,
            intensity: light.intensity,
            range: light.range,
            inner_angle: light.inner_angle,
            outer_angle: light.outer_angle,
            cast_shadows: light.cast_shadows,
            source_radius: source.and_then(|s| s.sanitized()),
        }));
        extracted.local_light_entities.push(entity);
        push_fog_candidate(&mut fog_candidates, entity, extracted.local_lights.last());
    }
    for (rect, transform, _) in rects
        .iter()
        .filter(|(.., inherited)| crate::visibility::is_drawn(*inherited))
    {
        let model = regia_core::glam::Mat4::from(transform.0);
        extracted
            .light_extras
            .area_lights
            .push(crate::light_profiles::AreaLightInput {
                light: crate::light_profiles::rect_emitter(rect, &model),
                range: rect.range,
                cast_shadows: rect.cast_shadows,
            });
    }
    for (tube, transform, _) in tubes
        .iter()
        .filter(|(.., inherited)| crate::visibility::is_drawn(*inherited))
    {
        let model = regia_core::glam::Mat4::from(transform.0);
        extracted
            .light_extras
            .area_lights
            .push(crate::light_profiles::AreaLightInput {
                light: crate::light_profiles::tube_emitter(tube, &model),
                range: tube.range,
                cast_shadows: tube.cast_shadows,
            });
    }
    // ADR-3247: on a tier with no emitter budget the rect and tube lights would emit nothing; with the profile's
    // opt-in they stand in as representative-point spots and points (the clustered loop every tier runs).
    if settings.profile.area_light_fallback && settings.quality().emissive_lights == 0 {
        let stand_ins: Vec<LabLocalLight> = extracted
            .light_extras
            .area_lights
            .iter()
            .flat_map(crate::light_profiles::area_light_fallback)
            .collect();
        extracted.light_extras.area_fallback_lights = stand_ins.len() as u32;
        extracted.local_lights.extend(stand_ins);
    }
    for volume in &interiors {
        extracted.interiors.push(*volume);
    }
    if let Some(enclosure) = enclosure.as_deref() {
        extracted.enclosure = Some(enclosure.scene.clone());
        extracted.enclosure_reach = enclosure.reach;
    }

    // The fog's local lights: the strongest at the primary camera (ADR-1365).
    let (eye, reach) = primary_view(&extracted.views).map_or(([0.0; 3], f32::INFINITY), |view| {
        (view.camera.eye, view.camera.far)
    });
    let lights: Vec<crate::FogLight> = match fog_pick {
        Some(mut pick) => {
            let previous: Vec<u64> = pick.chosen.iter().map(|c| c.key).collect();
            let chosen = crate::fog_lights::pick_fog_lights(&fog_candidates, eye, reach, &previous);
            let lights = chosen.iter().map(|c| c.light).collect();
            *pick = crate::fog_lights::FogLightPick {
                chosen,
                candidates: fog_candidates.len(),
                eye,
            };
            lights
        }
        None => crate::fog_lights::pick_fog_lights(&fog_candidates, eye, reach, &[])
            .iter()
            .map(|c| c.light)
            .collect(),
    };
    extracted.outdoor = crate::OutdoorParams {
        time: time.map(|value| *value),
        fog: volumetric_fog.map(|value| *value).unwrap_or_default(),
        volumes: fog_volumes.map(|value| value.volumes.clone()).unwrap_or_default(),
        lights,
        clouds: clouds.as_deref().copied().unwrap_or_default(),
        interiors: extracted.interiors.clone(),
        fog_shadow_probe: fog_probe.as_deref().cloned(),
        ..crate::OutdoorParams::default()
    };
    extracted.valley_mist = valley_mist.map(|mist| crate::ValleyMist::clone(&mist));
    if let Some(level) = level.as_deref() {
        extracted.outdoor.apply_level(&level.atmosphere, clouds.is_some());
    }
    // The haze's Ångström colour (ADR-1408), Earth air only.
    if let Some(aerosol) = aerosol.as_deref()
        && extracted.outdoor.is_earth()
    {
        extracted.outdoor.planet.set_aerosol_angstrom(aerosol.alpha());
    }
    // Weather on the frame (spec 99). The renderer reads these three resources, never `regia_vfx`.
    let airless = level
        .as_deref()
        .is_some_and(|level| level.atmosphere.preset == regia_scene::level::AtmospherePreset::Airless);
    extracted.outdoor.scene_wind = scene_wind
        .as_deref()
        .map(|wind| if airless { None } else { wind.active() });
    extracted.outdoor.surface_weather =
        surface_weather
            .as_deref()
            .map_or(SurfaceWeatherFrame::DRY, |surface| SurfaceWeatherFrame {
                wetness: surface.wetness,
                puddles: surface.puddles,
                snow_cover: surface.snow_cover,
                ripple: surface.ripple,
                clock_s: clouds.as_deref().map_or(0.0, |clouds| clouds.time_s),
            });
    extracted.outdoor.precip_extinction_per_m =
        weather_air.as_deref().map_or(0.0, |air| air.extinction_per_m);
}

/// The fog candidate for the local light just pushed, keyed by its entity.
fn push_fog_candidate(
    candidates: &mut Vec<crate::fog_lights::FogLightCandidate>,
    entity: Entity,
    light: Option<&LabLocalLight>,
) {
    if let Some(light) = light {
        candidates.push(crate::fog_lights::FogLightCandidate {
            key: entity.to_bits(),
            light: crate::FogLight::from_local(light),
        });
    }
}

/// The view the fog's lights are scored from: the window view drawn first (lowest order), else
/// the first view.
fn primary_view(views: &[ExtractedView]) -> Option<&ExtractedView> {
    views
        .iter()
        .filter(|view| matches!(view.target, ExtractedTarget::Window))
        .min_by_key(|view| view.order)
        .or_else(|| views.first())
}

/// What [`extract_glass`] reads per glass entity.
type GlassQueryData = (
    Entity,
    Option<&'static crate::glass::GlassView>,
    Option<&'static crate::glass::GlassDamage>,
    Option<&'static GlobalTransform>,
    crate::visibility::DrawVisibility,
);

/// Glass entities: a mesh with a view mode or damage.
type GlassQueryFilter = (
    With<MeshRenderer>,
    Without<LayerParked>,
    Or<(With<crate::glass::GlassView>, With<crate::glass::GlassDamage>)>,
);

/// Copies every [`crate::GlassView`] / [`crate::GlassDamage`] entity into
/// [`ExtractedFrame::glass`] (runs after [`extract_frame`] and
/// [`crate::glass::apply_glass_damage`]).
#[tracing::instrument(skip_all)]
pub fn extract_glass(
    mut extracted: ResMut<ExtractedFrame>,
    sets: Option<Res<crate::glass::GlassCrackSets>>,
    glass: Query<GlassQueryData, GlassQueryFilter>,
    mut cell_culled: Option<ResMut<crate::viscells::VisCellCulled>>,
) {
    let _span = tracing::debug_span!("extract_glass").entered();
    extracted.glass.clear();
    if glass.is_empty() {
        return;
    }
    let fallback;
    let sets = match sets.as_deref() {
        Some(sets) => sets,
        None => {
            fallback = crate::glass::GlassCrackSets::default();
            &fallback
        }
    };
    for (entity, view, damage, transform, inherited) in &glass {
        if !crate::visibility::is_drawn(inherited) {
            continue;
        }
        // A pane whose colour draw the cell cull left out has nothing to shade (stage 2).
        if let Some(culled) = cell_culled.as_deref_mut()
            && culled.note_glass(entity)
        {
            continue;
        }
        let model = transform.map_or(regia_core::math::Mat4::IDENTITY, |t| {
            regia_core::math::Mat4::from(t.0)
        });
        if let Some(instance) = crate::glass::glass_instance(entity.to_bits(), view, damage, sets, model) {
            extracted.glass.push(instance);
        }
    }
}

/// The open level's colour LUT, loaded from `LevelSettings.look.lut` once per path.
#[derive(Resource, Default)]
pub struct LevelColorLut {
    /// The path the LUT was loaded from (also set when the load failed, so it is not retried
    /// every frame).
    pub path: Option<String>,
    /// The loaded table, `None` for no path or a failed load.
    pub lut: Option<std::sync::Arc<crate::lut::ColorLut>>,
}

/// `PreUpdate`: loads the level's `.cube` / PNG-strip LUT when the path changes. The path
/// resolves through the asset server first (a `.cube` cooked into a pak or the dev cache),
/// then as a loose file under the project root; `\` is accepted as a separator. A failed load
/// warns once and keeps the identity.
pub fn load_level_lut(
    level: Option<Res<regia_scene::level::LevelSettings>>,
    mut assets: Option<ResMut<regia_asset::AssetServer>>,
    mut cache: ResMut<LevelColorLut>,
) {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::COLOR_LUTS);
    let wanted = level.as_deref().and_then(|l| l.look.lut.clone());
    if wanted == cache.path {
        return;
    }
    cache.lut = None;
    cache.path = wanted.clone();
    let Some(path) = wanted else {
        return;
    };
    match crate::lut::load_named(&path, assets.as_deref_mut()) {
        Ok(lut) => cache.lut = Some(lut),
        Err(err) => tracing::warn!(target: "regia_render", "level LUT {path}: {err}; using the identity"),
    }
}

/// Host frames a texture view may go unextracted before its per-view state, its texture and
/// the shared draw's state for it are released (about two seconds at 60 Hz).
pub const TEXTURE_VIEW_RELEASE_FRAMES: u64 = 120;

/// Per texture view: frames since its first and the host frame it was last extracted on.
#[derive(Default)]
pub struct TextureViewClock {
    frame: u64,
    views: std::collections::HashMap<u64, (u64, u64)>,
}

impl TextureViewClock {
    /// Views no longer extracted release their state after a grace period (a host toggling a
    /// screen off and on keeps it).
    fn release_gone(&mut self, renderer: &mut FrameRenderer, now: u64) {
        let gone: Vec<u64> = self
            .views
            .iter()
            .filter(|(_, (_, seen))| now.saturating_sub(*seen) > TEXTURE_VIEW_RELEASE_FRAMES)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.views.remove(&id);
            renderer.drop_texture_view(id);
        }
    }
}

/// The per-instance side tables a texture-view render reads: render overrides (F5), scalar overlays
/// (F10), material shading models (skin, eye, sheen) and compute skinning options (B2).
type ViewSideTables<'w> = (
    Option<Res<'w, crate::ExtractedRenderOverrides>>,
    Option<Res<'w, crate::ExtractedScalarOverlays>>,
    Option<Res<'w, crate::ExtractedMaterialShading>>,
    Option<Res<'w, crate::ExtractedSkinning>>,
);

/// The per-instance side tables of [`sync_view_tables`].
#[derive(Clone, Copy, Default)]
pub struct ViewTables<'a> {
    /// Render overrides (F5).
    pub overrides: Option<&'a crate::ExtractedRenderOverrides>,
    /// Scalar overlays (F10).
    pub scalar_overlays: Option<&'a crate::ExtractedScalarOverlays>,
    /// Material shading models (skin, eye, sheen).
    pub shading: Option<&'a crate::ExtractedMaterialShading>,
    /// Compute skinning options (B2).
    pub skinning: Option<&'a crate::ExtractedSkinning>,
}

/// Hands the renderer what `extracted` holds beside the meshes (overrides, overlays, shading,
/// skinning, the outdoor state, glass, decals, occluders, the interior shell, the IES / LTC /
/// area-light extras): what a view's frame reads and the window frame sets for itself. A host that renders views of its own (XR eyes) calls this first.
pub fn sync_view_tables(renderer: &mut FrameRenderer, extracted: &ExtractedFrame, tables: ViewTables<'_>) {
    renderer.set_gi_proxy_disabled(extracted.gi_proxy_disabled_keys());
    renderer.release_meshes(&extracted.released_mesh_keys);
    renderer.set_render_overrides(tables.overrides.iter().flat_map(|o| o.keys.iter().copied()));
    renderer.set_material_shading(tables.shading.iter().flat_map(|s| s.entries()));
    if let Some(skinning) = tables.skinning {
        renderer.set_compute_skinning(skinning.compute_all);
        renderer.set_skinning_options(skinning.keys.iter().cloned());
    }
    renderer.hold_mesh_identities(extracted.emissive_meshes());
    renderer.set_outdoor(extracted.outdoor.clone());
    renderer.set_outdoor_portals(Some(&extracted.outdoor_portals));
    renderer.set_valley_mist(extracted.valley_mist.clone());
    renderer.set_glass(extracted.glass.clone());
    renderer.set_scalar_overlays(
        tables
            .scalar_overlays
            .iter()
            .flat_map(|o| o.entries.iter().cloned()),
    );
    renderer.set_mask_decals(&extracted.mask_decals);
    renderer.set_mesh_occluders(extracted.mesh_occluders.clone());
    renderer.set_temporal_upscaler(extracted.temporal_upscaler);
    renderer.set_morph_instances(&extracted.morphs);
    renderer.sync_interior_scene(extracted.enclosure.as_ref());
    // IES profiles, LTC spheres and rect/tube lights: a view must not shade with the previous
    // window frame's (or, on a host that draws only texture views, none).
    renderer.set_light_extras(extracted.light_extras.clone());
}

/// `Render`: renders the frame's `RenderTarget::Texture` views into the renderer's persistent
/// textures (screens, mirrors, maps; `specs/13` § 9) before the window, each with its own view
/// state and the reduced feature set ([`crate::FrameRenderer::render_texture_view`],
/// ADR-184). [`crate::TextureViewSettings`] on the camera sets the size, the refresh rate
/// (`update_every`, `paused`) and the features. Publishes [`crate::TextureViewStats`].
#[tracing::instrument(skip_all)]
pub fn render_texture_views(
    device: Option<Res<RenderDevice>>,
    extracted: Option<Res<ExtractedFrame>>,
    mut runtime: ResMut<RuntimeRenderer>,
    mut game_warm: Option<ResMut<crate::GameFrameWarm>>,
    warm: Option<Res<crate::PipelineWarmQueue>>,
    mut clock: Local<TextureViewClock>,
    // Per-instance side tables: render overrides (F5), scalar overlays (F10), material
    // shading models (skin, eye, sheen) and compute skinning options (B2).
    (overrides, scalar_overlays, shading, skinning): ViewSideTables<'_>,
) {
    let _span = tracing::debug_span!("render_texture_views").entered();
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::FRAME);
    let Some(device) = device else { return };
    let Some(extracted) = extracted else { return };
    // The runtime renderer is on a warm worker (a stand-in holds its place, `pipeline_warm`):
    // the views' state is on the worker too, so nothing is drawn or released until it is back.
    // A host's scene prepare holds the views too until its warm decided (ADR-8122): they would
    // upload the map on this thread.
    if game_warm
        .as_deref()
        .is_some_and(crate::GameFrameWarm::holds_scene)
    {
        return;
    }
    clock.frame += 1;
    let now = clock.frame;
    let views = extracted.texture_views();
    if !extracted.is_renderable() {
        // Nothing to draw (no mesh: a menu after a level unloaded). The clock still runs, so the
        // views no longer extracted are released after their grace period; the ones still
        // extracted are kept (audit G render5 #10).
        for (_, view) in &views {
            if let ExtractedTarget::Texture { asset_id } = view.target
                && let Some(entry) = clock.views.get_mut(&asset_id)
            {
                entry.1 = now;
            }
        }
        clock.release_gone(&mut runtime.0, now);
        crate::frame_stats::set_texture_view_stats(crate::TextureViewStats {
            views: views.len() as u32,
            ..crate::TextureViewStats::default()
        });
        return;
    }
    let mut stats = crate::TextureViewStats {
        views: views.len() as u32,
        ..crate::TextureViewStats::default()
    };
    if !views.is_empty() {
        sync_view_tables(
            &mut runtime.0,
            &extracted,
            ViewTables {
                overrides: overrides.as_deref(),
                scalar_overlays: scalar_overlays.as_deref(),
                shading: shading.as_deref(),
                skinning: skinning.as_deref(),
            },
        );
    }
    let default_settings = crate::TextureViewSettings::default();
    for (view_index, view) in views {
        let ExtractedTarget::Texture { asset_id } = view.target else {
            continue;
        };
        let settings = extracted
            .texture_view_settings
            .get(&asset_id)
            .unwrap_or(&default_settings);
        let entry = clock.views.entry(asset_id).or_insert((0, now));
        let age = entry.0;
        *entry = (age + 1, now);
        if !settings.renders_on(age) {
            continue;
        }
        let (width, height) = settings.extent();
        let frame = extracted.frame_scene(view_index, width, height);
        match runtime.0.render_texture_view(&device, asset_id, frame, settings) {
            Ok(timing) => {
                if let Some(queue) = warm.as_deref() {
                    // Offscreen/headless hosts never present a window frame.
                    queue.note_scene_frame();
                }
                stats.rendered += 1;
                stats.cpu_ms += timing.cpu_ms;
                stats.gpu_ms += timing.gpu_ms;
                stats.per_view.push((asset_id, timing.cpu_ms, timing.gpu_ms));
            }
            Err(err) => tracing::warn!(target: "regia_render", "texture view {asset_id} failed: {err}"),
        }
    }
    clock.release_gone(&mut runtime.0, now);
    if stats.rendered > 0
        && let Some(game_warm) = game_warm.as_deref_mut()
    {
        // A headless prepare is `presented` once a frame is submitted after it was resident.
        game_warm.note_offscreen_frame();
    }
    crate::frame_stats::set_texture_view_stats(stats);
}

/// Renders the extracted frame into `target` with the persistent renderer
/// (used for window presents and offscreen agent screenshots alike).
#[tracing::instrument(skip_all)]
pub fn render_extracted_into(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    extracted: &ExtractedFrame,
    target: FrameTarget<'_>,
) -> Result<(), RenderError> {
    renderer.set_outdoor(extracted.outdoor.clone());
    renderer.set_outdoor_portals(Some(&extracted.outdoor_portals));
    renderer.set_valley_mist(extracted.valley_mist.clone());
    renderer.set_glass(extracted.glass.clone());
    renderer.set_mask_decals(&extracted.mask_decals);
    renderer.set_mesh_occluders(extracted.mesh_occluders.clone());
    renderer.sync_interior_scene(extracted.enclosure.as_ref());
    renderer.set_light_extras(extracted.light_extras.clone());
    renderer.set_morph_instances(&extracted.morphs);
    renderer.set_gizmos(Some(extracted.gizmos.clone()));
    renderer.set_gi_proxy_disabled(extracted.gi_proxy_disabled_keys());
    renderer.set_dynamic_gi_tier(extracted.dynamic_gi_tier);
    renderer.set_radiance_cache(extracted.radiance_cache);
    renderer.set_two_phase_occlusion(extracted.two_phase_occlusion);
    renderer.set_shadow_occlusion(extracted.shadow_occlusion);
    renderer.set_masked_occlusion(extracted.masked_occlusion);
    renderer.set_lights_views(&extracted.lights_views);
    renderer.release_meshes(&extracted.released_mesh_keys);
    renderer.hold_mesh_identities(extracted.emissive_meshes());
    let frame = extracted.frame_scene(0, target.width, target.height);
    renderer.bake_pending_local_captures(device, &frame)?;
    renderer.retain_occlusion_views(extracted.views.len().max(1));
    renderer.set_occlusion_view(0);
    renderer.render_into(device, &frame, target)
}

/// Spawns a [`crate::LabScene`] as ECS entities for the runtime path: one entity per
/// mesh (transform + optional skin palette), a camera entity, a directional
/// light entity, plus [`Environment`] and [`RenderSettings`] resources when
/// absent. Returns the camera entity.
///
/// This is how a loaded scene (samples, tests, quick prototypes) gets the full
/// pipeline; games spawn components directly. The settings carry the golden
/// harness's quality baseline so a lab scene renders exactly like its blessed
/// image.
pub fn spawn_lab_scene(world: &mut World, scene: &crate::LabScene) -> Entity {
    use regia_core::math::{Mat4, Quat, Vec3};

    // GlobalTransform is inserted with the correct value so the entities render
    // even before the scene plugin's propagation system has run once.
    for mesh in &scene.meshes {
        let model = Mat4::from_cols_array_2d(&mesh.model);
        let (scale, rotation, translation) = model.to_scale_rotation_translation();
        let transform = Transform {
            translation,
            rotation,
            scale,
        };
        let mut entity = world.spawn((
            regia_scene::components::Name::new("lab_mesh"),
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: mesh.vertices.clone(),
                    indices: mesh.indices.clone(),
                }),
                Arc::new(crate::lab_frame::LabMaterial::clone(
                    &scene.materials.get(mesh.material).cloned().unwrap_or_default(),
                )),
            ),
        ));
        if !mesh.joints.is_empty() {
            entity.insert(SkinJoints {
                palette: mesh.joints.clone(),
            });
        }
    }

    // Camera: look from eye to target.
    let eye = Vec3::from_array(scene.eye);
    let target = Vec3::from_array(scene.target);
    let forward = (target - eye).normalize_or_zero();
    let rotation = Quat::from_rotation_arc(-Vec3::Z, forward);
    let transform = Transform {
        translation: eye,
        rotation,
        scale: Vec3::ONE,
    };
    let camera = world
        .spawn((
            regia_scene::components::Name::new("lab_camera"),
            transform,
            GlobalTransform(transform.compute_affine()),
            Camera {
                fov_y: scene.fov_y,
                near: scene.near,
                far: scene.far,
                exposure_ev: Some(scene.exposure_ev),
                ..Camera::default()
            },
        ))
        .id();

    // Sun: travel direction from the scene's light.
    let dir = Vec3::from_array(scene.directional.direction).normalize_or_zero();
    let rotation = Quat::from_rotation_arc(-Vec3::Z, dir);
    let transform = Transform {
        translation: Vec3::ZERO,
        rotation,
        scale: Vec3::ONE,
    };
    world.spawn((
        regia_scene::components::Name::new("lab_sun"),
        transform,
        GlobalTransform(transform.compute_affine()),
        DirectionalLight {
            direction: scene.directional.direction,
            color: scene.directional.color,
            illuminance: scene.directional.illuminance,
            cast_shadows: scene.directional_shadows,
            static_shadow: false,
        },
    ));

    for light in &scene.local_lights {
        match *light {
            LabLocalLight::Point(point) => {
                let transform = Transform::from_translation(Vec3::from_array(point.position));
                let mut entity = world.spawn((
                    regia_scene::components::Name::new("lab_point_light"),
                    transform,
                    GlobalTransform(transform.compute_affine()),
                    PointLight {
                        position: point.position,
                        color: point.color,
                        intensity: point.intensity,
                        radius: point.radius,
                        cast_shadows: point.cast_shadows,
                    },
                ));
                if let Some(radius_m) = point.source_radius {
                    entity.insert(crate::LightSourceSize { radius_m });
                }
            }
            LabLocalLight::Spot(spot) => {
                let dir = Vec3::from_array(spot.direction).normalize_or_zero();
                let transform = Transform {
                    translation: Vec3::from_array(spot.position),
                    rotation: Quat::from_rotation_arc(-Vec3::Z, dir),
                    scale: Vec3::ONE,
                };
                let mut entity = world.spawn((
                    regia_scene::components::Name::new("lab_spot_light"),
                    transform,
                    GlobalTransform(transform.compute_affine()),
                    SpotLight {
                        position: spot.position,
                        direction: spot.direction,
                        color: spot.color,
                        intensity: spot.intensity,
                        range: spot.range,
                        inner_angle: spot.inner_angle,
                        outer_angle: spot.outer_angle,
                        cast_shadows: spot.cast_shadows,
                    },
                ));
                if let Some(radius_m) = spot.source_radius {
                    entity.insert(crate::LightSourceSize { radius_m });
                }
            }
        }
    }

    if !world.contains_resource::<Environment>() {
        world.insert_resource(Environment {
            ibl: scene.ibl,
            ambient_lux: scene.ambient_lux,
            double_sided: scene.double_sided,
        });
    }
    if !world.contains_resource::<RenderSettings>() {
        let post = scene.post.clone().unwrap_or_default();
        let auto_exposure = post.auto_exposure;
        // The lab baseline: blessed goldens are rendered at this quality.
        let mut settings = RenderSettings {
            profile: RenderProfile::from_preset(RenderProfilePreset::DesktopHigh),
            post,
            fog: crate::FogSettings::default(),
            gi_look: None,
            dynamic_gi_tier: None,
            radiance_cache: None,
            two_phase_occlusion: None,
            shadow_occlusion: None,
            visibility_default: crate::VisibilityDefault::Preset,
        };
        settings.profile.csm_cascade_count = FrameQuality::LAB.csm_cascade_count;
        settings.profile.csm_resolution = FrameQuality::LAB.csm_resolution;
        settings.profile.local_shadow_atlas_size = FrameQuality::LAB.local_shadow_atlas_size;
        settings.profile.local_shadow_max_lights = FrameQuality::LAB.local_shadow_max_lights;
        settings.profile.anti_aliasing = crate::profile::AntiAliasing::None;
        settings.profile.depth_prepass = scene.depth_prepass;
        // New profile columns stay at the golden-harness baseline. Desktop High
        // would otherwise turn on Hi-Z, GTAO and horizontal-plus in the lab.
        settings.profile.render_scale = 1.0;
        settings.profile.ssao = crate::profile::SsaoMode::Off;
        settings.profile.gpu_driven = false;
        settings.profile.horizontal_plus = false;
        settings.profile.shadow_filter = crate::profile::ShadowFilter::Pcf3x3;
        settings.profile.hdr_target = crate::profile::HdrTarget::Rgba16f;
        settings.profile.max_lights_per_cluster = FrameQuality::LAB.max_lights_per_cluster;
        settings.profile.max_reflection_probes = FrameQuality::LAB.max_reflection_probes;
        settings.profile.max_visible_lights = FrameQuality::LAB.max_visible_lights;
        settings.profile.frame_cap_fps = None;
        settings.profile.physical_atmosphere = false;
        settings.profile.volumetric_fog = false;
        settings.profile.volumetric_clouds = false;
        settings.post.auto_exposure = auto_exposure;
        world.insert_resource(settings);
    }
    camera
}

#[cfg(test)]
mod tests {
    #[test]
    fn layers_above_31_are_not_visible() {
        assert!(super::layer_visible(1 << 31, 31));
        assert!(!super::layer_visible(1 << 31, 32));
        assert!(!super::layer_visible(u32::MAX, 40));
        assert!(super::layer_visible(0b10, 1));
        assert!(!super::layer_visible(0b10, 0));
    }

    use super::*;

    #[test]
    fn low_with_tsr_stays_desktop_low() {
        let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopLow);
        settings.profile.temporal_upscaler = crate::TemporalUpscaler::Tsr;
        assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
    }

    #[test]
    fn runtime_upscaling_preserves_every_preset_without_changing_settings() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            for upscaler in [
                crate::TemporalUpscaler::Taau,
                crate::TemporalUpscaler::Tsr,
                crate::TemporalUpscaler::Fsr3,
            ] {
                let seeded = RenderSettings::from_profile(preset);
                for scale in [seeded.profile.render_scale, 0.5, 0.77, 1.0] {
                    let mut settings = seeded.clone();
                    settings.profile.temporal_upscaler = upscaler;
                    settings.profile.render_scale = scale;
                    let before = settings.profile.clone();
                    assert_eq!(settings.preset(), Some(preset), "{preset:?}/{upscaler:?}/{scale}");
                    assert_eq!(
                        settings.profile, before,
                        "classification must not mutate the profile"
                    );
                    // A real quality override still makes this a custom profile.
                    settings.profile.csm_resolution = 123;
                    assert_eq!(settings.preset(), None);
                }
            }
        }
    }

    #[test]
    fn very_low_runtime_upscaling_keeps_low_tier() {
        for upscaler in [
            crate::TemporalUpscaler::Taau,
            crate::TemporalUpscaler::Tsr,
            crate::TemporalUpscaler::Fsr3,
        ] {
            for scale in [1.0 / 3.0, 0.5, 1.0] {
                let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopLow);
                settings.profile = crate::quality::QualityTier::VeryLow.profile();
                settings.profile.temporal_upscaler = upscaler;
                settings.profile.render_scale = scale;
                assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
                settings.profile.csm_resolution = 123;
                assert_eq!(settings.preset(), None);
            }
        }
    }

    /// ADR-3149: the dense-scene default leaves every preset's row alone, writes the measured path
    /// only into Desktop High and Ultra, and never touches a path the game chose.
    #[test]
    fn the_dense_default_switch_keeps_presets_and_game_choices() {
        use crate::{VisibilityDefault as D, VisibilityDefaultState as S, VisibilityPath};
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let off = RenderSettings::from_profile(preset);
            assert_eq!(off.visibility_default, D::Preset, "{preset:?}");
            assert_eq!(off.visibility_default_state(), S::SwitchOff);
            // Every preset row has the path off: the switch is what would change that.
            assert!(!off.profile.visibility_buffer, "{preset:?}");
            assert_eq!(off.profile.visibility_path, VisibilityPath::DEFAULT, "{preset:?}");
            let mut on = off.clone();
            let state = on.set_visibility_default(D::Dense);
            assert_eq!(
                on.preset(),
                off.preset(),
                "{preset:?}: the tier other systems read stays"
            );
            let measured = matches!(
                preset,
                RenderProfilePreset::DesktopHigh | RenderProfilePreset::DesktopUltra
            );
            if measured {
                assert_eq!(state, S::Applied, "{preset:?}");
                assert!(on.profile.visibility_buffer);
                assert_eq!(on.profile.visibility_path, VisibilityPath::DENSE_DEFAULT);
                assert_eq!(on.quality().visibility_path, VisibilityPath::DENSE_DEFAULT);
            } else {
                assert_eq!(state, S::PresetUnmeasured, "{preset:?}");
                assert_eq!(
                    on.profile, off.profile,
                    "{preset:?}: unmeasured rows are untouched"
                );
            }
            // Off again: back to the row exactly.
            assert_eq!(on.set_visibility_default(D::Preset), S::SwitchOff);
            assert_eq!(on.profile, off.profile, "{preset:?}");
        }
    }

    #[test]
    fn the_dense_default_follows_live_preset_changes_and_leaves_game_paths() {
        use crate::{VisibilityDefault as D, VisibilityDefaultState as S, VisibilityPath};
        let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopLow);
        assert_eq!(settings.set_visibility_default(D::Dense), S::PresetUnmeasured);
        settings.apply_preset(RenderProfilePreset::DesktopUltra);
        assert_eq!(settings.visibility_default_state(), S::Applied);
        settings.apply_preset(RenderProfilePreset::MobileHigh);
        assert_eq!(settings.visibility_default_state(), S::PresetUnmeasured);
        assert!(!settings.profile.visibility_buffer);

        // A game on its own dense profile (single pass on): the default neither writes nor reverts.
        let mut game = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        game.profile = RenderProfile::dense_scenes(RenderProfilePreset::DesktopHigh).expect("high");
        let chosen = game.profile.clone();
        assert_eq!(game.set_visibility_default(D::Dense), S::ProfileChoice);
        assert_eq!(game.profile, chosen);
        assert_eq!(game.set_visibility_default(D::Preset), S::SwitchOff);
        assert_eq!(game.profile, chosen);

        // The path switched on by hand with the default resolve is the game's too.
        let mut manual = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        manual.profile.visibility_buffer = true;
        assert_eq!(manual.set_visibility_default(D::Dense), S::ProfileChoice);
        assert_eq!(manual.profile.visibility_path, VisibilityPath::DEFAULT);

        // Runtime upscaling retains the measured preset and its eligibility.
        let mut scaled = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        scaled.profile.render_scale = 0.77;
        scaled.profile.temporal_upscaler = crate::TemporalUpscaler::Tsr;
        assert_eq!(scaled.preset(), Some(RenderProfilePreset::DesktopHigh));
        assert_eq!(scaled.set_visibility_default(D::Dense), S::Applied);
        assert_eq!(scaled.profile.render_scale, 0.77);
        assert_eq!(scaled.profile.temporal_upscaler, crate::TemporalUpscaler::Tsr);

        // A genuine quality-column customization is not a measured preset.
        let mut custom = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        custom.profile.csm_resolution = 123;
        assert_eq!(custom.preset(), None);
        assert_eq!(custom.set_visibility_default(D::Dense), S::PresetUnmeasured);
        assert!(!custom.profile.visibility_buffer);
    }

    fn tri() -> CpuMesh {
        CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3],
            indices: vec![0, 1, 2],
        }
    }

    /// A bump of a mesh's content revision reaches its LOD, shadow-LOD and dithered draws
    /// (corr-render-rest LOW 17): those look up `lod_variant_key(base, ..)`, which the bump
    /// used to miss, so an edited mesh drawn at LOD 1 kept its old bytes on the GPU.
    #[test]
    fn a_content_bump_reaches_every_lod_variant_of_the_mesh() {
        let a = Arc::new(tri());
        let twin = Arc::new(tri());
        let (key, twin_key) = (mesh_key(&a), mesh_key(&twin));
        note_mesh_content_changed(&a);
        assert_eq!(mesh_content_revision(key), 1);
        for lod in 0..4 {
            for shadow in [false, true] {
                let variant = crate::mesh_lod::lod_variant_key(key, crate::mesh_lod_cache_key(lod, shadow));
                assert_eq!(mesh_content_revision(variant), 1, "lod {lod} shadow {shadow}");
                assert_eq!(
                    mesh_content_revision(variant ^ crate::frame::LOD_DITHER_MESH_TAG),
                    1,
                    "dithered lod {lod} shadow {shadow}"
                );
                let other =
                    crate::mesh_lod::lod_variant_key(twin_key, crate::mesh_lod_cache_key(lod, shadow));
                assert_eq!(
                    mesh_content_revision(other),
                    0,
                    "the twin's lod {lod} shadow {shadow}"
                );
            }
        }
    }

    #[test]
    fn mesh_keys_are_generation_ids_per_arc() {
        let mut registry = MeshRegistry::default();
        let a = Arc::new(tri());
        let key_a = registry.key(&a);
        assert_eq!(key_a >> 56, MESH_ID_KEY_TAG >> 56);
        assert_eq!(key_a & 0b111, 0, "LOD bits and bit 0 stay clear");
        assert!(!crate::frame::is_shadow_lod_key(key_a));
        assert_ne!(key_a >> 56, crate::EPHEMERAL_KEY >> 56);
        // Clones share the key; another Arc of the same content does not.
        assert_eq!(registry.key(&Arc::clone(&a)), key_a);
        let twin = Arc::new(tri());
        let key_twin = registry.key(&twin);
        assert_ne!(key_twin, key_a);
        let global = mesh_key(&a);
        assert_eq!(mesh_content_revision(global), 0);
        note_mesh_content_changed(&a);
        assert_eq!(mesh_content_revision(global), 1);
        assert_eq!(mesh_refresh(false, 1, 0), MeshRefresh::Rewrite);
        assert_eq!(mesh_refresh(false, 1, 1), MeshRefresh::Keep);
        assert_eq!(mesh_refresh(true, 1, 0), MeshRefresh::Upload);
        // The twin is a different Arc: noting one mesh does not move the other.
        assert_eq!(mesh_content_revision(mesh_key(&twin)), 0);
        // Every LOD / shadow variant of one key differs from every other mesh's key.
        for lod in 0..4 {
            for shadow in [false, true] {
                assert_ne!(
                    crate::mesh_lod::lod_variant_key(key_a, crate::mesh_lod_cache_key(lod, shadow)),
                    key_twin
                );
            }
        }
        // Drop `a`: its key is released once and never handed out again, whatever address
        // the next Arc lands at.
        drop(a);
        registry.sweep();
        assert_eq!(std::mem::take(&mut registry.released), vec![key_a]);
        let mut seen = std::collections::HashSet::from([key_a, key_twin]);
        for _ in 0..64 {
            let next = Arc::new(tri());
            assert!(seen.insert(registry.key(&next)), "a key was reused");
        }
        registry.sweep();
        assert_eq!(registry.released.len(), 64);
        assert_eq!(registry.by_addr.len(), 1, "only `twin` is live");
        assert_eq!(registry.key(&twin), key_twin);
    }

    /// The registry `Weak` blocks [`Arc::get_mut`]. An in-place edit still keeps the generation id.
    #[test]
    fn unique_edit_keeps_the_key_when_the_registry_holds_a_weak() {
        let mesh = Arc::new(tri());
        let key = mesh_key(&mesh);
        let mut mesh = mesh;
        assert!(
            Arc::get_mut(&mut mesh).is_none(),
            "the registry weak blocks get_mut"
        );
        let edited = with_unique_mesh_mut(&mut mesh, |cpu| {
            cpu.indices.push(7);
            cpu.indices.len()
        });
        assert_eq!(edited, Some(4));
        assert_eq!(mesh.indices.len(), 4);
        assert_eq!(
            mesh_key(&mesh),
            key,
            "the generation id stays with the allocation"
        );
        let mut shared = Arc::clone(&mesh);
        assert!(with_unique_mesh_mut(&mut shared, |_| ()).is_none());
        assert_eq!(mesh_key(&mesh), key);
    }

    #[test]
    fn the_registry_pins_the_arc_allocation() {
        // While registered, a dropped Arc's address cannot be reused: the Weak holds it.
        let mut registry = MeshRegistry::default();
        let a = Arc::new(tri());
        let addr = Arc::as_ptr(&a) as usize;
        registry.key(&a);
        drop(a);
        let live: Vec<Arc<CpuMesh>> = (0..256).map(|_| Arc::new(tri())).collect();
        assert!(live.iter().all(|m| Arc::as_ptr(m) as usize != addr));
    }

    #[test]
    fn level_components_are_registered_for_scene_files() {
        use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};
        let mut app = App::new();
        register_render_types(&mut app);
        let registry = app.world().resource::<AppTypeRegistry>().read();
        for path in [
            "regia_render::runtime::MeshSource",
            "regia_render::camera::Camera",
            "regia_render::lights::DirectionalLight",
            "regia_render::lights::PointLight",
            "regia_render::lights::SpotLight",
            // ADR-2313: a baked level keeps its receivers.
            "regia_render::lightmap::LightmapReceiver",
        ] {
            let registration = registry
                .get_with_type_path(path)
                .unwrap_or_else(|| panic!("{path} is not registered"));
            assert!(
                registration.data::<ReflectComponent>().is_some(),
                "{path} reflects as a component"
            );
            assert!(
                registration.data::<ReflectDefault>().is_some(),
                "{path} has a reflected default"
            );
        }
        // Scene snapshots (save, play-in-editor) and the inspector copy components through
        // `reflect_clone`; the derived field-wise clone fails on array fields.
        let values: [Box<dyn Reflect>; 6] = [
            Box::new(MeshSource::default()),
            Box::new(crate::lightmap::LightmapReceiver::default()),
            Box::new(crate::Camera::default()),
            Box::new(DirectionalLight::default()),
            Box::new(PointLight::default()),
            Box::new(SpotLight::default()),
        ];
        for value in values {
            assert!(
                value.reflect_clone().is_ok(),
                "{} clones through reflection",
                value.reflect_type_path()
            );
        }
    }

    #[test]
    fn extraction_feeds_the_fog_the_strongest_lights_in_candela() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.init_resource::<crate::FogLightPick>();
        app.add_systems(Render, extract_frame);
        app.world_mut().spawn((
            Transform::default(),
            GlobalTransform::default(),
            Camera::default(),
        ));
        // Candela at the camera / max(d², 16): 50, 20, 10, 250, 40, 100 (lumens = cd × 4π).
        let four_pi = 4.0 * std::f32::consts::PI;
        let mut entities = Vec::new();
        for (x, candela) in [
            (8.0, 3_200.0),
            (10.0, 2_000.0),
            (-30.0, 9_000.0),
            (2.0, 4_000.0),
            (-6.0, 1_440.0),
            (12.0, 14_400.0),
        ] {
            let transform = Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0));
            let entity = app
                .world_mut()
                .spawn((
                    transform,
                    GlobalTransform(transform.compute_affine()),
                    PointLight {
                        position: [x, 0.0, 0.0],
                        color: [1.0; 3],
                        intensity: candela * four_pi,
                        radius: 4.0,
                        cast_shadows: false,
                    },
                ))
                .id();
            entities.push(entity.to_bits());
        }
        let want: Vec<u64> = [3, 5, 0, 4].iter().map(|&i| entities[i]).collect();
        for frame in 0..3 {
            app.update();
            let pick = app.world().resource::<crate::FogLightPick>();
            let keys: Vec<u64> = pick.chosen.iter().map(|c| c.key).collect();
            assert_eq!(keys, want, "frame {frame}");
            assert_eq!(pick.candidates, 6);
            assert_eq!(pick.chosen.iter().all(|c| c.held), frame > 0, "frame {frame}");
            let outdoor = &app.world().resource::<ExtractedFrame>().outdoor;
            assert_eq!(outdoor.lights.len(), 4);
            // The strongest is 4000 cd at 2 m, inside its 4 m radius.
            assert!(
                (outdoor.lights[0].intensity - 4_000.0).abs() < 0.05,
                "{:?}",
                outdoor.lights[0]
            );
            assert!(outdoor.lights.iter().all(|l| l.cone.is_none()));
        }
        // A 1000 cd spot reaches the fog as 1000 cd with its cone; it outranks the others.
        let transform = Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, 0.0));
        let spot = app
            .world_mut()
            .spawn((
                transform,
                GlobalTransform(transform.compute_affine()),
                SpotLight {
                    intensity: 100_000.0,
                    ..SpotLight::default()
                },
            ))
            .id();
        app.update();
        let pick = app.world().resource::<crate::FogLightPick>();
        let chosen = pick.chosen.last().expect("four lights");
        assert_eq!(chosen.key, spot.to_bits(), "the spot takes the weakest slot");
        assert!(
            (chosen.light.intensity - 100_000.0).abs() < 0.5,
            "{:?}",
            chosen.light
        );
        let cone = chosen.light.cone.expect("spot cone");
        assert!((cone.direction[2] + 1.0).abs() < 1.0e-5, "entity -Z, {cone:?}");
        assert!((cone.cos_outer - 30f32.to_radians().cos()).abs() < 1.0e-6);
        let keys: Vec<u64> = pick.chosen.iter().take(3).map(|c| c.key).collect();
        assert_eq!(keys, want[..3].to_vec(), "the kept lights keep their slots");
    }

    /// Audit G finding 2 (render5 determinism): the frame's sun, primary view and local-light
    /// order must follow `Entity::to_bits`, never ECS query (archetype) order. `marker` moves
    /// the lower-entity items to a later archetype, so a query iterates them last; extraction
    /// must still pick them.
    #[test]
    fn extraction_ties_break_by_entity_not_query_order() {
        #[derive(regia_ecs::ecs::component::Component)]
        struct Marker;

        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);

        // Two directional lights; the one with the lower entity id casts shadows and is brighter.
        let north = app
            .world_mut()
            .spawn(DirectionalLight {
                direction: [0.0, -1.0, 0.2],
                color: [1.0; 3],
                illuminance: 100_000.0,
                cast_shadows: true,
                static_shadow: false,
            })
            .id();
        let west = app
            .world_mut()
            .spawn(DirectionalLight {
                direction: [-1.0, -0.4, 0.0],
                color: [1.0; 3],
                illuminance: 50_000.0,
                cast_shadows: false,
                static_shadow: false,
            })
            .id();
        let sun_low = north.min(west);
        let low_id = sun_low;
        // Two equal-order window cameras at different eyes.
        let transform_a = Transform::from_translation(regia_core::math::Vec3::new(1.0, 0.0, 0.0));
        let camera_a = app
            .world_mut()
            .spawn((
                transform_a,
                GlobalTransform(transform_a.compute_affine()),
                Camera::default(),
            ))
            .id();
        let transform_b = Transform::from_translation(regia_core::math::Vec3::new(2.0, 0.0, 0.0));
        let camera_b = app
            .world_mut()
            .spawn((
                transform_b,
                GlobalTransform(transform_b.compute_affine()),
                Camera::default(),
            ))
            .id();
        let cam_low = if camera_a.index_u32() < camera_b.index_u32() {
            camera_a
        } else {
            camera_b
        };
        let cam_bits = if camera_a.to_bits() < camera_b.to_bits() {
            camera_a
        } else {
            camera_b
        };
        assert_ne!(
            cam_low, cam_bits,
            "index order and Entity::to_bits disagree; the primary must follow the index"
        );
        let cam_low_eye = app
            .world()
            .get::<GlobalTransform>(cam_low)
            .expect("camera transform")
            .0
            .translation;
        // Two point lights at distinct positions (extraction reads the GlobalTransform).
        let light_a_transform = Transform::from_translation(regia_core::math::Vec3::new(4.0, 1.0, 0.0));
        let light_a = app
            .world_mut()
            .spawn((
                light_a_transform,
                GlobalTransform(light_a_transform.compute_affine()),
                PointLight {
                    position: [4.0, 1.0, 0.0],
                    ..PointLight::default()
                },
            ))
            .id();
        let light_b_transform = Transform::from_translation(regia_core::math::Vec3::new(8.0, 1.0, 0.0));
        let light_b = app
            .world_mut()
            .spawn((
                light_b_transform,
                GlobalTransform(light_b_transform.compute_affine()),
                PointLight {
                    position: [8.0, 1.0, 0.0],
                    ..PointLight::default()
                },
            ))
            .id();
        let light_low = if light_a.to_bits() < light_b.to_bits() {
            (light_a, [4.0f32, 1.0, 0.0])
        } else {
            (light_b, [8.0f32, 1.0, 0.0])
        };

        // Move the lower-entity items to a later archetype: queries now iterate them last.
        for entity in [sun_low, cam_low, light_low.0] {
            app.world_mut().entity_mut(entity).insert(Marker);
        }

        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let low_light = app
            .world()
            .get::<DirectionalLight>(low_id)
            .expect("the low light");
        let sun = extracted.directional.expect("a sun");
        assert!(
            (sun.illuminance - low_light.illuminance).abs() < 1.0,
            "the lower-entity light is the sun, got {sun:?} want {}",
            low_light.illuminance
        );
        assert_eq!(
            extracted.directional_shadows, low_light.cast_shadows,
            "shadows follow the lower-entity sun"
        );
        let primary = extracted.primary_view().expect("a primary view");
        assert!(
            (primary.camera.eye[0] - cam_low_eye.x).abs() < 1.0e-5,
            "the lower-entity camera is the primary view, got {:?}",
            primary.camera.eye
        );
        assert_eq!(extracted.local_lights.len(), 2, "both point lights extracted");
        let first_pos = match &extracted.local_lights[0] {
            crate::LabLocalLight::Point(p) => p.position,
            other => panic!("expected a point light, got {other:?}"),
        };
        assert!(
            (first_pos[0] - light_low.1[0]).abs() < 1.0e-5,
            "the lower-entity light sorts first, got {first_pos:?}"
        );
    }

    #[test]
    fn extraction_leaves_parked_layer_members_out() {
        use regia_scene::layers::LayerParked;
        let parked = LayerParked {
            restore: regia_scene::prelude::Visibility::Inherited,
            inherited: false,
        };
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ));
        let member = app
            .world_mut()
            .spawn((
                Transform::default(),
                MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
                parked,
            ))
            .id();
        app.world_mut().spawn((
            Transform::default(),
            GlobalTransform::default(),
            PointLight::default(),
            parked,
        ));
        app.world_mut().spawn((
            Transform::default(),
            GlobalTransform::default(),
            SpotLight::default(),
            parked,
        ));
        app.world_mut().spawn((Transform::default(), Camera::default()));
        app.add_systems(
            Render,
            (extract_frame, move |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                assert_eq!(extracted.meshes.len(), 1, "the parked mesh stays out");
                assert!(extracted.meshes.iter().all(|m| m.entity != member));
                assert!(extracted.local_lights.is_empty(), "parked lights stay out");
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn extraction_pulls_components_into_a_frame_scene() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(1.0, 2.0, 3.0)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ));
        app.world_mut().spawn((
            Transform::default(),
            Camera {
                exposure_ev: Some(14.0),
                ..Camera::default()
            },
        ));
        app.world_mut().spawn((
            Transform::default(),
            DirectionalLight {
                direction: [0.0, -1.0, 0.0],
                color: [1.0, 1.0, 1.0],
                illuminance: 100_000.0,
                cast_shadows: true,
                static_shadow: false,
            },
        ));
        let key = mesh_key(&mesh);
        app.add_systems(
            Render,
            (extract_frame, move |world: &mut World| {
                // Verify extraction right after it ran.
                let extracted = world.resource::<ExtractedFrame>();
                assert!(extracted.is_renderable(), "camera + mesh extracted");
                let frame = extracted.frame_scene(0, 32, 32);
                assert_eq!(frame.meshes.len(), 1);
                assert_eq!(frame.meshes[0].mesh_key, key);
                assert_eq!(frame.exposure_ev, 14.0, "camera EV wins");
                assert!(!frame.post.as_ref().expect("post").auto_exposure);
                assert_eq!(frame.directional.illuminance, 100_000.0);
                assert!(frame.directional_shadows);
                // Desktop-high default profile: 2048² CSM, 4 cascades.
                assert_eq!(frame.quality.csm_resolution, 2048);
                assert_eq!(frame.quality.csm_cascade_count, 4);
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn spec14_a1_static_marker_extracts_into_frame_meshes() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            regia_scene::components::Static,
        ));
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ));
        app.world_mut().spawn((Transform::default(), Camera::default()));
        app.add_systems(
            Render,
            (extract_frame, move |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                let frame = extracted.frame_scene(0, 32, 32);
                assert_eq!(frame.meshes.len(), 2);
                assert_eq!(
                    frame.meshes.iter().filter(|mesh| mesh.is_static).count(),
                    1,
                    "exactly the `Static` entity extracts with is_static"
                );
                assert_eq!(
                    extracted.meshes.iter().filter(|mesh| mesh.is_static).count(),
                    1,
                    "the ExtractedMesh carries the marker too"
                );
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn not_shadow_caster_maps_to_shadow_off_and_default_stays_all() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        // Default: casts a shadow everywhere.
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ));
        // Marker: casts no shadow.
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            NotShadowCaster,
        ));
        app.world_mut().spawn((Transform::default(), Camera::default()));
        app.add_systems(
            Render,
            (extract_frame, move |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                let frame = extracted.frame_scene(0, 32, 32);
                assert_eq!(frame.meshes.len(), 2);
                let shadows: Vec<crate::ShadowCast> = frame.meshes.iter().map(|m| m.shadow).collect();
                assert_eq!(
                    shadows.iter().filter(|s| **s == crate::ShadowCast::All).count(),
                    1,
                    "only the unmarked default mesh casts a shadow"
                );
                assert_eq!(
                    shadows.iter().filter(|s| **s == crate::ShadowCast::Off).count(),
                    1,
                    "the marker turns the shadow off"
                );
                assert!(
                    extracted.meshes.iter().all(|mesh| mesh.gi_proxy),
                    "NotShadowCaster does not opt the mesh out of GI"
                );
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn gi_proxy_false_keeps_the_shadow_and_opts_the_mesh_out() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        let solid = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let bit = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        assert!(MeshRenderer::new(Arc::clone(&solid), Arc::clone(&material)).gi_proxy);
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&solid), Arc::clone(&material)),
        ));
        let mut opted = MeshRenderer::new(Arc::clone(&bit), Arc::clone(&material));
        opted.gi_proxy = false;
        app.world_mut().spawn((Transform::default(), opted));
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(solid, material),
            NotShadowCaster,
        ));
        app.world_mut().spawn((Transform::default(), Camera::default()));
        let bit_key = mesh_key(&bit);
        app.add_systems(
            Render,
            (extract_frame, move |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                let frame = extracted.frame_scene(0, 32, 32);
                assert_eq!(frame.meshes.len(), 3);
                let disabled: Vec<u64> = extracted.gi_proxy_disabled_keys().collect();
                assert_eq!(disabled, vec![bit_key]);
                let mut shadows_off = 0;
                for mesh in &extracted.meshes {
                    let key = mesh_key(&mesh.mesh);
                    if key == bit_key {
                        assert!(!mesh.gi_proxy);
                        assert_eq!(mesh.shadow, crate::ShadowCast::All);
                    } else {
                        assert!(mesh.gi_proxy);
                    }
                    if mesh.shadow == crate::ShadowCast::Off {
                        shadows_off += 1;
                    }
                }
                assert_eq!(shadows_off, 1, "only NotShadowCaster drops the shadow");
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn a_level_lut_loads_once_and_reaches_the_view_post_settings() {
        let dir = std::env::temp_dir().join(format!("regia-level-lut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let cube = dir.join("warm.cube");
        let mut text = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text.push_str(&format!("{} {g} {b}\n", (r as f32 * 0.9)));
                }
            }
        }
        std::fs::write(&cube, text).expect("write cube");
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.init_resource::<LevelColorLut>();
        let mut level = regia_scene::level::LevelSettings::default();
        level.look.lut = Some(cube.display().to_string());
        app.insert_resource(level);
        app.world_mut().spawn((Transform::default(), Camera::default()));
        app.add_systems(Render, (load_level_lut, extract_frame).chain());
        app.update();
        let lut = app
            .world()
            .resource::<LevelColorLut>()
            .lut
            .clone()
            .expect("loaded");
        assert_eq!(lut.size(), 2);
        let extracted = app.world().resource::<ExtractedFrame>();
        let view_lut = extracted.views[0].post.lut.clone().expect("view carries the LUT");
        assert_eq!(view_lut.key(), lut.key());
        // A missing file keeps the identity (no LUT) and does not retry every frame.
        let mut level = regia_scene::level::LevelSettings::default();
        level.look.lut = Some(dir.join("missing.cube").display().to_string());
        app.insert_resource(level);
        app.update();
        assert!(app.world().resource::<LevelColorLut>().lut.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cooked-cache half of the resolution a packaged client relies on (the pak read is the
    /// same `read_cooked` path): a relative `looks/hall.cube` that exists only as a cooked
    /// asset — no loose file anywhere — loads, and a Windows-authored `looks\hall.cube`
    /// resolves to the same grade.
    #[test]
    fn a_level_lut_served_only_from_the_cooked_cache_loads_and_backslash_matches() {
        let root = regia_test_utils::unique_temp_dir("level-lut-cooked");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let id = regia_asset::AssetId::new_random();
        let mut text = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text.push_str(&format!("{} {g} {b}\n", (r as f32 * 0.9)));
                }
            }
        }
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of("regia_render::lut::ColorLut"),
            asset_id: id,
            importer_version: regia_asset::pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: text.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, text.as_bytes()).expect("encode");
        std::fs::write(cache.join(regia_asset::cooked_file_name(id)), &file).expect("cooked");
        let manifest = regia_asset::AssetManifest {
            entries: vec![regia_asset::ManifestEntry {
                path: "looks/hall.cube".to_string(),
                id,
                type_path: "regia_render::lut::ColorLut".to_string(),
            }],
        };
        manifest.write_to_cache(&cache).expect("manifest");
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.init_resource::<LevelColorLut>();
        app.insert_resource(server);
        let mut level = regia_scene::level::LevelSettings::default();
        level.look.lut = Some("looks/hall.cube".to_string());
        app.insert_resource(level);
        app.world_mut().spawn((Transform::default(), Camera::default()));
        app.add_systems(Render, (load_level_lut, extract_frame).chain());
        app.update();
        let lut = app
            .world()
            .resource::<LevelColorLut>()
            .lut
            .clone()
            .expect("the cooked LUT loads");
        assert_eq!(lut.size(), 2);
        let extracted = app.world().resource::<ExtractedFrame>();
        let view_lut = extracted.views[0].post.lut.clone().expect("view carries the LUT");
        assert_eq!(view_lut.key(), lut.key());
        // The same grade through a Windows-authored separator (nothing loose to open).
        app.world_mut().resource_mut::<LevelColorLut>().path = None;
        let mut level = regia_scene::level::LevelSettings::default();
        level.look.lut = Some("looks\\hall.cube".to_string());
        app.insert_resource(level);
        app.update();
        let backslash = app
            .world()
            .resource::<LevelColorLut>()
            .lut
            .clone()
            .expect("the backslash path resolves to the cooked LUT");
        assert_eq!(backslash.key(), lut.key());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_camera_inside_an_interior_volume_takes_the_interior_shade_for_its_box() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        app.world_mut()
            .spawn((Transform::default(), MeshRenderer::new(mesh, material)));
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.2, 0.4, -0.3)),
            Camera::default(),
        ));
        app.world_mut()
            .spawn(InteriorVolume::with_floor([0.0, 0.5, 0.0], [2.0, 1.2, 3.0]));
        app.add_systems(
            Render,
            (extract_frame, |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                let frame = extracted.frame_scene(0, 32, 32);
                assert!(frame.quality.interior_ambient);
                // The sky stays bound for the exterior; the renderer drops it for
                // pixels inside the volume, which reaches it through the outdoor params.
                assert!(frame.ibl);
                assert_eq!(extracted.outdoor.interiors.len(), 1);
                assert!(extracted.outdoor.interiors[0].contains(frame.eye));
                assert!((frame.ambient_lux - crate::INTERIOR_AMBIENT_FLOOR_LUX).abs() < 1.0e-3);
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn extraction_calls_detect_enclosure_when_no_volume_contains_the_eye() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(mesh, Arc::new(CpuMaterial::default())),
        ));
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.1, 0.2, -0.2)),
            Camera::default(),
        ));
        app.insert_resource(crate::EnclosureScene {
            scene: crate::hollow_box([0.0, 0.4, 0.0], [1.4, 1.0, 1.6], 0.08, [0.6, 0.58, 0.55]),
            reach: 4.0,
        });
        app.add_systems(
            Render,
            (extract_frame, |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                let frame = extracted.frame_scene(0, 32, 32);
                assert!(
                    frame.quality.interior_ambient,
                    "an enclosed eye with no volume still takes the interior shade"
                );
                assert!(!frame.ibl);
                assert!((frame.ambient_lux - crate::INTERIOR_AMBIENT_FLOOR_LUX).abs() < 1.0e-3);
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn extraction_collects_multiple_window_views_in_order() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        let material = Arc::new(CpuMaterial::default());
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ));
        // Two window cameras: right half draws first (order -1), left half second.
        app.world_mut().spawn((
            Transform::default(),
            Camera {
                order: -1,
                viewport: [0.5, 0.0, 0.5, 1.0],
                ..Camera::default()
            },
        ));
        app.world_mut().spawn((
            Transform::default(),
            Camera {
                viewport: [0.0, 0.0, 0.5, 1.0],
                ..Camera::default()
            },
        ));
        // A texture camera must not join the window list.
        app.world_mut().spawn((
            Transform::default(),
            Camera {
                viewport: [0.0, 0.0, 1.0, 1.0],
                target: RenderTarget::Texture { asset_id: 7 },
                ..Camera::default()
            },
        ));
        // A layer-1 mesh visible only to cameras with that bit.
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer {
                mesh: Arc::clone(&mesh),
                material: Arc::clone(&material),
                layer: 1,
                decal_layer: 0,
                gi_proxy: true,
            },
        ));
        app.add_systems(
            Render,
            (extract_frame, move |world: &mut World| {
                let extracted = world.resource::<ExtractedFrame>();
                let views = extracted.window_views();
                assert_eq!(views.len(), 2, "texture views are separate");
                assert_eq!(views[0].1.viewport, [0.5, 0.0, 0.5, 1.0], "order sorts first");
                assert_eq!(views[1].1.viewport, [0.0, 0.0, 0.5, 1.0]);
                assert_eq!(extracted.texture_views().len(), 1);
                // Layer mask filters per view: default mask (all bits) sees both
                // meshes; a layer-1-only mask sees one.
                let all = extracted.frame_scene(views[0].0, 32, 32);
                assert_eq!(all.meshes.len(), 2);
                let mut masked = extracted.clone();
                masked.views[views[0].0].layer_mask = 0b10;
                let frame = masked.frame_scene(views[0].0, 32, 32);
                assert_eq!(frame.meshes.len(), 1, "layer-1 mesh only");
            })
                .chain(),
        );
        app.update();
    }

    #[test]
    fn deck_cap_leaves_an_existing_cap_and_vrr() {
        let mut app = App::new();
        app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::Deck));
        app.insert_resource(crate::OutputSettings {
            present: crate::PresentPolicy::Vrr,
            hdr: crate::HdrOutput::Sdr,
            frame_cap_fps: Some(120),
        });
        app.insert_resource(regia_app::FramePacing::new(Some(120.0)));
        app.add_systems(PreUpdate, sync_profile_frame_cap);
        app.update();
        let output = app.world().resource::<crate::OutputSettings>();
        assert_eq!(output.frame_cap_fps, Some(120));
        assert_eq!(output.present, crate::PresentPolicy::Vrr);
        assert_eq!(
            app.world().resource::<regia_app::FramePacing>().target_fps,
            Some(120.0)
        );

        let mut fresh = App::new();
        fresh.insert_resource(RenderSettings::from_profile(RenderProfilePreset::Deck));
        fresh.insert_resource(crate::OutputSettings::default());
        fresh.insert_resource(regia_app::FramePacing::default());
        fresh.add_systems(PreUpdate, sync_profile_frame_cap);
        fresh.update();
        let output = fresh.world().resource::<crate::OutputSettings>();
        assert_eq!(
            output.frame_cap_fps,
            Some(RenderProfilePreset::DECK_FRAME_CAP_FPS)
        );
        assert_eq!(output.present, crate::PresentPolicy::Vsync);
        assert_eq!(
            fresh.world().resource::<regia_app::FramePacing>().target_fps,
            Some(40.0)
        );
    }

    #[test]
    fn extraction_copies_fog_onto_the_frame_scene() {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.add_systems(Render, extract_frame);
        let mesh = Arc::new(CpuMesh {
            vertices: vec![LabVertex::from_pos_normal_uv(
                [0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0],
            )],
            indices: vec![0, 0, 0],
        });
        app.world_mut().spawn((
            Transform::default(),
            MeshRenderer::new(mesh, Arc::new(CpuMaterial::default())),
        ));
        app.world_mut().spawn((Transform::default(), Camera::default()));
        let settings = RenderSettings {
            fog: crate::FogSettings::outdoor_haze(),
            ..RenderSettings::default()
        };
        app.insert_resource(settings);
        app.add_systems(
            Render,
            (extract_frame, |world: &mut World| {
                let frame = world.resource::<ExtractedFrame>().frame_scene(0, 32, 32);
                assert!(frame.fog.enabled, "fog settings reach the runtime frame");
                assert!((frame.fog.density - crate::FogSettings::outdoor_haze().density).abs() < 1e-6);
            })
                .chain(),
        );
        app.update();
    }

    /// ADR-3118: a profile with cluster residency on still reads as its preset.
    #[test]
    fn cluster_residency_keeps_the_preset() {
        let mut settings = RenderSettings::from_profile(RenderProfilePreset::Deck);
        settings.profile.cluster_residency =
            crate::visbuffer::ClusterResidency::for_preset(RenderProfilePreset::Deck);
        assert_eq!(settings.preset(), Some(RenderProfilePreset::Deck));
    }

    /// Audit G render5 #1: a material's cache key must not be its `Arc` address. The renderer
    /// keeps a material's textures by key, so a new material allocated where a dropped one lived
    /// would draw with the old one's albedo and normal maps until the entry was evicted.
    #[test]
    fn a_material_allocated_at_a_freed_address_gets_a_new_key() {
        let entity = Entity::PLACEHOLDER;
        let mesh = Arc::new(tri());
        let frame_key = |material: &Arc<CpuMaterial>| {
            let frame = ExtractedFrame {
                meshes: vec![ExtractedMesh {
                    is_static: false,
                    entity,
                    mesh: Arc::clone(&mesh),
                    material: Arc::clone(material),
                    layer: 0,
                    model: [
                        [1.0, 0.0, 0.0, 0.0],
                        [0.0, 1.0, 0.0, 0.0],
                        [0.0, 0.0, 1.0, 0.0],
                        [0.0, 0.0, 0.0, 1.0],
                    ],
                    joints: Vec::new(),
                    decal_layer: 0,
                    uv1: None,
                    shadow: crate::ShadowCast::All,
                    draw_indices: None,
                    lod_key: 0,
                    object_key_xor: 0,
                    gi_proxy: true,
                    lod_dither: 0.0,
                }],
                ..ExtractedFrame::default()
            };
            let scene = frame.frame_scene(0, 8, 8);
            scene.meshes[0].material_key
        };
        let a = Arc::new(CpuMaterial {
            base_color: [1.0, 0.0, 0.0, 1.0],
            ..CpuMaterial::default()
        });
        let key_a = frame_key(&a);
        assert_eq!(
            key_a,
            crate::shading_model::material_key(&a),
            "one key on every path"
        );
        assert_eq!(frame_key(&Arc::clone(&a)), key_a, "clones share the key");
        let address = Arc::as_ptr(&a) as usize;
        drop(a);
        // Allocate until one lands at the freed address (the allocator usually hands it back at
        // once); keep the misses alive so the search moves on. Every key must differ from A's.
        let mut kept = Vec::new();
        for _ in 0..256 {
            let b = Arc::new(CpuMaterial {
                base_color: [0.0, 1.0, 0.0, 1.0],
                ..CpuMaterial::default()
            });
            let key_b = frame_key(&b);
            assert_ne!(key_b, key_a, "a new material drew under a dropped material's key");
            assert_eq!(key_b, crate::shading_model::material_key(&b));
            if Arc::as_ptr(&b) as usize == address {
                break;
            }
            kept.push(b);
        }
    }

    /// ADR-3190: the extracted sun is a point (word 0, every preset's frame unchanged) until the
    /// profile opts in, then it carries the sun's angular radius for the forward's specular.
    #[test]
    fn extraction_lights_the_sun_specular_as_a_disc_only_when_the_profile_opts_in() {
        for opt_in in [false, true] {
            let mut app = App::new();
            app.init_resource::<ExtractedFrame>();
            let mut settings = RenderSettings::default();
            settings.profile.sun_specular_disc = opt_in;
            app.insert_resource(settings);
            app.world_mut().spawn((
                Transform::default(),
                DirectionalLight {
                    direction: [0.0, -1.0, 0.0],
                    color: [1.0, 1.0, 1.0],
                    illuminance: 100_000.0,
                    cast_shadows: false,
                    static_shadow: false,
                },
            ));
            app.add_systems(
                Render,
                (extract_frame, move |world: &mut World| {
                    let sun = world
                        .resource::<ExtractedFrame>()
                        .directional
                        .expect("sun extracted");
                    if opt_in {
                        assert_eq!(sun.specular_disc(), crate::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN);
                    } else {
                        assert_eq!(sun._pad_dir, 0, "the point sun every preset ships");
                    }
                })
                    .chain(),
            );
            app.update();
        }
    }

    /// Audit G render5 #8: texture views ([`render_texture_views`] through [`sync_view_tables`]) and
    /// [`render_extracted_into`] shade with this frame's IES profiles, LTC spheres and area lights,
    /// not the previous window frame's (or none, on a headless host that draws only texture views).
    #[test]
    fn view_tables_hand_the_frame_light_extras() {
        let mut renderer = FrameRenderer::new();
        let mut frame = ExtractedFrame::default();
        frame.light_extras.ltc_spheres = vec![false, true];
        sync_view_tables(&mut renderer, &frame, ViewTables::default());
        assert_eq!(
            renderer.light_extras().ltc_spheres,
            vec![false, true],
            "a texture view would shade with stale light extras"
        );
        // The next frame has none: the view frame drops them.
        sync_view_tables(&mut renderer, &ExtractedFrame::default(), ViewTables::default());
        assert!(renderer.light_extras().is_empty());
    }
}
