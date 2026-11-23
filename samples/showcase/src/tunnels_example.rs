// SPDX-License-Identifier: MIT-0
//! The generator of `samples/showcase/tunnels_scene` (ADR-8825): the Switchbacks tunnel layout armed on a
//! 1280 m canyon floor, for measuring tunnel frames against the matching outdoor frames.
//!
//! Every file is computed here from fixed numbers and the passages file: a flat 4096² heightfield at the
//! canyon floor (the ground lattice the cook and the terrain draw share), two splat layers with procedural
//! styles, a rock scatter kept off the mouths, a sky, `tunnels/tunnels.ron`, the mesh-layers document (its
//! fills are every mouth of the layout; the two known Switchbacks ramp facets carry the ramp-facet waiver the
//! cook asks for, ADR-8359 / ADR-8436) and `perf_views.json` with the view pairs: `tube_<pair>` inside a
//! sealed tube and `outdoor_<pair>` on the plain right above it, looking the same way, plus `mouth_drive`
//! down the `mid_east` skylight ramp into the tube. There are no downloaded or third-party assets.
//!
//! The passages file is the Switchbacks fixture of `regia_world_geom` (Regia-owned test data, its
//! `fixtures` feature), so the generator takes its text: the test `tunnels_scene` passes it in, compares
//! every generated file with the committed scene, and writes the scene with
//! `REGIA_WRITE_TUNNELS_SCENE=1 cargo test -p showcase --test tunnels_scene`.

use regia_render::viscells::{OUTDOOR_CELL, VisCells};
use regia_scene::level::{LevelAtmosphere, LevelLook, LevelTimeOfDay};
use regia_terrain::{
    LayerDesc, ScatterClass, ScatterExclusion, ScatterMeshRef, ScatterSet, TerrainLayers, TerrainMap,
};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::layout::{PassageKind, Sample, SampledLayout, SampledPassage};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, cook};
use regia_world_geom::ground::vertex_m;
use regia_world_geom::passages::PassagesDocument;
use serde::Serialize;
use serde_json::{Value, json};

use crate::example::encode_gray16;
use crate::scene::{
    CameraSpec, LayerMaterial, Manifest, MaterialSet, ReliefSpec, SCENE_VERSION, StandIn, StandInBinding,
};
use crate::sky::{SKY_VERSION, SkyFile, WeatherSpec};
use crate::tunnels::{TUNNELS_FILE_VERSION, TubeLights, TunnelsFile, ground_input, stage_a_sampler};

/// Heightfield side in samples: DO27's drawn grid, which is the cook's ground lattice.
pub const SIDE: u32 = 4096;
/// World width in metres.
pub const WORLD_WIDTH: f32 = 1280.0;
/// Height of ground count 0, metres.
pub const GROUND_OFFSET_M: f32 = -512.0;
/// Metres per ground count (the Stage A sampler's count).
pub const GROUND_STEP_M: f32 = 1.0 / 32.0;
/// The plain's ground count: the canyon floor at -256 m.
pub const PLAIN_COUNT: u16 = 8192;
/// The plain's height, metres.
pub const PLAIN_M: f32 = -256.0;
/// The map id of the tunnel files.
pub const MAP: &str = "switchbacks";
/// The mesh-layers document, inside `tunnels/`.
pub const DOCUMENT: &str = "switchbacks.meshlayers.ron";
/// The passages file, inside `tunnels/`.
pub const PASSAGES: &str = "switchbacks.passages.ron";
/// Eye height over a floor or the plain, metres (a hover hull's driver).
pub const EYE_M: f32 = 3.0;
/// Seconds a view's path takes.
pub const VIEW_SECONDS: f32 = 6.0;

/// The heightfield samples, row-major: the flat canyon floor.
#[must_use]
pub fn heightfield_samples() -> Vec<u16> {
    vec![PLAIN_COUNT; (SIDE * SIDE) as usize]
}

/// The heightfield as the scene loader builds it (16-bit samples over `height_scale` metres from
/// `height_offset`).
///
/// # Errors
/// The map constructor's message.
pub fn terrain_map() -> Result<TerrainMap, String> {
    let scale = height_scale();
    let heights: Vec<f32> = heightfield_samples()
        .iter()
        .map(|&s| f32::from(s) / 65535.0 * scale + GROUND_OFFSET_M)
        .collect();
    TerrainMap::from_heights(SIDE, SIDE, heights, WORLD_WIDTH, 1.0, 0.0).map_err(|e| e.to_string())
}

/// Height of sample 65535 over the offset: one ground count a sample step.
#[must_use]
pub fn height_scale() -> f32 {
    65535.0 * GROUND_STEP_M
}

fn tunnels_file() -> TunnelsFile {
    TunnelsFile {
        version: TUNNELS_FILE_VERSION,
        document: DOCUMENT.into(),
        ground_offset_m: GROUND_OFFSET_M,
        ground_step_m: GROUND_STEP_M,
        lights: TubeLights {
            spacing_m: 40.0,
            height_m: 9.0,
            intensity_lm: 60_000.0,
            radius_m: 36.0,
            color: (1.0, 0.78, 0.55),
        },
        rock: (0.42, 0.27, 0.17),
    }
}

/// Cooks `document` on the scene's ground, reading `passages` for every layer.
fn cook_on_plain(document: &str, passages: &str, map: &TerrainMap) -> Result<CookOutput, CookError> {
    let file = tunnels_file();
    let ground =
        ground_input(map, &file, std::path::Path::new(DOCUMENT)).map_err(|e| CookError::InputMissing {
            path: DOCUMENT.into(),
            reason: e.to_string(),
        })?;
    let sampler = stage_a_sampler(map);
    let read = |_: &str| Ok(passages.as_bytes().to_vec());
    cook(&CookInputs {
        document: document.as_bytes(),
        ground: &ground,
        terrain: Some(&sampler),
        read_file: &read,
    })
}

/// The mesh-layers document: one `tubes` layer of the passages file that fills every mouth, with the ramp-facet
/// waiver the cook asks for on this ground. Also returns the cook's output.
///
/// # Errors
/// The cook's error (a failure that is not only the known ramp facets).
pub fn document(passages: &str, map: &TerrainMap) -> Result<(String, CookOutput), String> {
    let layout = PassagesDocument::from_ron(passages)
        .map_err(|e| e.to_string())?
        .layout;
    let sampler = stage_a_sampler(map);
    let mouths = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&sampler)));
    let fills: Vec<String> = mouths.iter().map(|m| format!("{:?}", m.id)).collect();
    let base = format!(
        "(version: 1, ground_tile_cells: 64, layers: [(id: \"tubes\", source: Passages(path: \"{PASSAGES}\"), fills: [{}])])\n",
        fills.join(", ")
    );
    match cook_on_plain(&base, passages, map) {
        Ok(out) => Ok((base, out)),
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            let mut waived = waive_ramp_facets(&base, &report).map_err(|e| e.to_string())?;
            if !waived.ends_with('\n') {
                waived.push('\n');
            }
            let out = cook_on_plain(&waived, passages, map).map_err(|e| format!("{e} ({})", e.code()))?;
            Ok((waived, out))
        }
        Err(e) => Err(format!("{e} ({})", e.code())),
    }
}

fn layers() -> TerrainLayers {
    let layer = |name: &str, albedo: [f32; 3], slope: (f32, f32)| LayerDesc {
        name: name.into(),
        albedo,
        roughness: 0.92,
        tiling: 0.4,
        slope_min: slope.0,
        slope_max: slope.1,
        altitude_min: -600.0,
        altitude_max: 600.0,
        blend_sharpness: 0.15,
        triplanar: slope.0 > 0.0,
        ..LayerDesc::ground()
    };
    TerrainLayers {
        layers: vec![
            layer("regolith", [1.0, 0.86, 0.72], (0.0, 0.6)),
            layer("rock", [0.92, 0.8, 0.7], (0.35, 1.6)),
        ],
        ..TerrainLayers::default()
    }
}

/// Mouth exclusions: the centre of every cooked mouth ring, out to its farthest ring point plus 6 m.
fn mouth_exclusions(out: &CookOutput) -> Vec<ScatterExclusion> {
    let lattice = out.file.canonical.lattice;
    out.report
        .ground
        .mouths
        .iter()
        .filter_map(|mouth| {
            let points: Vec<[f32; 2]> = mouth
                .rings
                .iter()
                .flatten()
                .map(|&(i, j)| [vertex_m(&lattice, i), vertex_m(&lattice, j)])
                .collect();
            if points.is_empty() {
                return None;
            }
            let n = points.len() as f32;
            let c = points
                .iter()
                .fold([0.0f32; 2], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n]);
            let r = points
                .iter()
                .map(|p| ((p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sqrt())
                .fold(0.0f32, f32::max);
            Some(ScatterExclusion {
                points: vec![[round_cm(c[0]), round_cm(c[1])]],
                radius: (r + 6.0).ceil(),
            })
        })
        .collect()
}

fn round_cm(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

fn scatter(exclusions: Vec<ScatterExclusion>) -> ScatterSet {
    let mesh = |name: &str, weight: f32, scales: (f32, f32), bias: f32| ScatterMeshRef {
        mesh: name.into(),
        weight,
        min_scale: scales.0,
        max_scale: scales.1,
        scale_bias: bias,
    };
    ScatterSet {
        version: regia_terrain::SCATTER_SET_VERSION,
        seed: 20_261_119,
        classes: vec![
            ScatterClass {
                name: "rocks".into(),
                meshes: vec![
                    mesh("rock_a", 2.0, (0.5, 2.6), 2.5),
                    mesh("rock_b", 1.0, (0.6, 3.2), 2.5),
                ],
                cell_size: 9.0,
                density: 0.5,
                slope_max: 1.1,
                exclusions: exclusions.clone(),
                fade_start: 420.0,
                fade_end: 560.0,
                ..ScatterClass::default()
            },
            ScatterClass {
                name: "boulders".into(),
                meshes: vec![mesh("rock_b", 1.0, (3.0, 7.0), 1.5)],
                cell_size: 40.0,
                density: 0.35,
                slope_max: 1.1,
                exclusions,
                fade_start: 700.0,
                fade_end: 900.0,
                ..ScatterClass::default()
            },
        ],
    }
}

fn manifest(camera: CameraSpec) -> Manifest {
    Manifest {
        version: SCENE_VERSION,
        name: "tunnels".into(),
        heightfield: "heightfield.png".into(),
        world_width: WORLD_WIDTH,
        height_scale: height_scale(),
        height_offset: GROUND_OFFSET_M,
        heightfield_size: Some(SIDE),
        layers: "terrain.layers.ron".into(),
        material: Some(MaterialSet {
            layers: vec![
                LayerMaterial::Style {
                    style: "Dirt".into(),
                    size: 256,
                    seed: 41,
                },
                LayerMaterial::Style {
                    style: "Rock".into(),
                    size: 256,
                    seed: 42,
                },
            ],
            macro_world_size: 0.0,
            anti_tiling_cell: 4.0,
            relief: ReliefSpec::photo(),
        }),
        splat: None,
        scatter: Some("scatter/rocks.scatter.ron".into()),
        stand_ins: vec![
            StandInBinding {
                mesh: "rock_a".into(),
                stand_in: StandIn::Rock {
                    color: (0.24, 0.15, 0.1),
                },
            },
            StandInBinding {
                mesh: "rock_b".into(),
                stand_in: StandIn::Boulder {
                    color: (0.2, 0.13, 0.09),
                },
            },
        ],
        max_scatter_instances: crate::scene::DEFAULT_MAX_SCATTER_INSTANCES,
        sky: "sky.ron".into(),
        track: None,
        spawns: None,
        vfx_dir: None,
        camera,
        lead_in_frames: 0,
    }
}

fn sky() -> SkyFile {
    SkyFile {
        version: SKY_VERSION,
        atmosphere: LevelAtmosphere {
            time_of_day: Some(LevelTimeOfDay {
                hour: 15.0,
                day_of_year: 200,
                latitude_deg: 30.0,
            }),
            ..LevelAtmosphere::default()
        },
        look: LevelLook::default(),
        weather: WeatherSpec::Preset("fair".into()),
        changes: Vec::new(),
        mist: None,
        cloud_look: None,
    }
}

fn ron_text<T: Serialize>(value: &T) -> Result<String, String> {
    ron::ser::to_string_pretty(value, regia_core::ron_pretty()).map_err(|e| e.to_string())
}

fn r2(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

fn point(p: [f32; 3]) -> Value {
    json!([r2(p[0]), r2(p[1]), r2(p[2])])
}

fn add(p: [f32; 3], dy: f32) -> [f32; 3] {
    [p[0], p[1] + dy, p[2]]
}

/// Every run of covered segments of `passage` whose samples all lie on `side` of the map's north-south axis (`0`:
/// west, `x < 0`; `1`: east), as sample indices `(first, last)`, in passage order.
fn covered_runs(passage: &SampledPassage, side: usize) -> Vec<(usize, usize)> {
    let on_side = |k: usize| {
        passage
            .samples
            .get(k)
            .is_some_and(|s| usize::from(s.p[0] >= 0.0) == side)
    };
    let mut runs = Vec::new();
    let mut start = None;
    for k in 0..=passage.covered.len() {
        let good = passage.covered.get(k).copied().unwrap_or(false) && on_side(k) && on_side(k + 1);
        match (good, start) {
            (true, None) => start = Some(k),
            (false, Some(s)) => {
                runs.push((s, k));
                start = None;
            }
            _ => {}
        }
    }
    runs
}

/// The samples a view's eye follows from `start` (at most `last`): [`VIEW_TRAVEL_M`] of arc length.
fn travel(passage: &SampledPassage, start: usize, last: usize) -> Vec<&Sample> {
    let s0 = passage.samples[start].s;
    let mut picked = Vec::new();
    for sample in &passage.samples[start..=last] {
        picked.push(sample);
        if sample.s - s0 >= VIEW_TRAVEL_M {
            break;
        }
    }
    picked
}

/// Whether every eye of `picked` (3 m over the floor) lies in an indoor cell of `cells`.
fn all_indoor(cells: &VisCells, picked: &[&Sample]) -> bool {
    let outdoor = cells.index_of(OUTDOOR_CELL);
    picked.iter().all(|s| {
        let eye = add(s.p, EYE_M);
        Some(cells.locate_index(regia_core::math::Vec3::from_array(eye))) != outdoor
    })
}

/// Arc length a view's eye travels, metres.
pub const VIEW_TRAVEL_M: f32 = 30.0;

/// A tube view and its outdoor twin from covered run `(first, last)` of `passage`: the eye follows the tube's own
/// centreline samples from the middle of the run for [`VIEW_TRAVEL_M`] of arc length in [`VIEW_SECONDS`] and looks
/// along its heading; the outdoor twin is the same keys on the plain right above (the same heading).
fn view_pair(pair: &str, passage: &SampledPassage, picked: &[&Sample]) -> [Value; 2] {
    let s0 = picked.first().map_or(0.0, |p| p.s);
    let span = picked.last().map_or(0.0, |p| p.s - s0).max(1.0);
    let keys = |lift: &dyn Fn([f32; 3]) -> [f32; 3]| -> Vec<Value> {
        picked
            .iter()
            .map(|sample| {
                let t = (sample.s - s0) / span * VIEW_SECONDS;
                json!({ "time": r2(t), "position": point(lift(sample.p)) })
            })
            .collect()
    };
    let view = |name: String, description: String, keys: Vec<Value>| {
        json!({
            "name": name,
            "description": description,
            "path": { "interp": "Linear", "keys": keys },
            "fov": [{ "time": 0.0, "value": 60.0 }],
        })
    };
    [
        view(
            format!("tube_{pair}"),
            format!(
                "Inside the sealed {} at {EYE_M} m over the floor, along the bore",
                passage.id
            ),
            keys(&|p| add(p, EYE_M)),
        ),
        view(
            format!("outdoor_{pair}"),
            format!("The plain right above tube_{pair}, the same path and heading"),
            keys(&|p| [p[0], PLAIN_M + EYE_M, p[2]]),
        ),
    ]
}

/// Down the `ramp_mid_east` skylight climb from the plain into the tube, looking along the heading.
fn mouth_drive(sampled: &SampledLayout) -> Option<Value> {
    let ramp = sampled.passage("ramp_mid_east")?;
    let top = ramp.samples.last()?.p;
    let bottom = ramp.samples.first()?.p;
    let mid = ramp.samples[ramp.samples.len() / 2].p;
    let out = [top[0] - bottom[0], top[2] - bottom[2]];
    let norm = (out[0] * out[0] + out[1] * out[1]).sqrt().max(1e-3);
    let dir = [out[0] / norm, out[1] / norm];
    let start = [top[0] + dir[0] * 40.0, PLAIN_M, top[2] + dir[1] * 40.0];
    let inside = [bottom[0] - dir[0] * 30.0, bottom[1], bottom[2] - dir[1] * 30.0];
    let keys: Vec<Value> = [start, top, mid, bottom, inside]
        .iter()
        .enumerate()
        .map(|(i, p)| json!({ "time": i as f32 * 2.0, "position": point(add(*p, EYE_M)) }))
        .collect();
    Some(json!({
        "name": "mouth_drive",
        "description": "From the plain down the mid_east skylight ramp into the tube",
        "path": { "interp": "CatmullRom", "keys": keys },
        "fov": [{ "time": 0.0, "value": 60.0 }],
    }))
}

/// The views file: `tube_west`/`outdoor_west`, `tube_east`/`outdoor_east` (the two halves of the lava tube)
/// and `mouth_drive`.
fn views(sampled: &SampledLayout, cells: &VisCells) -> Result<Value, String> {
    // The longest covered run (at least 4 segments) on each side of the map's north-south axis, over every tube
    // passage: one pair per half of the lava tube.
    let mut best: [Option<(&SampledPassage, (usize, usize))>; 2] = [None, None];
    for passage in sampled.passages.iter().filter(|p| p.kind == PassageKind::Tube) {
        for side in [0, 1] {
            for (first, last) in covered_runs(passage, side) {
                if last - first < 4 {
                    continue;
                }
                if best[side].is_none_or(|(_, (a, b))| last - first > b - a) {
                    best[side] = Some((passage, (first, last)));
                }
            }
        }
    }
    let mut list = Vec::new();
    for (side, name) in [(0, "west"), (1, "east")] {
        let Some((passage, (first, last))) = best[side] else {
            continue;
        };
        // From the middle of the run outwards: the first start whose whole path is inside an indoor cell.
        let middle = (first + last) / 2;
        let starts = (0..=(last - first)).map(|d| {
            if d.is_multiple_of(2) {
                middle + d / 2
            } else {
                middle.saturating_sub(d / 2 + 1)
            }
        });
        let picked = starts
            .filter(|&k| (first..last).contains(&k))
            .map(|k| travel(passage, k, last))
            .find(|picked| picked.len() >= 2 && all_indoor(cells, picked));
        if let Some(picked) = picked {
            list.extend(view_pair(name, passage, &picked));
        }
    }
    list.extend(mouth_drive(sampled));
    if list.is_empty() {
        return Err("the layout has no covered tube run for a view".into());
    }
    Ok(json!({ "version": 1, "views": list }))
}

/// Every file of the tunnels scene as `(relative path, bytes)`, in a fixed order.
///
/// # Errors
/// A cook, serialization or encoding error as text.
pub fn tunnels_files(passages: &str) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let map = terrain_map()?;
    let (document, out) = document(passages, &map)?;
    let layout = PassagesDocument::from_ron(passages)
        .map_err(|e| e.to_string())?
        .layout;
    let sampled = layout.sample(Some(&stage_a_sampler(&map)));
    let cells = VisCells::from_ron(&out.viscells_ron).map_err(|e| e.to_string())?;
    let views = views(&sampled, &cells)?;
    let first = &views["views"][1]["path"];
    let camera = CameraSpec {
        position: tuple(&first["keys"][0]["position"]),
        target: tuple(&first["keys"][1]["position"]),
        fov_deg: 60.0,
        near: 0.2,
        far: 20_000.0,
        exposure_ev: None,
    };
    let mut views_text = serde_json::to_string_pretty(&views).map_err(|e| e.to_string())?;
    views_text.push('\n');
    let layers_text = layers().to_ron().map_err(|e| e.to_string())?;
    let scatter_text = scatter(mouth_exclusions(&out))
        .to_ron()
        .map_err(|e| e.to_string())?;
    let tunnels_text = ron_text(&tunnels_file())?;
    Ok(vec![
        ("scene.ron", ron_text(&manifest(camera))?.into_bytes()),
        (
            "heightfield.png",
            encode_gray16(SIDE, SIDE, &heightfield_samples())?,
        ),
        ("terrain.layers.ron", layers_text.into_bytes()),
        ("scatter/rocks.scatter.ron", scatter_text.into_bytes()),
        ("sky.ron", ron_text(&sky())?.into_bytes()),
        ("perf_views.json", views_text.into_bytes()),
        ("tunnels/tunnels.ron", tunnels_text.into_bytes()),
        ("tunnels/switchbacks.meshlayers.ron", document.into_bytes()),
        ("tunnels/switchbacks.passages.ron", passages.as_bytes().to_vec()),
    ])
}

fn tuple(v: &Value) -> (f32, f32, f32) {
    let c = |i: usize| v[i].as_f64().unwrap_or(0.0) as f32;
    (c(0), c(1), c(2))
}

/// Writes the tunnels scene into `dir` (created), each file through a temp file and a rename.
///
/// # Errors
/// An IO, cook, serialization or encoding error as text.
pub fn write_tunnels_scene(dir: &std::path::Path, passages: &str) -> Result<(), String> {
    for (rel, bytes) in tunnels_files(passages)? {
        crate::write_atomic(&dir.join(rel), &bytes)?;
    }
    Ok(())
}
