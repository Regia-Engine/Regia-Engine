// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion of the depth prepass in `FrameRenderer::render_into`
//! (`docs/specs/98-visibility-buffer.md`, ADR-1887): which frames take it, the per-instance cull
//! list built from the instanced depth batches, phase 1's upload and dispatch, and the
//! renderer's switch and reports. The GPU side is `crate::prepass_occlusion`.

use regia_core::glam::{Mat4, Vec3};

use super::shadow_passes::InstancedPasses;
use super::{Draw, FrameRenderer, FrameScene, Frustum, HIZ_MIN_RIGID_CASTERS, MeshEntry, gpu_cull};
use crate::prepass_occlusion::{OcclusionInstance, Phase1View, PrepassOcclusion};
use crate::{
    PrepassOcclusionCounters, PrepassOcclusionSkip, PrepassOcclusionStats, RenderDevice, RenderError,
};

/// The renderer fields phase 1 reads and writes.
pub(super) struct TwoPhaseFields<'a> {
    pub(super) occlusion: &'a mut PrepassOcclusion,
    pub(super) culler: &'a mut crate::gpu_driven::GpuCuller,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) hiz_occlusion_off: bool,
}

/// What phase 1 reads from the frame.
pub(super) struct TwoPhaseInputs<'a> {
    pub(super) scene: &'a FrameScene<'a>,
    pub(super) draws: &'a [Draw],
    pub(super) opaque_ix: &'a [usize],
    pub(super) instanced: &'a Option<InstancedPasses>,
    pub(super) depth_view: &'a wgpu::TextureView,
    pub(super) attach_w: u32,
    pub(super) attach_h: u32,
    pub(super) unjittered_vp: Mat4,
    pub(super) frustum: &'a Frustum,
    pub(super) view_pass: bool,
    pub(super) samples: u32,
}

/// A frame whose prepass culls in two phases.
#[derive(Clone, Copy, Debug)]
pub(super) struct TwoPhasePlan {
    /// Slot of the first depth instance in the instance object buffer.
    pub(super) depth_base: u32,
    /// Phase 1 tested last frame's pyramid, so phase 2 must run.
    pub(super) phase2: bool,
}

/// Decides whether this frame's prepass culls in two phases; if so, uploads the cull list and
/// records phase 1. A texture view never takes it and leaves the main view's report alone.
pub(super) fn record_phase1(
    fields: TwoPhaseFields<'_>,
    d: &crate::TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &TwoPhaseInputs<'_>,
) -> Result<Option<TwoPhasePlan>, RenderError> {
    let TwoPhaseInputs {
        scene,
        draws,
        opaque_ix,
        instanced,
        depth_view,
        attach_w,
        attach_h,
        unjittered_vp,
        frustum,
        view_pass,
        samples,
    } = *inputs;
    if view_pass {
        return Ok(None);
    }
    let occlusion = fields.occlusion;
    let skip = if !occlusion.enabled {
        Some(PrepassOcclusionSkip::Off)
    } else if !scene.depth_prepass {
        Some(PrepassOcclusionSkip::NoPrepass)
    } else if samples != 1 {
        Some(PrepassOcclusionSkip::Multisampled)
    } else if fields.hiz_occlusion_off {
        Some(PrepassOcclusionSkip::HizOff)
    } else {
        None
    };
    if let Some(why) = skip {
        occlusion.skip(why);
        return Ok(None);
    }
    let Some((_, _, depth_batches, _, Some(_), _)) = instanced.as_ref() else {
        occlusion.skip(PrepassOcclusionSkip::NoGpuObjects);
        return Ok(None);
    };
    let order = fields.culler.depth_order();
    if order.len() < HIZ_MIN_RIGID_CASTERS || depth_batches.is_empty() {
        occlusion.skip(PrepassOcclusionSkip::FewInstances);
        // A level that reserved a dense view ([`FrameRenderer::prewarm`]) gets the cull's
        // pipelines, buffers and bind group from warm-up on, as the colour cull's pyramid does.
        let reserved = fields.culler.reserved();
        if reserved as usize >= HIZ_MIN_RIGID_CASTERS {
            fields.culler.prepare_hiz(d, depth_view, attach_w, attach_h)?;
            if let Some(hiz) = fields.culler.hiz_view().cloned() {
                occlusion.prepare(d, reserved, &hiz)?;
            }
        }
        return Ok(None);
    }
    let _span = tracing::info_span!("prepass_two_phase").entered();
    let _crumb = crate::breadcrumbs::frame_pass("prepass_two_phase");
    let depth_base = depth_batches.iter().map(|b| b.start).min().unwrap_or(0);
    // One entry per depth instance, in object order (the batches are contiguous from
    // `depth_base`), so entry `k` is the instance at slot `depth_base + k`.
    let mut instances = std::mem::take(&mut occlusion.scratch);
    instances.clear();
    instances.resize(order.len(), OcclusionInstance::default());
    // Per entry: the object key and whether the bound is in the frustum, for the occlusion
    // readback (ADR-7255). Filled only while a feedback handle is held.
    let want_keys = occlusion.feedback.is_some();
    let mut keys = std::mem::take(&mut occlusion.keys);
    keys.clear();
    if want_keys {
        keys.resize(order.len(), (0, false));
    }
    let mut unbounded = 0u32;
    for batch in depth_batches {
        let mesh = fields.meshes.get(&batch.mesh);
        for slot in batch.start..batch.start + batch.count {
            let local = (slot - depth_base) as usize;
            let (Some(entry), Some(src), Some(out)) = (mesh, order.get(local), instances.get_mut(local))
            else {
                continue;
            };
            let Some(draw) = opaque_ix.get(*src as usize).and_then(|&ix| draws.get(ix)) else {
                continue;
            };
            // A vertex-stage graph moves its vertices past the mesh bound: it is bounded only
            // when it declares its largest offset (a wind sway; `draw_sphere` grows the bound by
            // it, as the colour cull does, ADR-5389). Such a graph without a declared offset, and
            // a draw that is neither rigid nor palette-bounded (a compute-skinned twin), have no
            // safe bound: both always draw in phase 1.
            let sphere = if (draw.graph_vertex && !draw.vertex_offset_bounded)
                || !(draw.rigid || draw.skin_sphere.is_some())
            {
                None
            } else {
                gpu_cull::draw_sphere(draw, entry)
            };
            if let Some(entry) = keys.get_mut(local) {
                *entry = (
                    draw.object,
                    sphere.is_some_and(|(center, radius)| frustum.intersects_sphere(center, radius.max(0.0))),
                );
            }
            let center_radius = match sphere {
                Some((center, radius)) => [center[0], center[1], center[2], radius.max(0.0)],
                None => {
                    unbounded += 1;
                    [0.0, 0.0, 0.0, -1.0]
                }
            };
            *out = OcclusionInstance {
                center_radius,
                index_count: entry.index_count,
                first_index: entry.first_index,
                // The command's `base_vertex` is an `i32`; the shader copies its bits.
                base_vertex: entry.base_vertex as u32,
                first_instance: slot,
            };
        }
    }
    fields.culler.prepare_hiz(d, depth_view, attach_w, attach_h)?;
    let Some(hiz) = fields.culler.hiz_view().cloned() else {
        occlusion.skip(PrepassOcclusionSkip::NoGpuObjects);
        return Ok(None);
    };
    let eye = Vec3::from_array(scene.eye);
    let forward = (Vec3::from_array(scene.target) - eye).normalize_or_zero();
    let phase2 = {
        let mut sink = crate::staging::LiveBelt {
            belt: fields.belt,
            device: d,
            encoder,
        };
        occlusion.upload(
            d,
            &mut sink,
            &instances,
            fields.culler.reserved(),
            &Phase1View {
                view_proj: unjittered_vp.to_cols_array_2d(),
                planes: frustum.planes(),
                forward: forward.to_array(),
                width: attach_w,
                height: attach_h,
                hiz: &hiz,
            },
        )?
    };
    occlusion.record_phase1(encoder);
    occlusion.stats = PrepassOcclusionStats {
        enabled: true,
        active: true,
        skipped: None,
        history: phase2,
        instances: instances.len() as u32,
        unbounded,
        batches: depth_batches.len() as u32,
        frames: occlusion.stats.frames + 1,
    };
    occlusion.scratch = instances;
    occlusion.keys = keys;
    Ok(Some(TwoPhasePlan { depth_base, phase2 }))
}

impl FrameRenderer {
    /// Culls the forward depth prepass in two phases (`docs/specs/98-visibility-buffer.md`,
    /// ADR-1887): instances last frame's depth pyramid hides are skipped, the pyramid is rebuilt
    /// from the rest, and what that pyramid does not hide draws in a second depth pass. The depth
    /// written is the unculled prepass's; the colour pass is unchanged. It runs on the main view
    /// of a single-sample frame with a GPU-driven (storage-object) prepass and at least 2048
    /// depth instances, with Hi-Z occlusion on ([`Self::set_hiz_occlusion`]);
    /// [`Self::prepass_occlusion_stats`] says why a frame did not. Off by default; the runtime
    /// sets it from `RenderProfile::two_phase_occlusion` or the player's override
    /// (`RenderSettings::effective_two_phase_occlusion`).
    ///
    /// Phase 1 is the frame graph's `prepass_occlusion` node (ADR-4905): the graph holds it only
    /// while this is on (`render.graph` lists it then), and the frame records it through the
    /// graph's dispatch, so a frame with it off records and creates nothing for it.
    pub fn set_two_phase_occlusion(&mut self, enabled: bool) {
        self.prepass_occlusion.enabled = enabled;
        self.prepass_occlusion.stats.enabled = enabled;
        self.extension_passes
            .set_graph_options(super::frame_graph::FrameGraphOptions {
                two_phase_occlusion: enabled,
                ..self.extension_passes.graph_options()
            });
    }

    /// Whether the depth prepass may cull in two phases ([`Self::set_two_phase_occlusion`]).
    #[must_use]
    pub fn two_phase_occlusion(&self) -> bool {
        self.prepass_occlusion.enabled
    }

    /// What the prepass's two-phase cull did in the last main-view frame (agent
    /// `render.get_occlusion`, `book/agents/occlusion.md`).
    #[must_use]
    pub fn prepass_occlusion_stats(&self) -> PrepassOcclusionStats {
        self.prepass_occlusion.stats
    }

    /// Hands the renderer an [`crate::OcclusionFeedback`] (or takes it away): each main-view
    /// frame (of the view [`Self::set_occlusion_view`] names) that culls its prepass in two phases ([`Self::set_two_phase_occlusion`]) copies the
    /// per-instance result into it and maps it after the submit, without waiting; a main-view
    /// frame that does not clears its result. [`crate::update_view_visibility`] folds it into
    /// `ViewVisibility` one frame later (ADR-7255). The runtime connects the world's handle
    /// ([`crate::occlusion_feedback::connect_occlusion_feedback`]); a host that renders by hand
    /// passes the handle from its world.
    pub fn set_occlusion_feedback(&mut self, feedback: Option<crate::OcclusionFeedback>) {
        self.prepass_occlusion.feedback = feedback;
    }

    /// The occlusion readback handle this renderer writes ([`Self::set_occlusion_feedback`]).
    #[must_use]
    pub fn occlusion_feedback(&self) -> Option<&crate::OcclusionFeedback> {
        self.prepass_occlusion.feedback.as_ref()
    }

    /// Names the view the next main-view frames draw, by its index in
    /// [`crate::ExtractedFrame::views`] (ADR-7286): each view the renderer culls in two phases
    /// gets its own readback ring in the [`crate::OcclusionFeedback`], and `ViewVisibility` asks
    /// each view's result about that view only. A host that draws several window views
    /// (split screen) sets it before each one, as the runtime's window and screenshot paths do;
    /// a host that draws one view leaves it at 0. Sticky until set again. Texture views never
    /// cull in two phases and ignore it.
    ///
    /// Each window view also keeps its own occlusion history (ADR-7329): its Hi-Z pyramid (the
    /// two-phase prepass's phase 1 tests the view's own last pyramid with the view's own
    /// matrix) and its visibility-buffer cluster bits (phase 1 draws what the view saw last
    /// frame). The pyramid is made the first time a view draws and kept while other views draw;
    /// [`Self::retain_occlusion_views`] frees the pyramids of views that went away, and
    /// [`Self::occlusion_pyramids_into`] lists them.
    pub fn set_occlusion_view(&mut self, view: usize) {
        self.prepass_occlusion.feedback_view = u32::try_from(view).unwrap_or(u32::MAX);
    }

    /// The view the next main-view frames speak for ([`Self::set_occlusion_view`]).
    #[must_use]
    pub fn occlusion_view(&self) -> usize {
        self.prepass_occlusion.feedback_view as usize
    }

    /// Frees the occlusion history (Hi-Z pyramid, visibility-buffer cluster bits) of window
    /// views at index `views` and above: the frame has fewer views now (ADR-7329). The runtime's
    /// window and screenshot paths call it with `ExtractedFrame::views.len()` before they draw
    /// the views; a host that draws several window views by hand does the same. The views' pyramid
    /// reports in the [`crate::OcclusionFeedback`] go too. Allocates nothing.
    pub fn retain_occlusion_views(&mut self, views: usize) {
        self.prepass_occlusion.retain_views(views, &mut self.culler);
        self.visbuffer.retain_bits_views(views);
    }

    /// Window views whose occlusion history the renderer freed because they went undrawn for
    /// more than [`crate::OCCLUSION_VIEW_RETIRE_ROUNDS`] rounds, since it was built (ADR-8620).
    #[must_use]
    pub fn retired_occlusion_views(&self) -> u64 {
        self.prepass_occlusion.retired_views()
    }

    /// Notes the window view about to draw and frees the occlusion history (pyramid, cluster
    /// bits, LOD history) of every view that went undrawn for more than
    /// [`crate::OCCLUSION_VIEW_RETIRE_ROUNDS`] rounds (ADR-8620). Allocates nothing once each
    /// view index has been seen.
    pub(super) fn retire_idle_occlusion_views(&mut self) {
        let mut retire = self.prepass_occlusion.tick_views();
        while retire != 0 {
            let view = retire.trailing_zeros();
            retire &= retire - 1;
            self.prepass_occlusion.retire_view(view, &mut self.culler);
            self.visbuffer.retire_view(view);
        }
    }

    /// Each window view's Hi-Z pyramid the renderer keeps for its occlusion history, ascending by
    /// view, into `out` (its capacity reused; ADR-7329). One view has one pyramid; split screen
    /// has one per view.
    pub fn occlusion_pyramids_into(&self, out: &mut Vec<crate::OcclusionViewPyramid>) {
        self.prepass_occlusion.pyramids_into(&self.culler, out);
    }

    /// Copies this main-view frame's two-phase result into the occlusion readback, or clears
    /// the readback's result when the frame did not cull in two phases (ADR-7255).
    pub(super) fn record_occlusion_feedback(
        &self,
        device: &crate::TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        culled: bool,
    ) {
        let occlusion = &self.prepass_occlusion;
        let Some(feedback) = occlusion.feedback.as_ref() else {
            return;
        };
        let view = occlusion.feedback_view;
        if !culled {
            feedback.note_no_cull(view);
            return;
        }
        let Some(phase1) = occlusion.phase1_commands() else {
            feedback.note_no_cull(view);
            return;
        };
        let phase2 = occlusion.phase2_commands().filter(|_| occlusion.ran_phase2());
        let _span = tracing::info_span!("occlusion_feedback_copy").entered();
        feedback.record(device, encoder, view, (phase1, phase2), &occlusion.keys);
    }

    /// Copies this main-view frame's cluster visibility bits into the occlusion readback, or
    /// clears the readback's cluster result when the frame drew no clusters (ADR-7286).
    pub(super) fn record_cluster_feedback(
        &mut self,
        device: &crate::TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        drawn: bool,
    ) {
        let occlusion = &mut self.prepass_occlusion;
        let Some(feedback) = occlusion.feedback.as_ref() else {
            return;
        };
        let view = occlusion.feedback_view;
        let bits = drawn.then(|| self.visbuffer.visibility_bits()).flatten();
        let Some(bits) = bits else {
            feedback.note_no_clusters(view);
            return;
        };
        let _span = tracing::info_span!("occlusion_feedback_clusters").entered();
        self.visbuffer.occlusion_ranges(&mut occlusion.cluster_ranges);
        feedback.record_clusters(device, encoder, view, bits, &occlusion.cluster_ranges);
    }

    /// Maps the occlusion readback slot this frame copied into (after the submit).
    pub(in crate::frame) fn map_occlusion_feedback(&self) {
        if let Some(feedback) = self.prepass_occlusion.feedback.as_ref() {
            feedback.map_after_submit();
        }
    }

    /// Reads back how many depth instances each phase of the last two-phase frame drew. Blocks
    /// on the GPU: tests and benches only. `Ok(None)` before the first two-phase frame.
    ///
    /// # Errors
    ///
    /// The readback failed.
    pub fn prepass_occlusion_counters(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<PrepassOcclusionCounters>, RenderError> {
        self.prepass_occlusion.read_counters(device)
    }
}
