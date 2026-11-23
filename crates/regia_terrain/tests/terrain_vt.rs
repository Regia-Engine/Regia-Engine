// SPDX-License-Identifier: MPL-2.0
//! Terrain virtual texture (ADR-117 "Terrain virtual texture").
//!
//! - The bake rebuilds, for every texel, the surface point the colour pass would interpolate
//!   at that XZ: its height matches the CPU mirror of the drawn CDLOD surface
//!   ([`regia_terrain::RenderedSurface::heights`]) under a morphing LOD.
//! - Page borders are crack-free: a page's border texels are bit-identical to its
//!   neighbours' edge texels, in every map.
//! - A held camera converges: every requested page resident, nothing re-baked.
//! - The frame with the texture matches the frame without it.
//! - VT-served pixels get the uncached path's horizon visibility (the planet-shine fill
//!   hidden behind the hills; ADR-117 addendum v0.1.2), and map E holds the slopes.
//! - A host that re-sends a jittering sun and ambient every frame does not flush the cache;
//!   a real sun turn does.

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw, SkyBody,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, RenderedSurface, Rgba8Image, TerrainLayers,
    TerrainMap, TerrainMaterial, TerrainSharedDraw, TerrainVtHandle, VirtualTextureSettings, VtCache,
};

const W: u32 = 320;
const H: u32 = 180;
const EYE: [f32; 3] = [10.0, 15.0, -60.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// 512 m, 1 m samples: rolling hills, so the CDLOD morph moves vertices everywhere, and a
/// cliff near the camera (cliff relief draws the fine grid there).
fn map() -> TerrainMap {
    let n = 513u32;
    let world = 512.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            // A 30 m cliff across the view (steep enough for cliff relief's fine grid).
            let t = ((x - 20.0) / 12.0).clamp(0.0, 1.0);
            let cliff = 30.0 * t * t * (3.0 - 2.0 * t);
            heights
                .push(8.0 * (x / 23.0).sin() * (z / 31.0).cos() + 3.0 * (x / 7.0 + z / 11.0).sin() + cliff);
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 40.0, -20.0).expect("map")
}

fn settings() -> VirtualTextureSettings {
    VirtualTextureSettings {
        lod0_texel_m: 0.05,
        page_texels: 30,
        atlas_slots: 24,
        near_m: 0.0,
        bakes_per_frame: 600,
        rebakes_per_frame: 0,
        // These frames have no temporal filter.
        stochastic_lod: false,
        ..VirtualTextureSettings::default()
    }
}

fn draw(vt: Option<VirtualTextureSettings>, debug_view: u32) -> TerrainSharedDraw {
    let mut draw = draw_without_host_choice(debug_view);
    draw.set_virtual_texture(vt).expect("virtual texture");
    draw
}

/// The draw before the host chooses anything about the virtual texture.
fn draw_without_host_choice(debug_view: u32) -> TerrainSharedDraw {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
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
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw =
        TerrainSharedDraw::new(device(), map(), &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: noise_albedo(v),
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
    draw.set_debug_view(debug_view);
    draw
}

/// A 64² albedo with texel-scale value noise around `v` (texture detail for the frame
/// comparison to reconstruct).
fn noise_albedo(v: u8) -> Rgba8Image {
    let mut img = Rgba8Image::solid(64, 64, [v, v, v, 255]);
    let mut seed = 0x9e37_79b9u32;
    for px in img.data.chunks_mut(4) {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let d = (seed % 61) as i32 - 30;
        let c = (i32::from(v) + d).clamp(0, 255) as u8;
        px[0] = c;
        px[1] = c;
        px[2] = c;
    }
    img
}

struct Run {
    pixels: Vec<u8>,
    handle: TerrainVtHandle,
    surface: RenderedSurface,
}

/// The light of a [`run_lit`] frame.
#[derive(Clone, Copy)]
struct Light {
    /// Sun travel direction and illuminance (lux).
    sun: ([f32; 3], f32),
    /// Planet-shine fill: direction toward the body and its lux.
    fill: Option<([f32; 3], f32)>,
    width: u32,
    height: u32,
    /// Camera position and target.
    eye: [f32; 3],
    target: [f32; 3],
}

const DEFAULT_LIGHT: Light = Light {
    sun: ([-0.5, -0.6, 0.3], 20_000.0),
    fill: None,
    width: W,
    height: H,
    eye: EYE,
    target: [0.0, 0.0, 0.0],
};

/// `frames` frames of the same view; the last frame's pixels.
fn run(vt: Option<VirtualTextureSettings>, debug_view: u32, frames: usize) -> Run {
    run_lit(vt, debug_view, frames, DEFAULT_LIGHT, |_, _, _| {})
}

/// [`run`] under `light`; `each(frame index, &mut scene, handle)` runs before every frame
/// (it may change the sun or the ambient; the handle's stats are the previous frame's).
fn run_lit(
    vt: Option<VirtualTextureSettings>,
    debug_view: u32,
    frames: usize,
    light: Light,
    mut each: impl FnMut(usize, &mut LabScene, &TerrainVtHandle),
) -> Run {
    let draw = draw(vt, debug_view);
    let handle = draw.vt_handle();
    let surface = draw.surface();
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let mut scene = LabScene {
        width: light.width,
        height: light.height,
        eye: light.eye,
        target: light.target,
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new(light.sun.0, [1.0, 1.0, 1.0], light.sun.1),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    // Cache fidelity is measured with isotropic filtering (v0.1.7). The whole-surface pages are
    // baked per texel and do not hold the uncached frame's anisotropic detail: at Desktop
    // High's 8x, Surface vs uncached is SSIM 0.949 (0.993 here) and the fill view 0.913
    // (0.975). ADR-192, open item.
    quality.terrain_budget.anisotropy = 0;
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        sky_body: light.fill.map(|(direction, lux)| SkyBody {
            direction,
            fill_lux: Some(lux),
            ..SkyBody::jupiter_from_europa()
        }),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut pixels = Vec::new();
    for i in 0..frames {
        each(i, &mut scene, &handle);
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    }
    Run {
        pixels,
        handle,
        surface,
    }
}

fn supported() -> bool {
    let ok = device()
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST);
    if !ok {
        eprintln!("SKIP: the device lacks SHADER_EARLY_DEPTH_TEST (the virtual texture stays off)");
    }
    ok
}

#[test]
fn the_bake_rebuilds_the_drawn_surface() {
    if !supported() {
        return;
    }
    // Debug view 96: map 1 holds fract(height), pyramid level, |n.y|, distance per texel.
    let r = run(Some(settings()), 96, 8);
    let layout = r.handle.layout().expect("layout");
    let pages = r.handle.resident_pages();
    assert!(pages.len() > 20, "only {} pages resident", pages.len());
    let terrain = map();
    let side = layout.page_texels + 2;
    let (mut n, mut close, mut worst) = (0usize, 0usize, 0.0f32);
    for &(lod, x, y) in pages.iter().filter(|p| p.0 <= 4).take(60) {
        let maps = r.handle.read_page(lod, x, y).expect("page");
        let mut points = Vec::new();
        for j in 0..side {
            for i in 0..side {
                points.push(layout.texel_xz(lod, x, y, i, j));
            }
        }
        let heights = r.surface.heights(&terrain, &points).expect("drawn surface");
        for (k, h) in heights.iter().enumerate() {
            let p = points[k];
            if !h.is_finite() || p[0].abs() > 250.0 || p[1].abs() > 250.0 {
                continue;
            }
            let baked = maps[1][k][0];
            let d = (baked - h.rem_euclid(1.0)).abs();
            let d = d.min(1.0 - d);
            n += 1;
            close += usize::from(d < 0.02);
            if d >= 0.02 && regia_core::brand::env_var_os("TERRAIN_VT_DUMP").is_some() {
                println!(
                    "bad lod {lod} page ({x},{y}) xz ({:.3},{:.3}) baked fract {:.3} m {:.3} ny {:.3} drawn {:.3}",
                    p[0], p[1], baked, maps[1][k][1], maps[1][k][2], h
                );
            }
            worst = worst.max(d);
        }
    }
    let share = close as f32 / n.max(1) as f32;
    println!(
        "{n} texels, {:.4} % within 2 cm of the drawn surface, worst {worst:.3} m",
        share * 100.0
    );
    assert!(n > 10_000, "too few texels checked: {n}");
    assert!(
        share > 0.999,
        "{:.4} % of texels within 2 cm (worst {worst} m)",
        share * 100.0
    );
}

#[test]
fn page_borders_are_crack_free() {
    if !supported() {
        return;
    }
    for cache in [VtCache::Relief, VtCache::Lighting, VtCache::Surface] {
        let r = run(Some(VirtualTextureSettings { cache, ..settings() }), 0, 8);
        let layout = r.handle.layout().expect("layout");
        let pages = r.handle.resident_pages();
        let p = layout.page_texels as usize;
        let side = p + 2;
        let mut pairs = 0;
        for &(lod, x, y) in &pages {
            if !pages.contains(&(lod, x + 1, y)) || !pages.contains(&(lod, x, y + 1)) {
                continue;
            }
            // Four maps, plus map E (the horizon slopes) in the whole-surface mode.
            let read = |lod, x, y| {
                let mut maps = r.handle.read_page(lod, x, y).expect("page").to_vec();
                if cache == VtCache::Surface {
                    maps.push(r.handle.read_page_horizon(lod, x, y).expect("map E"));
                }
                maps
            };
            let here = read(lod, x, y);
            let right = read(lod, x + 1, y);
            let below = read(lod, x, y + 1);
            for m in 0..here.len() {
                for j in 1..=p {
                    // This page's right border column = the right page's first payload column;
                    // this page's last payload column = the right page's left border column.
                    assert_eq!(
                        here[m][j * side + p + 1],
                        right[m][j * side + 1],
                        "map {m} lod {lod} ({x},{y}) row {j}"
                    );
                    assert_eq!(
                        here[m][j * side + p],
                        right[m][j * side],
                        "map {m} lod {lod} ({x},{y}) row {j}"
                    );
                    // The same downward.
                    assert_eq!(
                        here[m][(p + 1) * side + j],
                        below[m][side + j],
                        "map {m} lod {lod} ({x},{y}) col {j}"
                    );
                    assert_eq!(
                        here[m][p * side + j],
                        below[m][j],
                        "map {m} lod {lod} ({x},{y}) col {j}"
                    );
                }
            }
            pairs += 1;
            if pairs >= 12 {
                break;
            }
        }
        assert!(
            pairs >= 3,
            "{cache:?}: only {pairs} page pairs with both neighbors resident"
        );
    }
}

#[test]
fn a_held_view_converges_and_matches_the_uncached_frame() {
    if !supported() {
        return;
    }
    let off = run(None, 0, 20);
    let probe = run(Some(settings()), 96, 20);
    let diff_px = off
        .pixels
        .iter()
        .zip(&probe.pixels)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    // The texture's debug view (96) recolours every pixel it serves: the VT pipeline draws.
    assert!(diff_px > 20_000, "debug view 96 changed only {diff_px} channels");
    let rgb = |px: &[u8]| -> Vec<[f32; 3]> {
        px.as_chunks::<4>()
            .0
            .iter()
            .map(|c| [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0])
            .collect()
    };
    // Floors a little under what lavapipe measures (no TAA here, so the texel
    // reconstruction shows more than on the DO27 plates).
    for (cache, floor) in [
        (VtCache::Lighting, 0.999),
        (VtCache::Relief, 0.995),
        (VtCache::Surface, 0.95),
    ] {
        let on = run(Some(VirtualTextureSettings { cache, ..settings() }), 0, 20);
        let stats = on.handle.stats().expect("stats");
        assert_eq!(
            stats.missing, 0,
            "{cache:?}: requested pages left unbaked: {stats:?}"
        );
        assert_eq!(
            stats.baked + stats.rebaked,
            0,
            "{cache:?}: a held view still bakes: {stats:?}"
        );
        assert!(
            stats.resident > 0 && stats.hit_rate > 0.99,
            "{cache:?}: {stats:?}"
        );
        let ssim = regia_render::compare::ssim(&rgb(&off.pixels), &rgb(&on.pixels), W, H);
        let max = off
            .pixels
            .iter()
            .zip(&on.pixels)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        println!("{cache:?} vs uncached: ssim {ssim:.5}, max |d| {max}");
        if let Some(dir) = regia_core::brand::env_var_os("TERRAIN_VT_PNG") {
            for (name, px) in [("off", &off.pixels), (&format!("{cache:?}") as &str, &on.pixels)] {
                let path = std::path::Path::new(&dir).join(format!("{name}.png"));
                if let Ok(file) = std::fs::File::create(&path) {
                    let mut encoder = png::Encoder::new(file, W, H);
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    if let Ok(mut writer) = encoder.write_header() {
                        let _ = writer.write_image_data(px);
                    }
                }
            }
        }
        assert!(ssim > floor, "{cache:?}: ssim {ssim} under {floor}");
    }
}

/// Three frames of the default view with `TerrainBudget::virtual_texture` set to `want` in the
/// frame quality; the handle's settings afterwards.
fn profile_run(draw: TerrainSharedDraw, want: bool) -> Option<VirtualTextureSettings> {
    let handle = draw.vt_handle();
    let post = PostSettings::golden_deterministic();
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
    quality.terrain_budget.virtual_texture = want;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    for _ in 0..3 {
        renderer.render_offscreen(device(), &frame).expect("frame");
    }
    handle.settings()
}

#[test]
fn the_profile_lever_drives_the_texture_until_the_host_chooses() {
    // ADR-157 addendum v0.1.2: `TerrainBudget::virtual_texture` turns the texture on with the
    // profile settings (both LODs: Desktop Low has no TAA); off without it; an explicit host
    // call (`None` included) wins over the profile.
    assert_eq!(
        profile_run(draw_without_host_choice(0), true),
        Some(VirtualTextureSettings::for_profile())
    );
    assert!(!VirtualTextureSettings::for_profile().stochastic_lod);
    assert_eq!(profile_run(draw_without_host_choice(0), false), None);
    assert_eq!(profile_run(draw(None, 0), true), None, "the host chose off");
    assert_eq!(
        profile_run(draw(Some(settings()), 0), false),
        Some(settings()),
        "the host chose its own settings"
    );
}

/// Mean Rec. 709 luma (0–255) per pixel of an RGBA8 frame.
fn luma(px: &[u8]) -> Vec<f32> {
    px.as_chunks::<4>()
        .0
        .iter()
        .map(|c| 0.2126 * f32::from(c[0]) + 0.7152 * f32::from(c[1]) + 0.0722 * f32::from(c[2]))
        .collect()
}

#[test]
fn vt_served_pixels_keep_the_horizon_visibility_of_the_fill() {
    // ADR-117 addendum v0.1.2. A fill-only frame: a near-black sun, and the planet-shine fill
    // 16° over the horizon, so the hills' sloped faces and valleys hide it wherever the
    // horizon toward it rises above 16° (`fill_horizon_vis`). Before map E the whole-surface
    // mode read "horizon not computed" there and lit those faces with the full fill.
    if !supported() {
        return;
    }
    let el = 16f32.to_radians();
    let light = Light {
        sun: ([-0.5, -0.6, 0.3], 1.0),
        fill: Some(([0.8 * el.cos(), el.sin(), 0.6 * el.cos()], 60_000.0)),
        ..DEFAULT_LIGHT
    };
    let off = run_lit(None, 0, 20, light, |_, _, _| {});
    let on = run_lit(Some(settings()), 0, 20, light, |_, _, _| {});
    let stats = on.handle.stats().expect("stats");
    assert!(
        stats.resident > 0 && stats.hit_rate > 0.99 && stats.missing == 0,
        "{stats:?}"
    );
    let (l_off, l_on) = (luma(&off.pixels), luma(&on.pixels));
    // The frame must show both lit and fill-hidden ground (else it tests nothing).
    let lit = l_off.iter().filter(|&&l| l > 40.0).count();
    let hidden = l_off.iter().filter(|&&l| l < 8.0).count();
    let n = l_off.len();
    println!("fill frame: {lit} lit, {hidden} dark of {n} pixels");
    // Where the uncached frame is dark (fill hidden), the cached one must be too.
    let mut brighter = 0usize;
    let mut sum = 0.0f64;
    for (a, b) in l_off.iter().zip(&l_on) {
        if *a < 8.0 {
            sum += f64::from(b - a);
            brighter += usize::from(b - a > 16.0);
        }
    }
    let mean = sum / hidden.max(1) as f64;
    let share = brighter as f64 / hidden.max(1) as f64;
    let rgb = |px: &[u8]| -> Vec<[f32; 3]> {
        px.as_chunks::<4>()
            .0
            .iter()
            .map(|c| [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0])
            .collect()
    };
    let ssim = regia_render::compare::ssim(&rgb(&off.pixels), &rgb(&on.pixels), W, H);
    println!(
        "fill-hidden pixels: mean luma +{mean:.2}, {:.2} % brighter by > 16; ssim {ssim:.4}",
        share * 100.0
    );
    if let Some(dir) = regia_core::brand::env_var_os("TERRAIN_VT_PNG") {
        for (name, px) in [("fill_off", &off.pixels), ("fill_surface", &on.pixels)] {
            let path = std::path::Path::new(&dir).join(format!("{name}.png"));
            if let Ok(file) = std::fs::File::create(&path) {
                let mut encoder = png::Encoder::new(file, W, H);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                if let Ok(mut writer) = encoder.write_header() {
                    let _ = writer.write_image_data(px);
                }
            }
        }
    }
    assert!(lit > n / 20 && hidden > n / 20, "{lit} lit, {hidden} dark of {n}");
    assert!(
        mean < 2.0,
        "fill-hidden pixels lit by the cache: mean luma +{mean:.2}"
    );
    assert!(
        share < 0.01,
        "{:.2} % of fill-hidden pixels lit by the cache",
        share * 100.0
    );
    assert!(ssim > 0.95, "ssim {ssim}");

    // Map E holds slopes where the hills rise around a texel (not the "not computed" −1000).
    let pages = on.handle.resident_pages();
    let (mut texels, mut computed) = (0usize, 0usize);
    for &(lod, x, y) in pages.iter().take(40) {
        for t in on.handle.read_page_horizon(lod, x, y).expect("map E") {
            texels += 1;
            computed += usize::from(t.iter().all(|v| v.is_finite() && *v > -999.0 && *v < 100.0));
        }
    }
    assert!(
        texels > 0 && computed * 10 > texels * 9,
        "{computed} of {texels} texels carry slopes"
    );
}

#[test]
fn a_jittering_sun_and_ambient_do_not_flush_and_a_sun_turn_does() {
    // A host re-sends the sun and the ambient every frame with float drift (DO27's shell
    // recomputes them from a light focus that follows the camera): ±1e-4 rad on the sun and
    // ±1e-4 relative on its illuminance and the ambient must keep the pages (the content key's
    // hysteresis, `VT_SUN_TOLERANCE_RAD` = 0.1°); a 2° turn must flush.
    if !supported() {
        return;
    }
    let light = Light {
        width: 96,
        height: 54,
        ..DEFAULT_LIGHT
    };
    let base = [-0.5f32, -0.6, 0.3];
    let turn = |v: [f32; 3], rad: f32| {
        let (s, c) = rad.sin_cos();
        [v[0] * c - v[1] * s, v[0] * s + v[1] * c, v[2]]
    };
    let mut seed = 0x2545_f491u32;
    let mut flushes = Vec::new();
    let mut turn_flushed = None;
    let frames = 300;
    run_lit(Some(settings()), 0, frames + 2, light, |i, scene, handle| {
        // The previous frame's counters. Frames 0 and 1 set the cache up: frame 0 starts it
        // empty, and frame 1 sees the frame's sky irradiance map bound for the first time
        // (its scale is part of the key: the baked ground bounce reads it).
        let flushed = handle.stats().is_some_and(|s| s.flushed);
        if (3..=frames).contains(&i) && flushed {
            flushes.push(i - 1);
        }
        if i == frames + 1 {
            turn_flushed = Some(flushed);
        }
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let j = (seed % 2001) as f32 / 1000.0 - 1.0;
        let sun = if i >= frames {
            turn(base, 2f32.to_radians())
        } else {
            turn(base, 1e-4 * j)
        };
        scene.directional = DirectionalLightUniform::new(sun, [1.0, 1.0, 1.0], 20_000.0 * (1.0 + 1e-4 * j));
        scene.ambient_lux = 500.0 * (1.0 + 1e-4 * j);
    });
    println!("flushes during the jitter: {flushes:?}; the 2° turn flushed: {turn_flushed:?}");
    assert!(
        flushes.is_empty(),
        "jitter flushed the cache at frames {flushes:?}"
    );
    assert_eq!(turn_flushed, Some(true), "a 2° sun turn kept the cache");
}

#[test]
fn the_map_edge_has_no_dark_seam() {
    // The texels of a page that hang over the map's edge (its border and the part of the
    // last pages past the heightfield) used to bake "no surface" (normal −1000, relief shadow
    // 0), and the bilinear filter blended them into the last real texels: a thin dark line
    // along the terrain's silhouette against what lies beyond the map (the arc-sweep's
    // Switchbacks flyover). The bake now clamps those texels onto the edge.
    if !supported() {
        return;
    }
    let light = Light {
        eye: [40.0, 30.0, 150.0],
        target: [0.0, 5.0, 256.0],
        ..DEFAULT_LIGHT
    };
    let off = run_lit(None, 0, 20, light, |_, _, _| {});
    let on = run_lit(Some(settings()), 0, 20, light, |_, _, _| {});
    let stats = on.handle.stats().expect("stats");
    assert!(stats.resident > 0 && stats.hit_rate > 0.99, "{stats:?}");
    let (l_off, l_on) = (luma(&off.pixels), luma(&on.pixels));
    let darker = l_off.iter().zip(&l_on).filter(|(a, b)| **a - **b > 40.0).count();
    println!("map edge: {darker} pixels more than 40 levels darker with the texture");
    if let Some(dir) = regia_core::brand::env_var_os("TERRAIN_VT_PNG") {
        for (name, px) in [("edge_off", &off.pixels), ("edge_surface", &on.pixels)] {
            let path = std::path::Path::new(&dir).join(format!("{name}.png"));
            if let Ok(file) = std::fs::File::create(&path) {
                let mut encoder = png::Encoder::new(file, W, H);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                if let Ok(mut writer) = encoder.write_header() {
                    let _ = writer.write_image_data(px);
                }
            }
        }
    }
    assert!(
        darker < 20,
        "{darker} pixels darker by > 40 levels along the map edge"
    );
}
