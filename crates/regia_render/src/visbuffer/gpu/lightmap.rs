// SPDX-License-Identifier: MPL-2.0
//! Lightmap receivers on the visibility-buffer path (ADR-2107, `frame/lightmap_paths.rs`): a
//! receiver's UV2 in the geometry buffer and the resolve's `LIGHTMAP` shade pipeline.
//!
//! The UV2 words are a range of their own, not part of the mesh's vertices: every other shader reads
//! the vertices as before, and a mesh that went resident before it became a receiver needs no
//! rebuild (a cooked mesh could not be rebuilt: its clusters are consumed by the upload).
//!
//! **Variant per material** (ADR-2689): the pass records which material slots hold a receiver
//! instance this frame ([`ReceiverSlots`]), and [`VisBufferPass::resolve_variants`] splits the
//! frame's materials so only those slots shade through the `LIGHTMAP` pipeline; every other
//! material keeps the plain shade pipeline, whatever the rest of the frame holds. A non-receiver
//! instance inside a receiver material reads a zero table row there and keeps its probe light, as
//! it did when the whole frame took the variant, so the pixels do not change, only the pipeline
//! the other materials pay for.

use super::{MATERIAL_DEPTH_FORMAT, RESOLVE_STORAGE_BUFFERS, VisBufferPass};
use crate::RenderError;
use crate::device::TrackedDevice;
use crate::visbuffer::arena::ClusterKey;

/// A receiver mesh's UV2 range.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Uv2Slot {
    /// Its range in the cluster arena.
    key: ClusterKey,
    /// The first word of the range (two words per source vertex).
    pub(crate) uv2_base: u32,
    /// The first word of the mesh's vertices.
    pub(crate) vertex_base: u32,
    /// Source vertices this chart covers: the mesh's own, not vertices a voxel-far DAG appends.
    pub(crate) uv2_vertices: u32,
    /// A hash of the stream the range was written from: a buffer rewritten in place (the same
    /// address and length, new floats) is a new stream and re-uploads (the render2 determinism
    /// audit; the old address-and-length key read it as resident and the geometry buffer kept
    /// the stale UVs).
    source: u64,
}

/// This frame's lightmap receivers by material slot (ADR-2689). The renderer's draw list makes a
/// receiver's UV2 resident ([`VisBufferPass::ensure_uv2`]) immediately before it pushes the
/// instance ([`VisBufferPass::push_instance`]), so the UV2 call arms the mesh and the push of that
/// mesh marks its material slot; any other push disarms it.
#[derive(Debug, Default)]
pub(crate) struct ReceiverSlots {
    /// The mesh whose UV2 was made resident since the last push.
    armed: Option<u64>,
    /// Per material slot: holds a receiver this frame.
    flags: Vec<bool>,
    /// The flagged slots, for the reset.
    slots: Vec<u32>,
    /// Receiver instances pushed this frame.
    instances: u32,
}

impl ReceiverSlots {
    /// A new frame: no receivers.
    pub(crate) fn begin_frame(&mut self) {
        for &slot in &self.slots {
            if let Some(flag) = self.flags.get_mut(slot as usize) {
                *flag = false;
            }
        }
        self.slots.clear();
        self.armed = None;
        self.instances = 0;
    }

    /// Arms `mesh_key`: its next push is a receiver.
    fn arm(&mut self, mesh_key: u64) {
        self.armed = Some(mesh_key);
    }

    /// Whether this push of `mesh_key` is a receiver's (disarms in any case).
    pub(crate) fn take_armed(&mut self, mesh_key: u64) -> bool {
        self.armed.take() == Some(mesh_key)
    }

    /// A receiver instance of material slot `slot` was pushed.
    pub(crate) fn note(&mut self, slot: u32) {
        self.instances += 1;
        let i = slot as usize;
        if self.flags.len() <= i {
            self.flags.resize(i + 1, false);
        }
        if !self.flags[i] {
            self.flags[i] = true;
            self.slots.push(slot);
        }
    }

    /// Material slot `slot` holds a receiver this frame.
    pub(crate) fn has(&self, slot: u32) -> bool {
        self.flags.get(slot as usize).copied().unwrap_or(false)
    }

    /// Receiver instances pushed this frame.
    pub(crate) fn instances(&self) -> u32 {
        self.instances
    }

    /// Material slots with a receiver this frame.
    pub(crate) fn materials(&self) -> u32 {
        self.slots.len() as u32
    }
}

/// This frame's materials by the resolve pipeline that shades them (ADR-2689): `(slot, key)` pairs
/// as [`VisBufferPass::frame_materials`] lists them, in the same order within each list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolveVariants {
    /// Shaded by the plain shade pipeline.
    pub(crate) plain: Vec<(u32, u64)>,
    /// Shaded by the `LIGHTMAP` pipeline (some instance of the material is a receiver).
    pub(crate) lightmap: Vec<(u32, u64)>,
    /// Receiver instances this frame.
    pub(crate) receiver_instances: u32,
}

/// Geometry-word address of source vertex `index`'s UV2, or `None` when `index` is past the
/// chart. `uv2_vertices` is the mesh's own vertex count. A voxel-far cluster indexes vertices
/// appended after that, and the resolve must not load `vis_geo` there (it would read past the
/// allocation, or the chart origin under a robust load, and replace the probe term).
/// The resolve applies this rule in `vis_resolve.wgsl`; this is the CPU check.
#[cfg(test)]
fn uv2_sample_word(uv2_base: u32, uv2_vertices: u32, index: u32) -> Option<u32> {
    if index >= uv2_vertices {
        return None;
    }
    uv2_base.checked_add(index.checked_mul(2)?)
}

/// FNV-1a (Fowler-Noll-Vo, public reference parameters) over the UV2 words: distinguishes a
/// buffer rewritten in place that an address-and-length key cannot. One multiply per word; a
/// collision only skips one re-upload.
fn uv2_fingerprint(uv2: &[[f32; 2]]) -> u64 {
    let words: &[u32] = bytemuck::cast_slice(uv2);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &word in words {
        hash = (hash ^ u64::from(word)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl VisBufferPass {
    /// This frame's materials split by resolve variant (ADR-2689). `lightmap_bound`: the frame has
    /// the `LIGHTMAP` pipeline and its table group; without them every material is plain.
    pub(crate) fn resolve_variants(&self, lightmap_bound: bool) -> ResolveVariants {
        let mut variants = ResolveVariants {
            receiver_instances: self.receivers.instances,
            ..ResolveVariants::default()
        };
        for (slot, key) in self.frame_materials() {
            if lightmap_bound && self.receivers.has(slot) {
                variants.lightmap.push((slot, key));
            } else {
                variants.plain.push((slot, key));
            }
        }
        variants
    }

    /// The device can bind the resolve's receiver table beside its other storage buffers.
    pub(crate) fn lightmap_supported(device: &wgpu::Device) -> bool {
        device.limits().max_storage_buffers_per_shader_stage > RESOLVE_STORAGE_BUFFERS
    }

    /// Makes `mesh_key`'s UV2 resident (one `[u, v]` per source vertex). The mesh must be resident
    /// ([`Self::ensure_mesh`]). False when the stream does not match it or the arena is full: the
    /// receiver then draws forward.
    pub(crate) fn ensure_uv2(&mut self, mesh_key: u64, uv2: &[[f32; 2]]) -> bool {
        let resident = self.make_uv2_resident(mesh_key, uv2);
        if resident {
            self.receivers.arm(mesh_key);
        }
        resident
    }

    /// [`Self::ensure_uv2`] without arming the receiver.
    fn make_uv2_resident(&mut self, mesh_key: u64, uv2: &[[f32; 2]]) -> bool {
        let Some(mesh) = self.meshes.get(&mesh_key).copied() else {
            return false;
        };
        // A residency-managed mesh's vertices are not contiguous (ADR-3118): its UV2 cannot be
        // found from a vertex address, so the receiver draws forward.
        if self.residency_manages(mesh_key) {
            return false;
        }
        if uv2.len() != mesh.vertex_count {
            return false;
        }
        let Ok(uv2_vertices) = u32::try_from(uv2.len()) else {
            return false;
        };
        let source = uv2_fingerprint(uv2);
        if let Some(slot) = self.uv2.get_mut(&mesh_key) {
            if slot.source != source {
                slot.source = source;
                let base = slot.uv2_base;
                self.arena.write_geo(base, bytemuck::cast_slice(uv2));
            }
            return true;
        }
        let Ok(words) = u32::try_from(uv2.len() * 2) else {
            return false;
        };
        let Some((key, ranges)) = self.arena.alloc(words, 0) else {
            return false;
        };
        self.arena.write_geo(ranges.geo.0, bytemuck::cast_slice(uv2));
        self.uv2.insert(
            mesh_key,
            Uv2Slot {
                key,
                uv2_base: ranges.geo.0,
                vertex_base: mesh.vertex_base,
                uv2_vertices,
                source,
            },
        );
        true
    }

    /// Where `mesh_key`'s UV2 is, when it has one resident.
    pub(crate) fn uv2_of(&self, mesh_key: u64) -> Option<Uv2Slot> {
        self.uv2.get(&mesh_key).copied()
    }

    /// Returns `mesh_key`'s UV2 range to the arena.
    pub(super) fn drop_uv2(&mut self, mesh_key: u64) {
        if let Some(slot) = self.uv2.remove(&mesh_key) {
            self.arena.free(slot.key);
        }
    }

    /// Instances before this frame's mesh list in the instance buffer (the persistent sets).
    pub(crate) fn persistent_count(&self) -> usize {
        self.persistent.len()
    }

    /// Group 1 of the resolve pipelines.
    pub(crate) fn resolve_layout_group(
        &mut self,
        device: &wgpu::Device,
    ) -> Result<wgpu::BindGroupLayout, RenderError> {
        self.ensure_pipes(&TrackedDevice::observe(device))?;
        self.pipes
            .as_ref()
            .map(|pipes| pipes.resolve_bgl.clone())
            .ok_or_else(|| RenderError::Shader("visibility pipelines missing".into()))
    }

    /// The resolve's shade pipeline over `module` (the plain one's state: an equal test against
    /// the material depth, no depth write).
    pub(crate) fn shade_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        module: &wgpu::ShaderModule,
        label: &str,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let device = &TrackedDevice::observe(device);
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_vis_fullscreen"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_vis_resolve"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: MATERIAL_DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Equal),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(device).as_deref(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::visbuffer::MeshletMesh;
    use crate::{LabVertex, ShadowCast};

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    /// A pass with meshes 1 and 2 resident (one triangle each).
    fn pass() -> (VisBufferPass, Vec<[f32; 2]>) {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let vertices: Vec<LabVertex> = positions
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
            .collect();
        let indices = vec![0, 1, 2];
        let meshlets = Arc::new(MeshletMesh::single_level(&positions, &indices));
        let mut pass = VisBufferPass::default();
        for mesh in [1, 2] {
            pass.insert_cooked(mesh, Arc::clone(&meshlets));
            assert!(pass.ensure_mesh(mesh, &vertices, &indices));
        }
        (pass, vec![[0.25, 0.5]; 3])
    }

    /// What the renderer's draw list does for one instance: a receiver makes its UV2 resident
    /// first (`frame/draw_list.rs`), then the instance is pushed.
    fn push(pass: &mut VisBufferPass, mesh: u64, material: u64, uv2: Option<&[[f32; 2]]>) {
        if let Some(uv2) = uv2 {
            assert!(pass.ensure_uv2(mesh, uv2));
        }
        let pushed = pass.push_instance(mesh, material, IDENTITY, IDENTITY, ShadowCast::All, false);
        assert!(pushed.is_some());
    }

    fn keys(list: &[(u32, u64)]) -> Vec<u64> {
        list.iter().map(|&(_, key)| key).collect()
    }

    /// Only the materials with a receiver instance take the `LIGHTMAP` variant: on the base every
    /// material of such a frame did (ADR-2689).
    #[test]
    fn only_receiver_materials_take_the_lightmap_variant() {
        let (mut pass, uv2) = pass();
        pass.begin_frame();
        // Material 10: one receiver and one plain instance. 11 and 12: plain only.
        push(&mut pass, 1, 10, Some(&uv2));
        push(&mut pass, 1, 10, None);
        push(&mut pass, 2, 11, None);
        push(&mut pass, 1, 12, None);
        let variants = pass.resolve_variants(true);
        assert_eq!(keys(&variants.lightmap), vec![10]);
        assert_eq!(keys(&variants.plain), vec![11, 12]);
        assert_eq!(variants.receiver_instances, 1);
        assert_eq!(pass.receivers.materials(), 1);
        // Every material once, in the frame's order.
        let all: Vec<u64> = pass.frame_materials().map(|(_, key)| key).collect();
        assert_eq!(all, vec![10, 11, 12]);

        // Without the bound variant (no lightmap pipeline this frame) every material is plain.
        let unbound = pass.resolve_variants(false);
        assert!(unbound.lightmap.is_empty());
        assert_eq!(keys(&unbound.plain), vec![10, 11, 12]);
    }

    /// A mesh whose UV2 stays resident from an earlier receiver is not a receiver by itself: only a
    /// push right after its UV2 call counts, and the next frame starts with none.
    #[test]
    fn receivers_are_per_push_and_per_frame() {
        let (mut pass, uv2) = pass();
        pass.begin_frame();
        push(&mut pass, 1, 10, Some(&uv2));
        let first = pass.resolve_variants(true);
        assert_eq!(keys(&first.lightmap), vec![10]);

        pass.begin_frame();
        // Mesh 1's UV2 is still resident, but this frame draws it as a plain instance.
        assert!(pass.uv2_of(1).is_some());
        push(&mut pass, 1, 10, None);
        let second = pass.resolve_variants(true);
        assert!(second.lightmap.is_empty(), "{second:?}");
        assert_eq!(second.receiver_instances, 0);

        // An armed mesh that is not the pushed one does not mark the push.
        pass.begin_frame();
        assert!(pass.ensure_uv2(1, &uv2));
        push(&mut pass, 2, 11, None);
        let third = pass.resolve_variants(true);
        assert!(third.lightmap.is_empty(), "{third:?}");
        // A failed push (an unknown mesh) disarms too.
        assert!(pass.ensure_uv2(1, &uv2));
        assert!(
            pass.push_instance(99, 12, IDENTITY, IDENTITY, ShadowCast::All, false)
                .is_none()
        );
        push(&mut pass, 1, 13, None);
        assert!(pass.resolve_variants(true).lightmap.is_empty());
    }

    /// The residency key is the stream's content: a buffer rewritten in place (the same
    /// address, the same length, new floats) must re-upload. The old address-and-length key
    /// read it as resident and the geometry buffer kept the stale UVs, so a frame whose
    /// allocator did not reuse the address shaded differently (the render2 determinism audit).
    #[test]
    fn rewritten_uv2_reuploads_under_the_same_key() {
        let (mut pass, mut uv2) = pass();
        pass.begin_frame();
        assert!(pass.ensure_uv2(1, &uv2));
        let base = pass.uv2_of(1).expect("resident").uv2_base;
        // Exact binary fractions: the readback compares the words bitwise.
        uv2.fill([0.75, 0.125]);
        assert!(pass.ensure_uv2(1, &uv2));
        let words = pass
            .arena
            .geo_words(base, uv2.len() * 2)
            .expect("the range is inside the stream");
        let expected: &[u32] = bytemuck::cast_slice(&uv2);
        assert_eq!(words, expected, "the geometry buffer holds the rewritten UVs");
        // An unchanged buffer stays resident without a rewrite: the words are untouched and
        // the slot still resolves.
        assert!(pass.ensure_uv2(1, &uv2));
        assert!(pass.uv2_of(1).is_some());
    }

    /// A lightmap receiver whose DAG appends a voxel-far cluster (ADR-2655) must not form a
    /// lightmap UV from a word at or past the chart. The chart covers the mesh's own vertices;
    /// an appended vertex's address is `uv2_base + 2 * vertex_index`, one past that range.
    #[test]
    fn render2_correctness_appended_vertex_does_not_form_a_lightmap_uv_past_the_chart() {
        let positions = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
        ];
        let base: Vec<LabVertex> = positions[..3]
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.25, 0.5]))
            .collect();
        let indices = vec![0, 1, 2];
        let mut mesh = MeshletMesh::single_level(&positions, &[0, 1, 2, 3, 4, 5]);
        mesh.source_index_count = indices.len() as u32;
        mesh.appended_vertices = crate::visbuffer::meshlet::AppendedVertices(
            positions[3..]
                .iter()
                .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
                .collect(),
        );
        assert_eq!(mesh.base_vertex_count(), base.len());
        let mut pass = VisBufferPass::default();
        pass.insert_cooked(1, Arc::new(mesh));
        assert!(pass.ensure_mesh(1, &base, &indices));
        let uv2 = [[0.25, 0.75], [0.5, 0.25], [0.75, 0.5]];
        assert!(pass.ensure_uv2(1, &uv2));

        let slot = pass.meshes.get(&1).copied().expect("resident");
        let chart = pass.uv2_of(1).expect("uv2");
        assert_eq!(slot.vertex_count, base.len());
        let chart_end = chart.uv2_base + slot.vertex_count as u32 * 2;
        let records = pass.arena.meshlets(slot.meshlet_base, slot.meshlet_count);
        let mut appended = None;
        let mut inside = None;
        for record in records {
            let words = pass
                .arena
                .geo_words(record.vertex_offset, record.vertex_count as usize)
                .expect("cluster vertex list");
            for &word in words {
                let index = (word - chart.vertex_base) / super::super::VERTEX_WORDS;
                let at = chart.uv2_base + index * 2;
                if index as usize >= slot.vertex_count {
                    appended = Some((index, at));
                } else {
                    inside = Some((index, at));
                }
            }
        }
        let (index, at) = appended.expect("a voxel cluster indexes an appended vertex");
        let (near, near_at) = inside.expect("a near cluster stays inside the chart");
        assert!(
            near_at + 1 < chart_end,
            "near UV2 word {near_at} ends at {chart_end}"
        );
        assert!(
            at >= chart_end,
            "fixture: appended vertex {index} should address past {chart_end}, got {at}"
        );
        assert_eq!(chart.uv2_vertices, slot.vertex_count as u32);
        assert_eq!(
            super::uv2_sample_word(chart.uv2_base, chart.uv2_vertices, near),
            Some(near_at),
            "a charted vertex still forms a UV"
        );
        assert!(
            super::uv2_sample_word(chart.uv2_base, chart.uv2_vertices, index).is_none(),
            "appended vertex {index} forms a lightmap UV at word {at}; the chart ends at {chart_end}"
        );
        // The resolve uses the same rule: it does not load `vis_geo` past `uv2_count`, and a
        // zero scale keeps the probe term.
        let shader = include_str!("../vis_resolve.wgsl");
        assert!(
            shader.contains(">= lm.uv2_count"),
            "the resolve must skip the chart when a corner is past uv2_count"
        );
        assert!(
            shader.contains("lightmap_so = vec4<f32>(0.0)"),
            "an out-of-chart triangle must not sample the chart origin"
        );
    }
}
