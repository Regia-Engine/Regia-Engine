// SPDX-License-Identifier: MPL-2.0
//! Host (CPU) RAM budget for the engine's copies of mesh and texture data (ADR-7891).
//!
//! A mesh the renderer has uploaded is drawn from its GPU buffers; the CPU copy a host keeps
//! after that is only needed again by a CPU reader (a collider build, picking, skinning, a feature
//! that walks vertices), after device loss, or when the renderer evicts the GPU copy. A
//! [`HostMeshStore`] holds the CPU geometry of the meshes a host draws, each with its source: a
//! mesh read from a mounted pak keeps an asset id it can be re-read from
//! ([`HostMeshStore::insert_pak`]); a mesh the host built itself has no source and is kept and
//! counted ([`HostMeshStore::insert_owned`]).
//!
//! Per frame, [`HostMeshStore::prepare`] re-reads every mesh the frame draws whose CPU copy was
//! dropped and that the renderer needs bytes for ([`crate::FrameRenderer::host_mesh_bytes_needed`]:
//! not resident, or a feature this frame walks CPU vertices); [`HostMeshStore::frame_mesh`] then
//! builds the [`FrameMesh`] (a dropped copy draws as a resident draw: empty vertex and index
//! slices under the mesh's key). [`HostMeshStore::after_frame`] enforces the budget: while the
//! counted CPU mesh bytes exceed [`HostMemoryBudget::cpu_mesh_bytes`], it drops the CPU copy of the
//! least recently drawn mesh that is resident on the GPU, has a pak source, holds no CPU consumer
//! pin ([`HostMeshStore::pin`]) and drew GPU-only-capable this frame. It runs after the frame was
//! submitted (the uploads were copied into staging), so nothing the frame still needs is dropped.
//!
//! The budget comes from the active [`crate::RenderProfile::host_memory`], unless a host override
//! is set ([`set_host_budget_override`], the agent's `render.set_host_budget`). The counters every
//! store adds to are process-wide ([`host_memory_counters`]); they appear in
//! [`crate::MemoryReport::host`], `render.memory_budget` and the 10 s telemetry summary.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_asset::{AssetId, PayloadReader};

use crate::lab_frame::{LabMaterial, LabVertex};
use crate::{FrameMesh, FrameQuality, FrameRenderer, LOD_DITHER_NONE, ShadowCast};

/// Top byte of the mesh keys a [`HostMeshStore`] gives out (`'H'`): clear of the ECS mesh ids
/// ([`crate::MESH_ID_KEY_TAG`]), the morph and skin-twin tags, [`crate::EPHEMERAL_KEY`] and
/// [`crate::SHADOW_LOD_MESH_TAG`]. The id sits in bits 3..56 (the LOD bits 1-2 and the dither bit
/// 0 stay clear).
pub const HOST_MESH_KEY_TAG: u64 = 0x4800_0000_0000_0000;

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;

/// A per-tier cap on the CPU copies of mesh and texture data the engine keeps for a host
/// ([`HostMeshStore`]). [`Self::UNLIMITED`] (`u64::MAX`) turns a side off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostMemoryBudget {
    /// Most bytes of CPU vertex and index data kept for drawn meshes.
    pub cpu_mesh_bytes: u64,
    /// Most bytes of decoded texture payloads kept on the CPU. Counted and reported; texture
    /// payloads are not dropped yet (ADR-7891: deferred), so going over only counts a tick.
    pub cpu_texture_bytes: u64,
}

impl HostMemoryBudget {
    /// No cap on either side.
    pub const UNLIMITED: Self = Self {
        cpu_mesh_bytes: u64::MAX,
        cpu_texture_bytes: u64::MAX,
    };

    /// The value of `preset`'s [`crate::RenderProfile::host_memory`] (ADR-7891). The process RSS
    /// floors these sit under: DO27's lowest tier must run in 2.5 GB of process RSS, so the 2.5 GB
    /// tiers (Mobile High, Deck, Desktop Low) keep at most 768 MiB of mesh and texture copies and
    /// leave 1.7 GB for everything else; Mobile Low (2-3 GB phones, about 1 GB for the app) keeps
    /// 192 MiB. Desktop High and Ultra (16 GB and 32 GB machines) keep 2 GiB and 4 GiB; Cinematic
    /// (offline renders, built from the Ultra row) keeps Ultra's.
    #[must_use]
    pub const fn for_preset(preset: crate::RenderProfilePreset) -> Self {
        use crate::RenderProfilePreset as P;
        let (mesh, texture) = match preset {
            P::MobileLow => (128 * MIB, 64 * MIB),
            P::MobileHigh | P::Deck | P::DesktopLow => (512 * MIB, 256 * MIB),
            P::DesktopHigh => (1536 * MIB, 512 * MIB),
            P::DesktopUltra | P::Cinematic => (3072 * MIB, 1024 * MIB),
        };
        Self {
            cpu_mesh_bytes: mesh,
            cpu_texture_bytes: texture,
        }
    }
}

impl Default for HostMemoryBudget {
    fn default() -> Self {
        Self::for_preset(crate::RenderProfilePreset::DesktopHigh)
    }
}

static OVERRIDE_SET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static OVERRIDE_MESH: AtomicU64 = AtomicU64::new(u64::MAX);
static OVERRIDE_TEXTURE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Sets (or with `None` clears) the process-wide host budget override: every [`HostMeshStore`]
/// enforces it instead of its profile budget from its next [`HostMeshStore::after_frame`]. The
/// agent's `render.set_host_budget` calls it.
pub fn set_host_budget_override(budget: Option<HostMemoryBudget>) {
    if let Some(budget) = budget {
        OVERRIDE_MESH.store(budget.cpu_mesh_bytes, Ordering::Relaxed);
        OVERRIDE_TEXTURE.store(budget.cpu_texture_bytes, Ordering::Relaxed);
    }
    OVERRIDE_SET.store(budget.is_some(), Ordering::Release);
}

/// The host budget override set by [`set_host_budget_override`], if any.
#[must_use]
pub fn host_budget_override() -> Option<HostMemoryBudget> {
    OVERRIDE_SET.load(Ordering::Acquire).then(|| HostMemoryBudget {
        cpu_mesh_bytes: OVERRIDE_MESH.load(Ordering::Relaxed),
        cpu_texture_bytes: OVERRIDE_TEXTURE.load(Ordering::Relaxed),
    })
}

/// What the engine's CPU copies of mesh and texture data cost, and what the budget did.
/// Byte totals are live values; the rest count since the store was made (per store) or since
/// process start ([`host_memory_counters`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostMemoryCounters {
    /// CPU vertex and index bytes held now (vector capacity).
    pub cpu_mesh_bytes: u64,
    /// Decoded texture payload bytes the host reported holding ([`HostMeshStore::set_cpu_texture_bytes`]).
    pub cpu_texture_bytes: u64,
    /// Bytes of CPU copies dropped by the budget.
    pub dropped_bytes: u64,
    /// CPU copies dropped by the budget.
    pub drops: u64,
    /// Bytes re-read from paks after a drop.
    pub reread_bytes: u64,
    /// Meshes re-read from paks after a drop.
    pub rereads: u64,
    /// Frames that ended over budget (nothing more could be dropped).
    pub over_budget_ticks: u64,
    /// Bytes of meshes a CPU consumer pins ([`HostMeshStore::pin`]), included in `cpu_mesh_bytes`.
    pub pinned_bytes: u64,
    /// Bytes of meshes without a pak source (kept and counted), included in `cpu_mesh_bytes`.
    pub unsourced_bytes: u64,
    /// Bytes of distinct pak entries read through a memory mapping: those pages stay in the
    /// process's resident set while the pak is mapped and the kernel keeps them (file-backed,
    /// clean, reclaimable), so they count in RSS without being a CPU copy.
    pub mapped_touched_bytes: u64,
}

/// The process-wide counters ([`host_memory_counters`]): live byte totals rise and fall with every
/// store; event counts only rise.
struct GlobalCounters {
    cpu_mesh_bytes: AtomicU64,
    cpu_texture_bytes: AtomicU64,
    dropped_bytes: AtomicU64,
    drops: AtomicU64,
    reread_bytes: AtomicU64,
    rereads: AtomicU64,
    over_budget_ticks: AtomicU64,
    pinned_bytes: AtomicU64,
    unsourced_bytes: AtomicU64,
    mapped_touched_bytes: AtomicU64,
}

static GLOBAL: GlobalCounters = GlobalCounters {
    cpu_mesh_bytes: AtomicU64::new(0),
    cpu_texture_bytes: AtomicU64::new(0),
    dropped_bytes: AtomicU64::new(0),
    drops: AtomicU64::new(0),
    reread_bytes: AtomicU64::new(0),
    rereads: AtomicU64::new(0),
    over_budget_ticks: AtomicU64::new(0),
    pinned_bytes: AtomicU64::new(0),
    unsourced_bytes: AtomicU64::new(0),
    mapped_touched_bytes: AtomicU64::new(0),
};

/// The budget last enforced by any store ([`host_memory_counters`] reports it beside the counts).
static LAST_BUDGET_MESH: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_BUDGET_TEXTURE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Every [`HostMeshStore`] in the process, summed: live bytes now, events since start.
#[must_use]
pub fn host_memory_counters() -> HostMemoryCounters {
    let g = &GLOBAL;
    HostMemoryCounters {
        cpu_mesh_bytes: g.cpu_mesh_bytes.load(Ordering::Relaxed),
        cpu_texture_bytes: g.cpu_texture_bytes.load(Ordering::Relaxed),
        dropped_bytes: g.dropped_bytes.load(Ordering::Relaxed),
        drops: g.drops.load(Ordering::Relaxed),
        reread_bytes: g.reread_bytes.load(Ordering::Relaxed),
        rereads: g.rereads.load(Ordering::Relaxed),
        over_budget_ticks: g.over_budget_ticks.load(Ordering::Relaxed),
        pinned_bytes: g.pinned_bytes.load(Ordering::Relaxed),
        unsourced_bytes: g.unsourced_bytes.load(Ordering::Relaxed),
        mapped_touched_bytes: g.mapped_touched_bytes.load(Ordering::Relaxed),
    }
}

/// The budget the last [`HostMeshStore::after_frame`] in the process enforced
/// ([`HostMemoryBudget::UNLIMITED`] before any).
#[must_use]
pub fn last_enforced_host_budget() -> HostMemoryBudget {
    HostMemoryBudget {
        cpu_mesh_bytes: LAST_BUDGET_MESH.load(Ordering::Relaxed),
        cpu_texture_bytes: LAST_BUDGET_TEXTURE.load(Ordering::Relaxed),
    }
}

fn global_add(counter: &AtomicU64, bytes: u64) {
    counter.fetch_add(bytes, Ordering::Relaxed);
}

fn global_sub(counter: &AtomicU64, bytes: u64) {
    // Saturating: a store only subtracts what it added.
    let mut current = counter.load(Ordering::Relaxed);
    while let Err(seen) = counter.compare_exchange_weak(
        current,
        current.saturating_sub(bytes),
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        current = seen;
    }
}

/// One mesh in a [`HostMeshStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostMeshId(u32);

impl HostMeshId {
    /// The index of this mesh in its store.
    #[must_use]
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Errors from a [`HostMeshStore`].
#[derive(Debug, thiserror::Error)]
pub enum HostMeshError {
    /// The id names no mesh of this store (removed, or from another store).
    #[error("host mesh {0:?} is not in this store")]
    Unknown(HostMeshId),
    /// Reading or decoding the mesh from its pak failed (the pak was unmounted, or is corrupt).
    #[error("host mesh {id}: {source}")]
    Read {
        /// The mesh's asset id.
        id: AssetId,
        /// The decode error.
        source: Box<crate::cooked_model::CookedModelError>,
    },
}

/// Where a mesh's bytes can be read again.
#[derive(Clone)]
struct PakSource {
    reader: PayloadReader,
    id: AssetId,
}

struct Entry {
    key: u64,
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    /// `false` once the budget dropped the CPU copy (`vertices`/`indices` are then empty).
    cpu: bool,
    /// Bytes of the CPU copy when held (capacity), and what a re-read restores.
    bytes: u64,
    source: Option<PakSource>,
    pins: u32,
    last_drawn: u64,
    /// The renderer could draw this mesh without CPU bytes on its last [`HostMeshStore::prepare`].
    gpu_only: bool,
}

fn mesh_bytes(vertices: &Vec<LabVertex>, indices: &Vec<u32>) -> u64 {
    (vertices.capacity() * size_of::<LabVertex>() + indices.capacity() * size_of::<u32>()) as u64
}

/// The CPU geometry of the meshes a host draws, under a [`HostMemoryBudget`] (module docs).
pub struct HostMeshStore {
    entries: Vec<Option<Entry>>,
    next_key: u64,
    clock: u64,
    profile_budget: HostMemoryBudget,
    counters: HostMemoryCounters,
    touched: HashSet<AssetId>,
}

impl HostMeshStore {
    /// An empty store that enforces `budget` (normally the active profile's
    /// [`crate::RenderProfile::host_memory`]) unless a host override is set.
    #[must_use]
    pub fn new(budget: HostMemoryBudget) -> Self {
        static NEXT_STORE: AtomicU64 = AtomicU64::new(0);
        // Keys stay unique across stores: each store takes its own window of 2^29 ids.
        let store = NEXT_STORE.fetch_add(1, Ordering::Relaxed) & 0x3F_FFFF;
        Self {
            entries: Vec::new(),
            next_key: store << 29,
            clock: 0,
            profile_budget: budget,
            counters: HostMemoryCounters::default(),
            touched: HashSet::new(),
        }
    }

    /// Changes the profile budget (a profile switch). A host override still wins.
    pub fn set_budget(&mut self, budget: HostMemoryBudget) {
        self.profile_budget = budget;
    }

    /// The budget [`Self::after_frame`] enforces now: the host override, else the profile's.
    #[must_use]
    pub fn budget(&self) -> HostMemoryBudget {
        host_budget_override().unwrap_or(self.profile_budget)
    }

    /// This store's counters (live bytes now, events since it was made).
    #[must_use]
    pub fn counters(&self) -> HostMemoryCounters {
        self.counters
    }

    /// Meshes in the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    /// Whether the store holds no mesh.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn take_key(&mut self) -> u64 {
        self.next_key += 1;
        HOST_MESH_KEY_TAG | ((self.next_key & ((1 << 53) - 1)) << 3)
    }

    fn push(&mut self, vertices: Vec<LabVertex>, indices: Vec<u32>, source: Option<PakSource>) -> HostMeshId {
        let bytes = mesh_bytes(&vertices, &indices);
        let key = self.take_key();
        self.add_live(bytes, source.is_none(), false);
        self.entries.push(Some(Entry {
            key,
            vertices,
            indices,
            cpu: true,
            bytes,
            source,
            pins: 0,
            last_drawn: 0,
            gpu_only: false,
        }));
        HostMeshId((self.entries.len() - 1) as u32)
    }

    fn add_live(&mut self, bytes: u64, unsourced: bool, pinned: bool) {
        self.counters.cpu_mesh_bytes += bytes;
        global_add(&GLOBAL.cpu_mesh_bytes, bytes);
        if unsourced {
            self.counters.unsourced_bytes += bytes;
            global_add(&GLOBAL.unsourced_bytes, bytes);
        }
        if pinned {
            self.counters.pinned_bytes += bytes;
            global_add(&GLOBAL.pinned_bytes, bytes);
        }
    }

    fn sub_live(&mut self, bytes: u64, unsourced: bool, pinned: bool) {
        self.counters.cpu_mesh_bytes = self.counters.cpu_mesh_bytes.saturating_sub(bytes);
        global_sub(&GLOBAL.cpu_mesh_bytes, bytes);
        if unsourced {
            self.counters.unsourced_bytes = self.counters.unsourced_bytes.saturating_sub(bytes);
            global_sub(&GLOBAL.unsourced_bytes, bytes);
        }
        if pinned {
            self.counters.pinned_bytes = self.counters.pinned_bytes.saturating_sub(bytes);
            global_sub(&GLOBAL.pinned_bytes, bytes);
        }
    }

    /// Reads the cooked mesh `id` (LOD 0) through `reader` and keeps `reader` and `id` as its
    /// source, so the budget may drop the CPU copy once it is on the GPU and re-read it on demand.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Read`]: the mesh does not read or decode.
    pub fn insert_pak(&mut self, reader: &PayloadReader, id: AssetId) -> Result<HostMeshId, HostMeshError> {
        let (vertices, indices) = self.read(reader, id)?;
        Ok(self.push(
            vertices,
            indices,
            Some(PakSource {
                reader: reader.clone(),
                id,
            }),
        ))
    }

    /// Keeps a mesh the host built itself (procedural, runtime-built). It has no source to re-read
    /// from, so the budget never drops it; it is counted in `cpu_mesh_bytes` and `unsourced_bytes`.
    pub fn insert_owned(&mut self, vertices: Vec<LabVertex>, indices: Vec<u32>) -> HostMeshId {
        self.push(vertices, indices, None)
    }

    fn read(
        &mut self,
        reader: &PayloadReader,
        id: AssetId,
    ) -> Result<(Vec<LabVertex>, Vec<u32>), HostMeshError> {
        let (mesh, mapped) =
            crate::cooked_model::decode_mesh_counted(reader, id).map_err(|source| HostMeshError::Read {
                id,
                source: Box::new(source),
            })?;
        if mapped > 0 && self.touched.insert(id) {
            self.counters.mapped_touched_bytes += mapped;
            global_add(&GLOBAL.mapped_touched_bytes, mapped);
        }
        Ok((mesh.vertices, mesh.indices))
    }

    fn entry(&self, mesh: HostMeshId) -> Result<&Entry, HostMeshError> {
        self.entries
            .get(mesh.0 as usize)
            .and_then(Option::as_ref)
            .ok_or(HostMeshError::Unknown(mesh))
    }

    fn entry_mut(&mut self, mesh: HostMeshId) -> Result<&mut Entry, HostMeshError> {
        self.entries
            .get_mut(mesh.0 as usize)
            .and_then(Option::as_mut)
            .ok_or(HostMeshError::Unknown(mesh))
    }

    /// The renderer cache key of `mesh` ([`FrameMesh::mesh_key`]): stable for the mesh's life,
    /// never shared with another mesh of any store.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`].
    pub fn mesh_key(&self, mesh: HostMeshId) -> Result<u64, HostMeshError> {
        self.entry(mesh).map(|e| e.key)
    }

    /// Whether `mesh` holds its CPU copy now.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`].
    pub fn has_cpu_copy(&self, mesh: HostMeshId) -> Result<bool, HostMeshError> {
        self.entry(mesh).map(|e| e.cpu)
    }

    /// Re-reads `mesh`'s CPU copy when it was dropped (counted in `rereads` and `reread_bytes`).
    fn restore(&mut self, mesh: HostMeshId) -> Result<(), HostMeshError> {
        let entry = self.entry(mesh)?;
        if entry.cpu {
            return Ok(());
        }
        let Some(source) = entry.source.clone() else {
            // Unsourced meshes are never dropped.
            return Ok(());
        };
        let (vertices, indices) = self.read(&source.reader, source.id)?;
        let bytes = mesh_bytes(&vertices, &indices);
        let pinned = self.entry(mesh)?.pins > 0;
        self.add_live(bytes, false, pinned);
        self.counters.rereads += 1;
        self.counters.reread_bytes += bytes;
        global_add(&GLOBAL.rereads, 1);
        global_add(&GLOBAL.reread_bytes, bytes);
        let entry = self.entry_mut(mesh)?;
        entry.vertices = vertices;
        entry.indices = indices;
        entry.bytes = bytes;
        entry.cpu = true;
        Ok(())
    }

    /// The CPU geometry of `mesh` for a CPU reader (a late reader, a LOD rebuild, a debug
    /// export), re-read from its pak first when the budget dropped it. A reader that keeps the
    /// data across frames pins the mesh instead ([`Self::pin`]).
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`], or [`HostMeshError::Read`] when the re-read fails.
    pub fn cpu_geometry(&mut self, mesh: HostMeshId) -> Result<(&[LabVertex], &[u32]), HostMeshError> {
        self.restore(mesh)?;
        let entry = self.entry(mesh)?;
        Ok((&entry.vertices, &entry.indices))
    }

    /// Marks `mesh` as held by a CPU consumer (a physics collider, picking, CPU skinning or
    /// morphing, a mask decal or scalar overlay that reads its vertices): the budget never drops
    /// it while a pin is held. Re-reads it first when it was dropped. Pins nest.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`], or [`HostMeshError::Read`] when the re-read fails.
    pub fn pin(&mut self, mesh: HostMeshId) -> Result<(), HostMeshError> {
        self.restore(mesh)?;
        let entry = self.entry_mut(mesh)?;
        entry.pins += 1;
        if entry.pins == 1 {
            let bytes = entry.bytes;
            self.counters.pinned_bytes += bytes;
            global_add(&GLOBAL.pinned_bytes, bytes);
        }
        Ok(())
    }

    /// Releases one [`Self::pin`] of `mesh`.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`].
    pub fn unpin(&mut self, mesh: HostMeshId) -> Result<(), HostMeshError> {
        let entry = self.entry_mut(mesh)?;
        if entry.pins == 0 {
            return Ok(());
        }
        entry.pins -= 1;
        if entry.pins == 0 && entry.cpu {
            let bytes = entry.bytes;
            self.counters.pinned_bytes = self.counters.pinned_bytes.saturating_sub(bytes);
            global_sub(&GLOBAL.pinned_bytes, bytes);
        }
        Ok(())
    }

    /// Removes `mesh` and returns its renderer key: pass it to [`FrameRenderer::release_meshes`]
    /// so the GPU copy goes too.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`].
    pub fn remove(&mut self, mesh: HostMeshId) -> Result<u64, HostMeshError> {
        let entry = self
            .entries
            .get_mut(mesh.0 as usize)
            .and_then(Option::take)
            .ok_or(HostMeshError::Unknown(mesh))?;
        if entry.cpu {
            self.sub_live(entry.bytes, entry.source.is_none(), entry.pins > 0);
        }
        Ok(entry.key)
    }

    /// Reports the bytes of decoded texture payloads the host holds for the meshes of this store
    /// (for example [`crate::cooked_model::CookedModelDraws::texture_bytes`]): counted in `cpu_texture_bytes` and
    /// checked against [`HostMemoryBudget::cpu_texture_bytes`].
    pub fn set_cpu_texture_bytes(&mut self, bytes: u64) {
        global_sub(&GLOBAL.cpu_texture_bytes, self.counters.cpu_texture_bytes);
        global_add(&GLOBAL.cpu_texture_bytes, bytes);
        self.counters.cpu_texture_bytes = bytes;
    }

    /// Before building the frame's scene: for each mesh the frame draws (with its material), records
    /// whether the renderer can draw it without CPU bytes and re-reads every dropped copy the
    /// renderer needs this frame ([`FrameRenderer::host_mesh_bytes_needed`]). Marks the meshes
    /// drawn this tick. Returns how many meshes were re-read.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`], or [`HostMeshError::Read`] when a re-read fails (that mesh is
    /// then not drawn this frame: it has neither CPU bytes nor GPU buffers).
    pub fn prepare<'m>(
        &mut self,
        renderer: &FrameRenderer,
        quality: &FrameQuality,
        draws: impl IntoIterator<Item = (HostMeshId, &'m LabMaterial)>,
    ) -> Result<u32, HostMeshError> {
        let _span = tracing::info_span!("host_mesh_prepare").entered();
        self.clock += 1;
        let clock = self.clock;
        let mut reread = 0u32;
        let mut first_error = None;
        for (mesh, material) in draws {
            let entry = match self.entry_mut(mesh) {
                Ok(entry) => entry,
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            entry.last_drawn = clock;
            entry.gpu_only = renderer.gpu_only_draw_supported(material, quality);
            let needed = renderer.host_mesh_bytes_needed(entry.key, material, quality);
            if needed && !entry.cpu {
                match self.restore(mesh) {
                    Ok(()) => reread += 1,
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(reread),
        }
    }

    /// The [`FrameMesh`] of `mesh` for this frame: its CPU slices when held, else a resident draw
    /// (empty slices under its key, drawn from the GPU buffers). Rigid, whole, casts every shadow.
    ///
    /// # Errors
    ///
    /// [`HostMeshError::Unknown`].
    pub fn frame_mesh<'a>(
        &'a self,
        mesh: HostMeshId,
        material: &'a LabMaterial,
        material_key: u64,
        object_key: u64,
        model: [[f32; 4]; 4],
    ) -> Result<FrameMesh<'a>, HostMeshError> {
        let entry = self.entry(mesh)?;
        Ok(FrameMesh {
            mesh_key: entry.key,
            material_key,
            object_key,
            vertices: &entry.vertices,
            indices: &entry.indices,
            material,
            model,
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: LOD_DITHER_NONE,
            is_static: true,
        })
    }

    /// After the frame was submitted: drops CPU copies, least recently drawn first, until the
    /// counted CPU mesh bytes fit [`Self::budget`] (module docs for which copies may go). A frame
    /// that ends over either side of the budget counts an over-budget tick. Returns the bytes
    /// dropped.
    pub fn after_frame(&mut self, renderer: &FrameRenderer) -> u64 {
        let _span = tracing::info_span!("host_mesh_budget").entered();
        let budget = self.budget();
        LAST_BUDGET_MESH.store(budget.cpu_mesh_bytes, Ordering::Relaxed);
        LAST_BUDGET_TEXTURE.store(budget.cpu_texture_bytes, Ordering::Relaxed);
        let mut dropped = 0u64;
        if self.counters.cpu_mesh_bytes > budget.cpu_mesh_bytes {
            // Least recently drawn first; ties by id (insertion order).
            let mut candidates: Vec<(u64, u32)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    let e = e.as_ref()?;
                    (e.cpu
                        && e.source.is_some()
                        && e.pins == 0
                        && e.gpu_only
                        && renderer.mesh_buffers_resident(e.key))
                    .then_some((e.last_drawn, i as u32))
                })
                .collect();
            candidates.sort_unstable();
            for (_, index) in candidates {
                if self.counters.cpu_mesh_bytes <= budget.cpu_mesh_bytes {
                    break;
                }
                let Some(Some(entry)) = self.entries.get_mut(index as usize) else {
                    continue;
                };
                let bytes = entry.bytes;
                entry.vertices = Vec::new();
                entry.indices = Vec::new();
                entry.cpu = false;
                self.sub_live(bytes, false, false);
                self.counters.dropped_bytes += bytes;
                self.counters.drops += 1;
                global_add(&GLOBAL.dropped_bytes, bytes);
                global_add(&GLOBAL.drops, 1);
                dropped += bytes;
            }
        }
        if self.counters.cpu_mesh_bytes > budget.cpu_mesh_bytes
            || self.counters.cpu_texture_bytes > budget.cpu_texture_bytes
        {
            self.counters.over_budget_ticks += 1;
            global_add(&GLOBAL.over_budget_ticks, 1);
        }
        dropped
    }
}

impl Drop for HostMeshStore {
    fn drop(&mut self) {
        global_sub(&GLOBAL.cpu_mesh_bytes, self.counters.cpu_mesh_bytes);
        global_sub(&GLOBAL.pinned_bytes, self.counters.pinned_bytes);
        global_sub(&GLOBAL.unsourced_bytes, self.counters.unsourced_bytes);
        global_sub(&GLOBAL.cpu_texture_bytes, self.counters.cpu_texture_bytes);
    }
}

impl std::fmt::Debug for HostMeshStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMeshStore")
            .field("meshes", &self.len())
            .field("budget", &self.budget())
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_a_budget_and_the_floor_tiers_fit_2_5_gb() {
        use crate::RenderProfilePreset as P;
        for preset in P::WITH_OFFLINE {
            let budget = HostMemoryBudget::for_preset(preset);
            assert_eq!(
                crate::RenderProfile::from_preset(preset).host_memory,
                budget,
                "{preset:?}"
            );
        }
        for preset in [P::MobileHigh, P::Deck, P::DesktopLow] {
            let b = HostMemoryBudget::for_preset(preset);
            assert!(b.cpu_mesh_bytes + b.cpu_texture_bytes <= 768 * MIB, "{preset:?}");
        }
        assert_eq!(
            HostMemoryBudget::for_preset(P::Cinematic),
            HostMemoryBudget::for_preset(P::DesktopUltra)
        );
    }

    #[test]
    fn owned_meshes_are_counted_and_unsourced() {
        let mut store = HostMeshStore::new(HostMemoryBudget {
            cpu_mesh_bytes: 0,
            cpu_texture_bytes: 0,
        });
        let v = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 0.0, 1.0], [0.0; 2]); 3];
        let id = store.insert_owned(v, vec![0, 1, 2]);
        let bytes = store.counters().cpu_mesh_bytes;
        assert!(bytes >= (3 * size_of::<LabVertex>() + 12) as u64);
        assert_eq!(store.counters().unsourced_bytes, bytes);
        let key = store.mesh_key(id).expect("key");
        assert_eq!(key >> 56, HOST_MESH_KEY_TAG >> 56);
        assert_eq!(key & 0b111, 0, "LOD and dither bits stay clear");
        assert_eq!(store.remove(id).expect("removed"), key);
        assert_eq!(store.counters().cpu_mesh_bytes, 0);
        assert!(store.remove(id).is_err());
    }

    #[test]
    fn the_override_wins_over_the_profile_budget() {
        // Process-wide: only this test sets it, and it restores it.
        let store = HostMeshStore::new(HostMemoryBudget::UNLIMITED);
        let tight = HostMemoryBudget {
            cpu_mesh_bytes: 1,
            cpu_texture_bytes: 2,
        };
        set_host_budget_override(Some(tight));
        assert_eq!(store.budget(), tight);
        assert_eq!(host_budget_override(), Some(tight));
        set_host_budget_override(None);
        assert_eq!(store.budget(), HostMemoryBudget::UNLIMITED);
    }
}
