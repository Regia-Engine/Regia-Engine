// SPDX-License-Identifier: MPL-2.0
//! The static-screen reuse decision (ADR-2300): the draw signature, the screen cache key, the
//! held-frame count, and which screen-space passes run, reuse their gather or serve last frame's
//! composite. Moved out of `FrameRenderer::render_into` unchanged.

use crate::frame::*;

/// What the screen reuse step reads.
pub(in crate::frame) struct ScreenReuseInputs<'a> {
    /// The scene.
    pub(in crate::frame) scene: &'a FrameScene<'a>,
    /// The frame's draws.
    pub(in crate::frame) draws: &'a [Draw],
    /// Visible opaque draws.
    pub(in crate::frame) opaque_ix: &'a [usize],
    /// The accumulation pass's draws.
    pub(in crate::frame) oit_frame: &'a oit_gpu::OitFrame,
    /// The quality the GI passes read.
    pub(in crate::frame) gi_quality: FrameQuality,
    /// View-projection without jitter.
    pub(in crate::frame) unjittered_vp: regia_core::glam::Mat4,
    /// The sun's variant this frame.
    pub(in crate::frame) sun_variant: crate::hwrt::SunShadowVariant,
    /// The colour attachment's format.
    pub(in crate::frame) color_format: wgpu::TextureFormat,
    /// Pre-exposure of the linear HDR target.
    pub(in crate::frame) pre_exposure: f32,
    /// Output width.
    pub(in crate::frame) width: u32,
    /// Output height.
    pub(in crate::frame) height: u32,
    /// Internal width.
    pub(in crate::frame) internal_w: u32,
    /// Internal height.
    pub(in crate::frame) internal_h: u32,
    /// MSAA samples.
    pub(in crate::frame) samples: u32,
    /// Linear HDR target.
    pub(in crate::frame) linear_hdr: bool,
    /// The physical sky background is part of the forward image this frame.
    pub(in crate::frame) sky_pass: bool,
    /// A shadow diagnostic frame.
    pub(in crate::frame) shadow_debug: bool,
    /// Temporal AA/upscale runs.
    pub(in crate::frame) taau_on: bool,
    /// Something fills the visibility texture.
    pub(in crate::frame) visibility_active: bool,
    /// Bent normals run.
    pub(in crate::frame) bent_on: bool,
    /// Stochastic SSR is chosen for this frame's target.
    pub(in crate::frame) sssr_chosen: bool,
    /// Stochastic SSR runs.
    pub(in crate::frame) sssr_on: bool,
    /// The frame renders into a texture view.
    pub(in crate::frame) view_pass: bool,
}

/// Which screen-space passes run this frame and what they reuse.
pub(in crate::frame) struct ScreenReuse {
    /// Screen-space GI and SSR can run (single-sample linear HDR with a depth prepass).
    pub(in crate::frame) screen_space: bool,
    /// SSGI gathers at half resolution.
    pub(in crate::frame) ssgi_half: bool,
    /// SSGI uses relative depth.
    pub(in crate::frame) ssgi_relative: bool,
    /// The static-screen key (None: the frame never reuses).
    pub(in crate::frame) screen_key: Option<ScreenCacheKey>,
    /// Stochastic SSR runs this frame.
    pub(in crate::frame) sssr_frame: bool,
    /// The full-resolution SSR march runs.
    pub(in crate::frame) ssr_wanted: bool,
    /// A held view reuses its SSGI gather.
    pub(in crate::frame) gather_reuse: bool,
    /// The cached composite serves the view.
    pub(in crate::frame) reuse_hit: bool,
    /// The temporal resolve shows its last output as it is.
    pub(in crate::frame) taau_frozen: bool,
}

/// Flag bits for the shadow filter, and the draw-signature mix for bent normals.
///
/// `shadow_mode` is `FrameQuality::shadow_filter_mode` (`0..=4`). Mode 4 is three bits at
/// `<< 8`, so it occupies bit 10. Bent normals used to be packed in that same bit, and a held
/// view then reused the other setting's AO. Bent normals mix into the draw signature instead,
/// which leaves the rest of the flag word where it is.
fn shadow_and_bent_key(shadow_mode: u32, bent_on: bool) -> (u32, u64) {
    ((shadow_mode & 0b111) << 8, u64::from(bent_on) << 63)
}

impl FrameRenderer {
    /// Keys the frame's screen-space inputs, counts held frames, and decides what the AO, SSGI
    /// and SSR passes reuse; records the screen report's cache hit.
    pub(in crate::frame) fn screen_reuse(&mut self, inputs: &ScreenReuseInputs<'_>) -> ScreenReuse {
        let &ScreenReuseInputs {
            scene,
            draws,
            opaque_ix,
            oit_frame,
            gi_quality,
            unjittered_vp,
            sun_variant,
            color_format,
            pre_exposure,
            width,
            height,
            internal_w,
            internal_h,
            samples,
            linear_hdr,
            sky_pass,
            shadow_debug,
            taau_on,
            visibility_active,
            bent_on,
            sssr_chosen,
            sssr_on,
            view_pass,
        } = inputs;
        // Static camera + sun + draw list: keep last frame's AO, SSGI and SSR.
        // The first frame always misses. Lab and editor leave the gate off.
        let screen_space = linear_hdr && scene.depth_prepass && samples == 1;
        // SSGI gathers at half resolution on the 0.67 budget, or where a measurement asks.
        let ssgi_half = self.ssgi_half_res.unwrap_or_else(|| gi_quality.ssgi_half_res());
        let ssgi_relative = self.ssgi_relative_depth;
        // Persistent instance sets count as draws.
        // Material keys survive scalar edits (the uniform is rewritten in place, lab keys come
        // from slot and content since lab-stable-keys), so the material epoch joins the key: a
        // roughness change on a held view must not reuse last frame's AO / SSGI / SSR.
        let mut draw_sig = (draws.len() as u64)
            ^ self.visbuffer.sets_generation().rotate_left(32)
            ^ self.material_epoch.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for draw in draws {
            draw_sig = draw_sig
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(draw.mesh)
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(draw.material)
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(draw.object)
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(u64::from(draw.visible))
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(u64::from(draw.lod_dither.to_bits()))
                .wrapping_mul(0x1000_0000_01b3)
                .wrapping_add(u64::from(draw.over[0]) | (u64::from(draw.over[1]) << 32));
            for row in draw.model {
                for value in row {
                    draw_sig = draw_sig
                        .wrapping_mul(0x1000_0000_01b3)
                        .wrapping_add(u64::from(value.to_bits()));
                }
            }
        }
        // Morph weights change the vertices under unchanged draw keys.
        draw_sig = draw_sig.wrapping_add(self.morph.content_key());
        let ssao_flag = match gi_quality.ssao {
            crate::profile::SsaoMode::Off => 0u32,
            crate::profile::SsaoMode::HalfRes => 1,
            crate::profile::SsaoMode::GtaoHalfRes => 2,
            crate::profile::SsaoMode::GtaoFull => 3,
        };
        // The TSR and FSR 3.1 upscalers place every input sample by this frame's jitter,
        // and a static hit would hand them the previous jitter's composite (doubled thin
        // lines, stair-stepped edges) with no new sub-pixel samples: they opt out.
        let tsr_selected = scene.quality.taau
            && !shadow_debug
            && samples == 1
            && matches!(
                self.temporal_upscaler,
                crate::TemporalUpscaler::Tsr | crate::TemporalUpscaler::Fsr3
            );
        let screen_key = if scene.quality.reuse_static_screen() && screen_space && !tsr_selected {
            let vp = unjittered_vp.to_cols_array_2d();
            let (shadow_flags, bent_draw) = shadow_and_bent_key(scene.quality.shadow_filter_mode(), bent_on);
            Some(ScreenCacheKey {
                vp: [fbits(vp[0]), fbits(vp[1]), fbits(vp[2]), fbits(vp[3])],
                sun_dir: [
                    scene.directional.direction[0].to_bits(),
                    scene.directional.direction[1].to_bits(),
                    scene.directional.direction[2].to_bits(),
                ],
                sun_lux: scene.directional.illuminance.to_bits(),
                exposure: scene.exposure_ev.to_bits(),
                pre_exposure: pre_exposure.to_bits(),
                ambient: scene.ambient_lux.to_bits(),
                draws: draw_sig ^ bent_draw,
                width: internal_w,
                height: internal_h,
                flags: u32::from(gi_quality.screen_space_gi)
                    | (u32::from(scene.quality.ssr) << 1)
                    | (ssao_flag << 2)
                    | (u32::from(visibility_active) << 4)
                    | (u32::from(sun_variant.key_bits()) << 11)
                    | (u32::from(opaque_ix.iter().any(|&index| draws[index].subsurface)) << 13)
                    | (u32::from(oit_frame.runs()) << 14)
                    // A kernel switch on a held view has to show (ADR-3170); default keys keep the 0.
                    | (u32::from(
                        scene.quality.subsurface_kernel == crate::subsurface::SubsurfaceKernel::Tangent
                            && opaque_ix.iter().any(|&index| draws[index].subsurface),
                    ) << 15)
                    | (u32::from(scene.quality.reflection_occlusion) << 5)
                    | (u32::from(ssgi_half) << 6)
                    | (u32::from(ssgi_relative) << 7)
                    | shadow_flags
                    | (u32::from(scene.quality.render_scale_permille) << 16),
                outdoor: self.outdoor_key
                    ^ self.glass_key
                    ^ if self.glass_key == 0 {
                        0
                    } else {
                        u64::from(crate::glass::canopy_bits(self.canopy_glass())) << 56
                    }
                    ^ self.mask_decals.content_key()
                    ^ self.lightmap_forward.content_key(),
                sky: sky_pass.then(|| self.outdoor.sky_background_key()),
                shared: self.shared_draw.as_ref().map_or(0, |draw| draw.content_key())
                    // A new lookup resolution re-runs the lookup the forward pass reads.
                    ^ (u64::from(self.gi_lookup.step()) << 52)
                    // Capsules the baked screen pass must write again (ADR-2656); 0 without any.
                    ^ crate::baked_gi::capsule_list_key(&self.baked_gi.occluders)
                    // The gather that reads the radiance cache (ADR-2085, `radiance_cache_pass`).
                    ^ radiance_cache_pass::screen_key(
                        &self.radiance_cache,
                        !view_pass && gi_quality.screen_space_gi && self.ddgi.active,
                        self.frame_index,
                    )
                    // Probes that just moved or saw a new scene change the indirect light every
                    // frame; after DDGI_REUSE_FRAMES their young-probe updates are done.
                    ^ if self.ddgi.active && self.ddgi.settled_frames() < DDGI_REUSE_FRAMES {
                        self.frame_index.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    } else {
                        0
                    },
            })
        } else {
            None
        };
        if screen_key.is_none() {
            self.screen_cache_key = None;
            self.screen_cache_view = None;
        }
        let key_held =
            screen_key.is_some() && self.screen_cache_key == screen_key && self.screen_cache_view.is_some();
        self.screen_hold = if key_held {
            self.screen_hold.saturating_add(1)
        } else {
            0
        };
        // With TAA a hit froze one jittered frame: stable, but with native (aliased) edges.
        // Desktop High (half-resolution SSGI, no full-resolution SSR) keeps the bounce it
        // gathered and composites it onto every new jittered frame (`gather_reuse`). The other
        // tiers cache the whole composite, so their first held frames still render in full;
        // the cache serves the view only once the resolve has converged. Either way the
        // resolve then shows its last output as it is (`taau_frozen`).
        let reflections_on =
            scene.quality.ssr || (scene.quality.reflection_probes && !self.reflection_probes.is_empty());
        // Stochastic SSR runs on every Desktop High frame, the 0.67 budget included (its cost
        // is bounded by the ray list), and replaces the older full-screen march, which stays
        // off at that budget (ADR-041). A held view keeps reusing its SSGI gather under it.
        let sssr_capable =
            sssr_chosen && screen_space && reflections_on && crate::sssr::format_supported(color_format);
        let sssr_frame = sssr_capable && sssr_on;
        let ssr_wanted =
            !sssr_capable && screen_space && scene.quality.full_res_reflections() && reflections_on;
        let gather_reuse = key_held && ssgi_half && !ssr_wanted;
        let converged = key_held && self.screen_hold >= if taau_on { HELD_CONVERGE_FRAMES } else { 0 };
        let reuse_hit = converged && !gather_reuse;
        let taau_frozen = converged
            && taau_on
            && self.taau_ready
            && self.taau_size == (width, height)
            && self.taau_pass.last_output(width, height).is_some();
        self.screen_report = ScreenPassReport {
            screen_cache_hit: reuse_hit && !taau_frozen,
            ..ScreenPassReport::default()
        };
        ScreenReuse {
            screen_space,
            ssgi_half,
            ssgi_relative,
            screen_key,
            sssr_frame,
            ssr_wanted,
            gather_reuse,
            reuse_hit,
            taau_frozen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shadow_and_bent_key;

    /// PCSS-lite with the sun's angular penumbra is mode 4 (`4 << 8`, bit 10). Bent normals
    /// were packed in that same bit, so a held view reused the other setting's AO / SSGI / SSR.
    #[test]
    fn angular_pcss_does_not_alias_bent_normals_in_the_screen_key() {
        let (angular, angular_draw) = shadow_and_bent_key(4, false);
        let (bent, bent_draw) = shadow_and_bent_key(0, true);
        assert!(
            angular != bent || angular_draw != bent_draw,
            "mode 4 and bent normals share a screen-key bit ({angular:#x}, draw {angular_draw:#x})"
        );
        let (plain, plain_draw) = shadow_and_bent_key(3, false);
        let (penumbra, penumbra_draw) = shadow_and_bent_key(4, false);
        assert!(
            plain != penumbra || plain_draw != penumbra_draw,
            "angular penumbra must not reuse a plain PCSS-lite cache"
        );
        let (off, off_draw) = shadow_and_bent_key(0, false);
        let (on, on_draw) = shadow_and_bent_key(0, true);
        assert!(
            off != on || off_draw != on_draw,
            "turning bent normals off must not reuse the bent frame"
        );
    }
}
