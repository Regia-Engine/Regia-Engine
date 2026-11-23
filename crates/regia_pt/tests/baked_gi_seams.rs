// SPDX-License-Identifier: MPL-2.0
//! Baked GI across the seam between two separately baked tiles (ADR-1097): one scene baked by
//! the path tracer as a single volume and as two tiles cut one probe column apart (the way a
//! streamed world is baked), then looked up along floor lines that cross the seam.
//!
//! - The two tiles blended (`sample_static_gi_blend`) read as the single volume does, within the
//!   bake's noise and its edge filtering, at full coverage.
//! - Along the lines the blend has no step: its largest change between samples 10 cm apart is
//!   within three times the single volume's own (the tiles' edge probes are filtered without
//!   their neighbors across the seam, and the bake is noisy), while picking one tile or the
//!   other at the seam (no blend) jumps by more than four times the blend's.
//!
//! The thresholds are set from the bake's noise (128 rays × 8 samples a probe), not measured
//! yet on the reference adapter; the test prints every number it checks.
//!
//! The scene: the baked-GI trench (a floor between two long walls, a low sun across them), its
//! sunlit far wall orange west of x = 0 and blue east of it, so the bounce light changes
//! color right at the seam. Runs on whatever adapter `RenderDevice::headless` picks (lavapipe
//! in CI). No golden image.

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, TerrainLayer, VolumeLayout};
use regia_core::glam::Mat4;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume};
use regia_pt::{PathTracer, PtScene};
use regia_render::baked_gi::{BakedGiLayer, sample_static_gi_blend};
use regia_render::{
    BakedGiFrameLights, BakedGiSettings, DirectionalLightUniform, FrameQuality, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, StaticGiSample, box_mesh,
};

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("baked GI needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn lambert(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness: 0.95,
        ..LabMaterial::default()
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// The trench, its far (−z) wall in two colors split at x = 0.
fn scene() -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 13.5;
    LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 1.6, 2.2],
        target: [0.0, 0.6, -1.5],
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 13.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -0.574, -0.819], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            lambert([0.45; 3]),
            lambert([0.85, 0.5, 0.3]),
            lambert([0.6; 3]),
            lambert([0.25, 0.45, 0.85]),
        ],
        meshes: vec![
            block([0.0, -0.25, 0.0], [16.0, 0.25, 8.0], 0),
            block([-7.5, 2.0, -3.15], [7.5, 2.0, 0.15], 1),
            block([7.5, 2.0, -3.15], [7.5, 2.0, 0.15], 3),
            block([0.0, 2.0, 3.15], [15.0, 2.0, 0.15], 2),
        ],
    }
}

/// A flat terrain layer over the trench floor: `columns` × 5 columns, 2 m × 1.4 m, four levels,
/// column 0 at world x = `x0`.
fn layout(x0: f32, columns: u32) -> VolumeLayout {
    VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [x0, -2.8],
            spacing: [2.0, 1.4],
            counts: [columns, 5],
            levels: vec![0.5, 1.5, 3.0, 4.5],
            ground: vec![0.0; (columns * 5) as usize],
        }),
        bricks: Vec::new(),
        moment_resolution: 8,
        max_distance: 16.0,
    }
}

fn bake(scene: &PtScene, layout: &VolumeLayout) -> Arc<BakedGiVolume> {
    let settings = GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 128,
            samples: 8,
            max_bounces: 4,
            seed: 7,
            samples_per_submit: 8,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    };
    let bake = bake_gi_volume(tracer(), scene, layout, &settings, |_, _, _| true).expect("bake");
    eprintln!("baked {:?}", bake.report);
    Arc::new(bake.volume)
}

/// The single volume (x = −12..12) and the two tiles (x = −12..0 and 2..12; the seam cell is
/// x = 0..2), baked once.
fn volumes() -> &'static (Arc<BakedGiVolume>, Arc<BakedGiVolume>, Arc<BakedGiVolume>) {
    static V: OnceLock<(Arc<BakedGiVolume>, Arc<BakedGiVolume>, Arc<BakedGiVolume>)> = OnceLock::new();
    V.get_or_init(|| {
        let lab = scene();
        let pt = from_frame(
            &lab.as_frame_scene_with_quality(quality()),
            &ExtractOptions::default(),
        );
        (
            bake(&pt, &layout(-12.0, 13)),
            bake(&pt, &layout(-12.0, 7)),
            bake(&pt, &layout(2.0, 6)),
        )
    })
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn lights(lab: &LabScene) -> BakedGiFrameLights {
    let d = lab.directional.direction;
    let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    BakedGiFrameLights {
        sun: Some(lab.directional.color.map(|c| c * lab.directional.illuminance)),
        to_sun: Some([-d[0] / l, -d[1] / l, -d[2] / l]),
        fill: None,
    }
}

/// Floor receivers every 10 cm from x = −10 to 10 on the line z = `z`.
fn line(z: f32) -> Vec<[f32; 3]> {
    (0..=200).map(|i| [-10.0 + i as f32 * 0.1, 0.03, z]).collect()
}

/// The largest change of any channel between neighboring samples (the walls differ in hue,
/// so a seam shows in red and blue more than in luminance).
fn largest_step(samples: &[StaticGiSample]) -> f32 {
    samples
        .windows(2)
        .flat_map(|w| (0..3).map(move |c| (w[1].irradiance_over_pi[c] - w[0].irradiance_over_pi[c]).abs()))
        .fold(0.0, f32::max)
}

#[test]
fn two_baked_tiles_meet_without_a_seam() {
    let lab = scene();
    let (whole, west, east) = volumes();
    let settings = BakedGiSettings::default();
    let lights = lights(&lab);
    let up = [0.0, 1.0, 0.0];
    let tiles = [BakedGiLayer::new(west.clone()), BakedGiLayer::new(east.clone())];
    let one = [BakedGiLayer::new(whole.clone())];
    for z in [-1.4, 0.0, 1.4] {
        let at = |layers: &[BakedGiLayer], p: [f32; 3]| {
            let v = [lab.eye[0] - p[0], lab.eye[1] - p[1], lab.eye[2] - p[2]];
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            sample_static_gi_blend(layers, &settings, &lights, p, up, [v[0] / l, v[1] / l, v[2] / l])
        };
        let points = line(z);
        let blended: Vec<StaticGiSample> = points.iter().map(|p| at(&tiles, *p)).collect();
        let single: Vec<StaticGiSample> = points.iter().map(|p| at(&one, *p)).collect();
        // Picking a tile at the seam's middle instead of blending.
        let picked: Vec<StaticGiSample> = points
            .iter()
            .map(|p| at(if p[0] < 1.0 { &tiles[..1] } else { &tiles[1..] }, *p))
            .collect();
        let mut worst = 0.0f32;
        let mut sum = 0.0f32;
        for ((p, b), s) in points.iter().zip(&blended).zip(&single) {
            assert!(b.coverage > 0.999, "the tiles leave a hole at {p:?}: {b:?}");
            let rel = (lum(b.irradiance_over_pi) - lum(s.irradiance_over_pi)).abs()
                / lum(s.irradiance_over_pi).max(1e-3);
            worst = worst.max(rel);
            sum += rel;
        }
        let mean = sum / points.len() as f32;
        let (step_blend, step_single, step_picked) = (
            largest_step(&blended),
            largest_step(&single),
            largest_step(&picked),
        );
        let level = lum(single[100].irradiance_over_pi);
        eprintln!(
            "z {z}: tiles vs single volume worst {worst:.3} mean {mean:.3}; largest 10 cm step: \
             blend {step_blend:.2}, single {step_single:.2}, picked {step_picked:.2} (light ~{level:.0})"
        );
        assert!(
            worst < 0.15,
            "z {z}: the tiles depart from the single volume by {worst:.3}"
        );
        assert!(mean < 0.05, "z {z}: mean departure {mean:.3}");
        assert!(
            step_blend <= 3.0 * step_single + 0.005 * level,
            "z {z}: the blend steps by {step_blend:.2}, the single volume by {step_single:.2}"
        );
        assert!(
            step_picked > 4.0 * step_blend,
            "z {z}: the test sees a seam (picked {step_picked:.2} vs blend {step_blend:.2})"
        );
    }
}
