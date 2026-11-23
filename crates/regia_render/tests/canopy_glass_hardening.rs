// SPDX-License-Identifier: MPL-2.0
//! Hardening of the own canopy's cheap shader (v0.1.19e, ADR-262; `lab_forward.wgsl`
//! `shade_canopy`, `frame/canopy_view.rs`). The synthetic cockpit of `canopy_glass.rs` (a 1 m
//! glass sphere cap around the camera, point lights, emissive panels, sun and sky IBL), with:
//!
//! - a camera outside the glass while the instance keeps `inside` set (a chase or replay
//!   camera): the full shader draws it, bit for bit, whatever the setting;
//! - a closed hangar around the cockpit under a first sun cascade too short to reach the glass:
//!   the cheap path falls back to cascade 1 and the canopy stays as dark as with cascade 0 over
//!   it (the coarse bias ramp is checked in `canopy_shader_checks.rs`);
//! - exterior glass, unlit glass and the nearest-layer option without refraction: unchanged,
//!   bit for bit;
//! - a cracked canopy with holes, and a frame without the HDR post stack (display-encoded
//!   output): the cheap path stays close to the full shader.

use std::sync::Mutex;

use regia_core::glam::{Mat4, Vec3};
use regia_render::glass::{GlassCrackSets, GlassDamage, GlassView, apply_one, glass_instance};
use regia_render::{
    ALPHA_PREMULTIPLIED, CanopyDraws, CanopyGlass, CanopyShading, DirectionalLightUniform, FrameQuality,
    FrameRenderer, GlassInstance, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, PostSettings,
    RenderDevice,
};

const W: u32 = 128;
const H: u32 = 80;
const FOV_Y: f32 = 1.1;
const EYE: [f32; 3] = [0.0, 1.2, 0.0];
const TARGET: [f32; 3] = [0.0, 0.9, -4.0];
const SILL: f32 = -0.3;
const SILL_MARGIN: f32 = 0.08;
/// `canopy_glass.rs`'s bound on the mean |cheap − full| over the canopy pixels.
const MEAN_REL_DIFF_BOUND: f64 = 0.04;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One device per test binary: a fresh `RenderDevice::headless()` per render exhausts the NVIDIA
/// Vulkan instance/adapter enumeration after a handful of devices (adapter list comes back empty).
fn device() -> &'static RenderDevice {
    static DEVICE: std::sync::OnceLock<RenderDevice> = std::sync::OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn boxed(center: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = regia_render::box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(Vec3::from_array(center)).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn canopy_mesh(radius: f32, material: usize) -> LabMesh {
    let (vertices, indices) = regia_render::uv_sphere(radius, 48, 24);
    let mut kept = Vec::new();
    for tri in indices.as_chunks::<3>().0 {
        let y: f32 = tri.iter().map(|&i| vertices[i as usize].position[1]).sum::<f32>() / 3.0;
        if y > SILL * radius {
            kept.extend_from_slice(tri);
        }
    }
    LabMesh {
        vertices,
        indices: kept,
        material,
        model: Mat4::from_translation(Vec3::from_array(EYE)).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn point(position: [f32; 3], color: [f32; 3], lumens: f32) -> LabLocalLight {
    LabLocalLight::Point(LabPointLight {
        position,
        color,
        intensity: lumens,
        radius: 8.0,
        cast_shadows: false,
        source_radius: None,
    })
}

/// One test frame.
#[derive(Clone)]
struct Rig {
    eye: [f32; 3],
    target: [f32; 3],
    far: f32,
    /// A closed opaque box around the cockpit ([`HANGAR_HALF`]): no sunlight reaches the canopy.
    hangar: bool,
    /// Sun and sky only: no point lights, no emissive panels.
    sun_only: bool,
    /// The canopy material's transmission.
    transmission: f32,
    two_sheets: bool,
    unlit: bool,
    refraction: bool,
    /// The HDR post stack (off: the forward pass writes display-encoded colour).
    post: bool,
    /// `(cascade count, resolution)` over `FrameQuality::LAB`'s.
    csm: Option<(u32, u32)>,
    /// The canopy instance's view mode.
    inside: bool,
    /// Cracks with holes on the canopy.
    damaged: bool,
    /// The scene's EV100 (the forward pass applies it when the post stack is off).
    exposure_ev: f32,
}

impl Default for Rig {
    fn default() -> Self {
        Self {
            eye: EYE,
            target: TARGET,
            far: 200.0,
            hangar: false,
            sun_only: false,
            transmission: 0.95,
            two_sheets: false,
            unlit: false,
            refraction: false,
            post: true,
            csm: None,
            inside: true,
            damaged: false,
            exposure_ev: 12.0,
        }
    }
}

fn post() -> PostSettings {
    PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    }
}

fn scene(rig: &Rig) -> LabScene {
    let ground = LabMaterial {
        base_color: [0.35, 0.33, 0.3, 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    };
    let hull = LabMaterial {
        base_color: [0.5, 0.5, 0.52, 1.0],
        roughness: 0.4,
        metallic: 0.6,
        ..LabMaterial::default()
    };
    let panel = LabMaterial {
        base_color: [0.05, 0.05, 0.05, 1.0],
        emissive: [0.4, 0.8, 1.0],
        emissive_intensity: 3000.0,
        roughness: 0.9,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [0.9, 0.93, 0.96, 1.0],
        roughness: 0.05,
        transmission: rig.transmission,
        ior: 1.5,
        alpha_mode: ALPHA_PREMULTIPLIED,
        double_sided: true,
        unlit: rig.unlit,
        ..LabMaterial::default()
    };
    let exterior = LabMaterial {
        transmission_thickness: 0.02,
        double_sided: false,
        unlit: false,
        ..glass.clone()
    };
    let mut meshes = vec![
        boxed([0.0, -0.05, 0.0], [30.0, 0.05, 30.0], 0),
        boxed([0.0, 0.6, -2.4], [0.5, 0.25, 1.0], 1),
        boxed([-2.2, 0.7, -1.5], [0.9, 0.08, 0.6], 1),
        boxed([2.3, 0.7, -1.6], [0.9, 0.08, 0.6], 1),
    ];
    if !rig.sun_only {
        meshes.extend([
            boxed([-1.5, 1.1, -1.4], [0.25, 0.12, 0.03], 2),
            boxed([1.4, 1.7, -1.6], [0.2, 0.1, 0.03], 2),
            boxed([1.3, 1.0, -3.6], [0.5, 0.3, 0.03], 4),
        ]);
    }
    if rig.hangar {
        let (h, t) = (HANGAR_HALF + 0.025, 0.025);
        let e = Vec3::from_array(EYE);
        for (offset, half) in [
            (Vec3::X * h, [t, h + t, h + t]),
            (Vec3::NEG_X * h, [t, h + t, h + t]),
            (Vec3::Y * h, [h + t, t, h + t]),
            (Vec3::NEG_Y * h, [h + t, t, h + t]),
            (Vec3::Z * h, [h + t, h + t, t]),
            (Vec3::NEG_Z * h, [h + t, h + t, t]),
        ] {
            meshes.push(boxed((e + offset).to_array(), half, 1));
        }
    }
    meshes.push(canopy_mesh(1.0, 3));
    if rig.two_sheets {
        meshes.push(canopy_mesh(0.97, 3));
    }
    LabScene {
        width: W,
        height: H,
        eye: rig.eye,
        target: rig.target,
        fov_y: FOV_Y,
        near: 0.05,
        far: rig.far,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: rig.exposure_ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: rig.post.then(post),
        directional: DirectionalLightUniform::new([-0.3, -0.6, -0.75], [1.0, 0.97, 0.92], 40_000.0),
        local_lights: if rig.sun_only {
            Vec::new()
        } else {
            vec![
                point([0.9, 2.1, -1.5], [1.0, 0.6, 0.3], 4000.0),
                point([-1.3, 1.5, -0.5], [0.3, 0.6, 1.0], 4000.0),
                point([0.0, 0.7, -1.6], [1.0, 1.0, 1.0], 3000.0),
            ]
        },
        materials: vec![ground, hull, panel, glass, exterior],
        meshes,
    }
}

fn quality(rig: &Rig) -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.emissive_lights = 16;
    q.screen_space_refraction = rig.refraction;
    if let Some((count, resolution)) = rig.csm {
        q.csm_cascade_count = count;
        q.csm_resolution = resolution;
    }
    q
}

/// Cracks at D3 with holes (energy over the laminate's hole threshold) in front of the eye.
fn damaged(key: u64, mesh: &LabMesh, inside: bool) -> GlassInstance {
    let sets = GlassCrackSets::default();
    let mut damage = GlassDamage::new("laminate");
    for (point, size) in [
        (Vec3::new(0.0, -0.08, -0.99), 0.5),
        (Vec3::new(-0.35, 0.05, -0.93), 0.4),
        (Vec3::new(0.4, -0.12, -0.9), 0.4),
    ] {
        damage.impact_at_level(point.normalize(), 8.0, 3, size);
    }
    let model = Mat4::from_cols_array_2d(&mesh.model);
    let cooked = sets.get("laminate").expect("built-in set");
    apply_one(&mut damage, cooked, &mesh.vertices, &mesh.indices, 1, model);
    let view = if inside {
        GlassView::viewer_inside()
    } else {
        GlassView::default()
    };
    glass_instance(key, Some(&view), Some(&damage), &sets, model).expect("damaged instance")
}

struct Shot {
    /// HDR capture (cd/m²) when the post stack ran.
    hdr: Option<Vec<[f32; 3]>>,
    /// The output image (RGBA8).
    ldr: Vec<u8>,
    draws: CanopyDraws,
}

fn render(rig: &Rig, settings: CanopyGlass) -> Shot {
    let post = post();
    let lab = scene(rig);
    let mut frame = lab.as_frame_scene_with_quality(quality(rig));
    if rig.post {
        frame.post = Some(&post);
    }
    let sheets = if rig.two_sheets { 2 } else { 1 };
    let first = lab.meshes.len() - sheets;
    let glass = (first..lab.meshes.len())
        .map(|i| {
            let key = frame.meshes[i].object_key;
            if rig.damaged {
                damaged(key, &lab.meshes[i], rig.inside)
            } else {
                GlassInstance {
                    object_key: key,
                    inside: rig.inside,
                    specular_scale: if rig.inside { 0.15 } else { 1.0 },
                    damage: None,
                }
            }
        })
        .collect();
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.set_glass(glass);
    renderer.set_canopy_glass(Some(settings));
    if rig.post {
        renderer.request_hdr_capture();
    }
    let out = renderer.render_offscreen(device, &frame).expect("frame");
    let hdr = if rig.post {
        let hdr = renderer
            .take_hdr_capture(device)
            .expect("hdr readback")
            .expect("the frame ran the post stack");
        assert_eq!((hdr.width, hdr.height), (W, H));
        Some(hdr.rgb)
    } else {
        None
    };
    Shot {
        hdr,
        ldr: out.pixels,
        draws: renderer.canopy_draws(),
    }
}

const FULL: CanopyGlass = CanopyGlass {
    shading: CanopyShading::Full,
    nearest_layer_only: false,
};
const CHEAP: CanopyGlass = CanopyGlass {
    shading: CanopyShading::Cheap,
    nearest_layer_only: false,
};

fn luminance(c: [f32; 3]) -> f64 {
    f64::from(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
}

/// Pixels whose view ray from [`EYE`] to [`TARGET`] crosses the canopy well above the sill.
fn canopy_pixels() -> Vec<usize> {
    let forward = (Vec3::from_array(TARGET) - Vec3::from_array(EYE)).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let tan = (FOV_Y * 0.5).tan();
    let aspect = W as f32 / H as f32;
    let mut out = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let ndc_x = (x as f32 + 0.5) / W as f32 * 2.0 - 1.0;
            let ndc_y = 1.0 - (y as f32 + 0.5) / H as f32 * 2.0;
            let dir = (forward + right * ndc_x * tan * aspect + up * ndc_y * tan).normalize();
            if dir.y > SILL + SILL_MARGIN {
                out.push((y * W + x) as usize);
            }
        }
    }
    out
}

/// Mean |a − b| luminance over `pixels` relative to `a`'s mean there, and `a`'s mean.
fn rel_diff(a: &[[f32; 3]], b: &[[f32; 3]], pixels: &[usize]) -> (f64, f64) {
    let (mut sa, mut sd) = (0.0, 0.0);
    for &i in pixels {
        for c in a[i].iter().chain(b[i].iter()) {
            assert!(c.is_finite(), "pixel {i}: non-finite HDR value {c}");
        }
        sa += luminance(a[i]);
        sd += (luminance(b[i]) - luminance(a[i])).abs();
    }
    let n = pixels.len() as f64;
    (sd / n / (sa / n).max(1e-6), sa / n)
}

fn hdr(shot: &Shot) -> &[[f32; 3]] {
    shot.hdr.as_deref().expect("HDR capture")
}

#[test]
fn a_camera_outside_the_glass_draws_it_with_the_full_shader() {
    let _serial = serial();
    // A chase camera 7 m behind and above the cockpit, the instance still flagged inside.
    let chase = Rig {
        eye: [1.5, 3.5, 6.5],
        target: [0.0, 1.2, 0.0],
        ..Rig::default()
    };
    let cheap = render(&chase, CHEAP);
    let full = render(&chase, FULL);
    eprintln!(
        "chase camera: cheap setting {:?}, full setting {:?}",
        cheap.draws, full.draws
    );
    assert!(
        cheap.ldr == full.ldr && hdr(&cheap) == hdr(&full),
        "an outside camera sees the canopy exactly as the full shader draws it"
    );
    assert_eq!(
        cheap.draws,
        CanopyDraws {
            eye_outside: 1,
            ..CanopyDraws::default()
        },
        "the cheap setting falls back to the full shader for an outside camera"
    );
    assert_eq!(full.draws.full, 1);
    // From the cockpit the cheap path still runs.
    let inside = render(&Rig::default(), CHEAP);
    assert_eq!(
        inside.draws,
        CanopyDraws {
            cheap: 1,
            ..CanopyDraws::default()
        }
    );
}

/// Inner half-size (metres) of [`Rig::hangar`]: its far corners are 1.9 m from the eye.
const HANGAR_HALF: f32 = 1.1;

#[test]
fn the_cheap_sun_visibility_falls_back_to_cascade_1() {
    let _serial = serial();
    // A closed hangar around the cockpit: no sunlight reaches the canopy. With a 2 m view range
    // the two lab cascades split at 0.49 m, so cascade 0's map ends short of most of the glass a
    // metre out; with 200 m it holds all of it. The cheap canopy must be as dark either way: off
    // cascade 0 it takes cascade 1, not full sun (`canopy_sun_visibility`). Sun and sky only, and
    // glass that keeps 70 % of the light it does not transmit, so its own sunlight would show.
    let pixels = canopy_pixels();
    let rig = |far: f32| Rig {
        far,
        hangar: true,
        sun_only: true,
        transmission: 0.3,
        ..Rig::default()
    };
    let short = render(&rig(2.0), CHEAP);
    let long = render(&rig(200.0), CHEAP);
    let full_short = render(&rig(2.0), FULL);
    let full_long = render(&rig(200.0), FULL);
    let (vs_full, mean) = rel_diff(hdr(&full_short), hdr(&short), &pixels);
    let (range_cheap, _) = rel_diff(hdr(&long), hdr(&short), &pixels);
    let (range_full, _) = rel_diff(hdr(&full_long), hdr(&full_short), &pixels);
    eprintln!(
        "hangar, 2 m range: canopy mean full {mean:.1} cd/m², cheap off it by {:.3} %; 2 m vs 200 m: cheap {:.3} %, \
         full {:.3} %",
        100.0 * vs_full,
        100.0 * range_cheap,
        100.0 * range_full
    );
    // Measured on lavapipe: cheap vs full 1.7 % (31.7 % when the cheap path took full sun off
    // cascade 0); the range changes the full shader's canopy by 4.5 % (other cascades, other
    // texels) and the cheap one's by 2.8 % (33.8 % before).
    assert!(
        vs_full < MEAN_REL_DIFF_BOUND,
        "the cheap canopy past cascade 0 is lit inside a closed hangar: {:.2} % off the full shader",
        100.0 * vs_full
    );
    assert!(
        range_cheap < range_full + 0.01,
        "a shorter first cascade changes the cheap canopy by {:.2} %, the full one by {:.2} %",
        100.0 * range_cheap,
        100.0 * range_full
    );
}

#[test]
fn exterior_glass_draws_the_same_on_either_setting() {
    let _serial = serial();
    for (refraction, damaged) in [(false, false), (true, false), (false, true)] {
        let rig = Rig {
            inside: false,
            refraction,
            damaged,
            ..Rig::default()
        };
        let cheap = render(&rig, CHEAP);
        let full = render(&rig, FULL);
        assert_eq!(
            cheap.draws,
            CanopyDraws::default(),
            "exterior glass is not counted"
        );
        assert!(
            cheap.ldr == full.ldr && hdr(&cheap) == hdr(&full),
            "refraction {refraction}, damaged {damaged}: exterior glass takes the full shader whatever the setting"
        );
    }
}

#[test]
fn unlit_canopy_glass_is_the_same_on_both_paths() {
    let _serial = serial();
    for refraction in [false, true] {
        let rig = Rig {
            unlit: true,
            refraction,
            ..Rig::default()
        };
        let cheap = render(&rig, CHEAP);
        let full = render(&rig, FULL);
        assert_eq!(cheap.draws.cheap, 1);
        assert!(
            cheap.ldr == full.ldr && hdr(&cheap) == hdr(&full),
            "refraction {refraction}: unlit glass returns its base colour on both paths"
        );
    }
}

#[test]
fn nearest_layer_only_without_refraction_changes_nothing() {
    let _serial = serial();
    // No refraction: no glass front layer, so the option has nothing to act on.
    let rig = Rig {
        two_sheets: true,
        ..Rig::default()
    };
    let both = render(&rig, CHEAP);
    let nearest = render(
        &rig,
        CanopyGlass {
            nearest_layer_only: true,
            ..CHEAP
        },
    );
    assert_eq!(both.draws.cheap, 2);
    assert!(both.ldr == nearest.ldr && hdr(&both) == hdr(&nearest));
}

#[test]
fn a_cracked_canopy_with_holes_on_the_cheap_path() {
    let _serial = serial();
    let pixels = canopy_pixels();
    for refraction in [false, true] {
        let rig = Rig {
            damaged: true,
            refraction,
            ..Rig::default()
        };
        let full = render(&rig, FULL);
        let cheap = render(&rig, CHEAP);
        let clear = render(
            &Rig {
                damaged: false,
                ..rig.clone()
            },
            CHEAP,
        );
        let clear_full = render(
            &Rig {
                damaged: false,
                ..rig.clone()
            },
            FULL,
        );
        assert_eq!(cheap.draws.cheap, 1);
        let (rel, mean) = rel_diff(hdr(&full), hdr(&cheap), &pixels);
        let (cracks_cheap, _) = rel_diff(hdr(&clear), hdr(&cheap), &pixels);
        let (cracks_full, _) = rel_diff(hdr(&clear_full), hdr(&full), &pixels);
        eprintln!(
            "refraction {refraction}: cracked canopy mean full {mean:.1} cd/m², mean |cheap - full| {:.3} %; the \
             cracks change the canopy by {:.2} % cheap, {:.2} % full",
            100.0 * rel,
            100.0 * cracks_cheap,
            100.0 * cracks_full
        );
        assert!(cracks_full > 0.002, "the cracks show on the full shader");
        assert!(
            cracks_cheap > 0.5 * cracks_full && cracks_cheap < 2.0 * cracks_full,
            "refraction {refraction}: the cracks change the cheap canopy as much as the full one"
        );
        assert!(
            rel < MEAN_REL_DIFF_BOUND,
            "refraction {refraction}: cracked cheap canopy off the full shader by {:.2} %",
            100.0 * rel
        );
    }
}

#[test]
fn the_cheap_canopy_without_the_hdr_post_stack() {
    let _serial = serial();
    // The forward pass tone-maps and encodes itself (`post_exposure.x` 0): compare the RGBA8
    // output over the canopy pixels.
    let pixels = canopy_pixels();
    // EV 15: the canopy sits mid-range in the display encoding (EV 12 clips it to white).
    let rig = Rig {
        post: false,
        exposure_ev: 15.0,
        ..Rig::default()
    };
    let full = render(&rig, FULL);
    let cheap = render(&rig, CHEAP);
    assert!(full.hdr.is_none());
    assert_eq!(cheap.draws.cheap, 1);
    let rgb = |img: &[u8], i: usize| {
        [
            f32::from(img[i * 4]),
            f32::from(img[i * 4 + 1]),
            f32::from(img[i * 4 + 2]),
        ]
    };
    let a: Vec<[f32; 3]> = (0..(W * H) as usize).map(|i| rgb(&full.ldr, i)).collect();
    let b: Vec<[f32; 3]> = (0..(W * H) as usize).map(|i| rgb(&cheap.ldr, i)).collect();
    let (rel, mean) = rel_diff(&a, &b, &pixels);
    let max = pixels
        .iter()
        .map(|&i| (luminance(a[i]) - luminance(b[i])).abs())
        .fold(0.0f64, f64::max);
    eprintln!(
        "display-encoded: canopy mean full {mean:.2} / 255, mean |cheap - full| {:.3} %, max {max:.1} / 255",
        100.0 * rel
    );
    assert!(
        mean > 20.0 && mean < 200.0,
        "the canopy pixels are neither black nor clipped ({mean:.2} / 255)"
    );
    assert!(
        pixels.iter().all(|&i| luminance(b[i]) > 0.0),
        "a cheap canopy pixel went black"
    );
    assert!(
        rel < MEAN_REL_DIFF_BOUND,
        "display-encoded cheap canopy off the full shader by {:.2} %",
        100.0 * rel
    );
}
