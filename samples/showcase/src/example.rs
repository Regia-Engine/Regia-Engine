// SPDX-License-Identifier: MIT-0
//! The generator of `samples/showcase/example_scene`.
//!
//! Every file is computed here from fixed numbers: a procedural heightfield (value noise, a ridge
//! and a flat landing area), three splat layers with procedurally baked textures, a scatter set
//! with procedural stand-in meshes, a sky, a descending capsule and a schedule of an impact. There
//! are no downloaded or third-party assets. Regenerate with:
//!
//! ```text
//! cargo run -p showcase -- --write-example samples/showcase/example_scene
//! ```
//!
//! A test regenerates the scene into a temp directory and compares it with the committed files, so
//! the committed example cannot drift from this code.

use std::path::Path;

use regia_scene::level::{LevelAtmosphere, LevelLook, LevelTimeOfDay};
use regia_terrain::noise::fbm2;
use regia_terrain::{LayerDesc, ScatterClass, ScatterExclusion, ScatterMeshRef, ScatterSet, TerrainLayers};
use serde::Serialize;

use crate::scene::{
    CameraSpec, DEFAULT_MAX_SCATTER_INSTANCES, LayerMaterial, Manifest, MaterialSet, ReliefSpec,
    SCENE_VERSION, StandIn, StandInBinding,
};
use crate::sky::{SKY_VERSION, SkyFile, WeatherChange, WeatherSpec};
use crate::spawns::{DecalSpawn, EffectSpawn, Place, SPAWNS_VERSION, SpawnFile};
use crate::track::{CapsuleSpec, HeightMode, Interpolation, Key, TRACK_VERSION, Track};

/// Heightfield side in samples.
pub const SIDE: u32 = 257;
/// World width in metres.
pub const WORLD_WIDTH: f32 = 1024.0;
/// Height of the highest possible sample, metres.
pub const HEIGHT_SCALE: f32 = 140.0;
/// World position of the landing area (x, z).
pub const LANDING: (f32, f32) = (0.0, -100.0);

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The heightfield samples, row-major, row 0 at -Z.
#[must_use]
pub fn heightfield_samples() -> Vec<u16> {
    let n = SIDE as usize;
    let mut out = Vec::with_capacity(n * n);
    for row in 0..n {
        for col in 0..n {
            let u = col as f32 / (n - 1) as f32;
            let v = row as f32 / (n - 1) as f32;
            let rolling = fbm2(u * 3.0, v * 3.0, 11);
            let ridge = smoothstep(0.55, 1.0, v) * (0.55 + 0.9 * fbm2(u * 5.0, v * 5.0, 23));
            let raw = rolling * 0.32 + ridge * 0.7;
            // A flat landing area around `LANDING`.
            let x = u * WORLD_WIDTH - WORLD_WIDTH * 0.5;
            let z = v * WORLD_WIDTH - WORLD_WIDTH * 0.5;
            let d = ((x - LANDING.0).powi(2) + (z - LANDING.1).powi(2)).sqrt();
            let flat = 1.0 - smoothstep(60.0, 170.0, d);
            let h = raw + (0.2 - raw) * flat;
            out.push((h.clamp(0.0, 1.0) * 65535.0) as u16);
        }
    }
    out
}

/// Encodes a 16-bit grayscale PNG.
///
/// # Errors
/// The encoder's error as text.
pub fn encode_gray16(width: u32, height: u32, samples: &[u16]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        bytes.extend_from_slice(&s.to_be_bytes());
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&bytes).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn layers() -> TerrainLayers {
    let layer =
        |name: &str, albedo: [f32; 3], slope: (f32, f32), alt: (f32, f32), triplanar: bool| LayerDesc {
            name: name.into(),
            albedo,
            roughness: 0.9,
            tiling: 0.4,
            slope_min: slope.0,
            slope_max: slope.1,
            altitude_min: alt.0,
            altitude_max: alt.1,
            blend_sharpness: 0.15,
            triplanar,
            ..LayerDesc::ground()
        };
    TerrainLayers {
        layers: vec![
            layer("grass", [0.9, 1.0, 0.85], (0.0, 0.55), (-10.0, 70.0), false),
            layer("rock", [1.0, 0.98, 0.95], (0.35, 1.6), (-10.0, 200.0), true),
            layer("snow", [0.97, 0.99, 1.0], (0.0, 0.9), (85.0, 200.0), false),
        ],
        ..TerrainLayers::default()
    }
}

fn scatter() -> ScatterSet {
    let mesh = |name: &str, weight: f32, scales: (f32, f32), bias: f32| ScatterMeshRef {
        mesh: name.into(),
        weight,
        min_scale: scales.0,
        max_scale: scales.1,
        scale_bias: bias,
    };
    let landing = ScatterExclusion {
        points: vec![[LANDING.0, LANDING.1]],
        radius: 45.0,
    };
    ScatterSet {
        version: regia_terrain::SCATTER_SET_VERSION,
        seed: 20_261_022,
        classes: vec![
            ScatterClass {
                name: "rocks".into(),
                meshes: vec![
                    mesh("rock_a", 2.0, (0.6, 3.0), 2.5),
                    mesh("rock_b", 1.0, (0.6, 2.4), 2.5),
                ],
                cell_size: 8.0,
                density: 0.6,
                slope_max: 1.1,
                exclusions: vec![landing.clone()],
                fade_start: 400.0,
                fade_end: 520.0,
                ..ScatterClass::default()
            },
            ScatterClass {
                name: "pines".into(),
                meshes: vec![mesh("pine", 1.0, (5.0, 13.0), 1.0)],
                cell_size: 14.0,
                density: 0.45,
                slope_max: 0.4,
                layer_mask: 1,
                layer_min_weight: 0.5,
                align_to_normal: 0.0,
                tilt_max: 0.05,
                tilt_jitter: 0.02,
                exclusions: vec![landing],
                fade_start: 500.0,
                fade_end: 650.0,
                ..ScatterClass::default()
            },
        ],
    }
}

fn manifest() -> Manifest {
    Manifest {
        version: SCENE_VERSION,
        name: "example".into(),
        heightfield: "heightfield.png".into(),
        world_width: WORLD_WIDTH,
        height_scale: HEIGHT_SCALE,
        height_offset: 0.0,
        heightfield_size: None,
        layers: "terrain.layers.ron".into(),
        material: Some(MaterialSet {
            layers: vec![
                LayerMaterial::Style {
                    style: "Grass".into(),
                    size: 256,
                    seed: 31,
                },
                LayerMaterial::Style {
                    style: "Rock".into(),
                    size: 256,
                    seed: 32,
                },
                LayerMaterial::Style {
                    style: "Snow".into(),
                    size: 256,
                    seed: 33,
                },
            ],
            macro_world_size: 0.0,
            anti_tiling_cell: 4.0,
            relief: ReliefSpec::photo(),
        }),
        splat: None,
        scatter: Some("scatter/example.scatter.ron".into()),
        stand_ins: vec![
            StandInBinding {
                mesh: "rock_a".into(),
                stand_in: StandIn::Rock {
                    color: (0.2, 0.19, 0.18),
                },
            },
            StandInBinding {
                mesh: "rock_b".into(),
                stand_in: StandIn::Boulder {
                    color: (0.16, 0.15, 0.14),
                },
            },
            StandInBinding {
                mesh: "pine".into(),
                stand_in: StandIn::Cone {
                    color: (0.03, 0.08, 0.035),
                },
            },
        ],
        max_scatter_instances: DEFAULT_MAX_SCATTER_INSTANCES,
        sky: "sky.ron".into(),
        track: Some("track.ron".into()),
        spawns: Some("spawns.ron".into()),
        vfx_dir: None,
        camera: CameraSpec {
            position: (60.0, 46.0, -170.0),
            target: (0.0, 36.0, -100.0),
            fov_deg: 45.0,
            near: 0.3,
            far: 20_000.0,
            exposure_ev: None,
        },
        lead_in_frames: 0,
    }
}

fn sky() -> SkyFile {
    SkyFile {
        version: SKY_VERSION,
        atmosphere: LevelAtmosphere {
            time_of_day: Some(LevelTimeOfDay {
                hour: 8.0,
                day_of_year: 80,
                latitude_deg: 46.0,
            }),
            ..LevelAtmosphere::default()
        },
        look: LevelLook::default(),
        weather: WeatherSpec::Preset("fair".into()),
        changes: vec![WeatherChange {
            frame: 120,
            to: WeatherSpec::Preset("cloudy".into()),
            seconds: 6.0,
        }],
        mist: None,
        cloud_look: None,
    }
}

fn track() -> Track {
    let key = |frame: u32, position: (f32, f32, f32), euler_deg: (f32, f32, f32)| Key {
        frame,
        position,
        euler_deg,
    };
    Track {
        version: TRACK_VERSION,
        entity: "pod".into(),
        capsule: CapsuleSpec {
            radius: 0.9,
            half_length: 1.2,
        },
        mesh: None,
        swap_mesh: None,
        swap_frame: None,
        color: (0.55, 0.57, 0.6),
        height_mode: HeightMode::AboveGround,
        interpolation: Interpolation::CatmullRom,
        keys: vec![
            key(0, (-200.0, 160.0, -420.0), (-35.0, 35.0, 0.0)),
            key(150, (-8.0, 14.0, -112.0), (-12.0, 20.0, 0.0)),
            key(180, (LANDING.0, 2.1, LANDING.1), (-4.0, 15.0, 6.0)),
            key(240, (LANDING.0, 2.1, LANDING.1), (-4.0, 15.0, 6.0)),
        ],
    }
}

fn spawns() -> SpawnFile {
    let under = |y: f32| Place::UnderEntity {
        offset: (0.0, y, 0.0),
    };
    SpawnFile {
        version: SPAWNS_VERSION,
        effects: vec![
            EffectSpawn {
                frame: 170,
                effect: "dust_trail".into(),
                at: under(0.5),
                forward: (0.0, 1.0, 0.0),
                seed: Some(11),
            },
            EffectSpawn {
                frame: 180,
                effect: "explosion".into(),
                at: under(1.0),
                forward: (0.0, 1.0, 0.0),
                seed: Some(7),
            },
            EffectSpawn {
                frame: 182,
                effect: "smoke".into(),
                at: under(1.0),
                forward: (0.0, 1.0, 0.0),
                seed: Some(9),
            },
        ],
        decals: vec![
            DecalSpawn {
                frame: 180,
                at: under(0.0),
                normal: None,
                half_size: (6.0, 6.0, 2.0),
                color: (0.05, 0.04, 0.04, 0.9),
                lifetime: None,
                prefab: Some("scorch".into()),
                yaw_deg: 20.0,
            },
            // A plain tinted box (the message path beside the prefab above): the scorch's dark core,
            // inside the prefab's footprint. At 9 m it was an 18 m dark square around the landing
            // site, read as a square shadow in every shot of it (ADR-5792).
            DecalSpawn {
                frame: 181,
                at: under(0.0),
                normal: None,
                half_size: (1.2, 1.2, 2.0),
                color: (0.02, 0.018, 0.016, 0.8),
                lifetime: None,
                prefab: None,
                yaw_deg: 0.0,
            },
        ],
    }
}

fn ron_text<T: Serialize>(value: &T) -> Result<String, String> {
    // `\n` on every OS (ron's default is `\r\n` on Windows): the generated files are byte-identical
    // to the committed example everywhere.
    let config = regia_core::ron_pretty();
    ron::ser::to_string_pretty(value, config).map_err(|e| e.to_string())
}

/// Every file of the example scene as `(relative path, bytes)`, in a fixed order.
///
/// # Errors
/// A serialization or encoding error as text.
pub fn example_files() -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let layers_text = layers().to_ron().map_err(|e| e.to_string())?;
    let scatter_text = scatter().to_ron().map_err(|e| e.to_string())?;
    Ok(vec![
        ("scene.ron", ron_text(&manifest())?.into_bytes()),
        (
            "heightfield.png",
            encode_gray16(SIDE, SIDE, &heightfield_samples())?,
        ),
        ("terrain.layers.ron", layers_text.into_bytes()),
        ("scatter/example.scatter.ron", scatter_text.into_bytes()),
        ("sky.ron", ron_text(&sky())?.into_bytes()),
        ("track.ron", ron_text(&track())?.into_bytes()),
        ("spawns.ron", ron_text(&spawns())?.into_bytes()),
    ])
}

/// Writes the example scene into `dir` (created), each file through a temp file and a rename.
///
/// # Errors
/// An IO, serialization or encoding error as text.
pub fn write_example_scene(dir: &Path) -> Result<(), String> {
    for (rel, bytes) in example_files()? {
        crate::write_atomic(&dir.join(rel), &bytes)?;
    }
    Ok(())
}

/// The example's benchmark views: the committed file, so the demo works without the source tree.
const EXAMPLE_VIEWS: &[u8] = include_bytes!("../example_scene/perf_views.json");
/// The example's credits: the committed file, which also lists the source tree's hand-written cinematics.
const EXAMPLE_MANIFEST: &str = include_str!("../example_scene/asset_manifest.json");

/// Writes the example scene as `showcase --demo` runs it when it finds no scene folder (a machine without the
/// source tree): the generated files ([`write_example_scene`]), the benchmark's views and the credits of those
/// files. `--write-example` stays as it is: only the generated files.
///
/// # Errors
/// An IO, serialization or encoding error as text.
pub fn write_demo_example_scene(dir: &Path) -> Result<(), String> {
    use crate::demo::bench::VIEWS_FILE;
    use crate::demo::credits::{AssetManifest, MANIFEST_FILE};
    write_example_scene(dir)?;
    crate::write_atomic(&dir.join(VIEWS_FILE), EXAMPLE_VIEWS)?;
    let files = example_files()?;
    let mut manifest = AssetManifest::parse(EXAMPLE_MANIFEST)?;
    manifest
        .assets
        .retain(|asset| asset.path == VIEWS_FILE || files.iter().any(|(rel, _)| *rel == asset.path));
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    crate::write_atomic(&dir.join(MANIFEST_FILE), text.as_bytes())
}

/// Side of the feature scene's splat packs, texels.
pub const PACK_SIDE: u32 = 64;

/// Encodes an RGBA8 PNG.
///
/// # Errors
/// The encoder's error as text.
pub fn encode_rgba8(width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(data).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// The splat packs of the feature scene: the west half of the map is painted `grass` (pack0 R), the east
/// half `rock` (pack0 G); `snow` (pack0 B) is painted in a band along the +Z edge. Pack1 is empty.
#[must_use]
pub fn feature_packs() -> [Vec<u8>; 2] {
    let n = PACK_SIDE as usize;
    let mut pack0 = vec![0u8; n * n * 4];
    for row in 0..n {
        for col in 0..n {
            let i = (row * n + col) * 4;
            if row >= n - 6 {
                pack0[i + 2] = 255;
            } else if col < n / 2 {
                pack0[i] = 255;
            } else {
                pack0[i + 1] = 255;
            }
            pack0[i + 3] = 0;
        }
    }
    [pack0, vec![0u8; n * n * 4]]
}

/// The feature scene's stream: a meandering centerline from east to west, source first, as the JSON the
/// orchestrator's `terrain/stream.json` has.
#[must_use]
pub fn feature_stream_json() -> String {
    let points: Vec<String> = (0..40)
        .map(|i| {
            let t = i as f32 / 39.0;
            let x = 300.0 - 600.0 * t;
            let z = -60.0 + 40.0 * (t * 6.0).sin();
            let water_y = 30.0 - 6.0 * t;
            format!(
                "{{\"x\": {x}, \"z\": {z}, \"water_y\": {water_y}, \"width_m\": {}, \"depth_m\": 0.3}}",
                2.0 + 0.5 * t
            )
        })
        .collect();
    format!(
        "{{\"flows\": \"source first, east to west\", \"points\": [{}]}}\n",
        points.join(", ")
    )
}

fn feature_entry() -> crate::features::EntryFile {
    use crate::features::{ENTRY_FILE_VERSION, EntryFile, EntryKey, EntryTierSpec};
    EntryFile {
        version: ENTRY_FILE_VERSION,
        points: vec![
            EntryKey {
                time: 0.0,
                position: (8000.0, 60_000.0, 9000.0),
            },
            EntryKey {
                time: 2.0,
                position: (400.0, 120.0, -200.0),
            },
        ],
        start_frame: 0,
        tier: EntryTierSpec::Cheap,
        ground_y: None,
        seed: None,
        peak_radiance: None,
        head_radius_m: None,
        body_radius_m: None,
        ion_life_s: None,
        train_life_s: None,
        wind: None,
        shed_altitude_m: None,
        shed_count: Some(2),
        damage_smoke: None,
        min_px: None,
    }
}

/// Every file of the feature scene: the example scene with splat packs, a `terrain/grass.ron`, a
/// `terrain/stream.json`, an `entry.ron` and a `mist:` block in `sky.ron`, all from fixed numbers (no
/// downloaded assets). Files that the example scene also has are replaced.
///
/// # Errors
/// A serialization or encoding error as text.
pub fn feature_files() -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let mut manifest = manifest();
    manifest.splat = Some(crate::scene::SplatSpec {
        pack0: "terrain/macro/pack0.png".into(),
        pack1: "terrain/macro/pack1.png".into(),
    });
    let mut sky = sky();
    sky.mist = Some(crate::features::MistSpec {
        grid_cell_m: 16.0,
        concavity_radius_m: 120.0,
        ..crate::features::MistSpec::default()
    });
    let grass = crate::features::GrassFile {
        dead_grass: "grass".into(),
        shoots: "snow".into(),
        ..crate::features::GrassFile::default()
    };
    let [pack0, pack1] = feature_packs();
    let mut files: Vec<(&'static str, Vec<u8>)> = example_files()?
        .into_iter()
        .filter(|(name, _)| !matches!(*name, "scene.ron" | "sky.ron"))
        .collect();
    files.push(("scene.ron", ron_text(&manifest)?.into_bytes()));
    files.push(("sky.ron", ron_text(&sky)?.into_bytes()));
    files.push((
        "terrain/macro/pack0.png",
        encode_rgba8(PACK_SIDE, PACK_SIDE, &pack0)?,
    ));
    files.push((
        "terrain/macro/pack1.png",
        encode_rgba8(PACK_SIDE, PACK_SIDE, &pack1)?,
    ));
    files.push(("terrain/grass.ron", ron_text(&grass)?.into_bytes()));
    files.push(("terrain/stream.json", feature_stream_json().into_bytes()));
    files.push(("entry.ron", ron_text(&feature_entry())?.into_bytes()));
    Ok(files)
}

/// Writes the feature scene into `dir` (created), each file through a temp file and a rename.
///
/// # Errors
/// An IO, serialization or encoding error as text.
pub fn write_feature_scene(dir: &Path) -> Result<(), String> {
    for (rel, bytes) in feature_files()? {
        crate::write_atomic(&dir.join(rel), &bytes)?;
    }
    Ok(())
}
