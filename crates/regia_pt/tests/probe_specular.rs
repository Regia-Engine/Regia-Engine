// SPDX-License-Identifier: MPL-2.0
//! Probe specular end to end (ADR-1103): the trench of `baked_gi_runtime.rs` baked once by the
//! path tracer, drawn with the baked volume in both `ProbeSpecular` modes.
//!
//! - A satin metal floor (perceptual roughness 0.7, past the screen-space reflections' cutoff) in
//!   the trench's shadow, without sky image-based light: the earlier rule gave it no indirect
//!   specular at all under the bake (it was the sky map's alone); the lobe reads the baked light,
//!   so the floor shows the sunlit wall's bounce.
//! - A rough dielectric floor under the sky map barely changes between the modes: at the rough
//!   end the lobe meets the probes' irradiance the earlier rule used, and the diffuse probe light
//!   is the same in both.
//! - A hand-run look report (`--ignored look`): the floor's indirect specular in both modes
//!   against the path tracer's `indirect_specular` AOV, for the lead's look review.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, TerrainLayer, VolumeLayout};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume};
use regia_pt::{PathTracer, PtScene, PtSettings};
use regia_render::{
    BakedGiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh,
    LabScene, PostSettings, ProbeSpecular, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
    box_mesh,
};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("probe specular needs a GPU; lavapipe is a GPU here"))
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

fn material(rgb: [f32; 3], metallic: f32, roughness: f32) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        metallic,
        roughness,
        ..LabMaterial::default()
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// The trench (6 m wide, 4 m walls, long in x, a low sun across it: the far wall's inner face is
/// sunlit, the floor is in the near wall's shadow) with `floor` as the floor's material.
fn trench(floor: LabMaterial, ibl: bool) -> LabScene {
    let meshes = vec![
        block([0.0, -0.25, 0.0], [16.0, 0.25, 8.0], 0),
        block([0.0, 2.0, -3.15], [15.0, 2.0, 0.15], 1),
        block([0.0, 2.0, 3.15], [15.0, 2.0, 0.15], 2),
    ];
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 13.5;
    LabScene {
        width: W,
        height: H,
        eye: [-3.0, 1.6, 2.2],
        target: [2.0, 0.6, -1.5],
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl,
        exposure_ev: 13.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -0.574, -0.819], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            floor,
            material([0.85, 0.5, 0.3], 0.0, 0.95),
            material([0.6; 3], 0.0, 0.95),
        ],
        meshes,
    }
}

/// One bake for every test: the trench with a grey lambert floor (the floor material of a draw
/// only changes what that draw reflects, not the bake it reads).
fn volume() -> Arc<BakedGiVolume> {
    static V: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    V.get_or_init(|| {
        let scene = trench(material([0.45; 3], 0.0, 0.95), false);
        let pt: PtScene = from_frame(
            &scene.as_frame_scene_with_quality(quality()),
            &ExtractOptions::default(),
        );
        let counts = [13u32, 5u32];
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [-12.0, -2.8],
                spacing: [2.0, 1.4],
                counts,
                levels: vec![0.5, 1.5, 3.0, 4.5],
                ground: vec![0.0; (counts[0] * counts[1]) as usize],
            }),
            bricks: Vec::new(),
            moment_resolution: 8,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let settings = GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: 64,
                samples: 4,
                max_bounces: 3,
                seed: 11,
                samples_per_submit: 4,
                ..ProbeTraceSettings::default()
            },
            ..GiBakeSettings::default()
        };
        let bake = bake_gi_volume(tracer(), &pt, &layout, &settings, |_, _, _| true).expect("bake");
        Arc::new(bake.volume)
    })
    .clone()
}

/// The scene drawn under the bake with `mode`, two frames (the second is final).
fn draw(scene: &LabScene, mode: ProbeSpecular) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume()));
    renderer.set_baked_gi_settings(BakedGiSettings {
        specular: mode,
        ..BakedGiSettings::default()
    });
    let mut last = Vec::new();
    for _ in 0..2 {
        last = renderer
            .render_offscreen(device(), &scene.as_frame_scene_with_quality(quality()))
            .expect("render")
            .pixels;
    }
    assert!(renderer.baked_gi_status().active, "the bake lights the frame");
    last
}

/// Mean linear RGB over the pixel box `x0..x1` × `y0..y1`.
fn linear_rgb(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> [f32; 3] {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = [0.0f32; 3];
    let mut n = 0.0f32;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            for (s, c) in sum.iter_mut().zip(&pixels[i..i + 3]) {
                *s += lin(*c);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The floor in front of the camera (lower middle of the frame), as `baked_gi_runtime.rs`.
const FLOOR: (u32, u32, u32, u32) = (50, 70, 110, 92);

#[test]
fn a_satin_metal_floor_reflects_the_baked_light() {
    let scene = trench(material([0.9; 3], 1.0, 0.7), false);
    let lobe = draw(&scene, ProbeSpecular::Lobe);
    let rough_only = draw(&scene, ProbeSpecular::RoughOnly);
    let (x0, y0, x1, y1) = FLOOR;
    let lit = luma(linear_rgb(&lobe, x0, y0, x1, y1));
    let old = luma(linear_rgb(&rough_only, x0, y0, x1, y1));
    eprintln!("shadowed metal floor, no sky map: lobe {lit:.5}, rough_only {old:.5}");
    // Past the screen-space reflections' roughness cutoff nothing else lights a metal floor in
    // shadow without the sky map: under the bake it had no indirect light before.
    assert!(
        lit > old * 1.2 + 1e-4,
        "the lobe must reflect the baked light off the metal floor ({lit} vs {old})"
    );
}

#[test]
fn a_rough_floor_barely_changes_between_the_modes() {
    let scene = trench(material([0.45; 3], 0.0, 0.95), true);
    let lobe = draw(&scene, ProbeSpecular::Lobe);
    let rough_only = draw(&scene, ProbeSpecular::RoughOnly);
    let (x0, y0, x1, y1) = FLOOR;
    let a = luma(linear_rgb(&lobe, x0, y0, x1, y1));
    let b = luma(linear_rgb(&rough_only, x0, y0, x1, y1));
    eprintln!("rough grey floor under the sky map: lobe {a:.5}, rough_only {b:.5}");
    assert!(b > 0.0, "the floor is lit");
    assert!(
        (a - b).abs() <= 0.05 * b,
        "at the rough end the lobe meets the probe irradiance ({a} vs {b})"
    );
}

/// The world point where the view ray through full-resolution pixel `px` (its centre) meets the
/// plane y = 0, for the lab camera (as `baked_gi_runtime.rs`).
fn floor_hit(scene: &LabScene, px: [f32; 2]) -> Option<Vec3> {
    let eye = Vec3::from_array(scene.eye);
    let fwd = (Vec3::from_array(scene.target) - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let aspect = scene.width as f32 / scene.height as f32;
    let fov_y = if quality().horizontal_plus {
        regia_render::output::horizontal_plus_fov_y(scene.fov_y, aspect)
    } else {
        scene.fov_y
    };
    let t = (fov_y * 0.5).tan();
    let ndc = [
        (px[0] + 0.5) / scene.width as f32 * 2.0 - 1.0,
        1.0 - (px[1] + 0.5) / scene.height as f32 * 2.0,
    ];
    let dir = (fwd + right * ndc[0] * t * aspect + up * ndc[1] * t).normalize();
    (dir.y < -1e-3).then(|| eye + dir * (-eye.y / dir.y))
}

/// The real-time indirect light under the bake with `mode` (full frame minus a sun-only frame),
/// screen-space reflections off so the floor's indirect specular is the probes' alone.
fn realtime_indirect(scene: &LabScene, mode: ProbeSpecular) -> Vec<[f32; 3]> {
    let mut q = quality();
    q.ssr = false;
    let frame = scene.as_frame_scene_with_quality(q);
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume()));
    renderer.set_baked_gi_settings(BakedGiSettings {
        specular: mode,
        ..BakedGiSettings::default()
    });
    renderer.render_offscreen(device(), &frame).expect("frame");
    renderer.request_hdr_capture();
    renderer.render_offscreen(device(), &frame).expect("frame");
    let full = renderer
        .take_hdr_capture(device())
        .expect("readback")
        .expect("hdr");
    let mut direct = frame.clone();
    direct.ibl = false;
    direct.ambient_lux = 0.0;
    direct.quality.gi = GiMode::Ambient;
    direct.quality.baked_gi = false;
    direct.quality.screen_space_gi = false;
    direct.quality.ssr = false;
    direct.quality.reflection_probes = false;
    direct.quality.ssao = SsaoMode::Off;
    renderer.request_hdr_capture();
    renderer
        .render_offscreen(device(), &direct)
        .expect("direct frame");
    let sun = renderer
        .take_hdr_capture(device())
        .expect("readback")
        .expect("hdr");
    full.rgb
        .iter()
        .zip(&sun.rgb)
        .map(|(f, d)| {
            [
                (f[0] - d[0]).max(0.0),
                (f[1] - d[1]).max(0.0),
                (f[2] - d[2]).max(0.0),
            ]
        })
        .collect()
}

fn masked_luminance(img: &[[f32; 3]], mask: &[bool]) -> f32 {
    let (sum, n) = img
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (c, _)| {
            (s + f64::from(luma(*c)), n + 1)
        });
    (sum / n.max(1) as f64) as f32
}

/// For the look review (not the gate): the shadowed metal floor's indirect specular at two
/// roughnesses, both modes, against the path tracer. Prints the floor's mean luminance and the
/// relative error of each mode; asserts only that the lobe is nearer the reference than the
/// earlier rule.
#[test]
#[ignore = "PT reference look report: cargo test --release -p regia_pt --test probe_specular -- --ignored look"]
fn look_report_against_the_path_tracer() {
    for roughness in [0.35f32, 0.7] {
        let scene = trench(material([0.9; 3], 1.0, roughness), false);
        let mut q = quality();
        q.ssr = false;
        let pt_scene = from_frame(&scene.as_frame_scene_with_quality(q), &ExtractOptions::default());
        let reference = tracer()
            .render(
                &pt_scene,
                &PtSettings {
                    spp: 1024,
                    max_bounces: 4,
                    ..PtSettings::default()
                },
            )
            .expect("trace")
            .indirect_specular;
        // Floor pixels inside the trench, off its walls' feet.
        let mask: Vec<bool> = (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .map(|(x, y)| {
                floor_hit(&scene, [x as f32, y as f32]).is_some_and(|p| p.z.abs() < 2.7 && p.x.abs() < 12.0)
            })
            .collect();
        assert!(mask.iter().filter(|m| **m).count() > 500, "floor pixels");
        let pt = masked_luminance(&reference, &mask);
        let lobe = masked_luminance(&realtime_indirect(&scene, ProbeSpecular::Lobe), &mask);
        let old = masked_luminance(&realtime_indirect(&scene, ProbeSpecular::RoughOnly), &mask);
        let err = |v: f32| (v - pt).abs() / pt.max(1e-6);
        eprintln!(
            "metal floor r {roughness}: PT {pt:.2}, lobe {lobe:.2} ({:.0} % off), rough_only {old:.2} ({:.0} % off)",
            err(lobe) * 100.0,
            err(old) * 100.0
        );
        assert!(
            err(lobe) < err(old),
            "r {roughness}: the lobe must be nearer the path tracer ({lobe} vs {old}, PT {pt})"
        );
    }
}
