// SPDX-License-Identifier: MPL-2.0
//! The tangent-plane subsurface kernel and the per-reason counts in the frame (ADR-3170).
//!
//! A plate tilted away from the view by 0, 45 and 70 degrees carries a perfectly hard light edge
//! across its slope (its upper half's normals face the sun, its lower half's face away, on one
//! flat, depth-continuous surface). Each row of the frame is a known distance along the surface
//! from the edge, so the frame compares with the radial profile's half-plane response
//! ([`regia_render::subsurface::edge_response`]) measured on the surface. The screen kernel blurs
//! a slanted plate over more of itself than the profile says; the tangent-plane kernel does not.

use regia_core::glam::Vec3;
use regia_render::subsurface::edge_response;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SubsurfaceFactors, SubsurfaceKernel,
    SubsurfaceProfile, SubsurfaceUnsupported,
};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 160;
const EXTENT: f32 = 1.0;
const DISTANCE: f32 = 0.06;
const FOV_Y: f32 = 0.5;

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device")
}

/// The plate's normal and its in-plane direction up the slope, tilted `degrees` about x.
fn frame_of(degrees: f32) -> (Vec3, Vec3) {
    let theta = degrees.to_radians();
    (
        Vec3::new(0.0, theta.sin(), theta.cos()),
        Vec3::new(0.0, theta.cos(), -theta.sin()),
    )
}

/// The half of the plate between `s0` and `s1` up the slope, its normal `normal`.
fn half(degrees: f32, s0: f32, s1: f32, normal: Vec3, material: usize) -> LabMesh {
    let (_, up) = frame_of(degrees);
    let corner = |x: f32, s: f32| {
        let p = Vec3::X * x + up * s;
        LabVertex::from_pos_normal_uv(p.to_array(), normal.to_array(), [0.5 + 0.5 * x, 0.5 + 0.5 * s])
    };
    LabMesh {
        vertices: vec![
            corner(-EXTENT, s0),
            corner(EXTENT, s0),
            corner(EXTENT, s1),
            corner(-EXTENT, s1),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn matte(subsurface: SubsurfaceFactors) -> LabMaterial {
    LabMaterial {
        base_color: [0.8, 0.6, 0.5, 1.0],
        roughness: 1.0,
        ior: 1.001,
        subsurface,
        ..LabMaterial::default()
    }
}

fn plate(degrees: f32, materials: Vec<LabMaterial>, extra: Vec<LabMesh>) -> LabScene {
    let (normal, _) = frame_of(degrees);
    let mut meshes = vec![
        half(degrees, 0.0, EXTENT, normal, 0),
        half(degrees, -EXTENT, 0.0, -normal, 0),
    ];
    meshes.extend(extra);
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, DISTANCE],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.005,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new((-normal).to_array(), [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn quality(scatter: bool, kernel: SubsurfaceKernel) -> FrameQuality {
    FrameQuality {
        subsurface_scattering: scatter,
        subsurface_kernel: kernel,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

fn hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    quality: FrameQuality,
) -> Vec<[f32; 3]> {
    let scene = lab.as_frame_scene_with_quality(quality);
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists")
        .rgb
}

/// Millimeters up the slope from the edge at row `y`'s pixel centers (the plate is tilted about x,
/// so a row is one distance), `None` where the row misses the plate.
fn surface_mm(degrees: f32, y: u32) -> Option<f32> {
    let (normal, up) = frame_of(degrees);
    let eye = Vec3::new(0.0, 0.0, DISTANCE);
    let ndc_y = 1.0 - 2.0 * (y as f32 + 0.5) / HEIGHT as f32;
    let dir = Vec3::new(0.0, ndc_y * (FOV_Y * 0.5).tan(), -1.0);
    let t = -eye.dot(normal) / dir.dot(normal);
    (t > 0.0).then(|| (eye + dir * t).dot(up) * 1000.0)
}

/// Against the profile's response on the surface: the mean absolute error over the rows within one
/// reach of the edge (every channel, the columns' mean), and the measured and exact widths between
/// green's 25 % and 75 % on the surface, millimeters.
struct Fit {
    mean_error: f32,
    width_mm: f32,
    exact_width_mm: f32,
}

fn fit(degrees: f32, on: &[[f32; 3]], lit: [f32; 3]) -> Fit {
    let profile = SubsurfaceProfile::Skin;
    let reach = profile.reach_mm();
    let columns = WIDTH / 4..3 * WIDTH / 4;
    let (mut sum, mut n) = (0.0f32, 0u32);
    // (surface mm, measured green, exact green) per row, up the slope.
    let mut green = Vec::new();
    for y in 0..HEIGHT {
        let Some(s) = surface_mm(degrees, y) else {
            continue;
        };
        if s.abs() > 2.0 * reach {
            continue;
        }
        let mut mean = [0.0f32; 3];
        for x in columns.clone() {
            let value = on[(y * WIDTH + x) as usize];
            for ch in 0..3 {
                mean[ch] += value[ch] / lit[ch] / columns.len() as f32;
            }
        }
        let exact = edge_response(profile, s, reach);
        if s.abs() <= reach {
            for ch in 0..3 {
                sum += (mean[ch] - exact[ch]).abs();
                n += 1;
            }
        }
        green.push((s, mean[1], exact[1]));
    }
    green.sort_by(|a, b| a.0.total_cmp(&b.0));
    let crossing = |level: f32, pick: fn(&(f32, f32, f32)) -> f32| -> f32 {
        for pair in green.windows(2) {
            let (a, b) = (pick(&pair[0]), pick(&pair[1]));
            if (a - level) * (b - level) <= 0.0 && a != b {
                return pair[0].0 + (level - a) / (b - a) * (pair[1].0 - pair[0].0);
            }
        }
        f32::NAN
    };
    Fit {
        mean_error: sum / n.max(1) as f32,
        width_mm: crossing(0.75, |r| r.1) - crossing(0.25, |r| r.1),
        exact_width_mm: crossing(0.75, |r| r.2) - crossing(0.25, |r| r.2),
    }
}

/// The measured numbers of ADR-3170 (printed for the report): at 0 degrees both kernels fit the
/// profile alike; at 45 and 70 the tangent-plane kernel's error is below the screen kernel's.
#[test]
fn the_tangent_kernel_fits_a_slanted_plate_better_than_the_screen_kernel() {
    let device = device();
    let skin = SubsurfaceFactors::new(SubsurfaceProfile::Skin);
    let mut errors = Vec::new();
    for degrees in [0.0f32, 45.0, 70.0] {
        let lab = plate(degrees, vec![matte(skin)], Vec::new());
        let mut renderer = FrameRenderer::new();
        let off = hdr(
            &device,
            &mut renderer,
            &lab,
            quality(false, SubsurfaceKernel::Screen),
        );
        // The lit level: a row well up the lit half, past the blur.
        let lit_row = (0..HEIGHT)
            .find(|&y| surface_mm(degrees, y).is_some_and(|s| s < 3.0 * SubsurfaceProfile::Skin.reach_mm()))
            .expect("a lit row");
        let lit = off[(lit_row * WIDTH + WIDTH / 2) as usize];
        assert!(lit.iter().all(|&c| c > 0.1), "{degrees}°: lit {lit:?}");
        let mut fits = Vec::new();
        for kernel in [SubsurfaceKernel::Screen, SubsurfaceKernel::Tangent] {
            let on = hdr(&device, &mut renderer, &lab, quality(true, kernel));
            let stats = renderer.subsurface_stats();
            assert_eq!(
                (stats.reason, stats.drawn),
                (None, 2),
                "{degrees}° {kernel:?}: {stats:?}"
            );
            let f = fit(degrees, &on, lit);
            eprintln!(
                "{degrees:>4}° {:>7}: mean error {:.4}, green 25-75 % width {:.3} mm (profile {:.3} mm)",
                kernel.name(),
                f.mean_error,
                f.width_mm,
                f.exact_width_mm
            );
            fits.push(f);
        }
        errors.push((degrees, fits[0].mean_error, fits[1].mean_error));
    }
    let (_, screen0, tangent0) = errors[0];
    assert!(
        (screen0 - tangent0).abs() < 0.003,
        "a facing plate: screen {screen0}, tangent {tangent0}"
    );
    for &(degrees, screen, tangent) in &errors[1..] {
        assert!(
            tangent < screen,
            "{degrees}°: the tangent kernel's error {tangent} is not below the screen kernel's {screen}"
        );
    }
}

/// The screen kernel is the default, and asking for it is the default frame byte for byte.
/// The static-screen cache and the frozen temporal resolve keep a held camera's image until their
/// key changes. The kernel is in that key: a switch on a held view shows at once, on a tier that
/// keeps its gather and on one that keeps its composite.
#[test]
fn a_held_camera_shows_the_kernel_switch() {
    let device = device();
    let mut lab = plate(
        70.0,
        vec![matte(SubsurfaceFactors::new(SubsurfaceProfile::Skin))],
        Vec::new(),
    );
    lab.depth_prepass = true;
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: lab.exposure_ev,
        taa: true,
        ..PostSettings::default()
    };
    for preset in [
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let base = FrameQuality {
            subsurface_scattering: true,
            taau: true,
            ..FrameQuality::from_profile(&RenderProfile::from_preset(preset))
        };
        assert!(base.reuse_static_screen(), "{preset:?} keeps a held view");
        let mut renderer = FrameRenderer::new();
        let mut render = |kernel: SubsurfaceKernel| {
            let mut frame = lab.as_frame_scene_with_quality(FrameQuality {
                subsurface_kernel: kernel,
                ..base
            });
            frame.post = Some(&post);
            renderer.render_offscreen(&device, &frame).expect("frame").pixels
        };
        // The resolve freezes once a held view has converged; until then each frame is jittered.
        let mut held = render(SubsurfaceKernel::Screen);
        let mut frozen = false;
        for _ in 0..48 {
            let next = render(SubsurfaceKernel::Screen);
            frozen = next == held;
            held = next;
        }
        assert!(
            frozen,
            "{preset:?}: the held view stopped changing, so the cache is what serves it"
        );
        let (dark, bright) = held
            .iter()
            .fold((255u8, 0u8), |(lo, hi), &byte| (lo.min(byte), hi.max(byte)));
        assert!(
            bright < 255 || dark < 200,
            "{preset:?}: a frame that is not saturated ({dark}..{bright}), or no switch could show"
        );
        let switched = render(SubsurfaceKernel::Tangent);
        assert_ne!(switched, held, "{preset:?}: the switch shows on a held view");
    }
}

#[test]
fn the_screen_kernel_is_the_default_frame() {
    let device = device();
    let lab = plate(
        45.0,
        vec![matte(SubsurfaceFactors::new(SubsurfaceProfile::Skin))],
        Vec::new(),
    );
    let mut renderer = FrameRenderer::new();
    let default = renderer
        .render_offscreen(
            &device,
            &lab.as_frame_scene_with_quality(FrameQuality {
                subsurface_scattering: true,
                specular_aa: false,
                ..FrameQuality::LAB
            }),
        )
        .expect("frame")
        .pixels;
    let screen = renderer
        .render_offscreen(
            &device,
            &lab.as_frame_scene_with_quality(quality(true, SubsurfaceKernel::Screen)),
        )
        .expect("frame")
        .pixels;
    assert_eq!(FrameQuality::LAB.subsurface_kernel, SubsurfaceKernel::Screen);
    assert!(default == screen, "the screen kernel moved the default frame");
    let tangent = renderer
        .render_offscreen(
            &device,
            &lab.as_frame_scene_with_quality(quality(true, SubsurfaceKernel::Tangent)),
        )
        .expect("frame")
        .pixels;
    assert!(
        tangent != screen,
        "the tangent kernel changed nothing on a slanted plate"
    );
}

/// A frame counts the draws it could not scatter by reason, with stable codes.
#[test]
fn the_frame_counts_unsupported_draws_by_reason() {
    let device = device();
    let skin = SubsurfaceFactors::new(SubsurfaceProfile::Skin);
    let glassy = LabMaterial {
        transmission: 0.5,
        ..matte(skin)
    };
    let flat = LabMaterial {
        unlit: true,
        ..matte(skin)
    };
    let (normal, _) = frame_of(0.0);
    let lab = plate(
        0.0,
        vec![matte(skin), glassy, flat],
        vec![
            half(0.0, 0.2, 0.3, normal, 1),
            half(0.0, 0.3, 0.4, normal, 1),
            half(0.0, 0.4, 0.5, normal, 2),
        ],
    );
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(
            &device,
            &lab.as_frame_scene_with_quality(quality(true, SubsurfaceKernel::Screen)),
        )
        .expect("frame");
    let stats = renderer.subsurface_stats();
    assert_eq!(stats.unsupported, 3, "{stats:?}");
    let reasons: Vec<(&str, u32)> = stats
        .unsupported_reasons()
        .map(|(reason, count)| (reason.code(), count))
        .collect();
    assert_eq!(reasons, vec![("transmissive", 2), ("unlit", 1)]);
    assert_eq!(
        stats.unsupported_by[SubsurfaceUnsupported::Transmissive.index()],
        2
    );
    assert_eq!(
        stats.unsupported_by.iter().sum::<u32>() as usize,
        stats.unsupported
    );
    // The profile off: nothing is counted.
    renderer
        .render_offscreen(
            &device,
            &lab.as_frame_scene_with_quality(quality(false, SubsurfaceKernel::Screen)),
        )
        .expect("frame");
    let stats = renderer.subsurface_stats();
    assert_eq!(
        (stats.unsupported, stats.unsupported_reasons().count()),
        (0, 0),
        "{stats:?}"
    );
}
