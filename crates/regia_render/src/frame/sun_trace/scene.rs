// SPDX-License-Identifier: MPL-2.0
//! The traced sun's casters from outside the frame's mesh list (ADR-2310): a shared draw's proxy
//! (the terrain) and persistent instance sets (scatter). Each main-view frame that traces scene
//! geometry snapshots them here (Arc clones and matrices into kept vectors, no allocation once
//! warm); the scene build turns them into acceleration-structure meshes and instances, or into
//! triangles for the compute BVH.

use std::collections::HashMap;

use regia_core::glam::{Mat4, Vec3};

use crate::TrackedDevice;
use crate::hwrt::casters::{OPAQUE_INSTANCE_MASK, SHARED_CASTER_TAG};
use crate::hwrt::{AccelInstance, AccelMesh, GpuAccelMesh, TraceCasterInstance, TraceCasterMesh};

/// A shared index list and its buffer.
type IndexList = (std::sync::Arc<[u32]>, wgpu::Buffer);

/// One mesh's buffers for bottom-level builds.
struct GpuMesh {
    generation: u64,
    vertices: wgpu::Buffer,
    /// Key of the shared index list in [`ExtraCasters::index_buffers`].
    indices: (usize, usize),
}

/// The frame's casters from outside the mesh list.
#[derive(Default)]
pub(in crate::frame) struct ExtraCasters {
    /// What the shared draw was last told ([`crate::SharedPassDraw::set_trace_casters`]).
    pub(in crate::frame) requested: bool,
    /// This frame's meshes (keys tagged by source), unique.
    pub(in crate::frame) meshes: Vec<TraceCasterMesh>,
    /// This frame's placements, in the frame's space: the shared draw's first.
    pub(in crate::frame) instances: Vec<TraceCasterInstance>,
    /// Instances from the shared draw.
    pub(in crate::frame) shared: u32,
    /// Instances from persistent sets.
    pub(in crate::frame) sets: u32,
    /// Index of each mesh key in [`Self::meshes`].
    index: HashMap<u64, usize>,
    gpu: HashMap<u64, GpuMesh>,
    /// Index lists by `(Arc pointer, length)`: tiles of one grid share theirs. The entry holds the
    /// list, so its address cannot be reused by another list while the buffer is cached.
    index_buffers: HashMap<(usize, usize), IndexList>,
}

impl ExtraCasters {
    /// Replaces the snapshot: the shared draw's casters moved by `-origin_offset`, then every
    /// persistent set's casting instances.
    pub(in crate::frame) fn collect<'a>(
        &mut self,
        shared: Option<crate::hwrt::SharedTraceCasters<'_>>,
        origin_offset: [f32; 3],
        sets: impl Iterator<
            Item = (
                &'a TraceCasterMesh,
                &'a [[[f32; 4]; 4]],
                crate::ShadowCast,
                &'a [crate::ShadowCast],
            ),
        >,
    ) {
        self.clear_snapshot();
        if let Some(shared) = shared {
            let shift = Mat4::from_translation(-Vec3::from_array(origin_offset));
            let moved = origin_offset != [0.0; 3];
            for mesh in shared.meshes {
                let key = mesh.key ^ SHARED_CASTER_TAG;
                if !self.index.contains_key(&key) {
                    self.index.insert(key, self.meshes.len());
                    self.meshes.push(TraceCasterMesh { key, ..mesh.clone() });
                }
            }
            for instance in shared.instances {
                let transform = if moved {
                    (shift * Mat4::from_cols_array_2d(&instance.transform)).to_cols_array_2d()
                } else {
                    instance.transform
                };
                self.instances.push(TraceCasterInstance {
                    mesh: instance.mesh ^ SHARED_CASTER_TAG,
                    transform,
                });
            }
            self.shared = shared.instances.len() as u32;
        }
        for (mesh, models, shadow, shadows) in sets {
            if !self.index.contains_key(&mesh.key) {
                self.index.insert(mesh.key, self.meshes.len());
                self.meshes.push(mesh.clone());
            }
            for (i, model) in models.iter().enumerate() {
                let policy = shadows.get(i).copied().unwrap_or(shadow);
                if !policy.casts_sun() {
                    continue;
                }
                self.instances.push(TraceCasterInstance {
                    mesh: mesh.key,
                    transform: *model,
                });
                self.sets += 1;
            }
        }
    }

    /// Drops the snapshot (a frame that traces nothing keeps no references to the draw's data).
    pub(in crate::frame) fn clear_snapshot(&mut self) {
        self.index.clear();
        self.meshes.clear();
        self.instances.clear();
        self.shared = 0;
        self.sets = 0;
    }

    fn mesh(&self, key: u64) -> Option<&TraceCasterMesh> {
        self.index.get(&key).and_then(|&i| self.meshes.get(i))
    }

    fn generation_of(&self, key: u64) -> u64 {
        self.mesh(key).map_or(0, |m| m.generation)
    }

    /// Adds the snapshot's slots to a scene signature (after the frame's draws).
    pub(in crate::frame) fn sign(&self, builder: &mut crate::hwrt::casters::SignatureBuilder) {
        for instance in &self.instances {
            builder.slot(
                instance.mesh,
                self.generation_of(instance.mesh),
                2,
                &instance.transform,
            );
        }
    }

    /// Every valid mesh's triangles through its instances, for the compute BVH.
    pub(in crate::frame) fn bvh_triangles(&self, out: &mut Vec<[[f32; 3]; 3]>) -> u32 {
        let mut casters = 0;
        for instance in &self.instances {
            let Some(mesh) = self.mesh(instance.mesh).filter(|m| m.is_valid()) else {
                continue;
            };
            out.extend(mesh.world_triangles(&instance.transform));
            casters += 1;
        }
        casters
    }

    /// Uploads meshes that are new or changed into buffers a bottom-level build reads, and drops
    /// the buffers of meshes that left. Only on a device with ray queries (`BLAS_INPUT`).
    pub(in crate::frame) fn upload(&mut self, d: &TrackedDevice) {
        let _span = tracing::info_span!("sun_trace_extra_upload").entered();
        let live: std::collections::HashSet<u64> = self.meshes.iter().map(|m| m.key).collect();
        self.gpu.retain(|key, _| live.contains(key));
        let usage = wgpu::BufferUsages::BLAS_INPUT;
        // Valid meshes only: both lists are non-empty multiples of 4 bytes, the buffer's size.
        let create = |label: &str, bytes: &[u8]| {
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.len() as u64,
                usage,
                mapped_at_creation: true,
            });
            match buffer.slice(..).get_mapped_range_mut() {
                Ok(mut view) => view.copy_from_slice(bytes),
                Err(err) => {
                    tracing::warn!(target: "regia_render", %err, "sun trace: a caster upload was not mapped")
                }
            }
            buffer.unmap();
            buffer
        };
        for mesh in self.meshes.iter().filter(|m| m.is_valid()) {
            let index_key = (mesh.indices.as_ptr() as usize, mesh.indices.len());
            self.index_buffers.entry(index_key).or_insert_with(|| {
                (
                    std::sync::Arc::clone(&mesh.indices),
                    create("sun trace caster indices", bytemuck::cast_slice(&mesh.indices)),
                )
            });
            if self
                .gpu
                .get(&mesh.key)
                .is_some_and(|g| g.generation == mesh.generation)
            {
                continue;
            }
            let vertices = create("sun trace caster vertices", bytemuck::cast_slice(&mesh.positions));
            self.gpu.insert(
                mesh.key,
                GpuMesh {
                    generation: mesh.generation,
                    vertices,
                    indices: index_key,
                },
            );
        }
        let used: std::collections::HashSet<(usize, usize)> = self.gpu.values().map(|g| g.indices).collect();
        self.index_buffers.retain(|key, _| used.contains(key));
    }

    /// The uploaded meshes for the plan ([`AccelMesh`]) and their buffers ([`GpuAccelMesh`]).
    pub(in crate::frame) fn accel_meshes<'s>(
        &'s self,
        meshes: &mut Vec<AccelMesh>,
        buffers: &mut Vec<GpuAccelMesh<'s>>,
    ) {
        for mesh in &self.meshes {
            let (Some(gpu), true) = (self.gpu.get(&mesh.key), mesh.is_valid()) else {
                continue;
            };
            let Some((_, index_buffer)) = self.index_buffers.get(&gpu.indices) else {
                continue;
            };
            meshes.push(AccelMesh {
                key: mesh.key,
                generation: mesh.generation,
                vertex_count: mesh.positions.len() as u32,
                index_count: mesh.indices.len() as u32,
            });
            buffers.push(GpuAccelMesh {
                key: mesh.key,
                vertex_buffer: &gpu.vertices,
                first_vertex: 0,
                vertex_stride: std::mem::size_of::<[f32; 3]>() as u64,
                index_buffer: Some(index_buffer),
                first_index: 0,
                opaque: true,
            });
        }
    }

    /// The snapshot's instances whose mesh was uploaded, in snapshot order.
    pub(in crate::frame) fn accel_instances(&self, out: &mut Vec<AccelInstance>) {
        for instance in &self.instances {
            if self.gpu.contains_key(&instance.mesh) {
                out.push(AccelInstance {
                    mesh: instance.mesh,
                    transform: instance.transform,
                    custom_data: 0,
                    mask: OPAQUE_INSTANCE_MASK,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ShadowCast;
    use crate::hwrt::SharedTraceCasters;
    use crate::hwrt::casters::{SET_CASTER_TAG, SignatureBuilder};

    fn mesh(key: u64, generation: u64) -> TraceCasterMesh {
        TraceCasterMesh {
            key,
            generation,
            positions: Arc::from(vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]),
            indices: Arc::from(vec![0, 1, 2]),
        }
    }

    fn at(x: f32) -> [[f32; 4]; 4] {
        Mat4::from_translation(Vec3::new(x, 0.0, 0.0)).to_cols_array_2d()
    }

    #[test]
    fn the_snapshot_tags_moves_and_filters_its_casters() {
        let shared_meshes = [mesh(1, 1), mesh(1, 1)];
        let shared_instances = [TraceCasterInstance {
            mesh: 1,
            transform: at(10.0),
        }];
        let set_mesh = mesh(5 ^ SET_CASTER_TAG, 9);
        let models = [at(1.0), at(2.0), at(3.0)];
        let shadows = [ShadowCast::All, ShadowCast::Off, ShadowCast::All];
        let mut extra = ExtraCasters::default();
        extra.collect(
            Some(SharedTraceCasters {
                meshes: &shared_meshes,
                instances: &shared_instances,
            }),
            [4.0, 0.0, 0.0],
            [(&set_mesh, &models[..], ShadowCast::All, &shadows[..])].into_iter(),
        );
        assert_eq!(extra.shared, 1);
        assert_eq!(extra.sets, 2, "the instance with shadows off casts nothing");
        assert_eq!(extra.meshes.len(), 2, "a key listed twice is one mesh");
        assert_eq!(extra.instances[0].mesh, 1 ^ SHARED_CASTER_TAG);
        assert_eq!(extra.instances[0].transform[3][0], 6.0, "moved by -origin_offset");
        assert_eq!(extra.instances[1].transform[3][0], 1.0);
        assert_eq!(extra.instances[2].transform[3][0], 3.0);
        let mut triangles = Vec::new();
        assert_eq!(extra.bvh_triangles(&mut triangles), 3);
        assert_eq!(triangles.len(), 3);
        assert_eq!(triangles[0][0], [6.0, 0.0, 0.0]);

        // The signature follows the mesh's generation, and a fresh snapshot replaces the old one.
        let sign = |extra: &ExtraCasters| {
            let mut b = SignatureBuilder::new();
            extra.sign(&mut b);
            b.finish()
        };
        let before = sign(&extra);
        let regenerated = [mesh(1, 2)];
        extra.collect(
            Some(SharedTraceCasters {
                meshes: &regenerated,
                instances: &shared_instances,
            }),
            [4.0, 0.0, 0.0],
            std::iter::empty(),
        );
        assert_eq!(extra.sets, 0);
        assert_eq!(extra.instances.len(), 1);
        assert_ne!(sign(&extra).structure, before.structure);
        extra.clear_snapshot();
        assert!(extra.meshes.is_empty() && extra.instances.is_empty());
    }
}
