// SPDX-License-Identifier: MPL-2.0
//! Raster metadata of count sources (CT-21, ADR-8542): independent X/Z spacing, row/column
//! orientation by exact permutation, explicit sample-center/cell-edge registration and missing
//! data. Generated, game-independent fixtures; deterministic seeded property loops (no new
//! dependency).

use regia_terrain::holes::HoleShape;
use regia_terrain::overview::{MISSING_RGB, OverviewSettings, bake_overview};
use regia_terrain::source::{
    ColOrder, CountMapping, ImportLimits, MissingDataPolicy, RASTER_DESCRIPTOR_VERSION, RasterDescriptor,
    RasterLayout, RasterMaskSpec, RasterRegistration, RowOrder, TerrainSource, ValidMask, stored_index,
};

const MPC: f32 = 0.125;
const ZERO: u16 = 1000;

fn descriptor(spacing: [f32; 2], row0: RowOrder, col0: ColOrder) -> RasterDescriptor {
    RasterDescriptor {
        version: RASTER_DESCRIPTOR_VERSION,
        spacing_m: spacing,
        skew_m: [0.0, 0.0],
        row0,
        col0,
        registration: RasterRegistration::Center,
        origin_m: Some([10.0, -4.0]),
        nodata_counts: None,
        valid_mask: None,
    }
}

fn raster(
    counts: &[u16],
    dims: [u32; 2],
    d: &RasterDescriptor,
) -> Result<TerrainSource, regia_terrain::source::SourceError> {
    TerrainSource::from_raster(counts.to_vec(), dims, MPC, ZERO, d, ImportLimits::default())
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|h| h.to_bits()).collect()
}

/// Stored-order values for a canonical (south/west) grid stored as `row0`/`col0`.
fn store<T: Copy + Default>(canonical: &[T], dims: [u32; 2], row0: RowOrder, col0: ColOrder) -> Vec<T> {
    let mut out = vec![T::default(); canonical.len()];
    for row in 0..dims[1] {
        for col in 0..dims[0] {
            out[stored_index(dims, row0, col0, col, row)] = canonical[(row * dims[0] + col) as usize];
        }
    }
    out
}

#[test]
fn north_first_and_south_first_descriptions_map_to_identical_canonical_samples() {
    // Canonical 3x2: south row [992 1000 1008], north row [1016 1024 1032].
    let south: [u16; 6] = [992, 1000, 1008, 1016, 1024, 1032];
    let north: [u16; 6] = [1016, 1024, 1032, 992, 1000, 1008];
    let east_north: [u16; 6] = [1032, 1024, 1016, 1008, 1000, 992];
    let a = raster(
        &south,
        [3, 2],
        &descriptor([2.0, 2.0], RowOrder::South, ColOrder::West),
    )
    .expect("south");
    let b = raster(
        &north,
        [3, 2],
        &descriptor([2.0, 2.0], RowOrder::North, ColOrder::West),
    )
    .expect("north");
    let c = raster(
        &east_north,
        [3, 2],
        &descriptor([2.0, 2.0], RowOrder::North, ColOrder::East),
    )
    .expect("east");
    for other in [&b, &c] {
        assert_eq!(other.counts(), a.counts(), "canonical counts");
        assert_eq!(bits(&other.heights_m()), bits(&a.heights_m()));
        for (col, row) in [(0, 0), (2, 0), (0, 1), (2, 1)] {
            assert_eq!(other.sample_world(col, row), a.sample_world(col, row));
            assert_eq!(other.height_m(col, row).to_bits(), a.height_m(col, row).to_bits());
        }
    }
    assert_eq!(a.counts(), &south);
    assert_eq!(b.layout().row0, RowOrder::North);
    assert_eq!(c.layout().col0, ColOrder::East);
    // Southwest corner is -1 m, northeast +4 m whichever corner was stored first.
    assert_eq!(c.height_m(0, 0), -1.0);
    assert_eq!(c.height_m(2, 1), 4.0);
}

#[test]
fn unequal_x_z_spacing_preserves_control_point_coordinates() {
    let counts: Vec<u16> = (0..12).map(|i| 1000 + i * 8).collect();
    let source = raster(
        &counts,
        [4, 3],
        &descriptor([2.0, 0.5], RowOrder::South, ColOrder::West),
    )
    .expect("source");
    assert_eq!(source.spacing_m(), [2.0, 0.5]);
    assert_eq!(source.sample0(), [10.0, -4.0]);
    for row in 0..3u32 {
        for col in 0..4u32 {
            let want = [
                (10.0f64 + f64::from(col) * 2.0) as f32,
                (-4.0f64 + f64::from(row) * 0.5) as f32,
            ];
            assert_eq!(
                source.sample_world(col, row),
                want,
                "control point ({col}, {row})"
            );
        }
    }
    assert_eq!(source.sample_world(3, 2), [16.0, -3.0]);
    // The cooked index holds one cell size: refused, never resampled.
    let err = source.count_mapping().expect_err("unequal spacing");
    assert!(err.0.starts_with("terrain_mapping_unrepresentable"), "{err}");
    let err = source
        .terrain_map(MissingDataPolicy::Holes)
        .expect_err("unequal spacing");
    assert!(err.0.starts_with("terrain_mapping_unrepresentable"), "{err}");
    // The default first sample is the schema-2 expression on each axis.
    let mut d = descriptor([2.0, 0.5], RowOrder::South, ColOrder::West);
    d.origin_m = None;
    assert_eq!(
        raster(&counts, [4, 3], &d).expect("default").sample0(),
        [-4.0, -0.75]
    );
}

#[test]
fn half_cell_registration_is_explicit() {
    let counts = [1000u16; 12];
    let center = descriptor([2.0, 0.5], RowOrder::North, ColOrder::West);
    let edge = RasterDescriptor {
        registration: RasterRegistration::Edge,
        ..center.clone()
    };
    let a = raster(&counts, [4, 3], &center).expect("center");
    let b = raster(&counts, [4, 3], &edge).expect("edge");
    assert_eq!(a.sample0(), [10.0, -4.0]);
    assert_eq!(
        b.sample0(),
        [11.0, -3.75],
        "half a spacing inside the cell edge, per axis"
    );
    assert_eq!(b.layout().registration, RasterRegistration::Edge);
    // JSON must name the registration: a half-cell shift is never implied.
    let missing = br#"{"raster_descriptor":1,"spacing_m":[2.0,0.5]}"#;
    assert!(RasterDescriptor::from_json(missing, ImportLimits::default()).is_err());
    let named = br#"{"raster_descriptor":1,"spacing_m":[2.0,0.5],"registration":"edge","origin_m":[10.0,-4.0],"row0":"north"}"#;
    assert_eq!(
        RasterDescriptor::from_json(named, ImportLimits::default()).expect("named"),
        edge
    );
}

#[test]
fn x86_64_and_aarch64_coordinate_fixture_bits() {
    // DO27-style 0.3125 m spacing with a -0.15625 m cell edge, and a 0.1 m Z spacing far from
    // zero: f64 arithmetic rounded once to f32; the expected bits were computed independently.
    let d = RasterDescriptor {
        registration: RasterRegistration::Edge,
        origin_m: Some([-0.15625, 1000.3]),
        ..descriptor([0.3125, 0.1], RowOrder::South, ColOrder::West)
    };
    let source = raster(&[1000; 12], [4, 3], &d).expect("source");
    let world = |c, r| source.sample_world(c, r).map(f32::to_bits);
    assert_eq!(world(0, 0), [0x0000_0000, 0x447a_1666]);
    assert_eq!(world(3, 2), [0x3f70_0000, 0x447a_2333]);
    assert_eq!(world(4095, 7), [0x449f_f600, 0x447a_4333]);
}

#[test]
fn nodata_sentinels_never_become_extreme_terrain_heights() {
    // 4x4 with a sentinel at two samples; 65535 would be +8067 m.
    let mut counts = vec![1000u16; 16];
    counts[5] = 65535;
    counts[6] = 65535;
    counts[0] = 1008;
    let mut d = descriptor([1.0, 1.0], RowOrder::South, ColOrder::West);
    d.nodata_counts = Some(65535);
    // Equal center on X and Z: the runtime map holds it exactly.
    d.origin_m = Some([-2.0, -2.0]);
    let source = raster(&counts, [4, 4], &d).expect("source");
    assert_eq!(source.missing_samples(), 2);
    assert_eq!(source.counts()[5], 65535, "counts are kept verbatim");
    assert_eq!(source.sample_height_m(1, 1), None);
    assert!(source.height_m(1, 1).is_nan());
    let valid: Vec<f32> = source.heights_m().into_iter().filter(|h| !h.is_nan()).collect();
    assert_eq!(valid.len(), 14);
    assert!(valid.iter().all(|&h| (-1.0..=1.0).contains(&h)), "{valid:?}");
    // The preview shows the gap and never shades the sentinel.
    let image = bake_overview(
        &source,
        &OverviewSettings {
            size: [4, 4],
            blur_passes: 1,
            ..Default::default()
        },
    )
    .expect("overview");
    assert_eq!(image.missing_pixels(), 2);
    // North-up: canonical row 1 is image row 2.
    let px = |x: usize, y: usize| &image.rgb()[(y * 4 + x) * 3..(y * 4 + x) * 3 + 3];
    assert_eq!(px(1, 2), MISSING_RGB);
    assert_eq!(px(2, 2), MISSING_RGB);
    assert_ne!(px(0, 0), MISSING_RGB);
    // Collision/cook refuse the gap unless the hole policy is chosen.
    let err = source
        .terrain_map(MissingDataPolicy::Refuse)
        .expect_err("missing data");
    assert_eq!(err.code(), "terrain_raster_missing_data");
    assert!(source.count_mapping().is_err());
    let (map, hole) = source.terrain_map(MissingDataPolicy::Holes).expect("holes");
    let Some(HoleShape::Raster(hole)) = hole else {
        panic!("a raster hole");
    };
    let [x, z] = source.sample_world(1, 1);
    assert!(
        HoleShape::Raster(hole.clone()).contains(x, z),
        "missing sample is cut"
    );
    let [x, z] = source.sample_world(0, 0);
    assert!(!HoleShape::Raster(hole).contains(x, z), "valid sample is ground");
    let mut highest = f32::MIN;
    for row in 0..4i64 {
        for col in 0..4i64 {
            highest = highest.max(map.height_at_sample(col, row));
        }
    }
    assert!(highest <= 1.0, "no sentinel height in the runtime map: {highest}");
}

#[test]
fn invalid_mask_dimensions_fail() {
    let counts = [1000u16; 6];
    let with_mask = |cols, rows, valid: Vec<u8>| {
        let mut d = descriptor([1.0, 1.0], RowOrder::South, ColOrder::West);
        d.valid_mask = Some(RasterMaskSpec { cols, rows, valid });
        raster(&counts, [3, 2], &d)
    };
    for (cols, rows, valid) in [
        (2, 3, vec![1; 6]),
        (3, 2, vec![1; 5]),
        (3, 2, vec![1, 1, 2, 1, 1, 1]),
    ] {
        let err = with_mask(cols, rows, valid).expect_err("bad mask");
        assert_eq!(err.code(), "terrain_raster_mask", "{err}");
    }
    let err = with_mask(3, 2, vec![0; 6]).expect_err("nothing valid");
    assert_eq!(err.code(), "terrain_raster_missing_data");
    let ok = with_mask(3, 2, vec![1, 0, 1, 1, 1, 1]).expect("valid mask");
    assert_eq!(ok.missing_samples(), 1);
    assert!(!ok.is_valid(1, 0));
    assert!(ValidMask::from_flags(3, 2, &[true; 5]).is_err());
}

#[test]
fn rotated_sheared_and_future_descriptors_are_refused() {
    let counts = [1000u16; 4];
    let mut d = descriptor([1.0, 1.0], RowOrder::South, ColOrder::West);
    d.skew_m = [0.0, 0.25];
    assert_eq!(
        raster(&counts, [2, 2], &d).expect_err("sheared").code(),
        "terrain_raster_transform"
    );
    let json = br#"{"raster_descriptor":1,"spacing_m":[1,1],"registration":"center","skew_m":[0.1,0]}"#;
    let err = RasterDescriptor::from_json(json, ImportLimits::default()).expect_err("rotated");
    assert_eq!(err.code(), "terrain_raster_transform");
    let json = br#"{"raster_descriptor":2,"spacing_m":[1,1],"registration":"center","later":true}"#;
    let err = RasterDescriptor::from_json(json, ImportLimits::default()).expect_err("future");
    assert_eq!(err.code(), "terrain_raster_version");
    for bad in [
        &br#"{"raster_descriptor":1,"spacing_m":[0,1],"registration":"center"}"#[..],
        br#"{"raster_descriptor":1,"spacing_m":[1,1],"registration":"center","unknown":1}"#,
        br#"{"raster_descriptor":1,"spacing_m":[1,1],"registration":"corner"}"#,
        br#"{"raster_descriptor":1,"spacing_m":[1,1],"registration":"center","row0":"up"}"#,
    ] {
        let err = RasterDescriptor::from_json(bad, ImportLimits::default()).expect_err("bad descriptor");
        assert_eq!(err.code(), "height_io_decode", "{}", String::from_utf8_lossy(bad));
    }
    let limits = ImportLimits {
        max_bytes: 8,
        ..ImportLimits::default()
    };
    let json = br#"{"raster_descriptor":1,"spacing_m":[1,1],"registration":"center"}"#;
    let err = RasterDescriptor::from_json(json, limits).expect_err("bytes");
    assert_eq!(err.code(), "height_source_limit_bytes");
}

#[test]
fn legacy_schema2_exact_bits_and_mapping_bytes_are_unchanged() {
    let doc = serde_json::json!({
        "schema": 2, "grid": {"cols": 4, "rows": 2, "cell_m": 0.3125, "sample0_x_m": -0.15625},
        "units": {"kind": "counts", "meters_per_count": 0.03125, "zero_counts": 20000},
        "counts": [19968, 20000, 20032, 20064, 20096, 65535, 0, 20001],
    });
    let source =
        TerrainSource::from_json(doc.to_string().as_bytes(), ImportLimits::default()).expect("schema 2");
    assert_eq!(source.layout(), RasterLayout::LEGACY);
    assert_eq!(source.missing_samples(), 0);
    assert_eq!(source.spacing_m(), [0.3125, 0.3125]);
    // Heights (count - zero) * scale and positions sample0 + i * cell, pinned bit for bit.
    assert_eq!(
        bits(&source.heights_m()),
        bits(&[-1.0, 0.0, 1.0, 2.0, 3.0, 45_535.0 / 32.0, -625.0, 0.03125])
    );
    assert_eq!(source.sample0(), [-0.15625, -0.3125]);
    assert_eq!(source.sample_world(3, 1), [0.78125, 0.0]);
    let mapping = source.mapping();
    let mut want = Vec::new();
    want.extend_from_slice(&4u32.to_le_bytes());
    want.extend_from_slice(&2u32.to_le_bytes());
    want.extend_from_slice(&0.03125f32.to_bits().to_le_bytes());
    want.extend_from_slice(&20000u16.to_le_bytes());
    want.extend_from_slice(&0.3125f32.to_bits().to_le_bytes());
    want.extend_from_slice(&(-0.15625f32).to_bits().to_le_bytes());
    want.extend_from_slice(&(-0.3125f32).to_bits().to_le_bytes());
    assert_eq!(mapping.canonical_bytes().to_vec(), want);
    assert_eq!(source.count_mapping().expect("legacy mapping"), mapping);
    // The legacy descriptor reproduces the same source exactly.
    let again = source
        .with_raster(
            &RasterDescriptor::legacy(0.3125, Some(source.sample0())),
            ImportLimits::default(),
        )
        .expect("legacy descriptor");
    assert!(RasterDescriptor::legacy(0.3125, None).is_legacy_layout());
    assert_eq!(again.counts(), source.counts());
    assert_eq!(bits(&again.heights_m()), bits(&source.heights_m()));
    assert_eq!(again.mapping().canonical_bytes(), mapping.canonical_bytes());
    // The cook's heightfield.json reader (schema 2) is untouched by the descriptor.
    let json = br#"{"schema":2,"grid":{"cols":4,"rows":2,"cell_m":0.3125,"sample0_x_m":-0.15625},
        "units":{"kind":"counts","meters_per_count":0.03125,"zero_counts":20000}}"#;
    let cooked = CountMapping::from_heightfield_json(json)
        .expect("parse")
        .expect("mapping");
    assert_eq!(cooked.canonical_bytes(), mapping.canonical_bytes());
    // A legacy source's runtime map is unchanged by the new policy argument.
    let square = TerrainSource::from_counts(
        vec![20000; 25],
        CountMapping {
            cols: 5,
            rows: 5,
            meters_per_count: 0.125,
            zero_counts: 1000,
            cell_m: 2.0,
            sample0_m: [30.0, 30.0],
        },
        ImportLimits::default(),
    )
    .expect("square");
    let (map, hole) = square.terrain_map(MissingDataPolicy::Refuse).expect("legacy map");
    assert!(hole.is_none());
    let direct = square
        .mapping()
        .terrain_map(&square.heightfield())
        .expect("direct");
    assert_eq!(map.grid_origin(), direct.grid_origin());
    assert_eq!(map.world_width().to_bits(), direct.world_width().to_bits());
    assert!(
        source
            .with_raster(&RasterDescriptor::legacy(1.0, None), ImportLimits::default())
            .and_then(|s| s.with_raster(&RasterDescriptor::legacy(1.0, None), ImportLimits::default()))
            .is_err(),
        "a descriptor applies once"
    );
}

/// SplitMix64: deterministic inputs for the property loops.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn property_axis_permutations_are_exact_and_invertible() {
    let mut rng = Rng(0x8542);
    for case in 0..300 {
        let dims = [2 + rng.below(9) as u32, 2 + rng.below(9) as u32];
        let n = (dims[0] * dims[1]) as usize;
        let canonical: Vec<u16> = (0..n).map(|_| rng.next() as u16).collect();
        let row0 = if rng.below(2) == 0 {
            RowOrder::South
        } else {
            RowOrder::North
        };
        let col0 = if rng.below(2) == 0 {
            ColOrder::West
        } else {
            ColOrder::East
        };
        let stored = store(&canonical, dims, row0, col0);
        let spacing = [0.25 * (1 + rng.below(8)) as f32, 0.25 * (1 + rng.below(8)) as f32];
        let source = raster(&stored, dims, &descriptor(spacing, row0, col0)).expect("source");
        assert_eq!(
            source.counts(),
            &canonical[..],
            "case {case}: {dims:?} {row0:?} {col0:?}"
        );
        assert_eq!(
            source.counts().len(),
            n,
            "exactly one sample per cell, no interpolation"
        );
        // Applying the same permutation twice is the identity (it is an involution).
        assert_eq!(store(&stored, dims, row0, col0), canonical, "case {case}");
        let [x0, z0] = source.sample_world(0, 0);
        let [x1, z1] = source.sample_world(dims[0] - 1, dims[1] - 1);
        assert!(x1 > x0 && z1 > z0, "canonical order increases on both axes");
    }
}

#[test]
fn property_masks_round_trip_through_permutation() {
    let mut rng = Rng(0x21);
    for case in 0..300 {
        let dims = [2 + rng.below(9) as u32, 2 + rng.below(9) as u32];
        let n = (dims[0] * dims[1]) as usize;
        let mut flags: Vec<bool> = (0..n).map(|_| rng.below(4) != 0).collect();
        flags[rng.below(n as u64) as usize] = true;
        let mask = ValidMask::from_flags(dims[0], dims[1], &flags).expect("mask");
        assert_eq!(mask.to_flags(), flags, "case {case}");
        assert_eq!(mask.invalid_count(), flags.iter().filter(|f| !**f).count());
        let row0 = if rng.below(2) == 0 {
            RowOrder::South
        } else {
            RowOrder::North
        };
        let col0 = if rng.below(2) == 0 {
            ColOrder::West
        } else {
            ColOrder::East
        };
        let mut d = descriptor([1.0, 1.0], row0, col0);
        d.valid_mask = Some(RasterMaskSpec {
            cols: dims[0],
            rows: dims[1],
            valid: store(&flags, dims, row0, col0)
                .into_iter()
                .map(u8::from)
                .collect(),
        });
        let source = raster(&vec![1000; n], dims, &d).expect("source");
        let canonical = source
            .valid_mask()
            .map_or_else(|| vec![true; n], ValidMask::to_flags);
        assert_eq!(canonical, flags, "case {case}: {row0:?} {col0:?}");
        for row in 0..dims[1] {
            for col in 0..dims[0] {
                let valid = flags[(row * dims[0] + col) as usize];
                assert_eq!(source.sample_height_m(col, row).is_some(), valid);
            }
        }
    }
}

#[test]
fn the_json_schema_names_every_descriptor_field() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/formats/terrain-raster-descriptor.schema.json");
    let text = std::fs::read_to_string(&path).expect("schema file");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("schema json");
    let mut d = descriptor([1.0, 1.0], RowOrder::North, ColOrder::East);
    d.nodata_counts = Some(1);
    d.valid_mask = Some(RasterMaskSpec {
        cols: 2,
        rows: 2,
        valid: vec![1; 4],
    });
    let value = serde_json::to_value(&d).expect("serialize");
    let mut fields: Vec<&String> = value.as_object().expect("object").keys().collect();
    let mut named: Vec<&String> = schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .collect();
    fields.sort();
    named.sort();
    assert_eq!(fields, named);
    let mask: Vec<&String> = value["valid_mask"].as_object().expect("mask").keys().collect();
    for key in mask {
        assert!(
            schema["properties"]["valid_mask"]["properties"]
                .get(key)
                .is_some(),
            "{key}"
        );
    }
    assert_eq!(
        schema["properties"]["raster_descriptor"]["const"],
        RASTER_DESCRIPTOR_VERSION
    );
    // The serialized descriptor reads back as itself.
    let back = RasterDescriptor::from_json(value.to_string().as_bytes(), ImportLimits::default())
        .expect("round trip");
    assert_eq!(back, d);
}
