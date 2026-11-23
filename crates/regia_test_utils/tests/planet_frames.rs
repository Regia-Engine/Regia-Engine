// SPDX-License-Identifier: MPL-2.0
//! Lavapipe goldens for the M15c planet presets and the volumetric cloud layer, rendered as
//! whole Desktop High frames (ground plane, a tower that casts a shadow, the LUT sky).

use regia_render::{
    AtmosphereParameters, CloudPattern, CloudSettings, DirectionalLightUniform, FrameQuality, FrameRenderer,
    GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SkyBody, SkyBodyTexture, SsaoMode, TimeOfDay, sun_position,
};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png_on, golden_path};
use std::sync::Arc;

const W: u32 = 160;
const H: u32 = 90;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip planet frames: {err}");
            None
        }
    }
}

fn quad(min: [f32; 2], max: [f32; 2], y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([min[0], y, min[1]], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([max[0], y, min[1]], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([max[0], y, max[1]], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([min[0], y, max[1]], n, [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// An axis-aligned box as six outward quads.
fn tower(center: [f32; 3], half: [f32; 3]) -> LabMesh {
    let (cx, cy, cz) = (center[0], center[1], center[2]);
    let (hx, hy, hz) = (half[0], half[1], half[2]);
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, 0.0, 1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (n, t, b) in faces {
        let base = vertices.len() as u32;
        let o = [cx + n[0] * hx, cy + n[1] * hy, cz + n[2] * hz];
        let ext = |v: [f32; 3]| [v[0] * hx, v[1] * hy, v[2] * hz];
        let (te, be) = (ext(t), ext(b));
        for (s, u) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                o[0] + te[0] * s + be[0] * u,
                o[1] + te[1] * s + be[1] * u,
                o[2] + te[2] * s + be[2] * u,
            ];
            vertices.push(LabVertex::from_pos_normal_uv(p, n, [0.0, 0.0]));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }
}

struct Shot {
    eye: [f32; 3],
    planet: AtmosphereParameters,
    time: TimeOfDay,
    target: [f32; 3],
    ev: f32,
    albedo: [f32; 3],
    clouds: CloudSettings,
    body: Option<SkyBody>,
    moons: Vec<SkyBody>,
    moon_albedo: [Option<Arc<SkyBodyTexture>>; 2],
}

fn render(device: &RenderDevice, shot: &Shot) -> Vec<u8> {
    render_with(device, shot, |_, _| {})
}

/// [`render`] with `tweak` applied to the frame quality and the scene last
/// (`planet_earth_noon_feature_dump`).
fn render_with(
    device: &RenderDevice,
    shot: &Shot,
    tweak: impl Fn(&mut FrameQuality, &mut LabScene),
) -> Vec<u8> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    // These goldens test the sky, not the upscale: render at native scale with
    // the full PCSS taps they were blessed with. Desktop High's 0.67 TAAU look is
    // covered by the lookdev goldens, and this test turns post TAA off, which
    // that scale never ships without.
    quality.render_scale_permille = 1000;
    let mut ground = LabMaterial::EMPTY;
    ground.base_color = [shot.albedo[0], shot.albedo[1], shot.albedo[2], 1.0];
    ground.roughness = 0.9;
    let (vertices, indices) = quad([-6000.0, -6000.0], [6000.0, 6000.0], 0.0);
    let mut scene = LabScene {
        width: W,
        height: H,
        eye: shot.eye,
        target: shot.target,
        fov_y: 55.0_f32.to_radians(),
        near: 0.3,
        far: 15_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: shot.ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: shot.ev,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![ground],
        meshes: vec![
            LabMesh {
                vertices,
                indices,
                material: 0,
                ..LabMesh::unskinned()
            },
            tower([0.0, 6.0, 30.0], [3.0, 6.0, 3.0]),
        ],
    };
    tweak(&mut quality, &mut scene);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(shot.time),
        clouds: shot.clouds,
        planet: shot.planet,
        sky_body: shot.body,
        moons: shot.moons.clone(),
        moon_albedo: shot.moon_albedo.clone(),
        ..OutdoorParams::default()
    });
    renderer
        .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))
        .expect("planet frame")
        .pixels
}

fn rgb(pixels: &[u8], x: u32, y: u32) -> [f32; 3] {
    let i = ((y * W + x) * 4) as usize;
    [
        f32::from(pixels[i]),
        f32::from(pixels[i + 1]),
        f32::from(pixels[i + 2]),
    ]
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Mean colour of a pixel rectangle.
fn mean_rect(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> [f32; 3] {
    let mut acc = [0.0f32; 3];
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let c = rgb(pixels, x, y);
            acc = [acc[0] + c[0], acc[1] + c[1], acc[2] + c[2]];
            n += 1.0;
        }
    }
    [acc[0] / n, acc[1] / n, acc[2] / n]
}

fn noon() -> TimeOfDay {
    TimeOfDay {
        hour: 11.0,
        day_of_year: 172,
        latitude_deg: 30.0,
    }
}

/// `REGIA_DUMP_DIR=…` writes each frame there before any assertion (look-dev aid).
fn dump(name: &str, pixels: &[u8]) {
    if let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        if let Err(err) = image::save_buffer(&path, pixels, W, H, image::ColorType::Rgba8) {
            eprintln!("dump {name}: {err}");
        }
    }
}

fn golden(device: &RenderDevice, name: &str, pixels: &[u8]) {
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        pixels,
        W,
        H,
        &golden_path(name),
        DEFAULT_SSIM_THRESHOLD,
    )
    .unwrap_or_else(|err| panic!("{name}: {err}"));
}

/// The sun's azimuth as a horizontal look target 1 km out, raised by `lift` metres.
fn toward_sun(time: &TimeOfDay, lift: f32) -> [f32; 3] {
    let d = sun_position(time).direction;
    let h = (d[0] * d[0] + d[2] * d[2]).sqrt().max(1.0e-4);
    [d[0] / h * 1000.0, 3.0 + lift, d[2] / h * 1000.0]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn planet_presets_have_their_own_sky_haze_and_ambient() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // Looking north, away from the sun, with the tower's shadow toward the camera side.
    let north = [0.0, 60.0, 1000.0];
    let earth = render(
        &device,
        &Shot {
            eye: [0.0, 3.0, 0.0],
            planet: AtmosphereParameters::earth(),
            time: noon(),
            target: north,
            ev: 14.5,
            albedo: [0.15, 0.15, 0.15],
            clouds: CloudSettings::default(),
            body: None,
            moons: Vec::new(),
            moon_albedo: [None, None],
        },
    );
    let mars = render(
        &device,
        &Shot {
            eye: [0.0, 3.0, 0.0],
            planet: AtmosphereParameters::mars(),
            time: noon(),
            target: north,
            ev: 13.8,
            albedo: [0.15, 0.15, 0.15],
            clouds: CloudSettings::default(),
            body: None,
            moons: Vec::new(),
            moon_albedo: [None, None],
        },
    );
    let airless = render(
        &device,
        &Shot {
            eye: [0.0, 3.0, 0.0],
            planet: AtmosphereParameters::airless(),
            time: noon(),
            target: north,
            ev: 12.5,
            albedo: [0.15, 0.15, 0.15],
            clouds: CloudSettings::default(),
            body: None,
            moons: Vec::new(),
            moon_albedo: [None, None],
        },
    );
    dump("planet_earth_noon", &earth);
    dump("planet_mars_noon", &mars);
    dump("planet_airless_noon", &airless);
    // Upper sky: Earth blue, Mars butterscotch (red > green > blue), airless black.
    let sky = |p: &[u8]| mean_rect(p, 0, 0, W, H / 6);
    let (e, m, a) = (sky(&earth), sky(&mars), sky(&airless));
    assert!(e[2] > e[0] + 15.0, "earth sky not blue: {e:?}");
    assert!(
        m[0] > m[1] && m[1] > m[2] && m[0] > m[2] + 25.0,
        "mars sky not butterscotch: {m:?}"
    );
    assert!(luma(m) > 25.0, "mars sky is too dark to read: {m:?}");
    assert!(luma(a) < 6.0, "airless sky is not black: {a:?}");
    // Haze, seen from 400 m up over 1.5–6 km of ground: Earth's far ground drifts toward the
    // sky colour; with no air the far ground keeps the near ground's colour.
    let high = |planet: AtmosphereParameters, ev: f32| {
        render(
            &device,
            &Shot {
                eye: [0.0, 400.0, 0.0],
                planet,
                time: noon(),
                target: [0.0, 300.0, 1000.0],
                ev,
                albedo: [0.15, 0.15, 0.15],
                clouds: CloudSettings::default(),
                body: None,
                moons: Vec::new(),
                moon_albedo: [None, None],
            },
        )
    };
    let earth_high = high(AtmosphereParameters::earth(), 14.5);
    let airless_high = high(AtmosphereParameters::airless(), 12.5);
    dump("planet_earth_high", &earth_high);
    dump("planet_airless_high", &airless_high);
    let horizon_row = (0..H)
        .rev()
        .find(|&y| luma(rgb(&airless_high, 2, y)) < 6.0)
        .unwrap_or(H / 2);
    let far = |p: &[u8]| mean_rect(p, 0, horizon_row + 2, W / 4, horizon_row + 5);
    let near = |p: &[u8]| mean_rect(p, 0, H - 6, W / 4, H);
    let blue_shift = |p: &[u8]| {
        let (f, n) = (far(p), near(p));
        f[2] / f[0].max(1.0) - n[2] / n[0].max(1.0)
    };
    assert!(
        blue_shift(&earth_high) > 0.06,
        "earth has no aerial haze: {}",
        blue_shift(&earth_high)
    );
    assert!(
        blue_shift(&airless_high).abs() < 0.03,
        "airless ground is hazed: {}",
        blue_shift(&airless_high)
    );
    // Ambient: the shadow under a black sky is far darker relative to the lit ground and is
    // not tinted blue (ground bounce only).
    let shadow_ratio = |p: &[u8]| {
        let (mut lit, mut dark) = (f32::MAX, 0.0f32);
        let mut darkest = [0.0; 3];
        for y in H / 2..H {
            for x in 0..W {
                let c = rgb(p, x, y);
                let l = luma(c);
                if l < lit {
                    lit = l;
                    darkest = c;
                }
                dark = dark.max(l);
            }
        }
        (lit / dark.max(1.0), darkest)
    };
    let (earth_ratio, _) = shadow_ratio(&earth);
    let (airless_ratio, airless_dark) = shadow_ratio(&airless);
    assert!(
        airless_ratio < earth_ratio * 0.7,
        "airless shadows {airless_ratio} should be much darker than earth's {earth_ratio}"
    );
    assert!(
        airless_dark[2] <= airless_dark[0] + 6.0,
        "airless ambient is blue: {airless_dark:?}"
    );
    golden(&device, "planet_earth_noon", &earth);
    golden(&device, "planet_mars_noon", &mars);
    golden(&device, "planet_airless_noon", &airless);
}

#[test]
fn mars_sunset_is_blue_around_the_sun() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let time = TimeOfDay {
        hour: 18.6,
        day_of_year: 172,
        latitude_deg: 20.0,
    };
    let sunset = render(
        &device,
        &Shot {
            eye: [0.0, 3.0, 0.0],
            planet: AtmosphereParameters::mars(),
            time,
            target: toward_sun(&time, 60.0),
            ev: 10.5,
            albedo: [0.4, 0.22, 0.12],
            clouds: CloudSettings::default(),
            body: None,
            moons: Vec::new(),
            moon_albedo: [None, None],
        },
    );
    dump("planet_mars_sunset", &sunset);
    // The sun sits near the horizon in the middle column; compare a ring around it with
    // the sky high above.
    let mut best = (0, 0, 0.0f32);
    for y in 0..H {
        for x in 0..W {
            let l = luma(rgb(&sunset, x, y));
            if l > best.2 {
                best = (x, y, l);
            }
        }
    }
    let (sx, sy) = (best.0, best.1);
    let ring = mean_rect(
        &sunset,
        sx.saturating_sub(14),
        sy.saturating_sub(8),
        (sx + 14).min(W),
        sy,
    );
    let high = mean_rect(&sunset, 0, 0, W, 6);
    let ratio = |c: [f32; 3]| c[2] / c[0].max(1.0);
    assert!(
        ratio(ring) > ratio(high) + 0.12,
        "no blue glow around the setting sun: ring {ring:?} high sky {high:?}"
    );
    golden(&device, "planet_mars_sunset", &sunset);
}

#[test]
fn airless_sky_body_is_lit_on_the_sun_side() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let time = TimeOfDay {
        hour: 9.0,
        day_of_year: 172,
        latitude_deg: 10.0,
    };
    // Put the body 90° from the sun in azimuth, so it is half lit (quarter phase), and
    // look straight at it.
    let sun = sun_position(&time).direction;
    let flat = (sun[0] * sun[0] + sun[2] * sun[2]).sqrt().max(1.0e-4);
    let side = [-sun[2] / flat, 0.0, sun[0] / flat];
    let body_dir = [side[0], 0.35, side[2]];
    let body = SkyBody {
        direction: body_dir,
        angular_diameter_deg: 24.0,
        ..SkyBody::jupiter_from_europa()
    };
    let europa = render(
        &device,
        &Shot {
            eye: [0.0, 3.0, 0.0],
            planet: AtmosphereParameters::airless(),
            time,
            target: [body_dir[0] * 1000.0, 3.0 + 350.0, body_dir[2] * 1000.0],
            ev: 12.5,
            albedo: [0.6, 0.6, 0.63],
            clouds: CloudSettings::default(),
            body: Some(body),
            moons: Vec::new(),
            moon_albedo: [None, None],
        },
    );
    dump("planet_europa_jupiter", &europa);
    // Sample the disc toward and away from the sun on screen. Camera basis: right is
    // `forward × up`, up is `right × forward`.
    let f = {
        let l = (body_dir[0] * body_dir[0] + body_dir[1] * body_dir[1] + body_dir[2] * body_dir[2]).sqrt();
        [body_dir[0] / l, body_dir[1] / l, body_dir[2] / l]
    };
    let r = {
        let c = [-f[2], 0.0, f[0]];
        let l = (c[0] * c[0] + c[2] * c[2]).sqrt();
        [c[0] / l, 0.0, c[2] / l]
    };
    let u = [
        r[1] * f[2] - r[2] * f[1],
        r[2] * f[0] - r[0] * f[2],
        r[0] * f[1] - r[1] * f[0],
    ];
    let sx = sun[0] * r[0] + sun[1] * r[1] + sun[2] * r[2];
    let sy = sun[0] * u[0] + sun[1] * u[1] + sun[2] * u[2];
    let len = (sx * sx + sy * sy).sqrt().max(1.0e-4);
    let (dx, dy) = (sx / len * 11.0, -sy / len * 11.0);
    let (cx, cy) = ((W / 2) as f32, (H / 2) as f32);
    let patch = |x: f32, y: f32| mean_rect(&europa, x as u32 - 3, y as u32 - 3, x as u32 + 3, y as u32 + 3);
    let lit = patch(cx + dx, cy + dy);
    let dark = patch(cx - dx, cy - dy);
    assert!(luma(lit) > 40.0, "sun side of the body is not lit: {lit:?}");
    assert!(
        luma(lit) > luma(dark) * 1.5 + 5.0,
        "no phase: lit {lit:?} dark {dark:?}"
    );
    // Banded and warm, not a flat white disc.
    assert!(lit[0] > lit[2] + 8.0, "body tint {lit:?}");
    let corner = mean_rect(&europa, 0, 0, 12, 8);
    assert!(
        luma(corner) < 12.0,
        "sky around the body is not black: {corner:?}"
    );
    golden(&device, "planet_europa_jupiter", &europa);
}

/// Two tinted moons (no albedo) are the pre-texture look: lit and phase-correct on the sun
/// side, and a white 1×1 map is bit-identical to no map (tint · 1 == tint, so the textured
/// path does not perturb the unset look).
#[test]
fn europa_moons_without_albedo_are_bit_identical_to_today() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let time = TimeOfDay {
        hour: 9.0,
        day_of_year: 172,
        latitude_deg: 10.0,
    };
    let moons = vec![
        SkyBody {
            direction: [-0.80, 0.30, -0.48],
            angular_diameter_deg: 9.0,
            ..SkyBody::io_from_europa()
        },
        SkyBody {
            direction: [-0.45, 0.42, -0.72],
            angular_diameter_deg: 12.0,
            tint: [0.62, 0.55, 0.47],
            bands: 0.0,
            fill_lux: None,
        },
    ];
    let shot = |moon_albedo: [Option<Arc<SkyBodyTexture>>; 2]| Shot {
        eye: [0.0, 3.0, 0.0],
        planet: AtmosphereParameters::airless(),
        time,
        target: [-0.62 * 1000.0, 3.0 + 320.0, -0.6 * 1000.0],
        ev: 12.5,
        albedo: [0.6, 0.6, 0.63],
        clouds: CloudSettings::default(),
        body: None,
        moons: moons.clone(),
        moon_albedo,
    };
    let unset = render(&device, &shot([None, None]));
    dump("planet_europa_moons", &unset);
    // The moons are lit (bright pixels in the sky) and the space around them is black.
    let mut sky_max = 0.0f32;
    for y in 0..H / 2 {
        for x in 0..W {
            sky_max = sky_max.max(luma(rgb(&unset, x, y)));
        }
    }
    assert!(sky_max > 60.0, "moons are not lit: {sky_max}");
    let corner = mean_rect(&unset, 0, 0, 12, 8);
    assert!(
        luma(corner) < 12.0,
        "sky around the moons is not black: {corner:?}"
    );
    // A white map samples to 1.0 in every channel, so it must be byte-identical to no map:
    // this is the in-process "unset leaves today's tint unchanged" guarantee.
    let white = Arc::new(SkyBodyTexture::new(1, 1, vec![255, 255, 255, 255]).expect("white"));
    let whited = render(&device, &shot([Some(white), None]));
    assert_eq!(
        unset, whited,
        "a white moon map must be bit-identical to no moon map"
    );
    golden(&device, "planet_europa_moons", &unset);
}

/// A textured moon samples its albedo map and keeps the sun-driven phase: the sun side of the
/// disc is brighter than the far side, and a pure-red map removes the tint's green and blue
/// (a moon that ignored its map would stay tint-coloured).
#[test]
fn a_textured_moon_shows_detail_and_correct_phase() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let time = TimeOfDay {
        hour: 9.0,
        day_of_year: 172,
        latitude_deg: 10.0,
    };
    let sun = sun_position(&time).direction;
    // A moon 90° from the sun in azimuth: half lit (quarter phase), looked at straight.
    let flat = (sun[0] * sun[0] + sun[2] * sun[2]).sqrt().max(1.0e-4);
    let side = [-sun[2] / flat, 0.0, sun[0] / flat];
    let moon_dir = [side[0], 0.35, side[2]];
    let moon = SkyBody {
        direction: moon_dir,
        angular_diameter_deg: 24.0,
        bands: 0.0,
        ..SkyBody::io_from_europa()
    };
    let shot = |moon_albedo: [Option<Arc<SkyBodyTexture>>; 2]| Shot {
        eye: [0.0, 3.0, 0.0],
        planet: AtmosphereParameters::airless(),
        time,
        target: [moon_dir[0] * 1000.0, 3.0 + 350.0, moon_dir[2] * 1000.0],
        ev: 12.5,
        albedo: [0.6, 0.6, 0.63],
        clouds: CloudSettings::default(),
        body: None,
        moons: vec![moon],
        moon_albedo,
    };
    // A pure-red map (sRGB 255,0,0) is the crispest proof the moon samples its albedo map:
    // albedo = tint · (1,0,0), so the lit disc loses the tint's green and blue entirely.
    let red = Arc::new(SkyBodyTexture::new(1, 1, vec![255, 0, 0, 255]).expect("red"));
    let textured = render(&device, &shot([Some(red), None]));
    let tinted = render(&device, &shot([None, None]));
    dump("planet_europa_moon_textured", &textured);
    dump("planet_europa_moon_tinted", &tinted);

    // Phase: the sun side of the disc is brighter than the far side (same camera basis as the
    // body test). Check the tinted moon, whose flat disc makes the mean unambiguous.
    let f = {
        let l = (moon_dir[0] * moon_dir[0] + moon_dir[1] * moon_dir[1] + moon_dir[2] * moon_dir[2]).sqrt();
        [moon_dir[0] / l, moon_dir[1] / l, moon_dir[2] / l]
    };
    let r = {
        let c = [-f[2], 0.0, f[0]];
        let l = (c[0] * c[0] + c[2] * c[2]).sqrt();
        [c[0] / l, 0.0, c[2] / l]
    };
    let u = [
        r[1] * f[2] - r[2] * f[1],
        r[2] * f[0] - r[0] * f[2],
        r[0] * f[1] - r[1] * f[0],
    ];
    let sx = sun[0] * r[0] + sun[1] * r[1] + sun[2] * r[2];
    let sy = sun[0] * u[0] + sun[1] * u[1] + sun[2] * u[2];
    let len = (sx * sx + sy * sy).sqrt().max(1.0e-4);
    let (dx, dy) = (sx / len * 9.0, -sy / len * 9.0);
    let (cx, cy) = ((W / 2) as f32, (H / 2) as f32);
    let patch = |x: f32, y: f32| mean_rect(&tinted, x as u32 - 4, y as u32 - 4, x as u32 + 4, y as u32 + 4);
    let lit = patch(cx + dx, cy + dy);
    let dark = patch(cx - dx, cy - dy);
    assert!(luma(lit) > 40.0, "sun side of the moon is not lit: {lit:?}");
    assert!(
        luma(lit) > luma(dark) * 1.5 + 5.0,
        "no phase: lit {lit:?} dark {dark:?}"
    );

    // The map is sampled: the textured lit disc is red (green and blue gone), while the
    // tint-only disc keeps the tint's green. A moon that ignored its map would match the tint.
    let t_lit = mean_rect(
        &textured,
        (cx + dx) as u32 - 4,
        (cy + dy) as u32 - 4,
        (cx + dx) as u32 + 4,
        (cy + dy) as u32 + 4,
    );
    assert!(t_lit[0] > 30.0, "textured moon is not lit: {t_lit:?}");
    assert!(
        t_lit[1] < t_lit[0] * 0.15 && t_lit[2] < t_lit[0] * 0.15,
        "red map should remove the tint's green and blue: {t_lit:?}"
    );
    assert!(
        lit[1] > 0.5 * lit[0],
        "tint-only moon should keep its green: {lit:?}"
    );

    golden(&device, "planet_europa_moon_textured", &textured);
}

/// Europa's whole sky: Jupiter drawn over two moons, tinted before and textured after. The
/// textured moons change the frame (their maps are sampled); the report's before/after images
/// dump from here (`REGIA_DUMP_DIR`).
#[test]
fn europa_sky_draws_jupiter_over_two_moons_tinted_and_textured() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let time = TimeOfDay {
        hour: 9.0,
        day_of_year: 172,
        latitude_deg: 10.0,
    };
    let moons = vec![
        SkyBody::io_from_europa(),
        SkyBody {
            direction: [-0.30, 0.10, -0.90],
            angular_diameter_deg: 5.0,
            tint: [0.55, 0.55, 0.60],
            bands: 0.0,
            fill_lux: None,
        },
    ];
    // Io gets a red map, the second moon a blue map: at a glance the after frame shows both.
    let red = Arc::new(SkyBodyTexture::new(1, 1, vec![255, 0, 0, 255]).expect("red"));
    let blue = Arc::new(SkyBodyTexture::new(1, 1, vec![0, 0, 255, 255]).expect("blue"));
    let shot = |moon_albedo: [Option<Arc<SkyBodyTexture>>; 2]| Shot {
        eye: [0.0, 3.0, 0.0],
        planet: AtmosphereParameters::airless(),
        time,
        target: [-0.62 * 1000.0, 3.0 + 350.0, -0.6 * 1000.0],
        ev: 12.5,
        albedo: [0.6, 0.6, 0.63],
        clouds: CloudSettings::default(),
        body: Some(SkyBody::jupiter_from_europa()),
        moons: moons.clone(),
        moon_albedo,
    };
    let before = render(&device, &shot([None, None]));
    let after = render(&device, &shot([Some(Arc::clone(&red)), Some(Arc::clone(&blue))]));
    dump("europa_sky_tinted", &before);
    dump("europa_sky_textured", &after);
    // The textured moons change the sky (their maps are sampled, not the tint).
    assert_ne!(before, after, "textured moons should change the sky");
    // Jupiter and the moons are lit, and the space around them is black.
    let mut sky_max = 0.0f32;
    for y in 0..H / 2 {
        for x in 0..W {
            sky_max = sky_max.max(luma(rgb(&after, x, y)));
        }
    }
    assert!(sky_max > 60.0, "Europa sky is not lit: {sky_max}");
    let corner = mean_rect(&after, 0, 0, 12, 8);
    assert!(
        luma(corner) < 12.0,
        "sky away from the bodies is not black: {corner:?}"
    );
}

/// Mean over 8×8 blocks of the luma standard deviation inside each block, on rows `y0..y1`.
fn local_variance(pixels: &[u8], y0: u32, y1: u32) -> f32 {
    let mut total = 0.0f32;
    let mut blocks = 0.0f32;
    for by in (y0..y1.saturating_sub(7)).step_by(8) {
        for bx in (0..W - 7).step_by(8) {
            let mut sum = 0.0;
            let mut sq = 0.0;
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let l = luma(rgb(pixels, x, y));
                    sum += l;
                    sq += l * l;
                }
            }
            let mean = sum / 64.0;
            total += (sq / 64.0 - mean * mean).max(0.0).sqrt();
            blocks += 1.0;
        }
    }
    total / blocks.max(1.0)
}

#[test]
fn clouds_are_clear_at_zero_coverage_and_shaped_by_density() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let up = [0.0, 3.0 + 520.0, 1000.0];
    let shot = |clouds: CloudSettings| Shot {
        eye: [0.0, 3.0, 0.0],
        planet: AtmosphereParameters::earth(),
        time: noon(),
        target: up,
        ev: 14.5,
        albedo: [0.2, 0.22, 0.14],
        clouds,
        body: None,
        moons: Vec::new(),
        moon_albedo: [None, None],
    };
    let clear = render(&device, &shot(CloudSettings::default()));
    let zero = render(&device, &shot(CloudSettings::coverage_layer(0.0)));
    assert_eq!(clear, zero, "coverage 0 must leave the clear sky untouched");
    dump("clouds_clear", &clear);
    let cloudy = render(&device, &shot(CloudSettings::coverage_layer(0.5)));
    // Same camera, same sun, the cloud field moved by the wind: if cloud brightness came
    // from the view direction the two frames would match; it comes from the density field.
    let moved = render(
        &device,
        &shot(CloudSettings {
            wind_m_per_s: [40.0, 0.0],
            time_s: 120.0,
            ..CloudSettings::coverage_layer(0.5)
        }),
    );
    dump("clouds_noon", &cloudy);
    dump("clouds_moved", &moved);
    let sky_rows = H * 2 / 3;
    let delta = {
        let mut sum = 0.0;
        for y in 0..sky_rows {
            for x in 0..W {
                sum += (luma(rgb(&cloudy, x, y)) - luma(rgb(&moved, x, y))).abs();
            }
        }
        sum / (sky_rows * W) as f32
    };
    assert!(
        delta > 4.0,
        "moving the cloud field barely changed the sky: {delta}"
    );
    let clear_var = local_variance(&clear, 0, sky_rows);
    let cloudy_var = local_variance(&cloudy, 0, sky_rows);
    assert!(
        cloudy_var > clear_var * 3.0 + 2.0,
        "clouds add no structure: local stddev clear {clear_var} cloudy {cloudy_var}"
    );
    // Cloud pixels are brighter than the clear sky they cover (sunlit tops at noon).
    let clear_mean = luma(mean_rect(&clear, 0, 0, W, sky_rows));
    let mut brightest = 0.0f32;
    for y in 0..sky_rows {
        for x in 0..W {
            brightest = brightest.max(luma(rgb(&cloudy, x, y)));
        }
    }
    assert!(
        brightest > clear_mean + 40.0,
        "no bright cloud: {brightest} vs sky {clear_mean}"
    );
    golden(&device, "clouds_noon", &cloudy);
}

fn material(color: [f32; 3]) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.base_color = [color[0], color[1], color[2], 1.0];
    mat.roughness = 0.9;
    mat
}

/// Ground plus optional extra meshes, each with its own albedo. Same exposure and
/// quality as [`render`], so the cloud goldens share one lighting setup.
fn render_layers(
    device: &RenderDevice,
    eye: [f32; 3],
    target: [f32; 3],
    clouds: CloudSettings,
    extras: &[([f32; 3], [f32; 3], [f32; 3])],
) -> Vec<u8> {
    render_cloud_frame(
        device,
        &CloudFrame {
            eye,
            target,
            clouds,
            time: noon(),
            ev: 14.5,
            cone_light: true,
            size: (W, H),
        },
        extras,
    )
}

/// What a cloud frame varies: the camera, the layer, the clock, the exposure and the
/// cloud light march (Desktop High's cone march, or the v0.1 straight one).
struct CloudFrame {
    eye: [f32; 3],
    target: [f32; 3],
    clouds: CloudSettings,
    time: TimeOfDay,
    ev: f32,
    cone_light: bool,
    /// Frame size: (W, H) for the goldens, larger for the ignored look-dev dump.
    size: (u32, u32),
}

fn render_cloud_frame(
    device: &RenderDevice,
    shot: &CloudFrame,
    extras: &[([f32; 3], [f32; 3], [f32; 3])],
) -> Vec<u8> {
    let (eye, target, clouds) = (shot.eye, shot.target, shot.clouds);
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.render_scale_permille = 1000;
    quality.cloud_cone_light = shot.cone_light;
    let mut materials = vec![material([0.2, 0.22, 0.14])];
    let (vertices, indices) = quad([-6000.0, -6000.0], [6000.0, 6000.0], 0.0);
    let mut meshes = vec![LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }];
    for (i, (center, half, albedo)) in extras.iter().enumerate() {
        materials.push(material(*albedo));
        let mut mesh = tower(*center, *half);
        mesh.material = i + 1;
        meshes.push(mesh);
    }
    let scene = LabScene {
        width: shot.size.0,
        height: shot.size.1,
        eye,
        target,
        fov_y: 55.0_f32.to_radians(),
        near: 0.3,
        far: 20_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: shot.ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: shot.ev,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(shot.time),
        clouds,
        planet: AtmosphereParameters::earth(),
        ..OutdoorParams::default()
    });
    renderer
        .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))
        .expect("cloud layer frame")
        .pixels
}

fn mean_abs_luma(a: &[u8], b: &[u8]) -> f32 {
    let mut sum = 0.0;
    let n = (W * H) as f32;
    for y in 0..H {
        for x in 0..W {
            sum += (luma(rgb(a, x, y)) - luma(rgb(b, x, y))).abs();
        }
    }
    sum / n
}

/// Horizon from 2 m: the curved shell puts a cloud bank and gaps in the sky.
///
/// The look is a few degrees above the geometric horizon. A level ray enters the
/// layer near 60 km, where the aerial fade has already washed the field out; this
/// aim still shows the ground and puts the shell entry near 15 km, wide enough for
/// several weather cells.
#[test]
fn clouds_horizon_shows_structure_along_the_shell() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let eye = [0.0, 2.0, 0.0];
    let target = [0.0, 450.0, 4000.0];
    let clear = render_layers(&device, eye, target, CloudSettings::default(), &[]);
    let cloudy = render_layers(&device, eye, target, CloudSettings::coverage_layer(0.5), &[]);
    dump("clouds_horizon", &cloudy);
    dump("clouds_horizon_clear", &clear);
    let sky = H * 2 / 3;
    let mut bank = 0u32;
    let mut gap = 0u32;
    for y in 0..sky {
        for x in 0..W {
            let d = luma(rgb(&cloudy, x, y)) - luma(rgb(&clear, x, y));
            if d > 12.0 {
                bank += 1;
            }
            if d.abs() < 6.0 {
                gap += 1;
            }
        }
    }
    let sky_n = sky * W;
    assert!(bank * 8 > sky_n, "horizon sky has no cloud bank: {bank}/{sky_n}");
    assert!(gap * 8 > sky_n, "horizon sky has no gaps: {gap}/{sky_n}");
    golden(&device, "clouds_horizon", &cloudy);
}

/// Inside the layer the frame is lit cloud, not the clear sky and not black.
#[test]
fn clouds_inside_the_layer_are_lit() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let eye = [0.0, 2500.0, 0.0];
    let target = [0.0, 2500.0, 2000.0];
    let layer = CloudSettings {
        pattern: CloudPattern::Constant,
        ..CloudSettings::coverage_layer(0.75)
    };
    let clear = render_layers(&device, eye, target, CloudSettings::default(), &[]);
    let cloudy = render_layers(&device, eye, target, layer, &[]);
    dump("clouds_inside", &cloudy);
    let delta = mean_abs_luma(&clear, &cloudy);
    let mean = luma(mean_rect(&cloudy, 0, 0, W, H));
    assert!(delta > 6.0, "inside the layer the sky did not change: {delta}");
    assert!(mean > 12.0, "inside a cloud the frame is black: mean luma {mean}");
    golden(&device, "clouds_inside", &cloudy);
}

/// Above the layer, looking down: clouds cover some of the ground and gaps leave some.
#[test]
fn clouds_above_the_layer_leave_gaps() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let eye = [0.0, 6000.0, 0.0];
    let target = [0.0, 0.0, 800.0];
    let clear = render_layers(&device, eye, target, CloudSettings::default(), &[]);
    let cloudy = render_layers(&device, eye, target, CloudSettings::coverage_layer(0.45), &[]);
    dump("clouds_above", &cloudy);
    dump("clouds_above_clear", &clear);
    // Looking down, lit tops are brighter and shadowed ground is darker, so the
    // difference is absolute. Gaps stay close to the clear frame.
    let mut cloud_px = 0u32;
    let mut gap_px = 0u32;
    let mut max_abs = 0.0f32;
    let n = W * H;
    for y in 0..H {
        for x in 0..W {
            let d = (luma(rgb(&cloudy, x, y)) - luma(rgb(&clear, x, y))).abs();
            max_abs = max_abs.max(d);
            if d > 12.0 {
                cloud_px += 1;
            }
            if d < 8.0 {
                gap_px += 1;
            }
        }
    }
    assert!(
        cloud_px * 20 > n,
        "looking down shows almost no cloud: {cloud_px}/{n} max |dluma| {max_abs}"
    );
    assert!(
        gap_px * 20 > n,
        "looking down has no gaps: {gap_px}/{n} unchanged, max |dluma| {max_abs}"
    );
    golden(&device, "clouds_above", &cloudy);
}

/// The midsummer evening hour (latitude 30°) when the sun stands `elevation_deg` above the
/// horizon, found by bisection on the solar model rather than hard-coded.
fn evening_with_sun_at(elevation_deg: f32) -> TimeOfDay {
    let at = |hour: f32| TimeOfDay {
        hour,
        day_of_year: 172,
        latitude_deg: 30.0,
    };
    let target = elevation_deg.to_radians().sin();
    // The sun sets between 15:00 and 22:00; elevation falls through the evening.
    let (mut lo, mut hi) = (15.0_f32, 22.0_f32);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if sun_position(&at(mid)).direction[1] > target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    at(0.5 * (lo + hi))
}

/// The horizontal direction away from the sun, 1 km out, raised by `lift` metres.
fn away_from_sun(time: &TimeOfDay, lift: f32) -> [f32; 3] {
    let t = toward_sun(time, lift);
    [-t[0], t[1], -t[2]]
}

/// Sunset with the sun 2° up, looking away from it: the cloud faces that face the sun take
/// the reddened sun (transmitted to the layer's own height) and read warm and brighter than
/// the shaded undersides. Looking toward the sun the camera sees the back-lit sides.
#[test]
fn clouds_at_sunset_are_warm_and_lit() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let time = evening_with_sun_at(2.0);
    let shot = |clouds: CloudSettings| CloudFrame {
        eye: [0.0, 3.0, 0.0],
        target: away_from_sun(&time, 120.0),
        clouds,
        time,
        ev: 12.0,
        cone_light: true,
        size: (W, H),
    };
    let clear = render_cloud_frame(&device, &shot(CloudSettings::default()), &[]);
    let cloudy = render_cloud_frame(&device, &shot(CloudSettings::coverage_layer(0.5)), &[]);
    dump("clouds_sunset", &cloudy);
    dump("clouds_sunset_clear", &clear);
    let sky = H * 3 / 5;
    // Cloud pixels split into warm (the sunlit faces) and the rest (shaded undersides).
    let (mut n, mut warm) = (0u32, 0u32);
    let (mut warm_luma, mut shaded_luma) = (0.0f32, 0.0f32);
    for y in 0..sky {
        for x in 0..W {
            let c = rgb(&cloudy, x, y);
            if (luma(c) - luma(rgb(&clear, x, y))).abs() <= 10.0 {
                continue;
            }
            n += 1;
            if c[0] > c[2] * 1.15 {
                warm += 1;
                warm_luma += luma(c);
            } else {
                shaded_luma += luma(c);
            }
        }
    }
    let warm_luma = warm_luma / warm.max(1) as f32;
    let shaded_luma = shaded_luma / (n - warm).max(1) as f32;
    eprintln!("sunset: {n} cloud pixels, {warm} warm at luma {warm_luma}, the rest at {shaded_luma}");
    assert!(n * 10 > sky * W, "no cloud at sunset: {n}/{} changed", sky * W);
    assert!(
        warm * 8 > n,
        "too few warm, sunlit cloud faces: {warm} of {n} cloud pixels"
    );
    assert!(
        warm_luma > shaded_luma + 15.0,
        "the warm faces are not the lit ones: luma {warm_luma} vs shaded {shaded_luma}"
    );
    golden(&device, "clouds_sunset", &cloudy);
}

/// The camera's sun from the atmosphere tables at 3 m with the sun `elevation_deg` up, lux.
fn camera_sun_lux(elevation_deg: f32) -> f32 {
    let time = evening_with_sun_at(elevation_deg);
    let luts = regia_render::build_atmosphere_luts_for(
        &AtmosphereParameters::earth(),
        sun_position(&time).direction,
        3.0,
    );
    regia_render::transmitted_sun(&luts, &luts.params).1
}

/// Just after the camera's sunset (the highest sun the tables give the 3 m camera no sun at,
/// found by bisection): the ground is unlit, but the layer top still sees the sun over the
/// dipped horizon, and far cloud rims toward the sun turn red against the twilight sky. The
/// transmittance table's horizon is soft (rows ~1.9 km apart), so the table sunset at 3 m
/// comes ~1.7° late and leaves only the upper layer lit: the rendered check is a smoke test.
#[test]
fn clouds_stay_lit_after_the_camera_sunset() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let (mut lo, mut hi) = (-4.0_f32, 0.0_f32);
    for _ in 0..12 {
        let mid = 0.5 * (lo + hi);
        if camera_sun_lux(mid) < 1.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let elevation = lo - 0.05;
    let time = evening_with_sun_at(elevation);
    let luts = regia_render::build_atmosphere_luts_for(
        &AtmosphereParameters::earth(),
        sun_position(&time).direction,
        3.0,
    );
    let camera_lux = regia_render::transmitted_sun(&luts, &luts.params).1;
    let layer = CloudSettings::coverage_layer(0.5);
    let top_lux = regia_render::transmitted_sun_at(&luts, layer.altitude_top).1;
    assert!(
        camera_lux < 1.0,
        "the camera still sees the sun: {camera_lux} lux"
    );
    assert!(
        top_lux > 1.0,
        "the layer top lost the sun at {elevation}°: {top_lux} lux"
    );
    let shot = |clouds: CloudSettings| CloudFrame {
        eye: [0.0, 3.0, 0.0],
        target: toward_sun(&time, 320.0),
        clouds,
        time,
        ev: 9.5,
        cone_light: true,
        size: (W, H),
    };
    let clear = render_cloud_frame(&device, &shot(CloudSettings::default()), &[]);
    let cloudy = render_cloud_frame(&device, &shot(layer), &[]);
    dump("clouds_after_sunset", &cloudy);
    dump("clouds_after_sunset_clear", &clear);
    // Cloud lit only by the twilight sky is bluer than the sky behind it; sunlit rims are redder.
    let sky = H * 2 / 3;
    let mut redder = 0u32;
    for y in 0..sky {
        for x in 0..W {
            let (c, b) = (rgb(&cloudy, x, y), rgb(&clear, x, y));
            if (c[0] - c[2]) - (b[0] - b[2]) > 8.0 {
                redder += 1;
            }
        }
    }
    eprintln!("after sunset (sun {elevation}°, top {top_lux} lux): {redder} pixels redder than the sky");
    assert!(
        redder > 5,
        "no sunlit (red) cloud after the camera's sunset: {redder} pixels"
    );
}

/// The cone light march, dark edges and curl change the clouds, but the layer, its
/// coverage and its brightness stay where the straight v0.1 march put them.
#[test]
fn cone_light_changes_the_clouds_but_keeps_the_layer() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let shot = |cone_light: bool| CloudFrame {
        eye: [0.0, 2.0, 0.0],
        target: [0.0, 450.0, 4000.0],
        clouds: CloudSettings::coverage_layer(0.5),
        time: noon(),
        ev: 14.5,
        cone_light,
        size: (W, H),
    };
    let on = render_cloud_frame(&device, &shot(true), &[]);
    let off = render_cloud_frame(&device, &shot(false), &[]);
    dump("clouds_cone_on", &on);
    dump("clouds_cone_off", &off);
    let delta = mean_abs_luma(&on, &off);
    let ssim = regia_test_utils::golden::ssim_rgba8(&on, &off, W, H).unwrap_or(0.0);
    assert!(delta > 0.2, "the cone light march changed nothing: {delta}");
    assert!(
        delta < 25.0 && ssim > 0.6,
        "the cone light march moved the layer: mean |dluma| {delta}, SSIM {ssim}"
    );
}

/// Look-dev: noon horizon, looking up, above the layer, and sunset, after-sunset and
/// late-dusk views toward and away from the sun at 640×360, cone light on and off, written
/// to `REGIA_DUMP_DIR`. Nothing is asserted.
#[test]
#[ignore = "640×360 cloud lighting look-dev dump"]
fn cloud_lighting_lookdev_dump() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") else {
        eprintln!("set REGIA_DUMP_DIR");
        return;
    };
    let (w, h) = (640, 360);
    let sunset = evening_with_sun_at(2.0);
    let dusk = evening_with_sun_at(-1.8);
    let late = evening_with_sun_at(-0.9);
    let later = evening_with_sun_at(-1.2);
    let views = [
        (
            "horizon",
            [0.0, 2.0, 0.0],
            [0.0, 450.0, 4000.0],
            noon(),
            14.5,
            0.5,
        ),
        ("up", [0.0, 3.0, 0.0], [0.0, 523.0, 1000.0], noon(), 14.5, 0.5),
        ("above", [0.0, 6000.0, 0.0], [0.0, 0.0, 800.0], noon(), 14.5, 0.45),
        (
            "sunset",
            [0.0, 3.0, 0.0],
            toward_sun(&sunset, 320.0),
            sunset,
            12.0,
            0.5,
        ),
        (
            "sunset_away",
            [0.0, 3.0, 0.0],
            away_from_sun(&sunset, 120.0),
            sunset,
            12.0,
            0.5,
        ),
        ("dusk", [0.0, 3.0, 0.0], toward_sun(&dusk, 320.0), dusk, 9.0, 0.5),
        (
            "dusk_away",
            [0.0, 3.0, 0.0],
            away_from_sun(&dusk, 120.0),
            dusk,
            9.0,
            0.5,
        ),
        ("late", [0.0, 3.0, 0.0], toward_sun(&late, 320.0), late, 10.0, 0.5),
        (
            "later",
            [0.0, 3.0, 0.0],
            toward_sun(&later, 320.0),
            later,
            9.5,
            0.5,
        ),
    ];
    for (name, eye, target, time, ev, coverage) in views {
        for cone_light in [true, false] {
            let pixels = render_cloud_frame(
                &device,
                &CloudFrame {
                    eye,
                    target,
                    clouds: CloudSettings::coverage_layer(coverage),
                    time,
                    ev,
                    cone_light,
                    size: (w, h),
                },
                &[],
            );
            let tag = if cone_light { "cone" } else { "nocone" };
            let path = std::path::Path::new(&dir).join(format!("look_{name}_{tag}.png"));
            if let Err(err) = image::save_buffer(&path, &pixels, w, h, image::ColorType::Rgba8) {
                eprintln!("dump {name}: {err}");
            }
        }
    }
}

fn strongly_red(c: [f32; 3]) -> bool {
    c[0] > 40.0 && c[0] > c[1] * 1.35 && c[0] > c[2] * 1.8
}

fn strongly_blue(c: [f32; 3]) -> bool {
    c[2] > 40.0 && c[2] > c[0] * 1.8 && c[2] > c[1] * 1.25
}

/// Screen bounds of an axis-aligned box, inset by one pixel.
///
/// The projection matches the frame camera (DirectX clip, Y up in NDC). At 16:9,
/// horizontal-plus leaves the authored vertical FOV unchanged. The inset drops the
/// silhouette, where sky and the box share a pixel.
fn project_box(
    eye: [f32; 3],
    target: [f32; 3],
    fov_y: f32,
    center: [f32; 3],
    half: [f32; 3],
) -> (u32, u32, u32, u32) {
    let view = glam::camera::rh::view::look_at_mat4(
        glam::Vec3::from_array(eye),
        glam::Vec3::from_array(target),
        glam::Vec3::Y,
    );
    let proj = glam::camera::rh::proj::directx::perspective(fov_y, W as f32 / H as f32, 0.3, 20_000.0);
    let vp = proj * view;
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for sx in [-1.0_f32, 1.0] {
        for sy in [-1.0_f32, 1.0] {
            for sz in [-1.0_f32, 1.0] {
                let p = glam::Vec4::new(
                    center[0] + sx * half[0],
                    center[1] + sy * half[1],
                    center[2] + sz * half[2],
                    1.0,
                );
                let c = vp * p;
                if c.w <= 0.0 {
                    continue;
                }
                let px = (c.x / c.w * 0.5 + 0.5) * W as f32;
                let py = (0.5 - c.y / c.w * 0.5) * H as f32;
                min_x = min_x.min(px);
                max_x = max_x.max(px);
                min_y = min_y.min(py);
                max_y = max_y.max(py);
            }
        }
    }
    let inset = 1.0;
    let x0 = (min_x + inset).floor().clamp(0.0, W as f32) as u32;
    let y0 = (min_y + inset).floor().clamp(0.0, H as f32) as u32;
    let x1 = (max_x - inset).ceil().clamp(0.0, W as f32) as u32;
    let y1 = (max_y - inset).ceil().clamp(0.0, H as f32) as u32;
    (x0, y0, x1.max(x0), y1.max(y0))
}

/// A near peak below the layer keeps its hue. A far peak inside the layer is covered.
#[test]
fn clouds_occlude_a_far_peak_and_leave_a_near_one() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let eye = [0.0, 8.0, 0.0];
    let target = [40.0, 500.0, 2500.0];
    // Near box sits on the ground, well under the 1500 m base. Far box is entirely
    // inside the 1500–4000 m layer, so every ray to it crosses cloud first.
    let extras = [
        ([-25.0, 16.0, 40.0], [6.0, 14.0, 6.0], [0.9, 0.05, 0.04]),
        ([0.0, 2500.0, 6000.0], [600.0, 400.0, 80.0], [0.06, 0.12, 0.9]),
    ];
    let layer = CloudSettings {
        pattern: CloudPattern::Constant,
        ..CloudSettings::coverage_layer(0.8)
    };
    let clear = render_layers(&device, eye, target, CloudSettings::default(), &extras);
    let cloudy = render_layers(&device, eye, target, layer, &extras);
    dump("clouds_behind_peak", &cloudy);
    dump("clouds_behind_peak_clear", &clear);
    let mut red = 0u32;
    let mut red_kept = 0u32;
    for y in 0..H {
        for x in 0..W {
            let src = rgb(&clear, x, y);
            let dst = rgb(&cloudy, x, y);
            if strongly_red(src) {
                red += 1;
                if dst[0] > dst[2] && dst[0] + 8.0 > dst[1] {
                    red_kept += 1;
                }
            }
        }
    }
    // The far slab, not the blue sky. A full-frame blue count passes when clouds
    // only change the sky and leave the peak alone.
    let far = extras[1];
    let (x0, y0, x1, y1) = project_box(eye, target, 55.0_f32.to_radians(), far.0, far.1);
    let rect_n = x1.saturating_sub(x0) * y1.saturating_sub(y0);
    let mut blue = 0u32;
    let mut blue_hidden = 0u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let src = rgb(&clear, x, y);
            let dst = rgb(&cloudy, x, y);
            if strongly_blue(src) {
                blue += 1;
                let dist = (src[0] - dst[0]).abs() + (src[1] - dst[1]).abs() + (src[2] - dst[2]).abs();
                if dist > 40.0 || !strongly_blue(dst) {
                    blue_hidden += 1;
                }
            }
        }
    }
    assert!(red > 12, "near peak is not in frame: {red} red pixels");
    assert!(
        red_kept * 5 > red * 3,
        "near peak lost its hue under the cloud shadow: {red_kept}/{red}"
    );
    // The near peak has no black region on a renderer's first frame (ADR-5457). The box is wound
    // inside out, so the camera sees its far walls from inside; the probes in its core held the
    // black of a closed cavity and those walls took it (44 % of this rectangle read under 8).
    let near = extras[0];
    let (nx0, ny0, nx1, ny1) = project_box(eye, target, 55.0_f32.to_radians(), near.0, near.1);
    for (name, frame) in [("clear", &clear), ("cloudy", &cloudy)] {
        let black = (ny0..ny1)
            .flat_map(|y| (nx0..nx1).map(move |x| (x, y)))
            .filter(|&(x, y)| luma(rgb(frame, x, y)) < 8.0)
            .count();
        assert_eq!(
            black, 0,
            "{name}: {black} black pixels on the near peak in {nx0},{ny0}-{nx1},{ny1}"
        );
    }
    assert!(
        rect_n > 12 && rect_n * 8 < W * H,
        "far peak rect is not a small screen region: {x0},{y0}-{x1},{y1} ({rect_n} px)"
    );
    assert!(
        blue * 2 > rect_n,
        "far peak interior is not the blue slab: {blue}/{rect_n} in {x0},{y0}-{x1},{y1}"
    );
    assert!(
        blue_hidden * 4 > blue * 3,
        "far peak is not occluded by the cloud bank: {blue_hidden}/{blue} in {x0},{y0}-{x1},{y1}"
    );
    golden(&device, "clouds_behind_peak", &cloudy);
}

fn cloud_bench_quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.gi = GiMode::Ambient;
    quality.contact_shadows = false;
    quality.volumetric_fog = false;
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 0;
    quality.reflection_probes = false;
    quality.parallax_occlusion = false;
    quality.screen_space_refraction = false;
    quality.volumetric_clouds = true;
    quality.physical_atmosphere = true;
    quality.near_sun_tile = 0;
    quality.translucent_shadow_cascades = 0;
    quality.translucent_local_shadows = false;
    quality.depth_velocity = false;
    quality.reflection_occlusion = false;
    quality.paged_sun_shadows = false;
    quality
}

/// Cloud-pass GPU time at 1440p and 4K, coverage 0 / 0.5 / 1.
///
/// Strips GI, SSR, fog and the big shadow atlas so the frame is the sky plus the cloud
/// pass. Steps stay at 64 and the march at half resolution. Ignored: a 4K lavapipe march
/// is too slow for the default suite. Run with `--ignored --nocapture`.
#[test]
#[ignore = "1440p and 4K cloud-pass timings"]
fn cloud_pass_cost_at_1440p_and_4k() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let info = device.adapter.get_info();
    eprintln!(
        "cloud bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let quality = cloud_bench_quality();
    let (vertices, indices) = quad([-2000.0, -2000.0], [2000.0, 2000.0], 0.0);
    for (width, height) in [(2560, 1440), (3840, 2160)] {
        for coverage in [0.0_f32, 0.5, 1.0] {
            let scene = LabScene {
                width,
                height,
                eye: [0.0, 3.0, 0.0],
                target: [0.0, 900.0, 700.0],
                fov_y: 55.0_f32.to_radians(),
                near: 0.3,
                far: 40_000.0,
                depth_prepass: true,
                directional_shadows: false,
                ibl: false,
                exposure_ev: 14.5,
                ambient_lux: 0.0,
                double_sided: false,
                post: Some(PostSettings {
                    enabled: true,
                    bloom: false,
                    auto_exposure: false,
                    exposure_ev: 14.5,
                    fxaa: false,
                    taa: false,
                    film_grain: 0.0,
                    vignette: 0.0,
                    ..PostSettings::default()
                }),
                directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
                local_lights: Vec::new(),
                materials: vec![material([0.25, 0.27, 0.18])],
                meshes: vec![LabMesh {
                    vertices: vertices.clone(),
                    indices: indices.clone(),
                    material: 0,
                    ..LabMesh::unskinned()
                }],
            };
            let mut renderer = FrameRenderer::new();
            renderer.set_gpu_timing(&device, true);
            renderer.set_outdoor(OutdoorParams {
                time: Some(noon()),
                clouds: CloudSettings::coverage_layer(coverage),
                planet: AtmosphereParameters::earth(),
                ..OutdoorParams::default()
            });
            let frame = scene.as_frame_scene_with_quality(quality);
            // One warmup pays for the atmosphere tables and the pipeline compile.
            renderer
                .render_offscreen(&device, &frame)
                .expect("cloud bench warmup");
            let mut samples = Vec::new();
            let mut walls = Vec::new();
            for _ in 0..3 {
                let started = std::time::Instant::now();
                renderer
                    .render_offscreen(&device, &frame)
                    .expect("cloud bench frame");
                walls.push(started.elapsed().as_secs_f32() * 1000.0);
                samples.push(renderer.gpu_timings().clouds_ms);
            }
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            walls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            eprintln!(
                "cloud_bench {width}x{height} coverage {coverage} clouds_ms {:.3} frame_wall_ms {:.1}",
                samples[1], walls[1]
            );
        }
    }
}

/// Diagnostics for a device whose `planet_earth_noon` plate sits under its floor (ADR-7704: the
/// Intel Arc, 35.47 dB against 35.6): writes the earth noon frame under `REGIA_DUMP_DIR` once as the
/// golden test renders it and once with each lighting feature of Desktop High turned off, so the
/// frames of two devices can be compared feature by feature (the feature whose removal brings the
/// two devices together is where they differ). Asserts nothing. `REGIA_DUMP_DIR=<dir> cargo test
/// -p regia_test_utils --test planet_frames -- --ignored --nocapture planet_earth_noon_feature_dump`.
#[test]
#[ignore = "diagnostics: dumps the earth noon frame per feature, asserts nothing"]
fn planet_earth_noon_feature_dump() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    println!("adapter {}", device.adapter.get_info().name);
    let shot = Shot {
        eye: [0.0, 3.0, 0.0],
        planet: AtmosphereParameters::earth(),
        time: noon(),
        target: [0.0, 60.0, 1000.0],
        ev: 14.5,
        albedo: [0.15, 0.15, 0.15],
        clouds: CloudSettings::default(),
        body: None,
        moons: Vec::new(),
        moon_albedo: [None, None],
    };
    type Tweak = fn(&mut FrameQuality, &mut LabScene);
    let variants: [(&str, Tweak); 18] = [
        ("default", |_, _| {}),
        ("gi_ambient", |q, _| {
            q.gi = GiMode::Ambient;
            q.gi_probes = false;
        }),
        ("no_gi_probes", |q, _| q.gi_probes = false),
        ("no_ssgi", |q, _| q.screen_space_gi = false),
        ("ssao_off", |q, _| q.ssao = SsaoMode::Off),
        ("no_horizon_ao", |q, _| q.horizon_ao = false),
        ("no_contact_shadows", |q, _| q.contact_shadows = false),
        ("no_blue_noise", |q, _| q.blue_noise = false),
        ("no_ssr", |q, _| q.ssr = false),
        ("no_paged_sun_shadows", |q, _| q.paged_sun_shadows = false),
        ("no_aerial_tables", |q, _| q.aerial_from_tables = false),
        ("no_reflection_occlusion", |q, _| {
            q.reflection_occlusion = false;
            q.bent_normals = false;
        }),
        // Scene variants: where the tower's base meets the 12 km ground quad.
        ("no_depth_prepass", |_, s| s.depth_prepass = false),
        ("near_1m", |_, s| s.near = 1.0),
        ("ground_200m", |_, s| {
            let (vertices, indices) = quad([-100.0, -100.0], [100.0, 100.0], 0.0);
            s.meshes[0].vertices = vertices;
            s.meshes[0].indices = indices;
        }),
        ("tower_sunk_0_5m", |_, s| {
            s.meshes[1] = tower([0.0, 5.5, 30.0], [3.0, 6.0, 3.0])
        }),
        // The tower's bottom face (`tower`'s fourth face: indices 18..24) lies in the ground's plane.
        ("tower_no_bottom", |_, s| {
            s.meshes[1].indices.drain(18..24);
        }),
        ("double_sided", |_, s| s.double_sided = true),
    ];
    for (name, tweak) in variants {
        let frame = render_with(&device, &shot, tweak);
        dump(&format!("earth_noon_{name}"), &frame);
        let sky = mean_rect(&frame, 0, 0, W, H / 2);
        let ground = mean_rect(&frame, 0, H / 2, W, H);
        println!("{name}: sky mean {sky:?} ground mean {ground:?}");
    }
}
