// SPDX-License-Identifier: MPL-2.0
//! Photoreal terrain material tests (lavapipe): height blending, detail normals with
//! distance fade, macro variation, stochastic anti-tiling, the sim-delta debug view and
//! crack-free LOD boundaries. Each test fails on the broken result it measures.

use std::sync::OnceLock;

use regia_terrain::gpu::{
    TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen, render_offscreen_opts,
};
use regia_terrain::{
    CdlodConfig, Crater, LayerDesc, LayerTextures, MacroBake, MaterialParams, MinMaxPyramid, RoadDecal,
    TerrainLayers, TerrainMap, TerrainMaterial, apply_crater, select_nodes,
};

const W: u32 = 320;
const H: u32 = 180;
const MAGENTA: [f64; 4] = [1.0, 0.0, 1.0, 1.0];

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// A gently rolling 257² map over 256 m: enough relief for slope variety, gentle enough for
/// stable slope statistics.
fn rolling_map() -> TerrainMap {
    let n = 257u32;
    let width = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            heights
                .push(12.0 * (-(x * x + z * z) / 9000.0).exp() + 2.0 * (x * 0.06).sin() * (z * 0.05).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 16.0, -2.0).expect("map")
}

/// A perfectly flat map (anti-tiling measurement needs shading-free uniform texture).
fn flat_map() -> TerrainMap {
    TerrainMap::from_heights(65, 65, vec![5.0; 65 * 65], 64.0, 8.0, 0.0).expect("flat map")
}

/// Two grass-like layers split by altitude (for the blend-sharpness measurement).
fn two_layers(height_blend: f32) -> TerrainLayers {
    two_layers_detail(height_blend, 0.0)
}

fn two_layers_detail(height_blend: f32, detail_strength: f32) -> TerrainLayers {
    let make = |name: &str, alt_min: f32, alt_max: f32| LayerDesc {
        detail_normal_strength: detail_strength,
        name: name.into(),
        albedo: [0.55, 0.25, 0.1],
        roughness: 0.95,
        tiling: 0.5,
        slope_min: 0.0,
        slope_max: 1.6,
        altitude_min: alt_min,
        altitude_max: alt_max,
        blend_sharpness: 0.15,
        triplanar: false,
        height_blend,
        ..LayerDesc::ground()
    };
    // Overlapping altitude spans whose ramps cross around 7 m — the sharpness is
    // span-relative, so the ramps are a few metres wide and cross the visible hill face.
    TerrainLayers {
        layers: vec![make("low", -10.0, 8.0), make("high", 5.0, 30.0)],
        ..Default::default()
    }
}

fn params() -> MaterialParams {
    MaterialParams {
        anti_tiling_cell: 4.0,
        ..Default::default()
    }
}

/// A near-top-down camera with a slight forward tilt: a perfectly vertical view direction
/// is parallel to the look-at up vector and degenerates the view matrix.
fn top_down_camera(height: f32, target_y: f32) -> TerrainCamera {
    TerrainCamera {
        position: [0.0, target_y + height, height * 0.02],
        target: [0.0, target_y, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 4000.0,
    }
}

fn oblique_camera() -> TerrainCamera {
    TerrainCamera {
        position: [0.0, 40.0, 130.0],
        target: [0.0, 8.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 4000.0,
    }
}

fn frame(camera: &TerrainCamera, debug_view: u32) -> TerrainFrame {
    let mut frame = TerrainFrame::from_camera(camera, &TimeOfDay::default(), 0.0, 1.0);
    frame.debug_view = debug_view;
    frame
}

/// Renders the map with a material at a given debug view.
fn render_material(
    map: &TerrainMap,
    layers: &TerrainLayers,
    material: &TerrainMaterial,
    sim: Option<&TerrainMap>,
    camera: &TerrainCamera,
    debug_view: u32,
    resident: &[u32],
) -> Vec<u8> {
    render_material_relief(
        map,
        layers,
        material,
        sim,
        camera,
        debug_view,
        resident,
        regia_terrain::TerrainRelief::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_material_relief(
    map: &TerrainMap,
    layers: &TerrainLayers,
    material: &TerrainMaterial,
    sim: Option<&TerrainMap>,
    camera: &TerrainCamera,
    debug_view: u32,
    resident: &[u32],
    relief: regia_terrain::TerrainRelief,
) -> Vec<u8> {
    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(device(), map, layers, &config, 4, None, None).expect("gpu");
    gpu.set_material(material, resident).expect("material");
    gpu.set_relief(relief);
    if let Some(sim) = sim {
        gpu.set_sim_map(sim).expect("sim");
    }
    let pyramid = MinMaxPyramid::build(map, 4).expect("pyramid");
    let selection = select_nodes(
        map,
        &pyramid,
        &config,
        4,
        [camera.position[0], camera.position[2]],
        None,
    );
    render_offscreen(
        &mut gpu,
        device(),
        &frame(camera, debug_view),
        &selection,
        W,
        H,
        [0.02, 0.02, 0.05, 1.0],
    )
    .expect("render")
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = (y * W + x) as usize * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn luma(p: [u8; 4]) -> f64 {
    0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
}

/// Variance of the luminance over the frame's centre patch (texture energy proxy).
fn centre_luma_variance(pixels: &[u8]) -> f64 {
    let mut values = Vec::new();
    for y in (H / 4)..(H * 3 / 4) {
        for x in (W / 4)..(W * 3 / 4) {
            values.push(luma(pixel(pixels, x, y)));
        }
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / values.len() as f64
}

/// Mean-subtracted normalised autocorrelation peak over lags `min_lag..=max_lag` of the
/// centre row's luminance. A tiled texture shows a strong peak at its repeat period;
/// stochastic anti-tiling suppresses it.
fn autocorrelation_peak(pixels: &[u8], min_lag: usize, max_lag: usize) -> (usize, f64) {
    let y = H / 2;
    let row: Vec<f64> = (0..W).map(|x| luma(pixel(pixels, x, y))).collect();
    let x0 = (W / 8) as usize;
    let x1 = (W - max_lag as u32 - W / 8) as usize;
    let mean = row[x0..x1].iter().sum::<f64>() / (x1 - x0) as f64;
    let centred: Vec<f64> = row[x0..x1].iter().map(|v| v - mean).collect();
    let energy: f64 = centred.iter().map(|v| v * v).sum();
    let mut best = (0usize, f64::MIN);
    for lag in min_lag..=max_lag {
        let c: f64 = centred
            .iter()
            .zip(centred.iter().skip(lag))
            .map(|(a, b)| a * b)
            .sum();
        let norm = c / energy.max(1e-9);
        if norm > best.1 {
            best = (lag, norm);
        }
    }
    best
}

/// A high-contrast, exactly periodic test texture: four sine stripe pairs per repeat.
fn stripe_texture(size: u32) -> regia_terrain::Rgba8Image {
    use regia_terrain::Rgba8Image;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = x as f32 / size as f32;
            let v = y as f32 / size as f32;
            let wave = (u * 4.0 * std::f32::consts::TAU).sin();
            let wave2 = (v * 2.0 * std::f32::consts::TAU).sin();
            let r = (wave * 0.5 + 0.5) * 255.0;
            let g = (wave2 * 0.5 + 0.5) * 255.0;
            data.extend_from_slice(&[r as u8, g as u8, 128, 255]);
        }
    }
    Rgba8Image {
        width: size,
        height: size,
        data,
    }
}

/// The material path switches on and renders a recognisable scene (no black/blank frame).
#[test]
fn material_path_renders() {
    let map = rolling_map();
    let layers = two_layers(0.0);
    let material = TerrainMaterial::procedural_cc0(64, 7, params());
    let pixels = render_material(&map, &layers, &material, None, &oblique_camera(), 0, &[]);
    let terrain: Vec<f64> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| luma([p[0], p[1], p[2], p[3]]))
        .collect();
    let mean = terrain.iter().sum::<f64>() / terrain.len() as f64;
    assert!(mean > 15.0, "frame too dark: {mean}");
    assert!(
        centre_luma_variance(&pixels) > 30.0,
        "frame has no texture detail"
    );
    // Sanity: the accounting knows about the material.
    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
    let before = gpu.texture_bytes();
    gpu.set_material(&material, &[]).expect("material");
    assert!(gpu.texture_bytes() > before + 64 * 64 * 4 * 3 * 4 / 3 / 2);
}

/// Terrain edits keep the precomputed slope pyramid (`slope_mips`, the per-pixel footprint
/// normal) in step with the heights: a crater applied through `update_heights` renders what a
/// renderer built from the cratered map renders. Without the slope refresh the crater would
/// shade with the old ground's normals over its whole bowl and rim.
#[test]
fn edited_heights_refresh_the_slope_pyramid() {
    let map = rolling_map();
    let layers = two_layers(0.0);
    let material = TerrainMaterial::procedural_cc0(64, 7, params());
    let camera = oblique_camera();
    let mut edited = map.clone();
    let dirty = apply_crater(
        &mut edited,
        &Crater {
            center: [25.0, 30.0],
            radius: 28.0,
            depth: 6.0,
            rim_height: 1.5,
        },
    )
    .expect("crater");
    assert!(!dirty.is_empty());
    let before = render_material(&map, &layers, &material, None, &camera, 0, &[]);
    let fresh = render_material(&edited, &layers, &material, None, &camera, 0, &[]);

    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
    gpu.set_material(&material, &[]).expect("material");
    gpu.update_heights(&edited, &dirty);
    let pyramid = MinMaxPyramid::build(&edited, 4).expect("pyramid");
    let selection = select_nodes(
        &edited,
        &pyramid,
        &config,
        4,
        [camera.position[0], camera.position[2]],
        None,
    );
    let in_place = render_offscreen(
        &mut gpu,
        device(),
        &frame(&camera, 0),
        &selection,
        W,
        H,
        [0.02, 0.02, 0.05, 1.0],
    )
    .expect("render");

    let differ = |a: &[u8], b: &[u8], tol: u8| {
        a.as_chunks::<4>()
            .0
            .iter()
            .zip(b.as_chunks::<4>().0)
            .filter(|(p, q)| (0..3).any(|c| p[c].abs_diff(q[c]) > tol))
            .count()
    };
    let edit_px = differ(&before, &fresh, 8);
    assert!(
        edit_px > 300,
        "the crater must show in the frame ({edit_px} px differ)"
    );
    let stale_px = differ(&in_place, &fresh, 2);
    assert!(
        stale_px * 50 < edit_px,
        "an in-place edit must render like a fresh upload ({stale_px} px differ, the edit moved {edit_px})"
    );
}

/// Height blending sharpens layer transitions: fewer intermediate splat weights at the
/// altitude boundary (debug view 4 measures the first two layers' weights).
#[test]
fn height_blend_sharpens_transitions() {
    let map = rolling_map();
    let mut soft = TerrainMaterial::procedural_cc0(64, 7, params());
    soft.layers[0].as_mut().expect("layer").height = {
        use regia_terrain::Rgba8Image;
        Rgba8Image::solid(64, 64, [64, 64, 64, 255])
    };
    soft.layers[1].as_mut().expect("layer").height = {
        use regia_terrain::Rgba8Image;
        Rgba8Image::solid(64, 64, [200, 200, 200, 255])
    };
    let soft_layers = two_layers(0.0);
    let mut sharp = soft.clone();
    sharp.layers[0].as_mut().expect("layer").height =
        regia_terrain::Rgba8Image::solid(64, 64, [64, 64, 64, 255]);
    let sharp_layers = two_layers(0.9);

    let count_intermediate = |pixels: &[u8]| -> usize {
        let mut n = 0;
        for y in 0..H {
            for x in 0..W {
                let p = pixel(pixels, x, y);
                // View 4 = (w0, w1, 0) on terrain; the sky pass fills the background with a
                // pale gradient whose R/G would masquerade as weights — require terrain
                // (blue channel 0).
                if p[2] > 20 {
                    continue;
                }
                // Decode the sRGB-encoded weights to linear before thresholding.
                let lin = |b: u8| (f64::from(b) / 255.0).powf(2.2);
                let w0 = lin(p[0]);
                let w1 = lin(p[1]);
                if w0 > 0.2 && w0 < 0.8 && w1 > 0.2 && w1 < 0.8 {
                    n += 1;
                }
            }
        }
        n
    };
    let soft_px = render_material(&map, &soft_layers, &soft, None, &oblique_camera(), 4, &[]);
    let sharp_px = render_material(&map, &sharp_layers, &sharp, None, &oblique_camera(), 4, &[]);
    // Frame-wide intermediate count: with the height-blend code broken the two renders are
    // bit-identical (ratio 1.0); sharpening measurably narrows the band (≈0.78 measured).
    let soft_n = count_intermediate(&soft_px);
    let sharp_n = count_intermediate(&sharp_px);
    assert!(soft_n > 500, "soft blend should have a transition band: {soft_n}");
    assert!(
        sharp_n * 20 < soft_n * 17,
        "height blend should sharpen the transition: {sharp_n} vs {soft_n}"
    );
}

/// Micro normals add high-frequency shading up close and fade out with distance
/// (debug view 2 = N·L of the shaded normal).
#[test]
fn detail_normals_add_and_fade() {
    let map = rolling_map();
    let layers = two_layers_detail(0.0, 1.0);
    let mut flat = TerrainMaterial::procedural_cc0(64, 7, params());
    let mut bumpy = flat.clone();
    for set in flat.layers.iter_mut().flatten() {
        set.normal = regia_terrain::Rgba8Image::solid(64, 64, [128, 128, 255, 255]);
    }
    for set in bumpy.layers.iter_mut().flatten() {
        set.normal = regia_terrain::LayerStyle::Rock.bake(64, 3).normal;
    }
    let near = top_down_camera(12.0, 5.0);
    let far = top_down_camera(600.0, 5.0);
    let var = |material: &TerrainMaterial, camera: &TerrainCamera| {
        centre_luma_variance(&render_material(&map, &layers, material, None, camera, 2, &[]))
    };
    let flat_near = var(&flat, &near);
    let bumpy_near = var(&bumpy, &near);
    assert!(
        bumpy_near > flat_near * 1.5 + 5.0,
        "micro normals should add near shading detail: {bumpy_near} vs {flat_near}"
    );
    let flat_far = var(&flat, &far);
    let bumpy_far = var(&bumpy, &far);
    assert!(
        bumpy_far < flat_far * 1.2 + 3.0,
        "micro normals must fade with distance: {bumpy_far} vs {flat_far}"
    );
}

/// Macro variation modulates the albedo across the map (view 3).
#[test]
fn macro_variation_modulates_albedo() {
    let map = rolling_map();
    let layers = two_layers(0.0);
    let mut plain = TerrainMaterial::procedural_cc0(64, 7, params());
    let mut macroed = plain.clone();
    let bake = MacroBake::from_heightfield(&map, 2, 128, 11).expect("bake");
    macroed.macro_set = Some(bake);
    macroed.params.macro_world_size = map.world_width();
    macroed.params.macro_strength = 0.9;
    plain.params.macro_world_size = 0.0;

    let plain_px = render_material(&map, &layers, &plain, None, &oblique_camera(), 3, &[]);
    let macro_px = render_material(&map, &layers, &macroed, None, &oblique_camera(), 3, &[]);
    // That the bake adds broad variation is held over the ground's own pixels by
    // `macro_variation_raises_the_terrain_variance` (ADR-5713, ADR-5890). The centre patch is one
    // third sky, so its variance is mostly the sky/ground gap: a variance check there holds only
    // while the bake moves the ground's mean by less than 0.022 of an 8-bit level, finer than an
    // 8-bit sRGB decode resolves. It passed on NVIDIA, lavapipe, Mali and Apple and failed on RADV.
    // The frames differ measurably (the bake's tint and AO modulate the albedo).
    let diff: f64 = plain_px
        .iter()
        .zip(&macro_px)
        .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs() as f64)
        .sum::<f64>()
        / plain_px.len() as f64;
    assert!(diff > 0.05, "macro barely changed the frame: {diff}");
}

/// The macro bake raises the albedo variance of the terrain's own pixels (view 3), on every
/// adapter (ADR-5713). `macro_variation_modulates_albedo` measured the frame's centre patch until
/// ADR-5890, a third of which is sky: that variance (about 2709) is mostly the gap between sky and
/// ground, so it follows the tint's mean, and adapters decode the bake's mean texel differently
/// (the tint is 0.4 % apart on an RX 7900 XTX and an RTX 5080). Over the ground alone the bake
/// adds 30 % on both (5.2 to 6.7).
#[test]
fn macro_variation_raises_the_terrain_variance() {
    let map = rolling_map();
    let layers = two_layers(0.0);
    let mut plain = TerrainMaterial::procedural_cc0(64, 7, params());
    let mut macroed = plain.clone();
    let bake = MacroBake::from_heightfield(&map, 2, 128, 11).expect("bake");
    macroed.macro_set = Some(bake);
    macroed.params.macro_world_size = map.world_width();
    macroed.params.macro_strength = 0.9;
    plain.params.macro_world_size = 0.0;

    let plain_px = render_material(&map, &layers, &plain, None, &oblique_camera(), 3, &[]);
    let macro_px = render_material(&map, &layers, &macroed, None, &oblique_camera(), 3, &[]);
    // The ground of the centre patch: in the albedo view the sky is brighter than any ground
    // texel (luminance about 174 against 66, nothing in between).
    let mut ground = Vec::new();
    for y in (H / 4)..(H * 3 / 4) {
        for x in (W / 4)..(W * 3 / 4) {
            if luma(pixel(&plain_px, x, y)) < 120.0 {
                ground.push((x, y));
            }
        }
    }
    let patch = (W / 2 * (H / 2)) as usize;
    assert!(
        ground.len() > patch / 2 && ground.len() < patch,
        "the patch holds ground and sky: {} of {patch} pixels are ground",
        ground.len()
    );
    let variance = |pixels: &[u8]| {
        let values: Vec<f64> = ground.iter().map(|&(x, y)| luma(pixel(pixels, x, y))).collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / values.len() as f64
    };
    let (with, without) = (variance(&macro_px), variance(&plain_px));
    eprintln!(
        "ground luminance variance: {with:.3} with the macro bake, {without:.3} without ({} ground pixels)",
        ground.len()
    );
    assert!(
        with > without * 1.1,
        "the macro bake adds broad variation to the ground: {with:.3} against {without:.3}"
    );
}

/// Stochastic anti-tiling suppresses the periodic autocorrelation peak of a tiled texture
/// (view 3 over a flat single-layer map, top-down camera).
#[test]
fn anti_tiling_reduces_periodicity() {
    let map = flat_map();
    let layers = TerrainLayers {
        layers: vec![LayerDesc {
            name: "grass".into(),
            albedo: [1.0, 1.0, 1.0],
            tiling: 0.5, // one repeat every 2 m
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let stripes = stripe_texture(64);
    let mut off = TerrainMaterial::procedural_cc0(64, 5, params());
    for set in off.layers.iter_mut().flatten() {
        set.albedo = stripes.clone();
        set.normal = regia_terrain::Rgba8Image::solid(64, 64, [128, 128, 255, 255]);
    }
    off.params.anti_tiling_cell = 0.0; // disabled
    let mut on = off.clone();
    on.params.anti_tiling_cell = 3.0;

    let camera = top_down_camera(14.0, 5.0);
    // 14 m height, fov 1 rad → view width 2·14·tan(0.5) ≈ 15.3 m; 320 px / 15.3 ≈ 20.9 px/m.
    // One 2 m repeat ≈ 42 px. Search lags 30..60.
    // Near-field detail off: the stripes' cool, bright parts read as ice, and the crust's frost
    // patches (round 8: they resolve out to 30 cm pixels) break up the very periodicity this
    // measures. The layer texture's anti-tiling is what is under test.
    let relief = regia_terrain::TerrainRelief {
        near_detail: 0.0,
        ..regia_terrain::TerrainRelief::default()
    };
    let off_px = render_material_relief(&map, &layers, &off, None, &camera, 3, &[], relief);
    let on_px = render_material_relief(&map, &layers, &on, None, &camera, 3, &[], relief);
    let (lag_off, peak_off) = autocorrelation_peak(&off_px, 30, 60);
    let (lag_on, peak_on) = autocorrelation_peak(&on_px, 30, 60);
    assert!(
        peak_off > 0.55,
        "plain tiling should show a strong periodic peak (lag {lag_off}: {peak_off})"
    );
    assert!(
        peak_on < peak_off * 0.8,
        "stochastic tiling should suppress the peak: {peak_on} vs {peak_off} (lag {lag_on})"
    );
}

/// The visual-vs-sim delta debug view shows both agreement and divergence zones.
#[test]
fn sim_delta_view_renders() {
    // Visual: rolling map + a ripple that only exists east of x = 30 m.
    let visual = {
        let base = rolling_map();
        let n = base.width();
        let mut heights = base.heights().to_vec();
        for row in 0..n {
            for col in 0..n {
                let x = -128.0 + col as f32 / (n - 1) as f32 * 256.0;
                let z = -128.0 + row as f32 / (n - 1) as f32 * 256.0;
                let mask = ((x - 30.0) / 20.0).clamp(0.0, 1.0).powi(2);
                heights[(row * n + col) as usize] += 1.2 * mask * (x * 0.35).sin() * (z * 0.21).cos();
            }
        }
        TerrainMap::from_heights(n, n, heights, 256.0, 16.0, -2.0).expect("visual")
    };
    // The sim grid: the same base at half resolution (the DO27 shape: coarse sim ground).
    let n = 129u32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = (col as f32 / (n - 1) as f32) * 256.0 - 128.0;
            let z = (row as f32 / (n - 1) as f32) * 256.0 - 128.0;
            heights
                .push(12.0 * (-(x * x + z * z) / 9000.0).exp() + 2.0 * (x * 0.06).sin() * (z * 0.05).cos());
        }
    }
    let sim = TerrainMap::from_heights(n, n, heights, 256.0, 16.0, -2.0).expect("sim");

    let layers = two_layers(0.0);
    let material = TerrainMaterial::procedural_cc0(64, 7, params());
    let pixels = render_material(&visual, &layers, &material, Some(&sim), &oblique_camera(), 7, &[]);
    let mut green = 0usize;
    let mut heat = 0usize;
    let mut total = 0usize;
    for y in 0..H {
        for x in 0..W {
            let p = pixel(&pixels, x, y);
            if p[0] == 0 && p[1] == 0 && p[2] == 0 {
                continue; // sky (view off / unset regions render black)
            }
            total += 1;
            let (r, g) = (p[0], p[1]);
            if g > 200 && r < 60 {
                green += 1; // within 1 cm
            } else if r > 120 || (p[2] > 200 && g < 100) {
                heat += 1; // diverged
            }
        }
    }
    assert!(total > 5000, "terrain not rendered: {total}");
    assert!(green > total / 20, "expected agreement zones: {green}/{total}");
    assert!(heat > total / 20, "expected divergence zones: {heat}/{total}");
}

/// A LOD level boundary renders crack-free: with the sky pass disabled and a magenta clear,
/// no background pixel may appear inside the terrain's covered rectangle.
#[test]
fn lod_boundary_is_crack_free() {
    let map = rolling_map();
    let layers = two_layers(0.0);
    let config = CdlodConfig::with_grid_size(16);
    // Frame the map interior at the bottom of the view so the column scan can tell cracks
    // from sky: every column that reaches the bottom through terrain must stay covered up to
    // its topmost terrain pixel.
    let camera = TerrainCamera {
        position: [0.0, 60.0, 200.0],
        target: [0.0, 10.0, -100.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 4000.0,
    };
    let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid");
    let selection = select_nodes(
        &map,
        &pyramid,
        &config,
        4,
        [camera.position[0], camera.position[2]],
        None,
    );
    // The camera must actually straddle levels: at least two distinct levels selected and a
    // morph band in progress somewhere.
    let distinct: std::collections::BTreeSet<u32> = selection.nodes.iter().map(|n| n.node.level).collect();
    assert!(distinct.len() >= 2, "camera sees only level(s) {distinct:?}");
    assert!(
        selection
            .nodes
            .iter()
            .any(|n| n.morph_start > 0.0 && n.morph_end > n.morph_start),
        "no morph band active"
    );

    let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
    let pixels = render_offscreen_opts(
        &mut gpu,
        device(),
        &frame(&camera, 0),
        &selection,
        W,
        H,
        MAGENTA,
        false,
    )
    .expect("render");

    let is_magenta = |p: [u8; 4]| p[0] > 180 && p[1] < 70 && p[2] > 180;
    let mut min_x = W;
    let mut max_x = 0u32;
    let mut min_y = H;
    let mut max_y = 0u32;
    for y in 0..H {
        for x in 0..W {
            if !is_magenta(pixel(&pixels, x, y)) {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    assert!(max_x > min_x + 100, "terrain not visible");
    // Per column: when the bottom row is covered, the column must be solid terrain from the
    // bottom up to its topmost covered pixel; a magenta pixel inside that span is a crack.
    let mut leaks = 0usize;
    let mut columns = 0usize;
    for x in min_x..=max_x {
        if is_magenta(pixel(&pixels, x, H - 1)) {
            continue;
        }
        columns += 1;
        let mut top = H - 1;
        for y in (0..H).rev() {
            if !is_magenta(pixel(&pixels, x, y)) {
                top = y;
            }
        }
        for y in top..H {
            if is_magenta(pixel(&pixels, x, y)) {
                leaks += 1;
            }
        }
    }
    assert!(columns > 100, "too few covered columns: {columns}");
    assert_eq!(
        leaks, 0,
        "{leaks} background pixels leaked through the terrain (cracks)"
    );
}

/// Streaming residency: rendering with a subset of macro tiles still produces a coherent
/// frame (the coarse fallback covers the unstreamed tiles).
#[test]
fn macro_streaming_subset_renders() {
    let map = rolling_map();
    let layers = two_layers(0.0);
    let mut material = TerrainMaterial::procedural_cc0(64, 7, params());
    let bake = MacroBake::from_heightfield(&map, 2, 64, 3).expect("bake");
    material.macro_set = Some(bake);
    material.params.macro_world_size = map.world_width();

    let full = render_material(&map, &layers, &material, None, &oblique_camera(), 0, &[]);
    let subset = render_material(&map, &layers, &material, None, &oblique_camera(), 0, &[0]);
    // Both render sane frames; the subset relies on the coarse fallback for 3 of 4 tiles.
    assert!(centre_luma_variance(&subset) > 10.0, "subset frame washed out");
    let diff: f64 = full
        .iter()
        .zip(&subset)
        .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs() as f64)
        .sum::<f64>()
        / full.len() as f64;
    assert!(diff > 0.005, "residency had no visible effect: {diff}");
    // And streaming is real: the subset allocates fewer texture bytes than the full set.
    let config = CdlodConfig::with_grid_size(16);
    let bytes = |resident: &[u32]| -> u64 {
        let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
        gpu.set_material(&material, resident).expect("material");
        gpu.texture_bytes()
    };
    let full_bytes = bytes(&[]);
    let subset_bytes = bytes(&[0]);
    assert!(
        subset_bytes < full_bytes - 8 * 64 * 64,
        "streaming must save macro tile memory: {subset_bytes} vs {full_bytes}"
    );
}

fn solid_set(albedo: [u8; 4], height: u8, orm: [u8; 4], use_orm: bool) -> LayerTextures {
    LayerTextures {
        albedo: regia_terrain::Rgba8Image::solid(32, 32, albedo),
        normal: regia_terrain::Rgba8Image::solid(32, 32, [128, 128, 255, 255]),
        height: regia_terrain::Rgba8Image::solid(32, 32, [height, height, height, 255]),
        orm: regia_terrain::Rgba8Image::solid(32, 32, orm),
        use_orm,
    }
}

/// Albedo is red for v < 0.2 and blue above, so the top-plane UV (near v = 0 at the
/// origin) and the side-plane UV (v ≈ 0.5 when the surface sits near y = 10 and tiling
/// is 0.05) land in different constant regions, away from the filter edge.
fn split_plane_set() -> LayerTextures {
    let size = 64u32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        let v = (y as f32 + 0.5) / size as f32;
        let px = if v < 0.2 {
            [220u8, 16, 16, 255]
        } else {
            [16, 16, 220, 255]
        };
        for _x in 0..size {
            data.extend_from_slice(&px);
        }
    }
    LayerTextures {
        albedo: regia_terrain::Rgba8Image {
            width: size,
            height: size,
            data,
        },
        normal: regia_terrain::Rgba8Image::solid(size, size, [128, 128, 255, 255]),
        height: regia_terrain::Rgba8Image::solid(size, size, [128, 128, 128, 255]),
        orm: regia_terrain::Rgba8Image::solid(size, size, [255, 255, 0, 255]),
        use_orm: false,
    }
}

fn full_cover_layer(name: &str, triplanar: bool, height_blend: f32, tiling: f32) -> LayerDesc {
    LayerDesc {
        name: name.into(),
        albedo: [1.0, 1.0, 1.0],
        roughness: 0.8,
        tiling,
        slope_min: 0.0,
        slope_max: 2.0,
        altitude_min: -500.0,
        altitude_max: 500.0,
        blend_sharpness: 0.05,
        triplanar,
        height_blend,
        detail_normal_strength: 0.0,
        ..LayerDesc::ground()
    }
}

fn material_of(sets: Vec<LayerTextures>, anti_tiling_cell: f32) -> TerrainMaterial {
    TerrainMaterial {
        layers: sets.into_iter().map(Some).collect(),
        macro_set: None,
        params: MaterialParams {
            anti_tiling_cell,
            triplanar_auto_slope: 1.5,
            macro_strength: 0.0,
            ..Default::default()
        },
    }
}

fn render_black(
    map: &TerrainMap,
    layers: &TerrainLayers,
    material: &TerrainMaterial,
    sim: Option<&TerrainMap>,
    camera: &TerrainCamera,
    debug_view: u32,
    road: Option<RoadDecal>,
) -> Vec<u8> {
    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(device(), map, layers, &config, 4, None, None).expect("gpu");
    gpu.set_material(material, &[]).expect("material");
    // These probes look straight down from a few metres onto a 76° ramp: cliff relief
    // (round 8) moves that face metres toward the camera, which is not what they test (plane
    // selection, flags, ORM). `cliff_relief.rs` covers the relief.
    gpu.set_relief(regia_terrain::TerrainRelief {
        cliff_relief: 0.0,
        ..regia_terrain::TerrainRelief::default()
    });
    if let Some(sim) = sim {
        gpu.set_sim_map(sim).expect("sim");
    }
    if road.is_some() {
        gpu.set_road(road);
    }
    let pyramid = MinMaxPyramid::build(map, 4).expect("pyramid");
    let selection = select_nodes(
        map,
        &pyramid,
        &config,
        4,
        [camera.position[0], camera.position[2]],
        None,
    );
    render_offscreen(
        &mut gpu,
        device(),
        &frame(camera, debug_view),
        &selection,
        W,
        H,
        [0.0, 0.0, 0.0, 1.0],
    )
    .expect("render")
}

fn centre_mean(pixels: &[u8]) -> [f64; 3] {
    let mut s = [0.0; 3];
    let mut n = 0.0;
    for y in (H / 2 - 12)..(H / 2 + 12) {
        for x in (W / 2 - 12)..(W / 2 + 12) {
            let p = pixel(pixels, x, y);
            s[0] += f64::from(p[0]);
            s[1] += f64::from(p[1]);
            s[2] += f64::from(p[2]);
            n += 1.0;
        }
    }
    [s[0] / n, s[1] / n, s[2] / n]
}

fn flat_at(y: f32) -> TerrainMap {
    TerrainMap::from_heights(33, 33, vec![y; 33 * 33], 32.0, 40.0, -10.0).expect("flat")
}

/// A ramp of slope atan(4) whose centre sits at `y = 10`, plus a uniform height offset.
fn steep_pair(extra: f32) -> (TerrainMap, TerrainMap) {
    let n = 33u32;
    let world = 32.0f32;
    let mut visual = Vec::with_capacity((n * n) as usize);
    let mut sim = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let h = 4.0 * x + 10.0;
            sim.push(h);
            visual.push(h + extra);
        }
    }
    let map = |heights| TerrainMap::from_heights(n, n, heights, world, 200.0, -80.0).expect("ramp");
    (map(visual), map(sim))
}

/// Flat flagged triplanar keeps the top-plane sample. Removing the top weight, or
/// replacing the top with the side planes, returns the blue side colour instead.
#[test]
fn triplanar_keeps_the_top_plane_when_the_flag_is_on() {
    let map = flat_at(10.0);
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("paint", true, 0.0, 0.05)],
        ..Default::default()
    };
    let material = material_of(vec![split_plane_set()], 0.0);
    // Shift the look point to world Z ≈ 2 m so the top-plane V is 0.1 (tiling
    // 0.05), inside the red band. V = 0 is the repeat seam and mixes in the blue
    // row. Keep the camera's small Z tilt: a straight-down look is degenerate.
    let mut camera = top_down_camera(2.0, 10.0);
    camera.position[2] += 2.0;
    camera.target[2] += 2.0;
    let pixels = render_black(&map, &layers, &material, None, &camera, 3, None);
    let [r, _, b] = centre_mean(&pixels);
    assert!(
        r > 160.0 && b < 80.0,
        "flat triplanar must stay on the red top plane, got R {r:.0} B {b:.0} \
         (side-only blend is blue)"
    );
}

/// A steep face mixes the side-plane sample in. Dropping the side terms leaves the
/// red top colour and this assert fails.
#[test]
fn triplanar_steep_face_includes_the_side_planes() {
    let (map, _) = steep_pair(0.0);
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("paint", true, 0.0, 0.05)],
        ..Default::default()
    };
    let material = material_of(vec![split_plane_set()], 0.0);
    let pixels = render_black(
        &map,
        &layers,
        &material,
        None,
        &top_down_camera(8.0, 10.0),
        3,
        None,
    );
    let [r, _, b] = centre_mean(&pixels);
    assert!(
        b > r + 30.0 && b > 140.0,
        "steep triplanar must take the blue side planes (B {b:.0}, R {r:.0})"
    );
}

/// Height textures decide which full-coverage layer wins. Swapping the textures swaps
/// the weight, so a shader that ignores them produces two identical frames.
#[test]
fn height_blend_follows_the_height_textures() {
    let map = flat_at(5.0);
    let layers = TerrainLayers {
        layers: vec![
            full_cover_layer("low", false, 1.0, 0.25),
            full_cover_layer("high", false, 1.0, 0.25),
        ],
        ..Default::default()
    };
    let high_on_second = material_of(
        vec![
            solid_set([255, 0, 0, 255], 0, [255, 255, 0, 255], false),
            solid_set([0, 255, 0, 255], 255, [255, 255, 0, 255], false),
        ],
        0.0,
    );
    let high_on_first = material_of(
        vec![
            solid_set([255, 0, 0, 255], 255, [255, 255, 0, 255], false),
            solid_set([0, 255, 0, 255], 0, [255, 255, 0, 255], false),
        ],
        0.0,
    );
    let camera = top_down_camera(6.0, 5.0);
    let a = render_black(&map, &layers, &high_on_second, None, &camera, 4, None);
    let b = render_black(&map, &layers, &high_on_first, None, &camera, 4, None);
    let [ar, ag, _] = centre_mean(&a);
    let [br, bg, _] = centre_mean(&b);
    assert!(
        ag > ar * 2.0 && ag > 160.0,
        "height 1 on layer 1 must dominate G, got R {ar:.0} G {ag:.0}"
    );
    assert!(
        br > bg * 2.0 && br > 160.0,
        "height 1 on layer 0 must dominate R, got R {br:.0} G {bg:.0}"
    );
    let changed = a.iter().zip(&b).any(|(p, q)| p != q);
    assert!(changed, "swapping height textures changed nothing");
}

fn count_flag(pixels: &[u8], kind: &str) -> usize {
    pixels
        .chunks(4)
        .filter(|p| match kind {
            "green" => p[1] > 200 && p[0] < 40 && p[2] < 180,
            "red" => p[0] > 200 && p[1] < 40 && p[2] < 40,
            "blue" => p[2] > 200 && p[0] < 180 && p[2] > p[1] && p[2] > p[0],
            "yellow" => p[0] > 200 && p[1] > 200 && p[2] < 40,
            _ => false,
        })
        .count()
}

fn assert_flag(pixels: &[u8], kind: &str) {
    let n = count_flag(pixels, kind);
    assert!(
        n > (W * H / 3) as usize,
        "expected mostly {kind} agreement pixels, got {n}"
    );
}

/// Debug view 7 uses the DO27 bounds: ±0.25 m on driveable ground, ±2 m on cliffs.
/// A 1 m cliff error stays blue; the old 0.5 m heat painted it red.
#[test]
fn agreement_view_flags_driveable_and_cliff_bounds() {
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("ground", false, 0.0, 0.2)],
        ..Default::default()
    };
    let material = material_of(
        vec![solid_set([180, 180, 180, 255], 128, [255, 255, 0, 255], false)],
        0.0,
    );
    let flat_cam = top_down_camera(8.0, 5.0);
    let (within, sim) = {
        let visual = flat_at(5.10);
        let sim = flat_at(5.0);
        (visual, sim)
    };
    let px = render_black(&within, &layers, &material, Some(&sim), &flat_cam, 7, None);
    assert_flag(&px, "green");

    let over = flat_at(5.40);
    let px = render_black(&over, &layers, &material, Some(&sim), &flat_cam, 7, None);
    assert_flag(&px, "red");
    assert_eq!(
        count_flag(&px, "yellow"),
        0,
        "a driveable 0.40 m error is not a cliff"
    );

    let steep_cam = top_down_camera(12.0, 10.0);
    let (cliff_ok, cliff_sim) = steep_pair(1.0);
    let px = render_black(
        &cliff_ok,
        &layers,
        &material,
        Some(&cliff_sim),
        &steep_cam,
        7,
        None,
    );
    assert_flag(&px, "blue");
    assert!(
        count_flag(&px, "red") < (W * H / 10) as usize,
        "a 1 m cliff must not saturate to the driveable red"
    );

    let (cliff_over, _) = steep_pair(3.0);
    let px = render_black(
        &cliff_over,
        &layers,
        &material,
        Some(&cliff_sim),
        &steep_cam,
        7,
        None,
    );
    assert_flag(&px, "yellow");
}

/// glTF ORM occlusion (R) occludes indirect light, as glTF 2.0 § 3.9.4 defines
/// `occlusionTexture`: AO 0 crushes the sky-lit (ambient-only) ground and leaves the
/// sunlit albedo alone. ADR-060 (terrain round 5) moved it from the albedo, where it also
/// dimmed the sun (DO27's snow ORM averages 0.55 and dune sand 0.36: their sunlit ground
/// printed 2–3× under its reflectance).
#[test]
fn orm_occlusion_darkens_indirect_light_not_the_albedo() {
    let map = flat_at(4.0);
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("ground", false, 0.0, 0.2)],
        ..Default::default()
    };
    let camera = top_down_camera(6.0, 4.0);
    let dark = material_of(
        vec![solid_set([255, 255, 255, 255], 128, [0, 200, 0, 255], true)],
        0.0,
    );
    let bright = material_of(
        vec![solid_set([255, 255, 255, 255], 128, [255, 200, 0, 255], true)],
        0.0,
    );
    let render_with = |material: &TerrainMaterial, tod: &TimeOfDay, debug: u32| {
        let config = CdlodConfig::with_grid_size(16);
        let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
        gpu.set_material(material, &[]).expect("material");
        let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid");
        let selection = select_nodes(
            &map,
            &pyramid,
            &config,
            4,
            [camera.position[0], camera.position[2]],
            None,
        );
        let mut frame = TerrainFrame::from_camera(&camera, tod, 0.0, 1.0);
        frame.debug_view = debug;
        render_offscreen(&mut gpu, device(), &frame, &selection, W, H, [0.0, 0.0, 0.0, 1.0]).expect("render")
    };
    let luma3 = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    // Ambient only (no sun): AO 0 crushes the ground, AO 1 keeps it bright.
    let sky_only = TimeOfDay {
        illuminance_lux: 0.0,
        ambient: [0.5, 0.5, 0.5],
        ..TimeOfDay::default()
    };
    let dark_sky = luma3(centre_mean(&render_with(&dark, &sky_only, 0)));
    let bright_sky = luma3(centre_mean(&render_with(&bright, &sky_only, 0)));
    assert!(
        dark_sky < 30.0,
        "AO 0 must crush the sky light, got {dark_sky:.0}"
    );
    assert!(
        bright_sky > 120.0,
        "AO 1 must keep the sky light, got {bright_sky:.0}"
    );
    // Sun only: AO does not dim the sunlit ground, and the albedo view ignores AO.
    let sun_only = TimeOfDay {
        ambient: [0.0, 0.0, 0.0],
        ..TimeOfDay::default()
    };
    let dark_sun = luma3(centre_mean(&render_with(&dark, &sun_only, 0)));
    let bright_sun = luma3(centre_mean(&render_with(&bright, &sun_only, 0)));
    assert!(bright_sun > 60.0, "sunlit ground is dark: {bright_sun:.0}");
    assert!(
        (dark_sun - bright_sun).abs() < 3.0,
        "AO dims the sun: {dark_sun:.0} vs {bright_sun:.0}"
    );
    let dark_albedo = luma3(centre_mean(&render_with(&dark, &TimeOfDay::default(), 3)));
    let bright_albedo = luma3(centre_mean(&render_with(&bright, &TimeOfDay::default(), 3)));
    assert!(
        (dark_albedo - bright_albedo).abs() < 3.0,
        "AO changes the albedo: {dark_albedo:.0} vs {bright_albedo:.0}"
    );
}

/// The road replaces albedo in the same fragment, so the centre is the road colour and
/// the frame still contains the surrounding material (no second-surface z-fight).
#[test]
fn road_decal_recolours_without_a_second_depth() {
    let map = flat_at(2.0);
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("ground", false, 0.0, 0.2)],
        ..Default::default()
    };
    let material = material_of(
        vec![solid_set([200, 30, 30, 255], 128, [255, 255, 0, 255], false)],
        0.0,
    );
    // Linear 0.02 encodes to ~40 in the sRGB target, so the centre stays under the
    // red-channel cap while the material off the ribbon stays near 200.
    let road = RoadDecal::new([-20.0, 0.0], [20.0, 0.0], 1.5, [0.02, 0.85, 0.05]);
    let pixels = render_black(
        &map,
        &layers,
        &material,
        None,
        &top_down_camera(14.0, 2.0),
        3,
        Some(road),
    );
    let [cr, cg, cb] = centre_mean(&pixels);
    assert!(
        cg > 160.0 && cr < 80.0,
        "road center should be the decal green, got ({cr:.0}, {cg:.0}, {cb:.0})"
    );
    // A pixel well off the 1.5 m ribbon is still the red material.
    let edge = pixel(&pixels, W / 2, 8);
    assert!(
        edge[0] > 140 && edge[1] < 80,
        "off-ribbon pixel should stay the material, got {edge:?}"
    );
}

fn mean_abs(a: &[u8], b: &[u8]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(p, q)| (i16::from(*p) - i16::from(*q)).unsigned_abs() as f64)
        .sum::<f64>()
        / a.len() as f64
}

fn zero_morph(selection: &regia_terrain::LodSelection) -> regia_terrain::LodSelection {
    let mut snapped = selection.clone();
    for node in &mut snapped.nodes {
        node.morph_start = 0.0;
        node.morph_end = 0.0;
    }
    snapped
}

/// Thirty frames of a camera dolly. Each step renders the current CDLOD selection and
/// the previous frame's selection at the same camera. Morphing keeps that diff under
/// the bound; zeroing the morph band (the broken result) pops by more.
#[test]
fn lod_pop_stays_under_the_bound_across_30_frames() {
    // Wavelength ~2.9 m: the finest CDLOD cell (~1 m at root level 4 over 256 m)
    // resolves the ridges and the parent cells (~4 m and up) flatten them, so a
    // snapped LOD change moves the height image. A smooth hill does not.
    let n = 257u32;
    let width = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push(6.0 * (x * 2.2).sin().abs() * (0.65 + 0.35 * (z * 0.4).cos()));
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, width, 16.0, -1.0).expect("ridges");
    let layers = two_layers(0.0);
    let config = CdlodConfig::with_grid_size(16);
    let root_level = 4u32;
    let pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
    let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, root_level, None, None).expect("gpu");
    let mut max_pop = 0.0f64;
    let mut max_snap = 0.0f64;
    let mut prev: Option<regia_terrain::LodSelection> = None;
    let mut changes = 0u32;
    for step in 0..30 {
        let t = step as f32 / 29.0;
        let radius = 200.0 - 160.0 * t;
        let camera = TerrainCamera {
            position: [radius * 0.55, 36.0 + 10.0 * t, radius],
            target: [0.0, 8.0, 0.0],
            fov_y_radians: 1.0,
            aspect: W as f32 / H as f32,
            near: 0.5,
            far: 2000.0,
        };
        let selection = select_nodes(
            &map,
            &pyramid,
            &config,
            root_level,
            [camera.position[0], camera.position[2]],
            None,
        );
        if let Some(previous) = &prev {
            let same = previous.nodes.len() == selection.nodes.len()
                && previous
                    .nodes
                    .iter()
                    .zip(&selection.nodes)
                    .all(|(a, b)| a.node == b.node);
            if !same {
                changes += 1;
                let frame = frame(&camera, 6);
                let clear = [0.0, 0.0, 0.0, 1.0];
                let now = render_offscreen_opts(&mut gpu, device(), &frame, &selection, W, H, clear, false)
                    .expect("now");
                let before = render_offscreen_opts(&mut gpu, device(), &frame, previous, W, H, clear, false)
                    .expect("prev");
                max_pop = max_pop.max(mean_abs(&now, &before));
                let snap_now = zero_morph(&selection);
                let snap_prev = zero_morph(previous);
                let snapped_now =
                    render_offscreen_opts(&mut gpu, device(), &frame, &snap_now, W, H, clear, false)
                        .expect("snap now");
                let snapped_before =
                    render_offscreen_opts(&mut gpu, device(), &frame, &snap_prev, W, H, clear, false)
                        .expect("snap prev");
                max_snap = max_snap.max(mean_abs(&snapped_now, &snapped_before));
            }
        }
        prev = Some(selection);
    }
    assert!(changes >= 1, "the dolly never crossed a LOD boundary");
    eprintln!("lod pop morphed {max_pop:.3} snapped {max_snap:.3} changes {changes}");
    // Measured with morph on this map: the same-camera selection diff stays well under
    // 4/255. The snapped band (morph disabled) is the broken result and must sit above it.
    assert!(
        max_pop < 4.0,
        "30-frame LOD pop {max_pop:.3} exceeds the bound of 4"
    );
    assert!(
        max_snap > max_pop + 0.25,
        "disabling morph ({max_snap:.3}) must pop more than the morphed path ({max_pop:.3})"
    );
}

/// A layer whose slope/altitude band is zero stays at zero weight however tall its height
/// texture is. Round 3's additive height blend (`w + 0.7·hb·(h − ½)`) gave such a layer
/// weight wherever its texture peaked: the snow layer printed a white dot lattice over the
/// 50 m grass. Here the out-of-band green layer would take the frame.
#[test]
fn height_blend_cannot_raise_a_layer_outside_its_band() {
    let map = flat_at(5.0);
    let mut out_of_band = full_cover_layer("snow", false, 1.0, 0.25);
    out_of_band.altitude_min = 1000.0;
    out_of_band.altitude_max = 2000.0;
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("grass", false, 1.0, 0.25), out_of_band],
        ..Default::default()
    };
    let material = material_of(
        vec![
            solid_set([220, 16, 16, 255], 0, [255, 255, 0, 255], false),
            solid_set([16, 220, 16, 255], 255, [255, 255, 0, 255], false),
        ],
        0.0,
    );
    let pixels = render_black(
        &map,
        &layers,
        &material,
        None,
        &top_down_camera(6.0, 5.0),
        3,
        None,
    );
    let [r, g, _] = centre_mean(&pixels);
    assert!(
        r > 160.0 && g < 60.0,
        "the out-of-band layer leaked in: R {r:.0} G {g:.0} (expected the in-band red)"
    );
}

/// Mean absolute difference between neighbouring columns (and rows) of the luminance, and
/// the worst ratio of one step to the steps two pixels either side. A hard seam is a
/// single-pixel step far above its neighbours; a continuous blend has none.
fn worst_step_ratio(pixels: &[u8]) -> f64 {
    let l = |x: u32, y: u32| luma(pixel(pixels, x, y));
    let cols: Vec<f64> = (0..W - 1)
        .map(|x| (0..H).map(|y| (l(x + 1, y) - l(x, y)).abs()).sum::<f64>() / f64::from(H))
        .collect();
    let rows: Vec<f64> = (0..H - 1)
        .map(|y| (0..W).map(|x| (l(x, y + 1) - l(x, y)).abs()).sum::<f64>() / f64::from(W))
        .collect();
    let worst = |d: &[f64]| {
        (2..d.len() - 2)
            .map(|i| d[i] / (0.5 * (d[i - 2] + d[i + 2]) + 0.5))
            .fold(0.0f64, f64::max)
    };
    worst(&cols).max(worst(&rows))
}

/// Stochastic anti-tiling blends continuously across its cells. The round-3 version
/// switched hashed offsets at every square cell edge (a vertical seam through the 1 m
/// near shot at x = 40 m); a smooth low-frequency texture shows that as a step line.
#[test]
fn stochastic_tiling_has_no_cell_seam() {
    let map = flat_at(5.0);
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("ground", false, 0.0, 0.25)],
        ..Default::default()
    };
    // One smooth period per repeat in both axes (4 m at tiling 0.25).
    let size = 64u32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 * std::f32::consts::TAU;
            let v = (y as f32 + 0.5) / size as f32 * std::f32::consts::TAU;
            let value = 128.0 + 60.0 * u.sin() + 50.0 * v.cos();
            let b = value.clamp(0.0, 255.0) as u8;
            data.extend_from_slice(&[b, b, b, 255]);
        }
    }
    let smooth = LayerTextures {
        albedo: regia_terrain::Rgba8Image {
            width: size,
            height: size,
            data,
        },
        normal: regia_terrain::Rgba8Image::solid(size, size, [128, 128, 255, 255]),
        height: regia_terrain::Rgba8Image::solid(size, size, [128, 128, 128, 255]),
        orm: regia_terrain::Rgba8Image::solid(size, size, [255, 255, 0, 255]),
        use_orm: false,
    };
    let material = material_of(vec![smooth], 4.0);
    // Centred on x = 4 m (a cell edge of the old grid); edges at 0 and 8 m are in view too.
    let mut camera = top_down_camera(6.0, 5.0);
    camera.position[0] += 4.0;
    camera.target[0] += 4.0;
    let pixels = render_black(&map, &layers, &material, None, &camera, 3, None);
    let ratio = worst_step_ratio(&pixels);
    eprintln!("worst step ratio {ratio:.2}");
    assert!(
        ratio < 3.0,
        "a hard seam: one pixel step is {ratio:.2}x the steps beside it"
    );
}

/// Micro-normal tilt follows the normal map's slope. Round 3 rebuilt the micro normal as
/// `t·x + n·|x|` — a 45° tilt for any non-zero slope — so a barely-bumpy texel shaded like a
/// cliff and low suns went black. A 0.05 slope must move N·L only slightly; a −0.8 slope
/// clearly.
#[test]
fn micro_normal_tilt_follows_the_texture_slope() {
    let map = flat_at(5.0);
    let mut layer = full_cover_layer("ground", false, 0.0, 0.25);
    layer.detail_normal_strength = 1.0;
    let layers = TerrainLayers {
        layers: vec![layer],
        ..Default::default()
    };
    let with_normal = |n: [u8; 4]| {
        let mut set = solid_set([200, 200, 200, 255], 128, [255, 255, 0, 255], false);
        set.normal = regia_terrain::Rgba8Image::solid(32, 32, n);
        material_of(vec![set], 0.0)
    };
    let camera = top_down_camera(6.0, 5.0);
    let ndl = |m: &TerrainMaterial| centre_mean(&render_black(&map, &layers, m, None, &camera, 2, None))[0];
    let flat = ndl(&with_normal([128, 128, 255, 255]));
    let weak = ndl(&with_normal([134, 128, 255, 255]));
    // Tilted away from the sun (it sits toward +X), so N·L drops instead of saturating.
    let strong = ndl(&with_normal([26, 128, 180, 255]));
    eprintln!("N·L flat {flat:.1} weak {weak:.1} strong {strong:.1}");
    assert!(
        (weak - flat).abs() < 8.0,
        "a 0.05 micro slope moved N·L by {:.1} levels",
        (weak - flat).abs()
    );
    assert!(
        (strong - flat).abs() > 20.0,
        "a −0.8 micro slope barely moved N·L ({:.1} levels)",
        (strong - flat).abs()
    );
}

/// The incident-meter exposure keeps noon at the reference and opens up a low sun, and the
/// renderer applies it: the same low-sun frame is brighter with the metered compensation.
#[test]
fn metered_exposure_opens_up_a_low_sun() {
    assert!(TimeOfDay::default().metered_exposure_compensation_ev().abs() < 1e-4);
    let low = TimeOfDay {
        azimuth_radians: -0.9,
        elevation_radians: 0.3,
        sun_color: [1.4, 1.05, 0.72],
        illuminance_lux: 50_000.0,
        ambient: [0.32, 0.38, 0.55],
    };
    let ev = low.metered_exposure_compensation_ev();
    assert!((1.0..2.0).contains(&ev), "low-sun compensation {ev:.2} EV");

    let map = flat_at(5.0);
    let layers = TerrainLayers {
        layers: vec![full_cover_layer("ground", false, 0.0, 0.25)],
        ..Default::default()
    };
    let material = material_of(
        vec![solid_set([90, 110, 70, 255], 128, [255, 255, 0, 255], false)],
        0.0,
    );
    let camera = top_down_camera(6.0, 5.0);
    // The center patch's mean color, in 8-bit display codes, at exposure compensation `ev`, on
    // a fresh renderer.
    let render = |ev: f32| {
        let config = CdlodConfig::with_grid_size(16);
        let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
        gpu.set_material(&material, &[]).expect("material");
        let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid");
        let selection = select_nodes(
            &map,
            &pyramid,
            &config,
            4,
            [camera.position[0], camera.position[2]],
            None,
        );
        let mut frame = TerrainFrame::from_camera(&camera, &low, 0.0, 1.0);
        frame.exposure_compensation_ev = ev;
        let px = render_offscreen(&mut gpu, device(), &frame, &selection, W, H, [0.0, 0.0, 0.0, 1.0])
            .expect("render");
        centre_mean(&px)
    };
    // The frame's display luma below one code (ADR-7531). The target stores 8-bit sRGB codes,
    // and Vulkan lets an implementation turn a float into either neighboring code (it "should"
    // round to nearest). The patch is uniform, so its mean is one code per channel: lavapipe
    // rounds and read a ratio of 1.509, and the Intel Arc's 1.499 is exactly the codes below,
    // for a frame whose unquantized ratio is 1.504. The codes are averaged over exposures
    // spread evenly across a window centered on `ev`: the window spans 7 to 11 codes per channel,
    // so the rounding rule averages out and the mean is the unquantized display value
    // (to about 0.05 code), whichever rule the device uses.
    const SAMPLES: usize = 32;
    const WINDOW_EV: f32 = 0.25;
    let measure = |ev: f32| {
        let mut sum = [0.0f64; 3];
        for k in 0..SAMPLES {
            let offset = ((k as f32 + 0.5) / SAMPLES as f32 - 0.5) * WINDOW_EV;
            for (total, channel) in sum.iter_mut().zip(render(ev + offset)) {
                *total += channel;
            }
        }
        let [r, g, b] = sum.map(|total| total / SAMPLES as f64);
        eprintln!(
            "ev {ev:.4}: codes {:?}, mean over the window r {r:.3} g {g:.3} b {b:.3}",
            render(ev).map(|c| c as u8)
        );
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let plain = measure(0.0);
    let metered = measure(ev);
    eprintln!(
        "adapter {:?}: compensation {ev:.4} EV, luma {metered:.2} vs {plain:.2} (ratio {:.4})",
        device().adapter.get_info().name,
        metered / plain
    );
    assert!(
        metered > plain * 1.5,
        "metered exposure should brighten the low-sun frame: {metered:.2} vs {plain:.2}"
    );
}
