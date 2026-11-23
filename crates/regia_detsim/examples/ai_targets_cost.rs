// SPDX-License-Identifier: MPL-2.0
//! Time the target pick under `Exhaustive` (rules version 1) and `Ringed` (version 2) on a moving two-team
//! crowd (prints; asserts nothing about time):
//! `cargo run --release -p regia_detsim --example ai_targets_cost -- [ticks]`.
//!
//! The crowd is spread so a unit has about 11 enemies (22 units) within 80 m (the field grows with the count). Every unit
//! knows every live enemy, the shape of the gap this closes (900 units on two teams: 405,000 scores per tick).
//! Two score closures: `cheap` (nearest) and `do27` (slot rows: health, a kind weight table, threat, focus on
//! the current target and a distance falloff; no strings, no allocation). Columns per count and closure:
//! - `exh`: the `Exhaustive` pick over prebuilt known lists (the lists come from `TeamRosters`, built once per
//!   tick and not timed, as a game already has them);
//! - `ring`: the `Ringed {80/120/250 m, 8, 4}` pick alone over a spatial index built that tick;
//! - `ring_build`: the `DetSpatial` build those picks need (a game that already builds one for other queries
//!   pays it once).
//!
//! p50, p99 and max milliseconds per tick after 20 warm-up ticks, and scores per tick from the selector's
//! counters.

// A host-side tool, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::time::Instant;

use regia_detsim::ai::{
    AiRng, AiRules, RosterSettings, TargetHost, TargetRule, TargetSelector, TargetUnit, TeamRosters,
    team_group_mask,
};
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

const WARMUP: usize = 20;
const KIND_WEIGHT: [f32; 4] = [1.0, 1.5, 0.75, 2.0];

#[derive(Clone, Copy)]
struct Row {
    team: u8,
    pos: [f32; 2],
    vel: [f32; 2],
    hp: f32,
    threat: f32,
    kind: u8,
}

fn crowd(n: usize) -> (Vec<Row>, f32) {
    let mut rng = AiRng::new(0x7A26 ^ n as u64);
    // About one unit per 900 m²: 900 units on a 900 m square.
    let side = regia_detsim::math::sqrt(n as f32 * 900.0);
    let rows = (0..n)
        .map(|i| Row {
            team: (i % 2) as u8,
            pos: [rng.next_f32() * side, rng.next_f32() * side],
            vel: [rng.next_f32() * 2.0 - 1.0, rng.next_f32() * 2.0 - 1.0],
            hp: 0.25 + rng.next_f32() * 0.75,
            threat: rng.next_f32(),
            kind: (rng.next_u64() % 4) as u8,
        })
        .collect();
    (rows, side)
}

fn step(rows: &mut [Row], side: f32) {
    for r in rows {
        for axis in 0..2 {
            let next = r.pos[axis] + r.vel[axis];
            if !(0.0..=side).contains(&next) {
                r.vel[axis] = -r.vel[axis];
            } else {
                r.pos[axis] = next;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Cheap,
    Do27,
}

struct Host<'a> {
    rows: &'a [Row],
    targets: &'a [Option<usize>],
    shape: Shape,
}

impl TargetHost for Host<'_> {
    fn score(&mut self, unit: usize, candidate: usize) -> f32 {
        let (u, c) = (&self.rows[unit], &self.rows[candidate]);
        let dx = c.pos[0] - u.pos[0];
        let dz = c.pos[1] - u.pos[1];
        let d2 = dx * dx + dz * dz;
        match self.shape {
            Shape::Cheap => -d2,
            Shape::Do27 => {
                let focus = if self.targets[unit] == Some(candidate) {
                    1.25
                } else {
                    1.0
                };
                let weak = 1.0 - c.hp;
                (0.5 + weak) * KIND_WEIGHT[usize::from(c.kind)] * (1.0 + c.threat) * focus
                    / (1.0 + d2 * 0.0005)
            }
        }
    }
}

fn pct(mut ms: Vec<f64>) -> (f64, f64, f64) {
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.99), ms[ms.len() - 1])
}

struct Run {
    pick_ms: Vec<f64>,
    build_ms: Vec<f64>,
    scores: u64,
    ticks: u64,
}

fn run(n: usize, ticks: usize, rules: AiRules, shape: Shape) -> Run {
    let (mut rows, side) = crowd(n);
    let ringed = matches!(rules.target, TargetRule::Ringed { .. });
    let mut selector = TargetSelector::new(rules).unwrap_or_else(|e| panic!("{e}"));
    let mut rosters = TeamRosters::new(RosterSettings::default());
    let mut enemies: [Vec<u32>; 2] = Default::default();
    let mut known: [Vec<usize>; 2] = Default::default();
    let mut spatial = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..BroadphaseConfig::default()
    });
    let mut srows: Vec<SpatialRow> = Vec::with_capacity(n);
    let units: Vec<TargetUnit> = rows
        .iter()
        .enumerate()
        .map(|(slot, r)| TargetUnit {
            slot,
            entity_id: 1000 + slot as u32,
            enemy_mask: team_group_mask(1 - r.team),
        })
        .collect();
    let mut targets = vec![None; n];
    let mut prev = vec![None; n];
    let mut out = Run {
        pick_ms: Vec::new(),
        build_ms: Vec::new(),
        scores: 0,
        ticks: 0,
    };
    for tick in 0..(WARMUP + ticks) as u64 {
        let t0 = Instant::now();
        srows.clear();
        srows.extend(rows.iter().enumerate().map(|(slot, r)| SpatialRow {
            slot,
            body: BroadBody::new([r.pos[0], 0.0, r.pos[1]], 0.5, [0.0, 2.0]),
            group_mask: team_group_mask(r.team),
            flags: 0,
        }));
        spatial.build(&srows).unwrap_or_else(|e| panic!("{e}"));
        let build = t0.elapsed().as_secs_f64() * 1e3;
        if !ringed {
            rosters
                .build(&rows, |r| Some(r.team))
                .unwrap_or_else(|e| panic!("{e}"));
            for team in 0..2u8 {
                rosters.others_into(team, &mut enemies[usize::from(team)]);
                known[usize::from(team)].clear();
                known[usize::from(team)].extend(enemies[usize::from(team)].iter().map(|&s| s as usize));
            }
        }
        prev.copy_from_slice(&targets);
        let mut host = Host {
            rows: &rows,
            targets: &prev,
            shape,
        };
        let t1 = Instant::now();
        let report = selector.pick_tick(
            tick,
            &units,
            &mut targets,
            |slot| known[usize::from(rows[slot].team)].as_slice(),
            &mut spatial,
            &mut host,
        );
        let pick = t1.elapsed().as_secs_f64() * 1e3;
        if tick as usize >= WARMUP {
            out.pick_ms.push(pick);
            out.build_ms.push(build);
            out.scores += report.scores_evaluated;
            out.ticks += 1;
        }
        step(&mut rows, side);
    }
    out
}

fn main() {
    let ticks: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_else(|_| "n/a".into());
    println!(
        "ai_targets_cost ticks={ticks} warmup={WARMUP} arch={} os={} loadavg={}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        load.split_whitespace().take(3).collect::<Vec<_>>().join(",")
    );
    for n in [300usize, 600, 900, 1500] {
        for (shape, name) in [(Shape::Cheap, "cheap"), (Shape::Do27, "do27")] {
            let exh = run(n, ticks, AiRules::default(), shape);
            let ring = run(
                n,
                ticks,
                AiRules::for_version(2).unwrap_or_else(|e| panic!("{e}")),
                shape,
            );
            let (e50, e99, emax) = pct(exh.pick_ms);
            let (r50, r99, rmax) = pct(ring.pick_ms);
            let (b50, b99, bmax) = pct(ring.build_ms);
            println!(
                "n={n} closure={name} exh_p50_ms={e50:.3} exh_p99_ms={e99:.3} exh_max_ms={emax:.3} \
                 ring_p50_ms={r50:.3} ring_p99_ms={r99:.3} ring_max_ms={rmax:.3} \
                 ring_build_p50_ms={b50:.3} ring_build_p99_ms={b99:.3} ring_build_max_ms={bmax:.3} \
                 exh_scores_per_tick={} ring_scores_per_tick={}",
                exh.scores / exh.ticks.max(1),
                ring.scores / ring.ticks.max(1)
            );
        }
    }
}
