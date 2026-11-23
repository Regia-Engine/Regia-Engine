// SPDX-License-Identifier: MPL-2.0
//! Screen-space GI and reflections (M15b).
//!
//! Both passes are fullscreen draws on the HDR colour, after the forward pass and
//! before tonemap. They run only when the frame asked for them and a single-sample
//! depth prepass produced a depth texture the shader can load. Probe rays stay on
//! the CPU; this module only samples the uploaded field indirectly (the forward
//! shader) and the captured reflection probes (the SSR pass).
//!
//! Pipelines are created on first use so a frame that leaves the passes off never
//! compiles them.

use crate::frame_work::counted::*;
use std::borrow::Cow;

use crate::reflections::{EQUIRECT_HEIGHT, EQUIRECT_WIDTH, ReflectionProbe, select_probes};
use crate::{RenderError, shader_lib::compose_source};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SsgiParams {
    screen: [f32; 4],
    proj: [f32; 4],
    noise: [f32; 4],
    /// x = 1: the half-resolution upsample weighs taps by relative linear depth (ADR-1033).
    up: [f32; 4],
}

/// World radius (m) of the screen-space bounce gather.
pub const SSGI_RADIUS_M: f32 = 1.5;
/// Strength of the screen-space bounce (1 = the estimate as computed). Below 1 because
/// the gathered radiance includes view-dependent specular (a glossy floor reflecting a
/// bright window would otherwise light the walls as if it were diffuse).
pub const SSGI_STRENGTH: f32 = 0.5;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SsrProbeGpu {
    center: [f32; 4],
    half: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SsrParams {
    inv_view_proj: [[f32; 4]; 4],
    view_proj: [[f32; 4]; 4],
    camera: [f32; 4],
    screen: [f32; 4],
    misc: [f32; 4],
    probes: [SsrProbeGpu; 4],
    /// The local reflection captures' boxes (ADR-2860): a miss inside one keeps the forward
    /// pass's HDR capture instead of the probe. All zero without captures.
    captures: [SsrProbeGpu; 4],
}

/// A full-resolution colour target of one pass ("ssgi color", "ssr color").
struct ColorTarget {
    view: wgpu::TextureView,
    size: (u32, u32),
    format: wgpu::TextureFormat,
}

/// Frames without any reflection pass after which the reflection targets (the "ssr color"
/// output and the stochastic SSR set) are freed: a landscape map with nothing glossy in view
/// keeps no full-resolution reflection memory.
pub(crate) const REFLECTION_IDLE_FRAMES: u32 = 60;

struct HalfTarget {
    view: wgpu::TextureView,
    size: (u32, u32),
    format: wgpu::TextureFormat,
}

/// Persistent screen-space GI and reflection targets.
#[derive(Default)]
pub struct HighEndPass {
    ssgi_pipe: Option<wgpu::RenderPipeline>,
    /// The table of the next frames, `None` with the knob off (ADR-2075).
    blue_noise: Option<std::sync::Arc<crate::blue_noise::BlueNoiseGpu>>,
    /// The gather's blue-noise variant and the format it was built for.
    ssgi_blue_pipe: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    /// The colour format [`Self::ssgi_pipe`] was built for (the frame's HDR target).
    ssgi_format: Option<wgpu::TextureFormat>,
    ssgi_up_pipe: Option<wgpu::RenderPipeline>,
    /// The colour format [`Self::ssgi_up_pipe`] was built for.
    ssgi_up_format: Option<wgpu::TextureFormat>,
    ssr_pipe: Option<wgpu::RenderPipeline>,
    /// The colour format [`Self::ssr_pipe`] was built for.
    ssr_format: Option<wgpu::TextureFormat>,
    ssgi_bgl: Option<wgpu::BindGroupLayout>,
    ssgi_layout: Option<wgpu::PipelineLayout>,
    ssgi_module: Option<wgpu::ShaderModule>,
    ssr_bgl: Option<wgpu::BindGroupLayout>,
    ssgi_params: Option<wgpu::Buffer>,
    ssr_params: Option<wgpu::Buffer>,
    /// SSGI output, allocated by the first SSGI frame.
    ssgi_target: Option<ColorTarget>,
    /// Reflection output ("ssr color"), allocated by the first SSR or SSSR frame.
    ssr_target: Option<ColorTarget>,
    /// Consecutive frames without a reflection pass ([`Self::note_reflections`]).
    reflection_idle: u32,
    /// The input of the last stochastic SSR pass: what it returns, unchanged, while it is dormant
    /// ([`Self::reflection_views`], ADR-8122).
    reflection_input: Option<wgpu::TextureView>,
    half: Option<HalfTarget>,
    probes: Option<(wgpu::Texture, wgpu::TextureView)>,
    probe_sampler: Option<wgpu::Sampler>,
    /// Last uploaded SSGI params. A static camera skips the copy.
    ssgi_param_bytes: Vec<u8>,
    /// Last uploaded SSR params.
    ssr_param_bytes: Vec<u8>,
    /// Gather layout: params, colour, depth, the glass front layer's transmittance (`front`).
    ssgi_cache: crate::bind_cache::BindCache<(
        wgpu::Buffer,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
    )>,
    /// Upsample layout: params, half-resolution bounce, depth, full-resolution base colour, the
    /// glass front layer's transmittance.
    ssgi_up_bgl: Option<wgpu::BindGroupLayout>,
    ssgi_up_cache: crate::bind_cache::BindCache<(
        wgpu::Buffer,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
    )>,
    ssr_cache: crate::bind_cache::BindCache<(
        wgpu::Buffer,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
    )>,
    /// Last equirect layers uploaded. An unchanged set skips the texture copy.
    probe_bytes: Vec<Vec<u8>>,
    /// Stochastic SSR (Desktop High and Ultra).
    sssr: crate::sssr::SssrPass,
    /// The last [`Self::record_ssgi`] recorded the gather (not only the composite).
    ssgi_gathered: bool,
    /// Texel step (`1`, `2` or `4`) of the gather this pass last encoded. `0` before the first
    /// gather, so a change of step composites a new gather instead of the one it kept.
    ssgi_gather_step: u32,
    /// The gather with the radiance cache behind it (`ssgi_cached.wgsl`), built by the first frame
    /// that reads the cache; a frame without it never compiles this.
    ssgi_cached_pipe: Option<wgpu::RenderPipeline>,
    /// The colour format [`Self::ssgi_cached_pipe`] was built for.
    ssgi_cached_format: Option<wgpu::TextureFormat>,
    /// The local captures frames read (ADR-2860): centre and weight per layer.
    capture_centers: [[f32; 4]; 4],
    /// Their half extents and fade distances.
    capture_halves: [[f32; 4]; 4],
}

impl HighEndPass {
    /// The local reflection captures the frame's forward pass reads (ADR-2860, the `capture_center`
    /// and `capture_half` uniforms): where their boxes cover a point, the screen reflection
    /// passes' misses keep the forward pass's HDR capture instead of blending the coarse probe.
    /// All zero (the default) leaves both passes exactly as before.
    #[allow(
        dead_code,
        reason = "called by the frame hookup, which ships as a patch under the wave's frame rule (ADR-2860 § 5)"
    )]
    pub(crate) fn set_local_capture_boxes(&mut self, centers: [[f32; 4]; 4], halves: [[f32; 4]; 4]) {
        self.capture_centers = centers;
        self.capture_halves = halves;
    }

    fn capture_boxes(&self) -> [SsrProbeGpu; 4] {
        std::array::from_fn(|i| SsrProbeGpu {
            center: self.capture_centers[i],
            half: self.capture_halves[i],
        })
    }

    /// Drops this pass's temporal history (camera cut, `FrameRenderer::camera_cut`): the
    /// stochastic SSR history and its previous view. SSGI and the linear SSR keep none of their
    /// own: their reused gather is guarded by the static-screen key, which the cut clears.
    pub(crate) fn reset_history(&mut self) {
        self.sssr.reset_history();
    }

    /// Called once per frame: whether a reflection pass (SSR or stochastic SSR) is wanted.
    /// After [`REFLECTION_IDLE_FRAMES`] frames without one, the reflection output and the
    /// stochastic SSR targets are freed (they come back on the next reflective frame).
    pub(crate) fn note_reflections(&mut self, used: bool) {
        if used {
            self.reflection_idle = 0;
            return;
        }
        self.reflection_idle = self.reflection_idle.saturating_add(1);
        if self.reflection_idle >= REFLECTION_IDLE_FRAMES {
            self.release_reflection_targets();
        }
    }

    /// Complete frames (ADR-2582): the stochastic SSR pass never goes dormant on a readback.
    pub(crate) fn set_never_sleep(&mut self, on: bool) {
        self.sssr.set_never_sleep(on);
    }

    /// Maps the stochastic SSR counters this frame copied; call after the frame's submit.
    pub(crate) fn after_submit(&mut self) {
        self.sssr.after_submit();
    }

    /// The newest stochastic SSR ray and tile counts read back.
    pub(crate) fn sssr_stats(&self) -> Option<crate::sssr::SssrStats> {
        self.sssr.stats()
    }

    /// Memory pressure: frees the stochastic SSR targets and the reflection output when the
    /// pass is dormant (nothing glossy in view, so neither is in use). Returns the bytes freed.
    /// A pass in use keeps everything: freeing it would only re-create it next frame.
    pub(crate) fn trim_memory(&mut self) -> u64 {
        if !self.sssr.dormant() {
            return 0;
        }
        let before = self.reflection_target_bytes();
        self.sssr.trim();
        if self.ssr_target.take().is_some() {
            self.ssr_cache.clear();
        }
        before - self.reflection_target_bytes()
    }

    /// Whether the stochastic SSR pass is dormant (only its probe runs).
    pub(crate) fn sssr_dormant(&self) -> bool {
        self.sssr.dormant()
    }

    /// Bytes of the stochastic SSR targets alone (without the shared "ssr color" output).
    pub(crate) fn sssr_target_bytes(&self) -> u64 {
        self.sssr.target_bytes()
    }

    /// Frees the reflection output and the stochastic SSR targets now.
    pub(crate) fn release_reflection_targets(&mut self) {
        if self.ssr_target.take().is_some() {
            self.ssr_cache.clear();
        }
        self.reflection_input = None;
        self.sssr.release_targets();
    }

    /// The views the stochastic SSR pass hands the frame as its output on one frame or another
    /// without reallocating either (ADR-8122): its input, which it returns while dormant (no
    /// glossy pixel in its recent readbacks), and the "ssr color" target it writes while awake.
    /// The temporal upscaler builds its bind groups for both, so the pass falling asleep or
    /// waking up after the warm creates none. `[None, None]` before the first pass.
    pub(crate) fn reflection_views(&self) -> [Option<wgpu::TextureView>; 2] {
        [
            self.reflection_input.clone(),
            self.ssr_target.as_ref().map(|t| t.view.clone()),
        ]
    }

    /// Bytes of the reflection targets currently allocated (the "ssr color" output plus the
    /// stochastic SSR set), for tests and reports.
    #[must_use]
    pub(crate) fn reflection_target_bytes(&self) -> u64 {
        let ssr = self.ssr_target.as_ref().map_or(0, |t| {
            u64::from(t.size.0) * u64::from(t.size.1) * u64::from(t.format.block_copy_size(None).unwrap_or(8))
        });
        ssr + self.sssr.target_bytes()
    }

    /// Whether the last [`Self::record_ssgi`] encoded the bounce gather. `false` when a
    /// held view only composited its kept gather.
    pub(crate) fn ssgi_gathered(&self) -> bool {
        self.ssgi_gathered
    }

    /// Adds a screen-space one-bounce diffuse on top of `hdr` and returns the new view.
    /// `proj` is `[projection[0][0], projection[1][1], near, far]`.
    ///
    /// `half` gathers the bounce at half resolution and joint-bilateral-upsamples it onto
    /// the full-resolution colour. (It used to gather `colour + bounce` at half resolution
    /// and upsample that, so Desktop High showed one forward sample per 2×2 pixels.)
    /// `ScreenSpaceGiSettings::quarter_gather` (ADR-3964, default off) uses texel step 4
    /// instead, on the same upsample. `reuse_gather` (a reduced gather only, same view and
    /// inputs as the last call, and the same step) keeps the last gather and only composites
    /// it onto this frame's colour: a held view skips the gather but still shows each new
    /// jittered frame, so TAA converges. A step change gathers again. `screen.xy` in the
    /// uniform stays the full size so
    /// the gather still reconstructs view positions in full-resolution pixels.
    /// One uniform upload serves both passes (`noise.y` is the texel step,
    /// `noise.zw` the gather size). `relative_depth` (reduced gathers only, a measurement
    /// variant, ADR-1033) weighs the upsample's taps by relative linear depth instead of raw depth.
    /// `cache` is the radiance cache's read group: the gather is then `fs_cached`
    /// (`ssgi_cached.wgsl`) instead of `fs_main`; the upsample and the targets are the same.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all)]
    pub fn record_ssgi(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        hdr: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        front: &wgpu::TextureView,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        proj: [f32; 4],
        noise_index: f32,
        half: bool,
        reuse_gather: bool,
        relative_depth: bool,
        cache: Option<&wgpu::BindGroup>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_ssgi(device, format)?;
        self.ensure_ssgi_blue_noise(device, format)?;
        self.ensure_ssgi_target(device, width, height, format);
        let settings = crate::gi::ssgi_settings();
        let step = crate::gi::ssgi_gather_step(half, settings);
        let scale = step as f32;
        let (gather_w, gather_h) = crate::gi::ssgi_gather_size(width, height, step);
        crate::gi::note_ssgi_gather_step(step);
        let step_changed = self.ssgi_gather_step != step;
        self.ssgi_gather_step = step;
        let params = SsgiParams {
            screen: [width as f32, height as f32, SSGI_RADIUS_M, SSGI_STRENGTH],
            proj,
            noise: [noise_index, scale, gather_w as f32, gather_h as f32],
            up: [if relative_depth { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
        };
        let params_buf = self.ssgi_params.clone().expect("ssgi params");
        belt.write_if_changed(
            device,
            encoder,
            &params_buf,
            bytemuck::bytes_of(&params),
            &mut self.ssgi_param_bytes,
        );
        let Some(out) = self.ssgi_target.as_ref().map(|t| t.view.clone()) else {
            return Err(RenderError::Shader("ssgi: no target".into()));
        };
        if step > 1 {
            self.ensure_upsample(device, format)?;
            let fresh = self
                .half
                .as_ref()
                .is_none_or(|h| h.size != (gather_w.max(1), gather_h.max(1)) || h.format != format)
                || step_changed;
            self.ensure_half(device, gather_w, gather_h, format)?;
            let half_view = self.half.as_ref().expect("ssgi half").view.clone();
            self.ssgi_gathered = !reuse_gather || fresh;
            if self.ssgi_gathered {
                let gather_bg = self.ssgi_bind(device, hdr, depth, front);
                match (cache, self.ssgi_cached_pipe.as_ref()) {
                    (Some(cache), Some(pipe)) => {
                        draw_fs_cached(encoder, pipe, &gather_bg, cache, &half_view);
                    }
                    _ => self.draw_gather(encoder, &gather_bg, &half_view),
                }
            }
            let up_bg = self.upsample_bind(device, &half_view, depth, hdr, front);
            draw_fs(
                encoder,
                "ssgi upsample",
                self.ssgi_up_pipe.as_ref().expect("ssgi upsample"),
                &up_bg,
                &out,
            );
        } else {
            self.ssgi_gathered = true;
            let bg = self.ssgi_bind(device, hdr, depth, front);
            match (cache, self.ssgi_cached_pipe.as_ref()) {
                (Some(cache), Some(pipe)) => draw_fs_cached(encoder, pipe, &bg, cache, &out),
                _ => self.draw_gather(encoder, &bg, &out),
            }
        }
        Ok(out)
    }

    /// Reflects the floor with SSR and/or box-projected probes. Returns the new view.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all)]
    pub fn record_ssr(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        hdr: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        inv_view_proj: [[f32; 4]; 4],
        view_proj: [[f32; 4]; 4],
        camera: [f32; 3],
        probes: &[ReflectionProbe],
        budget: u32,
        probe_scale: f32,
        ssr_enabled: bool,
    ) -> Result<wgpu::TextureView, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_ssr(device, format)?;
        self.ensure_ssr_target(device, width, height, format);
        let (gpu_probes, chosen) = self.upload_probes(device, belt, encoder, probes, camera, budget)?;
        let params = SsrParams {
            inv_view_proj,
            view_proj,
            camera: [camera[0], camera[1], camera[2], 1.0],
            // Thickness 0.06 NDC and a 0.22 m step: sixteen steps reach a few
            // metres, which is the room scale these probes are captured at.
            screen: [width as f32, height as f32, 0.06, 0.22],
            misc: [
                chosen as f32,
                probe_scale,
                if ssr_enabled { 1.0 } else { 0.0 },
                0.72,
            ],
            probes: gpu_probes,
            captures: self.capture_boxes(),
        };
        let params_buf = self.ssr_params.clone().expect("ssr params");
        belt.write_if_changed(
            device,
            encoder,
            &params_buf,
            bytemuck::bytes_of(&params),
            &mut self.ssr_param_bytes,
        );
        let probe_view = self.probes.as_ref().expect("probe array").1.clone();
        let hdr_key = hdr.clone();
        let depth_key = depth.clone();
        let probe_key = probe_view.clone();
        let probe_view = &probe_view;
        let layout = self.ssr_bgl.clone().expect("ssr bgl");
        let sampler = self.probe_sampler.clone().expect("probe sampler");
        let bg =
            self.ssr_cache
                .get_or_insert_with((params_buf.clone(), hdr_key, depth_key, probe_key), || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("ssr"),
                        layout: &layout,
                        entries: &[
                            buf_entry(0, &params_buf),
                            view_entry(1, hdr),
                            depth_entry(2, depth),
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(probe_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::Sampler(&sampler),
                            },
                        ],
                    })
                });
        let Some(out) = self.ssr_target.as_ref().map(|t| t.view.clone()) else {
            return Err(RenderError::Shader("ssr: no target".into()));
        };
        draw_fs(
            encoder,
            "ssr",
            self.ssr_pipe.as_ref().expect("ssr pipe"),
            &bg,
            &out,
        );
        Ok(out)
    }

    /// Stochastic SSR (Desktop High and Ultra): FidelityFX SSSR plus its reflection denoiser,
    /// with the captured probes as the miss fallback. `hdr` alpha must carry the forward pass's
    /// surface code ([`crate::sssr::pack_surface_code`]). Returns the new view (the "ssr
    /// color" target). `stamps` = a query set
    /// and the first of three slots: after the classification, the trace and the denoiser.
    /// While the pass is dormant (no glossy pixel in its recent readbacks) this runs only a
    /// quarter-resolution probe and returns `hdr` itself; the "ssr color" target is freed.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all)]
    pub fn record_sssr(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        hdr: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        view_proj: regia_core::glam::Mat4,
        proj: regia_core::glam::Mat4,
        camera: [f32; 3],
        forward: [f32; 3],
        near: f32,
        frame_index: u64,
        probes: &[ReflectionProbe],
        budget: u32,
        probe_scale: f32,
        stamps: Option<(&wgpu::QuerySet, u32)>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.reflection_input = Some(hdr.clone());
        self.ensure_probes(device)?;
        // Dormant (nothing glossy in the last readbacks): only the probe runs. The output and
        // the pass's targets stay allocated (ADR-168), unless memory pressure freed them
        // (`trim_memory`), in which case the output goes too.
        let sleeping = self.sssr.sleeping();
        if sleeping && self.sssr.target_bytes() == 0 && self.ssr_target.take().is_some() {
            self.ssr_cache.clear();
        }
        let out = if sleeping {
            hdr.clone()
        } else {
            self.ensure_ssr_target(device, width, height, format);
            let Some(out) = self.ssr_target.as_ref().map(|t| t.view.clone()) else {
                return Err(RenderError::Shader("sssr: no output target".into()));
            };
            out
        };
        let (gpu_probes, chosen) = if sleeping {
            (
                [SsrProbeGpu {
                    center: [0.0; 4],
                    half: [0.0; 4],
                }; 4],
                0,
            )
        } else {
            self.upload_probes(device, belt, encoder, probes, camera, budget)?
        };
        let (probe_view, sampler) = match (self.probes.as_ref(), self.probe_sampler.as_ref()) {
            (Some((_, view)), Some(sampler)) => (view.clone(), sampler.clone()),
            _ => return Err(RenderError::Shader("sssr: probe array missing".into())),
        };
        let blue_noise = self.blue_noise.clone();
        let frame = crate::sssr::SssrFrame {
            blue_noise: blue_noise.as_ref(),
            hdr,
            depth,
            out: &out,
            stamps,
            width,
            height,
            format,
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            view_proj: view_proj.to_cols_array_2d(),
            proj_terms: [proj.z_axis.z, proj.w_axis.z],
            eye: camera,
            forward,
            near,
            frame_index,
            probes: gpu_probes.map(|p| crate::sssr::SssrProbe {
                center: p.center,
                half: p.half,
            }),
            probe_count: chosen,
            captures: self.capture_boxes().map(|p| crate::sssr::SssrProbe {
                center: p.center,
                half: p.half,
            }),
            probe_scale,
            probe_view: &probe_view,
            probe_sampler: &sampler,
        };
        if sleeping {
            self.sssr.record_probe(device, belt, encoder, &frame)?;
            return Ok(hdr.clone());
        }
        self.sssr.record(device, belt, encoder, &frame)
    }

    /// Picks up to four probes for `camera`, uploads their images when the set changed, and
    /// returns the shader records and the count.
    fn upload_probes(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        probes: &[ReflectionProbe],
        camera: [f32; 3],
        budget: u32,
    ) -> Result<([SsrProbeGpu; 4], u32), RenderError> {
        self.ensure_probes(device)?;
        let chosen = select_probes(probes, camera, budget);
        let mut gpu_probes = [SsrProbeGpu {
            center: [0.0; 4],
            half: [0.0; 4],
        }; 4];
        let mut images = vec![vec![0u8; (EQUIRECT_WIDTH * EQUIRECT_HEIGHT * 4) as usize]; 4];
        for (slot, blend) in chosen.iter().enumerate().take(4) {
            let probe = &probes[blend.index];
            gpu_probes[slot] = SsrProbeGpu {
                center: [probe.center[0], probe.center[1], probe.center[2], 1.0],
                half: [
                    probe.half_extent[0].abs().max(0.05),
                    probe.half_extent[1].abs().max(0.05),
                    probe.half_extent[2].abs().max(0.05),
                    blend.weight,
                ],
            };
            images[slot] = resample_equirect(&probe.equirect, probe.width, probe.height);
        }
        let Some((texture, _)) = self.probes.as_ref() else {
            return Err(RenderError::Shader("reflection probe array missing".into()));
        };
        if self.probe_bytes != images {
            for (layer, image) in images.iter().enumerate() {
                belt.write_texture(
                    device,
                    encoder,
                    texture,
                    0,
                    wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    image,
                    EQUIRECT_WIDTH,
                    EQUIRECT_HEIGHT,
                )?;
            }
            self.probe_bytes = images;
        }
        Ok((gpu_probes, chosen.len().min(4) as u32))
    }

    fn ensure_ssgi_target(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) {
        let size = (width.max(1), height.max(1));
        if self
            .ssgi_target
            .as_ref()
            .is_some_and(|t| t.size == size && t.format == format)
        {
            return;
        }
        let view = color_target(device, size.0, size.1, format, "ssgi color");
        self.ssgi_target = Some(ColorTarget { view, size, format });
        self.ssgi_cache.clear();
        self.ssgi_up_cache.clear();
    }

    fn ensure_ssr_target(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) {
        let size = (width.max(1), height.max(1));
        if self
            .ssr_target
            .as_ref()
            .is_some_and(|t| t.size == size && t.format == format)
        {
            return;
        }
        let view = color_target(device, size.0, size.1, format, "ssr color");
        self.ssr_target = Some(ColorTarget { view, size, format });
        self.ssr_cache.clear();
    }

    fn ensure_probes(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.probes.is_some() {
            return Ok(());
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("reflection probes"),
            size: wgpu::Extent3d {
                width: EQUIRECT_WIDTH,
                height: EQUIRECT_HEIGHT,
                depth_or_array_layers: 4,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("reflection probes"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        self.probe_sampler = Some(device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("reflection probe"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        }));
        self.probes = Some((texture, view));
        Ok(())
    }

    /// Moves `other`'s SSGI/SSR pipelines (with the layouts and parameter buffers built beside
    /// them) into a pass that has none yet (`FrameRenderer::adopt_warm_pipelines`). Parameter
    /// bytes are not moved, so the first frame rewrites the buffers.
    pub(crate) fn adopt_pipelines(&mut self, other: &mut HighEndPass) -> bool {
        let mut moved = false;
        if self.ssgi_pipe.is_none() && other.ssgi_pipe.is_some() {
            moved = true;
            self.ssgi_pipe = other.ssgi_pipe.take();
            self.ssgi_format = other.ssgi_format.take();
            self.ssgi_bgl = other.ssgi_bgl.take();
            self.ssgi_layout = other.ssgi_layout.take();
            self.ssgi_module = other.ssgi_module.take();
            self.ssgi_params = other.ssgi_params.take();
            self.ssgi_blue_pipe = other.ssgi_blue_pipe.take();
            if self.ssgi_up_pipe.is_none() {
                self.ssgi_up_pipe = other.ssgi_up_pipe.take();
                // The layout moves with its pipeline: a pipe without it panicked in
                // `upsample_bind` on the first half-resolution gather.
                self.ssgi_up_bgl = other.ssgi_up_bgl.take();
                self.ssgi_up_format = other.ssgi_up_format.take();
            }
            if self.ssgi_cached_pipe.is_none() {
                self.ssgi_cached_pipe = other.ssgi_cached_pipe.take();
                self.ssgi_cached_format = other.ssgi_cached_format.take();
            }
        }
        if self.ssr_pipe.is_none() && other.ssr_pipe.is_some() {
            moved = true;
            self.ssr_pipe = other.ssr_pipe.take();
            self.ssr_bgl = other.ssr_bgl.take();
            self.ssr_params = other.ssr_params.take();
            self.ssr_format = other.ssr_format.take();
        }
        moved
    }

    /// The colour format the plain gather was built for, once an SSGI frame drew.
    pub(crate) fn ssgi_format(&self) -> Option<wgpu::TextureFormat> {
        self.ssgi_format
    }

    /// Sets the blue-noise table of the next SSGI and stochastic SSR frames (ADR-2075); `None`
    /// runs their plain pipelines.
    pub(crate) fn set_blue_noise(&mut self, noise: Option<std::sync::Arc<crate::blue_noise::BlueNoiseGpu>>) {
        self.blue_noise = noise;
    }

    /// The gather's blue-noise variant for `format`, when the table is set.
    fn ensure_ssgi_blue_noise(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        let Some(noise) = self.blue_noise.as_ref() else {
            return Ok(());
        };
        if self.ssgi_blue_pipe.as_ref().is_some_and(|(f, _)| *f == format) {
            return Ok(());
        }
        let Some(bgl) = self.ssgi_bgl.as_ref() else {
            return Ok(());
        };
        let module = compose_source(
            include_str!("../shaders/ssgi.wgsl"),
            "ssgi.wgsl",
            crate::blue_noise::variant_defs(),
        )?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ssgi blue noise"),
            source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssgi blue noise layout"),
            bind_group_layouts: &[Some(bgl), Some(&noise.layout)],
            immediate_size: 0,
        });
        let pipe = fs_pipeline(device, &layout, &module, format, "fs_main", "ssgi blue noise");
        self.ssgi_blue_pipe = Some((format, pipe));
        Ok(())
    }

    /// One SSGI gather into `view`, through the blue-noise variant when the table is set.
    fn draw_gather(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        bg: &wgpu::BindGroup,
        view: &wgpu::TextureView,
    ) {
        match (&self.blue_noise, &self.ssgi_blue_pipe) {
            (Some(noise), Some((_, pipe))) => {
                draw_fs_groups(encoder, "ssgi", pipe, bg, Some(&noise.group), view)
            }
            _ => draw_fs(
                encoder,
                "ssgi",
                self.ssgi_pipe.as_ref().expect("ssgi pipe"),
                bg,
                view,
            ),
        }
    }

    /// Frame warms: the blue-noise variants of the gather and of stochastic SSR for the targets
    /// the warm frames drew. Returns how many pipelines that asks for.
    ///
    /// # Errors
    ///
    /// A variant's shader fails to compose.
    pub(crate) fn prewarm_blue_noise(&mut self, device: &wgpu::Device) -> Result<usize, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        let Some(noise) = self.blue_noise.clone() else {
            return Ok(0);
        };
        let mut asked = 0;
        if let Some(format) = self.ssgi_format {
            self.ensure_ssgi_blue_noise(device, format)?;
            asked += 1;
        }
        asked += self.sssr.prewarm_blue_noise(device, &noise)?;
        Ok(asked)
    }

    /// Frame warms (`FrameRenderer::prewarm_mid_play`): the half-resolution SSGI upsample, once
    /// the gather is built (an SSGI frame drew). Desktop High at exactly 0.67 with a 2048²
    /// cascade gathers at half resolution (`FrameQuality::ssgi_half_res`, DO27's 4K lever): a
    /// warm frame at native scale left the upsample to the first scaled frame. Returns whether
    /// it built it.
    ///
    /// # Errors
    ///
    /// The upsample pipeline failed to build.
    pub(crate) fn prewarm_half_res(&mut self, device: &wgpu::Device) -> Result<bool, RenderError> {
        let Some(format) = self.ssgi_format else {
            return Ok(false);
        };
        if self.ssgi_up_pipe.is_some() && self.ssgi_up_format == Some(format) {
            return Ok(false);
        }
        self.ensure_upsample(device, format)?;
        Ok(true)
    }

    fn ensure_ssgi(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> Result<(), RenderError> {
        if self.ssgi_pipe.is_some() && self.ssgi_format == Some(format) {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        // The layout, module and params carry no format: a later frame at another HDR
        // format only rebuilds the pipeline, so the cached bind groups stay valid.
        if self.ssgi_bgl.is_none() {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ssgi bgl"),
                entries: &[uniform_entry(0), float_tex(1), depth_tex(2), float_tex(4)],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ssgi layout"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let module = naga_module(device, "ssgi", include_str!("../shaders/ssgi.wgsl"), "ssgi.wgsl")?;
            self.ssgi_params = Some(uniform_buffer(
                device,
                "ssgi params",
                size_of::<SsgiParams>() as u64,
            ));
            self.ssgi_bgl = Some(bgl);
            self.ssgi_layout = Some(layout);
            self.ssgi_module = Some(module);
        }
        let layout = self.ssgi_layout.clone().expect("ssgi layout");
        let module = self.ssgi_module.as_ref().expect("ssgi module");
        self.ssgi_pipe = Some(fs_pipeline(device, &layout, module, format, "fs_main", "ssgi"));
        self.ssgi_format = Some(format);
        Ok(())
    }

    /// Builds the gather that reads the radiance cache (`read_layout` is the cache's read group),
    /// with the plain gather's targets and parameters. Returns whether it built it.
    pub(crate) fn ensure_ssgi_cached(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        read_layout: &wgpu::BindGroupLayout,
    ) -> Result<bool, RenderError> {
        if self.ssgi_cached_pipe.is_some() && self.ssgi_cached_format == Some(format) {
            return Ok(false);
        }
        self.ensure_ssgi(device, format)?;
        let device = &crate::device::TrackedDevice::observe(device);
        let bgl = self.ssgi_bgl.as_ref().expect("ssgi bgl");
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssgi cached layout"),
            bind_group_layouts: &[Some(bgl), Some(read_layout)],
            immediate_size: 0,
        });
        let source = crate::radiance_cache::gather_source();
        let module = naga_module(device, "ssgi cached", &source, "ssgi_cached.wgsl")?;
        self.ssgi_cached_pipe = Some(fs_pipeline(
            device,
            &layout,
            &module,
            format,
            "fs_cached",
            "ssgi cached",
        ));
        self.ssgi_cached_format = Some(format);
        Ok(true)
    }

    fn ensure_upsample(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        if self.ssgi_up_pipe.is_some() && self.ssgi_up_bgl.is_some() && self.ssgi_up_format == Some(format) {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        let module = self.ssgi_module.as_ref().expect("ssgi module");
        // A pipeline without its layout (the old adopt path moved one alone) is rebuilt
        // with it, as is one at another colour format.
        let bgl = match self.ssgi_up_bgl.clone() {
            Some(bgl) => bgl,
            None => {
                let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("ssgi upsample bgl"),
                    entries: &[
                        uniform_entry(0),
                        float_tex(1),
                        depth_tex(2),
                        float_tex(3),
                        float_tex(4),
                    ],
                });
                self.ssgi_up_bgl = Some(bgl.clone());
                bgl
            }
        };
        let up_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssgi upsample layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        self.ssgi_up_pipe = Some(fs_pipeline(
            device,
            &up_layout,
            module,
            format,
            "fs_upsample",
            "ssgi upsample",
        ));
        self.ssgi_up_format = Some(format);
        Ok(())
    }

    fn upsample_bind(
        &mut self,
        device: &crate::device::TrackedDevice,
        gather: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        base: &wgpu::TextureView,
        front: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let params_buf = self.ssgi_params.clone().expect("ssgi params");
        let layout = self.ssgi_up_bgl.clone().expect("ssgi upsample bgl");
        self.ssgi_up_cache.get_or_insert_with(
            (
                params_buf.clone(),
                gather.clone(),
                depth.clone(),
                base.clone(),
                front.clone(),
            ),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ssgi upsample"),
                    layout: &layout,
                    entries: &[
                        buf_entry(0, &params_buf),
                        view_entry(1, gather),
                        depth_entry(2, depth),
                        view_entry(3, base),
                        view_entry(4, front),
                    ],
                })
            },
        )
    }

    fn ensure_half(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        let size = (width.max(1), height.max(1));
        if self
            .half
            .as_ref()
            .is_some_and(|half| half.size == size && half.format == format)
        {
            return Ok(());
        }
        let view = color_target(device, size.0, size.1, format, "ssgi half");
        self.half = Some(HalfTarget { view, size, format });
        Ok(())
    }

    fn ssgi_bind(
        &mut self,
        device: &crate::device::TrackedDevice,
        hdr: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        front: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let params_buf = self.ssgi_params.clone().expect("ssgi params");
        let layout = self.ssgi_bgl.clone().expect("ssgi bgl");
        self.ssgi_cache.get_or_insert_with(
            (params_buf.clone(), hdr.clone(), depth.clone(), front.clone()),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ssgi"),
                    layout: &layout,
                    entries: &[
                        buf_entry(0, &params_buf),
                        view_entry(1, hdr),
                        depth_entry(2, depth),
                        view_entry(4, front),
                    ],
                })
            },
        )
    }

    fn ensure_ssr(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> Result<(), RenderError> {
        if self.ssr_pipe.is_some() && self.ssr_format == Some(format) {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        // The layout carries no format: a later frame at another HDR format only rebuilds
        // the pipeline, so the cached bind groups stay valid.
        let bgl = match self.ssr_bgl.clone() {
            Some(bgl) => bgl,
            None => {
                let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("ssr bgl"),
                    entries: &[
                        uniform_entry(0),
                        float_tex(1),
                        depth_tex(2),
                        wgpu::BindGroupLayoutEntry {
                            binding: 3,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2Array,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 4,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });
                self.ssr_params = Some(uniform_buffer(
                    device,
                    "ssr params",
                    size_of::<SsrParams>() as u64,
                ));
                self.ssr_bgl = Some(bgl.clone());
                bgl
            }
        };
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssr layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let module = naga_module(device, "ssr", include_str!("../shaders/ssr.wgsl"), "ssr.wgsl")?;
        self.ssr_pipe = Some(fs_pipeline(device, &layout, &module, format, "fs_main", "ssr"));
        self.ssr_format = Some(format);
        Ok(())
    }
}

fn resample_equirect(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (EQUIRECT_WIDTH * EQUIRECT_HEIGHT * 4) as usize];
    // `width` and `height` are public host fields: the byte count is computed in u64 so
    // a product past `u32` cannot wrap the short-buffer guard (it used to panic in debug
    // and pass the guard in release).
    let texels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || texels > (usize::MAX / 4) as u64 || src.len() < (texels * 4) as usize {
        return dst;
    }
    for y in 0..EQUIRECT_HEIGHT {
        for x in 0..EQUIRECT_WIDTH {
            let sx =
                ((u64::from(x) * u64::from(width)) / u64::from(EQUIRECT_WIDTH)).min(u64::from(width - 1));
            let sy =
                ((u64::from(y) * u64::from(height)) / u64::from(EQUIRECT_HEIGHT)).min(u64::from(height - 1));
            let s = ((sy * u64::from(width) + sx) * 4) as usize;
            let d = ((y * EQUIRECT_WIDTH + x) * 4) as usize;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    dst
}

fn naga_module(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    file: &str,
) -> Result<wgpu::ShaderModule, RenderError> {
    let module = compose_source(source, file, std::collections::HashMap::new())?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    }))
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn color_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        // COPY_SRC: the frame's translucent decals composite over a copy of this output.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn fs_pipeline(
    device: &crate::device::TrackedDevice,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    entry: &'static str,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

/// `draw_fs` with the radiance cache's read group as group 1.
fn draw_fs_cached(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
    cache: &wgpu::BindGroup,
    view: &wgpu::TextureView,
) {
    let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("ssgi cached"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.counted_set_pipeline(pipeline);
    pass.counted_set_bind_group(0, bg, &[]);
    pass.counted_set_bind_group(1, cache, &[]);
    pass.counted_draw(0..3, 0..1);
}

fn draw_fs(
    encoder: &mut wgpu::CommandEncoder,
    label: &str,
    pipeline: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
    view: &wgpu::TextureView,
) {
    draw_fs_groups(encoder, label, pipeline, bg, None, view);
}

fn draw_fs_groups(
    encoder: &mut wgpu::CommandEncoder,
    label: &str,
    pipeline: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
    group1: Option<&wgpu::BindGroup>,
    view: &wgpu::TextureView,
) {
    let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.counted_set_pipeline(pipeline);
    pass.counted_set_bind_group(0, bg, &[]);
    if let Some(group1) = group1 {
        pass.counted_set_bind_group(1, group1, &[]);
    }
    pass.counted_draw(0..3, 0..1);
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn float_tex(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn depth_tex(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn buf_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn view_entry(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn depth_entry(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    view_entry(binding, view)
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "ssgi.wgsl,ssgi_cached.wgsl",
            "SsgiParams",
            SsgiParams {
                screen,
                proj,
                noise,
                up
            }
        ),
        crate::contract_row!(
            "ssr.wgsl",
            "SsrParams",
            SsrParams {
                inv_view_proj,
                view_proj,
                camera,
                screen,
                misc,
                probes,
                captures
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(device: &wgpu::Device, format: wgpu::TextureFormat, label: &str) -> wgpu::TextureView {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 8,
                    height: 8,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn depth_view(device: &wgpu::Device) -> wgpu::TextureView {
        view(device, wgpu::TextureFormat::Depth32Float, "depth")
    }

    /// Records one half-resolution SSGI frame and submits it (wgpu panics on a validation
    /// error, so a stale-pipeline draw fails the test).
    fn record_half_ssgi(pass: &mut HighEndPass, device: &crate::RenderDevice, format: wgpu::TextureFormat) {
        let mut belt = crate::staging::StagingBelt::default();
        belt.begin_frame();
        let hdr = view(&device.device, format, "hdr");
        let depth = depth_view(&device.device);
        let front = view(&device.device, wgpu::TextureFormat::Rgba8Unorm, "front");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ssgi") });
        pass.record_ssgi(
            &device.device,
            &mut belt,
            &mut encoder,
            &hdr,
            &depth,
            &front,
            8,
            8,
            format,
            [1.0, 1.0, 0.1, 100.0],
            0.0,
            true,
            false,
            false,
            None,
        )
        .expect("ssgi");
        belt.upload(&crate::device::TrackedQueue::observe(&device.queue));
        wgpu::Queue::submit(&device.queue, [encoder.finish()]);
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
    }

    fn record_ssr_frame(pass: &mut HighEndPass, device: &crate::RenderDevice, format: wgpu::TextureFormat) {
        let mut belt = crate::staging::StagingBelt::default();
        belt.begin_frame();
        let hdr = view(&device.device, format, "hdr");
        let depth = depth_view(&device.device);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ssr") });
        pass.record_ssr(
            &device.device,
            &mut belt,
            &mut encoder,
            &hdr,
            &depth,
            8,
            8,
            format,
            [[0.0; 4]; 4],
            [[0.0; 4]; 4],
            [0.0; 3],
            &[],
            4,
            1.0,
            true,
        )
        .expect("ssr");
        belt.upload(&crate::device::TrackedQueue::observe(&device.queue));
        wgpu::Queue::submit(&device.queue, [encoder.finish()]);
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
    }

    /// A warm handoff that adopted a half-resolution SSGI upsample pipeline used to leave the
    /// adopter without the upsample's bind group layout, and the first half-resolution gather
    /// panicked in `upsample_bind`.
    #[test]
    fn adopted_half_res_ssgi_keeps_the_upsample_layout() {
        let device = crate::test_device();
        let format = wgpu::TextureFormat::Rgba16Float;
        let mut donor = HighEndPass::default();
        record_half_ssgi(&mut donor, &device, format);
        assert!(donor.ssgi_up_pipe.is_some() && donor.ssgi_up_bgl.is_some());
        let mut adopter = HighEndPass::default();
        assert!(adopter.adopt_pipelines(&mut donor));
        assert!(
            adopter.ssgi_up_bgl.is_some(),
            "the upsample layout moves with its pipeline"
        );
        record_half_ssgi(&mut adopter, &device, format);
    }

    /// The gather, upsample and SSR pipelines bake the colour target's format; a later frame
    /// at another HDR format must rebuild them (the target is reallocated in the new format,
    /// and a stale pipeline fails the pass's validation).
    #[test]
    fn ssgi_and_ssr_rebuild_when_the_colour_format_changes() {
        let device = crate::test_device();
        let mut pass = HighEndPass::default();
        record_half_ssgi(&mut pass, &device, wgpu::TextureFormat::Rgba16Float);
        record_half_ssgi(&mut pass, &device, wgpu::TextureFormat::Rg11b10Ufloat);
        record_ssr_frame(&mut pass, &device, wgpu::TextureFormat::Rgba16Float);
        record_ssr_frame(&mut pass, &device, wgpu::TextureFormat::Rg11b10Ufloat);
    }

    /// Probe dimensions are public host fields; a product that does not fit `u32` used to
    /// overflow the short-buffer guard (panic in debug, a wrapped pass in release).
    #[test]
    fn resample_equirect_rejects_overflowing_probe_dimensions() {
        let dst = resample_equirect(&[0u8; 16], 65536, 65536);
        assert_eq!(dst.len(), (EQUIRECT_WIDTH * EQUIRECT_HEIGHT * 4) as usize);
        assert!(dst.iter().all(|&b| b == 0), "a short buffer stays black");
    }
}
