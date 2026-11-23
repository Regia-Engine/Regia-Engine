// SPDX-License-Identifier: MPL-2.0
//! The display-referred passes at the end of `FrameRenderer::render_into` (ADR-1552 split by
//! pass): the translucent layers drawn over the tonemapped image, the scalar overlays and the
//! gizmos. Moved out of `frame.rs` unchanged; their inputs are named in [`LayerInputs`] and
//! [`OverlayInputs`].

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use regia_core::glam::Mat4;

use super::{FrameRenderer, FrameScene, FrameTarget, begin_load_pass};
use crate::device::TrackedQueue;
use crate::{RenderError, TrackedDevice};

/// What the display-referred translucent layers read.
pub(super) struct LayerInputs<'a> {
    /// The output-sized depth the layers test against (`depth_for_output`).
    pub(super) layer_depth: &'a wgpu::TextureView,
    /// The HDR translucent pass already drew the layers that participate in HDR.
    pub(super) hdr_particles: bool,
    /// The decal receiver stamps of this frame.
    pub(super) stamps: &'a [crate::decal_receiver::Stamp],
    /// Jittered view-projection.
    pub(super) view_proj: Mat4,
    /// Viewport rectangle in the output target.
    pub(super) viewport: [f32; 4],
    /// Colour sample count.
    pub(super) samples: u32,
}

/// What the translucent layers' simulate step reads.
pub(super) struct SimulateInputs<'a> {
    /// The HDR image the layers draw into, on an HDR frame.
    pub(super) layer_hdr: &'a Option<wgpu::TextureView>,
    /// Its format.
    pub(super) layer_hdr_format: wgpu::TextureFormat,
    /// The layers draw into the HDR image (before the post stack).
    pub(super) hdr_particles: bool,
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// Output size.
    pub(super) width: u32,
    /// Output size.
    pub(super) height: u32,
    /// The post stack's exposure multiplier.
    pub(super) post_exposure: f32,
    /// Pre-exposure of the HDR target.
    pub(super) pre_exposure: f32,
    /// The frame's sun.
    pub(super) directional: &'a crate::DirectionalLightUniform,
    /// The multisampled depth, on MSAA frames.
    pub(super) msaa_depth: &'a Option<wgpu::TextureView>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// Attachment size of the scene depth.
    pub(super) attach: (u32, u32),
}

/// What the HDR translucent layers read.
pub(super) struct HdrLayerInputs<'a> {
    /// The output-sized depth the layers test against (`depth_for_output`).
    pub(super) layer_depth: &'a wgpu::TextureView,
    /// The HDR target's format.
    pub(super) color_format: wgpu::TextureFormat,
    /// The decal receiver stamps of this frame.
    pub(super) stamps: &'a [crate::decal_receiver::Stamp],
    /// Jittered view-projection.
    pub(super) view_proj: Mat4,
    /// Colour sample count.
    pub(super) samples: u32,
    /// The source image is reused next frame (a cached composite or the TAAU history): draw into
    /// a scratch copy instead.
    pub(super) keep_source: bool,
    /// The scene draws double-sided (the decal receiver's cull mode).
    pub(super) double_sided: bool,
}

/// What the overlays (scalar heatmaps, gizmos) read.
pub(super) struct OverlayInputs<'a> {
    /// The multisampled depth, on MSAA frames.
    pub(super) msaa_depth: &'a Option<wgpu::TextureView>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// Viewport rectangle in the output target.
    pub(super) viewport: [f32; 4],
    /// Unjittered view-projection.
    pub(super) unjittered_vp: Mat4,
    /// Unjittered projection.
    pub(super) unjittered_proj: Mat4,
}

impl FrameRenderer {
    /// Records the translucent layers over the tonemapped image (decals first on an LDR frame).
    pub(super) fn record_translucent_layers(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        scene: &FrameScene<'_>,
        target: &FrameTarget<'_>,
        inputs: &LayerInputs<'_>,
    ) -> Result<(), RenderError> {
        let LayerInputs {
            layer_depth,
            hdr_particles,
            stamps,
            view_proj,
            viewport,
            samples,
        } = *inputs;
        let _span = tracing::info_span!("translucent_layers").entered();
        let rw = target.width.max(1);
        let rh = target.height.max(1);
        let decals_on = self.translucent.iter().any(|layer| layer.has_decals());
        if !hdr_particles && decals_on {
            self.record_decal_receiver(
                d,
                q,
                encoder,
                layer_depth,
                stamps,
                view_proj.to_cols_array_2d(),
                viewport,
                scene.double_sided,
                samples,
            )?;
            if let (Some(src), Some(dst)) = (target.color, self.decal_receiver.scene_texture_cloned())
                && dst.format() == src.format()
                && src.usage().contains(wgpu::TextureUsages::COPY_SRC)
                && src.size() == dst.size()
            {
                encoder.copy_texture_to_texture(
                    src.as_image_copy(),
                    dst.as_image_copy(),
                    wgpu::Extent3d {
                        width: rw,
                        height: rh,
                        depth_or_array_layers: 1,
                    },
                );
            }
            for layer in &self.translucent {
                layer.note_colour_format(target.format);
            }
        }
        let _crumb = crate::breadcrumbs::frame_pass("translucent_layers");
        let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("translucent layers"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: layer_depth,
                depth_ops: None,
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        for layer in &self.translucent {
            if hdr_particles {
                if !layer.participates_in_hdr() {
                    layer.draw(&mut pass, target.format);
                }
                continue;
            }
            {
                let _span = tracing::info_span!("decal_pass").entered();
                let _crumb = crate::breadcrumbs::frame_pass("decal_pass");
                layer.draw_decals(&mut pass, target.format);
            }
            layer.draw(&mut pass, target.format);
        }
        Ok(())
    }

    /// Simulates the translucent layers (VFX) with the main camera, binds their decal targets and
    /// returns the output-sized depth the layer passes test against (`depth_for_output`).
    pub(super) fn simulate_translucent_layers(
        &mut self,
        device: &crate::RenderDevice,
        d: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        scene: &FrameScene<'_>,
        target: &FrameTarget<'_>,
        inputs: &SimulateInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let SimulateInputs {
            layer_hdr,
            layer_hdr_format,
            hdr_particles,
            fov_y,
            width,
            height,
            post_exposure,
            pre_exposure,
            directional,
            msaa_depth,
            depth_view,
            attach: (attach_w, attach_h),
        } = *inputs;
        let _span = tracing::info_span!("translucent_simulate").entered();
        // HDR effects draw into this player's image, not the whole split-screen output.
        // Matching the window extent here pairs a full-window depth with a smaller color
        // attachment (ADR-7956). LDR effects still draw directly into the output target.
        let (rw, rh) = layer_hdr
            .as_ref()
            .map_or((target.width.max(1), target.height.max(1)), |view| {
                (view.texture().width(), view.texture().height())
            });
        self.decal_receiver.ensure(d, rw, rh);
        // The scene-colour copy decals composite over: the HDR buffer on an HDR
        // frame, the display target otherwise.
        let hdr_source = layer_hdr.as_ref().map(|view| view.texture().clone());
        // The HDR copy is any HDR format; the display copy must match the target's format.
        let source = if hdr_particles {
            hdr_source.as_ref().map(|tex| (tex, None))
        } else {
            target.color.map(|tex| (tex, Some(target.format)))
        };
        let blocker = source.map_or(Some(SceneCopyBlocker::Missing), |(tex, format)| {
            scene_copy_blocker(tex, (rw, rh), format)
        });
        let copy_ok = blocker.is_none();
        if copy_ok {
            let format = if hdr_particles {
                layer_hdr_format
            } else {
                target.format
            };
            self.decal_receiver.ensure_scene(d, rw, rh, format);
        } else if (hdr_particles || target.color.is_some()) && self.decal_receiver.take_copy_warning() {
            let reason = blocker.unwrap_or(SceneCopyBlocker::Missing);
            let detail = match (reason, source) {
                (SceneCopyBlocker::Size, Some((tex, _))) => format!(
                    "source {}x{}, decal target {rw}x{rh}",
                    tex.size().width,
                    tex.size().height
                ),
                (SceneCopyBlocker::Format, Some((tex, _))) => {
                    format!("source {:?}, display target {:?}", tex.format(), target.format)
                }
                _ => reason.detail().to_owned(),
            };
            tracing::warn!(
                target: "regia_render",
                reason = reason.name(),
                "decal scene-color copy skipped ({}): {detail}",
                reason.name()
            );
        }
        let receiver_view = self.decal_receiver.view_cloned();
        let scene_view = if copy_ok {
            self.decal_receiver.scene_view_cloned()
        } else {
            None
        };
        for layer in &self.translucent {
            layer.bind_decal_targets(receiver_view.as_ref(), scene_view.as_ref());
        }
        if let Some((size, atlas)) = self.atlas.clone() {
            self.bind_precip_occlusion(&atlas, size);
        }
        let layer_camera = crate::translucent::LayerCamera {
            eye: scene.eye,
            target: scene.target,
            fov_y,
            near: scene.near,
            far: scene.far,
            width,
            height,
            // The layer divides display-referred colour by this to write radiance;
            // the HDR target holds radiance × pre-exposure.
            exposure: if hdr_particles {
                post_exposure / pre_exposure
            } else {
                post_exposure
            },
            display_exposure: post_exposure,
            hdr: hdr_particles,
            sun_direction: [
                -directional.direction[0],
                -directional.direction[1],
                -directional.direction[2],
            ],
            sun_color: directional.color,
            sun_illuminance: directional.illuminance,
            sky_ambient: self.outdoor.hemisphere_ambient(),
        };
        let (source_depth, multisampled) = match msaa_depth.as_ref() {
            Some(view) => (view, true),
            None => (depth_view, false),
        };
        let layer_depth = self.depth_for_output(
            device,
            encoder,
            source_depth,
            multisampled,
            (attach_w, attach_h),
            (rw, rh),
        )?;
        for layer in &self.translucent {
            layer.simulate(&layer_depth, &layer_camera, encoder);
        }
        // The receiver's scene-depth group exists from the first frame on, not from the
        // first decal spawned after warm-up.
        let _ = self.decal_receiver.scene_depth_group(d, &layer_depth);
        Ok(layer_depth)
    }

    /// Records the decals and the HDR-participating translucent layers into `hdr` (or into a
    /// scratch copy of it) and returns the image they drew into, which the post stack reads.
    pub(super) fn record_translucent_hdr(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        hdr: wgpu::TextureView,
        inputs: &HdrLayerInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let HdrLayerInputs {
            layer_depth,
            color_format,
            stamps,
            view_proj,
            samples,
            keep_source,
            double_sided,
        } = *inputs;
        let _span = tracing::info_span!("translucent_hdr").entered();
        let _crumb = crate::breadcrumbs::frame_pass("translucent_hdr");
        let rw = hdr.texture().width();
        let rh = hdr.texture().height();
        // The player's HDR image starts at the origin; the window offset applies only at
        // the final composite. Receiver stamps and scratch copies use that same extent.
        let viewport = [0.0, 0.0, rw as f32, rh as f32];
        // Asked after simulate, so the layers know this frame's decals and emitters.
        // Without decals there is no receiver stamp and no scene copy; without a
        // refraction layer no second copy (each is a full-screen pass).
        let decals_on = self.translucent.iter().any(|layer| layer.has_decals());
        let copy_after_decals = self
            .translucent
            .iter()
            .any(|layer| layer.wants_scene_after_decals());
        if decals_on {
            self.record_decal_receiver(
                d,
                q,
                encoder,
                layer_depth,
                stamps,
                view_proj.to_cols_array_2d(),
                viewport,
                double_sided,
                samples,
            )?;
        }
        let source = hdr.texture().clone();
        let extent = wgpu::Extent3d {
            width: rw,
            height: rh,
            depth_or_array_layers: 1,
        };
        let can_copy = source.usage().contains(wgpu::TextureUsages::COPY_SRC);
        if can_copy
            && decals_on
            && let Some(dst) = self.decal_receiver.scene_texture_cloned()
            && dst.format() == source.format()
            && dst.size() == source.size()
        {
            encoder.copy_texture_to_texture(source.as_image_copy(), dst.as_image_copy(), extent);
        }
        // A cached SSGI/SSR composite, or the TAAU resolve (next frame's history), is
        // reused next frame: draw into a copy so this frame's effects stay out of it.
        let draw_target = if keep_source && can_copy {
            let scratch = self.hdr_scratch(
                super::hdr_scratch::HdrScratch::Translucent,
                d,
                rw,
                rh,
                color_format,
            );
            encoder.copy_texture_to_texture(source.as_image_copy(), scratch.texture.as_image_copy(), extent);
            scratch.view
        } else {
            hdr
        };
        for layer in &self.translucent {
            layer.note_colour_format(color_format);
        }
        if decals_on {
            let mut pass = begin_load_pass(encoder, "translucent hdr decals", &draw_target, layer_depth);
            let _span = tracing::info_span!("decal_pass").entered();
            let _crumb = crate::breadcrumbs::frame_pass("decal_pass");
            for layer in &self.translucent {
                layer.draw_decals(&mut pass, color_format);
            }
        }
        // Refresh the scene copy after the decals: heat haze bends the scene the
        // particles sit in front of, scorch included (same texture, decals are done).
        let after_decals = draw_target.texture().clone();
        if can_copy
            && copy_after_decals
            && let Some(dst) = self.decal_receiver.scene_texture_cloned()
            && dst.format() == after_decals.format()
            && dst.size() == after_decals.size()
        {
            encoder.copy_texture_to_texture(after_decals.as_image_copy(), dst.as_image_copy(), extent);
        }
        {
            let mut pass = begin_load_pass(encoder, "translucent hdr", &draw_target, layer_depth);
            for layer in &self.translucent {
                if layer.participates_in_hdr() {
                    layer.draw_hdr(&mut pass, color_format);
                }
            }
        }
        Ok(draw_target)
    }

    /// Records the scalar overlays and the gizmos over the finished image.
    pub(super) fn record_overlays(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        target: &FrameTarget<'_>,
        inputs: &OverlayInputs<'_>,
    ) {
        let OverlayInputs {
            msaa_depth,
            depth_view,
            viewport,
            unjittered_vp,
            unjittered_proj,
        } = *inputs;
        // ---- Scalar overlays: per-vertex heatmaps, unlit over the finished frame ----
        if !self.scalar_overlays.is_empty() && !self.capturing_probe {
            let _span = tracing::info_span!("scalar_overlays").entered();
            let _crumb = crate::breadcrumbs::frame_pass("scalar_overlays");
            let (depth, multisampled_depth) = match msaa_depth.as_ref() {
                Some(view) => (view, true),
                None => (depth_view, false),
            };
            let overlay_target = crate::scalar_overlay_pass::OverlayTarget {
                view: target.view,
                format: target.format,
                viewport,
                depth,
                multisampled_depth,
                clip_from_world: unjittered_vp,
            };
            if let Err(err) = self.scalar_pass.record(
                &crate::device::TrackedDevice::observe(d),
                q,
                encoder,
                &overlay_target,
            ) {
                tracing::warn!(target: "regia_render", "scalar overlay pass skipped: {err}");
            }
        }
        // ---- Gizmos: debug lines over the final image, before any overlay (UI) ----
        // The scene depth is sampled in the fragment shader (any render scale, the viewport
        // rectangle, MSAA sample 0), so every quality preset takes this one path.
        if let Some(gizmos) = self.gizmos.clone() {
            let _span = tracing::info_span!("gizmos").entered();
            let _crumb = crate::breadcrumbs::frame_pass("gizmos");
            let (depth, multisampled_depth) = match msaa_depth.as_ref() {
                Some(view) => (view, true),
                None => (depth_view, false),
            };
            let gizmo_target = crate::gizmos::render::GizmoTarget {
                view: target.view,
                format: target.format,
                viewport,
                depth,
                multisampled_depth,
                clip_from_world: unjittered_vp,
                clip_from_view: unjittered_proj,
            };
            if let Err(err) = self.gizmo_renderer.record(d, q, encoder, &gizmos, &gizmo_target) {
                tracing::warn!(target: "regia_render", "gizmo pass skipped: {err}");
            }
        }
    }
}

/// Why the decal scene-color copy cannot read its source (v0.5.1: the warning names the reason,
/// so a report says which).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SceneCopyBlocker {
    /// No source image this frame (an HDR layer without the HDR target).
    Missing,
    /// The source texture was created without `COPY_SRC` usage (v0.1.21's TSR output, ADR-1863).
    Usage,
    /// The source is not the decal target's size.
    Size,
    /// The display copy's format is not the display target's.
    Format,
}

impl SceneCopyBlocker {
    /// Stable name: `missing`, `usage`, `size`, `format`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Usage => "usage",
            Self::Size => "size",
            Self::Format => "format",
        }
    }

    /// The warning text for reasons that need no numbers.
    fn detail(self) -> &'static str {
        match self {
            Self::Missing => "no source image this frame",
            Self::Usage => "the source texture lacks COPY_SRC usage",
            Self::Size => "the source is not the decal target's size",
            Self::Format => "the source format is not the display target's",
        }
    }
}

/// The first reason `tex` cannot be the scene-color copy's source for a `want` sized target
/// (`format`: the format it must have, `None` for any), else `None`.
fn scene_copy_blocker(
    tex: &wgpu::Texture,
    want: (u32, u32),
    format: Option<wgpu::TextureFormat>,
) -> Option<SceneCopyBlocker> {
    scene_copy_blocker_of(
        tex.usage(),
        (tex.size().width, tex.size().height),
        tex.format(),
        want,
        format,
    )
}

fn scene_copy_blocker_of(
    usage: wgpu::TextureUsages,
    size: (u32, u32),
    actual: wgpu::TextureFormat,
    want: (u32, u32),
    format: Option<wgpu::TextureFormat>,
) -> Option<SceneCopyBlocker> {
    if !usage.contains(wgpu::TextureUsages::COPY_SRC) {
        Some(SceneCopyBlocker::Usage)
    } else if size != want {
        Some(SceneCopyBlocker::Size)
    } else if format.is_some_and(|f| f != actual) {
        Some(SceneCopyBlocker::Format)
    } else {
        None
    }
}

#[cfg(test)]
mod scene_copy_tests {
    use super::*;

    /// v0.5.1: each blocker of the decal scene-color copy has its own reason, checked in order.
    #[test]
    fn scene_copy_blockers_name_their_reason() {
        use wgpu::{TextureFormat as F, TextureUsages as U};
        let ok = U::COPY_SRC | U::RENDER_ATTACHMENT;
        let srgb = F::Rgba8UnormSrgb;
        assert_eq!(scene_copy_blocker_of(ok, (8, 8), srgb, (8, 8), Some(srgb)), None);
        assert_eq!(
            scene_copy_blocker_of(ok, (8, 8), F::Rgba16Float, (8, 8), None),
            None
        );
        assert_eq!(
            scene_copy_blocker_of(U::RENDER_ATTACHMENT, (9, 8), F::Bgra8Unorm, (8, 8), Some(srgb)),
            Some(SceneCopyBlocker::Usage),
            "usage first"
        );
        assert_eq!(
            scene_copy_blocker_of(ok, (9, 8), F::Bgra8Unorm, (8, 8), Some(srgb)),
            Some(SceneCopyBlocker::Size)
        );
        assert_eq!(
            scene_copy_blocker_of(ok, (8, 8), F::Bgra8Unorm, (8, 8), Some(srgb)),
            Some(SceneCopyBlocker::Format)
        );
        assert_eq!(
            [
                SceneCopyBlocker::Missing,
                SceneCopyBlocker::Usage,
                SceneCopyBlocker::Size,
                SceneCopyBlocker::Format
            ]
            .map(SceneCopyBlocker::name),
            ["missing", "usage", "size", "format"]
        );
    }
}
