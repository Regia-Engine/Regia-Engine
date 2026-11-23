// SPDX-License-Identifier: MIT-0
//! CPU cost of many moving instanced units (`ddgi prepare`).
//!
//! v0.1.3 walked every vertex of every mesh inside `ddgi prepare` on every frame. A fight
//! with a couple of hundred units made `render_into` grow with the hull's vertex count.
//! Benches that drew a handful of units never saw it. This draws [`INSTANCES`] copies of one
//! hull, all moving, with the probes on, and compares that encode time (inside `render_into`)
//! with the same instances of the box alone. `render_into` p50 and p99 are recorded too.
//!
//! The two runs draw the same triangles. The hull buffer carries extra vertices the prepare
//! step walks and the box does not, so the difference is that walk. Cached object bounds
//! (v0.1.4) pay it once; v0.1.3 pays it every frame and exceeds [`ENCODE_P50_SLACK_MS`].
//!
//! [`UnitsReport::passed`] is what `moving_units_cpu` asserts. `cargo xtask smoke` prints
//! the same numbers as a report-only row: encode time moves with machine load, and this
//! lavapipe slack is not a hardware smoke gate.

use regia_render::{
    DdgiSettings, DirectionalLightUniform, FogSettings, FrameCpuTimings, FrameMesh, FrameQuality,
    FrameRenderer, FrameScene, FrameTarget, GiMode, LabMaterial, LabVertex, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, ShadowCast, SsaoMode,
};

use crate::LookdevError;
use crate::chase::percentile;

/// Moving instances. Inside the 150–200 band the owner's fight reached.
pub const INSTANCES: u32 = 180;
/// Vertices in the hull buffer. The box control is the indexed hull without this padding.
///
/// Large on purpose: v0.1.3's per-frame walk has to move the encode time by more than
/// lavapipe's submit noise. A few thousand vertices disappeared into that noise.
pub const HULL_VERTICES: usize = 65_536;
/// How far the hull run's encode p50 may sit above the box run's, milliseconds.
///
/// Encode time is `render_into`'s recording (`total − submit − readback`), which is where
/// `ddgi prepare` walks vertices. `render_into` p99 on lavapipe is the maximum of a short
/// sample and tracks submit time: two runs of the same 8192-vertex hull swapped which one
/// looked slower. Both p50 and p99 of `render_into` and of encode are recorded. The gate
/// is encode p50. On lavapipe this hull's encode p50 sat 14–17 ms above the box (0.3 ms);
/// a bounds cache leaves about 0, under this slack.
pub const ENCODE_P50_SLACK_MS: f32 = 8.0;
const WARMUP: u32 = 5;
const FRAMES: u32 = 16;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;

/// `render_into` CPU of one run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuStats {
    /// Measured frames.
    pub frames: u32,
    /// `FrameCpuTimings::total_ms` p50.
    pub render_into_p50_ms: f32,
    /// `FrameCpuTimings::total_ms` p99.
    pub render_into_p99_ms: f32,
    /// `FrameCpuTimings::encode_ms` p50 (recording, including `ddgi prepare`).
    pub encode_p50_ms: f32,
    /// `FrameCpuTimings::encode_ms` p99.
    pub encode_p99_ms: f32,
}

/// The hull run against the box control.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitsReport {
    /// Instances drawn.
    pub instances: u32,
    /// Vertices in the hull buffer.
    pub hull_vertices: u32,
    /// The hull run.
    pub hull: CpuStats,
    /// The same instances, box vertices only.
    pub control: CpuStats,
    /// The probes ran on the hull frames.
    pub probes: bool,
    /// The check holds.
    pub passed: bool,
    /// One line for the smoke row.
    pub reason: String,
}

impl UnitsReport {
    /// The smoke row's evidence line.
    #[must_use]
    pub fn evidence(&self) -> String {
        format!(
            "{} units, hull {} verts: render_into p50 {:.2} / p99 {:.2} ms (box p50 {:.2} / p99 {:.2}), encode p50 {:.2} / p99 {:.2} ms (box encode p50 {:.2} / p99 {:.2}, slack {ENCODE_P50_SLACK_MS} ms)",
            self.instances,
            self.hull_vertices,
            self.hull.render_into_p50_ms,
            self.hull.render_into_p99_ms,
            self.control.render_into_p50_ms,
            self.control.render_into_p99_ms,
            self.hull.encode_p50_ms,
            self.hull.encode_p99_ms,
            self.control.encode_p50_ms,
            self.control.encode_p99_ms
        )
    }
}

/// `true` when the hull's encode p50 stays within [`ENCODE_P50_SLACK_MS`] of the box control.
/// Both samples must be finite. Encode time is the part of `render_into` that walks vertices.
#[must_use]
pub fn within_budget(hull_encode_p50_ms: f32, control_encode_p50_ms: f32) -> bool {
    hull_encode_p50_ms.is_finite()
        && control_encode_p50_ms.is_finite()
        && hull_encode_p50_ms <= control_encode_p50_ms + ENCODE_P50_SLACK_MS
}

/// Renders the box control and the hull and scores the budget.
///
/// # Errors
///
/// A frame failed to record, or the probes never ran.
#[tracing::instrument(skip_all)]
pub fn run(device: &RenderDevice) -> Result<UnitsReport, LookdevError> {
    let _span = tracing::info_span!("moving_units", instances = INSTANCES).entered();
    let (box_v, indices) = regia_render::box_mesh([1.0, 0.6, 2.0]);
    let hull_v = pad_vertices(&box_v, HULL_VERTICES);
    let material = LabMaterial {
        base_color: [0.45, 0.46, 0.48, 1.0],
        roughness: 0.7,
        ..LabMaterial::default()
    };
    let post = post();
    let quality = quality();
    let control = measure(device, &box_v, &indices, &material, &post, quality)?;
    let (hull, probes) = measure_probes(device, &hull_v, &indices, &material, &post, quality)?;
    if !probes {
        return Err(LookdevError::Config(
            "ddgi prepare did not run; the vertex walk was not measured".into(),
        ));
    }
    let finite = hull.encode_p50_ms.is_finite() && control.encode_p50_ms.is_finite();
    let passed = finite && within_budget(hull.encode_p50_ms, control.encode_p50_ms);
    let reason = if !finite {
        format!(
            "encode times are not finite (hull {:.2} ms, box {:.2} ms)",
            hull.encode_p50_ms, control.encode_p50_ms
        )
    } else if passed {
        format!(
            "encode p50 {:.2} ms is within {ENCODE_P50_SLACK_MS} ms of the box ({:.2} ms); render_into p50 {:.2} / p99 {:.2} ms",
            hull.encode_p50_ms, control.encode_p50_ms, hull.render_into_p50_ms, hull.render_into_p99_ms
        )
    } else {
        format!(
            "encode p50 {:.2} ms exceeds the box ({:.2} ms) by more than {ENCODE_P50_SLACK_MS} ms; render_into p50 {:.2} / p99 {:.2} ms",
            hull.encode_p50_ms, control.encode_p50_ms, hull.render_into_p50_ms, hull.render_into_p99_ms
        )
    };
    Ok(UnitsReport {
        instances: INSTANCES,
        hull_vertices: hull_v.len() as u32,
        hull,
        control,
        probes,
        passed,
        reason,
    })
}

/// Repeats the first vertex until `count`. The indexed triangles stay the box, so the one-time
/// brick bake and the draws do not grow; `ddgi prepare` still walks every vertex.
fn pad_vertices(corners: &[LabVertex], count: usize) -> Vec<LabVertex> {
    let mut out = corners.to_vec();
    if let Some(pad) = corners.first() {
        out.resize(count.max(corners.len()), *pad);
    }
    out
}

fn measure(
    device: &RenderDevice,
    vertices: &[LabVertex],
    indices: &[u32],
    material: &LabMaterial,
    post: &PostSettings,
    quality: FrameQuality,
) -> Result<CpuStats, LookdevError> {
    Ok(measure_probes(device, vertices, indices, material, post, quality)?.0)
}

fn measure_probes(
    device: &RenderDevice,
    vertices: &[LabVertex],
    indices: &[u32],
    material: &LabMaterial,
    post: &PostSettings,
    quality: FrameQuality,
) -> Result<(CpuStats, bool), LookdevError> {
    let models = placements();
    let meshes: Vec<FrameMesh<'_>> = models
        .iter()
        .enumerate()
        .map(|(i, model)| FrameMesh {
            is_static: false,
            // One hull asset: one brick. Each instance is its own object, and prepare walks
            // the shared vertex slice once per instance until the bounds are cached.
            mesh_key: 1,
            material_key: 2,
            object_key: 1_000 + i as u64,
            vertices,
            indices,
            material,
            model: *model,
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let mut frame = FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 6.0, 18.0],
        target: [0.0, 0.5, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 20.0,
        double_sided: false,
        post: Some(post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.3], [1.0, 1.0, 1.0], 40_000.0),
        local_lights: &[],
        quality,
        meshes,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("moving units"),
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
    let mut renderer = FrameRenderer::new();
    let settings = DdgiSettings {
        cascades: 1,
        dims: [8, 4, 8],
        bake_on_caller: true,
        emitters: false,
        max_instances_per_cascade: INSTANCES,
        ..DdgiSettings::default()
    };
    renderer.set_ddgi_settings(settings);
    renderer.set_ddgi_enabled(Some(true));
    let mut totals = Vec::with_capacity(FRAMES as usize);
    let mut encodes = Vec::with_capacity(FRAMES as usize);
    let mut probes = false;
    for frame_i in 0..WARMUP + FRAMES {
        slide(&mut frame, frame_i);
        renderer.render_into(
            device,
            &frame,
            FrameTarget::full(&view, wgpu::TextureFormat::Rgba8Unorm, WIDTH, HEIGHT),
        )?;
        probes |= renderer.ddgi_active();
        if frame_i >= WARMUP {
            let cpu: FrameCpuTimings = renderer.cpu_timings();
            totals.push(cpu.total_ms);
            encodes.push(cpu.encode_ms);
        }
    }
    Ok((
        CpuStats {
            frames: totals.len() as u32,
            render_into_p50_ms: percentile(&totals, 0.5),
            render_into_p99_ms: percentile(&totals, 0.99),
            encode_p50_ms: percentile(&encodes, 0.5),
            encode_p99_ms: percentile(&encodes, 0.99),
        },
        probes,
    ))
}

/// Slides every instance along X. The vertex data does not change, so a bounds cache hits
/// and a per-frame vertex walk does not.
fn slide(frame: &mut FrameScene<'_>, frame_i: u32) {
    let dx = frame_i as f32 * 0.05;
    for (i, mesh) in frame.meshes.iter_mut().enumerate() {
        let base = placements_at(i);
        mesh.model = translation([base[0] + dx, base[1], base[2]]);
    }
}

fn placements() -> Vec<[[f32; 4]; 4]> {
    (0..INSTANCES as usize)
        .map(|i| translation(placements_at(i)))
        .collect()
}

fn placements_at(index: usize) -> [f32; 3] {
    let side = (INSTANCES as f32).sqrt().ceil() as i32;
    let x = (index as i32 % side - side / 2) as f32 * 3.0;
    let z = (index as i32 / side - side / 2) as f32 * 3.0;
    [x, 0.6, z]
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
    quality.gi = GiMode::Probes;
    quality.gi_probes = true;
    quality.reflection_probes = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.physical_atmosphere = false;
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.horizontal_plus = false;
    quality.contact_shadows = false;
    quality.gpu_driven = false;
    quality.paged_sun_shadows = false;
    quality.near_sun_tile = 0;
    quality.screen_space_refraction = false;
    quality.emissive_lights = 0;
    quality.msaa_samples = 1;
    quality.csm_cascade_count = 1;
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
    fn budget_rejects_a_vertex_walk_and_accepts_a_cache() {
        assert!(!within_budget(40.0, 10.0), "a 30 ms walk is over the slack");
        assert!(
            within_budget(12.0, 10.0),
            "two milliseconds is the cache, not the walk"
        );
        assert!(!within_budget(f32::NAN, 10.0));
        assert_eq!(INSTANCES, 180);
        const {
            assert!(HULL_VERTICES > 1_000);
        }
    }

    #[test]
    fn padding_keeps_the_box_triangles_and_grows_the_buffer() {
        let (corners, indices) = regia_render::box_mesh([1.0, 1.0, 1.0]);
        let padded = pad_vertices(&corners, HULL_VERTICES);
        assert_eq!(padded.len(), HULL_VERTICES);
        assert_eq!(indices.len(), 36, "twelve triangles");
        assert_eq!(padded[0].position, corners[0].position);
    }
}
