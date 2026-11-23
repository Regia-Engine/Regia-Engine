// SPDX-License-Identifier: MPL-2.0
//! The air under a cloud layer takes the layer's shadow (ADR-5944).
//!
//! With `CloudSettings::shadow_map` on (ADR-320) the sun term of a surface reads the cloud
//! transmittance map. The aerial composite (`outdoor_sky.wgsl` `fs_aerial`) did not: the haze
//! between the eye and a shadowed surface stayed sunlit, and a view from high up washed out
//! under a closed cover (ADR-5792 measured it on the forest demo). Two measures hold the fix:
//!
//! - **Coverage in the air.** A split sky over a black ground (a black metal: no diffuse light,
//!   and of the sun and the image light only what Fresnel reflects at a grazing view, the same in
//!   both halves), so the frame is the air's in-scatter and little else: the half under the layer
//!   must lose its sun, the clear half must keep the light it has without a layer.
//! - **Contrast from high up.** Dark and bright ground patches seen from 1.2 km: their contrast
//!   under a closed cover, at an exposure that puts the bright patches where a clear sky puts
//!   them, must stay near the clear sky's (under one illumination the apparent contrast of two
//!   surfaces depends on the air alone, Koschmieder 1924; a veil that keeps a sun the ground has
//!   lost lowers it).
//!
//! Lavapipe-deterministic like `cloud_shadow.rs`: one frame per renderer, fixed exposure, no
//! temporal passes. Without a GPU adapter the tests print a note and pass.

use regia_render::{
    AtmosphereParameters, CloudPattern, CloudSettings, DdgiSettings, DirectionalLightUniform, FogSettings,
    FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, box_mesh,
};

const W: u32 = 320;
const H: u32 = 180;
/// Vertical field of view of the test camera.
const FOV_Y_DEG: f32 = 40.0;
/// The camera: 1.2 km up (under the layer's 1.5 km base), looking down at the ground 1.8 km ahead.
const EYE: [f32; 3] = [0.0, 1200.0, 0.0];
const TARGET: [f32; 3] = [0.0, 0.0, 1800.0];
/// Ground cells are this many metres square.
const CELL: f32 = 400.0;
/// Aerosol optical depth of the test air: about six times the published clear baseline (a hazy
/// day; the forest demo's `sky.ron` has 0.1), so the aerosol's forward lobe carries much of the
/// in-scatter, as it does there, and a clear sky still shows the ground's contrast over 2 to 4 km.
const TURBIDITY: f32 = 0.03;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cloud shadow haze: {err}");
            None
        }
    }
}

/// Desktop High with the sky and clouds and nothing that moves pixels on its own
/// (`cloud_shadow.rs`'s quality).
fn quality() -> FrameQuality {
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
    quality.cloud_reprojection = false;
    quality
}

/// A matte dielectric of `albedo`, or with `None` a black metal: no diffuse light and no
/// reflection at normal incidence, so its pixels show the air in front of it and, toward the
/// horizon, a faint reflection of the image light (Schlick's Fresnel term at a grazing view).
fn material(albedo: Option<f32>) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.roughness = 0.9;
    match albedo {
        Some(a) => mat.base_color = [a, a, a, 1.0],
        None => {
            mat.base_color = [0.0, 0.0, 0.0, 1.0];
            mat.metallic = 1.0;
        }
    }
    mat
}

/// Whether the ground cell under world (`x`, `z`) is a bright one. Mirrored in x, so the test
/// does not depend on which way the view's x axis runs.
fn bright_cell(x: f32, z: f32) -> bool {
    let cx = (x.abs() / CELL).floor() as i64;
    let cz = (z / CELL).floor() as i64;
    (cx + cz).rem_euclid(2) == 0
}

/// The ground as two meshes of `CELL` squares on y = 0, 16 km wide and deep: mesh 0 holds the
/// bright cells (material 0), mesh 1 the dark ones (material 1).
fn ground_meshes() -> Vec<LabMesh> {
    let n = [0.0, 1.0, 0.0];
    let mut cells: [(Vec<LabVertex>, Vec<u32>); 2] = [(Vec::new(), Vec::new()), (Vec::new(), Vec::new())];
    for iz in -4..36 {
        for ix in -20..20 {
            let (x0, z0) = (ix as f32 * CELL, iz as f32 * CELL);
            let (x1, z1) = (x0 + CELL, z0 + CELL);
            let slot = usize::from(!bright_cell(x0 + 0.5 * CELL, z0 + 0.5 * CELL));
            let (vertices, indices) = &mut cells[slot];
            let first = vertices.len() as u32;
            vertices.push(LabVertex::from_pos_normal_uv([x0, 0.0, z0], n, [0.0, 0.0]));
            vertices.push(LabVertex::from_pos_normal_uv([x1, 0.0, z0], n, [1.0, 0.0]));
            vertices.push(LabVertex::from_pos_normal_uv([x1, 0.0, z1], n, [1.0, 1.0]));
            vertices.push(LabVertex::from_pos_normal_uv([x0, 0.0, z1], n, [0.0, 1.0]));
            indices.extend([first, first + 2, first + 1, first, first + 3, first + 2]);
        }
    }
    let [bright, dark] = cells;
    vec![
        LabMesh {
            vertices: bright.0,
            indices: bright.1,
            material: 0,
            ..LabMesh::unskinned()
        },
        LabMesh {
            vertices: dark.0,
            indices: dark.1,
            material: 1,
            ..LabMesh::unskinned()
        },
    ]
}

/// The high view over the checkered ground. `to_sun` points at the sun (an authored light: no
/// clock); `albedo` is the bright and the dark cells' material.
fn lab(to_sun: [f32; 3], exposure_ev: f32, albedo: [Option<f32>; 2]) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y_DEG.to_radians(),
        near: 1.0,
        far: 60_000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([-to_sun[0], -to_sun[1], -to_sun[2]], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![material(albedo[0]), material(albedo[1])],
        meshes: ground_meshes(),
    }
}

/// Earth's air with the forest demo's aerosol.
fn planet() -> AtmosphereParameters {
    let mut planet = AtmosphereParameters::earth();
    planet.set_aerosol_turbidity(TURBIDITY);
    planet
}

fn render(device: &RenderDevice, lab: &LabScene, clouds: CloudSettings) -> Vec<u8> {
    render_on(device, lab, clouds, planet(), 0.0).0
}

/// One frame on a new renderer over `planet`: the pixels, and how many of the renderer's frames
/// read the cloud map for the air (`FrameRenderer::cloud_air_frames`).
/// `rain_per_m` is the precipitation's extinction (1/m; 0 for dry air). A rainy frame has the
/// fog's sky in-scatter mode on (`FogSettings::sky_inscatter`, the fog itself off): the rain's
/// veil then scatters the sky's radiance, a physical one. The flat mode's colour follows the
/// exposure on these surfaces and says nothing about the light.
fn render_on(
    device: &RenderDevice,
    lab: &LabScene,
    clouds: CloudSettings,
    planet: AtmosphereParameters,
    rain_per_m: f32,
) -> (Vec<u8>, u64) {
    render_air(
        device,
        lab,
        clouds,
        planet,
        Air {
            rain_per_m,
            ..Air::default()
        },
    )
}

/// What is in the air of a test frame besides the planet's own.
#[derive(Clone, Copy, Default)]
struct Air {
    /// The precipitation's extinction (1/m; 0 for dry air).
    rain_per_m: f32,
    /// The forward fog's settings, when the frame sets them: the flat mode for the rain's veil.
    /// `None` with rain is the sky in-scatter mode. (The fog's own haze never shows here: a frame
    /// that draws the atmosphere tables switches the forward fog off, `frame/steps/modes.rs`.)
    haze: Option<FogSettings>,
    /// The scene's own exposure setting (EV100) when it stays put while the final exposure
    /// moves, as under an auto exposure: the forward pass divides the flat fog colour by this one.
    scene_ev: Option<f32>,
}

/// [`render_on`] through `air`.
fn render_air(
    device: &RenderDevice,
    lab: &LabScene,
    clouds: CloudSettings,
    planet: AtmosphereParameters,
    air: Air,
) -> (Vec<u8>, u64) {
    let Air {
        rain_per_m,
        haze,
        scene_ev,
    } = air;
    let pinned;
    let lab = if let Some(ev) = scene_ev {
        pinned = LabScene {
            exposure_ev: ev,
            ..lab.clone()
        };
        &pinned
    } else {
        lab
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(OutdoorParams {
        clouds,
        planet,
        precip_extinction_per_m: rain_per_m,
        ..OutdoorParams::default()
    });
    let mut scene = lab.as_frame_scene_with_quality(quality());
    if let Some(haze) = haze {
        scene.fog = haze;
    } else if rain_per_m > 0.0 {
        scene.fog = FogSettings {
            sky_inscatter: true,
            ..FogSettings::default()
        };
    }
    let pixels = renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("cloud shadow haze frame: {err}"))
        .pixels;
    (pixels, renderer.cloud_air_frames())
}

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

fn dump(name: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    if let Err(err) = image::save_buffer(&path, pixels, W, H, image::ColorType::Rgba8) {
        eprintln!("dump {name}: {err}");
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / len, v[1] / len, v[2] / len]
}

/// Where the centre of pixel (`px`, `py`) meets the ground: (|x|, z) in metres. The camera has
/// no roll and looks along +z, so a pixel's world x is its screen offset times a sign the test
/// does not need (the ground pattern and the measures are mirrored in x).
fn ground_point(px: u32, py: u32) -> Option<(f32, f32)> {
    let f = normalize([TARGET[0] - EYE[0], TARGET[1] - EYE[1], TARGET[2] - EYE[2]]);
    // Camera up: world up with its part along the view taken out.
    let up = normalize([-f[0] * f[1], 1.0 - f[1] * f[1], -f[2] * f[1]]);
    let tan_half = (0.5 * FOV_Y_DEG.to_radians()).tan();
    let sx = ((px as f32 + 0.5) / W as f32 * 2.0 - 1.0) * tan_half * (W as f32 / H as f32);
    let sy = (1.0 - (py as f32 + 0.5) / H as f32 * 2.0) * tan_half;
    // The camera's right is world ±x.
    let dir = normalize([sx, f[1] + up[1] * sy, f[2] + up[2] * sy]);
    if dir[1] >= -1.0e-4 {
        return None;
    }
    let t = -EYE[1] / dir[1];
    Some(((dir[0] * t).abs(), EYE[2] + dir[2] * t))
}

/// Mean luma of the measured pixels `pick` accepts by their ground point, and how many there
/// are. Measured pixels lie 1.0 to 3.6 km ahead and within 3 km of the centre line: 1.6 to 3.8 km
/// of air, well inside the cloud map's window.
fn mean_where(pixels: &[u8], pick: impl Fn(f32, f32) -> bool) -> (f32, usize) {
    let mut sum = 0.0;
    let mut count = 0usize;
    for py in 0..H {
        for px in 0..W {
            let Some((x, z)) = ground_point(px, py) else {
                continue;
            };
            if !(1000.0..=3600.0).contains(&z) || x > 3000.0 || !pick(x, z) {
                continue;
            }
            let i = ((py * W + px) * 4) as usize;
            sum += luma(&pixels[i..i + 4]);
            count += 1;
        }
    }
    (sum / count.max(1) as f32, count)
}

/// At least 80 m inside its ground cell (a pixel's footprint is under 50 m in the measured band).
fn inside_cell(x: f32, z: f32) -> bool {
    let edge = |v: f32| {
        let f = v.rem_euclid(CELL);
        f.min(CELL - f)
    };
    edge(x) >= 80.0 && edge(z) >= 80.0
}

/// Mean luma of the bright and of the dark cells.
fn patches(pixels: &[u8]) -> (f32, f32) {
    let (bright, n_bright) = mean_where(pixels, |x, z| inside_cell(x, z) && bright_cell(x, z));
    let (dark, n_dark) = mean_where(pixels, |x, z| inside_cell(x, z) && !bright_cell(x, z));
    assert!(
        n_bright > 800 && n_dark > 800,
        "the view does not show enough of the ground ({n_bright} bright, {n_dark} dark pixels)"
    );
    (bright, dark)
}

/// Michelson contrast of two luma levels.
fn contrast(bright: f32, dark: f32) -> f32 {
    (bright - dark) / (bright + dark).max(1.0e-3)
}

/// A closed stratus deck in still air, with the cloud map and (when `overcast`) the overcast sky
/// light.
fn deck(overcast: bool) -> CloudSettings {
    CloudSettings {
        shadow_map: true,
        overcast_ibl: overcast,
        pattern: CloudPattern::Constant,
        wind_m_per_s: [0.0, 0.0],
        ..CloudSettings::coverage_layer(0.95)
    }
}

/// Half the sky cloudy (world x ≥ 0) in still air, the cloud map on (`cloud_shadow.rs`'s split).
fn split_sky() -> CloudSettings {
    CloudSettings {
        shadow_map: true,
        pattern: CloudPattern::SplitHemisphere,
        map_resolution: 1024,
        wind_m_per_s: [0.0, 0.0],
        coverage: 0.5,
        ..CloudSettings::default()
    }
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Coverage in the air: over a black ground the frame is the air's in-scatter (and a faint
/// grazing reflection of the image light, the same in both halves). Under the cloudy half of a
/// split sky the air loses the sun's share of it; under the clear half it keeps what it has with
/// no layer at all.
#[test]
fn the_air_under_the_cloudy_half_loses_its_sun() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // The sun straight overhead: the split's shadow falls on the plane x = 0 and every view ray
    // stays on its own side of it.
    let lab = lab([0.0, 1.0, 0.0], 11.0, [None, None]);
    let no_layer = render(&device, &lab, CloudSettings::default());
    let split = render(&device, &lab, split_sky());
    dump("haze_air_no_layer", &no_layer);
    dump("haze_air_split", &split);
    // The screen halves, away from the split's edge. Which one is cloudy depends on the view's
    // handedness: the cloudy one is the darker.
    let side = |pixels: &[u8], right: bool| {
        let mut sum = 0.0;
        let mut count = 0usize;
        for py in 0..H {
            for px in 0..W {
                if (px >= W / 2) != right {
                    continue;
                }
                let Some((x, z)) = ground_point(px, py) else {
                    continue;
                };
                if (1000.0..=3600.0).contains(&z) && (1000.0..=3000.0).contains(&x) {
                    let i = ((py * W + px) * 4) as usize;
                    sum += luma(&pixels[i..i + 4]);
                    count += 1;
                }
            }
        }
        assert!(
            count > 1500,
            "a screen half shows too little ground ({count} pixels)"
        );
        sum / count as f32
    };
    let (left, right) = (side(&split, false), side(&split, true));
    let cloudy_is_right = right < left;
    let (cloudy, clear) = (left.min(right), left.max(right));
    let (clear_before, cloudy_before) = (
        side(&no_layer, !cloudy_is_right),
        side(&no_layer, cloudy_is_right),
    );
    eprintln!(
        "air in-scatter luma: split sky cloudy half {cloudy:.2}, clear half {clear:.2}; no layer {cloudy_before:.2} and {clear_before:.2}"
    );
    assert!(
        clear_before > 30.0,
        "the air's in-scatter is too dim to measure ({clear_before})"
    );
    assert!(
        cloudy < 0.8 * clear,
        "the air under the cloudy half kept its sun: in-scatter luma {cloudy:.2} against {clear:.2} under the clear half"
    );
    assert!(
        cloudy > 0.05 * clear,
        "the shadowed air lost its multiple scattering too ({cloudy:.2} against {clear:.2})"
    );
    assert!(
        (clear - clear_before).abs() < 1.5,
        "the air under the clear half changed with the layer: {clear:.2}, {clear_before:.2} with no layer"
    );
    // The air is shadowed on the side where the ground is: the same frame over grey ground has
    // its darker half on the same side of the screen.
    let grey = self::lab([0.0, 1.0, 0.0], 13.5, [Some(0.20), Some(0.20)]);
    let ground = render(&device, &grey, split_sky());
    dump("haze_air_split_ground", &ground);
    let (ground_left, ground_right) = (side(&ground, false), side(&ground, true));
    assert!(
        (ground_left - ground_right).abs() > 10.0,
        "the split sky does not shadow half the ground ({ground_left:.1}, {ground_right:.1})"
    );
    assert_eq!(
        ground_right < ground_left,
        cloudy_is_right,
        "the air is shadowed on the other side from the ground (ground {ground_left:.1} | {ground_right:.1}, air {left:.1} | {right:.1})"
    );
}

/// The exposure (EV100) at which `clouds` puts the bright cells at `target` luma: bisection, as
/// an auto exposure would settle. Returns the frame too.
fn expose_to(device: &RenderDevice, to_sun: [f32; 3], clouds: CloudSettings, target: f32) -> (f32, Vec<u8>) {
    expose_rain_to(device, to_sun, clouds, target, 0.0)
}

/// [`expose_to`] through precipitation of extinction `rain_per_m`.
fn expose_rain_to(
    device: &RenderDevice,
    to_sun: [f32; 3],
    clouds: CloudSettings,
    target: f32,
    rain_per_m: f32,
) -> (f32, Vec<u8>) {
    expose_air_to(
        device,
        to_sun,
        clouds,
        target,
        Air {
            rain_per_m,
            ..Air::default()
        },
    )
}

/// [`expose_rain_to`] through `air`.
fn expose_air_to(
    device: &RenderDevice,
    to_sun: [f32; 3],
    clouds: CloudSettings,
    target: f32,
    air: Air,
) -> (f32, Vec<u8>) {
    // A lower EV is a brighter frame.
    let (mut bright_ev, mut dim_ev) = (6.0_f32, 16.0_f32);
    let mut best: Option<(f32, f32, Vec<u8>)> = None;
    for _ in 0..9 {
        let ev = 0.5 * (bright_ev + dim_ev);
        let frame = render_air(
            device,
            &lab(to_sun, ev, [Some(0.20), Some(0.04)]),
            clouds,
            planet(),
            air,
        )
        .0;
        let (bright, _) = patches(&frame);
        let miss = (bright - target).abs();
        if best.as_ref().is_none_or(|(_, m, _)| miss < *m) {
            best = Some((ev, miss, frame));
        }
        if miss < 1.0 {
            break;
        }
        if bright > target {
            bright_ev = ev;
        } else {
            dim_ev = ev;
        }
    }
    let (ev, miss, frame) = best.unwrap_or_else(|| panic!("no exposure tried"));
    assert!(
        miss < 4.0,
        "no exposure puts the bright cells at {target:.1} (closest misses by {miss:.1} at EV {ev:.2})"
    );
    (ev, frame)
}

/// Contrast from high up: under a closed cover, exposed so the bright ground sits where the
/// clear sky's frame has it, the dark and bright cells keep the contrast the clear sky gives
/// them. A veil of sunlit haze over ground that has lost its sun fails this.
#[test]
fn a_high_view_keeps_its_contrast_under_a_closed_cover() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // The sun 35° up, ahead of the camera: the view looks into the aerosol's forward lobe, as
    // the demo's high views do in the afternoon.
    let to_sun = normalize([0.0, 0.574, 0.819]);
    let clear_ev = 13.5;
    let clear = render(
        &device,
        &lab(to_sun, clear_ev, [Some(0.20), Some(0.04)]),
        CloudSettings::default(),
    );
    dump("haze_high_clear", &clear);
    let (clear_bright, clear_dark) = patches(&clear);
    let clear_contrast = contrast(clear_bright, clear_dark);
    eprintln!(
        "clear sky: bright {clear_bright:.1} dark {clear_dark:.1} contrast {clear_contrast:.3} at EV {clear_ev}"
    );
    assert!(
        (60.0..=200.0).contains(&clear_bright) && clear_contrast > 0.15,
        "the clear frame is not a usable reference (bright {clear_bright:.1}, contrast {clear_contrast:.3})"
    );
    // Both covers are measured before any is judged, so one run reports both.
    let mut washed_out = Vec::new();
    for (name, overcast) in [("map", false), ("map_overcast", true)] {
        let (ev, frame) = expose_to(&device, to_sun, deck(overcast), clear_bright);
        dump(&format!("haze_high_{name}"), &frame);
        let (bright, dark) = patches(&frame);
        let c = contrast(bright, dark);
        let kept = c / clear_contrast;
        eprintln!(
            "closed cover ({name}): bright {bright:.1} dark {dark:.1} contrast {c:.3} at EV {ev:.2}; {kept:.2} of the clear sky's"
        );
        assert!(
            ev < clear_ev - 0.5,
            "{name}: the cover did not shadow the ground (EV {ev:.2} against {clear_ev} for the same level)"
        );
        if kept < 0.85 {
            washed_out.push(format!(
                "{name}: contrast {c:.3}, {kept:.2} of the clear sky's {clear_contrast:.3}"
            ));
        }
    }
    assert!(
        washed_out.is_empty(),
        "the high view washed out under the cover: {}",
        washed_out.join("; ")
    );
}

/// Coverage on the ground from the same view: under the closed cover every measured ground
/// pixel is darker than under the clear sky at the same exposure (the map reaches the whole
/// view, not a patch of it), and none is black (the sky light stays).
#[test]
fn the_cover_shadows_all_of_the_ground_in_view() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let to_sun = normalize([0.0, 0.574, 0.819]);
    let lab = lab(to_sun, 13.5, [Some(0.20), Some(0.20)]);
    let clear = render(&device, &lab, CloudSettings::default());
    let covered = render(&device, &lab, deck(false));
    dump("haze_cover_clear", &clear);
    dump("haze_cover_deck", &covered);
    let mut measured = 0usize;
    let mut shadowed = 0usize;
    let mut black = 0usize;
    for py in 0..H {
        for px in 0..W {
            let Some((x, z)) = ground_point(px, py) else {
                continue;
            };
            if !(1000.0..=3600.0).contains(&z) || x > 3000.0 {
                continue;
            }
            let i = ((py * W + px) * 4) as usize;
            let (before, after) = (luma(&clear[i..i + 4]), luma(&covered[i..i + 4]));
            measured += 1;
            shadowed += usize::from(after < 0.75 * before);
            black += usize::from(after < 2.0);
        }
    }
    eprintln!("ground under the cover: {shadowed} of {measured} pixels shadowed, {black} black");
    assert!(
        measured > 5000,
        "the view shows too little ground ({measured} pixels)"
    );
    assert!(
        shadowed as f32 >= 0.98 * measured as f32,
        "the cover shadows only {shadowed} of {measured} ground pixels"
    );
    assert_eq!(black, 0, "{black} shadowed ground pixels kept no sky light");
}

/// The air reads the cloud map on exactly the frames whose surfaces do: with `shadow_map` on,
/// under a layer, on a planet with air. With the map off (alone, or built only for the fog or
/// with the overcast sky light) the aerial composite never reads it, and neither does an airless
/// world's, which draws no air at all.
#[test]
fn the_air_reads_the_map_only_with_the_switch_on() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab([0.0, 1.0, 0.0], 13.5, [Some(0.20), Some(0.04)]);
    let on = split_sky();
    let frames =
        |clouds: CloudSettings, planet: AtmosphereParameters| render_on(&device, &lab, clouds, planet, 0.0).1;
    assert_eq!(frames(on, planet()), 1, "the map is on: the air reads it");
    for (name, clouds) in [
        (
            "no switch",
            CloudSettings {
                shadow_map: false,
                ..on
            },
        ),
        (
            "the fog's map only",
            CloudSettings {
                shadow_map: false,
                fog_coupling: true,
                ..on
            },
        ),
        (
            "the overcast sky light only",
            CloudSettings {
                shadow_map: false,
                overcast_ibl: true,
                ..on
            },
        ),
        ("no layer", CloudSettings { coverage: 0.0, ..on }),
    ] {
        assert_eq!(frames(clouds, planet()), 0, "{name}: the air read the cloud map");
    }
    assert_eq!(
        frames(on, AtmosphereParameters::airless()),
        0,
        "an airless world has no air to shadow"
    );
}

/// The planet's own surface beyond the terrain (a sky pixel whose ray meets the planet) is under
/// the cover too: it loses the sun's share of its light as the terrain in front of it does, and
/// keeps the sky's. Before, the land beyond a shadowed terrain stayed sunlit to the horizon.
#[test]
fn the_planets_surface_beyond_the_terrain_is_shadowed_too() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let to_sun = normalize([0.0, 0.574, 0.819]);
    // No terrain in view: one square behind the camera, so every pixel is the planet's surface.
    let mut scene = lab(to_sun, 13.5, [Some(0.20), Some(0.20)]);
    let n = [0.0, 1.0, 0.0];
    scene.meshes = vec![LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-200.0, 0.0, -2200.0], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([200.0, 0.0, -2200.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([200.0, 0.0, -1800.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-200.0, 0.0, -1800.0], n, [0.0, 1.0]),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    }];
    let clear = render(&device, &scene, CloudSettings::default());
    let covered = render(&device, &scene, deck(false));
    dump("haze_planet_clear", &clear);
    dump("haze_planet_deck", &covered);
    let (before, count) = mean_where(&clear, |_, _| true);
    let (after, _) = mean_where(&covered, |_, _| true);
    eprintln!(
        "the planet's surface: luma {before:.1} under a clear sky, {after:.1} under the cover ({count} pixels)"
    );
    assert!(
        count > 5000,
        "the view shows too little of the planet ({count} pixels)"
    );
    assert!(
        before > 40.0,
        "the planet's surface is too dark to measure ({before:.1})"
    );
    assert!(
        after < 0.75 * before,
        "the planet's surface kept its sun under the cover: luma {after:.1} against {before:.1} under a clear sky"
    );
    assert!(after > 2.0, "the planet's surface kept no sky light ({after:.1})");
}

/// A wall under the cover is lit like the ground beside it: by the sky light the cover leaves and
/// by the bounce of the ground, which is in the cover's shadow too. Before, the image light kept
/// a sunlit ground's bounce under any cover, so every steep face (a wall, a tree, a hillside)
/// stood several times brighter than the shadowed ground around it.
#[test]
fn a_wall_under_the_cover_takes_no_sunlit_grounds_bounce() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let to_sun = normalize([0.0, 0.574, 0.819]);
    // Grey ground and a grey wall of the same albedo 2.4 km ahead, 1.2 km wide and 600 m high,
    // facing the camera (its back to the sun, so only the sky and the ground light it).
    let mut scene = lab(to_sun, 9.5, [Some(0.20), Some(0.20)]);
    let toward_camera = [0.0, 0.0, -1.0];
    scene.meshes.push(LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-600.0, 0.0, 2400.0], toward_camera, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([600.0, 0.0, 2400.0], toward_camera, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([600.0, 600.0, 2400.0], toward_camera, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-600.0, 600.0, 2400.0], toward_camera, [0.0, 1.0]),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    });
    let covered = render(&device, &scene, deck(true));
    dump("haze_wall_deck", &covered);
    // The wall covers columns 101..219 of rows 2..59; the ground right below it is as far away.
    let block = |x0: u32, x1: u32, y0: u32, y1: u32| {
        let mut sum = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * W + x) * 4) as usize;
                sum += luma(&covered[i..i + 4]);
            }
        }
        sum / ((x1 - x0) * (y1 - y0)) as f32
    };
    let wall = block(120, 200, 12, 48);
    let ground = block(120, 200, 72, 100);
    eprintln!("under the cover: wall luma {wall:.1}, the ground below it {ground:.1}");
    assert!(
        ground > 40.0 && ground < 230.0,
        "the ground is not exposed for the measure ({ground:.1})"
    );
    assert!(
        wall <= 1.05 * ground,
        "the wall outshines the shadowed ground: luma {wall:.1} against {ground:.1} (a sunlit ground's bounce under the cover)"
    );
    assert!(
        wall > 0.3 * ground,
        "the wall lost the sky light too ({wall:.1} against {ground:.1})"
    );
}

/// Rain under the cover scatters the light under the cover. The precipitation's own veil (its
/// extinction over the view distance, in-scattering the haze's horizon sky and sun glow in the
/// fog's sky in-scatter mode) keeps the contrast of the ground where the same rain under a clear
/// sky has it. Before, the veil in-scattered a clear sky's sunlit radiance under any cover, and a
/// high view through light rain washed out as the haze did.
#[test]
fn rain_under_the_cover_scatters_the_covers_light() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let to_sun = normalize([0.0, 0.574, 0.819]);
    // The weather's light rain (the demo's overcast preset has 5.2e-5 /m): about a tenth of the
    // light of a surface 2 km away is replaced by the veil.
    let rain_per_m = 5.2e-5;
    let clear_ev = 13.5;
    let clear = render_on(
        &device,
        &lab(to_sun, clear_ev, [Some(0.20), Some(0.04)]),
        CloudSettings::default(),
        planet(),
        rain_per_m,
    )
    .0;
    dump("haze_rain_clear", &clear);
    let (clear_bright, clear_dark) = patches(&clear);
    let clear_contrast = contrast(clear_bright, clear_dark);
    eprintln!(
        "rain under a clear sky: bright {clear_bright:.1} dark {clear_dark:.1} contrast {clear_contrast:.3} at EV {clear_ev}"
    );
    assert!(
        (60.0..=210.0).contains(&clear_bright) && clear_contrast > 0.05,
        "the clear frame is not a usable reference (bright {clear_bright:.1}, contrast {clear_contrast:.3})"
    );
    // The map alone: the ground keeps the sky's share of its light, and the veil keeps the
    // haze's floor, which is that share.
    let (ev, frame) = expose_rain_to(&device, to_sun, deck(false), clear_bright, rain_per_m);
    dump("haze_rain_deck", &frame);
    let (bright, dark) = patches(&frame);
    let c = contrast(bright, dark);
    let kept = c / clear_contrast;
    eprintln!(
        "rain under the closed cover (map): bright {bright:.1} dark {dark:.1} contrast {c:.3} at EV {ev:.2}; {kept:.2} of the clear sky's"
    );
    assert!(
        kept >= 0.85,
        "the high view washed out in the rain under the cover: contrast {c:.3}, {kept:.2} of the clear sky's {clear_contrast:.3}"
    );
    // With the overcast sky light too the ground's light dims further and the veil's floor is a
    // constant (ADR-5944's limit): reported, not held to the bar.
    let (ev, frame) = expose_rain_to(&device, to_sun, deck(true), clear_bright, rain_per_m);
    dump("haze_rain_deck_overcast", &frame);
    let (bright, dark) = patches(&frame);
    let c = contrast(bright, dark);
    eprintln!(
        "rain under the closed cover (map_overcast): bright {bright:.1} dark {dark:.1} contrast {c:.3} at EV {ev:.2}; {:.2} of the clear sky's",
        c / clear_contrast
    );
}

/// Rain in the fog's flat mode, under an exposure that adapts. The forward pass divides the flat
/// fog colour by the scene's own exposure setting; when the final exposure moves away from that
/// setting (an auto exposure under a cover) the veil is a fixed radiance, a clear day's, and it
/// washed out the surfaces under the cover as the terrain's did. With the cloud map it keeps the
/// haze's floor of it, and the ground keeps the contrast the same rain leaves it under a clear sky.
#[test]
fn rain_in_the_flat_fog_mode_under_the_cover_keeps_the_contrast() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let to_sun = normalize([0.0, 0.574, 0.819]);
    let clear_ev = 13.5;
    let rain = Air {
        rain_per_m: 5.2e-5,
        haze: Some(FogSettings::default()),
        scene_ev: Some(clear_ev),
    };
    let clear = render_air(
        &device,
        &lab(to_sun, clear_ev, [Some(0.20), Some(0.04)]),
        CloudSettings::default(),
        planet(),
        rain,
    )
    .0;
    dump("haze_flat_rain_clear", &clear);
    let (clear_bright, clear_dark) = patches(&clear);
    let clear_contrast = contrast(clear_bright, clear_dark);
    eprintln!(
        "flat-mode rain under a clear sky: bright {clear_bright:.1} dark {clear_dark:.1} contrast {clear_contrast:.3} at EV {clear_ev}"
    );
    assert!(
        (60.0..=210.0).contains(&clear_bright) && clear_contrast > 0.05,
        "the clear frame is not a usable reference (bright {clear_bright:.1}, contrast {clear_contrast:.3})"
    );
    let (ev, frame) = expose_air_to(&device, to_sun, deck(false), clear_bright, rain);
    dump("haze_flat_rain_deck", &frame);
    let (bright, dark) = patches(&frame);
    let c = contrast(bright, dark);
    let kept = c / clear_contrast;
    eprintln!(
        "flat-mode rain under the closed cover (map): bright {bright:.1} dark {dark:.1} contrast {c:.3} at EV {ev:.2}, the scene's setting at {clear_ev}; {kept:.2} of the clear sky's"
    );
    assert!(
        kept >= 0.85,
        "the high view washed out in the flat-mode rain under the cover: contrast {c:.3}, {kept:.2} of the clear sky's {clear_contrast:.3}"
    );
}

/// A box of half extents `half` centred at `centre`.
fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: glam::Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// Mean BT.709 luma of a pixel block, decoded from sRGB to linear light (ratios of it are
/// ratios of light, up to the tone curve).
fn linear_block(pixels: &[u8], x0: u32, x1: u32, y0: u32, y1: u32) -> f32 {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]);
        }
    }
    sum / ((x1 - x0) * (y1 - y0)) as f32
}

/// A wall on open ground from eye height, the sun 55° up behind it: the face toward the camera
/// has no sun under any sky, so it shows the indirect light alone. Renders it under `clouds`
/// with the GI probes (40 frames, converged) or with the image light alone (4 frames), and
/// returns the mean linear luma of the wall's face and of the ground at its foot.
fn wall_from_eye_height(
    device: &RenderDevice,
    clouds: CloudSettings,
    probes: bool,
    name: &str,
) -> (f32, f32) {
    let exposure_ev = 12.5;
    let to_sun = normalize([0.0, 0.819, 0.574]);
    let scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.7, -5.0],
        target: [0.0, 1.6, 0.0],
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([-to_sun[0], -to_sun[1], -to_sun[2]], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![material(Some(0.6))],
        // An 80 m ground slab and a wall 16 m wide and 4 m high across the view, 5 m ahead.
        meshes: vec![
            block([0.0, -0.25, 0.0], [40.0, 0.25, 40.0], 0),
            block([0.0, 2.0, 0.0], [8.0, 2.0, 0.15], 0),
        ],
    };
    let mut quality = quality();
    if probes {
        quality.gi = GiMode::Probes;
        quality.gi_probes = true;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_camera_cut_detection(None);
    // Probe volumes small enough for lavapipe (`regia_render/tests/ddgi.rs`): two cascades of
    // 12×8×12 at 1 m and 3 m, every probe traced every frame.
    renderer.set_ddgi_settings(DdgiSettings {
        cascades: 2,
        dims: [12, 8, 12],
        spacing: 1.0,
        update_divisor: 1,
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    });
    renderer.set_ddgi_enabled(Some(probes));
    renderer.set_outdoor(OutdoorParams {
        clouds,
        planet: planet(),
        ..OutdoorParams::default()
    });
    let mut pixels = Vec::new();
    for _ in 0..if probes { 40 } else { 4 } {
        pixels = renderer
            .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))
            .unwrap_or_else(|err| panic!("probe wall frame: {err}"))
            .pixels;
    }
    assert_eq!(
        renderer.ddgi_active(),
        probes,
        "{name}: the probes are not in the state the frame asked for"
    );
    dump(name, &pixels);
    // The wall's face covers rows 12..147 of every column; the ground at its foot the rows below.
    (
        linear_block(&pixels, 110, 210, 50, 110),
        linear_block(&pixels, 110, 210, 158, 176),
    )
}

/// The GI probes are under the cover too. Their ray hits are lit by the sun through the
/// tracer's own shadow rays, which do not read the cloud transmittance map: under a closed cover
/// the probes kept the bounce of a sunlit ground, and everything they light (a wall, a tree, a
/// hillside on the tiers that run them) stood several times brighter than the image light alone
/// leaves it there. With the map on the hits keep the share of the sun the layer leaves on the
/// ground, so a wall with its back to the sun is lit by the probes as by the image light: the
/// cover's sky and the bounce of ground that has lost its sun.
#[test]
fn the_probes_under_the_cover_gather_no_sunlit_bounce() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // A closed deck: the cloud map and the layer's mean cover agree that no sun gets through.
    let closed = CloudSettings {
        shadow_map: true,
        overcast_ibl: true,
        pattern: CloudPattern::Constant,
        wind_m_per_s: [0.0, 0.0],
        ..CloudSettings::coverage_layer(1.0)
    };
    let (wall_flat, ground_flat) = wall_from_eye_height(&device, closed, false, "haze_probe_wall_deck_flat");
    let (wall, ground) = wall_from_eye_height(&device, closed, true, "haze_probe_wall_deck");
    // The same wall under a clear sky: there the probes do carry the sunlit ground's bounce.
    let (wall_clear, ground_clear) =
        wall_from_eye_height(&device, CloudSettings::default(), true, "haze_probe_wall_clear");
    eprintln!(
        "under the closed cover: wall {wall:.4} with the probes, {wall_flat:.4} with the image light alone ({:.2} times); ground {ground:.4} and {ground_flat:.4}; under a clear sky the probes' wall {wall_clear:.4}, ground {ground_clear:.4} (linear luma)",
        wall / wall_flat.max(1.0e-6)
    );
    assert!(
        wall_flat > 0.01 && wall_flat < 0.5,
        "the wall is not exposed for the measure ({wall_flat:.4})"
    );
    assert!(
        ground > 0.6 * ground_flat && ground < 1.5 * ground_flat,
        "the probes' open ground does not read the cover's sky: {ground:.4} against {ground_flat:.4} with the image light alone"
    );
    assert!(
        wall <= 1.6 * wall_flat,
        "the probes light the wall with a sunlit ground's bounce under the cover: {wall:.4} against {wall_flat:.4} with the image light alone"
    );
    assert!(
        wall > 0.5 * wall_flat,
        "the probes lost the cover's sky light too: {wall:.4} against {wall_flat:.4}"
    );
    assert!(
        wall_clear > 2.0 * wall,
        "under a clear sky the probes' wall takes the sunlit ground's bounce: {wall_clear:.4} against {wall:.4} under the cover"
    );
}
