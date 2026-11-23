// SPDX-License-Identifier: MPL-2.0
//! The shadow passes of `FrameRenderer::render_into` (ADR-1552 split by pass): sun cascades
//! first, then the local-light atlas tiles, with the visibility-buffer cluster shadows and the
//! shared draw's (terrain) shadow inside each job. Moved out of `frame.rs` unchanged. The frame
//! still borrows its pipeline sets here, so the pass takes the renderer fields it touches
//! ([`ShadowFields`]) instead of `self`.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use std::collections::HashMap;

use super::shadow_plan::ShadowPlan;
use super::*;
use crate::TrackedDevice;

/// The shared draw's (terrain) last shadow stamp per cascade: view-projection and content key.
pub(super) type SharedShadowKeys = [Option<([[f32; 4]; 4], u64)>; 4];

/// The frame's material-graph pipeline sets, keyed by `(code key, cull-none)`.
pub(super) type GraphSets = HashMap<(u64, bool), std::sync::Arc<crate::graph_pipelines::GraphPipelineSet>>;

/// The GPU-driven instanced passes of the frame: the instance bind group, shadow batches per
/// job, depth batches, and the `[culled, cull-none]` shadow, `[cull][mask]` depth and
/// `[culled, cull-none]` alpha-tested shadow pipelines.
pub(super) type InstancedPasses = (
    wgpu::BindGroup,
    Vec<Vec<crate::gpu_driven::InstanceBatch>>,
    Vec<crate::gpu_driven::InstanceBatch>,
    Option<[wgpu::RenderPipeline; 2]>,
    Option<[[wgpu::RenderPipeline; 2]; 2]>,
    Option<[wgpu::RenderPipeline; 2]>,
);

/// The renderer fields the shadow passes read and write.
pub(super) struct ShadowFields<'a> {
    pub(super) tile_vp_bufs: &'a mut Vec<wgpu::Buffer>,
    pub(super) shadow_report: &'a mut ShadowCacheReport,
    pub(super) tile_vp_bytes: &'a mut Vec<Vec<u8>>,
    pub(super) shared_shadow_keys: &'a mut SharedShadowKeys,
    pub(super) visbuffer: &'a mut crate::visbuffer::VisBufferPass,
    pub(super) tile_vp_bgs: &'a mut Vec<wgpu::BindGroup>,
    pub(super) shared_draw: &'a mut Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
    pub(super) meshes: &'a mut crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) materials: &'a mut crate::key_hash::KeyMap<u64, MaterialEntry>,
    pub(super) entries: &'a mut lifecycle::EntryLifecycle,
    pub(super) shared_shadow_region_draws: &'a mut u64,
    pub(super) shared_shadow_draws: &'a mut u64,
    pub(super) objects: &'a mut crate::key_hash::KeyMap<ObjectUniformKey, ObjectEntry>,
    pub(super) csm_valid: &'a mut bool,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    /// Two-phase occlusion of the casters ([`FrameRenderer::set_shadow_occlusion`]).
    pub(super) occlusion: &'a mut crate::shadow_occlusion::ShadowOcclusion,
}

/// What the shadow passes read from the frame.
pub(super) struct ShadowInputs<'a> {
    pub(super) scene: &'a FrameScene<'a>,
    pub(super) pipes: &'a Pipes,
    pub(super) plan: &'a ShadowPlan,
    pub(super) shadow_jobs: &'a [ShadowJob],
    pub(super) job_casters: &'a [std::sync::Arc<[usize]>],
    pub(super) draws: &'a [Draw],
    pub(super) graph_sets: &'a GraphSets,
    pub(super) instanced: &'a Option<InstancedPasses>,
    pub(super) set: &'a PipelineSet,
    pub(super) set_ds: &'a PipelineSet,
    pub(super) csm_views: &'a [wgpu::TextureView],
    pub(super) atlas_view: &'a wgpu::TextureView,
    pub(super) active_cascades: u32,
    pub(super) cascade_count: usize,
    pub(super) csm_size: u32,
    pub(super) samples: u32,
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    pub(super) timer: &'a Option<(FrameStamps, f32)>,
    pub(super) trims: bool,
    pub(super) vis_shadow_drawn: bool,
    /// A render-to-texture view is drawing (only the main view culls its casters).
    pub(super) view_pass: bool,
    /// [`FrameRenderer::set_alpha_tested_shadows`] (the instance lists' grouping).
    pub(super) alpha_tested_shadows: bool,
    /// The frame graph's dispatch records the `shadow_occlusion` node this frame (ADR-5178): the
    /// graph holds it (two-phase shadow occlusion is on). When `false` phase 1 records nothing.
    pub(super) shadow_phase1: bool,
}

/// The frame's counters the shadow passes add to.
pub(super) struct ShadowCounters<'a> {
    pub(super) binds: &'a mut crate::submission::BindCounts,
    pub(super) draw_counts: &'a mut crate::frame_stats::FrameDrawCounts,
    pub(super) shadow_cpu: &'a mut ShadowCpuTimings,
}

/// Records every shadow job of the frame.
pub(super) fn record_shadow_passes(
    fields: ShadowFields<'_>,
    d: &TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &ShadowInputs<'_>,
    counters: ShadowCounters<'_>,
) -> Result<(), RenderError> {
    let ShadowFields {
        tile_vp_bufs,
        shadow_report,
        tile_vp_bytes,
        shared_shadow_keys,
        visbuffer,
        tile_vp_bgs,
        shared_draw,
        meshes,
        materials,
        entries,
        shared_shadow_region_draws,
        shared_shadow_draws,
        objects,
        csm_valid,
        belt,
        occlusion,
    } = fields;
    let ShadowInputs {
        scene,
        pipes,
        plan,
        shadow_jobs,
        job_casters,
        draws,
        graph_sets,
        instanced,
        set,
        set_ds,
        csm_views,
        atlas_view,
        active_cascades,
        cascade_count,
        csm_size,
        samples,
        texture_table_group,
        timer,
        trims,
        vis_shadow_drawn,
        view_pass,
        alpha_tested_shadows,
        shadow_phase1,
    } = *inputs;
    let ShadowCounters {
        binds,
        draw_counts,
        shadow_cpu,
    } = counters;
    let _span = tracing::info_span!("shadow_passes").entered();
    // A skipped cascade has no job. Leave its view-projection where the
    // last redraw put it, and skip the copy when those bytes match.
    let mut vp_for_slot = vec![None; tile_vp_bufs.len()];
    if scene.directional_shadows {
        for (i, cascade) in plan.csm.cascades.iter().take(cascade_count).enumerate() {
            if i < vp_for_slot.len() {
                vp_for_slot[i] = Some(ShadowVp {
                    light_view_proj: cascade.light_view_proj,
                    clock: plan.clock,
                    clock_live: [f32::from(plan.clock_live), 0.0, 0.0, 0.0],
                });
            }
        }
    }
    for (i, tile) in plan.tiles.iter().enumerate() {
        let slot = cascade_count + i;
        if slot < vp_for_slot.len() {
            vp_for_slot[slot] = Some(ShadowVp {
                light_view_proj: tile.gpu.view_proj,
                clock: plan.clock,
                clock_live: [f32::from(plan.clock_live), 0.0, 0.0, 0.0],
            });
        }
    }
    let mut seen = vec![false; tile_vp_bufs.len()];
    for job in shadow_jobs {
        let slot = job.slot;
        if slot >= tile_vp_bufs.len() || seen[slot] {
            continue;
        }
        seen[slot] = true;
        let Some(vp) = vp_for_slot[slot] else {
            continue;
        };
        let bytes = bytemuck::bytes_of(&vp);
        if tile_vp_bytes
            .get(slot)
            .is_some_and(|cached| cached.as_slice() == bytes)
        {
            continue;
        }
        let buf = tile_vp_bufs[slot].clone();
        belt.write(d, encoder, &buf, 0, bytes);
        if tile_vp_bytes.len() <= slot {
            tile_vp_bytes.resize(slot + 1, Vec::new());
        }
        // Reuses the slot's buffer: under a live clock every slot's bytes change every frame
        // (corr-render-frame-upscaler LOW 9).
        tile_vp_bytes[slot].clear();
        tile_vp_bytes[slot].extend_from_slice(bytes);
    }
    // Two-phase occlusion of the casters (ADR-2072): each culled kind's phase 1, against the
    // pyramid its last redraw left, before any shadow pass draws.
    let two_phase_inputs = shadow_two_phase::ShadowTwoPhaseInputs {
        plan,
        shadow_jobs,
        job_casters,
        draws,
        instanced,
        meshes: &*meshes,
        csm_views,
        atlas_view,
        cascade_count,
        csm_size,
        view_pass,
        alpha_tested_shadows,
    };
    // Phase 1 is the frame graph's `shadow_occlusion` node (ADR-5178): without it the frame records
    // and creates nothing for the cull, as with the switch off. A cull that cannot make its
    // resources skips (the casters draw directly), not the frame.
    if !shadow_phase1 {
        shadow_two_phase::without_shadow_phase1(occlusion, view_pass);
    } else if let Err(err) =
        shadow_two_phase::record_shadow_phase1(occlusion, d, belt, encoder, &two_phase_inputs)
    {
        tracing::warn!(target: "regia_render", %err, "shadow occlusion skipped this frame");
        occlusion.skip(crate::ShadowOcclusionSkip::Failed);
    }
    let page_clear_pipe = pipes.page_clear.clone();
    for kind in 0..5u8 {
        // Per-cascade CPU cost in a profile (`render_into>encode>shadow_passes>…`).
        let _kind_span = match kind {
            0 => tracing::info_span!("shadow_cascade0"),
            1 => tracing::info_span!("shadow_cascade1"),
            2 => tracing::info_span!("shadow_cascade2"),
            3 => tracing::info_span!("shadow_cascade3"),
            _ => tracing::info_span!("shadow_atlas"),
        }
        .entered();
        if let Some((set, _)) = timer {
            set.write(encoder, SHADOW_KIND_QUERY_BASE + u32::from(kind));
        }
        let batch: Vec<(usize, &ShadowJob)> = shadow_jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| job.kind == kind)
            .collect();
        // A cascade with no mesh jobs still has to receive the terrain.
        // A clean paged cascade loads the stored depth so mesh shadows
        // stay; the heightfield is drawn on top. A new map is cleared.
        let terrain_cascade =
            kind < 4 && (kind as u32) < active_cascades && samples == 1 && shared_draw.is_some();
        let retain_cached = batch.is_empty() && *csm_valid;
        // A retained cascade already holds this terrain when the key and
        // the cascade view-projection match the last draw into it.
        let terrain_stamp = if terrain_cascade {
            let vp = plan.csm.cascades.get(kind as usize).map(|c| c.light_view_proj);
            let key = shared_draw
                .as_ref()
                .and_then(|draw| draw.cascade_shadow_key(u32::from(kind)));
            vp.zip(key)
        } else {
            None
        };
        let terrain_clean =
            retain_cached && terrain_stamp.is_some() && shared_shadow_keys[kind as usize] == terrain_stamp;
        shadow_report.jobs_per_kind[kind as usize] = batch.len() as u32;
        if batch.is_empty() && (!terrain_cascade || terrain_clean) {
            continue;
        }
        shadow_report.shadow_passes += 1;
        // Only recorded passes count: a skipped cascade stays at 0.
        let _time = AddElapsedMs(
            &mut shadow_cpu.encode_ms[kind as usize],
            std::time::Instant::now(),
        );
        let partial = batch.iter().any(|(_, job)| job.page_clear);
        let view = if kind == 4 {
            atlas_view
        } else {
            &csm_views[(kind as usize).min(csm_views.len() - 1)]
        };
        let _crumb = crate::breadcrumbs::frame_pass("shadow_cascades");
        let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shadow"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view,
                depth_ops: Some(wgpu::Operations {
                    load: if partial || retain_cached {
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
        let mut tally = crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref());
        let instanced_shadow = instanced.as_ref().and_then(|inst| {
            inst.3
                .as_ref()
                .map(|pipe| (pipe, &inst.0, &inst.1, inst.5.as_ref()))
        });
        let instanced_jobs = instanced_shadow.map(|(pipe, obj_bg, job_lists, mask_pipe)| InstancedJobs {
            batch: &batch,
            job_lists,
            meshes: &*meshes,
            materials: &*materials,
            graph_sets,
            pipe,
            obj_bg,
            mask_pipe,
            tile_vp_bgs: tile_vp_bgs.as_slice(),
            page_clear_pipe: &page_clear_pipe,
            csm_size,
            kind,
            trims,
            vis_shadow_drawn,
        });
        // A kind culled in two phases (`shadow_two_phase`) draws phase 1's survivors here.
        let culled_kind = instanced_jobs.is_some() && occlusion.kind_active(usize::from(kind));
        if let Some(jobs) = instanced_jobs.as_ref() {
            let mut mesh_binds = crate::submission::PassBinds::new(trims);
            let mode = match occlusion.phase1_commands().filter(|_| culled_kind) {
                Some(commands) => BatchDraw::Phase1(commands),
                None => BatchDraw::Direct,
            };
            draw_instanced_jobs(&mut pass, &mut tally, &mut mesh_binds, visbuffer, jobs, mode);
            binds.add(mesh_binds.counts());
        } else {
            let mut mesh_binds = crate::submission::PassBinds::new(trims);
            for &(job_index, job) in &batch {
                if job.page_clear {
                    tally.set_pipeline(&mut pass, &page_clear_pipe);
                    pass.set_viewport(
                        job.rect[0] as f32,
                        job.rect[1] as f32,
                        job.rect[2] as f32,
                        job.rect[3] as f32,
                        0.0,
                        1.0,
                    );
                    pass.set_scissor_rect(job.rect[0], job.rect[1], job.rect[2], job.rect[3]);
                    tally.draw(&mut pass, 0..3, 0..1);
                }
                tally.set_pipeline(&mut pass, &set.shadow);
                tally.set_bind_group(&mut pass, 0, &tile_vp_bgs[job.slot], &[]);
                if kind == 4 {
                    pass.set_viewport(
                        job.rect[0] as f32,
                        job.rect[1] as f32,
                        job.rect[2] as f32,
                        job.rect[3] as f32,
                        0.0,
                        1.0,
                    );
                    pass.set_scissor_rect(job.rect[0], job.rect[1], job.rect[2], job.rect[3]);
                } else if job.page_clear {
                    // A sun page is drawn with the whole cascade's matrix: full
                    // viewport, scissored to the page. (A page-sized viewport
                    // squeezed the cascade into the page, and a moving caster's
                    // shadow vanished once the cascade stayed in place.)
                    pass.set_viewport(0.0, 0.0, csm_size as f32, csm_size as f32, 0.0, 1.0);
                    pass.set_scissor_rect(job.rect[0], job.rect[1], job.rect[2], job.rect[3]);
                }
                let mut graph_bound = false;
                let mut bound_cull_off = false;
                for draw_index in job_casters.get(job_index).map_or(&[][..], |list| &list[..]) {
                    let draw = &draws[*draw_index];
                    let Some(mesh) = meshes.get(&draw.mesh) else {
                        missing_draw_entry(&entries.skips, "mesh", draw.mesh);
                        continue;
                    };
                    let Some(object) = objects.get(&draw.uniform_key()) else {
                        missing_draw_entry(&entries.skips, "object", draw.object);
                        continue;
                    };
                    // A vertex-stage graph casts its displaced shadow.
                    let graph = draw
                        .graph
                        .filter(|_| draw.graph_vertex)
                        .and_then(|key| graph_sets.get(&(key, draw.cull_off)))
                        .and_then(|graph_set| Some((graph_set, materials.get(&draw.material)?)));
                    match graph {
                        Some((graph_set, material)) => {
                            let Some(graph_pipe) = graph_set.shadow.as_ref() else {
                                continue;
                            };
                            tally.set_pipeline(&mut pass, graph_pipe);
                            tally.set_bind_group(&mut pass, 2, &material.bg, material.offsets());
                            if graph_set.uv1 {
                                let Some(uv1) = mesh.uv1.as_ref() else {
                                    continue;
                                };
                                mesh_binds.vertex(&mut pass, 1, uv1);
                            }
                            graph_bound = true;
                        }
                        // Double-sided casters use the cull-none pipeline.
                        None if graph_bound || draw.cull_off != bound_cull_off => {
                            tally.set_pipeline(
                                &mut pass,
                                if draw.cull_off {
                                    &set_ds.shadow
                                } else {
                                    &set.shadow
                                },
                            );
                            graph_bound = false;
                            bound_cull_off = draw.cull_off;
                        }
                        None => {}
                    }
                    tally.set_bind_group(&mut pass, 1, &object.bg, &[]);
                    mesh_binds.mesh(&mut pass, &mesh.vb, &mesh.ib);
                    tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
                }
                if vis_shadow_drawn {
                    visbuffer.draw_shadow_job(&mut pass, job_index);
                    tally.forget_state();
                    mesh_binds.forget();
                }
            }
            binds.add(mesh_binds.counts());
        }
        draw_counts.shadow = draw_counts.shadow.plus(tally.finish());
        // The ground under the precipitation occlusion tile, when the tile redraws.
        let precip_job = plan
            .precip_tile
            .and_then(|tile| batch.iter().find(|(_, job)| job.slot == cascade_count + tile));
        if let (Some((_, job)), 1, Some(draw)) = (precip_job, samples, shared_draw.as_ref()) {
            let [x, y, w, h] = job.rect;
            pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
            pass.set_scissor_rect(x, y, w, h);
            draw.draw_precip_occlusion(&mut pass);
        }
        if terrain_cascade && let Some(draw) = shared_draw.as_ref() {
            pass.set_viewport(0.0, 0.0, csm_size as f32, csm_size as f32, 0.0, 1.0);
            // Page jobs only (a clean cascade under moved meshes): the cleared pages
            // need the shared geometry back, the rest of the cascade already holds it.
            let pages_only = !batch.is_empty()
                && batch.iter().all(|(_, job)| job.page_clear)
                && terrain_stamp.is_some()
                && shared_shadow_keys[kind as usize] == terrain_stamp;
            if pages_only {
                let (mut x0, mut y0, mut x1, mut y1) = (csm_size, csm_size, 0u32, 0u32);
                for (_, job) in &batch {
                    x0 = x0.min(job.rect[0]);
                    y0 = y0.min(job.rect[1]);
                    x1 = x1.max(job.rect[0] + job.rect[2]);
                    y1 = y1.max(job.rect[1] + job.rect[3]);
                }
                let (x1, y1) = (x1.min(csm_size), y1.min(csm_size));
                pass.set_scissor_rect(x0, y0, x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
                draw.draw_shadow_region(&mut pass, u32::from(kind));
                *shared_shadow_region_draws += 1;
                shadow_report.shared_per_cascade[(kind as usize).min(3)] = 2;
            } else {
                pass.set_scissor_rect(0, 0, csm_size, csm_size);
                draw.draw(
                    &mut pass,
                    crate::SharedPass::Shadow {
                        index: u32::from(kind),
                    },
                );
                *shared_shadow_draws += 1;
                shadow_report.shared_per_cascade[(kind as usize).min(3)] = 1;
            }
            shared_shadow_keys[kind as usize] = terrain_stamp;
        }
        drop(pass);
        // Phase 2: the pyramid from phase 1's depth, the cull of what phase 1 skipped, and a
        // second pass over the loaded map for the casters it finds visible.
        if culled_kind && let Some(jobs) = instanced_jobs.as_ref() {
            occlusion.rebuild_pyramid(encoder, usize::from(kind));
            // Without history phase 1 drew every caster: no phase 2.
            let phase2 = occlusion.kind_has_history(usize::from(kind));
            if phase2 {
                occlusion.record_phase2(encoder, usize::from(kind));
            }
            if let Some(commands) = occlusion.phase2_commands().filter(|_| phase2) {
                let _phase2 = tracing::info_span!("shadow_phase2").entered();
                let _crumb = crate::breadcrumbs::frame_pass("shadow_phase2");
                let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("shadow phase 2"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    ..Default::default()
                });
                let mut tally =
                    crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref());
                let mut mesh_binds = crate::submission::PassBinds::new(trims);
                draw_instanced_jobs(
                    &mut pass,
                    &mut tally,
                    &mut mesh_binds,
                    visbuffer,
                    jobs,
                    BatchDraw::Phase2(commands),
                );
                binds.add(mesh_binds.counts());
                draw_counts.shadow = draw_counts.shadow.plus(tally.finish());
            }
        }
    }
    shadow_two_phase::note_shadow_history(occlusion, &two_phase_inputs);
    Ok(())
}

/// What the visibility-buffer shadow cull reads from the frame.
pub(super) struct VisShadowInputs<'a> {
    /// The frame's scene (double-sided switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame's shadow plan.
    pub(super) plan: &'a ShadowPlan,
    /// The frame's shadow jobs (cascades first, then atlas tiles).
    pub(super) shadow_jobs: &'a [ShadowJob],
    /// The cascade split distances.
    pub(super) csm_splits: &'a [f32],
    /// The visibility set's eye, snapped to its cell.
    pub(super) vis_set_eye: regia_core::glam::Vec3,
    /// The sun cascade resolution.
    pub(super) csm_size: u32,
    /// Shadow buffer slots taken by the cascades.
    pub(super) cascade_count: usize,
    /// The frame draws visibility-buffer casters into its shadows.
    pub(super) vis_shadows: bool,
}

/// Records the visibility-buffer casters' cluster cull, one per shadow job (`vis_shadow.wgsl`);
/// the culled clusters are drawn inside each job's pass by [`record_shadow_passes`]. Returns
/// whether the cull ran.
pub(super) fn record_vis_shadow_cull(
    visbuffer: &mut crate::visbuffer::VisBufferPass,
    d: &TrackedDevice,
    q: &crate::device::TrackedQueue,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &VisShadowInputs<'_>,
    shadow_cpu: &mut ShadowCpuTimings,
) -> Result<bool, RenderError> {
    let VisShadowInputs {
        scene,
        plan,
        shadow_jobs,
        csm_splits,
        vis_set_eye,
        csm_size,
        cascade_count,
        vis_shadows,
    } = *inputs;
    let drawn = if vis_shadows && !shadow_jobs.is_empty() && visbuffer.prepare(d, q)? {
        let _span = tracing::info_span!("vis_shadow_cull").entered();
        let _crumb = crate::breadcrumbs::frame_pass("vis_shadow_cull");
        let _time = AddElapsedMs(&mut shadow_cpu.vis_cull_ms, std::time::Instant::now());
        let res = csm_size as f32;
        let vis_jobs: Vec<crate::visbuffer::VisShadowJob> = shadow_jobs
            .iter()
            .map(|job| {
                if job.kind < 4 {
                    let [x, y, w, h] = job.rect;
                    crate::visbuffer::VisShadowJob {
                        view_proj: plan.csm.cascades[usize::from(job.kind)].light_view_proj,
                        // Texel rows grow downward, NDC y upward.
                        ndc_rect: [
                            2.0 * x as f32 / res - 1.0,
                            1.0 - 2.0 * (y + h) as f32 / res,
                            2.0 * (x + w) as f32 / res - 1.0,
                            1.0 - 2.0 * y as f32 / res,
                        ],
                        resolution: res,
                        cascade: Some(u32::from(job.kind)),
                    }
                } else {
                    let tile = &plan.tiles[job.slot - cascade_count];
                    crate::visbuffer::VisShadowJob {
                        view_proj: tile.gpu.view_proj,
                        ndc_rect: [-1.0, -1.0, 1.0, 1.0],
                        resolution: tile.size as f32,
                        cascade: None,
                    }
                }
            })
            .collect();
        let mut splits = [0.0f32; 4];
        for (dst, src) in splits.iter_mut().zip(csm_splits) {
            *dst = *src;
        }
        visbuffer.record_shadow_cull(
            d,
            q,
            encoder,
            &vis_jobs,
            &crate::visbuffer::VisShadowFrame {
                camera_eye: vis_set_eye.to_array(),
                // Half the cell's diagonal: the farthest the camera is from the cell centre.
                reach_slack: VIS_SET_EYE_SNAP_M * 0.5 * 3f32.sqrt(),
                splits,
                cascade_count: cascade_count as u32,
                double_sided: scene.double_sided,
            },
        )?
    } else {
        false
    };
    Ok(drawn)
}

/// The renderer fields the translucent shadow casters read and write.
pub(super) struct TranslucentShadowFields<'a> {
    pub(super) glass_gpu: &'a mut glass_gpu::GlassGpu,
    pub(super) glass: &'a [crate::glass::GlassInstance],
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) objects: &'a crate::key_hash::KeyMap<ObjectUniformKey, ObjectEntry>,
    pub(super) materials: &'a crate::key_hash::KeyMap<u64, MaterialEntry>,
    pub(super) entries: &'a lifecycle::EntryLifecycle,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) translucent_maps: &'a mut translucent_shadow::TranslucentMaps,
    pub(super) shadow_report: &'a mut ShadowCacheReport,
}

/// What the translucent shadow casters read from the frame.
pub(super) struct TranslucentShadowInputs<'a> {
    /// The frame's scene (double-sided switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame's pipeline sets (translucent shadow pipelines).
    pub(super) pipes: &'a Pipes,
    /// The frame's shadow plan.
    pub(super) plan: &'a ShadowPlan,
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Indices of the translucent casters.
    pub(super) trans_ix: &'a [usize],
    /// Each translucent caster's world bounds and cascade mask.
    pub(super) trans_info: &'a [(Option<crate::paged_shadow::Aabb3>, u8)],
    /// The bindless texture table, when the frame binds one.
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    /// Sun cascades that take translucent casters.
    pub(super) trans_cascades: u32,
    /// The local-light atlas tiles take translucent casters.
    pub(super) trans_local: bool,
    /// Materials bind through the texture table.
    pub(super) table_path: bool,
}

/// Records the translucent casters into the translucency maps (cascades and atlas tiles),
/// inside the shadow bucket, and counts them in the shadow report.
pub(super) fn record_translucent_shadows(
    fields: TranslucentShadowFields<'_>,
    d: &TrackedDevice,
    q: &crate::device::TrackedQueue,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &TranslucentShadowInputs<'_>,
    shadow_cpu: &mut ShadowCpuTimings,
) {
    let TranslucentShadowFields {
        glass_gpu,
        glass,
        meshes,
        objects,
        materials,
        entries,
        belt,
        translucent_maps,
        shadow_report,
    } = fields;
    let TranslucentShadowInputs {
        scene,
        pipes,
        plan,
        draws,
        trans_ix,
        trans_info,
        texture_table_group,
        trans_cascades,
        trans_local,
        table_path,
    } = *inputs;
    if trans_cascades > 0 || trans_local {
        let _time = AddElapsedMs(&mut shadow_cpu.translucent_ms, std::time::Instant::now());
        let trans_vps: Vec<[[f32; 4]; 4]> = plan
            .csm
            .cascades
            .iter()
            .take(trans_cascades as usize)
            .map(|cascade| cascade.light_view_proj)
            .collect();
        let trans_tiles: Vec<translucent_shadow::Tile> = if trans_local {
            // The near sun tile has no translucency square: its receivers take the glass
            // tint from the cascades (`sample_csm_transmittance`).
            plan.tiles
                .iter()
                .take(plan.precip_tile.or(plan.near_tile).unwrap_or(plan.tiles.len()))
                .map(|tile| translucent_shadow::Tile {
                    view_proj: tile.gpu.view_proj,
                    rect: [tile.x, tile.y, tile.size, tile.size],
                })
                .collect()
        } else {
            Vec::new()
        };
        // Damaged and own-canopy glass casts through its crack page (holes cast nothing).
        let glass_groups = if trans_ix.iter().any(|&index| draws[index].glass.is_some()) {
            glass_gpu.caster_groups(d, q, pipes.translucent.glass_bgl(), glass)
        } else {
            Vec::new()
        };
        let casters: Vec<translucent_shadow::Caster<'_>> = trans_ix
            .iter()
            .zip(trans_info)
            .filter_map(|(&index, &(aabb, cascades))| {
                let draw = &draws[index];
                let Some(mesh) = meshes.get(&draw.mesh) else {
                    missing_draw_entry(&entries.skips, "mesh", draw.mesh);
                    return None;
                };
                let Some(object) = objects.get(&draw.uniform_key()) else {
                    missing_draw_entry(&entries.skips, "object", draw.object);
                    return None;
                };
                let Some(material) = materials.get(&draw.material) else {
                    missing_draw_entry(&entries.skips, "material", draw.material);
                    return None;
                };
                Some(translucent_shadow::Caster {
                    vb: &mesh.vb,
                    ib: &mesh.ib,
                    index_count: mesh.index_count,
                    first_index: mesh.first_index,
                    base_vertex: mesh.base_vertex,
                    object: &object.bg,
                    material: &material.bg,
                    material_offsets: material.offsets(),
                    // A graph material on the table path binds its own textures (ADR-863).
                    own_textures: table_path && material.table.is_none(),
                    double_sided: draw.cull_off || scene.double_sided,
                    glass: draw.glass.and_then(|glass| glass_groups.get(glass)),
                    aabb,
                    cascades,
                })
            })
            .collect();
        let recorded = translucent_shadow::record(
            d,
            encoder,
            belt,
            &pipes.translucent,
            translucent_maps,
            &translucent_shadow::Frame {
                cascades: &trans_vps,
                tiles: &trans_tiles,
                atlas_size: plan.atlas_size,
                casters: &casters,
                texture_table: texture_table_group.as_ref(),
            },
        );
        shadow_report.translucent_casters = casters.len() as u32;
        shadow_report.translucent_passes = recorded.passes;
        shadow_report.translucent_draws = recorded.draws;
    }
}

/// How [`draw_instanced_jobs`] draws each instanced batch.
#[derive(Clone, Copy)]
enum BatchDraw<'b> {
    /// `draw_indexed` over the batch's instances (the unculled pass).
    Direct,
    /// One multi-draw over phase 1's commands (`crate::shadow_occlusion`), with the page clears
    /// and the visibility-buffer clusters.
    Phase1(&'b wgpu::Buffer),
    /// One multi-draw over phase 2's commands, in each job's viewport and scissor, without the
    /// page clears (the map is loaded) and without the clusters (phase 1 drew them).
    Phase2(&'b wgpu::Buffer),
}

/// What [`draw_instanced_jobs`] draws: one kind's jobs on the instanced (storage-object) path.
struct InstancedJobs<'a> {
    batch: &'a [(usize, &'a ShadowJob)],
    job_lists: &'a [Vec<crate::gpu_driven::InstanceBatch>],
    meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    materials: &'a crate::key_hash::KeyMap<u64, MaterialEntry>,
    graph_sets: &'a GraphSets,
    pipe: &'a [wgpu::RenderPipeline; 2],
    obj_bg: &'a wgpu::BindGroup,
    mask_pipe: Option<&'a [wgpu::RenderPipeline; 2]>,
    tile_vp_bgs: &'a [wgpu::BindGroup],
    page_clear_pipe: &'a wgpu::RenderPipeline,
    csm_size: u32,
    kind: u8,
    trims: bool,
    vis_shadow_drawn: bool,
}

/// One kind's jobs on the instanced path: per job its page clear, viewport and scissor, then its
/// batches (one draw per mesh per job; `instance_index` reads the object buffer, so 100k casters
/// are not 100k CPU draws), then its visibility-buffer clusters.
fn draw_instanced_jobs(
    pass: &mut wgpu::RenderPass<'_>,
    tally: &mut crate::frame_stats::DrawTally,
    mesh_binds: &mut crate::submission::PassBinds,
    visbuffer: &mut crate::visbuffer::VisBufferPass,
    jobs: &InstancedJobs<'_>,
    mode: BatchDraw<'_>,
) {
    let InstancedJobs {
        batch,
        job_lists,
        meshes,
        materials,
        graph_sets,
        pipe,
        obj_bg,
        mask_pipe,
        tile_vp_bgs,
        page_clear_pipe,
        csm_size,
        kind,
        trims,
        vis_shadow_drawn,
    } = *jobs;
    let phase2 = matches!(mode, BatchDraw::Phase2(_));
    // Vertex and index buffers stay bound across draws and pipelines in
    // a pass: a batch whose arena chunk is already bound does not rebind it
    // (ADR-508; with the trims off, only the same mesh skips).
    let mut bound_mesh: Option<u64> = None;
    for &(job_index, job) in batch {
        let batches: &[crate::gpu_driven::InstanceBatch] =
            job_lists.get(job_index).map_or(&[], Vec::as_slice);
        if job.page_clear && !phase2 {
            tally.set_pipeline(pass, page_clear_pipe);
            pass.set_viewport(
                job.rect[0] as f32,
                job.rect[1] as f32,
                job.rect[2] as f32,
                job.rect[3] as f32,
                0.0,
                1.0,
            );
            pass.set_scissor_rect(job.rect[0], job.rect[1], job.rect[2], job.rect[3]);
            tally.draw(pass, 0..3, 0..1);
        }
        tally.set_pipeline(pass, &pipe[0]);
        tally.set_bind_group(pass, 0, &tile_vp_bgs[job.slot], &[]);
        tally.set_bind_group(pass, 1, obj_bg, &[]);
        if kind == 4 {
            pass.set_viewport(
                job.rect[0] as f32,
                job.rect[1] as f32,
                job.rect[2] as f32,
                job.rect[3] as f32,
                0.0,
                1.0,
            );
            pass.set_scissor_rect(job.rect[0], job.rect[1], job.rect[2], job.rect[3]);
        } else if job.page_clear {
            // A sun page is drawn with the whole cascade's matrix: full
            // viewport, scissored to the page. (A page-sized viewport
            // squeezed the cascade into the page, and a moving caster's
            // shadow vanished once the cascade stayed in place.)
            pass.set_viewport(0.0, 0.0, csm_size as f32, csm_size as f32, 0.0, 1.0);
            pass.set_scissor_rect(job.rect[0], job.rect[1], job.rect[2], job.rect[3]);
        }
        let mut graph_bound = false;
        // Double-sided casters batch after the culled ones (`group_instances`):
        // one switch to the cull-none pipeline per job.
        let mut bound_ds = false;
        for group in batches {
            let Some(mesh) = meshes.get(&group.mesh) else {
                continue;
            };
            // Material 0: the standard caster. Otherwise a vertex-stage graph
            // casting its displaced shadow, or a masked standard caster under
            // alpha-tested shadows (`set_alpha_tested_shadows`).
            let material = (group.material != 0)
                .then(|| materials.get(&group.material))
                .flatten();
            let graph = material.and_then(|material| {
                let set = material
                    .graph_key
                    .and_then(|key| graph_sets.get(&(key, group.double_sided)))?;
                Some((material, set))
            });
            let masked = match (graph, material, mask_pipe) {
                (None, Some(material), Some(mask_pipe)) if material.graph_key.is_none() => {
                    Some((material, &mask_pipe[usize::from(group.double_sided)]))
                }
                _ => None,
            };
            if let Some((material, mask_pipe)) = masked {
                tally.set_pipeline(pass, mask_pipe);
                tally.set_bind_group(pass, 2, &material.bg, material.offsets());
                // The next standard batch rebinds the opaque pipeline.
                graph_bound = true;
            }
            match graph {
                _ if masked.is_some() => {}
                Some((material, set)) => {
                    let Some(graph_pipe) = set.shadow_instanced.as_ref() else {
                        continue;
                    };
                    tally.set_pipeline(pass, graph_pipe);
                    tally.set_bind_group(pass, 2, &material.bg, material.offsets());
                    if set.uv1 {
                        let Some(uv1) = mesh.uv1.as_ref() else {
                            continue;
                        };
                        mesh_binds.vertex(pass, 1, uv1);
                    }
                    graph_bound = true;
                }
                None if graph_bound || group.double_sided != bound_ds => {
                    tally.set_pipeline(pass, &pipe[usize::from(group.double_sided)]);
                    graph_bound = false;
                    bound_ds = group.double_sided;
                }
                None => {}
            }
            if trims || bound_mesh != Some(group.mesh) {
                mesh_binds.mesh(pass, &mesh.vb, &mesh.ib);
                bound_mesh = Some(group.mesh);
            }
            match mode {
                BatchDraw::Direct => tally.draw_indexed(
                    pass,
                    mesh.indices(),
                    mesh.base_vertex,
                    group.start..group.start + group.count,
                ),
                // Command `k` is the instance at slot `k` (the shadow lists start the object
                // buffer), so a batch is the commands `start..start + count`.
                BatchDraw::Phase1(commands) | BatchDraw::Phase2(commands) => tally
                    .multi_draw_indexed_indirect(
                        pass,
                        commands,
                        u64::from(group.start) * crate::shadow_occlusion::CMD_STRIDE,
                        group.count,
                    ),
            }
        }
        if vis_shadow_drawn && !phase2 {
            visbuffer.draw_shadow_job(pass, job_index);
            tally.forget_state();
            mesh_binds.forget();
        }
    }
}
