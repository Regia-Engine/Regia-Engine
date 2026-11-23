// SPDX-License-Identifier: MPL-2.0
//! The froxel light-cluster build and the irradiance probes of `FrameRenderer::render_into`
//! (ADR-1552 split by pass): the probes' CPU step before the frame's bind groups (the baked
//! volume's frame, or the GPU probes' update plan), then the cluster build and the probe trace,
//! relocate and blend. Moved out of `frame.rs` unchanged. The frame still borrows its pipeline
//! sets here, so the steps take the renderer fields they touch ([`ClusterProbeFields`],
//! [`ProbeBeginFields`]) instead of `self`.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use regia_core::glam::Mat4;

use super::{CLUSTER_COUNT, DDGI_QUERY_BASE, DDGI_STAGE_QUERY_BASE, FrameScene, IblGpu, Pipes, binding};
use crate::{DirectionalLightUniform, RenderError, TrackedDevice};

/// The renderer fields the cluster build and the probe update read and write.
pub(super) struct ClusterProbeFields<'a> {
    pub(super) frame_buf: &'a Option<wgpu::Buffer>,
    pub(super) lights_buf: &'a Option<wgpu::Buffer>,
    pub(super) counts_buf: &'a Option<wgpu::Buffer>,
    pub(super) indices_buf: &'a Option<wgpu::Buffer>,
    pub(super) cluster_bg:
        &'a mut crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    pub(super) ddgi: &'a mut crate::ddgi_gpu::DdgiRuntime,
    pub(super) shared_draw: &'a Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
    pub(super) ibl: &'a Option<IblGpu>,
}

/// What the cluster build and the probe update read from the frame.
pub(super) struct ClusterProbeInputs<'a> {
    /// The frame's scene (image-based lighting switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame's pipeline sets (cluster pipeline and layout, white fallback view).
    pub(super) pipes: &'a Pipes,
    /// The frame's timestamp query set, when GPU timing is on.
    pub(super) timer: &'a Option<(super::FrameStamps, f32)>,
    /// MSAA sample count (the probes trace the shared heightfield on single-sample frames only).
    pub(super) samples: u32,
    /// The renderer's IBL generation: the identity of the sky the probes trace.
    pub(super) ibl_generation: u64,
}

/// Records the cluster build, then the probe trace, relocate and blend (stamps
/// `DDGI_QUERY_BASE`, `+ 1` and 2; the stage stamps read 0 ms on a frame without an update).
pub(super) fn record_cluster_and_probes(
    fields: ClusterProbeFields<'_>,
    d: &TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &ClusterProbeInputs<'_>,
) -> Result<(), RenderError> {
    let ClusterProbeFields {
        frame_buf,
        lights_buf,
        counts_buf,
        indices_buf,
        cluster_bg,
        ddgi,
        shared_draw,
        ibl,
    } = fields;
    let ClusterProbeInputs {
        scene,
        pipes,
        timer,
        samples,
        ibl_generation,
    } = *inputs;
    // Froxel cluster build. The four buffers are created once, so the group is too.
    {
        let _span = tracing::info_span!("cluster_lights").entered();
        let _crumb = crate::breadcrumbs::frame_pass("cluster_lights");
        let frame_buf = frame_buf.clone().expect("frame buffer");
        let lights_buf = lights_buf.clone().expect("lights buffer");
        let counts_buf = counts_buf.clone().expect("counts");
        let indices_buf = indices_buf.clone().expect("indices");
        let cluster_layout = pipes.cluster_bgl.clone();
        let cluster_bg = cluster_bg.get_or_insert_with(
            (
                frame_buf.clone(),
                lights_buf.clone(),
                counts_buf.clone(),
                indices_buf.clone(),
            ),
            || {
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("cluster bg"),
                    layout: &cluster_layout,
                    entries: &[
                        binding(0, frame_buf.as_entire_binding()),
                        binding(1, lights_buf.as_entire_binding()),
                        binding(2, counts_buf.as_entire_binding()),
                        binding(3, indices_buf.as_entire_binding()),
                    ],
                })
            },
        );
        let mut pass = encoder.segmented_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("cluster lights"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipes.cluster_pipeline);
        pass.set_bind_group(0, &cluster_bg, &[]);
        let groups = CLUSTER_COUNT.div_ceil(64);
        pass.dispatch_workgroups(groups, 1, 1);
    }
    // GPU irradiance probes: trace, relocate, blend (stamps 27..28, inside the
    // cluster interval).
    if let Some((set, _)) = timer {
        set.write(encoder, DDGI_QUERY_BASE);
    }
    crate::pipeline_warm::warm_checkpoint()?;
    // Holes through the traced heightfield (tunnel mouths); a new mask wakes the probes.
    ddgi.set_holes(
        shared_draw
            .as_ref()
            .filter(|_| samples == 1)
            .and_then(|draw| draw.gi_holes()),
    );
    // The sky's identity keys its bind group and wakes settled probes when it changes: a
    // constant key kept the first IBL's view bound after a studio environment replaced it
    // (ADR-2000).
    let sky_key = match (scene.ibl, ibl.as_ref()) {
        (true, Some(_)) => ibl_generation,
        _ => u64::MAX,
    };
    ddgi.set_sky_identity(sky_key);
    // A probe volume whose traced scene stays unchanged stops updating once every finest probe
    // has taken its share of updates at the layout's divisor (ADR-5001; a fixed 600 frames held
    // the shipped tiers' start-up transient, ADR-4948 finding A).
    if ddgi.active && ddgi.settled_frames() < ddgi.freeze_frames() {
        let _span = tracing::info_span!("ddgi").entered();
        let _crumb = crate::breadcrumbs::frame_pass("ddgi");
        let heights = shared_draw
            .as_ref()
            .filter(|_| samples == 1)
            .and_then(|draw| draw.gi_heightfield());
        let sky_view = match (scene.ibl, ibl.as_ref()) {
            (true, Some(ibl)) => ibl.prefilter.clone(),
            _ => pipes.white_view.clone(),
        };
        let albedo_map = shared_draw
            .as_ref()
            .filter(|_| samples == 1)
            .and_then(|draw| draw.gi_albedo_map());
        ddgi.record(
            d,
            encoder,
            heights.as_ref().map(|(view, info)| (*view, info)),
            albedo_map,
            (&sky_view, sky_key),
            timer
                .as_ref()
                .map(|(set, _)| (set.lend(DDGI_STAGE_QUERY_BASE, 8), DDGI_STAGE_QUERY_BASE)),
        );
    } else if let Some((set, _)) = timer {
        // No update: the stage stamps read 0 ms.
        for q in DDGI_STAGE_QUERY_BASE..DDGI_STAGE_QUERY_BASE + 8 {
            set.write(encoder, q);
        }
    }
    if let Some((set, _)) = timer {
        set.write(encoder, DDGI_QUERY_BASE + 1);
        set.write(encoder, 2);
    }
    Ok(())
}

/// The renderer fields the probes' CPU step reads and writes.
pub(super) struct ProbeBeginFields<'a> {
    pub(super) ddgi: &'a mut crate::ddgi_gpu::DdgiRuntime,
    pub(super) baked_gi: &'a mut crate::baked_gi::BakedGiRuntime,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) shared_draw: &'a Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
    pub(super) gi_lookup: crate::ddgi::GiLookupResolution,
    /// The visibility pass: its persistent instance sets enter the probes' scene (ADR-5457).
    pub(super) visbuffer: &'a crate::visbuffer::VisBufferPass,
}

/// What the probes' CPU step reads from the frame.
pub(super) struct ProbeBeginInputs<'a> {
    /// The local origin in the world-anchored space (`FrameRenderer::origin_offset`).
    pub(super) origin_offset: [f32; 3],
    /// The frame's scene (eye, meshes, ambient, image-based lighting switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame's sun.
    pub(super) directional: &'a DirectionalLightUniform,
    /// The share of the sun the probes' ray hits keep under the cloud layer
    /// (`OutdoorState::probe_sun_share`, ADR-5944 item 9); 1 without the cloud transmittance map.
    pub(super) sun_share: f32,
    /// The sky body's fill light, if the sky has one.
    pub(super) fill_light: Option<[[f32; 3]; 2]>,
    /// The image light's ground plane, if the frame has one: its `env_ground` uniform and its
    /// radiance (`StudioState::ground_uniforms`, ADR-5716).
    pub(super) env_ground: Option<([f32; 4], [f32; 3])>,
    /// The frame's extra directional lights, packed as `LightsViewsUniform::extra_dir` (ADR-7254).
    pub(super) extra_directional: [[f32; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL],
    /// The jittered projection times the view.
    pub(super) view_proj: Mat4,
    /// Internal (pre-upscale) render size.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render size.
    pub(super) internal_h: u32,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// The GI quality asks for the probes.
    pub(super) gi_probes: bool,
    /// The baked volume replaces the probes this frame (if its upload goes through).
    pub(super) baked_on: bool,
    /// The SSAO kernel runs this frame.
    pub(super) ssao_active: bool,
    /// The frame renders a texture view (no probe update).
    pub(super) view_pass: bool,
    /// The scene renders to a linear HDR target.
    pub(super) linear_hdr: bool,
}

/// The probes' CPU work and uploads, before the frame's bind groups: the baked volume's frame
/// (when it takes the probes' place) or the GPU probes' update plan. Returns whether the baked
/// volume lights this frame.
pub(super) fn begin_probes(
    fields: ProbeBeginFields<'_>,
    d: &TrackedDevice,
    inputs: &ProbeBeginInputs<'_>,
) -> bool {
    let ProbeBeginFields {
        ddgi,
        baked_gi,
        belt,
        shared_draw,
        gi_lookup,
        visbuffer,
    } = fields;
    let ProbeBeginInputs {
        scene,
        directional,
        sun_share,
        fill_light,
        env_ground,
        extra_directional,
        view_proj,
        internal_w,
        internal_h,
        samples,
        gi_probes,
        baked_on,
        ssao_active,
        view_pass,
        linear_hdr,
        origin_offset,
    } = *inputs;
    // The baked volume lights this frame (`baked_on` and its upload went through).
    let mut baked_lit = false;
    // GPU irradiance probes: CPU work and uploads (the trace is recorded after the
    // cluster build).
    let ddgi_hf = shared_draw
        .as_ref()
        .filter(|_| samples == 1)
        .and_then(|draw| draw.gi_heightfield())
        .map(|(_, info)| info);
    {
        let to_sun_len = (directional.direction[0].powi(2)
            + directional.direction[1].powi(2)
            + directional.direction[2].powi(2))
        .sqrt();
        let sun_on = directional.illuminance > 0.0 && to_sun_len > 1e-6 && directional.direction[1] < 0.0;
        let light = crate::ddgi::DdgiLight {
            to_sun: sun_on.then(|| directional.direction.map(|c| -c / to_sun_len)),
            sun: directional.color.map(|c| c * directional.illuminance),
            sky_scale: if scene.ibl { crate::SKY_IBL_SCALE } else { 0.0 },
            sky_lod: ddgi.settings.sky_lod,
            flat_sky: [scene.ambient_lux / std::f32::consts::PI; 3],
            fill: fill_light,
            // The plane belongs to the image light: a frame without it traces the flat sky.
            ground: env_ground.filter(|_| scene.ibl),
            extra: crate::ddgi::DdgiLight::extra_from_rows(&extra_directional),
        };
        // Opt-in (`FrameQuality::gi_probes`, `set_ddgi_enabled`). Probes light in physical
        // units; the LDR lab path pre-exposes its ambient instead, so they need linear HDR.
        let wants = gi_probes;
        // The screen lookup (half resolution unless `set_gi_lookup_resolution` measures
        // another) needs the single-sample depth prepass; without it the probes are off.
        let screen = (scene.depth_prepass && samples == 1).then(|| {
            let half = gi_lookup.lookup_size(internal_w, internal_h);
            crate::ddgi_gpu::ScreenParamsGpu {
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                eye: [scene.eye[0], scene.eye[1], scene.eye[2], 0.0],
                size: [internal_w, internal_h, half.0, half.1],
                origin: [origin_offset[0], origin_offset[1], origin_offset[2], 0.0],
            }
        });
        // The probes reach the shading through the half-resolution screen lookup only, so
        // they also need the single-sample depth prepass it reads.
        // A texture view updates no probes (they follow the main camera); its frame was
        // started with them inactive, so it shades with the ambient.
        // A baked map's volume takes the probes' place (they stay off and keep no work).
        if baked_on && let Some(params) = screen.as_ref() {
            ddgi.active = false;
            // The capsules leave the SSAO's share of their occlusion to it (ADR-1104).
            baked_gi.ssao_radius = if ssao_active {
                crate::ssao::SSAO_RADIUS_M
            } else {
                0.0
            };
            baked_lit = baked_gi.begin(
                d,
                belt,
                params,
                &crate::BakedGiFrameLights {
                    sun: light.to_sun.map(|_| light.sun),
                    to_sun: light.to_sun,
                    fill: light.fill.map(|f| f[1]),
                },
                gi_lookup,
            );
        }
        if !view_pass && !baked_lit {
            // The persistent instance sets draw beside the frame's meshes while the visibility
            // path runs (the traced sun's rule, `sun_trace`): the probes trace them too, or a
            // mesh drawn as a set would light and shade unlike the same mesh placed (ADR-5457).
            let sets: Vec<crate::ddgi::ProbeSet<'_>> =
                if scene.quality.visibility_buffer && visbuffer.has_persistent_sets() {
                    visbuffer.probe_sets().collect()
                } else {
                    Vec::new()
                };
            // ADR-5944 item 9: the tracer lights a ray hit with the sun through its own shadow
            // rays, which do not read the cloud transmittance map. Under the map the hits keep
            // the share of the sun the layer leaves on the ground on average (times exactly 1 on
            // every other frame), so the probes gather no sunlit ground's bounce under a cover.
            let light = crate::ddgi::DdgiLight {
                sun: light.sun.map(|c| c * sun_share),
                ..light
            };
            ddgi.begin(
                d,
                belt,
                wants,
                linear_hdr && screen.is_some(),
                scene.eye,
                &light,
                &crate::ddgi::ProbeGeometry {
                    meshes: &scene.meshes,
                    sets: &sets,
                },
                ddgi_hf.as_ref(),
                screen,
                gi_lookup,
            );
        }
    }
    baked_lit
}
