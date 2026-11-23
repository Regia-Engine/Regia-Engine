// SPDX-License-Identifier: MPL-2.0
//! ADR-8595 (CT-22): the source tile-set descriptor and its bounded reader. Structure errors are
//! found from the descriptor alone; tile files are checked against it when decoded; shared
//! samples must agree; the file's tile order never matters; placement math is pinned bit for bit.

use std::ops::ControlFlow;

use regia_terrain::source::{ImportLimits, SourceError, SourceTileRef, TileSetDescriptor, TileSetReader};
use serde_json::{Value, json};

fn descriptor(tiles: Value) -> Value {
    json!({"terrain_source_tiles": 1,
        "grid": {"cols": 5, "rows": 4, "cell_m": 2.0, "sample0_m": [-4.0, -3.0]},
        "units": {"meters_per_count": 0.25, "zero_counts": 1000},
        "tiles": tiles})
}

fn parse(doc: &Value) -> Result<TileSetDescriptor, SourceError> {
    TileSetDescriptor::from_json(doc.to_string().as_bytes(), ImportLimits::default())
}

fn code(doc: &Value) -> &'static str {
    parse(doc).expect_err("refused").code()
}

fn count(c: u32, r: u32) -> u16 {
    (1000 + c * 7 + r * 31) as u16
}

/// A schema-2 JSON tile of the global grid above, placed at its origin.
fn tile_json(origin: [u32; 2], size: [u32; 2], bump: Option<([u32; 2], u16)>) -> Vec<u8> {
    let counts: Vec<u16> = (0..size[1])
        .flat_map(|r| (0..size[0]).map(move |c| [origin[0] + c, origin[1] + r]))
        .map(|g| count(g[0], g[1]) + bump.filter(|(at, _)| *at == g).map_or(0, |(_, v)| v))
        .collect();
    json!({"schema": 2,
        "grid": {"cols": size[0], "rows": size[1], "cell_m": 2.0,
                 "sample0_x_m": -4.0 + 2.0 * origin[0] as f32, "sample0_z_m": -3.0 + 2.0 * origin[1] as f32},
        "units": {"kind": "counts", "meters_per_count": 0.25, "zero_counts": 1000},
        "counts": counts})
    .to_string()
    .into_bytes()
}

fn halves() -> Value {
    json!([{"path": "e.json", "origin": [2, 0], "size": [3, 4]},
           {"path": "w.json", "origin": [0, 0], "size": [3, 4]}])
}

/// Assembles the grid; `bump` = (tile origin, global sample, added count) edits one tile's file.
fn read_all(d: &TileSetDescriptor, bump: Option<([u32; 2], [u32; 2], u16)>) -> Result<Vec<u16>, SourceError> {
    let mut reader = TileSetReader::new(
        d,
        ImportLimits::default(),
        |t: &SourceTileRef| {
            let edit = bump
                .filter(|(tile, _, _)| *tile == t.origin)
                .map(|(_, at, v)| (at, v));
            Ok(tile_json(t.origin, t.size, edit))
        },
        |_, _| ControlFlow::Continue(()),
    );
    reader.assemble(2).map(|s| s.counts().to_vec())
}

#[test]
fn structure_is_validated_from_the_descriptor_alone() {
    let d = parse(&descriptor(halves())).expect("valid");
    assert_eq!(d.tiles()[0].id(), "r0_c0", "sorted by origin");
    let coverage = d.coverage().expect("coverage");
    assert_eq!(
        [
            coverage.tiles,
            coverage.overlapping_pairs,
            coverage.max_tile_samples
        ],
        [2, 1, 12]
    );
    assert_eq!([coverage.grid_samples, coverage.tile_samples], [20, 24]);

    let mut future = descriptor(halves());
    future["terrain_source_tiles"] = json!(2);
    assert_eq!(code(&future), "terrain_tiles_version");
    assert_eq!(code(&descriptor(json!([]))), "terrain_tiles_limit");
    let many: Vec<Value> = (0..4097)
        .map(|_| json!({"path": "a.json", "origin": [0, 0], "size": [5, 4]}))
        .collect();
    assert_eq!(code(&descriptor(Value::Array(many))), "terrain_tiles_limit");
    for bad in [
        "../a.json",
        "/a.json",
        "a\\b.json",
        "c:a.json",
        "a/./b.json",
        "a.raw",
        "",
    ] {
        let doc = descriptor(json!([{"path": bad, "origin": [0, 0], "size": [5, 4]}]));
        assert_eq!(code(&doc), "terrain_tiles_path", "{bad}");
    }
    let outside = json!([{"path": "a.json", "origin": [0, 0], "size": [3, 4]},
                         {"path": "b.json", "origin": [2, 0], "size": [4, 4]}]);
    assert_eq!(code(&descriptor(outside)), "terrain_tiles_bounds");
    let thin = json!([{"path": "a.json", "origin": [0, 0], "size": [5, 1]}]);
    assert_eq!(code(&descriptor(thin)), "terrain_tiles_bounds");
    let wide = json!([{"path": "a.json", "origin": [0, 0], "size": [4, 4]},
                      {"path": "b.json", "origin": [2, 0], "size": [3, 4]}]);
    assert_eq!(code(&descriptor(wide)), "terrain_tiles_overlap");
    let mut wide_ok = descriptor(json!([{"path": "a.json", "origin": [0, 0], "size": [4, 4]},
                                        {"path": "b.json", "origin": [2, 0], "size": [3, 4]}]));
    wide_ok["max_overlap"] = json!(2);
    parse(&wide_ok).expect("a declared two-sample border");
    let gap = json!([{"path": "a.json", "origin": [0, 0], "size": [2, 4]},
                     {"path": "b.json", "origin": [3, 0], "size": [2, 4]}]);
    let error = parse(&descriptor(gap)).expect_err("gap");
    assert_eq!(error.code(), "terrain_tiles_gap");
    assert!(error.0.contains("(col 2, row 0)"), "{}", error.0);
    let corner = json!([{"path": "a.json", "origin": [0, 0], "size": [5, 2]},
                        {"path": "b.json", "origin": [0, 2], "size": [3, 2]}]);
    assert_eq!(code(&descriptor(corner)), "terrain_tiles_gap");
    let mut unknown = descriptor(halves());
    unknown["extra"] = json!(1);
    assert_eq!(code(&unknown), "height_io_decode");
    let tiny = TileSetDescriptor::from_json(
        descriptor(halves()).to_string().as_bytes(),
        ImportLimits {
            max_samples: 11,
            ..ImportLimits::default()
        },
    )
    .expect_err("tile over the budget");
    assert_eq!(tiny.code(), "height_source_limit_samples");
}

#[test]
fn tiles_assemble_exactly_and_shared_samples_must_agree() {
    let d = parse(&descriptor(halves())).expect("valid");
    let counts = read_all(&d, None).expect("assembled");
    let expected: Vec<u16> = (0..4).flat_map(|r| (0..5).map(move |c| count(c, r))).collect();
    assert_eq!(counts, expected);
    let seam = read_all(&d, Some(([2, 0], [2, 3], 1))).expect_err("seam");
    assert_eq!(seam.code(), "terrain_tiles_seam");
    assert!(
        seam.0.contains("(col 2, row 3)") && seam.0.contains("r0_c0") && seam.0.contains("r0_c2"),
        "{}",
        seam.0
    );
    // An interior bump is just a different height, not a seam.
    let interior = read_all(&d, Some(([0, 0], [0, 1], 1))).expect("interior edit");
    assert_eq!(interior[5], count(0, 1) + 1);
}

#[test]
fn decoded_tiles_are_checked_against_the_descriptor() {
    let d = parse(&descriptor(halves())).expect("valid");
    let limits = ImportLimits::default();
    let mut doc: Value = serde_json::from_slice(&tile_json([0, 0], [3, 4], None)).expect("json");
    d.decode_tile(0, doc.to_string().as_bytes(), limits)
        .expect("good tile");
    doc["grid"]["sample0_x_m"] = json!(-3.0);
    let e = d
        .decode_tile(0, doc.to_string().as_bytes(), limits)
        .expect_err("half a cell off");
    assert_eq!(e.code(), "terrain_tiles_misregistered");
    doc["grid"]["sample0_x_m"] = json!(-3.9995);
    d.decode_tile(0, doc.to_string().as_bytes(), limits)
        .expect("within 1 mm");
    doc["grid"]["sample0_x_m"] = json!(-4.0);
    doc["units"]["zero_counts"] = json!(999);
    let e = d
        .decode_tile(0, doc.to_string().as_bytes(), limits)
        .expect_err("datum");
    assert_eq!(e.code(), "terrain_tiles_mapping");
    let short = tile_json([0, 0], [3, 3], None);
    let e = d.decode_tile(0, &short, limits).expect_err("size");
    assert_eq!(e.code(), "terrain_tiles_dimensions");
}

#[test]
fn file_order_never_changes_the_tiles_or_the_samples() {
    let a = parse(&descriptor(halves())).expect("a");
    let mut reversed = halves();
    reversed.as_array_mut().expect("array").reverse();
    let b = parse(&descriptor(reversed)).expect("b");
    assert_eq!(a, b);
    assert_eq!(read_all(&a, None).expect("a"), read_all(&b, None).expect("b"));
}

#[test]
fn reader_holds_only_the_tiles_a_rectangle_needs_and_can_cancel() {
    // 4x4 tiles of 4x4 samples sharing one-sample borders: a 13x13 grid.
    let tiles: Vec<Value> = (0..4)
        .flat_map(|ty| (0..4).map(move |tx| json!({"path": format!("t{tx}{ty}.json"), "origin": [tx * 3, ty * 3], "size": [4, 4]})))
        .collect();
    let mut doc = descriptor(Value::Array(tiles));
    doc["grid"]["cols"] = json!(13);
    doc["grid"]["rows"] = json!(13);
    let d = parse(&doc).expect("valid");
    let mut reader = TileSetReader::new(
        &d,
        ImportLimits::default(),
        |t: &SourceTileRef| Ok(tile_json(t.origin, t.size, None)),
        |_, _| ControlFlow::Continue(()),
    );
    let mut seen = 0usize;
    reader
        .for_each_rect(3, |_, samples| {
            seen += samples.len();
            Ok(())
        })
        .expect("read");
    assert_eq!(seen, 169);
    let stats = reader.stats();
    assert!(stats.peak_charged_samples <= 4 * 16, "{stats:?}");
    assert_eq!(stats.samples_read, 169);
    let mut calls = 0;
    let mut cancelling = TileSetReader::new(
        &d,
        ImportLimits::default(),
        |t: &SourceTileRef| Ok(tile_json(t.origin, t.size, None)),
        |_, decoded| {
            calls += 1;
            if decoded == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    let e = cancelling.assemble(16).expect_err("cancelled");
    assert_eq!(e.code(), "height_source_cancelled");
    assert_eq!(calls, 3);
}

/// Tile placement uses the mapping's f64-once-rounded formula; pinned bits (computed
/// independently) agree on x86-64 and aarch64.
#[test]
fn x86_64_and_aarch64_tile_origin_fixture_bits() {
    let doc = json!({"terrain_source_tiles": 1,
        "grid": {"cols": 2048, "rows": 1024, "cell_m": 0.3125, "sample0_m": [-1000.1, 37.7]},
        "units": {"meters_per_count": 0.25, "zero_counts": 1000},
        "tiles": [{"path": "a.png", "origin": [0, 0], "size": [2048, 1024]}]});
    let d = parse(&doc).expect("valid");
    let m = d.mapping();
    for ((c, r), [x, z]) in [
        ((0, 0), [0xc47a_0666_u32, 0x4216_cccd]),
        ((1024, 0), [0xc42a_0666, 0x4216_cccd]),
        ((2047, 1023), [0xc3b4_34cc, 0x43b2_b19a]),
        ((333, 777), [0xc460_0266, 0x438c_419a]),
    ] {
        let p = m.sample_world(c, r);
        assert_eq!([p[0].to_bits(), p[1].to_bits()], [x, z], "({c}, {r})");
    }
}

#[test]
fn the_tile_set_schema_names_every_descriptor_field() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats");
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("terrain-source-tiles.schema.json")).expect("schema"),
    )
    .expect("schema json");
    let example_text =
        std::fs::read_to_string(root.join("terrain-source-tiles.example.json")).expect("example");
    let example: Value = serde_json::from_str(&example_text).expect("example json");
    parse(&example).expect("the example is a valid descriptor");
    let levels = [
        (&schema["properties"], &example),
        (&schema["properties"]["grid"]["properties"], &example["grid"]),
        (&schema["properties"]["units"]["properties"], &example["units"]),
        (
            &schema["properties"]["tiles"]["items"]["properties"],
            &example["tiles"][0],
        ),
    ];
    for (properties, value) in levels {
        let mut named: Vec<&String> = properties.as_object().expect("properties").keys().collect();
        let mut fields: Vec<&String> = value.as_object().expect("object").keys().collect();
        named.sort();
        fields.sort();
        assert_eq!(named, fields, "the example uses every schema field");
    }
    // Every level refuses an unknown field, as the schema's additionalProperties: false says.
    for pointer in ["", "/grid", "/units", "/tiles/0"] {
        let mut doc = example.clone();
        doc.pointer_mut(pointer).expect("level")["unknown_field"] = json!(1);
        assert_eq!(code(&doc), "height_io_decode", "{pointer}");
    }
}
