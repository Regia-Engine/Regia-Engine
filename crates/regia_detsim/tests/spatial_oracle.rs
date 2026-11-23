// SPDX-License-Identifier: MPL-2.0
//! Slot-order and floating-point boundary oracles for the spatial index (ADR-8318).
use std::ops::ControlFlow;

use regia_detsim::broadphase::{BroadBody, FloorRule};

#[path = "support/spatial_fixture.rs"]
mod fixture;
use fixture::*;

#[test]
fn within_matches_the_linear_scan_in_slot_order() {
    let mut rows = field();
    let mut index = grid(FloorRule::Bands);
    let mut actual = Vec::new();
    for tick in 0..600 {
        move_rows(&mut rows, tick);
        index.build(&rows).expect("valid test slot");
        for q in &rows {
            let mask = !(1 << (q.slot % 40));
            actual.clear();
            let _ = index.within(q.body, 80.0, mask, |slot| {
                if accepted(&q.body, 80.0, mask, &rows[slot], FloorRule::Bands) {
                    actual.push(slot);
                }
                ControlFlow::Continue(())
            });
            let expected: Vec<_> = rows
                .iter()
                .filter(|b| accepted(&q.body, 80.0, mask, b, FloorRule::Bands))
                .map(|b| b.slot)
                .collect();
            assert_eq!(actual, expected, "tick {tick}, query {}", q.slot);
        }
    }
}
#[test]
fn nearest_matches_the_linear_scan_with_lowest_slot_ties() {
    let mut rows = field();
    let mut index = grid(FloorRule::Bands);
    for tick in 0..600 {
        move_rows(&mut rows, tick);
        // Deliberate equal-distance ties, in different cell directions.
        rows[1].body.pos = [rows[0].body.pos[0] + 1.0, 0.0, rows[0].body.pos[2]];
        rows[2].body.pos = [rows[0].body.pos[0] - 1.0, 0.0, rows[0].body.pos[2]];
        index.build(&rows).expect("valid test slot");
        for q in &rows {
            let mask = !(1 << (q.slot % 40));
            let mut best = None;
            let mut best_d = 120.0 * 120.0;
            for b in &rows {
                if b.group_mask & mask != 0 {
                    let d = distance(&q.body, &b.body);
                    if d <= best_d && (best.is_none() || d < best_d) {
                        best = Some(b.slot);
                        best_d = d;
                    }
                }
            }
            assert_eq!(
                index.nearest(q.body, mask, 120.0, |slot| distance(&q.body, &rows[slot].body)),
                best,
                "tick {tick} query {}",
                q.slot
            );
        }
    }
}
#[test]
fn updates_inside_a_query_see_moves_without_revisiting_slots() {
    let mut rows = vec![
        row(0, 0.0, 0.0),
        row(1, 500.0, 0.0),
        row(2, 0.0, 0.0),
        row(3, 500.0, 0.0),
    ];
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid test slot");
    let q = rows[0].body;
    let mut seen = Vec::new();
    let _ = index.within_mut(q, 2.0, u64::MAX, |index, slot| {
        if accepted(&q, 2.0, u64::MAX, &rows[slot], FloorRule::Never) {
            seen.push(slot);
        }
        if slot == 0 {
            rows[1].body.pos = [0.5, 0.0, 0.0];
            index.update(1, [0.5, 0.0]).expect("valid test slot");
            rows[2].body.pos = [500.0, 0.0, 0.0];
            index.update(2, [500.0, 0.0]).expect("valid test slot");
        }
        if slot == 1 {
            rows[0].body.pos = [1.0, 0.0, 0.0];
            index.update(0, [1.0, 0.0]).expect("valid test slot");
            rows[3].body.pos = [1.0, 0.0, 0.0];
            index.update(3, [1.0, 0.0]).expect("valid test slot");
        }
        ControlFlow::Continue(())
    });
    assert_eq!(seen, [0, 1, 3]);
    assert_eq!(index.stats().refiles, 3);
}
#[test]
fn boundary_cases_are_never_decided_by_the_index() {
    let r = 80.0f32;
    for center in [0.0, -0.0, 1e6, -1e6, f32::MAX, f32::NAN] {
        let mut rows: Vec<_> = [
            r,
            f32::from_bits(r.to_bits() - 1),
            f32::from_bits(r.to_bits() + 1),
            -0.0,
            40.0,
            -40.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ]
        .into_iter()
        .enumerate()
        .map(|(slot, x)| row(slot, center + x, 0.0))
        .collect();
        rows.push(row(rows.len(), -f32::MAX, 0.0));
        for radius in [r, 0.0, f32::MIN_POSITIVE, 1e20, f32::INFINITY, f32::NAN] {
            for rule in [FloorRule::Bands, FloorRule::UndergroundOnly, FloorRule::Never] {
                let mut index = grid(rule);
                index.build(&rows).expect("valid test slot");
                let q = BroadBody::new([center, 0.0, 0.0], 0.0, [-1.0, 1.0]);
                let mut seen = Vec::new();
                let _ = index.within(q, radius, u64::MAX, |slot| {
                    if accepted(&q, radius, u64::MAX, &rows[slot], rule) {
                        seen.push(slot);
                    }
                    ControlFlow::Continue(())
                });
                let expected: Vec<_> = rows
                    .iter()
                    .filter(|b| accepted(&q, radius, u64::MAX, b, rule))
                    .map(|b| b.slot)
                    .collect();
                assert_eq!(seen, expected, "center {center}, radius {radius}, rule {rule:?}");
            }
        }
    }
}
#[test]
fn first_k_within_returns_the_first_k_in_slot_order() {
    let rows = field();
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid test slot");
    let q = rows[0].body;
    let mut all = Vec::new();
    let _ = index.within(q, 80.0, u64::MAX, |slot| {
        all.push(slot);
        ControlFlow::Continue(())
    });
    for k in [0, 1, 24, 900, 1000] {
        let mut out = Vec::new();
        index.first_k_within(q, 80.0, u64::MAX, k, &mut out);
        assert_eq!(out, all[..k.min(all.len())]);
        index.first_k_within_by(q, 80.0, u64::MAX, k, &mut out, |slot| {
            accepted(&q, 80.0, u64::MAX, &rows[slot], FloorRule::Never)
        });
        let exact: Vec<_> = rows
            .iter()
            .filter(|b| accepted(&q, 80.0, u64::MAX, b, FloorRule::Never))
            .take(k)
            .map(|b| b.slot)
            .collect();
        assert_eq!(out, exact);
    }
    let mut first = Vec::new();
    assert_eq!(
        index.within(q, 80.0, u64::MAX, |slot| {
            first.push(slot);
            ControlFlow::Break(())
        }),
        ControlFlow::Break(())
    );
    assert_eq!(first, all[..1]);
}
#[test]
fn query_candidates_are_bounded() {
    let rows: Vec<_> = (0..900)
        .map(|slot| row(slot, (slot % 30) as f32 * 30.0, (slot / 30) as f32 * 30.0))
        .collect();
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid test slot");
    for q in &rows {
        let _ = index.within(q.body, 80.0, u64::MAX, |_| ControlFlow::Continue(()));
    }
    // Padded diameter < 162 m, plus one 40 m cell on each edge: < 242 m.
    // At 30 m spacing at most 9 rows per axis => at most 81 candidates/query.
    assert!(index.stats().candidates <= 900 * 81, "{:?}", index.stats());
}
#[test]
fn bands_masks_wild_rows_and_slot_validation() {
    let mut rows = vec![row(0, 0.0, 0.0), row(1, 0.0, 0.0), row(2, f32::NAN, 0.0)];
    rows[1].body.band = [10.0, 11.0];
    for rule in [FloorRule::Bands, FloorRule::UndergroundOnly, FloorRule::Never] {
        for underground in [false, true] {
            rows[1].body.underground = underground;
            let mut index = grid(rule);
            let mut reversed = rows.clone();
            reversed.reverse();
            index.build(&reversed).expect("valid test slot");
            let mut got = Vec::new();
            let _ = index.within(rows[0].body, 2.0, 6, |slot| {
                got.push(slot);
                ControlFlow::Continue(())
            });
            let want: Vec<_> = rows
                .iter()
                .filter(|b| accepted(&rows[0].body, 2.0, 6, b, rule))
                .map(|b| b.slot)
                .collect();
            assert_eq!(got, want);
            assert_eq!(index.stats().wild_rows, 1);
            assert_eq!(
                index
                    .update(99, [0.0, 0.0])
                    .expect_err("invalid test slot")
                    .code(),
                "spatial_slot"
            );
            reversed[0].slot = reversed[1].slot;
            assert_eq!(
                index.build(&reversed).expect_err("invalid test slot").code(),
                "spatial_slot"
            );
        }
    }
}

#[test]
fn moving_during_600_ticks_matches_the_linear_scan_bit_for_bit() {
    let mut indexed = field();
    let mut linear = indexed.clone();
    let mut index = grid(FloorRule::Bands);
    fn push(rows: &mut [regia_detsim::spatial::SpatialRow], slot: usize, tick: usize, q: BroadBody) -> usize {
        let moved = (slot + 73 + tick) % rows.len();
        rows[moved].body.pos[0] = q.pos[0] + (slot % 5) as f32;
        rows[moved].body.pos[2] = q.pos[2] + (slot % 7) as f32;
        moved
    }
    for tick in 0..600 {
        move_rows(&mut indexed, tick);
        move_rows(&mut linear, tick);
        index.build(&indexed).expect("valid fixture");
        for query in 0..900 {
            let q = indexed[query].body;
            let mask = 1 << (query % 40);
            let mut expected = Vec::new();
            for slot in 0..linear.len() {
                if accepted(&q, 80.0, mask, &linear[slot], FloorRule::Bands) {
                    expected.push(slot);
                    push(&mut linear, slot, tick, q);
                }
            }
            let mut actual = Vec::new();
            let _ = index.within_mut(q, 80.0, mask, |index, slot| {
                if accepted(&q, 80.0, mask, &indexed[slot], FloorRule::Bands) {
                    actual.push(slot);
                    let moved = push(&mut indexed, slot, tick, q);
                    let p = indexed[moved].body.pos;
                    index.update(moved, [p[0], p[2]]).expect("valid fixture");
                }
                ControlFlow::Continue(())
            });
            assert_eq!(actual, expected, "tick {tick}, query {query}");
            let mut best = None;
            let mut best_d = 80.0 * 80.0;
            for b in &linear {
                let d = distance(&q, &b.body);
                if b.group_mask & mask != 0 && d <= best_d && (best.is_none() || d < best_d) {
                    best = Some(b.slot);
                    best_d = d;
                }
            }
            assert_eq!(
                index.nearest(q, mask, 80.0, |s| distance(&q, &indexed[s].body)),
                best
            );
        }
        for (a, b) in indexed.iter().zip(&linear) {
            assert_eq!(a.body.pos.map(f32::to_bits), b.body.pos.map(f32::to_bits));
        }
    }
}

#[test]
fn empty_queries_and_failed_build_preserve_the_previous_index() {
    let mut index = grid(FloorRule::Never);
    let q = row(0, 0.0, 0.0).body;
    assert_eq!(index.nearest(q, 1, 80.0, |_| panic!("empty")), None);
    let _ = index.within(q, 80.0, 1, |_| panic!("empty"));
    index.build(&[]).expect("empty build");
    index.build(&[row(0, 0.0, 0.0)]).expect("one row");
    let before = index.stats();
    assert!(index.build(&[row(2, 100.0, 100.0)]).is_err());
    assert_eq!(index.stats(), before);
    assert_eq!(index.nearest(q, 1, 0.0, |_| 0.0), Some(0));
    assert_eq!(index.nearest(q, 1, f32::NAN, |_| 0.0), None);
    assert_eq!(index.nearest(q, 1, -1.0, |_| 0.0), None);
    assert_eq!(index.nearest(q, 1, f32::INFINITY, |_| f32::NAN), None);
    assert_eq!(index.nearest(q, 1, f32::INFINITY, |_| f32::INFINITY), Some(0));
    index.reset_stats();
    assert_eq!(index.stats(), Default::default());
}

#[test]
fn subnormal_cell_sizes_and_extreme_updates_are_conservative() {
    use regia_detsim::broadphase::BroadphaseConfig;
    use regia_detsim::spatial::DetSpatial;
    let rows = [row(0, 0.0, 0.0)];
    for size in [f32::from_bits(1), f32::MIN_POSITIVE, 1.0, f32::MAX] {
        let mut index = DetSpatial::new(BroadphaseConfig {
            cell_size: size,
            max_cells: 4,
            ..Default::default()
        });
        index.build(&rows).expect("one row");
        let mut out = Vec::new();
        index.first_k_within(rows[0].body, 0.0, 1, 1, &mut out);
        assert_eq!(out, [0]);
        for x in [-f32::MAX, f32::MAX, f32::NAN, 0.0] {
            index.update(0, [x, x]).expect("known slot");
            let center = index.row(0).expect("row").body;
            index.first_k_within(center, 0.0, 1, 1, &mut out);
            assert_eq!(out, [0]);
        }
    }
}

// ---- Segment and shape queries (ADR-8478) ----
//
// The "caller's exact tests" below stand in for a game's own predicates. The index only
// prunes; these tests compare the accepted sequence against a linear scan in slot order.

use regia_detsim::spatial::SpatialRow;

struct Lcg(u64);
impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 40) as f32 * (1.0 / 16_777_216.0)
    }
}

/// Squared XZ distance from `p` to the segment `a`-`b`. A NaN parameter (zero length or an
/// overflow) selects the start point, as the segment contract requires.
fn segment_distance(a: [f32; 2], b: [f32; 2], p: [f32; 3]) -> f32 {
    let dx = b[0] - a[0];
    let dz = b[1] - a[1];
    let px = p[0] - a[0];
    let pz = p[2] - a[1];
    let mut t = (px * dx + pz * dz) / (dx * dx + dz * dz);
    if t.is_nan() || t <= 0.0 {
        t = 0.0;
    } else if t > 1.0 {
        t = 1.0;
    }
    let ex = px - t * dx;
    let ez = pz - t * dz;
    ex * ex + ez * ez
}

fn on_floor(q: &BroadBody, mask: u64, b: &SpatialRow, rule: FloorRule) -> bool {
    b.group_mask & mask != 0 && !regia_detsim::broadphase::bodies_on_different_floors(rule, q, &b.body)
}

/// The exact capsule test: NaN distances are accepted (the fixture's NaN policy).
fn hit_segment(from: &BroadBody, to: [f32; 2], r: f32, mask: u64, b: &SpatialRow, rule: FloorRule) -> bool {
    if !on_floor(from, mask, b, rule) {
        return false;
    }
    let reach = r + b.body.radius;
    let d = segment_distance([from.pos[0], from.pos[2]], to, b.body.pos);
    d.partial_cmp(&(reach * reach)) != Some(std::cmp::Ordering::Greater)
}

/// The exact sector test on the row center, within `r + radius` of the apex.
fn hit_sector(
    apex: &BroadBody,
    dir: [f32; 2],
    cos_half: f32,
    r: f32,
    mask: u64,
    b: &SpatialRow,
    rule: FloorRule,
) -> bool {
    if !accepted(apex, r, mask, b, rule) {
        return false;
    }
    let vx = b.body.pos[0] - apex.pos[0];
    let vz = b.body.pos[2] - apex.pos[2];
    let d2 = vx * vx + vz * vz;
    if d2 == 0.0 {
        return true;
    }
    let dot = vx * dir[0] + vz * dir[1];
    let norm = regia_detsim::math::sqrt(d2) * regia_detsim::math::sqrt(dir[0] * dir[0] + dir[1] * dir[1]);
    // NaN accepted, like the range fixture.
    dot.partial_cmp(&(cos_half * norm)) != Some(std::cmp::Ordering::Less)
}

/// The exact disc/box overlap test: clamp the center into the box.
fn hit_box(
    probe: &BroadBody,
    min: [f32; 2],
    max: [f32; 2],
    mask: u64,
    b: &SpatialRow,
    rule: FloorRule,
) -> bool {
    if !on_floor(probe, mask, b, rule) {
        return false;
    }
    let clamp = |v: f32, lo: f32, hi: f32| {
        if v < lo {
            lo
        } else if v > hi {
            hi
        } else {
            v
        }
    };
    let p = b.body.pos;
    let ex = p[0] - clamp(p[0], min[0], max[0]);
    let ez = p[2] - clamp(p[2], min[1], max[1]);
    let r = b.body.radius;
    (ex * ex + ez * ez).partial_cmp(&(r * r)) != Some(std::cmp::Ordering::Greater)
}

/// The 900-row field with disc radii up to 2.5 m and a raised floor on every 17th row.
fn shot_field() -> Vec<SpatialRow> {
    let mut rows = field();
    for b in &mut rows {
        b.body.radius = (b.slot % 11) as f32 * 0.25;
        if b.slot % 17 == 0 {
            b.body.band = [10.0, 11.0];
        }
    }
    rows
}

fn segment_scan(
    rows: &[SpatialRow],
    from: &BroadBody,
    to: [f32; 2],
    r: f32,
    mask: u64,
    rule: FloorRule,
) -> Vec<usize> {
    rows.iter()
        .filter(|b| hit_segment(from, to, r, mask, b, rule))
        .map(|b| b.slot)
        .collect()
}

fn segment_index(
    index: &mut regia_detsim::spatial::DetSpatial,
    rows: &[SpatialRow],
    from: &BroadBody,
    to: [f32; 2],
    r: f32,
    mask: u64,
    rule: FloorRule,
) -> Vec<usize> {
    let mut seen = Vec::new();
    let mut last = None;
    let _ = index.along_segment(*from, to, r, mask, |slot| {
        assert!(last.is_none_or(|l| slot > l), "slot order: {slot} after {last:?}");
        last = Some(slot);
        if hit_segment(from, to, r, mask, &rows[slot], rule) {
            seen.push(slot);
        }
        ControlFlow::Continue(())
    });
    seen
}

#[test]
fn along_segment_matches_the_linear_scan() {
    let mut rows = shot_field();
    let mut index = grid(FloorRule::Bands);
    let mut rng = Lcg(0x5107_5eed);
    let mut walked = 0;
    for tick in 0..600 {
        move_rows(&mut rows, tick);
        index.build(&rows).expect("valid test slot");
        for shot in 0..200 {
            let shooter = (shot * 7 + tick) % rows.len();
            let from = rows[shooter].body;
            let mask = !(1 << (shooter % 40));
            // 0.1 m to 400 m, log-uniform-ish, any direction.
            let len = 0.1 + rng.unit() * rng.unit() * 399.9;
            let (dx, dz) = (rng.unit() * 2.0 - 1.0, rng.unit() * 2.0 - 1.0);
            let norm = regia_detsim::float::det_max(regia_detsim::math::sqrt(dx * dx + dz * dz), 1e-3);
            let to = [from.pos[0] + dx / norm * len, from.pos[2] + dz / norm * len];
            // Radius 0 (a bullet) and 60 m (larger than a 40 m cell).
            let r = if shot % 2 == 0 { 0.0 } else { 60.0 };
            let expected = segment_scan(&rows, &from, to, r, mask, FloorRule::Bands);
            let actual = segment_index(&mut index, &rows, &from, to, r, mask, FloorRule::Bands);
            assert_eq!(actual, expected, "tick {tick}, shot {shot}, len {len}, r {r}");
            let first = index.first_along_segment(from, to, r, mask, |slot| {
                hit_segment(&from, to, r, mask, &rows[slot], FloorRule::Bands)
            });
            assert_eq!(first, expected.first().copied(), "tick {tick}, shot {shot}");
        }
        walked = index.stats().cells_walked;
    }
    let stats = index.stats();
    assert_eq!(stats.segment_queries, 600 * 200 * 2);
    assert!(walked > 0, "{stats:?}");
    // Pruning: far fewer candidates than the linear scan's 900 per query.
    assert!(stats.candidates < 600 * 400 * 900 / 2, "{stats:?}");
}

#[test]
fn segment_edge_cases() {
    // A 40 m lattice: the grid origin is 0, so every multiple of 40 is a cell edge.
    let r = 2.0_f32;
    let up = |v: f32| f32::from_bits(v.to_bits() + 1);
    let down = |v: f32| {
        if v > 0.0 {
            f32::from_bits(v.to_bits() - 1)
        } else {
            -up(-v)
        }
    };
    let mut base: Vec<_> = (0..121)
        .map(|i| row(i, (i % 11) as f32 * 40.0, (i / 11) as f32 * 40.0))
        .collect();
    let extra = [
        // Exactly r from the zero-length segment at (100, 100), then 1 ulp in and out.
        [100.0 + r, 100.0],
        [up(100.0 + r), 100.0],
        [down(100.0 + r), 100.0],
        // Either side of the x = 40 cell edge at distance exactly r, then 1 ulp out.
        [40.0 - r, 200.0],
        [40.0 + r, 200.0],
        [down(40.0 - r), 200.0],
        [up(40.0 + r), 200.0],
        [down(40.0), 120.0],
        // Exactly r past the end point (320, 0) of a shot, and 1 ulp further.
        [320.0 + r, 0.0],
        [up(320.0 + r), 0.0],
        // Rows a long way out: clamped into edge cells.
        [1e6, 1e6],
        [up(1e6), 1e6 + r],
        [-1e6, 200.0],
        [f32::NAN, 0.0],
        [-f32::MAX, f32::MAX],
    ];
    for (i, [x, z]) in extra.into_iter().enumerate() {
        base.push(row(121 + i, x, z));
    }
    for b in &mut base {
        b.group_mask = 1;
    }
    let q = |x: f32, z: f32| BroadBody::new([x, 0.0, z], 0.0, [-1.0, 1.0]);
    let cases: Vec<(BroadBody, [f32; 2], f32)> = vec![
        // Zero-length segment.
        (q(100.0, 100.0), [100.0, 100.0], r),
        (q(80.0, 80.0), [80.0, 80.0], 0.0),
        // Exactly along a cell edge, both directions, and along the grid's outer edge.
        (q(40.0, 0.0), [40.0, 400.0], r),
        (q(40.0, 400.0), [40.0, 0.0], r),
        (q(0.0, 0.0), [400.0, 0.0], 0.0),
        (q(-0.0, 400.0), [-0.0, 0.0], r),
        // Through a lattice corner diagonally (ties in the cell walk).
        (q(0.0, 0.0), [400.0, 400.0], 0.0),
        (q(400.0, 0.0), [0.0, 400.0], r),
        // End point exactly on a disc boundary.
        (q(0.0, 0.0), [320.0, 0.0], r),
        (q(320.0, 0.0), [0.0, 0.0], r),
        // Endpoints at 1e6.
        (q(1e6, 1e6), [up(1e6), 1e6], r),
        (q(0.0, 0.0), [1e6, 1e6], r),
        (q(-1e6, 200.0), [1e6, 200.0], 0.0),
        (q(1e6, -1e6), [-1e6, 1e6], 50.0),
        (q(1e6, 0.0), [1e6, 400.0], r),
        // Radius bigger than the whole grid, negative radius, extreme radius.
        (q(200.0, 200.0), [210.0, 200.0], 1000.0),
        (q(200.0, 200.0), [210.0, 200.0], -r),
        (q(200.0, 200.0), [210.0, 200.0], 1e30),
        // NaN and infinite endpoints, NaN and infinite radius: everything.
        (q(0.0, 0.0), [f32::NAN, 0.0], r),
        (q(f32::NAN, 0.0), [0.0, 0.0], r),
        (q(0.0, 0.0), [f32::INFINITY, 0.0], r),
        (q(0.0, 0.0), [100.0, 0.0], f32::NAN),
        (q(0.0, 0.0), [100.0, 0.0], f32::INFINITY),
        (q(-f32::MAX, 0.0), [f32::MAX, 0.0], r),
    ];
    // Without the far rows the grid keeps its 40 m cells; with them it widens.
    let mut near: Vec<_> = base[..131].to_vec();
    near.push(SpatialRow {
        slot: 131,
        ..base[134]
    });
    for (rows, rule, cell) in [&near, &base]
        .into_iter()
        .flat_map(|rows| [FloorRule::Bands, FloorRule::Never].map(|rule| (rows, rule)))
        .flat_map(|(rows, rule)| [40.0, 1.0, 1e9].map(|cell| (rows, rule, cell)))
    {
        {
            let mut index =
                regia_detsim::spatial::DetSpatial::new(regia_detsim::broadphase::BroadphaseConfig {
                    cell_size: cell,
                    floor_rule: rule,
                    ..Default::default()
                });
            index.build(rows).expect("valid test slot");
            for (from, to, r) in &cases {
                let expected = segment_scan(rows, from, *to, *r, 1, rule);
                let actual = segment_index(&mut index, rows, from, *to, *r, 1, rule);
                assert_eq!(
                    actual, expected,
                    "from {:?} to {to:?} r {r} cell {cell}",
                    from.pos
                );
                let nan = from.pos.iter().chain(to).any(|v| v.is_nan());
                if nan {
                    // Falls back to every mask/floor-matching row, like the wild rule.
                    let mut all = Vec::new();
                    let _ = index.along_segment(*from, *to, *r, 1, |slot| {
                        all.push(slot);
                        ControlFlow::Continue(())
                    });
                    assert_eq!(all, (0..rows.len()).collect::<Vec<_>>());
                }
            }
        }
    }
    // The boundary rows really are boundary cases for the exact test.
    let zero = segment_scan(&base, &q(100.0, 100.0), [100.0, 100.0], r, 1, FloorRule::Bands);
    assert!(zero.contains(&121) && zero.contains(&123) && !zero.contains(&122));
    let edge = segment_scan(&base, &q(40.0, 0.0), [40.0, 400.0], r, 1, FloorRule::Bands);
    assert!(edge.contains(&124) && edge.contains(&125) && !edge.contains(&126) && !edge.contains(&127));
    let end = segment_scan(&base, &q(0.0, 0.0), [320.0, 0.0], r, 1, FloorRule::Bands);
    assert!(end.contains(&129) && !end.contains(&130));
}

#[test]
fn sector_and_box_match_the_linear_scan() {
    let mut rows = shot_field();
    let mut index = grid(FloorRule::Bands);
    let mut rng = Lcg(0xc0e5_b0c5);
    for tick in 0..60 {
        move_rows(&mut rows, tick);
        index.build(&rows).expect("valid test slot");
        for (cone, q) in rows.iter().enumerate() {
            // Friendly-fire cones: allies only, 2 to 120 degrees wide, 20 to 200 m long.
            let mask = q.group_mask;
            let angle = rng.unit() * std::f32::consts::TAU;
            let dir = [regia_detsim::math::cos(angle), regia_detsim::math::sin(angle)];
            let dir = if cone % 3 == 0 {
                [dir[0] * 5.0, dir[1] * 5.0]
            } else {
                dir
            };
            let half = 0.017 + rng.unit() * 1.03;
            let cos_half = regia_detsim::math::cos(half);
            let r = 20.0 + rng.unit() * 180.0;
            let expected: Vec<_> = rows
                .iter()
                .filter(|b| hit_sector(&q.body, dir, cos_half, r, mask, b, FloorRule::Bands))
                .map(|b| b.slot)
                .collect();
            let mut actual = Vec::new();
            let _ = index.within_sector(q.body, dir, cos_half, r, mask, |slot| {
                assert!(actual.last().is_none_or(|&l| slot > l));
                if hit_sector(&q.body, dir, cos_half, r, mask, &rows[slot], FloorRule::Bands) {
                    actual.push(slot);
                }
                ControlFlow::Continue(())
            });
            assert_eq!(actual, expected, "tick {tick}, cone {cone}");
            if cone % 4 == 0 {
                let c = [q.body.pos[0], q.body.pos[2]];
                let (w, h) = (rng.unit() * 120.0, rng.unit() * 60.0);
                // Inverted corners on some queries: the index sorts them.
                let (min, max) = if cone % 8 == 0 {
                    ([c[0] + w, c[1] + h], [c[0] - w, c[1] - h])
                } else {
                    ([c[0] - w, c[1] - h], [c[0] + w, c[1] + h])
                };
                let mask = !q.group_mask;
                let expected: Vec<_> = rows
                    .iter()
                    .filter(|b| hit_box(&q.body, min, max, mask, b, FloorRule::Bands))
                    .map(|b| b.slot)
                    .collect();
                let mut actual = Vec::new();
                let _ = index.within_box(q.body, min, max, mask, |slot| {
                    if hit_box(&q.body, min, max, mask, &rows[slot], FloorRule::Bands) {
                        actual.push(slot);
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(actual, expected, "tick {tick}, box {cone}");
            }
        }
    }
    // Edge parameters: every cone half-angle class, degenerate directions and radii.
    let rows = shot_field();
    index.build(&rows).expect("valid test slot");
    let apex = rows[5].body;
    for dir in [
        [1.0, 0.0],
        [0.0, -1.0],
        [-1.0, 1e-30],
        [0.0, 0.0],
        [f32::NAN, 1.0],
        [1e30, 1e30],
    ] {
        for cos_half in [1.0, 1.5, 0.99999, 0.5, 0.0, -0.5, -1.0, -2.0, f32::NAN] {
            for r in [0.0, 40.0, 150.0, 1e6, f32::INFINITY, f32::NAN, -40.0] {
                let expected: Vec<_> = rows
                    .iter()
                    .filter(|b| hit_sector(&apex, dir, cos_half, r, u64::MAX, b, FloorRule::Bands))
                    .map(|b| b.slot)
                    .collect();
                let mut actual = Vec::new();
                let _ = index.within_sector(apex, dir, cos_half, r, u64::MAX, |slot| {
                    if hit_sector(&apex, dir, cos_half, r, u64::MAX, &rows[slot], FloorRule::Bands) {
                        actual.push(slot);
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(actual, expected, "dir {dir:?} cos {cos_half} r {r}");
            }
        }
    }
    let probe = rows[0].body;
    for (min, max) in [
        ([0.0, 0.0], [0.0, 0.0]),
        ([-450.0, -450.0], [450.0, 450.0]),
        ([10.0, -5.0], [-10.0, 5.0]),
        ([f32::NAN, 0.0], [10.0, 10.0]),
        ([-f32::INFINITY, 0.0], [f32::INFINITY, 1.0]),
        ([-f32::MAX, -f32::MAX], [f32::MAX, f32::MAX]),
        ([1e6, 1e6], [2e6, 2e6]),
    ] {
        let expected: Vec<_> = rows
            .iter()
            .filter(|b| hit_box(&probe, min, max, u64::MAX, b, FloorRule::Bands))
            .map(|b| b.slot)
            .collect();
        let mut actual = Vec::new();
        let _ = index.within_box(probe, min, max, u64::MAX, |slot| {
            if hit_box(&probe, min, max, u64::MAX, &rows[slot], FloorRule::Bands) {
                actual.push(slot);
            }
            ControlFlow::Continue(())
        });
        assert_eq!(actual, expected, "box {min:?} {max:?}");
    }
    assert!(index.stats().shape_queries > 0);
}

/// The game's own history row: what it kept for one entity at a past tick.
#[derive(Clone, Copy)]
struct HistoryRow {
    x: f32,
    z: f32,
    radius: f32,
    team: u8,
}

#[test]
fn rewind_index_matches_a_scan_of_the_history_rows() {
    const KEEP: usize = 16;
    let mut live = shot_field();
    let mut history: Vec<Vec<HistoryRow>> = Vec::new();
    let mut index = grid(FloorRule::Never);
    let mut rewind = grid(FloorRule::Never);
    let mut rng = Lcg(0x1a9c_0de5);
    let to_row = |slot: usize, h: &HistoryRow| SpatialRow {
        slot,
        body: BroadBody::new([h.x, 0.0, h.z], h.radius, [-1.0, 1.0]),
        group_mask: 1 << h.team,
        flags: 0,
    };
    for tick in 0..200 {
        move_rows(&mut live, tick);
        // Entities come and go: the history row count changes from tick to tick.
        let alive = 800 + (tick * 37) % 100;
        let snapshot: Vec<_> = live[..alive]
            .iter()
            .map(|b| HistoryRow {
                x: b.body.pos[0],
                z: b.body.pos[2],
                radius: b.body.radius,
                team: (b.slot % 40) as u8,
            })
            .collect();
        if history.len() == KEEP {
            history.remove(0);
        }
        history.push(snapshot);
        index.build(&live).expect("valid live rows");
        // A cache of one rewind index per distinct tick, keyed by the caller.
        let mut cached = None;
        for shot in 0..40 {
            let lag = shot % history.len().min(4);
            let past = &history[history.len() - 1 - lag];
            if cached != Some(lag) {
                rewind.build_from(past, to_row).expect("valid history rows");
                cached = Some(lag);
            }
            let rows: Vec<_> = past.iter().enumerate().map(|(i, h)| to_row(i, h)).collect();
            let shooter = (shot * 13 + tick) % past.len();
            let from = rows[shooter].body;
            let mask = !(1 << (shooter % 40));
            let len = 1.0 + rng.unit() * 300.0;
            let angle = rng.unit() * std::f32::consts::TAU;
            let to = [
                from.pos[0] + regia_detsim::math::cos(angle) * len,
                from.pos[2] + regia_detsim::math::sin(angle) * len,
            ];
            let expected = segment_scan(&rows, &from, to, 0.5, mask, FloorRule::Never);
            let actual = segment_index(&mut rewind, &rows, &from, to, 0.5, mask, FloorRule::Never);
            assert_eq!(actual, expected, "tick {tick}, lag {lag}, shot {shot}");
            assert_eq!(rewind.row(shooter).map(|r| r.body), Some(from));
        }
    }
    // A history slice whose projection repeats a slot is rejected and keeps the old index.
    let before = rewind.stats();
    let bad = rewind.build_from(&history[0], |_, h| to_row(0, h));
    assert_eq!(bad.map_err(|e| e.code()), Err("spatial_slot"));
    assert_eq!(rewind.stats(), before);
}

#[test]
fn segment_walk_covers_rows_moved_outside_the_build_bounds() {
    for (cell, max_cells) in [(7.0, 65536), (40.0, 65536), (300.0, 65536), (7.0, 16), (40.0, 3)] {
        let mut rng = Lcg(0x0b5e_55ed ^ max_cells as u64);
        let mut rows: Vec<_> = (0..300)
            .map(|slot| row(slot, rng.unit() * 200.0, rng.unit() * 200.0))
            .collect();
        for b in &mut rows {
            b.group_mask = 1;
            b.body.radius = (b.slot % 4) as f32;
        }
        let mut index = regia_detsim::spatial::DetSpatial::new(regia_detsim::broadphase::BroadphaseConfig {
            cell_size: cell,
            max_cells,
            floor_rule: FloorRule::Never,
            ..Default::default()
        });
        index.build(&rows).expect("valid test slot");
        // A third of the rows leave the build bounds; they refile into clamped edge cells.
        for slot in (0..300).step_by(3) {
            let p = [rng.unit() * 4000.0 - 2000.0, rng.unit() * 4000.0 - 2000.0];
            rows[slot].body.pos = [p[0], 0.0, p[1]];
            index.update(slot, p).expect("valid test slot");
        }
        for shot in 0..2000 {
            let a = [rng.unit() * 5000.0 - 2500.0, rng.unit() * 5000.0 - 2500.0];
            // Short and long segments, inside, crossing and outside the grid.
            let scale = [5.0, 100.0, 5000.0][shot % 3];
            let to = [
                a[0] + (rng.unit() - 0.5) * scale,
                a[1] + (rng.unit() - 0.5) * scale,
            ];
            let from = BroadBody::new([a[0], 0.0, a[1]], 0.0, [-1.0, 1.0]);
            let r = [0.0, 3.0, 50.0][shot % 3];
            let expected = segment_scan(&rows, &from, to, r, 1, FloorRule::Never);
            let actual = segment_index(&mut index, &rows, &from, to, r, 1, FloorRule::Never);
            assert_eq!(actual, expected, "cell {cell}, max {max_cells}, shot {shot}");
        }
    }
}

// --- Outliers and large rows (DC-3.4c, ADR-8837) -------------------------------------------------

/// Every row queries `within(r)` once; returns the candidates delivered and checks the accepted
/// slots against the linear scan, in slot order.
fn within_sweep(
    index: &mut regia_detsim::spatial::DetSpatial,
    rows: &[SpatialRow],
    r: f32,
    what: &str,
) -> u64 {
    index.build(rows).expect("valid test slot");
    index.reset_stats();
    let mut actual = Vec::new();
    for q in rows {
        let mask = !(1 << (q.slot % 40));
        actual.clear();
        let _ = index.within(q.body, r, mask, |slot| {
            if accepted(&q.body, r, mask, &rows[slot], FloorRule::Bands) {
                actual.push(slot);
            }
            ControlFlow::Continue(())
        });
        let expected: Vec<_> = rows
            .iter()
            .filter(|b| accepted(&q.body, r, mask, b, FloorRule::Bands))
            .map(|b| b.slot)
            .collect();
        assert_eq!(actual, expected, "{what}: query {}", q.slot);
    }
    index.stats().candidates
}

fn spatial(config: regia_detsim::broadphase::BroadphaseConfig) -> regia_detsim::spatial::DetSpatial {
    regia_detsim::spatial::DetSpatial::new(config)
}

#[test]
fn a_parked_row_does_not_collapse_the_query_grid() {
    use regia_detsim::broadphase::BroadphaseConfig;
    // 900 rows over 900 m x 900 m, plus one parked 1000 km away (a pooled entity).
    let rows = field();
    let mut parked = rows.clone();
    parked.push(row(900, 1.0e6, 1.0e6));
    let bounded = BroadphaseConfig {
        cell_size: 40.0,
        world_bounds: Some([-500.0, -500.0, 500.0, 500.0]),
        ..BroadphaseConfig::default()
    };
    let base = within_sweep(&mut spatial(bounded), &rows, 30.0, "no outlier");
    let mut index = spatial(bounded);
    let got = within_sweep(&mut index, &parked, 30.0, "parked, bounded");
    assert_eq!(index.stats().outlier_rows, 1);
    // Control: without bounds the parked row stretches the grid (the gap this closes).
    let unbounded = BroadphaseConfig {
        world_bounds: None,
        ..bounded
    };
    let collapsed = within_sweep(&mut spatial(unbounded), &parked, 30.0, "parked, unbounded");
    println!("candidates: no outlier {base}, parked with bounds {got}, parked without bounds {collapsed}");
    assert!(
        got * 10 <= base * 11,
        "one parked row raised the candidates from {base} to {got}"
    );
    assert!(
        collapsed > base * 5,
        "the control scene should collapse the grid: {collapsed} vs {base}"
    );
}

#[test]
fn a_large_row_does_not_widen_every_query() {
    use regia_detsim::broadphase::BroadphaseConfig;
    // 900 point rows plus one 300 m disc (a hill) in the middle.
    let rows = field();
    let mut big = rows.clone();
    let mut hill = row(900, 100.0, 100.0);
    hill.body.radius = 300.0;
    big.push(hill);
    let config = BroadphaseConfig {
        cell_size: 40.0,
        ..BroadphaseConfig::default()
    };
    let base = within_sweep(&mut spatial(config), &rows, 30.0, "no large row");
    let mut index = spatial(config);
    let got = within_sweep(&mut index, &big, 30.0, "large row");
    assert_eq!(index.stats().large_rows, 1);
    // Control: with the large class off the hill widens every query (the gap this closes).
    let off = BroadphaseConfig {
        large_radius: Some(f32::INFINITY),
        ..config
    };
    let wide = within_sweep(&mut spatial(off), &big, 30.0, "large class off");
    println!("candidates: no large row {base}, large row {got}, large class off {wide}");
    assert!(
        got * 10 <= base * 12,
        "one large row raised the candidates from {base} to {got}"
    );
    assert!(
        wide > base * 5,
        "the control scene should widen every query: {wide} vs {base}"
    );
}

#[test]
fn random_bounds_outliers_and_large_rows_match_the_linear_scans() {
    use regia_detsim::broadphase::BroadphaseConfig;
    let mut rng = Lcg(0x8837_0001);
    let (mut outliers, mut large) = (0_usize, 0_usize);
    for case in 0..200 {
        let n = 2 + (rng.unit() * 160.0) as usize;
        let side = 5.0 + rng.unit() * 300.0;
        let mut rows: Vec<SpatialRow> = (0..n)
            .map(|slot| {
                let mut b = row(
                    slot,
                    (rng.unit() * 2.0 - 1.0) * side,
                    (rng.unit() * 2.0 - 1.0) * side,
                );
                if rng.unit() < 0.08 {
                    let far = if rng.unit() < 0.5 {
                        1.0e6
                    } else {
                        1.0e3 + rng.unit() * 1.0e7
                    };
                    b.body.pos[0] = if rng.unit() < 0.5 { far } else { -far };
                }
                if rng.unit() < 0.02 {
                    b.body.pos[2] = f32::NAN;
                }
                b.body.radius = match (rng.unit() * 20.0) as u32 {
                    0 => 5.0 + rng.unit() * 120.0,
                    1 => 2.0 + rng.unit() * 10.0,
                    _ => rng.unit() * 3.0,
                };
                if slot % 13 == 0 {
                    b.body.band = [10.0, 11.0];
                }
                b
            })
            .collect();
        let world_bounds = if rng.unit() < 0.25 {
            None
        } else {
            let w = side * (0.2 + rng.unit() * 1.3);
            Some([-w, -w * (0.5 + rng.unit()), w, w])
        };
        let large_radius = match (rng.unit() * 4.0) as u32 {
            0 => None,
            1 => Some(f32::INFINITY),
            _ => Some(0.5 + rng.unit() * 40.0),
        };
        let config = BroadphaseConfig {
            cell_size: 0.5 + rng.unit() * 40.0,
            max_cells: if rng.unit() < 0.2 {
                1 + (rng.unit() * 16.0) as usize
            } else {
                1 << 16
            },
            floor_rule: FloorRule::Bands,
            world_bounds,
            large_radius,
            ..BroadphaseConfig::default()
        };
        let mut index = spatial(config);
        index.build(&rows).expect("valid test slot");
        // Move a few rows across the bounds after the build.
        for slot in (0..n).step_by(7) {
            let p = [
                (rng.unit() * 2.0 - 1.0) * side * 3.0,
                (rng.unit() * 2.0 - 1.0) * side * 3.0,
            ];
            rows[slot].body.pos[0] = p[0];
            rows[slot].body.pos[2] = p[1];
            index.update(slot, p).expect("valid test slot");
        }
        outliers += index.stats().outlier_rows;
        large += index.stats().large_rows;
        let what = format!("case {case} {config:?}");
        for q in rows.iter().step_by(3) {
            let mask = !(1 << (q.slot % 40));
            let r = rng.unit() * 60.0;
            let mut actual = Vec::new();
            let _ = index.within(q.body, r, mask, |slot| {
                if accepted(&q.body, r, mask, &rows[slot], FloorRule::Bands) {
                    actual.push(slot);
                }
                ControlFlow::Continue(())
            });
            let expected: Vec<_> = rows
                .iter()
                .filter(|b| accepted(&q.body, r, mask, b, FloorRule::Bands))
                .map(|b| b.slot)
                .collect();
            assert_eq!(actual, expected, "{what}: within {}", q.slot);
            assert_eq!(
                index.any_within(q.body, r, mask, |slot| accepted(
                    &q.body,
                    r,
                    mask,
                    &rows[slot],
                    FloorRule::Bands
                )),
                !expected.is_empty(),
                "{what}: any_within {}",
                q.slot
            );
            // nearest by center distance (the row radius does not count), lowest slot on ties.
            let mut best = None;
            let mut best_d = r * r;
            for b in &rows {
                if on_floor(&q.body, mask, b, FloorRule::Bands) {
                    let d = distance(&q.body, &b.body);
                    if d <= best_d && (best.is_none() || d < best_d) {
                        best = Some(b.slot);
                        best_d = d;
                    }
                }
            }
            assert_eq!(
                index.nearest(q.body, mask, r, |slot| distance(&q.body, &rows[slot].body)),
                best,
                "{what}: nearest {}",
                q.slot
            );
            let to = [
                q.body.pos[0] + (rng.unit() - 0.5) * side,
                q.body.pos[2] + (rng.unit() - 0.5) * side,
            ];
            let seg = r * 0.25;
            assert_eq!(
                segment_index(&mut index, &rows, &q.body, to, seg, mask, FloorRule::Bands),
                segment_scan(&rows, &q.body, to, seg, mask, FloorRule::Bands),
                "{what}: segment {}",
                q.slot
            );
            let lo = [q.body.pos[0] - r, q.body.pos[2] - r * 0.5];
            let hi = [q.body.pos[0] + r * 0.5, q.body.pos[2] + r];
            let mut boxed = Vec::new();
            let _ = index.within_box(q.body, lo, hi, mask, |slot| {
                if hit_box(&q.body, lo, hi, mask, &rows[slot], FloorRule::Bands) {
                    boxed.push(slot);
                }
                ControlFlow::Continue(())
            });
            let want: Vec<_> = rows
                .iter()
                .filter(|b| hit_box(&q.body, lo, hi, mask, b, FloorRule::Bands))
                .map(|b| b.slot)
                .collect();
            assert_eq!(boxed, want, "{what}: box {}", q.slot);
        }
    }
    assert!(outliers > 100, "only {outliers} outlier rows");
    assert!(large > 100, "only {large} large rows");
}
