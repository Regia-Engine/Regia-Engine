// SPDX-License-Identifier: MPL-2.0
//! Terrain look regressions from the 2026-10-07 UE parity plates (lavapipe). Each test
//! measures one failure shape the plates showed and fails on the pre-fix shader:
//!
//! - stair-step terraces: a straight diagonal wall seen through coarse CDLOD cells became a
//!   saw-tooth (vertices point-sampled the full-resolution map);
//! - white caps: the crest line of a map fell out of every altitude band and through to the
//!   fallback layer;
//! - tiling at 1 m: a 4 m stochastic cell held four repeats of a 1 /m layer;
//! - blue slopes: the shared pass lit every normal with the Earth sky's zenith blue;
//! - strata, crust and glints: steep faces carried no bedding, ice no sparkle.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerStyle, LayerTextures, MaterialParams, MinMaxPyramid, Rgba8Image, SkyAmbient,
    TerrainLayers, TerrainMap, TerrainMaterial, TerrainRelief, TerrainSharedDraw, select_nodes,
};

const W: u32 = 320;
const H: u32 = 180;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// `TERRAIN_LOOK_DUMP=<dir>` writes each measured frame as `<dir>/<name>.png` for review.
fn dump(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("TERRAIN_LOOK_DUMP") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let mut encoder = png::Encoder::new(file, W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = (y * W + x) as usize * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn luma(p: [u8; 4]) -> f64 {
    0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
}

fn layer(name: &str, albedo: [f32; 3], slope: (f32, f32), alt: (f32, f32), tiling: f32) -> LayerDesc {
    LayerDesc {
        name: name.into(),
        albedo,
        roughness: 0.9,
        tiling,
        slope_min: slope.0,
        slope_max: slope.1,
        altitude_min: alt.0,
        altitude_max: alt.1,
        blend_sharpness: 0.15,
        triplanar: false,
        ..LayerDesc::ground()
    }
}

fn layers(list: Vec<LayerDesc>) -> TerrainLayers {
    TerrainLayers {
        layers: list,
        ..Default::default()
    }
}

/// Flat-colour texture set (albedo bytes as given).
fn solid_set(rgba: [u8; 4]) -> LayerTextures {
    LayerTextures {
        albedo: Rgba8Image::solid(64, 64, rgba),
        normal: Rgba8Image::neutral_normal(64),
        height: Rgba8Image::neutral_height(64),
        orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
        use_orm: false,
    }
}

fn material(sets: Vec<LayerTextures>, params: MaterialParams) -> TerrainMaterial {
    TerrainMaterial {
        layers: sets.into_iter().map(Some).collect(),
        macro_set: None,
        params,
    }
}

/// Offscreen render through the hook (`fs_main`) at a debug view (0 = shaded).
#[allow(clippy::too_many_arguments)]
fn render(
    map: &TerrainMap,
    stack: &TerrainLayers,
    material: Option<&TerrainMaterial>,
    relief: TerrainRelief,
    camera: &TerrainCamera,
    root_level: u32,
    debug_view: u32,
    tod: &TimeOfDay,
) -> Vec<u8> {
    render_with(
        map,
        stack,
        material,
        relief,
        camera,
        root_level,
        debug_view,
        tod,
        CdlodConfig::with_grid_size(16),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_with(
    map: &TerrainMap,
    stack: &TerrainLayers,
    material: Option<&TerrainMaterial>,
    relief: TerrainRelief,
    camera: &TerrainCamera,
    root_level: u32,
    debug_view: u32,
    tod: &TimeOfDay,
    config: CdlodConfig,
    anisotropy: Option<u8>,
) -> Vec<u8> {
    let mut gpu = TerrainGpu::new(device(), map, stack, &config, root_level, None, None).expect("gpu");
    if let Some(material) = material {
        gpu.set_material(material, &[]).expect("material");
    }
    if let Some(level) = anisotropy {
        gpu.set_anisotropy(level);
    }
    gpu.set_relief(relief);
    let pyramid = MinMaxPyramid::build(map, root_level).expect("pyramid");
    let selection = select_nodes(
        map,
        &pyramid,
        &config,
        root_level,
        [camera.position[0], camera.position[2]],
        None,
    );
    let mut frame = TerrainFrame::from_camera(camera, tod, 0.0, 1.0);
    frame.debug_view = debug_view;
    render_offscreen(&mut gpu, device(), &frame, &selection, W, H, [0.0, 0.0, 1.0, 1.0]).expect("render")
}

// ---------------------------------------------------------------------------
// 1. Stair-step terraces
// ---------------------------------------------------------------------------

/// 1025² over 320 m (0.3125 m texels, the DO27 visual spacing): a 20 m plateau whose
/// 0.6 m-thick wall runs through the origin at 37° to the sample grid.
fn diagonal_wall_map() -> (TerrainMap, [f32; 2]) {
    let n = 1025u32;
    let width = 320.0f32;
    let angle = 37.0f32.to_radians();
    let normal = [angle.cos(), angle.sin()];
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let d = x * normal[0] + z * normal[1];
            heights.push(20.0 * (0.5 + d / 0.6).clamp(0.0, 1.0));
        }
    }
    (
        TerrainMap::from_heights(n, n, heights, width, 24.0, -2.0).expect("map"),
        normal,
    )
}

/// RMS distance (pixels) of the wall's mid-height contour from its best straight line. In
/// debug view 6 the height is a grey ramp across the wall; per column the contour is the
/// sub-pixel row where the encoded height crosses the middle of the ramp.
fn contour_ripple(pixels: &[u8]) -> (f64, usize) {
    let mid = 170.0; // sRGB of h01 ≈ 0.42 (8.4 m up a 20 m wall on the −2..22 m range)
    let mut points = Vec::new();
    for x in 10..W - 10 {
        for y in 1..H {
            let a = pixel(pixels, x, y - 1);
            let b = pixel(pixels, x, y);
            let grey = |p: [u8; 4]| p[0].abs_diff(p[1]) < 4 && p[1].abs_diff(p[2]) < 4;
            if !grey(a) || !grey(b) {
                continue;
            }
            let (va, vb) = (f64::from(a[0]), f64::from(b[0]));
            if (va - mid) * (vb - mid) <= 0.0 && (va - vb).abs() > 1e-6 {
                let t = (mid - va) / (vb - va);
                points.push((f64::from(x), f64::from(y - 1) + t));
                break;
            }
        }
    }
    let n = points.len().max(1) as f64;
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(a, b), p| (a + p.0, b + p.1));
    let (mx, my) = (sx / n, sy / n);
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for p in &points {
        sxx += (p.0 - mx) * (p.0 - mx);
        sxy += (p.0 - mx) * (p.1 - my);
    }
    let slope = sxy / sxx.max(1e-9);
    let rms = (points
        .iter()
        .map(|p| {
            let fit = my + slope * (p.0 - mx);
            (p.1 - fit) * (p.1 - fit)
        })
        .sum::<f64>()
        / n)
        .sqrt();
    (rms, points.len())
}

#[test]
fn a_diagonal_wall_stays_straight_through_coarse_lod_cells() {
    let (map, normal) = diagonal_wall_map();
    // High above the floor 150 m from the wall, looking steeply down at it: CDLOD picks
    // 40 m nodes there (2.5 m cells, eight texels; range factor 2, the coarsest the
    // selection allows), and the narrow lens magnifies one cell to about five pixels.
    let at = |d: f32, y: f32| [normal[0] * d, y, normal[1] * d];
    let camera = TerrainCamera {
        position: at(-150.0, 260.0),
        target: at(0.0, 10.0),
        fov_y_radians: 0.3,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 2000.0,
    };
    let stack = layers(vec![layer(
        "ice",
        [0.8, 0.8, 0.8],
        (0.0, 1.6),
        (-100.0, 100.0),
        1.0,
    )]);
    let pixels = render_with(
        &map,
        &stack,
        None,
        TerrainRelief::default(),
        &camera,
        6,
        6,
        &TimeOfDay::default(),
        CdlodConfig {
            range_factor: 2.0,
            ..CdlodConfig::with_grid_size(16)
        },
        None,
    );
    dump("diagonal_wall", &pixels);
    let (ripple, columns) = contour_ripple(&pixels);
    eprintln!("wall contour: rms {ripple:.2} px over {columns} columns");
    assert!(
        columns > 200,
        "the wall contour was not found ({columns} columns)"
    );
    assert!(
        ripple < 0.6,
        "the wall is a saw-tooth: contour rms {ripple:.2} px"
    );
}

// ---------------------------------------------------------------------------
// 2. White caps on crests
// ---------------------------------------------------------------------------

/// A mesa with a flat 30 m top (radius ~13 m): the top is the map's highest ground.
fn cone_map() -> TerrainMap {
    let n = 257u32;
    let width = 128.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push((40.0 - (x * x + z * z).sqrt() * 0.75).clamp(0.0, 30.0));
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 32.0, -1.0).expect("map")
}

#[test]
fn a_band_that_reaches_the_summit_keeps_the_crest() {
    let map = cone_map();
    let (lo, hi) = map.min_max_height();
    // Slot 0 is a green layer whose band never covers the mesa (the fallback layer); slot 1
    // is red rock from the lowest to the highest ground — it must own the top and the foot.
    let stack = layers(vec![
        layer(
            "green",
            [0.1, 0.8, 0.1],
            (0.0, 1.6),
            (lo - 200.0, lo - 100.0),
            1.0,
        ),
        layer("red", [0.8, 0.1, 0.05], (0.0, 1.6), (lo, hi), 1.0),
    ]);
    let mat = material(
        vec![solid_set([255, 255, 255, 255]), solid_set([255, 255, 255, 255])],
        MaterialParams::default(),
    );
    let camera = TerrainCamera {
        position: [0.0, hi + 45.0, 0.4],
        target: [0.0, hi - 2.0, 0.0],
        fov_y_radians: 1.1,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 400.0,
    };
    let pixels = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::off(),
        &camera,
        4,
        3,
        &TimeOfDay::default(),
    );
    dump("summit", &pixels);
    // Over the whole frame (mesa top, flanks and the lowest ground), count albedo pixels
    // of the green fallback layer.
    let mut pale = 0;
    let mut total = 0;
    for y in 0..H {
        for x in 0..W {
            let p = pixel(&pixels, x, y);
            total += 1;
            if u16::from(p[1]) > u16::from(p[0]) + 60 && u16::from(p[1]) > u16::from(p[2]) + 60 {
                pale += 1;
            }
        }
    }
    eprintln!("mesa: {pale} fallback pixels of {total}");
    assert!(
        pale == 0,
        "the mesa top or foot fell through to the fallback layer: {pale} pixels"
    );
}

// ---------------------------------------------------------------------------
// 3. Tiling at 1 m
// ---------------------------------------------------------------------------

/// One dark dot per repeat on a light ground: exactly periodic, so any visible repeat
/// shows as a strong autocorrelation peak at the repeat period.
fn dot_texture(size: u32) -> Rgba8Image {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 / size as f32 - 0.3;
            let dy = y as f32 / size as f32 - 0.6;
            let v = if dx * dx + dy * dy < 0.02 { 40 } else { 210 };
            data.extend_from_slice(&[v, v, v, 255]);
        }
    }
    Rgba8Image {
        width: size,
        height: size,
        data,
    }
}

/// Normalised autocorrelation of the luminance along every row at a horizontal lag.
fn row_autocorrelation(pixels: &[u8], lag: u32) -> f64 {
    let mut num = 0.0;
    let mut den = 0.0;
    for y in (H / 4..H * 3 / 4).step_by(2) {
        let row: Vec<f64> = (0..W).map(|x| luma(pixel(pixels, x, y))).collect();
        let mean = row.iter().sum::<f64>() / f64::from(W);
        for x in 0..(W - lag) as usize {
            num += (row[x] - mean) * (row[x + lag as usize] - mean);
            den += (row[x] - mean) * (row[x] - mean);
        }
    }
    num / den.max(1e-9)
}

#[test]
fn anti_tiling_hides_the_repeat_at_one_metre() {
    let map = TerrainMap::from_heights(65, 65, vec![2.0; 65 * 65], 64.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![layer(
        "dust",
        [1.0, 1.0, 1.0],
        (0.0, 1.6),
        (-100.0, 100.0),
        1.0,
    )]);
    let mut set = solid_set([255, 255, 255, 255]);
    set.albedo = dot_texture(64);
    // The DO27 default cell: 4 m.
    let mat = material(
        vec![set],
        MaterialParams {
            anti_tiling_cell: 4.0,
            ..MaterialParams::default()
        },
    );
    // Straight down from 2.4 m: 1 m of ground spans ~ W / 2.6 px.
    let camera = TerrainCamera {
        position: [0.3, 4.4, 0.31],
        target: [0.3, 2.0, 0.3],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.05,
        far: 50.0,
    };
    let pixels = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::off(),
        &camera,
        3,
        3,
        &TimeOfDay::default(),
    );
    dump("tiling", &pixels);
    // Pixels per metre on the ground at the view centre.
    let half_h = 2.4 * (0.45f32).tan();
    let px_per_m = H as f32 / (2.0 * half_h);
    let lag = px_per_m.round() as u32;
    let peak = (lag - 2..=lag + 2)
        .map(|l| row_autocorrelation(&pixels, l))
        .fold(f64::MIN, f64::max);
    eprintln!("autocorrelation at the 1 m repeat ({lag} px): {peak:.3}");
    assert!(
        peak < 0.35,
        "the 1 m repeat is visible: autocorrelation {peak:.3}"
    );
}

// ---------------------------------------------------------------------------
// 4. Blue slopes in the shared frame
// ---------------------------------------------------------------------------

/// A flat floor with a 10 m east-facing wall (the sun shines from the west, so the
/// wall's face is in its own shadow).
fn wall_floor_map() -> TerrainMap {
    let n = 129u32;
    let width = 64.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push(10.0 * (1.0 - ((x + 4.0) / 3.0).clamp(0.0, 1.0)));
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 12.0, -1.0).expect("map")
}

fn shared_frame(ambient: Option<SkyAmbient>) -> Vec<u8> {
    shared_frame_on(ambient, None)
}

/// `planet`: the frame's atmosphere preset (IBL on, so meshes take the preset's bake).
fn shared_frame_on(ambient: Option<SkyAmbient>, planet: Option<AtmospherePreset>) -> Vec<u8> {
    let stack = layers(vec![layer(
        "grey",
        [0.6, 0.6, 0.6],
        (0.0, 1.6),
        (-100.0, 100.0),
        1.0,
    )]);
    let mut draw = TerrainSharedDraw::new(device(), wall_floor_map(), &stack, CdlodConfig::default(), 3)
        .expect("terrain");
    draw.set_material(
        &material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default()),
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    draw.set_sky_ambient(ambient);
    let post = PostSettings {
        exposure_ev: 12.5,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: [14.0, 6.0, 0.0],
        target: [-3.5, 5.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: planet.is_some(),
        exposure_ev: 12.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        // Travel toward +X and down: the sun is in the west, behind the wall.
        directional: DirectionalLightUniform::new([0.75, -0.66, 0.0], [1.0, 1.0, 1.0], 4_400.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    if let Some(preset) = planet {
        renderer.set_outdoor(OutdoorParams {
            planet: AtmosphereParameters::from_preset(preset),
            ..OutdoorParams::default()
        });
    }
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer.render_offscreen(device(), &frame).expect("frame").pixels
}

/// Mean blue / red ratio over the wall face (frame centre).
fn wall_blue_ratio(pixels: &[u8]) -> (f64, f64) {
    let (mut r, mut b, mut l) = (0.0, 0.0, 0.0);
    let mut n = 0.0;
    for y in H / 2 - 20..H / 2 + 5 {
        for x in W / 2 - 40..W / 2 + 40 {
            let p = pixel(pixels, x, y);
            r += f64::from(p[0]);
            b += f64::from(p[2]);
            l += luma(p);
            n += 1.0;
        }
    }
    (b / r.max(1.0), l / n)
}

#[test]
fn shadowed_walls_are_not_painted_with_the_zenith_blue() {
    // Host default (flat ambient_lux): the wall sees half sky, half neutral ground.
    let default = shared_frame(None);
    dump("blue_default", &default);
    let (ratio, lum) = wall_blue_ratio(&default);
    eprintln!("default ambient: wall blue/red {ratio:.2}, luma {lum:.1}");
    // The pre-fix pass lit the wall with the zenith blue only: b/r 2.87 on these bytes.
    // Half sky, half neutral ground measures 1.90.
    assert!(ratio < 2.2, "the shadowed wall is zenith blue: b/r {ratio:.2}");

    // Airless map: a black sky and the sunlit floor's bounce — neutral and still readable.
    let airless = shared_frame(Some(SkyAmbient::airless(4_400.0, 0.72, [0.6, 0.6, 0.62])));
    dump("blue_airless", &airless);
    let (ratio, lum) = wall_blue_ratio(&airless);
    eprintln!("airless ambient: wall blue/red {ratio:.2}, luma {lum:.1}");
    assert!(ratio < 1.12, "airless wall is tinted: b/r {ratio:.2}");
    assert!(lum > 6.0, "airless wall is black: luma {lum:.1}");
}

#[test]
fn terrain_follows_the_airless_preset_without_a_host_call() {
    // The frame's planet is airless and IBL is on: meshes take the preset's ambient bake
    // (black sky, ground bounce). The terrain must follow it with no `set_sky_ambient`.
    let pixels = shared_frame_on(None, Some(AtmospherePreset::Airless));
    dump("blue_airless_preset", &pixels);
    let (ratio, lum) = wall_blue_ratio(&pixels);
    eprintln!("airless preset, no host call: wall blue/red {ratio:.2}, luma {lum:.1}");
    assert!(
        ratio < 1.2,
        "terrain ignored the airless preset's ambient: b/r {ratio:.2}"
    );
    assert!(lum > 3.0, "airless wall is black: luma {lum:.1}");
}

// ---------------------------------------------------------------------------
// 5. Strata, crust and glints
// ---------------------------------------------------------------------------

#[test]
fn steep_faces_carry_strata_and_the_toggle_removes_them() {
    let map = wall_floor_map();
    let stack = layers(vec![layer(
        "rock",
        [0.7, 0.6, 0.5],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.5,
    )]);
    let mat = material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default());
    let camera = TerrainCamera {
        position: [12.0, 5.0, 0.0],
        target: [-3.5, 5.0, 0.0],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 200.0,
    };
    // Vertical variation of the albedo along the wall's centre column.
    let column_variance = |pixels: &[u8]| {
        let values: Vec<f64> = (H / 2 - 40..H / 2 + 30)
            .map(|y| luma(pixel(pixels, W / 2, y)))
            .collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / values.len() as f64
    };
    let tod = TimeOfDay::default();
    let with = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        3,
        3,
        &tod,
    );
    let without = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::off(),
        &camera,
        3,
        3,
        &tod,
    );
    dump("strata_on", &with);
    dump("strata_off", &without);
    let (a, b) = (column_variance(&with), column_variance(&without));
    eprintln!("wall albedo variance: strata {a:.1}, off {b:.1}");
    assert!(
        b < 4.0,
        "a flat-color wall without strata should be uniform: {b:.1}"
    );
    assert!(a > 30.0, "no visible bedding on the steep face: {a:.1}");
}

#[test]
fn ice_glints_resolve_near_the_camera_and_toggle_off() {
    let map = TerrainMap::from_heights(65, 65, vec![2.0; 65 * 65], 64.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness: 0.5,
        ..layer("ice", [0.95, 0.97, 1.0], (0.0, 1.6), (-100.0, 100.0), 1.0)
    }]);
    let mut set = LayerStyle::Ice.bake(64, 5);
    set.normal = Rgba8Image::neutral_normal(64);
    let mat = material(vec![set], MaterialParams::default());
    // Looking toward a low sun: the half vector is near the surface normal for most
    // fragments, so facets that tilt by a few degrees flash.
    let tod = TimeOfDay {
        azimuth_radians: 0.0,
        elevation_radians: 0.5,
        ..TimeOfDay::default()
    };
    let camera = TerrainCamera {
        position: [0.0, 3.0, -0.5],
        target: [0.0, 2.0, 1.2],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.05,
        far: 50.0,
    };
    let count_flashes = |pixels: &[u8]| {
        let mut n = 0;
        for y in 1..H - 1 {
            for x in 1..W - 1 {
                let c = luma(pixel(pixels, x, y));
                let around = (luma(pixel(pixels, x - 1, y))
                    + luma(pixel(pixels, x + 1, y))
                    + luma(pixel(pixels, x, y - 1))
                    + luma(pixel(pixels, x, y + 1)))
                    / 4.0;
                if c > around + 40.0 {
                    n += 1;
                }
            }
        }
        n
    };
    // Glints alone (the near-field crust has its own bright edges).
    let base = TerrainRelief {
        near_detail: 0.0,
        ..TerrainRelief::default()
    };
    let on = render(&map, &stack, Some(&mat), base, &camera, 3, 0, &tod);
    let off = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief { glints: 0.0, ..base },
        &camera,
        3,
        0,
        &tod,
    );
    dump("glints_on", &on);
    dump("glints_off", &off);
    let (a, b) = (count_flashes(&on), count_flashes(&off));
    eprintln!("isolated flashes: glints {a}, off {b}");
    assert!(a >= 3, "no sparkle on near ice: {a}");
    assert!(b <= a / 4, "flashes without glints: {b} (with {a})");
}

// ---------------------------------------------------------------------------
// 6. Layer setter on the shared draw (DO27 drops its TerrainGpu wrapper)
// ---------------------------------------------------------------------------

#[test]
fn shared_draw_layers_and_material_can_be_replaced_after_construction() {
    let map = TerrainMap::from_heights(33, 33, vec![1.0; 33 * 33], 32.0, 4.0, 0.0).expect("map");
    let green = layers(vec![layer(
        "green",
        [0.1, 0.6, 0.1],
        (0.0, 1.6),
        (-10.0, 10.0),
        1.0,
    )]);
    let red = layers(vec![layer(
        "red",
        [0.7, 0.05, 0.05],
        (0.0, 1.6),
        (-10.0, 10.0),
        1.0,
    )]);
    let mut draw = TerrainSharedDraw::new(device(), map, &green, CdlodConfig::default(), 2).expect("terrain");
    assert_eq!(draw.gpu().layers().layers[0].name, "green");
    draw.set_layers(&red).expect("valid stack");
    assert_eq!(draw.gpu().layers().layers[0].name, "red");
    // An invalid stack is rejected and the previous one stays.
    let mut bad = red.clone();
    bad.layers[0].slope_max = -1.0;
    assert!(draw.set_layers(&bad).is_err());
    assert_eq!(draw.gpu().layers(), &red);
    // The material installs on the shared draw (the photoreal path).
    let before = draw.gpu().texture_bytes();
    draw.set_material(
        &material(vec![LayerStyle::Rock.bake(64, 1)], MaterialParams::default()),
        &[],
    )
    .expect("material");
    assert!(draw.gpu().texture_bytes() > before);
    assert_eq!(draw.sky_ambient(), None);
    draw.set_sky_ambient(Some(SkyAmbient::from_flat_lux(1000.0)));
    assert!(draw.sky_ambient().is_some());
}

#[test]
fn set_layers_changes_what_the_terrain_draws() {
    let map = TerrainMap::from_heights(33, 33, vec![1.0; 33 * 33], 32.0, 4.0, 0.0).expect("map");
    let green = layers(vec![layer(
        "green",
        [0.1, 0.6, 0.1],
        (0.0, 1.6),
        (-10.0, 10.0),
        1.0,
    )]);
    let red = layers(vec![layer(
        "red",
        [0.7, 0.05, 0.05],
        (0.0, 1.6),
        (-10.0, 10.0),
        1.0,
    )]);
    let camera = TerrainCamera {
        position: [0.0, 6.0, 6.0],
        target: [0.0, 1.0, 0.0],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 100.0,
    };
    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(device(), &map, &green, &config, 2, None, None).expect("gpu");
    gpu.set_layers(&red).expect("layers");
    let pyramid = MinMaxPyramid::build(&map, 2).expect("pyramid");
    let selection = select_nodes(&map, &pyramid, &config, 2, [0.0, 6.0], None);
    let mut frame = TerrainFrame::from_camera(&camera, &TimeOfDay::default(), 0.0, 1.0);
    frame.debug_view = 3;
    let pixels =
        render_offscreen(&mut gpu, device(), &frame, &selection, W, H, [0.0, 0.0, 1.0, 1.0]).expect("render");
    let p = pixel(&pixels, W / 2, H * 3 / 4);
    assert!(p[0] > p[1] + 60, "the replaced red stack did not draw: {p:?}");
}

// ---------------------------------------------------------------------------
// 7. Strata read as rock, not as altitude contours (round 1b art direction)
// ---------------------------------------------------------------------------

/// A steep (63°) 40 m mesa with a flat top.
fn steep_mesa_map() -> TerrainMap {
    let n = 513u32;
    let width = 160.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push((100.0 - (x * x + z * z).sqrt() * 2.0).clamp(0.0, 40.0));
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 44.0, -2.0).expect("map")
}

/// Fraction of the albedo variance on the mesa flank explained by altitude alone (R² of
/// the tone against altitude bins). Pure altitude bands — contour lines — score near 1.
fn contour_ness(albedo: &[u8], height: &[u8]) -> f64 {
    let mut bins: Vec<(f64, f64, f64)> = vec![(0.0, 0.0, 0.0); 256];
    let mut all = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let h = pixel(height, x, y);
            let grey = h[0].abs_diff(h[1]) < 4 && h[1].abs_diff(h[2]) < 4;
            // Flank only: between the foot (h01 ≈ 0.05) and the top (h01 ≈ 0.95).
            if !grey || h[0] < 90 || h[0] > 235 {
                continue;
            }
            let v = luma(pixel(albedo, x, y));
            let b = &mut bins[h[0] as usize];
            b.0 += v;
            b.1 += v * v;
            b.2 += 1.0;
            all.push(v);
        }
    }
    let n = all.len().max(1) as f64;
    let mean = all.iter().sum::<f64>() / n;
    let total: f64 = all.iter().map(|v| (v - mean) * (v - mean)).sum();
    let within: f64 = bins
        .iter()
        .filter(|b| b.2 > 0.0)
        .map(|b| b.1 - b.0 * b.0 / b.2)
        .sum();
    1.0 - within / total.max(1e-9)
}

#[test]
fn strata_bands_dip_and_drift_instead_of_following_contours() {
    let map = steep_mesa_map();
    let stack = layers(vec![layer(
        "rock",
        [0.7, 0.6, 0.5],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.5,
    )]);
    let mat = material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.0, 30.0, 110.0],
        target: [0.0, 18.0, 30.0],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 600.0,
    };
    let tod = TimeOfDay::default();
    let albedo = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        5,
        3,
        &tod,
    );
    let height = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        5,
        6,
        &tod,
    );
    dump("strata_albedo", &albedo);
    dump("strata_height", &height);
    let r2 = contour_ness(&albedo, &height);
    eprintln!("strata contour-ness (R² tone ~ altitude): {r2:.3}");
    assert!(r2 < 0.5, "strata follow the altitude contours: R² {r2:.3}");
}

#[test]
fn rough_dust_faces_do_not_sparkle() {
    let map = steep_mesa_map();
    // Bright Mars dust, roughness 0.9: glints are for smooth crystalline layers only.
    let stack = layers(vec![LayerDesc {
        roughness: 0.9,
        ..layer("dust", [0.95, 0.72, 0.52], (0.0, 1.6), (-100.0, 100.0), 0.8)
    }]);
    let mat = material(vec![LayerStyle::Sand.bake(64, 9)], MaterialParams::default());
    // A low sun behind the camera, the flank 2–3 m away: glint cells are resolved here.
    let tod = TimeOfDay {
        azimuth_radians: 0.0,
        elevation_radians: 0.35,
        ..TimeOfDay::default()
    };
    let camera = TerrainCamera {
        position: [0.0, 21.5, 41.5],
        target: [0.0, 19.0, 39.5],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.05,
        far: 600.0,
    };
    let shaded = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        5,
        0,
        &tod,
    );
    let lambert = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief {
            glints: 0.0,
            ..TerrainRelief::default()
        },
        &camera,
        5,
        0,
        &tod,
    );
    dump("dust_shaded", &shaded);
    let diff: f64 = shaded
        .iter()
        .zip(&lambert)
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum::<f64>()
        / shaded.len() as f64;
    eprintln!("rough dust: glint contribution {diff:.4}");
    assert!(diff < 1e-6, "rough dust sparkles: mean byte difference {diff:.4}");
}

// ---------------------------------------------------------------------------
// 8. Round 2: near-field detail at 1 m, soft reflections on glazed ice
// ---------------------------------------------------------------------------

/// Mean |discrete Laplacian| of the luminance over the lower 60 % of the frame (the ground
/// at a 1 m eye height): the high-frequency energy a crust or grain adds.
fn ground_hf_energy(pixels: &[u8]) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in (H * 2 / 5)..H - 1 {
        for x in 1..W - 1 {
            let c = luma(pixel(pixels, x, y));
            let lap = 4.0 * c
                - luma(pixel(pixels, x - 1, y))
                - luma(pixel(pixels, x + 1, y))
                - luma(pixel(pixels, x, y - 1))
                - luma(pixel(pixels, x, y + 1));
            sum += lap.abs();
            n += 1.0;
        }
    }
    sum / n
}

fn near_ground(style: LayerStyle, albedo: [f32; 3], roughness: f32, eye_height: f32, near: f32) -> f64 {
    let map = TerrainMap::from_heights(129, 129, vec![2.0; 129 * 129], 128.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness,
        ..layer("ground", albedo, (0.0, 1.6), (-100.0, 100.0), 1.0)
    }]);
    let mat = material(vec![style.bake(128, 21)], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.0, 2.0 + eye_height, 0.0],
        target: [0.0, 2.0, eye_height * 2.5],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.02,
        far: 400.0,
    };
    let relief = TerrainRelief {
        near_detail: near,
        ..TerrainRelief::default()
    };
    // Isotropic layer filtering (v0.1.7): this measures the procedural near field over the
    // layer texture's own detail, with the 1.4x bar set against that filter. Anisotropic
    // filtering sharpens the texture baseline itself (it is measured in `terrain_aniso.rs`).
    let pixels = render_with(
        &map,
        &stack,
        Some(&mat),
        relief,
        &camera,
        4,
        0,
        &TimeOfDay::default(),
        CdlodConfig::with_grid_size(16),
        Some(0),
    );
    dump(&format!("near_{style:?}_{eye_height}_{near}"), &pixels);
    ground_hf_energy(&pixels)
}

#[test]
fn near_field_detail_adds_crust_and_grain_at_one_metre_and_fades_by_thirty() {
    for (style, albedo, rough) in [
        (LayerStyle::Snow, [0.93, 0.95, 0.98], 0.85),
        (LayerStyle::Dirt, [0.8, 0.44, 0.26], 0.92),
    ] {
        let off = near_ground(style, albedo, rough, 1.0, 0.0);
        let on = near_ground(style, albedo, rough, 1.0, 1.0);
        eprintln!("{style:?} at 1 m: high-frequency energy {off:.2} → {on:.2}");
        assert!(
            on > off * 1.4,
            "{style:?}: no near-field detail at 1 m ({off:.2} → {on:.2})"
        );
        // From 40 m up the whole frame is past the 30 m fade.
        let far_off = near_ground(style, albedo, rough, 40.0, 0.0);
        let far_on = near_ground(style, albedo, rough, 40.0, 1.0);
        eprintln!("{style:?} at 40 m: {far_off:.2} → {far_on:.2}");
        assert!(
            (far_on - far_off).abs() < far_off * 0.05 + 0.05,
            "{style:?}: near detail did not fade"
        );
    }
}

/// A red box floating over a flat sheet of glazed ice, seen from just above the ice:
/// the box sits in the upper half of the frame, its reflection would land in the lower half.
fn ice_reflection_frame(with_box: bool, roughness: f32) -> Vec<u8> {
    let map = TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 64.0, 4.0, -1.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness,
        ..layer("glazed ice", [0.85, 0.9, 0.95], (0.0, 1.6), (-100.0, 100.0), 1.0)
    }]);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &material(vec![solid_set([235, 238, 242, 255])], MaterialParams::default()),
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    let post = PostSettings {
        exposure_ev: 13.0,
        ..PostSettings::golden_deterministic()
    };
    let (box_verts, box_idx) = regia_render::box_mesh([0.6, 0.5, 0.6]);
    // Within the lab SSR's 3.5 m march of the ice in front of it.
    let box_verts = regia_render::translate_mesh(&box_verts, [0.0, 1.15, -2.6]);
    let mut scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.4, 0.0],
        target: [0.0, 0.55, -2.6],
        fov_y: 0.9,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.5], [1.0, 1.0, 1.0], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.9, 0.05, 0.03, 1.0],
            roughness: 0.6,
            ..regia_render::LabMaterial::default()
        }],
        meshes: Vec::new(),
    };
    if with_box {
        scene.meshes.push(regia_render::LabMesh {
            vertices: box_verts,
            indices: box_idx,
            material: 0,
            ..regia_render::LabMesh::unskinned()
        });
    }
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopUltra));
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer.render_offscreen(device(), &frame).expect("frame").pixels
}

/// Mean absolute colour change (summed over RGB, 0–765) of the ice in the lower half of the
/// frame when the box is present: the box's reflection.
fn reflection_contrast(with: &[u8], without: &[u8]) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in H * 11 / 20..H {
        for x in W / 3..W * 2 / 3 {
            let a = pixel(with, x, y);
            let b = pixel(without, x, y);
            sum += (0..3).map(|c| f64::from(a[c].abs_diff(b[c]))).sum::<f64>();
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn glazed_ice_reflects_softly_not_like_a_mirror() {
    let rough = 0.3;
    let with = ice_reflection_frame(true, rough);
    let without = ice_reflection_frame(false, rough);
    dump("ice_reflection_box", &with);
    dump("ice_reflection_bare", &without);
    let contrast = reflection_contrast(&with, &without);
    eprintln!("glazed ice (roughness {rough}): reflection contrast under the box {contrast:.2}");
    // The lab SSR blends a sharp ray at up to 72 %: the pre-fix terrain (perceptual
    // roughness in alpha) mirrored the box. A dielectric at this angle reflects a few percent.
    assert!(
        contrast < 8.0,
        "glazed ice mirrors the box: reflection contrast {contrast:.2}"
    );
}

// ---------------------------------------------------------------------------
// 9. Near detail is in world metres and picks grain for warm sand
// ---------------------------------------------------------------------------

/// A flat map of `samples`² over 64 m (texel 64 / (samples − 1) m) under a near camera, with
/// one textured layer; returns the frame with near detail on and off.
fn near_pair(samples: u32, style: LayerStyle, albedo: [f32; 3]) -> (Vec<u8>, Vec<u8>) {
    let n = samples;
    let map = TerrainMap::from_heights(n, n, vec![2.0; (n * n) as usize], 64.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness: 0.9,
        ..layer("ground", albedo, (0.0, 1.6), (-100.0, 100.0), 2.8)
    }]);
    let mat = material(vec![style.bake(128, 5)], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.3, 3.2, 0.2],
        target: [0.3, 2.0, 2.6],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.02,
        far: 200.0,
    };
    let root = regia_terrain::gpu::full_detail_root_level(&map, &CdlodConfig::with_grid_size(16));
    let tod = TimeOfDay::default();
    let on = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        root,
        0,
        &tod,
    );
    let off = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief {
            near_detail: 0.0,
            ..TerrainRelief::default()
        },
        &camera,
        root,
        0,
        &tod,
    );
    (on, off)
}

/// Luminance of `on − off` over the lower half of the frame: the near detail alone.
fn near_signal(on: &[u8], off: &[u8]) -> Vec<Vec<f64>> {
    (H / 2..H)
        .map(|y| {
            (0..W)
                .map(|x| luma(pixel(on, x, y)) - luma(pixel(off, x, y)))
                .collect()
        })
        .collect()
}

/// Horizontal lag (px) at which the near-detail signal's autocorrelation first drops below
/// one half — its dominant feature size on screen — and the signal's RMS.
fn feature_size(signal: &[Vec<f64>]) -> (f64, f64) {
    let mut energy = 0.0;
    let mut count = 0.0;
    for row in signal {
        for v in row {
            energy += v * v;
            count += 1.0;
        }
    }
    let corr = |lag: usize| -> f64 {
        let mut num = 0.0;
        for row in signal {
            for x in 0..row.len() - lag {
                num += row[x] * row[x + lag];
            }
        }
        num / energy.max(1e-9)
    };
    let mut lag = 1;
    while lag < 40 && corr(lag) > 0.5 {
        lag += 1;
    }
    // Sub-pixel crossing between lag − 1 and lag.
    let (a, b) = (corr(lag - 1), corr(lag));
    let t = if (a - b).abs() > 1e-9 {
        (a - 0.5) / (a - b)
    } else {
        0.0
    };
    ((lag - 1) as f64 + t, (energy / count).sqrt())
}

#[test]
fn near_detail_is_sized_in_metres_on_any_heightfield_resolution() {
    // 0.125 m and 1.0 m heightfield texels, same flat ground, same camera.
    let (fine_on, fine_off) = near_pair(513, LayerStyle::Dirt, [0.8, 0.44, 0.26]);
    let (coarse_on, coarse_off) = near_pair(65, LayerStyle::Dirt, [0.8, 0.44, 0.26]);
    dump("metres_fine", &fine_on);
    dump("metres_coarse", &coarse_on);
    let (fine_size, fine_rms) = feature_size(&near_signal(&fine_on, &fine_off));
    let (coarse_size, coarse_rms) = feature_size(&near_signal(&coarse_on, &coarse_off));
    eprintln!(
        "near detail feature size: 0.125 m texels {fine_size:.2} px (rms {fine_rms:.2}), 1 m texels {coarse_size:.2} px (rms {coarse_rms:.2})"
    );
    assert!(fine_rms > 1.0 && coarse_rms > 1.0, "no near detail");
    assert!(
        (fine_size - coarse_size).abs() < 0.35 + 0.1 * fine_size,
        "near detail scales with the heightfield texel: {fine_size:.2} vs {coarse_size:.2} px"
    );
    assert!(
        (fine_rms / coarse_rms - 1.0).abs() < 0.3,
        "near detail strength depends on the texel"
    );
}

#[test]
fn bright_warm_sand_gets_grain_not_ice_crust() {
    // The decal set piece's bright Mars sand (luma ≈ 0.6): warm, so dust grain, never the
    // ice crust's bluish plates and frost.
    let (on, off) = near_pair(129, LayerStyle::Sand, [1.0, 0.94, 0.82]);
    dump("warm_sand_near", &on);
    let blue_red = |p: &[u8]| {
        let (mut r, mut b) = (0.0, 0.0);
        for y in H / 2..H {
            for x in 0..W {
                let q = pixel(p, x, y);
                r += f64::from(q[0]);
                b += f64::from(q[2]);
            }
        }
        b / r.max(1.0)
    };
    let (a, b) = (blue_red(&on), blue_red(&off));
    eprintln!("warm sand b/r: near detail on {a:.3}, off {b:.3}");
    assert!(
        (a - b).abs() < 0.02,
        "warm sand turned to ice crust: b/r {b:.3} → {a:.3}"
    );
}

// ---------------------------------------------------------------------------
// 10. Round 4 (DO27's real content): relief that holds on the content's own albedos
// ---------------------------------------------------------------------------

/// A mesa whose flank slopes `deg` degrees, 30 m high, flat top.
fn mesa_with_flank(deg: f32) -> TerrainMap {
    let n = 513u32;
    let width = 160.0f32;
    let grade = deg.to_radians().tan();
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push(((50.0 - (x * x + z * z).sqrt()) * grade).clamp(0.0, 30.0));
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 34.0, -2.0).expect("map")
}

fn flank_camera() -> TerrainCamera {
    TerrainCamera {
        position: [0.0, 16.0, 95.0],
        target: [0.0, 14.0, 40.0],
        fov_y_radians: 0.7,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 600.0,
    }
}

/// Luma variance down the frame's centre column over the flank (albedo view).
fn flank_column_variance(pixels: &[u8]) -> f64 {
    let values: Vec<f64> = (H / 2 - 30..H / 2 + 30)
        .map(|y| luma(pixel(pixels, W / 2, y)))
        .collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / values.len() as f64
}

#[test]
fn forty_degree_mesa_flanks_carry_strata() {
    // DO27's Switchbacks mesas are 35–50° flanks, not the 63° walls the strata were
    // first tuned on; the beds must read there too.
    let map = mesa_with_flank(40.0);
    let stack = layers(vec![layer(
        "sandstone",
        [0.62, 0.36, 0.22],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.5,
    )]);
    let mat = material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default());
    let tod = TimeOfDay::default();
    let with = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &flank_camera(),
        5,
        3,
        &tod,
    );
    let without = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::off(),
        &flank_camera(),
        5,
        3,
        &tod,
    );
    dump("flank40_strata", &with);
    let (a, b) = (flank_column_variance(&with), flank_column_variance(&without));
    eprintln!("40° flank albedo variance: strata {a:.1}, off {b:.1}");
    assert!(a > 25.0, "no strata on a 40° flank: variance {a:.1}");
}

/// Mean |luma difference| between horizontal neighbours and between vertical neighbours
/// over the frame's centre (the wall face).
fn anisotropy(pixels: &[u8]) -> (f64, f64) {
    let (mut h, mut v, mut n) = (0.0, 0.0, 0.0);
    for y in H / 2 - 40..H / 2 + 20 {
        for x in W / 2 - 60..W / 2 + 60 {
            let c = luma(pixel(pixels, x, y));
            h += (luma(pixel(pixels, x + 1, y)) - c).abs();
            v += (luma(pixel(pixels, x, y + 1)) - c).abs();
            n += 1.0;
        }
    }
    (h / n, v / n)
}

#[test]
fn steep_ice_faces_streak_vertically_and_rock_faces_band_horizontally() {
    // An ice cliff (cool albedo) shows vertical flutes and streaks like the UE plate's
    // bore_twelve cliff; a warm rock face keeps horizontal beds.
    let map = wall_floor_map();
    let camera = TerrainCamera {
        position: [12.0, 5.0, 0.0],
        target: [-3.5, 5.0, 0.0],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 200.0,
    };
    let mat = material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default());
    let tod = TimeOfDay::default();
    let ice = layers(vec![layer(
        "ice cliff",
        [0.62, 0.68, 0.76],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.5,
    )]);
    let rock = layers(vec![layer(
        "rock",
        [0.62, 0.40, 0.26],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.5,
    )]);
    let ice_px = render(
        &map,
        &ice,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        3,
        3,
        &tod,
    );
    let rock_px = render(
        &map,
        &rock,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        3,
        3,
        &tod,
    );
    dump("ice_cliff_albedo", &ice_px);
    dump("rock_cliff_albedo", &rock_px);
    let (ih, iv) = anisotropy(&ice_px);
    let (rh, rv) = anisotropy(&rock_px);
    eprintln!("ice cliff: horizontal {ih:.2} vertical {iv:.2}; rock: horizontal {rh:.2} vertical {rv:.2}");
    assert!(
        ih > iv * 1.5,
        "ice cliff does not streak vertically: h {ih:.2} v {iv:.2}"
    );
    assert!(
        rv > rh * 1.5,
        "rock face lost its horizontal beds: h {rh:.2} v {rv:.2}"
    );
}

#[test]
fn rough_snow_still_sparkles() {
    // DO27's snow layer is roughness 0.88: rough on average, but its crystals are
    // individual mirror facets. Glints follow the snow, not the layer roughness.
    let map = TerrainMap::from_heights(65, 65, vec![2.0; 65 * 65], 64.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness: 0.88,
        ..layer("snow", [0.95, 0.97, 1.0], (0.0, 1.6), (-100.0, 100.0), 1.0)
    }]);
    let mut set = LayerStyle::Snow.bake(64, 5);
    set.normal = Rgba8Image::neutral_normal(64);
    let mat = material(vec![set], MaterialParams::default());
    let tod = TimeOfDay {
        azimuth_radians: 0.0,
        elevation_radians: 0.5,
        ..TimeOfDay::default()
    };
    let camera = TerrainCamera {
        position: [0.0, 3.0, -0.5],
        target: [0.0, 2.0, 1.2],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.05,
        far: 50.0,
    };
    let base = TerrainRelief {
        near_detail: 0.0,
        ..TerrainRelief::default()
    };
    let on = render(&map, &stack, Some(&mat), base, &camera, 3, 0, &tod);
    let off = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief { glints: 0.0, ..base },
        &camera,
        3,
        0,
        &tod,
    );
    let diff: f64 = on
        .iter()
        .zip(&off)
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum::<f64>()
        / on.len() as f64;
    eprintln!("rough snow glint contribution {diff:.4}");
    assert!(diff > 0.01, "rough snow does not sparkle ({diff:.4})");
}

/// Mean ground luma and 8×8 block contrast of a flat cool (icy) plain seen from 60 m away,
/// with mid-range relief on or off.
fn mid_plain(mid: f32) -> (f64, f64) {
    let map = TerrainMap::from_heights(257, 257, vec![2.0; 257 * 257], 256.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![layer(
        "snow",
        [0.66, 0.69, 0.75],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.3,
    )]);
    let mat = material(vec![LayerStyle::Snow.bake(128, 3)], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.0, 14.0, 0.0],
        target: [0.0, 2.0, 70.0],
        fov_y_radians: 0.6,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 600.0,
    };
    let relief = TerrainRelief {
        mid_relief: mid,
        ..TerrainRelief::default()
    };
    let px = render(
        &map,
        &stack,
        Some(&mat),
        relief,
        &camera,
        5,
        0,
        &TimeOfDay::default(),
    );
    dump(&format!("mid_plain_{mid}"), &px);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in H / 2..H {
        for x in 0..W {
            sum += luma(pixel(&px, x, y));
            n += 1.0;
        }
    }
    let mut block = 0.0;
    let mut blocks = 0.0;
    for by in (H / 2..H - 8).step_by(8) {
        for bx in (0..W - 8).step_by(8) {
            let v: Vec<f64> = (0..64)
                .map(|i| luma(pixel(&px, bx + i % 8, by + i / 8)))
                .collect();
            let m = v.iter().sum::<f64>() / 64.0;
            block += (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / 64.0).sqrt();
            blocks += 1.0;
        }
    }
    (sum / n, block / blocks)
}

#[test]
fn mid_relief_adds_contrast_without_darkening_the_ground() {
    // DO27's cooked layers already carry their own regolith; the mid relief must add
    // relief and tone variation around the content's albedo, not a net darkening.
    let (l_off, c_off) = mid_plain(0.0);
    let (l_on, c_on) = mid_plain(1.0);
    eprintln!("mid plain: luma {l_off:.1} → {l_on:.1}, contrast {c_off:.2} → {c_on:.2}");
    assert!(c_on > c_off * 1.5, "no mid-range contrast");
    assert!(
        (l_on / l_off - 1.0).abs() < 0.05,
        "mid relief shifts the ground luma {l_off:.1} → {l_on:.1}"
    );
}

#[test]
fn ice_crust_keeps_the_snow_bright_around_the_content_albedo() {
    // The crust's plates, cracks and frost vary around the layer's own albedo: DO27's snow
    // must not print as dark plates.
    for albedo in [[0.95, 0.97, 1.0], [0.66, 0.69, 0.75]] {
        let (on, off) = near_pair(129, LayerStyle::Snow, albedo);
        let mean = |p: &[u8]| {
            let (mut s, mut n) = (0.0, 0.0);
            for y in H / 2..H {
                for x in 0..W {
                    s += luma(pixel(p, x, y));
                    n += 1.0;
                }
            }
            s / n
        };
        let (a, b) = (mean(&on), mean(&off));
        let signal = near_signal(&on, &off);
        let (_, rms) = feature_size(&signal);
        eprintln!("snow {albedo:?}: crust luma {b:.1} → {a:.1}, crust rms {rms:.2}");
        assert!(rms > 2.0, "no crust contrast");
        assert!(a > b * 0.94, "the crust darkens the snow: {b:.1} → {a:.1}");
    }
}

/// Distinct bands down the centre column of a shaded mesa flank: luma extrema whose
/// prominence over the neighbouring extremum exceeds `min_step` levels.
fn band_count(pixels: &[u8], y0: u32, y1: u32, min_step: f64) -> usize {
    let col: Vec<f64> = (y0..y1)
        .map(|y| {
            (W / 2 - 2..W / 2 + 3)
                .map(|x| luma(pixel(pixels, x, y)))
                .sum::<f64>()
                / 5.0
        })
        .collect();
    let mut count = 0;
    let mut last = col[0];
    let mut rising = None;
    for &v in &col[1..] {
        let d = v - last;
        if d.abs() < min_step {
            continue;
        }
        let up = d > 0.0;
        if rising != Some(up) {
            count += 1;
            rising = Some(up);
        }
        last = v;
    }
    count
}

#[test]
fn mesa_flanks_read_as_bands_and_ledges_from_150_m() {
    // switchbacks_wide/mid see 45–65° mesa flanks from 150–400 m: the beds must still read
    // as distinct bands and lit/shadowed ledges at that range, not a smooth pale face.
    let map = mesa_with_flank(52.0);
    let stack = layers(vec![layer(
        "sandstone",
        [0.62, 0.36, 0.22],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.3,
    )]);
    let mat = material(vec![LayerStyle::Rock.bake(128, 4)], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.0, 20.0, 200.0],
        target: [0.0, 15.0, 40.0],
        fov_y_radians: 0.25,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 2000.0,
    };
    let tod = TimeOfDay {
        azimuth_radians: 0.6,
        elevation_radians: 0.45,
        ..TimeOfDay::default()
    };
    let shaded = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        5,
        0,
        &tod,
    );
    dump("mesa_150m", &shaded);
    let bands = band_count(&shaded, H / 2 - 50, H / 2 + 40, 6.0);
    eprintln!("mesa flank at 160 m: {bands} bands over 90 px");
    assert!(
        bands >= 8,
        "the flank reads as a smooth face at 160 m: {bands} bands"
    );
}

// ---------------------------------------------------------------------------
// 11. The sky ambient is exposed like the sun on the LDR shared path (DO27 pin 1132)
// ---------------------------------------------------------------------------

/// The airless wall-and-floor frame through `FrameRenderer`, at EV100 14.5 (the terrain's
/// LDR reference exposure), on the linear HDR path (post on) or the LDR path (no post, no
/// TAAU: the terrain tonemaps in its own shader).
fn airless_frame(hdr: bool) -> Vec<u8> {
    let stack = layers(vec![layer(
        "ice",
        [0.6, 0.6, 0.6],
        (0.0, 1.6),
        (-100.0, 100.0),
        1.0,
    )]);
    let mut draw = TerrainSharedDraw::new(device(), wall_floor_map(), &stack, CdlodConfig::default(), 3)
        .expect("terrain");
    draw.set_material(
        &material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default()),
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    // DO27's Bore Twelve: 18 klx sun at 24°, ground albedo ≈ 0.56.
    draw.set_sky_ambient(Some(SkyAmbient::airless(18_000.0, 24f32.to_radians(), [0.56; 3])));
    let post = PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: [14.0, 6.0, 0.0],
        target: [-3.5, 3.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: if hdr { Some(post.clone()) } else { None },
        // Sun in the west behind the wall: the wall's east face is in shadow, the floor lit.
        directional: DirectionalLightUniform::new([0.75, -0.66, 0.0], [1.0, 1.0, 1.0], 18_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = if hdr { Some(&post) } else { None };
    let mut renderer = FrameRenderer::new();
    // The LDR path has no irradiance probes; compare like with like.
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer.render_offscreen(device(), &frame).expect("frame").pixels
}

/// Mean luma of a rect in fractions of the frame.
fn rect_luma(pixels: &[u8], rect: [f32; 4]) -> f64 {
    let (x0, x1) = ((rect[0] * W as f32) as u32, (rect[2] * W as f32) as u32);
    let (y0, y1) = ((rect[1] * H as f32) as u32, (rect[3] * H as f32) as u32);
    let (mut s, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            s += luma(pixel(pixels, x, y));
            n += 1.0;
        }
    }
    s / n
}

#[test]
fn sky_ambient_is_exposed_like_the_sun_on_the_ldr_path() {
    let hdr = airless_frame(true);
    let ldr = airless_frame(false);
    dump("airless_hdr", &hdr);
    dump("airless_ldr", &ldr);
    // The shadowed wall face (ambient only) against the lit floor (sun + ambient).
    let wall = [0.35, 0.25, 0.65, 0.45];
    let floor = [0.2, 0.85, 0.8, 0.98];
    let ratio = |p: &[u8]| rect_luma(p, wall) / rect_luma(p, floor).max(1.0);
    let (rh, rl) = (ratio(&hdr), ratio(&ldr));
    eprintln!(
        "shadow/lit luma: HDR {rh:.3} (wall {:.1}, floor {:.1}), LDR {rl:.3} (wall {:.1}, floor {:.1})",
        rect_luma(&hdr, wall),
        rect_luma(&hdr, floor),
        rect_luma(&ldr, wall),
        rect_luma(&ldr, floor)
    );
    assert!(rect_luma(&hdr, floor) > 20.0, "the lit floor is black");
    assert!(
        (rl / rh - 1.0).abs() < 0.05,
        "the LDR path's ambient is not exposed like its sun: shadow/lit {rl:.3} vs HDR {rh:.3}"
    );
}

#[test]
fn mesa_flanks_keep_formation_bands_at_400_m() {
    // switchbacks_wide's far mesas are 300–500 m away, where 1.8 m beds are about a pixel
    // and average out: thicker formation bands must carry the banding there.
    let map = mesa_with_flank(52.0);
    let stack = layers(vec![layer(
        "sandstone",
        [0.62, 0.36, 0.22],
        (0.0, 1.6),
        (-100.0, 100.0),
        0.3,
    )]);
    let mat = material(vec![LayerStyle::Rock.bake(128, 4)], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.0, 25.0, 440.0],
        target: [0.0, 15.0, 40.0],
        fov_y_radians: 0.25,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let tod = TimeOfDay {
        azimuth_radians: 0.6,
        elevation_radians: 0.45,
        ..TimeOfDay::default()
    };
    let shaded = render(
        &map,
        &stack,
        Some(&mat),
        TerrainRelief::default(),
        &camera,
        6,
        0,
        &tod,
    );
    dump("mesa_400m", &shaded);
    // The 30 m flank spans ~38 px here.
    let bands = band_count(&shaded, H / 2 - 22, H / 2 + 18, 5.0);
    eprintln!("mesa flank at 400 m: {bands} bands over 40 px");
    assert!(bands >= 4, "the far flank reads as a smooth face: {bands} bands");
}

// ---------------------------------------------------------------------------
// 12. Round 5: the ground prints its authored reflectance (macro tint is relative)
// ---------------------------------------------------------------------------

/// sRGB byte of a linear value.
fn srgb_byte(linear: f32) -> u8 {
    regia_terrain::linear_to_srgb8(linear)
}

/// A macro bake whose albedo alternates 0.25 / 0.75 linear texel by texel (mean 0.5), AO 1.
fn half_mean_macro(world: f32) -> regia_terrain::MacroBake {
    let tile = |size: u32| {
        let mut data = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let v = srgb_byte(if (x + y) % 2 == 0 { 0.25 } else { 0.75 });
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        Rgba8Image {
            width: size,
            height: size,
            data,
        }
    };
    regia_terrain::MacroBake {
        world_size: world,
        tiles: 1,
        tile_size: 64,
        albedo: vec![tile(64)],
        normal: vec![Rgba8Image::solid(64, 64, [128, 128, 128, 255])],
        coarse_albedo: tile(32),
        coarse_normal: Rgba8Image::solid(32, 32, [128, 128, 128, 255]),
    }
}

/// Mean luma of the flat ground and of a 0.5-grey box top beside it, sun only.
fn ground_vs_grey_mesh() -> (f64, f64) {
    let world = 64.0;
    let map = TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], world, 4.0, -1.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness: 0.9,
        ..layer("ground", [1.0, 1.0, 1.0], (0.0, 1.6), (-100.0, 100.0), 1.0)
    }]);
    let v = srgb_byte(0.5);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(solid_set([v, v, v, 255]))],
            macro_set: Some(half_mean_macro(world)),
            params: MaterialParams {
                macro_world_size: world,
                macro_strength: 0.85,
                macro_normal_strength: 0.0,
                ..MaterialParams::default()
            },
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    let post = PostSettings {
        exposure_ev: 15.0,
        ..PostSettings::golden_deterministic()
    };
    // A 0.5-grey box whose top sits 0.4 m above the ground, right half of the frame.
    let (verts, idx) = regia_render::box_mesh([3.0, 0.2, 3.0]);
    let verts = regia_render::translate_mesh(&verts, [3.5, 0.2, 0.0]);
    let scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 14.0, 0.3],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 15.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.1, -1.0, 0.05], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.9,
            ..regia_render::LabMaterial::default()
        }],
        meshes: vec![regia_render::LabMesh {
            vertices: verts,
            indices: idx,
            material: 0,
            ..regia_render::LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let px = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    dump("reflectance_probe", &px);
    // Ground: the left third; box top: the middle of the right part.
    (
        rect_luma(&px, [0.05, 0.2, 0.3, 0.8]),
        rect_luma(&px, [0.6, 0.4, 0.75, 0.6]),
    )
}

#[test]
fn ground_prints_its_authored_reflectance_next_to_a_grey_mesh() {
    // A 0.5-reflectance layer under a mean-0.5 macro map must print like a 0.5-grey mesh:
    // the macro is a relative tint (DO27: "macro_albedo cropped to the map and
    // normalised"), not a second absolute albedo multiplied in.
    let (ground, mesh) = ground_vs_grey_mesh();
    eprintln!(
        "0.5 ground luma {ground:.1} vs 0.5-gray mesh {mesh:.1} (ratio {:.3})",
        ground / mesh
    );
    assert!(
        mesh > 40.0 && mesh < 240.0,
        "probe exposure out of range: mesh {mesh:.1}"
    );
    assert!(
        (ground / mesh - 1.0).abs() < 0.05,
        "ground {ground:.1} vs mesh {mesh:.1}"
    );
}

// ---------------------------------------------------------------------------
// 13. Round 6: sunlight bounced off sunlit walls onto shadowed ground
// ---------------------------------------------------------------------------

/// A 20 m-deep, 24 m-wide trench running along Z (walls at x = ±12), flat plain around it.
fn trench_map() -> TerrainMap {
    let n = 257u32;
    let width = 128.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push(-20.0 * (1.0 - ((x.abs() - 12.0) / 2.0).clamp(0.0, 1.0)));
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 24.0, -22.0).expect("map")
}

/// Mean luma of the shadowed trench floor and of the sunlit east wall, with or
/// without the bounce, under a low sun from the west and an airless (black) sky.
fn trench_frame(bounce: f32, probes: bool) -> (f64, f64) {
    let stack = layers(vec![layer(
        "ice",
        [0.7, 0.72, 0.75],
        (0.0, 1.6),
        (-100.0, 100.0),
        1.0,
    )]);
    let mut draw =
        TerrainSharedDraw::new(device(), trench_map(), &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &material(vec![solid_set([255, 255, 255, 255])], MaterialParams::default()),
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief {
        bounce,
        ..TerrainRelief::off()
    });
    // A black sky and no ground fill: what is left in the shadow is the bounce.
    draw.set_sky_ambient(Some(SkyAmbient {
        sky: [0.0; 3],
        ground: [0.0; 3],
    }));
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        // In the trench, looking north along it from 8 m above the floor.
        eye: [0.0, -12.0, -40.0],
        target: [0.0, -19.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 300.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        // Sun 30° up in the west (travel toward +X): the east wall (x = +12, facing −X) is
        // sunlit, the west half of the floor lies in the west wall's shadow.
        directional: DirectionalLightUniform::new([0.866, -0.5, 0.0], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    // `probes: false` is the tier without irradiance probes (the terrain's own bounce).
    renderer.set_ddgi_enabled(Some(probes));
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let px = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    dump(&format!("trench_bounce_{bounce}_probes_{probes}"), &px);
    // Looking +Z, +X is on the left: the sunlit east wall is the left wall; the floor (all
    // of it in the west wall's shadow at a 30° sun) fills the lower frame.
    (
        rect_luma(&px, [0.55, 0.75, 0.95, 0.98]),
        rect_luma(&px, [0.05, 0.15, 0.3, 0.4]),
    )
}

#[test]
fn sunlit_walls_bounce_light_onto_the_shadowed_trench_floor() {
    let (floor_off, wall) = trench_frame(0.0, false);
    let (floor_on, wall_on) = trench_frame(1.0, false);
    eprintln!("trench floor in shadow: {floor_off:.1} → {floor_on:.1}; sunlit wall {wall:.1} / {wall_on:.1}");
    assert!(wall > 60.0, "the east wall is not sunlit: {wall:.1}");
    assert!(
        floor_off < 12.0,
        "the floor is not in shadow without bounce: {floor_off:.1}"
    );
    // The floor gains visibly, and stays well below the wall that lights it (the bounce is
    // bounded by albedo × the walls' irradiance × their view factor).
    assert!(
        floor_on > floor_off + 15.0,
        "no bounce on the shadowed floor: {floor_off:.1} → {floor_on:.1}"
    );
    assert!(
        floor_on < wall * 0.75,
        "bounce brighter than physically possible: floor {floor_on:.1}, wall {wall:.1}"
    );
}

#[test]
fn probes_light_the_shadowed_trench_floor_from_the_sunlit_wall() {
    // The irradiance probes trace the trench: the shadowed floor takes the sunlit wall's
    // bounce (and the bounce of that bounce) without the terrain's own estimate, and stays
    // below the wall that lights it.
    let (dark, wall) = trench_frame(0.0, false);
    let (floor, wall_probes) = trench_frame(0.0, true);
    eprintln!("probes: trench floor {dark:.1} → {floor:.1}; sunlit wall {wall:.1} / {wall_probes:.1}");
    assert!(
        floor > dark + 15.0,
        "the probes add no bounce: {dark:.1} → {floor:.1}"
    );
    assert!(
        floor < wall_probes * 0.75,
        "bounce brighter than possible: floor {floor:.1}, wall {wall_probes:.1}"
    );
}

// ---------------------------------------------------------------------------
// 14. Motion-look round 2: the near grain has no lattice
// ---------------------------------------------------------------------------

/// Power of `signal`'s windowed 2D spectrum on the two axis lines (|kx| or |ky| ≤ 1 bin)
/// over that on the next three lines each side, per bin, within 1/40..1/4 cycles/px.
/// Straight features along the screen axes (a square lattice) put power on the axes; an
/// isotropic texture — however the sun's direction weights it — does not.
fn axis_line_power(signal: &[Vec<f64>]) -> f64 {
    let (h, w) = (signal.len(), signal[0].len());
    let mean = signal.iter().flatten().sum::<f64>() / (h * w) as f64;
    let hann = |i: usize, n: usize| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos();
    // Row DFTs, then column DFTs (plain O(n²) transforms: the frame is small).
    let mut rows = vec![vec![(0.0f64, 0.0f64); w]; h];
    for (y, row) in signal.iter().enumerate() {
        for (k, out) in rows[y].iter_mut().enumerate() {
            let (mut re, mut im) = (0.0, 0.0);
            for (x, v) in row.iter().enumerate() {
                let a = (v - mean) * hann(x, w) * hann(y, h);
                let t = -std::f64::consts::TAU * (k * x) as f64 / w as f64;
                re += a * t.cos();
                im += a * t.sin();
            }
            *out = (re, im);
        }
    }
    let (mut axis, mut axis_n, mut near, mut near_n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for kx in 0..w {
        for ky in 0..h {
            let (mut re, mut im) = (0.0, 0.0);
            for (y, row) in rows.iter().enumerate() {
                let t = -std::f64::consts::TAU * (ky * y) as f64 / h as f64;
                re += row[kx].0 * t.cos() - row[kx].1 * t.sin();
                im += row[kx].0 * t.sin() + row[kx].1 * t.cos();
            }
            let fx = kx.min(w - kx) as f64;
            let fy = ky.min(h - ky) as f64;
            let r = ((fx / w as f64).powi(2) + (fy / h as f64).powi(2)).sqrt();
            if !(1.0 / 40.0..1.0 / 4.0).contains(&r) {
                continue;
            }
            let power = re * re + im * im;
            if fx <= 1.0 || fy <= 1.0 {
                axis += power;
                axis_n += 1.0;
            } else if fx <= 4.0 || fy <= 4.0 {
                near += power;
                near_n += 1.0;
            }
        }
    }
    (axis / axis_n.max(1.0)) / (near / near_n.max(1.0)).max(1e-12)
}

#[test]
fn near_grain_has_no_square_lattice() {
    // Flat dust seen from 0.6 m, almost straight down, so world X/Z are the screen axes. The
    // grain was value noise differenced a quarter cell each way; value noise's slope is zero
    // across every lattice line, so the grain's normal shaded a square grid of 5 cm cells
    // (~30 px at 1440p on DO27's Switchbacks foreground).
    let n = 129;
    let map = TerrainMap::from_heights(n, n, vec![2.0; (n * n) as usize], 64.0, 8.0, 0.0).expect("flat");
    let stack = layers(vec![LayerDesc {
        roughness: 0.9,
        ..layer("ground", [0.8, 0.44, 0.26], (0.0, 1.6), (-100.0, 100.0), 2.8)
    }]);
    // A flat-coloured layer: the near detail is the only texture.
    let mat = material(vec![solid_set([200, 110, 66, 255])], MaterialParams::default());
    let camera = TerrainCamera {
        position: [0.3, 2.6, 0.2],
        target: [0.3, 2.0, 0.25],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.02,
        far: 200.0,
    };
    let root = regia_terrain::gpu::full_detail_root_level(&map, &CdlodConfig::with_grid_size(16));
    let tod = TimeOfDay::default();
    let frame = |near: f32| {
        render(
            &map,
            &stack,
            Some(&mat),
            TerrainRelief {
                near_detail: near,
                ..TerrainRelief::default()
            },
            &camera,
            root,
            0,
            &tod,
        )
    };
    let (on, off) = (frame(1.0), frame(0.0));
    dump("near_grain_top_down", &on);
    dump("near_grain_top_down_off", &off);
    let signal: Vec<Vec<f64>> = (H / 8..H * 7 / 8)
        .map(|y| {
            (W / 8..W * 7 / 8)
                .map(|x| luma(pixel(&on, x, y)) - luma(pixel(&off, x, y)))
                .collect()
        })
        .collect();
    let rms =
        (signal.iter().flatten().map(|v| v * v).sum::<f64>() / signal.iter().flatten().count() as f64).sqrt();
    let axis = axis_line_power(&signal);
    eprintln!("near grain top-down: rms {rms:.2}, axis-line power {axis:.2}× the neighboring lines'");
    assert!(rms > 1.0, "no near grain: rms {rms:.2}");
    // Measured: 1.42 with the value-noise grain, 0.82 with gradient noise.
    assert!(
        axis < 1.1,
        "the near grain's spectrum has {axis:.2}× the power on the world axes: a square lattice"
    );
}
