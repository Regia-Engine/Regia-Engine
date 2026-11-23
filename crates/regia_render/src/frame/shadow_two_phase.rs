// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion of the shadow casters in `FrameRenderer::render_into` (ADR-2072): which
//! shadow-map kinds take it this frame, the per-instance cull list built from each job's instanced
//! shadow batches, phase 1's upload and dispatch, the pyramids' history, and the renderer's switch
//! and reports. The GPU side is `crate::shadow_occlusion`; the passes are
//! `shadow_passes::record_shadow_passes`.

use super::shadow_passes::InstancedPasses;
use super::shadow_plan::{ShadowJob, ShadowPlan, shadow_model};
use super::{Draw, FrameRenderer, MeshEntry, shadow_batches_by_material};
use crate::shadow_occlusion::{
    SHADOW_OCCLUSION_KINDS, ShadowCullEntry, ShadowCullJob, ShadowKindOcclusion, ShadowOcclusion,
    ShadowOcclusionCounters, ShadowOcclusionSkip, ShadowOcclusionStats,
};
use crate::{RenderDevice, RenderError};

/// What the shadow cull reads from the frame.
pub(super) struct ShadowTwoPhaseInputs<'a> {
    pub(super) plan: &'a ShadowPlan,
    pub(super) shadow_jobs: &'a [ShadowJob],
    pub(super) job_casters: &'a [std::sync::Arc<[usize]>],
    pub(super) draws: &'a [Draw],
    pub(super) instanced: &'a Option<InstancedPasses>,
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) csm_views: &'a [wgpu::TextureView],
    pub(super) atlas_view: &'a wgpu::TextureView,
    pub(super) cascade_count: usize,
    pub(super) csm_size: u32,
    pub(super) view_pass: bool,
    pub(super) alpha_tested_shadows: bool,
}

/// The map of `kind` and its edge in texels.
fn kind_map<'a>(inputs: &ShadowTwoPhaseInputs<'a>, kind: usize) -> Option<(&'a wgpu::TextureView, u32)> {
    if kind < 4 {
        inputs.csm_views.get(kind).map(|view| (view, inputs.csm_size))
    } else {
        Some((inputs.atlas_view, inputs.plan.atlas_size))
    }
}

/// The world sphere a caster's shadow draw stays inside, or `None` when it has no safe bound: a
/// vertex-stage graph moves its vertices (bounded only when it declares its largest offset, as a
/// wind sway does, and draws its shadow at its own model: the mesh bound grows by that offset,
/// ADR-5389), a morph follows its weights, a draw that is neither rigid nor palette-bounded (a
/// compute-skinned twin) is unbounded, and a palette sphere does not follow a shadow scale or
/// lift.
fn caster_sphere(draw: &Draw, entry: &MeshEntry) -> Option<([f32; 3], f32)> {
    if (draw.graph_vertex && !draw.vertex_offset_bounded)
        || draw.morphed
        || !(draw.rigid || draw.skin_sphere.is_some())
    {
        return None;
    }
    let model = shadow_model(draw.model, draw.shadow);
    // The declared offset is the vertex stage's at the drawn model; a shadow scale or lift moves
    // the vertices it offsets.
    if draw.graph_vertex && model != draw.model {
        return None;
    }
    let (center, radius) = if let Some(sphere) = draw.skin_sphere {
        if model != draw.model {
            return None;
        }
        sphere
    } else if draw.rigid {
        let bounds = entry.bounds?;
        let m = regia_core::glam::Mat4::from_cols_array_2d(&model);
        let center = m.transform_point3(regia_core::glam::Vec3::from_array(bounds.center));
        let scale = (m
            .x_axis
            .length_squared()
            .max(m.y_axis.length_squared())
            .max(m.z_axis.length_squared()))
        .sqrt();
        (center.to_array(), bounds.radius * scale + draw.inflate)
    } else {
        return None;
    };
    (center.iter().all(|v| v.is_finite()) && radius.is_finite() && radius >= 0.0).then_some((center, radius))
}

/// A frame whose graph does not hold the `shadow_occlusion` node (ADR-5178): no kind culls, and
/// the report says what phase 1 said with the switch off (a texture view leaves it alone).
pub(super) fn without_shadow_phase1(occlusion: &mut ShadowOcclusion, view_pass: bool) {
    if view_pass {
        occlusion.pass_through();
    } else {
        occlusion.skip(ShadowOcclusionSkip::Off);
    }
}

/// Decides which kinds cull their casters in two phases this frame; uploads their cull lists and
/// records each one's phase 1. A texture view never culls and leaves the main view's report alone.
pub(super) fn record_shadow_phase1(
    occlusion: &mut ShadowOcclusion,
    d: &crate::TrackedDevice,
    belt: &mut crate::staging::StagingBelt,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &ShadowTwoPhaseInputs<'_>,
) -> Result<(), RenderError> {
    if inputs.view_pass {
        occlusion.pass_through();
        return Ok(());
    }
    if !occlusion.enabled {
        occlusion.skip(ShadowOcclusionSkip::Off);
        return Ok(());
    }
    // A kind whose map changed or went away (fewer cascades, a new size) drops its pyramid and
    // history, so neither holds an old map alive.
    for kind in 0..SHADOW_OCCLUSION_KINDS {
        let current = kind_map(inputs, kind);
        occlusion.forget_stale(kind, current.map(|(view, _)| view));
    }
    // Checked first: a frame that redraws no map builds no instanced shadow lists either.
    if inputs.shadow_jobs.is_empty() {
        occlusion.skip(ShadowOcclusionSkip::NoJobs);
        return Ok(());
    }
    let Some((_, job_lists, _, Some(_), _, _)) = inputs.instanced.as_ref() else {
        occlusion.skip(ShadowOcclusionSkip::NoGpuObjects);
        return Ok(());
    };
    let _span = tracing::info_span!("shadow_two_phase").entered();
    let _crumb = crate::breadcrumbs::frame_pass("shadow_two_phase");
    // Caster instances per kind, from the jobs' batches.
    let mut per_kind = [0u32; SHADOW_OCCLUSION_KINDS];
    for (job, batches) in inputs.shadow_jobs.iter().zip(job_lists) {
        if let Some(n) = per_kind.get_mut(usize::from(job.kind)) {
            *n += batches.iter().map(|b| b.count).sum::<u32>();
        }
    }
    let min = occlusion.min_casters().max(1);
    let active: [bool; SHADOW_OCCLUSION_KINDS] = std::array::from_fn(|k| per_kind[k] >= min);
    if !active.iter().any(|a| *a) {
        occlusion.skip(ShadowOcclusionSkip::FewCasters);
        return Ok(());
    }
    for (kind, _) in active.iter().enumerate().filter(|(_, a)| **a) {
        if let Some((view, size)) = kind_map(inputs, kind) {
            occlusion.ensure_pyramid(d, kind, view, size)?;
        }
    }
    let mut entries = std::mem::take(&mut occlusion.entries_scratch);
    let mut jobs = std::mem::take(&mut occlusion.jobs_scratch);
    let mut keys = std::mem::take(&mut occlusion.sort_scratch);
    entries.clear();
    jobs.clear();
    jobs.resize(inputs.shadow_jobs.len(), ShadowCullJob::default());
    let mut ranges = [(0u32, 0u32); SHADOW_OCCLUSION_KINDS];
    let mut kinds = [ShadowKindOcclusion::default(); SHADOW_OCCLUSION_KINDS];
    let mut slots = 0u32;
    for kind in 0..SHADOW_OCCLUSION_KINDS {
        if !active[kind] {
            continue;
        }
        let Some((map_view, map_size)) = kind_map(inputs, kind) else {
            continue;
        };
        let offset = entries.len() as u32;
        let stats = &mut kinds[kind];
        stats.active = true;
        for (j, job) in inputs.shadow_jobs.iter().enumerate() {
            if usize::from(job.kind) != kind {
                continue;
            }
            stats.jobs += 1;
            let full = [0, 0, map_size, map_size];
            // A job whose matrix is not in the plan keeps no history: phase 1 draws all of it.
            let resolved = if kind < 4 {
                // A page job draws with the whole cascade's matrix, scissored to its page.
                let scissor = if job.page_clear { job.rect } else { full };
                inputs
                    .plan
                    .csm
                    .cascades
                    .get(kind)
                    .map(|cascade| (cascade.light_view_proj, full, scissor))
            } else {
                inputs
                    .plan
                    .tiles
                    .get(job.slot.wrapping_sub(inputs.cascade_count))
                    .map(|tile| (tile.gpu.view_proj, job.rect, job.rect))
            };
            let (view_proj, viewport, clip) = resolved.unwrap_or(([[0.0; 4]; 4], full, full));
            let prev = resolved.and_then(|_| occlusion.history_for(kind, map_view, map_size, job.rect));
            if prev.is_some() {
                stats.history_jobs += 1;
            }
            let as_f = |r: [u32; 4]| r.map(|v| v as f32);
            if let Some(slot) = jobs.get_mut(j) {
                *slot = ShadowCullJob {
                    view_proj,
                    prev_view_proj: prev.unwrap_or(view_proj),
                    viewport: as_f(viewport),
                    clip: [
                        clip[0] as f32,
                        clip[1] as f32,
                        (clip[0] + clip[2]) as f32,
                        (clip[1] + clip[3]) as f32,
                    ],
                    flags: [u32::from(prev.is_some()), 0, 0, 0],
                };
            }
            // Without its caster list every instance of the job is unbounded (drawn in phase 1):
            // each slot the pass draws from must have its command written.
            let list: &[usize] = inputs.job_casters.get(j).map_or(&[], |l| &l[..]);
            let Some(batches) = job_lists.get(j) else {
                continue;
            };
            let Some(base) = batches.first().map(|b| b.start) else {
                continue;
            };
            // The instance lists group a job's casters by (cull-none, mesh, material or 0,
            // index) (`gpu_driven::group_instances`): the k-th of that order is slot `base + k`.
            keys.clear();
            keys.extend(list.iter().enumerate().filter_map(|(local, &index)| {
                let draw = inputs.draws.get(index)?;
                let by_material = draw.graph_vertex
                    || shadow_batches_by_material(inputs.alpha_tested_shadows, draw.alpha_mode, draw.hair);
                Some((
                    draw.cull_off,
                    draw.mesh,
                    if by_material { draw.material } else { 0 },
                    local as u32,
                ))
            }));
            keys.sort_unstable();
            for batch in batches {
                let Some(mesh) = inputs.meshes.get(&batch.mesh) else {
                    // The pass skips a batch without its mesh.
                    continue;
                };
                for slot in batch.start..batch.start + batch.count {
                    let key = keys.get((slot - base) as usize);
                    // A caster the order does not pin to this batch has no trusted bound.
                    let draw = key
                        .filter(|(cull_off, mesh_key, material, _)| {
                            *cull_off == batch.double_sided
                                && *mesh_key == batch.mesh
                                && *material == batch.material
                        })
                        .and_then(|(_, _, _, local)| list.get(*local as usize))
                        .and_then(|&index| inputs.draws.get(index));
                    let sphere = draw.and_then(|draw| caster_sphere(draw, mesh));
                    let center_radius = match sphere {
                        Some((c, r)) => [c[0], c[1], c[2], r],
                        None => {
                            stats.unbounded += 1;
                            [0.0, 0.0, 0.0, -1.0]
                        }
                    };
                    entries.push(ShadowCullEntry {
                        center_radius,
                        index_count: mesh.index_count,
                        first_index: mesh.first_index,
                        // The command's `base_vertex` is an `i32`; the shader copies its bits.
                        base_vertex: mesh.base_vertex as u32,
                        job: j as u32,
                        slot,
                        pad: [0; 3],
                    });
                    stats.instances += 1;
                    slots = slots.max(slot + 1);
                }
            }
        }
        ranges[kind] = (offset, entries.len() as u32 - offset);
    }
    occlusion.entries_scratch = entries;
    occlusion.jobs_scratch = jobs;
    occlusion.sort_scratch = keys;
    {
        let _upload = tracing::info_span!("shadow_occlusion_upload").entered();
        occlusion.upload(d, belt, encoder, ranges, slots)?;
    }
    for kind in 0..SHADOW_OCCLUSION_KINDS {
        if occlusion.kind_active(kind) {
            occlusion.record_phase1(encoder, kind);
        }
    }
    let frames = occlusion.stats.frames + 1;
    occlusion.stats = ShadowOcclusionStats {
        enabled: true,
        active: true,
        skipped: None,
        kinds,
        min_casters: occlusion.min_casters(),
        pyramid_bytes: occlusion.held_pyramid_bytes(),
        frames,
    };
    Ok(())
}

/// After the shadow passes: each culled kind's pyramid now holds this frame's map (as phase 1
/// left it) drawn with this frame's matrices, which next frame's phase 1 tests.
pub(super) fn note_shadow_history(occlusion: &mut ShadowOcclusion, inputs: &ShadowTwoPhaseInputs<'_>) {
    if inputs.view_pass {
        return;
    }
    for kind in 0..SHADOW_OCCLUSION_KINDS {
        if !occlusion.kind_active(kind) {
            continue;
        }
        let Some((view, size)) = kind_map(inputs, kind) else {
            continue;
        };
        if kind < 4 {
            let Some(cascade) = inputs.plan.csm.cascades.get(kind) else {
                continue;
            };
            occlusion.note_history(
                kind,
                view,
                size,
                std::iter::once(([0, 0, size, size], cascade.light_view_proj)),
            );
        } else {
            occlusion.note_history(
                kind,
                view,
                size,
                inputs
                    .plan
                    .tiles
                    .iter()
                    .map(|tile| ([tile.x, tile.y, tile.size, tile.size], tile.gpu.view_proj)),
            );
        }
    }
}

impl FrameRenderer {
    /// Enters the `shadows` record site's nodes through the graph's dispatch. `shadow_occlusion`
    /// is active only when phase 1 can cull this frame: a main view, with shadow jobs, whose
    /// shadow lists are GPU-driven (ADR-5626), as `prepass_occlusion` is active only on a
    /// GPU-culled frame. Returns whether to call `record_shadow_phase1` (see
    /// `FrameDispatch::enter_shadows`).
    pub(super) fn enter_shadow_site(
        &mut self,
        view_pass: bool,
        shadow_jobs: &[ShadowJob],
        instanced: &Option<InstancedPasses>,
    ) -> bool {
        let gpu_objects = matches!(instanced, Some((_, _, _, Some(_), _, _)));
        let active = self.shadow_occlusion.enabled && !view_pass && !shadow_jobs.is_empty() && gpu_objects;
        self.extension_passes.dispatch.enter_shadows(active)
    }

    /// Culls the GPU-driven shadow casters in two phases (ADR-2072,
    /// `docs/specs/98-visibility-buffer.md` § 10a): per redrawn sun cascade and for the
    /// local-light atlas, casters the kind's last pyramid hides are skipped, the pyramid is
    /// rebuilt from the rest, and what it does not hide draws in a second pass. Every shadow map
    /// texel is the unculled pass's. Runs on the main view, for each kind whose jobs hold at least
    /// [`Self::set_shadow_occlusion_min_casters`] caster instances; [`Self::shadow_occlusion_stats`]
    /// says why a frame did not. Off by default; turning it off frees its pyramids. No settings
    /// field or preset turns it on: a host calls this, or the agent's `render.set_shadow_occlusion`.
    ///
    /// Phase 1 is the frame graph's `shadow_occlusion` node (ADR-5178): the graph holds it only
    /// while this is on (`render.graph` lists it then, right before `shadows`), and the frame
    /// records it through the graph's dispatch, so a frame with it off records and creates nothing
    /// for it.
    pub fn set_shadow_occlusion(&mut self, enabled: bool) {
        self.shadow_occlusion.set_enabled(enabled);
        self.extension_passes
            .set_graph_options(super::frame_graph::FrameGraphOptions {
                shadow_occlusion: enabled,
                ..self.extension_passes.graph_options()
            });
    }

    /// Whether the shadow casters may cull in two phases ([`Self::set_shadow_occlusion`]).
    #[must_use]
    pub fn shadow_occlusion(&self) -> bool {
        self.shadow_occlusion.enabled
    }

    /// Fewest caster instances in one kind's jobs (a cascade's, or the atlas tiles') for that kind
    /// to cull in two phases; below it the pyramid would cost more than the draws it saves.
    /// `None` restores the default ([`crate::SHADOW_OCCLUSION_MIN_CASTERS`]).
    pub fn set_shadow_occlusion_min_casters(&mut self, min_casters: Option<u32>) {
        self.shadow_occlusion.min_casters = min_casters;
        self.shadow_occlusion.stats.min_casters = self.shadow_occlusion.min_casters();
    }

    /// What the shadow casters' two-phase cull did in the last main-view frame (agent
    /// `render.get_occlusion` `shadows`, `book/agents/occlusion.md`).
    #[must_use]
    pub fn shadow_occlusion_stats(&self) -> ShadowOcclusionStats {
        let mut stats = self.shadow_occlusion.stats;
        stats.enabled = self.shadow_occlusion.enabled;
        stats.min_casters = self.shadow_occlusion.min_casters();
        stats
    }

    /// Reads back how many caster instances each phase of the last frame's shadow cull drew, per
    /// kind. Blocks on the GPU: tests, benches and the agent's `render.get_occlusion` (which calls
    /// it only when the last frame culled). `Ok(None)` before the first culled frame.
    ///
    /// # Errors
    ///
    /// The readback failed.
    pub fn shadow_occlusion_counters(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<ShadowOcclusionCounters>, RenderError> {
        self.shadow_occlusion.read_counters(device)
    }

    /// The depth of the main view's shadow map `kind` (cascades 0..4, 4 = the local-light atlas),
    /// row by row. Blocks on the GPU: tests and benches only (the culling differential test
    /// compares the maps texel by texel). `Ok(None)` before the first frame with that map.
    ///
    /// # Errors
    ///
    /// The readback failed.
    pub fn shadow_map_depth(
        &self,
        device: &RenderDevice,
        kind: usize,
    ) -> Result<Option<Vec<f32>>, RenderError> {
        let map = if kind < 4 {
            self.csm
                .as_ref()
                .filter(|(_, active, _)| kind < *active as usize)
                .and_then(|(size, _, views)| Some((views.get(kind)?.clone(), *size)))
        } else {
            self.atlas.as_ref().map(|(size, view)| (view.clone(), *size))
        };
        let Some((view, size)) = map else {
            return Ok(None);
        };
        crate::shadow_occlusion::read_depth_map(device, &view, size).map(Some)
    }
}
