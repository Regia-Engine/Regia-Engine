// SPDX-License-Identifier: MPL-2.0
//! The frame's pipelines for its target (ADR-2300): the frame problems, the pipeline keys, the
//! forward and lite sets, the deferred variants, and the mask-decal, lightmap, thin-film,
//! subsurface and accumulation sets. Moved out of `FrameRenderer::render_into` unchanged; the
//! caller looks the forward sets up again by key (a map read).

use crate::frame::*;

/// What the pipelines step reads.
pub(in crate::frame) struct PipelineInputs<'a> {
    /// The scene.
    pub(in crate::frame) scene: &'a FrameScene<'a>,
    /// Visible opaque draws.
    pub(in crate::frame) opaque_ix: &'a [usize],
    /// Material-graph pipeline sets of the frame's draws.
    pub(in crate::frame) graph_sets: &'a shadow_passes::GraphSets,
    /// The colour attachment's format.
    pub(in crate::frame) color_format: wgpu::TextureFormat,
    /// The target is not sRGB: the shader encodes.
    pub(in crate::frame) encode_target: bool,
    /// Linear HDR target.
    pub(in crate::frame) linear_hdr: bool,
    /// MSAA samples.
    pub(in crate::frame) samples: u32,
    /// The forward key's sun-trace bit.
    pub(in crate::frame) sun_rt: bool,
    /// The forward key's studio-off bit (ADR-2308).
    pub(in crate::frame) studio_off: bool,
    /// The forward key's many-light bit (ADR-2652): the opaque colour pipeline shades the
    /// reservoir.
    pub(in crate::frame) restir_di: bool,
    /// Objects live in GPU storage.
    pub(in crate::frame) gpu_objects: bool,
    /// Masked decals the frame cannot draw.
    pub(in crate::frame) mask_unsupported: usize,
    /// The frame's forward tier.
    pub(in crate::frame) forward_tier: crate::forward_lite::ForwardLite,
    /// The frame renders into a texture view.
    pub(in crate::frame) view_pass: bool,
}

/// The frame's pipeline keys and the per-feature sets it resolved.
pub(in crate::frame) struct FramePipelines {
    /// The frame's pipeline key.
    pub(in crate::frame) key: PipelineKey,
    /// Its cull-none variant, for double-sided materials in a culled frame.
    pub(in crate::frame) key_ds: PipelineKey,
    /// A draw takes the cull-none set.
    pub(in crate::frame) any_cull_off: bool,
    /// The Mobile tiers' lite colour pipelines (culled, cull-none).
    pub(in crate::frame) lite_colors: Option<[std::sync::Arc<forward_lite_gpu::LiteColor>; 2]>,
    /// Mask-decal pipeline sets (culled, cull-none).
    pub(in crate::frame) mask_sets: [Option<mask_decals_gpu::MaskSet>; 2],
    /// The lightmap receivers' sets.
    pub(in crate::frame) lightmap_frame: lightmap_forward::LightmapFrame,
    /// The thin film's sets.
    pub(in crate::frame) film_frame: thin_film_gpu::FilmFrame,
    /// The scattering materials' sets.
    pub(in crate::frame) subsurface_frame: subsurface_gpu::SubsurfaceFrame,
    /// The accumulation pass's draws.
    pub(in crate::frame) oit_frame: oit_gpu::OitFrame,
}

impl FrameRenderer {
    /// Builds (or finds) every pipeline the frame's draws need for this target; drops the
    /// blended draws whose deferred variant is not built yet and moves draws to the feature
    /// passes that take them.
    pub(in crate::frame) fn frame_pipelines(
        &mut self,
        device: &RenderDevice,
        d: &crate::TrackedDevice,
        draws: &mut Vec<Draw>,
        blend_ix: &mut Vec<usize>,
        inputs: &PipelineInputs<'_>,
    ) -> Result<FramePipelines, RenderError> {
        let &PipelineInputs {
            scene,
            opaque_ix,
            graph_sets,
            color_format,
            encode_target,
            linear_hdr,
            samples,
            sun_rt,
            studio_off,
            restir_di,
            gpu_objects,
            mask_unsupported,
            forward_tier,
            view_pass,
        } = inputs;
        let pipes = self.pipes.as_mut().expect("pipelines");
        settings::collect_frame_problems(
            settings::ProblemFields {
                frame_problems: &mut self.frame_problems,
                texture_problems: &self.texture_problems,
                capability_problems: &mut self.capability_problems,
                capability_device: &mut self.capability_device,
            },
            device,
            scene,
            samples,
            self.shared_draw.is_some(),
            &self.outdoor,
        );
        let key = PipelineKey {
            format: color_format,
            encode: if linear_hdr { false } else { encode_target },
            double_sided: scene.double_sided,
            samples,
            sun_rt,
            studio_off,
            restir_di,
        };
        // The cull-none variant of the key, for double-sided materials in a culled frame.
        let key_ds = PipelineKey {
            double_sided: true,
            ..key
        };
        let any_cull_off = pipeline_sets::build_forward_sets(
            pipeline_sets::ForwardSetFields {
                pipes: &mut *pipes,
                culler: &mut self.culler,
                meshes: &self.meshes,
                serial_forward_families: self.serial_forward_families,
            },
            d,
            &pipeline_sets::ForwardSetInputs {
                draws,
                opaque_ix,
                key,
                key_ds,
                color_format,
                samples,
                gpu_objects,
                double_sided: scene.double_sided,
            },
        )?;
        let lite_weather = self.weather.lite_variant();
        let lite_colors = forward_lite_gpu::frame_lite_colors(
            &mut *pipes,
            d,
            (key.plain(), key_ds.plain()),
            forward_tier,
            lite_weather,
        )?;
        if self.weather.warm_lite && !lite_weather {
            // The project uses the surface weather: build the wet variant with the dry one (a
            // cache hit after the first lite frame of this key), so going wet compiles nothing.
            forward_lite_gpu::frame_lite_colors(
                &mut *pipes,
                d,
                (key.plain(), key_ds.plain()),
                forward_tier,
                true,
            )?;
        }
        let set = pipes.sets.get(&key).expect("just inserted");
        // Pipelines of a double-sided draw (`Draw::cull_off`); the frame's own set otherwise.
        let set_ds = if any_cull_off {
            pipes.sets.get(&key_ds).expect("just inserted")
        } else {
            set
        };
        // Deferred forward variants (`pipeline_warm::DeferredPipeline`). Where this thread may
        // wait (a warm, a loading screen, no warm workers) every variant a blended or glass
        // material of this frame can draw — visible or not — is built now, side by side. Mid-play
        // a draw whose variant is not built yet is skipped this frame and its build jumps the
        // background queue: the frame path never compiles one.
        pipeline_sets::resolve_deferred_variants(set, set_ds, draws, blend_ix);
        let mask_sets = pipeline_sets::resolve_mask_sets(
            &mut pipes.mask,
            [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl],
            &mut self.mask_decals,
            d,
            (key.plain(), key_ds.plain()),
            draws,
            mask_unsupported,
        );
        let lightmap_frame = lightmap_forward::resolve_lightmap_sets(
            lightmap_forward::LightmapResolve {
                pipes: &mut pipes.lightmap,
                groups: [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl],
                culler: &mut self.culler,
                visbuffer: &mut self.visbuffer,
                state: &mut self.lightmap_forward,
                device,
                d,
                keys: (key.plain(), key_ds.plain()),
            },
            draws,
        );
        let film_frame = thin_film_gpu::resolve_film_sets(
            &mut pipes.film,
            [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl],
            &mut self.thin_film,
            d,
            (key, key_ds),
            draws,
        );
        let subsurface_frame = subsurface_gpu::resolve_subsurface(
            &mut pipes.subsurface,
            &pipes.forward_layout,
            &mut self.subsurface,
            d,
            (key.plain(), key_ds.plain()),
            draws,
            &subsurface_gpu::SubsurfaceAsk {
                requested: scene.quality.subsurface_scattering,
                blocked: subsurface_gpu::blocked_reason(linear_hdr, samples, forward_tier),
                main_view: !view_pass,
            },
        );
        // Translucent draws leave the sorted run for the accumulation pass (`oit_gpu`).
        let oit_frame = oit_gpu::resolve_oit(
            &mut pipes.oit,
            &pipes.forward_layout,
            &mut self.oit,
            d,
            (key.plain(), key_ds.plain()),
            (draws, blend_ix, graph_sets),
            &oit_gpu::OitAsk {
                requested: scene.quality.order_independent_transparency,
                blocked: oit_gpu::blocked_reason(
                    device,
                    scene.quality.order_independent_transparency,
                    linear_hdr,
                    samples,
                    forward_tier != crate::forward_lite::ForwardLite::Off,
                ),
                main_view: !view_pass,
            },
        );

        Ok(FramePipelines {
            key,
            key_ds,
            any_cull_off,
            lite_colors,
            mask_sets,
            lightmap_frame,
            film_frame,
            subsurface_frame,
            oit_frame,
        })
    }
}
