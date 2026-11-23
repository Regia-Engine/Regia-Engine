// SPDX-License-Identifier: MPL-2.0
//! The terrain editing remainder's formats (ADR-2411): a v12 level that names its terrain, through
//! the scene reader, the settings reader and the block's validation, and `<map>.grassdensity.bin`;
//! the cooked terrain index v2 and its exact final heights (ADR-8024); a v14 level whose terrain is
//! cell-centered (ADR-10079).

use std::fmt::Display;

use regia_terrain::{GrassDensityPaint, GrassDensityStroke};

use super::formats::text;
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// The seed of `scene.terrain`: a v12 level naming its terrain.
const TERRAIN_LEVEL_SEED: &str = r#"SceneFile(
    format_version: 12,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (
            terrain: Some((
                layers: "maps/arena/terrain.layers.ron",
                map: "maps/arena",
                world_width_m: Some(512.0),
                height_scale_m: 120.0,
                height_offset_m: -4.0,
            )),
        ),
    },
)"#;

fn terrain_level_load(s: &str) -> Result<String, String> {
    let file = regia_scene::format::SceneFile::from_ron_str(s).map_err(e)?;
    if let Some(raw) = file.resources.get(regia_scene::level::LEVEL_SETTINGS_PATH) {
        let settings = regia_scene::level::LevelSettings::from_ron(raw.get_ron()).map_err(e)?;
        if let Some(t) = &settings.terrain {
            // Validation and the file resolution must refuse, never panic.
            if t.validate().is_ok() {
                let _ = regia_terrain::LevelTerrainFiles::resolve(
                    std::path::Path::new("assets"),
                    "maps/a.level.ron",
                    Some(t),
                );
            }
        }
    }
    file.to_ron_string().map_err(e)
}

/// The seed of `scene.terrain_layout`: a v14 level whose terrain is cell-centered (ADR-10079).
const CELL_CENTERED_LEVEL_SEED: &str = r#"SceneFile(
    format_version: 14,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (
            terrain: Some((
                layers: "maps/arena/terrain.layers.ron",
                map: "maps/arena",
                world_width_m: Some(64.0),
                height_scale_m: 40.0,
                height_offset_m: 0.0,
                sample_layout: CellCentered,
            )),
        ),
    },
)"#;

/// The scene and settings readers, then a small map built with the block's layout and width
/// (ADR-10079): a hostile width or layout must be refused or give finite sample positions, never
/// panic.
fn terrain_layout_load(s: &str) -> Result<String, String> {
    let file = regia_scene::format::SceneFile::from_ron_str(s).map_err(e)?;
    if let Some(raw) = file.resources.get(regia_scene::level::LEVEL_SETTINGS_PATH) {
        let settings = regia_scene::level::LevelSettings::from_ron(raw.get_ron()).map_err(e)?;
        if let Some(t) = settings.terrain.as_ref().filter(|t| t.validate().is_ok()) {
            let n = 5u32;
            let width = t.world_width_m.unwrap_or(n as f32);
            let map = regia_terrain::TerrainMap::from_heights(n, n, vec![0.0; 25], width, 1.0, 0.0)
                .map_err(e)?
                .with_sample_layout(t.sample_layout);
            for i in [0, n / 2, n - 1] {
                let [x, z] = map.sample_to_world(i, i);
                if !(x.is_finite() && z.is_finite()) {
                    return Err(format!("sample {i} at ({x}, {z})"));
                }
            }
        }
    }
    file.to_ron_string().map_err(e)
}

/// A 16-texel image with one stroke.
fn density_fixture() -> Vec<u8> {
    let Ok(mut paint) = GrassDensityPaint::new(16) else {
        return Vec::new();
    };
    let _ = paint.stroke(
        &GrassDensityStroke {
            path: vec![[0.0, 0.0], [4.0, 2.0]],
            radius_m: 3.0,
            hardness: 0.5,
            strength: 1.0,
            target: 0.0,
        },
        16.0,
    );
    paint.to_bytes().unwrap_or_default()
}

fn density_load(b: &[u8]) -> Result<Vec<u8>, String> {
    GrassDensityPaint::from_bytes(b)
        .and_then(|p| p.to_bytes())
        .map_err(e)
}

/// The edit-layers fixture: two layers (add and set) over a 70 x 70 ramp, and the map they fold to
/// (a `.terrain.editlayers` file only reads against its own terrain).
fn edit_layers_stack() -> Option<(regia_terrain::TerrainEditLayers, regia_terrain::TerrainMap)> {
    use regia_terrain::brush::{Brush, StrokeRecord, StrokeSample};
    let n = 70u32;
    let heights = (0..n * n).map(|i| (i % n) as f32 * 0.25).collect();
    let mut map = regia_terrain::TerrainMap::from_heights(n, n, heights, 70.0, 100.0, 0.0).ok()?;
    let mut stack = regia_terrain::TerrainEditLayers::new(&map);
    for (name, blend, lift) in [
        ("Hills", regia_terrain::EditLayerBlend::Add, 1.5f32),
        ("Plateau", regia_terrain::EditLayerBlend::Set, 4.0),
    ] {
        stack.create(name, blend).ok()?;
        let samples: Vec<StrokeSample<f32>> = (60..68)
            .flat_map(|r| (3..9).map(move |c| (c, r)))
            .map(|(c, r)| {
                let before = map.heights()[(r * n + c) as usize];
                StrokeSample {
                    position: regia_core::math::IVec2::new(c as i32, r as i32),
                    before,
                    after: before + lift,
                }
            })
            .collect();
        let mut record = StrokeRecord {
            brush: Brush::default(),
            samples,
            dirty: regia_terrain::DirtyRect {
                col_start: 3,
                col_end: 9,
                row_start: 60,
                row_end: 68,
            },
            tiles: [(0, 1), (0, 2)].into_iter().collect(),
        };
        stack.absorb_stroke(&mut record, &mut map).ok()?;
    }
    Some((stack, map))
}

fn edit_layers_fixture() -> Vec<u8> {
    edit_layers_stack()
        .and_then(|(stack, _)| stack.to_bytes().ok())
        .unwrap_or_default()
}

fn edit_layers_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let (_, map) = edit_layers_stack().ok_or("the fixture terrain could not be built")?;
    regia_terrain::TerrainEditLayers::from_bytes(b, &map)
        .and_then(|s| s.to_bytes())
        .map_err(e)
}

/// A current terrain index with every optional entry: exact heights, holes and a photoreal
/// material (ADR-8024), a count mapping (ADR-8471), and since version 3 a Z spacing other than X
/// and a valid-data mask with the center sample missing (ADR-8871), over a 3×3 R16 grid.
fn terrain_index_fixture() -> Vec<u8> {
    use regia_terrain::source::CountMapping;
    use regia_terrain::stream::{
        TERRAIN_INDEX_FORMAT_VERSION, TerrainHeightfieldIndex, TerrainHeightsIndex, TerrainHolesIndex,
        TerrainIndex, TerrainMaterialIndex, TerrainValidMaskIndex,
    };
    let index = TerrainIndex {
        version: TERRAIN_INDEX_FORMAT_VERSION,
        map_id: regia_asset::AssetId::sub_asset(regia_asset::AssetId::NIL, "maps/arena"),
        slots: Vec::new(),
        heightfield: Some(TerrainHeightfieldIndex {
            width: 3,
            height: 3,
            tile_size: 256,
            tiles_x: 1,
            tiles_y: 1,
            bytes: 120,
        }),
        layers: vec!["ground".into(), "rock".into()],
        world_size_m: Some(12.0),
        world_origin: Some([0.0, 0.0, 0.0]),
        heights: Some(TerrainHeightsIndex {
            width: 3,
            height: 3,
            encoding: regia_terrain::TerrainHeightsEncoding::Residual,
            height_scale: 2.4,
            height_offset: 0.01,
            bytes: 64,
        }),
        holes: Some(TerrainHolesIndex { bytes: 90 }),
        material: Some(TerrainMaterialIndex {
            params: regia_terrain::MaterialParams::default().into(),
            macro_tiles: 2,
            layer_albedo: 2,
            layer_normal: 2,
            layer_height: 2,
            layer_orm: 0,
            macro_albedo_tiles: 4,
            macro_normal_tiles: 4,
            macro_coarse_albedo: true,
            macro_coarse_normal: true,
            bytes: 4096,
        }),
        source_mapping: Some(CountMapping {
            cols: 3,
            rows: 3,
            meters_per_count: 0.01,
            zero_counts: 1000,
            cell_m: 2.0,
            sample0_m: [-3.0, -1.5],
        }),
        source_spacing_z_m: Some(1.5),
        // Nine flags, least significant bit first: every sample but the center (bit 4).
        source_valid_mask: Some(TerrainValidMaskIndex {
            cols: 3,
            rows: 3,
            missing: 1,
            bits: vec![0b1110_1111, 0b0000_0001],
        }),
    };
    index.to_postcard().unwrap_or_default()
}

fn terrain_index_load(b: &[u8]) -> Result<Vec<u8>, String> {
    regia_terrain::stream::load_terrain_index_payload(b)
        .map_err(e)
        .and_then(|index| index.to_postcard().map_err(e))
}

/// A residual final-heights payload over a 3×3 R16 grid (ADR-8024).
fn final_heights_fixture() -> Vec<u8> {
    use regia_terrain::cooked_terrain::{encode_final_heights, quantize_heights};
    let heights: Vec<f32> = (0..9).map(|i| 0.01 + i as f32 * 0.3).collect();
    let r16 = quantize_heights(&heights, 2.4, 0.01);
    encode_final_heights(3, 3, &heights, &r16, 2.4, 0.01)
        .map(|(_, bytes)| bytes)
        .unwrap_or_default()
}

fn final_heights_load(b: &[u8]) -> Result<Vec<u8>, String> {
    regia_terrain::cooked_terrain::FinalHeightsPayload::parse(b)
        .map(|payload| payload.to_bytes())
        .map_err(e)
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "terrain.index",
            format: Some("terrain-index"),
            format_version: regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: terrain_index_fixture,
                load: terrain_index_load,
            },
        },
        FuzzTarget {
            name: "terrain.final_heights",
            format: Some("terrain-final-heights"),
            format_version: regia_terrain::TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: final_heights_fixture,
                load: final_heights_load,
            },
        },
        text(
            "scene.terrain",
            None,
            regia_scene::format::SCENE_FORMAT_VERSION,
            true,
            || TERRAIN_LEVEL_SEED.into(),
            terrain_level_load,
        ),
        text(
            "scene.terrain_layout",
            None,
            regia_scene::format::SCENE_FORMAT_VERSION,
            true,
            || CELL_CENTERED_LEVEL_SEED.into(),
            terrain_layout_load,
        ),
        FuzzTarget {
            name: "terrain.edit_layers",
            format: Some("terrain-editlayers"),
            format_version: regia_terrain::EDIT_LAYERS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: edit_layers_fixture,
                load: edit_layers_load,
            },
        },
        FuzzTarget {
            name: "terrain.grass_density",
            format: Some("grass-density"),
            format_version: regia_terrain::GRASS_DENSITY_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: density_fixture,
                load: density_load,
            },
        },
    ]
}
