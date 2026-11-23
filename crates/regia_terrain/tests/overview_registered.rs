// SPDX-License-Identifier: MPL-2.0
//! Registered terrain overviews (CT-23, ADR-8596): explicit fit, image-to-world mapping,
//! no-data versus zero elevation, the source/settings cache, cancel before publication, north-up
//! and hillshade invariants, marker coordinates and the declared cross-architecture tolerance.
//! Generated, game-independent fixtures; no golden images; counters, not clocks.

use std::ops::ControlFlow;
use std::path::Path;

use regia_terrain::overview::{
    LETTERBOX_RGB, MARKER_RGB, MISSING_RGB, OverviewFit, OverviewMapping, OverviewMarker, OverviewSettings,
    bake_overview, bake_overview_registered, draw_markers,
};
use regia_terrain::overview_artifact::{
    OVERVIEW_DESCRIPTOR_VERSION, OVERVIEW_TOLERANCE, OverviewJob, cached_overview, descriptor_path,
    overview_cache_key, publish_overview, source_content_digest,
};
use regia_terrain::source::{
    ColOrder, CountMapping, ImportLimits, RASTER_DESCRIPTOR_VERSION, RasterDescriptor, RasterMaskSpec,
    RasterRegistration, RowOrder, TerrainSource,
};
use regia_test_utils::TempDir;

const MPC: f32 = 0.125;
const ZERO: u16 = 1000;
/// 6 x 3 samples, 2 m apart, first sample at (-5, -2): the cells span X -6..6 and Z -3..3.
const COLS: u32 = 6;
const ROWS: u32 = 3;

fn mapping() -> CountMapping {
    CountMapping {
        cols: COLS,
        rows: ROWS,
        meters_per_count: MPC,
        zero_counts: ZERO,
        cell_m: 2.0,
        sample0_m: [-5.0, -2.0],
    }
}

fn grid(counts: Vec<u16>) -> TerrainSource {
    TerrainSource::from_counts(counts, mapping(), ImportLimits::default()).expect("source")
}

/// The same grid with a validity mask (`valid[i] == 0`: no data at canonical sample `i`).
fn masked(counts: Vec<u16>, valid: Vec<u8>) -> TerrainSource {
    let d = RasterDescriptor {
        version: RASTER_DESCRIPTOR_VERSION,
        spacing_m: [2.0, 2.0],
        skew_m: [0.0, 0.0],
        row0: RowOrder::South,
        col0: ColOrder::West,
        registration: RasterRegistration::Center,
        origin_m: Some([-5.0, -2.0]),
        nodata_counts: None,
        valid_mask: Some(RasterMaskSpec {
            cols: COLS,
            rows: ROWS,
            valid,
        }),
    };
    TerrainSource::from_raster(counts, [COLS, ROWS], MPC, ZERO, &d, ImportLimits::default()).expect("source")
}

fn flat() -> Vec<u16> {
    vec![ZERO; (COLS * ROWS) as usize]
}

fn settings(size: [u32; 2], fit: OverviewFit) -> OverviewSettings {
    OverviewSettings {
        size,
        blur_passes: 0,
        fit,
        ..Default::default()
    }
}

fn pixel(rgb: &[u8], width: u32, [x, y]: [u32; 2]) -> [u8; 3] {
    let i = (y * width + x) as usize * 3;
    [rgb[i], rgb[i + 1], rgb[i + 2]]
}

fn close(a: [f64; 2], b: [f64; 2]) -> bool {
    (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9
}

fn assert_corners(m: &OverviewMapping, [cx, cy, cw, ch]: [u32; 4]) {
    let (l, t, r, b) = (
        f64::from(cx),
        f64::from(cy),
        f64::from(cx + cw),
        f64::from(cy + ch),
    );
    for (uv, xz) in [
        ([l, t], [-6.0, 3.0]),                        // north-west
        ([r, t], [6.0, 3.0]),                         // north-east
        ([l, b], [-6.0, -3.0]),                       // south-west
        ([r, b], [6.0, -3.0]),                        // south-east
        ([(l + r) / 2.0, (t + b) / 2.0], [0.0, 0.0]), // center
    ] {
        assert!(
            close(m.pixel_to_world(uv), xz),
            "{uv:?} -> {:?}, want {xz:?}",
            m.pixel_to_world(uv)
        );
        assert!(
            close(m.world_to_pixel(xz), uv),
            "{xz:?} -> {:?}, want {uv:?}",
            m.world_to_pixel(xz)
        );
    }
}

#[test]
fn rectangular_grid_corners_and_center_map_under_both_fits() {
    let source = grid(flat());
    // Legacy stretch: the 12 m x 6 m rectangle fills the square; two scales.
    let stretch = bake_overview_registered(&source, &settings([64, 64], OverviewFit::Stretch), |_| {
        ControlFlow::Continue(())
    })
    .expect("stretch");
    assert_eq!(stretch.mapping.content_px, [0, 0, 64, 64]);
    assert_eq!(stretch.mapping.world_min, [-6.0, -3.0]);
    assert_eq!(stretch.mapping.world_max, [6.0, 3.0]);
    assert_eq!(stretch.mapping.meters_per_pixel(), [12.0 / 64.0, 6.0 / 64.0]);
    assert_corners(&stretch.mapping, [0, 0, 64, 64]);
    // The stretch image is the legacy `bake_overview` image, byte for byte.
    let legacy = bake_overview(
        &source,
        &OverviewSettings {
            size: [64, 64],
            blur_passes: 0,
            ..Default::default()
        },
    )
    .expect("legacy");
    assert_eq!(stretch.image, legacy);
    assert!(
        stretch
            .image
            .rgb()
            .as_chunks::<3>()
            .0
            .iter()
            .all(|p| *p != LETTERBOX_RGB)
    );

    // Preserve-aspect: one scale, centered, letterboxed above and below.
    let fit = bake_overview_registered(&source, &settings([64, 64], OverviewFit::PreserveAspect), |_| {
        ControlFlow::Continue(())
    })
    .expect("fit");
    assert_eq!(fit.mapping.content_px, [0, 16, 64, 32]);
    assert_eq!(fit.mapping.meters_per_pixel(), [0.1875, 0.1875]);
    assert_corners(&fit.mapping, [0, 16, 64, 32]);
    assert_eq!(fit.image.dimensions(), [64, 64]);
    for y in 0..64 {
        for x in 0..64 {
            let p = pixel(fit.image.rgb(), 64, [x, y]);
            if (16..48).contains(&y) {
                assert!(p[0] == p[1] && p[1] == p[2], "relief is gray at {x},{y}: {p:?}");
            } else {
                assert_eq!(p, LETTERBOX_RGB, "letterbox at {x},{y}");
            }
        }
    }
    // A tall output letterboxes left and right instead.
    let tall = bake_overview_registered(&source, &settings([20, 64], OverviewFit::PreserveAspect), |_| {
        ControlFlow::Continue(())
    })
    .expect("tall");
    assert_eq!(tall.mapping.content_px, [0, 27, 20, 10]);
    let wide = bake_overview_registered(&source, &settings([128, 32], OverviewFit::PreserveAspect), |_| {
        ControlFlow::Continue(())
    })
    .expect("wide");
    assert_eq!(wide.mapping.content_px, [32, 0, 64, 32]);
    assert_corners(&wide.mapping, [32, 0, 64, 32]);
}

#[test]
fn the_image_agrees_with_its_mapping() {
    // No data in the north-east sample only (canonical col 5, row 2): its cell is X 4..6, Z 1..3.
    let mut valid = vec![1u8; (COLS * ROWS) as usize];
    valid[(2 * COLS + 5) as usize] = 0;
    let source = masked(flat(), valid);
    for fit in [OverviewFit::Stretch, OverviewFit::PreserveAspect] {
        let baked =
            bake_overview_registered(&source, &settings([64, 64], fit), |_| ControlFlow::Continue(()))
                .expect("bake");
        let m = baked.mapping;
        let mut expected = 0;
        for y in 0..64u32 {
            for x in 0..64u32 {
                let p = pixel(baked.image.rgb(), 64, [x, y]);
                if !m.in_content(x, y) {
                    assert_eq!(p, LETTERBOX_RGB);
                    continue;
                }
                // A pixel shows no data exactly when its whole footprint lies in the missing cell.
                let nw = m.pixel_to_world([f64::from(x), f64::from(y)]);
                let se = m.pixel_to_world([f64::from(x + 1), f64::from(y + 1)]);
                let inside =
                    nw[0] >= 4.0 - 1e-9 && se[0] <= 6.0 + 1e-9 && se[1] >= 1.0 - 1e-9 && nw[1] <= 3.0 + 1e-9;
                assert_eq!(
                    p == MISSING_RGB,
                    inside,
                    "{fit:?} pixel {x},{y} at {nw:?}..{se:?}: {p:?}"
                );
                expected += usize::from(inside);
            }
        }
        assert!(expected > 0, "{fit:?}");
        assert_eq!(baked.image.missing_pixels(), expected, "{fit:?}");
    }
}

#[test]
fn no_data_stays_distinct_from_zero_elevation() {
    // Identical counts: sample 7 is exactly the datum (0 m) in one source and has no data in the
    // other.
    let counts: Vec<u16> = (0..COLS * ROWS).map(|i| ZERO + 8 * (i as u16 % 3)).collect();
    let mut with_zero = counts.clone();
    with_zero[7] = ZERO;
    let mut valid = vec![1u8; (COLS * ROWS) as usize];
    valid[7] = 0;
    let zero = masked(with_zero.clone(), vec![1u8; (COLS * ROWS) as usize]);
    let gap = masked(with_zero, valid);
    assert_eq!(zero.sample_height_m(1, 1), Some(0.0));
    assert_eq!(gap.sample_height_m(1, 1), None);
    assert_eq!(
        zero.counts(),
        gap.counts(),
        "the counts are the same; only the mask differs"
    );
    let s = settings([COLS * 4, ROWS * 4], OverviewFit::Stretch);
    let a = bake_overview(&zero, &s).expect("zero");
    let b = bake_overview(&gap, &s).expect("gap");
    assert_eq!(a.missing_pixels(), 0);
    assert!(
        a.rgb().as_chunks::<3>().0.iter().all(|p| *p != MISSING_RGB),
        "0 m is relief"
    );
    assert_eq!(b.missing_pixels(), 16, "the 4x4 pixels over sample (1, 1)");
    // Sample (1, 1) is image columns 4..8 and rows 4..8 (row 1 of 3 from the south is the middle).
    for y in 4..8 {
        for x in 4..8 {
            assert_eq!(pixel(b.rgb(), COLS * 4, [x, y]), MISSING_RGB);
            let p = pixel(a.rgb(), COLS * 4, [x, y]);
            assert!(p[0] == p[1] && p[1] == p[2], "{p:?}");
        }
    }
    // ... and the two never share a cache entry.
    assert_ne!(source_content_digest(&zero), source_content_digest(&gap));
}

fn job<'a>(
    source: &'a TerrainSource,
    settings: OverviewSettings,
    markers: Vec<OverviewMarker>,
) -> OverviewJob<'a> {
    OverviewJob {
        source,
        source_path: "terrain/source.json".into(),
        source_digest: "input-digest".into(),
        settings,
        markers,
    }
}

fn publish_counting(job: &OverviewJob<'_>, png: &Path) -> (bool, u32) {
    let mut calls = 0;
    let published = publish_overview(job, png, |_| {
        calls += 1;
        ControlFlow::Continue(())
    })
    .expect("publish");
    (published.cached, calls)
}

fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("dir")
        .map(|e| {
            let path = e.expect("entry").path();
            (
                path.file_name().expect("name").to_string_lossy().into_owned(),
                std::fs::read(&path).expect("read"),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn source_and_settings_changes_invalidate_the_cache() {
    let dir = TempDir::new("overview_cache");
    let png = dir.path().join("area.png");
    let ramp: Vec<u16> = (0..COLS * ROWS).map(|i| ZERO + i as u16).collect();
    let source = grid(ramp.clone());
    let base = job(
        &source,
        settings([24, 12], OverviewFit::PreserveAspect),
        Vec::new(),
    );

    let (cached, calls) = publish_counting(&base, &png);
    assert!(!cached);
    assert!(calls > 0);
    assert_eq!(
        listing(dir.path())
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>(),
        ["area.overview.json", "area.png"]
    );
    let first = listing(dir.path());
    // Same source and settings: nothing is baked or written.
    assert_eq!(publish_counting(&base, &png), (true, 0));
    assert_eq!(listing(dir.path()), first);

    let key = overview_cache_key(&base);
    let mut nudged = ramp.clone();
    nudged[3] += 1;
    let other_source = grid(nudged);
    let mut changed = Vec::new();
    // Every setting that moves a pixel, the fit, the source, its input digest and the markers.
    for edit in 0..9 {
        let mut s = base.settings.clone();
        let mut j = base.clone();
        match edit {
            0 => s.relief = 2.0,
            1 => s.size = [24, 24],
            2 => s.fit = OverviewFit::Stretch,
            3 => s.contour_step_m = Some(0.5),
            4 => s.light = [1.0, 1.0, 0.0],
            5 => s.blur_passes = 1,
            6 => j.source_digest = "another input digest".into(),
            7 => {
                j.markers = vec![OverviewMarker {
                    label: "spawn".into(),
                    world_xz: [0.0, 0.0],
                    rgb: MARKER_RGB,
                }];
            }
            _ => {}
        }
        j.settings = s;
        if edit == 8 {
            j.source = &other_source;
        }
        let k = overview_cache_key(&j);
        assert_ne!(k, key, "edit {edit}");
        assert!(cached_overview(&png, &k).is_none(), "edit {edit}");
        changed.push(k);
    }
    changed.sort();
    changed.dedup();
    assert_eq!(changed.len(), 9);
    // The pixel budget moves no pixel: same key.
    let mut budget = base.clone();
    budget.settings.max_pixels = 1 << 20;
    assert_eq!(overview_cache_key(&budget), key);

    // A changed setting re-bakes and republishes; going back re-bakes again.
    let mut relief = base.clone();
    relief.settings.relief = 3.0;
    assert!(!publish_counting(&relief, &png).0);
    assert!(cached_overview(&png, &key).is_none());
    assert!(!publish_counting(&base, &png).0);
    assert_eq!(listing(dir.path()), first, "a re-bake is byte-identical");

    // An edited PNG or a deleted descriptor is not a cache hit.
    std::fs::write(&png, b"not the overview").expect("edit");
    assert!(cached_overview(&png, &key).is_none());
    assert!(!publish_counting(&base, &png).0);
    assert_eq!(listing(dir.path()), first);
    std::fs::remove_file(descriptor_path(&png)).expect("remove");
    assert!(!publish_counting(&base, &png).0);
    assert_eq!(listing(dir.path()), first);
}

#[test]
fn cancel_never_publishes_half_a_pair() {
    let source = grid((0..COLS * ROWS).map(|i| ZERO + 3 * i as u16).collect());
    let mut s = settings([12, 8], OverviewFit::Stretch);
    s.blur_passes = 1;
    let fresh = job(&source, s.clone(), Vec::new());
    // Steps: 8 area rows, 1 blur pass, 8 shading rows, then the commit.
    let mut steps = 0;
    let dir = TempDir::new("overview_cancel_count");
    publish_overview(&fresh, &dir.path().join("o.png"), |p| {
        steps = p.steps;
        ControlFlow::Continue(())
    })
    .expect("publish");
    assert_eq!(steps, 18);

    let mut old_settings = s.clone();
    old_settings.relief = 4.0;
    let old = job(&source, old_settings, Vec::new());
    for stop in 1..=steps {
        for with_old in [false, true] {
            let dir = TempDir::new("overview_cancel");
            let png = dir.path().join("o.png");
            if with_old {
                publish_overview(&old, &png, |_| ControlFlow::Continue(())).expect("old pair");
            }
            let before = listing(dir.path());
            let mut seen = 0;
            let err = publish_overview(&fresh, &png, |p| {
                seen = p.step;
                if p.step == stop {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .expect_err("cancelled");
            assert_eq!(err.code(), "terrain_overview_cancelled", "{err}");
            assert_eq!(seen, stop);
            // No temporary, no new PNG, no new descriptor; an earlier pair is untouched and
            // still a valid cache entry for its own settings.
            assert_eq!(listing(dir.path()), before, "stop {stop}, old pair {with_old}");
            if with_old {
                assert!(cached_overview(&png, &overview_cache_key(&old)).is_some());
            }
            assert!(cached_overview(&png, &overview_cache_key(&fresh)).is_none());
        }
    }
    // A PNG renamed in place without its descriptor (an interrupted publication) is never
    // trusted: the old descriptor names the old PNG's hash.
    let dir = TempDir::new("overview_half");
    let png = dir.path().join("o.png");
    publish_overview(&old, &png, |_| ControlFlow::Continue(())).expect("old");
    let other = dir.path().join("other.png");
    publish_overview(&fresh, &other, |_| ControlFlow::Continue(())).expect("fresh");
    std::fs::copy(&other, &png).expect("swap png");
    assert!(cached_overview(&png, &overview_cache_key(&old)).is_none());
    assert!(cached_overview(&png, &overview_cache_key(&fresh)).is_none());
}

#[test]
fn north_up_and_hillshade_invariants_are_repeatable() {
    // Heights rise to the north only; no relief: brightness is elevation alone.
    let north: Vec<u16> = (0..ROWS)
        .flat_map(|r| (0..COLS).map(move |_| ZERO + 80 * r as u16))
        .collect();
    let source = grid(north);
    let mut s = settings([12, 24], OverviewFit::Stretch);
    s.relief = 0.0;
    let image = bake_overview(&source, &s).expect("north");
    // ambient 0.5 x (0.7 + 0.3 x elevation): the top (north) row is the brightest, 128; the
    // bottom (south) row the darkest, round(0.5 x 0.7 x 255) = 89.
    assert_eq!(pixel(image.rgb(), 12, [0, 0]), [128; 3]);
    assert_eq!(pixel(image.rgb(), 12, [11, 23]), [89; 3]);
    for y in 1..24 {
        assert!(
            pixel(image.rgb(), 12, [0, y])[0] <= pixel(image.rgb(), 12, [0, y - 1])[0],
            "brightness never rises southward"
        );
    }

    // Hillshade: with the light in the west, ground rising to the east faces it and is brighter
    // than flat ground; the mirror image (rising west) is darker. Elevation modulation is the same
    // for both (the same heights, mirrored), so only the facing differs.
    let east: Vec<u16> = (0..ROWS)
        .flat_map(|_| (0..COLS).map(|c| ZERO + 40 * c as u16))
        .collect();
    let west: Vec<u16> = (0..ROWS)
        .flat_map(|_| (0..COLS).map(|c| ZERO + 40 * (COLS - 1 - c) as u16))
        .collect();
    let mut lit = settings([12, 6], OverviewFit::Stretch);
    lit.light = [-1.0, 1.0, 0.0];
    let facing = bake_overview(&grid(east), &lit).expect("east");
    let away = bake_overview(&grid(west), &lit).expect("west");
    for y in 0..6 {
        for x in 1..11 {
            let a = pixel(facing.rgb(), 12, [x, y])[0];
            let b = pixel(away.rgb(), 12, [11 - x, y])[0];
            assert!(a > b, "facing {a} > away {b} at {x},{y}");
        }
    }

    // Repeatable bit for bit, and the declared cross-architecture tolerance is what it says.
    assert_eq!(bake_overview(&source, &s).expect("again"), image);
    let rgb = image.rgb().to_vec();
    OVERVIEW_TOLERANCE.compare(&rgb, &rgb).expect("identical");
    let mut one_step = rgb.clone();
    one_step[0] = one_step[0].wrapping_sub(1);
    assert!(
        rgb.len() / 3 < 1000,
        "fixture smaller than the fraction's denominator"
    );
    OVERVIEW_TOLERANCE
        .compare(&rgb, &one_step)
        .expect_err("one pixel of 288 is over 0.1 %");
    let big_a = vec![100u8; 3 * 2000];
    let mut big_b = big_a.clone();
    big_b[5] = 101;
    big_b[3000] = 99;
    OVERVIEW_TOLERANCE
        .compare(&big_a, &big_b)
        .expect("two pixels of 2000, one step each");
    big_b[3000] = 98;
    OVERVIEW_TOLERANCE.compare(&big_a, &big_b).expect_err("two steps");
    OVERVIEW_TOLERANCE
        .compare(&big_a, &big_a[3..])
        .expect_err("size mismatch");
}

#[test]
fn marker_coordinates_are_repeatable() {
    let source = grid(flat());
    let s = settings([64, 64], OverviewFit::PreserveAspect);
    let marker = |label: &str, xz: [f64; 2]| OverviewMarker {
        label: label.into(),
        world_xz: xz,
        rgb: MARKER_RGB,
    };
    let markers = vec![
        marker("center", [0.0, 0.0]),
        marker("north-west", [-6.0, 3.0]),
        marker("south-east", [6.0, -3.0]),
        marker("sample (1, 1)", [-3.0, 0.0]),
        marker("outside", [100.0, 0.0]),
    ];
    let mut baked = bake_overview_registered(&source, &s, |_| ControlFlow::Continue(())).expect("bake");
    let placed = draw_markers(&mut baked.image, &baked.mapping, &markers);
    let pixels: Vec<Option<[u32; 2]>> = placed.iter().map(|p| p.pixel).collect();
    assert_eq!(
        pixels,
        [
            Some([32, 32]),
            Some([0, 16]),
            Some([63, 47]),
            Some([16, 32]),
            None
        ]
    );
    for p in pixels.iter().flatten() {
        assert_eq!(pixel(baked.image.rgb(), 64, *p), MARKER_RGB);
    }
    // Arms are clipped to the source rectangle: the letterbox above the north-west marker stays.
    assert_eq!(pixel(baked.image.rgb(), 64, [0, 15]), LETTERBOX_RGB);
    assert_eq!(pixel(baked.image.rgb(), 64, [0, 19]), MARKER_RGB);
    assert_eq!(pixel(baked.image.rgb(), 64, [3, 16]), MARKER_RGB);
    assert_ne!(pixel(baked.image.rgb(), 64, [4, 16]), MARKER_RGB);

    // Published twice in two folders: the same descriptor, markers included.
    let descriptors: Vec<serde_json::Value> = (0..2)
        .map(|_| {
            let dir = TempDir::new("overview_markers");
            let published = publish_overview(
                &job(&source, s.clone(), markers.clone()),
                &dir.path().join("m.png"),
                |_| ControlFlow::Continue(()),
            )
            .expect("publish");
            assert_eq!(
                std::fs::read(&published.descriptor_path).expect("descriptor"),
                serde_json::to_vec_pretty(&published.descriptor).expect("json")
            );
            published.descriptor
        })
        .collect();
    assert_eq!(descriptors[0], descriptors[1]);
    let d = &descriptors[0];
    assert_eq!(d["markers"][0]["pixel"], serde_json::json!([32, 32]));
    assert_eq!(d["markers"][4]["pixel"], serde_json::Value::Null);
    assert_eq!(d["overview_descriptor"], OVERVIEW_DESCRIPTOR_VERSION);
    assert_eq!(d["mapping"]["content_px"], serde_json::json!([0, 16, 64, 32]));
    assert_eq!(d["mapping"]["world_min"], serde_json::json!([-6.0, -3.0]));
    assert_eq!(d["mapping"]["world_max"], serde_json::json!([6.0, 3.0]));
    assert_eq!(d["orientation"]["north_up"], true);
    // 12 m wide: a quarter is 3 m, the round scale bar 2 m = 2 / 0.1875 pixels.
    assert_eq!(d["legend"]["scale_bar_m"], 2.0);
    assert_eq!(d["legend"]["scale_bar_px"], 2.0 / 0.1875);
    assert_eq!(d["settings"]["fit"], "preserve_aspect");
    assert_eq!(d["presentation_only"], true);
}

#[test]
fn the_json_schema_names_every_descriptor_field() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/formats/terrain-overview-descriptor.schema.json");
    let text = std::fs::read_to_string(&path).expect("schema file");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("schema json");
    let dir = TempDir::new("overview_schema");
    let source = grid(flat());
    let published = publish_overview(
        &job(&source, settings([8, 8], OverviewFit::Stretch), Vec::new()),
        &dir.path().join("s.png"),
        |_| ControlFlow::Continue(()),
    )
    .expect("publish");
    let d = published.descriptor;
    let mut fields: Vec<&String> = d.as_object().expect("object").keys().collect();
    let mut named: Vec<&String> = schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .collect();
    fields.sort();
    named.sort();
    assert_eq!(fields, named);
    for (key, value) in d.as_object().expect("object") {
        if let Some(inner) = value.as_object() {
            for k in inner.keys() {
                assert!(
                    schema["properties"][key]["properties"].get(k).is_some(),
                    "{key}.{k}"
                );
            }
        }
    }
    assert_eq!(
        schema["properties"]["overview_descriptor"]["const"],
        OVERVIEW_DESCRIPTOR_VERSION
    );
}

/// A named hand edit of a published descriptor.
type DescriptorEdit = (&'static str, fn(&mut serde_json::Value));

/// ADR-8871 (plan W-19, import-chain review finding 3): a published pair whose descriptor gained
/// an unknown field, or whose mapping was edited by hand, is not the cached overview: the next
/// publish bakes again and restores the pair. Uses only the publish/cache API, so it also runs
/// against a build without the digest (where it fails at the first edit).
#[test]
fn an_edited_or_extended_descriptor_is_not_the_cached_overview() {
    let dir = TempDir::new("overview_digest");
    let source = grid(flat());
    let j = job(&source, settings([8, 8], OverviewFit::Stretch), Vec::new());
    let png = dir.path().join("d.png");
    publish_overview(&j, &png, |_| ControlFlow::Continue(())).expect("publish");
    let key = overview_cache_key(&j);
    let original = std::fs::read(descriptor_path(&png)).expect("descriptor");
    assert!(
        cached_overview(&png, &key).is_some(),
        "the published pair is current"
    );

    let edits: [DescriptorEdit; 2] = [
        ("an unknown top-level field", |d| {
            d["note"] = serde_json::json!("x");
        }),
        ("a hand-edited mapping", |d| {
            d["mapping"]["world_min"][0] = serde_json::json!(-1000.0);
        }),
    ];
    for (what, edit) in edits {
        let mut d: serde_json::Value = serde_json::from_slice(&original).expect("json");
        edit(&mut d);
        std::fs::write(
            descriptor_path(&png),
            serde_json::to_vec_pretty(&d).expect("json"),
        )
        .expect("write");
        assert!(cached_overview(&png, &key).is_none(), "{what}");
        let again = publish_overview(&j, &png, |_| ControlFlow::Continue(())).expect("republish");
        assert!(!again.cached, "{what}: bakes again");
        assert_eq!(
            std::fs::read(descriptor_path(&png)).expect("descriptor"),
            original,
            "{what}"
        );
    }
}

/// ADR-8871 (plan W-19): `descriptor_digest` is the digest of every other field; a removed digest
/// or a version-1 descriptor is not current; key order and whitespace are not part of it.
#[test]
fn the_descriptor_digest_covers_every_other_field() {
    let dir = TempDir::new("overview_digest_fields");
    let source = grid(flat());
    let j = job(&source, settings([8, 8], OverviewFit::Stretch), Vec::new());
    let png = dir.path().join("e.png");
    let published = publish_overview(&j, &png, |_| ControlFlow::Continue(())).expect("publish");
    let key = overview_cache_key(&j);
    let original = std::fs::read(descriptor_path(&png)).expect("descriptor");
    let digest = published.descriptor["descriptor_digest"]
        .as_str()
        .expect("a digest")
        .to_string();
    assert_eq!(
        digest,
        regia_terrain::overview_artifact::descriptor_digest(&published.descriptor)
    );
    assert_eq!(digest.len(), 64);
    assert_eq!(OVERVIEW_DESCRIPTOR_VERSION, 2);

    let edits: [DescriptorEdit; 2] = [
        ("a removed digest", |d| {
            d.as_object_mut().expect("object").remove("descriptor_digest");
        }),
        ("version 1 (no digest field in its shape)", |d| {
            d["overview_descriptor"] = serde_json::json!(1);
            d.as_object_mut().expect("object").remove("descriptor_digest");
        }),
    ];
    for (what, edit) in edits {
        let mut d: serde_json::Value = serde_json::from_slice(&original).expect("json");
        edit(&mut d);
        std::fs::write(
            descriptor_path(&png),
            serde_json::to_vec_pretty(&d).expect("json"),
        )
        .expect("write");
        assert!(cached_overview(&png, &key).is_none(), "{what}");
    }
    // Key order and whitespace are not part of the digest: a compact rewrite is still current.
    let d: serde_json::Value = serde_json::from_slice(&original).expect("json");
    std::fs::write(descriptor_path(&png), serde_json::to_vec(&d).expect("json")).expect("write");
    assert!(
        cached_overview(&png, &key).is_some(),
        "a reformatted descriptor is current"
    );
}
