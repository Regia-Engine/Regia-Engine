// SPDX-License-Identifier: MPL-2.0
//! The forward colour pass of `FrameRenderer::render_into` (ADR-1552 split by pass): opaque
//! (CPU and GPU-driven), the sky, then the sorted blended, transmissive, glass and x-ray draws,
//! the refraction copy and the glass front layer. Moved out of `frame.rs` unchanged. The frame
//! still borrows its pipeline sets here, so the pass takes the renderer fields it touches
//! ([`ForwardFields`]) instead of `self`.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use super::shadow_passes::GraphSets;
use super::*;

/// The GPU-driven colour pass: `[cull mode][culled, cull-none]` pipelines, the instanced object
/// bind group, the batch command buffer and the batches.
pub(super) type GpuColor = (
    [[wgpu::RenderPipeline; 2]; 2],
    wgpu::BindGroup,
    Option<wgpu::Buffer>,
    Vec<crate::gpu_driven::GpuBatch>,
);

/// What a frame-debugger draw record names of one frame draw (ADR-8224): its mesh, material and
/// object keys. Built only while a capture armed the draw log.
pub(super) fn draw_identity(draw: &Draw) -> crate::frame_stats::DrawIdentity {
    crate::frame_stats::DrawIdentity {
        mesh: Some(draw.mesh),
        material: Some(draw.material),
        objects: vec![draw.object],
        instances: None,
    }
}

/// The object keys of a GPU-driven batch (ADR-8224): the frame draws whose indirect command lies
/// in `start..start + count` (the culler's sorted order). Capture frames only.
fn batch_objects(draws: &[Draw], start: u32, count: u32) -> Vec<u64> {
    let end = start.saturating_add(count);
    draws
        .iter()
        .filter(|draw| {
            draw.indirect
                .is_some_and(|command| (start..end).contains(&command))
        })
        .map(|draw| draw.object)
        .collect()
}

/// The draw-record name of a blended draw's pipeline (ADR-8224), as the blend loop picks it.
fn blend_pipeline_name(graph: bool, xray: bool, alpha_mode: u32) -> &'static str {
    match (graph, alpha_mode) {
        (false, _) if xray => "xray",
        (true, ALPHA_PREMULTIPLIED) => "material_graph_premultiplied",
        (true, ALPHA_ADDITIVE) => "material_graph_additive",
        (true, _) => "material_graph_blend",
        (false, ALPHA_PREMULTIPLIED) => "premultiplied",
        (false, ALPHA_ADDITIVE) => "additive",
        (false, _) => "blend",
    }
}

/// The renderer fields the forward pass reads and writes.
pub(super) struct ForwardFields<'a> {
    pub(super) entries: &'a mut lifecycle::EntryLifecycle,
    pub(super) meshes: &'a mut crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) materials: &'a mut crate::key_hash::KeyMap<u64, MaterialEntry>,
    pub(super) objects: &'a mut crate::key_hash::KeyMap<ObjectUniformKey, ObjectEntry>,
    pub(super) mask_decals: &'a mut mask_decals_gpu::MaskDecalState,
    pub(super) glass: &'a mut Vec<crate::glass::GlassInstance>,
    pub(super) shared_draw: &'a mut Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
}

/// The forward pass's pipelines.
pub(super) struct ForwardPipes<'a> {
    pub(super) color_pipe: &'a wgpu::RenderPipeline,
    pub(super) color_pipe_ds: &'a wgpu::RenderPipeline,
    /// The lite path's alpha-tested pipelines `[culled, cull-none]` (ADR-1874): its `color_pipe`s
    /// have no `discard`.
    pub(super) lite_tested: Option<[&'a wgpu::RenderPipeline; 2]>,
    pub(super) sky_pipe: &'a wgpu::RenderPipeline,
    pub(super) add_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) add_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) blend_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) blend_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) premul_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) premul_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) xray_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) xray_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_add_depth_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_add_depth_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_add_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_add_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_front_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_front_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_mul_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_mul_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_refract_pipe: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    pub(super) transmit_refract_pipe_ds: &'a crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
}

/// What the forward pass reads from the frame.
pub(super) struct ForwardInputs<'a> {
    pub(super) scene: &'a FrameScene<'a>,
    pub(super) target: &'a FrameTarget<'a>,
    pub(super) draws: &'a [Draw],
    pub(super) opaque_ix: &'a [usize],
    pub(super) blend_ix: &'a [usize],
    pub(super) graph_sets: &'a GraphSets,
    pub(super) gpu_color: &'a Option<GpuColor>,
    pub(super) glass_pipes: &'a Option<(glass_gpu::GlassSet, wgpu::BindGroupLayout)>,
    pub(super) glass_pipes_ds: &'a Option<(glass_gpu::GlassSet, wgpu::BindGroupLayout)>,
    pub(super) glass_bgs: &'a [wgpu::BindGroup],
    pub(super) mask_sets: &'a [Option<mask_decals_gpu::MaskSet>; 2],
    /// The lightmap receivers' variants and group 3 (`lightmap_forward`, ADR-1623).
    pub(super) lightmap: &'a lightmap_forward::LightmapFrame,
    /// The thin film's variants (`thin_film_gpu`, ADR-1977).
    pub(super) film: &'a thin_film_gpu::FilmFrame,
    pub(super) single_indirect: &'a Option<wgpu::Buffer>,
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    pub(super) timer: &'a Option<(FrameStamps, f32)>,
    pub(super) view_bg: &'a wgpu::BindGroup,
    pub(super) depth_view: &'a wgpu::TextureView,
    pub(super) msaa_depth: &'a Option<wgpu::TextureView>,
    pub(super) msaa_color: &'a Option<wgpu::TextureView>,
    pub(super) hdr_view: &'a Option<wgpu::TextureView>,
    pub(super) front_views: &'a Option<(wgpu::TextureView, wgpu::TextureView)>,
    pub(super) refract: &'a Option<(wgpu::Texture, wgpu::Texture, wgpu::BindGroup)>,
    pub(super) viewport: [f32; 4],
    pub(super) strategy: crate::gpu_driven::IndirectStrategy,
    pub(super) attach_w: u32,
    pub(super) attach_h: u32,
    pub(super) internal_w: u32,
    pub(super) internal_h: u32,
    pub(super) samples: u32,
    pub(super) pre_exposure: f32,
    pub(super) airless: bool,
    pub(super) canopy_cheap: bool,
    pub(super) in_pass_stamps: bool,
    pub(super) linear_hdr: bool,
    pub(super) sky_pass: bool,
    /// Physical sky background, recorded after opaque depth and before every blended draw.
    pub(super) physical_sky: Option<&'a crate::outdoor::SkyPass>,
    pub(super) trims: bool,
    pub(super) vis_resolved: bool,
    /// The scattering draws ran first (`subsurface_gpu`): the colour and depth hold them.
    pub(super) subsurface_drawn: bool,
    /// The frame's translucent accumulation and composite (`oit_gpu`, ADR-2065).
    pub(super) oit: Option<&'a oit_gpu::OitPass>,
}

/// The frame state the forward pass updates.
pub(super) struct ForwardState<'a> {
    pub(super) binds: &'a mut crate::submission::BindCounts,
    pub(super) draw_counts: &'a mut crate::frame_stats::FrameDrawCounts,
    pub(super) glass_stamped: &'a mut bool,
    pub(super) shared_color_stamped: &'a mut bool,
}

/// The colour an uncovered pixel clears to. A linear HDR clear is radiance too: pre-exposed like the
/// draws.
pub(super) fn forward_clear(pre_exposure: f32) -> wgpu::Color {
    let k = f64::from(pre_exposure);
    wgpu::Color {
        r: 0.02 * k,
        g: 0.02 * k,
        b: 0.025 * k,
        a: 1.0,
    }
}

/// Records the forward colour pass.
pub(super) fn record_forward(
    fields: ForwardFields<'_>,
    encoder: &mut wgpu::CommandEncoder,
    pipes: &ForwardPipes<'_>,
    inputs: &ForwardInputs<'_>,
    state: ForwardState<'_>,
) -> Result<(), RenderError> {
    let ForwardFields {
        entries,
        meshes,
        materials,
        objects,
        mask_decals,
        glass,
        shared_draw,
    } = fields;
    let ForwardPipes {
        color_pipe,
        color_pipe_ds,
        lite_tested,
        sky_pipe,
        add_pipe,
        add_pipe_ds,
        blend_pipe,
        blend_pipe_ds,
        premul_pipe,
        premul_pipe_ds,
        xray_pipe,
        xray_pipe_ds,
        transmit_add_depth_pipe,
        transmit_add_depth_pipe_ds,
        transmit_add_pipe,
        transmit_add_pipe_ds,
        transmit_front_pipe,
        transmit_front_pipe_ds,
        transmit_mul_pipe,
        transmit_mul_pipe_ds,
        transmit_refract_pipe,
        transmit_refract_pipe_ds,
    } = *pipes;
    let ForwardInputs {
        scene,
        target,
        draws,
        opaque_ix,
        blend_ix,
        graph_sets,
        gpu_color,
        glass_pipes,
        glass_pipes_ds,
        glass_bgs,
        mask_sets,
        film,
        lightmap,
        single_indirect,
        texture_table_group,
        timer,
        view_bg,
        depth_view,
        msaa_depth,
        msaa_color,
        hdr_view,
        front_views,
        refract,
        viewport,
        strategy,
        attach_w,
        attach_h,
        internal_w,
        internal_h,
        samples,
        pre_exposure,
        airless,
        canopy_cheap,
        in_pass_stamps,
        linear_hdr,
        sky_pass,
        physical_sky,
        trims,
        vis_resolved,
        subsurface_drawn,
        oit,
    } = *inputs;
    let ForwardState {
        binds,
        draw_counts,
        glass_stamped,
        shared_color_stamped,
    } = state;
    let _span = tracing::info_span!("forward_pass").entered();
    let color_view = if linear_hdr {
        hdr_view.as_ref().expect("hdr view")
    } else {
        target.view
    };
    let (pass_color, resolve) = match msaa_color {
        Some(msaa) => (msaa, Some(color_view)),
        None => (color_view, None),
    };
    let forward_depth = msaa_depth.as_ref().unwrap_or(depth_view);
    let _crumb = crate::breadcrumbs::frame_pass("opaque");
    let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("forward"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: pass_color,
            depth_slice: None,
            resolve_target: resolve,
            ops: wgpu::Operations {
                // The visibility resolve already cleared and shaded this target.
                load: if target.load_color || vis_resolved || subsurface_drawn {
                    wgpu::LoadOp::Load
                } else {
                    wgpu::LoadOp::Clear(forward_clear(pre_exposure))
                },
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: forward_depth,
            depth_ops: Some(wgpu::Operations {
                load: if scene.depth_prepass || subsurface_drawn {
                    wgpu::LoadOp::Load
                } else {
                    wgpu::LoadOp::Clear(1.0)
                },
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    });
    // With post or TAAU the colour attachment is the view-sized HDR
    // target, not the window. Drawing at the window origin (the right
    // half of a split) puts the scissor outside that attachment.
    // The window rectangle is only for the final blit onto `target.view`.
    let draw_vp = if linear_hdr {
        [0.0, 0.0, internal_w as f32, internal_h as f32]
    } else {
        [
            viewport[0],
            viewport[1],
            viewport[2].max(1.0),
            viewport[3].max(1.0),
        ]
    };
    let sc_x = (draw_vp[0] as u32).min(attach_w.saturating_sub(1));
    let sc_y = (draw_vp[1] as u32).min(attach_h.saturating_sub(1));
    let sc_w = (draw_vp[2] as u32).clamp(1, attach_w - sc_x);
    let sc_h = (draw_vp[3] as u32).clamp(1, attach_h - sc_y);
    // The color target the draw records name (ADR-8224).
    let target_name = match (msaa_color.is_some(), linear_hdr) {
        (true, _) => "msaa_color",
        (false, true) => "hdr_color",
        (false, false) => "output",
    };
    let mut tally = crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref())
        .recording("forward", "opaque", target_name);
    tally.set_bind_group(&mut pass, 0, view_bg, &[]);
    pass.set_viewport(draw_vp[0], draw_vp[1], draw_vp[2], draw_vp[3], 0.0, 1.0);
    pass.set_scissor_rect(sc_x, sc_y, sc_w, sc_h);
    let mut mesh_binds = crate::submission::PassBinds::new(trims);
    if let Some((color_pipes, obj_bg, batch_cmds, batches)) = gpu_color {
        tally.set_bind_group(&mut pass, 1, obj_bg, &[]);
        // (graph key or 0, mask, cull-none, lightmap) of the bound pipeline.
        let mut bound_pipe: Option<(u64, bool, bool, bool)> = None;
        let mut bound_material: Option<u64> = None;
        let mut bound_mesh: Option<u64> = None;
        for batch in batches.iter().filter(|b| b.count > 0) {
            let Some(mesh) = meshes.get(&batch.mesh) else {
                continue;
            };
            let Some(material) = materials.get(&batch.material) else {
                continue;
            };
            // Same test as `fs_main`: `u32(material.alpha.x + 0.5) == 1` is mask.
            let mask = (material.params.alpha[0] + 0.5) as u32 == 1 || mesh.dithered;
            // A graph batch draws with its instanced pipelines; a graph whose set is
            // still building draws with the standard surface.
            let graph = material
                .graph_key
                .and_then(|key| Some((key, graph_sets.get(&(key, batch.double_sided))?)));
            let instanced = graph.and_then(|(key, set)| Some((key, set.instanced.as_ref()?, set.uv1)));
            // A batch with a lightmap receiver (`lightmap_paths`, ADR-2107): its variant, group 3
            // and the UV2 stream.
            let lit = lightmap
                .paths
                .instanced_batch(batch.batch_id, batch.double_sided, mask);
            let want = (
                instanced.map_or(0, |(key, ..)| key),
                mask,
                batch.double_sided,
                lit.is_some(),
            );
            if bound_pipe != Some(want) {
                let (pipe, name) = match (lit, instanced) {
                    (Some((pipe, _)), _) => (pipe, "lightmap_instanced"),
                    (None, Some((_, inst, _))) if mask => (&inst.color_mask, "material_graph_mask_instanced"),
                    (None, Some((_, inst, _))) => (&inst.color_opaque, "material_graph_instanced"),
                    (None, None) if mask => (
                        &color_pipes[usize::from(batch.double_sided)][1],
                        "standard_mask_instanced",
                    ),
                    (None, None) => (
                        &color_pipes[usize::from(batch.double_sided)][0],
                        "standard_instanced",
                    ),
                };
                tally.set_pipeline_named(&mut pass, pipe, name);
                bound_pipe = Some(want);
            }
            if bound_material != Some(batch.material) {
                tally.set_bind_group(&mut pass, 2, &material.bg, material.offsets());
                bound_material = Some(batch.material);
            }
            if let Some((_, table)) = lit {
                tally.set_bind_group(&mut pass, 3, table, &[]);
            }
            if lit.is_some() || instanced.is_some_and(|(.., uv1)| uv1) {
                let Some(uv1) = mesh.uv1.as_ref() else {
                    continue;
                };
                mesh_binds.vertex(&mut pass, 1, uv1);
            }
            if trims || bound_mesh != Some(batch.mesh) {
                mesh_binds.mesh(&mut pass, &mesh.vb, &mesh.ib);
                bound_mesh = Some(batch.mesh);
            }
            if let Some(cmds) = batch_cmds.as_ref() {
                tally.identify(|| crate::frame_stats::DrawIdentity {
                    mesh: Some(batch.mesh),
                    material: Some(batch.material),
                    objects: batch_objects(draws, batch.start, batch.count),
                    instances: Some(batch.count),
                });
                tally.draw_indexed_indirect(&mut pass, cmds, u64::from(batch.batch_id) * 20);
            }
        }
    }
    tally.set_pipeline_named(&mut pass, color_pipe, "standard");
    let mut bound_graph: (Option<u64>, bool) = (None, false);
    // A masked instance's or a lightmap receiver's pipeline is bound (`mask_decals_gpu`,
    // `lightmap_forward`): the next draw rebinds.
    let mut mask_bound = false;
    let mut bound_tested = false;
    for draw_index in opaque_ix {
        let draw = &draws[*draw_index];
        if gpu_color.is_some() && draw.indirect.is_some() {
            continue;
        }
        // Drawn before this pass (`subsurface_gpu`).
        if draw.subsurface {
            continue;
        }
        let lite_test = lite_tested
            .filter(|_| draw.alpha_mode != crate::ALPHA_OPAQUE || draw.lod_dither != super::LOD_DITHER_NONE);
        let graph = draw.graph.and_then(|key| graph_sets.get(&(key, draw.cull_off)));
        // A lightmap receiver: the `LIGHTMAP` pipeline, group 3 at its entry's block and the UV2
        // stream in slot 1 (below).
        let lightmapped = draw
            .lightmap
            .and_then(|entry| lightmap.bind(draw.cull_off, lite_test.is_some(), entry));
        if let Some((pipe, bg, offset)) = lightmapped {
            tally.set_pipeline_named(&mut pass, pipe, "lightmap");
            tally.set_bind_group(&mut pass, 3, bg, &[offset]);
            mask_bound = true;
        } else if draw.mask
            && let Some(set) = mask_sets[usize::from(draw.cull_off)].as_ref()
            && let Some(bg) = mask_decals.bind_group(draw.object)
            && let Some(pipe) = set.opaque.try_get()
        {
            tally.set_pipeline_named(&mut pass, pipe, "mask_decals");
            tally.set_bind_group(&mut pass, 3, bg, &[]);
            mask_bound = true;
        } else if draw.film
            // The Mobile lite tiers (ADR-1874) shade an opaque film as its base Fresnel.
            && lite_tested.is_none()
            && let Some(pipe) = film.pipeline(draw.cull_off, draw.alpha_mode)
            && let Some(binding) = materials.get(&draw.material).and_then(|m| m.film.as_ref())
        {
            tally.set_pipeline_named(&mut pass, pipe, "thin_film");
            tally.set_bind_group(&mut pass, 3, binding.bind_group(), &[]);
            mask_bound = true;
        } else if (draw.graph, draw.cull_off) != bound_graph
            || mask_bound
            || lite_test.is_some() != bound_tested
        {
            match graph {
                Some(graph) => tally.set_pipeline_named(&mut pass, &graph.color, "material_graph"),
                None if lite_test.is_some() => {
                    if let Some(pipes) = lite_test {
                        tally.set_pipeline_named(&mut pass, pipes[usize::from(draw.cull_off)], "lite_tested");
                    }
                }
                None if draw.cull_off => {
                    tally.set_pipeline_named(&mut pass, color_pipe_ds, "standard_double_sided")
                }
                None => tally.set_pipeline_named(&mut pass, color_pipe, "standard"),
            }
            bound_graph = (draw.graph, draw.cull_off);
            bound_tested = lite_test.is_some();
            mask_bound = false;
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
        if lightmapped.is_some() || graph.is_some_and(|graph| graph.uv1) {
            let Some(uv1) = mesh.uv1.as_ref() else {
                continue;
            };
            mesh_binds.vertex(&mut pass, 1, uv1);
        }
        tally.set_bind_group(&mut pass, 1, &object.bg, &[]);
        tally.set_bind_group(&mut pass, 2, &material.bg, material.offsets());
        mesh_binds.mesh(&mut pass, &mesh.vb, &mesh.ib);
        tally.identify(|| draw_identity(draw));
        if strategy == crate::gpu_driven::IndirectStrategy::SingleIndirect
            && draw.rigid
            && let Some(index) = draw.indirect
            && let Some(buf) = single_indirect.as_ref()
        {
            tally.draw_indexed_indirect(&mut pass, buf, u64::from(index) * 20);
        } else {
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
        }
    }
    draw_counts.opaque = tally.finish();
    if samples == 1
        && let Some(draw) = shared_draw.as_ref()
    {
        let stamp = timer.as_ref().filter(|_| in_pass_stamps);
        if let Some((set, _)) = stamp {
            set.write_in_pass(&mut pass, SHARED_QUERY_BASE + 2);
        }
        draw.draw(&mut pass, crate::SharedPass::Color);
        mesh_binds.forget();
        if let Some((set, _)) = stamp {
            set.write_in_pass(&mut pass, SHARED_QUERY_BASE + 3);
            *shared_color_stamped = true;
        }
        // The shared draw binds its own group 0. The sky and the
        // translucent loop below use the frame's view group, and the
        // sky pass is optional, so put the view group back here.
        pass.set_bind_group(0, view_bg, &[]);
    }
    // Sky behind the opaque geometry. A view composited over an earlier one
    // (`load_color`) keeps what is already there; no IBL means no sky to show.
    if scene.ibl && !target.load_color && !sky_pass && !airless {
        pass.set_pipeline(sky_pipe);
        pass.set_bind_group(0, view_bg, &[]);
        pass.draw(0..3, 0..1);
    }
    if let Some(sky) = physical_sky {
        // The sky samples opaque depth, so release its attachment first. Its old late
        // replacement erased additive/alpha-blended meshes wherever depth stayed clear.
        drop(pass);
        sky.draw_background(encoder, pass_color);
        mesh_binds.forget();
        pass = oit_gpu::begin_blended_run(encoder, pass_color, forward_depth);
        pass.set_bind_group(0, view_bg, &[]);
        pass.set_viewport(draw_vp[0], draw_vp[1], draw_vp[2], draw_vp[3], 0.0, 1.0);
        pass.set_scissor_rect(sc_x, sc_y, sc_w, sc_h);
    }
    // Translucent draws accumulate and composite here, over the opaque image and under the rest of
    // the blended run: glass draws over them, and the refraction copy below sees them (ADR-2065).
    if let Some(oit) = oit {
        drop(pass);
        let counts = oit_gpu::record_oit(
            encoder,
            oit,
            &oit_gpu::OitDraws {
                entries,
                meshes,
                materials,
                objects,
                draws,
                graph_sets,
                view_bg,
                depth: forward_depth,
                color: pass_color,
                texture_table_group,
                viewport: draw_vp,
                scissor: [sc_x, sc_y, sc_w, sc_h],
                trims,
            },
            binds,
        );
        draw_counts.translucent = draw_counts.translucent.plus(counts);
        // A new pass holds no buffers.
        mesh_binds.forget();
        pass = oit_gpu::begin_blended_run(encoder, pass_color, forward_depth);
        pass.set_bind_group(0, view_bg, &[]);
        pass.set_viewport(draw_vp[0], draw_vp[1], draw_vp[2], draw_vp[3], 0.0, 1.0);
        pass.set_scissor_rect(sc_x, sc_y, sc_w, sc_h);
    }
    if let Some((source, copy, refract_bg)) = refract.as_ref() {
        let _span = tracing::info_span!("refraction_copy").entered();
        let _crumb = crate::breadcrumbs::frame_pass("refraction_copy");
        drop(pass);
        // `refraction_copy` / `glass_front_layer` (ADR-7986): stamps between the passes, so the
        // split works without in-pass timestamps too.
        let split_stamp = |encoder: &mut wgpu::CommandEncoder, k: u32| {
            if let Some((set, _)) = timer.as_ref() {
                set.write(encoder, super::timing::GLASS_SPLIT_QUERY_BASE + k);
            }
        };
        split_stamp(encoder, 0);
        encoder.copy_texture_to_texture(source.as_image_copy(), copy.as_image_copy(), source.size());
        split_stamp(encoder, 1);
        // Glass front layer: the nearest transmissive layer's depth and transmittance
        // (`fs_transmit`, depth-tested among the panes only: a pane nearer than a visible
        // one is visible too). The panes' own light is added after their run's
        // multiplies, the panes behind the nearest weighted by its transmittance
        // (`front_layer_weight`), so the far side of a canopy no longer adds its
        // reflection over the near side unattenuated.
        if let Some((front_depth, front_t)) = front_views.as_ref() {
            let _span = tracing::info_span!("glass_front_layer").entered();
            let _crumb = crate::breadcrumbs::frame_pass("glass_front_layer");
            split_stamp(encoder, 2);
            let mut front = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glass front layer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: front_t,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: front_depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                ..Default::default()
            });
            let mut front_tally =
                crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref());
            front_tally.set_bind_group(&mut front, 0, view_bg, &[]);
            front.set_viewport(draw_vp[0], draw_vp[1], draw_vp[2], draw_vp[3], 0.0, 1.0);
            front.set_scissor_rect(sc_x, sc_y, sc_w, sc_h);
            let mut front_binds = crate::submission::PassBinds::new(trims);
            for draw_index in blend_ix {
                let draw = &draws[*draw_index];
                if !draw.transmissive || draw.graph.is_some() {
                    continue;
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
                front_tally.set_pipeline(
                    &mut front,
                    if draw.cull_off {
                        transmit_front_pipe_ds.get()
                    } else {
                        transmit_front_pipe.get()
                    },
                );
                front_tally.set_bind_group(&mut front, 1, &object.bg, &[]);
                front_tally.set_bind_group(&mut front, 2, &material.bg, material.offsets());
                front_binds.mesh(&mut front, &mesh.vb, &mesh.ib);
                front_tally.draw_indexed(&mut front, mesh.indices(), mesh.base_vertex, 0..1);
            }
            draw_counts.translucent = draw_counts.translucent.plus(front_tally.finish());
            binds.add(front_binds.counts());
            drop(front);
            split_stamp(encoder, 3);
        }
        // A new pass holds no buffers.
        mesh_binds.forget();
        pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("forward blended (refraction)"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: pass_color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            // Read-only: `fs_refract` samples the same depth.
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: forward_depth,
                depth_ops: None,
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        pass.set_bind_group(0, view_bg, &[]);
        pass.set_bind_group(3, refract_bg, &[]);
        pass.set_viewport(draw_vp[0], draw_vp[1], draw_vp[2], draw_vp[3], 0.0, 1.0);
        pass.set_scissor_rect(sc_x, sc_y, sc_w, sc_h);
    }
    // With the front layer, a run of consecutive transmissive draws multiplies first and
    // adds its panes' own light after (`deferred`), flushed before any other blended
    // draw and at the end. Without it each pane adds right after its multiply.
    let mut tally = crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref())
        .recording("forward", "translucent", target_name);
    let defer_adds = front_views.is_some();
    let mut deferred: Vec<usize> = Vec::new();
    // `forward_glass`: the translucent section below, timed inside the pass.
    let glass_stamp = timer.as_ref().filter(|_| in_pass_stamps && !blend_ix.is_empty());
    if let Some((set, _)) = glass_stamp {
        set.write_in_pass(&mut pass, FORWARD_GLASS_QUERY_BASE);
    }
    let flush_adds = |pass: &mut wgpu::RenderPass<'_>,
                      tally: &mut crate::frame_stats::DrawTally,
                      deferred: &mut Vec<usize>,
                      mesh_binds: &mut crate::submission::PassBinds| {
        for draw_index in deferred.drain(..) {
            let draw = &draws[draw_index];
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
            let ds = draw.cull_off;
            tally.set_bind_group(pass, 1, &object.bg, &[]);
            tally.set_bind_group(pass, 2, &material.bg, material.offsets());
            mesh_binds.mesh(pass, &mesh.vb, &mesh.ib);
            tally.identify(|| draw_identity(draw));
            let glass_for_draw = if ds {
                glass_pipes_ds.as_ref()
            } else {
                glass_pipes.as_ref()
            };
            if let (Some(index), Some((glass_set, _))) = (draw.glass, glass_for_draw)
                && let Some(glass_bg) = glass_bgs.get(index)
            {
                tally.set_bind_group(pass, 3, glass_bg, &[]);
                let cheap =
                    canopy_cheap && !draw.canopy_outside && glass.get(index).is_some_and(|g| g.inside);
                tally.set_pipeline_named(pass, glass_set.add_for(true, cheap), "glass_add");
                tally.draw_indexed(pass, mesh.indices(), mesh.base_vertex, 0..1);
                if let Some((_, _, refract_bg)) = refract.as_ref() {
                    tally.set_bind_group(pass, 3, refract_bg, &[]);
                }
                continue;
            }
            tally.set_pipeline_named(
                pass,
                if ds {
                    transmit_add_depth_pipe_ds.get()
                } else {
                    transmit_add_depth_pipe.get()
                },
                "transmit_add",
            );
            tally.draw_indexed(pass, mesh.indices(), mesh.base_vertex, 0..1);
        }
    };
    for draw_index in blend_ix {
        let draw = &draws[*draw_index];
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
        let ds = draw.cull_off;
        let graph = draw.graph.and_then(|key| graph_sets.get(&(key, draw.cull_off)));
        let defer = defer_adds && draw.transmissive && draw.graph.is_none();
        if !defer && !deferred.is_empty() {
            flush_adds(&mut pass, &mut tally, &mut deferred, &mut mesh_binds);
        }
        let pipeline = match (graph, draw.alpha_mode) {
            (None, _) if draw.xray && !draw.transmissive && ds => xray_pipe_ds.get(),
            (None, _) if draw.xray && !draw.transmissive => xray_pipe.get(),
            // Transmissive graph: `transmit_mul` below, then its own light added
            // (`fs_main` writes alpha 0 for it, so the target's alpha stays; ADR-347).
            (Some(graph), _) if draw.transmissive => &graph.add,
            (Some(graph), ALPHA_PREMULTIPLIED) => &graph.premul,
            (Some(graph), ALPHA_ADDITIVE) => &graph.add,
            (Some(graph), _) => &graph.blend,
            (None, ALPHA_PREMULTIPLIED) if ds => premul_pipe_ds.get(),
            (None, ALPHA_PREMULTIPLIED) => premul_pipe.get(),
            (None, ALPHA_ADDITIVE) if ds => add_pipe_ds.get(),
            (None, ALPHA_ADDITIVE) => add_pipe.get(),
            (None, _) if ds => blend_pipe_ds.get(),
            (None, _) => blend_pipe.get(),
        };
        // A masked instance (`mask_decals_gpu`): its blend variant and group 3.
        let mask_draw = if draw.mask {
            mask_sets[usize::from(ds)]
                .as_ref()
                .zip(mask_decals.bind_group(draw.object))
                .and_then(|(set, bg)| Some((set.blend.try_get()?, bg)))
        } else {
            None
        };
        // A thin film (`thin_film_gpu`): its blend variant and the material's group 3.
        let film_draw = if draw.film {
            film.pipeline(ds, draw.alpha_mode).zip(material.film.as_ref())
        } else {
            None
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
        tally.identify(|| draw_identity(draw));
        // Cull-none `GLASS` set for a double-sided pane (a canopy seen from inside, a
        // window's back face). The frame's set culls back faces.
        let glass_for_draw = if ds {
            glass_pipes_ds.as_ref()
        } else {
            glass_pipes.as_ref()
        };
        if let (Some(index), Some((glass_set, _))) = (draw.glass, glass_for_draw)
            && let Some(glass_bg) = glass_bgs.get(index)
        {
            // Own-canopy / damaged glass: the `GLASS` variant with its own group 3. The
            // refraction copy was taken before any blended draw, so glass seen through
            // this instance still refracts.
            tally.set_bind_group(&mut pass, 3, glass_bg, &[]);
            tally.set_pipeline_named(&mut pass, &glass_set.mul, "glass_multiply");
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            if refract.is_some() && draw.refracts {
                tally.set_pipeline_named(&mut pass, &glass_set.refract, "glass_refract");
                tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            }
            if defer {
                deferred.push(*draw_index);
            } else {
                let cheap =
                    canopy_cheap && !draw.canopy_outside && glass.get(index).is_some_and(|g| g.inside);
                tally.set_pipeline_named(&mut pass, glass_set.add_for(false, cheap), "glass_add");
                tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            }
            if let Some((_, _, refract_bg)) = refract.as_ref() {
                tally.set_bind_group(&mut pass, 3, refract_bg, &[]);
            }
            continue;
        }
        if draw.transmissive && draw.graph.is_none() {
            // What is behind × transmittance, then + the surface's own light.
            tally.set_pipeline_named(
                &mut pass,
                if ds {
                    transmit_mul_pipe_ds.get()
                } else {
                    transmit_mul_pipe.get()
                },
                "transmit_multiply",
            );
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            if refract.is_some() && draw.refracts {
                // + transmittance × (bent background − unbent background).
                tally.set_pipeline_named(
                    &mut pass,
                    if ds {
                        transmit_refract_pipe_ds.get()
                    } else {
                        transmit_refract_pipe.get()
                    },
                    "transmit_refract",
                );
                tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            }
            if defer {
                deferred.push(*draw_index);
                continue;
            }
            // With the opaque depth bound (refraction copy), the pane's own reflection
            // takes its traced visibility (`fs_transmit_add`).
            tally.set_pipeline_named(
                &mut pass,
                match (refract.is_some(), ds) {
                    (true, true) => transmit_add_depth_pipe_ds.get(),
                    (true, false) => transmit_add_depth_pipe.get(),
                    (false, true) => transmit_add_pipe_ds.get(),
                    (false, false) => transmit_add_pipe.get(),
                },
                "transmit_add",
            );
        } else if draw.transmissive
            && let Some(graph) = graph
        {
            // A transmissive graph (ADR-347): what is behind × its tinted transmittance
            // (`fs_transmit` on the graph's base colour), then + its own light. It has no
            // refraction pass and no glass front layer.
            tally.set_pipeline_named(&mut pass, &graph.transmit_mul, "material_graph_transmit_multiply");
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            tally.set_pipeline_named(&mut pass, pipeline, "material_graph_add");
        } else if let Some((pipe, bg)) = mask_draw {
            tally.set_pipeline_named(&mut pass, pipe, "mask_decals_blend");
            tally.set_bind_group(&mut pass, 3, bg, &[]);
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            // The transmissive draws after it read the refraction group there.
            if let Some((_, _, refract_bg)) = refract.as_ref() {
                tally.set_bind_group(&mut pass, 3, refract_bg, &[]);
            }
            continue;
        } else if let Some((pipe, binding)) = film_draw {
            tally.set_pipeline_named(&mut pass, pipe, "thin_film_blend");
            tally.set_bind_group(&mut pass, 3, binding.bind_group(), &[]);
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
            if let Some((_, _, refract_bg)) = refract.as_ref() {
                tally.set_bind_group(&mut pass, 3, refract_bg, &[]);
            }
            continue;
        } else {
            tally.set_pipeline_named(
                &mut pass,
                pipeline,
                blend_pipeline_name(graph.is_some(), draw.xray, draw.alpha_mode),
            );
        }
        tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
    }
    if !deferred.is_empty() {
        flush_adds(&mut pass, &mut tally, &mut deferred, &mut mesh_binds);
    }
    draw_counts.translucent = draw_counts.translucent.plus(tally.finish());
    if let Some((set, _)) = glass_stamp {
        set.write_in_pass(&mut pass, FORWARD_GLASS_QUERY_BASE + 1);
        *glass_stamped = true;
    }
    binds.add(mesh_binds.counts());
    Ok(())
}

/// The renderer fields the GPU-driven colour batches read and write.
pub(super) struct GpuColorFields<'a> {
    pub(super) culler: &'a mut crate::gpu_driven::GpuCuller,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) scratch: &'a mut scratch::FrameScratch,
    pub(super) skin_buf: &'a Option<wgpu::Buffer>,
    pub(super) late_gpu_color_builds: &'a mut u64,
}

/// What the GPU-driven colour batches read from the frame.
pub(super) struct GpuColorInputs<'a> {
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// The view bind group layout.
    pub(super) view_bgl: &'a wgpu::BindGroupLayout,
    /// The material bind group layout.
    pub(super) mat_bgl: &'a wgpu::BindGroupLayout,
    /// The colour format of the pass.
    pub(super) color_format: wgpu::TextureFormat,
    /// The pipelines encode sRGB in the shader.
    pub(super) encode: bool,
    /// The sample count the frame picked before its targets.
    pub(super) samples_early: u32,
    /// The scene draws both faces.
    pub(super) double_sided: bool,
}

/// The GPU-driven colour pass of the frame when it draws instanced batches (`mdi`): uploads the
/// object table, builds the instanced object group and the colour pipeline variants the batches
/// bind (counted in `late_gpu_color_builds` when the forward-set overlap missed one).
pub(super) fn prepare_gpu_color(
    fields: GpuColorFields<'_>,
    d: &crate::TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &GpuColorInputs<'_>,
    mdi: bool,
) -> Result<Option<GpuColor>, RenderError> {
    let GpuColorFields {
        culler,
        belt,
        scratch,
        skin_buf,
        late_gpu_color_builds,
    } = fields;
    let GpuColorInputs {
        draws,
        view_bgl,
        mat_bgl,
        color_format,
        encode,
        samples_early,
        double_sided,
    } = *inputs;
    if !mdi {
        return Ok(None);
    }
    // The record id is the draw index, so a shared object key does not share a slot.
    // A repeated id keeps the first draw, same as a linear scan. A scan is quadratic in
    // the instance count (the 100k stress scene spends seconds here). The map and the
    // bytes are scratch: a steady frame of 100k instances allocates neither (ADR-385).
    let mut draw_at = scratch.object_draw.take();
    let mut bytes = scratch.object_bytes.take();
    scratch::pack_gpu_objects(
        &mut bytes,
        &mut draw_at,
        draws
            .iter()
            .enumerate()
            .map(|(index, _)| scratch::gpu_record_id(index)),
        culler.draws(),
        |index| {
            let draw = &draws[index];
            ObjectData {
                model: draw.model,
                skin: [
                    draw.joint_base,
                    draw.lod_dither.to_bits(),
                    draw.over[0],
                    draw.over[1],
                ],
            }
        },
    );
    scratch.object_draw.give(draw_at);
    {
        let mut sink = crate::staging::LiveBelt {
            belt,
            device: d,
            encoder,
        };
        culler.write_objects(d, &mut sink, &bytes)?;
    }
    scratch.object_bytes.give(bytes);
    let skin = skin_buf.clone().expect("skin buffer");
    // Each (mesh, material) batch is one instanced indirect draw: the cull packs the
    // batch's visible object slots and counts them into the batch command, so a
    // thousand rocks of one mesh cost one draw, not a thousand (specs/13 § 5 step 4).
    let bg = culler.instanced_object_bind_group(d, &skin)?;
    // Opaque omits `discard` so the depth prepass can reject fragments.
    // Mask keeps it. Draws stay in batch order; only the pipeline changes.
    // `[cull mode][mask]`: `[culled, cull-none]` × `[opaque, mask]`. Double-sided batches
    // ([`crate::gpu_driven::GpuBatch::double_sided`]) sort after the culled ones.
    let any_ds = culler.batches().iter().any(|batch| batch.double_sided);
    // Every variant the frame may bind, compiled side by side
    // (`GpuCuller::color_pipelines`).
    let mut variants = vec![(double_sided, false), (double_sided, true)];
    if any_ds {
        variants.extend([(true, false), (true, true)]);
    }
    let built_before = culler.color_pipelines_built();
    let pipes = culler.color_pipelines(
        d,
        color_format,
        encode,
        samples_early,
        true,
        view_bgl,
        mat_bgl,
        &variants,
    )?;
    // Built here, not with the forward set: the overlap above missed a variant.
    *late_gpu_color_builds += culler.color_pipelines_built() - built_before;
    let culled = [pipes[0].clone(), pipes[1].clone()];
    let cull_none = if any_ds {
        [pipes[2].clone(), pipes[3].clone()]
    } else {
        culled.clone()
    };
    Ok(Some((
        [culled, cull_none],
        bg,
        culler.batch_commands().cloned(),
        culler.batches().to_vec(),
    )))
}

/// The frame's forward draw statistics (instanced or per-object opaque and graph draws,
/// double-sided per-object draws, skinned crowds and their batches) into `stats`.
pub(super) fn count_forward_draws(stats: &mut DrawStats, draws: &[Draw], gpu_color: &Option<GpuColor>) {
    // Same test as the colour loop: an instanced opaque draw (rigid, or skinned with a palette
    // sphere) skips the CPU loop. Only those two kinds get an indirect slot.
    let instanced_draw =
        |draw: &Draw| gpu_color.is_some() && draw.indirect.is_some() && draw.alpha_mode < ALPHA_BLEND;
    let graph_draws = draws.iter().filter(|draw| draw.visible && draw.graph.is_some());
    stats.graph_instanced = graph_draws.clone().filter(|draw| instanced_draw(draw)).count();
    stats.graph_per_object = graph_draws.filter(|draw| !instanced_draw(draw)).count();
    let forward_opaque = draws
        .iter()
        .filter(|draw| draw.visible && !draw.vis && draw.alpha_mode < ALPHA_BLEND);
    stats.instanced = forward_opaque.clone().filter(|draw| instanced_draw(draw)).count();
    stats.per_object = forward_opaque
        .clone()
        .filter(|draw| !instanced_draw(draw))
        .count();
    stats.double_sided_per_object = forward_opaque
        .clone()
        .filter(|draw| draw.cull_off && !instanced_draw(draw))
        .count();
    // Skinned crowds (ADR-1368): how many skinned draws the colour pass made, how many of
    // them went through the instanced batches, and how many draws they would take if every
    // `(cull mode, mesh, material, pipeline)` group were one.
    let skinned = forward_opaque.filter(|draw| !draw.rigid);
    let skinned_counts =
        skinned_instancing::SkinnedCounts::count(skinned.map(|draw| skinned_instancing::SkinnedDraw {
            cull_off: draw.cull_off,
            mesh: draw.mesh,
            material: draw.material,
            graph: draw.graph,
            solo: (draw.mask || draw.film || draw.subsurface).then_some(draw.object),
            instanced: instanced_draw(draw),
        }));
    skinned_counts.apply(stats);
    stats.skinned_batches = gpu_color.as_ref().map_or(0, |(.., batches)| {
        batches
            .iter()
            .filter(|batch| batch.skinned && batch.count > 0)
            .count()
    });
    stats.static_batches = gpu_color.as_ref().map_or(0, |(.., batches)| {
        batches.iter().filter(|batch| batch.is_static).count()
    });
}

/// What the refraction copy and the glass groups are built from.
pub(super) struct RefractionInputs<'a> {
    /// The frame's scene (screen-space refraction switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The linear HDR colour texture, `None` on a display-referred frame.
    pub(super) hdr_texture: &'a Option<wgpu::Texture>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Indices of the blended draws, sorted.
    pub(super) blend_ix: &'a [usize],
    /// The refraction bind group layout.
    pub(super) refract_bgl: &'a wgpu::BindGroupLayout,
    /// The refraction sampler.
    pub(super) refract_sampler: &'a wgpu::Sampler,
    /// The culled glass pipelines and their instance layout, when a draw needs them.
    pub(super) glass_pipes: &'a Option<(glass_gpu::GlassSet, wgpu::BindGroupLayout)>,
    /// The cull-none glass pipelines and their instance layout, when a draw needs them.
    pub(super) glass_pipes_ds: &'a Option<(glass_gpu::GlassSet, wgpu::BindGroupLayout)>,
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// Internal (pre-upscale) render height.
    pub(super) internal_h: u32,
    /// MSAA sample count.
    pub(super) samples: u32,
}

/// The refraction copy, glass front layer and per-instance glass groups the forward pass binds.
pub(super) struct RefractionPrep {
    /// The glass front layer (depth, transmittance), when the frame refracts.
    pub(super) front_views: Option<(wgpu::TextureView, wgpu::TextureView)>,
    /// The HDR source, its copy and the refraction group, when the frame refracts.
    pub(super) refract: Option<(wgpu::Texture, wgpu::Texture, wgpu::BindGroup)>,
    /// Each glass instance's group 3 this frame.
    pub(super) glass_bgs: Vec<wgpu::BindGroup>,
    /// Viewer-inside glass uses the cheap canopy shader.
    pub(super) canopy_cheap: bool,
}

impl FrameRenderer {
    /// Prepares the screen-space refraction (copy target, front layer, group) and the glass
    /// instances' groups for the forward pass.
    pub(super) fn prepare_refraction(
        &mut self,
        d: &crate::TrackedDevice,
        q: &crate::device::TrackedQueue,
        inputs: &RefractionInputs<'_>,
    ) -> RefractionPrep {
        let RefractionInputs {
            scene,
            hdr_texture,
            depth_view,
            draws,
            blend_ix,
            refract_bgl,
            refract_sampler,
            glass_pipes,
            glass_pipes_ds,
            fov_y,
            internal_h,
            samples,
        } = *inputs;
        // Screen-space refraction: an HDR frame without MSAA with at least one refracting
        // glass draw. The forward pass then splits after the sky; the opaque frame is
        // copied and the blended draws run in a second pass that reads it (depth read-only).
        // The glass front layer runs with the refraction copy (its depth and transmittance).
        let mut front_views: Option<(wgpu::TextureView, wgpu::TextureView)> = None;
        let mut refract_view: Option<wgpu::TextureView> = None;
        let refract = match hdr_texture.as_ref() {
            Some(source)
                if scene.quality.screen_space_refraction
                    && samples == 1
                    && blend_ix
                        .iter()
                        .any(|&i| draws[i].refracts && draws[i].graph.is_none()) =>
            {
                let size = source.size();
                let crate::TransientTexture {
                    texture: copy,
                    view: copy_view,
                    ..
                } = self.refract_copy(d, size.width, size.height, source.format());
                let front = self.front_layer(d, size.width, size.height);
                let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("lab refraction bg"),
                    layout: refract_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::TextureView(&front.0),
                        },
                        wgpu::BindGroupEntry {
                            binding: 8,
                            resource: wgpu::BindingResource::TextureView(&front.1),
                        },
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&copy_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(depth_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(refract_sampler),
                        },
                    ],
                });
                front_views = Some(front);
                refract_view = Some(copy_view);
                Some((source.clone(), copy, bg))
            }
            _ => None,
        };

        // Per-instance glass: this frame's group 3 for each instance (refraction copy + page).
        // Viewer-inside instances add their own light with the cheap canopy shader unless the
        // setting asks for the full one or the camera is clearly outside them
        // (`Draw::canopy_outside`, ADR-262).
        let canopy = self.canopy_glass();
        let canopy_cheap = canopy.shading == crate::glass::CanopyShading::Cheap;
        let glass_bgs = match glass_pipes.as_ref().or(glass_pipes_ds.as_ref()) {
            Some((_, glass_bgl)) => {
                let frame = glass_gpu::GlassFrame {
                    refract: refract_view.as_ref().map(|view| (view, depth_view)),
                    front: front_views.as_ref().map(|(depth, t)| (depth, t)),
                    pixel_angle: 2.0 * (fov_y * 0.5).tan() / internal_h.max(1) as f32,
                    nearest_layer_only: canopy_cheap && canopy.nearest_layer_only,
                };
                self.glass_gpu.prepare(d, q, glass_bgl, &self.glass, &frame)
            }
            None => Vec::new(),
        };
        RefractionPrep {
            front_views,
            refract,
            glass_bgs,
            canopy_cheap,
        }
    }
}
