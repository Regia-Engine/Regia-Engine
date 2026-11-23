// SPDX-License-Identifier: MPL-2.0
//! Where the `specs/27` § 2.2 `specular_flip` row's light went (ADR-3190): `sssr_specular`'s metal
//! sphere scored 0.287 FLIP against the path tracer on 2026-10-08 and 0.612 on 2026-11-05, with the
//! real-time indirect specular at 0.31× the traced luminance and screen-space reflections adding
//! almost nothing. This file renders the same scene (`sssr_specular.rs`'s `ibl_spheres` layout:
//! Earth sky, the level probe at the metal sphere) and splits the real-time side by feature, so the
//! regression has a name and a pinned number.
//!
//! Hand-run on a device (lavapipe is one; a few minutes):
//! `cargo test -p regia_pt --test specular_regression -- --ignored --test-threads=1 --nocapture`.

use regia_core::glam::Vec3;
use regia_pt::extract::{ExtractOptions, atmosphere_sky, from_frame};
use regia_pt::{PathTracer, PtSettings};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabMaterial, LabMesh, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode,
};

const W: u32 = 160;
const H: u32 = 96;
const EV: f32 = 14.0;
const METAL: [f32; 3] = [-0.55, 0.0, 0.0];
const RED: [f32; 3] = [0.55, 0.0, 0.0];
const RADIUS: f32 = 0.42;
const EYE: [f32; 3] = [0.0, 0.3, 1.85];
const FOV: f32 = 0.6981317; // 40°

fn toward_sun() -> [f32; 3] {
    let (el, az) = (35f32.to_radians(), 120f32.to_radians());
    [el.cos() * az.cos(), el.sin(), el.cos() * az.sin()]
}

/// `sssr_specular.rs`'s scene, unchanged.
fn scene() -> LabScene {
    let sphere = |at: [f32; 3], material: usize| {
        let (v, i) = regia_render::uv_sphere(RADIUS, 64, 32);
        LabMesh {
            vertices: regia_render::translate_mesh(&v, at),
            indices: i,
            material,
            ..LabMesh::unskinned()
        }
    };
    let (gv, gi) = regia_render::box_mesh([30.0, 0.05, 30.0]);
    let ground = LabMesh {
        vertices: regia_render::translate_mesh(&gv, [0.0, -RADIUS - 0.05, 0.0]),
        indices: gi,
        material: 2,
        ..LabMesh::unskinned()
    };
    let s = toward_sun();
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: FOV,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: EV,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-s[0], -s[1], -s[2]], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.95, 0.95, 0.97, 1.0],
                metallic: 1.0,
                roughness: 0.15,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.75, 0.25, 0.15, 1.0],
                metallic: 0.0,
                roughness: 0.65,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.3, 0.3, 0.3, 1.0],
                metallic: 0.0,
                roughness: 0.9,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![sphere(METAL, 0), sphere(RED, 1), ground],
    }
}

/// `sssr_specular.rs`'s quality: Desktop High, clouds and fog off, SSR off (the shared base).
fn base_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q.ssr = false;
    q.gpu_driven = true;
    q
}

fn outdoor() -> OutdoorParams {
    OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
        ..OutdoorParams::default()
    }
}

/// The metal sphere's mean luminance in the full frame, the sun-only frame and their difference
/// (`sssr_specular.rs`'s "real-time indirect"), with `quality` and the level probe on or off.
fn split(device: &RenderDevice, quality: FrameQuality, probe: bool, frames: u32) -> (f32, f32, f32) {
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(outdoor());
    if probe {
        let p = renderer
            .capture_reflection_probe(device, &frame, 1, METAL, [3.0, 1.5, 3.0])
            .expect("probe capture");
        renderer.set_reflection_probes(vec![p]);
    }
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device, &frame).expect("frame");
    }
    let full = renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("hdr")
        .rgb;
    let mut direct = frame.clone();
    direct.ibl = false;
    direct.ambient_lux = 0.0;
    direct.quality.gi = GiMode::Ambient;
    direct.quality.screen_space_gi = false;
    direct.quality.ssr = false;
    direct.quality.reflection_probes = false;
    direct.quality.ssao = SsaoMode::Off;
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &direct).expect("direct frame");
    let sun = renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("hdr")
        .rgb;
    let indirect: Vec<[f32; 3]> = full
        .iter()
        .zip(&sun)
        .map(|(f, d)| {
            [
                (f[0] - d[0]).max(0.0),
                (f[1] - d[1]).max(0.0),
                (f[2] - d[2]).max(0.0),
            ]
        })
        .collect();
    let mask = metal_mask();
    (
        masked_luminance(&full, &mask),
        masked_luminance(&sun, &mask),
        masked_luminance(&indirect, &mask),
    )
}

fn metal_mask() -> Vec<bool> {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(Vec3::from_array(EYE), Vec3::ZERO, Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(FOV, W as f32 / H as f32, 0.1, 20.0);
    let inv = (proj * view).inverse();
    let eye = Vec3::from_array(EYE);
    let center = Vec3::from_array(METAL);
    let hits = |x: f32, y: f32| {
        let ndc = Vec3::new(x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0, 1.0);
        let dir = (inv.project_point3(ndc) - eye).normalize();
        let oc = eye - center;
        let b = oc.dot(dir);
        b * b - (oc.length_squared() - (RADIUS * 0.97) * (RADIUS * 0.97)) > 0.0
    };
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| hits(x as f32 + 0.5, y as f32 + 0.5))
        .collect()
}

fn masked_luminance(img: &[[f32; 3]], mask: &[bool]) -> f32 {
    let (sum, n) = img
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (c, _)| {
            (
                s + f64::from(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]),
                n + 1,
            )
        });
    (sum / n.max(1) as f64) as f32
}

#[test]
#[ignore = "hand-run diagnostic on a device (ADR-3190)"]
fn metal_sphere_indirect_specular_by_feature() {
    let device = RenderDevice::headless().expect("headless device");
    type Tweak = fn(&mut FrameQuality);
    let variants: [(&str, Tweak, bool); 10] = [
        ("desktop_high (probe)", |_| {}, true),
        ("desktop_high (no probe)", |_| {}, false),
        (
            "reflection_occlusion off",
            |q| q.reflection_occlusion = false,
            true,
        ),
        ("bent_normals off", |q| q.bent_normals = false, true),
        ("ssao off", |q| q.ssao = SsaoMode::Off, true),
        ("gi_probes off", |q| q.gi_probes = false, true),
        (
            "gi ambient, no ssgi",
            |q| {
                q.gi = GiMode::Ambient;
                q.screen_space_gi = false;
            },
            true,
        ),
        ("reflection_probes off", |q| q.reflection_probes = false, true),
        ("baked_gi off", |q| q.baked_gi = false, true),
        (
            "all of the above off",
            |q| {
                q.reflection_occlusion = false;
                q.bent_normals = false;
                q.ssao = SsaoMode::Off;
                q.gi_probes = false;
                q.gi = GiMode::Ambient;
                q.screen_space_gi = false;
                q.reflection_probes = false;
                q.baked_gi = false;
            },
            false,
        ),
    ];
    for (name, tweak, probe) in variants {
        let mut q = base_quality();
        tweak(&mut q);
        let (full, sun, indirect) = split(&device, q, probe, 2);
        eprintln!("RT metal [{name}]: full {full:.1}, sun-only {sun:.1}, indirect {indirect:.1}");
    }
    // The probes converge over frames (`sssr_specular` scores its stochastic SSR after 24).
    for (name, gi_probes) in [
        ("desktop_high, 24 frames", true),
        ("gi_probes off, 24 frames", false),
    ] {
        let mut q = base_quality();
        q.gi_probes = gi_probes;
        let (full, sun, indirect) = split(&device, q, true, 24);
        eprintln!("RT metal [{name}]: full {full:.1}, sun-only {sun:.1}, indirect {indirect:.1}");
    }
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(base_quality());
    let pt_scene = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(atmosphere_sky(&outdoor().planet, toward_sun(), EYE[1].max(1.0))),
            sun_angular_radius: outdoor().planet.sun_angular_radius,
            terrain: None,
            skip_blended: true,
            ..ExtractOptions::default()
        },
    );
    let pt = PathTracer::new(&device)
        .expect("compile")
        .render(
            &pt_scene,
            &PtSettings {
                spp: 256,
                max_bounces: 6,
                ..PtSettings::default()
            },
        )
        .expect("trace");
    let mask = metal_mask();
    eprintln!(
        "PT metal: indirect_specular {:.1}, direct {:.1}",
        masked_luminance(&pt.indirect_specular, &mask),
        masked_luminance(&pt.direct, &mask)
    );
}
