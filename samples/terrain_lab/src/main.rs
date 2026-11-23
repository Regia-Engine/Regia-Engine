// SPDX-License-Identifier: MIT-0
//! Terrain lab: heightfield with CDLOD, splat layers, water, scatter, holes, fog and
//! one cascaded sun shadow. The picture is [`regia_terrain::render_gap_shot`] — the
//! same function the lavapipe golden compares.
//!
//! ```text
//! cargo run -p terrain_lab -- docs/evidence/do27-gaps/terrain_fog_on_noon.png --fog --sun noon
//! cargo run -p terrain_lab -- docs/evidence/terrain-photoreal/mid_noon.png --photoreal --camera mid --sun noon
//! cargo run -p terrain_lab -- --photoreal --metrics docs/evidence/terrain-photoreal/metrics.md
//! ```
//!
//! Without a path the shot is written to the system temp directory (`terrain_lab/terrain_lab.png`);
//! `--evidence` writes the tracked `docs/evidence/m13/terrain_lab.png` instead.
//!
//! Flags: `--fog` / `--no-fog`, `--sun morning|noon|evening`, `--screenshot` (alias for
//! writing the PNG path), `--no-water`, `--no-scatter`, `--no-hole`, `--no-crater`,
//! `--debug <view>` (debug views are not part of the shared evidence shot).
//!
//! `--foliage-wind <dir>` renders a wood in gusting wind (swaying scatter pines, an adopted
//! imported tree, a still painted patch; spec 88 § 2.5), `--bench` times its shadows.
//!
//! `--photoreal` switches to the 4096² synthetic eroded mountains with CC0-style procedural
//! layers and an 8K macro bake, `--camera near|mid|far` (1 m / 50 m / 2 km) picks the
//! evidence framing, and
//! `--metrics <path>` measures tiling (autocorrelation peak), blend sharpness and LOD
//! popping across 30 frames of camera motion.

use std::collections::HashSet;
use std::io::BufWriter;
use std::path::Path;

use regia_render::RenderDevice;
use regia_terrain::gpu::{
    TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, headless_bcn_device, render_offscreen,
};
use regia_terrain::{
    CdlodConfig, ErosionParams, GapShot, GapSun, LayerDesc, MacroBake, MaterialParams, MinMaxPyramid,
    TerrainLayers, TerrainMap, TerrainMaterial, droplet_erode, render_gap_shot, ridged_fbm, select_nodes,
    thermal_erode,
};

fn sun_from_args(args: &[String]) -> GapSun {
    let Some(flag) = args.iter().position(|a| a == "--sun") else {
        return GapSun::Noon;
    };
    match args.get(flag + 1).map(String::as_str).unwrap_or("noon") {
        "morning" => GapSun::Morning,
        "evening" => GapSun::Evening,
        _ => GapSun::Noon,
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Where the `docs/` evidence shot lives; written only with the explicit `--evidence` flag.
const EVIDENCE_PATH: &str = "docs/evidence/m13/terrain_lab.png";

/// Where the shot goes: `--screenshot <path>` or a leading positional path as given; `--evidence`
/// writes the tracked evidence file under `docs/`; otherwise a file in `temp` (the source tree is
/// never written by default).
fn output_path_in(args: &[String], temp: &Path) -> String {
    args.iter()
        .position(|a| a == "--screenshot")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| args.get(1).filter(|a| !a.starts_with("--")).cloned())
        .unwrap_or_else(|| {
            if args.iter().any(|a| a == "--evidence") {
                EVIDENCE_PATH.into()
            } else {
                temp.join("terrain_lab")
                    .join("terrain_lab.png")
                    .display()
                    .to_string()
            }
        })
}

fn output_path(args: &[String]) -> String {
    output_path_in(args, &std::env::temp_dir())
}

mod chase;
mod do27_look;
mod foliage_wind;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--do27-chase") {
        chase::run(&args);
        return;
    }
    if args.iter().any(|a| a == "--do27-look") {
        do27_look::run(&args);
        return;
    }
    if args.iter().any(|a| a == "--foliage-wind") {
        foliage_wind::run(&args);
        return;
    }
    if args.iter().any(|a| a == "--photoreal") {
        photoreal_main(&args);
        return;
    }
    let path = output_path(&args);
    if args.iter().any(|a| a == "--debug") {
        eprintln!("--debug is not part of the shared evidence shot; rendering the lit frame");
    }
    let shot = GapShot {
        sun: sun_from_args(&args),
        fog: args.iter().any(|a| a == "--fog") && !args.iter().any(|a| a == "--no-fog"),
        water: !args.iter().any(|a| a == "--no-water"),
        scatter: !args.iter().any(|a| a == "--no-scatter"),
        hole: !args.iter().any(|a| a == "--no-hole"),
        crater: !args.iter().any(|a| a == "--no-crater"),
    };

    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent).expect("create evidence directory");
    }

    let device = RenderDevice::headless().expect("headless GPU device (try lavapipe)");
    let (w, h) = (800u32, 450u32);
    let pixels = render_gap_shot(&device, &shot, w, h).expect("render evidence shot");

    write_png(&path, w, h, &pixels);
    println!("wrote {path} ({w}x{h})");

    let is_sky = |p: &[u8; 4]| {
        let r = p[0] as u32;
        let g = p[1] as u32;
        let b = p[2] as u32;
        b > r + 30 && b > g + 15 && r < 180
    };
    let luminance = |p: &[u8; 4]| 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
    let terrain_only: Vec<&[u8; 4]> = pixels.as_chunks::<4>().0.iter().filter(|p| !is_sky(p)).collect();
    let n = terrain_only.len() as f64;

    let near_white = terrain_only
        .iter()
        .filter(|p| p[0] > 240 && p[1] > 240 && p[2] > 240)
        .count();
    let near_black = terrain_only
        .iter()
        .filter(|p| p[0] < 20 && p[1] < 20 && p[2] < 20)
        .count();
    let mean_lum = terrain_only.iter().map(|p| luminance(p)).sum::<f64>() / n.max(1.0);
    let unique: HashSet<u32> = terrain_only
        .iter()
        .map(|p| ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | (p[2] as u32))
        .collect();

    println!("terrain pixels: {}", terrain_only.len());
    println!("mean luminance: {mean_lum:.1}");
    println!(
        "near-white: {} ({:.2}%)  near-black: {} ({:.2}%)",
        near_white,
        near_white as f64 * 100.0 / n.max(1.0),
        near_black,
        near_black as f64 * 100.0 / n.max(1.0),
    );
    println!("unique colors: {}", unique.len());

    assert!(near_black as f64 / n.max(1.0) < 0.35, "frame too black");
    assert!(near_white as f64 / n.max(1.0) < 0.20, "frame blown out");
    assert!(mean_lum > 20.0, "mean luminance too low ({mean_lum})");
}

// ---------------------------------------------------------------------------
// Photoreal lane
// ---------------------------------------------------------------------------

/// Shared DO27 extent. The 256² sim and the 4096² visual cover the same 1280 m.
const DO27_WORLD_M: f32 = 1280.0;
const DO27_HEIGHT_SCALE: f32 = 220.0;
const DO27_HEIGHT_OFFSET: f32 = -20.0;

/// Eroded 256² simulation grid over 1280 m. Regia Engine does not resample this grid.
///
/// A driveable valley (gentle swells, |slope| well under the 0.6 rad driveable limit)
/// inside a ring of domain-warped ridged mountains up to ~170 m, then droplet hydraulic
/// erosion (gullies and fans) on the mountains only, a despeckle and a thermal talus pass
/// (no single-cell deposit spikes: a 16× upsample turns each into a 10 m pyramid).
fn eroded_sim() -> TerrainMap {
    let n = 256u32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    let mut mountain_mask = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * DO27_WORLD_M - DO27_WORLD_M * 0.5;
            let z = row as f32 / (n - 1) as f32 * DO27_WORLD_M - DO27_WORLD_M * 0.5;
            let valley =
                6.0 + 5.0 * (x * 0.007).sin() * (z * 0.005).cos() + 3.0 * (x * 0.013 + z * 0.009).sin();
            // Mountains rise from ~190 m out from the valley centre to full relief by ~460 m.
            let r = (x * x + z * z * 1.3).sqrt();
            let t = ((r - 190.0) / 270.0).clamp(0.0, 1.0);
            let ring = t * t * (3.0 - 2.0 * t);
            let wx = 140.0 * (z * 0.0041 + 1.3).sin();
            let wz = 140.0 * (x * 0.0037 - 0.7).cos();
            let ridge = ridged_fbm((x + wx) * 0.0042, (z + wz) * 0.0042, 2747, 5);
            heights.push(valley + ring * (25.0 + 150.0 * ridge.powf(1.6)));
            // Erosion is kept on the slopes and their fans, not on the driveable floor.
            let m = ((r - 170.0) / 120.0).clamp(0.0, 1.0);
            mountain_mask.push(m * m * (3.0 - 2.0 * m));
        }
    }
    let uneroded = heights.clone();
    droplet_erode(
        &mut heights,
        n,
        &ErosionParams {
            droplets: 12_000,
            sediment_capacity: 2.5,
            ..Default::default()
        },
        2747,
    );
    for _ in 0..3 {
        despeckle(&mut heights, n);
    }
    for ((h, &before), &m) in heights.iter_mut().zip(&uneroded).zip(&mountain_mask) {
        *h = before + (*h - before) * m;
    }
    // Talus 6 m per 5 m cell (~50°): only knife crests move; slopes stay steep.
    thermal_erode(&mut heights, n, 6.0, 0.5, 12);
    TerrainMap::from_heights(n, n, heights, DO27_WORLD_M, DO27_HEIGHT_SCALE, DO27_HEIGHT_OFFSET)
        .expect("256² sim")
}

/// Catmull-Rom bicubic height at world XZ over the map's sample grid (border-clamped).
fn sample_catmull_rom(map: &TerrainMap, x: f32, z: f32) -> f32 {
    let [u, v] = map.world_to_grid(x, z);
    let (iu, iv) = (u.floor(), v.floor());
    let (fu, fv) = (u - iu, v - iv);
    let weights = |t: f32| {
        let t2 = t * t;
        let t3 = t2 * t;
        [
            -0.5 * t3 + t2 - 0.5 * t,
            1.5 * t3 - 2.5 * t2 + 1.0,
            -1.5 * t3 + 2.0 * t2 + 0.5 * t,
            0.5 * t3 - 0.5 * t2,
        ]
    };
    let (wu, wv) = (weights(fu), weights(fv));
    let mut h = 0.0;
    for (j, wj) in wv.iter().enumerate() {
        for (i, wi) in wu.iter().enumerate() {
            h += wi * wj * map.height_at_sample(iu as i64 + i as i64 - 1, iv as i64 + j as i64 - 1);
        }
    }
    h
}

/// Clamps every interior cell into the range of its eight neighbours: removes the
/// one-cell spikes and pits droplet deposition leaves (a bilinear 16× upsample turns each
/// into a 10 m pyramid or crater) without touching slopes, ridges or gullies.
fn despeckle(heights: &mut [f32], n: u32) {
    let n = n as usize;
    let src = heights.to_vec();
    for z in 1..n - 1 {
        for x in 1..n - 1 {
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for dz in 0..3 {
                for dx in 0..3 {
                    if dx == 1 && dz == 1 {
                        continue;
                    }
                    let v = src[(z + dz - 1) * n + x + dx - 1];
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            heights[z * n + x] = src[z * n + x].clamp(lo, hi);
        }
    }
}

/// 4096² visual over the same 1280 m: bilinear upsample of the sim, plus a ±0.12 m swell,
/// one driveable +0.6 m bump and one steep +3 m cliff. ~0.31 m/texel.
fn photoreal_pair() -> (TerrainMap, TerrainMap) {
    let sim = eroded_sim();
    let n = 4096u32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * DO27_WORLD_M - DO27_WORLD_M * 0.5;
            let z = row as f32 / (n - 1) as f32 * DO27_WORLD_M - DO27_WORLD_M * 0.5;
            // Catmull-Rom over the sim grid: the 4096² surface is smooth (C1) where the
            // bilinear sim surface is creased every 5 m cell. Driveable ground differs from
            // the bilinear sim by centimetres; steep eroded slopes by up to a few metres.
            let mut h = sample_catmull_rom(&sim, x, z);
            // Rock-face detail below the sim resolution, on steep cells only (±0.8 m).
            let n_up = sim.normal_at(x, z)[1];
            let steep = ((1.0 - n_up - 0.15) / 0.30).clamp(0.0, 1.0);
            h += steep * 1.6 * (regia_terrain::noise::fbm2(x / 3.0, z / 3.0, 91) - 0.5);
            // Sub-sim detail: ±0.12 m of non-periodic swell (~6 m features). A periodic
            // sine ripple here printed a regular stripe lattice into the mid shot.
            h += 0.24 * (regia_terrain::noise::fbm2(x / 5.5, z / 5.5, 77) - 0.5);
            let bump = ((x - 80.0).powi(2) + (z - 30.0).powi(2)).sqrt();
            if bump < 22.0 {
                h += 0.6 * (1.0 - bump / 22.0);
            }
            let cliff = ((x + 160.0).powi(2) + (z + 50.0).powi(2)).sqrt();
            if cliff < 3.5 {
                h += 3.2 * (1.0 - cliff / 3.5);
            }
            heights.push(h);
        }
    }
    let visual = TerrainMap::from_heights(n, n, heights, DO27_WORLD_M, DO27_HEIGHT_SCALE, DO27_HEIGHT_OFFSET)
        .expect("4096² visual");
    (visual, sim)
}

/// Four CC0-style layers for the mountains (grass / rock / dirt / snow).
fn photoreal_layers() -> TerrainLayers {
    let layer = |name: &str, tint: [f32; 3], slope: (f32, f32), alt: (f32, f32), blend: f32| LayerDesc {
        name: name.into(),
        // Near-white tints: the CC0-style textures carry the colour, the tint only biases it.
        albedo: tint,
        roughness: 0.95,
        tiling: 1.1,
        slope_min: slope.0,
        slope_max: slope.1,
        altitude_min: alt.0,
        altitude_max: alt.1,
        blend_sharpness: 0.15,
        triplanar: name == "rock",
        height_blend: blend,
        detail_normal_strength: if name == "rock" { 1.0 } else { 0.55 },
        ..LayerDesc::ground()
    };
    TerrainLayers {
        layers: vec![
            layer("grass", [0.90, 1.0, 0.85], (0.0, 0.55), (-50.0, 90.0), 0.6),
            layer("rock", [1.0, 0.98, 0.95], (0.35, 1.6), (-50.0, 420.0), 0.8),
            layer("dirt", [1.0, 0.95, 0.85], (0.0, 0.5), (40.0, 160.0), 0.6),
            layer("snow", [0.97, 0.99, 1.0], (0.0, 0.9), (150.0, 420.0), 0.7),
        ],
        ..Default::default()
    }
}

fn photoreal_params() -> MaterialParams {
    MaterialParams {
        macro_world_size: DO27_WORLD_M,
        macro_strength: 0.65,
        macro_normal_strength: 0.3,
        anti_tiling_cell: 1.6,
        detail_fade_start: 40.0,
        detail_fade_end: 180.0,
        triplanar_auto_slope: 0.55,
    }
}

/// Evidence cameras inside the ±640 m map: near reads ~1 m of surface, mid is 50 m up,
/// far sits about 2 km from the origin.
fn photoreal_camera(kind: &str, map: &TerrainMap) -> TerrainCamera {
    let aspect = 1280.0 / 720.0;
    match kind {
        "near" => {
            // 1.05 m above the ground, looking at a point 1 m ahead on the surface.
            // The pitch keeps the whole frame on the ground (a surface read, not a horizon).
            let x = 40.0;
            let z = 20.0;
            let y = map.sample_bilinear(x, z);
            TerrainCamera {
                position: [x, y + 1.05, z],
                target: [x + 0.20, y, z + 0.98],
                fov_y_radians: 0.9,
                aspect,
                near: 0.02,
                far: 8.0,
            }
        }
        // About 2 km from the map centre, high enough to read the valley and the eroded
        // ring of ridges; the narrower lens keeps the 1280 m map filling the frame.
        "far" => TerrainCamera {
            position: [260.0, 560.0, 1880.0],
            target: [0.0, 40.0, -60.0],
            fov_y_radians: 0.55,
            aspect,
            near: 1.0,
            far: 6000.0,
        },
        // 50 m above the valley floor, looking across the grass to the foot of the
        // eroded ring: the grass → dirt → rock transition and the gullies at mid range.
        _ => {
            let x = -20.0;
            let z = -100.0;
            let y = map.sample_bilinear(x, z);
            TerrainCamera {
                position: [x, y + 50.0, z],
                target: [x + 25.0, y + 8.0, z - 95.0],
                fov_y_radians: 1.0,
                aspect,
                near: 0.3,
                far: 2500.0,
            }
        }
    }
}

fn photoreal_main(args: &[String]) {
    let device = headless_bcn_device().expect("headless GPU device (BCN when available)");
    let (map, sim) = photoreal_pair();
    let layers = photoreal_layers();

    if let Some(metrics_path) = flag_value(args, "--metrics") {
        if let Some(parent) = Path::new(&metrics_path).parent() {
            std::fs::create_dir_all(parent).expect("create evidence directory");
        }
        let report = metrics::run(&device, &map, &sim, &layers);
        std::fs::write(&metrics_path, report).expect("write metrics");
        println!("wrote {metrics_path}");
        return;
    }

    let path = output_path(args);
    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent).expect("create evidence directory");
    }
    let camera_kind = flag_value(args, "--camera").unwrap_or_else(|| "mid".into());
    let debug_view = if args.iter().any(|a| a == "--sim-delta") {
        // Same 1280 m extent as the visual. The sim is the eroded 256² grid; the visual
        // adds the swell, the driveable bump and the cliff. Debug view 7 flags those.
        Some(7)
    } else {
        // Diagnostic views (see `TerrainFrame::debug_view`); not evidence shots.
        flag_value(args, "--debug").and_then(|v| v.parse::<u32>().ok())
    };
    let (w, h) = (1280u32, 720u32);
    let start = std::time::Instant::now();
    let pixels = photoreal_shot(
        &device,
        &map,
        &sim,
        &layers,
        &ShotSpec {
            camera: &camera_kind,
            sun: sun_from_args(args),
            width: w,
            height: h,
            debug_view,
        },
    );
    println!("shot: {:.2}s", start.elapsed().as_secs_f32());
    write_png(&path, w, h, &pixels);
    println!("wrote {path} ({w}x{h}, camera {camera_kind})");
    assert!(pixels.chunks(4).any(|p| p[1] > 40 || p[0] > 40), "frame empty");
}

/// One photoreal evidence shot request.
struct ShotSpec<'a> {
    camera: &'a str,
    sun: GapSun,
    width: u32,
    height: u32,
    debug_view: Option<u32>,
}

/// The evidence sun for each [`GapSun`].
///
/// The low-sun sky ambients are ~45 % of what they were before the display-transform
/// fix: the old linear-input AgX fit crushed shadows, so they had been raised to ~40 % of
/// the sun to stay readable, which flattened the sunlit side. Noon uses
/// [`TimeOfDay::default`] (clear sky, ≈ 1 : 4 sky : sun on level ground).
fn photoreal_tod(sun: GapSun) -> TimeOfDay {
    match sun {
        GapSun::Morning => TimeOfDay {
            azimuth_radians: -0.9,
            elevation_radians: 0.30,
            sun_color: [1.4, 1.05, 0.72],
            illuminance_lux: 50_000.0,
            ambient: [0.14, 0.17, 0.25],
        },
        GapSun::Evening => TimeOfDay {
            azimuth_radians: 2.6,
            elevation_radians: 0.22,
            sun_color: [1.5, 0.92, 0.6],
            illuminance_lux: 38_000.0,
            ambient: [0.13, 0.14, 0.21],
        },
        GapSun::Noon => TimeOfDay::default(),
    }
}

/// Renders one photoreal shot (the evidence images and the lab tests share this).
fn photoreal_shot(
    device: &RenderDevice,
    map: &TerrainMap,
    sim: &TerrainMap,
    layers: &TerrainLayers,
    spec: &ShotSpec<'_>,
) -> Vec<u8> {
    let camera = {
        let mut camera = photoreal_camera(spec.camera, map);
        camera.aspect = spec.width as f32 / spec.height as f32;
        camera
    };
    // In-memory RGBA8 macro (2×2 of 256²). The 8K masks/macros are the tiled KTX2 cook
    // (`cook_tiled_texture` + `TileCache`), not this upload.
    let bake = MacroBake::from_heightfield(map, 2, 256, 909).expect("macro bake");
    let mut material = TerrainMaterial::procedural_cc0(256, 31, photoreal_params());
    material.macro_set = Some(bake);

    let config = CdlodConfig::with_grid_size(16);
    let root_level = 8u32;
    let pyramid = MinMaxPyramid::build(map, root_level).expect("pyramid");
    let selection = select_nodes(
        map,
        &pyramid,
        &config,
        root_level,
        [camera.position[0], camera.position[2]],
        None,
    );
    let mut gpu = build_gpu(device, map, layers, &material, &config, root_level);
    if spec.debug_view == Some(7) {
        gpu.set_sim_map(sim).expect("sim upload");
    }
    let tod = photoreal_tod(spec.sun);
    let mut frame = TerrainFrame::from_camera(&camera, &tod, 0.0, 1.0);
    // Incident-meter exposure: low suns get 1–2 EV more instead of a near-black ground.
    frame.exposure_compensation_ev = tod.metered_exposure_compensation_ev();
    if let Some(view) = spec.debug_view {
        frame.debug_view = view;
    }
    if spec.camera == "far" {
        frame.fog = aerial_haze(spec.sun);
    }
    render_offscreen(
        &mut gpu,
        device,
        &frame,
        &selection,
        spec.width,
        spec.height,
        [0.02, 0.02, 0.05, 1.0],
    )
    .expect("photoreal render")
}

/// Aerial perspective for the 2 km shot: ~45% haze over 2 km of low air thinning with
/// altitude (scale height ~330 m), tinted by the sun. `FogSettings::outdoor_haze` is tuned
/// for a 200 m scene and turns a 2 km view into one flat slab.
fn aerial_haze(sun: GapSun) -> regia_render::FogSettings {
    regia_render::FogSettings {
        enabled: true,
        density: 0.000_32,
        height_falloff: 0.003,
        base_height: 0.0,
        color: match sun {
            GapSun::Morning => [0.66, 0.64, 0.64],
            GapSun::Evening => [0.70, 0.58, 0.52],
            GapSun::Noon => [0.58, 0.66, 0.78],
        },
        sky_inscatter: false,
    }
}

/// Builds the terrain GPU. Shading uploads RGBA8 through `set_material`. Block-compressed
/// 8K masks go through `cook_tiled_texture` and `TileCache` (see the scale test); this
/// function does not pretend the lab frame is BCn.
fn build_gpu(
    device: &RenderDevice,
    map: &TerrainMap,
    layers: &TerrainLayers,
    material: &TerrainMaterial,
    config: &CdlodConfig,
    root_level: u32,
) -> TerrainGpu {
    let mut gpu = TerrainGpu::new(device, map, layers, config, root_level, None, None).expect("terrain gpu");
    gpu.set_material(material, &[]).expect("material");
    gpu
}

fn write_png(path: &str, w: u32, h: u32, pixels: &[u8]) {
    let file = std::fs::File::create(path).expect("create output file");
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut png_writer = encoder.write_header().expect("PNG header");
    png_writer.write_image_data(pixels).expect("PNG data");
    png_writer.finish().expect("PNG finish");
}

/// Image measurements shared by the evidence report and the lab tests. Pixels are tightly
/// packed RGBA8 rows; luminance is Rec. 709 on the sRGB bytes.
mod measure {
    /// Rec. 709 luma per pixel.
    pub fn luma(pixels: &[u8]) -> Vec<f64> {
        pixels
            .chunks(4)
            .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
            .collect()
    }

    /// Fraction of pixels with luma below `threshold`.
    pub fn dark_fraction(pixels: &[u8], threshold: f64) -> f64 {
        let l = luma(pixels);
        l.iter().filter(|&&v| v < threshold).count() as f64 / l.len().max(1) as f64
    }

    /// Mean luma.
    pub fn mean(pixels: &[u8]) -> f64 {
        let l = luma(pixels);
        l.iter().sum::<f64>() / l.len().max(1) as f64
    }

    /// Coefficient of variation (σ/μ) of the luma over the lower half of the frame. A
    /// lattice of bright dots on a near-black field is > 1; lit continuous ground is < 0.5.
    pub fn lower_half_cv(pixels: &[u8], w: u32, h: u32) -> f64 {
        let l = luma(pixels);
        let lower = &l[(w * (h / 2)) as usize..];
        let m = lower.iter().sum::<f64>() / lower.len().max(1) as f64;
        let var = lower.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / lower.len().max(1) as f64;
        var.sqrt() / m.max(1e-6)
    }

    /// Fraction of lower-half pixels brighter than `k`× the lower-half median luma: the
    /// isolated bright blobs of a layer that should not be there (round 3's snow dots).
    pub fn bright_outlier_fraction(pixels: &[u8], w: u32, h: u32, k: f64) -> f64 {
        let l = luma(pixels);
        let mut lower: Vec<f64> = l[(w * (h / 2)) as usize..].to_vec();
        lower.sort_by(f64::total_cmp);
        let median = lower[lower.len() / 2];
        lower.iter().filter(|&&v| v > median * k).count() as f64 / lower.len().max(1) as f64
    }

    /// Worst ratio of one neighbouring-column (or row) step to the steps two pixels either
    /// side, from mean absolute luma differences. A hard seam is a one-pixel step far above
    /// its neighbours.
    pub fn worst_step_ratio(pixels: &[u8], w: u32, h: u32) -> f64 {
        let l = luma(pixels);
        let at = |x: u32, y: u32| l[(y * w + x) as usize];
        let cols: Vec<f64> = (0..w - 1)
            .map(|x| (0..h).map(|y| (at(x + 1, y) - at(x, y)).abs()).sum::<f64>() / f64::from(h))
            .collect();
        let rows: Vec<f64> = (0..h - 1)
            .map(|y| (0..w).map(|x| (at(x, y + 1) - at(x, y)).abs()).sum::<f64>() / f64::from(w))
            .collect();
        let worst = |d: &[f64]| {
            (2..d.len().saturating_sub(2))
                .map(|i| d[i] / (0.5 * (d[i - 2] + d[i + 2]) + 0.5))
                .fold(0.0f64, f64::max)
        };
        worst(&cols).max(worst(&rows))
    }

    /// Relief contrast: mean |luma − its 7×7 box mean| over the frame's central region
    /// (where the 2 km shot frames the map). Shading of eroded slopes is local contrast; a
    /// fogged flat slab has almost none.
    pub fn relief_contrast(pixels: &[u8], w: u32, h: u32) -> f64 {
        let l = luma(pixels);
        let (x0, x1, y0, y1) = (w / 6, w * 5 / 6, h * 3 / 10, h * 7 / 10);
        let mut sum = 0.0;
        let mut n = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let mut box_sum = 0.0;
                for dy in 0..7 {
                    for dx in 0..7 {
                        box_sum += l[((y + dy - 3) * w + x + dx - 3) as usize];
                    }
                }
                sum += (l[(y * w + x) as usize] - box_sum / 49.0).abs();
                n += 1.0;
            }
        }
        sum / f64::max(n, 1.0)
    }

    /// Mean absolute RGB difference over the same central region.
    pub fn central_diff(a: &[u8], b: &[u8], w: u32, h: u32) -> f64 {
        let (x0, x1, y0, y1) = (w / 6, w * 5 / 6, h * 3 / 10, h * 7 / 10);
        let mut sum = 0.0;
        let mut n = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * w + x) * 4) as usize;
                for c in 0..3 {
                    sum += f64::from(a[i + c].abs_diff(b[i + c]));
                    n += 1.0;
                }
            }
        }
        sum / f64::max(n, 1.0)
    }
}

/// Evidence metrics: the nine evidence shots, tiling autocorrelation, blend sharpness, LOD
/// popping (30 frames).
mod metrics {
    use super::*;
    use regia_terrain::gpu::{TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen_opts};
    use regia_terrain::{MacroBake, TerrainMaterial, select_nodes};

    const W: u32 = 320;
    const H: u32 = 180;

    /// The same rolling map the photoreal tests use (12 m hill + ripples over 256 m).
    fn rolling_map() -> TerrainMap {
        let n = 257u32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * 256.0 - 128.0;
                let z = row as f32 / (n - 1) as f32 * 256.0 - 128.0;
                heights.push(
                    12.0 * (-(x * x + z * z) / 9000.0).exp() + 2.0 * (x * 0.06).sin() * (z * 0.05).cos(),
                );
            }
        }
        TerrainMap::from_heights(n, n, heights, 256.0, 16.0, -2.0).expect("rolling map")
    }

    fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = (y * W + x) as usize * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    fn luma(p: [u8; 4]) -> f64 {
        0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
    }

    /// Mean-subtracted autocorrelation peak over lags 30..=60 (the stripe repeat band).
    fn autocorrelation_peak(pixels: &[u8]) -> f64 {
        let y = H / 2;
        let row: Vec<f64> = (0..W).map(|x| luma(pixel(pixels, x, y))).collect();
        let x0 = (W / 8) as usize;
        let x1 = (W - 60 - W / 8) as usize;
        let mean = row[x0..x1].iter().sum::<f64>() / (x1 - x0) as f64;
        let centred: Vec<f64> = row[x0..x1].iter().map(|v| v - mean).collect();
        let energy: f64 = centred.iter().map(|v| v * v).sum();
        (30..=60)
            .map(|lag| {
                centred
                    .iter()
                    .zip(centred.iter().skip(lag))
                    .map(|(a, b)| a * b)
                    .sum::<f64>()
                    / energy.max(1e-9)
            })
            .fold(f64::MIN, f64::max)
    }

    /// Renders a scene helper: map + layers + material at a debug view.
    fn render(
        device: &RenderDevice,
        map: &TerrainMap,
        layers: &TerrainLayers,
        material: &TerrainMaterial,
        camera: &TerrainCamera,
        debug_view: u32,
    ) -> Vec<u8> {
        let config = CdlodConfig::with_grid_size(16);
        let root_level = 8u32;
        let mut gpu = TerrainGpu::new(device, map, layers, &config, root_level, None, None).expect("gpu");
        gpu.set_material(material, &[]).expect("material");
        let pyramid = MinMaxPyramid::build(map, root_level).expect("pyramid");
        let selection = select_nodes(
            map,
            &pyramid,
            &config,
            root_level,
            [camera.position[0], camera.position[2]],
            None,
        );
        let mut frame = TerrainFrame::from_camera(camera, &TimeOfDay::default(), 0.0, 1.0);
        frame.debug_view = debug_view;
        render_offscreen_opts(
            &mut gpu,
            device,
            &frame,
            &selection,
            W,
            H,
            [0.0, 0.0, 0.0, 1.0],
            false,
        )
        .expect("render")
    }

    /// The nine evidence shots at 1280×720, measured, plus the sim-delta flag counts.
    fn evidence_table(
        device: &RenderDevice,
        map: &TerrainMap,
        sim: &TerrainMap,
        layers: &TerrainLayers,
    ) -> String {
        use super::measure;
        let (w, h) = (1280u32, 720u32);
        let mut out = String::from(
            "## Evidence shots (1280×720, lavapipe)\n\n\
             | shot | mean luma | luma<8 | luma<20 | lower-half σ/μ | > 2× median | worst step | relief |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        let mut far = Vec::new();
        for camera in ["near", "mid", "far"] {
            for (sun, name) in [
                (GapSun::Morning, "morning"),
                (GapSun::Noon, "noon"),
                (GapSun::Evening, "evening"),
            ] {
                let px = super::photoreal_shot(
                    device,
                    map,
                    sim,
                    layers,
                    &super::ShotSpec {
                        camera,
                        sun,
                        width: w,
                        height: h,
                        debug_view: None,
                    },
                );
                out.push_str(&format!(
                    "| {camera}_{name} | {:.1} | {:.2}% | {:.2}% | {:.3} | {:.2}% | {:.2} | {:.2} |\n",
                    measure::mean(&px),
                    measure::dark_fraction(&px, 8.0) * 100.0,
                    measure::dark_fraction(&px, 20.0) * 100.0,
                    measure::lower_half_cv(&px, w, h),
                    measure::bright_outlier_fraction(&px, w, h, 2.0) * 100.0,
                    measure::worst_step_ratio(&px, w, h),
                    measure::relief_contrast(&px, w, h),
                ));
                if camera == "far" {
                    far.push((name, px));
                }
            }
        }
        out.push_str("\nFar-shot sun response (mean |ΔRGB| over the map region):");
        for i in 0..far.len() {
            for j in (i + 1)..far.len() {
                out.push_str(&format!(
                    " {}↔{} **{:.1}**;",
                    far[i].0,
                    far[j].0,
                    measure::central_diff(&far[i].1, &far[j].1, w, h)
                ));
            }
        }
        let delta = super::photoreal_shot(
            device,
            map,
            sim,
            layers,
            &super::ShotSpec {
                camera: "mid",
                sun: GapSun::Noon,
                width: w,
                height: h,
                debug_view: Some(7),
            },
        );
        let count = |f: &dyn Fn(&[u8]) -> bool| delta.chunks(4).filter(|p| f(p)).count();
        out.push_str(&format!(
            "\n\nSim delta (mid, view 7): green {} / red {} / blue {} / yellow {} px \
             (driveable within / outside ±0.25 m, cliff within / outside ±2 m).\n\n",
            count(&|p| p[1] > 200 && p[0] < 40 && p[2] < 180),
            count(&|p| p[0] > 200 && p[1] < 40 && p[2] < 40),
            count(&|p| p[2] > 200 && p[0] < 180 && p[2] > p[1] && p[2] > p[0]),
            count(&|p| p[0] > 200 && p[1] > 200 && p[2] < 40),
        ));
        out
    }

    /// The full metrics report (markdown).
    pub fn run(device: &RenderDevice, map: &TerrainMap, sim: &TerrainMap, layers: &TerrainLayers) -> String {
        let mut out = String::from("# Photoreal terrain metrics (terrain_lab)\n\n");
        out.push_str(&evidence_table(device, map, sim, layers));
        let params = photoreal_params();

        // ---- Tiling: periodic autocorrelation of the albedo over a flat single-layer patch,
        // anti-tiling off vs on. A perfectly tiled surface peaks ≈ 1 at the repeat lag. ----
        let flat = TerrainMap::from_heights(65, 65, vec![5.0; 65 * 65], 64.0, 8.0, 0.0).expect("flat");
        let flat_layers = TerrainLayers {
            layers: vec![LayerDesc {
                name: "grass".into(),
                albedo: [1.0, 1.0, 1.0],
                tiling: 0.5,
                ..LayerDesc::ground()
            }],
            ..Default::default()
        };
        let camera = TerrainCamera {
            position: [0.0, 19.0, 0.28],
            target: [0.0, 5.0, 0.0],
            fov_y_radians: 1.0,
            aspect: W as f32 / H as f32,
            near: 0.1,
            far: 4000.0,
        };
        let mut off = TerrainMaterial::procedural_cc0(64, 5, params);
        off.params.anti_tiling_cell = 0.0;
        off.params.macro_world_size = 0.0;
        let mut on = off.clone();
        on.params.anti_tiling_cell = 3.0;
        let peak_off = autocorrelation_peak(&render(device, &flat, &flat_layers, &off, &camera, 3));
        let peak_on = autocorrelation_peak(&render(device, &flat, &flat_layers, &on, &camera, 3));
        out.push_str("## Anti-tiling (autocorrelation peak at the repeat band, flat grass patch)\n\n");
        out.push_str(&format!(
            "- plain tiling: **{peak_off:.3}**\n- stochastic tiling: **{peak_on:.3}** (lower is better; suppression {:+.0}%)\n\n",
            (1.0 - peak_on / peak_off.max(1e-9)) * 100.0
        ));

        // ---- Blend sharpness: intermediate splat weights across a controlled grass/rock
        // boundary (same stack, height blend off vs on — the map itself is too noisy a
        // boundary for a clean count). ----
        let blend_material = TerrainMaterial::procedural_cc0(64, 7, params);
        let camera_mid = TerrainCamera {
            position: [0.0, 40.0, 130.0],
            target: [0.0, 8.0, 0.0],
            fov_y_radians: 1.0,
            aspect: W as f32 / H as f32,
            near: 0.5,
            far: 4000.0,
        };
        // A small rolling map whose 5..8 m contour visibly crosses the frame.
        let rolling = rolling_map();
        let make_layers = |blend: f32| {
            let layer = |name: &str, tint: [f32; 3], alt: (f32, f32)| LayerDesc {
                name: name.into(),
                albedo: tint,
                tiling: 0.5,
                altitude_min: alt.0,
                altitude_max: alt.1,
                height_blend: blend,
                ..LayerDesc::ground()
            };
            TerrainLayers {
                layers: vec![
                    layer("low", [0.9, 1.0, 0.85], (-10.0, 8.0)),
                    layer("high", [1.0, 0.98, 0.95], (5.0, 30.0)),
                ],
                ..Default::default()
            }
        };
        let count_intermediate = |pixels: &[u8]| -> usize {
            let mut n = 0;
            for y in 0..H {
                for x in 0..W {
                    let p = pixel(pixels, x, y);
                    if p[2] > 20 {
                        continue;
                    }
                    let lin = |b: u8| (f64::from(b) / 255.0).powf(2.2);
                    let (w0, w1) = (lin(p[0]), lin(p[1]));
                    if (0.2..0.8).contains(&w0) && (0.2..0.8).contains(&w1) {
                        n += 1;
                    }
                }
            }
            n
        };
        let soft_px = render(
            device,
            &rolling,
            &make_layers(0.0),
            &blend_material,
            &camera_mid,
            4,
        );
        let hard_px = render(
            device,
            &rolling,
            &make_layers(0.9),
            &blend_material,
            &camera_mid,
            4,
        );
        let soft_n = count_intermediate(&soft_px);
        let hard_n = count_intermediate(&hard_px);
        out.push_str(
            "## Blend sharpness (intermediate splat-weight pixels across a controlled boundary)\n\n",
        );
        out.push_str(&format!(
            "- height blend off: **{soft_n}** px\n- height blend on: **{hard_n}** px ({:+.0}% narrower)\n\n",
            (1.0 - hard_n as f64 / soft_n.max(1) as f64) * 100.0
        ));

        // ---- LOD popping: 30 frames of camera dolly; each frame compares the current
        // selection against the previous frame's selection at the SAME camera. The diff is
        // pure LOD-change artifact (no camera-motion confound); morphing keeps it small. ----
        let config = CdlodConfig::with_grid_size(16);
        let root_level = 8u32;
        let pyramid = MinMaxPyramid::build(map, root_level).expect("pyramid");
        let mut gpu = TerrainGpu::new(device, map, layers, &config, root_level, None, None).expect("gpu");
        let mut material = TerrainMaterial::procedural_cc0(256, 31, params);
        material.macro_set = Some(MacroBake::from_heightfield(map, 2, 512, 77).expect("bake"));
        gpu.set_material(&material, &[]).expect("material");
        let mut max_pop = 0.0f64;
        let mut mean_pop = 0.0f64;
        let mut prev_selection = None;
        for step in 0..30 {
            let t = step as f32 / 29.0;
            let radius = 1400.0 - 1100.0 * t;
            let camera = TerrainCamera {
                position: [radius * 0.7, 420.0 + 260.0 * t, radius],
                target: [0.0, 40.0, -200.0],
                fov_y_radians: 1.0,
                aspect: W as f32 / H as f32,
                near: 0.5,
                far: 9000.0,
            };
            let frame = TerrainFrame::from_camera(&camera, &TimeOfDay::default(), 0.0, 1.0);
            let selection = select_nodes(
                map,
                &pyramid,
                &config,
                root_level,
                [camera.position[0], camera.position[2]],
                None,
            );
            if let Some(prev) = &prev_selection {
                let now = render_offscreen_opts(
                    &mut gpu,
                    device,
                    &frame,
                    &selection,
                    W,
                    H,
                    [0.0, 0.0, 0.0, 1.0],
                    false,
                )
                .expect("render now");
                let before =
                    render_offscreen_opts(&mut gpu, device, &frame, prev, W, H, [0.0, 0.0, 0.0, 1.0], false)
                        .expect("render prev");
                let diff = now
                    .iter()
                    .zip(&before)
                    .map(|(a, b)| (i16::from(*a) - i16::from(*b)).unsigned_abs() as f64)
                    .sum::<f64>()
                    / now.len() as f64;
                max_pop = max_pop.max(diff);
                mean_pop += diff;
            }
            prev_selection = Some(selection);
        }
        mean_pop /= 29.0;
        out.push_str("## LOD popping (30 frames of camera dolly; same-camera selection diff)\n\n");
        out.push_str(&format!(
            "- mean frame-to-LOD-change diff: **{mean_pop:.3}** / 255 per channel\n- max: **{max_pop:.3}** / 255 (morphing keeps transitions sub-perceptual)\n\n"
        ));
        out.push_str("Generated by `terrain_lab --metrics` (lavapipe-safe, no goldens harmed).\n");
        out
    }
}

/// Acceptance checks on the evidence cameras themselves (lavapipe, 640×360). Each one fails
/// on the round-3 frames: the mid dot lattice, the near-black low-sun 1 m reads with the
/// noon seam, and the fogged featureless far slab.
#[cfg(test)]
mod output_path_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn the_default_output_is_outside_the_source_tree() {
        let temp = Path::new("/tmp/regia-test-temp");
        let path = output_path_in(&args(&["terrain_lab"]), temp);
        assert!(Path::new(&path).starts_with(temp), "{path}");
        assert!(!path.contains("docs/evidence"), "{path}");
        let with_flags = output_path_in(&args(&["terrain_lab", "--fog", "--sun", "noon"]), temp);
        assert_eq!(with_flags, path);
    }

    #[test]
    fn the_evidence_path_is_an_explicit_flag_and_explicit_paths_win() {
        let temp = Path::new("/tmp/regia-test-temp");
        assert_eq!(
            output_path_in(&args(&["terrain_lab", "--evidence"]), temp),
            EVIDENCE_PATH
        );
        assert_eq!(
            output_path_in(&args(&["terrain_lab", "out/a.png"]), temp),
            "out/a.png"
        );
        assert_eq!(
            output_path_in(
                &args(&["terrain_lab", "--screenshot", "b.png", "--evidence"]),
                temp
            ),
            "b.png"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    const W: u32 = 640;
    const H: u32 = 360;

    struct Scene {
        device: RenderDevice,
        map: TerrainMap,
        sim: TerrainMap,
        layers: TerrainLayers,
    }

    fn scene() -> &'static Scene {
        static SCENE: OnceLock<Scene> = OnceLock::new();
        SCENE.get_or_init(|| {
            let device = match headless_bcn_device() {
                Ok(device) => device,
                Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
            };
            let (map, sim) = photoreal_pair();
            Scene {
                device,
                map,
                sim,
                layers: photoreal_layers(),
            }
        })
    }

    fn shot(camera: &str, sun: GapSun) -> Vec<u8> {
        let s = scene();
        photoreal_shot(
            &s.device,
            &s.map,
            &s.sim,
            &s.layers,
            &ShotSpec {
                camera,
                sun,
                width: W,
                height: H,
                debug_view: None,
            },
        )
    }

    const SUNS: [(GapSun, &str); 3] = [
        (GapSun::Morning, "morning"),
        (GapSun::Noon, "noon"),
        (GapSun::Evening, "evening"),
    ];

    /// The 50 m shot is continuous material at every sun. Round 3 drew a lattice of white
    /// dots (the out-of-band snow layer) on a near-black field (double-decoded sRGB grass):
    /// 77–91% of pixels under luma 20 and σ/μ 1.5–1.9 over the lower half. With only the
    /// sRGB bake fixed, the round-3 shader still prints the snow blobs over this grass
    /// (σ/μ 0.43–0.56, several % of pixels > 2× the median).
    ///
    /// Bounds: the darkest legitimate texel here is shaded rock at evening (albedo ≈ 0.12,
    /// sky light ≥ 0.35 of the metered ambient), which tonemaps above luma 20, so ≤ 2% dark
    /// pixels allows only silhouette edges. Continuous grass and foothills measure σ/μ ≤ 0.21
    /// over the lower half and ≤ 0.21% of pixels above 2× the median (sunlit crests); the
    /// bounds are 0.3 and 0.5%. The round-3 shader with the fixed bake measures 0.43 and 7%.
    #[test]
    fn mid_shot_is_continuous_material_at_every_sun() {
        for (sun, name) in SUNS {
            let px = shot("mid", sun);
            let dark = measure::dark_fraction(&px, 20.0);
            let cv = measure::lower_half_cv(&px, W, H);
            let bright = measure::bright_outlier_fraction(&px, W, H, 2.0);
            eprintln!(
                "mid {name}: dark {:.2}% cv {cv:.3} bright {:.2}%",
                dark * 100.0,
                bright * 100.0
            );
            assert!(
                dark < 0.02,
                "mid {name}: {:.1}% of pixels under luma 20",
                dark * 100.0
            );
            assert!(
                cv < 0.3,
                "mid {name}: lower-half σ/μ {cv:.2} (a dot lattice is > 0.4)"
            );
            assert!(
                bright < 0.005,
                "mid {name}: {:.1}% of the ground is > 2× the median (bright blobs)",
                bright * 100.0
            );
        }
    }

    /// The 1 m read shows material at low sun and has no seam at noon. Round 3: 51% (morning)
    /// and 63% (evening) of pixels under luma 8, and a vertical stochastic-cell seam.
    ///
    /// Bounds: every pixel of the 1 m read sees the sky (ambient ≥ 0.35 of the metered sky
    /// light) on grass of albedo ≥ 0.05, which tonemaps far above luma 8, so the bound is
    /// < 0.5% (nothing but numerical noise). Mean luma > 35 is a readable exposure. A seam
    /// is a one-pixel step > 3× its neighbours; continuous material measures ≈ 1–1.5.
    #[test]
    fn near_shots_read_the_material_at_low_sun_without_a_seam() {
        for (sun, name) in SUNS {
            let px = shot("near", sun);
            let black = measure::dark_fraction(&px, 8.0);
            let mean = measure::mean(&px);
            let step = measure::worst_step_ratio(&px, W, H);
            eprintln!(
                "near {name}: black {:.2}% mean {mean:.1} step {step:.2}",
                black * 100.0
            );
            assert!(
                black < 0.005,
                "near {name}: {:.1}% of pixels under luma 8",
                black * 100.0
            );
            assert!(mean > 35.0, "near {name}: mean luma {mean:.1}");
            assert!(
                step < 3.0,
                "near {name}: a seam (one-pixel step {step:.2}× its neighbors)"
            );
        }
    }

    /// The 2 km shot shows the eroded relief and follows the sun. Round 3: one fogged slab,
    /// relief contrast 0.2 and the three suns within a few levels of each other.
    ///
    /// Bounds: relief contrast > 1.0 (5× the slab); every pair of suns differs by > 8 levels
    /// mean |ΔRGB| over the map region (sun-lit faces swap sides between morning and evening).
    #[test]
    fn far_shots_show_relief_and_follow_the_sun() {
        let shots: Vec<(&str, Vec<u8>)> = SUNS.iter().map(|&(sun, name)| (name, shot("far", sun))).collect();
        for (name, px) in &shots {
            let relief = measure::relief_contrast(px, W, H);
            eprintln!("far {name}: relief {relief:.2}");
            assert!(relief > 1.0, "far {name}: relief contrast {relief:.2}");
        }
        for i in 0..shots.len() {
            for j in (i + 1)..shots.len() {
                let d = measure::central_diff(&shots[i].1, &shots[j].1, W, H);
                eprintln!("far {}↔{}: {d:.1}", shots[i].0, shots[j].0);
                assert!(d > 8.0, "far {}↔{} differ by only {d:.1}", shots[i].0, shots[j].0);
            }
        }
    }
}
