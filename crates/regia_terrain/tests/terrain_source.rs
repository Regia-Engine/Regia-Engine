// SPDX-License-Identifier: MPL-2.0
//! Source import acceptance (ADR-8063); generated, game-independent fixtures.
use regia_terrain::source::{ImportLimits, PngImportSettings, TerrainSource};

fn png16(samples: &[u16], datum: Option<&str>) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        if let Some(value) = datum {
            encoder
                .add_text_chunk("terrain_zero_counts".into(), value.into())
                .expect("text");
        }
        let mut writer = encoder.write_header().expect("header");
        let raw: Vec<u8> = samples.iter().flat_map(|v| v.to_be_bytes()).collect();
        writer.write_image_data(&raw).expect("pixels");
        writer.finish().expect("complete PNG");
    }
    bytes
}

#[test]
fn zero_datum_png_imports_to_meters() {
    let settings = PngImportSettings {
        meters_per_count: 0.125,
        cell_m: 3.0,
        ..Default::default()
    };
    let source = TerrainSource::from_png(
        &png16(&[992, 1000, 1008, 1024], Some("1000")),
        &settings,
        ImportLimits::default(),
    )
    .expect("source");
    assert_eq!(source.heights_m(), vec![-1.0, 0.0, 1.0, 3.0]);
    assert_eq!(source.counts(), &[992, 1000, 1008, 1024]);
    assert_eq!(source.zero_counts(), 1000);
}

fn json_grid(cols: u32, rows: u32, counts: &[u16]) -> serde_json::Value {
    serde_json::json!({
        "schema": 2, "grid": {"cols": cols,"rows": rows,"cell_m": 3.0},
        "units": {"kind":"counts","meters_per_count":0.125,"zero_counts":1000},
        "counts": counts,
    })
}
fn parse(doc: &serde_json::Value) -> Result<TerrainSource, regia_terrain::source::SourceError> {
    TerrainSource::from_json(doc.to_string().as_bytes(), ImportLimits::default())
}

#[test]
fn schema2_keeps_rectangular_counts_origin_and_meters() {
    let mut doc = json_grid(4, 2, &[992, 1000, 1008, 1016, 1024, 1032, 1040, 1048]);
    let source = parse(&doc).expect("rectangular source");
    assert_eq!(source.dimensions(), [4, 2]);
    assert_eq!(source.sample0(), [-6.0, -3.0]);
    assert_eq!(source.cell_m(), 3.0);
    assert_eq!(source.heights_m(), vec![-1.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(source.height_m(u32::MAX, u32::MAX), 6.0);
    assert_eq!(source.heightfield().samples, source.counts());
    doc["grid"]["sample0_x_m"] = serde_json::json!(-4.5);
    doc["grid"]["sample0_z_m"] = serde_json::json!(-1.5);
    assert_eq!(parse(&doc).expect("cell centers").sample0(), [-4.5, -1.5]);
}

#[test]
fn json_and_png_yield_identical_counts_and_meter_bits() {
    let counts = [992, 1000, 1008, 1024];
    let json = parse(&json_grid(2, 2, &counts)).expect("json");
    let png = TerrainSource::from_png(
        &png16(&counts, Some("1000")),
        &PngImportSettings {
            meters_per_count: 0.125,
            cell_m: 3.0,
            ..Default::default()
        },
        ImportLimits::default(),
    )
    .expect("png");
    assert_eq!(json.heightfield(), png.heightfield());
    assert_eq!(json.sample0(), png.sample0());
    assert_eq!(
        json.heights_m().iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        png.heights_m().iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
}

#[test]
fn schema2_refuses_wrong_versions_axes_units_counts_and_overflow() {
    use serde_json::json;
    let good = json_grid(2, 2, &[1000; 4]);
    for (pointer, value) in [
        ("/schema", json!(1)),
        ("/schema", json!(4294967298u64)),
        ("/grid/cols", json!(1)),
        ("/grid/rows", json!(4294967298u64)),
        ("/grid/cell_m", json!(0)),
        ("/grid/cell_m", json!(-1)),
        ("/units/kind", json!("meters")),
        ("/units/meters_per_count", json!(0)),
        ("/units/meters_per_count", json!(-0.125)),
        ("/units/meters_per_count", json!(1e38)),
        ("/units/zero_counts", json!(65536)),
        ("/units/zero_counts", json!(4294968296u64)),
        ("/counts", json!([1, 2, 3])),
        ("/counts/0", json!(-1)),
        ("/counts/0", json!(65536)),
        ("/counts/0", json!(1.5)),
    ] {
        let mut bad = good.clone();
        *bad.pointer_mut(pointer).expect("field") = value;
        assert!(parse(&bad).is_err(), "accepted {bad}");
    }
    for (field, value) in [
        ("row0", json!("north")),
        ("col0", json!("east")),
        ("row0", json!(3)),
        ("sample0_x_m", json!(1e38)),
        ("sample0_z_m", json!(1e40)),
    ] {
        let mut bad = good.clone();
        bad["grid"][field] = value;
        assert!(parse(&bad).is_err(), "accepted {bad}");
    }
    let mut bad = good;
    bad.as_object_mut().expect("object").remove("schema");
    assert!(parse(&bad).is_err());
}

#[test]
fn budgets_refuse_encoded_and_decoded_sources() {
    let png = png16(&[1000; 4], Some("1000"));
    let json = json_grid(2, 2, &[1000; 4]).to_string();
    for limits in [
        ImportLimits {
            max_bytes: 8,
            ..Default::default()
        },
        ImportLimits {
            max_samples: 3,
            ..Default::default()
        },
    ] {
        assert!(TerrainSource::from_png(&png, &PngImportSettings::default(), limits).is_err());
        assert!(TerrainSource::from_json(json.as_bytes(), limits).is_err());
    }
    let mut doc = json_grid(2, 2, &[1000; 4]);
    doc["grid"]["cols"] = serde_json::json!(u32::MAX);
    doc["grid"]["rows"] = serde_json::json!(u32::MAX);
    assert!(parse(&doc).is_err());
}

#[test]
fn png_missing_datum_policy_is_explicit_and_malformed_is_never_a_fallback() {
    use regia_terrain::source::MissingDatum;
    let plain = png16(&[992, 1000, 1008, 1024], None);
    let mut settings = PngImportSettings {
        meters_per_count: 0.125,
        ..Default::default()
    };
    assert!(TerrainSource::from_png(&plain, &settings, ImportLimits::default()).is_err());
    settings.missing_datum = MissingDatum::CenterSample;
    let source = TerrainSource::from_png(&plain, &settings, ImportLimits::default()).expect("center");
    assert_eq!(source.heights_m(), vec![-4.0, -3.0, -2.0, 0.0]);
    settings.missing_datum = MissingDatum::Counts(1000);
    assert_eq!(
        TerrainSource::from_png(&plain, &settings, ImportLimits::default())
            .expect("datum")
            .zero_counts(),
        1000
    );
    for datum in ["bad", "-1", "65536", "4294968296", "1.5", ""] {
        assert!(
            TerrainSource::from_png(
                &png16(&[1000; 4], Some(datum)),
                &settings,
                ImportLimits::default()
            )
            .is_err(),
            "{datum}"
        );
    }
}

fn custom_png(
    color: png::ColorType,
    depth: png::BitDepth,
    pixels: &[u8],
    texts: &[(&str, &str)],
    after: bool,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(color);
        encoder.set_depth(depth);
        if !after {
            for &(key, value) in texts {
                encoder.add_text_chunk(key.into(), value.into()).expect("text");
            }
        }
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(pixels).expect("pixels");
        if after {
            for &(key, value) in texts {
                writer
                    .write_text_chunk(&png::text_metadata::TEXtChunk::new(key, value))
                    .expect("text after image");
            }
        }
        writer.finish().expect("finish");
    }
    bytes
}

#[test]
fn png_accepts_eight_bit_grayscale_expands_counts_and_reads_text_after_idat() {
    let bytes = custom_png(
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &[0, 1, 2, 255],
        &[("terrain_zero_counts", "257")],
        true,
    );
    let source = TerrainSource::from_png(&bytes, &PngImportSettings::default(), ImportLimits::default())
        .expect("gray8");
    assert_eq!(source.counts(), &[0, 257, 514, 65535]);
    assert_eq!(source.heights_m(), vec![-257.0, 0.0, 257.0, 65278.0]);
}

#[test]
fn png_refuses_rgb_alpha_bad_streams_duplicate_metadata_and_invalid_settings() {
    for (color, pixels) in [
        (png::ColorType::Rgb, vec![0; 12]),
        (png::ColorType::GrayscaleAlpha, vec![0; 8]),
    ] {
        let bytes = custom_png(
            color,
            png::BitDepth::Eight,
            &pixels,
            &[("terrain_zero_counts", "0")],
            false,
        );
        assert!(
            TerrainSource::from_png(&bytes, &PngImportSettings::default(), ImportLimits::default()).is_err()
        );
    }
    let duplicate = custom_png(
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &[0; 4],
        &[("terrain_zero_counts", "0"), ("terrain_zero_counts", "0")],
        false,
    );
    assert!(
        TerrainSource::from_png(&duplicate, &PngImportSettings::default(), ImportLimits::default()).is_err()
    );
    let valid = png16(&[1000; 4], Some("1000"));
    for bytes in [
        b"not png".as_slice(),
        &valid[..valid.len() / 2],
        &valid[..valid.len() - 5],
    ] {
        assert!(
            TerrainSource::from_png(bytes, &PngImportSettings::default(), ImportLimits::default()).is_err()
        );
    }
    for settings in [
        PngImportSettings {
            cell_m: 0.0,
            ..Default::default()
        },
        PngImportSettings {
            meters_per_count: f32::INFINITY,
            ..Default::default()
        },
        PngImportSettings {
            sample0: Some([f32::NAN, 0.0]),
            ..Default::default()
        },
    ] {
        assert!(TerrainSource::from_png(&valid, &settings, ImportLimits::default()).is_err());
    }
}

#[test]
fn custom_datum_keyword_handles_legacy_metadata_without_hard_coded_game_names() {
    let bytes = custom_png(
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &[0, 1, 2, 3],
        &[("legacy_datum", "257")],
        false,
    );
    let settings = PngImportSettings {
        datum_keyword: "legacy_datum".into(),
        ..Default::default()
    };
    assert_eq!(
        TerrainSource::from_png(&bytes, &settings, ImportLimits::default())
            .expect("legacy")
            .zero_counts(),
        257
    );
}

#[test]
fn overview_is_north_up_repeatable_and_leaves_source_unchanged() {
    use regia_terrain::overview::{OverviewSettings, bake_overview};
    let source = parse(&json_grid(
        4,
        2,
        &[1000, 1000, 1000, 1000, 1080, 1080, 1080, 1080],
    ))
    .expect("source");
    let counts = source.counts().to_vec();
    let settings = OverviewSettings {
        size: [4, 2],
        relief: 0.0,
        blur_passes: 0,
        ..Default::default()
    };
    let first = bake_overview(&source, &settings).expect("bake");
    assert!(first.rgb()[0] > first.rgb()[12], "north is brighter");
    assert_eq!(first, bake_overview(&source, &settings).expect("again"));
    assert_eq!(
        first.encode_png().expect("encode"),
        first.encode_png().expect("same bytes")
    );
    assert_eq!(source.counts(), counts);
    let png = first.encode_png().expect("png");
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .expect("read");
    let mut pixels = vec![0; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut pixels).expect("pixels");
    assert_eq!(first.rgb(), pixels);
}

#[test]
fn overview_flat_field_contours_frame_and_limits() {
    use regia_terrain::overview::{OverviewSettings, bake_overview};
    let flat = parse(&json_grid(2, 2, &[1000; 4])).expect("flat");
    let settings = OverviewSettings {
        size: [6, 4],
        ..Default::default()
    };
    let image = bake_overview(&flat, &settings).expect("flat bake");
    assert!(image.rgb().iter().all(|&v| v == image.rgb()[0]));
    let framed = bake_overview(
        &flat,
        &OverviewSettings {
            frame_pixels: 1,
            ..settings.clone()
        },
    )
    .expect("frame");
    assert_eq!(framed.rgb()[0], 128);
    assert_eq!(framed.rgb()[(6 + 1) * 3], image.rgb()[(6 + 1) * 3]);
    let source = parse(&json_grid(2, 2, &[1000, 1008, 1016, 1024])).expect("slope");
    let base = OverviewSettings {
        size: [2, 2],
        blur_passes: 0,
        relief: 0.0,
        ..Default::default()
    };
    let plain = bake_overview(&source, &base).expect("plain");
    let contours = bake_overview(
        &source,
        &OverviewSettings {
            contour_step_m: Some(1.0),
            ..base.clone()
        },
    )
    .expect("contours");
    assert_eq!(contours.rgb()[0], plain.rgb()[0]);
    assert!(contours.rgb()[3] < plain.rgb()[3]);
    for bad in [
        OverviewSettings {
            size: [1, 2],
            ..base.clone()
        },
        OverviewSettings {
            max_pixels: 3,
            ..base.clone()
        },
        OverviewSettings {
            relief: f32::NAN,
            ..base.clone()
        },
        OverviewSettings {
            light: [0.0; 3],
            ..base.clone()
        },
        OverviewSettings {
            blur_passes: 9,
            ..base.clone()
        },
        OverviewSettings {
            contour_step_m: Some(0.0),
            ..base.clone()
        },
        OverviewSettings {
            frame_pixels: 2,
            ..base
        },
    ] {
        assert!(bake_overview(&source, &bad).is_err());
    }
}

#[test]
fn overview_rectangular_gradient_uses_independent_world_axes() {
    use regia_terrain::overview::{OverviewSettings, bake_overview};
    // Same eastward gradient; rows are repeated, so changing map aspect cannot tilt X.
    let a = parse(&json_grid(
        4,
        2,
        &[1000, 1008, 1016, 1024, 1000, 1008, 1016, 1024],
    ))
    .expect("a");
    let b = parse(&json_grid(
        4,
        4,
        &[
            1000, 1008, 1016, 1024, 1000, 1008, 1016, 1024, 1000, 1008, 1016, 1024, 1000, 1008, 1016, 1024,
        ],
    ))
    .expect("b");
    let settings = OverviewSettings {
        size: [4, 4],
        blur_passes: 0,
        ..Default::default()
    };
    assert_eq!(
        bake_overview(&a, &settings).expect("a"),
        bake_overview(&b, &settings).expect("b")
    );
    let west = bake_overview(
        &a,
        &OverviewSettings {
            light: [-1.0, 1.0, 0.0],
            ..settings.clone()
        },
    )
    .expect("west");
    let east = bake_overview(
        &a,
        &OverviewSettings {
            light: [1.0, 1.0, 0.0],
            ..settings
        },
    )
    .expect("east");
    assert!(west.rgb()[0] > east.rgb()[0], "eastward slope faces west");
}

#[test]
fn overview_fractional_resize_does_not_amplify_roundoff_on_flat_terrain() {
    use regia_terrain::overview::{OverviewSettings, bake_overview};
    let mut doc = json_grid(3, 2, &[1007; 6]);
    doc["units"]["meters_per_count"] = serde_json::json!(0.1);
    let source = parse(&doc).expect("nonbinary flat height");
    for passes in [0, 1, 8] {
        let image = bake_overview(
            &source,
            &OverviewSettings {
                size: [7, 5],
                blur_passes: passes,
                ..Default::default()
            },
        )
        .expect("bake");
        assert!(
            image.rgb().iter().all(|&v| v == image.rgb()[0]),
            "passes {passes}"
        );
    }
}

#[test]
fn overview_unit_slope_survives_area_downsampling() {
    use regia_terrain::overview::{OverviewSettings, bake_overview};
    // A fine ramp and its exact pair averages have the same meter-space slope.
    let fine = parse(&json_grid(
        4,
        2,
        &[1000, 1008, 1016, 1024, 1000, 1008, 1016, 1024],
    ))
    .expect("fine");
    let mut coarse_doc = json_grid(2, 2, &[1004, 1020, 1004, 1020]);
    coarse_doc["grid"]["cell_m"] = serde_json::json!(6.0);
    let coarse = parse(&coarse_doc).expect("coarse");
    let settings = OverviewSettings {
        size: [2, 2],
        blur_passes: 0,
        ..Default::default()
    };
    assert_eq!(
        bake_overview(&fine, &settings).expect("fine"),
        bake_overview(&coarse, &settings).expect("coarse")
    );
}

#[test]
#[ignore = "CPU bake measurement; no timing threshold or golden"]
fn overview_cpu_cost() {
    use regia_terrain::overview::{OverviewSettings, bake_overview};
    let width = 1024u32;
    let height = 512u32;
    let counts: Vec<u16> = (0..width * height)
        .map(|i| {
            let x = i % width;
            let z = i / width;
            (1000 + x + z + ((x / 32 + z / 32) % 2) * 100) as u16
        })
        .collect();
    let source = parse(&json_grid(width, height, &counts)).expect("source");
    let settings = OverviewSettings {
        size: [1024, 1024],
        contour_step_m: Some(8.0),
        ..Default::default()
    };
    let begin = std::time::Instant::now();
    let image = bake_overview(&source, &settings).expect("bake");
    let bake = begin.elapsed();
    let encode_start = std::time::Instant::now();
    let png = image.encode_png().expect("PNG");
    eprintln!(
        "1024x512 source -> 1024x1024 RGB: bake={bake:?}, encode={:?}, png_bytes={}",
        encode_start.elapsed(),
        png.len()
    );
    assert_eq!(image.dimensions(), [1024, 1024]);
}

#[test]
fn png_rejects_compressed_datum_and_animation_even_with_a_fallback() {
    use regia_terrain::source::MissingDatum;
    for animated in [false, true] {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            if animated {
                encoder.set_animated(1, 0).expect("animation");
            } else {
                encoder
                    .add_ztxt_chunk("terrain_zero_counts".into(), "0".into())
                    .expect("compressed text");
            }
            let mut writer = encoder.write_header().expect("header");
            writer.write_image_data(&[0; 4]).expect("pixels");
            writer.finish().expect("finish");
        }
        let settings = PngImportSettings {
            missing_datum: MissingDatum::Counts(0),
            ..Default::default()
        };
        let error = TerrainSource::from_png(&bytes, &settings, ImportLimits::default()).expect_err("refuse");
        assert!(
            error
                .to_string()
                .contains(if animated { "nonanimated" } else { "uncompressed" }),
            "{error}"
        );
    }
}

#[test]
fn schema2_decimal_tokens_round_directly_to_f32() {
    // Just above the exact midpoint 1 + 2^-24. Parsing through f64 first rounds
    // it to the midpoint, then ties to 1.0; direct f32 must choose the next value.
    let decimal = "1.0000000596046447753906250000001";
    let text = format!(
        r#"{{"schema":2,
        "grid":{{"cols":2,"rows":2,"cell_m":{decimal},"sample0_x_m":{decimal}}},
        "units":{{"kind":"counts","meters_per_count":{decimal},"zero_counts":0}},
        "counts":[0,1,2,3]}}"#
    );
    let source = TerrainSource::from_json(text.as_bytes(), ImportLimits::default()).expect("source");
    assert_eq!(source.cell_m().to_bits(), 0x3f80_0001);
    assert_eq!(source.meters_per_count().to_bits(), 0x3f80_0001);
    assert_eq!(source.sample0()[0].to_bits(), 0x3f80_0001);
    assert_eq!(source.height_m(1, 0).to_bits(), 0x3f80_0001);
}
