// SPDX-License-Identifier: MIT-0
//! DO27-shaped terrain look shots through the runtime frame (`--do27-look`).
//!
//! Two procedural stand-ins in the DO27 data shape — a 4096² visual heightfield over
//! 1280 m (0.31 m texels), a photoreal splat material, a planet preset — drawn through
//! [`TerrainSharedDraw`] inside [`FrameRenderer`] on Desktop High at native scale, the
//! configuration DO27's shell renders. The cameras frame the same kinds of view as the UE
//! parity plates (`docs/evidence/parity/2026-10-07/`):
//!
//! - `bore_twelve_*` (Europa, airless): an ice plain cut by straight trenches with steep
//!   walls running at an angle to the grid, double ridges, a crevasse, fine crust relief;
//! - `switchbacks_*` (Mars): sedimentary mesas with stepped risers, talus, a dusty floor.
//!
//! ```text
//! cargo run -p terrain_lab -- --do27-look docs/evidence/terrain-look/after
//! cargo run -p terrain_lab -- --do27-look docs/evidence/terrain-look/after --shot switchbacks_mid
//! ```
//!
//! These maps are stand-ins: DO27's own cook (six layers, its textures) is not in this
//! repository. They reproduce the plates' failure shapes (diagonal walls, mesa crests,
//! 1 m dust) so the engine fix can be judged on them.

use std::path::Path;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::gpu::SkyAmbient;
use regia_terrain::noise::{fbm2, value_noise2};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerStyle, LayerTextures, MacroBake, MaterialParams, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

/// DO27 world extent (sim 256² and visual 4096² cover the same square).
const WORLD_M: f32 = 1280.0;
/// Visual heightfield edge in samples (0.3125 m texels).
const VISUAL_N: u32 = 4096;
/// Evidence frame size.
const W: u32 = 960;
/// Evidence frame size.
const H: u32 = 540;

/// The two stand-in planets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Planet {
    Europa,
    Mars,
}

/// One evidence camera.
pub(crate) struct Shot {
    name: &'static str,
    planet: Planet,
    /// Eye XZ and height above the ground under it.
    eye: [f32; 2],
    eye_up: f32,
    /// Target XZ and height above the ground under it.
    target: [f32; 2],
    target_up: f32,
    /// Horizontal field of view, degrees (the plates' `fov_horizontal_deg`).
    hfov_deg: f32,
}

const SHOTS: [Shot; 6] = [
    // High oblique over the plain: both trenches run diagonally away to the right.
    Shot {
        name: "bore_twelve_wide",
        planet: Planet::Europa,
        eye: [-253.0, -2.0],
        eye_up: 55.0,
        target: [61.6, 22.7],
        target_up: 0.0,
        hfov_deg: 55.0,
    },
    // 14 m up, across the crusty plain to the double ridge, sun behind the camera.
    Shot {
        name: "bore_twelve_mid",
        planet: Planet::Europa,
        eye: [96.0, -234.0],
        eye_up: 14.0,
        target: [200.0, -130.0],
        target_up: 3.0,
        hfov_deg: 50.0,
    },
    // On the trench rim, looking down along the wall into the trench.
    Shot {
        name: "bore_twelve_close",
        planet: Planet::Europa,
        eye: [-6.6, -24.3],
        eye_up: 3.2,
        target: [18.4, -32.2],
        target_up: 0.0,
        hfov_deg: 60.0,
    },
    // High over the mesas.
    Shot {
        name: "switchbacks_wide",
        planet: Planet::Mars,
        eye: [-490.0, 535.0],
        eye_up: 140.0,
        target: [0.0, 60.0],
        target_up: 0.0,
        hfov_deg: 55.0,
    },
    // Valley floor between mesas, looking at a stepped flank.
    Shot {
        name: "switchbacks_mid",
        planet: Planet::Mars,
        eye: [185.0, -265.0],
        eye_up: 6.0,
        target: [330.0, -60.0],
        target_up: 15.0,
        hfov_deg: 50.0,
    },
    // 1.7 m over the dust floor.
    Shot {
        name: "switchbacks_close",
        planet: Planet::Mars,
        eye: [200.0, -250.0],
        eye_up: 1.7,
        target: [196.0, -255.0],
        target_up: 0.0,
        hfov_deg: 60.0,
    },
];

/// A chase-camera shot on `planet` (90° horizontal field of view, 3.5 m boom) for the perf
/// harness; the harness moves the eye every frame.
pub(crate) fn chase_shot(planet: Planet, eye: [f32; 2], target: [f32; 2]) -> Shot {
    Shot {
        name: "chase",
        planet,
        eye,
        eye_up: 3.5,
        target,
        target_up: 1.0,
        hfov_deg: 90.0,
    }
}

/// `--do27-look <dir> [--shot <name>] [--legacy-ambient]`: renders the shots into
/// `<dir>/<name>.png`. The terrain follows the frame's planet preset with no host call
/// (`SharedFrameInfo::sky_ambient`). `--legacy-ambient` forces the pre-2026-10-07 sky
/// light (the frame's flat `ambient_lux` in the zenith blue) for the `before/` shots.
pub fn run(args: &[String]) {
    let dir = args
        .iter()
        .position(|a| a == "--do27-look")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "docs/evidence/terrain-look/after".into());
    let only = args
        .iter()
        .position(|a| a == "--shot")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let legacy_ambient = args.iter().any(|a| a == "--legacy-ambient");
    // `--perf`: GPU pass timings at 2560×1440 over a slowly moving camera instead of stills.
    let perf = args.iter().any(|a| a == "--perf");
    // A/B toggles for the relief features (`--relief off|no-self-shadow|no-strata`).
    let relief = match args
        .iter()
        .position(|a| a == "--relief")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
    {
        Some("off") => TerrainRelief::off(),
        Some("no-self-shadow") => TerrainRelief {
            self_shadow_reach_m: 0.0,
            ..TerrainRelief::default()
        },
        Some("no-near") => TerrainRelief {
            near_detail: 0.0,
            ..TerrainRelief::default()
        },
        Some("no-strata") => TerrainRelief {
            strata_strength: 0.0,
            ..TerrainRelief::default()
        },
        _ => TerrainRelief::default(),
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("terrain_lab: cannot create {dir}: {err}");
        return;
    }
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("terrain_lab: no GPU: {err}");
            return;
        }
    };
    for planet in [Planet::Europa, Planet::Mars] {
        let shots: Vec<&Shot> = SHOTS
            .iter()
            .filter(|s| s.planet == planet && only.as_deref().is_none_or(|o| o == s.name))
            .collect();
        let heightmap = args.iter().any(|a| a == "--heightmap") || args.iter().any(|a| a == "--lod-error");
        if shots.is_empty() && !heightmap {
            continue;
        }
        let started = std::time::Instant::now();
        let map = planet_map(planet);
        println!(
            "{planet:?}: {}² heightfield in {:.1}s",
            map.width(),
            started.elapsed().as_secs_f32()
        );
        if args.iter().any(|a| a == "--lod-error") {
            print_lod_error(&map, planet);
        }
        if heightmap {
            write_heightmap(&map, &Path::new(&dir).join(format!("{planet:?}_heights.png")));
        }
        for shot in shots {
            if perf {
                match perf_shot(&device, &map, shot, relief) {
                    Ok(line) => println!("{line}"),
                    Err(err) => eprintln!("terrain_lab: {}: {err}", shot.name),
                }
                continue;
            }
            let started = std::time::Instant::now();
            match render(&device, &map, shot, legacy_ambient, relief) {
                Ok(pixels) => {
                    let path = Path::new(&dir).join(format!("{}.png", shot.name));
                    crate::write_png(&path.to_string_lossy(), W, H, &pixels);
                    println!(
                        "wrote {} ({:.1}s)",
                        path.display(),
                        started.elapsed().as_secs_f32()
                    );
                }
                Err(err) => eprintln!("terrain_lab: {}: {err}", shot.name),
            }
        }
    }
}

/// Per-level vertical departure of the runtime frame's CDLOD surface from the heightfield
/// (`--lod-error`): all ground, and driveable ground only (slope ≤ 0.6 rad) where scatter sits.
fn print_lod_error(map: &TerrainMap, planet: Planet) {
    let config = CdlodConfig::with_grid_size(regia_terrain::gpu::SHARED_MIN_GRID_SIZE);
    let root = regia_terrain::gpu::full_detail_root_level(map, &config);
    let mips = regia_terrain::HeightMips::build(map);
    let all = regia_terrain::lod_height_error(map, &mips, &config, root, 160, f32::INFINITY);
    let flat = regia_terrain::lod_height_error(map, &mips, &config, root, 160, 0.6f32.tan());
    println!(
        "{planet:?} {}² grid {} root {root}",
        map.width(),
        config.grid_size
    );
    println!("level | cell m | all: max / p99 / rms m | driveable: max / p99 / rms m");
    for (a, f) in all.iter().zip(&flat) {
        println!(
            "{} | {:.3} | {:.3} / {:.3} / {:.3} | {:.3} / {:.3} / {:.3}",
            a.level, a.cell_m, a.max_m, a.p99_m, a.rms_m, f.max_m, f.p99_m, f.rms_m
        );
    }
}

/// Top-down 512² preview of the heights (+X right, +Z down), shot eyes marked.
fn write_heightmap(map: &TerrainMap, path: &Path) {
    let n = 512u32;
    let (lo, hi) = map.min_max_height();
    let mut pixels = Vec::with_capacity((n * n * 4) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = (col as f32 + 0.5) / n as f32 * WORLD_M - WORLD_M * 0.5;
            let z = (row as f32 + 0.5) / n as f32 * WORLD_M - WORLD_M * 0.5;
            let v = ((map.sample_bilinear(x, z) - lo) / (hi - lo).max(1e-3) * 255.0) as u8;
            let mark = SHOTS.iter().any(|s| {
                let near = |p: [f32; 2]| (p[0] - x).abs() < 4.0 && (p[1] - z).abs() < 4.0;
                near(s.eye) || near(s.target)
            });
            if mark {
                pixels.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
    }
    crate::write_png(&path.to_string_lossy(), n, n, &pixels);
}

/// Smooth step `0..1` over `[a, b]`.
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Signed distance of `(x, z)` from the line through `origin` with direction angle `deg`
/// (positive on the left), and the coordinate along it.
fn line_frame(x: f32, z: f32, origin: [f32; 2], deg: f32) -> (f32, f32) {
    let (s, c) = deg.to_radians().sin_cos();
    let dx = x - origin[0];
    let dz = z - origin[1];
    (dx * -s + dz * c, dx * c + dz * s)
}

/// Europa stand-in: rolling ice plain, two straight trenches with 75–80° walls, a double
/// ridge, a jagged crevasse and fine crust, all at angles to the sample grid.
fn europa_height(x: f32, z: f32) -> f32 {
    let mut h = 14.0 * fbm2(x / 260.0, z / 260.0, 11) + 5.0 * fbm2(x / 70.0, z / 70.0, 12);
    // Two parallel trenches at 23° to the grid: 14 m deep, 22 m floors, ~3 m walls.
    for (offset, depth) in [(-40.0f32, 14.0f32), (40.0, 11.0)] {
        let (d, along) = line_frame(x, z, [0.0, offset], 23.0);
        let wobble = 2.5 * (fbm2(along / 60.0, offset, 21) - 0.5);
        let a = (d + wobble).abs();
        let wall = 1.0 - smooth(11.0, 14.0, a);
        let rim = 1.6 * (1.0 - smooth(0.0, 6.0, (a - 15.0).abs()));
        h += -depth * wall + rim;
    }
    // A double ridge at -31°: two 7 m crests 30 m apart with sharp tops.
    let (d, along) = line_frame(x, z, [180.0, -150.0], -31.0);
    let warp = 6.0 * (fbm2(along / 90.0, 3.0, 31) - 0.5);
    for centre in [-15.0f32, 15.0] {
        let a = (d + warp - centre).abs();
        h += 7.0 * (1.0 - smooth(0.0, 9.0, a)).powf(1.6);
    }
    // A crevasse: 7 m wide, 22 m deep, meandering at 52°.
    let (d, along) = line_frame(x, z, [-60.0, 140.0], 52.0);
    let meander = 9.0 * (fbm2(along / 45.0, 7.0, 41) - 0.5);
    let a = (d + meander).abs();
    h -= 22.0 * (1.0 - smooth(2.5, 4.5, a));
    // Crust: decimetre bumps and centimetre grain that the 0.31 m texels resolve.
    h += 0.35 * (fbm2(x / 2.2, z / 2.2, 51) - 0.5) + 0.12 * (value_noise2(x / 0.7, z / 0.7, 52) - 0.5);
    h
}

/// Mars stand-in: stepped sedimentary mesas over a dusty floor with talus aprons.
fn mars_height(x: f32, z: f32) -> f32 {
    let mesa = fbm2(x / 230.0, z / 230.0, 61) * 0.75 + fbm2(x / 80.0, z / 80.0, 62) * 0.25;
    let mask = smooth(0.50, 0.62, mesa);
    // Stepped risers: 11 m beds, each a steep riser and a gently sloping tread.
    let raw = mask * 95.0 + 25.0 * smooth(0.62, 0.9, mesa);
    let step = 11.0;
    let bed = (raw / step).floor();
    let within = raw / step - bed;
    let terraced = (bed + smooth(0.55, 0.9, within)) * step;
    // Mostly smooth flanks with low benches (the plates' mesas are domes; their banding is
    // material, not geometry).
    let mut h = raw + (terraced - raw) * 0.3;
    // Dune floor and talus at the foot.
    h += 1.2 * (fbm2(x / 9.0, z / 9.0, 63) - 0.5) * (1.0 - mask);
    h += 0.25 * (fbm2(x / 1.8, z / 1.8, 64) - 0.5);
    h
}

pub(crate) fn planet_map(planet: Planet) -> TerrainMap {
    let n = VISUAL_N;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        let z = row as f32 / (n - 1) as f32 * WORLD_M - WORLD_M * 0.5;
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * WORLD_M - WORLD_M * 0.5;
            heights.push(match planet {
                Planet::Europa => europa_height(x, z),
                Planet::Mars => mars_height(x, z),
            });
        }
    }
    // Quantise through the 16-bit cook so the stand-in carries DO27's height precision.
    let (lo, hi) = heights
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &h| {
            (a.min(h), b.max(h))
        });
    let scale = (hi - lo).max(1.0);
    let samples: Vec<u16> = heights
        .iter()
        .map(|&h| (((h - lo) / scale) * 65535.0).round().clamp(0.0, 65535.0) as u16)
        .collect();
    let field = regia_asset::Heightfield {
        width: n,
        height: n,
        samples,
    };
    match TerrainMap::new(&field, WORLD_M, scale, lo) {
        Ok(map) => map,
        Err(err) => panic!("stand-in heightfield: {err}"),
    }
}

/// The six-layer-shaped stack (four textured layers here) for each planet.
fn planet_layers(planet: Planet, map: &TerrainMap) -> TerrainLayers {
    let (lo, hi) = map.min_max_height();
    let span = hi - lo;
    // Roughness: wind-packed snow and frost crust 0.85–0.9, glazed ice only in its own
    // patches (0.3), Mars dust and rock 0.9.
    let layer =
        |name: &str, tint: [f32; 3], slope: (f32, f32), alt: (f32, f32), tiling: f32, roughness: f32| {
            LayerDesc {
                name: name.into(),
                albedo: tint,
                roughness,
                tiling,
                slope_min: slope.0,
                slope_max: slope.1,
                altitude_min: alt.0,
                altitude_max: alt.1,
                blend_sharpness: 0.15,
                triplanar: name.contains("cliff"),
                height_blend: 0.7,
                detail_normal_strength: 0.8,
                ..LayerDesc::ground()
            }
        };
    let layers = match planet {
        // Europa's surface reflectance is ~0.45–0.6 (water ice with silicate and salt
        // contamination), not snow-white: the textures (≈ 0.88) are tinted down to ≈ 0.46
        // on the plain and ≈ 0.53 on frost and cliffs.
        Planet::Europa => vec![
            layer(
                "ice plain",
                [0.52, 0.54, 0.57],
                (0.0, 0.55),
                (lo - 1.0, hi + 1.0),
                0.9,
                0.85,
            ),
            layer(
                "dirty ice",
                [0.52, 0.49, 0.46],
                (0.0, 0.45),
                (lo - 1.0, lo + span * 0.45),
                0.7,
                0.9,
            ),
            layer(
                "ice cliff",
                [0.58, 0.61, 0.65],
                (0.45, 1.6),
                (lo - 1.0, hi + 1.0),
                0.6,
                0.75,
            ),
            layer(
                "frost cap",
                [0.6, 0.61, 0.64],
                (0.0, 0.35),
                (lo + span * 0.6, hi + 1.0),
                1.2,
                0.9,
            ),
            // Glazed ice in the hollows: the height blend breaks it into patches.
            LayerDesc {
                height_blend: 1.0,
                ..layer(
                    "glazed ice",
                    [0.6, 0.65, 0.71],
                    (0.0, 0.25),
                    (lo - 1.0, lo + span * 0.3),
                    0.5,
                    0.3,
                )
            },
        ],
        // Red-brown dust and rock (the plates' Mars), not ochre.
        Planet::Mars => vec![
            layer(
                "dust",
                [0.80, 0.44, 0.26],
                (0.0, 0.5),
                (lo - 1.0, hi + 1.0),
                0.8,
                0.92,
            ),
            layer(
                "talus",
                [0.72, 0.40, 0.26],
                (0.3, 0.85),
                (lo - 1.0, hi + 1.0),
                0.7,
                0.92,
            ),
            layer(
                "sandstone cliff",
                [0.84, 0.48, 0.32],
                (0.65, 1.6),
                (lo - 1.0, hi + 1.0),
                0.35,
                0.9,
            ),
            layer(
                "pale crest",
                [0.92, 0.64, 0.46],
                (0.0, 0.4),
                (lo + span * 0.7, hi + 1.0),
                0.9,
                0.9,
            ),
        ],
    };
    TerrainLayers {
        layers,
        ..Default::default()
    }
}

fn planet_material(planet: Planet, map: &TerrainMap) -> TerrainMaterial {
    let styles: Vec<LayerStyle> = match planet {
        Planet::Europa => vec![
            LayerStyle::Snow,
            LayerStyle::Dirt,
            LayerStyle::Ice,
            LayerStyle::Snow,
            LayerStyle::Ice,
        ],
        // Fine dust grain (Dirt's clumps and pebbles), not wind ripples; ripples only on
        // the pale crests.
        Planet::Mars => vec![
            LayerStyle::Dirt,
            LayerStyle::Dirt,
            LayerStyle::Rock,
            LayerStyle::Sand,
        ],
    };
    let layers: Vec<Option<LayerTextures>> = styles
        .iter()
        .enumerate()
        .map(|(i, style)| Some(style.bake(256, 70 + i as u32)))
        .collect();
    let macro_set = MacroBake::from_heightfield(map, 2, 256, 909).ok();
    TerrainMaterial {
        layers,
        macro_set,
        params: MaterialParams {
            macro_world_size: WORLD_M,
            ..MaterialParams::default()
        },
    }
}

/// Sun, exposure and outdoor preset per planet. DO27 leaves `Environment::ambient_lux` at
/// its 12 000 lx default; the shots keep that so the frame matches what the shell draws.
struct Lighting {
    sun_travel: [f32; 3],
    sun_color: [f32; 3],
    sun_lux: f32,
    exposure_ev: f32,
    preset: AtmospherePreset,
}

fn lighting(planet: Planet) -> Lighting {
    match planet {
        // Low hard sun (≈ 24°) at 5.2 AU: 4.4 klx.
        Planet::Europa => Lighting {
            sun_travel: normalize([0.62, -0.42, 0.66]),
            sun_color: [1.0, 0.98, 0.95],
            sun_lux: 120_000.0 / (5.2 * 5.2),
            // Stand-in camera exposure, matched to the UE plates now that the ice is not
            // white (round 1 picked EV 10 to show white snow; round 4: 11.0 with the mean-preserving mid relief). Per-map exposure belongs to
            // the look lane.
            exposure_ev: 11.0,
            preset: AtmospherePreset::Airless,
        },
        // Mid-afternoon Mars sun (≈ 38°): 52 klx through dust.
        Planet::Mars => Lighting {
            sun_travel: normalize([-0.55, -0.62, -0.56]),
            sun_color: [1.0, 0.88, 0.72],
            sun_lux: 52_000.0,
            exposure_ev: 13.3,
            preset: AtmospherePreset::Mars,
        },
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// Mean GPU pass times of one shot at 2560×1440 (Desktop High, native) over 40 frames of a
/// camera drifting 5 cm per frame, after 10 warm-up frames.
fn perf_shot(
    device: &RenderDevice,
    map: &TerrainMap,
    shot: &Shot,
    relief: TerrainRelief,
) -> Result<String, String> {
    let (pw, ph) = (2560u32, 1440u32);
    let (draw, mut scene, post, preset) = build(device, map, shot, false, relief, pw, ph)?;
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(preset),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let (mut pre, mut sh, mut fwd, mut total, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let eye0 = scene.eye;
    for i in 0..50 {
        scene.eye = [eye0[0] + i as f32 * 0.05, eye0[1], eye0[2] + i as f32 * 0.03];
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        renderer
            .render_offscreen(device, &frame)
            .map_err(|err| err.to_string())?;
        if i >= 10 {
            let t = renderer.gpu_timings();
            pre += t.prepass_ms;
            sh += t.shadows_ms;
            fwd += t.forward_ms;
            total += t.scene_ms;
            n += 1.0;
        }
    }
    Ok(format!(
        "{:<20} 2560x1440  prepass {:.2} ms  shadows {:.2} ms  forward {:.2} ms  scene {:.2} ms",
        shot.name,
        pre / n,
        sh / n,
        fwd / n,
        total / n
    ))
}

/// The shot's terrain draw, lab scene, post settings and planet preset.
#[allow(clippy::type_complexity)]
pub(crate) fn build(
    device: &RenderDevice,
    map: &TerrainMap,
    shot: &Shot,
    legacy_ambient: bool,
    relief: TerrainRelief,
    width: u32,
    height: u32,
) -> Result<(TerrainSharedDraw, LabScene, PostSettings, AtmospherePreset), String> {
    let light = lighting(shot.planet);
    let layers = planet_layers(shot.planet, map);
    let mut draw = TerrainSharedDraw::new(device, map.clone(), &layers, CdlodConfig::default(), 8)
        .map_err(|err| err.to_string())?;
    draw.set_material(&planet_material(shot.planet, map), &[])
        .map_err(|err| err.to_string())?;
    draw.set_relief(relief);
    if legacy_ambient {
        // The old shared pass: flat `ambient_lux` in the zenith blue. The current terrain
        // follows the frame's planet preset by itself (no host call).
        draw.set_sky_ambient(Some(SkyAmbient::from_flat_lux(12_000.0)));
    }

    let eye_ground = map.sample_bilinear(shot.eye[0], shot.eye[1]);
    let target_ground = map.sample_bilinear(shot.target[0], shot.target[1]);
    let aspect = width as f32 / height as f32;
    let hfov = shot.hfov_deg.to_radians();
    let fov_y = 2.0 * ((hfov * 0.5).tan() / aspect).atan();
    let post = PostSettings {
        exposure_ev: light.exposure_ev,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width,
        height,
        eye: [shot.eye[0], eye_ground + shot.eye_up, shot.eye[1]],
        target: [shot.target[0], target_ground + shot.target_up, shot.target[1]],
        fov_y,
        near: 0.1,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: light.exposure_ev,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new(light.sun_travel, light.sun_color, light.sun_lux),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    Ok((draw, scene, post, light.preset))
}

fn render(
    device: &RenderDevice,
    map: &TerrainMap,
    shot: &Shot,
    legacy_ambient: bool,
    relief: TerrainRelief,
) -> Result<Vec<u8>, String> {
    render_sized(device, map, shot, legacy_ambient, relief, W, H)
}

fn render_sized(
    device: &RenderDevice,
    map: &TerrainMap,
    shot: &Shot,
    legacy_ambient: bool,
    relief: TerrainRelief,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let (draw, scene, post, preset) = build(device, map, shot, legacy_ambient, relief, width, height)?;
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(preset),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let image = renderer
        .render_offscreen(device, &frame)
        .map_err(|err| err.to_string())?;
    Ok(image.pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mean luma, mean 8×8-block luma standard deviation, and the fraction of pixels whose
    /// luma gradient exceeds 6 levels, over the lower 45 % of the frame (the ground).
    fn ground_stats(pixels: &[u8], w: u32, h: u32) -> (f64, f64, f64) {
        let luma = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            0.2126 * f64::from(pixels[i])
                + 0.7152 * f64::from(pixels[i + 1])
                + 0.0722 * f64::from(pixels[i + 2])
        };
        let y0 = (h as f64 * 0.55) as u32;
        let (mut sum, mut n) = (0.0, 0.0);
        let mut steep = 0.0;
        for y in y0..h - 1 {
            for x in 0..w - 1 {
                let l = luma(x, y);
                sum += l;
                n += 1.0;
                let g = (luma(x + 1, y) - l).hypot(luma(x, y + 1) - l);
                if g > 6.0 {
                    steep += 1.0;
                }
            }
        }
        let mut block_std = 0.0;
        let mut blocks = 0.0;
        let mut by = y0;
        while by + 8 <= h {
            let mut bx = 0;
            while bx + 8 <= w {
                let vals: Vec<f64> = (0..64).map(|i| luma(bx + i % 8, by + i / 8)).collect();
                let m = vals.iter().sum::<f64>() / 64.0;
                block_std += (vals.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / 64.0).sqrt();
                blocks += 1.0;
                bx += 8;
            }
            by += 8;
        }
        (sum / n, block_std / blocks, steep / n)
    }

    /// Fine detail at the ~7 px scale over the ground region (lower 45 %): mean |luma −
    /// 7×7 box mean| over mean luma — the look lane's "fine detail" measure.
    fn fine_detail(pixels: &[u8], w: u32, h: u32) -> f64 {
        let luma = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            0.2126 * f64::from(pixels[i])
                + 0.7152 * f64::from(pixels[i + 1])
                + 0.0722 * f64::from(pixels[i + 2])
        };
        let y0 = (h as f64 * 0.55) as u32 + 3;
        let (mut dev, mut sum, mut n) = (0.0, 0.0, 0.0);
        for y in y0..h - 3 {
            for x in 3..w - 3 {
                let mut m = 0.0;
                for dy in 0..7 {
                    for dx in 0..7 {
                        m += luma(x + dx - 3, y + dy - 3);
                    }
                }
                m /= 49.0;
                let l = luma(x, y);
                dev += (l - m).abs();
                sum += l;
                n += 1.0;
            }
        }
        dev / sum.max(1e-9) * n / n
    }

    /// The UE v13 bore_twelve_mid plate's ground region (rows 55–100 % at 960×540): mean luma
    /// 104.5, block contrast 19.3, gradient density 0.79 (measured once from
    /// `parity/bore_twelve_mid_ue.png`, which is DO27's file and not in this repository).
    const UE_MID_GROUND_LUMA: f64 = 104.5;

    #[test]
    fn mid_range_ground_reads_as_rocky_relief() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU: {err}"),
        };
        let map = planet_map(Planet::Europa);
        let shot = SHOTS
            .iter()
            .find(|s| s.name == "bore_twelve_mid")
            .expect("mid shot");
        // The plates' 960×540: fine detail is a pixel-scale measure.
        let (w, h) = (960, 540);
        let render = |relief: TerrainRelief| {
            render_sized(&device, &map, shot, false, relief, w, h)
                .unwrap_or_else(|err| panic!("render: {err}"))
        };
        let with = render(TerrainRelief::default());
        let without = render(TerrainRelief {
            mid_relief: 0.0,
            ..TerrainRelief::default()
        });
        let (luma, contrast, density) = ground_stats(&with, w, h);
        let fine = fine_detail(&with, w, h);
        let fine_off = fine_detail(&without, w, h);
        eprintln!("mid ground fine detail (7 px): {fine_off:.4} → {fine:.4}");
        // UE's bore_twelve_mid ground measures 0.124 on this scale (look lane, 960×540); the
        // mid relief's finest octave must carry at least half of it.
        assert!(fine > 0.06, "mid-range fine detail too soft: {fine:.4}");
        let (_, contrast_off, density_off) = ground_stats(&without, w, h);
        eprintln!(
            "mid ground: luma {luma:.1} (UE {UE_MID_GROUND_LUMA}), block contrast {contrast_off:.2} → {contrast:.2}, gradient density {density_off:.3} → {density:.3}"
        );
        assert!(contrast > contrast_off * 1.6, "no mid-range relief contrast");
        assert!(density > density_off * 1.5, "no mid-range relief detail");
        assert!(
            (luma / UE_MID_GROUND_LUMA - 1.0).abs() < 0.15,
            "mid ground luma {luma:.1} is not within 15 % of the UE plate's {UE_MID_GROUND_LUMA}"
        );
    }
}
