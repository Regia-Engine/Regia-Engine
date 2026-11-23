// SPDX-License-Identifier: MPL-2.0
//! The `specs/27` § 2.2 calibration suite (ADR-2660): every per-technique target scored with an
//! automatic metric against the reference path tracer, on scenes a lane can build (no fetched or
//! DO27 content). Hand-run: every test here is `#[ignore]`d; `docs/06` § 2 and
//! `book/agents/spec27-calibration.md` give the commands.
//!
//! Each `spec27_*` test measures one or more § 2.2 rows and records them
//! ([`regia_test_utils::spec27::record`]) without asserting the target: the suite reports the
//! truth, passing or not. `spec27_write_report` then reads the rows, the logs of the older tests
//! that own the remaining rows (`gi_validation`, `sssr_specular`, `aerial_parity`, `cloud_field`)
//! and writes `$REGIA_SPEC27_OUT/report.md`, the page committed under `docs/evidence/spec27/`.
//! A row that fails also has a `target_*` test that asserts the target, ignored with the reason.
//!
//! The real-time frame has no AOVs, so each technique is isolated by a pair of frames that
//! differ in that technique alone: sun visibility is the frame over the same frame without sun
//! shadows, local-light visibility the frame over the frame with that light's shadow off,
//! reflections the frame over the frame without screen-space reflections. The tracer's side is
//! its AOV (`shadow`, `direct`, `indirect_specular`, `sky`) or, for local lights (the `shadow` AOV
//! is the sun's), the direct light with the caster over the direct light without it.

use std::sync::{Mutex, OnceLock, PoisonError};

use regia_core::glam::Vec3;
use regia_pt::extract::{ExtractOptions, atmosphere_sky, from_frame, media_from_outdoor};
use regia_pt::{PathTracer, PtImage, PtSettings, PtSky};
use regia_render::compare::{DEFAULT_PPD, display_encode, flip, srgb8_to_unit};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene,
    GiMode, HardSurfaceFactors, LabLocalLight, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, VolumetricFogSettings,
};
use regia_test_utils::gi_error::{GiErrorInput, luminance, measure};
use regia_test_utils::spec27::{
    self, COLORCHECKER_SRGB, Row, Verdict, centroid, ciede2000, colorchecker_linear, edge_width, ev_offset,
    miss_over, quantile, srgb_to_lab,
};

/// Angular radius (tangent) of the real sun, as the raster's PCSS uses it
/// (`regia_render::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN`).
const SUN_RADIUS: f32 = 0.004_675;
/// Sun elevation of the shadow scenes, degrees.
const SUN_ELEVATION_DEG: f32 = 45.0;
/// Sun illuminance of the lab scenes, lux.
const SUN_LUX: f32 = 100_000.0;
/// Display exposure of the sun-lit lab scenes.
const SUN_EV: f32 = 14.4;

static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("the spec 27 suite needs a GPU; lavapipe is one"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn gpu() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner)
}

fn record(row: Row) {
    if let Err(e) = spec27::record(&row) {
        eprintln!("spec27: could not write row {}: {e}", row.id);
    }
}

/// A row: `(scene, spp)` and `(miss, note)` travel together.
fn row(
    id: &str,
    area: &str,
    target: &str,
    measured: String,
    verdict: Verdict,
    (scene, spp): (&str, u32),
    (miss, note): (Option<f64>, &str),
) -> Row {
    Row {
        id: id.to_owned(),
        area: area.to_owned(),
        target: target.to_owned(),
        measured,
        verdict,
        scene: scene.to_owned(),
        spp: if spp == 0 { "-".to_owned() } else { spp.to_string() },
        miss,
        note: note.to_owned(),
        commit: None,
    }
}

// --- Scene building --------------------------------------------------------------------------

/// A quad centred on `c` spanning `±u` and `±v`, facing `u × v` (counter-clockwise from the front).
fn quad(c: Vec3, u: Vec3, v: Vec3, material: usize) -> LabMesh {
    let n = u.cross(v).normalize_or(Vec3::Y).to_array();
    let corners = [c - u - v, c + u - v, c + u + v, c - u + v];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    LabMesh {
        vertices: corners
            .iter()
            .zip(uvs)
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p.to_array(), n, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// An axis-aligned box from `lo` to `hi`, faces outward.
fn cuboid(lo: Vec3, hi: Vec3, material: usize) -> Vec<LabMesh> {
    let c = 0.5 * (lo + hi);
    let h = 0.5 * (hi - lo);
    let (x, y, z) = (Vec3::X * h.x, Vec3::Y * h.y, Vec3::Z * h.z);
    vec![
        quad(c + x, -z, y, material),
        quad(c - x, z, y, material),
        quad(c + y, x, -z, material),
        quad(c - y, x, z, material),
        quad(c + z, x, y, material),
        quad(c - z, -x, y, material),
    ]
}

/// A ground quad at y = 0 of half size `h` around the origin.
fn ground(h: f32, material: usize) -> LabMesh {
    quad(Vec3::ZERO, Vec3::X * h, -Vec3::Z * h, material)
}

fn grey(albedo: f32) -> LabMaterial {
    LabMaterial {
        base_color: [albedo, albedo, albedo, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..LabMaterial::default()
    }
}

fn post(ev: f32) -> PostSettings {
    PostSettings {
        exposure_ev: ev,
        use_physical_camera: false,
        exposure_compensation: 0.0,
        ..PostSettings::golden_deterministic()
    }
}

/// Desktop High at native scale without temporal upscaling, the authored vertical field of view
/// (the tracer's camera), no fog, clouds, physical sky or reflections, the flat sky term for GI
/// (each row turns its own technique on).
///
/// The frames also render under an airless planet ([`airless`]): Desktop High's sky otherwise
/// adds light no shadow or occlusion term touches, which reads as a missing shadow (a point
/// light's shadow at 0.75 visibility) and as weaker contact darkening.
fn desktop_high() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

/// Light travelling down at `elevation_deg`, toward +X.
fn sun_dir(elevation_deg: f32) -> [f32; 3] {
    let e = elevation_deg.to_radians();
    [e.cos(), -e.sin(), 0.0]
}

/// A lab scene with the few fields the rows vary; the camera is `(eye, target, fov_y)`.
fn lab(
    (width, height): (u32, u32),
    (eye, target, fov_y): ([f32; 3], [f32; 3], f32),
    ev: f32,
    directional: DirectionalLightUniform,
    materials: Vec<LabMaterial>,
    meshes: Vec<LabMesh>,
) -> LabScene {
    LabScene {
        width,
        height,
        eye,
        target,
        fov_y,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post(ev)),
        directional,
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

// --- Capture ---------------------------------------------------------------------------------

/// The last of `frames` frames: linear HDR and the display pixels (RGBA8).
fn capture(renderer: &mut FrameRenderer, frame: &FrameScene<'_>, frames: u32) -> (Vec<[f32; 3]>, Vec<u8>) {
    let mut pixels = Vec::new();
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        pixels = renderer
            .render_offscreen(device(), frame)
            .expect("real-time frame")
            .pixels;
    }
    let hdr = renderer
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the post stack ran, so the HDR copy exists");
    (hdr.rgb, pixels)
}

/// The outdoor parameters of every row but the sky's: no air, so the sun and the authored lights
/// are the only light.
fn airless() -> OutdoorParams {
    OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    }
}

fn raster(lab: &LabScene, quality: FrameQuality, frames: u32) -> (Vec<[f32; 3]>, Vec<u8>) {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(airless());
    capture(&mut renderer, &lab.as_frame_scene_with_quality(quality), frames)
}

fn trace(frame: &FrameScene<'_>, options: &ExtractOptions, spp: u32, bounces: u32) -> PtImage {
    let pt = from_frame(frame, options);
    tracer()
        .render(
            &pt,
            &PtSettings {
                spp,
                max_bounces: bounces,
                seed: 27,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference")
}

fn sun_options() -> ExtractOptions {
    ExtractOptions {
        sky: Some(PtSky::None),
        sun_angular_radius: SUN_RADIUS,
        ..ExtractOptions::default()
    }
}

/// `num / den` per pixel on luminance, clamped to `[0, 1.5]`; `fallback` where `den` is dark.
fn ratio(num: &[[f32; 3]], den: &[[f32; 3]], fallback: f32) -> Vec<f32> {
    num.iter()
        .zip(den)
        .map(|(n, d)| {
            let d = luminance(*d);
            if d > 1e-4 {
                (luminance(*n) / d).clamp(0.0, 1.5)
            } else {
                fallback
            }
        })
        .collect()
}

/// Mean FLIP of two visibility images (shown as grey, `[0, 1]`) over `mask`; outside it both
/// read the reference.
fn visibility_flip(reference: &[f32], test: &[f32], mask: &[bool], width: u32, height: u32) -> (f32, f32) {
    let r: Vec<[f32; 3]> = reference.iter().map(|v| [v.clamp(0.0, 1.0); 3]).collect();
    let t: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| [if *m { *t } else { *r }.clamp(0.0, 1.0); 3])
        .collect();
    let f = flip(&r, &t, width, height, DEFAULT_PPD);
    masked_stats(&f.map, mask)
}

/// Mean and 95th percentile of `map` over `mask`.
fn masked_stats(map: &[f32], mask: &[bool]) -> (f32, f32) {
    let v: Vec<f32> = map
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .map(|(v, _)| *v)
        .collect();
    let mean = v.iter().map(|x| f64::from(*x)).sum::<f64>() / v.len().max(1) as f64;
    (mean as f32, quantile(&v, 0.95))
}

/// Mean FLIP of linear `test` against linear `reference` over `mask`, both through the runtime's
/// display transform at `ev`.
fn linear_flip(
    reference: &[[f32; 3]],
    test: &[[f32; 3]],
    mask: &[bool],
    width: u32,
    height: u32,
    ev: f32,
) -> (f32, f32) {
    let blended: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let f = flip(
        &display_encode(reference, ev),
        &display_encode(&blended, ev),
        width,
        height,
        DEFAULT_PPD,
    );
    masked_stats(&f.map, mask)
}

/// Pixels whose 4-neighbourhood all hit a surface (away from silhouettes).
fn interior(pt: &PtImage, keep: impl Fn(usize) -> bool) -> Vec<bool> {
    let (w, h) = (pt.width as usize, pt.height as usize);
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            x > 0
                && y > 0
                && x + 1 < w
                && y + 1 < h
                && [i, i - 1, i + 1, i - w, i + w].iter().all(|&j| pt.depth[j] > 0.0)
                && keep(i)
        })
        .collect()
}

/// With `$REGIA_SPEC27_OUT` set, writes `plates/<name>.png`: a visibility image as grey.
fn plate_grey(name: &str, width: u32, height: u32, values: &[f32]) {
    let rgba: Vec<u8> = values
        .iter()
        .flat_map(|v| {
            let b = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            [b, b, b, 255]
        })
        .collect();
    plate_rgba(name, width, height, &rgba);
}

/// With `$REGIA_SPEC27_OUT` set, writes `plates/<name>.png`: linear radiance through the
/// display transform at `ev`.
fn plate_linear(name: &str, width: u32, height: u32, linear: &[[f32; 3]], ev: f32) {
    let rgba: Vec<u8> = display_encode(linear, ev)
        .iter()
        .flat_map(|c| {
            let e = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            [e(c[0]), e(c[1]), e(c[2]), 255]
        })
        .collect();
    plate_rgba(name, width, height, &rgba);
}

fn plate_rgba(name: &str, width: u32, height: u32, rgba: &[u8]) {
    let Some(dir) = spec27::out_dir() else {
        return;
    };
    let dir = dir.join("plates");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("spec27 plates: {e}");
        return;
    }
    if let Err(e) = regia_pt::output::write_png(&dir.join(format!("{name}.png")), width, height, rgba) {
        eprintln!("spec27 plate {name}: {e}");
    }
}

fn count(mask: &[bool]) -> usize {
    mask.iter().filter(|m| **m).count()
}

// --- Exposure / tonemap ----------------------------------------------------------------------

const CARD: f32 = 0.18;

/// The ColorChecker's 24 patches (6 × 4, 0.2 m) above a mid-grey card, facing the camera, lit by
/// the sun alone (no sky: the row scores the exposure and the display transform, not ambient).
fn chart() -> LabScene {
    let mut materials = vec![grey(CARD)];
    let mut meshes = vec![quad(Vec3::new(0.0, -0.62, 0.0), Vec3::X * 0.5, Vec3::Y * 0.16, 0)];
    for (i, (_, srgb)) in COLORCHECKER_SRGB.iter().enumerate() {
        let c = colorchecker_linear(*srgb);
        materials.push(LabMaterial {
            base_color: [c[0], c[1], c[2], 1.0],
            roughness: 1.0,
            metallic: 0.0,
            ..LabMaterial::default()
        });
        let (col, r) = ((i % 6) as f32, (i / 6) as f32);
        let centre = Vec3::new((col - 2.5) * 0.23, 0.35 - r * 0.23, 0.0);
        meshes.push(quad(centre, Vec3::X * 0.1, Vec3::Y * 0.1, i + 1));
    }
    let mut scene = lab(
        (384, 384),
        ([0.0, 0.0, 3.2], [0.0, -0.05, 0.0], 0.62),
        0.0,
        DirectionalLightUniform::new([0.25, -0.45, -0.86], [1.0; 3], SUN_LUX),
        materials,
        meshes,
    );
    scene.directional_shadows = false;
    // Mid grey lands at the display's mid grey: L = 0.18 E n·l / π and exposure 1 / (1.2 · 2^EV).
    let to_sun = -Vec3::from_array(scene.directional.direction).normalize();
    let card_l = CARD * SUN_LUX * to_sun.z.max(0.0) / std::f32::consts::PI;
    let ev = (card_l / (0.18 * 1.2)).log2();
    scene.exposure_ev = ev;
    scene.post = Some(post(ev));
    scene
}

fn albedo_mask(pt: &PtImage, albedo: [f32; 3]) -> Vec<bool> {
    interior(pt, |i| {
        pt.albedo[i].iter().zip(albedo).all(|(a, b)| (a - b).abs() < 2e-3)
    })
}

fn mean_rgb<T: Copy>(values: &[T], mask: &[bool], f: impl Fn(T) -> [f32; 3]) -> [f32; 3] {
    let (mut s, mut n) = ([0.0f64; 3], 0usize);
    for (v, _) in values.iter().zip(mask).filter(|(_, m)| **m) {
        let c = f(*v);
        for (acc, v) in s.iter_mut().zip(c) {
            *acc += f64::from(v);
        }
        n += 1;
    }
    s.map(|v| (v / n.max(1) as f64) as f32)
}

fn score_exposure() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let scene = chart();
    let ev = scene.exposure_ev;
    let quality = FrameQuality {
        ssao: SsaoMode::Off,
        ..desktop_high()
    };
    let (_, pixels) = raster(&scene, quality, 2);
    let frame = scene.as_frame_scene_with_quality(quality);
    let spp = 4;
    let pt = trace(&frame, &sun_options(), spp, 1);
    let display = srgb8_to_unit(&pixels);

    let card = albedo_mask(&pt, [CARD; 3]);
    assert!(count(&card) > 500, "card pixels: {}", count(&card));
    let shown = mean_rgb(&display, &card, |c| c);
    let pt_card = mean_rgb(&pt.beauty, &card, |c| c);
    let target = f64::from(shown[1]);
    let delta = ev_offset(target, |d| {
        let s = 2f32.powf(d as f32);
        f64::from(display_encode(&[pt_card.map(|c| c * s)], ev)[0][1])
    });
    rows.push(row(
        "exposure_mid_grey",
        "Exposure / tonemap",
        "mid grey within ±0.1 EV of PT",
        format!(
            "{delta:+.3} EV (display {:.4} vs PT through AgX {:.4})",
            shown[1],
            { display_encode(&[pt_card], ev)[0][1] }
        ),
        Verdict::of(delta.abs() <= 0.1),
        ("chart (0.18 card, sun only)", spp),
        (
            miss_over(delta.abs(), 0.1),
            "frame's display pixels against the tracer's beauty through compare::display_encode at the same EV",
        ),
    ));

    let mut worst = (0.0f64, "");
    let mut sum = 0.0f64;
    for (name, srgb) in COLORCHECKER_SRGB {
        let mask = albedo_mask(&pt, colorchecker_linear(srgb));
        assert!(count(&mask) > 100, "{name}: {} pixels", count(&mask));
        let shown = mean_rgb(&display, &mask, |c| c);
        let pt_shown = display_encode(&[mean_rgb(&pt.beauty, &mask, |c| c)], ev)[0];
        let de = ciede2000(srgb_to_lab(shown), srgb_to_lab(pt_shown));
        sum += de;
        if de > worst.0 {
            worst = (de, name);
        }
    }
    let mean = sum / COLORCHECKER_SRGB.len() as f64;
    rows.push(row(
        "exposure_colorchecker",
        "Exposure / tonemap",
        "ColorChecker ΔE2000 ≤ 2 (every patch) ★",
        format!("mean {mean:.2}, worst {:.2} ({})", worst.0, worst.1),
        Verdict::of(worst.0 <= 2.0),
        ("chart (24 patches, sun only)", spp),
        (
            miss_over(worst.0, 2.0),
            "CIEDE2000 (Sharma 2005) of the displayed patch means",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_exposure() {
    score_exposure().into_iter().for_each(record);
}

// --- Direct lighting + materials -------------------------------------------------------------

/// `forward_parity`'s material grid: dielectric, metal and clearcoat rows × 5 roughness.
fn material_grid() -> LabScene {
    let (verts, idx) = regia_render::uv_sphere(1.0, 64, 32);
    let mut materials = Vec::new();
    let mut meshes = Vec::new();
    for r in 0..3 {
        for col in 0..5 {
            let roughness = [0.1, 0.3, 0.5, 0.7, 1.0][col];
            let mut m = LabMaterial {
                roughness,
                ..LabMaterial::EMPTY
            };
            match r {
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
            meshes.push(LabMesh {
                vertices: verts.clone(),
                indices: idx.clone(),
                material: materials.len(),
                model: regia_core::glam::Mat4::from_translation(Vec3::new(
                    (col as f32 - 2.0) * 2.6,
                    (1.0 - r as f32) * 2.6,
                    0.0,
                ))
                .to_cols_array_2d(),
                ..LabMesh::unskinned()
            });
            materials.push(m);
        }
    }
    let mut scene = lab(
        (320, 192),
        ([0.0, 0.0, 16.0], [0.0, 0.0, 0.0], 0.62),
        13.0,
        DirectionalLightUniform::new([-0.4, -0.5, -0.77], [1.0, 0.98, 0.95], 20_000.0),
        materials,
        meshes,
    );
    scene.directional_shadows = false;
    scene
}

fn score_direct() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let scene = material_grid();
    // Specular AA widens the forward lobe by the screen-space normal variance, a filter the
    // pixel-centre trace does not apply (as in `forward_parity`).
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    let (hdr, _) = raster(&scene, quality, 1);
    let frame = scene.as_frame_scene_with_quality(quality);
    let pt = trace(&frame, &sun_options(), 1, 1);
    let surface = interior(&pt, |i| pt.shadow[i] >= 0.5);
    let none = vec![false; surface.len()];
    let e = measure(&GiErrorInput {
        reference: &pt.direct,
        test: &hdr,
        surface: &surface,
        shadow: &none,
        width: pt.width,
        height: pt.height,
        ev100: scene.exposure_ev,
        scale: None,
    });
    // Where the error sits: the same RMSE without the highlight cores, and `forward_parity`'s
    // mean per-pixel relative error.
    let lum: Vec<f32> = pt.direct.iter().map(|c| luminance(*c)).collect();
    let masked: Vec<f32> = lum
        .iter()
        .zip(&surface)
        .filter(|(_, m)| **m)
        .map(|(v, _)| *v)
        .collect();
    let cut = quantile(&masked, 0.99);
    let core: Vec<bool> = surface.iter().zip(&lum).map(|(m, v)| *m && *v <= cut).collect();
    let trimmed = measure(&GiErrorInput {
        surface: &core,
        ..GiErrorInput {
            reference: &pt.direct,
            test: &hdr,
            surface: &surface,
            shadow: &none,
            width: pt.width,
            height: pt.height,
            ev100: scene.exposure_ev,
            scale: Some(e.mean_reference),
        }
    });
    let floor_l = 0.01 * e.mean_reference;
    let rel: Vec<f64> = (0..surface.len())
        .filter(|&i| surface[i] && lum[i] > floor_l)
        .map(|i| f64::from((luminance(hdr[i]) - lum[i]).abs() / lum[i]))
        .collect();
    let mean_rel = rel.iter().sum::<f64>() / rel.len().max(1) as f64;
    let note = "lab forward (LAB quality, specular AA off) against the tracer's direct AOV, pixel centres, interior sphere pixels";
    rows.push(row(
        "direct_rel_rmse",
        "Direct lighting + materials",
        "mean rel. RMSE ≤ 1 %",
        format!(
            "{:.3} % ({} px); without the brightest 1 % of reference pixels {:.3} %; mean per-pixel relative error {:.3} %",
            100.0 * e.rel_rmse,
            e.pixels,
            100.0 * trimmed.rel_rmse,
            100.0 * mean_rel
        ),
        Verdict::of(e.rel_rmse <= 0.01),
        ("material_grid", 1),
        (miss_over(f64::from(e.rel_rmse), 0.01), note),
    ));
    rows.push(row(
        "direct_flip",
        "Direct lighting + materials",
        "FLIP ≤ 0.03 ★",
        format!("mean {:.4}", e.flip_mean),
        Verdict::of(e.flip_mean <= 0.03),
        ("material_grid", 1),
        (miss_over(f64::from(e.flip_mean), 0.03), note),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_direct() {
    score_direct().into_iter().for_each(record);
}

// --- Sun shadows -----------------------------------------------------------------------------

/// A 30 m wall (the x = 0 plane from z = −12 to 0) whose vertical edge at z = 0 casts a shadow
/// line along +X that widens with the distance from contact, and a 1 m cube on open ground for
/// the contact gap, under a 45° sun travelling toward +X.
fn sun_scene(size: (u32, u32), eye: [f32; 3], target: [f32; 3], fov_y: f32) -> LabScene {
    let mut materials = vec![grey(0.5), grey(0.5)];
    materials[1].double_sided = true;
    let mut meshes = vec![
        ground(60.0, 0),
        quad(Vec3::new(0.0, 15.0, -6.0), Vec3::Z * 6.0, Vec3::Y * 15.0, 1),
    ];
    meshes.extend(cuboid(Vec3::new(2.0, 0.0, 4.0), Vec3::new(3.0, 1.0, 5.0), 0));
    lab(
        size,
        (eye, target, fov_y),
        SUN_EV,
        DirectionalLightUniform::new(sun_dir(SUN_ELEVATION_DEG), [1.0; 3], SUN_LUX),
        materials,
        meshes,
    )
}

/// The raster's sun visibility (frame over the frame without sun shadows) at Desktop High.
fn sun_visibility(scene: &LabScene, quality: FrameQuality) -> Vec<f32> {
    let (lit, _) = raster(scene, quality, 3);
    let mut open = scene.clone();
    open.directional_shadows = false;
    let (unshadowed, _) = raster(&open, quality, 3);
    ratio(&lit, &unshadowed, 1.0)
}

/// 10–90 % width (pixels) of the shadow edge in a top-down window: the visibility averaged over
/// a strip of rows (or columns) across the image centre, read across the edge. Picks the axis
/// where the tracer's profile has the larger range.
fn penumbra_px(pt_vis: &[f32], rt_vis: &[f32], w: usize, h: usize) -> (Option<f32>, Option<f32>) {
    let strip = (h / 20).max(2);
    let profile = |vis: &[f32], along_x: bool| -> Vec<f32> {
        if along_x {
            (0..w)
                .map(|x| {
                    let s: f32 = (h / 2 - strip..h / 2 + strip).map(|y| vis[y * w + x]).sum();
                    s / (2 * strip) as f32
                })
                .collect()
        } else {
            (0..h)
                .map(|y| {
                    let s: f32 = (w / 2 - strip..w / 2 + strip).map(|x| vis[y * w + x]).sum();
                    s / (2 * strip) as f32
                })
                .collect()
        }
    };
    let range =
        |p: &[f32]| p.iter().copied().fold(f32::MIN, f32::max) - p.iter().copied().fold(f32::MAX, f32::min);
    let along_x = range(&profile(pt_vis, true)) > range(&profile(pt_vis, false));
    (
        edge_width(&profile(pt_vis, along_x), 0.1, 0.9),
        edge_width(&profile(rt_vis, along_x), 0.1, 0.9),
    )
}

fn score_sun_shadows() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let quality = desktop_high();
    let e = SUN_ELEVATION_DEG.to_radians();

    // FLIP of the visibility over the lit surfaces of an overview.
    let spp = 64;
    let overview = sun_scene(
        (384, 216),
        [16.0, 10.0, 14.0],
        [3.0, 0.0, -1.0],
        50f32.to_radians(),
    );
    let rt = sun_visibility(&overview, quality);
    let pt = trace(
        &overview.as_frame_scene_with_quality(quality),
        &sun_options(),
        spp,
        1,
    );
    let to_sun = -Vec3::from_array(sun_dir(SUN_ELEVATION_DEG));
    let lit_pt = interior(&pt, |i| Vec3::from_array(pt.normal[i]).dot(to_sun) > 0.05);
    let (mean, p95) = visibility_flip(&pt.shadow, &rt, &lit_pt, pt.width, pt.height);
    plate_grey("sun_overview_raster", pt.width, pt.height, &rt);
    plate_grey("sun_overview_pt", pt.width, pt.height, &pt.shadow);
    rows.push(row(
        "sun_shadow_flip",
        "Sun shadows",
        "FLIP ≤ 0.05",
        format!("mean {mean:.4}, p95 {p95:.3} ({} px)", count(&lit_pt)),
        Verdict::of(mean <= 0.05),
        ("sun wall + cube (overview)", spp),
        (
            miss_over(f64::from(mean), 0.05),
            "visibility as grey: frame / frame without sun shadows (Desktop High, contact shadows on) vs the shadow AOV, disc sun r = 0.004675",
        ),
    ));

    // Penumbra across the wall edge's shadow line at 1, 5 and 20 m from contact (distance along
    // the sun ray from the edge to the ground), top-down from 4 m with a 20° lens.
    let (n, height, fov) = (512u32, 4.0f32, 20f32.to_radians());
    let pitch_m = 2.0 * height * (0.5 * fov).tan() / n as f32;
    for (id, d) in [
        ("sun_penumbra_1m", 1.0f32),
        ("sun_penumbra_5m", 5.0),
        ("sun_penumbra_20m", 20.0),
    ] {
        let p = [d * e.cos(), 0.0, 0.0];
        let scene = sun_scene((n, n), [p[0], height, 0.08], p, fov);
        let spp = 128;
        let rt = sun_visibility(&scene, quality);
        let pt = trace(
            &scene.as_frame_scene_with_quality(quality),
            &sun_options(),
            spp,
            1,
        );
        let (pt_w, rt_w) = penumbra_px(&pt.shadow, &rt, n as usize, n as usize);
        plate_grey(&format!("{id}_raster"), n, n, &rt);
        plate_grey(&format!("{id}_pt"), n, n, &pt.shadow);
        let analytic_mm = 2.0 * SUN_RADIUS * d * 1000.0;
        let (measured, verdict, miss) = match (pt_w, rt_w) {
            (Some(p), Some(r)) => {
                let k = f64::from(r / p);
                (
                    format!(
                        "raster {:.1} mm / PT {:.1} mm = {k:.2}× (disc diameter {analytic_mm:.1} mm)",
                        r * pitch_m * 1000.0,
                        p * pitch_m * 1000.0
                    ),
                    Verdict::of((0.8..=1.2).contains(&k)),
                    (!(0.8..=1.2).contains(&k)).then(|| (k / 1.2).max(0.8 / k)),
                )
            }
            (p, r) => (
                format!("edge not resolved in the window (PT {p:?} px, raster {r:?} px)"),
                Verdict::Fail,
                Some(f64::INFINITY),
            ),
        };
        rows.push(row(
            id,
            "Sun shadows",
            &format!("penumbra width within 20 % of PT at {d} m from contact ★"),
            measured,
            verdict,
            ("sun wall (top-down window, 2.7 mm px)", spp),
            (
                miss,
                "10–90 % width of the strip-averaged visibility across the edge's shadow line; Desktop High PCSS",
            ),
        ));
    }

    // Contact gap at 1440p: lit raster pixels between the cube's base and its shadow, where the
    // tracer has shadow. The tracer's path state for a whole 2560×1440 frame exceeds a storage
    // binding, so it traces the frame's centred 512×288 window: the same eye and target with the
    // field of view narrowed to that window, which maps pixel for pixel onto the frame's centre.
    let (fw, fh) = (2560u32, 1440u32);
    let (cw, ch) = (512u32, 288u32);
    let (eye, target, fov) = ([6.0, 1.6, 4.5], [3.0, 0.05, 4.5], 60f32.to_radians());
    let scene = sun_scene((fw, fh), eye, target, fov);
    let rt_full = sun_visibility(&scene, quality);
    let crop_fov = 2.0 * ((0.5 * fov).tan() * ch as f32 / fh as f32).atan();
    let window = sun_scene((cw, ch), eye, target, crop_fov);
    let spp = 16;
    let pt = trace(
        &window.as_frame_scene_with_quality(quality),
        &sun_options(),
        spp,
        1,
    );
    let (w, h) = (cw as usize, ch as usize);
    let (ox, oy) = (((fw - cw) / 2) as usize, ((fh - ch) / 2) as usize);
    let rt: Vec<f32> = (0..w * h)
        .map(|i| rt_full[(oy + i / w) * fw as usize + ox + i % w])
        .collect();
    plate_grey("sun_contact_raster", cw, ch, &rt);
    plate_grey("sun_contact_pt", cw, ch, &pt.shadow);
    let face = |i: usize| pt.depth[i] > 0.0 && pt.normal[i][0] > 0.9;
    let floor = |i: usize| pt.depth[i] > 0.0 && pt.normal[i][1] > 0.9;
    let mut gaps = Vec::new();
    for x in 0..w {
        let Some(yf) = (1..h - 4).rev().find(|&y| face(y * w + x)) else {
            continue;
        };
        let below = (yf + 1) * w + x;
        if !floor(below) || pt.shadow[below + 2 * w] >= 0.5 {
            continue;
        }
        let mut k = 0;
        while yf + 1 + k < h
            && floor((yf + 1 + k) * w + x)
            && pt.shadow[(yf + 1 + k) * w + x] < 0.5
            && rt[(yf + 1 + k) * w + x] > 0.5
        {
            k += 1;
        }
        gaps.push(k as f32);
    }
    assert!(gaps.len() > 100, "contact columns: {}", gaps.len());
    let (p50, p95, max) = (quantile(&gaps, 0.5), quantile(&gaps, 0.95), quantile(&gaps, 1.0));
    rows.push(row(
        "sun_contact_gap",
        "Sun shadows",
        "contact gap ≤ 1 px at 1440p ★",
        format!(
            "p50 {p50:.0}, p95 {p95:.0}, max {max:.0} px over {} columns",
            gaps.len()
        ),
        Verdict::of(p95 <= 1.0),
        ("sun cube (2560×1440 frame, centred 512×288 window, 60°, 3.5 m)", spp),
        (
            miss_over(f64::from(p95), 1.0),
            "per column below the cube's sun-facing-away face: raster-lit ground pixels from the base where the tracer has shadow; verdict on p95",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_sun_shadows() {
    score_sun_shadows().into_iter().for_each(record);
}

// --- Local-light shadows ---------------------------------------------------------------------

fn score_local_shadows() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let mut base = regia_test_utils::scenes::light_shadows_scene();
    base.width = 384;
    base.height = 384;
    base.ambient_lux = 0.0;
    base.directional = DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0);
    base.directional_shadows = false;
    base.post = Some(post(base.exposure_ev));
    let quality = desktop_high();
    let spp = 16;
    let mut present = Vec::new();
    let mut misses = Vec::new();
    for light in base.local_lights.clone() {
        let (id, name) = match light {
            LabLocalLight::Point(_) => ("local_shadow_flip_point", "point"),
            LabLocalLight::Spot(_) => ("local_shadow_flip_spot", "spot"),
        };
        let mut one = base.clone();
        one.local_lights = vec![light];
        let mut unshadowed = one.clone();
        match &mut unshadowed.local_lights[0] {
            LabLocalLight::Point(p) => p.cast_shadows = false,
            LabLocalLight::Spot(s) => s.cast_shadows = false,
        }
        let (lit, _) = raster(&one, quality, 3);
        let (open, _) = raster(&unshadowed, quality, 3);
        let rt = ratio(&lit, &open, 1.0);
        // The tracer's light visibility: direct light with the sphere over direct light without it.
        let mut no_caster = one.clone();
        no_caster.meshes.truncate(1);
        let pt = trace(&one.as_frame_scene_with_quality(quality), &sun_options(), spp, 1);
        let pt_open = trace(
            &no_caster.as_frame_scene_with_quality(quality),
            &sun_options(),
            spp,
            1,
        );
        let floor = interior(&pt, |i| {
            pt.normal[i][1] > 0.9
                && (pt.depth[i] - pt_open.depth[i]).abs() < 1e-3
                && luminance(pt_open.direct[i]) > 1e-3
        });
        let pt_vis = ratio(&pt.direct, &pt_open.direct, 1.0);
        let (mean, p95) = visibility_flip(&pt_vis, &rt, &floor, pt.width, pt.height);
        plate_grey(&format!("local_{name}_raster"), pt.width, pt.height, &rt);
        plate_grey(&format!("local_{name}_pt"), pt.width, pt.height, &pt_vis);
        plate_linear(
            &format!("local_{name}_raster_beauty"),
            pt.width,
            pt.height,
            &lit,
            one.exposure_ev,
        );
        let pt_shadow: Vec<usize> = (0..floor.len())
            .filter(|&i| floor[i] && pt_vis[i] < 0.5)
            .collect();
        let covered = pt_shadow.iter().filter(|&&i| rt[i] < 0.5).count();
        let share = covered as f32 / pt_shadow.len().max(1) as f32;
        present.push(format!("{name} {share:.2} of {} px", pt_shadow.len()));
        if pt_shadow.len() >= 50 && share < 0.5 {
            misses.push(name);
        }
        rows.push(row(
            id,
            "Local-light shadows",
            &format!("FLIP ≤ 0.06 ({name} light) ★"),
            format!("mean {mean:.4}, p95 {p95:.3} ({} floor px)", count(&floor)),
            Verdict::of(mean <= 0.06),
            ("light_shadows (384²)", spp),
            (
                miss_over(f64::from(mean), 0.06),
                "visibility as grey: frame / frame with the light's shadow off vs the tracer's direct with / without the sphere",
            ),
        ));
    }
    rows.push(row(
        "local_shadow_present",
        "Local-light shadows",
        "no missing shadow for any light that casts in the PT ★",
        format!(
            "raster shadow covers {} of the tracer's shadow",
            present.join(", ")
        ),
        Verdict::of(misses.is_empty()),
        ("light_shadows (384²)", spp),
        (
            (!misses.is_empty()).then_some(2.0),
            "a light misses its shadow when the raster's visibility < 0.5 covers under half of the tracer's ≥ 50 px shadow; atlas budget drops are not exercised (2 lights)",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_local_shadows() {
    score_local_shadows().into_iter().for_each(record);
}

// --- AO --------------------------------------------------------------------------------------

/// A 1 m cube (x from −1 to 0) on open ground under a uniform sky, seen straight down from 40 m
/// (a 3 m wide window, 1.2 cm pixels), so ground distance from the cube's +X face is the column
/// offset from its edge.
fn ao_scene(cube: bool) -> LabScene {
    let mut meshes = vec![ground(40.0, 0)];
    if cube {
        meshes.extend(cuboid(Vec3::new(-1.0, 0.0, -0.5), Vec3::new(0.0, 1.0, 0.5), 0));
    }
    let mut scene = lab(
        (256, 256),
        ([0.6, 40.0, 0.8], [0.6, 0.0, 0.0], 2.0 * (1.5f32 / 40.0).atan()),
        12.0,
        DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        vec![grey(0.5)],
        meshes,
    );
    scene.directional_shadows = false;
    scene.ambient_lux = 20_000.0;
    scene
}

fn score_ao() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let quality = desktop_high();
    let spp = 512;
    let (with, without) = (ao_scene(true), ao_scene(false));
    let (rt, _) = raster(&with, quality, 8);
    let (rt_open, _) = raster(&without, quality, 8);
    let pt = trace(
        &with.as_frame_scene_with_quality(quality),
        &ExtractOptions::default(),
        spp,
        4,
    );
    let pt_open = trace(
        &without.as_frame_scene_with_quality(quality),
        &ExtractOptions::default(),
        spp,
        4,
    );
    let (w, h) = (pt.width as usize, pt.height as usize);
    let pitch = 3.0 / w as f32;
    let ground_depth = pt_open.depth[(h / 2) * w + w / 2];
    // Rows that cross the cube's +X edge: the cube top is ~1 m nearer than the ground.
    // Rows that cross the cube's +X edge: the cube top is ~1 m nearer than the ground. Each row
    // gives the first ground pixel past the edge and the step away from the cube (the image may
    // be mirrored relative to world X).
    let top = |i: usize| pt.depth[i] > 0.0 && pt.depth[i] < ground_depth - 0.5;
    let is_ground = |i: usize| pt.depth[i] > 0.0 && pt.normal[i][1] > 0.9 && !top(i);
    let edges: Vec<(usize, usize, isize)> = (0..h)
        .filter_map(|y| {
            let i = |x: usize| y * w + x;
            let (edge, step) = (1..w).find_map(|x| match (top(i(x - 1)), top(i(x))) {
                (true, false) => Some((x, 1isize)),
                (false, true) => Some((x - 1, -1isize)),
                _ => None,
            })?;
            // Skip the sliver of the cube's side face the perspective shows.
            let mut x = edge as isize;
            for _ in 0..4 {
                if (0..w as isize).contains(&x) && is_ground(i(x as usize)) {
                    return Some((y, x as usize, step));
                }
                x += step;
            }
            None
        })
        .collect();
    assert!(edges.len() > 40, "rows crossing the cube edge: {}", edges.len());
    let middle = &edges[edges.len() / 4..3 * edges.len() / 4];
    let band = |img: &[[f32; 3]], open: &[[f32; 3]]| -> f64 {
        let (mut s, mut n) = (0.0f64, 0usize);
        for &(y, x0, step) in middle {
            for k in 0..w as isize {
                let x = x0 as isize + k * step;
                if (k as f32 + 0.5) * pitch > 0.10 || !(0..w as isize).contains(&x) {
                    break;
                }
                let i = y * w + x as usize;
                s += f64::from(luminance(img[i]) / luminance(open[i]).max(1e-6));
                n += 1;
            }
        }
        1.0 - s / n.max(1) as f64
    };
    let (d_rt, d_pt) = (band(&rt, &rt_open), band(&pt.beauty, &pt_open.beauty));
    plate_linear("ao_raster", pt.width, pt.height, &rt, with.exposure_ev);
    plate_linear("ao_pt", pt.width, pt.height, &pt.beauty, with.exposure_ev);
    let k = d_rt / d_pt.max(1e-6);
    rows.push(row(
        "ao_contact",
        "AO",
        "contact darkening within 15 % of PT in a 10 cm band ★",
        format!(
            "raster {:.1} % / PT {:.1} % darker = {k:.2}×",
            100.0 * d_rt,
            100.0 * d_pt
        ),
        Verdict::of((k - 1.0).abs() <= 0.15),
        ("AO cube (top-down, 1.2 cm px, uniform sky)", spp),
        (
            ((k - 1.0).abs() > 0.15).then(|| (k / 1.15).max(0.85 / k)),
            "darkening = 1 − ground with the cube / open ground, mean over the 10 cm band beside the face; Desktop High SSAO, flat sky term, no GI",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_ao() {
    score_ao().into_iter().for_each(record);
}

// --- Volumetric fog --------------------------------------------------------------------------

/// A closed black room (16 × 6 × 16 m) with a 1 m slit in its roof full of height fog: the sun
/// (60° high, toward +X) draws one shaft from the slit to the floor.
fn fog_scene() -> (LabScene, OutdoorParams) {
    let mut black = grey(0.03);
    black.double_sided = true;
    let mut meshes = vec![ground(8.0, 0)];
    let (hx, hy, hz) = (8.0f32, 3.0f32, 8.0f32);
    for (c, u, v) in [
        (Vec3::new(hx, hy, 0.0), -Vec3::Z * hz, Vec3::Y * hy),
        (Vec3::new(-hx, hy, 0.0), Vec3::Z * hz, Vec3::Y * hy),
        (Vec3::new(0.0, hy, hz), Vec3::X * hx, Vec3::Y * hy),
        (Vec3::new(0.0, hy, -hz), -Vec3::X * hx, Vec3::Y * hy),
        // The roof in two plates around the slit x ∈ [−0.5, 0.5].
        (Vec3::new(-4.25, 2.0 * hy, 0.0), Vec3::X * 3.75, Vec3::Z * hz),
        (Vec3::new(4.25, 2.0 * hy, 0.0), Vec3::X * 3.75, Vec3::Z * hz),
    ] {
        meshes.push(quad(c, u, v, 0));
    }
    let mut scene = lab(
        (256, 160),
        ([-1.0, 3.0, 7.5], [1.5, 2.6, 0.0], 70f32.to_radians()),
        8.0,
        DirectionalLightUniform::new(sun_dir(60.0), [1.0; 3], SUN_LUX),
        vec![black],
        meshes,
    );
    scene.far = 200.0;
    let outdoor = OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        fog: VolumetricFogSettings {
            // Exponential height fog that thins above the roof: an unbounded uniform layer would
            // put an infinite column between the room and the sun in the tracer (the froxel fog
            // has no such column), so the falloff keeps the sun's path above the room at a
            // transmittance of about 0.98 in both.
            density: 0.08,
            height_falloff: 0.4,
            base_height: 0.0,
            albedo: [0.9; 3],
            anisotropy: 0.0,
            planet_medium: false,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    };
    (scene, outdoor)
}

fn score_fog() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let (scene, outdoor) = fog_scene();
    let quality = FrameQuality {
        volumetric_fog: true,
        physical_atmosphere: false,
        ..desktop_high()
    };
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(outdoor.clone());
    let (rt, _) = capture(&mut renderer, &frame, 12);
    let mut media = media_from_outdoor(&outdoor);
    media.layers.retain(|l| !l.aerial);
    let spp = 1024;
    let pt = trace(
        &frame,
        &ExtractOptions {
            media,
            ..sun_options()
        },
        spp,
        4,
    );
    let (w, h) = (pt.width as usize, pt.height as usize);
    let lum: Vec<f32> = pt.beauty.iter().map(|c| luminance(*c)).collect();
    let ev = (quantile(&lum, 0.95).max(1e-3) / 1.2).log2();
    let all = vec![true; w * h];
    plate_linear("fog_raster", pt.width, pt.height, &rt, ev);
    plate_linear("fog_pt", pt.width, pt.height, &pt.beauty, ev);
    let (mean, p95) = linear_flip(&pt.beauty, &rt, &all, pt.width, pt.height, ev);
    let rt_mean = rt.iter().map(|c| f64::from(luminance(*c))).sum::<f64>() / (w * h) as f64;
    let pt_mean = lum.iter().map(|v| f64::from(*v)).sum::<f64>() / (w * h) as f64;
    rows.push(row(
        "fog_flip",
        "Volumetric fog",
        "FLIP ≤ 0.10 ★",
        format!("mean {mean:.4}, p95 {p95:.3}; mean luminance raster {rt_mean:.1} / PT {pt_mean:.1} cd/m²"),
        Verdict::of(mean <= 0.10),
        (
            "fog room (slit shaft, σt 0.08/m at the floor, falloff 0.4/m, albedo 0.9)",
            spp,
        ),
        (
            miss_over(f64::from(mean), 0.10),
            "beauty of a black room (fog in-scatter dominates) through display_encode at the tracer's p95 EV; froxel fog after 12 frames vs ratio tracking",
        ),
    ));

    // Shaft position: per row, the centroid of the light above the row's median.
    let mut offsets = Vec::new();
    for y in h / 6..5 * h / 6 {
        let row_of = |img: &[[f32; 3]]| -> Vec<f32> { (0..w).map(|x| luminance(img[y * w + x])).collect() };
        let (p, r) = (row_of(&pt.beauty), row_of(&rt));
        let (pm, rm) = (quantile(&p, 0.5), quantile(&r, 0.5));
        if p.iter().copied().fold(0.0, f32::max) < 2.0 * pm.max(1e-6) {
            continue;
        }
        let peak_floor = |v: &[f32], m: f32| m + 0.5 * (v.iter().copied().fold(0.0, f32::max) - m);
        if let (Some(cp), Some(cr)) = (centroid(&p, peak_floor(&p, pm)), centroid(&r, peak_floor(&r, rm))) {
            offsets.push((cp - cr).abs());
        }
    }
    let (verdict, measured, miss) = if offsets.len() < 10 {
        (
            Verdict::Fail,
            format!("shaft found in {} rows only", offsets.len()),
            Some(f64::INFINITY),
        )
    } else {
        let (m, p) = (quantile(&offsets, 0.5), quantile(&offsets, 0.95));
        (
            Verdict::of(p <= 1.0),
            format!("|Δ| p50 {m:.2} px, p95 {p:.2} px over {} rows", offsets.len()),
            miss_over(f64::from(p), 1.0),
        )
    };
    rows.push(row(
        "fog_shaft_position",
        "Volumetric fog",
        "light-shaft position within 1 px",
        measured,
        verdict,
        ("fog room (256×160)", spp),
        (
            miss,
            "per row, centroid of the luminance above half the row's peak over its median; verdict on p95",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_fog() {
    score_fog().into_iter().for_each(record);
}

// --- Atmosphere ------------------------------------------------------------------------------

fn score_sky_bands() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let e = 30f32.to_radians();
    // The sun behind the camera (toward +Z), the camera looking level toward −Z.
    let to_sun = Vec3::new(0.0, e.sin(), e.cos());
    let planet = AtmosphereParameters::from_preset(AtmospherePreset::Earth);
    let mut scene = lab(
        (256, 256),
        ([0.0, 1.8, 0.0], [0.0, 1.8, -10.0], 90f32.to_radians()),
        12.0,
        DirectionalLightUniform::new((-to_sun).to_array(), [1.0; 3], SUN_LUX),
        vec![grey(0.5)],
        Vec::new(),
    );
    scene.far = 1000.0;
    let quality = FrameQuality {
        physical_atmosphere: true,
        ..desktop_high()
    };
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(OutdoorParams {
        planet,
        ..OutdoorParams::default()
    });
    let (rt, _) = capture(&mut renderer, &frame, 3);
    let spp = 4;
    let pt = trace(
        &frame,
        &ExtractOptions {
            sky: Some(atmosphere_sky(&planet, to_sun.to_array(), 1.8)),
            sun_angular_radius: planet.sun_angular_radius,
            ..ExtractOptions::default()
        },
        spp,
        1,
    );
    let (w, h) = (pt.width as usize, pt.height as usize);
    let bands = 8usize;
    let upper = h / 2 - 2;
    let mut worst = (0.0f64, 0usize);
    let mut ratios = Vec::new();
    for b in 0..bands {
        let (y0, y1) = (b * upper / bands, (b + 1) * upper / bands);
        let (mut r, mut p) = (0.0f64, 0.0f64);
        for y in y0..y1 {
            for x in 0..w {
                r += f64::from(luminance(rt[y * w + x]));
                p += f64::from(luminance(pt.sky[y * w + x]));
            }
        }
        let k = r / p.max(1e-9);
        ratios.push(format!("{k:.3}"));
        if (k - 1.0).abs() > worst.0 {
            worst = ((k - 1.0).abs(), b);
        }
    }
    rows.push(row(
        "atmosphere_sky_bands",
        "Atmosphere / aerial",
        "sky-view luminance within 5 % per band",
        format!(
            "worst band {} off by {:.1} %; raster / PT per band, zenith to horizon: {}",
            worst.1,
            100.0 * worst.0,
            ratios.join(", ")
        ),
        Verdict::of(worst.0 <= 0.05),
        ("planets: Earth sky, sun 30° behind the camera", spp),
        (
            miss_over(worst.0, 0.05),
            "the frame's HDR sky (sky-view LUT) against the tracer's sky AOV built from the same planet; 8 bands above the horizon. Both read the runtime's tables, so this is a plumbing check of the raster sky, not an independent transport",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_sky_bands() {
    score_sky_bands().into_iter().for_each(record);
}

// --- Mirror floor ----------------------------------------------------------------------------

fn score_mirror_floor() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let mirror = LabMaterial {
        base_color: [0.95, 0.95, 0.95, 1.0],
        roughness: 0.05,
        metallic: 1.0,
        ..LabMaterial::default()
    };
    let red = LabMaterial {
        base_color: [0.8, 0.1, 0.08, 1.0],
        roughness: 0.6,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    let (verts, idx) = regia_render::uv_sphere(0.5, 64, 32);
    let sphere = LabMesh {
        vertices: verts,
        indices: idx,
        material: 1,
        model: regia_core::glam::Mat4::from_translation(Vec3::new(0.0, 0.8, 0.0)).to_cols_array_2d(),
        ..LabMesh::unskinned()
    };
    let scene = lab(
        (256, 160),
        ([0.0, 1.4, 4.0], [0.0, 0.2, 0.0], 50f32.to_radians()),
        13.0,
        DirectionalLightUniform::new([0.3, -0.7, -0.65], [1.0; 3], 20_000.0),
        vec![mirror, red],
        vec![ground(20.0, 0), sphere],
    );
    let on = FrameQuality {
        ssr: true,
        ..desktop_high()
    };
    let (with_ssr, _) = raster(&scene, on, 24);
    let (without, _) = raster(&scene, desktop_high(), 2);
    let reflection: Vec<[f32; 3]> = with_ssr
        .iter()
        .zip(&without)
        .map(|(a, b)| std::array::from_fn(|c| (a[c] - b[c]).max(0.0)))
        .collect();
    let spp = 256;
    let pt = trace(&scene.as_frame_scene_with_quality(on), &sun_options(), spp, 4);
    let spec: Vec<f32> = pt.indirect_specular.iter().map(|c| luminance(*c)).collect();
    let peak = quantile(&spec, 0.99);
    let mask = interior(&pt, |i| pt.normal[i][1] > 0.9 && spec[i] > 0.05 * peak);
    let sum = |img: &[[f32; 3]]| -> f64 {
        img.iter()
            .zip(&mask)
            .filter(|(_, m)| **m)
            .map(|(c, _)| f64::from(luminance(*c)))
            .sum()
    };
    let k = sum(&reflection) / sum(&pt.indirect_specular).max(1e-9);
    plate_linear(
        "mirror_raster_reflection",
        pt.width,
        pt.height,
        &reflection,
        scene.exposure_ev,
    );
    plate_linear(
        "mirror_pt_indirect_specular",
        pt.width,
        pt.height,
        &pt.indirect_specular,
        scene.exposure_ev,
    );
    plate_linear(
        "mirror_raster_ssr",
        pt.width,
        pt.height,
        &with_ssr,
        scene.exposure_ev,
    );
    plate_linear(
        "mirror_pt_beauty",
        pt.width,
        pt.height,
        &pt.beauty,
        scene.exposure_ev,
    );
    let (flip_mean, _) = linear_flip(
        &pt.indirect_specular,
        &reflection,
        &mask,
        pt.width,
        pt.height,
        scene.exposure_ev,
    );
    // The tracer's reflection carries sun caustics (the sun off the mirror onto the sphere) as
    // fireflies at 256 spp, which inflate its energy; coverage (the share of its reflection
    // pixels where the frame shows at least a quarter of its luminance) is robust to them.
    let shown = (0..mask.len())
        .filter(|&i| mask[i] && luminance(reflection[i]) >= 0.25 * spec[i])
        .count();
    let coverage = shown as f64 / count(&mask).max(1) as f64;
    rows.push(row(
        "specular_mirror_floor",
        "GI (specular) / SSR / probes",
        "a mirror floor under a lit object shows the object",
        format!(
            "frame shows the reflection on {:.0} % of the tracer's {} px; energy raster / PT {k:.2}; FLIP {flip_mean:.3}",
            100.0 * coverage,
            count(&mask)
        ),
        Verdict::of(coverage >= 0.75 && count(&mask) > 200),
        ("mirror floor (metal 0.05) under a lit red sphere", spp),
        (
            miss_over(0.75, coverage),
            "frame with SSR minus frame without (24 frames, Desktop High) against the tracer's indirect_specular where it holds the reflection; 'shows' = at least a quarter of the tracer's luminance on 75 % of its reflection pixels",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_mirror_floor() {
    score_mirror_floor().into_iter().for_each(record);
}

// --- Decals ----------------------------------------------------------------------------------

/// The decal row's VFX profile: room for the one decal, nothing else simulated.
fn decal_vfx_profile() -> regia_vfx::VfxProfile {
    regia_vfx::VfxProfile {
        particle_capacity: 64,
        decal_capacity: 16,
        sort: false,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 0,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

/// Tangent-space x of the decal's authored normal (a 17.5° tilt toward the decal's right axis).
const DECAL_NORMAL_X: f32 = 0.3;
/// The decal's roughness and the plane's.
const DECAL_ROUGH: f32 = 0.25;
const DECAL_PLANE_ROUGH: f32 = 0.8;
/// "Present in the AOV": within this angle of the decal's analytic normal (twice the 8-bit
/// normal readback's worst case) and this far from its roughness (ADR-2951).
const DECAL_NORMAL_TOL_DEG: f32 = 2.0;
const DECAL_ROUGH_TOL: f32 = 0.02;

/// A 1.6 m decal at the origin of a grey plane: red paint (replace), roughness 0.25, the
/// tangent-space normal (0.3, 0) at full strength, hard edge, no fades; as the frame's
/// `regia_vfx` decal and as the tracer's [`regia_pt::PtDecal`] with the same fields.
fn decal_pair() -> (regia_vfx::Decal, regia_vfx::DecalSurface, regia_pt::PtDecal) {
    let decal = regia_vfx::Decal {
        position: Vec3::ZERO,
        rotation: regia_vfx::align_to_normal(Vec3::Y, 0.0),
        half_size: Vec3::new(0.8, 0.8, 0.3),
        color: [0.8, 0.1, 0.1, 1.0],
        age: 0.0,
        lifetime: None,
    };
    let mut surface = regia_vfx::DecalSurface::from_decal(&decal);
    surface.roughness = DECAL_ROUGH;
    surface.normal_xy = [DECAL_NORMAL_X, 0.0];
    surface.normal_strength = 1.0;
    surface.angle_fade = [0.0, 0.0];
    surface.edge_softness = 0.0;
    surface.channels = regia_vfx::CHANNEL_ALBEDO | regia_vfx::CHANNEL_NORMAL | regia_vfx::CHANNEL_ROUGHNESS;
    surface.blend = 1.0;
    surface.receiver_mask = regia_vfx::LAYER_WORLD;
    let mut pt = regia_pt::PtDecal::from_box(decal.position, decal.rotation, decal.half_size, decal.color);
    pt.roughness = surface.roughness;
    pt.normal_xy = surface.normal_xy;
    pt.normal_strength = surface.normal_strength;
    pt.angle_fade = surface.angle_fade;
    pt.edge_softness = surface.edge_softness;
    pt.channels = surface.channels;
    pt.blend = surface.blend;
    pt.receiver_mask = surface.receiver_mask;
    (decal, surface, pt)
}

/// The decal's normal on the plane: whiteout of (0.3, 0, √0.91) over (0, 0, 1) in the frame
/// t = the decal's right axis projected on the plane, b = n × t, n = +Y.
fn decal_analytic_normal(pt: &regia_pt::PtDecal) -> [f32; 3] {
    let (right, _, _) = pt.axes();
    let n = Vec3::Y;
    let t = (right - n * right.dot(n)).normalize();
    let nz = (1.0 - DECAL_NORMAL_X * DECAL_NORMAL_X).sqrt();
    (t * DECAL_NORMAL_X + n * nz).normalize().to_array()
}

fn decal_scene() -> LabScene {
    let mut scene = lab(
        (256, 192),
        ([0.0, 3.2, 2.4], [0.0, 0.0, 0.0], 0.7),
        13.0,
        DirectionalLightUniform::new(sun_dir(SUN_ELEVATION_DEG), [1.0, 1.0, 1.0], 20_000.0),
        vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: DECAL_PLANE_ROUGH,
            ..LabMaterial::EMPTY
        }],
        vec![ground(4.0, 0)],
    );
    scene.directional_shadows = false;
    scene
}

/// Pixels whose 5×5 neighbourhood all satisfy `keep`.
fn eroded_mask(w: usize, h: usize, keep: impl Fn(usize) -> bool) -> Vec<bool> {
    let ok: Vec<bool> = (0..w * h).map(&keep).collect();
    (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as i64, (i / w) as i64);
            (-2..=2).all(|dy| {
                (-2..=2).all(|dx| {
                    let (xx, yy) = (x + dx, y + dy);
                    xx >= 0
                        && yy >= 0
                        && (xx as usize) < w
                        && (yy as usize) < h
                        && ok[yy as usize * w + xx as usize]
                })
            })
        })
        .collect()
}

fn score_decals() -> Vec<Row> {
    let mut rows = Vec::new();
    let _gpu = gpu();
    let scene = decal_scene();
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    let frame = scene.as_frame_scene_with_quality(quality);
    let (decal, surface, pt_decal) = decal_pair();
    let want_n = decal_analytic_normal(&pt_decal);

    // The frame: the decal as a game draws it. With the forward-decal switch on in this process
    // (`REGIA_FORWARD_DECALS=1`, ADR-3255) it is routed like the runtime routes it: the forward
    // pass on the plane (a mesh receiver), the projected layer for whatever is left (nothing
    // here: no terrain, no emission). Off, the projected layer composites it after the forward
    // pass, as before.
    let forward_on = regia_render::process_forward_decals();
    let vfx = std::sync::Arc::new(Mutex::new(
        regia_vfx::VfxGpu::new(device(), decal_vfx_profile()).expect("vfx gpu"),
    ));
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(airless());
    renderer.set_translucent_layers(vec![regia_vfx::translucent_layer(std::sync::Arc::clone(&vfx))]);
    vfx.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .write_decals_with(&[], &[]);
    renderer.set_forward_decals(&[]);
    let (bare_hdr, _) = capture(&mut renderer, &frame, 1);
    let route = regia_vfx::route_decals(&[decal], &[surface], forward_on);
    let path = route.paths[0].as_str();
    renderer.set_forward_decals(&route.forward);
    vfx.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .write_decals_with(&route.post, &route.post_surfaces);
    let (hdr, pixels) = capture(&mut renderer, &frame, 1);
    let aov = regia_render::capture_frame_aov(&mut renderer, device(), &frame).expect("frame AOV");
    // Where the projected layer's brightening comes from (the brief's ×1.69): the footprint gain
    // with its channels added one at a time, on a renderer of its own.
    let attribution_imgs = if forward_on {
        Vec::new()
    } else {
        let mut gains = Vec::new();
        for (name, channels) in [
            ("albedo", regia_vfx::CHANNEL_ALBEDO),
            ("+normal", regia_vfx::CHANNEL_ALBEDO | regia_vfx::CHANNEL_NORMAL),
            (
                "+roughness",
                regia_vfx::CHANNEL_ALBEDO | regia_vfx::CHANNEL_NORMAL | regia_vfx::CHANNEL_ROUGHNESS,
            ),
        ] {
            let mut s = surface;
            s.channels = channels;
            let mut r = FrameRenderer::new();
            r.set_ddgi_enabled(Some(false));
            r.set_outdoor(airless());
            let layer = std::sync::Arc::new(Mutex::new(
                regia_vfx::VfxGpu::new(device(), decal_vfx_profile()).expect("vfx gpu"),
            ));
            r.set_translucent_layers(vec![regia_vfx::translucent_layer(std::sync::Arc::clone(&layer))]);
            layer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_decals_with(&[decal], &[s]);
            let (img, _) = capture(&mut r, &frame, 1);
            gains.push((name, img));
        }
        gains
    };

    // The reference: the same decal on the traced material.
    let mut pt_scene = from_frame(&frame, &sun_options());
    pt_scene.decals.push(pt_decal);
    let spp = 64;
    let pt = tracer()
        .render(
            &pt_scene,
            &PtSettings {
                spp,
                max_bounces: 1,
                seed: 27,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the decal");
    let (w, h) = (pt.width as usize, pt.height as usize);
    let inside = eroded_mask(w, h, |i| {
        pt.albedo[i][0] > 0.6 && pt.albedo[i][1] < 0.2 && pt.depth[i] > 0.0
    });
    let outside = eroded_mask(w, h, |i| {
        (pt.albedo[i][0] - 0.5).abs() < 0.02 && (pt.albedo[i][1] - 0.5).abs() < 0.02 && pt.depth[i] > 0.0
    });
    let want = vec![want_n; w * h];
    let up = vec![[0.0f32, 1.0, 0.0]; w * h];
    let rough_err = |r: &[f32], mask: &[bool], target: f32| -> f32 {
        let v: Vec<f32> = r
            .iter()
            .zip(mask)
            .filter(|(_, m)| **m)
            .map(|(x, _)| (x - target).abs())
            .collect();
        v.iter().sum::<f32>() / v.len().max(1) as f32
    };
    let (_, pt_rms, pt_px) = regia_render::frame_aov::angular_error_deg(&pt.normal, &want, &inside);
    let pt_rough = rough_err(&pt.roughness, &inside, DECAL_ROUGH);
    let frame_inside: Vec<bool> = inside.iter().zip(&aov.coverage).map(|(a, b)| *a && *b).collect();
    let frame_outside: Vec<bool> = outside.iter().zip(&aov.coverage).map(|(a, b)| *a && *b).collect();
    let (_, rt_rms, rt_px) = regia_render::frame_aov::angular_error_deg(&aov.normal, &want, &frame_inside);
    let (_, rt_rms_recv, _) = regia_render::frame_aov::angular_error_deg(&aov.normal, &up, &frame_inside);
    let rt_rough = rough_err(&aov.roughness, &frame_inside, DECAL_ROUGH);
    let rt_rough_recv = rough_err(&aov.roughness, &frame_inside, DECAL_PLANE_ROUGH);
    // The AOV path itself on the bare plane around the decal.
    let (_, rt_rms_plane, _) = regia_render::frame_aov::angular_error_deg(&aov.normal, &up, &frame_outside);
    let rt_rough_plane = rough_err(&aov.roughness, &frame_outside, DECAL_PLANE_ROUGH);
    let pt_ok = pt_px > 0 && pt_rms <= DECAL_NORMAL_TOL_DEG && pt_rough <= DECAL_ROUGH_TOL;
    let rt_ok = rt_px > 0 && rt_rms <= DECAL_NORMAL_TOL_DEG && rt_rough <= DECAL_ROUGH_TOL;
    let note = if forward_on {
        "frame: forward decal (ADR-3255): the forward pass blends the decal's normal and roughness into the surface before it is lit, so its AOV (seeing.frame_aov) reads them. tracer: PtDecal on the material inputs"
    } else {
        "frame: the projected decal is regia_vfx's layer, composited after the forward pass (it relights the lit colour from its own normal and adds a roughness-shaped lobe); the forward pass never shades with the decal's normal or roughness, so its AOV (seeing.frame_aov, the forward shader's own outputs) holds the receiver's on the footprint. tracer: PtDecal on the material inputs"
    };
    rows.push(row(
        "decal_aovs",
        "Decals",
        "decal normal/roughness present in the AOVs (not stripped) ★",
        format!(
            "frame decal path {path}; tracer: normal RMS {pt_rms:.2}° from the decal's, roughness off {pt_rough:.3} ({pt_px} px, present); frame: normal RMS {rt_rms:.2}° from the decal's ({rt_rms_recv:.2}° from the receiver's +Y), roughness off {rt_rough:.3} (from the receiver's 0.8: {rt_rough_recv:.3}) over {rt_px} px: {}; the frame AOV on the bare plane: {rt_rms_plane:.2}°, roughness off {rt_rough_plane:.3}",
            if rt_ok { "present" } else { "stripped" }
        ),
        Verdict::of(pt_ok && rt_ok),
        ("decal_plane", spp),
        (miss_over(f64::from(rt_rms), f64::from(DECAL_NORMAL_TOL_DEG)), note),
    ));
    // The decal area's beauty: the frame (decal layer on) against the trace.
    let (flip_mean, flip_p95) =
        linear_flip(&pt.beauty, &hdr, &inside, pt.width, pt.height, scene.exposure_ev);
    let lum_in = |img: &[[f32; 3]]| -> f64 {
        img.iter()
            .zip(&inside)
            .filter(|(_, m)| **m)
            .map(|(c, _)| f64::from(luminance(*c)))
            .sum::<f64>()
    };
    let decal_gain = lum_in(&hdr) / lum_in(&bare_hdr).max(1e-9);
    let attribution = attribution_imgs
        .iter()
        .map(|(name, img)| format!("{name} ×{:.3}", lum_in(img) / lum_in(&bare_hdr).max(1e-9)))
        .collect::<Vec<_>>()
        .join(", ");
    let pt_gain = lum_in(&pt.beauty) / lum_in(&hdr).max(1e-9);
    // The bare plane around the decal: how well the two renderers agree without it.
    let (base_flip, _) = linear_flip(&pt.beauty, &hdr, &outside, pt.width, pt.height, scene.exposure_ev);
    let lum_out = |img: &[[f32; 3]]| -> f64 {
        img.iter()
            .zip(&outside)
            .filter(|(_, m)| **m)
            .map(|(c, _)| f64::from(luminance(*c)))
            .sum::<f64>()
    };
    let base_ratio = lum_out(&pt.beauty) / lum_out(&hdr).max(1e-9);
    rows.push(row(
        "decal_flip",
        "Decals",
        "FLIP on the decal area ≤ 0.06 ★",
        format!(
            "frame decal path {path}; mean {flip_mean:.3}, p95 {flip_p95:.3} over {} px; the decal moved the frame's footprint luminance ×{decal_gain:.3}; trace over frame ×{pt_gain:.3} on the footprint, ×{base_ratio:.3} on the bare plane around it (FLIP {base_flip:.3} there){}",
            count(&inside),
            if attribution.is_empty() {
                String::new()
            } else {
                format!("; projected-layer image gain by channels: {attribution}")
            }
        ),
        Verdict::of(flip_mean <= 0.06),
        ("decal_plane", spp),
        (
            miss_over(f64::from(flip_mean), 0.06),
            if forward_on {
                "the frame's forward pass lights the decal's material with the BRDF, as the trace does"
            } else {
                "the frame's decal relights the composited colour (paint at shade × paint / 0.45, N·L ratio, a Phong-shaped lobe); the trace lights the decal's material with the BRDF"
            },
        ),
    ));
    plate_linear("decal_frame", pt.width, pt.height, &hdr, scene.exposure_ev);
    plate_linear("decal_trace", pt.width, pt.height, &pt.beauty, scene.exposure_ev);
    plate_rgba("decal_frame_display", pt.width, pt.height, &pixels);
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_decals() {
    score_decals().into_iter().for_each(record);
}

// --- Terrain relief --------------------------------------------------------------------------

/// The § 2.2 terrain-relief bound: normal-AOV angular RMSE.
const TERRAIN_NORMAL_RMSE_DEG: f32 = 5.0;

/// Rolling ground, 128 m square at 1 m spacing: two crossed swells (6 m, 4 m) on a 3 % tilt.
fn rolling_terrain() -> regia_terrain::TerrainSharedDraw {
    let n = 129u32;
    let world = 128.0f32;
    let (scale, offset) = (40.0f32, -20.0f32);
    let samples = (0..n * n)
        .map(|i| {
            let (row, col) = (i / n, i % n);
            let (x, z) = (col as f32 - 64.0, row as f32 - 64.0);
            let h = 6.0 * (x / 12.0).sin() * (z / 15.0).cos() + 4.0 * ((x + z) / 9.0).sin() + 0.03 * x;
            ((h - offset) / scale * 65535.0).round() as u16
        })
        .collect();
    let map = regia_terrain::TerrainMap::new(
        &regia_asset::Heightfield {
            width: n,
            height: n,
            samples,
        },
        world,
        scale,
        offset,
    )
    .expect("terrain map");
    let layers = regia_terrain::TerrainLayers {
        layers: vec![regia_terrain::LayerDesc {
            name: "ground".into(),
            albedo: [0.4, 0.35, 0.3],
            roughness: 0.9,
            ..regia_terrain::LayerDesc::ground()
        }],
        ..Default::default()
    };
    regia_terrain::TerrainSharedDraw::new(
        device(),
        map,
        &layers,
        regia_terrain::cdlod::CdlodConfig::with_grid_size(8),
        3,
    )
    .expect("terrain draw")
}

fn score_terrain_normal() -> Vec<Row> {
    use regia_render::SharedPassDraw as _;
    let _gpu = gpu();
    let (w, h) = (256u32, 192u32);
    let (eye, target, fov) = ([0.0f32, 38.0, 62.0], [0.0f32, 0.0, 0.0], 0.8f32);
    let mut draw = rolling_terrain();
    // The frame's terrain normal: the terrain shader's own shading-normal view (debug view 16)
    // from this camera, the CDLOD selection and LOD it draws with.
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(eye),
        Vec3::from_array(target),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(fov, w as f32 / h as f32, 0.1, 1000.0);
    let normal_view = draw
        .render_normal_view(device(), (proj * view).to_cols_array_2d(), eye, w, h)
        .expect("terrain normal view");
    // The view is sRGB-encoded like any colour: decode to the shader's 0.5 + 0.5·n.
    let decoded = regia_render::frame_aov::srgb8_to_linear(&normal_view.data);
    let frame_n: Vec<[f32; 3]> = decoded
        .iter()
        .map(|c| {
            let v = Vec3::new(c[0] * 2.0 - 1.0, c[1] * 2.0 - 1.0, c[2] * 2.0 - 1.0);
            v.normalize_or_zero().to_array()
        })
        .collect();
    // The tracer's displaced surface: the same heightfield, its own interpolated normal (no view
    // normal installed).
    let reference = draw.reference_heightfield().expect("the draw's heightfield");
    let (hf, mut mat, tex) = regia_pt::extract::heightfield(&reference);
    let mut scene = regia_pt::PtScene::new(regia_pt::PtCamera::pinhole(eye, target, fov, w, h));
    if let Some(t) = tex {
        scene.textures.push(t);
        mat.albedo = Some(scene.textures.len() - 1);
    }
    let m = scene.add_material(mat);
    scene.terrain = Some(regia_pt::PtHeightfield { material: m, ..hf });
    scene.sun = Some(regia_pt::PtSun {
        toward: [0.0, 1.0, 0.0],
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    let pt = tracer()
        .render(
            &scene,
            &PtSettings {
                spp: 1,
                max_bounces: 1,
                seed: 27,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace the terrain");
    let alpha: Vec<bool> = normal_view
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[3] == 255)
        .collect();
    let mask = interior(&pt, |i| alpha[i] && frame_n[i] != [0.0; 3]);
    let (mean, rms, px) = regia_render::frame_aov::angular_error_deg(&frame_n, &pt.normal, &mask);
    // How far the frame's normal sits from the surface's smooth normal where the relief detail
    // lives: the 95th percentile.
    let angles: Vec<f32> = (0..mask.len())
        .filter(|&i| mask[i])
        .map(|i| regia_render::frame_aov::angular_error_deg(&frame_n[i..=i], &pt.normal[i..=i], &[true]).0)
        .collect();
    let p95 = quantile(&angles, 0.95);
    let to_rgba = |n: &[[f32; 3]]| -> Vec<u8> {
        n.iter()
            .flat_map(|v| {
                let b = |c: f32| ((c * 0.5 + 0.5).clamp(0.0, 1.0) * 255.0).round() as u8;
                [b(v[0]), b(v[1]), b(v[2]), 255]
            })
            .collect()
    };
    plate_rgba("terrain_normal_frame", w, h, &to_rgba(&frame_n));
    plate_rgba("terrain_normal_trace", w, h, &to_rgba(&pt.normal));
    vec![row(
        "terrain_normal",
        "Terrain relief",
        "normal-AOV angular RMSE ≤ 5° vs the PT's displaced surface ★",
        format!("RMSE {rms:.2}° (mean {mean:.2}°, p95 {p95:.2}°) over {px} px"),
        Verdict::of(px > 0 && rms <= TERRAIN_NORMAL_RMSE_DEG),
        ("rolling_terrain", 1),
        (
            miss_over(f64::from(rms), f64::from(TERRAIN_NORMAL_RMSE_DEG)),
            "frame: the terrain shader's shading normal (TerrainSharedDraw::render_normal_view, debug view 16: CDLOD mesh, the normal map it builds from the heightfield, relief and detail normals); tracer: the full-resolution heightfield's interpolated normal (PtImage::normal, no view normal installed); a 129² rolling heightfield at 1 m",
        ),
    )]
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_terrain_normal() {
    score_terrain_normal().into_iter().for_each(record);
}

// --- The report ------------------------------------------------------------------------------

fn read_log(dir: &std::path::Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join("logs").join(format!("{name}.log"))).ok()
}

/// The note of the GI plate rows. They were provisional until lane/sprint-dyn-gi-remainder fixed
/// the plate layouts' coverage (its "probe deficit": the full-weight probe windows did not hold
/// what the camera sees); that lane is in this tree, so they are scored (ADR-2951).
const GI_PLATES: &str = "from gi_validation (which prints the § 2.2 target, not asserts it); the plate layouts cover the view since lane/sprint-dyn-gi-remainder (its probe-deficit fix)";

/// Rows owned by older tests, from their logs under `$REGIA_SPEC27_OUT/logs/`, and the rows no
/// lane scene measures yet.
fn harvested_rows(dir: &std::path::Path) -> Vec<Row> {
    let mut rows = Vec::new();
    let gi = read_log(dir, "gi_validation");
    for (id, scene) in [
        ("gi_diffuse_flip_trench", "trench"),
        ("gi_diffuse_flip_color_room", "color_room"),
    ] {
        let v = gi
            .as_deref()
            .and_then(|l| spec27::parse_after(l, &format!("{scene} / probes_ssgi: FLIP")));
        rows.push(match v {
            Some(v) => row(
                id,
                "GI (diffuse)",
                "mean FLIP ≤ 0.08 ★",
                format!("{v:.3} (probes + SSGI, the Desktop High default)"),
                Verdict::of(v <= 0.08),
                (scene, 1024),
                (miss_over(v, 0.08), GI_PLATES),
            ),
            None => row(
                id,
                "GI (diffuse)",
                "mean FLIP ≤ 0.08 ★",
                "no gi_validation log".into(),
                Verdict::NotMeasured,
                (scene, 0),
                (
                    None,
                    "run gi_validation with --nocapture into logs/gi_validation.log",
                ),
            ),
        });
    }
    // The worst scene's p95 (ADR-2951): gi_validation prints `<scene> / probes_ssgi: p95 FLIP`.
    let p95: Vec<(&str, f64)> = ["trench", "color_room"]
        .into_iter()
        .filter_map(|scene| {
            gi.as_deref()
                .and_then(|l| spec27::parse_after(l, &format!("{scene} / probes_ssgi: p95 FLIP")))
                .map(|v| (scene, v))
        })
        .collect();
    rows.push(match p95.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1)) {
        Some((scene, v)) => row(
            "gi_diffuse_p95",
            "GI (diffuse)",
            "p95 FLIP ≤ 0.20 ★",
            format!(
                "{v:.3} on {scene} (probes + SSGI; {})",
                p95.iter()
                    .map(|(s, v)| format!("{s} {v:.3}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Verdict::of(v <= 0.20),
            (scene, 1024),
            (miss_over(v, 0.20), GI_PLATES),
        ),
        None => row(
            "gi_diffuse_p95",
            "GI (diffuse)",
            "p95 FLIP ≤ 0.20 ★",
            "no p95 line in the gi_validation log".into(),
            Verdict::NotMeasured,
            ("trench, color_room", 0),
            (
                None,
                "run gi_validation (ADR-2951 prints `<scene> / probes_ssgi: p95 FLIP`) with --nocapture into logs/gi_validation.log",
            ),
        ),
    });
    let leak = gi
        .as_deref()
        .and_then(|l| spec27::parse_after(l, "sealed_box / probes_ssgi: leak"));
    rows.push(match leak {
        Some(v) => row(
            "gi_leak_sealed_box",
            "GI (diffuse)",
            "leak ≤ 0.5 % of the lit exterior ground ★",
            format!("{v:.3} % (probes + SSGI)"),
            if v <= 0.5 { Verdict::Pass } else { Verdict::Fail },
            ("sealed_box", 1024),
            (miss_over(v, 0.5), "from gi_validation"),
        ),
        None => row(
            "gi_leak_sealed_box",
            "GI (diffuse)",
            "leak ≤ 0.5 % ★",
            "no gi_validation log".into(),
            Verdict::NotMeasured,
            ("sealed_box", 0),
            (None, ""),
        ),
    });
    let sssr = read_log(dir, "sssr_specular")
        .as_deref()
        .and_then(|l| spec27::parse_after(l, "indirect specular FLIP vs PT: SSSR"));
    rows.push(match sssr {
        Some(v) => row(
            "specular_flip",
            "GI (specular) / SSR / probes",
            "mean FLIP ≤ 0.10 ★",
            format!("{v:.3} (stochastic SSR, metal sphere)"),
            Verdict::of(v <= 0.10),
            ("ibl_spheres layout (Earth sky)", 2048),
            (miss_over(v, 0.10), "from sssr_specular"),
        ),
        None => row(
            "specular_flip",
            "GI (specular) / SSR / probes",
            "mean FLIP ≤ 0.10 ★",
            "no sssr_specular log".into(),
            Verdict::NotMeasured,
            ("ibl_spheres layout", 0),
            (None, ""),
        ),
    });
    let aerial = read_log(dir, "aerial_parity")
        .as_deref()
        .and_then(|l| spec27::parse_after(l, "worst |table/PT - 1| ="));
    rows.push(match aerial {
        Some(v) => row("atmosphere_aerial", "Atmosphere / aerial", "aerial transmittance error ≤ 3 % ★", format!("in-scatter worst {:.1} % (transmittance not scored apart)", 100.0 * v), Verdict::Provisional, ("planets: Mars air over open ground", 0), (miss_over(v, 0.03), "from aerial_parity: the aerial table's in-scatter against the traced air; the row's transmittance has no separate metric yet")),
        None => row("atmosphere_aerial", "Atmosphere / aerial", "aerial transmittance error ≤ 3 % ★", "no aerial_parity log".into(), Verdict::NotMeasured, ("planets", 0), (None, "")),
    });
    let clouds = read_log(dir, "cloud_field")
        .as_deref()
        .and_then(spec27::test_result);
    rows.push(match clouds {
        Some((passed, failed)) => row(
            "clouds",
            "Volumetric clouds",
            "transmittance and sun map within 0.03 of the trace (cloud_field tests)",
            format!("cloud_field: {passed} passed, {failed} failed"),
            Verdict::of(failed == 0 && passed > 0),
            ("cloud_field fixtures", 0),
            (
                (failed > 0).then_some(f64::from(failed) + 1.0),
                "the row's own tests; in-scatter reported in stops, no bar (ADR-2127)",
            ),
        ),
        None => row(
            "clouds",
            "Volumetric clouds",
            "within 0.03 of the trace",
            "no cloud_field log".into(),
            Verdict::NotMeasured,
            ("cloud_field", 0),
            (None, ""),
        ),
    });
    rows
}

fn commit() -> String {
    if let Ok(c) = regia_core::brand::env_var(spec27::COMMIT_ENV) {
        return c;
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_owned(), |s| s.trim().to_owned())
}

/// Today's UTC date as `YYYY-MM-DD` (civil-from-days, Hinnant's algorithm).
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16): run after the scoring tests"]
fn spec27_write_report() {
    let Some(dir) = spec27::out_dir() else {
        panic!("set {} to the suite's output directory", spec27::OUT_ENV);
    };
    for r in harvested_rows(&dir) {
        // A missing log leaves a row carried over from an earlier page (`spec27_seed_rows`) as it
        // was, rather than replacing it with `not measured` (ADR-2951).
        let seeded = dir.join("rows").join(format!("{}.tsv", r.id)).exists();
        if r.verdict == Verdict::NotMeasured && seeded {
            continue;
        }
        record(r);
    }
    let adapter = {
        let _gpu = gpu();
        let info = device().adapter.get_info();
        format!("{} ({})", info.name, info.driver)
    };
    let rows = spec27::read_rows(&dir);
    let md = spec27::render_report(&rows, &adapter, &commit(), &today());
    regia_test_utils::write_atomic(&dir.join("report.md"), md.as_bytes()).expect("write the report");
    eprintln!("{md}");
}

/// Seeds `$REGIA_SPEC27_OUT/rows/` with the rows of an earlier evidence page
/// (`$REGIA_SPEC27_SEED`), each keeping the commit it was measured at, so a run that re-measures
/// some rows regenerates the whole page; rows already recorded are kept (ADR-2951). Run it
/// before the scoring tests.
#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16): carry an earlier page's rows over"]
fn spec27_seed_rows() {
    let Some(dir) = spec27::out_dir() else {
        panic!("set {} to the suite's output directory", spec27::OUT_ENV);
    };
    let Some(seed) = regia_core::brand::env_var_os("REGIA_SPEC27_SEED") else {
        panic!("set REGIA_SPEC27_SEED to the earlier evidence page");
    };
    let md = std::fs::read_to_string(&seed).expect("read the seed page");
    let rows = spec27::rows_from_report(&md);
    assert!(!rows.is_empty(), "no rows in {seed:?}");
    for r in rows {
        if !dir.join("rows").join(format!("{}.tsv", r.id)).exists() {
            record(r);
        }
    }
}

// --- Failing rows as named tests -------------------------------------------------------------

/// Asserts that row `id` of a scoring run meets its § 2.2 target.
fn assert_target(rows: Vec<Row>, id: &str) {
    let Some(row) = rows.into_iter().find(|r| r.id == id) else {
        panic!("the scoring run recorded no row {id}");
    };
    assert_eq!(
        row.verdict,
        Verdict::Pass,
        "{id}: measured {} against the § 2.2 target {}",
        row.measured,
        row.target
    );
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-05 (ADR-2660): rel. RMSE 11.3 % against 1 %, all of it in the brightest 1 % of pixels (0.86 % without them): the forward's highlight cores on the 0.1-rough spheres"]
fn target_direct_rel_rmse() {
    assert_target(score_direct(), "direct_rel_rmse");
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-05 (ADR-2660): sun penumbra at 1 m from contact is 2.07× the tracer's (14.1 vs 6.8 mm); 5 m (1.02×) and 20 m (0.82×) pass: the PCSS filter floor"]
fn target_sun_penumbra_1m() {
    assert_target(score_sun_shadows(), "sun_penumbra_1m");
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-05 (ADR-2660): contact darkening in the 10 cm band is 0.32× the tracer's (9.5 % vs 29.9 % darker)"]
fn target_ao_contact() {
    assert_target(score_ao(), "ao_contact");
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-05 (ADR-2660): froxel fog FLIP 0.48 against 0.10; the fog under the roof is lit where the tracer has it in the roof's shadow (1.43× the tracer's luminance)"]
fn target_fog_flip() {
    assert_target(score_fog(), "fog_flip");
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-05 (ADR-2660): the shaft's centroid is p95 22 px (p50 5 px) off the tracer's, against 1 px"]
fn target_fog_shaft_position() {
    assert_target(score_fog(), "fog_shaft_position");
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-05 (ADR-2660): screen-space reflections show the sphere's upper half only; its underside is not on screen and nothing else fills the mirror"]
fn target_specular_mirror_floor() {
    assert_target(score_mirror_floor(), "specular_mirror_floor");
}

// --- Volumetric fog under the calibrated sun-shadow rule (ADR-3122) ---------------------------

/// [`score_fog`] with the fog's sun-shadow rule set to `rule`
/// (`VolumetricFogSettings::sun_shadow`): the same room, frames, trace and metrics. Row ids and
/// plates carry the rule's name (`fog_flip_calibrated`, `fog_raster_calibrated`), so the default
/// rows stay the default rule's.
fn score_fog_rule(rule: regia_render::FogSunShadowRule) -> Vec<Row> {
    let name = rule.as_str();
    let mut rows = Vec::new();
    let _gpu = gpu();
    let (scene, mut outdoor) = fog_scene();
    outdoor.fog.sun_shadow = rule;
    let quality = FrameQuality {
        volumetric_fog: true,
        physical_atmosphere: false,
        ..desktop_high()
    };
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(outdoor.clone());
    let (rt, _) = capture(&mut renderer, &frame, 12);
    let mut media = media_from_outdoor(&outdoor);
    media.layers.retain(|l| !l.aerial);
    let spp = 1024;
    let pt = trace(
        &frame,
        &ExtractOptions {
            media,
            ..sun_options()
        },
        spp,
        4,
    );
    let (w, h) = (pt.width as usize, pt.height as usize);
    let lum: Vec<f32> = pt.beauty.iter().map(|c| luminance(*c)).collect();
    let ev = (quantile(&lum, 0.95).max(1e-3) / 1.2).log2();
    let all = vec![true; w * h];
    plate_linear(&format!("fog_raster_{name}"), pt.width, pt.height, &rt, ev);
    let (mean, p95) = linear_flip(&pt.beauty, &rt, &all, pt.width, pt.height, ev);
    let rt_mean = rt.iter().map(|c| f64::from(luminance(*c))).sum::<f64>() / (w * h) as f64;
    let pt_mean = lum.iter().map(|v| f64::from(*v)).sum::<f64>() / (w * h) as f64;
    rows.push(row(
        &format!("fog_flip_{name}"),
        "Volumetric fog",
        "FLIP ≤ 0.10 ★",
        format!("mean {mean:.4}, p95 {p95:.3}; mean luminance raster {rt_mean:.1} / PT {pt_mean:.1} cd/m²"),
        Verdict::of(mean <= 0.10),
        (
            "fog room (slit shaft, σt 0.08/m at the floor, falloff 0.4/m, albedo 0.9)",
            spp,
        ),
        (
            miss_over(f64::from(mean), 0.10),
            "fog_flip with VolumetricFogSettings::sun_shadow set to this rule (ADR-3122)",
        ),
    ));
    let mut offsets = Vec::new();
    for y in h / 6..5 * h / 6 {
        let row_of = |img: &[[f32; 3]]| -> Vec<f32> { (0..w).map(|x| luminance(img[y * w + x])).collect() };
        let (p, r) = (row_of(&pt.beauty), row_of(&rt));
        let (pm, rm) = (quantile(&p, 0.5), quantile(&r, 0.5));
        if p.iter().copied().fold(0.0, f32::max) < 2.0 * pm.max(1e-6) {
            continue;
        }
        let peak_floor = |v: &[f32], m: f32| m + 0.5 * (v.iter().copied().fold(0.0, f32::max) - m);
        if let (Some(cp), Some(cr)) = (centroid(&p, peak_floor(&p, pm)), centroid(&r, peak_floor(&r, rm))) {
            offsets.push((cp - cr).abs());
        }
    }
    let (verdict, measured, miss) = if offsets.len() < 10 {
        (
            Verdict::Fail,
            format!("shaft found in {} rows only", offsets.len()),
            Some(f64::INFINITY),
        )
    } else {
        let (m, p) = (quantile(&offsets, 0.5), quantile(&offsets, 0.95));
        (
            Verdict::of(p <= 1.0),
            format!("|Δ| p50 {m:.2} px, p95 {p:.2} px over {} rows", offsets.len()),
            miss_over(f64::from(p), 1.0),
        )
    };
    rows.push(row(
        &format!("fog_shaft_position_{name}"),
        "Volumetric fog",
        "light-shaft position within 1 px",
        measured,
        verdict,
        ("fog room (256×160)", spp),
        (
            miss,
            "fog_shaft_position with VolumetricFogSettings::sun_shadow set to this rule (ADR-3122)",
        ),
    ));
    rows
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16)"]
fn spec27_fog_calibrated() {
    score_fog_rule(regia_render::FogSunShadowRule::Calibrated)
        .into_iter()
        .for_each(record);
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16): the fog rows under the opt-in calibrated sun-shadow rule (ADR-3122)"]
fn target_fog_flip_calibrated() {
    assert_target(
        score_fog_rule(regia_render::FogSunShadowRule::Calibrated),
        "fog_flip_calibrated",
    );
}

#[test]
#[ignore = "hand-run spec 27 suite (docs/06 § 16): the fog rows under the opt-in calibrated sun-shadow rule (ADR-3122)"]
fn target_fog_shaft_position_calibrated() {
    assert_target(
        score_fog_rule(regia_render::FogSunShadowRule::Calibrated),
        "fog_shaft_position_calibrated",
    );
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-06 (ADR-2951) on the default (projected) decal path: the frame strips the decal from its AOVs: on the footprint its normal is 17.51° off the decal's (0.08° off the receiver's +Y) and its roughness 0.547 off (the receiver's 0.8 within 0.003); the projected decal relights after the forward pass. The tracer holds both (0.00°, 0.000). With forward decals (ADR-3255, default off) it passes: 0.36°, 0.001 (target_decal_rows_pass_with_forward_decals)"]
fn target_decal_aovs() {
    assert_target(score_decals(), "decal_aovs");
}

#[test]
#[ignore = "spec 27 § 2.2 fails on lavapipe 2026-11-06 (ADR-2951) on the default (projected) decal path: FLIP 0.755 (p95 0.798) on the decal area against 0.06; the bare plane around it agrees (FLIP 0.002, luminance ×1.000), but the frame's decal layer brightens the footprint ×1.69 where the trace's decal material is ×0.185 of the frame there (its normal relight divides by a flipped geometric N·L: ADR-3255 item 8). With forward decals (ADR-3255, default off) it passes: FLIP 0.003 (target_decal_rows_pass_with_forward_decals)"]
fn target_decal_flip() {
    assert_target(score_decals(), "decal_flip");
}

/// ADR-3255: both decal rows pass on the forward-decal path. The switch latches per process, so
/// the ignored targets run in a child of this binary with `REGIA_FORWARD_DECALS=1`; the default
/// path keeps its ignores until forward decals are the default.
#[test]
fn target_decal_rows_pass_with_forward_decals() {
    let exe = std::env::current_exe().expect("test binary");
    for id in ["target_decal_aovs", "target_decal_flip"] {
        let status = std::process::Command::new(&exe)
            .args(["--ignored", "--exact", id, "--test-threads=1"])
            .env("REGIA_FORWARD_DECALS", "1")
            .env_remove("REGIA_SPEC27_OUT")
            .status()
            .expect("run the child");
        assert!(status.success(), "{id} fails with forward decals on");
    }
}

/// ADR-2951: the p95 row is read from gi_validation's `p95 FLIP` lines (the worst scene wins),
/// and the mean rows still read their own lines past them. CPU only: a fixture log.
#[test]
fn harvested_gi_p95_reads_the_worst_scene() {
    let dir = regia_test_utils::unique_temp_dir("spec27_gi_p95");
    std::fs::create_dir_all(dir.join("logs")).expect("log dir");
    std::fs::write(
        dir.join("logs").join("gi_validation.log"),
        "trench / probes_ssgi: FLIP 0.494 (§ 2.2 target 0.08: not met)\n\
         trench / probes_ssgi: p95 FLIP 0.812 (§ 2.2 target 0.2: not met)\n\
         color_room / probes_ssgi: FLIP 0.385 (§ 2.2 target 0.08: not met)\n\
         color_room / probes_ssgi: p95 FLIP 0.701 (§ 2.2 target 0.2: not met)\n",
    )
    .expect("fixture log");
    let rows = harvested_rows(&dir);
    let get = |id: &str| rows.iter().find(|r| r.id == id).expect(id);
    let p95 = get("gi_diffuse_p95");
    assert_ne!(p95.verdict, Verdict::NotMeasured, "{p95:?}");
    assert!(p95.measured.starts_with("0.812 on trench"), "{p95:?}");
    assert!(p95.miss.is_some_and(|m| m > 0.0), "{p95:?}");
    assert!(get("gi_diffuse_flip_trench").measured.starts_with("0.494"));
    assert!(get("gi_diffuse_flip_color_room").measured.starts_with("0.385"));
    let _ = std::fs::remove_dir_all(&dir);
}
