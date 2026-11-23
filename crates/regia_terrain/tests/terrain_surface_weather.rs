// SPDX-License-Identifier: MPL-2.0
//! The surface weather on the terrain (spec 99 § 11): compiled out of the shared colour pass on a
//! dry frame, darker when wet, snow on flat ground before slopes, hollows pooling before flats.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    SsaoMode, SurfaceWeatherFrame,
};
use regia_terrain::gpu::{shared_source, without_surface_weather};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, LayerWeather, MaterialParams, Rgba8Image, TerrainLayers,
    TerrainMap, TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

/// Serialises device use across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

const W: u32 = 160;
const H: u32 = 90;
const EDGE: usize = 129;
const WORLD_M: f32 = 128.0;

/// The one-layer grey stack of [`ground`].
fn grey_stack() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    }
}

/// A grey photoreal ground over `heights(x, z)` (world metres).
fn ground(heights: impl Fn(f32, f32) -> f32) -> TerrainSharedDraw {
    ground_with(heights, &grey_stack())
}

/// [`ground`] with its own layer stack: every layer gets the same grey texture.
fn ground_with(heights: impl Fn(f32, f32) -> f32, stack: &TerrainLayers) -> TerrainSharedDraw {
    let mut samples = Vec::with_capacity(EDGE * EDGE);
    for j in 0..EDGE {
        for i in 0..EDGE {
            let x = i as f32 - 64.0;
            let z = j as f32 - 64.0;
            samples.push(heights(x, z));
        }
    }
    let map = TerrainMap::from_heights(EDGE as u32, EDGE as u32, samples, WORLD_M, 80.0, -40.0).expect("map");
    let v = regia_terrain::linear_to_srgb8(0.3);
    let mut draw = TerrainSharedDraw::new(device(), map, stack, CdlodConfig::default(), 3).expect("terrain");
    let grey = || {
        Some(LayerTextures {
            albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
            normal: Rgba8Image::neutral_normal(64),
            height: Rgba8Image::neutral_height(64),
            orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
            use_orm: false,
        })
    };
    draw.set_material(
        &TerrainMaterial {
            layers: (0..stack.layers.len()).map(|_| grey()).collect(),
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    draw
}

fn renderer(draw: TerrainSharedDraw) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer
}

/// One frame of the shared draw (Desktop High without history passes).
fn frame(renderer: &mut FrameRenderer) -> Vec<u8> {
    frame_sized(renderer, W, H)
}

fn frame_sized(renderer: &mut FrameRenderer, width: u32, height: u32) -> Vec<u8> {
    let post = PostSettings {
        exposure_ev: 11.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width,
        height,
        eye: [0.0, 12.0, -40.0],
        target: [0.0, 0.0, 10.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 11.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.2, -0.7, 0.6], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.ssao = SsaoMode::Off;
    quality.gi = GiMode::Ambient;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.cloud_reprojection = false;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    renderer.render_offscreen(device(), &scene).expect("frame").pixels
}

fn settled(renderer: &mut FrameRenderer) -> Vec<u8> {
    for _ in 0..3 {
        frame(renderer);
    }
    frame(renderer)
}

/// Mean brightness of the pixels of columns `x0..x1`, rows `y0..y1`.
fn mean(pixels: &[u8], (x0, x1): (u32, u32), (y0, y1): (u32, u32)) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let at = ((y * W + x) * 4) as usize;
            sum += f32::from(pixels[at]) + f32::from(pixels[at + 1]) + f32::from(pixels[at + 2]);
            n += 3.0;
        }
    }
    sum / n
}

const DRY: SurfaceWeatherFrame = SurfaceWeatherFrame::DRY;

fn wet(wetness: f32, puddles: f32) -> SurfaceWeatherFrame {
    SurfaceWeatherFrame {
        wetness,
        puddles,
        ..DRY
    }
}

fn snow(cover: f32) -> SurfaceWeatherFrame {
    SurfaceWeatherFrame {
        snow_cover: cover,
        ..DRY
    }
}

/// The stripped source has none of the rule, the full one all of it, and the stripper is what
/// `shared_source`'s dry path runs.
#[test]
fn the_stripped_source_has_none_of_the_weather() {
    for (name, full) in [
        ("shared", shared_source(false, false)),
        ("shared, lights", shared_source(false, true)),
        ("shared, hw height, lights", shared_source(true, true)),
    ] {
        let off = without_surface_weather(&full);
        for word in [
            "weather_apply(",
            "engine_precip",
            "material.weather",
            "weather_exposure",
        ] {
            assert!(full.contains(word), "{name}: lost `{word}`");
            assert!(!off.contains(word), "{name}: `{word}` survives");
        }
    }
}

/// Rows zero: the variant is off and the bytes are the frame without the feature. Wet: the
/// variant turns on and the ground darkens. Dry again: the first bytes.
#[test]
fn a_dry_frame_keeps_its_bytes_and_a_wet_one_darkens() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut renderer = renderer(ground(|_, _| 0.0));
    let plain = settled(&mut renderer);
    assert!(!renderer.surface_weather_variant_on(), "dry: the stripped shader");
    renderer.set_surface_weather(wet(1.0, 0.0));
    let soaked = settled(&mut renderer);
    assert!(renderer.surface_weather_variant_on(), "wet: the variant");
    let window = (40, 120);
    let rows = (50, 80);
    assert!(
        mean(&soaked, window, rows) < 0.9 * mean(&plain, window, rows),
        "{} vs {}",
        mean(&soaked, window, rows),
        mean(&plain, window, rows)
    );
    renderer.set_surface_weather(DRY);
    let again = settled(&mut renderer);
    assert!(!renderer.surface_weather_variant_on());
    assert_eq!(plain, again, "dry again draws the dry picture");
}

/// Snow settles on the flat half and not on a 50° slope.
#[test]
fn snow_covers_flat_ground_before_a_slope() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Flat for x < 0, rising 1.19 m per m (50°) for x > 0.
    let mut renderer = renderer(ground(|x, _| 1.19 * x.clamp(0.0, 30.0)));
    let dry = settled(&mut renderer);
    renderer.set_surface_weather(snow(1.0));
    let covered = settled(&mut renderer);
    // The camera looks along +z, so -x (flat) is to the right of the frame.
    let flat = (110, 150);
    let slope = (10, 60);
    let rows = (45, 85);
    let lift = |x| mean(&covered, x, rows) - mean(&dry, x, rows);
    assert!(lift(flat) > 10.0, "flat ground: {}", lift(flat));
    assert!(
        lift(flat) > 1.5 * lift(slope),
        "{} vs {}",
        lift(flat),
        lift(slope)
    );
}

/// A rut pools before the flat ground beside it.
#[test]
fn hollows_pool_before_flats() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A rut 3 m wide and 15 cm deep along z, left of the frame's center.
    let mut renderer = renderer(ground(|x, _| if (10.0..14.0).contains(&x) { -0.15 } else { 0.0 }));
    // At half wetness a pool is darker than the film around it.
    renderer.set_surface_weather(wet(0.5, 0.0));
    let film = settled(&mut renderer);
    renderer.set_surface_weather(wet(0.5, 0.2));
    let pooled = settled(&mut renderer);
    let change = |x0: u32, x1: u32| {
        let mut moved = 0;
        for y in 50..80 {
            for x in x0..x1 {
                let at = ((y * W + x) * 4) as usize;
                if pooled[at].abs_diff(film[at]) > 3 {
                    moved += 1;
                }
            }
        }
        moved
    };
    let rut = change(0, W / 2);
    let flat = change(W - W / 4, W);
    assert!(rut > 2 * flat.max(1), "rut {rut} vs flat {flat}");
}

/// The precipitation's extinction fades the far ground and turns the variant on.
#[test]
fn the_extinction_reaches_the_ground() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut renderer = renderer(ground(|_, _| 0.0));
    let plain = settled(&mut renderer);
    renderer.set_precipitation_extinction(0.03);
    let thick = settled(&mut renderer);
    assert!(renderer.surface_weather_variant_on());
    assert_ne!(plain, thick);
}

/// The ground is in the occlusion tile: the map reads its height.
#[test]
fn the_ground_is_in_the_occlusion_tile() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut renderer = renderer(ground(|_, _| 0.0));
    renderer.set_surface_weather(wet(1.0, 0.0));
    renderer.set_precipitation_occlusion_budget(Some((256, 32.0)), 16_384);
    renderer.request_precip_occlusion_readback();
    frame(&mut renderer);
    let map = renderer.take_precip_occlusion_readback().expect("copied");
    let (open, cover) = map.exposure_at([0.0, 0.5, -40.0]);
    assert_eq!((open, cover), (1.0, None), "above the ground: open");
    let (below, cover) = map.exposure_at([0.0, -2.0, -40.0]);
    assert_eq!(below, 0.0, "under the ground: covered");
    let cover = cover.expect("the ground is above");
    assert!(cover.abs() < 0.1, "{cover}");
}

/// With the warm on, the first wet frame compiles nothing (spec 99 § 6.4).
#[test]
fn a_warmed_project_compiles_nothing_on_its_first_wet_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut draw = ground(|_, _| 0.0);
    draw.set_warm_surface_weather(true);
    let mut renderer = renderer(draw);
    frame(&mut renderer);
    renderer.warm_shared_pipelines();
    let before = regia_render::pipeline_cache::pipelines_created();
    renderer.set_surface_weather(wet(1.0, 0.0));
    settled(&mut renderer);
    assert!(renderer.surface_weather_variant_on());
    assert_eq!(regia_render::pipeline_cache::pipelines_created(), before);
}

/// Two flat terraces, the left one 5 m up (camera looks along +z, so +x is the frame's left):
/// a "low" layer below 2 m and a "high" one above. `high` is the high layer's weather block.
fn terraces(high: Option<LayerWeather>) -> TerrainSharedDraw {
    let layer = |name: &str, lo: f32, hi: f32| LayerDesc {
        name: name.into(),
        albedo: [1.0, 1.0, 1.0],
        roughness: 1.0,
        slope_min: 0.0,
        slope_max: 1.6,
        altitude_min: lo,
        altitude_max: hi,
        triplanar: false,
        ..LayerDesc::ground()
    };
    let stack = TerrainLayers {
        layers: vec![
            layer("low", -100.0, 2.0),
            LayerDesc {
                weather: high,
                ..layer("high", 2.0, 100.0)
            },
        ],
        ..Default::default()
    };
    ground_with(|x, _| (2.5 * x).clamp(0.0, 5.0), &stack)
}

const HIGH: (u32, u32) = (10, 60);
const LOW: (u32, u32) = (110, 150);

/// `.terrain.layers.ron` v4 (spec 99 § 7.3): a layer whose block sets `snow: 0.0` stays bare in
/// a snowfall while the layer beside it whitens; the same stack without the block whitens both.
#[test]
fn a_layer_with_snow_off_stays_bare() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rows = (45, 85);
    let lifts = |high: Option<LayerWeather>| {
        let mut renderer = renderer(terraces(high));
        let dry = settled(&mut renderer);
        renderer.set_surface_weather(snow(1.0));
        let covered = settled(&mut renderer);
        let lift = |x| mean(&covered, x, rows) - mean(&dry, x, rows);
        (lift(HIGH), lift(LOW))
    };
    let (high, low) = lifts(None);
    assert!(
        high > 10.0 && low > 10.0,
        "default: both terraces whiten ({high}, {low})"
    );
    // A bare block is the default response, through the no-override path.
    let (bare_high, bare_low) = lifts(Some(LayerWeather::default()));
    assert_eq!((bare_high, bare_low), (high, low), "a bare block changes nothing");
    let (high, low) = lifts(Some(LayerWeather {
        porosity: None,
        snow: 0.0,
        porosity_from_orm: false,
    }));
    assert!(low > 10.0, "the low layer still whitens: {low}");
    assert!(high.abs() < 1.0, "snow 0 keeps the high layer bare: {high}");
}

/// A layer that names a low porosity darkens less in rain than the derived porosity of its
/// roughness (1.0 → porosity 1, half the albedo): sand soaks, rock sheds a film.
#[test]
fn a_low_porosity_layer_darkens_less() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rows = (45, 85);
    let ratios = |high: Option<LayerWeather>| {
        let mut renderer = renderer(terraces(high));
        let dry = settled(&mut renderer);
        renderer.set_surface_weather(wet(1.0, 0.0));
        let soaked = settled(&mut renderer);
        let ratio = |x| mean(&soaked, x, rows) / mean(&dry, x, rows);
        (ratio(HIGH), ratio(LOW))
    };
    let (high, low) = ratios(None);
    // The same bound as `a_dry_frame_keeps_its_bytes_and_a_wet_one_darkens` (measured 0.81 here).
    assert!(high < 0.9 && low < 0.9, "default: both darken ({high}, {low})");
    let (shed_high, shed_low) = ratios(Some(LayerWeather {
        porosity: Some(0.0),
        snow: 1.0,
        porosity_from_orm: false,
    }));
    assert!(
        (shed_low - low).abs() < 0.02,
        "the low layer is unchanged: {shed_low} vs {low}"
    );
    assert!(
        shed_high > high + 0.05,
        "porosity 0 darkens less: {shed_high} vs {high}"
    );
}

/// A stack with `puddles: false` holds no pools: the puddled frame is the wet-film frame, byte
/// for byte, where the default stack pools in the rut.
#[test]
fn a_stack_without_puddles_holds_none() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rut = |x: f32, _z: f32| if (10.0..14.0).contains(&x) { -0.15 } else { 0.0 };
    let film_and_pooled = |puddles: bool| {
        let stack = TerrainLayers {
            puddles,
            ..grey_stack()
        };
        let mut renderer = renderer(ground_with(rut, &stack));
        renderer.set_surface_weather(wet(0.5, 0.0));
        let film = settled(&mut renderer);
        renderer.set_surface_weather(wet(0.5, 0.6));
        (film, settled(&mut renderer))
    };
    let (film, pooled) = film_and_pooled(true);
    assert_ne!(film, pooled, "the default stack pools");
    let (film, pooled) = film_and_pooled(false);
    assert_eq!(film, pooled, "puddles: false never pools");
}

/// Writes the wet, puddled and snowy ground to `REGIA_WEATHER_SHEET_DIR` for a look by eye.
/// Not a test of the picture:
/// `cargo test -p regia_terrain --test terrain_surface_weather sheet -- --ignored`.
#[test]
#[ignore = "writes images for a look by eye"]
fn sheet() {
    let Some(dir) = std::env::var_os("REGIA_WEATHER_SHEET_DIR") else {
        return;
    };
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rolling =
        |x: f32, z: f32| 0.4 * (x * 0.21).sin() * (z * 0.17).cos() + 0.15 * (x * 0.9 + z * 0.7).sin();
    let mut renderer = renderer(ground(rolling));
    renderer.set_outdoor(OutdoorParams::default());
    let states = [
        ("dry", DRY),
        ("wet", wet(1.0, 0.0)),
        (
            "puddles",
            SurfaceWeatherFrame {
                ripple: 1.0,
                clock_s: 3.0,
                ..wet(1.0, 0.8)
            },
        ),
        ("thin_snow", snow(0.4)),
        ("snow", snow(1.0)),
    ];
    for (name, state) in states {
        renderer.set_surface_weather(state);
        for _ in 0..3 {
            frame_sized(&mut renderer, 640, 360);
        }
        let pixels = frame_sized(&mut renderer, 640, 360);
        let path = std::path::Path::new(&dir).join(format!("terrain_{name}.png"));
        let file = std::fs::File::create(&path).expect("file");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 640, 360);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("header")
            .write_image_data(&pixels)
            .expect("pixels");
    }
}

/// Rain of this extinction (1/m) for [`rain_under_a_closed_cover_scatters_the_covers_light`]:
/// over the view's 20 to 85 m the far ground is mostly the rain's veil, the near ground mostly
/// ground.
const COVER_RAIN_PER_M: f32 = 0.02;

/// A renderer over Earth's air with `clouds` overhead and rain of [`COVER_RAIN_PER_M`] on flat
/// grey ground.
fn rain_renderer(clouds: regia_render::CloudSettings) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(OutdoorParams {
        clouds,
        planet: AtmosphereParameters::earth(),
        precip_extinction_per_m: COVER_RAIN_PER_M,
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(
        Box::new(ground(|_, _| 0.0)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    renderer
}

/// One frame of [`rain_renderer`]'s ground under a 100 klx sun, exposed at `ev`: the mean
/// brightness of the near ground (the bottom rows, about 20 m away) and of the far ground (the
/// rows 60 to 85 m away, under the terrain's far edge).
fn rain_frame(renderer: &mut FrameRenderer, ev: f32) -> (f32, f32) {
    let pixels = cover_frame(renderer, ev);
    (
        mean(&pixels, (W / 4, 3 * W / 4), (H - 10, H)),
        mean(&pixels, (W / 4, 3 * W / 4), (36, 42)),
    )
}

/// One frame of the shared draw over Earth's air under a 100 klx sun 48° up behind the camera,
/// exposed at `ev`, with the sky and the clouds drawn (Desktop High without history passes and
/// without the GI probes: the terrain's analytic sky and ground light).
fn cover_frame(renderer: &mut FrameRenderer, ev: f32) -> Vec<u8> {
    let post = PostSettings {
        exposure_ev: ev,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 12.0, -40.0],
        target: [0.0, 0.0, 10.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.2, -0.7, 0.6], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.ssao = SsaoMode::Off;
    quality.gi = GiMode::Ambient;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = true;
    quality.physical_atmosphere = true;
    quality.cloud_reprojection = false;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    renderer.render_offscreen(device(), &scene).expect("frame").pixels
}

/// The exposure (EV100) at which [`rain_frame`] shows the near ground at `target`, by bisection
/// (a lower EV is a brighter frame), as an auto exposure would settle: the EV, the near and the
/// far ground there.
fn rain_exposed_to(renderer: &mut FrameRenderer, target: f32) -> (f32, f32, f32) {
    let (mut bright_ev, mut dim_ev) = (4.0_f32, 20.0_f32);
    let mut best = (0.0_f32, f32::MAX, 0.0_f32, 0.0_f32);
    for _ in 0..12 {
        let ev = 0.5 * (bright_ev + dim_ev);
        let (near, far) = rain_frame(renderer, ev);
        let miss = (near - target).abs();
        if miss < best.1 {
            best = (ev, miss, near, far);
        }
        if miss < 1.0 {
            break;
        }
        if near > target {
            bright_ev = ev;
        } else {
            dim_ev = ev;
        }
    }
    let (ev, miss, near, far) = best;
    assert!(
        miss < 4.0,
        "no exposure puts the near ground at {target:.1} (closest {near:.1} at EV {ev:.2})"
    );
    (ev, near, far)
}

/// Rain under a closed cloud cover scatters the light under the cover (ADR-5944). The terrain's
/// rain veil mixes the far ground toward the fog's colour at a clear day's sky radiance; under a
/// cover whose shadow the ground takes from the cloud transmittance map, the veil falls with the
/// map too. Exposed so the near ground sits where the clear sky's frame has it, the far ground
/// (mostly veil) is then no brighter than under the clear sky. Before, the veil stayed at the
/// clear day's radiance over ground that had lost its sun, and the far ground washed out.
#[test]
fn rain_under_a_closed_cover_scatters_the_covers_light() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut clear = rain_renderer(regia_render::CloudSettings::default());
    for _ in 0..3 {
        rain_frame(&mut clear, 14.0);
    }
    let (clear_ev, clear_near, clear_far) = rain_exposed_to(&mut clear, 120.0);
    assert!(clear.surface_weather_variant_on());
    eprintln!("rain under a clear sky: near {clear_near:.1} far {clear_far:.1} at EV {clear_ev:.2}");
    assert!(
        (40.0..=200.0).contains(&clear_far),
        "the clear frame is not a usable reference (near {clear_near:.1}, far {clear_far:.1})"
    );
    // A closed stratus deck in still air with the cloud map on (the map alone: the ground keeps
    // the sky's share of its light, the share the veil's floor stands for).
    let deck = regia_render::CloudSettings {
        shadow_map: true,
        pattern: regia_render::CloudPattern::Constant,
        wind_m_per_s: [0.0, 0.0],
        ..regia_render::CloudSettings::coverage_layer(0.95)
    };
    let mut covered = rain_renderer(deck);
    for _ in 0..3 {
        rain_frame(&mut covered, clear_ev);
    }
    let (ev, near, far) = rain_exposed_to(&mut covered, clear_near);
    eprintln!(
        "rain under the closed cover: near {near:.1} far {far:.1} at EV {ev:.2} ({:.2} of the clear sky's far ground)",
        far / clear_far
    );
    assert!(
        ev < clear_ev - 0.5,
        "the cover does not shadow the ground (the near ground needs EV {ev:.2} against {clear_ev:.2})"
    );
    assert!(
        far <= 1.15 * clear_far,
        "the far ground washed out in the rain under the cover: {far:.1} against {clear_far:.1} under a clear sky, the near ground at {near:.1} and {clear_near:.1}"
    );
}

/// A cliff under a closed cloud cover takes no sunlit ground's bounce (ADR-5944 item 10). On the
/// analytic path (no GI probes) a sloped face takes the bounce of the ground in front of it, lit
/// by the sun where the cascades see it sunlit (`ground_sun_ahead`). The cascades do not hold the
/// cloud layer: under a cover whose shadow the face and the ground take from the cloud
/// transmittance map, the face kept the bounce of a sunlit ground and outshone the ground it
/// stands on. With the map the sunlit share of that ground falls with the map at the face.
#[test]
fn a_cliff_under_a_closed_cover_takes_no_sunlit_grounds_bounce() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Flat ground and, 46 m ahead of the camera, a 24 m cliff rising over 4 m: it faces the
    // camera and the sun behind it, so only the cover takes its sun.
    let cliff = |_x: f32, z: f32| 24.0 * ((z - 6.0) / 4.0).clamp(0.0, 1.0);
    // A closed stratus deck in still air, the cloud map and the overcast sky light on.
    let deck = regia_render::CloudSettings {
        shadow_map: true,
        overcast_ibl: true,
        pattern: regia_render::CloudPattern::Constant,
        wind_m_per_s: [0.0, 0.0],
        ..regia_render::CloudSettings::coverage_layer(1.0)
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(OutdoorParams {
        clouds: deck,
        planet: AtmosphereParameters::earth(),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(
        Box::new(ground(cliff)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    for _ in 0..3 {
        cover_frame(&mut renderer, 10.0);
    }
    // The flat ground at the bottom of the frame (about 20 m away) and the cliff's face above
    // the middle (its upper half, clear of its foot).
    let regions = |pixels: &[u8]| {
        (
            mean(pixels, (W / 4, 3 * W / 4), (H - 10, H)),
            mean(pixels, (W / 4, 3 * W / 4), (8, 26)),
        )
    };
    // Exposed so the ground sits at mid level, by bisection (a lower EV is a brighter frame).
    let (mut bright_ev, mut dim_ev) = (4.0_f32, 18.0_f32);
    let mut best = (0.0_f32, f32::MAX, 0.0_f32, 0.0_f32);
    for _ in 0..12 {
        let ev = 0.5 * (bright_ev + dim_ev);
        let (ground, face) = regions(&cover_frame(&mut renderer, ev));
        let miss = (ground - 110.0).abs();
        if miss < best.1 {
            best = (ev, miss, ground, face);
        }
        if miss < 2.0 {
            break;
        }
        if ground > 110.0 {
            bright_ev = ev;
        } else {
            dim_ev = ev;
        }
    }
    let (ev, miss, ground, face) = best;
    eprintln!("under the closed cover: cliff face {face:.1}, the ground before it {ground:.1} at EV {ev:.2}");
    assert!(
        miss < 8.0,
        "no exposure puts the ground at mid level (closest {ground:.1} at EV {ev:.2})"
    );
    assert!(
        face <= 1.05 * ground,
        "the cliff outshines the shadowed ground before it: {face:.1} against {ground:.1} (a sunlit ground's bounce under the cover)"
    );
    assert!(
        face > 0.3 * ground,
        "the cliff lost the sky light too ({face:.1} against {ground:.1})"
    );
}
