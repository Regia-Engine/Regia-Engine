// SPDX-License-Identifier: MPL-2.0
//! The outdoor passes of `FrameRenderer::render_into` (ADR-1552 split by pass): the physical
//! sky and aerial perspective, the volumetric clouds and the froxel fog (or valley mist),
//! composited over the shaded HDR image. Moved out of `frame.rs` unchanged. Nothing else of the
//! renderer is borrowed here, so this is a `&mut self` method.

use regia_core::glam::Mat4;

use super::{
    FrameRenderer, FrameScene, OUTDOOR_QUERY_BASE, fog_cascade_index, fog_light_shadows, outdoor_query_slot,
    shadow_plan::ShadowPlan,
};
use crate::device::TrackedQueue;
use crate::{DirectionalLightUniform, RenderDevice, RenderError, TrackedDevice};

/// What the outdoor passes read from the frame.
pub(super) struct OutdoorInputs<'a> {
    /// The frame's scene (eye, planes, fog and cloud quality, shadow switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// The frame's timestamp query set, when GPU timing is on.
    pub(super) timer: &'a Option<(super::FrameStamps, f32)>,
    /// The frame's shadow plan (cascade matrices, local atlas tiles).
    pub(super) plan: &'a ShadowPlan,
    /// The sun cascade views.
    pub(super) csm_views: &'a [wgpu::TextureView],
    /// The local lights uploaded this frame.
    pub(super) uploaded: &'a [crate::LabLocalLight],
    /// The frame's sun.
    pub(super) directional: &'a DirectionalLightUniform,
    /// The jittered projection times the view.
    pub(super) view_proj: Mat4,
    /// The projection without TAAU jitter times the view.
    pub(super) unjittered_vp: Mat4,
    /// Last frame's unjittered view-projection (cloud reprojection).
    pub(super) prev_vp: [[f32; 4]; 4],
    /// The sun cascade resolution.
    pub(super) csm_size: u32,
    /// Attachment size.
    pub(super) attach_w: u32,
    /// Attachment size.
    pub(super) attach_h: u32,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// The HDR colour format.
    pub(super) color_format: wgpu::TextureFormat,
    /// Pre-exposure of the HDR image.
    pub(super) pre_exposure: f32,
    /// The scene renders to a linear HDR target.
    pub(super) linear_hdr: bool,
    /// The frame draws the physical sky.
    pub(super) planet_sky: bool,
    /// The atmosphere prepared before the forward pass, whose sky background is already drawn.
    pub(super) prepared_sky: Option<&'a crate::outdoor::SkyPass>,
    /// The TAAU resolve shows its last output: nothing here runs.
    pub(super) taau_frozen: bool,
    /// The frame has a static-screen cache key (the composite is copied before blending).
    pub(super) screen_keyed: bool,
}

impl FrameRenderer {
    /// Prepares the atmosphere shared by the early background and late aerial draws.
    pub(super) fn prepare_frame_sky(
        &mut self,
        device: &RenderDevice,
        queue: &TrackedQueue,
        scene: &FrameScene<'_>,
        modes: &super::steps::modes::FrameModes,
        depth: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) -> Result<Option<crate::outdoor::SkyPass>, RenderError> {
        if !modes.sky_pass {
            return Ok(None);
        }
        self.outdoor
            .ensure_gpu(&device.device, device.shared_pipelines())?;
        Ok(self.outdoor.prepare_sky(
            &device.device,
            queue,
            &crate::outdoor::SkyInputs {
                depth,
                width: depth.texture().width(),
                height: depth.texture().height(),
                format,
                inv_view_proj: modes.view_proj.inverse().to_cols_array_2d(),
                eye: scene.eye,
                far: scene.far,
                pre_exposure: modes.pre_exposure,
                clouds: scene.quality.volumetric_clouds && self.outdoor.wants_clouds(),
            },
        ))
    }

    /// Records the outdoor composite over `shaded` (stamps `OUTDOOR_QUERY_BASE..+6`; passes that
    /// did not run write back-to-back stamps) and returns the image the resolve reads: `shaded`,
    /// or its scratch copy when the static-screen cache holds the original.
    pub(super) fn record_outdoor(
        &mut self,
        device: &RenderDevice,
        d: &TrackedDevice,
        q: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        mut shaded: Option<wgpu::TextureView>,
        inputs: &OutdoorInputs<'_>,
    ) -> Result<Option<wgpu::TextureView>, RenderError> {
        let OutdoorInputs {
            scene,
            depth_view,
            timer,
            plan,
            csm_views,
            uploaded,
            directional,
            view_proj,
            unjittered_vp,
            prev_vp,
            csm_size,
            attach_w,
            attach_h,
            samples,
            color_format,
            pre_exposure,
            linear_hdr,
            planet_sky,
            prepared_sky,
            taau_frozen,
            screen_keyed,
        } = *inputs;
        let mut outdoor_written = 0u32;
        let outdoor_hdr = samples == 1 && linear_hdr;
        let run_atmo = outdoor_hdr && planet_sky;
        let run_clouds = outdoor_hdr && scene.quality.volumetric_clouds && self.outdoor.wants_clouds();
        // The valley mist picks its tier from the profile and may run the fog pass on a profile
        // without volumetric fog (ADR-1394).
        let mist_only = self.outdoor.plan_mist(scene.quality.volumetric_fog);
        let run_fog = outdoor_hdr && (scene.quality.volumetric_fog || mist_only) && self.outdoor.wants_fog();
        if outdoor_hdr {
            let target = (
                color_format,
                scene.quality.volumetric_fog,
                scene.quality.volumetric_clouds,
            );
            if !self.outdoor_targets.contains(&target) {
                self.outdoor_targets.push(target);
            }
        }
        crate::pipeline_warm::warm_checkpoint()?;
        // The static-screen cache holds the SSGI/SSR composite, which is `shaded`, including
        // the sky background. Aerial perspective, clouds and froxel fog blend into a copy, or a held
        // camera blended them again onto last frame's every frame (the haze over a held
        // canyon wall brightened 126 → 191 levels in 40 frames, then dropped back the
        // frame the camera moved: motion-look lane).
        if (run_atmo || run_clouds || run_fog)
            && !taau_frozen
            && screen_keyed
            && let Some(cached) = shaded.clone()
            && cached.texture().usage().contains(wgpu::TextureUsages::COPY_SRC)
        {
            let source = cached.texture().clone();
            let scratch = self.hdr_scratch(
                super::hdr_scratch::HdrScratch::Outdoor,
                d,
                source.size().width,
                source.size().height,
                source.format(),
            );
            encoder.copy_texture_to_texture(
                source.as_image_copy(),
                scratch.texture.as_image_copy(),
                source.size(),
            );
            shaded = Some(scratch.view);
        }
        self.fog_light_shadowed = [false; 4];
        if (run_atmo || run_clouds || run_fog)
            && !taau_frozen
            && let Some(hdr) = shaded.as_ref()
        {
            // Fog samples one cascade's depth with the same light matrix the shadow pass
            // just drew: the first that reaches FOG_SHADOW_RANGE (cascade 0 is a few tens
            // of metres on a long view range, too short for light shafts).
            let fog_cascade = fog_cascade_index(&plan.csm, csm_views.len());
            let shadow_view = if scene.directional_shadows {
                csm_views.get(fog_cascade)
            } else {
                None
            };
            // Any cascade a later cut may pick: its fog groups are built ahead (ADR-2303).
            self.outdoor
                .set_fog_shadow_views(if scene.directional_shadows { csm_views } else { &[] });
            // Shadowed spot shafts: the picked fog spots that cast into the local atlas.
            let fog_light_shadows = fog_light_shadows(&self.outdoor.params().lights, uploaded, plan);
            let local_atlas = self.atlas.as_ref().map(|(_, view)| view.clone());
            let fog_shadowed = run_fog && fog_light_shadows.iter().any(|tile| tile[5][3] > 0.5);
            self.fog_light_shadowed =
                std::array::from_fn(|slot| fog_shadowed && fog_light_shadows[slot][5][3] > 0.5);
            let composite = crate::outdoor::OutdoorComposite {
                prepared_sky,
                depth_is_scene: true,
                pre_exposure,
                hdr,
                depth: depth_view,
                shadow: shadow_view,
                width: attach_w,
                height: attach_h,
                format: color_format,
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                unjittered_inv_view_proj: unjittered_vp.inverse().to_cols_array_2d(),
                prev_view_proj: prev_vp,
                eye: scene.eye,
                near: scene.near,
                far: scene.far,
                sun_travel: directional.direction,
                sun_color: directional.color,
                sun_illuminance: directional.illuminance,
                light_vp: plan.csm.cascades[fog_cascade].light_view_proj,
                shadow_size: if scene.directional_shadows {
                    csm_size as f32
                } else {
                    0.0
                },
                atmosphere: run_atmo,
                fog: run_fog,
                clouds: run_clouds,
                unjittered_view_proj: unjittered_vp.to_cols_array_2d(),
                cloud_reprojection: scene.quality.cloud_reprojection,
                cloud_step_percent: scene.quality.cloud_step_percent,
                cloud_cone_light: scene.quality.cloud_cone_light,
                cloud_march: scene.quality.cloud_march,
                local_shadow: local_atlas.as_ref().filter(|_| fog_shadowed),
                fog_light_shadows,
            };
            let set = timer.as_ref().map(|(set, _)| set.clone());
            // Spec 54: the composite's begin/end stamps bracket each outdoor pass's breadcrumb.
            let mut crumb = None;
            self.outdoor.composite(
                d,
                q,
                device.shared_pipelines(),
                encoder,
                &composite,
                |encoder, name, end| {
                    drop(crumb.take());
                    if !end {
                        let _ = crumb.replace(crate::breadcrumbs::frame_pass(name));
                    }
                    if let Some(set) = &set {
                        let slot = outdoor_query_slot(name) + u32::from(end);
                        set.write(encoder, slot);
                        outdoor_written |= 1 << (slot - OUTDOOR_QUERY_BASE);
                    }
                },
            )?;
        }
        // Passes that did not run write back-to-back stamps (0 ms) so the
        // fixed resolve range never reads an unwritten query.
        if let Some((set_ts, _)) = timer {
            for slot in OUTDOOR_QUERY_BASE..OUTDOOR_QUERY_BASE + 6 {
                if outdoor_written & (1 << (slot - OUTDOOR_QUERY_BASE)) == 0 {
                    set_ts.write(encoder, slot);
                }
            }
            set_ts.write(encoder, OUTDOOR_QUERY_BASE + 6);
        }
        Ok(shaded)
    }
}
