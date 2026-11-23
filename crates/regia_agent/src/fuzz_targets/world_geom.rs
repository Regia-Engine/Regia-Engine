// SPDX-License-Identifier: MPL-2.0
//! The meshlayers formats (spec 121 § 9, ADR-8112): the `<map>.meshlayers.ron` document, the
//! `<map>.passages.ron` it names (both `terrain-mesh-layers`), and the cooked `<map>.meshlayers`
//! (`terrain-mesh-layers-cooked`). Each loader parses, checks and writes back; a mutated file must
//! only be refused by name. The cooked file's presentation section (the render meshes, plan row W-07,
//! ADR-8175) and chunk section (the terrain's layer chunks, ADR-8398, ADR-8576) are decoded and
//! checked as well.

use regia_world_geom::compat::layout::{LAYOUT_VERSION, Passage, PassageKind, SAMPLE_STEP_M, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{COOKED_FORMAT_VERSION, CookedMeshLayers};
use regia_world_geom::meshlayers::{MESH_LAYERS_FORMAT_VERSION, MeshLayersDocument};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::render::PresentationCooked;

use super::{FuzzInput, FuzzTarget};

/// A document with one passages layer, settings, a section rule and a look.
const DOCUMENT: &str = r#"(
    version: 1,
    chunk_m: 64.0,
    geom: (max_floors: 5, wall_pad_m: 6.5),
    ground: Full,
    ground_tile_cells: 16,
    sections: (section_chord_m: 4.0, plug_length_m: 6.0),
    layers: [
        (
            id: "tubes",
            source: Passages(path: "demo.passages.ron"),
            fills: ["north_mouth"],
            cast_shadows: true,
            material: "materials/rock.mat.ron",
            look: (detail: Some((normal_displace: 0.0, noise_amplitude: 0.02, noise_scale: 0.9, seed: 7)), uv_tile_m: 4.0),
        ),
        (
            id: "arch",
            source: Gltf(path: "arch.glb", node: None, collision_node: Some("collision")),
            map_layer: 2,
        ),
    ],
)"#;

/// A small passage layout: one tube with a bend.
fn layout() -> TunnelLayout {
    TunnelLayout {
        map_id: "demo".into(),
        version: LAYOUT_VERSION,
        sample_step_m: SAMPLE_STEP_M,
        passages: vec![Passage {
            id: "core".into(),
            kind: PassageKind::Tube,
            width_m: 9.0,
            ceiling_m: 10.0,
            points: vec![[-40.0, -20.0, 0.0], [0.0, -20.0, 10.0], [40.0, -20.0, 0.0]],
        }],
        openings: Vec::new(),
        junctions: Vec::new(),
        crawlways: Vec::new(),
    }
}

fn document_fixture() -> String {
    DOCUMENT.into()
}

fn document_load(text: &str) -> Result<String, String> {
    let doc = MeshLayersDocument::from_ron(text).map_err(|e| format!("{e} ({})", e.code()))?;
    doc.to_ron().map_err(|e| e.to_string())
}

/// The layout imported from its own `do27-tunnels/1` export, so the sampled block is present.
fn passages_fixture() -> String {
    PassagesDocument::import_do27_json(&layout().export_json(None))
        .and_then(|doc| doc.to_ron())
        .unwrap_or_default()
}

fn passages_load(text: &str) -> Result<String, String> {
    let doc = PassagesDocument::from_ron(text).map_err(|e| format!("{e} ({})", e.code()))?;
    doc.to_ron().map_err(|e| e.to_string())
}

/// The layout cooked over a flat 4 x 4 heightfield.
///
/// The lattice and the sampler are both 40 m, above the tube roof. A missing sampler is height 0,
/// which would mark this tube covered and then fail `above_ground_outside_cut` against a lattice
/// that sits on the floor.
fn cooked_fixture() -> Vec<u8> {
    let passages = PassagesDocument::from_layout(layout())
        .to_ron()
        .unwrap_or_default();
    let ground = GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![17_664; 16],
    };
    let Some(grid) = HeightGrid::square(4, vec![17_664; 16]) else {
        return Vec::new();
    };
    let terrain = TerrainSampler::new(grid, ground.width_m).with_zero_counts(16_384);
    let read = |_: &str| -> Result<Vec<u8>, String> { Ok(passages.clone().into_bytes()) };
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"demo.passages.ron\"))])";
    cook(&CookInputs {
        document: doc.as_bytes(),
        ground: &ground,
        terrain: Some(&terrain),
        read_file: &read,
    })
    .ok()
    .and_then(|out| {
        CookedMeshLayers {
            chunks: chunks_fixture(),
            ..out.file
        }
        .encode()
        .ok()
    })
    .unwrap_or_default()
}

/// A chunk section (the terrain's layer chunks, ADR-8398, ADR-8576): one layer, one chunk.
fn chunks_fixture() -> Vec<u8> {
    use regia_terrain::{CdlodConfig, CookedChunkLod, CookedLayer, CookedLayerChunk, LayerChunkGrid};
    let config = CdlodConfig::default();
    regia_terrain::TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        chunk_importer: 0,
        grid: LayerChunkGrid::new(1280.0, 0, config.grid_size, config.range_factor, [0.0, 0.0], 64.0),
        layers: vec![CookedLayer {
            id: "tubes".into(),
            map_layer: 0,
            cast_shadows: true,
            collider: true,
            material: "materials/rock.mat.ron".into(),
            material_id: Some(regia_asset::AssetId::sub_asset(
                regia_asset::AssetId::default(),
                "materials/rock.mat.ron",
            )),
            chunks: vec![CookedLayerChunk {
                x: 0,
                z: 0,
                bounds: [[-10.0, -50.0, -10.0], [10.0, -30.0, 10.0]],
                lods: vec![CookedChunkLod {
                    mesh: regia_asset::AssetId::sub_asset(
                        regia_asset::AssetId::default(),
                        "meshlayers/tubes/0_0/lod0",
                    ),
                    triangles: 12,
                    bytes: 640,
                    error_m: 0.0,
                }],
            }],
        }],
    }
    .encode()
    .unwrap_or_default()
}

fn cooked_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    // An older file migrates first (v1 -> v2, the chunk section's `collider`, ADR-8872).
    let bytes = &regia_terrain::upgrade_cooked_mesh_layers(bytes)?;
    let file = CookedMeshLayers::decode(bytes).map_err(|e| format!("{e} ({})", e.code()))?;
    // The presentation section (the render meshes, ADR-8175) is checked too: a mutated one is refused by name.
    PresentationCooked::decode(&file.presentation).map_err(|e| format!("{e} ({})", e.code()))?;
    // So is the chunk section, the terrain crate's (ADR-8398, ADR-8576).
    regia_terrain::TerrainMeshLayersCooked::decode(&file.chunks)
        .map_err(|e| format!("{e} ({})", e.code()))?;
    file.encode().map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "terrain.mesh_layers",
            format: Some("terrain-mesh-layers"),
            format_version: MESH_LAYERS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: document_fixture,
                load: document_load,
            },
        },
        FuzzTarget {
            name: "terrain.passages",
            format: Some("terrain-mesh-layers"),
            format_version: MESH_LAYERS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: passages_fixture,
                load: passages_load,
            },
        },
        FuzzTarget {
            name: "terrain.mesh_layers_cooked",
            format: Some("terrain-mesh-layers-cooked"),
            format_version: COOKED_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: cooked_fixture,
                load: cooked_load,
            },
        },
    ]
}
