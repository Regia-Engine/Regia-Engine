// SPDX-License-Identifier: MPL-2.0
//! Wet and snowy grass blades (spec 99 § 6.4, grass stage 3, ADR-3249) on the runtime frame path
//! (lavapipe): a [`GrassLayer`] on the shared terrain draw, the frame rendered by `FrameRenderer`.
//!
//! - A dry frame is the frame it always was: the grass compiles no weather pipeline, a frame after
//!   a wet spell is byte-identical to one that never saw weather, and dry frames compile nothing.
//! - Wet blades darken by the ground layer's response: over the blade pixels, the wet/dry ratio is
//!   the ratio of the bare ground under them within [`RATIO_TOLERANCE`].
//! - A sealed layer (porosity 0) does not darken its blades but glosses them: under a low sun in
//!   front of the camera, the wet blades brighten (the film's sheen) and no blade darkens.
//! - Snow whitens the blade tips by the layer's snow amount; a layer with `snow: 0.0` keeps green
//!   blades.
//! - The wind still moves wet blades, and the weather warm builds the variant before the first
//!   wet frame.
//!
//! `REGIA_WRITE_EVIDENCE=<dir>` writes the frames as PNGs under `<dir>/grass_weather/` (the
//! owner's plates: dry, wet, snow).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw, SsaoMode, SurfaceWeatherFrame,
};
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet, LayerWeather,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw,
};

const W: u32 = 320;
const H: u32 = 180;

/// How far the blades' wet/dry ratio may sit from the bare ground's. Both take the same albedo
/// factor `1 - 0.5 φ w` (φ = 1 on this rough layer, so 0.5 at full wetness); the frames differ
/// by what is not albedo: the ground's film sheen, the blades' back light (albedo squared) and
/// the display transform at the two brightness levels.
const RATIO_TOLERANCE: f32 = 0.12;

/// A rough brown ground (porosity 1 by the § 6.2 formula) with an optional weather block.
fn layers(weather: Option<LayerWeather>) -> TerrainLayers {
    let mut layers = TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "ground", albedo: (0.20, 0.16, 0.11), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .unwrap_or_else(|err| panic!("layers: {err}"));
    layers.layers[0].weather = weather;
    layers
}

/// 512 m of gently rolling ground.
fn map() -> TerrainMap {
    let n = 257u32;
    let world = 512.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let z = (row as f32 / (n - 1) as f32 - 0.5) * world;
            let x = (col as f32 / (n - 1) as f32 - 0.5) * world;
            heights.push(1.5 * (x * 0.021).sin() * (z * 0.017).cos() + 0.8 * (x * 0.07 + z * 0.05).sin());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).unwrap_or_else(|err| panic!("map: {err}"))
}

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU (lavapipe?): {err}"))
}

fn write_evidence(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("grass_weather");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(dir.join(name)) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// Where the sun comes from.
#[derive(Clone, Copy)]
enum Sun {
    /// Low, from the side (the grass field's light).
    Side,
    /// Low, ahead of the camera and travelling toward it: the film's sheen faces the eye.
    Ahead,
    /// No sun: the blades are lit by the sky alone, which they take without a specular term.
    Off,
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    handle: Option<TerrainGrassHandle>,
    map: TerrainMap,
    post: PostSettings,
    sun: Sun,
}

impl Rig {
    /// The terrain with `weather` on its layer, with summer grass (`grass`) or without.
    fn new(device: RenderDevice, grass: bool, weather: Option<LayerWeather>, warm: bool) -> Self {
        let map = map();
        let layers = layers(weather);
        let mut draw = TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 4)
            .unwrap_or_else(|err| panic!("terrain: {err}"));
        draw.set_warm_surface_weather(warm);
        let mut handle = None;
        if grass {
            let set = GrassSet::summer();
            let mask = GrassMask::build(
                &set,
                &GrassMaskInputs {
                    layers: Some(&layers),
                    texel_m: 1.0,
                    ..GrassMaskInputs::new(&map)
                },
            )
            .unwrap_or_else(|err| panic!("mask: {err}"));
            let layer = GrassLayer::new(&device.device, &device.queue, set, mask)
                .unwrap_or_else(|err| panic!("grass layer: {err}"));
            let h = layer.handle();
            h.set_request(GrassRequest {
                quality: Some(GrassQuality::High),
                ..GrassRequest::default()
            });
            draw.set_grass(Some(layer));
            handle = Some(h);
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        };
        Self {
            device,
            renderer,
            handle,
            map,
            post,
            sun: Sun::Side,
        }
    }

    fn frame(&mut self, weather: SurfaceWeatherFrame) -> Vec<u8> {
        self.renderer.set_surface_weather(weather);
        let (x, z) = (12.0, 40.0);
        let ground = self.map.sample_bilinear(x, z);
        let target = self.map.sample_bilinear(x, z - 30.0);
        let directional = match self.sun {
            Sun::Side => DirectionalLightUniform::new([0.75, -0.21, -0.62], [1.0, 0.93, 0.84], 60_000.0),
            Sun::Ahead => DirectionalLightUniform::new([0.05, -0.25, 0.97], [1.0, 0.93, 0.84], 60_000.0),
            Sun::Off => DirectionalLightUniform::new([0.05, -0.25, 0.97], [1.0, 0.93, 0.84], 0.0),
        };
        let lab = LabScene {
            width: W,
            height: H,
            eye: [x, ground + 1.2, z],
            target: [x, target + 0.2, z - 30.0],
            fov_y: 0.9,
            near: 0.05,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            directional,
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        quality.render_scale_permille = 1000;
        // No temporal or screen-space history (the terrain weather test's setup): a held frame
        // then draws the same bytes, so the comparisons below see the weather and nothing else.
        quality.ssao = SsaoMode::Off;
        quality.gi = GiMode::Ambient;
        quality.screen_space_gi = false;
        quality.gi_probes = false;
        quality.ssr = false;
        quality.volumetric_fog = false;
        quality.volumetric_clouds = false;
        quality.cloud_reprojection = false;
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&self.post);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }

    /// Three frames, the last returned (the shared draw's caches settle).
    fn settled(&mut self, weather: SurfaceWeatherFrame) -> Vec<u8> {
        for _ in 0..2 {
            self.frame(weather);
        }
        self.frame(weather)
    }

    fn grass_report(&self) -> regia_terrain::GrassReport {
        self.handle
            .as_ref()
            .map(TerrainGrassHandle::report)
            .unwrap_or_default()
    }
}

const DRY: SurfaceWeatherFrame = SurfaceWeatherFrame::DRY;

fn wet() -> SurfaceWeatherFrame {
    SurfaceWeatherFrame { wetness: 1.0, ..DRY }
}

fn snow() -> SurfaceWeatherFrame {
    SurfaceWeatherFrame {
        snow_cover: 1.0,
        ..DRY
    }
}

/// Pixels where the grass rig's dry frame differs from the bare ground's: the blades.
fn blade_mask(grass: &[u8], bare: &[u8]) -> Vec<bool> {
    (0..(W * H) as usize)
        .map(|i| {
            let d: i32 = (0..3)
                .map(|k| (i32::from(grass[i * 4 + k]) - i32::from(bare[i * 4 + k])).abs())
                .sum();
            d > 12
        })
        .collect()
}

/// sRGB byte to linear.
fn linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Mean linear luminance over the masked pixels.
fn mean_luma(pixels: &[u8], mask: &[bool]) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for (i, on) in mask.iter().enumerate() {
        if *on {
            let at = i * 4;
            sum += 0.2126 * linear(pixels[at])
                + 0.7152 * linear(pixels[at + 1])
                + 0.0722 * linear(pixels[at + 2]);
            n += 1.0;
        }
    }
    sum / n.max(1.0)
}

/// Serialises device use across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The dry blade draw has none of the weather code, the weather draw has it, and the dry text is
/// the composition the field always compiled (the stripper is `without_surface_weather`).
#[test]
fn the_dry_blade_source_has_none_of_the_weather() {
    use regia_terrain::gpu::grass::{draw_source, weather_draw_source};
    let terrain = include_str!("../src/gpu/terrain.wgsl");
    let dry = draw_source(&regia_terrain::gpu::without_surface_weather(terrain));
    let full = weather_draw_source(terrain);
    for name in [
        "grass_weather",
        "grass_ground_response",
        "blade_rough",
        "material.weather",
    ] {
        assert!(!dry.contains(name), "the dry draw holds `{name}`");
        assert!(full.contains(name), "the weather draw lacks `{name}`");
    }
    assert!(dry.contains("let albedo = in.albedo;"));
    assert!(dry.contains("let rough = clamp(cls.base.w, 0.04, 1.0);"));
}

/// Dry frames compile no weather pipeline and keep their bytes: a rig that went wet and back is
/// byte-identical to one that never saw weather, and dry frames after the first compile nothing.
#[test]
fn a_dry_frame_keeps_its_bytes_and_pipelines() {
    let _gpu = serial();
    let device = device();
    let mut never = Rig::new(device.clone(), true, None, false);
    let first = never.frame(DRY);
    let before = regia_render::pipeline_cache::pipelines_created();
    let dry = never.settled(DRY);
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        before,
        "a dry frame compiled a pipeline"
    );
    let report = never.grass_report();
    assert!(report.drawing, "{report:?}");
    assert!(!report.surface_weather, "{report:?}");
    assert_eq!(report.weather_frames, 0, "{report:?}");
    assert_eq!(first.len(), dry.len());
    // The same rig, dry, wet, then dry again: the first dry picture (the terrain's own rule).
    let mut spell = Rig::new(device.clone(), true, None, false);
    let before = spell.settled(DRY);
    let wet_frame = spell.settled(wet());
    assert!(spell.grass_report().surface_weather);
    assert_ne!(wet_frame, before, "the wet frame changed nothing");
    let back = spell.settled(DRY);
    assert!(!spell.grass_report().surface_weather);
    assert!(
        back == before,
        "dry again differs in {} pixels",
        differing(&back, &before)
    );
    // And a rig that never saw weather, drawn the same number of frames, draws it too.
    let mut never_wet = Rig::new(device, true, None, false);
    let mut same = Vec::new();
    for _ in 0..9 {
        same = never_wet.frame(DRY);
    }
    assert!(
        same == back,
        "the never-wet rig differs in {} pixels",
        differing(&same, &back)
    );
}

/// Pixels whose bytes differ.
fn differing(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p != q)
        .count()
}

/// Mean linear luminance of the brightest `share` of the masked pixels (the highlights).
fn highlight_luma(pixels: &[u8], mask: &[bool], share: f32) -> f32 {
    let mut lumas: Vec<f32> = mask
        .iter()
        .enumerate()
        .filter(|(_, on)| **on)
        .map(|(i, _)| {
            let at = i * 4;
            0.2126 * linear(pixels[at]) + 0.7152 * linear(pixels[at + 1]) + 0.0722 * linear(pixels[at + 2])
        })
        .collect();
    lumas.sort_by(f32::total_cmp);
    let keep = ((lumas.len() as f32 * share).ceil() as usize).clamp(1, lumas.len().max(1));
    let top = &lumas[lumas.len().saturating_sub(keep)..];
    top.iter().sum::<f32>() / top.len().max(1) as f32
}

/// The largest 8-bit step a sealed layer's wet blade may move by under the sky alone (ADR-5123):
/// the two frames come from two compiled pipelines, and llvmpipe rounds them one step apart.
const SEALED_MAX_STEP: u8 = 1;

/// At most one blade pixel in this many may differ at all between the sealed wet and dry frames
/// (0.1 %, ADR-5123).
const SEALED_CHANGED_ONE_IN: usize = 1000;

/// Masked pixels whose RGB bytes differ between `a` and `b`, and the largest per-channel
/// difference among them, in 8-bit steps.
fn sealed_rounding(mask: &[bool], a: &[u8], b: &[u8]) -> (usize, u8) {
    let (mut changed, mut largest) = (0, 0u8);
    for (i, on) in mask.iter().enumerate() {
        if !*on {
            continue;
        }
        let step = (0..3)
            .map(|c| a[i * 4 + c].abs_diff(b[i * 4 + c]))
            .max()
            .unwrap_or(0);
        if step > 0 {
            changed += 1;
            largest = largest.max(step);
        }
    }
    (changed, largest)
}

/// The sealed-layer bound counts what it claims: the alpha channel and unmasked pixels are
/// ignored, a one-step pixel counts as changed, and a two-step pixel is reported as two.
#[test]
fn the_sealed_bound_measures_masked_rgb_steps() {
    let mask = [true, true, false, true];
    let a = [10, 20, 30, 255, 10, 20, 30, 255, 0, 0, 0, 255, 50, 60, 70, 255];
    let mut b = a;
    b[3] = 0; // alpha only: not a change
    b[8] = 200; // unmasked pixel: not counted
    assert_eq!(sealed_rounding(&mask, &a, &b), (0, 0));
    b[5] = 21; // pixel 1, green one step
    assert_eq!(sealed_rounding(&mask, &a, &b), (1, 1));
    b[14] = 68; // pixel 3, blue two steps
    assert_eq!(sealed_rounding(&mask, &a, &b), (2, 2));
}

/// Wet blades darken by the ground layer's ratio: the blades' wet/dry ratio is the bare ground's.
#[test]
fn wet_blades_darken_like_the_ground_under_them() {
    let _gpu = serial();
    let device = device();
    let mut grass = Rig::new(device.clone(), true, None, false);
    let mut bare = Rig::new(device, false, None, false);
    let grass_dry = grass.settled(DRY);
    let grass_wet = grass.settled(wet());
    let bare_dry = bare.settled(DRY);
    let bare_wet = bare.settled(wet());
    write_evidence("dry.png", &grass_dry);
    write_evidence("wet.png", &grass_wet);
    let mask = blade_mask(&grass_dry, &bare_dry);
    let blades = mask.iter().filter(|m| **m).count();
    assert!(blades > (W * H / 10) as usize, "only {blades} blade pixels");
    let blade_ratio = mean_luma(&grass_wet, &mask) / mean_luma(&grass_dry, &mask);
    let ground_ratio = mean_luma(&bare_wet, &mask) / mean_luma(&bare_dry, &mask);
    println!(
        "wet/dry luminance: blades {blade_ratio:.3}, bare ground under them {ground_ratio:.3}; brightest 1 % of \
         the blades dry {:.4}, wet {:.4}",
        highlight_luma(&grass_dry, &mask, 0.01),
        highlight_luma(&grass_wet, &mask, 0.01)
    );
    assert!(blade_ratio < 0.9, "wet blades did not darken: {blade_ratio}");
    assert!(
        (blade_ratio - ground_ratio).abs() <= RATIO_TOLERANCE,
        "blades {blade_ratio:.3} against the ground {ground_ratio:.3} (tolerance {RATIO_TOLERANCE})"
    );
    let report = grass.grass_report();
    assert!(report.surface_weather && report.weather_frames >= 3, "{report:?}");
}

/// A sealed layer (porosity 0) keeps its wet blades' albedo and changes only their gloss. Under the
/// sky alone (no sun, so no specular term on a blade) its wet blades draw the dry bytes, while the
/// porous default darkens them; with the sun ahead the film's roughness changes the blades' sheen.
/// The wet and dry frames come from two compiled pipelines (the weather variant and the stripped
/// one), so the sky-alone check allows the rounding of two programs (ADR-5123): no blade pixel may
/// move by more than one 8-bit step in any channel, and at most 0.1 % of blade pixels may move at
/// all. The porous layer darkens the blades by about half, far outside that bound.
/// (Under this low back light the narrower lobe of the film dims the average and even the brightest
/// 1 % of blade pixels: the numbers are printed for the report and the owner's plates.)
#[test]
fn a_sealed_layer_keeps_its_blades_albedo_and_changes_their_gloss() {
    let _gpu = serial();
    let device = device();
    let sealed = Some(LayerWeather {
        porosity: Some(0.0),
        ..LayerWeather::default()
    });
    let rig = |weather, sun| {
        let mut rig = Rig::new(device.clone(), true, weather, false);
        rig.sun = sun;
        rig
    };
    let mut bare = Rig::new(device.clone(), false, sealed, false);
    bare.sun = Sun::Off;
    let mut sealed_sky = rig(sealed, Sun::Off);
    let dry = sealed_sky.settled(DRY);
    let wet_frame = sealed_sky.settled(wet());
    let mask = blade_mask(&dry, &bare.settled(DRY));
    let blades = mask.iter().filter(|m| **m).count();
    assert!(blades > (W * H / 10) as usize, "too few blade pixels");
    let (changed, largest_step) = sealed_rounding(&mask, &dry, &wet_frame);
    assert!(
        largest_step <= SEALED_MAX_STEP,
        "a sealed layer's wet blades moved a pixel by {largest_step} 8-bit steps under the sky \
         alone (at most {SEALED_MAX_STEP}: its albedo must stay)"
    );
    assert!(
        changed * SEALED_CHANGED_ONE_IN <= blades,
        "a sealed layer's wet blades changed {changed} of {blades} pixels under the sky alone \
         (at most 0.1 % may round apart)"
    );
    let mut porous_sky = rig(None, Sun::Off);
    let porous_dry = porous_sky.settled(DRY);
    let porous_wet = porous_sky.settled(wet());
    let ratio = mean_luma(&porous_wet, &mask) / mean_luma(&porous_dry, &mask);
    assert!(ratio < 0.9, "porous blades under the sky alone: wet/dry {ratio}");
    let mut sealed_sun = rig(sealed, Sun::Ahead);
    let sun_dry = sealed_sun.settled(DRY);
    let sun_wet = sealed_sun.settled(wet());
    let sheen = (0..mask.len())
        .filter(|&i| mask[i] && sun_dry[i * 4..i * 4 + 3] != sun_wet[i * 4..i * 4 + 3])
        .count();
    println!(
        "sealed layer: sky alone, wet blades change {changed} of {blades} pixels by at most {largest_step}; porous wet/dry {ratio:.3}; sun ahead, the film \
         changes {sheen} blade pixels, mean dry {:.4} wet {:.4}, brightest 1 % dry {:.4} wet {:.4}",
        mean_luma(&sun_dry, &mask),
        mean_luma(&sun_wet, &mask),
        highlight_luma(&sun_dry, &mask, 0.01),
        highlight_luma(&sun_wet, &mask, 0.01)
    );
    assert!(sheen > 100, "the film changed {sheen} blade pixels under the sun");
}

/// Snow whitens the blade tips by the layer's snow amount: the blades brighten under snow, and a
/// layer with `snow: 0.0` keeps its blades as they were.
#[test]
fn snow_whitens_tips_by_the_layers_snow_amount() {
    let _gpu = serial();
    let device = device();
    let mut grass = Rig::new(device.clone(), true, None, false);
    let mut bare = Rig::new(device.clone(), false, None, false);
    let dry = grass.settled(DRY);
    let snowy = grass.settled(snow());
    write_evidence("snow.png", &snowy);
    let mask = blade_mask(&dry, &bare.settled(DRY));
    let (dry_luma, snow_luma) = (mean_luma(&dry, &mask), mean_luma(&snowy, &mask));
    println!("snow: blades dry {dry_luma:.4}, snowy {snow_luma:.4}");
    assert!(
        snow_luma > dry_luma * 1.2,
        "snow did not whiten the blades: {dry_luma} -> {snow_luma}"
    );
    // Whiter, not just brighter: the blades' green lead over blue shrinks.
    let chroma = |pixels: &[u8]| {
        let (mut g, mut b, mut n) = (0.0f32, 0.0f32, 0.0f32);
        for (i, on) in mask.iter().enumerate() {
            if *on {
                g += linear(pixels[i * 4 + 1]);
                b += linear(pixels[i * 4 + 2]);
                n += 1.0;
            }
        }
        (g - b) / n.max(1.0) / (g / n.max(1.0)).max(1e-6)
    };
    assert!(chroma(&snowy) < chroma(&dry), "snow left the blades as green");
    // A layer that holds no snow keeps green blades (the ground stays bare too).
    let bare_layer = Some(LayerWeather {
        snow: 0.0,
        ..LayerWeather::default()
    });
    let mut warm_road = Rig::new(device, true, bare_layer, false);
    let road_dry = warm_road.settled(DRY);
    let road_snow = warm_road.settled(snow());
    let (a, b) = (mean_luma(&road_dry, &mask), mean_luma(&road_snow, &mask));
    println!("snow: 0.0 layer: blades dry {a:.4}, snowy {b:.4}");
    assert!(
        (b / a - 1.0).abs() < 0.02,
        "a layer with snow 0 whitened its blades: {a} -> {b}"
    );
}

/// The wind still moves wet blades: two clocks give two frames, a held clock the same frame.
#[test]
fn wind_still_moves_wet_blades() {
    let _gpu = serial();
    let mut grass = Rig::new(device(), true, None, false);
    let handle = grass.handle.clone().unwrap_or_else(|| panic!("no grass handle"));
    handle.set_time(1.0);
    let a = grass.settled(wet());
    let held = grass.frame(wet());
    handle.set_time(1.7);
    let b = grass.settled(wet());
    assert!(grass.grass_report().surface_weather);
    assert!(a == held, "a held clock moved the wet blades");
    let moved = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p != q)
        .count();
    assert!(moved > 200, "the wind moved {moved} wet-blade pixels");
}

/// With the weather warm on, the blades' variant is built before the first wet frame, which then
/// compiles nothing; with it off, the first wet frame builds it.
#[test]
fn the_weather_warm_builds_the_blades_variant() {
    let _gpu = serial();
    let device = device();
    let mut warmed = Rig::new(device.clone(), true, None, true);
    warmed.frame(DRY);
    warmed.renderer.warm_shared_pipelines();
    let before = regia_render::pipeline_cache::pipelines_created();
    warmed.settled(wet());
    assert!(warmed.grass_report().surface_weather);
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        before,
        "the first wet frame compiled"
    );
    let mut cold = Rig::new(device, true, None, false);
    cold.frame(DRY);
    cold.renderer.warm_shared_pipelines();
    let before = regia_render::pipeline_cache::pipelines_created();
    cold.settled(wet());
    assert!(
        regia_render::pipeline_cache::pipelines_created() > before,
        "without the warm the first wet frame should build the variants"
    );
}
