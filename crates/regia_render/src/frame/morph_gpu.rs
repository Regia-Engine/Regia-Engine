// SPDX-License-Identifier: MPL-2.0
//! GPU side of morph targets ([`crate::morph`], Tincal engine item B1): the compute pipeline,
//! the packed data of each target set, each instance's weight and output buffers, the keys of
//! the instances' deformed copies, and the pre-pass that fills those copies.
//!
//! A morphed instance draws under a mesh key of its own ([`crate::MORPH_MESH_KEY_TAG`]): an
//! ordinary mesh-cache entry uploaded from the base vertices like any mesh. When the instance's
//! weights change, `cs_morph` writes the deformed moved-vertex span into the instance's output
//! buffer, and a buffer copy puts that span into the entry's vertex range (also after the entry
//! was uploaded again: eviction, memory pressure, a shader reload). Every pass then reads the
//! entry as it reads any mesh. Only morphed instances touch any of this.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use super::{FrameMesh, FrameRenderer, MeshEntry, binding, make_buffer, shader_module, storage_entry};
use crate::lab_frame::LabVertex;
use crate::mesh_storage::ArenaLease;
use crate::morph::{MORPH_MESH_KEY_TAG, MorphInstance, MorphStats, MorphTargets};
use crate::pipeline_warm::DeferredPipeline;

/// Invocations per workgroup (`@workgroup_size` in `morph.wgsl`).
const WORKGROUP: u32 = 64;
/// Most workgroups in one dispatch dimension; larger sets spill into `y`.
const MAX_GROUPS_X: u32 = 65_535;
/// Header words of an instance buffer before its weights.
const PARAM_WORDS: usize = 4;
/// Bytes of one vertex in a mesh's vertex buffer.
const VERTEX_BYTES: u64 = std::mem::size_of::<LabVertex>() as u64;
/// Bits of the deformed-copy key counter (below the tag byte, above the LOD bits).
const TWIN_ID_MASK: u64 = (1 << 53) - 1;

/// The morph pass's layout and pipeline (kept in [`super::Pipes`], so a renderer that adopts
/// warmed pipelines adopts it too).
#[derive(Default)]
pub(super) struct MorphPipes {
    /// The layout and a process-unique id: a bind group built against another layout rebinds.
    bgl: Option<(wgpu::BindGroupLayout, u64)>,
    pipeline: Option<DeferredPipeline<wgpu::ComputePipeline>>,
}

fn next_layout_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl MorphPipes {
    fn bgl(&mut self, d: &wgpu::Device) -> (wgpu::BindGroupLayout, u64) {
        self.bgl
            .get_or_insert_with(|| {
                let compute = wgpu::ShaderStages::COMPUTE;
                let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("morph targets bgl"),
                    entries: &[
                        storage_entry(0, compute, true),
                        storage_entry(1, compute, true),
                        storage_entry(2, compute, false),
                    ],
                });
                (bgl, next_layout_id())
            })
            .clone()
    }

    /// The pipeline, registered on first use: built right away in a warm (or without warm
    /// workers), in the background mid-play ([`DeferredPipeline::try_get`]).
    fn pipeline(&mut self, d: &wgpu::Device) -> DeferredPipeline<wgpu::ComputePipeline> {
        if let Some(pipeline) = &self.pipeline {
            return pipeline.clone();
        }
        let (bgl, _) = self.bgl(d);
        let device = wgpu::Device::clone(d);
        let pipeline = DeferredPipeline::new("morph targets", move || build_pipeline(&device, &bgl));
        self.pipeline = Some(pipeline.clone());
        pipeline
    }
}

fn build_pipeline(d: &wgpu::Device, bgl: &wgpu::BindGroupLayout) -> wgpu::ComputePipeline {
    // The engine's own shader with no defines: `shader_check` validates it, so a failure here is
    // a build defect, as for the other engine passes.
    let module = shader_module(
        d,
        "morph targets",
        include_str!("../../shaders/morph.wgsl"),
        "morph.wgsl",
        HashMap::new(),
    )
    .expect("morph.wgsl");
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("morph targets"),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });
    d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("morph targets"),
        layout: Some(&layout),
        module: &module,
        entry_point: Some("cs_morph"),
        compilation_options: Default::default(),
        cache: crate::pipeline_cache::cache_for(d).as_deref(),
    })
}

/// The packed data of one target set on the GPU (shared by its instances).
struct SetGpu {
    buf: wgpu::Buffer,
}

/// Where an instance's deformed span was last copied: a mesh entry re-uploaded since (a new
/// buffer, range or lease) gets the span again.
struct CopyTarget {
    vb: wgpu::Buffer,
    base_vertex: i32,
    /// The entry's vertex-range lease (`Weak`: pins the allocation, so a new lease never
    /// reuses its address while this is held).
    lease: Option<Weak<ArenaLease>>,
}

impl CopyTarget {
    fn of(entry: &MeshEntry) -> Self {
        Self {
            vb: entry.vb.clone(),
            base_vertex: entry.base_vertex,
            lease: entry.leases[0].as_ref().map(Arc::downgrade),
        }
    }

    fn matches(&self, entry: &MeshEntry) -> bool {
        let lease = match (&self.lease, &entry.leases[0]) {
            (None, None) => true,
            (Some(old), Some(new)) => std::ptr::eq(old.as_ptr(), Arc::as_ptr(new)),
            _ => false,
        };
        lease && self.base_vertex == entry.base_vertex && self.vb == entry.vb
    }
}

/// One instance's buffers.
struct InstanceGpu {
    /// Header + weights (`morph.wgsl` binding 1).
    params: wgpu::Buffer,
    /// The deformed moved-vertex span (binding 2), copied into the mesh entry.
    out: wgpu::Buffer,
    /// Bind group and the [`MorphPipes::bgl`] id it was built against.
    bg: Option<(wgpu::BindGroup, u64)>,
    /// This frame's and last frame's deformation while the weights change (ADR-2652).
    motion: Option<MorphMotion>,
}

/// The velocity pass's two poses of a morphing instance (ADR-2652): whole-mesh copies of the
/// deformed vertices, `full[cur]` this frame's and `full[cur ^ 1]` last frame's. Created with the
/// instance's other buffers on its first rigid draw (a weight change creates nothing) and written
/// only on frames whose weights change: steady weights cost no GPU work.
struct MorphMotion {
    full: [wgpu::Buffer; 2],
    cur: usize,
    /// `full[cur ^ 1]` does not hold last frame's span yet: the next dispatch copies the old
    /// span there first.
    prime: bool,
    /// The weights changed this frame: the velocity pass draws both poses.
    live: bool,
}

/// One deformed copy of an instance's mesh: an object drawn under several mesh keys (two
/// primitives, a dither switch) has one per key, and the pre-pass fills each one it draws.
struct Twin {
    /// The draw's mesh key.
    base: u64,
    /// The deformed copy's mesh key.
    key: u64,
    /// Where the deformed span was last copied into this copy.
    copied: Option<CopyTarget>,
    /// Drawn by the frame being built.
    used: bool,
    /// The draw is a counted mesh's (no CPU vertices, ADR-7721): the copy is a GPU copy of the
    /// adopted vertex buffer ([`FrameRenderer::ensure_counted_morph_twin`]).
    counted: bool,
}

/// One instance's state.
struct Instance {
    targets: Arc<MorphTargets>,
    /// Effective weights ([`MorphTargets::effective_weights`]).
    weights: Vec<f32>,
    /// Cull-bound growth for the weights, metres.
    inflate: f32,
    /// The deformed copies, one per mesh key the object is drawn with.
    twins: Vec<Twin>,
    gpu: Option<InstanceGpu>,
    /// The weights are in [`InstanceGpu::params`].
    uploaded: bool,
    /// [`InstanceGpu::out`] holds the deformed span for the weights.
    computed: bool,
    /// [`InstanceGpu::out`] holds a deformed span of some earlier weights (a change from it
    /// moves the surface: the velocity pass draws both poses).
    had_span: bool,
    /// Drawn this frame by a non-skinned colour draw (skinned morphs keep their palette motion).
    motion_wanted: bool,
    /// Drawn by the frame being built.
    drawn: bool,
}

impl Instance {
    fn new(targets: Arc<MorphTargets>, weights: Vec<f32>) -> Self {
        Self {
            inflate: targets.max_displacement(&weights),
            targets,
            weights,
            twins: Vec::new(),
            gpu: None,
            uploaded: false,
            computed: false,
            had_span: false,
            motion_wanted: false,
            drawn: false,
        }
    }
}

/// Per-renderer morph state ([`FrameRenderer::set_morph_instances`]).
#[derive(Default)]
pub(crate) struct MorphState {
    instances: HashMap<u64, Instance>,
    /// GPU data per [`MorphTargets::id`].
    sets: HashMap<u64, SetGpu>,
    next_twin: u64,
    /// Changes whenever any instance's weights or targets change (0 without instances).
    content_key: u64,
    stats: MorphStats,
    /// Deformed copies of counted draws (ADR-7721): copy key → the draw's mesh key.
    counted_twins: HashMap<u64, u64>,
    /// GPU copies that start a counted instance's buffers from the adopted vertex buffer
    /// (source, source offset, destination, destination offset, bytes), recorded first by the
    /// next [`encode`].
    seeds: Vec<Seed>,
}

impl MorphState {
    /// The instance table without GPU state (a shader reload rebuilds the renderer; the deformed
    /// copies are uploaded and filled again on their next draw).
    pub fn carry(&mut self) -> Self {
        let instances = std::mem::take(&mut self.instances)
            .into_iter()
            .map(|(key, instance)| (key, Instance::new(instance.targets, instance.weights)))
            .collect();
        Self {
            instances,
            sets: HashMap::new(),
            next_twin: self.next_twin,
            content_key: self.content_key,
            stats: MorphStats {
                buffers_created: self.stats.buffers_created,
                ..MorphStats::default()
            },
            counted_twins: HashMap::new(),
            seeds: Vec::new(),
        }
    }

    /// The draw's mesh key a counted instance's deformed copy `key` was made for (ADR-7721).
    pub fn counted_twin_source(&self, key: u64) -> Option<u64> {
        if self.counted_twins.is_empty() {
            return None;
        }
        self.counted_twins.get(&key).copied()
    }

    /// Queues a GPU copy into a counted instance's buffer (ADR-7721), recorded by the next
    /// [`encode`] before any morph work.
    pub fn seed(
        &mut self,
        src: &wgpu::Buffer,
        src_offset: u64,
        dst: &wgpu::Buffer,
        dst_offset: u64,
        bytes: u64,
    ) {
        seed_into(&mut self.seeds, src, (src_offset, dst_offset, bytes), dst);
    }

    /// Hash of the instance table (joins the static-screen key; 0 without instances).
    pub fn content_key(&self) -> u64 {
        self.content_key
    }

    /// Cull-bound growth of `object_key`'s deformed copy, metres.
    pub fn inflate(&self, object_key: u64) -> f32 {
        self.instances.get(&object_key).map_or(0.0, |i| i.inflate)
    }
}

/// A GPU copy of `bytes` from `src` at `offsets.0` into `dst` at `offsets.1` (ADR-7721).
type Seed = (wgpu::Buffer, u64, wgpu::Buffer, u64, u64);

/// Queues one [`Seed`] (`offsets`: source offset, destination offset, bytes).
fn seed_into(seeds: &mut Vec<Seed>, src: &wgpu::Buffer, offsets: (u64, u64, u64), dst: &wgpu::Buffer) {
    let (src_offset, dst_offset, bytes) = offsets;
    if bytes > 0 {
        seeds.push((src.clone(), src_offset, dst.clone(), dst_offset, bytes));
    }
}

/// Order-independent hash of one instance's state.
fn instance_hash(object_key: u64, targets: &MorphTargets, weights: &[f32]) -> u64 {
    let mut h = object_key.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ targets.id().rotate_left(29);
    for w in weights {
        h = h
            .wrapping_mul(0x1000_0000_01b3)
            .wrapping_add(u64::from(w.to_bits()));
    }
    h
}

impl FrameRenderer {
    /// Replaces the frame's morphed instances (Tincal B1, [`crate::morph`]), keyed by
    /// [`FrameMesh::object_key`]: a draw whose object has an instance draws its mesh deformed by
    /// the instance's weights. Pass [`crate::ExtractedFrame::morphs`] every frame; an empty slice
    /// clears them. Instances whose weights did not change since the last call cost no GPU work.
    ///
    /// A draw is deformed only when its mesh has the targets' vertex count (a LOD with other
    /// vertices draws undeformed, counted in [`MorphStats::mismatched`]).
    pub fn set_morph_instances(&mut self, instances: &[MorphInstance]) {
        let mut seen = HashSet::with_capacity(instances.len());
        let mut released = Vec::new();
        let mut content_key = 0u64;
        for given in instances {
            if !seen.insert(given.object_key) {
                continue;
            }
            let weights = given.targets.effective_weights(&given.weights, given.mode);
            content_key = content_key.wrapping_add(instance_hash(given.object_key, &given.targets, &weights));
            match self.morph.instances.entry(given.object_key) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let instance = slot.get_mut();
                    if !Arc::ptr_eq(&instance.targets, &given.targets) {
                        released.extend(instance.twins.iter().map(|t| t.key));
                        *instance = Instance::new(Arc::clone(&given.targets), weights);
                    } else if instance.weights != weights {
                        instance.inflate = instance.targets.max_displacement(&weights);
                        instance.weights = weights;
                        instance.uploaded = false;
                        instance.computed = false;
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(Instance::new(Arc::clone(&given.targets), weights));
                }
            }
        }
        self.morph.instances.retain(|key, instance| {
            let keep = seen.contains(key);
            if !keep {
                released.extend(instance.twins.iter().map(|t| t.key));
            }
            keep
        });
        for key in released {
            self.morph.counted_twins.remove(&key);
            self.meshes.remove(&key);
            self.rigid_skin.remove(&key);
        }
        let used: HashSet<u64> = self.morph.instances.values().map(|i| i.targets.id()).collect();
        self.morph.sets.retain(|id, _| used.contains(id));
        self.morph.content_key = content_key;
        self.morph.stats.instances = self.morph.instances.len();
    }

    /// The morph counters of the last frame ([`MorphStats`]).
    #[must_use]
    pub fn morph_stats(&self) -> MorphStats {
        let mut stats = self.morph.stats;
        stats.instances = self.morph.instances.len();
        stats.gpu_bytes = self.morph.sets.values().map(|s| s.buf.size()).sum::<u64>()
            + self
                .morph
                .instances
                .values()
                .filter_map(|i| i.gpu.as_ref())
                .map(|g| {
                    g.params.size()
                        + g.out.size()
                        + g.motion
                            .as_ref()
                            .map_or(0, |m| m.full[0].size() + m.full[1].size())
                })
                .sum::<u64>();
        stats
    }

    /// Reads back a morphed instance's deformed span as the last pre-pass wrote it (tests, the
    /// agent's `render.morph_readback`): `(first vertex, vertices)`, or `None` when `object_key`
    /// has not been deformed yet or the readback failed. Blocks on the GPU: never call it from
    /// the frame path.
    pub fn read_morphed_span(
        &self,
        device: &crate::RenderDevice,
        object_key: u64,
    ) -> Option<(u32, Vec<LabVertex>)> {
        let instance = self.morph.instances.get(&object_key)?;
        let gpu = instance.gpu.as_ref().filter(|_| instance.computed)?;
        let size = gpu.out.size();
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("morph readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("morph readback"),
            });
        encoder.copy_buffer_to_buffer(&gpu.out, 0, &staging, 0, size);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.tracked().poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let vertices = {
            let mapped = staging.slice(..).get_mapped_range().ok()?;
            bytemuck::pod_collect_to_vec::<u8, LabVertex>(&mapped)
        };
        staging.unmap();
        Some((instance.targets.moved_span().start, vertices))
    }

    /// The mesh key `mesh` draws with when its object is a morphed instance (its deformed copy),
    /// creating the instance's buffers on its first draw and queueing changed weights on the
    /// staging belt. `None` draws `mesh` as submitted.
    pub(super) fn morph_draw_key(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
        shadow_only: bool,
    ) -> Option<u64> {
        if self.morph.instances.is_empty() || shadow_only {
            return None;
        }
        // A counted draw (no CPU vertices, ADR-7721) deforms a GPU copy of its adopted upload:
        // its vertex count and buffer come from the adopted entry.
        let counted = Self::counted_draw(mesh).map(|(base, _)| base);
        let (vertex_count, counted_vb) = match counted {
            Some(base) => (
                self.asset_vertex_count(base),
                self.meshes
                    .get(&base)
                    .filter(|_| self.asset_mesh_keys.contains(&base))
                    .map(|entry| entry.vb.clone()),
            ),
            None => (mesh.vertices.len(), None),
        };
        let state = &mut self.morph;
        let instance = state.instances.get_mut(&mesh.object_key)?;
        let targets = Arc::clone(&instance.targets);
        if vertex_count != targets.vertex_count() as usize || (counted.is_some() && counted_vb.is_none()) {
            state.stats.mismatched += 1;
            return None;
        }
        if targets.moved_vertex_count() == 0 {
            return None;
        }
        let max_binding = d.limits().max_storage_buffer_binding_size;
        let span = targets.moved_span();
        let span_bytes = u64::from(span.end - span.start) * VERTEX_BYTES;
        if targets.gpu_bytes() > max_binding || span_bytes > max_binding {
            state.stats.mismatched += 1;
            return None;
        }
        // The object's first draw this frame: drop the copies the last frame did not draw (a
        // LOD or dither switch left them). No draw of this frame holds them yet; one still in
        // flight from an earlier pass window waits for its end (`lifecycle`, W2.2).
        if !instance.drawn {
            for old in instance.twins.iter().filter(|t| !t.used) {
                state.counted_twins.remove(&old.key);
                if !self.entries.defer_mesh_removal(old.key) {
                    self.meshes.remove(&old.key);
                    self.rigid_skin.remove(&old.key);
                }
            }
            instance.twins.retain(|t| t.used);
            for t in &mut instance.twins {
                t.used = false;
            }
        }
        // The deformed copy for this draw's mesh key: an object drawn under two keys in one
        // frame keeps one copy per key, each filled by the pre-pass (none is replaced while
        // another draw holds it, and none is re-created every frame).
        let twin = match instance.twins.iter_mut().find(|t| t.base == mesh.mesh_key) {
            Some(t) => {
                t.used = true;
                t.key
            }
            None => {
                state.next_twin = (state.next_twin + 1) & TWIN_ID_MASK;
                let key = MORPH_MESH_KEY_TAG | (state.next_twin << 3);
                instance.twins.push(Twin {
                    base: mesh.mesh_key,
                    key,
                    copied: None,
                    used: true,
                    counted: counted.is_some(),
                });
                if counted.is_some() {
                    state.counted_twins.insert(key, mesh.mesh_key);
                }
                key
            }
        };
        if !state.sets.contains_key(&targets.id()) {
            let buf = make_buffer(
                d,
                "morph targets",
                targets.gpu_bytes(),
                wgpu::BufferUsages::STORAGE,
            );
            q.write_buffer(&buf, 0, bytemuck::cast_slice(targets.words()));
            state.stats.buffers_created += 1;
            state.sets.insert(targets.id(), SetGpu { buf });
        }
        if instance.gpu.is_none() {
            let params_bytes = ((PARAM_WORDS + targets.len()) * 4) as u64;
            let params = make_buffer(d, "morph weights", params_bytes, wgpu::BufferUsages::STORAGE);
            let out = make_buffer(
                d,
                "morph span",
                span_bytes,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            );
            // Unmoved vertices of the span and every other attribute stay the base's: from the
            // CPU vertices, or a GPU copy of a counted draw's upload (ADR-7721).
            match counted_vb.as_ref() {
                Some(vb) => seed_into(
                    &mut state.seeds,
                    vb,
                    (u64::from(span.start) * VERTEX_BYTES, 0, span_bytes),
                    &out,
                ),
                None => {
                    let base = &mesh.vertices[span.start as usize..span.end as usize];
                    q.write_buffer(&out, 0, bytemuck::cast_slice(base));
                }
            }
            state.stats.buffers_created += 2;
            // The velocity pass's two poses (ADR-2652) for a rigid instance: whole meshes that
            // start as the base; the pre-pass copies each changed span in.
            let motion = mesh.joints.is_empty().then(|| {
                let bytes = (vertex_count as u64 * VERTEX_BYTES).max(VERTEX_BYTES);
                let full = [0, 1].map(|_| {
                    let buf = make_buffer(d, "morph motion", bytes, wgpu::BufferUsages::VERTEX);
                    match counted_vb.as_ref() {
                        Some(vb) => {
                            seed_into(
                                &mut state.seeds,
                                vb,
                                (0, 0, vertex_count as u64 * VERTEX_BYTES),
                                &buf,
                            );
                        }
                        None => q.write_buffer(&buf, 0, bytemuck::cast_slice(mesh.vertices)),
                    }
                    buf
                });
                state.stats.buffers_created += 2;
                MorphMotion {
                    full,
                    cur: 0,
                    prime: true,
                    live: false,
                }
            });
            instance.gpu = Some(InstanceGpu {
                params,
                out,
                bg: None,
                motion,
            });
            instance.uploaded = false;
            instance.computed = false;
        }
        if !instance.uploaded
            && let Some(gpu) = instance.gpu.as_ref()
        {
            let mut words = Vec::with_capacity(PARAM_WORDS + targets.len());
            words.extend([
                targets.moved_vertex_count(),
                targets.entry_base_word(),
                span.start,
                targets.len() as u32,
            ]);
            words.extend(instance.weights.iter().map(|w| w.to_bits()));
            self.belt.defer(d, &gpu.params, 0, bytemuck::cast_slice(&words));
            instance.uploaded = true;
        }
        // Motion vectors (ADR-2652): a rigid draw whose weights changed from a deformation it
        // already drew gets both poses.
        if !instance.computed && instance.had_span && mesh.joints.is_empty() {
            instance.motion_wanted = true;
        }
        instance.drawn = true;
        Some(twin)
    }

    /// Both poses of the morphed copy `mesh_key` when its weights changed this frame (ADR-2652):
    /// `(this frame's vertices, last frame's)`, whole meshes from vertex 0. `None` for steady
    /// weights, skinned morphs and every other mesh.
    pub(super) fn morph_motion(&self, mesh_key: u64) -> Option<(wgpu::Buffer, wgpu::Buffer)> {
        if mesh_key & MORPH_MESH_KEY_TAG != MORPH_MESH_KEY_TAG || self.morph.instances.is_empty() {
            return None;
        }
        self.morph
            .instances
            .values()
            .find(|i| i.twins.iter().any(|t| t.key == mesh_key))
            .and_then(|i| i.gpu.as_ref())
            .and_then(|g| g.motion.as_ref())
            .filter(|m| m.live)
            .map(|m| (m.full[m.cur].clone(), m.full[m.cur ^ 1].clone()))
    }
}

/// Records the morph pre-pass: deforms the drawn instances whose weights changed and copies
/// each fresh (or re-uploaded) span into its deformed copy's vertex range. Runs before any
/// pass reads vertices, after the staging belt's copies (weights, mesh uploads) landed.
pub(super) fn encode(
    state: &mut MorphState,
    pipes: &mut MorphPipes,
    meshes: &crate::key_hash::KeyMap<u64, MeshEntry>,
    d: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
) {
    let mut stats = MorphStats {
        buffers_created: state.stats.buffers_created,
        mismatched: std::mem::take(&mut state.stats.mismatched),
        ..MorphStats::default()
    };
    // Counted instances' buffers start as their upload (ADR-7721): before the dispatch writes
    // the moved rows and before the spans are copied into the deformed copies.
    for (src, src_offset, dst, dst_offset, bytes) in state.seeds.drain(..) {
        encoder.copy_buffer_to_buffer(&src, src_offset, &dst, dst_offset, bytes);
    }
    if state.instances.values().any(|i| i.drawn) {
        let handle = pipes.pipeline(d);
        let (bgl, layout_id) = pipes.bgl(d);
        let ready = handle.try_get();
        let MorphState { instances, sets, .. } = &mut *state;
        let mut dispatches: Vec<(wgpu::BindGroup, u32, u32)> = Vec::new();
        let mut copies: Vec<(wgpu::Buffer, wgpu::Buffer, u64)> = Vec::new();
        // Copies of the old span into a motion copy, recorded before the dispatch overwrites it.
        let mut pre_copies: Vec<(wgpu::Buffer, wgpu::Buffer, u64)> = Vec::new();
        for instance in instances.values_mut().filter(|i| i.drawn) {
            instance.drawn = false;
            let motion_wanted = std::mem::take(&mut instance.motion_wanted);
            stats.drawn += 1;
            if instance.twins.iter().any(|t| t.used && t.counted) {
                stats.counted += 1;
            }
            let (Some(gpu), Some(set)) = (instance.gpu.as_mut(), sets.get(&instance.targets.id())) else {
                continue;
            };
            let mut fresh = false;
            if !instance.computed {
                if ready.is_none() {
                    stats.waiting_pipeline += 1;
                } else {
                    if gpu.bg.as_ref().is_none_or(|(_, id)| *id != layout_id) {
                        let bg = crate::device::TrackedDevice::observe(d).create_bind_group(
                            &wgpu::BindGroupDescriptor {
                                label: Some("morph targets"),
                                layout: &bgl,
                                entries: &[
                                    binding(0, set.buf.as_entire_binding()),
                                    binding(1, gpu.params.as_entire_binding()),
                                    binding(2, gpu.out.as_entire_binding()),
                                ],
                            },
                        );
                        gpu.bg = Some((bg, layout_id));
                    }
                    if let Some((bg, _)) = gpu.bg.as_ref() {
                        let rows = instance.targets.moved_vertex_count();
                        let groups = rows.div_ceil(WORKGROUP).max(1);
                        let x = groups.min(MAX_GROUPS_X);
                        dispatches.push((bg.clone(), x, groups.div_ceil(x)));
                        stats.dispatched += 1;
                        stats.dispatched_vertices += u64::from(rows);
                        instance.computed = true;
                        fresh = true;
                        // Both poses for the velocity pass (ADR-2652): last frame's span stays
                        // in `full[cur ^ 1]`, this frame's goes into `full[cur]`.
                        let span_offset = u64::from(instance.targets.moved_span().start) * VERTEX_BYTES;
                        match gpu.motion.as_mut() {
                            Some(m) if motion_wanted => {
                                if m.prime {
                                    pre_copies.push((
                                        gpu.out.clone(),
                                        m.full[m.cur ^ 1].clone(),
                                        span_offset,
                                    ));
                                    m.prime = false;
                                } else {
                                    m.cur ^= 1;
                                }
                                copies.push((gpu.out.clone(), m.full[m.cur].clone(), span_offset));
                                m.live = true;
                                stats.motion_instances += 1;
                            }
                            Some(m) => {
                                // A change the copies missed (a skinned draw of the instance):
                                // the next one starts again from the span it replaces.
                                m.live = false;
                                m.prime = true;
                            }
                            None => {}
                        }
                        instance.had_span = true;
                    }
                }
            }
            if !fresh && let Some(m) = gpu.motion.as_mut() {
                // Steady weights: no motion, no copy.
                m.live = false;
            }
            if !instance.computed {
                continue;
            }
            // Every copy this frame draws gets the span (a fresh one, or one re-uploaded since).
            for twin in instance.twins.iter_mut().filter(|t| t.used) {
                let Some(entry) = meshes.get(&twin.key) else {
                    continue;
                };
                if fresh || twin.copied.as_ref().is_none_or(|c| !c.matches(entry)) {
                    let first = u64::try_from(entry.base_vertex).unwrap_or(0)
                        + u64::from(instance.targets.moved_span().start);
                    copies.push((gpu.out.clone(), entry.vb.clone(), first * VERTEX_BYTES));
                    twin.copied = Some(CopyTarget::of(entry));
                }
            }
        }
        for (src, dst, offset) in &pre_copies {
            encoder.copy_buffer_to_buffer(src, 0, dst, *offset, src.size());
        }
        if let (Some(pipeline), false) = (ready, dispatches.is_empty()) {
            let _span = tracing::info_span!("morph_targets", instances = dispatches.len()).entered();
            let _crumb = crate::breadcrumbs::frame_pass("morph_targets");
            let mut pass = encoder.segmented_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("morph targets"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            for (bg, x, y) in &dispatches {
                pass.set_bind_group(0, bg, &[]);
                pass.dispatch_workgroups(*x, *y, 1);
            }
        }
        for (src, dst, offset) in &copies {
            encoder.copy_buffer_to_buffer(src, 0, dst, *offset, src.size());
        }
    }
    state.stats = stats;
}
