// SPDX-License-Identifier: MPL-2.0
//! Audited persistent frame buffers (ADR-8065).

use super::FrameRenderer;
use crate::{GraphInput, GraphOutput, GraphResource, RenderGraph};

/// Allocation and access audit for a persistent frame buffer (ADR-8065).
///
/// Current/history reservoir rows name roles, not fixed parity slots. Their byte counts do
/// not overlap. Readers include accesses inside the writer node; those are sequenced by the
/// node's implementation, while graph inputs describe only reads across node boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameGraphBuffer {
    /// Resource name used by graph inputs and outputs.
    pub name: &'static str,
    /// Renderer subsystem retaining the allocation.
    pub owner: &'static str,
    /// `gpu` for a wgpu buffer, `cpu` for the reusable draw vector.
    pub memory: &'static str,
    /// Allocated bytes, including spare vector capacity; zero before allocation.
    pub bytes: u64,
    /// Allocation generation, unchanged by uploads, parity changes or history resets.
    pub generation: u64,
    /// Version zero is supplied before graph execution by an upload or previous frame.
    pub imported: bool,
    /// Previous-frame reservoir role; it is ignored when the sampler resets history.
    pub history: bool,
    /// Writer nodes, or the named external producer for imported contents.
    pub writers: Vec<String>,
    /// Nodes that may read the resource, including accesses internal to its writer.
    pub readers: Vec<String>,
}

struct BufferDecl {
    name: &'static str,
    owner: &'static str,
    imported: bool,
    history: bool,
    writers: &'static [&'static str],
    readers: &'static [&'static str],
}

const SHADE_READERS: &[&str] = &[
    "many_light_sampling",
    "visbuffer_resolve",
    "subsurface_draws",
    "forward",
    "oit_accum",
];

// The CPU uploads and preparation run before the graph's recording loop. GI records at
// the end of ScreenOcclusion (sun_shadow_trace), including its internal resolve/composite.
const BUFFERS: &[BufferDecl] = &[
    BufferDecl {
        name: "lights_buf",
        owner: "view",
        imported: true,
        history: false,
        writers: &["frame_upload"],
        readers: &[
            "cluster_lights",
            "many_light_sampling",
            "visbuffer_resolve",
            "subsurface_draws",
            "forward",
            "oit_accum",
        ],
    },
    BufferDecl {
        name: "counts_buf",
        owner: "renderer",
        imported: false,
        history: false,
        writers: &["cluster_lights"],
        readers: SHADE_READERS,
    },
    BufferDecl {
        name: "indices_buf",
        owner: "renderer",
        imported: false,
        history: false,
        writers: &["cluster_lights"],
        readers: SHADE_READERS,
    },
    BufferDecl {
        name: "cpu_draw_list",
        owner: "frame_scratch",
        imported: true,
        history: false,
        writers: &["frame_prepare", "gpu_cull"],
        readers: &[
            "shadow_occlusion",
            "shadows",
            "prepass_occlusion",
            "depth_prepass",
            "gpu_cull",
            "sun_shadow_trace",
            "visbuffer_resolve",
            "subsurface_draws",
            "forward",
            "oit_accum",
            "velocity",
            "translucent_hdr",
        ],
    },
    BufferDecl {
        name: "di_reservoir_current",
        owner: "many_light",
        imported: false,
        history: false,
        writers: &["many_light_sampling"],
        readers: &["many_light_sampling", "forward"],
    },
    BufferDecl {
        name: "di_reservoir_history",
        owner: "many_light",
        imported: true,
        history: true,
        writers: &["previous_frame:many_light_sampling"],
        readers: &["many_light_sampling"],
    },
    BufferDecl {
        name: "di_reservoir_temporal",
        owner: "many_light",
        imported: false,
        history: false,
        writers: &["many_light_sampling"],
        readers: &["many_light_sampling"],
    },
    BufferDecl {
        name: "gi_reservoir_current",
        owner: "restir_gi",
        imported: false,
        history: false,
        writers: &["sun_shadow_trace"],
        readers: &["sun_shadow_trace"],
    },
    BufferDecl {
        name: "gi_reservoir_history",
        owner: "restir_gi",
        imported: true,
        history: true,
        writers: &["previous_frame:sun_shadow_trace"],
        readers: &["sun_shadow_trace"],
    },
    BufferDecl {
        name: "gi_reservoir_temporal",
        owner: "restir_gi",
        imported: false,
        history: false,
        writers: &["sun_shadow_trace"],
        readers: &["sun_shadow_trace"],
    },
];

pub(super) fn is_buffer(name: &str) -> bool {
    BUFFERS.iter().any(|b| b.name == name)
}

pub(super) fn declare(graph: &mut RenderGraph) {
    for buffer in BUFFERS {
        graph.declare_buffer(buffer.name, buffer.owner, buffer.imported);
    }
}

pub(super) fn node_reads(node: &str) -> impl Iterator<Item = GraphInput> + '_ {
    BUFFERS
        .iter()
        .filter(move |b| b.readers.contains(&node) && (b.imported || !b.writers.contains(&node)))
        .map(|b| GraphInput {
            name: b.name.to_owned(),
        })
}

pub(super) fn node_writes(node: &str) -> impl Iterator<Item = GraphOutput> + '_ {
    BUFFERS
        .iter()
        .filter(move |b| b.writers.contains(&node))
        .map(|b| GraphOutput {
            name: b.name.to_owned(),
            desc: GraphResource::Imported,
        })
}

impl FrameRenderer {
    pub(super) fn graph_buffer_report(&self) -> Vec<FrameGraphBuffer> {
        let di = self.many_light.graph_buffers();
        let gi = self.restir_gi.graph_buffers();
        BUFFERS
            .iter()
            .map(|b| {
                let (bytes, generation) = match b.name {
                    "lights_buf" => (
                        self.lights_buf.as_ref().map_or(0, wgpu::Buffer::size),
                        u64::from(self.lights_buf.is_some()),
                    ),
                    "counts_buf" => (
                        self.counts_buf.as_ref().map_or(0, wgpu::Buffer::size),
                        u64::from(self.counts_buf.is_some()),
                    ),
                    "indices_buf" => (
                        self.indices_buf.as_ref().map_or(0, wgpu::Buffer::size),
                        self.cluster_indices_gen,
                    ),
                    "cpu_draw_list" => self.scratch.draw_buffer_allocation(),
                    "di_reservoir_current" => (di.0[0], di.1),
                    "di_reservoir_history" => (di.0[1], di.1),
                    "di_reservoir_temporal" => (di.0[2], di.1),
                    "gi_reservoir_current" => (gi.0[0], gi.1),
                    "gi_reservoir_history" => (gi.0[1], gi.1),
                    "gi_reservoir_temporal" => (gi.0[2], gi.1),
                    _ => unreachable!("audited buffer table"),
                };
                let (mut writers, readers) = self.extension_passes.graph().buffer_accesses(b.name, b.readers);
                // External initialization and intra-node stages have no separate graph node.
                for producer in b
                    .writers
                    .iter()
                    .filter(|name| name.starts_with("frame_") || name.starts_with("previous_frame:"))
                {
                    writers.insert(0, (*producer).to_owned());
                }
                FrameGraphBuffer {
                    name: b.name,
                    owner: b.owner,
                    memory: if b.name == "cpu_draw_list" { "cpu" } else { "gpu" },
                    bytes,
                    generation,
                    imported: b.imported,
                    history: b.history,
                    writers,
                    readers,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::FrameRenderer;

    fn scene() -> crate::LabScene {
        let (vertices, indices) = crate::box_mesh([1.0; 3]);
        crate::LabScene {
            width: 48,
            height: 32,
            eye: [0.0, 2.0, -6.0],
            target: [0.0; 3],
            fov_y: 1.0,
            near: 0.1,
            far: 30.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 4.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(crate::PostSettings {
                exposure_ev: 4.0,
                ..crate::PostSettings::golden_deterministic()
            }),
            directional: crate::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
            local_lights: vec![crate::LabLocalLight::Point(crate::LabPointLight {
                position: [0.0, 3.0, -3.0],
                color: [1.0; 3],
                intensity: 400.0,
                radius: 12.0,
                cast_shadows: false,
                source_radius: None,
            })],
            materials: vec![crate::LabMaterial::default()],
            meshes: vec![crate::LabMesh {
                vertices,
                indices,
                material: 0,
                ..crate::LabMesh::unskinned()
            }],
        }
    }

    #[test]
    fn graph_buffers_report_live_bytes_and_resize_generations() {
        let device = crate::RenderDevice::headless().expect("lavapipe device required");
        let mut renderer = FrameRenderer::new();
        let mut scene = scene();
        let mut quality = crate::FrameQuality {
            many_light_sampling: true,
            max_lights_per_cluster: 16,
            ..crate::FrameQuality::LAB
        };
        let draw = |renderer: &mut FrameRenderer, scene: &crate::LabScene, quality| {
            renderer
                .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
                .expect("buffer audit");
            renderer.graph_report().graph.buffers
        };
        let first = draw(&mut renderer, &scene, quality);
        assert!(
            renderer.many_light_report().active,
            "this test must exercise real reservoirs"
        );
        let second = draw(&mut renderer, &scene, quality);
        assert_eq!(first, second, "steady frames keep buffer allocations");
        for b in &first {
            assert!(!b.writers.is_empty() && !b.readers.is_empty(), "{}", b.name);
            if !b.name.starts_with("gi_") {
                assert!(b.bytes > 0 && b.generation > 0, "{}", b.name);
            }
        }
        let at = |list: &[super::FrameGraphBuffer], name: &str| {
            list.iter()
                .find(|b| b.name == name)
                .expect("buffer audit")
                .clone()
        };
        assert_eq!(
            at(&first, "lights_buf").bytes,
            renderer.lights_buf.as_ref().expect("buffer audit").size()
        );
        assert_eq!(
            at(&first, "counts_buf").bytes,
            u64::from(super::super::CLUSTER_COUNT) * 4
        );
        assert_eq!(
            at(&first, "indices_buf").bytes,
            crate::cluster_index_bytes(renderer.cluster_indices_cap)
        );
        for name in [
            "di_reservoir_current",
            "di_reservoir_history",
            "di_reservoir_temporal",
        ] {
            assert_eq!(at(&first, name).bytes, 48 * 32 * 16);
        }
        assert_eq!(at(&first, "cpu_draw_list").memory, "cpu");
        let old_groups: Vec<_> = renderer.view_bgs.values().cloned().collect();
        assert!(!old_groups.is_empty());
        scene.width = 64;
        scene.height = 40;
        let resized = draw(&mut renderer, &scene, quality);
        assert_eq!(at(&resized, "di_reservoir_current").bytes, 64 * 40 * 16);
        assert!(
            at(&resized, "di_reservoir_current").generation > at(&first, "di_reservoir_current").generation
        );
        assert!(
            renderer.view_bgs.values().all(|g| !old_groups.contains(g)),
            "old reservoir view groups retained"
        );
        assert_eq!(at(&resized, "indices_buf"), at(&first, "indices_buf"));
        let old_indices = renderer.indices_buf.as_ref().expect("buffer audit").clone();
        quality.max_lights_per_cluster = 256;
        let grown = draw(&mut renderer, &scene, quality);
        assert!(at(&grown, "indices_buf").generation > at(&resized, "indices_buf").generation);
        assert!(renderer.cluster_bg.keys().all(|key| key.3 != old_indices));
    }

    #[test]
    fn graph_buffers_swapped_main_view_drops_retired_cluster_groups() {
        let device = crate::RenderDevice::headless().expect("lavapipe device required");
        let mut renderer = FrameRenderer::new();
        let scene = scene();
        renderer
            .render_offscreen(&device, &scene.as_frame_scene())
            .expect("buffer audit");
        let old = renderer.indices_buf.as_ref().expect("buffer audit").clone();
        let old_groups: Vec<_> = renderer.view_bgs.values().cloned().collect();
        assert!(!old_groups.is_empty());
        let mut view = super::super::view_state::ViewState::new();
        renderer.swap_view_state(&mut view);
        renderer
            .ensure_frame_buffers(&device.device, 256)
            .expect("buffer audit");
        assert_ne!(renderer.indices_buf.as_ref().expect("buffer audit"), &old);
        renderer.swap_view_state(&mut view);
        assert!(renderer.view_bgs.values().all(|g| !old_groups.contains(g)));
        assert_eq!(renderer.cluster_indices_view_gen, renderer.cluster_indices_gen);
    }

    #[test]
    fn graph_buffers_have_explicit_node_dependencies() {
        let report = FrameRenderer::new().graph_report();
        let node = |name: &str| {
            report
                .graph
                .nodes
                .iter()
                .find(|n| n.name == name)
                .expect("buffer audit")
        };
        assert!(
            node("cluster_lights")
                .buffer_reads
                .iter()
                .any(|n| n == "lights_buf#0")
        );
        for name in ["counts_buf#1", "indices_buf#1"] {
            assert!(node("cluster_lights").buffer_writes.iter().any(|n| n == name));
            assert!(node("many_light_sampling").buffer_reads.iter().any(|n| n == name));
            assert!(node("forward").buffer_reads.iter().any(|n| n == name));
        }
        assert!(
            node("shadows")
                .buffer_reads
                .iter()
                .any(|n| n == "cpu_draw_list#0")
        );
        assert!(
            node("many_light_sampling")
                .buffer_reads
                .iter()
                .any(|n| n == "di_reservoir_history#0")
        );
        assert!(
            node("forward")
                .buffer_reads
                .iter()
                .any(|n| n == "di_reservoir_current#1")
        );
        assert!(
            node("sun_shadow_trace")
                .buffer_reads
                .iter()
                .any(|n| n == "gi_reservoir_history#0")
        );
        assert!(
            node("sun_shadow_trace")
                .buffer_writes
                .iter()
                .any(|n| n == "gi_reservoir_current#1")
        );
    }

    #[test]
    fn graph_buffers_cluster_growth_releases_old_cached_indices() {
        let device = crate::RenderDevice::headless().expect("lavapipe device required");
        let d = &device.device;
        let mut renderer = FrameRenderer::new();
        renderer.ensure_frame_buffers(d, 16).expect("buffer audit");
        let old = renderer.indices_buf.as_ref().expect("buffer audit").clone();
        let old_generation = renderer.cluster_indices_gen;
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("graph buffer audit"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        renderer.cluster_bg.get_or_insert_with(
            (
                renderer.frame_buf.as_ref().expect("buffer audit").clone(),
                renderer.lights_buf.as_ref().expect("buffer audit").clone(),
                renderer.counts_buf.as_ref().expect("buffer audit").clone(),
                old.clone(),
            ),
            || {
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("graph buffer audit"),
                    layout: &layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: old.as_entire_binding(),
                    }],
                })
            },
        );
        assert!(renderer.cluster_bg.keys().any(|key| key.3 == old));
        renderer.ensure_frame_buffers(d, 256).expect("buffer audit");
        assert!(renderer.cluster_indices_gen > old_generation);
        assert_ne!(renderer.indices_buf.as_ref().expect("buffer audit"), &old);
        assert!(
            renderer.cluster_bg.keys().all(|key| key.3 != old),
            "a retired index buffer is still held by the cluster cache"
        );
    }
}
