// SPDX-License-Identifier: MPL-2.0
//! Visibility-buffer clusters of counted meshes from their GPU upload (ADR-7462).
//!
//! A counted mesh (ADR-7284) has no CPU vertices once uploaded. [`VisBufferPass::ensure_counted_mesh`]
//! admits it from cooked meshlets and the upload's counts: the cluster arena takes the same
//! ranges as for the CPU path, the base vertices' words are a GPU-owned range
//! ([`super::super::arena::ClusterArena::own_on_gpu`]) and everything else (cluster vertex lists,
//! triangles, records) goes through the mirror. One repack per admitted mesh
//! (`vis_vertex_repack.wgsl`) then copies the upload's vertex buffer into that range in the CPU
//! path's 16-word order, in a submission of its own at `prepare`, after the arena's upload, so it
//! writes the buffer the frame binds. Steady frames dispatch nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::super::MeshletMesh;
use super::super::arena::ClusterKey;
use super::{Admit, VertexInput, VisBufferPass, buffer_entry, module, storage, whole};
use crate::LabVertex;
use crate::device::{TrackedDevice, TrackedQueue};

/// Words per uploaded vertex (one [`LabVertex`]).
const SOURCE_WORDS: u32 = (std::mem::size_of::<LabVertex>() / 4) as u32;
/// Invocations per workgroup (`vis_vertex_repack.wgsl`).
const WORKGROUP: u32 = 64;
/// Workgroups per dispatch row (the per-dimension limit every device has).
const MAX_ROW_GROUPS: u32 = 65_535;

/// The repack's uniform (`vis_vertex_repack.wgsl` `RepackParams`).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct RepackParams {
    /// Vertices to copy.
    pub(crate) count: u32,
    /// First geometry word of the destination range.
    pub(crate) dst_word: u32,
    /// Workgroups per dispatch row.
    pub(crate) row_groups: u32,
    /// Words per uploaded vertex.
    pub(crate) source_words: u32,
}

/// The dispatch size for `vertices`: (workgroups per row, rows).
pub(super) fn repack_groups(vertices: u32) -> (u32, u32) {
    let groups = vertices.div_ceil(WORKGROUP).max(1);
    let rows = groups.div_ceil(MAX_ROW_GROUPS);
    (groups.min(MAX_ROW_GROUPS), rows)
}

/// One pending repack.
struct RepackJob {
    mesh_key: u64,
    /// The mesh's arena key: a job whose mesh went (and whose range another mesh may hold now)
    /// does not run.
    key: ClusterKey,
    source: wgpu::Buffer,
    dst_word: u32,
    vertices: u32,
}

struct RepackPipe {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
}

/// The pass's counted-mesh state.
#[derive(Default)]
pub(crate) struct CountedVertices {
    jobs: Vec<RepackJob>,
    pipe: Option<RepackPipe>,
    /// Counted meshes whose clusters are resident from their upload.
    resident: HashSet<u64>,
    /// The cooked meshlets of every counted mesh admitted, kept while the key's meshlets are not
    /// removed or released: a re-upload under the same key admits them again (ADR-7462).
    pub(super) meshlets: HashMap<u64, Arc<MeshletMesh>>,
    /// Repacks dispatched since the pass was made.
    repacks: u64,
    /// Counted whole draws of this frame that drew forward for want of clusters.
    fallbacks: u32,
    /// The device's largest storage binding, once a visibility frame clamped the pass to it: a
    /// counted vertex buffer past it is not admitted (the repack could not bind it).
    pub(super) max_binding: Option<u64>,
}

impl CountedVertices {
    /// A mesh's clusters went: its pending repack goes too.
    pub(super) fn forget(&mut self, mesh_key: u64) {
        if self.resident.remove(&mesh_key) {
            self.jobs.retain(|job| job.mesh_key != mesh_key);
        }
    }

    /// A frame starts: the fallback count is this frame's.
    pub(super) fn begin_frame(&mut self) {
        self.fallbacks = 0;
    }
}

impl VisBufferPass {
    /// Makes counted mesh `mesh_key`'s clusters resident from its upload (ADR-7462): `source` is
    /// the adopted vertex buffer ([`LabVertex`] layout, `vertex_count` vertices) and `index_count`
    /// the upload's index count. Needs cooked meshlets under the key
    /// ([`crate::FrameRenderer::insert_meshlets`]) that match the counts, a `STORAGE` vertex
    /// buffer and cluster residency off; false keeps the mesh on the forward path (counted in
    /// [`Self::counted_stats`]).
    pub(crate) fn ensure_counted_mesh(
        &mut self,
        mesh_key: u64,
        source: &wgpu::Buffer,
        vertex_count: usize,
        index_count: usize,
    ) -> bool {
        // The repack binds the whole buffer: one past the device's storage-binding limit draws
        // forward from its first frame, not from a range the repack never fills.
        let fits = source.usage().contains(wgpu::BufferUsages::STORAGE)
            && source.size() >= vertex_count as u64 * u64::from(SOURCE_WORDS) * 4
            && self.counted.max_binding.is_none_or(|max| source.size() <= max);
        let admitted = if fits {
            self.admit_mesh(
                mesh_key,
                VertexInput::Gpu {
                    vertices: vertex_count,
                    indices: index_count,
                },
            )
        } else {
            Admit::Forward
        };
        match admitted {
            Admit::Forward => {
                self.counted.fallbacks += 1;
                false
            }
            Admit::Resident => true,
            Admit::Repack {
                key,
                dst_word,
                vertices,
            } => {
                self.counted.resident.insert(mesh_key);
                self.counted.jobs.push(RepackJob {
                    mesh_key,
                    key,
                    source: source.clone(),
                    dst_word,
                    vertices,
                });
                true
            }
        }
    }

    /// The cooked meshlets counted mesh `mesh_key` was admitted with (`None` when it was not, or
    /// its meshlets were removed or released since): what a re-upload under the same key (a hot
    /// reload) inserts again, checked against the new upload's counts at its next draw.
    pub(crate) fn counted_meshlets(&self, mesh_key: u64) -> Option<Arc<MeshletMesh>> {
        self.counted.meshlets.get(&mesh_key).cloned()
    }

    /// Takes the cooked meshlets a re-upload of counted mesh `mesh_key` under the same key (a hot
    /// reload) should be admitted with, before its old clusters are released: meshlets inserted
    /// since it was admitted and still pending (the meshlet bridge's, for a `.fmlt` reloaded in the
    /// same update) are fresher than the ones it was admitted with, so they win (ADR-7542). The
    /// pending entry is removed; the caller inserts the result again after the release.
    pub(crate) fn take_reload_meshlets(&mut self, mesh_key: u64) -> Option<Arc<MeshletMesh>> {
        self.cooked
            .remove(&mesh_key)
            .or_else(|| self.counted_meshlets(mesh_key))
    }

    /// One more counted whole draw of this frame that drew forward for want of clusters (no
    /// adopted entry to read the vertices from).
    pub(crate) fn note_counted_fallback(&mut self) {
        self.counted.fallbacks += 1;
    }

    /// Counted meshes on the cluster path (ADR-7462).
    pub(crate) fn counted_stats(&self) -> crate::visbuffer::CountedVisStats {
        let mut stats = crate::visbuffer::CountedVisStats {
            repacks: self.counted.repacks,
            forward_fallbacks: self.counted.fallbacks,
            kept_meshlet_bytes: self
                .counted
                .meshlets
                .values()
                .map(|mesh| meshlet_cpu_bytes(mesh))
                .sum(),
            ..Default::default()
        };
        for slot in self
            .counted
            .resident
            .iter()
            .filter_map(|key| self.meshes.get(key))
        {
            stats.meshes += 1;
            stats.clusters += u64::from(slot.meshlet_count);
        }
        stats
    }

    /// Runs the pending repacks into the geometry buffer (after the arena's upload of this
    /// frame), in a submission of their own. A repack the device cannot bind (a vertex buffer
    /// past its storage-binding limit) or that fails to build drops its mesh to the forward path.
    pub(super) fn run_repacks(&mut self, d: &TrackedDevice, queue: &TrackedQueue) {
        if self.counted.jobs.is_empty() {
            return;
        }
        let jobs = std::mem::take(&mut self.counted.jobs);
        let Some((geo, _)) = self.buffers.geo.as_ref() else {
            // No geometry buffer yet: try again at the next prepare.
            self.counted.jobs = jobs;
            return;
        };
        let geo = geo.clone();
        let _span = tracing::info_span!("visbuffer_counted_repack", meshes = jobs.len()).entered();
        if self.counted.pipe.is_none() {
            match repack_pipe(d) {
                Ok(pipe) => self.counted.pipe = Some(pipe),
                Err(err) => {
                    tracing::warn!(
                        target: "regia_render",
                        "the counted-mesh vertex repack did not build ({err}); counted meshes draw forward"
                    );
                    for job in &jobs {
                        self.drop_resident(job.mesh_key);
                        self.declined.insert(job.mesh_key);
                    }
                    return;
                }
            }
        }
        let Some(pipe) = self.counted.pipe.as_ref() else {
            return;
        };
        let max_binding = d.limits().max_storage_buffer_binding_size;
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vis counted repack"),
        });
        let mut refused = Vec::new();
        let mut ran = 0u64;
        for job in &jobs {
            if !self.arena.is_live(job.key) {
                continue;
            }
            if job.source.size() > max_binding {
                refused.push(job.mesh_key);
                continue;
            }
            let (row_groups, rows) = repack_groups(job.vertices);
            let params = RepackParams {
                count: job.vertices,
                dst_word: job.dst_word,
                row_groups,
                source_words: SOURCE_WORDS,
            };
            let uniform = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vis counted repack params"),
                size: std::mem::size_of::<RepackParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&uniform, 0, bytemuck::bytes_of(&params));
            let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis counted repack"),
                layout: &pipe.bgl,
                entries: &[whole(0, &job.source), whole(1, &geo), whole(2, &uniform)],
            });
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis counted repack"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipe.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(row_groups, rows, 1);
            ran += 1;
        }
        if ran > 0 {
            queue.submit([encoder.finish()]);
        }
        self.counted.repacks += ran;
        for mesh_key in refused {
            tracing::warn!(
                target: "regia_render",
                mesh_key,
                "a counted mesh's vertex buffer is past the storage-binding limit; it draws forward"
            );
            self.drop_resident(mesh_key);
            self.declined.insert(mesh_key);
        }
    }
}

/// CPU bytes of `mesh`'s lists: cluster records, cluster vertex lists, triangles and appended
/// vertices.
fn meshlet_cpu_bytes(mesh: &MeshletMesh) -> u64 {
    (std::mem::size_of_val(mesh.meshlets.as_slice())
        + std::mem::size_of_val(mesh.vertices.as_slice())
        + std::mem::size_of_val(mesh.triangles.as_slice())
        + std::mem::size_of_val(mesh.appended_vertices.0.as_slice())) as u64
}

fn repack_pipe(d: &TrackedDevice) -> Result<RepackPipe, crate::RenderError> {
    let compute = wgpu::ShaderStages::COMPUTE;
    let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vis counted repack"),
        entries: &[
            buffer_entry(0, compute, storage(true)),
            buffer_entry(1, compute, storage(false)),
            buffer_entry(2, compute, wgpu::BufferBindingType::Uniform),
        ],
    });
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vis counted repack"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let module = module(
        d,
        "vis_vertex_repack.wgsl",
        include_str!("../vis_vertex_repack.wgsl"),
    )?;
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("vis counted repack"),
        layout: Some(&layout),
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: crate::pipeline_cache::cache_for(d).as_deref(),
    });
    Ok(RepackPipe { pipeline, bgl })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::visbuffer::MeshletMesh;

    /// A UV sphere and its cooked clusters (the cooker links its own copy of this crate: cross by
    /// the format).
    fn sphere() -> (Vec<LabVertex>, Vec<u32>, Arc<MeshletMesh>) {
        let (vertices, indices) = crate::uv_sphere(1.0, 32, 16);
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let mesh = regia_asset_pipeline::build_meshlet_mesh(
            &positions,
            &indices,
            &regia_asset_pipeline::MeshletSettings::default(),
        )
        .expect("cook");
        let mesh = MeshletMesh::from_bytes(&mesh.to_bytes()).expect("round trip");
        (vertices, indices, Arc::new(mesh))
    }

    /// ADR-7462: a counted mesh admitted from its upload's counts takes the same arena ranges and
    /// cluster records as the same mesh from CPU vertices, and the same geometry words after its
    /// vertices; its vertex words are GPU-owned (the mirror holds zeros there and refuses writes)
    /// and one repack covers exactly them. Without cooked meshlets, with counts that do not match
    /// them, or with residency on, it stays forward and no builder runs.
    #[test]
    fn a_counted_mesh_takes_the_cpu_paths_ranges_and_one_repack_of_its_vertices() {
        let (vertices, indices, meshlets) = sphere();
        let (n, words) = (
            vertices.len(),
            vertices.len() * super::super::VERTEX_WORDS as usize,
        );
        let mut cpu = VisBufferPass::default();
        cpu.insert_cooked(1, Arc::clone(&meshlets));
        assert_eq!(
            cpu.admit_mesh(1, VertexInput::Cpu(&vertices, &indices)),
            Admit::Resident
        );
        let mut gpu = VisBufferPass::default();
        // A builder is installed but never runs for a counted mesh (it has no vertices to cook).
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&built);
        gpu.set_builder(Some(Arc::new(move |_: &[LabVertex], _: &[u32]| {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        })));
        let counts = VertexInput::Gpu {
            vertices: n,
            indices: indices.len(),
        };
        assert_eq!(
            gpu.admit_mesh(2, counts),
            Admit::Forward,
            "no cooked meshlets yet"
        );
        gpu.insert_cooked(2, Arc::clone(&meshlets));
        let wrong = VertexInput::Gpu {
            vertices: n + 1,
            indices: indices.len(),
        };
        let mut mismatched = VisBufferPass::default();
        mismatched.insert_cooked(3, Arc::clone(&meshlets));
        assert_eq!(
            mismatched.admit_mesh(3, wrong),
            Admit::Forward,
            "counts must match"
        );
        let Admit::Repack {
            key,
            dst_word,
            vertices: repacked,
        } = gpu.admit_mesh(2, counts)
        else {
            panic!("a counted mesh with cooked meshlets is admitted for a repack");
        };
        assert_eq!(
            built.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no builder ran"
        );
        assert_eq!(
            gpu.admit_mesh(2, counts),
            Admit::Resident,
            "resident from then on"
        );

        let (a, b) = (&cpu.meshes[&1], &gpu.meshes[&2]);
        assert_eq!(
            (
                a.meshlet_base,
                a.meshlet_count,
                a.vertex_base,
                a.vertex_count,
                a.index_count
            ),
            (
                b.meshlet_base,
                b.meshlet_count,
                b.vertex_base,
                b.vertex_count,
                b.index_count
            ),
            "the same ranges and counts"
        );
        assert_eq!(a.bounds, b.bounds);
        assert_eq!(cpu.arena.stats().geometry_words, gpu.arena.stats().geometry_words);
        assert_eq!(cpu.arena.stats().clusters, gpu.arena.stats().clusters);
        assert_eq!(
            cpu.arena.meshlets(a.meshlet_base, a.meshlet_count),
            gpu.arena.meshlets(b.meshlet_base, b.meshlet_count),
            "the same cluster records"
        );
        let total = gpu.arena.stats().geometry_words as usize;
        assert_eq!(
            cpu.arena.geo_words(a.vertex_base + words as u32, total - words),
            gpu.arena.geo_words(b.vertex_base + words as u32, total - words),
            "the same lists and triangles after the vertices"
        );
        assert_eq!(
            gpu.arena.geo_words(b.vertex_base, words),
            Some(&vec![0u32; words][..]),
            "the mirror never holds the vertices"
        );
        assert_ne!(
            cpu.arena.geo_words(a.vertex_base, words),
            gpu.arena.geo_words(b.vertex_base, words),
            "the CPU path wrote them"
        );
        assert_eq!((dst_word, repacked as usize), (b.vertex_base, n));
        assert!(gpu.arena.is_live(key));
        assert!(
            !gpu.arena.own_on_gpu(b.vertex_base, 1),
            "the vertex words are GPU-owned already"
        );

        // The mesh's clusters go: its range and GPU ownership go with them. The meshlets it was
        // admitted with stay for a re-upload under its key (a hot reload), until it is released.
        assert!(gpu.drop_resident(2));
        assert!(!gpu.arena.is_live(key));
        assert_eq!(gpu.arena.stats().geometry_words, 0);
        let kept = gpu.counted_meshlets(2).expect("kept for a re-upload");
        assert_eq!(
            gpu.counted_stats().kept_meshlet_bytes,
            meshlet_cpu_bytes(&meshlets),
            "the kept copy is reported"
        );
        assert!(gpu.counted_stats().kept_meshlet_bytes > 0);
        gpu.insert_cooked(2, kept);
        assert!(
            matches!(gpu.admit_mesh(2, counts), Admit::Repack { .. }),
            "a re-upload is admitted again, with a repack"
        );
        gpu.release_meshes(&[2]);
        assert!(gpu.counted_meshlets(2).is_none(), "released with the key");
        assert_eq!(
            gpu.counted_stats().kept_meshlet_bytes,
            0,
            "and no longer reported"
        );
        assert_eq!(gpu.admit_mesh(2, counts), Admit::Forward);

        // Residency on: a counted mesh draws forward (residency packs vertices on the CPU).
        let mut managed = VisBufferPass::default();
        managed.settings.residency = Some(crate::visbuffer::ClusterResidency {
            enabled: true,
            ..crate::visbuffer::ClusterResidency::default()
        });
        managed.insert_cooked(4, Arc::clone(&meshlets));
        assert_eq!(managed.admit_mesh(4, counts), Admit::Forward);
    }

    /// rev-claude-60 finding 4: a counted mesh whose vertex buffer is past the device's
    /// storage-binding limit is not admitted (it stays forward from its first frame). Before the fix
    /// it was admitted, drew one frame from a range the repack never filled (the mesh at the
    /// origin), and only the repack dropped it.
    #[test]
    fn a_vertex_buffer_past_the_storage_binding_limit_is_not_admitted() {
        let base = crate::test_device();
        let (vertices, indices, meshlets) = sphere();
        let bytes = (vertices.len() * std::mem::size_of::<LabVertex>()) as u64;
        let mut limits = base.device.limits();
        // Room for the arena's buffers (16 words a vertex, the lists and triangles), not for this
        // vertex buffer (24 words a vertex) as one binding.
        limits.max_storage_buffer_binding_size = bytes - 4;
        let Ok((device, _queue)) =
            regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("counted repack limited binding"),
                required_limits: limits,
                required_features: base.device.features(),
                ..Default::default()
            }))
        else {
            eprintln!("SKIP: the adapter refused a limited device");
            return;
        };
        let source = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("counted vertices"),
            size: bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let mut pass = VisBufferPass::default();
        pass.clamp_to_device(&device);
        pass.insert_cooked(1, Arc::clone(&meshlets));
        assert!(
            !pass.ensure_counted_mesh(1, &source, vertices.len(), indices.len()),
            "a buffer the repack cannot bind is not admitted"
        );
        assert!(!pass.meshes.contains_key(&1), "no clusters resident");
        assert_eq!(pass.counted_stats().forward_fallbacks, 1);
        // Within the limit the same mesh is admitted.
        let mut fits = VisBufferPass::default();
        fits.clamp_to_device(&base.device);
        fits.insert_cooked(1, meshlets);
        let source = base.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("counted vertices"),
            size: bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        assert!(fits.ensure_counted_mesh(1, &source, vertices.len(), indices.len()));
    }

    /// The repack's dispatch covers every vertex, in rows past the per-dimension group limit.
    #[test]
    fn the_repack_dispatch_covers_every_vertex() {
        assert_eq!(repack_groups(1), (1, 1));
        assert_eq!(repack_groups(64), (1, 1));
        assert_eq!(repack_groups(65), (2, 1));
        let many = MAX_ROW_GROUPS * WORKGROUP + 1;
        let (row, rows) = repack_groups(many);
        assert_eq!((row, rows), (MAX_ROW_GROUPS, 2));
        assert!(u64::from(row) * u64::from(rows) * u64::from(WORKGROUP) >= u64::from(many));
    }

    /// The repack's word order is the CPU path's `push_vertex`.
    #[test]
    fn the_repack_order_is_push_vertex() {
        let v = LabVertex {
            position: [1.0, 2.0, 3.0],
            normal: [4.0, 5.0, 6.0],
            uv: [7.0, 8.0],
            tangent: [9.0, 10.0, 11.0, 12.0],
            color: [13.0, 14.0, 15.0, 16.0],
            joints: [0; 4],
            weights: [0.0; 4],
        };
        let mut packed = Vec::new();
        super::super::push_vertex(&mut packed, &v);
        let source: &[u32] = bytemuck::cast_slice(std::slice::from_ref(&v));
        assert_eq!(source.len(), SOURCE_WORDS as usize);
        // `vis_vertex_repack.wgsl`'s mapping, destination word -> source word.
        let map = [0usize, 1, 2, 6, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let repacked: Vec<u32> = map.iter().map(|&s| source[s]).collect();
        assert_eq!(repacked, packed);
        let shader = include_str!("../vis_vertex_repack.wgsl");
        for (d, s) in map.iter().enumerate().take(8) {
            let line = if d == 0 {
                "geo[d] = source[s];".to_string()
            } else if *s == 0 {
                format!("geo[d + {d}u] = source[s];")
            } else {
                format!("geo[d + {d}u] = source[s + {s}u];")
            };
            assert!(shader.contains(&line), "the shader writes `{line}`");
        }
        assert!(shader.contains("for (var i = 8u; i < 16u; i = i + 1u)"));
    }
}
