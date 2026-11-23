// SPDX-License-Identifier: MPL-2.0
//! Cliff relief: real geometry on steep faces (terrain-look round 8, ADR-075).
//!
//! DO27's switchbacks walls read smoother than UE's sculpted cliffs because the relief on
//! them was shading only: the silhouette of a cliff against the sky was the smooth
//! heightfield. `TerrainRelief::cliff_relief` moves CDLOD vertices on steep faces along the
//! face normal (buttresses, fractured blocks, ledges, flutes) and draws near steep nodes with a
//! grid twice as fine. These tests render a 60 m butte with near-vertical walls against an
//! airless sky, lit from the camera, and measure:
//!
//! * the roughness of the butte's left and right silhouettes against the sky (relief on vs
//!   off). Sky is the depth buffer's far-plane clear, not a black pixel: the planet surface
//!   below the horizon is lit, so a colour test reads that ground as the cliff;
//! * cracks: background pixels inside the terrain (a gap between nodes, LOD levels or the fine
//!   and regular grids shows the far clear through the ground or the butte);
//! * that the fine grid is in use.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 480;
const H: u32 = 270;
const EYE: [f32; 3] = [0.0, 20.0, -95.0];
const TARGET: [f32; 3] = [0.0, 38.0, 0.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Wall width of the steep butte: 60 m over 6 m, a smoothstep profile up to ~86°.
const STEEP_WALL_M: f32 = 6.0;
/// Wall width of the drivable-limit butte: 60 m over 30 m, at most rise/run 3 (~72°), under
/// DO27's hover drive limit (rise/run 4).
const HOVER_WALL_M: f32 = 30.0;

/// A 60 m butte (walls between r = 24 m and 24 m + `wall_m`) on flat ground, 300 m map at
/// 0.29 m.
fn butte(wall_m: f32) -> TerrainMap {
    let n = 1025u32;
    let width = 300.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let r = (x * x + z * z).sqrt();
            let t = ((r - 24.0) / wall_m).clamp(0.0, 1.0);
            heights.push(60.0 * (1.0 - t * t * (3.0 - 2.0 * t)));
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 64.0, -2.0).expect("butte")
}

struct Shot {
    pixels: Vec<u8>,
    /// Scene depth, row-major. Sky (no geometry) is [`SKY_DEPTH`].
    depth: Vec<f32>,
    dense_nodes: u32,
}

/// Far-plane clear. `outdoor_sky.wgsl` `fs_sky_hdr` draws the sky only where the
/// depth load is not strictly below 1. Geometry is strictly below that.
const SKY_DEPTH: f32 = 1.0;

fn render(cliff_relief: f32, wall_m: f32) -> Shot {
    render_view(cliff_relief, wall_m, true)
}

/// `planet` false sets the bottom radius to 0, so `ray_hits_planet` skips the
/// surface (`ground.w` ≤ 0). The mesh and the depth pass are the same.
fn render_view(cliff_relief: f32, wall_m: f32, planet: bool) -> Shot {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "rock".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    // Warm grey rock (not icy: ledges rather than flutes).
    let albedo = [
        regia_terrain::linear_to_srgb8(0.5),
        regia_terrain::linear_to_srgb8(0.42),
        regia_terrain::linear_to_srgb8(0.36),
    ];
    let mut draw =
        TerrainSharedDraw::new(device(), butte(wall_m), &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [albedo[0], albedo[1], albedo[2], 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief {
        cliff_relief,
        ..TerrainRelief::default()
    });
    let stats = draw.stats();
    let post = PostSettings {
        exposure_ev: 10.5,
        ..PostSettings::golden_deterministic()
    };
    // The sun shines from behind and above the camera: every face the camera sees is lit, the
    // sky is black.
    let fwd = [0.0, -0.35, 1.0];
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 10.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new(fwd, [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut atmosphere = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
    if !planet {
        atmosphere.bottom_radius_km = 0.0;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: atmosphere,
        // Airless skies leave the surface off by default (ADR-162); this test is about
        // drawing it, so force it on.
        planet_surface: Some(true),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    let depth = renderer.read_scene_depth(device(), W, H).expect("scene depth");
    assert_eq!(depth.len(), (W * H) as usize, "scene depth is not the frame size");
    if let Some(dir) = regia_core::brand::env_var_os("CLIFF_RELIEF_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("butte_wall{wall_m}_cliff{cliff_relief}.png"));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        }
    }
    Shot {
        pixels,
        depth,
        dense_nodes: stats.dense_nodes(),
    }
}

fn shot(cliff_relief: f32) -> &'static Shot {
    static ON: OnceLock<Shot> = OnceLock::new();
    static OFF: OnceLock<Shot> = OnceLock::new();
    if cliff_relief > 0.0 {
        ON.get_or_init(|| render(cliff_relief, STEEP_WALL_M))
    } else {
        OFF.get_or_init(|| render(0.0, STEEP_WALL_M))
    }
}

/// Geometry at a pixel: the depth pass wrote a value in front of the far clear.
/// Stars and the lit planet surface do not write depth, so they stay sky.
fn terrain_at(depth: &[f32], x: u32, y: u32) -> bool {
    depth[(y * W + x) as usize] < SKY_DEPTH
}

/// Far-plane sky at a pixel (the sky mask `fs_sky_hdr` uses).
fn sky_at(depth: &[f32], x: u32, y: u32) -> bool {
    depth[(y * W + x) as usize] >= SKY_DEPTH
}

/// Terrain at `x` and the next two pixels toward `dir`.
fn solid_at(depth: &[f32], x: u32, y: u32, dir: i32) -> bool {
    (0..3).all(|k| {
        let xx = x as i32 + dir * k;
        (0..W as i32).contains(&xx) && terrain_at(depth, xx as u32, y)
    })
}

/// Rows where both sides of the butte stand against the sky: above the horizon (the far
/// ground is below it) and below the butte's top.
fn sky_rows(depth: &[f32]) -> Vec<u32> {
    (0..H)
        .filter(|&y| {
            // Background on both image edges (the sky), terrain in the middle (the butte).
            sky_at(depth, 0, y) && sky_at(depth, W - 1, y) && terrain_at(depth, W / 2, y)
        })
        .collect()
}

/// RMS (pixels) of a silhouette's departure from the least-squares quadratic through it (the
/// butte's smooth flank in perspective).
fn roughness(rows: &[u32], edge: &[f32]) -> f32 {
    let n = edge.len();
    if n < 3 {
        return 0.0;
    }
    // Normal equations for e ≈ a + b·y + c·y² (y centred and scaled for conditioning).
    let y0 = rows.iter().map(|&r| f64::from(r)).sum::<f64>() / n as f64;
    let ys: Vec<f64> = rows.iter().map(|&r| (f64::from(r) - y0) / 100.0).collect();
    let mut m = [[0.0f64; 3]; 3];
    let mut v = [0.0f64; 3];
    for (y, e) in ys.iter().zip(edge) {
        let basis = [1.0, *y, y * y];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += basis[i] * basis[j];
            }
            v[i] += basis[i] * f64::from(*e);
        }
    }
    let coef = solve3(m, v);
    let sum: f64 = ys
        .iter()
        .zip(edge)
        .map(|(y, e)| (f64::from(*e) - (coef[0] + coef[1] * y + coef[2] * y * y)).powi(2))
        .sum();
    (sum / n as f64).sqrt() as f32
}

/// Solves a 3×3 system by Cramer's rule.
fn solve3(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    let det = |a: [[f64; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let d = det(m);
    if d.abs() < 1e-12 {
        return [0.0; 3];
    }
    std::array::from_fn(|k| {
        let mut a = m;
        for (row, value) in a.iter_mut().zip(v) {
            row[k] = value;
        }
        det(a) / d
    })
}

/// (left, right) silhouette roughness of the butte against the sky, and the rows measured.
fn silhouette(depth: &[f32]) -> (f32, f32, usize) {
    let rows = sky_rows(depth);
    // The straight flanks: the middle of the run (its top is the butte's rounded rim, its
    // bottom meets the horizon).
    let n = rows.len();
    let rows = &rows[n / 4..n * 17 / 20];
    let mut left = Vec::with_capacity(rows.len());
    let mut right = Vec::with_capacity(rows.len());
    for &y in rows {
        let l = (0..W).find(|&x| solid_at(depth, x, y, 1)).unwrap_or(0);
        let r = (0..W).rev().find(|&x| solid_at(depth, x, y, -1)).unwrap_or(W - 1);
        left.push(l as f32);
        right.push(r as f32);
    }
    (roughness(rows, &left), roughness(rows, &right), rows.len())
}

/// Background pixels with terrain on both sides (left and right, or above and below) within
/// two pixels: a crack between nodes or LOD levels. The image border is skipped.
fn cracks(depth: &[f32]) -> u32 {
    let mut count = 0;
    for y in 2..H - 2 {
        for x in 2..W - 2 {
            if !sky_at(depth, x, y) {
                continue;
            }
            let horizontal = (terrain_at(depth, x - 1, y) || terrain_at(depth, x - 2, y))
                && (terrain_at(depth, x + 1, y) || terrain_at(depth, x + 2, y));
            let vertical = (terrain_at(depth, x, y - 1) || terrain_at(depth, x, y - 2))
                && (terrain_at(depth, x, y + 1) || terrain_at(depth, x, y + 2));
            if horizontal && vertical {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn cliff_relief_roughens_the_silhouette_of_a_cliff_against_the_sky() {
    let off = shot(0.0);
    let on = shot(1.0);
    let (l0, r0, n0) = silhouette(&off.depth);
    let (l1, r1, n1) = silhouette(&on.depth);
    eprintln!(
        "silhouette roughness (px RMS, left/right): off {l0:.2}/{r0:.2} over {n0} rows, on {l1:.2}/{r1:.2} over {n1} rows; fine-grid nodes {}",
        on.dense_nodes
    );
    assert!(
        n0 >= 40 && n1 >= 40,
        "the butte's flanks are not against the sky ({n0}, {n1} rows)"
    );
    let (off_mean, on_mean) = ((l0 + r0) * 0.5, (l1 + r1) * 0.5);
    assert!(
        on_mean > 2.5 * off_mean.max(0.1) && on_mean > 1.0,
        "cliff relief does not roughen the silhouette: {off_mean:.2} → {on_mean:.2} px"
    );
}

#[test]
fn cliff_relief_leaves_no_cracks_and_uses_the_fine_grid_near_steep_ground() {
    let off = shot(0.0);
    let on = shot(1.0);
    let (c0, c1) = (cracks(&off.depth), cracks(&on.depth));
    eprintln!(
        "crack pixels: off {c0}, on {c1}; fine-grid nodes off {}, on {}",
        off.dense_nodes, on.dense_nodes
    );
    assert_eq!(off.dense_nodes, 0, "relief off still draws the fine grid");
    assert!(on.dense_nodes > 0, "no node near the butte drew the fine grid");
    assert_eq!(c1, 0, "cliff relief opened {c1} crack pixels (relief off: {c0})");
}

#[test]
fn cliff_relief_leaves_driveable_ground_untouched() {
    // The ground in front of the butte, away from its foot: relief on and off draw the same
    // pixels (no displacement, the regular grid, the same shading), so `rendered_height`
    // and the sim agreement on driveable ground hold with relief on.
    let off = shot(0.0);
    let on = shot(1.0);
    let rows = sky_rows(&off.depth);
    let horizon = rows.last().copied().unwrap_or(H / 2) + 1;
    // Columns of the butte on a low flank row, widened by 40 px for its foot.
    let y = rows.get(rows.len() * 17 / 20).copied().unwrap_or(H / 2);
    let l = (0..W)
        .find(|&x| solid_at(&off.depth, x, y, 1))
        .unwrap_or(0)
        .saturating_sub(40);
    let r = ((0..W)
        .rev()
        .find(|&x| solid_at(&off.depth, x, y, -1))
        .unwrap_or(W - 1)
        + 40)
        .min(W - 1);
    let mut compared = 0;
    let mut worst = 0u8;
    for y in horizon + 8..H {
        for x in (0..W).filter(|&x| x < l || x > r) {
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                worst = worst.max(on.pixels[i + c].abs_diff(off.pixels[i + c]));
            }
            compared += 1;
        }
    }
    eprintln!("driveable ground: {compared} pixels compared, worst channel difference {worst}");
    assert!(compared > 2000, "too little ground in view ({compared} pixels)");
    assert!(
        worst <= 1,
        "cliff relief changed driveable ground by {worst} levels"
    );
}

fn hover_shot(cliff_relief: f32) -> &'static Shot {
    static ON: OnceLock<Shot> = OnceLock::new();
    static OFF: OnceLock<Shot> = OnceLock::new();
    if cliff_relief > 0.0 {
        ON.get_or_init(|| render(cliff_relief, HOVER_WALL_M))
    } else {
        OFF.get_or_init(|| render(0.0, HOVER_WALL_M))
    }
}

#[test]
fn faces_under_the_hover_drive_limit_are_only_carved_into() {
    // DO27's hulls may press against or slide down faces up to rise/run 4 (~76°): relief there
    // only carves into the rock, so no relief vertex lies outside the undisplaced surface. With
    // the butte against the far-plane sky, relief on may only turn terrain into sky, never sky
    // into terrain; the rim is still broken (inward carving), so the silhouette roughens.
    let off = hover_shot(0.0);
    let on = hover_shot(1.0);
    // The fine grid (relief on) tessellates the convex rim a little outside the regular grid's
    // chords (relief off), so a pixel next to the old edge may turn: a block pushed outward
    // would reach further. Grown = sky with relief off, terrain with it on, and no relief-off
    // terrain within one pixel.
    let near_off_terrain = |x: u32, y: u32| {
        (y.saturating_sub(1)..=(y + 1).min(H - 1))
            .any(|yy| (x.saturating_sub(1)..=(x + 1).min(W - 1)).any(|xx| terrain_at(&off.depth, xx, yy)))
    };
    let mut grown = 0;
    let mut carved = 0;
    for y in 0..H {
        for x in 0..W {
            if sky_at(&off.depth, x, y) && terrain_at(&on.depth, x, y) && !near_off_terrain(x, y) {
                grown += 1;
            }
            if terrain_at(&off.depth, x, y) && sky_at(&on.depth, x, y) {
                carved += 1;
            }
        }
    }
    // The lit outline, not the depth edge. Inward carving on this gentle face is a
    // smooth inset: the depth edge's RMS only moves about 0.38 → 0.42 px, under this
    // bar. A carved ledge still drops pixels under the lit-terrain threshold, which
    // is what 1.15× was calibrated on. The depth edge is the steep-butte test.
    let (l0, r0, _) = color_silhouette(&off.pixels);
    let (l1, r1, _) = color_silhouette(&on.pixels);
    eprintln!(
        "hover-limit butte: {grown} px grew outward, {carved} px carved; silhouette roughness off {l0:.2}/{r0:.2}, on {l1:.2}/{r1:.2}; fine-grid nodes {}",
        on.dense_nodes
    );
    assert_eq!(
        grown, 0,
        "relief pushed {grown} pixels of the face outward under the drive limit"
    );
    assert!(carved > 50, "relief carved nothing ({carved} px)");
    // Carving only inward shifts the whole outline in by up to 2 m, and only its variation
    // shows as roughness: less than the steep butte's ± relief (4.6×), still above off.
    assert!(
        (l1 + r1) > 1.15 * (l0 + r0).max(0.2),
        "inward carving does not break the silhouette: {:.2} → {:.2} px",
        (l0 + r0) * 0.5,
        (l1 + r1) * 0.5
    );
}

fn color_sky(px: &[u8], x: u32, y: u32) -> bool {
    let i = ((y * W + x) * 4) as usize;
    u32::from(px[i]) + u32::from(px[i + 1]) + u32::from(px[i + 2]) <= 12
}
fn color_terrain(px: &[u8], x: u32, y: u32) -> bool {
    let i = ((y * W + x) * 4) as usize;
    u32::from(px[i]) + u32::from(px[i + 1]) + u32::from(px[i + 2]) > 120
}
fn color_solid(px: &[u8], x: u32, y: u32, dir: i32) -> bool {
    (0..3).all(|k| {
        let xx = x as i32 + dir * k;
        (0..W as i32).contains(&xx) && color_terrain(px, xx as u32, y)
    })
}
fn color_silhouette(px: &[u8]) -> (f32, f32, usize) {
    let rows: Vec<u32> = (0..H)
        .filter(|&y| color_sky(px, 0, y) && color_sky(px, W - 1, y) && color_terrain(px, W / 2, y))
        .collect();
    let n = rows.len();
    let rows = &rows[n / 4..n * 17 / 20];
    let mut left = Vec::with_capacity(rows.len());
    let mut right = Vec::with_capacity(rows.len());
    for &y in rows {
        left.push((0..W).find(|&x| color_solid(px, x, y, 1)).unwrap_or(0) as f32);
        right.push((0..W).rev().find(|&x| color_solid(px, x, y, -1)).unwrap_or(W - 1) as f32);
    }
    (roughness(rows, &left), roughness(rows, &right), rows.len())
}

/// Left and right depth edges (pixels) of the measured flanks.
fn flank_edges(depth: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let rows = sky_rows(depth);
    let n = rows.len();
    let rows = &rows[n / 4..n * 17 / 20];
    let mut left = Vec::with_capacity(rows.len());
    let mut right = Vec::with_capacity(rows.len());
    for &y in rows {
        let l = (0..W).find(|&x| solid_at(depth, x, y, 1)).unwrap_or(0);
        let r = (0..W).rev().find(|&x| solid_at(depth, x, y, -1)).unwrap_or(W - 1);
        left.push(l as f32);
        right.push(r as f32);
    }
    (left, right)
}

fn max_abs_delta(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(p, q)| (p - q).abs())
        .fold(0.0_f32, f32::max)
}

/// The planet surface is a sky colour. Drawing it must not move the cliff's depth edge
/// on either flank, with relief on or off.
#[test]
fn the_planet_surface_does_not_move_the_cliff_depth_edge() {
    for relief in [0.0_f32, 1.0] {
        let drawn = render_view(relief, STEEP_WALL_M, true);
        let skipped = render_view(relief, STEEP_WALL_M, false);
        let mismatch = drawn
            .depth
            .iter()
            .zip(skipped.depth.iter())
            .filter(|(a, b)| (**a >= SKY_DEPTH) != (**b >= SKY_DEPTH))
            .count();
        let (l_on, r_on) = flank_edges(&drawn.depth);
        let (l_off, r_off) = flank_edges(&skipped.depth);
        let dl = if l_on.len() == l_off.len() {
            max_abs_delta(&l_on, &l_off)
        } else {
            f32::MAX
        };
        let dr = if r_on.len() == r_off.len() {
            max_abs_delta(&r_on, &r_off)
        } else {
            f32::MAX
        };
        eprintln!(
            "planet surface vs skipped, relief {relief}: sky-mask mismatches {mismatch}, flank edge Δ left {dl:.3} px / right {dr:.3} px over {} rows",
            l_on.len()
        );
        assert!(
            l_on.len() >= 40 && l_on.len() == l_off.len(),
            "flank rows differ with the planet surface (relief {relief}: {} vs {})",
            l_on.len(),
            l_off.len()
        );
        assert_eq!(
            mismatch, 0,
            "the planet surface changed the cliff's depth mask at relief {relief}"
        );
        assert!(
            dl < 1e-3 && dr < 1e-3,
            "cliff depth edge moved at relief {relief}: left {dl:.3} px, right {dr:.3} px"
        );
    }
}
