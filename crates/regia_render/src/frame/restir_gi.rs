// SPDX-License-Identifier: MPL-2.0
//! ReSTIR GI in the production frame (ADR-3349). This file is delivered in
//! `reports/sprint-restir-gi-ray-query-frame.patch` while the frame-owner lane holds the files.
//!
//! The probe update and its ordinary screen lookup run first. When ReSTIR GI is active this
//! state replaces that lookup's texels before the forward pass; the existing forward and terrain
//! shaders therefore need no new permutation. Every fallback leaves the ordinary lookup intact.

use regia_core::glam::{Mat4, Vec3};

use super::{Draw, FrameRenderer, FrameScene, MaterialEntry, MeshEntry};
use crate::restir_di::{DepthGBuffer, DepthGBufferInputs};
use crate::restir_gi::{
    RestirGiError, RestirGiFallback, RestirGiGpu, RestirGiInputs, RestirGiReport, RestirGiSettings,
    RestirGiSupport, restir_gi_fallback,
};
use crate::{RenderError, TrackedDevice};

const SEED: u32 = 0x3302_2021;
const TARGET_MATERIAL: [f32; 4] = [0.8, 0.8, 0.8, 0.5];

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct HitMaterialGpu {
    base_color: [f32; 4],
    emissive: [f32; 4],
}

impl Default for HitMaterialGpu {
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8, 1.0],
            emissive: [0.0; 4],
        }
    }
}

/// What one frame's ReSTIR GI record reads.
pub(super) struct RestirGiRecord<'a> {
    pub(super) scene: &'a FrameScene<'a>,
    pub(super) view_pass: bool,
    pub(super) linear_hdr: bool,
    pub(super) depth_view: &'a wgpu::TextureView,
    pub(super) size: (u32, u32),
    pub(super) view_proj: Mat4,
    pub(super) unjittered_vp: Mat4,
    pub(super) unjittered_proj: Mat4,
    pub(super) viewport: [f32; 4],
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) draws: &'a [Draw],
    pub(super) materials: &'a crate::key_hash::KeyMap<u64, MaterialEntry>,
    pub(super) ddgi_active: bool,
    pub(super) ddgi_uniform: &'a wgpu::Buffer,
    pub(super) ddgi_data: &'a wgpu::Buffer,
    pub(super) ddgi_settings: &'a crate::DdgiSettings,
    pub(super) lookup: crate::ddgi::GiLookupResolution,
}

/// Renderer-owned ReSTIR GI state; empty until the profile asks on a capable device.
#[derive(Default)]
pub(super) struct RestirGiState {
    gbuffer: Option<DepthGBuffer>,
    sampler: Option<RestirGiGpu>,
    settings: RestirGiSettings,
    report: RestirGiReport,
    reset_pending: bool,
    material_records: Vec<HitMaterialGpu>,
    material_previous: Vec<u8>,
    material_buffer: Option<wgpu::Buffer>,
    material_capacity: usize,
    material_builds: u64,
}

impl RestirGiState {
    pub(super) fn graph_buffers(&self) -> ([u64; 3], u64) {
        self.sampler
            .as_ref()
            .map_or(([0; 3], 0), RestirGiGpu::graph_buffers)
    }

    pub(super) fn reset(&mut self) {
        self.reset_pending = true;
    }

    fn settings(&self) -> &RestirGiSettings {
        &self.settings
    }

    fn set_settings(&mut self, settings: RestirGiSettings) -> Result<(), RestirGiError> {
        settings.validate()?;
        if let Some(sampler) = self.sampler.as_mut() {
            sampler.set_settings(settings.clone())?;
        }
        self.settings = settings;
        self.reset();
        Ok(())
    }

    fn fallback(&mut self, reason: RestirGiFallback) {
        if self.report.active {
            self.reset();
        }
        self.report.active = false;
        self.report.fallback = Some(reason);
        self.report.history_valid = false;
        self.report.work = None;
    }

    fn fail(&mut self, error: String) {
        tracing::warn!(target: "regia_render", %error, "ReSTIR GI skipped this frame");
        self.report.last_error = Some(error);
        self.fallback(RestirGiFallback::InvalidInput);
    }

    fn upload_materials(
        &mut self,
        d: &TrackedDevice,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &RestirGiRecord<'_>,
    ) {
        self.material_records.clear();
        self.material_records.reserve(inputs.draws.len().max(1));
        for draw in inputs.draws {
            let params = inputs.materials.get(&draw.material).map(|entry| &entry.params);
            self.material_records
                .push(params.map_or_else(HitMaterialGpu::default, |p| HitMaterialGpu {
                    base_color: p.base_color,
                    emissive: p.emissive,
                }));
        }
        if self.material_records.is_empty() {
            self.material_records.push(HitMaterialGpu::default());
        }
        if self.material_capacity < self.material_records.len() {
            self.material_capacity = self.material_records.len().next_power_of_two();
            self.material_buffer = Some(d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("restir gi hit materials"),
                size: (self.material_capacity * size_of::<HitMaterialGpu>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.material_previous.clear();
            self.material_builds += 1;
        }
        if let Some(buffer) = self.material_buffer.as_ref() {
            belt.write_if_changed(
                d,
                encoder,
                buffer,
                bytemuck::cast_slice(&self.material_records),
                &mut self.material_previous,
            );
        }
    }

    /// Builds/uses the traced scene, then records ReSTIR GI and its screen composite.
    pub(super) fn record(
        &mut self,
        d: &TrackedDevice,
        q: &crate::device::TrackedQueue,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        sun_trace: &mut super::sun_trace::SunTraceState,
        inputs: &RestirGiRecord<'_>,
    ) -> Result<(), RenderError> {
        if inputs.view_pass {
            return Ok(());
        }
        self.report.requested = inputs.scene.quality.restir_gi;
        let features = d.features();
        let support = RestirGiSupport {
            requested: inputs.scene.quality.restir_gi,
            ray_query: features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
            vertex_return: features.contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN),
            main_view: !inputs.view_pass,
            single_sample: inputs.scene.quality.msaa_samples == 1 && inputs.scene.depth_prepass,
            linear_hdr: inputs.linear_hdr,
            probes_active: inputs.ddgi_active,
            tlas: true,
        };
        if let Some(reason) = restir_gi_fallback(support) {
            self.fallback(reason);
            return Ok(());
        }
        let Some((tlas, scene_epoch)) = sun_trace.restir_scene(
            d,
            q,
            encoder,
            &super::sun_trace::SunTraceInputs {
                scene: inputs.scene,
                variant: crate::hwrt::SunShadowVariant::RayQuery,
                depth_view: inputs.depth_view,
                size: inputs.size,
                view_proj: inputs.view_proj,
                viewport: inputs.viewport,
                meshes: inputs.meshes,
                draws: inputs.draws,
            },
        ) else {
            self.fallback(RestirGiFallback::NoTlas);
            return Ok(());
        };
        let Some((screen_w, screen_h, screen_step)) = lookup_size(inputs) else {
            self.fallback(RestirGiFallback::NoProbes);
            return Ok(());
        };
        if self.gbuffer.is_none() {
            self.gbuffer = Some(DepthGBuffer::new(d));
        }
        if self.sampler.is_none() {
            match RestirGiGpu::new(d, self.settings.clone(), SEED) {
                Ok(sampler) => self.sampler = Some(sampler),
                Err(error) => {
                    self.fail(error.to_string());
                    return Ok(());
                }
            }
        }
        self.upload_materials(d, belt, encoder, inputs);
        let (Some(gbuffer), Some(sampler), Some(materials)) = (
            self.gbuffer.as_mut(),
            self.sampler.as_mut(),
            self.material_buffer.as_ref(),
        ) else {
            self.fail("ReSTIR GI resources were not created".into());
            return Ok(());
        };
        if self.reset_pending {
            sampler.reset_history();
            gbuffer.reset_motion();
            self.reset_pending = false;
            self.report.resets += 1;
        }
        let view_from_world = inputs.unjittered_proj.inverse() * inputs.unjittered_vp;
        let forward = view_from_world
            .inverse()
            .transform_vector3(Vec3::NEG_Z)
            .normalize_or(Vec3::NEG_Z);
        if let Err(error) = gbuffer.prepare(
            d,
            &DepthGBufferInputs {
                depth: inputs.depth_view,
                viewport: [0.0, 0.0, inputs.size.0 as f32, inputs.size.1 as f32],
                color_size: inputs.size,
                clip_from_world: inputs.unjittered_vp,
                eye: inputs.scene.eye,
                forward: forward.to_array(),
                material: TARGET_MATERIAL,
                pre_exposure: 1.0,
            },
        ) {
            self.fail(error.to_string());
            return Ok(());
        }
        let Some(views) = gbuffer.views().cloned() else {
            self.fail("ReSTIR GI has no depth G-buffer".into());
            return Ok(());
        };
        let prepared = sampler.prepare(
            d,
            &RestirGiInputs {
                size: inputs.size,
                screen_size: (screen_w, screen_h),
                screen_step,
                depth: &views[0],
                normal_roughness: &views[1],
                motion: &views[3],
                scene_depth: inputs.depth_view,
                world_from_clip: inputs.view_proj.inverse(),
                tlas,
                scene_epoch,
                ddgi_uniform: inputs.ddgi_uniform,
                ddgi_data: inputs.ddgi_data,
                materials,
                material_count: self.material_records.len() as u32,
                eye: inputs.scene.eye,
                far: inputs.scene.far,
            },
        );
        if let Err(error) = prepared {
            self.fail(error.to_string());
            return Ok(());
        }
        let (buffer, bytes) = gbuffer.params_upload();
        q.write_buffer(buffer, 0, bytes);
        let (buffer, bytes) = sampler.params_upload();
        q.write_buffer(buffer, 0, bytes);
        let (buffer, bytes) = sampler.denoise_params_upload();
        q.write_buffer(buffer, 0, bytes);
        gbuffer.encode(encoder);
        sampler.encode(encoder);
        self.report.active = true;
        self.report.fallback = None;
        self.report.frames = u64::from(sampler.frames());
        self.report.resource_builds = gbuffer.builds() + sampler.resource_builds() + self.material_builds;
        self.report.bind_group_builds = sampler.bind_group_builds();
        self.report.size = (screen_w, screen_h);
        self.report.history_valid = sampler.has_history();
        self.report.work = sampler.work();
        self.report.total_work = sampler.total_work();
        self.report.last_error = None;
        Ok(())
    }
}

fn lookup_size(inputs: &RestirGiRecord<'_>) -> Option<(u32, u32, u32)> {
    let region = inputs.ddgi_settings.screen_region();
    let fits = |w, h| w <= region.0 && h <= region.1;
    crate::ddgi::fit_lookup(inputs.size, inputs.lookup, fits).or_else(|| {
        let (w, h) = crate::ddgi::GiLookupResolution::Quarter.lookup_size(inputs.size.0, inputs.size.1);
        (w > 0 && h > 0 && fits(w, h)).then_some((w, h, crate::ddgi::GiLookupResolution::Quarter.step()))
    })
}

impl FrameRenderer {
    /// Last main-view ReSTIR GI state (`render.restir_gi`).
    #[must_use]
    pub fn restir_gi_report(&self) -> RestirGiReport {
        self.restir_gi.report.clone()
    }

    /// ReSTIR GI settings in use.
    #[must_use]
    pub fn restir_gi_settings(&self) -> RestirGiSettings {
        self.restir_gi.settings().clone()
    }

    /// Replaces ReSTIR GI settings and resets its history on the next active frame.
    ///
    /// # Errors
    /// [`RestirGiError::InvalidSetting`]; the old settings stay.
    pub fn set_restir_gi_settings(&mut self, settings: RestirGiSettings) -> Result<(), RestirGiError> {
        self.restir_gi.set_settings(settings)
    }

    /// Drops ReSTIR GI reservoir and denoiser history (also done by the frame on a camera cut).
    pub fn reset_restir_gi_history(&mut self) {
        self.restir_gi.reset();
    }
}
