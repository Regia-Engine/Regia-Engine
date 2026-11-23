// SPDX-License-Identifier: MPL-2.0
//! `specs/27` § 1.5 item 4 with a sized local light (ADR-1367): on the material-sphere grid lit
//! by one point light with a `LightSourceSize` of 0.2 m, the forward pass's direct term (centre
//! diffuse, representative-point specular with the energy term of Lagarde & de Rousiers 2014
//! § 4.7.5 / Karis 2013 eqs. 10, 11, 14) stays within the item's bound of the path tracer's
//! `direct` AOV, which integrates a true 0.2 m sphere light of the same intensity.
//!
//! Two differences between the renderers are not what this compares, and are removed per pixel
//! from the geometry the test knows:
//! - the forward falloff is windowed and softened, `window² / (d² + 1)` (`distance_attenuation`),
//!   the tracer's is the physical `1 / d²`; the forward value is rescaled by their ratio at the
//!   pixel's distance to the light;
//! - a sphere that dips below a pixel's tangent plane lights it less than the point at its centre
//!   (Karis 2013: "equivalent to a point light if the sphere is above the horizon"); pixels within
//!   two angular radii of the light's horizon are skipped. The point-light run uses the same mask.

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, HardSurfaceFactors,
    LabLocalLight, LabMaterial, LabPointLight, PostSettings, RenderDevice, ShadowCast,
};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 96;
const EYE: [f32; 3] = [0.0, 0.0, 16.0];
const FOV_Y: f32 = 0.62;
/// In front of the grid, between it and the camera: 5 m from the nearest sphere's surface.
const LIGHT_POS: [f32; 3] = [0.0, 0.0, 6.0];
/// Cutoff radius far past the grid, so the forward window is 1 to within 1e-7 here.
const LIGHT_RANGE: f32 = 1000.0;
/// About 20 klx at 6 m (the sun's illuminance in the item-4 sun test).
const LUMENS: f32 = 9.0e6;
/// The brief's source size.
const SOURCE_RADIUS: f32 = 0.2;

fn materials() -> Vec<LabMaterial> {
    let mut materials = Vec::new();
    for row in 0..3 {
        for col in 0..5 {
            let roughness = [0.1, 0.3, 0.5, 0.7, 1.0][col];
            let mut m = LabMaterial {
                roughness,
                ..LabMaterial::EMPTY
            };
            match row {
                0 => m.base_color = [0.8, 0.2, 0.15, 1.0],
                1 => {
                    m.base_color = [1.0, 0.78, 0.34, 1.0];
                    m.metallic = 1.0;
                }
                _ => {
                    m.base_color = [0.1, 0.25, 0.6, 1.0];
                    m.hard_surface = HardSurfaceFactors {
                        clearcoat: 1.0,
                        clearcoat_roughness: 0.2,
                        ..HardSurfaceFactors::OFF
                    };
                }
            }
            materials.push(m);
        }
    }
    materials
}

fn center(i: usize) -> Vec3 {
    let (row, col) = ((i / 5) as f32, (i % 5) as f32);
    Vec3::new((col - 2.0) * 2.6, (1.0 - row) * 2.6, 0.0)
}

/// Nearest unit-sphere hit of the pixel-centre camera ray: (world point, sphere index).
fn hit(x: usize, y: usize) -> Option<(Vec3, usize)> {
    let eye = Vec3::from_array(EYE);
    let fwd = (Vec3::ZERO - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let tan_half = (FOV_Y * 0.5).tan();
    let aspect = WIDTH as f32 / HEIGHT as f32;
    let sx = ((x as f32 + 0.5) / WIDTH as f32 * 2.0 - 1.0) * tan_half * aspect;
    let sy = (1.0 - (y as f32 + 0.5) / HEIGHT as f32 * 2.0) * tan_half;
    let dir = (fwd + right * sx + up * sy).normalize();
    let mut best: Option<(f32, usize)> = None;
    for i in 0..15 {
        let oc = eye - center(i);
        let b = oc.dot(dir);
        let disc = b * b - (oc.length_squared() - 1.0);
        if disc < 0.0 {
            continue;
        }
        let t = -b - disc.sqrt();
        if t > 0.0 && best.is_none_or(|(bt, _)| t < bt) {
            best = Some((t, i));
        }
    }
    best.map(|(t, i)| (eye + dir * t, i))
}

/// Forward linear HDR and PT `direct` of the grid under one point light.
fn render(device: &RenderDevice, source_radius: Option<f32>) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let (verts, idx) = regia_render::uv_sphere(1.0, 64, 32);
    let materials = materials();
    let meshes: Vec<FrameMesh<'_>> = materials
        .iter()
        .enumerate()
        .map(|(i, m)| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 100 + i as u64,
            object_key: 200 + i as u64,
            vertices: &verts,
            indices: &idx,
            material: m,
            model: Mat4::from_translation(center(i)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let lights = [LabLocalLight::Point(LabPointLight {
        position: LIGHT_POS,
        color: [1.0, 0.98, 0.95],
        intensity: LUMENS,
        radius: LIGHT_RANGE,
        cast_shadows: false,
        source_radius,
    })];
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let scene = FrameScene {
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
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(&post),
        fog: regia_render::FogSettings::default(),
        // No sun: the point light is the only light.
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: &lights,
        // As in item 4: specular AA filters the forward lobe by screen-space normal variance,
        // which the 1-ray-per-pixel-centre trace does not.
        quality: FrameQuality {
            specular_aa: false,
            ..FrameQuality::LAB
        },
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("forward frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists");
    assert_eq!((hdr.width, hdr.height), (WIDTH, HEIGHT));

    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    assert_eq!(pt_scene.lights.len(), 1);
    let tracer = PathTracer::new(device).expect("compile");
    // The sphere is sampled by one cone direction per path; 1024 paths per pixel bring the
    // glossy highlights' noise well under the bound. The point light needs one.
    let settings = PtSettings {
        spp: if source_radius.is_some() { 1024 } else { 1 },
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let img = tracer.render(&pt_scene, &settings).expect("trace");
    (hdr.rgb, img.direct)
}

/// Per-pixel comparison of a forward frame with a PT `direct` image.
struct Stats {
    /// Compared pixels.
    n: usize,
    /// Mean relative error.
    mre: f64,
    /// Per sphere: (pixels, mean relative error, mean signed relative error).
    spheres: Vec<(usize, f64, f64)>,
}

fn compare(forward: &[[f32; 3]], direct: &[[f32; 3]]) -> Stats {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let lum = |c: [f32; 3]| 0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2]);
    let lit: Vec<f64> = direct.iter().map(|c| lum(*c)).filter(|v| *v > 0.0).collect();
    let floor = 0.01 * lit.iter().sum::<f64>() / lit.len().max(1) as f64;
    let light = Vec3::from_array(LIGHT_POS);
    let mut spheres = vec![(0usize, 0.0f64, 0.0f64); 15];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let Some((p, sphere)) = hit(x, y) else {
                continue;
            };
            // Interior sphere pixels (all four neighbours on the same sphere).
            let interior = [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]
                .iter()
                .all(|&(nx, ny)| hit(nx, ny).is_some_and(|(_, s)| s == sphere));
            if !interior {
                continue;
            }
            let to_light = light - p;
            let d = to_light.length();
            let normal = (p - center(sphere)).normalize();
            if normal.dot(to_light / d) < 2.0 * SOURCE_RADIUS / d {
                continue;
            }
            let pt = lum(direct[i]);
            if pt < floor {
                continue;
            }
            let window = (1.0 - (d / LIGHT_RANGE).powi(4)).clamp(0.0, 1.0).powi(2);
            let falloff = f64::from((d * d + 1.0) / (d * d) / window.max(1e-12));
            let fwd = lum(forward[i]) * falloff;
            let entry = &mut spheres[sphere];
            entry.0 += 1;
            entry.1 += (fwd - pt).abs() / pt;
            entry.2 += (fwd - pt) / pt;
        }
    }
    let n: usize = spheres.iter().map(|s| s.0).sum();
    let mre = spheres.iter().map(|s| s.1).sum::<f64>() / n.max(1) as f64;
    for s in &mut spheres {
        let k = s.0.max(1) as f64;
        s.1 /= k;
        s.2 /= k;
    }
    Stats { n, mre, spheres }
}

fn report(label: &str, stats: &Stats) {
    eprintln!("{label}: {} px, mean relative error {:.5}", stats.n, stats.mre);
    for (i, (n, mre, signed)) in stats.spheres.iter().enumerate() {
        let roughness = [0.1, 0.3, 0.5, 0.7, 1.0][i % 5];
        let row = ["dielectric", "metal", "clearcoat"][i / 5];
        eprintln!("  {row} r{roughness}: {n} px, mean relative {mre:.5}, signed {signed:+.5}");
    }
}

#[test]
fn forward_direct_equals_pt_direct_under_a_sized_point_light() {
    let device = RenderDevice::headless().expect("headless device");
    let (fwd_point, pt_point) = render(&device, None);
    let (fwd_sphere, pt_sphere) = render(&device, Some(SOURCE_RADIUS));
    let point = compare(&fwd_point, &pt_point);
    let sphere = compare(&fwd_sphere, &pt_sphere);
    // What the forward pass would score without the source size (punctual specular against the
    // sphere): the effect the widening has to account for.
    let punctual = compare(&fwd_point, &pt_sphere);
    report("forward vs PT direct, point light", &point);
    report("forward vs PT direct, 0.2 m sphere light", &sphere);
    report("punctual forward vs PT 0.2 m sphere (no source size)", &punctual);
    assert!(
        point.n > 2000 && sphere.n > 2000,
        "{} / {} compared pixels",
        point.n,
        sphere.n
    );
    assert!(
        point.mre < 0.01,
        "forward vs PT direct, point light: mean relative error {}",
        point.mre
    );
    assert!(
        sphere.mre < 0.01,
        "forward vs PT direct, 0.2 m sphere light: mean relative error {}",
        sphere.mre
    );
    // The source size reached the forward specular: the glossy highlights changed.
    let changed = fwd_point.iter().zip(&fwd_sphere).filter(|(a, b)| a != b).count();
    assert!(changed > 0, "the 0.2 m source left the forward frame unchanged");
}

/// One glossy gold sphere 1.5 m in front of the 0.2 m light, close to the camera, so the
/// highlight's core spans tens of pixels: forward linear HDR and PT `direct`.
fn render_close_up(device: &RenderDevice, source_radius: Option<f32>) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    render_close_up_with(device, source_radius, false, 1024, 4.0e5)
}

/// [`render_close_up`] with the light shaded through the LTC sphere (`LightSourceLtc`, `ltc`), the trace at `spp`
/// samples and the light at `lumens`.
fn render_close_up_with(
    device: &RenderDevice,
    source_radius: Option<f32>,
    ltc: bool,
    spp: u32,
    lumens: f32,
) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let (verts, idx) = regia_render::uv_sphere(1.0, 128, 64);
    let material = LabMaterial {
        base_color: [1.0, 0.78, 0.34, 1.0],
        metallic: 1.0,
        roughness: 0.2,
        ..LabMaterial::EMPTY
    };
    let lights = [LabLocalLight::Point(LabPointLight {
        position: CLOSE_LIGHT,
        color: [1.0; 3],
        intensity: lumens,
        radius: LIGHT_RANGE,
        cast_shadows: false,
        source_radius,
    })];
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let scene = FrameScene {
        width: CLOSE_SIZE,
        height: CLOSE_SIZE,
        eye: CLOSE_EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: CLOSE_FOV,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(&post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: &lights,
        quality: FrameQuality {
            specular_aa: false,
            ..FrameQuality::LAB
        },
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 100,
            object_key: 200,
            vertices: &verts,
            indices: &idx,
            material: &material,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    let mut renderer = FrameRenderer::new();
    if ltc {
        let mut extras = regia_render::light_profiles::LightExtras::default();
        extras.mark_ltc_sphere(0);
        renderer.set_light_extras(extras);
    }
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("forward frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists");
    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(device).expect("compile");
    let img = tracer
        .render(
            &pt_scene,
            &PtSettings {
                spp,
                max_bounces: 1,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace");
    (hdr.rgb, img.direct)
}

const CLOSE_SIZE: u32 = 128;
const CLOSE_EYE: [f32; 3] = [0.0, 0.0, 4.0];
const CLOSE_FOV: f32 = 0.6;
const CLOSE_LIGHT: [f32; 3] = [0.6, 0.4, 2.5];

/// Mean relative error of `forward` against `direct` over the highlight: pixels where the
/// traced sphere's highlight is above a tenth of its peak. Also the summed-energy ratio there.
fn highlight_error(forward: &[[f32; 3]], direct: &[[f32; 3]]) -> (usize, f64, f64) {
    highlight_error_with(forward, direct, true)
}

/// [`highlight_error`]; `knee` takes the punctual 1 m knee out of the forward value (an LTC sphere has none).
fn highlight_error_with(forward: &[[f32; 3]], direct: &[[f32; 3]], knee: bool) -> (usize, f64, f64) {
    let n = CLOSE_SIZE as usize;
    let lum = |c: [f32; 3]| 0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2]);
    let peak = direct.iter().map(|c| lum(*c)).fold(0.0f64, f64::max);
    let eye = Vec3::from_array(CLOSE_EYE);
    let tan_half = (CLOSE_FOV * 0.5).tan();
    let light = Vec3::from_array(CLOSE_LIGHT);
    let (mut count, mut rel, mut sum_f, mut sum_p) = (0usize, 0.0f64, 0.0f64, 0.0f64);
    for y in 0..n {
        for x in 0..n {
            let i = y * n + x;
            let pt = lum(direct[i]);
            if pt < 0.1 * peak {
                continue;
            }
            // The pixel's point on the unit sphere (camera on +Z looking at the origin).
            let sx = ((x as f32 + 0.5) / n as f32 * 2.0 - 1.0) * tan_half;
            let sy = (1.0 - (y as f32 + 0.5) / n as f32 * 2.0) * tan_half;
            let dir = Vec3::new(sx, sy, -1.0).normalize();
            let b = eye.dot(dir);
            let disc = b * b - (eye.length_squared() - 1.0);
            if disc < 0.0 {
                continue;
            }
            let p = eye + dir * (-b - disc.sqrt());
            let d = (light - p).length();
            let window = (1.0 - (d / LIGHT_RANGE).powi(4)).clamp(0.0, 1.0).powi(2);
            let softened = if knee { (d * d + 1.0) / (d * d) } else { 1.0 };
            let fwd = lum(forward[i]) * f64::from(softened / window.max(1e-12));
            rel += (fwd - pt).abs() / pt;
            sum_f += fwd;
            sum_p += pt;
            count += 1;
        }
    }
    (count, rel / count.max(1) as f64, sum_f / sum_p.max(1e-30))
}

/// On a highlight large enough to resolve, the sized forward specular is closer to the traced
/// sphere light than the punctual one (the reason the component exists).
#[test]
fn a_resolved_highlight_follows_the_traced_sphere_better_than_a_point() {
    let device = RenderDevice::headless().expect("headless device");
    let (fwd_point, _) = render_close_up(&device, None);
    let (fwd_sphere, pt_sphere) = render_close_up(&device, Some(SOURCE_RADIUS));
    let (n, mre_point, energy_point) = highlight_error(&fwd_point, &pt_sphere);
    let (_, mre_sphere, energy_sphere) = highlight_error(&fwd_sphere, &pt_sphere);
    eprintln!(
        "close-up highlight, {n} px against the traced 0.2 m sphere: punctual forward mean relative \
         error {mre_point:.4} (energy ×{energy_point:.3}); sized forward {mre_sphere:.4} \
         (energy ×{energy_sphere:.3})"
    );
    assert!(n > 40, "{n} highlight pixels");
    assert!(
        mre_sphere < mre_point,
        "sized {mre_sphere} vs punctual {mre_point} on the resolved highlight"
    );
}

/// ADR-3247 (item 2 of the light-shape lane): the same resolved highlight shaded through the LTC sphere
/// (`LightSourceLtc`) follows the traced sphere's flat-topped disc, not a lobe. At the 4e5 lm of
/// `a_resolved_highlight_follows_the_traced_sphere_better_than_a_point` the highlight's core reflects about 230 000
/// cd/m², above the `Rgba16Float` capture's 65 504, so the forward's core is clipped and both forms read about ×0.71 of
/// the trace's energy there (measured 11-06: representative 21.8 %, LTC 20.2 %, 4096 spp); part of ADR-1367's 22 % is
/// that clip. This row lights the gold at a tenth of that (core about 23 000 cd/m²) and traces at 4096 spp.
#[test]
fn an_ltc_sphere_follows_the_resolved_highlight_closer_than_the_representative_point() {
    let device = RenderDevice::headless().expect("headless device");
    let (fwd_rep, pt) = render_close_up_with(&device, Some(SOURCE_RADIUS), false, 4096, 4.0e4);
    let (fwd_ltc, _) = render_close_up_with(&device, Some(SOURCE_RADIUS), true, 1, 4.0e4);
    let peak = |img: &[[f32; 3]]| img.iter().flat_map(|c| c.iter().copied()).fold(0.0f32, f32::max);
    assert!(
        peak(&pt) < 6.0e4,
        "the traced core {} would clip in the f16 capture",
        peak(&pt)
    );
    dump_close_up(&[&pt, &fwd_rep, &fwd_ltc]);
    let (n, mre_rep, energy_rep) = highlight_error_with(&fwd_rep, &pt, true);
    let (_, mre_ltc, energy_ltc) = highlight_error_with(&fwd_ltc, &pt, false);
    eprintln!(
        "close-up highlight, {n} px against the traced 0.2 m sphere (4096 spp): representative point {mre_rep:.4} \
         (energy ×{energy_rep:.3}); LTC sphere {mre_ltc:.4} (energy ×{energy_ltc:.3})"
    );
    assert!(n > 40, "{n} highlight pixels");
    assert!(
        mre_ltc < mre_rep,
        "LTC {mre_ltc} vs representative {mre_rep} on the resolved highlight"
    );
    assert!(mre_ltc < 0.1, "LTC sphere highlight error {mre_ltc}");
    assert!(
        (energy_ltc - 1.0).abs() < 0.1,
        "LTC sphere highlight energy ×{energy_ltc}"
    );
}

/// `REGIA_SIZED_LTC_DUMP=<dir>` writes `sized_light_ltc.png` for the owner (ADR-3247): the traced 0.2 m sphere, the
/// representative point (the shipped look) and the LTC sphere (`RenderProfile::sized_light_ltc`) on the gold close-up
/// at 4e4 lm, side by side at EV 10 and, below, 4 EV less so the highlight's core is not clipped.
fn dump_close_up(images: &[&[[f32; 3]]]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_SIZED_LTC_DUMP") else {
        return;
    };
    let encode = |v: f32, exposure: f32| {
        let v = (v * exposure).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let (w, h) = (CLOSE_SIZE, CLOSE_SIZE);
    let out_w = w * images.len() as u32;
    let mut rgb = Vec::with_capacity((out_w * h * 6) as usize);
    for ev in [10.0f32, 14.0] {
        let exposure = 1.0 / (1.2 * 2f32.powf(ev));
        for y in 0..h {
            for img in images {
                for x in 0..w {
                    rgb.extend(img[(y * w + x) as usize].iter().map(|v| encode(*v, exposure)));
                }
            }
        }
    }
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let file = std::fs::File::create(dir.join("sized_light_ltc.png")).expect("dump file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), out_w, h * 2);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgb))
        .expect("dump png");
}
