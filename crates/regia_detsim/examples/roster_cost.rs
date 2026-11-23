// SPDX-License-Identifier: MPL-2.0
//! Time the per-unit team filter against `TeamRosters`, and against rosters plus the slot-ordered spatial
//! index, on a 4-team crowd (prints; asserts only that every path agrees):
//! `cargo run --release -p regia_detsim --example roster_cost -- [ticks]`.
//!
//! Each tick every unit finds its nearest enemy (squared XZ distance, lowest slot on ties) and counts allies
//! within 30 m: (1) by filtering every row, the scan the rosters replace; (2) from the rosters built at the top
//! of the tick; (3) from the rosters plus `DetSpatial` (`nearest` and `within` with `team_group_mask`
//! group bits, built once per tick on the same slots). Prints p50 and max milliseconds per tick for 300, 600,
//! 900 and 1500 units, and the team reads per tick.

use std::ops::ControlFlow;
use std::time::Instant;

use regia_detsim::ai::{AiRng, RosterSettings, TeamRosters, team_group_mask};
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

const ALLY_R: f32 = 30.0;

#[derive(Clone, Copy)]
struct Row {
    team: u8,
    pos: [f32; 2],
}

fn crowd(n: usize) -> Vec<Row> {
    let mut rng = AiRng::new(0xC057 ^ n as u64);
    let side = regia_detsim::math::sqrt(n as f32 * 400.0);
    (0..n)
        .map(|_| Row {
            team: (rng.next_u64() % 4) as u8,
            pos: [rng.next_f32() * side, rng.next_f32() * side],
        })
        .collect()
}

fn dist_sq(a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dz) = (a[0] - b[0], a[1] - b[1]);
    dx * dx + dz * dz
}

/// One tick with the per-unit filter: `(sum of nearest slots, sum of ally counts, team reads)`.
fn tick_scan(rows: &[Row]) -> (u64, u64, u64) {
    let (mut near_sum, mut ally_sum, mut reads) = (0u64, 0u64, 0u64);
    for me in rows {
        let mut best = (f32::MAX, 0usize);
        let mut allies = 0u64;
        for (slot, o) in rows.iter().enumerate() {
            reads += 1;
            if o.team != me.team {
                let d = dist_sq(me.pos, o.pos);
                if d < best.0 {
                    best = (d, slot);
                }
            } else if dist_sq(me.pos, o.pos) <= ALLY_R * ALLY_R {
                allies += 1;
            }
        }
        near_sum += best.1 as u64;
        ally_sum += allies;
    }
    (near_sum, ally_sum, reads)
}

/// The same tick from rosters built once.
fn tick_rosters(rows: &[Row], rosters: &mut TeamRosters, enemies: &mut [Vec<u32>; 4]) -> (u64, u64, u64) {
    let mut reads = 0u64;
    rosters
        .build(rows, |r| {
            reads += 1;
            Some(r.team)
        })
        .unwrap_or_else(|e| panic!("{e}"));
    // The enemies of a team are the same for each of its units: fill each list once per tick.
    for (team, list) in enemies.iter_mut().enumerate() {
        rosters.others_into(team as u8, list);
    }
    let (mut near_sum, mut ally_sum) = (0u64, 0u64);
    for me in rows {
        let mut best = (f32::MAX, 0usize);
        for &slot in enemies[usize::from(me.team)].iter() {
            let d = dist_sq(me.pos, rows[slot as usize].pos);
            if d < best.0 {
                best = (d, slot as usize);
            }
        }
        let allies = rosters
            .team(me.team)
            .iter()
            .filter(|&&s| dist_sq(me.pos, rows[s as usize].pos) <= ALLY_R * ALLY_R)
            .count() as u64;
        near_sum += best.1 as u64;
        ally_sum += allies;
    }
    (near_sum, ally_sum, reads)
}

fn body(row: &Row) -> BroadBody {
    BroadBody::new([row.pos[0], 0.0, row.pos[1]], 0.5, [-1.0, 1.0])
}

/// The same tick from rosters plus the spatial index (one build of each per tick).
fn tick_spatial(
    rows: &[Row],
    rosters: &mut TeamRosters,
    spatial: &mut DetSpatial,
    srows: &mut Vec<SpatialRow>,
) -> (u64, u64, u64) {
    let mut reads = 0u64;
    rosters
        .build(rows, |r| {
            reads += 1;
            Some(r.team)
        })
        .unwrap_or_else(|e| panic!("{e}"));
    srows.clear();
    srows.extend(rows.iter().enumerate().map(|(slot, r)| SpatialRow {
        slot,
        body: body(r),
        group_mask: team_group_mask(r.team),
        flags: 0,
    }));
    spatial.build(srows).unwrap_or_else(|e| panic!("{e}"));
    let all_teams = (0..4u8).fold(0u64, |m, t| m | team_group_mask(t));
    let (mut near_sum, mut ally_sum) = (0u64, 0u64);
    for me in rows {
        let own = team_group_mask(me.team);
        let best = spatial.nearest(body(me), all_teams & !own, 1.0e6, |slot| {
            dist_sq(me.pos, rows[slot].pos)
        });
        let mut allies = 0u64;
        let _ = spatial.within(body(me), ALLY_R, own, |slot| {
            if dist_sq(me.pos, rows[slot].pos) <= ALLY_R * ALLY_R {
                allies += 1;
            }
            ControlFlow::Continue(())
        });
        near_sum += best.unwrap_or(0) as u64;
        ally_sum += allies;
    }
    (near_sum, ally_sum, reads)
}

fn p50_max(mut ms: Vec<f64>) -> (f64, f64) {
    ms.sort_by(f64::total_cmp);
    (ms[ms.len() / 2], ms[ms.len() - 1])
}

#[expect(
    clippy::disallowed_methods,
    reason = "bench example: the tick count argument and the wall-clock timing of each path are host-side and never reach the sim (ADR-8367)"
)]
fn main() {
    let ticks: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(30);
    println!("roster_cost ticks={ticks}");
    for n in [300usize, 600, 900, 1500] {
        let rows = crowd(n);
        let mut rosters = TeamRosters::new(RosterSettings::default());
        let mut enemies: [Vec<u32>; 4] = Default::default();
        let mut spatial = DetSpatial::new(BroadphaseConfig {
            cell_size: 20.0,
            ..BroadphaseConfig::default()
        });
        let mut srows = Vec::new();
        let (mut scan_ms, mut roster_ms, mut spatial_ms) = (Vec::new(), Vec::new(), Vec::new());
        let (mut scan_reads, mut roster_reads) = (0, 0);
        for _ in 0..ticks {
            let t0 = Instant::now();
            let a = tick_scan(&rows);
            scan_ms.push(t0.elapsed().as_secs_f64() * 1e3);
            let t1 = Instant::now();
            let b = tick_rosters(&rows, &mut rosters, &mut enemies);
            roster_ms.push(t1.elapsed().as_secs_f64() * 1e3);
            assert_eq!((a.0, a.1), (b.0, b.1), "the rosters changed a result");
            let t2 = Instant::now();
            let c = tick_spatial(&rows, &mut rosters, &mut spatial, &mut srows);
            spatial_ms.push(t2.elapsed().as_secs_f64() * 1e3);
            assert_eq!((a.0, a.1), (c.0, c.1), "the spatial index changed a result");
            (scan_reads, roster_reads) = (a.2, b.2);
        }
        let (s50, smax) = p50_max(scan_ms);
        let (r50, rmax) = p50_max(roster_ms);
        let (g50, gmax) = p50_max(spatial_ms);
        println!(
            "n={n} scan_p50_ms={s50:.3} scan_max_ms={smax:.3} roster_p50_ms={r50:.3} roster_max_ms={rmax:.3} \
             roster_spatial_p50_ms={g50:.3} roster_spatial_max_ms={gmax:.3} \
             scan_team_reads={scan_reads} roster_team_reads={roster_reads}"
        );
    }
}
