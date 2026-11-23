// SPDX-License-Identifier: MPL-2.0
//! Time the sighting refresh: the all-pairs scan (`SightingsBoard::refresh_linear`) against the indexed
//! refresh (`refresh_with` over a reused `PerceptionIndex`), on a 4-team battle (prints; asserts only that
//! both boards agree every tick):
//! `cargo run --release -p regia_detsim --example perception_cost -- [ticks] [cell_size_m]`.
//!
//! Units drift every tick; observers use the medium profile (a LOS re-check every 12 ticks, 220 m sight)
//! and the 400 m radar; one unit in seven is lit. Line of sight is a height field ray march over a blocky
//! analytic terrain ("terrain"), the same calls on both paths, and again with an always-clear line
//! ("open") to show the scan's own cost. Two densities: "dense" (one unit per 4 000 m²) and
//! "spread" (one per 20 000 m²). Prints p50, p99 and max milliseconds per refresh for 300, 600, 900 and
//! 1500 units, and the candidates per refresh against the scan's reads.

// A host-side tool, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::time::Instant;

use regia_detsim::ai::{
    AiRng, HeightfieldLos, LineOfSight, LosFn, PerceptEnemy, PerceptUnit, PerceptionIndex,
    PerceptionIndexSettings, PerceptionParams, RADAR_RANGE_M, SightingsBoard,
};

const TEAMS: usize = 4;

#[derive(Clone, Copy)]
struct Body {
    id: u32,
    team: u8,
    pos: [f32; 3],
    vel: [f32; 2],
    lit: bool,
}

fn battle(n: usize, side: f32) -> Vec<Body> {
    let mut rng = AiRng::new(0x5EE5 ^ n as u64);
    (0..n)
        .map(|i| Body {
            id: i as u32 * 2 + 1,
            team: (rng.next_u64() % TEAMS as u64) as u8,
            pos: [rng.next_f32() * side, 1.5, rng.next_f32() * side],
            vel: [rng.next_f32() * 4.0 - 2.0, rng.next_f32() * 4.0 - 2.0],
            lit: rng.next_u64().is_multiple_of(7),
        })
        .collect()
}

fn step(bodies: &mut [Body], side: f32) {
    for b in bodies {
        b.pos[0] += b.vel[0];
        b.pos[2] += b.vel[1];
        if b.pos[0] < 0.0 || b.pos[0] > side {
            b.vel[0] = -b.vel[0];
        }
        if b.pos[2] < 0.0 || b.pos[2] > side {
            b.vel[1] = -b.vel[1];
        }
    }
}

fn units(bodies: &[Body]) -> Vec<PerceptUnit> {
    bodies
        .iter()
        .map(|b| PerceptUnit {
            id: b.id,
            team: b.team,
            pos: b.pos,
            sight_range_m: 220.0,
            radar_scale: 1.0,
        })
        .collect()
}

fn enemies(bodies: &[Body]) -> Vec<PerceptEnemy> {
    bodies
        .iter()
        .map(|b| PerceptEnemy {
            id: b.id,
            team: b.team,
            pos: b.pos,
            vel: b.vel,
            dps: 60.0,
            band: [0.0, 150.0],
            lit: b.lit,
        })
        .collect()
}

/// Blocky hills: 0 to 7 m on a 100 m checker of hashed heights.
fn height(x: f32, z: f32) -> f32 {
    let (cx, cz) = ((x * 0.01) as i64, (z * 0.01) as i64);
    ((cx.wrapping_mul(73) ^ cz.wrapping_mul(151)) & 7) as f32
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let at = ((sorted.len() - 1) as f64 * p) as usize;
    sorted[at]
}

fn main() {
    let ticks: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(300);
    let mut settings = PerceptionIndexSettings::default();
    if let Some(cell) = std::env::args().nth(2).and_then(|a| a.parse().ok()) {
        settings.cell_size_m = cell;
    }
    println!(
        "perception_cost: {ticks} ticks after {WARM} warm-up, {TEAMS} teams, medium profile, {} m cells",
        settings.cell_size_m
    );
    println!(
        "los      density  units  path     p50_ms   p99_ms   max_ms   cand/refresh  scan_reads/refresh  los/refresh"
    );
    bench("terrain", HeightfieldLos(height), settings, ticks);
    bench("open", LosFn(|_: [f32; 3], _: [f32; 3]| true), settings, ticks);
}

const WARM: usize = 24;

/// One line of sight over both densities and every size.
fn bench<L: LineOfSight + Copy>(los_label: &str, los: L, settings: PerceptionIndexSettings, ticks: usize) {
    let params = vec![PerceptionParams::default(); TEAMS];
    for (label, area_per_unit) in [("dense", 4000.0f32), ("spread", 20_000.0)] {
        for n in [300usize, 600, 900, 1500] {
            let side = regia_detsim::math::sqrt(n as f32 * area_per_unit);
            let mut bodies = battle(n, side);
            let mut linear = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
            let mut indexed = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
            let mut scratch = PerceptionIndex::new(settings);
            let (mut t_lin, mut t_idx) = (Vec::new(), Vec::new());
            for tick in 0..(WARM + ticks) as u64 {
                let (u, e) = (units(&bodies), enemies(&bodies));
                if tick as usize == WARM {
                    scratch.reset_stats();
                }
                // Alternate which path runs first so cache warmth does not favor one.
                let run_lin = |board: &mut SightingsBoard| {
                    let t0 = Instant::now();
                    board.refresh_linear(los, tick, &u, &e, &params);
                    t0.elapsed().as_secs_f64() * 1e3
                };
                let (a, b) = if tick.is_multiple_of(2) {
                    let a = run_lin(&mut linear);
                    let t0 = Instant::now();
                    indexed.refresh_with(&mut scratch, los, tick, &u, &e, &params);
                    (a, t0.elapsed().as_secs_f64() * 1e3)
                } else {
                    let t0 = Instant::now();
                    indexed.refresh_with(&mut scratch, los, tick, &u, &e, &params);
                    let b = t0.elapsed().as_secs_f64() * 1e3;
                    (run_lin(&mut linear), b)
                };
                assert!(linear == indexed, "boards differ at tick {tick} ({label}, {n})");
                if tick as usize >= WARM {
                    t_lin.push(a);
                    t_idx.push(b);
                }
                step(&mut bodies, side);
            }
            let stats = scratch.stats();
            let per = |v: u64| v as f64 / ticks as f64;
            for (path, mut times) in [("linear", t_lin), ("indexed", t_idx)] {
                times.sort_by(f64::total_cmp);
                println!(
                    "{los_label:<8} {label:<8} {n:>5}  {path:<7} {:>8.3} {:>8.3} {:>8.3}  {:>12.0}  {:>18.0}  {:>11.0}",
                    percentile(&times, 0.5),
                    percentile(&times, 0.99),
                    times[times.len() - 1],
                    if path == "indexed" {
                        per(stats.candidates)
                    } else {
                        per(stats.scan_pairs)
                    },
                    per(stats.scan_pairs),
                    per(stats.los_calls),
                );
            }
        }
    }
}
