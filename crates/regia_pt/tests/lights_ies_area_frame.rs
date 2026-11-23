// SPDX-License-Identifier: MPL-2.0
//! IES profiles and authored rect lights through a whole forward frame (ADR-2592; needs the frame hookup,
//! `FrameRenderer::set_light_extras`). On lavapipe in CI.
//!
//! - An IES spot: the profiled frame over the same frame without the profile is, at every lit floor pixel, the
//!   CPU profile lookup toward that pixel (`light_profiles::relative_gpu`).
//! - A light without a profile renders bit for bit as with no extras at all.
//! - A rect light: the forward frame's floor matches the path tracer's direct light from the same rect.
//! - An LTC sphere (`LightSourceLtc`): a sized point and a sized spot match the traced sphere, also with the sphere's
//!   centre below the floor, where the representative point is black; a marked light without a radius and an unmarked
//!   sized light are bit-identical to no extras.

use std::sync::Arc;

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame, push_area_lights};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::light_profiles::{AreaLightInput, LightExtras, profile_use, rect_emitter, relative_gpu};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabLocalLight, LabMaterial,
    LabPointLight, LabSpotLight, LabVertex, PostSettings, RectLight, RenderDevice, ShadowCast,
};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 64;
const EYE: [f32; 3] = [0.0, 5.0, 5.0];
const FOV_Y: f32 = 1.0;
const LIGHT: [f32; 3] = [0.0, 2.5, 0.0];

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("skip: {e}");
            None
        }
    }
}

fn floor() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    let v = [
        [-30.0, 0.0, -30.0],
        [30.0, 0.0, -30.0],
        [30.0, 0.0, 30.0],
        [-30.0, 0.0, 30.0],
    ]
    .into_iter()
    .map(|p| LabVertex::from_pos_normal_uv(p, n, [0.0, 0.0]))
    .collect();
    (v, vec![0, 2, 1, 0, 3, 2])
}

/// The floor point the centre of pixel (x, y) sees.
fn floor_hit(x: u32, y: u32) -> Option<Vec3> {
    let eye = Vec3::from_array(EYE);
    let fwd = (Vec3::ZERO - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let th = (FOV_Y * 0.5).tan();
    let aspect = WIDTH as f32 / HEIGHT as f32;
    let sx = ((x as f32 + 0.5) / WIDTH as f32 * 2.0 - 1.0) * th * aspect;
    let sy = (1.0 - (y as f32 + 0.5) / HEIGHT as f32 * 2.0) * th;
    let d = (fwd + right * sx + up * sy).normalize();
    (d.y < -1e-3).then(|| eye + d * (-eye.y / d.y))
}

fn scene<'a>(
    lights: &'a [LabLocalLight],
    post: &'a PostSettings,
    mesh: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
) -> FrameScene<'a> {
    FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 10.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: lights,
        quality: FrameQuality {
            specular_aa: false,
            ..FrameQuality::LAB
        },
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 2,
            object_key: 3,
            vertices: &mesh.0,
            indices: &mesh.1,
            material,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    }
}

fn hdr(device: &RenderDevice, scene: &FrameScene<'_>, extras: Option<LightExtras>) -> Vec<[f32; 3]> {
    let mut renderer = FrameRenderer::new();
    if let Some(extras) = extras {
        renderer.set_light_extras(extras);
    }
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, scene).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("post ran")
        .rgb
}

fn spot() -> LabLocalLight {
    LabLocalLight::Spot(LabSpotLight {
        position: LIGHT,
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        intensity: 400.0,
        range: 60.0,
        inner_angle: 1.3,
        outer_angle: 1.4,
        cast_shadows: false,
        source_radius: None,
    })
}

#[test]
fn an_ies_spot_matches_the_cpu_profile_at_sample_points() {
    let Some(device) = device() else { return };
    let file = regia_asset::ies::IesFile::parse(&regia_asset::ies::example_text()).expect("parse");
    let profile = Arc::new(regia_asset::ies::CookedIesProfile::from_file(&file).expect("cook"));
    let (mesh, material, post) = (
        floor(),
        LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.8,
            ..LabMaterial::EMPTY
        },
        post(),
    );
    let lights = [spot()];
    let frame = scene(&lights, &post, &mesh, &material);
    let plain = hdr(&device, &frame, None);
    let mut extras = LightExtras::default();
    let uses = profile_use(Vec3::NEG_Y, Vec3::X, 0.6, None);
    assert!(extras.assign(0, &profile, uses));
    let shaped = hdr(&device, &frame, Some(extras.clone()));
    let code = u32::from(extras.per_light[0].expect("assigned").slot) | (u32::from(uses.turn) << 3);
    let (mut n, mut worst, mut dark) = (0usize, 0.0f64, 0usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(p) = floor_hit(x, y) else { continue };
            let i = (y * WIDTH + x) as usize;
            let base = f64::from(plain[i][1]);
            if base < 1e-3 {
                continue;
            }
            let d = (p - Vec3::from_array(LIGHT)).normalize();
            let want = f64::from(relative_gpu(&profile, code, Vec3::NEG_Y, d));
            let got = f64::from(shaped[i][1]) / base;
            if want < 1e-3 {
                dark += 1;
            }
            worst = worst.max((got - want).abs());
            n += 1;
        }
    }
    eprintln!(
        "ies spot: {n} lit floor pixels ({dark} outside the profile), worst |ratio − profile| {worst:.5}"
    );
    assert!(n > 1000, "{n}");
    assert!(dark > 100, "the 60° cutoff darkens the far floor: {dark}");
    assert!(worst < 0.01, "worst {worst}");
}

/// `hdr` on a forced lite forward tier (the Mobile path, ADR-1874).
fn hdr_lite(
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    extras: Option<LightExtras>,
    tier: regia_render::forward_lite::ForwardLite,
) -> Vec<[f32; 3]> {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(tier));
    if let Some(extras) = extras {
        renderer.set_light_extras(extras);
    }
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, scene).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("post ran")
        .rgb
}

/// ADR-3247: the Mobile (lite) forward tiers read the IES profile too. On both, the profiled frame over the same
/// frame without the profile is the CPU lookup toward each lit floor pixel, as on the full shader. Before, the lite
/// loop skipped the profile and the ratio was 1 everywhere (the 60° cutoff stayed lit).
#[test]
fn an_ies_spot_on_the_mobile_tiers_matches_the_cpu_profile() {
    use regia_render::forward_lite::ForwardLite;
    let Some(device) = device() else { return };
    let file = regia_asset::ies::IesFile::parse(&regia_asset::ies::example_text()).expect("parse");
    let profile = Arc::new(regia_asset::ies::CookedIesProfile::from_file(&file).expect("cook"));
    let (mesh, material, post) = (
        floor(),
        LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.8,
            ..LabMaterial::EMPTY
        },
        post(),
    );
    let lights = [spot()];
    let frame = scene(&lights, &post, &mesh, &material);
    let uses = profile_use(Vec3::NEG_Y, Vec3::X, 0.6, None);
    for tier in [ForwardLite::Low, ForwardLite::High] {
        let plain = hdr_lite(&device, &frame, None, tier);
        let mut extras = LightExtras::default();
        assert!(extras.assign(0, &profile, uses));
        let shaped = hdr_lite(&device, &frame, Some(extras.clone()), tier);
        let code = u32::from(extras.per_light[0].expect("assigned").slot) | (u32::from(uses.turn) << 3);
        let (mut n, mut worst, mut dark) = (0usize, 0.0f64, 0usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let Some(p) = floor_hit(x, y) else { continue };
                let i = (y * WIDTH + x) as usize;
                let base = f64::from(plain[i][1]);
                if base < 1e-3 {
                    continue;
                }
                let d = (p - Vec3::from_array(LIGHT)).normalize();
                let want = f64::from(relative_gpu(&profile, code, Vec3::NEG_Y, d));
                let got = f64::from(shaped[i][1]) / base;
                if want < 1e-3 {
                    dark += 1;
                }
                worst = worst.max((got - want).abs());
                n += 1;
            }
        }
        eprintln!(
            "ies spot, lite {tier:?}: {n} lit floor pixels ({dark} outside the profile), worst |ratio − profile| {worst:.5}"
        );
        assert!(n > 1000, "{tier:?}: {n}");
        assert!(
            dark > 100,
            "{tier:?}: the 60° cutoff darkens the far floor: {dark}"
        );
        assert!(worst < 0.01, "{tier:?}: worst {worst}");
    }
}

#[test]
fn a_light_without_a_profile_is_bit_identical_with_extras_set() {
    let Some(device) = device() else { return };
    let (mesh, material, post) = (floor(), LabMaterial::EMPTY, post());
    let lights = [spot(), spot()];
    let frame = scene(&lights, &post, &mesh, &material);
    let plain = hdr(&device, &frame, None);
    let file = regia_asset::ies::IesFile::parse(&regia_asset::ies::example_text()).expect("parse");
    let profile = Arc::new(regia_asset::ies::CookedIesProfile::from_file(&file).expect("cook"));
    // A profile on a light index the frame does not have: the bands are written, no record changes.
    let mut extras = LightExtras::default();
    assert!(extras.assign(5, &profile, profile_use(Vec3::NEG_Y, Vec3::X, 0.0, None)));
    let with = hdr(&device, &frame, Some(extras));
    let bits = |v: &[[f32; 3]]| v.iter().flat_map(|c| c.map(f32::to_bits)).collect::<Vec<_>>();
    assert_eq!(bits(&plain), bits(&with));
}

#[test]
fn a_rect_light_frame_matches_the_path_tracer() {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let rect = RectLight {
        width: 1.2,
        height: 0.6,
        intensity: 6000.0,
        cast_shadows: false,
        range: 40.0,
        ..RectLight::default()
    };
    let model = Mat4::from_rotation_translation(
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        Vec3::new(0.0, 1.6, 0.0),
    );
    let area = AreaLightInput {
        light: rect_emitter(&rect, &model),
        range: rect.range,
        cast_shadows: false,
    };
    let mut extras = LightExtras::default();
    extras.area_lights.push(area);
    let mut frame = scene(&[], &post, &mesh, &material);
    // `LAB` has no emitter budget, like every tier under `DesktopHigh`: the rect would emit nothing.
    frame.quality.emissive_lights = 16;
    let forward = hdr(&device, &frame, Some(extras));
    let mut pt = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    push_area_lights(&mut pt, &[area]);
    let tracer = PathTracer::new(&device).expect("compile");
    let img = tracer
        .render(
            &pt,
            &PtSettings {
                spp: 512,
                max_bounces: 1,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace");
    let (mut n, mut err, mut bias) = (0usize, 0.0f64, 0.0f64);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            if floor_hit(x, y).is_none() {
                continue;
            }
            let i = (y * WIDTH + x) as usize;
            let reference = f64::from(img.direct[i][1]);
            if reference < 0.05 {
                continue;
            }
            let rel = f64::from(forward[i][1]) / reference - 1.0;
            err += rel.abs();
            bias += rel;
            n += 1;
        }
    }
    let (mre, bias) = (err / n.max(1) as f64, bias / n.max(1) as f64);
    eprintln!("rect light frame: {n} px, forward vs PT mean relative error {mre:.4}, bias {bias:+.4}");
    assert!(n > 500, "{n}");
    assert!(mre < 0.06, "mean relative error {mre}");
    assert!(bias.abs() < 0.04, "bias {bias}");
}

const SPHERE: [f32; 3] = [0.0, 0.4, 0.0];
const SPHERE_RADIUS: f32 = 0.5;

fn sized_lights(radius: Option<f32>) -> [LabLocalLight; 2] {
    [
        LabLocalLight::Point(LabPointLight {
            position: SPHERE,
            color: [1.0; 3],
            intensity: 4000.0,
            radius: 1000.0,
            cast_shadows: false,
            source_radius: radius,
        }),
        LabLocalLight::Spot(LabSpotLight {
            position: [0.0, 1.0, 0.0],
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            intensity: 400.0,
            range: 1000.0,
            inner_angle: 0.9,
            outer_angle: 1.2,
            cast_shadows: false,
            source_radius: radius,
        }),
    ]
}

/// The path tracer's direct light from the scene's sized lights.
fn traced_direct(device: &RenderDevice, frame: &FrameScene<'_>) -> Vec<[f32; 3]> {
    let pt = from_frame(
        frame,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    PathTracer::new(device)
        .expect("compile")
        .render(
            &pt,
            &PtSettings {
                spp: 512,
                max_bounces: 1,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace")
        .direct
}

/// `(forward, reference)` green values of the floor pixels `floor_error` measures: the ray misses the sphere (the
/// tracer draws it, the forward frame does not), `keep` holds, and the reference is above 1 % of its lit mean.
fn floor_pairs(
    forward: &[[f32; 3]],
    reference: &[[f32; 3]],
    center: Vec3,
    radius: f32,
    falloff: impl Fn(f32) -> f64,
    keep: impl Fn(Vec3) -> bool,
) -> Vec<(f64, f64)> {
    let eye = Vec3::from_array(EYE);
    let lit: Vec<f64> = reference
        .iter()
        .map(|c| f64::from(c[1]))
        .filter(|v| *v > 0.0)
        .collect();
    let dim = 0.01 * lit.iter().sum::<f64>() / lit.len().max(1) as f64;
    let mut pairs = Vec::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(p) = floor_hit(x, y) else { continue };
            let ray = (p - eye).normalize();
            let along = (center - eye).dot(ray);
            if (eye + ray * along - center).length() < radius + 0.1 || !keep(p) {
                continue;
            }
            let i = (y * WIDTH + x) as usize;
            let want = f64::from(reference[i][1]);
            if want < dim {
                continue;
            }
            pairs.push((f64::from(forward[i][1]) * falloff((p - center).length()), want));
        }
    }
    pairs
}

/// `(pixels, mean relative error, mean signed error)` of `forward` against `reference` on the floor (`floor_pairs`).
fn floor_error(
    forward: &[[f32; 3]],
    reference: &[[f32; 3]],
    center: Vec3,
    radius: f32,
    falloff: impl Fn(f32) -> f64,
    keep: impl Fn(Vec3) -> bool,
) -> (usize, f64, f64) {
    let pairs = floor_pairs(forward, reference, center, radius, falloff, keep);
    let n = pairs.len().max(1) as f64;
    let err = pairs.iter().map(|(f, w)| (f / w - 1.0).abs()).sum::<f64>();
    let bias = pairs.iter().map(|(f, w)| f / w - 1.0).sum::<f64>();
    (pairs.len(), err / n, bias / n)
}

/// `(energy error, highlight error)` of `forward` on the floor (`floor_pairs`): `Σ|f − r| / Σr` over the pixels, and
/// the mean relative error of the pixels at or above a tenth of the brightest reference.
fn floor_energy_and_highlight(
    forward: &[[f32; 3]],
    reference: &[[f32; 3]],
    center: Vec3,
    radius: f32,
    falloff: impl Fn(f32) -> f64,
) -> (f64, f64) {
    let pairs = floor_pairs(forward, reference, center, radius, falloff, |_| true);
    let want: f64 = pairs.iter().map(|(_, w)| w).sum();
    let energy = pairs.iter().map(|(f, w)| (f - w).abs()).sum::<f64>() / want.max(1e-30);
    let peak = pairs.iter().map(|(_, w)| *w).fold(0.0f64, f64::max);
    let bright: Vec<_> = pairs.iter().filter(|(_, w)| *w >= 0.1 * peak).collect();
    let highlight = bright.iter().map(|(f, w)| (f / w - 1.0).abs()).sum::<f64>() / bright.len().max(1) as f64;
    (energy, highlight)
}

#[test]
fn an_ltc_sphere_frame_matches_the_traced_sphere() {
    // The study (`ltc::sphere_study`) has the LTC form ahead on glossy lobes and the representative point ahead on
    // rough ones; the glossy floor is `an_ltc_sphere_on_glossy_metal_matches_the_traced_sphere`.
    ltc_sphere_rows(&[
        ("rough dielectric", 0.9, 0.0, 0.05, false),
        ("mid dielectric", 0.5, 0.0, 0.07, false),
    ]);
}

/// The glossy metal row (perceptual 0.3), un-ignored by ADR-3247. Its 12.2 % energy / 11.4 % highlight miss (ADR-2592)
/// was the reference's noise: the tracer samples the sphere uniformly over its cone, and at 512 spp two seeds of the
/// trace differ by 14 % in energy and 16 % in the highlight (`glossy_ltc_sphere_error_split`), so one trace sits about
/// 11 % from the converged value there whatever the forward does. The row is held to the converged direct light, a
/// brute-force CPU integral of the same GGX over the sphere (`cpu_metal_floor_under_the_sphere`), and the 512-spp
/// trace must agree with that integral in total (signed) energy, where its noise averages out.
#[test]
fn an_ltc_sphere_on_glossy_metal_matches_the_traced_sphere() {
    // The per-pixel mean is the wrong measure here: the LTC fit under-reads GGX's far tail (values under a tenth of
    // the peak), on the CPU too (per point 66 %, bias -62 %). The row is held to its energy and its highlight.
    ltc_sphere_rows(&[("glossy metal", 0.3, 1.0, 0.08, true)]);
}

/// The LTC sphere against the traced sphere on a floor of each `(label, roughness, metallic, bound, glossy)`: a
/// glossy row (a metal) is held to its energy and highlight error against the converged integral the trace estimates
/// (the trace must match it in signed energy) and must beat the representative point's energy, the others to the
/// per-pixel mean relative error against the trace.
fn ltc_sphere_rows(rows: &[(&str, f32, f32, f64, bool)]) {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let light = sized_lights(Some(SPHERE_RADIUS));
    let point = std::slice::from_ref(&light[0]);
    let center = Vec3::from_array(SPHERE);
    for &(label, roughness, metallic, bound, glossy) in rows {
        let material = LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness,
            metallic,
            ..LabMaterial::EMPTY
        };
        let frame = scene(point, &post, &mesh, &material);
        let traced = traced_direct(&device, &frame);
        let reference = if glossy {
            assert_eq!(metallic, 1.0, "{label}: the CPU integral is the metal's specular");
            let exact = cpu_metal_floor_under_the_sphere(roughness, 0.6, 4000.0).exact;
            let pairs = floor_pairs(&traced, &exact, center, SPHERE_RADIUS, |_| 1.0, |_| true);
            let want: f64 = pairs.iter().map(|(_, w)| w).sum();
            let signed = pairs.iter().map(|(t, w)| t - w).sum::<f64>() / want.max(1e-30);
            let (noise_energy, noise_highlight) =
                floor_energy_and_highlight(&traced, &exact, center, SPHERE_RADIUS, |_| 1.0);
            eprintln!(
                "{label}: 512-spp trace vs the converged integral: signed energy {signed:+.4}, energy {noise_energy:.4}, \
                 highlight {noise_highlight:.4}"
            );
            assert!(
                signed.abs() < 0.03,
                "{label}: the trace does not converge to the CPU integral ({signed:+.4})"
            );
            exact
        } else {
            traced
        };
        let mut extras = LightExtras::default();
        extras.mark_ltc_sphere(0);
        let ltc = hdr(&device, &frame, Some(extras));
        let representative = hdr(&device, &frame, None);
        // The unmarked form has the punctual knee: take it out, as `source_size_parity` does, so only the shading
        // differs.
        let knee = |d: f32| f64::from((d * d + 1.0) / (d * d));
        let (n, mre, bias) = floor_error(&ltc, &reference, center, SPHERE_RADIUS, |_| 1.0, |_| true);
        let (_, rep, rep_bias) =
            floor_error(&representative, &reference, center, SPHERE_RADIUS, knee, |_| true);
        let (energy, highlight) =
            floor_energy_and_highlight(&ltc, &reference, center, SPHERE_RADIUS, |_| 1.0);
        let (rep_energy, rep_highlight) =
            floor_energy_and_highlight(&representative, &reference, center, SPHERE_RADIUS, knee);
        eprintln!(
            "{label}: {n} px, LTC {mre:.4} (bias {bias:+.4}, energy {energy:.4}, highlight {highlight:.4}), \
             representative {rep:.4} (bias {rep_bias:+.4}, energy {rep_energy:.4}, highlight {rep_highlight:.4})"
        );
        assert!(n > 500, "{n}");
        if glossy {
            assert!(energy < bound, "{label}: energy error {energy}");
            assert!(highlight < bound, "{label}: highlight error {highlight}");
            assert!(
                energy < rep_energy,
                "{label}: the LTC form's energy error {energy} is no smaller than the representative point's {rep_energy}"
            );
        } else {
            assert!(mre < bound, "{label}: mean relative error {mre}");
        }
    }
}

/// Diagnosis of the glossy-metal row (ADR-3247), ignored: splits the frame's error against the tracer into the
/// tracer's own noise (the trace against a brute-force CPU integral with per-sample Schlick), the method (the CPU
/// mirror of the LTC sphere with the forward's DFG magnitude against that integral) and the shader (the frame against
/// its CPU mirror), at every floor pixel the row measures. `REGIA_GLOSSY_SPP` sets the trace's samples (512).
#[test]
#[ignore = "diagnosis: prints the glossy LTC sphere's error split (ADR-3247)"]
fn glossy_ltc_sphere_error_split() {
    let Some(device) = device() else { return };
    let spp: u32 = std::env::var("REGIA_GLOSSY_SPP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    let (mesh, post) = (floor(), post());
    let light = sized_lights(Some(SPHERE_RADIUS));
    let point = std::slice::from_ref(&light[0]);
    let center = Vec3::from_array(SPHERE);
    let (perceptual, f0) = (0.3f32, 0.6f32);
    let material = LabMaterial {
        base_color: [f0, f0, f0, 1.0],
        roughness: perceptual,
        metallic: 1.0,
        ..LabMaterial::EMPTY
    };
    let frame = scene(point, &post, &mesh, &material);
    let pt = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(&device).expect("compile");
    let trace = |seed: u32| {
        tracer
            .render(
                &pt,
                &PtSettings {
                    spp,
                    max_bounces: 1,
                    jitter: false,
                    seed,
                    ..PtSettings::default()
                },
            )
            .expect("trace")
            .direct
    };
    let reference = trace(1);
    let reference2 = if std::env::var_os("REGIA_GLOSSY_ONE_SEED").is_some() {
        reference.clone()
    } else {
        trace(2)
    };
    let mut extras = LightExtras::default();
    extras.mark_ltc_sphere(0);
    let ltc = hdr(&device, &frame, Some(extras));
    let cpu = cpu_metal_floor_under_the_sphere(perceptual, f0, 4000.0);
    let (cpu_ref, cpu_ref_nof, cpu_ltc, cpu_ltc_e) =
        (&cpu.exact, &cpu.exact_no_fresnel, &cpu.ltc_ab, &cpu.ltc_e);
    let r = SPHERE_RADIUS;
    let one = |_: f32| 1.0;
    let row = |label: &str, f: &[[f32; 3]], want: &[[f32; 3]]| {
        let (energy, highlight) = floor_energy_and_highlight(f, want, center, r, one);
        let (n, mre, bias) = floor_error(f, want, center, r, one, |_| true);
        eprintln!(
            "{label}: energy {energy:.4}, highlight {highlight:.4}, per pixel {mre:.4} (bias {bias:+.4}, {n} px)"
        );
    };
    eprintln!("glossy LTC sphere error split, trace {spp} spp:");
    row("frame LTC vs trace (the test's row)", &ltc, &reference);
    row("trace seed 2 vs trace seed 1 (noise)", &reference2, &reference);
    row("trace vs CPU integral (noise + tracer)", &reference, cpu_ref);
    row("frame LTC vs CPU integral", &ltc, cpu_ref);
    row("CPU LTC (F0 A + B) vs CPU integral (method)", cpu_ltc, cpu_ref);
    row("CPU LTC (F0 E) vs CPU integral", cpu_ltc_e, cpu_ref);
    row("frame LTC vs CPU LTC (shader vs mirror)", &ltc, cpu_ltc);
    row("CPU integral without Fresnel vs with", cpu_ref_nof, cpu_ref);
    let signed = |f: &[[f32; 3]], want: &[[f32; 3]]| {
        let pairs = floor_pairs(f, want, center, r, one, |_| true);
        let sum: f64 = pairs.iter().map(|(_, w)| w).sum();
        pairs.iter().map(|(f, w)| f - w).sum::<f64>() / sum.max(1e-30)
    };
    eprintln!(
        "signed energy: trace vs CPU integral {:+.4}, frame vs CPU integral {:+.4}",
        signed(&reference, cpu_ref),
        signed(&ltc, cpu_ref)
    );
}

/// The glossy row's floor on the CPU, per pixel of the frame (ADR-3247): the sphere light of `lumens` at `SPHERE`
/// reflected by a metal floor of `f0` and perceptual roughness `perceptual`.
struct CpuSphereFloor {
    /// The brute-force integral of the engine's GGX (height-correlated Smith) with per-sample Schlick Fresnel over
    /// the sphere's solid angle: the direct light the tracer converges to.
    exact: Vec<[f32; 3]>,
    /// The same with `F = 1`: the lobe alone.
    exact_no_fresnel: Vec<[f32; 3]>,
    /// The forward's LTC sphere on the CPU (`ltc::sphere_form_factor`) with the DFG magnitude `F0 · A + B`.
    ltc_ab: Vec<[f32; 3]>,
    /// The same with the single-scatter albedo `F0 · E(μ)` (the DFG's B channel) as the magnitude.
    ltc_e: Vec<[f32; 3]>,
}

fn cpu_metal_floor_under_the_sphere(perceptual: f32, f0: f32, lumens: f64) -> CpuSphereFloor {
    let center = Vec3::from_array(SPHERE);
    // `intensity` is the point light's lumens: I = Φ / 4π cd, the sphere's radiance I / (π r²).
    let radiance = lumens
        / (4.0 * std::f64::consts::PI)
        / (std::f64::consts::PI * f64::from(SPHERE_RADIUS * SPHERE_RADIUS));
    let alpha = (perceptual * perceptual).max(0.002_025);
    let table = regia_render::ltc::ltc_table();
    let lut = regia_render::build_brdf_lut();
    let dfg = |no_v: f32| {
        // Bilinear, clamp to edge, texel centres at (i + 0.5) / 32, as the forward's sampler.
        let s = lut.width as f32;
        let (x, y) = (no_v * s - 0.5, perceptual * s - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let at = |cx: f32, cy: f32, k: usize| {
            let cx = cx.clamp(0.0, s - 1.0) as u32;
            let cy = cy.clamp(0.0, s - 1.0) as u32;
            f32::from(lut.rgba[((cy * lut.width + cx) * 4) as usize + k]) / 255.0
        };
        let ch = |k: usize| {
            let top = at(x0, y0, k) * (1.0 - fx) + at(x0 + 1.0, y0, k) * fx;
            let bot = at(x0, y0 + 1.0, k) * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0, k) * fx;
            top * (1.0 - fy) + bot * fy
        };
        (ch(0), ch(1), ch(2))
    };
    let eye = Vec3::from_array(EYE);
    let r = SPHERE_RADIUS;
    let mut cpu_ref = vec![[0.0f32; 3]; (WIDTH * HEIGHT) as usize];
    let mut cpu_ref_nof = cpu_ref.clone();
    let mut cpu_ltc = cpu_ref.clone();
    let mut cpu_ltc_e = cpu_ref.clone();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(p) = floor_hit(x, y) else { continue };
            let v_world = (eye - p).normalize();
            let t1 = Vec3::new(v_world.x, 0.0, v_world.z).normalize_or_zero();
            let t1 = if t1 == Vec3::ZERO { Vec3::X } else { t1 };
            let n = Vec3::Y;
            let t2 = n.cross(t1);
            let local = |w: Vec3| Vec3::new(w.dot(t1), w.dot(t2), w.dot(n));
            let v = local(v_world);
            let c = local(center - p);
            let d = c.length();
            if d <= r {
                continue;
            }
            let axis = c / d;
            let cos_max = (1.0 - (r * r) / (d * d)).max(0.0).sqrt();
            let u = axis.any_orthonormal_vector();
            let w = axis.cross(u);
            let k = 128usize;
            let omega = 2.0 * std::f64::consts::PI * f64::from(1.0 - cos_max);
            let (mut sum, mut sum_nof) = (0.0f64, 0.0f64);
            for i in 0..k {
                for j in 0..k {
                    let cos_t = 1.0 - (1.0 - cos_max) * ((i as f32 + 0.5) / k as f32);
                    let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
                    let phi = std::f32::consts::TAU * (j as f32 + 0.5) / k as f32;
                    let l = axis * cos_t + (u * phi.cos() + w * phi.sin()) * sin_t;
                    let g = f64::from(regia_render::ltc::ggx_cos(v, l, alpha));
                    let h = (v + l).normalize_or_zero();
                    let fc = f64::from((1.0 - v.dot(h).clamp(0.0, 1.0)).powi(5));
                    sum += g * (f64::from(f0) + (1.0 - f64::from(f0)) * fc);
                    sum_nof += g;
                }
            }
            let i = (y * WIDTH + x) as usize;
            let scale = omega / (k * k) as f64 * radiance;
            cpu_ref[i] = [(sum * scale) as f32; 3];
            cpu_ref_nof[i] = [(sum_nof * scale) as f32; 3];
            let ff = f64::from(regia_render::ltc::sphere_form_factor(
                regia_render::ltc::ltc_inverse(&table, perceptual, v.z),
                c,
                r,
            ));
            let (a, b, e) = dfg(v.z);
            cpu_ltc[i] = [(ff * f64::from(f0 * a + b) * radiance) as f32; 3];
            // The magnitude as the single-scatter albedo E(μ) with the centre's Fresnel (`f0`, the DFG's B channel).
            cpu_ltc_e[i] = [(ff * f64::from(f0 * e) * radiance) as f32; 3];
        }
    }
    CpuSphereFloor {
        exact: cpu_ref,
        exact_no_fresnel: cpu_ref_nof,
        ltc_ab: cpu_ltc,
        ltc_e: cpu_ltc_e,
    }
}

#[test]
fn a_sphere_centred_below_the_floor_still_lights_it() {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let center = Vec3::new(0.0, -0.1, 0.0);
    let light = [LabLocalLight::Point(LabPointLight {
        position: center.to_array(),
        color: [1.0; 3],
        intensity: 40000.0,
        radius: 1000.0,
        cast_shadows: false,
        source_radius: Some(SPHERE_RADIUS),
    })];
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let frame = scene(&light, &post, &mesh, &material);
    let reference = traced_direct(&device, &frame);
    let mut extras = LightExtras::default();
    extras.mark_ltc_sphere(0);
    let ltc = hdr(&device, &frame, Some(extras));
    let representative = hdr(&device, &frame, None);
    let near = |p: Vec3| (p - center).length() < 3.0;
    let (n, mre, bias) = floor_error(&ltc, &reference, center, SPHERE_RADIUS, |_| 1.0, near);
    let (_, rep, _) = floor_error(&representative, &reference, center, SPHERE_RADIUS, |_| 1.0, near);
    eprintln!("sphere below the floor: {n} px, LTC {mre:.4} (bias {bias:+.4}), representative {rep:.4}");
    assert!(n > 200, "{n}");
    assert!(mre < 0.15, "mean relative error {mre}");
    // The representative point's centre is under the horizon, so the floor is black where the sphere's cap lights it.
    assert!(rep > 0.9, "representative point error {rep}");
}

#[test]
fn a_sized_ltc_spot_keeps_its_cone_and_matches_the_tracer() {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let light = sized_lights(Some(0.25));
    let spot = std::slice::from_ref(&light[1]);
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.8,
        ..LabMaterial::EMPTY
    };
    let frame = scene(spot, &post, &mesh, &material);
    let reference = traced_direct(&device, &frame);
    let mut extras = LightExtras::default();
    extras.mark_ltc_sphere(0);
    let ltc = hdr(&device, &frame, Some(extras));
    let (n, mre, bias) = floor_error(
        &ltc,
        &reference,
        Vec3::new(0.0, 1.0, 0.0),
        0.25,
        |_| 1.0,
        |_| true,
    );
    eprintln!("sized LTC spot: {n} px, forward vs PT mean relative error {mre:.4}, bias {bias:+.4}");
    assert!(n > 300, "{n}");
    assert!(mre < 0.06, "mean relative error {mre}");
    // Past the outer cone the forward floor stays black, as the tracer's does.
    let (mut outside, mut lit_outside) = (0usize, 0usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(p) = floor_hit(x, y) else { continue };
            let d = (p - Vec3::new(0.0, 1.0, 0.0)).normalize();
            if d.dot(Vec3::NEG_Y) < 1.2f32.cos() - 0.01 {
                outside += 1;
                lit_outside += usize::from(ltc[(y * WIDTH + x) as usize][1] > 0.0);
            }
        }
    }
    assert!(outside > 100, "{outside}");
    assert_eq!(lit_outside, 0);
}

#[test]
fn an_unmarked_sized_light_and_a_marked_unsized_one_are_bit_identical() {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.4,
        ..LabMaterial::EMPTY
    };
    let bits = |v: &[[f32; 3]]| v.iter().flat_map(|c| c.map(f32::to_bits)).collect::<Vec<_>>();
    // Sized and unmarked, with another light index marked.
    let sized = sized_lights(Some(0.3));
    let frame = scene(&sized, &post, &mesh, &material);
    let plain = hdr(&device, &frame, None);
    let mut extras = LightExtras::default();
    extras.mark_ltc_sphere(7);
    assert_eq!(bits(&plain), bits(&hdr(&device, &frame, Some(extras))));
    // Marked without a radius: the punctual light.
    let unsized_lights = sized_lights(None);
    let frame = scene(&unsized_lights, &post, &mesh, &material);
    let plain = hdr(&device, &frame, None);
    let mut extras = LightExtras::default();
    extras.mark_ltc_sphere(0);
    extras.mark_ltc_sphere(1);
    assert_eq!(bits(&plain), bits(&hdr(&device, &frame, Some(extras))));
}

#[test]
fn a_small_marked_sphere_falls_back_with_the_physical_falloff() {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.8,
        ..LabMaterial::EMPTY
    };
    // 2 cm at 2.5 m and more: under `LTC_SPHERE_MIN_TAN` at every floor pixel, so it is the representative point at
    // `1 / d²`: the unmarked frame scaled by the knee, pixel by pixel. The capture is `Rgba16Float`, whose step is
    // 2⁻¹⁰ of a value, so two captures of the same value scaled differently agree to about 0.1 %.
    let light = [LabLocalLight::Point(LabPointLight {
        position: LIGHT,
        color: [1.0; 3],
        intensity: 4000.0,
        radius: 1000.0,
        cast_shadows: false,
        source_radius: Some(0.02),
    })];
    let frame = scene(&light, &post, &mesh, &material);
    let plain = hdr(&device, &frame, None);
    let mut extras = LightExtras::default();
    extras.mark_ltc_sphere(0);
    let marked = hdr(&device, &frame, Some(extras));
    let center = Vec3::from_array(LIGHT);
    let (mut n, mut worst) = (0usize, 0.0f64);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(p) = floor_hit(x, y) else { continue };
            let i = (y * WIDTH + x) as usize;
            if plain[i][1] < 1e-3 {
                continue;
            }
            let d = (p - center).length();
            let want = f64::from(plain[i][1]) * f64::from((d * d + 1.0) / (d * d));
            worst = worst.max((f64::from(marked[i][1]) / want - 1.0).abs());
            n += 1;
        }
    }
    eprintln!("small marked sphere: {n} px, worst |marked / (plain · knee) − 1| {worst:.6}");
    assert!(n > 1000, "{n}");
    assert!(worst < 0.002, "{worst}");
}

/// `REGIA_AREA_FALLBACK_DUMP=<dir>` writes `<label>.png` for the owner (ADR-3247): the traced area light, the
/// frame with the fallback off (the shipped look on a tier with no emitter budget) and with it on, side by side at
/// the scene's EV 10, sRGB.
fn dump_plates(label: &str, images: &[&[[f32; 3]]]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_AREA_FALLBACK_DUMP") else {
        return;
    };
    let encode = |v: f32| {
        let v = (v / (1.2 * 2f32.powf(10.0))).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let out_w = WIDTH * images.len() as u32;
    let mut rgb = Vec::with_capacity((out_w * HEIGHT * 3) as usize);
    for y in 0..HEIGHT {
        for img in images {
            for x in 0..WIDTH {
                rgb.extend(img[(y * WIDTH + x) as usize].iter().map(|v| encode(*v)));
            }
        }
    }
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let file = std::fs::File::create(dir.join(format!("{label}.png"))).expect("dump file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), out_w, HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgb))
        .expect("dump png");
}

/// ADR-3247: on a tier with no emitter budget (`emissive_lights` 0, as `LAB`, `DesktopLow` and both Mobile tiers) a
/// rect or tube light emits nothing (the base: every floor pixel is black, error 100 %); its representative-point
/// stand-ins (`light_profiles::area_light_fallback`, the switch `RenderProfile::area_light_fallback`) light the
/// floor close to the traced area light. Rough floor, so the diffuse dominates; the stand-ins keep the punctual 1 m
/// knee, which reads dark under the light.
#[test]
fn area_light_stand_ins_match_the_traced_rect_and_tube() {
    let Some(device) = device() else { return };
    let (mesh, post) = (floor(), post());
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let rect = RectLight {
        width: 1.2,
        height: 0.6,
        intensity: 6000.0,
        cast_shadows: false,
        range: 40.0,
        ..RectLight::default()
    };
    let rect_model = Mat4::from_rotation_translation(
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        Vec3::new(0.0, 1.6, 0.0),
    );
    let tube = regia_render::TubeLight {
        length: 1.2,
        radius: 0.05,
        intensity: 6000.0,
        cast_shadows: false,
        range: 40.0,
        ..regia_render::TubeLight::default()
    };
    let tube_model = Mat4::from_translation(Vec3::new(0.0, 1.6, 0.0));
    let rows = [
        (
            "rect",
            AreaLightInput {
                light: rect_emitter(&rect, &rect_model),
                range: rect.range,
                cast_shadows: false,
            },
        ),
        (
            "tube",
            AreaLightInput {
                light: regia_render::light_profiles::tube_emitter(&tube, &tube_model),
                range: tube.range,
                cast_shadows: false,
            },
        ),
    ];
    let tracer = PathTracer::new(&device).expect("compile");
    let mut fails = Vec::new();
    for (label, area) in rows {
        // The base: the area light rides the emitter path, which a zero budget drops.
        let mut extras = LightExtras::default();
        extras.area_lights.push(area);
        let off_frame = scene(&[], &post, &mesh, &material);
        assert_eq!(off_frame.quality.emissive_lights, 0, "LAB has no emitter budget");
        let off = hdr(&device, &off_frame, Some(extras.clone()));
        let stand_ins = regia_render::light_profiles::area_light_fallback(&area);
        let frame = scene(&stand_ins, &post, &mesh, &material);
        let forward = hdr(&device, &frame, Some(extras));
        let mut pt = from_frame(
            &scene(&[], &post, &mesh, &material),
            &ExtractOptions {
                sky: Some(PtSky::None),
                ..ExtractOptions::default()
            },
        );
        push_area_lights(&mut pt, &[area]);
        let img = tracer
            .render(
                &pt,
                &PtSettings {
                    spp: 512,
                    max_bounces: 1,
                    jitter: false,
                    ..PtSettings::default()
                },
            )
            .expect("trace");
        // `knee` takes the punctual lights' 1 m falloff knee out of the stand-ins' value, `(d² + 1) / d²` from the
        // light's centre, as `source_size_parity` does, so only the stand-in's shape is left.
        let centre = Vec3::from_array(area.light.centre);
        let stats = |f: &[[f32; 3]], knee: bool| {
            let (mut n, mut err, mut bias, mut sum_f, mut sum_r) = (0usize, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let Some(p) = floor_hit(x, y) else { continue };
                    let i = (y * WIDTH + x) as usize;
                    let reference = f64::from(img.direct[i][1]);
                    if reference < 0.05 {
                        continue;
                    }
                    let d2 = f64::from((p - centre).length_squared());
                    let value = f64::from(f[i][1]) * if knee { (d2 + 1.0) / d2 } else { 1.0 };
                    let rel = value / reference - 1.0;
                    err += rel.abs();
                    bias += rel;
                    sum_f += value;
                    sum_r += reference;
                    n += 1;
                }
            }
            let k = n.max(1) as f64;
            (n, err / k, bias / k, sum_f / sum_r.max(1e-30))
        };
        let (n, mre, bias, energy) = stats(&forward, false);
        let (_, shape_mre, shape_bias, shape_energy) = stats(&forward, true);
        let (_, off_mre, _, off_energy) = stats(&off, false);
        eprintln!(
            "{label} stand-ins: {n} px, forward vs PT mean relative error {mre:.4}, bias {bias:+.4}, energy ×{energy:.3}; \
             without the 1 m knee {shape_mre:.4}, bias {shape_bias:+.4}, energy ×{shape_energy:.3}; fallback off: error \
             {off_mre:.4}, energy ×{off_energy:.3}"
        );
        dump_plates(&format!("area_fallback_{label}"), &[&img.direct, &off, &forward]);
        let mut check = |ok: bool, what: String| {
            if !ok {
                fails.push(format!("{label}: {what}"));
            }
        };
        check(n > 500, format!("{n} px"));
        check(
            off_mre > 0.99,
            format!("with no budget and no stand-ins the floor is black ({off_mre})"),
        );
        // The stand-in's shape, the knee taken out.
        check(
            shape_mre < 0.08,
            format!("mean relative error without the knee {shape_mre}"),
        );
        check(
            (shape_energy - 1.0).abs() < 0.08,
            format!("energy without the knee ×{shape_energy}"),
        );
        // As shipped, the knee darkens the floor under the light.
        check(mre < 0.2, format!("mean relative error {mre}"));
        check((energy - 1.0).abs() < 0.2, format!("energy ×{energy}"));
    }
    assert!(fails.is_empty(), "{fails:?}");
}

/// One frame of the penumbra scene: the floor and, with `plate`, a 1 m square plate 1 m above it under a sized spot
/// light 3 m up (`source_radius` 0.25 m, shadowed), seen close from beside the shadow's +X edge. Returns the forward's
/// linear HDR and the tracer's direct light.
fn penumbra_frame(device: &RenderDevice, plate: bool) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let (floor_mesh, post) = (floor(), post());
    let plate_mesh = regia_render::box_mesh([0.5, 0.02, 0.5]);
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let lights = [LabLocalLight::Spot(LabSpotLight {
        position: [0.0, 3.0, 0.0],
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        intensity: 400.0,
        range: 60.0,
        inner_angle: 1.0,
        outer_angle: 1.2,
        cast_shadows: true,
        source_radius: Some(PENUMBRA_SOURCE),
    })];
    let mut meshes = vec![FrameMesh {
        is_static: false,
        mesh_key: 1,
        material_key: 2,
        object_key: 3,
        vertices: &floor_mesh.0,
        indices: &floor_mesh.1,
        material: &material,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }];
    if plate {
        meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 4,
            material_key: 2,
            object_key: 5,
            vertices: &plate_mesh.0,
            indices: &plate_mesh.1,
            material: &material,
            model: Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    let frame = FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: PENUMBRA_EYE,
        target: PENUMBRA_TARGET,
        fov_y: PENUMBRA_FOV,
        near: 0.05,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 10.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(&post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: &lights,
        quality: FrameQuality {
            specular_aa: false,
            shadow_filter: regia_render::ShadowFilter::PcssLite,
            ..FrameQuality::LAB
        },
        meshes,
    };
    let forward = hdr(device, &frame, None);
    let pt = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let traced = PathTracer::new(device)
        .expect("compile")
        .render(
            &pt,
            &PtSettings {
                spp: 1024,
                max_bounces: 1,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace")
        .direct;
    (forward, traced)
}

const PENUMBRA_SOURCE: f32 = 0.25;
const PENUMBRA_EYE: [f32; 3] = [0.8, 2.2, 1.6];
const PENUMBRA_TARGET: [f32; 3] = [0.8, 0.0, 0.0];
const PENUMBRA_FOV: f32 = 0.55;

/// The floor point under pixel (x, y) of the penumbra view.
fn penumbra_hit(x: u32, y: u32) -> Option<Vec3> {
    let eye = Vec3::from_array(PENUMBRA_EYE);
    let fwd = (Vec3::from_array(PENUMBRA_TARGET) - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let th = (PENUMBRA_FOV * 0.5).tan();
    let aspect = WIDTH as f32 / HEIGHT as f32;
    let sx = ((x as f32 + 0.5) / WIDTH as f32 * 2.0 - 1.0) * th * aspect;
    let sy = (1.0 - (y as f32 + 0.5) / HEIGHT as f32 * 2.0) * th;
    let d = (fwd + right * sx + up * sy).normalize();
    (d.y < -1e-3).then(|| eye + d * (-eye.y / d.y))
}

/// The visibility (with the plate / without) across the shadow's +X edge, in 2 cm bins of the floor's x from 0.4 to
/// 1.2 m, over the strip |z| < 0.3 m away from the plate's corners.
fn penumbra_profile(with: &[[f32; 3]], without: &[[f32; 3]]) -> Vec<Option<f64>> {
    let bins = 40usize;
    let (mut sum, mut n) = (vec![0.0f64; bins], vec![0usize; bins]);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(p) = penumbra_hit(x, y) else { continue };
            if p.z.abs() > 0.3 || p.x < 0.4 || p.x >= 1.2 {
                continue;
            }
            let i = (y * WIDTH + x) as usize;
            let open = f64::from(without[i][1]);
            if open < 1e-3 {
                continue;
            }
            let b = (((p.x - 0.4) / 0.02) as usize).min(bins - 1);
            sum[b] += (f64::from(with[i][1]) / open).clamp(0.0, 1.5);
            n[b] += 1;
        }
    }
    sum.iter()
        .zip(&n)
        .map(|(s, &k)| (k > 0).then(|| s / k as f64))
        .collect()
}

/// The x (m) where the profile first rises through `level`, linear between bins.
fn crossing(profile: &[Option<f64>], level: f64) -> Option<f64> {
    let mut last: Option<(f64, f64)> = None;
    for (b, v) in profile.iter().enumerate() {
        let Some(v) = *v else { continue };
        let x = 0.4 + 0.02 * (b as f64 + 0.5);
        if let Some((x0, v0)) = last
            && v0 < level
            && v >= level
        {
            return Some(x0 + (x - x0) * (level - v0) / (v - v0).max(1e-9));
        }
        last = Some((x, v));
    }
    None
}

/// ADR-3247 (item 3, gone deeper): a sized spot's shadow penumbra in the forward frame (the local-light PCSS, its
/// source radius in the tile's `bias.w`, ADR-1367) against the traced sphere spot (`PtLight::SphereSpot`,
/// ADR-2592), on the floor under a 1 m plate 1 m up, the 0.25 m source 3 m up. Geometry gives a full penumbra of
/// `0.5 · (3 − 2) / 2 = 0.25` m on the floor (source diameter × (receiver − blocker) / blocker distance), centred
/// 0.75 m out. The 10 %-90 % widths and the 50 % positions must
/// agree; before ADR-2592 the tracer drew this shadow from a delta spot, with no penumbra at all.
#[test]
fn a_sized_spot_penumbra_matches_the_traced_sphere_spot() {
    let Some(device) = device() else { return };
    let (fwd_open, pt_open) = penumbra_frame(&device, false);
    let (fwd_shadow, pt_shadow) = penumbra_frame(&device, true);
    let raster = penumbra_profile(&fwd_shadow, &fwd_open);
    let traced = penumbra_profile(&pt_shadow, &pt_open);
    let edges = |p: &[Option<f64>]| (crossing(p, 0.1), crossing(p, 0.5), crossing(p, 0.9));
    let (r10, r50, r90) = edges(&raster);
    let (t10, t50, t90) = edges(&traced);
    let show = |p: &[Option<f64>]| {
        p.iter()
            .map(|v| v.map_or("  -  ".to_string(), |v| format!("{v:.2}")))
            .collect::<Vec<_>>()
            .join(" ")
    };
    eprintln!(
        "penumbra raster: {}\npenumbra traced: {}",
        show(&raster),
        show(&traced)
    );
    eprintln!("raster 10/50/90 % at {r10:?} / {r50:?} / {r90:?}; traced at {t10:?} / {t50:?} / {t90:?}");
    let (Some(r10), Some(r50), Some(r90), Some(t10), Some(t50), Some(t90)) = (r10, r50, r90, t10, t50, t90)
    else {
        panic!("a profile does not cross 10, 50 and 90 % between 0.4 and 1.2 m");
    };
    let (raster_width, traced_width) = (r90 - r10, t90 - t10);
    eprintln!(
        "10-90 % width: raster {raster_width:.3} m, traced {traced_width:.3} m (ratio {:.2}); 50 % offset {:+.3} m",
        raster_width / traced_width,
        r50 - t50
    );
    assert!(
        traced_width > 0.15,
        "the traced sphere spot casts a soft shadow: {traced_width}"
    );
    assert!(
        (raster_width / traced_width - 1.0).abs() < 0.5,
        "the raster penumbra {raster_width:.3} m against the traced {traced_width:.3} m"
    );
    assert!(
        (r50 - t50).abs() < 0.08,
        "the 50 % edge moved: raster {r50:.3}, traced {t50:.3}"
    );
}
