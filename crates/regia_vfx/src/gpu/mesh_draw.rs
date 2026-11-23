// SPDX-License-Identifier: MPL-2.0
//! Mesh-asset particles (`.vfx.ron` v8 `render.mesh`, ADR-7210, ADR-7261).
//!
//! A [`crate::format::RenderMode::Mesh`] emitter with a `mesh` draws that registered mesh
//! ([`crate::scene::VfxLibrary::add_mesh`]) instead of the built-in cube. The first use of a
//! mesh uploads it once into its own vertex and index buffers: three vertices per triangle, each
//! `[x, y, z, slot]` plus the triangle's face normal (the slot is the one the emitter record
//! holds in `events.y`).
//!
//! Each frame, after the simulation, a three-dispatch compute pass buckets the mesh list by
//! render-mesh slot (ADR-7261): `mesh_bucket_rank` gives every entry its rank among the entries
//! of its slot inside its 256-entry chunk and counts each slot per chunk, `mesh_bucket_scan`
//! turns the counts into each slot's start and each chunk's offset inside it (and writes each
//! slot's instance count into its indexed indirect argument block), and `mesh_bucket_scatter`
//! copies every entry to `start + chunk offset + rank`. The scatter keeps the mesh list's
//! back-to-front order inside every slot, and the result is the same on every run (no atomics
//! decide a position). The draw then issues one `draw_indexed_indirect` per registered mesh over
//! only that mesh's instances: the vertex shader reads the bucketed list (bound in place of the
//! mesh list) at its slot's start. The built-in cube draw is unchanged (it skips mesh-asset
//! instances in the vertex shader). With no registered mesh nothing of this runs.
//!
//! A mesh whose emitter is `lit` is shaded by the frame's sun and ambient (Lambert on the face
//! normal; under a sky, the sky from above and the ground from below) and draws its front faces
//! only (counter-clockwise, glTF's convention); an unlit mesh keeps the flat tint and both faces.

use std::collections::HashMap;
use std::sync::Arc;

use regia_render::TrackedDevice;

use crate::mesh_shape::VfxMesh;

use super::layout::EmitterGpu;

/// Most distinct render meshes the GPU holds at once (slots `1..=MAX_RENDER_MESHES`).
pub const MAX_RENDER_MESHES: u32 = 64;

/// Most triangles one render mesh may have (a particle mesh is a shard, a rock, a leaf).
pub const MAX_RENDER_MESH_TRIANGLES: usize = 65_536;

/// The emitter-record slot (`EmitterGpu::events[1]`) of a mesh particle that draws nothing: its
/// mesh is not registered or was refused.
pub const RENDER_MESH_NONE: u32 = u32::MAX;

/// Bytes of one `draw_indexed_indirect` argument block.
const ARGS_STRIDE: u64 = 20;

/// Entries of one bucketing chunk (the rank and scatter workgroup size).
const BUCKET_CHUNK: u32 = 256;

/// Buckets per chunk: slot 0 (the cube, not bucketed) and slots `1..=MAX_RENDER_MESHES`.
const BUCKETS: u32 = MAX_RENDER_MESHES + 1;

/// Head entries of the bucketed list: `(start, count)` of slot `k` at entry `k`; the bucketed
/// entries follow.
pub(crate) const BUCKET_HEAD: u32 = BUCKETS;

/// `vec4` words of one emitter record, and the word that holds `events`.
const EMITTER_VEC4S: usize = std::mem::size_of::<EmitterGpu>() / 16;
const EMITTER_EVENTS_VEC4: usize = std::mem::offset_of!(EmitterGpu, events) / 16;
const _: () = assert!(std::mem::size_of::<EmitterGpu>().is_multiple_of(16));
const _: () = assert!(std::mem::offset_of!(EmitterGpu, events).is_multiple_of(16));

/// The mesh-list bucketing kernels. `sort_n` (the list region size) is a pipeline override; the
/// mesh list is `lists[2 sort_n ..]`, its length `counters[2]`, capped by `bound.x` (the CPU's
/// upper bound of live mesh particles, which also sizes the dispatch).
pub const MESH_BUCKET: &str = r#"
override sort_n: u32 = 64u;
// `mesh_draw::EMITTER_VEC4S` and `EMITTER_EVENTS_VEC4`: the emitter record as raw words.
override emitter_vec4s: u32 = 17u;
override emitter_events_vec4: u32 = 12u;

const CHUNK: u32 = 256u;
const BUCKETS: u32 = 65u;

@group(0) @binding(0) var<storage, read> counters: array<u32>;
@group(0) @binding(1) var<storage, read> lists: array<vec2<u32>>;
@group(0) @binding(2) var<storage, read> attrib: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> emitters: array<vec4<u32>>;
// Ranks `[0, sort_n)`, then the chunk table `[sort_n, sort_n + chunks × BUCKETS)`.
@group(0) @binding(4) var<storage, read_write> scratch: array<u32>;
// Head `(start, count)` per slot `[0, BUCKETS)`, then the bucketed entries.
@group(0) @binding(5) var<storage, read_write> buckets: array<vec2<u32>>;
// One `draw_indexed_indirect` block (5 words) per slot `1..BUCKETS`.
@group(0) @binding(6) var<storage, read_write> args: array<u32>;
@group(0) @binding(7) var<uniform> bound: vec4<u32>;

var<workgroup> wg_slot: array<u32, 256>;
var<workgroup> wg_count: u32;
var<workgroup> wg_total: array<u32, 64>;

fn live_count() -> u32 {
    return min(min(counters[2u], bound.x), sort_n);
}

// The bucket of mesh-list entry `i`: its render-mesh slot, or 0 for the cube and for a mesh
// that draws nothing (`RENDER_MESH_NONE`).
fn slot_of(i: u32) -> u32 {
    let p = lists[2u * sort_n + i].y;
    let e = u32(attrib[p].w);
    let s = emitters[e * emitter_vec4s + emitter_events_vec4].y;
    return select(0u, s, s < BUCKETS);
}

@compute @workgroup_size(256)
fn mesh_bucket_rank(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_index) t: u32,
) {
    if (t == 0u) {
        wg_count = live_count();
    }
    let count = workgroupUniformLoad(&wg_count);
    let start = wg.x * CHUNK;
    if (start >= count) {
        return;
    }
    let i = start + t;
    var slot = 0u;
    if (i < count) {
        slot = slot_of(i);
    }
    wg_slot[t] = slot;
    workgroupBarrier();
    // Rank among the earlier entries of the same slot in this chunk (stable).
    var rank = 0u;
    for (var j = 0u; j < t; j = j + 1u) {
        rank = rank + select(0u, 1u, wg_slot[j] == slot);
    }
    if (i < count && slot != 0u) {
        scratch[i] = rank;
    }
    // Thread b counts slot b in this chunk (entries past the list are slot 0, never counted).
    if (t != 0u && t < BUCKETS) {
        var n = 0u;
        for (var j = 0u; j < CHUNK; j = j + 1u) {
            n = n + select(0u, 1u, wg_slot[j] == t);
        }
        scratch[sort_n + wg.x * BUCKETS + t] = n;
    }
}

@compute @workgroup_size(64)
fn mesh_bucket_scan(@builtin(local_invocation_index) t: u32) {
    if (t == 0u) {
        wg_count = live_count();
    }
    // Uniform, so the loop below ends in uniform control flow before the barrier.
    let count = workgroupUniformLoad(&wg_count);
    let chunks = (count + CHUNK - 1u) / CHUNK;
    let b = t + 1u;
    // Each chunk's offset inside slot b, in chunk order.
    var total = 0u;
    for (var c = 0u; c < chunks; c = c + 1u) {
        let k = sort_n + c * BUCKETS + b;
        let n = scratch[k];
        scratch[k] = total;
        total = total + n;
    }
    wg_total[t] = total;
    workgroupBarrier();
    var start = 0u;
    for (var j = 0u; j < t; j = j + 1u) {
        start = start + wg_total[j];
    }
    buckets[b] = vec2<u32>(start, total);
    args[t * 5u + 1u] = total;
}

@compute @workgroup_size(256)
fn mesh_bucket_scatter(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_index) t: u32,
) {
    let i = wg.x * CHUNK + t;
    if (i >= live_count()) {
        return;
    }
    let s = slot_of(i);
    if (s == 0u) {
        return;
    }
    let dst = buckets[s].x + scratch[sort_n + wg.x * BUCKETS + s] + scratch[i];
    buckets[BUCKETS + dst] = lists[2u * sort_n + i];
}
"#;

/// Why the GPU refused a render mesh (the agent reports these as `bad_mesh`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderMeshRefusal {
    /// Every slot is taken.
    #[error("render mesh slots full ({MAX_RENDER_MESHES})")]
    SlotsFull,
    /// More triangles than a particle mesh may draw.
    #[error("render mesh has {0} triangles, more than {MAX_RENDER_MESH_TRIANGLES}")]
    TooManyTriangles(usize),
}

/// One registered mesh's draw on the last simulated frame ([`super::VfxGpu::read_render_mesh_draws`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMeshDraw {
    /// The registered name (`render.mesh`).
    pub mesh: String,
    /// Its render-mesh slot (`1..=`[`MAX_RENDER_MESHES`]).
    pub slot: u32,
    /// Indices one instance draws (three per triangle).
    pub indices: u32,
    /// Instances the draw covers: that mesh's live particles, nothing else.
    pub instances: u32,
}

/// One uploaded render mesh.
struct RenderMeshGpu {
    /// The library mesh it was built from (a re-registered name uploads again).
    source: Arc<VfxMesh>,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// The bucketing kernels, built with the context (no compile on the frame path).
pub(crate) struct BucketPipes {
    bgl: wgpu::BindGroupLayout,
    rank: wgpu::ComputePipeline,
    scan: wgpu::ComputePipeline,
    scatter: wgpu::ComputePipeline,
}

impl BucketPipes {
    /// Compiles the three kernels for a list region of `sort_n` entries.
    pub(crate) fn new(device: &TrackedDevice, sort_n: u32) -> Self {
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx mesh bucket bgl"),
            entries: &[
                storage(0, true),
                storage(1, true),
                storage(2, true),
                storage(3, true),
                storage(4, false),
                storage(5, false),
                storage(6, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx mesh bucket layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx mesh bucket"),
            source: wgpu::ShaderSource::Wgsl(MESH_BUCKET.into()),
        });
        let constants = [
            ("sort_n", f64::from(sort_n)),
            ("emitter_vec4s", EMITTER_VEC4S as f64),
            ("emitter_events_vec4", EMITTER_EVENTS_VEC4 as f64),
        ];
        let pipe = |label: &str, entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &constants,
                    ..Default::default()
                },
                cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
            })
        };
        Self {
            rank: pipe("vfx mesh bucket rank", "mesh_bucket_rank"),
            scan: pipe("vfx mesh bucket scan", "mesh_bucket_scan"),
            scatter: pipe("vfx mesh bucket scatter", "mesh_bucket_scatter"),
            bgl,
        }
    }
}

/// The simulation buffers the bucketing reads.
pub(crate) struct BucketInputs<'a> {
    pub(crate) counters: &'a wgpu::Buffer,
    pub(crate) lists: &'a wgpu::Buffer,
    pub(crate) attrib: &'a wgpu::Buffer,
    pub(crate) emitters: &'a wgpu::Buffer,
}

/// The per-context buffers of the mesh-asset draw, created with the first registered mesh.
struct BucketGpu {
    /// Head `(start, count)` per slot, then the bucketed list (bound as the mesh-asset
    /// pipeline's `lists`).
    buckets: wgpu::Buffer,
    /// One indexed indirect block per slot: index count (written at upload), instance count
    /// (written by the scan every frame), zeros.
    args: wgpu::Buffer,
    /// `x`: the live mesh-particle bound of this frame.
    bound: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// The bound last written, so an unchanged frame writes nothing.
    last_bound: Option<u32>,
}

/// The render meshes uploaded so far, their indirect arguments and the bucketing state.
pub(crate) struct RenderMeshes {
    by_name: HashMap<String, u32>,
    /// Slot `k` is `meshes[k - 1]`.
    meshes: Vec<RenderMeshGpu>,
    pipes: BucketPipes,
    sort_n: u32,
    gpu: Option<BucketGpu>,
}

impl RenderMeshes {
    pub(crate) fn new(device: &TrackedDevice, sort_n: u32) -> Self {
        Self {
            by_name: HashMap::new(),
            meshes: Vec::new(),
            pipes: BucketPipes::new(device, sort_n),
            sort_n,
            gpu: None,
        }
    }

    /// Registered render meshes.
    pub(crate) fn len(&self) -> u32 {
        self.meshes.len() as u32
    }

    /// No render mesh is registered (the common case: nothing to draw).
    pub(crate) fn is_empty(&self) -> bool {
        self.meshes.is_empty()
    }

    /// Index count of slot `slot` (1-based), if uploaded.
    pub(crate) fn index_count(&self, slot: u32) -> Option<u32> {
        let index = slot.checked_sub(1)? as usize;
        self.meshes.get(index).map(|m| m.index_count)
    }

    /// The bucketed list, once a mesh is registered (the mesh-asset render bind group binds it
    /// in place of the particle lists).
    pub(crate) fn bucket_buffer(&self) -> Option<&wgpu::Buffer> {
        self.gpu.as_ref().map(|g| &g.buckets)
    }

    /// The slot of `name`, uploading `mesh` on first use (or when the library replaced it).
    pub(crate) fn slot(
        &mut self,
        device: &TrackedDevice,
        queue: &wgpu::Queue,
        name: &str,
        mesh: &Arc<VfxMesh>,
    ) -> Result<u32, RenderMeshRefusal> {
        if let Some(&slot) = self.by_name.get(name) {
            let index = (slot - 1) as usize;
            if Arc::ptr_eq(&self.meshes[index].source, mesh) {
                return Ok(slot);
            }
            let gpu = upload(device, queue, slot, mesh)?;
            self.write_args(queue, slot, gpu.index_count);
            self.meshes[index] = gpu;
            return Ok(slot);
        }
        if self.meshes.len() as u32 >= MAX_RENDER_MESHES {
            return Err(RenderMeshRefusal::SlotsFull);
        }
        let slot = self.meshes.len() as u32 + 1;
        let gpu = upload(device, queue, slot, mesh)?;
        let index_count = gpu.index_count;
        self.meshes.push(gpu);
        self.by_name.insert(name.to_string(), slot);
        self.write_args(queue, slot, index_count);
        Ok(slot)
    }

    /// Writes slot `slot`'s static argument words (index count; the instance count is written
    /// per frame by the scan, the rest is zero).
    fn write_args(&mut self, queue: &wgpu::Queue, slot: u32, index_count: u32) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        queue.write_buffer(
            &gpu.args,
            u64::from(slot - 1) * ARGS_STRIDE,
            bytemuck::bytes_of(&index_count),
        );
    }

    /// Creates the bucketing buffers on the first registered mesh. Returns `true` when it did
    /// (the caller then builds the mesh-asset render bind group).
    pub(crate) fn ensure_buffers(
        &mut self,
        device: &TrackedDevice,
        queue: &wgpu::Queue,
        inputs: &BucketInputs<'_>,
    ) -> bool {
        if self.gpu.is_some() || self.meshes.is_empty() {
            return false;
        }
        let _span = tracing::info_span!("vfx_render_mesh_buffers", sort_n = self.sort_n).entered();
        let chunks = self.sort_n.div_ceil(BUCKET_CHUNK);
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let storage = wgpu::BufferUsages::STORAGE;
        let scratch = buffer(
            "vfx render mesh scratch",
            (u64::from(self.sort_n) + u64::from(chunks) * u64::from(BUCKETS)) * 4,
            storage,
        );
        let buckets = buffer(
            "vfx render mesh buckets",
            (u64::from(BUCKET_HEAD) + u64::from(self.sort_n)) * 8,
            storage | wgpu::BufferUsages::COPY_SRC,
        );
        let args = buffer(
            "vfx render mesh args",
            u64::from(MAX_RENDER_MESHES) * ARGS_STRIDE,
            storage
                | wgpu::BufferUsages::INDIRECT
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
        );
        let bound = buffer(
            "vfx render mesh bound",
            16,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        fn entry(binding: u32, buf: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: buf.as_entire_binding(),
            }
        }
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx mesh bucket bg"),
            layout: &self.pipes.bgl,
            entries: &[
                entry(0, inputs.counters),
                entry(1, inputs.lists),
                entry(2, inputs.attrib),
                entry(3, inputs.emitters),
                entry(4, &scratch),
                entry(5, &buckets),
                entry(6, &args),
                entry(7, &bound),
            ],
        });
        self.gpu = Some(BucketGpu {
            buckets,
            args,
            bound,
            bind_group,
            last_bound: None,
        });
        let counts: Vec<u32> = self.meshes.iter().map(|m| m.index_count).collect();
        for (index, count) in counts.into_iter().enumerate() {
            self.write_args(queue, index as u32 + 1, count);
        }
        true
    }

    /// Records the bucketing of this frame's mesh list (after the simulation's
    /// `prepare_indirect`, in the same pass). `bound` is the upper bound of live mesh particles
    /// ([`super::VfxGpu::set_mesh_bound`]; 0 = none).
    pub(crate) fn record_buckets(
        &mut self,
        queue: &wgpu::Queue,
        pass: &mut wgpu::ComputePass<'_>,
        bound: u32,
    ) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let bound = bound.min(self.sort_n);
        if gpu.last_bound != Some(bound) {
            queue.write_buffer(&gpu.bound, 0, bytemuck::cast_slice(&[bound, 0u32, 0, 0]));
            gpu.last_bound = Some(bound);
        }
        let groups = bound.div_ceil(BUCKET_CHUNK);
        pass.set_bind_group(0, Some(&gpu.bind_group), &[]);
        if groups > 0 {
            pass.set_pipeline(&self.pipes.rank);
            pass.dispatch_workgroups(groups, 1, 1);
        }
        // The scan always runs: with no mesh particle it writes zero instances.
        pass.set_pipeline(&self.pipes.scan);
        pass.dispatch_workgroups(1, 1, 1);
        if groups > 0 {
            pass.set_pipeline(&self.pipes.scatter);
            pass.dispatch_workgroups(groups, 1, 1);
        }
    }

    /// Records one indexed indirect draw per registered mesh (pipeline and bind groups 0 and 1
    /// already set by the caller to the mesh-asset pipeline and the bucketed particle group).
    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        for (index, mesh) in self.meshes.iter().enumerate() {
            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed_indirect(&gpu.args, index as u64 * ARGS_STRIDE);
        }
    }

    /// Reads every registered mesh's draw of the last submitted frame (blocks; tests and the
    /// agent). Empty with no registered mesh or when the read fails.
    pub(crate) fn read_draws(&self, device: &TrackedDevice, queue: &wgpu::Queue) -> Vec<RenderMeshDraw> {
        let Some(gpu) = self.gpu.as_ref() else {
            return Vec::new();
        };
        let size = u64::from(self.len()) * ARGS_STRIDE;
        if size == 0 {
            return Vec::new();
        }
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx render mesh args read"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_buffer(&gpu.args, 0, &staging, 0, size);
        queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let words: Option<Vec<u32>> = (|| {
            device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
            rx.recv().ok()?.ok()?;
            let data = slice.get_mapped_range().ok()?;
            Some(bytemuck::cast_slice::<u8, u32>(&data[..]).to_vec())
        })();
        staging.unmap();
        let Some(words) = words else {
            return Vec::new();
        };
        let mut names: Vec<(&String, u32)> = self.by_name.iter().map(|(n, &s)| (n, s)).collect();
        names.sort_by_key(|(_, slot)| *slot);
        names
            .into_iter()
            .filter_map(|(name, slot)| {
                let block = words.get((slot as usize - 1) * 5..(slot as usize - 1) * 5 + 2)?;
                Some(RenderMeshDraw {
                    mesh: name.clone(),
                    slot,
                    indices: block[0],
                    instances: block[1],
                })
            })
            .collect()
    }
}

/// One mesh-asset vertex: position and slot, then the face normal (`w` unused).
pub(crate) const VERTEX_STRIDE: u64 = 32;

/// The face normals of `positions` / `triangles`, unit length (`+z` for a degenerate triangle),
/// with each triangle's three vertices emitted on their own: `[x, y, z, slot]`, `[nx, ny, nz, 0]`.
fn flat_vertices(mesh: &VfxMesh, slot: u32) -> Vec<[f32; 8]> {
    let positions = mesh.positions();
    let mut out = Vec::with_capacity(mesh.triangles().len() * 3);
    for tri in mesh.triangles() {
        let [a, b, c] = tri.map(|i| positions.get(i as usize).copied().unwrap_or_default());
        let n = (b - a).cross(c - a).normalize_or_zero();
        let n = if n == regia_core::math::Vec3::ZERO {
            regia_core::math::Vec3::Z
        } else {
            n
        };
        for p in [a, b, c] {
            out.push([p.x, p.y, p.z, slot as f32, n.x, n.y, n.z, 0.0]);
        }
    }
    out
}

/// Uploads `mesh` as slot `slot`: three vertices per triangle (position, slot, face normal) and
/// the sequential index list.
fn upload(
    device: &TrackedDevice,
    queue: &wgpu::Queue,
    slot: u32,
    mesh: &Arc<VfxMesh>,
) -> Result<RenderMeshGpu, RenderMeshRefusal> {
    let triangles = mesh.triangles();
    if triangles.len() > MAX_RENDER_MESH_TRIANGLES {
        return Err(RenderMeshRefusal::TooManyTriangles(triangles.len()));
    }
    let _span = tracing::info_span!("vfx_render_mesh_upload", triangles = triangles.len()).entered();
    let vertices = flat_vertices(mesh, slot);
    let indices: Vec<u32> = (0..vertices.len() as u32).collect();
    let buffer = |label: &str, bytes: &[u8], usage: wgpu::BufferUsages| {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (bytes.len() as u64).max(4),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytes);
        buffer
    };
    Ok(RenderMeshGpu {
        source: Arc::clone(mesh),
        vertices: buffer(
            "vfx render mesh vertices",
            bytemuck::cast_slice(&vertices),
            wgpu::BufferUsages::VERTEX,
        ),
        indices: buffer(
            "vfx render mesh indices",
            bytemuck::cast_slice(&indices),
            wgpu::BufferUsages::INDEX,
        ),
        index_count: indices.len() as u32,
    })
}

/// The mesh-asset render pipeline of `format` (`pl_mesh` layout, the mesh particle module's
/// `vs_mesh_asset` and `fs_mesh`, premultiplied alpha, depth test without writes, like the cube).
/// Built with the format's other effect pipelines ([`super::PipeSource::build`]), so the pipeline
/// warm covers it.
pub(crate) fn build_pipeline(
    device: &TrackedDevice,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let premult_alpha = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("vfx mesh-asset particles"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_mesh_asset"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: VERTEX_STRIDE,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
            })],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_mesh"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(premult_alpha),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: regia_render::pipeline_cache::cache_for(&device).as_deref(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_kernel_reads_the_emitter_record_where_the_layout_puts_it() {
        assert_eq!(EMITTER_VEC4S, 17);
        assert_eq!(EMITTER_EVENTS_VEC4, 12);
        assert!(MESH_BUCKET.contains("override emitter_vec4s: u32 = 17u;"));
        assert!(MESH_BUCKET.contains("override emitter_events_vec4: u32 = 12u;"));
        assert!(MESH_BUCKET.contains(&format!("const BUCKETS: u32 = {BUCKETS}u;")));
        assert!(MESH_BUCKET.contains(&format!("const CHUNK: u32 = {BUCKET_CHUNK}u;")));
        // The scan's 64 threads serve slots 1..=64.
        assert_eq!(MAX_RENDER_MESHES, 64);
    }

    #[test]
    fn flat_vertices_carry_the_slot_and_the_outward_face_normal() {
        let mesh = VfxMesh::from_triangles(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], &[0, 1, 2])
            .expect("triangle");
        let v = flat_vertices(&mesh, 3);
        assert_eq!(v.len(), 3);
        for (i, p) in [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
            .iter()
            .enumerate()
        {
            assert_eq!(v[i][..3], p[..]);
            assert_eq!(v[i][3], 3.0, "slot in w");
            assert_eq!(
                v[i][4..],
                [0.0, 0.0, 1.0, 0.0],
                "counter-clockwise from +z faces +z"
            );
        }
    }
}
