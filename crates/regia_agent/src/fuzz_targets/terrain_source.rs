// SPDX-License-Identifier: MPL-2.0
//! CT-18 bounded JSON/PNG authoring readers (ADR-8362), synthetic fixtures, and the two frozen
//! terrain source descriptors (ADR-8639): the count-raster layout (`terrain-raster-descriptor`,
//! ADR-8542) and the tile set (`terrain-source-tiles`, ADR-8595).
use regia_terrain::source::{
    ImportLimits, PngImportSettings, RASTER_DESCRIPTOR_VERSION, RasterDescriptor, TILE_SET_VERSION,
    TerrainSource, TileSetDescriptor,
};

use super::{FuzzInput, FuzzTarget};

const LIMITS: ImportLimits = ImportLimits {
    max_bytes: 16_384,
    max_samples: 64,
    max_json_depth: 8,
    max_unknown_values: 64,
};

fn json_fixture() -> String {
    r#"{"counts":[992,1000,1008,1024],"extra":{"label":[null,true,"seed"]},"schema":2,"grid":{"cols":2,"rows":2,"cell_m":3},"units":{"kind":"counts","meters_per_count":0.125,"zero_counts":1000}}"#.into()
}

fn json_load(text: &str) -> Result<String, String> {
    let source = TerrainSource::from_json_with_observer(text.as_bytes(), LIMITS, |p| {
        assert!(p.samples_charged <= LIMITS.max_samples);
        assert!(p.samples_stored <= p.samples_charged);
        assert!(p.sample_capacity <= LIMITS.max_samples);
        assert!(p.unknown_values <= LIMITS.max_unknown_values);
        assert!(p.max_depth <= LIMITS.max_json_depth);
        std::ops::ControlFlow::Continue(())
    })
    .map_err(|e| e.to_string())?;
    consume(&source);
    Ok(String::new())
}

fn consume(source: &TerrainSource) {
    assert!(source.counts().len() <= LIMITS.max_samples);
    let [cols, rows] = source.dimensions();
    assert_eq!(cols as usize * rows as usize, source.counts().len());
    assert!(source.heights_m().iter().all(|v| v.is_finite()));
}

fn png_fixture() -> Vec<u8> {
    fn encode() -> Result<Vec<u8>, png::EncodingError> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            encoder.add_text_chunk("terrain_zero_counts".into(), "1000".into())?;
            let mut writer = encoder.write_header()?;
            let raw: Vec<_> = [992u16, 1000, 1008, 1024]
                .into_iter()
                .flat_map(u16::to_be_bytes)
                .collect();
            writer.write_image_data(&raw)?;
            writer.finish()?;
        }
        Ok(bytes)
    }
    // Registry fixture validation reports an encoding failure as invalid seed.
    encode().unwrap_or_default()
}

fn png_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let source = TerrainSource::from_png(
        bytes,
        &PngImportSettings {
            meters_per_count: 0.125,
            cell_m: 3.0,
            ..Default::default()
        },
        LIMITS,
    )
    .map_err(|e| e.to_string())?;
    consume(&source);
    Ok(Vec::new())
}

/// The example of `docs/formats/terrain.md` § 14 (also the corpus fixture
/// `format-corpus/terrain-raster-descriptor/v1.json`).
fn raster_fixture() -> String {
    r#"{"raster_descriptor": 1, "spacing_m": [2.0, 0.5], "row0": "north", "col0": "west",
 "registration": "edge", "origin_m": [100.0, -40.0], "nodata_counts": 65535,
 "valid_mask": {"cols": 3, "rows": 2, "valid": [1, 1, 1, 1, 0, 1]}}
"#
    .into()
}

/// Parses and validates a raster descriptor (version first, unknown fields refused), then writes it
/// back with every field spelled out: the writer's canonical form.
fn raster_load(text: &str) -> Result<String, String> {
    let descriptor = RasterDescriptor::from_json(text.as_bytes(), LIMITS).map_err(|e| e.to_string())?;
    assert!(
        descriptor
            .valid_mask
            .as_ref()
            .is_none_or(|m| m.valid.len() <= LIMITS.max_samples)
    );
    serde_json::to_string(&descriptor).map_err(|e| e.to_string())
}

/// `docs/formats/terrain-source-tiles.example.json`, copied (checked by the registry test
/// `copied_fixtures_match_their_sources`).
fn tiles_fixture() -> String {
    include_str!("fixtures/terrain-source-tiles.example.json").into()
}

/// Validates a tile-set descriptor's structure (paths, bounds, overlap, coverage) without reading a
/// tile. There is no tile-set writer, so the output is empty.
fn tiles_load(text: &str) -> Result<String, String> {
    let descriptor =
        TileSetDescriptor::from_json(text.as_bytes(), ImportLimits::default()).map_err(|e| e.to_string())?;
    let coverage = descriptor.coverage().map_err(|e| e.to_string())?;
    assert_eq!(coverage.tiles, descriptor.tiles().len());
    assert!(coverage.tile_samples >= coverage.grid_samples);
    Ok(String::new())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "terrain.source_json",
            format: None,
            format_version: 2,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: json_fixture,
                load: json_load,
            },
        },
        FuzzTarget {
            name: "terrain.source_png",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: png_fixture,
                load: png_load,
            },
        },
        FuzzTarget {
            name: "terrain.raster_descriptor",
            format: Some("terrain-raster-descriptor"),
            format_version: RASTER_DESCRIPTOR_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: raster_fixture,
                load: raster_load,
            },
        },
        FuzzTarget {
            name: "terrain.source_tiles",
            format: Some("terrain-source-tiles"),
            format_version: TILE_SET_VERSION,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: tiles_fixture,
                load: tiles_load,
            },
        },
    ]
}
