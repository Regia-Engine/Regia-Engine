// SPDX-License-Identifier: MPL-2.0
//! Long-run soak of world streaming (`docs/specs/78-world-streaming.md` §§ 8, 12, 15, 18): a seeded
//! random walk of several sources over a generated 24 by 24 cell world (6 km a side), headless, in
//! lockstep. Teleports, fast flyovers (past the floating origin's 5 km threshold, so the origin
//! rebases), and a level switch midway; at least 5,000 fixed steps.
//!
//! Counters and fixed steps only. After each phase the sources move to fresh ground (a teleport
//! beyond the unload radius of everything loaded), the run settles, and the loaded cell set must
//! equal what a fresh planner orders for those sources. When the sources leave, entities, load-set
//! entries, pending loads and cell bytes return to their baseline. Every step honors its budgets.
//! The same seed gives the same trace hashes on two runs.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::level::LevelId;
use regia_scene::stream_grid::CellCoord;
use regia_scene::stream_plan::{CellInfo, SourceId, SourceView, StreamPlanner};
use regia_scene::streaming::{
    CellMember, LevelStreaming, OriginOffset, SourceKind, StreamBudgets, StreamingSource,
};
use regia_world::{CellState, WorldStreaming};

/// Cells per side of the generated world.
const GRID: i64 = 24;
/// Cell side, meters: 24 cells make 6 km, past the origin's 5 km threshold.
const CELL_M: f64 = 250.0;
const LOAD_M: f64 = 300.0;
const UNLOAD_M: f64 = 400.0;
/// World extent, meters.
const WORLD_M: f64 = GRID as f64 * CELL_M;
/// Entities of one cell of the main level: two hierarchies of three.
const PER_CELL_ENTITIES: usize = 6;

const MAX_LOADS: u32 = 4;
const MAX_INFLIGHT: u32 = 4;
const APPLY_ENTITIES: u32 = 12;

/// splitmix64: a small seeded generator, so the walk is the same on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `[-range, range]` in quarter meters, so positions stay exact in `f32`.
    fn quarter(&mut self, range: f64) -> f64 {
        let quarters = (range * 4.0) as u64;
        let pick = self.next() % (2 * quarters + 1);
        (pick as f64 - quarters as f64) / 4.0
    }

    /// A position in `[100, WORLD_M - 100]`, quarter meters.
    fn site(&mut self) -> f64 {
        let quarters = ((WORLD_M - 200.0) * 4.0) as u64;
        100.0 + (self.next() % quarters) as f64 / 4.0
    }
}

fn level_b() -> LevelId {
    LevelId(Guid::from_u128(0xfeee))
}

fn soak_settings() -> LevelStreaming {
    let mut s = settings(LOAD_M, UNLOAD_M);
    s.cell_size_m = CELL_M;
    s.budgets = Some(StreamBudgets {
        max_loads_per_step: MAX_LOADS,
        max_inflight: MAX_INFLIGHT,
        max_pending_bytes: 1 << 30,
        memory_budget_bytes: 1 << 30,
        apply_budget_entities: APPLY_ENTITIES,
        ..StreamBudgets::default()
    });
    s
}

struct Source {
    entity: Entity,
    /// World position, meters (`x`, `z`), exact in quarter meters.
    world: [f64; 2],
    scale: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Motion {
    Walk,
    /// Walk, and now and then jump somewhere else.
    Teleports,
    /// Source 0 flies along x at a quarter of a cell per step; the others walk.
    Flyover,
}

struct Soak {
    app: App,
    rng: Rng,
    sources: Vec<Source>,
    /// The level now open and its world (index cells with their sizes).
    level: LevelId,
    world_bytes: BTreeMap<CellCoord, u64>,
    per_cell_entities: usize,
    steps: u64,
    flight_dir: f64,
    /// Entities of the app before any level opened.
    baseline: usize,
    baseline_set: BTreeSet<Entity>,
    /// Entities with the level open and no cell loaded: the baseline plus the level's own.
    open_baseline: usize,
    /// `(label, trace_hash, world_hash)` at every phase end.
    marks: Vec<(String, u64, u64)>,
    prev_loads: u64,
    prev_plans: u64,
    prev_evaluated: u64,
}

fn entity_count(app: &mut App) -> usize {
    app.world_mut()
        .query_filtered::<Entity, Without<regia_ecs::ecs::resource::IsResource>>()
        .iter(app.world())
        .count()
}

impl Soak {
    fn new(seed: u64) -> Self {
        let mut app = app();
        let mut rng = Rng(seed);
        let mut sources = Vec::new();
        for (i, scale) in [1.0f32, 1.0, 1.5].into_iter().enumerate() {
            let world = [
                1500.0 + 800.0 * i as f64 + rng.quarter(100.0),
                1500.0 + rng.quarter(300.0),
            ];
            let entity = app
                .world_mut()
                .spawn((
                    Transform::from_translation(Vec3::new(world[0] as f32, 0.0, world[1] as f32)),
                    StreamingSource {
                        kind: SourceKind::Player,
                        view_bias: 0.0,
                        radius_scale: scale,
                        drives_origin: i == 0,
                        ..StreamingSource::default()
                    },
                ))
                .id();
            sources.push(Source { entity, world, scale });
        }
        let baseline = entity_count(&mut app);
        let baseline_set = app
            .world_mut()
            .query_filtered::<Entity, Without<regia_ecs::ecs::resource::IsResource>>()
            .iter(app.world())
            .collect();
        Self {
            app,
            rng,
            sources,
            level: level(),
            world_bytes: BTreeMap::new(),
            per_cell_entities: 0,
            steps: 0,
            flight_dir: 1.0,
            baseline,
            baseline_set,
            open_baseline: baseline,
            marks: Vec::new(),
            prev_loads: 0,
            prev_plans: 0,
            prev_evaluated: 0,
        }
    }

    fn open_main(&mut self) {
        let world = build(&self.app, GRID, GRID, 2, 3, soak_settings());
        self.adopt(&world, level(), PER_CELL_ENTITIES);
        open(&mut self.app, &world, 2);
        self.open_baseline = entity_count(&mut self.app);
    }

    fn open_empty(&mut self) {
        let world = build_empty_level(level_b(), GRID, GRID, soak_settings());
        self.adopt(&world, level_b(), 0);
        open_additive(&mut self.app, level_b(), &world);
        self.app
            .world_mut()
            .resource_mut::<WorldStreaming>()
            .set_timing(regia_world::StreamTiming::Lockstep { latency_steps: 2 });
        self.open_baseline = entity_count(&mut self.app);
    }

    fn adopt(&mut self, world: &common::World, level: LevelId, per_cell: usize) {
        self.level = level;
        self.per_cell_entities = per_cell;
        self.world_bytes = world.index.cells.iter().map(|r| (r.coord, r.own_bytes)).collect();
        // The counters run on across levels; the per-step deltas start from where they are.
        let now = stats(&self.app);
        self.prev_loads = now.loads_issued;
        self.prev_plans = now.plans;
        self.prev_evaluated = now.plan_cells_evaluated;
    }

    fn close(&mut self) {
        regia_scene::level::unload_level(self.app.world_mut(), self.level).expect("unload");
        self.app.update();
    }

    /// The entities that were not there before any level opened, with their component names.
    fn extras(&mut self) -> Vec<String> {
        let now: Vec<Entity> = self
            .app
            .world_mut()
            .query_filtered::<Entity, Without<regia_ecs::ecs::resource::IsResource>>()
            .iter(self.app.world())
            .collect();
        now.into_iter()
            .filter(|e| !self.baseline_set.contains(e))
            .map(|e| {
                let names: Vec<String> = self
                    .app
                    .world()
                    .inspect_entity(e)
                    .map(|c| c.map(|i| i.1.name().to_string()).collect())
                    .unwrap_or_default();
                format!("{e:?}: {names:?}")
            })
            .collect()
    }

    /// Every entity that was not there before the first level opened is gone.
    fn assert_home(&mut self, what: &str) {
        let count = entity_count(&mut self.app);
        if count != self.baseline {
            let extras = self.extras();
            panic!(
                "{what}: {count} entities against a baseline of {}: {extras:?}",
                self.baseline
            );
        }
    }

    fn origin_offset(&self) -> [f64; 3] {
        self.app.world().resource::<OriginOffset>().0
    }

    /// Puts every source's entity where its world position says, in the current local frame.
    fn place(&mut self) {
        let offset = self.origin_offset();
        for s in &self.sources {
            if let Some(mut t) = self.app.world_mut().get_mut::<Transform>(s.entity) {
                t.translation = Vec3::new(
                    (s.world[0] - offset[0]) as f32,
                    0.0,
                    (s.world[1] - offset[2]) as f32,
                );
            }
        }
    }

    /// One fixed step, and the per-step budget checks.
    fn step(&mut self, context: &str) {
        self.place();
        self.app.update();
        self.steps += 1;
        self.check_budgets(context);
    }

    fn check_budgets(&mut self, context: &str) {
        let now = stats(&self.app);
        let cells = self.world_bytes.len() as u64;
        let at = format!("{context} step {}: {now:?}", self.steps);
        assert!(now.last_apply_entities <= APPLY_ENTITIES, "apply budget, {at}");
        assert_eq!(now.apply_over_budget, 0, "{at}");
        assert!(now.inflight <= MAX_INFLIGHT, "in flight, {at}");
        assert!(now.pending_bytes <= 1 << 30, "pending bytes, {at}");
        // IO requests of one step: the planner orders at most `MAX_LOADS` per plan and a read needs
        // a free slot, so a step issues no more than the slots it has.
        let reads = now.loads_issued - self.prev_loads;
        assert!(reads <= u64::from(MAX_INFLIGHT), "reads this step {reads}, {at}");
        // Plans and the cells they look at: one plan per step walks the registered cells once.
        let plans = now.plans - self.prev_plans;
        let looked = now.plan_cells_evaluated - self.prev_evaluated;
        assert!(plans <= 1, "plans this step {plans}, {at}");
        assert!(
            looked <= cells,
            "cells evaluated this step {looked} of {cells}, {at}"
        );
        assert_eq!(now.failures, 0, "{at}");
        assert_eq!(now.lockstep_stalls, 0, "{at}");
        assert_eq!(now.sources_dropped, 0, "{at}");
        self.prev_loads = now.loads_issued;
        self.prev_plans = now.plans;
        self.prev_evaluated = now.plan_cells_evaluated;
        // The high-water marks agree with the budgets too.
        assert!(now.peak_apply_entities <= APPLY_ENTITIES, "{at}");
        assert!(now.peak_inflight <= MAX_INFLIGHT, "{at}");
        assert!(now.peak_loads_per_step <= MAX_INFLIGHT, "{at}");
        assert!(u64::from(now.peak_plan_cells) <= cells, "{at}");
    }

    /// Moves the sources by one step of `motion`.
    fn advance(&mut self, motion: Motion) {
        for i in 0..self.sources.len() {
            let fly = motion == Motion::Flyover && i == 0;
            if fly {
                let s = &mut self.sources[0];
                s.world[0] += self.flight_dir * CELL_M / 4.0;
                if s.world[0] >= WORLD_M - 100.0 {
                    s.world[0] = WORLD_M - 100.0;
                    self.flight_dir = -1.0;
                } else if s.world[0] <= 100.0 {
                    s.world[0] = 100.0;
                    self.flight_dir = 1.0;
                }
                continue;
            }
            if motion == Motion::Teleports && self.rng.next().is_multiple_of(90) {
                let (x, z) = (self.rng.site(), self.rng.site());
                self.sources[i].world = [x, z];
                continue;
            }
            let (dx, dz) = (self.rng.quarter(1.5), self.rng.quarter(1.5));
            let s = &mut self.sources[i];
            s.world[0] = (s.world[0] + dx).clamp(100.0, WORLD_M - 100.0);
            s.world[1] = (s.world[1] + dz).clamp(100.0, WORLD_M - 100.0);
        }
    }

    fn run_phase(&mut self, name: &str, motion: Motion, steps: u64) {
        if motion == Motion::Flyover {
            // From the origin's home cell the flight crosses the 5 km threshold out and back.
            self.app
                .world_mut()
                .resource_mut::<WorldStreaming>()
                .request_rebase([0; 3]);
            // The step that takes the request skips the threshold check; let the next one run it.
            self.step(name);
            self.step(name);
        }
        for _ in 0..steps {
            self.advance(motion);
            self.step(name);
            // The floating origin keeps the focus (source 0) near the local origin: it rebases
            // once the focus is more than 5 km out.
            let local = self
                .app
                .world()
                .get::<Transform>(self.sources[0].entity)
                .map_or(0.0, |t| t.translation.x.abs().max(t.translation.z.abs()));
            assert!(local < 5200.0, "{name} step {}: focus local {local}", self.steps);
        }
    }

    /// The cells a fresh planner orders for the sources' world positions: what the runtime must
    /// hold once it settles on ground its old cells left behind.
    fn expected(&self) -> BTreeSet<CellCoord> {
        let streaming = self.app.world().resource::<WorldStreaming>();
        let settings = streaming.settings(self.level).expect("settings").clone();
        let budgets = StreamBudgets {
            max_loads_per_step: 0,
            ..settings.effective_budgets()
        };
        let grid = settings.grid().expect("grid");
        let mut planner = StreamPlanner::new(grid, settings.planner_config(&budgets)).expect("planner");
        for (cell, bytes) in &self.world_bytes {
            planner
                .register_cell(
                    *cell,
                    CellInfo {
                        bytes: *bytes,
                        priority: 0,
                    },
                )
                .expect("register");
        }
        let views: Vec<SourceView> = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| SourceView {
                radius_scale: s.scale,
                view_bias: 0.0,
                ..SourceView::at(SourceId::Pin(i as u64), [s.world[0], 0.0, s.world[1]])
            })
            .collect();
        let plan = planner.plan_views(&views).expect("plan");
        assert!(
            plan.deferred.is_empty(),
            "the oracle's budgets bound nothing: {plan:?}"
        );
        plan.loads.iter().map(|l| l.cell).collect()
    }

    fn spawned(&self) -> BTreeSet<CellCoord> {
        let streaming = self.app.world().resource::<WorldStreaming>();
        streaming
            .cells(self.level)
            .filter(|(_, s)| s.is_spawned())
            .map(|(c, _)| c)
            .collect()
    }

    /// Teleports every source to ground at least 1,200 m from everywhere it was and from the other
    /// sources' new sites, so no old cell stays inside the unload radius.
    fn relocate(&mut self) {
        let old: Vec<[f64; 2]> = self.sources.iter().map(|s| s.world).collect();
        let mut fresh: Vec<[f64; 2]> = Vec::new();
        for _ in 0..self.sources.len() {
            let mut tries = 0;
            loop {
                let site = [self.rng.site(), self.rng.site()];
                let far = |p: &[f64; 2]| (p[0] - site[0]).hypot(p[1] - site[1]) >= 1200.0;
                if old.iter().all(far) && fresh.iter().all(far) {
                    fresh.push(site);
                    break;
                }
                tries += 1;
                assert!(tries < 10_000, "no free ground");
            }
        }
        for (s, site) in self.sources.iter_mut().zip(fresh) {
            s.world = site;
        }
    }

    /// Steps until nothing is in flight and the spawned set stopped changing, up to `limit`.
    fn settle(&mut self, name: &str, limit: u64) {
        let mut quiet = 0;
        let mut last = self.spawned();
        for _ in 0..limit {
            self.step(name);
            let now = self.spawned();
            let s = stats(&self.app);
            if now == last && s.inflight == 0 && s.pending_bytes == 0 && s.last_apply_entities == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
            last = now;
            if quiet >= 6 {
                return;
            }
        }
        panic!("{name}: did not settle in {limit} steps: {:?}", stats(&self.app));
    }

    /// The end-of-phase checks: the loaded set is the planner's set, nothing leaks, nothing is
    /// left over.
    fn check_settled(&mut self, name: &str) {
        let expected = self.expected();
        let spawned = self.spawned();
        assert_eq!(
            spawned, expected,
            "{name}: loaded cells differ from the planner's"
        );
        let streaming = self.app.world().resource::<WorldStreaming>();
        for (cell, state) in streaming.cells(self.level) {
            if !expected.contains(&cell) {
                assert_eq!(
                    state,
                    CellState::Unloaded,
                    "{name}: {cell:?} was left in {state:?}"
                );
            } else {
                assert_eq!(state, CellState::Visible, "{name}: {cell:?}");
            }
        }
        let s = stats(&self.app);
        assert_eq!(s.pending_bytes, 0, "{name}");
        assert_eq!(s.inflight, 0, "{name}");
        assert_eq!(s.pending_waits, 0, "{name}");
        assert_eq!(s.resident_cells as usize, expected.len(), "{name}");
        let bytes: u64 = expected.iter().map(|c| self.world_bytes[c]).sum();
        assert_eq!(s.resident_bytes_estimate, bytes, "{name}");

        // No streamed entity outlives its cell, and each cell holds what the cook put in it.
        let mut members: BTreeMap<CellCoord, usize> = BTreeMap::new();
        for member in self.app.world_mut().query::<&CellMember>().iter(self.app.world()) {
            *members.entry(member.0).or_default() += 1;
        }
        let members_total: usize = members.values().sum();
        for (cell, count) in &members {
            assert!(
                expected.contains(cell),
                "{name}: {cell:?} has {count} members but is not loaded"
            );
            assert_eq!(*count, self.per_cell_entities, "{name}: {cell:?}");
        }
        assert_eq!(members_total, expected.len() * self.per_cell_entities, "{name}");
        let streaming = self.app.world().resource::<WorldStreaming>();
        for cell in &expected {
            assert_eq!(
                streaming.entities(self.level, *cell).len(),
                self.per_cell_entities,
                "{name}"
            );
        }
        // Entities: the baseline plus the members, nothing else.
        let total = entity_count(&mut self.app);
        assert_eq!(
            total,
            self.open_baseline + members_total,
            "{name}: entity count drifted"
        );
    }

    /// A phase end: relocate, settle, compare, and record the hashes.
    fn end_phase(&mut self, name: &str) {
        self.relocate();
        self.settle(name, 900);
        self.check_settled(name);
        let s = stats(&self.app);
        self.marks.push((name.to_string(), s.trace_hash, s.world_hash));
    }

    /// The sources leave: far outside the world, everything goes, and the counters are at baseline.
    fn leave(&mut self, name: &str) {
        for (i, s) in self.sources.iter_mut().enumerate() {
            s.world = [-3000.0 - 600.0 * i as f64, -3000.0];
        }
        self.settle(name, 900);
        let s = stats(&self.app);
        assert!(self.spawned().is_empty(), "{name}");
        assert_eq!(s.resident_cells, 0, "{name}: {s:?}");
        assert_eq!(s.resident_bytes_estimate, 0, "{name}");
        assert_eq!(s.resident_bytes_measured, 0, "{name}");
        assert_eq!(s.pending_bytes, 0, "{name}");
        assert_eq!(s.inflight, 0, "{name}");
        assert_eq!(s.pending_waits, 0, "{name}");
        assert_eq!(
            entity_count(&mut self.app),
            self.open_baseline,
            "{name}: entities did not return to baseline"
        );
        assert_eq!(count_members(&mut self.app), 0, "{name}");
        let streaming = self.app.world().resource::<WorldStreaming>();
        assert!(
            streaming
                .cells(self.level)
                .all(|(_, st)| st == CellState::Unloaded),
            "{name}"
        );
        self.marks.push((name.to_string(), s.trace_hash, s.world_hash));
    }
}

/// What a run returns: the hashes at every phase end and the totals.
#[derive(Debug, PartialEq)]
struct Outcome {
    marks: Vec<(String, u64, u64)>,
    steps: u64,
}

fn run_soak(seed: u64) -> Outcome {
    let mut soak = Soak::new(seed);
    let sets = regia_asset::LoadSets::default();
    soak.app.insert_resource(sets.clone());

    // Level A: walk, teleports, flyovers.
    soak.open_main();
    soak.run_phase("a_walk", Motion::Walk, 1000);
    soak.end_phase("a_walk_end");
    soak.run_phase("a_teleports", Motion::Teleports, 1000);
    soak.end_phase("a_teleports_end");
    let rebases_before = stats(&soak.app).rebases;
    soak.run_phase("a_flyover", Motion::Flyover, 1000);
    soak.end_phase("a_flyover_end");
    let flown = stats(&soak.app);
    assert!(
        flown.rebases >= rebases_before + 2,
        "the flyover crosses 5 km both ways: {flown:?}"
    );
    assert!(flown.teleports > 0, "{flown:?}");
    soak.leave("a_leave");
    // The stream-cell load set lists nothing resident.
    let set_name = regia_world::stream_cells_set(level());
    let set = sets.get(&set_name).expect("the load set");
    assert!(
        set.items.iter().all(|i| !sets.item_resident(i.kind, &i.key)),
        "load-set entries left resident"
    );

    // The level switch: close A (the origin comes home), open B, which holds no entities.
    soak.close();
    soak.assert_home("closing A");
    assert_eq!(
        soak.origin_offset(),
        [0.0; 3],
        "the origin came home with the level"
    );
    for s in &mut soak.sources {
        s.world = [1500.0, 1500.0];
    }
    soak.open_empty();
    soak.run_phase("b_walk", Motion::Walk, 600);
    soak.run_phase("b_flyover", Motion::Flyover, 600);
    soak.end_phase("b_end");
    soak.leave("b_leave");
    soak.close();
    soak.assert_home("closing B");

    // Back to A after the switch: nothing of the first visit stuck.
    soak.open_main();
    soak.run_phase("a2_teleports", Motion::Teleports, 600);
    soak.run_phase("a2_flyover", Motion::Flyover, 400);
    soak.end_phase("a2_end");
    soak.leave("a2_leave");
    let end = stats(&soak.app);
    assert!(
        end.peak_apply_entities > 0 && end.peak_inflight > 0 && end.peak_loads_per_step > 0,
        "{end:?}"
    );
    soak.close();
    soak.assert_home("the end");
    Outcome {
        marks: soak.marks,
        steps: soak.steps,
    }
}

#[test]
fn a_long_seeded_soak_holds_every_budget_and_leaks_nothing() {
    // `cargo xtask stream-bench --soak --seed N` names another seed.
    let seed = std::env::var("STREAM_SOAK_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0001);
    let outcome = run_soak(seed);
    assert!(outcome.steps >= 5000, "{}", outcome.steps);
    assert!(outcome.marks.len() >= 8, "{:?}", outcome.marks);
    let line = format!(
        "{{\"soak\":{{\"seed\":{seed},\"steps\":{},\"phases\":{},\"final_trace_hash\":\"{}\"}}}}",
        outcome.steps,
        outcome.marks.len(),
        outcome.marks.last().map_or(0, |m| m.1)
    );
    println!("{line}");
    if let Ok(path) = std::env::var("STREAM_SOAK_JSON") {
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, &line).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[test]
fn the_same_soak_seed_gives_the_same_trace_hashes_twice() {
    let a = run_soak(0x5eed_0002);
    let b = run_soak(0x5eed_0002);
    assert_eq!(a, b);
    let c = run_soak(0x5eed_0003);
    assert_ne!(a.marks, c.marks, "another seed walks another way");
}
