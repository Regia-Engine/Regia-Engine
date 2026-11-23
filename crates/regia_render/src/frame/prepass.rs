// SPDX-License-Identifier: MPL-2.0
//! The depth prepass of `FrameRenderer::render_into` (ADR-1552 split by pass): opaque and masked
//! draws (CPU and GPU-driven), material-graph depth variants and the shared draw's depth, into
//! the scene depth. Moved out of `frame.rs` unchanged. The frame still borrows its pipeline sets
//! here, so the pass takes the renderer fields it touches ([`PrepassFields`]) instead of `self`.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use super::shadow_passes::{GraphSets, InstancedPasses};
use super::*;

/// The renderer fields the prepass reads and writes.
pub(super) struct PrepassFields<'a> {
    pub(super) entries: &'a mut lifecycle::EntryLifecycle,
    pub(super) materials: &'a mut crate::key_hash::KeyMap<u64, MaterialEntry>,
    pub(super) meshes: &'a mut crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) objects: &'a mut crate::key_hash::KeyMap<ObjectUniformKey, ObjectEntry>,
    pub(super) shared_draw: &'a mut Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
}

/// What the prepass reads from the frame.
pub(super) struct PrepassInputs<'a> {
    pub(super) draws: &'a [Draw],
    pub(super) opaque_ix: &'a [usize],
    pub(super) graph_sets: &'a GraphSets,
    pub(super) instanced: &'a Option<InstancedPasses>,
    pub(super) depth_pipe: &'a wgpu::RenderPipeline,
    pub(super) depth_pipe_ds: &'a wgpu::RenderPipeline,
    pub(super) depth_view: &'a wgpu::TextureView,
    pub(super) msaa_depth: &'a Option<wgpu::TextureView>,
    pub(super) view_bg: &'a wgpu::BindGroup,
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    pub(super) timer: &'a Option<(FrameStamps, f32)>,
    pub(super) samples: u32,
    pub(super) trims: bool,
    pub(super) in_pass_stamps: bool,
    /// The prepass culls in two phases (`crate::prepass_occlusion`): phase 1's and phase 2's
    /// command buffers (one `DrawIndexedIndirect` per depth instance), and the first depth
    /// instance's slot in the instance object buffer. `None`: every instance draws.
    pub(super) two_phase: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    /// Slot of the first depth-prepass instance (with [`Self::two_phase`]).
    pub(super) depth_base: u32,
    /// The XR eye's hidden-area mask, drawn before everything else (ADR-1918).
    pub(super) eye_mask: Option<&'a stereo::EyeMaskDraw>,
}

/// The frame's counters and flags the prepass updates.
pub(super) struct PrepassCounters<'a> {
    pub(super) binds: &'a mut crate::submission::BindCounts,
    pub(super) draw_counts: &'a mut crate::frame_stats::FrameDrawCounts,
    pub(super) shared_depth_stamped: &'a mut bool,
}

/// Records the depth prepass.
pub(super) fn record_depth_prepass(
    fields: PrepassFields<'_>,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &PrepassInputs<'_>,
    counters: PrepassCounters<'_>,
) {
    let PrepassFields {
        entries,
        materials,
        meshes,
        objects,
        shared_draw,
    } = fields;
    let PrepassInputs {
        draws,
        opaque_ix,
        graph_sets,
        instanced,
        depth_pipe,
        depth_pipe_ds,
        depth_view,
        msaa_depth,
        view_bg,
        texture_table_group,
        timer,
        samples,
        trims,
        in_pass_stamps,
        two_phase,
        depth_base,
        eye_mask,
    } = *inputs;
    let PrepassCounters {
        binds,
        draw_counts,
        shared_depth_stamped,
    } = counters;
    let _span = tracing::info_span!("depth_prepass").entered();
    let prepass_depth = msaa_depth.as_ref().unwrap_or(depth_view);
    let _crumb = crate::breadcrumbs::frame_pass("depth_prepass");
    let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("depth prepass"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: prepass_depth,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    });
    if let Some(mask) = eye_mask.filter(|_| samples == 1) {
        mask.record(&mut pass);
    }
    // Draw records for a frame-debugger capture (ADR-8224); nothing while none is armed.
    let mut tally = crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref())
        .recording("depth_prepass", "depth_prepass", "depth");
    // Direct draws, not a render bundle. On the RTX 5080 a bundled replay of
    // these same draws landed in a multi-millisecond depth interval; the CPU
    // encode is already under a millisecond without the bundle.
    let mut mesh_binds = crate::submission::PassBinds::new(trims);
    if let Some((bg, _shadow_batches, depth_batches, _shadow_pipe, Some(depth_pipes), _mask_shadow_pipe)) =
        instanced.as_ref()
    {
        draw_instanced_depth(
            &mut pass,
            &mut tally,
            &mut mesh_binds,
            &InstancedDepth {
                meshes,
                materials,
                graph_sets,
                view_bg,
                bg,
                depth_batches,
                depth_pipes,
                trims,
                indirect: two_phase.map(|(phase1, _)| (phase1, depth_base)),
            },
        );
    } else {
        tally.set_pipeline(&mut pass, depth_pipe);
        tally.set_bind_group(&mut pass, 0, view_bg, &[]);
        let mut bound_graph: (Option<u64>, bool) = (None, false);
        for draw_index in opaque_ix {
            let draw = &draws[*draw_index];
            let graph = draw.graph.and_then(|key| graph_sets.get(&(key, draw.cull_off)));
            if (draw.graph, draw.cull_off) != bound_graph {
                match graph {
                    Some(graph) => tally.set_pipeline_named(&mut pass, &graph.depth, "material_graph_depth"),
                    None if draw.cull_off => {
                        tally.set_pipeline_named(&mut pass, depth_pipe_ds, "depth_double_sided")
                    }
                    None => tally.set_pipeline_named(&mut pass, depth_pipe, "depth"),
                }
                bound_graph = (draw.graph, draw.cull_off);
            }
            let Some(mesh) = meshes.get(&draw.mesh) else {
                missing_draw_entry(&entries.skips, "mesh", draw.mesh);
                continue;
            };
            let Some(object) = objects.get(&draw.uniform_key()) else {
                missing_draw_entry(&entries.skips, "object", draw.object);
                continue;
            };
            let Some(material) = materials.get(&draw.material) else {
                missing_draw_entry(&entries.skips, "material", draw.material);
                continue;
            };
            if graph.is_some_and(|graph| graph.uv1) {
                let Some(uv1) = mesh.uv1.as_ref() else {
                    continue;
                };
                mesh_binds.vertex(&mut pass, 1, uv1);
            }
            tally.set_bind_group(&mut pass, 1, &object.bg, &[]);
            tally.set_bind_group(&mut pass, 2, &material.bg, material.offsets());
            mesh_binds.mesh(&mut pass, &mesh.vb, &mesh.ib);
            tally.identify(|| super::forward_pass::draw_identity(draw));
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
        }
    }
    draw_counts.depth_prepass = tally.finish();
    binds.add(mesh_binds.counts());
    if samples == 1
        && let Some(draw) = shared_draw.as_ref()
    {
        let stamp = timer.as_ref().filter(|_| in_pass_stamps);
        if let Some((set, _)) = stamp {
            set.write_in_pass(&mut pass, SHARED_QUERY_BASE);
        }
        draw.draw(&mut pass, crate::SharedPass::Depth);
        if let Some((set, _)) = stamp {
            set.write_in_pass(&mut pass, SHARED_QUERY_BASE + 1);
            *shared_depth_stamped = true;
        }
    }
}

/// What [`draw_instanced_depth`] draws.
struct InstancedDepth<'a> {
    meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    materials: &'a crate::key_hash::KeyMap<u64, MaterialEntry>,
    graph_sets: &'a GraphSets,
    view_bg: &'a wgpu::BindGroup,
    bg: &'a wgpu::BindGroup,
    depth_batches: &'a [crate::gpu_driven::InstanceBatch],
    depth_pipes: &'a [[wgpu::RenderPipeline; 2]; 2],
    trims: bool,
    /// Draw each batch from these commands (one per instance from the first depth instance's
    /// slot) instead of directly.
    indirect: Option<(&'a wgpu::Buffer, u32)>,
}

/// The instanced depth batches (storage-object path): directly, or from a two-phase cull's
/// commands with one multi-draw per batch.
fn draw_instanced_depth(
    pass: &mut wgpu::RenderPass<'_>,
    tally: &mut crate::frame_stats::DrawTally,
    mesh_binds: &mut crate::submission::PassBinds,
    batch: &InstancedDepth<'_>,
) {
    let InstancedDepth {
        meshes,
        materials,
        graph_sets,
        view_bg,
        bg,
        depth_batches,
        depth_pipes,
        trims,
        indirect,
    } = *batch;
    tally.set_bind_group(pass, 0, view_bg, &[]);
    tally.set_bind_group(pass, 1, bg, &[]);
    // (graph key or 0, mask, cull-none) of the bound pipeline.
    let mut bound_pipe: Option<(u64, bool, bool)> = None;
    let mut bound_material: Option<u64> = None;
    let mut bound_mesh: Option<u64> = None;
    for group in depth_batches {
        let Some(mesh) = meshes.get(&group.mesh) else {
            continue;
        };
        let Some(material) = materials.get(&group.material) else {
            continue;
        };
        // Draw order is unchanged. Opaque batches omit `discard` so
        // early-Z applies; mask batches (and LOD cross-fade dither) keep it. A graph batch draws with its
        // own depth pipelines (its mask test and vertex stage are the graph's).
        let mask = (material.params.alpha[0] + 0.5) as u32 == 1 || mesh.dithered;
        let graph = material.graph_key.and_then(|key| {
            Some((
                key,
                graph_sets.get(&(key, group.double_sided))?.instanced.as_ref()?,
            ))
        });
        let want = (graph.map_or(0, |(key, _)| key), mask, group.double_sided);
        if bound_pipe != Some(want) {
            let pipe = match graph {
                Some((_, inst)) if mask => &inst.depth_mask,
                Some((_, inst)) => &inst.depth_opaque,
                None => &depth_pipes[usize::from(group.double_sided)][usize::from(mask)],
            };
            let name = match (graph.is_some(), mask) {
                (true, true) => "material_graph_depth_mask_instanced",
                (true, false) => "material_graph_depth_instanced",
                (false, true) => "depth_mask_instanced",
                (false, false) => "depth_instanced",
            };
            tally.set_pipeline_named(pass, pipe, name);
            bound_pipe = Some(want);
        }
        if bound_material != Some(group.material) {
            tally.set_bind_group(pass, 2, &material.bg, material.offsets());
            bound_material = Some(group.material);
        }
        if graph.is_some()
            && let Some(set) = material
                .graph_key
                .and_then(|key| graph_sets.get(&(key, group.double_sided)))
            && set.uv1
        {
            let Some(uv1) = mesh.uv1.as_ref() else {
                continue;
            };
            mesh_binds.vertex(pass, 1, uv1);
        }
        // Batches sort by mesh: the next material of the same mesh keeps its buffers,
        // and so does the next mesh in the same arena chunk (ADR-508).
        if trims || bound_mesh != Some(group.mesh) {
            mesh_binds.mesh(pass, &mesh.vb, &mesh.ib);
            bound_mesh = Some(group.mesh);
        }
        tally.identify(|| crate::frame_stats::DrawIdentity {
            mesh: Some(group.mesh),
            material: Some(group.material),
            objects: Vec::new(),
            instances: Some(group.count),
        });
        match indirect {
            Some((commands, depth_base)) => tally.multi_draw_indexed_indirect(
                pass,
                commands,
                u64::from(group.start.saturating_sub(depth_base)) * crate::prepass_occlusion::CMD_STRIDE,
                group.count,
            ),
            None => tally.draw_indexed(
                pass,
                mesh.indices(),
                mesh.base_vertex,
                group.start..group.start + group.count,
            ),
        }
    }
}

/// Phase 2 of a two-phase prepass (`crate::prepass_occlusion`): the instances phase 1 skipped
/// that this frame's pyramid does not hide, into the same depth (loaded). Adds its counts to
/// `draw_counts.depth_prepass`.
pub(super) fn record_depth_phase2(
    fields: PrepassFields<'_>,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &PrepassInputs<'_>,
    counters: PrepassCounters<'_>,
) {
    let _span = tracing::info_span!("depth_prepass_phase2").entered();
    let _crumb = crate::breadcrumbs::frame_pass("depth_prepass_phase2");
    let (Some((_, phase2)), Some((bg, _, depth_batches, _, Some(depth_pipes), _))) =
        (inputs.two_phase, inputs.instanced.as_ref())
    else {
        return;
    };
    let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("depth prepass phase 2"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: inputs.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    });
    let mut tally = crate::frame_stats::DrawTally::with_texture_table(inputs.texture_table_group.as_ref());
    let mut mesh_binds = crate::submission::PassBinds::new(inputs.trims);
    draw_instanced_depth(
        &mut pass,
        &mut tally,
        &mut mesh_binds,
        &InstancedDepth {
            meshes: fields.meshes,
            materials: fields.materials,
            graph_sets: inputs.graph_sets,
            view_bg: inputs.view_bg,
            bg,
            depth_batches,
            depth_pipes,
            trims: inputs.trims,
            indirect: Some((phase2, inputs.depth_base)),
        },
    );
    counters.draw_counts.depth_prepass = counters.draw_counts.depth_prepass.plus(tally.finish());
    counters.binds.add(mesh_binds.counts());
}
