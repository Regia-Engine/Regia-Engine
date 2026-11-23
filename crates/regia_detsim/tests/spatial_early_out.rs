// SPDX-License-Identifier: MPL-2.0
//! Early-out counters of the spatial index on fixed scenes (ADR-8875): `nearest` stops at the
//! first ring past the incumbent and `first_k_within(_by)` stops gathering at the k-th hit,
//! while every result (lowest-slot ties included), visit sequence and candidate count stays
//! the one a full walk gives.
use std::ops::ControlFlow;

use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, FloorRule};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

#[path = "support/spatial_fixture.rs"]
mod fixture;
use fixture::*;

/// The linear-scan nearest: lowest slot among equal distances, `d <= max_r²`.
fn linear_nearest(rows: &[SpatialRow], q: &BroadBody, mask: u64, max_r: f32) -> Option<usize> {
    let mut best = None;
    let mut best_d = max_r * max_r;
    for b in rows {
        if b.group_mask & mask != 0 {
            let d = distance(q, &b.body);
            if d <= best_d && (best.is_none() || d < best_d) {
                best = Some(b.slot);
                best_d = d;
            }
        }
    }
    best
}

/// 900 rows, 40 m cells, 120 m nearest (the evidence mix's query): the walk used to read every
/// cell of the 120 m rectangle (the same rectangle and lists `within(.., 120.0, ..)` reads, so
/// the same candidate count). With the early-out the incumbent (about 15 m on this field) ends
/// the walk after rings 0 and 1 for almost every query.
#[test]
fn nearest_stops_at_the_first_ring_past_the_incumbent() {
    let rows = field();
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid rows");
    let mut full = 0_u64;
    let mut early = 0_u64;
    let mut rings = 0_u64;
    for q in &rows {
        let mask = !(1 << (q.slot % 40));
        let before = index.stats();
        let _ = index.within(q.body, 120.0, mask, |_| ControlFlow::Continue(()));
        let mid = index.stats();
        let found = index.nearest(q.body, mask, 120.0, |slot| distance(&q.body, &rows[slot].body));
        let after = index.stats();
        assert_eq!(
            found,
            linear_nearest(&rows, &q.body, mask, 120.0),
            "query {}",
            q.slot
        );
        let walked = after.candidates - mid.candidates;
        let gathered = mid.candidates - before.candidates;
        assert!(walked <= gathered, "query {}: {walked} > {gathered}", q.slot);
        full += gathered;
        early += walked;
        rings += after.nearest_rings - mid.nearest_rings;
    }
    println!("nearest candidates: full walk {full}, early-out {early}; rings {rings} for 900 queries");
    // Fail-before (the full walk): early == full and 4 to 7 rings a query.
    assert!(early * 3 <= full, "early-out tested {early} of {full} candidates");
    assert!((1800..=2000).contains(&rings), "rings {rings}");
}

/// Two rows at one distance from the point, the lower slot in ring 2 and the higher in ring 1:
/// the incumbent after ring 1 does not exclude ring 2 (equal scores tie), so the lower slot wins.
#[test]
fn equal_scores_across_rings_keep_the_lowest_slot() {
    let at = |slot: usize, x: f32, z: f32| SpatialRow {
        slot,
        body: BroadBody::new([x, 0.0, z], 0.0, [-1.0, 1.0]),
        group_mask: 1,
        flags: 0,
    };
    // Origin -200 (the lowest row), 40 m cells: x = 39 is in cell 5, -2 in cell 4 (ring 1) and
    // 80 in cell 7 (ring 2); both are exactly 41 m away.
    let mut rows: Vec<_> = (0..10)
        .map(|slot| at(slot, -200.0, 200.0 + slot as f32 * 10.0))
        .collect();
    rows[3] = at(3, 80.0, 0.0);
    rows[7] = at(7, -2.0, 0.0);
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid rows");
    let point = BroadBody::new([39.0, 0.0, 0.0], 0.0, [-1.0, 1.0]);
    let before = index.stats();
    let found = index.nearest(point, 1, 120.0, |slot| distance(&point, &rows[slot].body));
    assert_eq!(found, Some(3));
    assert_eq!(found, linear_nearest(&rows, &point, 1, 120.0));
    let rings = index.stats().nearest_rings - before.nearest_rings;
    assert_eq!(rings, 3, "rings 0, 1 and 2; ring 3 starts beyond 80 m");
    // 39.5 m on the ring-1 side: ring 2 starts more than 39.8 m out (40 m less the rounding
    // pad), so it can neither win nor tie and is skipped.
    rows[7] = at(7, -0.5, 0.0);
    index.build(&rows).expect("valid rows");
    let before = index.stats();
    let found = index.nearest(point, 1, 120.0, |slot| distance(&point, &rows[slot].body));
    assert_eq!(found, Some(7));
    assert_eq!(found, linear_nearest(&rows, &point, 1, 120.0));
    assert_eq!(index.stats().nearest_rings - before.nearest_rings, 2);
}

/// Moves (immediate updates, rows sent outside the build bounds and to NaN), wide and huge
/// radii and an infinite radius: every result equals the linear scan.
#[test]
fn nearest_early_out_matches_the_linear_scan_after_moves() {
    let mut rows = field();
    for (radius, cell) in [(0.0_f32, 40.0_f32), (3.0, 40.0), (0.0, 7.0), (0.0, 1000.0)] {
        for row in &mut rows {
            row.body.radius = radius;
        }
        let mut index = DetSpatial::new(BroadphaseConfig {
            cell_size: cell,
            ..Default::default()
        });
        let mut moved = rows.clone();
        for tick in 0..30_usize {
            // A fresh build each tick (rows sent away earlier stretch or clamp the grid), then
            // immediate updates on top of it.
            move_rows(&mut moved, tick);
            index.build(&moved).expect("valid rows");
            for step in 0..30 {
                let slot = (tick * 31 + step * 17) % moved.len();
                let pos = match step % 5 {
                    0 => [2000.0, -3000.0],
                    1 => [f32::NAN, 0.0],
                    _ => [
                        moved[slot].body.pos[0] + (step as f32 - 15.0) * 3.0,
                        moved[slot].body.pos[2] - (tick as f32 - 15.0) * 2.0,
                    ],
                };
                moved[slot].body.pos[0] = pos[0];
                moved[slot].body.pos[2] = pos[1];
                index.update(slot, pos).expect("valid slot");
            }
            for q in moved.iter().step_by(7) {
                let mask = !(1 << (q.slot % 40));
                for max_r in [0.0, 25.0, 120.0, 1.0e4, f32::INFINITY] {
                    let found =
                        index.nearest(q.body, mask, max_r, |slot| distance(&q.body, &moved[slot].body));
                    assert_eq!(
                        found,
                        linear_nearest(&moved, &q.body, mask, max_r),
                        "radius {radius}, cell {cell}, tick {tick}, query {}, max_r {max_r}",
                        q.slot
                    );
                }
            }
        }
    }
}

/// `first_k_within` with k = 1 reads one list per query (the one holding the lowest candidate
/// slot); exact first-k reads far fewer lists than a full gather. Results, the visit sequence
/// and the candidate count equal `within` plus a break at the k-th hit (the old path).
#[test]
fn first_k_stops_gathering_at_the_kth_hit() {
    let rows = field();
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid rows");
    let mut lists = [0_u64; 3];
    let mut out = Vec::new();
    for q in &rows {
        let before = index.stats();
        index.first_k_within(q.body, 80.0, u64::MAX, 1, &mut out);
        let one = index.stats();
        index.first_k_within_by(q.body, 80.0, u64::MAX, 3, &mut out, |slot| {
            slot != q.slot && accepted(&q.body, 80.0, u64::MAX, &rows[slot], FloorRule::Never)
        });
        let three = index.stats();
        index.first_k_within(q.body, 80.0, u64::MAX, usize::MAX, &mut out);
        let all = index.stats();
        lists[0] += one.first_k_lists - before.first_k_lists;
        lists[1] += three.first_k_lists - one.first_k_lists;
        lists[2] += all.first_k_lists - three.first_k_lists;
    }
    println!(
        "first-k lists read: k=1 {}, exact k=3 {}, all {}",
        lists[0], lists[1], lists[2]
    );
    // Fail-before: `first_k_lists` did not exist, and the gather read every list of the rectangle.
    assert_eq!(lists[0], 900, "k = 1 reads the list of the lowest candidate only");
    assert!(
        lists[1] * 2 <= lists[2],
        "exact k = 3 read {} of {} lists",
        lists[1],
        lists[2]
    );
}

/// The old first-k path: `within` and a break at the k-th acceptance.
fn old_first_k(
    index: &mut DetSpatial,
    q: BroadBody,
    r: f32,
    mask: u64,
    k: usize,
    accept: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let mut out = Vec::new();
    let _ = index.within(q, r, mask, |slot| {
        if out.len() == k {
            return ControlFlow::Break(());
        }
        if accept(slot) {
            out.push(slot);
        }
        if out.len() == k {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    out
}

/// Updates move rows between cells (and to the wild and large lists), so each list's lowest slot
/// changes; the early stop still yields the old path's results and candidate counts.
#[test]
fn first_k_matches_the_old_path_after_moves() {
    let mut rows = field();
    for slot in [5, 300, 899] {
        rows[slot].body.radius = 500.0;
    }
    let mut index = grid(FloorRule::Never);
    index.build(&rows).expect("valid rows");
    let mut out = Vec::new();
    for tick in 0..40_usize {
        for step in 0..25 {
            let slot = (tick * 37 + step * 11) % rows.len();
            let pos = match step % 6 {
                0 => [f32::NAN, 1.0],
                1 => [-4000.0, 9000.0],
                _ => [
                    rows[slot].body.pos[0] + (step as f32 - 12.0) * 4.0,
                    rows[slot].body.pos[2] + (tick as f32 - 20.0) * 3.0,
                ],
            };
            rows[slot].body.pos[0] = pos[0];
            rows[slot].body.pos[2] = pos[1];
            index.update(slot, pos).expect("valid slot");
        }
        for q in rows.iter().step_by(11) {
            let mask = !(1 << (q.slot % 40));
            for k in [0, 1, 2, 5, 40, 2000] {
                for r in [0.0, 30.0, 80.0] {
                    let accept = |slot: usize| accepted(&q.body, r, mask, &rows[slot], FloorRule::Never);
                    let before = index.stats();
                    let expected = old_first_k(&mut index, q.body, r, mask, k, accept);
                    let mid = index.stats();
                    index.first_k_within_by(q.body, r, mask, k, &mut out, accept);
                    let after = index.stats();
                    let what = format!("tick {tick}, query {}, k {k}, r {r}", q.slot);
                    assert_eq!(out, expected, "{what}");
                    assert_eq!(
                        after.candidates - mid.candidates,
                        mid.candidates - before.candidates,
                        "{what}: candidate count"
                    );
                    let expected = old_first_k(&mut index, q.body, r, mask, k, |_| true);
                    let mid = index.stats();
                    index.first_k_within(q.body, r, mask, k, &mut out);
                    assert_eq!(out, expected, "{what}: conservative");
                    assert_eq!(
                        index.stats().candidates - mid.candidates,
                        mid.candidates - after.candidates,
                        "{what}: conservative candidate count"
                    );
                }
            }
        }
    }
}
