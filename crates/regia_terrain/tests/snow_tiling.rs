// SPDX-License-Identifier: MPL-2.0
//! A biased tangent-space normal map must not show the anti-tiling lattice (ADR-1428).
//!
//! The showcase's foreground snow (Poly Haven `snow_02`) is nearly uniform white, but its normal
//! map is not zero-mean: R and G average 147 where a flat map averages 128, a constant tilt of
//! about 12°. The stochastic anti-tiling gives every lattice cell its own texture rotation and
//! rotates the micro normal back with it, so the bias pointed a different way in every cell. Under a
//! low sun the cells lit and shaded differently: a hexagonal grid of dark and light blotches at 50-200 m.
//!
//! A flat photoreal terrain with one synthetic tileable layer (uniform albedo, a faint zero-mean
//! normal ripple plus a bias) seen from 100 m straight down (a pixel is 10 cm on the ground: the
//! footprint of the showcase's foreground snow 100 m out, with the micro-normal detail fade pushed
//! past it) under a low sun. Measured on the luma rows, each with its slow lighting trend removed:
//! - the lattice-scale autocorrelation of the strip (the largest normalised autocorrelation at lags
//!   of 0.8-2.5 lattice cells; independent random cells make it large, a uniform field with fine
//!   detail makes it near zero) of the biased layer stays at the level of the same layer with the
//!   bias removed by hand, and under an absolute bound;
//! - so does the lattice-scale contrast (the strip's standard deviation after a box average one cell
//!   wide, over the mean luma).
//!
//! Before the fix (the bias reaching the shader) the autocorrelation peak was far above the
//! zero-mean control; the numbers are in ADR-1428.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 270;
const H: u32 = 480;
const WORLD_M: f32 = 1024.0;
const EYE_Y: f32 = 100.0;
/// Vertical field of view: 480 rows over 48 m of ground, 10 cm a pixel.
const FOV_Y: f32 = 0.4709;
/// Metres of ground per pixel.
const M_PER_PX: f32 = 0.1;
/// Texture repeats per metre (a 2 m tile, like the showcase's Poly Haven layers).
const TILING: f32 = 0.5;
/// The engine's anti-tiling lattice cell for this tiling (`layer_cell`: `0.85 / tiling`, capped
/// by `MaterialParams::anti_tiling_cell`).
const CELL_M: f32 = 0.85 / TILING;
/// Texels of the synthetic layer.
const SIZE: u32 = 64;
/// Amplitude of the zero-mean normal ripple, byte levels (about 0.012 slope: a near-uniform layer).
const RIPPLE: f32 = 1.5;

fn map() -> TerrainMap {
    let n = 257u32;
    TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], WORLD_M, 1.0, 0.0).expect("map")
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "snow".into(),
            albedo: [0.8, 0.82, 0.85],
            roughness: 0.55,
            tiling: TILING,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.3,
            triplanar: false,
            height_blend: 0.0,
            detail_normal_strength: 1.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    }
}

/// A tileable synthetic layer: uniform albedo, a zero-mean normal ripple of two periods across the
/// tile (`RIPPLE` levels), and `bias` levels added to the normal's X and Y.
fn layer(bias: i32) -> LayerTextures {
    let mut normal = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let tau = std::f32::consts::TAU;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let u = x as f32 / SIZE as f32;
            let v = y as f32 / SIZE as f32;
            let rx = RIPPLE * (tau * (2.0 * u + 3.0 * v)).sin();
            let ry = RIPPLE * (tau * (3.0 * u - 2.0 * v)).cos();
            let r = (128.0 + rx).round() as i32 + bias;
            let g = (128.0 + ry).round() as i32 + bias;
            normal.extend_from_slice(&[r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, 250, 255]);
        }
    }
    LayerTextures {
        albedo: Rgba8Image::solid(SIZE, SIZE, [220, 222, 226, 255]),
        normal: Rgba8Image {
            width: SIZE,
            height: SIZE,
            data: normal,
        },
        height: Rgba8Image::solid(SIZE, SIZE, [128, 128, 128, 255]),
        orm: Rgba8Image::solid(SIZE, SIZE, [255, 255, 0, 255]),
        use_orm: false,
    }
}

fn material(bias: i32) -> TerrainMaterial {
    TerrainMaterial {
        layers: vec![Some(layer(bias))],
        macro_set: None,
        params: MaterialParams {
            // The micro normals stay at full strength over the whole frame.
            detail_fade_start: 400.0,
            detail_fade_end: 800.0,
            ..MaterialParams::default()
        },
    }
}

fn eye() -> [f32; 3] {
    [0.3, EYE_Y, 0.0]
}

fn target() -> [f32; 3] {
    [0.3, 0.0, -1.0]
}

/// One frame's luma.
fn frame(device: &RenderDevice, bias: i32) -> Vec<f32> {
    let mut draw =
        TerrainSharedDraw::new(device, map(), &layers(), CdlodConfig::default(), 5).expect("terrain");
    draw.set_material(&material(bias), &[]).expect("material");
    // The engine's own relief (mid-range pits, near detail, glints) would add cell-scale noise
    // of its own; the layer's texture is what is measured.
    draw.set_relief(TerrainRelief::off());
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: eye(),
        target: target(),
        fov_y: FOV_Y,
        near: 0.1,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        // A low sun (about 14° up) from the front-left.
        directional: DirectionalLightUniform::new([0.45, -0.25, -0.85], [1.0, 0.95, 0.9], 60_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    let mut scene = lab.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    let image = renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("frame: {err}"));
    image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

/// The strip's two lattice-scale measures over the frame's rows.
struct StripStats {
    /// Largest mean normalised autocorrelation at lags of 0.8-2.5 lattice cells (a repeating
    /// grid peaks at its period; the 2 m tile repeat is in this range).
    autocorr_peak: f32,
    /// Coefficient of variation of the strip after a box average one cell wide.
    cell_contrast: f32,
    /// Rows used.
    rows: usize,
}

fn strip_stats(luma: &[f32]) -> StripStats {
    let w = W as usize;
    let cell_px = CELL_M / M_PER_PX;
    let lag_lo = (0.8 * cell_px).round() as usize;
    let lag_hi = (2.5 * cell_px).round() as usize;
    let box_px = (cell_px.round() as usize).max(3);
    // The slow lighting gradient across a row (sky and sun falloff) is taken out with a box
    // six cells wide; the strip is the interior where that box fits.
    let trend_px = 6 * box_px;
    let half = trend_px / 2;
    assert!(
        w > trend_px + lag_hi + 8,
        "the row must be wider than the detrend box"
    );
    let mut acf = vec![0.0f32; lag_hi + 1];
    let mut cv_sum = 0.0;
    for y in 0..H as usize {
        let row = &luma[y * w..(y + 1) * w];
        let mean = row.iter().sum::<f32>() / w as f32;
        let detrended: Vec<f32> = (half..w - half)
            .map(|x| row[x] - row[x - half..x + half].iter().sum::<f32>() / trend_px as f32)
            .collect();
        let n = detrended.len();
        let var = detrended.iter().map(|v| v * v).sum::<f32>() / n as f32;
        for (lag, slot) in acf.iter_mut().enumerate() {
            let c = (0..n - lag)
                .map(|x| detrended[x] * detrended[x + lag])
                .sum::<f32>()
                / (n - lag) as f32;
            *slot += c / var.max(1e-9);
        }
        let smooth: Vec<f32> = (0..n - box_px)
            .map(|x| detrended[x..x + box_px].iter().sum::<f32>() / box_px as f32)
            .collect();
        let sm_var = smooth.iter().map(|v| v * v).sum::<f32>() / smooth.len() as f32;
        cv_sum += sm_var.sqrt() / mean.max(1e-3);
    }
    let n = H as f32;
    let autocorr_peak = acf[lag_lo..=lag_hi]
        .iter()
        .map(|v| v / n)
        .fold(f32::NEG_INFINITY, f32::max);
    StripStats {
        autocorr_peak,
        cell_contrast: cv_sum / n,
        rows: H as usize,
    }
}

/// The strip measures themselves: random blotches one lattice cell across show contrast, cells
/// repeating every two cells show an autocorrelation peak at the period, and a uniform field with
/// fine pixel noise shows neither (no GPU).
#[test]
fn the_strip_measures_tell_blotches_from_a_uniform_field() {
    let cell_px = (CELL_M / M_PER_PX).round() as u32;
    let hash = |x: u32, y: u32| {
        let mut h = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca6b);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2c1b_3c6d);
        h ^= h >> 12;
        (h & 0xff) as f32 / 255.0
    };
    let mut blotchy = Vec::new();
    let mut periodic = Vec::new();
    let mut uniform = Vec::new();
    for y in 0..H {
        for x in 0..W {
            // Square cells with an independent level each, plus the same fine noise on both.
            let noise = hash(x + 1000, y + 7) * 2.0 - 1.0;
            blotchy.push(150.0 + 40.0 * hash(x / cell_px, y / cell_px) + noise);
            // The same cells repeating every two cells: a regular grid.
            periodic.push(150.0 + 40.0 * hash((x / cell_px) % 2, (y / cell_px) % 2) + noise);
            uniform.push(170.0 + noise);
        }
    }
    let b = strip_stats(&blotchy);
    let p = strip_stats(&periodic);
    let u = strip_stats(&uniform);
    assert!(b.cell_contrast > 0.03, "blotchy contrast {:.4}", b.cell_contrast);
    assert!(u.cell_contrast < 0.01, "uniform contrast {:.4}", u.cell_contrast);
    assert!(p.autocorr_peak > 0.3, "periodic grid peak {:.3}", p.autocorr_peak);
    assert!(
        u.autocorr_peak < 0.15,
        "uniform field peak {:.3}",
        u.autocorr_peak
    );
}

#[test]
fn a_biased_normal_map_shows_no_lattice_grid() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    // 20 levels = 0.16 slope, the showcase snow's bias.
    let biased = strip_stats(&frame(&device, 20));
    let control = strip_stats(&frame(&device, 0));
    eprintln!(
        "{} rows; autocorrelation peak: biased {:.3}, zero-mean {:.3}; cell contrast: biased {:.4}, zero-mean {:.4}",
        biased.rows, biased.autocorr_peak, control.autocorr_peak, biased.cell_contrast, control.cell_contrast
    );
    assert!(
        biased.autocorr_peak <= 0.15,
        "biased layer's lattice-period autocorrelation peak {:.3}",
        biased.autocorr_peak
    );
    assert!(
        biased.autocorr_peak <= control.autocorr_peak + 0.1,
        "biased layer's lattice autocorrelation {:.3} vs zero-mean {:.3}",
        biased.autocorr_peak,
        control.autocorr_peak
    );
    assert!(
        biased.cell_contrast <= control.cell_contrast * 1.5 + 0.01,
        "biased layer's cell contrast {:.4} vs zero-mean {:.4}",
        biased.cell_contrast,
        control.cell_contrast
    );
    assert!(
        biased.cell_contrast <= 0.02,
        "biased layer's cell contrast {:.4} over the absolute bound",
        biased.cell_contrast
    );
}
