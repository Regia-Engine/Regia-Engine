// SPDX-License-Identifier: MPL-2.0
//! GPU upload of counted built-in meshes (`docs/specs/12-assets.md` § 3, ADR-7256, ADR-7284).
//!
//! [`AssetMeshUploads`] is the [`regia_asset::PrepareAsset`] for [`regia_asset::CpuMesh`]: register
//! a clone with `AssetServer::register_prepare` and every counted mesh load
//! (`load::<CpuMesh>(id)`) uploads its vertex and index buffers before the server stores the value;
//! the server then drops the CPU streams unless the load asked for `keep_cpu_data`. When the asset
//! unloads (its last handle dropped and the grace frames ran out, or a budget evicted it) the server
//! calls [`regia_asset::PrepareAsset::unload`] and the buffers are released (freed once a renderer
//! that adopted them drops its entry, at the start of its next frame). Clones share one
//! table, so the renderer keeps a clone to look the buffers up by `AssetId`.
//!
//! The buffers hold the frame renderer's own vertex layout ([`crate::LabVertex`], MikkTSpace
//! tangents, exactly the vertices [`crate::cooked_model::load_cooked_model`] builds) and `u32`
//! indices, so a [`crate::FrameRenderer`] given the uploader
//! ([`crate::FrameRenderer::set_asset_meshes`]) draws a counted mesh straight from them: a
//! [`crate::FrameMesh`] with the key [`asset_mesh_key`] and no CPU vertices (ADR-7284).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use regia_asset::{AssetId, CpuMesh, PrepareAsset};
use wgpu::util::DeviceExt;

/// Bytes per vertex in [`AssetMeshBuffers::vertex`]: one [`crate::LabVertex`] (the frame
/// renderer's vertex layout, `regia_asset::GPU_VERTEX_STRIDE`).
pub const ASSET_MESH_VERTEX_STRIDE: u64 = regia_asset::GPU_VERTEX_STRIDE as u64;

/// Top byte of the frame mesh keys [`asset_mesh_key`] gives counted meshes (`'A'`): clear of
/// [`crate::MESH_ID_KEY_TAG`] (`'M'`), [`crate::MORPH_MESH_KEY_TAG`], [`crate::EPHEMERAL_KEY`]
/// (`0xFE`) and [`crate::SHADOW_LOD_MESH_TAG`] (`0xFF`). The id hash sits in bits 3..56, so the
/// LOD and dither bits (0–2) stay clear.
pub const ASSET_MESH_KEY_TAG: u64 = 0x4100_0000_0000_0000;

/// The frame mesh key of counted mesh `id` ([`ASSET_MESH_KEY_TAG`] and a 53-bit fold of the id).
/// A [`crate::FrameMesh`] with this key and empty `vertices` / `indices` draws the buffers
/// [`AssetMeshUploads`] holds for `id`.
#[must_use]
pub const fn asset_mesh_key(id: AssetId) -> u64 {
    let bits = id.0.as_u128();
    let folded = (bits as u64) ^ ((bits >> 64) as u64);
    ASSET_MESH_KEY_TAG | ((folded & ((1 << 53) - 1)) << 3)
}

/// Whether `key` is a counted-mesh key ([`asset_mesh_key`]).
#[must_use]
pub const fn is_asset_mesh_key(key: u64) -> bool {
    key >> 56 == ASSET_MESH_KEY_TAG >> 56
}

/// The counted-mesh key ([`asset_mesh_key`]) a submitted mesh key derives from, and the LOD index
/// of the variant (ADR-7428): the key itself (LOD 0), a LOD variant (bits 1–2), a shadow-LOD-only
/// variant (top byte `0xFF`, `mesh_lod::lod_variant_key`) or a cross-fade draw's key (xored with
/// the dither tag). `None` for any other key. The shadow tag overwrites the counted tag, so such a
/// key is only taken for a counted one when the caller knows the draw has no CPU vertices and the
/// result names an adopted upload.
pub(crate) fn counted_base_key(key: u64) -> Option<(u64, usize)> {
    let tag = ASSET_MESH_KEY_TAG >> 56;
    let dither = crate::frame::LOD_DITHER_MESH_TAG;
    let key = match key >> 56 {
        top if top == tag => key,
        top if top == tag ^ (dither >> 56) => key ^ dither,
        top if top == crate::SHADOW_LOD_MESH_TAG >> 56 => {
            (key & !crate::SHADOW_LOD_MESH_TAG) | ASSET_MESH_KEY_TAG
        }
        _ => return None,
    };
    Some((key & !0b110, ((key >> 1) & 0b11) as usize))
}

/// The GPU copy of one counted mesh.
#[derive(Clone, Debug)]
pub struct AssetMeshBuffers {
    /// [`crate::LabVertex`] vertices ([`ASSET_MESH_VERTEX_STRIDE`] bytes a vertex).
    pub vertex: wgpu::Buffer,
    /// `u32` triangle indices.
    pub index: wgpu::Buffer,
    /// Indices in [`Self::index`].
    pub index_count: u32,
}

impl AssetMeshBuffers {
    /// Bytes of both buffers.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.vertex.size() + self.index.size()
    }
}

/// What the frame renderer needs to draw a resident counted mesh without its CPU copy.
#[derive(Clone)]
pub(crate) struct ResidentAssetMesh {
    pub(crate) buffers: AssetMeshBuffers,
    /// Object-space bounds of the vertices (the frame's culling sphere).
    pub(crate) bounds: Option<crate::MeshBounds>,
    /// Object-space metres per UV unit (texture streaming).
    pub(crate) uv_span: f32,
    /// Which upload this is (a reload uploads again under the same key).
    pub(crate) serial: u64,
    /// What a lightmap receiver checks against without CPU vertices (ADR-7330).
    pub(crate) layout: AssetMeshLayout,
    /// The skin checks of a skinned upload (`None`: uploaded rigid, ADR-8610).
    pub(crate) skin: Option<Arc<crate::frame::AssetMeshSkin>>,
    /// The mesh's morph targets packed against the uploaded vertices (`None`: it has none,
    /// ADR-7721).
    pub(crate) morph: Option<Arc<crate::MorphTargets>>,
    /// Object-space box of every uploaded vertex (`[min, max]`): what the probes' size test reads
    /// without CPU vertices (ADR-7721).
    pub(crate) extent: [[f32; 3]; 2],
}

/// The shape of an uploaded counted mesh, kept after its CPU streams go (ADR-7330): what a
/// lightmap receiver compares its entry's unwrap with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AssetMeshLayout {
    /// Uploaded vertices.
    pub(crate) vertex_count: usize,
    /// [`index_list_hash`] of the uploaded indices.
    pub(crate) index_hash: u64,
    /// `regia_asset::lightmap::lightmap_source_hash` of the positions and indices.
    pub(crate) source_hash: u64,
}

/// A 64-bit blake3 hash of a triangle list's bytes (equal lists hash equal).
pub(crate) fn index_list_hash(indices: &[u32]) -> u64 {
    let hash = blake3::hash(bytemuck::cast_slice(indices));
    let h = hash.as_bytes();
    u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]])
}

/// `regia_asset::lightmap::lightmap_source_hash` with one hasher update per stream instead of one
/// per scalar (the same bytes in the same order, so the same hash; a test checks it).
fn source_hash(positions: &[[f32; 3]], indices: &[u32]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(positions.len() as u64).to_le_bytes());
    let mut bytes = Vec::with_capacity(positions.len() * 12);
    for c in positions.iter().flatten() {
        bytes.extend_from_slice(&(c + 0.0).to_bits().to_le_bytes());
    }
    hasher.update(&bytes);
    hasher.update(&(indices.len() as u64).to_le_bytes());
    for i in indices {
        hasher.update(&i.to_le_bytes());
    }
    let h = hasher.finalize();
    let h = h.as_bytes();
    u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]).max(1)
}

#[derive(Default)]
struct Table {
    resident: HashMap<AssetId, ResidentAssetMesh>,
    /// [`asset_mesh_key`] of each resident id.
    by_key: HashMap<u64, AssetId>,
    uploads: u64,
    frees: u64,
    /// GPU readbacks of uploaded vertices for the probes' signed-distance bricks (ADR-7721).
    gi_readbacks: u64,
}

/// Uploads counted [`CpuMesh`] assets and frees them when they unload. Cheap to clone: clones
/// share the device, queue and buffer table.
#[derive(Clone)]
pub struct AssetMeshUploads {
    device: wgpu::Device,
    queue: wgpu::Queue,
    table: Arc<Mutex<Table>>,
    /// `STORAGE` where the device has the visibility-buffer path (ADR-7462), else empty.
    vis_storage: wgpu::BufferUsages,
}

impl std::fmt::Debug for AssetMeshUploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetMeshUploads")
            .field("resident", &self.resident_count())
            .finish_non_exhaustive()
    }
}

impl AssetMeshUploads {
    /// An uploader on `device`'s device and queue.
    #[must_use]
    pub fn new(device: &crate::RenderDevice) -> Self {
        let vis_storage = if crate::visbuffer::gpu::VisBufferPass::supported(&device.device) {
            wgpu::BufferUsages::STORAGE
        } else {
            wgpu::BufferUsages::empty()
        };
        Self {
            device: device.device.clone(),
            queue: device.queue.clone(),
            table: Arc::default(),
            vis_storage,
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        // A poisoned lock means a panic elsewhere while holding it; the table itself stays valid.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The GPU buffers of mesh `id`, while it is loaded.
    #[must_use]
    pub fn buffers(&self, id: AssetId) -> Option<AssetMeshBuffers> {
        self.table().resident.get(&id).map(|r| r.buffers.clone())
    }

    /// The resident mesh a frame key names ([`asset_mesh_key`]), cloned (buffer handles are
    /// reference counted; no allocation).
    pub(crate) fn resident_by_key(&self, key: u64) -> Option<ResidentAssetMesh> {
        let table = self.table();
        let id = table.by_key.get(&key)?;
        table.resident.get(id).cloned()
    }

    /// The morph targets of resident mesh `id`, packed at the upload against the vertices it
    /// uploaded (ADR-7721): the set [`crate::MorphTargets::from_cooked`] builds on the CPU path,
    /// without a CPU vertex copy. Put it in a [`crate::MeshMorphTargets`] next to a
    /// [`crate::MeshRenderer::counted`] of the same asset (or pass it in a [`crate::MorphInstance`]):
    /// the renderer deforms a copy of the uploaded vertex buffer. `None` when the mesh is not
    /// resident, has no morph targets, or its targets do not fit its vertices.
    #[must_use]
    pub fn morph_targets(&self, id: AssetId) -> Option<Arc<crate::MorphTargets>> {
        self.table().resident.get(&id).and_then(|r| r.morph.clone())
    }

    /// GPU readbacks of uploaded vertices made so far for the probes' signed-distance bricks
    /// (ADR-7721): one per counted mesh the probes trace, again only after its brick was freed or
    /// the mesh was uploaded again.
    #[must_use]
    pub fn gi_readbacks(&self) -> u64 {
        self.table().gi_readbacks
    }

    /// The shape of resident mesh `id` (ADR-7330).
    pub(crate) fn layout(&self, id: AssetId) -> Option<AssetMeshLayout> {
        self.table().resident.get(&id).map(|r| r.layout)
    }

    /// The upload serial of the mesh a frame key names, while it is resident.
    pub(crate) fn serial_by_key(&self, key: u64) -> Option<u64> {
        let table = self.table();
        let id = table.by_key.get(&key)?;
        table.resident.get(id).map(|r| r.serial)
    }

    /// Whether the mesh a frame key names is resident.
    #[must_use]
    pub fn is_resident_key(&self, key: u64) -> bool {
        self.table().by_key.contains_key(&key)
    }

    /// Meshes with buffers on the GPU.
    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.table().resident.len()
    }

    /// Bytes of every resident mesh's buffers.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.table().resident.values().map(|r| r.buffers.bytes()).sum()
    }

    /// Uploads so far (a reload uploads again) and frees so far.
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        let t = self.table();
        (t.uploads, t.frees)
    }

    /// Hands the GPU work queued by uploads to the device (tests; a frame submits it anyway).
    pub fn flush(&self) {
        self.queue.submit(std::iter::empty());
    }
}

impl PrepareAsset for AssetMeshUploads {
    type Asset = CpuMesh;

    fn prepare(&self, id: AssetId, mesh: &CpuMesh) -> Result<u64, String> {
        let _span = tracing::info_span!("asset_mesh_upload").entered();
        if mesh.positions.is_empty() || mesh.indices.is_empty() {
            return Err(format!("mesh {id} has no vertices or no indices to upload"));
        }
        if let Some(&bad) = mesh.indices.iter().find(|&&i| i as usize >= mesh.positions.len()) {
            return Err(format!("mesh {id} has index {bad} past its last vertex"));
        }
        let index_count =
            u32::try_from(mesh.indices.len()).map_err(|_| format!("mesh {id} has too many indices"))?;
        // The vertices `load_cooked_model` draws (MikkTSpace tangents, white, rigid), so a counted
        // mesh draws the same pixels as the CPU path; a skin that fits one palette adds its four
        // strongest influences, as `CpuMesh::from_cooked` does (ADR-8610).
        let skinned = mesh
            .skin
            .as_ref()
            .is_some_and(|skin| skin.joint_names.len() <= crate::MAX_JOINTS_PER_SKIN);
        let vertices = if skinned {
            crate::cooked_model::skinned_lab_vertices(mesh)
        } else {
            crate::cooked_model::lab_vertices(mesh)
        };
        let skin = skinned.then(|| Arc::new(crate::frame::asset_mesh_skin(&vertices)));
        // Morph targets packed against the vertices uploaded, as `MorphTargets::from_cooked` does
        // on the CPU path (ADR-7721): the deltas and the moved vertices' base, no vertex copy.
        let morph = if mesh.morph_targets.is_empty() {
            None
        } else {
            match crate::MorphTargets::new(&vertices, &mesh.morph_targets) {
                Ok(targets) => Some(Arc::new(targets)),
                Err(error) => {
                    tracing::warn!(target: "regia_render", %id, %error, "counted mesh morph targets refused; it draws undeformed");
                    None
                }
            }
        };
        let extent = vertices
            .iter()
            .fold([[f32::MAX; 3], [f32::MIN; 3]], |[lo, hi], v| {
                [
                    std::array::from_fn(|i| lo[i].min(v.position[i])),
                    std::array::from_fn(|i| hi[i].max(v.position[i])),
                ]
            });
        // The frame's mesh arenas' usages: a vertex or index stream, and a BLAS input where the
        // device builds acceleration structures. The vertices are a storage input too where the
        // device has the visibility-buffer path: its clusters repack them (ADR-7462).
        let blas = crate::mesh_storage::blas_input_usage(self.device.features());
        let vertex = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("asset_mesh_vertex"),
            contents: bytemuck::cast_slice(&vertices),
            // `COPY_SRC`: a morphed instance's copy, a compute-skinning source and the probes'
            // brick readback copy it on the GPU (ADR-7721).
            usage: wgpu::BufferUsages::VERTEX
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC
                | blas
                | self.vis_storage,
        });
        let index = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("asset_mesh_index"),
            contents: bytemuck::cast_slice(&mesh.indices),
            usage: wgpu::BufferUsages::INDEX
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC
                | blas,
        });
        let buffers = AssetMeshBuffers {
            vertex,
            index,
            index_count,
        };
        let bytes = buffers.bytes();
        let bounds = crate::frame::mesh_bounds(&vertices);
        let uv_span = crate::texture_stream::uv_span(&vertices, &mesh.indices);
        let layout = AssetMeshLayout {
            vertex_count: vertices.len(),
            index_hash: index_list_hash(&mesh.indices),
            source_hash: source_hash(&mesh.positions, &mesh.indices),
        };
        let mut table = self.table();
        table.uploads += 1;
        let serial = table.uploads;
        let key = asset_mesh_key(id);
        if let Some(other) = table.by_key.insert(key, id)
            && other != id
        {
            tracing::warn!(target: "regia_render", %id, %other, "two counted meshes share a frame key; the newer one draws");
        }
        // A reload replaces the old copy. Its buffers are dropped, not destroyed: a frame that
        // adopted them may still be recording; they are freed when its entry goes too.
        table.resident.insert(
            id,
            ResidentAssetMesh {
                buffers,
                bounds,
                uv_span,
                serial,
                layout,
                skin,
                morph,
                extent,
            },
        );
        Ok(bytes)
    }

    fn unload(&self, id: AssetId) {
        let mut table = self.table();
        if let Some(old) = table.resident.remove(&id) {
            let key = asset_mesh_key(id);
            if table.by_key.get(&key) == Some(&id) {
                table.by_key.remove(&key);
            }
            // Dropped, not destroyed: the memory goes with the last handle, at the latest when a
            // renderer that adopted the mesh drops its entry at the start of its next frame.
            drop(old);
            table.frees += 1;
        }
    }
}

/// What the probes' size test reads of a resident counted mesh without CPU vertices (ADR-7721).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CountedProxyInfo {
    /// Object-space box of the uploaded vertices.
    pub(crate) lo: [f32; 3],
    /// Object-space box of the uploaded vertices.
    pub(crate) hi: [f32; 3],
    /// Uploaded vertices.
    pub(crate) vertices: usize,
    /// Uploaded indices.
    pub(crate) indices: usize,
}

/// The uploaded geometry of a counted mesh, read back from its GPU buffers for one brick bake
/// (ADR-7721); dropped once the bake has its copies.
#[derive(Debug, Default)]
pub(crate) struct CountedGiGeometry {
    /// Vertex positions, object space.
    pub(crate) positions: Vec<[f32; 3]>,
    /// Vertex normals.
    pub(crate) normals: Vec<[f32; 3]>,
    /// Triangle indices.
    pub(crate) indices: Vec<u32>,
}

/// A readback in flight: the staging buffer, the vertex bytes at its start, the upload serial it
/// copied, and whether the map completed (`Some(ok)`).
struct GiRead {
    staging: wgpu::Buffer,
    vertex_bytes: u64,
    serial: u64,
    done: Arc<Mutex<Option<bool>>>,
}

/// The probes' source of counted meshes' geometry (ADR-7721): a counted mesh keeps no CPU
/// vertices, so the signed-distance brick the probes trace it with is baked from a GPU readback of
/// its uploaded buffers, made on the brick's miss path only (once per mesh, again after its brick
/// was freed or the mesh uploaded again). The bake reads exactly the vertices the frame draws.
#[derive(Clone)]
pub(crate) struct CountedGiSource {
    uploads: AssetMeshUploads,
    reads: Arc<Mutex<HashMap<u64, GiRead>>>,
}

impl CountedGiSource {
    /// A source reading `uploads`' buffers.
    pub(crate) fn new(uploads: AssetMeshUploads) -> Self {
        Self {
            uploads,
            reads: Arc::default(),
        }
    }

    /// The size-test inputs of the resident counted mesh frame key `key` (`None`: not resident,
    /// or not a counted whole-mesh key).
    pub(crate) fn info(&self, key: u64) -> Option<CountedProxyInfo> {
        let resident = self.uploads.resident_by_key(key)?;
        let [lo, hi] = resident.extent;
        Some(CountedProxyInfo {
            lo,
            hi,
            vertices: resident.layout.vertex_count,
            indices: resident.buffers.index_count as usize,
        })
    }

    /// The uploaded geometry of counted mesh `key` once its readback has landed. The first call
    /// for a mesh starts the readback (a copy of its buffers and a map); with `wait` the call
    /// blocks until it lands (synchronous bakes: tests, headless renders), otherwise a later
    /// frame's call returns it. `None` while it is in flight, or when the mesh is not resident.
    pub(crate) fn fetch(&self, key: u64, wait: bool) -> Option<CountedGiGeometry> {
        let resident = self.uploads.resident_by_key(key)?;
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        // A readback of an older upload (a reload since) is dropped.
        if reads.get(&key).is_some_and(|read| read.serial != resident.serial) {
            reads.remove(&key);
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = reads.entry(key) {
            let _span = tracing::info_span!("counted_gi_readback").entered();
            let vertex = &resident.buffers.vertex;
            let index = &resident.buffers.index;
            let staging = self.uploads.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("counted gi readback"),
                size: vertex.size() + index.size(),
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut encoder = self
                .uploads
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("counted gi readback"),
                });
            encoder.copy_buffer_to_buffer(vertex, 0, &staging, 0, vertex.size());
            encoder.copy_buffer_to_buffer(index, 0, &staging, vertex.size(), index.size());
            self.uploads.queue.submit([encoder.finish()]);
            let done: Arc<Mutex<Option<bool>>> = Arc::default();
            let flag = Arc::clone(&done);
            staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
                *flag.lock().unwrap_or_else(PoisonError::into_inner) = Some(result.is_ok());
            });
            self.uploads.table().gi_readbacks += 1;
            slot.insert(GiRead {
                staging,
                vertex_bytes: vertex.size(),
                serial: resident.serial,
                done,
            });
        }
        if wait {
            // Blocks the caller only: synchronous bakes already wait for their bricks.
            let _ = self.uploads.device.poll(wgpu::PollType::wait_indefinitely());
        }
        let landed = reads
            .get(&key)
            .and_then(|read| *read.done.lock().unwrap_or_else(PoisonError::into_inner))?;
        let read = reads.remove(&key)?;
        if !landed {
            tracing::warn!(target: "regia_render", key, "counted gi readback failed; the next frame tries again");
            return None;
        }
        let geometry = {
            let mapped = read.staging.slice(..).get_mapped_range().ok()?;
            let split = usize::try_from(read.vertex_bytes).ok()?.min(mapped.len());
            let vertices: Vec<crate::LabVertex> = bytemuck::pod_collect_to_vec(&mapped[..split]);
            CountedGiGeometry {
                positions: vertices.iter().map(|v| v.position).collect(),
                normals: vertices.iter().map(|v| v.normal).collect(),
                indices: bytemuck::pod_collect_to_vec(&mapped[split..]),
            }
        };
        read.staging.unmap();
        Some(geometry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_mesh_keys_keep_their_tag_and_clear_the_lod_and_dither_bits() {
        for _ in 0..64 {
            let key = asset_mesh_key(AssetId::new_random());
            assert!(is_asset_mesh_key(key));
            assert_eq!(key & 0b111, 0, "LOD bits 1-2 and the dither bit 0 stay clear");
            assert!(!crate::frame::is_shadow_lod_key(key));
            assert_ne!(key >> 56, crate::EPHEMERAL_KEY >> 56);
            assert_ne!(key >> 56, crate::MESH_ID_KEY_TAG >> 56);
            assert_ne!(key >> 56, crate::MORPH_MESH_KEY_TAG >> 56);
        }
        let id = AssetId::new_random();
        assert_eq!(asset_mesh_key(id), asset_mesh_key(id), "stable");
    }

    #[test]
    fn every_lod_shadow_and_fade_variant_of_a_counted_key_finds_its_base_and_lod() {
        use crate::mesh_lod::{lod_variant_key, mesh_lod_cache_key};
        for _ in 0..64 {
            let base = asset_mesh_key(AssetId::new_random());
            for lod in 0..4usize {
                for shadow in [false, true] {
                    let variant = lod_variant_key(base, mesh_lod_cache_key(lod, shadow));
                    assert_eq!(
                        counted_base_key(variant),
                        Some((base, lod)),
                        "lod {lod} shadow {shadow}"
                    );
                    if !shadow {
                        let faded = variant ^ crate::frame::LOD_DITHER_MESH_TAG;
                        assert_eq!(counted_base_key(faded), Some((base, lod)), "faded lod {lod}");
                    }
                }
            }
        }
        assert_eq!(counted_base_key(crate::MESH_ID_KEY_TAG | (7 << 3)), None);
        assert_eq!(
            counted_base_key((crate::MESH_ID_KEY_TAG | (7 << 3)) ^ crate::frame::LOD_DITHER_MESH_TAG),
            None
        );
        assert_eq!(counted_base_key(0), None);
    }

    #[test]
    fn the_buffered_source_hash_is_the_lightmap_source_hash() {
        let positions = [[0.0f32, -0.0, 1.5], [2.0, 3.25, -4.0], [1.0, 0.5, 0.25]];
        let indices = [0u32, 1, 2, 2, 1, 0];
        assert_eq!(
            source_hash(&positions, &indices),
            regia_asset::lightmap::lightmap_source_hash(positions.iter().copied(), &indices)
        );
        assert_eq!(
            source_hash(&[], &[]),
            regia_asset::lightmap::lightmap_source_hash(std::iter::empty(), &[])
        );
        assert_ne!(index_list_hash(&indices), index_list_hash(&[0, 1, 2]));
    }
}
