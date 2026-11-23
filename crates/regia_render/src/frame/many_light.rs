// SPDX-License-Identifier: MPL-2.0
//! Stochastic many-light sampling in the frame (M15d; ADR-2470 § 3, ADR-2652): the
//! `many_light_sampling` step between the sun trace and the forward pass.
//!
//! When [`crate::FrameQuality::many_light_sampling`] asks (and the frame is a single-sampled,
//! linear-HDR main view with local lights), the step rebuilds the sampler's G-buffer from the
//! prepass depth ([`crate::restir_di::DepthGBuffer`]) and runs the spatiotemporal reservoir
//! passes ([`crate::restir_di::RestirDiGpu`]) over the frame's own light buffer and cluster
//! lists. The opaque forward permutation `RESTIR_DI` then reads the pixel's reservoir from
//! group 0 binding 37 and shades its light in place of the list's points and spots.
//!
//! The frame binds the reservoir buffer in its view group before the depth prepass, so the work
//! is split: [`ManyLightState::begin`] (before the view group: decides, sizes the per-size
//! buffers, names the buffer this frame writes) and [`ManyLightState::record`] (after the sun
//! trace: binds the depth and the lights, uploads the parameters, encodes). Nothing is created
//! after the first frame at a size; the reservoirs alternate between two buffers by frame
//! parity, so the view group has two cached keys.

use regia_core::glam::{Mat4, Vec3};

use crate::TrackedDevice;
use crate::restir_di::{
    DepthGBuffer, DepthGBufferInputs, RestirCamera, RestirClusters, RestirDiGpu, RestirDiSettings,
    RestirInputs,
};

/// The flat material of the sampler's target (ADR-2470 § 3.3): only the light choice uses it;
/// the forward pass shades the chosen light with the true material.
const TARGET_MATERIAL: [f32; 4] = [0.8, 0.8, 0.8, 0.5];

/// Seed of the frame's random streams.
const SEED: u32 = 0x5eed_2652;

/// Why the last frame did not sample ([`ManyLightReport::skipped`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ManyLightSkip {
    /// The profile does not ask (the default).
    #[default]
    Off,
    /// The frame is multisampled (the G-buffer stage reads single-sampled depth).
    Multisampled,
    /// The frame does not render through the linear HDR target.
    NotLinearHdr,
    /// No local light this frame.
    NoLights,
    /// The device's view layout has no reservoir binding (too few storage buffers per stage).
    NoBinding,
    /// The sampler failed to prepare this frame (see [`ManyLightReport::last_error`]).
    Failed,
}

impl ManyLightSkip {
    /// Lowercase name (`render.many_light_stats`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Multisampled => "multisampled",
            Self::NotLinearHdr => "not_linear_hdr",
            Self::NoLights => "no_lights",
            Self::NoBinding => "no_binding",
            Self::Failed => "failed",
        }
    }
}

/// What the frame's many-light step did ([`crate::FrameRenderer::many_light_report`],
/// `render.many_light_stats`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ManyLightReport {
    /// The last main-view frame shaded its points and spots from the reservoirs.
    pub active: bool,
    /// Why it did not, when [`Self::active`] is false.
    pub skipped: ManyLightSkip,
    /// Frames that sampled.
    pub frames: u64,
    /// History resets (camera cuts, a changed light list, a resize).
    pub resets: u64,
    /// Times the per-size buffers were (re)built.
    pub resource_builds: u64,
    /// Bind groups the sampler and its G-buffer stage built.
    pub bind_group_builds: u64,
    /// Local lights in the last sampled frame.
    pub lights: u32,
    /// Size of the last sampled frame.
    pub size: (u32, u32),
    /// The last prepare error, if any.
    pub last_error: Option<String>,
}

/// The renderer's many-light sampler (empty until a profile asks).
#[derive(Default)]
pub(super) struct ManyLightState {
    gbuffer: Option<DepthGBuffer>,
    sampler: Option<RestirDiGpu>,
    /// This frame samples ([`Self::begin`] said so).
    frame_active: bool,
    /// The size [`Self::begin`] sized for.
    frame_size: (u32, u32),
    /// The light list's signature (count, kinds, colours, ranges: what does not move).
    light_signature: u64,
    /// A reset is due before the next prepare.
    reset_pending: bool,
    report: ManyLightReport,
    settings: RestirDiSettings,
    /// The last main-view frame's profile asked (the warm key builds the pipelines then).
    requested: bool,
}

/// What [`ManyLightState::begin`] decides from.
pub(super) struct ManyLightBegin {
    /// [`crate::FrameQuality::many_light_sampling`].
    pub(super) requested: bool,
    /// MSAA samples.
    pub(super) samples: u32,
    /// The frame renders through the linear HDR target.
    pub(super) linear_hdr: bool,
    /// A texture-view frame.
    pub(super) view_pass: bool,
    /// Render-resolution size.
    pub(super) size: (u32, u32),
    /// The device's view layout carries binding 37.
    pub(super) binding: bool,
}

/// What [`ManyLightState::record`] reads.
pub(super) struct ManyLightRecord<'a> {
    /// The prepass depth (single-sampled, standard 0..1).
    pub(super) depth: &'a wgpu::TextureView,
    /// The frame's local lights (`GpuLight` records).
    pub(super) lights: &'a wgpu::Buffer,
    /// Records in `lights`.
    pub(super) light_count: u32,
    /// The cluster build's counts.
    pub(super) counts: &'a wgpu::Buffer,
    /// The cluster build's indices.
    pub(super) indices: &'a wgpu::Buffer,
    /// Slots per cluster (`frame.screen.w`).
    pub(super) max_per_cluster: u32,
    /// Unjittered view-projection.
    pub(super) clip_from_world: Mat4,
    /// Unjittered projection.
    pub(super) clip_from_view: Mat4,
    /// Camera position.
    pub(super) eye: [f32; 3],
    /// Vertical field of view.
    pub(super) fov_y: f32,
    /// Near plane (the cluster grid's).
    pub(super) near: f32,
    /// Far plane (the cluster grid's).
    pub(super) far: f32,
}

impl super::PipelineKey {
    /// This key without the many-light bit: the feature families that keep the light loop
    /// (lite tiers, mask decals, lightmap receivers, thin film, subsurface, OIT, glass) share
    /// one cache entry for both values.
    pub(super) fn plain(self) -> Self {
        Self {
            restir_di: false,
            ..self
        }
    }
}

/// Whether a device's view layout carries the reservoir binding (37): desktop devices with
/// room for one more fragment storage buffer. Android keeps its layout (the sampler is a
/// Desktop Ultra feature, `specs/26` § 4).
pub(super) fn binding_supported(limits: &wgpu::Limits) -> bool {
    !cfg!(target_os = "android") && limits.max_storage_buffers_per_shader_stage >= MIN_STORAGE_BUFFERS
}

/// Fragment storage buffers a device needs for the reservoir binding: the forward pass's own
/// (8 in group 0 with it, the object, slot and material arenas) with headroom.
pub(super) const MIN_STORAGE_BUFFERS: u32 = 16;

/// A signature of the frame's light list that moving lights keep: count, then per light its
/// kind, colour, intensity and range in list order. A changed visible set or order changes it.
pub(super) fn light_list_signature(lights: &[crate::LabLocalLight]) -> u64 {
    // FNV-1a over the fields' bits: no allocation, a few ns per light.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u32| {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(lights.len() as u32);
    for light in lights {
        match light {
            crate::LabLocalLight::Point(p) => {
                mix(1);
                p.color.iter().for_each(|c| mix(c.to_bits()));
                mix(p.intensity.to_bits());
                mix(p.radius.to_bits());
            }
            crate::LabLocalLight::Spot(s) => {
                mix(2);
                s.color.iter().for_each(|c| mix(c.to_bits()));
                mix(s.intensity.to_bits());
                mix(s.range.to_bits());
            }
        }
    }
    h
}

impl ManyLightState {
    pub(super) fn invalidate_cluster_bindings(&mut self) {
        if let Some(sampler) = self.sampler.as_mut() {
            sampler.invalidate_bindings();
        }
    }

    pub(super) fn graph_buffers(&self) -> ([u64; 3], u64) {
        self.sampler
            .as_ref()
            .map_or(([0; 3], 0), RestirDiGpu::graph_buffers)
    }

    /// Decides whether this frame samples and sizes the per-size buffers. Returns the buffer
    /// the view group binds at 37 and its key bits (0 = the stand-in, 1 + parity otherwise);
    /// `None` binds the stand-in.
    pub(super) fn begin(
        &mut self,
        d: &TrackedDevice,
        inputs: &ManyLightBegin,
        signature: u64,
        light_count: u32,
    ) -> Option<(wgpu::Buffer, u64)> {
        self.frame_active = false;
        if !inputs.view_pass {
            self.requested = inputs.requested;
        }
        let skip = if !inputs.requested {
            Some(ManyLightSkip::Off)
        } else if !inputs.binding {
            Some(ManyLightSkip::NoBinding)
        } else if inputs.view_pass {
            // A texture view keeps the main view's history and report.
            return None;
        } else if inputs.samples != 1 {
            Some(ManyLightSkip::Multisampled)
        } else if !inputs.linear_hdr {
            Some(ManyLightSkip::NotLinearHdr)
        } else if light_count == 0 || inputs.size.0 == 0 || inputs.size.1 == 0 {
            Some(ManyLightSkip::NoLights)
        } else {
            None
        };
        if let Some(skip) = skip {
            if self.report.active {
                // History built for another state is stale when sampling resumes.
                self.reset_pending = true;
            }
            self.report.active = false;
            self.report.skipped = skip;
            return None;
        }
        if self.sampler.is_none() {
            match RestirDiGpu::new(d, self.settings.clone(), SEED) {
                Ok(sampler) => self.sampler = Some(sampler),
                Err(e) => {
                    self.report.active = false;
                    self.report.skipped = ManyLightSkip::Failed;
                    self.report.last_error = Some(e.to_string());
                    return None;
                }
            }
        }
        if self.gbuffer.is_none() {
            self.gbuffer = Some(DepthGBuffer::new(d));
        }
        if signature != self.light_signature {
            self.light_signature = signature;
            self.reset_pending = true;
        }
        let sampler = self.sampler.as_mut()?;
        let builds = sampler.resource_builds();
        sampler.ensure_size(d, inputs.size);
        if sampler.resource_builds() != builds {
            self.report.resource_builds += 1;
            self.reset_pending = true;
        }
        let buffer = sampler.next_reservoirs()?.clone();
        let bits = 1 + u64::from(sampler.next_parity());
        self.frame_active = true;
        self.frame_size = inputs.size;
        Some((buffer, bits))
    }

    /// Generation of the per-size buffers: the view groups that bind the old ones are dropped
    /// when it changes.
    pub(super) fn generation(&self) -> u64 {
        self.report.resource_builds
    }

    /// Drops the history (a camera cut).
    pub(super) fn reset_history(&mut self) {
        self.reset_pending = true;
    }

    /// The report.
    pub(super) fn report(&self) -> &ManyLightReport {
        &self.report
    }

    /// Replaces the sampler's settings (the next frame starts without history).
    pub(super) fn set_settings(
        &mut self,
        settings: RestirDiSettings,
    ) -> Result<(), crate::restir_di::RestirDiError> {
        settings.validate()?;
        if let Some(sampler) = self.sampler.as_mut() {
            sampler.set_settings(settings.clone())?;
        }
        self.settings = settings;
        Ok(())
    }

    /// The settings in use.
    pub(super) fn settings(&self) -> &RestirDiSettings {
        &self.settings
    }

    /// Builds the G-buffer stage's and the sampler's pipelines ahead of play (the warm key
    /// `restir_di`). Creates nothing once built.
    pub(super) fn warm(&mut self, d: &TrackedDevice) {
        if self.sampler.is_none()
            && let Ok(sampler) = RestirDiGpu::new(d, self.settings.clone(), SEED)
        {
            self.sampler = Some(sampler);
        }
        if self.gbuffer.is_none() {
            self.gbuffer = Some(DepthGBuffer::new(d));
        }
    }

    /// [`Self::begin`] chose to sample this frame: [`Self::record`] records something (the frame
    /// graph's `many_light_sampling` switch, ADR-4905).
    pub(super) fn frame_active(&self) -> bool {
        self.frame_active
    }

    /// Records the G-buffer stage and the sampler's passes of a frame [`Self::begin`] sized.
    /// A failure leaves the frame's forward permutation reading the previous reservoirs and is
    /// reported, never raised: the frame still draws.
    pub(super) fn record(
        &mut self,
        d: &TrackedDevice,
        q: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &ManyLightRecord<'_>,
    ) {
        if !self.frame_active {
            return;
        }
        let _span = tracing::info_span!("many_light_sampling").entered();
        let _crumb = crate::breadcrumbs::frame_pass("many_light_sampling");
        let (Some(gbuffer), Some(sampler)) = (self.gbuffer.as_mut(), self.sampler.as_mut()) else {
            return;
        };
        let size = self.frame_size;
        let view_from_world = inputs.clip_from_view.inverse() * inputs.clip_from_world;
        let forward = view_from_world
            .inverse()
            .transform_vector3(Vec3::NEG_Z)
            .normalize_or(Vec3::NEG_Z);
        let camera = RestirCamera {
            clip_from_world: inputs.clip_from_world,
            view_from_world,
            eye: inputs.eye,
            forward: forward.to_array(),
            fov_y: inputs.fov_y,
            near: inputs.near,
            far: inputs.far,
        };
        if self.reset_pending {
            sampler.reset_history();
            gbuffer.reset_motion();
            self.reset_pending = false;
            self.report.resets += 1;
        }
        let g_before = gbuffer.builds();
        let prepared = gbuffer.prepare(
            d,
            &DepthGBufferInputs {
                depth: inputs.depth,
                viewport: [0.0, 0.0, size.0 as f32, size.1 as f32],
                color_size: size,
                clip_from_world: inputs.clip_from_world,
                eye: inputs.eye,
                forward: camera.forward,
                material: TARGET_MATERIAL,
                pre_exposure: 1.0,
            },
        );
        if gbuffer.builds() != g_before {
            self.report.resource_builds += 1;
        }
        if let Err(e) = prepared {
            self.fail(e.to_string());
            return;
        }
        let Some(views) = gbuffer.views() else {
            self.fail("the G-buffer has no outputs".into());
            return;
        };
        let groups = sampler.bind_group_builds();
        let prepared = sampler.prepare(
            d,
            &RestirInputs {
                size,
                depth: &views[0],
                normal_roughness: &views[1],
                albedo_metallic: &views[2],
                motion: &views[3],
                lights: inputs.lights,
                light_count: inputs.light_count,
                clusters: Some(RestirClusters {
                    counts: inputs.counts,
                    indices: inputs.indices,
                    max_per_cluster: inputs.max_per_cluster.max(1),
                }),
                camera,
                reference: false,
            },
        );
        self.report.bind_group_builds += sampler.bind_group_builds() - groups;
        if let Err(e) = prepared {
            self.fail(e.to_string());
            return;
        }
        let (buffer, bytes) = gbuffer.params_upload();
        q.write_buffer(buffer, 0, bytes);
        let (buffer, bytes) = sampler.params_upload();
        q.write_buffer(buffer, 0, bytes);
        gbuffer.encode(encoder);
        sampler.encode(encoder);
        self.report.active = true;
        self.report.skipped = ManyLightSkip::Off;
        self.report.frames += 1;
        self.report.lights = inputs.light_count;
        self.report.size = size;
    }

    fn fail(&mut self, error: String) {
        tracing::warn!(target: "regia_render", %error, "many-light sampling skipped this frame");
        self.report.active = false;
        self.report.skipped = ManyLightSkip::Failed;
        self.report.last_error = Some(error);
        self.reset_pending = true;
    }
}

impl super::FrameRenderer {
    /// What the last main-view frame's many-light step did (`render.many_light_stats`,
    /// ADR-2652). Inactive until [`crate::FrameQuality::many_light_sampling`] asks.
    #[must_use]
    pub fn many_light_report(&self) -> ManyLightReport {
        self.many_light.report().clone()
    }

    /// The production sampler's settings ([`RestirDiSettings::default`] until set).
    #[must_use]
    pub fn many_light_settings(&self) -> RestirDiSettings {
        self.many_light.settings().clone()
    }

    /// Replaces the production sampler's settings; the next sampled frame starts without
    /// history.
    ///
    /// # Errors
    /// [`crate::restir_di::RestirDiError::InvalidSetting`]; the old settings stay.
    pub fn set_many_light_settings(
        &mut self,
        settings: RestirDiSettings,
    ) -> Result<(), crate::restir_di::RestirDiError> {
        self.many_light.set_settings(settings)?;
        self.many_light.reset_history();
        Ok(())
    }

    /// Whether this renderer's forward view layout binds the blue-noise table (binding 38,
    /// ADR-2652): the forward pass's shadow rotation, hashed alpha and pane SSR then read it on
    /// frames with [`crate::FrameQuality::blue_noise`]. `None` before the first frame.
    #[must_use]
    pub fn forward_blue_noise_binding(&self) -> Option<bool> {
        self.pipes.as_ref().map(|p| p.blue_noise_binding)
    }

    /// Drops the many-light history (the frame does it on a camera cut and when the light list
    /// changes).
    pub fn reset_many_light_history(&mut self) {
        self.many_light.reset_history();
    }

    /// Builds the many-light pipelines ahead of play (the `restir_di` warm key): the sampler's
    /// and its G-buffer stage's, and the `RESTIR_DI` opaque twin of every single-sampled forward
    /// set the warm drew. Nothing unless the warm's profile asked.
    pub(super) fn prewarm_many_light(
        &mut self,
        device: &crate::RenderDevice,
    ) -> Result<usize, crate::RenderError> {
        if !self.many_light.requested {
            return Ok(0);
        }
        let tracked = device.tracked();
        self.many_light.warm(&tracked);
        let mut asked = 2;
        let Some(pipes) = self.pipes.as_ref() else {
            return Ok(asked);
        };
        if !pipes.many_light_binding {
            return Ok(asked);
        }
        let keys: Vec<super::PipelineKey> = pipes
            .sets
            .keys()
            .copied()
            .filter(|k| !k.restir_di && k.samples == 1)
            .collect();
        for key in keys {
            crate::pipeline_warm::warm_checkpoint()?;
            let twin = super::PipelineKey {
                restir_di: true,
                ..key
            };
            let Some(pipes) = self.pipes.as_mut() else {
                return Ok(asked);
            };
            if !pipes.sets.contains_key(&twin) {
                let set = super::pipelines::build_pipeline_set(&device.device, pipes, twin);
                pipes.sets.insert(twin, set);
            }
            asked += 1;
            if crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
                let (view_bgl, mat_bgl) = (pipes.view_bgl.clone(), pipes.mat_bgl.clone());
                let (frame_sun, frame_restir) =
                    (self.culler.forward_sun_rt(), self.culler.forward_restir_di());
                self.culler.set_forward_sun_rt(key.sun_rt);
                self.culler.set_forward_restir_di(true);
                let built = self.culler.color_pipelines(
                    &device.device,
                    key.format,
                    key.encode,
                    key.samples,
                    true,
                    &view_bgl,
                    &mat_bgl,
                    &[(key.double_sided, false), (key.double_sided, true)],
                );
                self.culler.set_forward_sun_rt(frame_sun);
                self.culler.set_forward_restir_di(frame_restir);
                built?;
                asked += 1;
            }
        }
        Ok(asked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f32, intensity: f32) -> crate::LabLocalLight {
        crate::LabLocalLight::Point(crate::LabPointLight {
            position: [x, 1.0, 0.0],
            color: [1.0, 0.9, 0.8],
            intensity,
            radius: 5.0,
            cast_shadows: false,
            source_radius: None,
        })
    }

    #[test]
    fn a_moving_light_keeps_the_signature_and_a_new_order_changes_it() {
        let a = [point(0.0, 100.0), point(1.0, 200.0)];
        let moved = [point(3.0, 100.0), point(-2.0, 200.0)];
        let swapped = [point(1.0, 200.0), point(0.0, 100.0)];
        let fewer = [point(0.0, 100.0)];
        assert_eq!(light_list_signature(&a), light_list_signature(&moved));
        assert_ne!(light_list_signature(&a), light_list_signature(&swapped));
        assert_ne!(light_list_signature(&a), light_list_signature(&fewer));
    }

    #[test]
    fn skip_names_are_lowercase_codes() {
        for skip in [
            ManyLightSkip::Off,
            ManyLightSkip::Multisampled,
            ManyLightSkip::NotLinearHdr,
            ManyLightSkip::NoLights,
            ManyLightSkip::NoBinding,
            ManyLightSkip::Failed,
        ] {
            assert!(
                skip.as_str().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{skip:?}"
            );
        }
    }
}
