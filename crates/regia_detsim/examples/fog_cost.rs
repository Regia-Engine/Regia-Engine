// SPDX-License-Identifier: MPL-2.0
//! Time one frame of fog for every team: gather the vision, answer every entity, rebuild the visible
//! cells (prints numbers, asserts only that the indexed and linear answers agree):
//! `cargo run --release -p regia_detsim --example fog_cost -- [entities=900] [teams=4] [frames=200]
//! [half_extent_m=2000]`.
//!
//! "linear" is the gather-once scan (every query tests every watcher, as the first fog release did);
//! "indexed" files the watchers in the spatial index (the default from 16 watchers on). Each mode
//! runs every team's view `frames` times on a world whose units move a little each frame.

use std::time::Instant;

use regia_detsim::fog::{
    FogCounters, FogEntity, FogGrid, FogRules, FogWorld, SightClass, TeamId, TeamVision,
};
use regia_detsim::los::OpenGround;

struct Row {
    id: u64,
    kind: &'static str,
    class: SightClass,
    team: TeamId,
    position: [f32; 3],
}

struct World(Vec<Row>);

impl FogWorld for World {
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
        self.0
            .iter()
            .map(|r| FogEntity::new(r.id, r.kind, r.class, r.team, r.position))
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "bench example: command-line arguments, read before the run (ADR-8367)"
)]
fn arg(i: usize, default: f32) -> f32 {
    std::env::args()
        .nth(i)
        .and_then(|a| a.parse::<f32>().ok())
        .unwrap_or(default)
}

/// RTS-shaped: mostly units, 15 % buildings, 15 % crew, 1 % radar masts, 3 % neutral features.
fn world(n: usize, teams: TeamId, half: f32) -> World {
    let mut s: u64 = 0xF0C5;
    let mut next = move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (s >> 40) as f32 * (1.0 / 16_777_216.0)
    };
    let rows = (0..n)
        .map(|i| {
            let roll = (next() * 100.0) as u32;
            let (class, kind) = match roll {
                0 => (SightClass::Building, "radar_mast"),
                1..=14 => (SightClass::Building, "depot"),
                15..=29 => (SightClass::OnFoot, "crew"),
                30..=32 => (SightClass::Feature, "vent"),
                _ => (SightClass::Unit, "scout"),
            };
            let team = if class == SightClass::Feature {
                0
            } else {
                1 + ((next() * teams as f32) as TeamId).min(teams - 1)
            };
            let position = [(next() * 2.0 - 1.0) * half, 0.0, (next() * 2.0 - 1.0) * half];
            Row {
                id: i as u64 + 1,
                kind,
                class,
                team,
                position,
            }
        })
        .collect();
    World(rows)
}

fn step(world: &mut World, frame: usize) {
    for (i, row) in world.0.iter_mut().enumerate() {
        if row.class == SightClass::Unit {
            let dir = if (i + frame / 50).is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            row.position[0] += dir * 0.5;
        }
    }
}

struct Run {
    ms_per_frame: f64,
    counters: FogCounters,
    visible: u64,
}

#[expect(
    clippy::disallowed_methods,
    reason = "bench example: wall-clock timing of the fog passes, outside the sim state (ADR-8367)"
)]
fn run(
    world: &mut World,
    rules: &FogRules,
    teams: TeamId,
    frames: usize,
    min_watchers: usize,
    grid: bool,
) -> Run {
    let mut visions: Vec<TeamVision> = (0..teams)
        .map(|_| TeamVision::default().with_index_min_watchers(min_watchers))
        .collect();
    let mut grids: Vec<FogGrid> = (0..teams).map(|_| FogGrid::covering(2048.0, 2048.0)).collect();
    let mut counters = FogCounters::default();
    let mut visible = 0u64;
    let start = Instant::now();
    for frame in 0..frames {
        step(world, frame);
        for (t, vision) in visions.iter_mut().enumerate() {
            vision.rebuild(&*world, rules, t as TeamId + 1);
            for entity in world.fog_entities() {
                if vision.entity_visible_counted(&*world, &entity, &OpenGround, &mut counters) {
                    visible += 1;
                }
            }
            if grid {
                grids[t].update(vision, &OpenGround);
            }
        }
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    Run {
        ms_per_frame: ms,
        counters,
        visible,
    }
}

fn main() {
    let n = arg(1, 900.0) as usize;
    let teams = (arg(2, 4.0) as TeamId).max(1);
    let frames = (arg(3, 200.0) as usize).max(1);
    let half = arg(4, 2000.0);
    let rules = FogRules {
        team_count: teams,
        sight: regia_detsim::fog::SightTable::default().with_kind("radar_mast", 400.0),
        ..FogRules::default()
    };
    println!(
        "fog_cost: {n} entities, {teams} teams, {frames} frames, +-{half} m, release={}",
        !cfg!(debug_assertions)
    );
    for (label, grid) in [("entities", false), ("entities+grid", true)] {
        let mut a = world(n, teams, half);
        let mut b = world(n, teams, half);
        let linear = run(&mut a, &rules, teams, frames, usize::MAX, grid);
        let indexed = run(&mut b, &rules, teams, frames, 0, grid);
        assert_eq!(
            linear.visible, indexed.visible,
            "indexed and linear answers differ"
        );
        for (mode, r) in [("linear", &linear), ("indexed", &indexed)] {
            let per = |v: u64| v as f64 / frames as f64;
            println!(
                "{label:14} {mode:8} {:9.4} ms/frame  queries/frame {:8.0}  candidates/frame {:10.0}  los/frame {:8.0}  visible/frame {:8.0}",
                r.ms_per_frame,
                per(r.counters.queries),
                per(r.counters.index_candidates),
                per(r.counters.los_calls),
                per(r.visible),
            );
        }
        println!(
            "{label:14} speedup  {:.2}x",
            linear.ms_per_frame / regia_detsim::float::det_max_f64(indexed.ms_per_frame, 1e-9)
        );
    }
}
