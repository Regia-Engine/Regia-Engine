// SPDX-License-Identifier: MPL-2.0
//! Far painted-patch outlines (ADR-1456, look round 2 finding 8): a splat-pack edge seen from far away
//! must not stair-step.
//!
//! A flat photoreal terrain, 512 m wide, two layers (dark ground, bright snow) with the showcase's height
//! blend (0.6), and splat packs of 2 m texels that paint the snow texel by texel past the line
//! z = 0.5 · x + 150.3 (a binary edge, like a painted pack). The eye is 80 m up and 300 m away, looking
//! down 15°: a pack texel spans about four pixels across, where the bilinear field's bends at every
//! texel edge, drawn sharp by the height blend, read as steps. A static camera with TAA resolves the
//! outline below the pixel; each column's outline is projected back onto the ground and its distance
//! from the best straight line is measured in metres.
//!
//! With the far-field B-spline the outline's deviation must stay under the bound (the CPU mirror,
//! `regia_terrain::far_splat`, halves the texel-scale steps of such an edge); without it (two bilinear
//! taps everywhere) it measured over the bound (numbers in ADR-1456).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw, TerrainBudget,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainSharedDraw,
};

const W: u32 = 320;
const H: u32 = 180;
const WORLD_M: f32 = 512.0;
/// Splat pack side: 2 m texels.
const PACK: u32 = 256;
const FOV_Y: f32 = 0.32;
const EYE: [f32; 3] = [0.0, 80.0, -150.0];
const TARGET: [f32; 3] = [0.0, 0.0, 150.0];
/// The painted edge: snow where z > SLOPE · x + OFFSET (world metres, texel centres).
const SLOPE: f32 = 0.5;
const OFFSET: f32 = 150.3;
/// TAA frames of the static camera before the measured one.
const SETTLE: usize = 32;

fn map() -> TerrainMap {
    let n = 257u32;
    TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], WORLD_M, 1.0, 0.0).expect("map")
}

fn layer(name: &str) -> LayerDesc {
    LayerDesc {
        name: name.into(),
        albedo: [1.0, 1.0, 1.0],
        roughness: 0.9,
        tiling: 0.5,
        slope_min: 0.0,
        slope_max: 1.6,
        altitude_min: -100.0,
        altitude_max: 100.0,
        blend_sharpness: 0.12,
        triplanar: false,
        height_blend: 0.6,
        detail_normal_strength: 0.0,
        ..LayerDesc::ground()
    }
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![layer("ground"), layer("snow")],
        ..Default::default()
    }
}

fn solid(grey: u8) -> LayerTextures {
    let size = 16u32;
    LayerTextures {
        albedo: Rgba8Image::solid(size, size, [grey, grey, grey, 255]),
        normal: Rgba8Image::solid(size, size, [128, 128, 255, 255]),
        height: Rgba8Image::solid(size, size, [128, 128, 128, 255]),
        orm: Rgba8Image::solid(size, size, [255, 255, 0, 255]),
        use_orm: false,
    }
}

fn material() -> TerrainMaterial {
    TerrainMaterial {
        layers: vec![Some(solid(40)), Some(solid(200))],
        macro_set: None,
        params: MaterialParams {
            anti_tiling_cell: 0.0,
            ..MaterialParams::default()
        },
    }
}

/// Pack 0: red = ground (layer 0), green = snow (layer 1), binary per texel; pack 1 empty.
fn packs() -> (Rgba8Image, Rgba8Image) {
    let texel = WORLD_M / PACK as f32;
    let mut data = Vec::with_capacity((PACK * PACK * 4) as usize);
    for j in 0..PACK {
        for i in 0..PACK {
            let x = -0.5 * WORLD_M + (i as f32 + 0.5) * texel;
            let z = -0.5 * WORLD_M + (j as f32 + 0.5) * texel;
            let snow = z > SLOPE * x + OFFSET;
            data.extend_from_slice(if snow { &[0, 255, 0, 0] } else { &[255, 0, 0, 0] });
        }
    }
    let pack0 = Rgba8Image {
        width: PACK,
        height: PACK,
        data,
    };
    (pack0, Rgba8Image::solid(PACK, PACK, [0, 0, 0, 0]))
}

/// Ground point (x, z) under the ray through pixel position (`px`, `py`) (pixel centres at + 0.5).
fn ground_at(px: f32, py: f32) -> Option<(f32, f32)> {
    let e = glam::Vec3::from_array(EYE);
    let f = (glam::Vec3::from_array(TARGET) - e).normalize();
    let r = f.cross(glam::Vec3::Y).normalize();
    let u = r.cross(f);
    let t = (FOV_Y * 0.5).tan();
    let aspect = W as f32 / H as f32;
    let sx = (px / W as f32 * 2.0 - 1.0) * t * aspect;
    let sy = (1.0 - py / H as f32 * 2.0) * t;
    let d = (f + r * sx + u * sy).normalize();
    (d.y < -1e-4).then(|| {
        let s = -e.y / d.y;
        (e.x + d.x * s, e.z + d.z * s)
    })
}

/// Luma (0..255) of a static frame after `SETTLE` TAA frames.
fn settled_luma(device: &RenderDevice) -> Vec<f32> {
    let mut draw =
        TerrainSharedDraw::new(device, map(), &layers(), CdlodConfig::default(), 5).expect("terrain");
    draw.set_material(&material(), &[]).expect("material");
    let (pack0, pack1) = packs();
    draw.set_splat_weights(Some([&pack0, &pack1])).expect("packs");
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.3, -0.8, -0.5], [1.0, 0.95, 0.9], 60_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = true;
    quality.render_scale_permille = 1000;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.terrain_budget = TerrainBudget::with_anisotropy(16);
    let mut last = Vec::new();
    for _ in 0..=SETTLE {
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        let image = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        last = image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
            .collect();
    }
    last
}

/// Median of `v` (empty: NaN).
fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// The outline on the ground, one point per column: where the luma crosses halfway between the
/// ground's and the snow's, walking up the column from the near ground.
fn outline(luma: &[f32]) -> Vec<(f32, f32)> {
    let side = |snow: bool| {
        let mut v = Vec::new();
        for y in 0..H {
            for x in 0..W {
                if let Some((gx, gz)) = ground_at(x as f32 + 0.5, y as f32 + 0.5) {
                    let past = gz - (SLOPE * gx + OFFSET);
                    if (snow && past > 15.0 && past < 40.0) || (!snow && past < -15.0 && past > -40.0) {
                        v.push(luma[(y * W + x) as usize]);
                    }
                }
            }
        }
        median(v)
    };
    let (ground, snow) = (side(false), side(true));
    assert!(
        snow > ground + 40.0,
        "the snow ({snow:.1}) must read clearly brighter than the ground ({ground:.1})"
    );
    let mid = 0.5 * (ground + snow);
    let mut points = Vec::new();
    for x in 16..W - 16 {
        // Rows whose ground lies within 12 m of the line, near to far (bottom to top).
        let mut prev: Option<(u32, f32)> = None;
        for y in (0..H).rev() {
            let Some((gx, gz)) = ground_at(x as f32 + 0.5, y as f32 + 0.5) else {
                break;
            };
            if (gz - (SLOPE * gx + OFFSET)).abs() > 12.0 {
                prev = None;
                continue;
            }
            let l = luma[(y * W + x) as usize];
            if let Some((py, pl)) = prev
                && pl < mid
                && l >= mid
            {
                // Between the centres of rows `py` (below) and `y`.
                let frac = (mid - pl) / (l - pl).max(1e-3);
                let row = py as f32 + 0.5 - frac;
                if let Some(p) = ground_at(x as f32 + 0.5, row) {
                    points.push(p);
                }
                break;
            }
            prev = Some((y, l));
        }
    }
    points
}

/// Distances (metres) of the outline points from their least-squares line z = a · x + b, measured
/// across the line: (RMS, peak to peak).
fn deviation(points: &[(f32, f32)]) -> (f32, f32) {
    let n = points.len() as f32;
    let (sx, sz) = points.iter().fold((0.0, 0.0), |(a, b), &(x, z)| (a + x, b + z));
    let (mx, mz) = (sx / n, sz / n);
    let (sxx, sxz) = points.iter().fold((0.0, 0.0), |(a, b), &(x, z)| {
        (a + (x - mx) * (x - mx), b + (x - mx) * (z - mz))
    });
    let a = sxz / sxx;
    let norm = (1.0 + a * a).sqrt();
    let residual: Vec<f32> = points
        .iter()
        .map(|&(x, z)| (z - (mz + a * (x - mx))) / norm)
        .collect();
    let rms = (residual.iter().map(|r| r * r).sum::<f32>() / n).sqrt();
    let lo = residual.iter().copied().fold(f32::MAX, f32::min);
    let hi = residual.iter().copied().fold(f32::MIN, f32::max);
    (rms, hi - lo)
}

#[test]
fn a_far_painted_edge_keeps_a_smooth_outline() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let points = outline(&settled_luma(&device));
    assert!(
        points.len() as u32 > (W - 32) * 9 / 10,
        "the outline must cross most columns ({} of {})",
        points.len(),
        W - 32
    );
    // The footprint here puts the far-field B-spline fully on: a texel spans about four pixels.
    let (gx0, gz0) = ground_at(0.5 * W as f32, 0.5 * H as f32).expect("ground at the centre");
    let (gx1, gz1) = ground_at(0.5 * W as f32 + 1.0, 0.5 * H as f32).expect("ground at the centre");
    let across = ((gx1 - gx0).powi(2) + (gz1 - gz0).powi(2)).sqrt();
    let k = regia_terrain::far_splat::far_splat_fade(across, WORLD_M / PACK as f32);
    let (rms, peak) = deviation(&points);
    eprintln!(
        "far painted edge: {} points, pixel {across:.2} m across (B-spline share {k:.2}); outline off its line by \
         {rms:.3} m RMS, {peak:.3} m peak to peak",
        points.len()
    );
    assert!(
        k > 0.9,
        "the test view must use the far-field reconstruction (share {k:.2})"
    );
    assert!(
        rms < FAR_EDGE_RMS_M,
        "the far outline steps {rms:.3} m RMS off its line (bound {FAR_EDGE_RMS_M} m)"
    );
}

/// Bound on the outline's RMS distance from its line, metres (ADR-1456: see the numbers there).
const FAR_EDGE_RMS_M: f32 = 0.25;
