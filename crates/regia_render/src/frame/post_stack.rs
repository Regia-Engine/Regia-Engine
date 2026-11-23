// SPDX-License-Identifier: MPL-2.0
//! The post stack pass of `FrameRenderer::render_into` (ADR-1552 split by pass): host passes at
//! `before_post`, camera lens effects, the HDR capture copy, and the post chain into the output
//! target. Moved out of `frame.rs` unchanged; its inputs are named in [`PostStackInputs`].

use regia_core::glam::Mat4;

use super::{FrameRenderer, FrameScene, FrameTarget, extension, record_hdr_copy};
use crate::device::TrackedQueue;
use crate::{RenderError, TrackedDevice};

/// What the post stack reads from the frame it closes.
pub(super) struct PostStackInputs<'a> {
    /// The post settings this frame records (exposure, tonemap, grade, AA).
    pub(super) settings: &'a crate::post::PostSettings,
    /// The temporally upscaled HDR image, when TAAU or TSR ran.
    pub(super) upscaled: &'a Option<wgpu::TextureView>,
    /// The shaded HDR image at the internal size otherwise.
    pub(super) shaded: &'a Option<wgpu::TextureView>,
    /// Output size.
    pub(super) width: u32,
    /// Output size.
    pub(super) height: u32,
    /// Internal (render-scale) size.
    pub(super) internal_w: u32,
    /// Internal (render-scale) size.
    pub(super) internal_h: u32,
    /// The multisampled depth, on MSAA frames.
    pub(super) msaa_depth: &'a Option<wgpu::TextureView>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// Unjittered view-projection.
    pub(super) unjittered_vp: Mat4,
    /// Unjittered projection.
    pub(super) unjittered_proj: Mat4,
    /// Pre-exposure of the HDR target.
    pub(super) pre_exposure: f32,
    /// Colour sample count.
    pub(super) samples: u32,
    /// TAAU runs this frame.
    pub(super) taau_on: bool,
    /// The TAAU history is frozen (a held, converged view).
    pub(super) taau_frozen: bool,
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// Viewport rectangle in the output target.
    pub(super) viewport: [f32; 4],
    /// The held-view screen cache is keyed this frame.
    pub(super) screen_key_some: bool,
    /// The SGSR spatial upscaler runs in the post stack.
    pub(super) sgsr_on: bool,
}

impl FrameRenderer {
    /// Records the post stack; returns the readback milliseconds it spent (`readback_ms`).
    pub(super) fn record_post_stack(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        scene: &FrameScene<'_>,
        target: &FrameTarget<'_>,
        inputs: &PostStackInputs<'_>,
    ) -> Result<f32, RenderError> {
        let PostStackInputs {
            settings,
            upscaled,
            shaded,
            width,
            height,
            internal_w,
            internal_h,
            msaa_depth,
            depth_view,
            unjittered_vp,
            unjittered_proj,
            pre_exposure,
            samples,
            taau_on,
            taau_frozen,
            fov_y,
            viewport,
            screen_key_some,
            sgsr_on,
        } = *inputs;
        let _span = tracing::info_span!("post_stack").entered();
        let hdr_for_post = upscaled
            .as_ref()
            .or(shaded.as_ref())
            .expect("hdr view when post is on");
        let (post_w, post_h) = if upscaled.is_some() {
            (width, height)
        } else {
            (internal_w, internal_h)
        };
        // Host passes over the scene-referred HDR image (ADR-1552 `before_post`); they see the
        // view's post settings (the lens effects pass reads its strengths there, ADR-1863).
        // A writing pass gets a per-frame copy (the image here may be the temporal history or
        // the held-screen cache), and the rest of the post stack reads that copy.
        let exposure = self
            .post
            .as_ref()
            .and_then(crate::post_pass::PostChain::prepared_exposure)
            .unwrap_or(pre_exposure);
        // `after_dof` (ADR-3196) moves the slot behind the camera lens (depth of field and
        // motion blur) so the flare, dirt and starburst stay sharp; it only applies when the lens
        // runs this frame, else the slot stays where it was.
        let lens_wanted = samples == 1 && crate::lens::LensPasses::wanted(settings);
        let slot_after_lens = lens_wanted && settings.lens_effects.after_dof;
        macro_rules! run_before_post {
            ($color:expr) => {
                self.run_extension_slot_with_post(
                    crate::GraphSlot::BeforePost,
                    d,
                    q,
                    encoder,
                    &extension::SlotView {
                        color: $color,
                        color_format: $color.texture().format(),
                        color_size: (post_w, post_h),
                        viewport: [0.0, 0.0, post_w as f32, post_h as f32],
                        msaa_depth,
                        depth_view,
                        clip_from_world: unjittered_vp,
                        clip_from_view: unjittered_proj,
                        eye: scene.eye,
                        pre_exposure,
                    },
                    Some(extension::SlotPost { settings, exposure }),
                )
            };
        }
        let written = if slot_after_lens {
            None
        } else {
            run_before_post!(hdr_for_post)
        };
        let hdr_for_post = written.as_ref().unwrap_or(hdr_for_post);
        // Camera lens effects (opt-in per preset and camera, ADR-041): `None` leaves the
        // post stack reading the scene HDR. Motion reads this frame's velocity target only;
        // a held (frozen) view drew none.
        let lensed = if lens_wanted {
            let velocity = if taau_on && !taau_frozen {
                self.taau_pass.velocity_view()
            } else {
                None
            };
            let inputs = crate::lens::LensInputs {
                hdr: hdr_for_post,
                width: post_w,
                height: post_h,
                velocity: velocity.as_ref(),
                depth: depth_view,
                projection: (fov_y, scene.near, scene.far),
                frame: self.frame_index.wrapping_sub(self.cut.origin),
            };
            self.lens.record(d, &mut self.belt, encoder, settings, &inputs)?
        } else {
            None
        };
        let hdr_for_post = lensed.as_ref().unwrap_or(hdr_for_post);
        let written_after = if slot_after_lens {
            run_before_post!(hdr_for_post)
        } else {
            None
        };
        let hdr_for_post = written_after.as_ref().unwrap_or(hdr_for_post);
        if std::mem::take(&mut self.hdr_capture_requested) {
            self.hdr_capture = record_hdr_copy(d, encoder, hdr_for_post.texture(), post_w, post_h)
                .map(|copy| (copy, pre_exposure));
        }
        if self.post.is_none() {
            self.post = Some(Default::default());
        }
        let post = self.post.as_mut().expect("post chain");
        post.set_pre_exposure(pre_exposure);
        post.set_output_dither(self.output_dither);
        let belt = &mut self.belt;
        let readback_ms = post.record(
            d,
            belt,
            encoder,
            hdr_for_post,
            post_w,
            post_h,
            settings,
            target.view,
            target.format,
            viewport,
            self.display_headroom,
            crate::post_pass::PostAa {
                smaa: scene.quality.smaa,
                sgsr_output: (upscaled.is_none() && sgsr_on).then_some((width, height)),
            },
        )?;
        // The held-view reuse path hands post an HDR view the moving frames did not (the
        // converged TAAU history, the cached composite, a scratch copy): build those
        // bind groups now so the first parked frame creates none (ADR-1361).
        // With a temporal resolve the cached composite and the outdoor copy are its input, never
        // post's: at native scale they pass the size check, so the first hold whose composite sat
        // in the other screen-space target built post groups for it mid-session (ADR-2585).
        if screen_key_some {
            let mut candidates = self.taau_pass.history_views(post_w, post_h);
            if upscaled.is_none() {
                candidates.extend(self.screen_cache_view.clone());
                candidates.extend(self.hdr_scratch_view(super::hdr_scratch::HdrScratch::Outdoor));
            }
            // The scratch images are keyed transients of the view's pool (ADR-7877): the latest
            // of each slot, which the pool holds while post may be handed it.
            candidates.extend(self.hdr_scratch_view(super::hdr_scratch::HdrScratch::Translucent));
            if let Some(post) = self.post.as_mut() {
                for view in candidates {
                    let size = view.texture().size();
                    if size.width == post_w && size.height == post_h && view != *hdr_for_post {
                        let _ = post.prewarm_input(d, &view);
                    }
                }
            }
        }
        Ok(readback_ms)
    }
}
