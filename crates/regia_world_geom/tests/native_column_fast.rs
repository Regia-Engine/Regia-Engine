// SPDX-License-Identifier: MPL-2.0
//! Lane W-11 D4 (ADR-8817): the fast column path (the fine directory, the millimetre bounds
//! window, the small buffers, the ground table) returns what the per-call form returns, bit for bit:
//! every span of `floors_at`, the overflow flag, the plane-test count, `support` at several
//! heights, and the roof lookups (`ceiling_above`, `surface_at`), on the four T3 vector fixtures
//! and on the cooked Switchbacks, Cinder Steps and Atacama Rail maps, with D1 craters on the ground
//! and with blocker plugs clear and active.
#![cfg(all(feature = "stage_b", feature = "stats", feature = "fixtures"))]
#![allow(clippy::unwrap_used)]
pub mod native_mouths;
pub mod native_support;
use regia_world_geom::blockers::BlockerId;
use regia_world_geom::{GeomState, GroundEdit, QueryStats, Span, SpanBuf, SurfaceHit, WorldGeom, m_to_mm};

/// A small deterministic generator (SplitMix64).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `[-1, 1)`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

/// A hit as comparable bits: height, normal, and the rest as text.
type HitBits = (u32, [u32; 3], String);

fn hit_bits(h: &SurfaceHit) -> HitBits {
    (
        h.y.to_bits(),
        h.normal.map(f32::to_bits),
        format!("{:?} {:?} {:?} {}", h.kind, h.tri, h.section, h.material),
    )
}

fn span_bits(s: &Span) -> (HitBits, Option<HitBits>) {
    (hit_bits(&s.floor), s.ceiling.as_ref().map(hit_bits))
}

/// What one map's comparison covered.
#[derive(Debug, Default)]
struct Seen {
    points: u64,
    spans: u64,
    lids: u64,
    multi_floor: u64,
    plane_tests: u64,
}

/// Compares every column query at `(x, z)` with the per-call form.
fn compare(geom: &WorldGeom, state: &GeomState, x: f32, z: f32, heights: &[f32], seen: &mut Seen) {
    let mut fast = SpanBuf::default();
    let mut reference = SpanBuf::default();
    let mut fast_stats = QueryStats::default();
    let mut reference_stats = QueryStats::default();
    geom.floors_at_with_stats(state, x, z, &mut fast, &mut fast_stats);
    geom.floors_at_reference(state, x, z, &mut reference, &mut reference_stats);
    assert_eq!(fast.overflowed, reference.overflowed, "x={x} z={z}");
    assert_eq!(fast_stats, reference_stats, "x={x} z={z}");
    assert_eq!(fast.spans().len(), reference.spans().len(), "x={x} z={z}");
    for (a, b) in fast.spans().iter().zip(reference.spans()) {
        assert_eq!(span_bits(a), span_bits(b), "x={x} z={z}");
    }
    seen.points += 1;
    seen.spans += reference.spans().len() as u64;
    seen.multi_floor += u64::from(reference.spans().len() > 2);
    seen.plane_tests += u64::from(reference_stats.triangle_tests);
    let mut ys: Vec<f32> = heights.to_vec();
    for span in reference.spans() {
        ys.extend([span.floor.y, span.floor.y + 0.9, span.floor.y - 0.01]);
        if let Some(lid) = &span.ceiling {
            ys.extend([lid.y, lid.y + 0.3]);
            seen.lids += 1;
        }
        // The lowest roof over a floor is that span's lid: the roof lookup against the column.
        if !reference.overflowed && x.is_finite() && z.is_finite() {
            let over = geom.ceiling_above(state, x, z, span.floor.y);
            assert_eq!(
                over.as_ref().map(hit_bits),
                span.ceiling.as_ref().map(hit_bits),
                "ceiling_above x={x} z={z} floor={}",
                span.floor.y
            );
        }
    }
    for y in ys {
        let a = geom.support(state, x, z, y);
        let b = geom.support_reference(state, x, z, y);
        assert_eq!(span_bits(&a), span_bits(&b), "support x={x} z={z} y={y}");
        if y.is_finite() && x.is_finite() && z.is_finite() {
            // `surface_at` reads the same column and the same roofs.
            let at = geom.surface_at(state, x, z, y, 0.05);
            let under = reference
                .spans()
                .iter()
                .rfind(|s| s.floor.y <= y + 0.05)
                .map(|s| hit_bits(&s.floor));
            assert_eq!(
                at.under.as_ref().map(hit_bits),
                under,
                "surface_at x={x} z={z} y={y}"
            );
        }
    }
}

/// The points of one cooked map: every triangle vertex and edge midpoint (edge and corner
/// ownership), their one-ulp neighbours, points scattered around each, and the non-finite and
/// off-map cases.
fn map_points(geom: &WorldGeom, seed: u64, per_vertex: usize) -> Vec<(f32, f32)> {
    let mut rng = Rng(seed);
    let mut points = Vec::new();
    for tri in geom.triangles() {
        for k in 0..3 {
            let (a, b) = (tri.v[k], tri.v[(k + 1) % 3]);
            points.push((a[0], a[2]));
            points.push(((a[0] + b[0]) * 0.5, (a[2] + b[2]) * 0.5));
            if k == 0 {
                points.push((a[0].next_up(), a[2]));
                points.push((a[0], a[2].next_down()));
                let c = tri.v[2];
                points.push(((a[0] + b[0] + c[0]) / 3.0, (a[2] + b[2] + c[2]) / 3.0));
            }
            for _ in 0..per_vertex {
                points.push((a[0] + rng.unit() * 6.0, a[2] + rng.unit() * 6.0));
            }
        }
    }
    for _ in 0..2_000 {
        points.push((rng.unit() * 700.0, rng.unit() * 700.0));
    }
    points.extend([
        (f32::NAN, 0.0),
        (0.0, f32::INFINITY),
        (1.0e9, 3.0),
        (-17_000.0, 16_500.0),
        (640.0, 640.0),
        (-640.0, -640.0),
    ]);
    points
}

const HEIGHTS: [f32; 5] = [0.0, -256.0, 40.0, f32::NAN, -1.0e6];

#[test]
fn fast_column_equals_the_per_call_form_on_the_t3_fixtures() {
    for name in native_support::FIXTURES {
        let file = native_support::read(name);
        let geom = native_support::native(&file, name, 16.0);
        let state = geom.new_state();
        let mut seen = Seen::default();
        for row in file["columns"].as_array().unwrap() {
            let x = native_support::hex(&row["x"]);
            let z = native_support::hex(&row["z"]);
            let ats: Vec<f32> = row["at"]
                .as_array()
                .unwrap()
                .iter()
                .map(|at| native_support::hex(&at[0]))
                .collect();
            compare(&geom, &state, x, z, &ats, &mut seen);
        }
        // The bench's points (`benches/column.rs`): plane tests per column and floors per column.
        eprintln!(
            "COLUMN_T3_ROWS {name} columns={} plane_tests_per_column={:.2} spans_per_column={:.2} lids={}",
            seen.points,
            seen.plane_tests as f64 / seen.points as f64,
            seen.spans as f64 / seen.points as f64,
            seen.lids
        );
        // The canyon cook holds a 33 MB lattice; its triangles are the Switchbacks mesh's.
        let per_vertex = if name == "switchbacks_canyon" { 0 } else { 2 };
        for (x, z) in map_points(&geom, 5, per_vertex) {
            compare(&geom, &state, x, z, &HEIGHTS, &mut seen);
        }
        eprintln!("COLUMN_FAST_EQUAL {name} {seen:?}");
        assert!(
            seen.spans > seen.points / 2 && seen.lids > 0 && seen.plane_tests > 0,
            "{name}: {seen:?}"
        );
    }
}

#[test]
fn fast_column_equals_the_per_call_form_on_the_cooked_maps() {
    for map in ["switchbacks", "cinder_steps", "atacama_rail"] {
        let (_, geom, _, sampled) = native_mouths::world(map);
        let points = map_points(&geom, 9, 1);
        let mut state = geom.new_state();
        let mut seen = Seen::default();
        for &(x, z) in &points {
            compare(&geom, &state, x, z, &HEIGHTS, &mut seen);
        }
        eprintln!("COLUMN_FAST_EQUAL {map} clear {seen:?}");
        // Cinder Steps has no lid over its floors on this line (the Stage B correctness lane's row).
        assert!(
            seen.spans > seen.points / 2 && seen.plane_tests > 0,
            "{map}: {seen:?}"
        );
        // Craters along every passage (the D1 overrides and promoted tiles of the ground path).
        let mut craters = 0;
        for passage in &sampled.passages {
            for sample in passage.samples.iter().step_by(9) {
                let edit = GroundEdit::Crater {
                    x_mm: m_to_mm(sample.p[0]),
                    z_mm: m_to_mm(sample.p[2]),
                    radius_mm: 6_000,
                    depth_mm: 1_500,
                };
                craters += u32::from(state.apply_ground_edit(&geom, &edit).is_ok());
            }
        }
        let mut edited = Seen::default();
        for &(x, z) in &points {
            compare(&geom, &state, x, z, &HEIGHTS, &mut edited);
        }
        eprintln!("COLUMN_FAST_EQUAL {map} craters={craters} {edited:?}");
        assert!(craters > 0, "{map}: no crater applied");
    }
}

/// The Switchbacks fixture cooked with two collapsible runs (as `tests/blockers.rs` cooks it).
fn plugged_switchbacks() -> WorldGeom {
    use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
    use regia_world_geom::compat::fixtures::for_map;
    use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
    use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
    use regia_world_geom::passages::PassagesDocument;
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0), (passage: \"tube\", from_s_m: 200.0, to_s_m: 230.0)])])";
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    };
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    let out = match run(doc) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(doc, &report).unwrap())
        }
        other => other,
    }
    .unwrap();
    WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap()
}

/// Active blocker plugs put their faces in the column (a lid under each plug, a floor on top):
/// the fast path appends them as the per-call form does, with one plug active and with both.
#[test]
fn fast_column_equals_the_per_call_form_with_plugs_active() {
    let geom = plugged_switchbacks();
    let plugs: Vec<u16> = geom.blockers().iter().map(|p| p.id).collect();
    assert_eq!(plugs.len(), 2);
    let points = map_points(&geom, 21, 1);
    let mut state = geom.new_state();
    let mut clear = Seen::default();
    for &(x, z) in &points {
        compare(&geom, &state, x, z, &HEIGHTS, &mut clear);
    }
    state.set_blocker(&geom, BlockerId(plugs[0]), true).unwrap();
    let mut one = Seen::default();
    for &(x, z) in &points {
        compare(&geom, &state, x, z, &HEIGHTS, &mut one);
    }
    state.set_blocker(&geom, BlockerId(plugs[1]), true).unwrap();
    let mut both = Seen::default();
    for &(x, z) in &points {
        compare(&geom, &state, x, z, &HEIGHTS, &mut both);
    }
    eprintln!(
        "COLUMN_FAST_EQUAL plugs clear {clear:?}\nCOLUMN_FAST_EQUAL plugs one {one:?}\nCOLUMN_FAST_EQUAL plugs both {both:?}"
    );
    // A plug's faces add spans and plane tests to the columns through it.
    assert!(
        one.spans > clear.spans && both.spans > one.spans,
        "{clear:?} {one:?} {both:?}"
    );
    assert!(one.plane_tests > clear.plane_tests && both.plane_tests > one.plane_tests);
}
