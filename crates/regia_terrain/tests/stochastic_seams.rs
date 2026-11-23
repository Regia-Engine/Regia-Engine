// SPDX-License-Identifier: MPL-2.0
//! The stochastic anti-tiling blend cuts no seam on its lattice edges (ADR-5792).
//!
//! `terrain.wgsl` `stochastic_taps` blends three texture taps per fragment, one per vertex of a
//! triangle lattice, and the blend is continuous only when a vertex hashes to the same rotation and
//! offset from each of the six triangles around it. The vertex ids were floats built as
//! `base + (1, 0)` and so on, and on the RTX 5080 and 5090 a vertex did not reach its hash as the
//! same float from every triangle (a sum taken in another order, such as the offset folded into the
//! hash's own additive constant, rounds one step apart). The hash and the rotation about the world
//! origin turn that last bit into another texture position, and the blend steps along the shared
//! edge. The forest demo's meadow showed those steps as hard straight cuts every 2 m on both cards
//! at every tier, and not on lavapipe.
//!
//! A flat photoreal terrain with one smooth, high-contrast, tileable layer, seen straight down from
//! 24 m at 2 cm a pixel, 170 m from the world origin (the demo's meadow). A smooth layer blended
//! with continuous weights is smooth everywhere, so a pixel whose difference to its neighbour
//! stands far above the differences on either side of it is on a cut:
//! - `a_shared_lattice_vertex_cuts_no_seam` counts those pixels. RTX 5080: 8585 on the old shader
//!   (up to 136 levels), none on the new one (worst 4.1);
//! - `lattice_vertex_ids_are_integers_until_hashed` holds the rule in the source on every device;
//! - `the_step_count_tells_a_cut_from_a_smooth_field` checks the measure itself (no GPU).
//!
//! Lavapipe never drew the cuts (old and new shader give it the same frame), but it draws thin
//! lines of its own, one or two pixels wide, exactly on the lattice edges (cause not found; no RTX
//! frame has them). They are not what this test is about, so the frame goes through a 7x7 median
//! first: a median keeps a step between two regions where it is and removes a line narrower than
//! half its window. After it lavapipe's frame has no step either, with old or new shader, so on a
//! software device the image test guards the scene and the measure, and a card run is the
//! regression check for the cuts.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 960;
const H: u32 = 540;
const WORLD_M: f32 = 1024.0;
const EYE_Y: f32 = 24.0;
/// Vertical field of view: 540 rows over 10.8 m of ground, 2 cm a pixel.
const FOV_Y: f32 = 0.4422;
/// Texture repeats per metre (the example scene's layers): a lattice cell of `0.85 / 0.4` m.
const TILING: f32 = 0.4;
/// Texels of the synthetic layer.
const SIZE: u32 = 64;
/// Luma levels a pixel's difference to its neighbour must stand above the differences beside it
/// to count as a step, after the median. The smooth layer's own curvature stays under 5 levels
/// (RTX 5080: 4.1; lavapipe with its lines removed: 10.0); a cut of the old shader stood up to 135.
const STEP_LEVELS: f32 = 16.0;
/// Side of the median window, pixels: wider than twice lavapipe's two-pixel lines.
const MEDIAN: usize = 7;

fn map() -> TerrainMap {
    let n = 257u32;
    TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], WORLD_M, 1.0, 0.0).expect("map")
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "smooth".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 0.9,
            tiling: TILING,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.3,
            triplanar: false,
            height_blend: 0.0,
            detail_normal_strength: 0.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    }
}

/// A tileable layer whose albedo is one smooth period of light and dark across the tile (no texel
/// edge anywhere), with a flat normal map.
fn layer() -> LayerTextures {
    let mut albedo = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let tau = std::f32::consts::TAU;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let u = (x as f32 + 0.5) / SIZE as f32;
            let v = (y as f32 + 0.5) / SIZE as f32;
            let level = (140.0 + 100.0 * (tau * u).sin() * (tau * v).sin()).round() as u8;
            albedo.extend_from_slice(&[level, level, level, 255]);
        }
    }
    LayerTextures {
        albedo: Rgba8Image {
            width: SIZE,
            height: SIZE,
            data: albedo,
        },
        normal: Rgba8Image::solid(SIZE, SIZE, [128, 128, 255, 255]),
        height: Rgba8Image::solid(SIZE, SIZE, [128, 128, 128, 255]),
        orm: Rgba8Image::solid(SIZE, SIZE, [255, 255, 0, 255]),
        use_orm: false,
    }
}

/// One frame's luma, row by row.
fn frame(device: &RenderDevice) -> Vec<f32> {
    let mut draw =
        TerrainSharedDraw::new(device, map(), &layers(), CdlodConfig::default(), 5).expect("terrain");
    let material = TerrainMaterial {
        layers: vec![Some(layer())],
        macro_set: None,
        params: MaterialParams::default(),
    };
    draw.set_material(&material, &[]).expect("material");
    // The engine's own relief (pits, grain, glints) is detail of its own; the layer's blend is
    // what is measured.
    draw.set_relief(TerrainRelief::off());
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        fxaa: false,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        // The demo's meadow: far enough from the origin for a last-bit difference in a vertex's
        // rotation to move its tap by whole texels.
        eye: [-120.3, EYE_Y, -125.0],
        target: [-120.3, 0.0, -126.0],
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
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.5], [1.0, 0.98, 0.95], 60_000.0),
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
    if let Some(dir) = regia_core::brand::env_var_os("REGIA_DUMP_DIR") {
        let path = std::path::Path::new(&dir).join("stochastic_seams.png");
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&image.pixels);
            }
        }
    }
    image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

/// What the frame's luma says about cuts.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Steps {
    /// Pixels whose difference to the next pixel (along a row, or down a column) stands more than
    /// [`STEP_LEVELS`] above the mean of the two differences beside it.
    count: usize,
    /// The largest such excess, luma levels.
    worst: f32,
    /// Largest difference between neighbours anywhere: the field's own slope (and its contrast).
    slope: f32,
}

/// The [`MEDIAN`]-wide median of `luma` (the frame's edge repeated outward).
fn median(luma: &[f32], w: usize, h: usize) -> Vec<f32> {
    let r = MEDIAN / 2;
    let mut out = Vec::with_capacity(w * h);
    let mut window = [0.0f32; MEDIAN * MEDIAN];
    for y in 0..h {
        for x in 0..w {
            for dy in 0..MEDIAN {
                let yy = (y + dy).saturating_sub(r).min(h - 1);
                for dx in 0..MEDIAN {
                    let xx = (x + dx).saturating_sub(r).min(w - 1);
                    window[dy * MEDIAN + dx] = luma[yy * w + xx];
                }
            }
            let mid = window.len() / 2;
            let (_, middle, _) = window.select_nth_unstable_by(mid, f32::total_cmp);
            out.push(*middle);
        }
    }
    out
}

/// The steps of `luma` after the median.
fn steps(luma: &[f32], w: usize, h: usize) -> Steps {
    steps_of(&median(luma, w, h), w, h)
}

fn steps_of(luma: &[f32], w: usize, h: usize) -> Steps {
    let mut out = Steps {
        count: 0,
        worst: 0.0,
        slope: 0.0,
    };
    let mut probe = |a: f32, b: f32, c: f32, d: f32| {
        // a b | c d: the step between b and c against the slopes on either side.
        let across = c - b;
        let beside = 0.5 * ((b - a) + (d - c));
        let excess = (across - beside).abs();
        out.slope = out.slope.max(across.abs());
        if excess > STEP_LEVELS {
            out.count += 1;
        }
        out.worst = out.worst.max(excess);
    };
    for y in 0..h {
        let row = &luma[y * w..(y + 1) * w];
        for x in 1..w - 2 {
            probe(row[x - 1], row[x], row[x + 1], row[x + 2]);
        }
    }
    for x in 0..w {
        for y in 1..h - 2 {
            probe(
                luma[(y - 1) * w + x],
                luma[y * w + x],
                luma[(y + 1) * w + x],
                luma[(y + 2) * w + x],
            );
        }
    }
    out
}

/// The measure itself: a smooth field has no step, the same field cut along a slanted line (one
/// side shifted, as a tap of another texture position is) has one for every pixel of the line, and
/// lines one or two pixels wide over the smooth field have none after the median.
#[test]
fn the_step_count_tells_a_cut_from_a_smooth_field() {
    let (w, h) = (240usize, 160usize);
    let tau = std::f32::consts::TAU;
    let smooth = |x: f32, y: f32| 140.0 + 100.0 * (tau * x / 125.0).sin() * (tau * y / 125.0).sin();
    let mut plain = Vec::with_capacity(w * h);
    let mut cut = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32, y as f32);
            plain.push(smooth(fx, fy));
            // Right of the line x = 80 + y / 2 the field is read 40 pixels further on.
            let shifted = fx > 80.0 + 0.5 * fy;
            cut.push(if shifted {
                smooth(fx + 40.0, fy + 25.0)
            } else {
                smooth(fx, fy)
            });
        }
    }
    // The smooth field with a bright line two pixels tall across it and a dashed slanted line one
    // pixel wide: what lavapipe draws on the lattice edges.
    let mut lined = plain.clone();
    for x in 0..w {
        lined[60 * w + x] = 230.0;
        lined[61 * w + x] = 230.0;
    }
    for y in (0..h).filter(|y| y % 12 < 5) {
        lined[y * w + 40 + y / 2] = 20.0;
    }
    let raw = steps_of(&lined, w, h);
    let plain = steps(&plain, w, h);
    let cut = steps(&cut, w, h);
    let lined = steps(&lined, w, h);
    assert_eq!(plain.count, 0, "a smooth field has no step: {plain:?}");
    assert!(plain.slope > 3.0, "the field has contrast: {plain:?}");
    assert!(
        cut.count > h / 2,
        "a cut down the frame is a step on most of its rows: {cut:?}"
    );
    assert!(raw.count > w, "the lines are steps before the median: {raw:?}");
    assert_eq!(lined.count, 0, "thin lines are not cuts: {lined:?}");
}

/// The rule in the source, on every device: the lattice's vertex ids are integers until the one
/// conversion that feeds the hash, so no triangle can reach a vertex with another float.
#[test]
fn lattice_vertex_ids_are_integers_until_hashed() {
    let source = include_str!("../src/gpu/terrain.wgsl");
    let start = source
        .find("fn stochastic_taps(")
        .expect("terrain.wgsl has stochastic_taps");
    let end = start + source[start..].find("\n}\n").expect("stochastic_taps ends");
    let body = &source[start..end];
    assert!(
        body.contains("let base = vec2<i32>("),
        "stochastic_taps takes its lattice base as integers (ADR-5792)"
    );
    for float_offset in [
        "+ vec2<f32>(1.0, 0.0)",
        "+ vec2<f32>(0.0, 1.0)",
        "+ vec2<f32>(1.0, 1.0)",
    ] {
        assert!(
            !body.contains(float_offset),
            "stochastic_taps offsets a lattice vertex in floats (`{float_offset}`): a driver may fold \
             the offset into the hash's constant, and the vertex then differs between its triangles \
             (ADR-5792)"
        );
    }
}

#[test]
fn a_shared_lattice_vertex_cuts_no_seam() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip stochastic seams: {err}");
            return;
        }
    };
    let luma = frame(&device);
    let found = steps(&luma, W as usize, H as usize);
    eprintln!(
        "stochastic seams {W}x{H}: {} step pixels, worst excess {:.1} levels, largest slope {:.1} levels",
        found.count, found.worst, found.slope
    );
    // The layer is in the frame with its contrast: a flat or black frame would pass any step count.
    assert!(
        found.slope > 2.0,
        "the layer's light and dark are not in the frame: {found:?}"
    );
    assert!(
        found.count == 0,
        "{} pixels sit on a cut of the anti-tiling blend (worst {:.1} levels over the slopes beside it)",
        found.count,
        found.worst
    );
}
