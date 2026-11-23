// SPDX-License-Identifier: MIT-0
//! Paged-shadow churn from networked-style unit jitter.
//!
//! A fight's units do not sit still between snapshots: their poses wobble by a fraction of
//! a millimetre. The page cache keeps a page while the caster's displacement stays inside
//! `PAGE_MOVE_TOLERANCE_UV` of the last **drawn** pose (about a fifth of a texel at 2048²,
//! and a hard 0.1 mm on the local atlas). Jitter under that must not dirty pages. A real
//! step, half a metre, must. This scene is the check: [`UNITS`] boxes, a fixed camera and
//! sun so the cascade fit does not move, paging on, then one control move.
//!
//! The tolerance itself is the existing page cache (Mittring, SIGGRAPH 2008, already in
//! `docs/provenance.md`). This scene only chooses a wobble smaller than that tolerance.

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    GiMode, LabMaterial, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCacheReport,
    ShadowCast, SsaoMode,
};

use crate::LookdevError;

/// Units in the stress scene. Above the 150–200 band a networked fight reaches.
pub const UNITS: u32 = 200;
/// Peak offset of one jitter sample, metres. The next frame flips the sign, so the
/// peak-to-peak travel is twice this and stays under the 0.1 mm atlas epsilon.
pub const JITTER_M: f32 = 2.0e-5;
/// One unit steps this far on the control frame, metres. It has to dirty pages.
pub const CONTROL_MOVE_M: f32 = 0.5;

const WARMUP: u32 = 8;
const MEASURE: u32 = 6;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;

/// What the jitter window and the control move did to the page cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitterReport {
    /// Units drawn.
    pub units: u32,
    /// Dirty pages summed over the measured jitter frames.
    pub jitter_dirty_pages: u32,
    /// Casters the cache called moved over those frames.
    pub jitter_moved: u32,
    /// Dirty pages on the control frame.
    pub control_dirty_pages: u32,
    /// Casters the cache called moved on the control frame.
    pub control_moved: u32,
    /// Jitter kept every page and the control move dirtied at least one.
    pub passed: bool,
    /// One line for a log.
    pub reason: String,
}

impl JitterReport {
    /// The log line.
    #[must_use]
    pub fn evidence(&self) -> String {
        format!(
            "{} units jitter ±{JITTER_M} m: dirty pages {}, moved casters {}; control move {CONTROL_MOVE_M} m dirty pages {}, moved {}",
            self.units,
            self.jitter_dirty_pages,
            self.jitter_moved,
            self.control_dirty_pages,
            self.control_moved
        )
    }
}

/// World-space wobble for `unit` on `frame`. X only, sign flips every frame, magnitude
/// [`JITTER_M`].
#[must_use]
pub fn jitter_delta(unit: u32, frame: u32) -> [f32; 3] {
    let flip = if (frame + unit).is_multiple_of(2) {
        1.0
    } else {
        -1.0
    };
    [JITTER_M * flip, 0.0, 0.0]
}

/// `true` when the jitter window left the cache alone and the control move did not.
#[must_use]
pub fn churn_caught(
    jitter_dirty_pages: u32,
    jitter_moved: u32,
    control_dirty_pages: u32,
    control_moved: u32,
) -> bool {
    jitter_dirty_pages == 0 && jitter_moved == 0 && (control_dirty_pages > 0 || control_moved > 0)
}

/// Renders the stress scene.
///
/// # Errors
///
/// A frame failed to record.
#[tracing::instrument(skip_all)]
pub fn run(device: &RenderDevice) -> Result<JitterReport, LookdevError> {
    let _span = tracing::info_span!("jitter_units", units = UNITS).entered();
    let (vertices, indices) = regia_render::box_mesh([0.4, 0.9, 0.4]);
    let material = LabMaterial {
        base_color: [0.45, 0.42, 0.38, 1.0],
        roughness: 0.7,
        ..LabMaterial::default()
    };
    let models: Vec<[[f32; 4]; 4]> = (0..UNITS as usize).map(|i| translation(base_pos(i))).collect();
    let meshes: Vec<FrameMesh<'_>> = models
        .iter()
        .enumerate()
        .map(|(i, model)| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 2,
            object_key: 1_000 + i as u64,
            vertices: &vertices,
            indices: &indices,
            material: &material,
            model: *model,
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let post = post();
    let mut frame = FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 18.0, 36.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.5,
        far: 120.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 20.0,
        double_sided: false,
        post: Some(&post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.25, -1.0, 0.2], [1.0, 0.98, 0.95], 80_000.0),
        local_lights: &[],
        quality: quality(),
        meshes,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("jitter units"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let target = FrameTarget::full(&view, wgpu::TextureFormat::Rgba8Unorm, WIDTH, HEIGHT);
    let mut renderer = FrameRenderer::new();
    let mut jitter_dirty_pages = 0u32;
    let mut jitter_moved = 0u32;
    for frame_i in 0..WARMUP + MEASURE {
        place(&mut frame, frame_i, false);
        renderer.render_into(device, &frame, target)?;
        if frame_i >= WARMUP {
            let report = renderer.shadow_cache_report();
            jitter_dirty_pages += report.dirty_pages.len() as u32;
            jitter_moved += moved_casters(report);
        }
    }
    place(&mut frame, WARMUP + MEASURE, true);
    renderer.render_into(device, &frame, target)?;
    let control = renderer.shadow_cache_report();
    let control_dirty_pages = control.dirty_pages.len() as u32;
    let control_moved = moved_casters(control);
    let passed = churn_caught(
        jitter_dirty_pages,
        jitter_moved,
        control_dirty_pages,
        control_moved,
    );
    let reason = if passed {
        format!(
            "jitter of ±{JITTER_M} m dirtied no pages; a {CONTROL_MOVE_M} m step dirtied {control_dirty_pages} (moved {control_moved})"
        )
    } else if control_dirty_pages == 0 && control_moved == 0 {
        "the control move did not dirty a page; paging was not observed".to_owned()
    } else {
        format!(
            "jitter dirtied {jitter_dirty_pages} pages and moved {jitter_moved} casters; sub-tolerance wobble must keep the cache"
        )
    };
    Ok(JitterReport {
        units: UNITS,
        jitter_dirty_pages,
        jitter_moved,
        control_dirty_pages,
        control_moved,
        passed,
        reason,
    })
}

fn moved_casters(report: &ShadowCacheReport) -> u32 {
    report.caster_events.iter().map(|event| event[0]).sum()
}

fn place(frame: &mut FrameScene<'_>, frame_i: u32, control: bool) {
    for (i, mesh) in frame.meshes.iter_mut().enumerate() {
        let mut pos = base_pos(i);
        let delta = jitter_delta(i as u32, frame_i);
        pos[0] += delta[0];
        pos[1] += delta[1];
        pos[2] += delta[2];
        if control && i == 0 {
            pos[0] += CONTROL_MOVE_M;
        }
        mesh.model = translation(pos);
    }
}

fn base_pos(index: usize) -> [f32; 3] {
    let side = (UNITS as f32).sqrt().ceil() as i32;
    let x = (index as i32 % side - side / 2) as f32 * 2.5;
    let z = (index as i32 / side - side / 2) as f32 * 2.5;
    [x, 0.9, z]
}

fn translation(t: [f32; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [t[0], t[1], t[2], 1.0],
    ]
}

fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.ssao = SsaoMode::Off;
    quality.gi = GiMode::Ambient;
    quality.gi_probes = false;
    quality.reflection_probes = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.physical_atmosphere = false;
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.horizontal_plus = false;
    quality.contact_shadows = false;
    quality.gpu_driven = false;
    quality.paged_sun_shadows = true;
    quality.near_sun_tile = 0;
    quality.screen_space_refraction = false;
    quality.emissive_lights = 0;
    quality.msaa_samples = 1;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.translucent_shadow_cascades = 0;
    quality
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_under_a_tenth_of_a_millimetre() {
        assert_eq!(UNITS, 200);
        for unit in 0..UNITS {
            for frame in 0..8 {
                let delta = jitter_delta(unit, frame);
                assert!(delta[0].abs() <= JITTER_M);
                assert_eq!(delta[1], 0.0);
                assert_eq!(delta[2], 0.0);
            }
            let span = (jitter_delta(unit, 0)[0] - jitter_delta(unit, 1)[0]).abs();
            assert!(span < 1.0e-4, "peak-to-peak {span} m is inside the atlas epsilon");
            assert!(span > 0.0);
        }
        const { assert!(CONTROL_MOVE_M > 1.0e-4) };
        assert!(churn_caught(0, 0, 3, 1));
        assert!(!churn_caught(1, 0, 3, 1), "a dirty jitter frame is churn");
        assert!(!churn_caught(0, 0, 0, 0), "the control move has to be visible");
    }
}
