// SPDX-License-Identifier: MPL-2.0
//! CT-18: reject hostile sources before growing sample storage past policy.
use regia_terrain::source::{ImportLimits, TerrainSource};

const GRID: &str = r#""grid":{"cols":2,"rows":2,"cell_m":1}"#;
const UNITS: &str = r#""units":{"kind":"counts","meters_per_count":0.125,"zero_counts":0}"#;

fn limits() -> ImportLimits {
    ImportLimits {
        max_samples: 4,
        ..Default::default()
    }
}

#[test]
fn refuses_fifth_sample_before_decoding_it_in_either_field_order() {
    // An invalid fifth token distinguishes early budget refusal from decoding the
    // complete Vec and only then checking dimensions. The unread suffix is large.
    let counts = format!("[0,1,2,3,{{}},{}0]", "0,".repeat(999_995));
    for text in [
        format!(r#"{{"schema":2,{GRID},{UNITS},"counts":{counts}}}"#),
        format!(r#"{{"counts":{counts},{UNITS},{GRID},"schema":2}}"#),
    ] {
        let error = TerrainSource::from_json(text.as_bytes(), limits()).expect_err("sample budget");
        assert!(error.to_string().contains("max_samples"), "{error}");
    }
}

#[test]
fn deeply_nested_unknown_fields_are_bounded_at_every_object() {
    let nested = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    for text in [
        format!(r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3],"extra":{nested}}}"#),
        format!(
            r#"{{"schema":2,"grid":{{"cols":2,"rows":2,"cell_m":1,"extra":{nested}}},{UNITS},"counts":[0,1,2,3]}}"#
        ),
        format!(
            r#"{{"schema":2,{GRID},"units":{{"kind":"counts","meters_per_count":1,"zero_counts":0,"extra":{nested}}},"counts":[0,1,2,3]}}"#
        ),
    ] {
        assert!(TerrainSource::from_json(text.as_bytes(), limits()).is_err());
    }
}

#[test]
fn wide_unknown_data_is_bounded() {
    let text = format!(
        r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3],"extra":[{}0]}}"#,
        "0,".repeat(65_536)
    );
    assert!(TerrainSource::from_json(text.as_bytes(), limits()).is_err());
}

#[test]
fn field_order_preserves_sample_and_meter_bits() {
    let a = format!(r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,65535]}}"#);
    let b = format!(r#"{{"counts":[0,1,2,65535],{UNITS},{GRID},"schema":2}}"#);
    let a = TerrainSource::from_json(a.as_bytes(), limits()).expect("dimensions first");
    let b = TerrainSource::from_json(b.as_bytes(), limits()).expect("counts first");
    assert_eq!(a.counts(), b.counts());
    assert_eq!(
        a.heights_m().iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        b.heights_m().iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
}

#[test]
fn truncated_parse_never_returns_a_partial_source() {
    let text = format!(r#"{{"counts":[0,1,2,3],{GRID},{UNITS},"schema":2}}"#);
    for end in 0..text.len() {
        assert!(
            TerrainSource::from_json(&text.as_bytes()[..end], limits()).is_err(),
            "end {end}"
        );
    }
}

#[test]
fn million_counts_charge_and_store_only_four_samples() {
    use regia_terrain::source::JsonImportProgress;
    use std::ops::ControlFlow;
    let counts = format!("[{}0]", "0,".repeat(999_999));
    for text in [
        format!(r#"{{"schema":2,{GRID},{UNITS},"counts":{counts}}}"#),
        format!(r#"{{"counts":{counts},{UNITS},{GRID},"schema":2}}"#),
    ] {
        let mut progress = JsonImportProgress::default();
        let mut callbacks = 0;
        let error = TerrainSource::from_json_with_observer(text.as_bytes(), limits(), |current| {
            progress = current;
            callbacks += 1;
            ControlFlow::Continue(())
        })
        .expect_err("sample cap");
        assert_eq!(error.code(), "height_source_limit_samples");
        assert_eq!(progress.samples_charged, 4);
        assert_eq!(progress.samples_stored, 4);
        assert_eq!(progress.sample_capacity, 4);
        assert_eq!(callbacks, 9, "initial + charge/store per accepted sample");
    }
}

#[test]
fn cancellation_at_every_callback_never_publishes_partial_source() {
    use std::ops::ControlFlow;
    let text = format!(r#"{{"counts":[0,1,2,3],{GRID},{UNITS},"schema":2}}"#);
    let mut steps = 0;
    TerrainSource::from_json_with_observer(text.as_bytes(), limits(), |_| {
        steps += 1;
        ControlFlow::Continue(())
    })
    .expect("complete");
    assert_eq!(steps, 10);
    for cancel_at in 1..=steps {
        let mut current = 0;
        let error = TerrainSource::from_json_with_observer(text.as_bytes(), limits(), |_| {
            current += 1;
            if current == cancel_at {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .expect_err("cancelled");
        assert_eq!(error.code(), "height_source_cancelled");
        assert_eq!(current, cancel_at);
    }
}

#[test]
fn metadata_counter_and_depth_are_hard_bounded() {
    use regia_terrain::source::JsonImportProgress;
    use std::ops::ControlFlow;
    let mut progress = JsonImportProgress::default();
    let text = format!(
        r#"{{"extra":[{}0],"schema":2,{GRID},{UNITS},"counts":[0,1,2,3]}}"#,
        "0,".repeat(100)
    );
    let error = TerrainSource::from_json_with_observer(
        text.as_bytes(),
        ImportLimits {
            max_unknown_values: 5,
            ..limits()
        },
        |p| {
            progress = p;
            ControlFlow::Continue(())
        },
    )
    .expect_err("metadata");
    assert_eq!(error.code(), "height_source_limit_metadata");
    assert_eq!(progress.unknown_values, 5);
    assert_eq!(progress.samples_stored, 0);
    assert_eq!(progress.sample_capacity, 0);
    assert!(progress.max_depth <= 3);

    let text = format!(r#"{{"extra":{}0{}}}"#, "[".repeat(1000), "]".repeat(1000));
    let error = TerrainSource::from_json_with_observer(
        text.as_bytes(),
        ImportLimits {
            max_json_depth: 6,
            ..limits()
        },
        |p| {
            progress = p;
            ControlFlow::Continue(())
        },
    )
    .expect_err("depth");
    assert_eq!(error.code(), "height_source_limit_depth");
    assert_eq!(progress.max_depth, 6);
    assert_eq!(progress.samples_stored, 0);
}

#[test]
fn dimensions_and_allocation_are_checked_before_counts_when_available() {
    use std::ops::ControlFlow;
    for (cols, rows) in [(u32::MAX, u32::MAX), (2, 3), (1, 4), (0, 0)] {
        let text = format!(
            r#"{{"grid":{{"cols":{cols},"rows":{rows},"cell_m":1}},"counts":[0,1,2,3],{UNITS},"schema":2}}"#
        );
        let mut stored = 0;
        let error = TerrainSource::from_json_with_observer(text.as_bytes(), limits(), |p| {
            stored = p.samples_stored;
            ControlFlow::Continue(())
        })
        .expect_err("dimensions");
        assert_eq!(stored, 0);
        if cols >= 2 && rows >= 2 {
            assert_eq!(error.code(), "height_source_limit_samples");
        }
    }
}

#[test]
fn caller_cannot_raise_engine_ceilings() {
    let unlimited = ImportLimits {
        max_bytes: usize::MAX,
        max_samples: usize::MAX,
        max_json_depth: usize::MAX,
        max_unknown_values: usize::MAX,
    };
    assert_eq!(unlimited.effective(), ImportLimits::HARD_MAX);
    let text = format!(
        r#"{{"grid":{{"cols":4294967295,"rows":4294967295,"cell_m":1}},"counts":[],{UNITS},"schema":2}}"#
    );
    assert_eq!(
        TerrainSource::from_json(text.as_bytes(), unlimited)
            .expect_err("hard samples")
            .code(),
        "height_source_limit_samples"
    );
    assert_eq!(unlimited.restricted_by(limits()).max_samples, 4);
}

#[test]
fn duplicate_and_malformed_known_fields_never_escape_validation() {
    let good = format!(r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3]}}"#);
    for text in [
        good.replace("\"schema\":2", "\"schema\":2,\"schema\":2"),
        good.replace("\"counts\":[0,1,2,3]", "\"counts\":[0,1,2,3],\"counts\":[]"),
        good.replace("\"cols\":2", "\"cols\":2,\"cols\":2"),
        good.replace(
            "\"cell_m\":1",
            "\"cell_m\":1,\"sample0_x_m\":null,\"sample0_x_m\":0",
        ),
        good.replace("\"zero_counts\":0", "\"zero_counts\":0,\"zero_counts\":0"),
        good.replace("[0,1,2,3]", "[0,1,2,-1]"),
        good.replace("[0,1,2,3]", "[0,1,2,65536]"),
        good.replace("[0,1,2,3]", "[0,1,2,1.5]"),
        good.replace("[0,1,2,3]", "null"),
        format!("{good} false"),
    ] {
        assert!(
            TerrainSource::from_json(text.as_bytes(), limits()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn png_sample_limit_still_precedes_pixel_decode() {
    use regia_terrain::source::{MissingDatum, PngImportSettings};
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 3, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(&[0; 6]).expect("image");
        writer.finish().expect("end");
    }
    let data = bytes.windows(4).position(|w| w == b"IDAT").expect("IDAT") + 4;
    bytes[data] ^= 0xff; // Invalid deflate/header and CRC, reached only at pixel decode.
    let settings = PngImportSettings {
        missing_datum: MissingDatum::Counts(0),
        ..Default::default()
    };
    let error = TerrainSource::from_png(&bytes, &settings, limits()).expect_err("predecode limit");
    assert_eq!(error.code(), "height_source_limit_samples");
    let error = TerrainSource::from_png(
        &bytes,
        &settings,
        ImportLimits {
            max_samples: 6,
            ..limits()
        },
    )
    .expect_err("bad pixels");
    assert_eq!(error.code(), "height_io_decode");
}

#[test]
#[ignore = "release-only CPU measurement; no timing assertion"]
fn terrain_json_budget_cost() {
    let counts = format!("[{}0]", "0,".repeat(999_999));
    let hostile = format!(r#"{{"counts":{counts},{GRID},{UNITS},"schema":2}}"#);
    let valid = format!(r#"{{"counts":[0,1,2,3],{GRID},{UNITS},"schema":2}}"#);
    for (label, text) in [("million_sample_refusal", hostile), ("four_sample_valid", valid)] {
        let begin = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(TerrainSource::from_json(
                std::hint::black_box(text.as_bytes()),
                limits(),
            ))
            .ok();
        }
        eprintln!("{label}: iterations=10000 elapsed={:?}", begin.elapsed());
    }
}

#[global_allocator]
static ALLOCATOR: regia_test_utils::alloc_counter::CountingAlloc =
    regia_test_utils::alloc_counter::CountingAlloc::new();

#[test]
fn independent_allocator_counters_bound_million_count_refusal() {
    // Input construction is outside the measured region. Per-thread counters
    // exclude concurrently running tests and need no scheduling/timing assumptions.
    let counts = format!("[{}0]", "0,".repeat(999_999));
    for text in [
        format!(r#"{{"schema":2,{GRID},{UNITS},"counts":{counts}}}"#),
        format!(r#"{{"counts":{counts},{UNITS},{GRID},"schema":2}}"#),
    ] {
        let (result, allocated) =
            regia_test_utils::alloc_counter::measure(|| TerrainSource::from_json(text.as_bytes(), limits()));
        assert!(result.is_err());
        assert!(
            allocated.bytes <= 4096,
            "four-sample refusal allocated {allocated:?}"
        );
    }
}

#[test]
fn portable_schema2_direct_f32_and_png_meter_bit_fixtures() {
    use regia_terrain::source::PngImportSettings;
    let decimal = "1.0000000596046447753906250000001";
    // These exact bits run unchanged on x86-64 and aarch64. No epsilon or f64
    // intermediate: the decimal lies just above an f32 halfway point.
    for text in [
        format!(
            r#"{{"schema":2,"grid":{{"cols":2,"rows":2,"cell_m":{decimal},"sample0_x_m":{decimal}}},"units":{{"kind":"counts","meters_per_count":{decimal},"zero_counts":0}},"counts":[0,1,2,65535]}}"#
        ),
        format!(
            r#"{{"counts":[0,1,2,65535],"units":{{"zero_counts":0,"meters_per_count":{decimal},"kind":"counts"}},"grid":{{"sample0_x_m":{decimal},"cell_m":{decimal},"rows":2,"cols":2}},"schema":2}}"#
        ),
    ] {
        let source = TerrainSource::from_json(text.as_bytes(), limits()).expect("decimal");
        assert_eq!(source.cell_m().to_bits(), 0x3f80_0001);
        assert_eq!(source.meters_per_count().to_bits(), 0x3f80_0001);
        assert_eq!(source.sample0().map(f32::to_bits), [0x3f80_0001, 0xbf80_0001]);
        assert_eq!(
            source.heights_m().iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            [0, 0x3f80_0001, 0x4000_0001, 0x477f_ff02]
        );
    }
    let counts = [992u16, 1000, 1008, 1024];
    let json = br#"{"counts":[992,1000,1008,1024],"schema":2,"grid":{"cols":2,"rows":2,"cell_m":3},"units":{"kind":"counts","meters_per_count":0.125,"zero_counts":1000}}"#;
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 2, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder
            .add_text_chunk("terrain_zero_counts".into(), "1000".into())
            .expect("datum");
        let mut writer = encoder.write_header().expect("header");
        writer
            .write_image_data(&counts.into_iter().flat_map(u16::to_be_bytes).collect::<Vec<_>>())
            .expect("samples");
        writer.finish().expect("end");
    }
    for source in [
        TerrainSource::from_json(json, limits()).expect("JSON"),
        TerrainSource::from_png(
            &png,
            &PngImportSettings {
                meters_per_count: 0.125,
                cell_m: 3.0,
                ..Default::default()
            },
            limits(),
        )
        .expect("PNG"),
    ] {
        assert_eq!(source.counts(), counts);
        assert_eq!(source.sample0().map(f32::to_bits), [0xc040_0000; 2]);
        assert_eq!(
            source.heights_m().iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            [0xbf80_0000, 0, 0x3f80_0000, 0x4040_0000]
        );
    }
}

#[test]
fn exact_zero_and_metadata_boundaries_are_enforced() {
    let good = format!(r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3]}}"#);
    let no_metadata = ImportLimits {
        max_unknown_values: 0,
        ..limits()
    };
    assert!(TerrainSource::from_json(good.as_bytes(), no_metadata).is_ok());
    let extra = good.replace("\"schema\":2", "\"schema\":2,\"label\":null");
    assert_eq!(
        TerrainSource::from_json(extra.as_bytes(), no_metadata)
            .expect_err("no unknowns")
            .code(),
        "height_source_limit_metadata"
    );
    assert!(
        TerrainSource::from_json(
            extra.as_bytes(),
            ImportLimits {
                max_unknown_values: 2,
                ..limits()
            }
        )
        .is_ok()
    );
    let exact = ImportLimits {
        max_bytes: good.len(),
        max_json_depth: 3,
        ..no_metadata
    };
    assert!(TerrainSource::from_json(good.as_bytes(), exact).is_ok());
    assert_eq!(
        TerrainSource::from_json(
            good.as_bytes(),
            ImportLimits {
                max_bytes: good.len() - 1,
                ..exact
            }
        )
        .expect_err("one byte too long")
        .code(),
        "height_source_limit_bytes"
    );
    let counts_first = format!(r#"{{"counts":[0,1,2,3],"schema":2,{GRID},{UNITS}}}"#);
    assert_eq!(
        TerrainSource::from_json(
            counts_first.as_bytes(),
            ImportLimits {
                max_samples: 0,
                ..limits()
            }
        )
        .expect_err("zero samples")
        .code(),
        "height_source_limit_samples"
    );
}

#[test]
fn ignored_metadata_keeps_large_number_tokens_compatible() {
    // Ignoring metadata must not impose f64 range limits on otherwise valid JSON.
    // The original derived reader skips these tokens without interpreting numbers.
    for text in [
        format!(r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3],"extra":1e400}}"#),
        format!(
            r#"{{"schema":2,"grid":{{"cols":2,"rows":2,"cell_m":1,"extra":[-1e400,{{"id":1e9999}}]}},{UNITS},"counts":[0,1,2,3]}}"#
        ),
        format!(
            r#"{{"schema":2,{GRID},"units":{{"kind":"counts","meters_per_count":0.125,"zero_counts":0,"extra":1e400}},"counts":[0,1,2,3]}}"#
        ),
    ] {
        let source = TerrainSource::from_json(text.as_bytes(), limits()).expect("ignored numeric range");
        assert_eq!(source.counts(), [0, 1, 2, 3]);
    }
}

#[test]
fn bounded_container_grammar_rejects_malformed_json() {
    for extra in [
        "[1,]",
        "[1 2]",
        "[,1]",
        "[truefalse]",
        "[01]",
        "[1e]",
        "[+1]",
        "[NaN]",
        r#"{"x":1,}"#,
        r#"{"x" 1}"#,
        r#"{"x":1 "y":2}"#,
        r#"{1:2}"#,
        r#"{"x":}"#,
        r#"{"x":"\q"}"#,
    ] {
        let text = format!(r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3],"extra":{extra}}}"#);
        assert!(
            TerrainSource::from_json(text.as_bytes(), limits()).is_err(),
            "{extra}"
        );
    }
    for counts in ["[0,1,2,3,]", "[0 1,2,3]", "[,0,1,2,3]", "[00,1,2,3]"] {
        let text = format!(r#"{{"schema":2,{GRID},{UNITS},"counts":{counts}}}"#);
        assert!(
            TerrainSource::from_json(text.as_bytes(), limits()).is_err(),
            "{counts}"
        );
    }
    let good = format!(
        r#"{{"schema":2,{GRID},{UNITS},"counts":[0,1,2,3],"extra":{{"escaped\"key":[null,true,false,"\u0061",-1.25e+3]}}}}"#
    );
    assert!(TerrainSource::from_json(good.as_bytes(), limits()).is_ok());
    for suffix in [",", "{}", "null"] {
        assert!(TerrainSource::from_json(format!("{good}{suffix}").as_bytes(), limits()).is_err());
    }
}

#[test]
fn storage_growth_and_work_stay_bounded_across_capacity_boundaries() {
    use std::ops::ControlFlow;
    for samples in [4, 6, 254, 256, 258, 510, 512, 514] {
        let counts = format!("[{}0]", "0,".repeat(samples - 1));
        let text = format!(
            r#"{{"counts":{counts},"grid":{{"cols":2,"rows":{},"cell_m":1}},{UNITS},"schema":2}}"#,
            samples / 2
        );
        let mut callbacks = 0;
        let source = TerrainSource::from_json_with_observer(
            text.as_bytes(),
            ImportLimits {
                max_samples: samples,
                ..limits()
            },
            |p| {
                callbacks += 1;
                assert!(p.samples_stored <= p.samples_charged);
                assert!(p.samples_charged <= samples);
                assert!(p.sample_capacity <= samples);
                ControlFlow::Continue(())
            },
        )
        .expect("exact sample cap");
        assert_eq!(source.counts().len(), samples);
        assert_eq!(callbacks, 2 * samples + 2);
    }
}
