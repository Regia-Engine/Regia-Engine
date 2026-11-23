// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for world formats: procedural generation, biomes, roads, level directories, layers, splines, shapes,
//! content chunks and video sources. Reflected components go through the scene loader's path
//! (`formats::reflected_load`), so a mutated component is decoded and migrated the way a level does it.

use std::fmt::Display;
use std::path::Path;
use std::sync::OnceLock;

use regia_ecs::prelude::TypeRegistry;
use regia_procgen::biome::def::BiomeDef;
use regia_procgen::biome::ecs::{AreaShape, BiomeArea};
use regia_procgen::graph::ProcGraphVariant;
use regia_procgen::painted::{PaintedPoint, PaintedPoints};
use regia_procgen::pgout::{self, PgoutHeader};
use regia_procgen::plant::{PlantsIndex, PlantsIndexEntry};
use regia_procgen::road::{RoadOf, RoadProfile};
use regia_procgen::tiles::ProcGenAnchor;
use regia_procgen::world::{
    InstanceCollision, InstancedMeshSet, ProcBakeTier, ProcBounds, ProcGenMode, ProcGenerator, ProcLimits,
    SavedInstance,
};
use regia_procgen::{ProcGraph, ProcNodeRegistry, RunOutput};
use regia_scene::shape::{Shape, ShapeKind};
use regia_scene::spline::{
    Spline, SplineAttrDecl, SplineAttrType, SplinePoint, SplineUp, SplineValue, TangentMode,
};

use super::formats::{pretty, reflected_fixture, reflected_load, text};
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// A graph goes past the parse: compiled against the built-in nodes. A compile error is a report about the graph,
/// not a rejection (the editor opens such files), so the text still re-encodes.
fn pgraph_load(s: &str) -> Result<String, String> {
    static NODES: OnceLock<ProcNodeRegistry> = OnceLock::new();
    let graph = ProcGraph::from_ron(s).map_err(e)?;
    let _ = graph.compile(NODES.get_or_init(ProcNodeRegistry::with_builtins));
    Ok(graph.to_ron())
}

fn pgout_fixture() -> Vec<u8> {
    let mut output = RunOutput::default();
    output.instance_sets.push(regia_procgen::exec::InstanceSet {
        mesh: "m.glb".into(),
        material: Some("mat".into()),
        cast_shadows: true,
        collision: regia_procgen::exec::Collision::Simple,
        draw_distance_m: 300.0,
        fade_m: 20.0,
        cull_below_px: 2.0,
        instances: vec![regia_procgen::exec::Instance {
            translation: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.5, 0.0, 0.0, 0.0],
        }],
        layer: None,
    });
    // Two prefab records, the second with overrides, so mutations reach the optional override
    // section of version 4 (ADR-7750).
    for (k, overrides) in [
        (0, Vec::new()),
        (
            1,
            vec![
                regia_procgen::exec::PrefabFieldOverride {
                    component: "Glow".into(),
                    field: "intensity".into(),
                    value: regia_procgen::data::AttrValue::Float(2.5),
                },
                regia_procgen::exec::PrefabFieldOverride {
                    component: "my_game::Sign".into(),
                    field: "text".into(),
                    value: regia_procgen::data::AttrValue::Name("north".into()),
                },
            ],
        ),
    ] {
        output.prefabs.push(regia_procgen::exec::PrefabSpawn {
            prefab: "props/lamp.prefab.ron".into(),
            name: format!("lamp_{k}"),
            translation: [k as f32 * 2.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            tags: vec!["lamp".into()],
            overrides,
        });
    }
    let header = PgoutHeader {
        graph_hash: [1; 32],
        param_hash: [2; 32],
        seed: 7,
        input_hash: [3; 32],
        output_hash: output.hash(),
        quality: 3,
    };
    pgout::encode(&header, &output)
}

fn procgen_register(registry: &mut TypeRegistry) {
    registry.register::<ProcGenerator>();
    registry.register::<ProcGenMode>();
    registry.register::<ProcBounds>();
    registry.register::<ProcBakeTier>();
    registry.register::<ProcLimits>();
    registry.register::<InstancedMeshSet>();
    registry.register::<SavedInstance>();
    registry.register::<InstanceCollision>();
    registry.register::<PaintedPoints>();
    registry.register::<PaintedPoint>();
    registry.register::<RoadOf>();
    registry.register::<ProcGenAnchor>();
    registry.register::<BiomeArea>();
    registry.register::<AreaShape>();
}

fn scene_register(registry: &mut TypeRegistry) {
    registry.register::<Spline>();
    registry.register::<SplinePoint>();
    registry.register::<SplineUp>();
    registry.register::<TangentMode>();
    registry.register::<SplineAttrDecl>();
    registry.register::<SplineAttrType>();
    registry.register::<SplineValue>();
    registry.register::<Shape>();
    registry.register::<ShapeKind>();
}

fn spline_load(s: &str) -> Result<String, String> {
    reflected_load::<Spline>(s, scene_register, |spline| {
        spline.migrate().map_err(e)?;
        spline.validate().map_err(e)
    })
}

/// The procgen and scene targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        // Procedural graphs and bakes (specs 62, 66).
        text(
            "pgraph",
            Some("pgraph"),
            regia_procgen::graph::PGRAPH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/fuzz.pgraph.ron").into(),
            pgraph_load,
        ),
        text(
            "pgvar",
            Some("pgvar"),
            regia_procgen::graph::PGVAR_FORMAT_VERSION,
            true,
            || include_str!("fixtures/pgvar.ron").into(),
            |s| ProcGraphVariant::from_ron(s).map(|v| v.to_ron()).map_err(e),
        ),
        FuzzTarget {
            name: "pgout",
            format: Some("pgout"),
            format_version: pgout::PGOUT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: pgout_fixture,
                load: |b| {
                    pgout::decode(b)
                        .map(|(header, output)| pgout::encode(&header, &output))
                        .map_err(e)
                },
            },
        },
        text(
            "procgen-generator",
            Some("procgen-generator"),
            regia_procgen::world::PROCGEN_GENERATOR_FORMAT_VERSION,
            true,
            || reflected_fixture(procgen_register, &ProcGenerator::default()),
            |s| reflected_load::<ProcGenerator>(s, procgen_register, ProcGenerator::migrate),
        ),
        text(
            "procgen-instanced-set",
            Some("procgen-instanced-set"),
            regia_procgen::world::INSTANCED_SET_FORMAT_VERSION,
            true,
            || {
                reflected_fixture(
                    procgen_register,
                    &InstancedMeshSet {
                        mesh: "rocks.glb".into(),
                        instances: vec![SavedInstance::default()],
                        ..InstancedMeshSet::default()
                    },
                )
            },
            |s| reflected_load::<InstancedMeshSet>(s, procgen_register, |_| Ok(())),
        ),
        text(
            "procgen-anchor",
            Some("procgen-anchor"),
            regia_procgen::tiles::PROCGEN_ANCHOR_FORMAT_VERSION,
            true,
            || reflected_fixture(procgen_register, &ProcGenAnchor::default()),
            |s| reflected_load::<ProcGenAnchor>(s, procgen_register, ProcGenAnchor::migrate),
        ),
        text(
            "procgen-painted-points",
            Some("procgen-painted-points"),
            regia_procgen::painted::PAINTED_POINTS_FORMAT_VERSION,
            true,
            || {
                reflected_fixture(
                    procgen_register,
                    &PaintedPoints {
                        points: vec![PaintedPoint::default()],
                        layer_names: vec!["trees".into()],
                        ..PaintedPoints::default()
                    },
                )
            },
            |s| reflected_load::<PaintedPoints>(s, procgen_register, PaintedPoints::migrate),
        ),
        // Roads (specs/64) have no writer: nothing edits a `.road.ron` but a text editor.
        text(
            "road",
            Some("road"),
            regia_procgen::road::ROAD_FORMAT_VERSION,
            false,
            || include_str!("fixtures/road.ron").into(),
            |s| RoadProfile::from_ron(s).map(|_| String::new()),
        ),
        text(
            "road-of",
            Some("road-of"),
            regia_procgen::road::ROAD_OF_FORMAT_VERSION,
            true,
            || reflected_fixture(procgen_register, &RoadOf::new("roads/main.road.ron")),
            |s| reflected_load::<RoadOf>(s, procgen_register, RoadOf::migrate),
        ),
        // Biomes (specs/65) and the plant index (specs/68).
        text(
            "biome",
            Some("biome"),
            regia_procgen::biome::BIOME_FORMAT_VERSION,
            true,
            || include_str!("fixtures/biome.ron").into(),
            |s| BiomeDef::from_ron(s).map(|b| b.to_ron()).map_err(e),
        ),
        text(
            "biome-area",
            Some("biome-area"),
            regia_procgen::biome::ecs::BIOME_AREA_FORMAT_VERSION,
            true,
            || reflected_fixture(procgen_register, &BiomeArea::new("biomes/pine.biome.ron")),
            |s| reflected_load::<BiomeArea>(s, procgen_register, BiomeArea::migrate),
        ),
        text(
            "plants-index",
            Some("plants-index"),
            regia_procgen::plant::PLANTS_INDEX_FORMAT_VERSION,
            true,
            || {
                PlantsIndex {
                    graph: "plants/pine.pgraph.ron".into(),
                    graph_hash: "ab12".into(),
                    variants: vec![PlantsIndexEntry {
                        file: "pine_0.glb".into(),
                        seed: 1,
                        triangles_per_lod: vec![1200, 300],
                        bounds: ([-1.0, 0.0, -1.0], [1.0, 9.0, 1.0]),
                    }],
                }
                .to_ron()
            },
            |s| PlantsIndex::from_ron(s).map(|p| p.to_ron()),
        ),
        // Level directories and layers (spec 37).
        text(
            "level-dir.header",
            Some("level-dir"),
            regia_scene::level_dir::LEVEL_DIR_FORMAT_VERSION,
            true,
            || include_str!("fixtures/level-dir-header.ron").into(),
            |s| {
                let header = regia_scene::level_dir::parse_header(Path::new("level.ron"), s).map_err(e)?;
                pretty(&header)
            },
        ),
        text(
            "level-dir.entity",
            Some("level-dir"),
            regia_scene::level_dir::LEVEL_DIR_FORMAT_VERSION,
            true,
            || include_str!("fixtures/level-dir-entity.ron").into(),
            |s| {
                let file =
                    regia_scene::level_dir::parse_entity_file(Path::new("a.entity.ron"), s).map_err(e)?;
                pretty(&file)
            },
        ),
        text(
            "layers",
            Some("layers"),
            regia_scene::layers::LAYERS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/layers.ron").into(),
            |s| {
                let defs = regia_scene::layers::LayerDefs::from_ron_str(s).map_err(e)?;
                let _ = regia_scene::layers::validate_defs(&defs);
                defs.to_ron_string().map_err(e)
            },
        ),
        // A layer stored in its own file next to the level (ADR-7792).
        text(
            "layer-file",
            Some("layer-file"),
            regia_scene::layer_file::LAYER_FILE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/layer-file.ron").into(),
            |s| {
                let file = regia_scene::layer_file::parse_layer_file(Path::new("a.night.layer.ron"), s)
                    .map_err(e)?;
                file.to_ron_string().map_err(e)
            },
        ),
        // Splines and shapes (specs/64).
        text(
            "spline",
            Some("spline"),
            regia_scene::spline::SPLINE_FORMAT_VERSION,
            true,
            || {
                let points = [
                    regia_core::math::Vec3::ZERO,
                    regia_core::math::Vec3::X,
                    regia_core::math::Vec3::new(2.0, 0.0, 1.0),
                ];
                reflected_fixture(scene_register, &Spline::through(points))
            },
            spline_load,
        ),
        text(
            "shape",
            Some("shape"),
            regia_scene::shape::SHAPE_FORMAT_VERSION,
            true,
            || reflected_fixture(scene_register, &Shape::default()),
            |s| reflected_load::<Shape>(s, scene_register, Shape::migrate),
        ),
        // Content chunks (spec 40).
        text(
            "content-manifest",
            Some("content-manifest"),
            regia_asset::content::CONTENT_MANIFEST_FORMAT_VERSION,
            true,
            || include_str!("fixtures/content-manifest.ron").into(),
            |s| {
                regia_asset::content::ContentManifest::from_ron_str(s)
                    .and_then(|m| m.to_ron_string())
                    .map_err(e)
            },
        ),
        FuzzTarget {
            name: "chunk-map",
            format: Some("chunk-map"),
            format_version: regia_asset::content::CHUNK_MAP_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: || {
                    regia_asset::content::ChunkMap {
                        chunks: vec!["base".into(), "arena".into()],
                        entries: vec![
                            (regia_asset::AssetId(regia_core::id::Guid::from_u128(1)), 0),
                            (regia_asset::AssetId(regia_core::id::Guid::from_u128(2)), 1),
                        ],
                    }
                    .to_bytes()
                    .unwrap_or_default()
                },
                load: |b| {
                    regia_asset::content::ChunkMap::from_bytes(b)
                        .and_then(|m| m.to_bytes())
                        .map_err(e)
                },
            },
        },
        // Video sources (spec 50).
        text(
            "video",
            Some("video"),
            regia_media::source::VIDEO_SOURCE_VERSION,
            true,
            || include_str!("fixtures/video.ron").into(),
            |s| {
                regia_media::source::VideoSource::from_ron(s)
                    .and_then(|v| v.to_ron())
                    .map_err(e)
            },
        ),
        text(
            "videolist",
            Some("videolist"),
            regia_media::source::VIDEO_LIST_VERSION,
            true,
            || include_str!("fixtures/videolist.ron").into(),
            |s| {
                let list = regia_media::source::VideoList::from_ron(s).map_err(e)?;
                pretty(&list)
            },
        ),
        // Streamed levels (spec 78 § 5): the cooked world index and a cell blob.
        FuzzTarget {
            name: "world-index",
            format: Some("world-index"),
            format_version: regia_world::format::WORLD_INDEX_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: world_index_fixture,
                load: world_index_load,
            },
        },
        FuzzTarget {
            name: "cell-blob",
            format: Some("cell-blob"),
            format_version: regia_world::format::CELL_BLOB_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: cell_blob_fixture,
                load: cell_blob_load,
            },
        },
        // `.wstream` version 1 through the migrator, and version 2 with baked HLOD atlases
        // (ADR-2483).
        FuzzTarget {
            name: "world-index.v1",
            format: Some("world-index"),
            format_version: regia_world::format::WORLD_INDEX_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: world_index_v1_fixture,
                load: world_index_load,
            },
        },
        FuzzTarget {
            name: "world-index.atlas",
            format: Some("world-index"),
            format_version: regia_world::format::WORLD_INDEX_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: world_index_atlas_fixture,
                load: world_index_load,
            },
        },
        // The HLOD voxel table of a level with scatter (`.wvoxels`, ADR-3253).
        FuzzTarget {
            name: "hlod-voxels",
            format: Some("hlod-voxels"),
            format_version: regia_world::hlod_voxels::HLOD_VOXELS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: hlod_voxels_fixture,
                load: hlod_voxels_load,
            },
        },
    ]
}

/// A voxel table of two nodes, the second with a voxel proxy.
fn hlod_voxels_fixture() -> Vec<u8> {
    use regia_world::hlod_voxels::{
        HLOD_VOXELS_FORMAT_VERSION, HlodVoxelNode, HlodVoxels, hlod_voxel_proxy_id,
    };
    HlodVoxels {
        format_version: HLOD_VOXELS_FORMAT_VERSION,
        level: STREAM_LEVEL.0,
        resolution: 32,
        nodes: vec![
            None,
            Some(HlodVoxelNode {
                proxy: hlod_voxel_proxy_id(STREAM_LEVEL, 1),
                anchor: [1, 0, -2],
                instances: 64,
                voxels: 1500,
                voxel_size_m: 4.0,
                bytes: 24_000,
            }),
        ],
    }
    .to_bytes()
    .unwrap_or_default()
}

/// Decodes and validates as the runtime does on open, and re-encodes.
fn hlod_voxels_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    regia_world::hlod_voxels::HlodVoxels::from_bytes(bytes, STREAM_LEVEL)
        .map_err(e)?
        .to_bytes()
        .map_err(e)
}

/// The fixture index with an HLOD tree of one proxy per cell plus a root.
fn world_index_with_hlod(atlas: bool) -> regia_world::format::WorldIndex {
    use regia_world::format::{HlodAtlas, HlodIndex, WorldIndex, hlod_proxy_id};
    let mut index =
        WorldIndex::from_bytes(&world_index_fixture(), STREAM_LEVEL).unwrap_or_else(|_| empty_index());
    let proxies = vec![regia_scene::hlod::CellProxy {
        cell: stream_cell(),
        min_y: -1.0,
        max_y: 12.5,
        bytes: 900,
    }];
    let proxy = hlod_proxy_id(STREAM_LEVEL, 0);
    index.settings.hlod_atlas_px = if atlas { 8 } else { 0 };
    index.hlod = Some(HlodIndex {
        config: regia_scene::hlod::HlodConfig::default(),
        proxies,
        node_proxies: vec![Some(proxy)],
        node_anchors: vec![[1, 0, -2]],
        node_colors: vec![Some([0.25, 0.5, 0.75, 1.0])],
        node_atlases: vec![
            atlas.then(|| HlodAtlas {
                material: regia_asset::AssetId::sub_asset(proxy, "atlas/material"),
                textures: ["atlas/base", "atlas/normal", "atlas/orm"]
                    .map(|n| regia_asset::AssetId::sub_asset(proxy, n)),
                size: [256, 128],
                texels_per_m: 4.0,
                asked_texels_per_m: 8.0,
                capped: true,
                bake_error_m: 0.05,
                miss_ratio: 0.002,
            }),
        ],
    });
    index
}

/// A stand-in index if the base fixture ever failed to decode, so the fixture stays total.
fn empty_index() -> regia_world::format::WorldIndex {
    regia_world::format::WorldIndex {
        format_version: regia_world::format::WORLD_INDEX_FORMAT_VERSION,
        level: STREAM_LEVEL.0,
        content_hash: [0; 32],
        settings: regia_scene::streaming::LevelStreaming::default(),
        cells: Vec::new(),
        bounds: [stream_cell(), stream_cell()],
        hlod: None,
        replicated: Vec::new(),
        nav: None,
    }
}

fn world_index_v1_fixture() -> Vec<u8> {
    world_index_with_hlod(false).to_v1_bytes().unwrap_or_default()
}

fn world_index_atlas_fixture() -> Vec<u8> {
    world_index_with_hlod(true).to_bytes().unwrap_or_default()
}

/// The level the streaming fixtures belong to: the container names it, so a body naming another
/// level is refused (the runtime opens an index by its level).
const STREAM_LEVEL: regia_scene::level::LevelId =
    regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0x5747_7265_616d));

fn stream_cell() -> regia_scene::stream_grid::CellCoord {
    regia_scene::stream_grid::CellCoord::new(1, -2)
}

fn world_index_fixture() -> Vec<u8> {
    use regia_world::format::{CellRecord, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id};
    let blob = cell_blob_id(STREAM_LEVEL, stream_cell());
    let cells = vec![CellRecord {
        coord: stream_cell(),
        blob,
        blob_hash: [3; 32],
        entity_count: 4,
        own_bytes: 4096,
        dep_bytes: 1024,
        vram_bytes: 2048,
        min_y: -1.0,
        max_y: 12.5,
        priority: 1,
        deps: vec![regia_asset::AssetId::sub_asset(blob, "mesh")],
        nav_tiles: 2,
        has_terrain_collision: true,
    }];
    let index = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level: STREAM_LEVEL.0,
        content_hash: WorldIndex::hash_cells(&cells),
        settings: regia_scene::streaming::LevelStreaming {
            enabled: true,
            ..regia_scene::streaming::LevelStreaming::default()
        },
        cells,
        bounds: [stream_cell(), stream_cell()],
        hlod: None,
        replicated: vec![(regia_core::id::Guid::from_u128(7), stream_cell())],
        nav: None,
    };
    index.to_bytes().unwrap_or_default()
}

/// Decodes, validates as the runtime does on open, and re-encodes.
fn world_index_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let index = regia_world::format::WorldIndex::from_bytes(bytes, STREAM_LEVEL).map_err(e)?;
    if index.level != STREAM_LEVEL.0 {
        return Err(format!(
            "index of level {} in the file of {}",
            index.level, STREAM_LEVEL.0
        ));
    }
    index.validate().map_err(e)?;
    index.to_bytes().map_err(e)
}

fn cell_blob_fixture() -> Vec<u8> {
    use regia_world::format::{CELL_BLOB_FORMAT_VERSION, CellBlob, cell_blob_id};
    let blob = CellBlob {
        format_version: CELL_BLOB_FORMAT_VERSION,
        coord: stream_cell(),
        scene: regia_scene::cooked::CookedScene {
            format_version: regia_scene::cooked::COOKED_SCENE_VERSION,
            strings: vec!["a".into(), "b".into()],
            entities: vec![],
            resources: vec![],
        },
        anchor: [1, 0, -2],
        nav: Some(vec![9; 32]),
    };
    blob.to_bytes(cell_blob_id(STREAM_LEVEL, stream_cell()), Vec::new())
        .unwrap_or_default()
}

/// Decodes past the record's hash (the hash of the input itself, so the parsers meet the damage) and
/// re-encodes. The record's dependency list is not part of the blob, so the re-encoding has none.
fn cell_blob_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let id = regia_world::format::cell_blob_id(STREAM_LEVEL, stream_cell());
    let hash = *blake3::hash(bytes).as_bytes();
    let blob = regia_world::format::CellBlob::from_bytes(bytes, id, &hash).map_err(e)?;
    blob.to_bytes(id, Vec::new()).map_err(e)
}
