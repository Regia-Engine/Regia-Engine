// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the versioned target selector (`regia_detsim::ai::targets`, audit gap AI-2).
//!
//! A seeded 900-unit world (2 and 4 teams) moves, forgets and dies over 600 fixed ticks. `Exhaustive` must
//! give a plain loop's picks bit for bit; `Ringed` is checked by counters (scores per tick), by a brute-force
//! ring oracle, by hand-built invalidation and tie scenes, and by a pinned hash of every pick. No timing.

#![allow(unsafe_code)]
// clippy 1.99 reports `missing_const_for_thread_local` on aarch64-linux-android only, at the `thread_local!`
// below whose initializers are already `const { }` (the same false positive spatial_alloc.rs documents).
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ops::ControlFlow;

use regia_detsim::ai::{
    AiRng, AiRules, AiRulesError, PickKind, TargetHost, TargetPickReport, TargetRule, TargetSelector,
    TargetUnit, best_target, team_group_mask,
};
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

// ---------------------------------------------------------------------------
// Allocation counter (thread-local, so only the arming test's thread counts)
// ---------------------------------------------------------------------------

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    let armed = ARMED.try_with(Cell::get).unwrap_or(false);
    if armed {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: forwards every call to the system allocator unchanged; the only addition is a thread-local
// counter update, which neither allocates nor touches the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations_in(f: impl FnOnce()) -> u64 {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    COUNT.with(Cell::get)
}

// ---------------------------------------------------------------------------
// The seeded world
// ---------------------------------------------------------------------------

const FIELD_M: f32 = 1000.0;
const SIGHT_M: f32 = 200.0;
const TICKS: u64 = 600;
const UNITS: usize = 900;

#[derive(Debug, Clone, Copy)]
struct Unit {
    id: u32,
    team: u8,
    pos: [f32; 2],
    vel: [f32; 2],
    hp: f32,
    threat: f32,
    kind: u8,
    death_tick: u64,
    alive: bool,
}

#[derive(Debug, Clone)]
struct World {
    units: Vec<Unit>,
    tick: u64,
}

fn make_world(n: usize, teams: u8, seed: u64) -> World {
    let mut rng = AiRng::new(seed);
    let units = (0..n)
        .map(|i| Unit {
            // Non-contiguous ids, so a phase is the id's, not the slot's.
            id: 11 + 3 * i as u32,
            team: (i % usize::from(teams)) as u8,
            pos: [rng.next_f32() * FIELD_M, rng.next_f32() * FIELD_M],
            vel: [rng.next_f32() * 3.0 - 1.5, rng.next_f32() * 3.0 - 1.5],
            hp: 0.25 + rng.next_f32() * 0.75,
            threat: rng.next_f32(),
            kind: (rng.next_u64() % 4) as u8,
            death_tick: rng.range(0, 1500) as u64,
            alive: true,
        })
        .collect();
    World { units, tick: 0 }
}

impl World {
    /// Advance one tick: move (bounce at the field edges) and die on schedule.
    fn step(&mut self) {
        self.tick += 1;
        let tick = self.tick;
        for u in &mut self.units {
            if !u.alive {
                continue;
            }
            for axis in 0..2 {
                let next = u.pos[axis] + u.vel[axis];
                if !(0.0..=FIELD_M).contains(&next) {
                    u.vel[axis] = -u.vel[axis];
                } else {
                    u.pos[axis] = next;
                }
            }
            if u.death_tick == tick {
                u.alive = false;
            }
        }
    }

    fn rows(&self) -> Vec<SpatialRow> {
        self.units
            .iter()
            .enumerate()
            .map(|(slot, u)| SpatialRow {
                slot,
                body: BroadBody::new([u.pos[0], 0.0, u.pos[1]], 0.5, [0.0, 2.0]),
                group_mask: if u.alive { team_group_mask(u.team) } else { 0 },
                flags: 0,
            })
            .collect()
    }

    fn enemy_mask(&self, slot: usize, teams: u8) -> u64 {
        let own = self.units[slot].team;
        (0..teams)
            .filter(|&t| t != own)
            .fold(0, |m, t| m | team_group_mask(t))
    }

    fn units(&self, teams: u8) -> Vec<TargetUnit> {
        (0..self.units.len())
            .filter(|&s| self.units[s].alive)
            .map(|slot| TargetUnit {
                slot,
                entity_id: self.units[slot].id,
                enemy_mask: self.enemy_mask(slot, teams),
            })
            .collect()
    }

    /// The unit forgot the enemy this tick (a 50-tick window hash): perception's churn.
    fn forgotten(&self, unit: usize, enemy: usize) -> bool {
        let a = u64::from(self.units[unit].id);
        let b = u64::from(self.units[enemy].id);
        AiRng::mix(a.wrapping_mul(0x1_0000_0001) ^ b, self.tick / 50).is_multiple_of(5)
    }

    fn d2(&self, a: usize, b: usize) -> f32 {
        let dx = self.units[b].pos[0] - self.units[a].pos[0];
        let dz = self.units[b].pos[1] - self.units[a].pos[1];
        dx * dx + dz * dz
    }

    /// A unit's known-enemy list as a perception layer would hand it over: live, not forgotten, within sight,
    /// in a rotated (sighting) order rather than slot order.
    fn known_lists(&self, spatial: &mut DetSpatial, teams: u8) -> Vec<Vec<usize>> {
        let mut lists = vec![Vec::new(); self.units.len()];
        for (slot, list) in lists.iter_mut().enumerate() {
            if !self.units[slot].alive {
                continue;
            }
            let u = self.units[slot];
            let center = BroadBody::new([u.pos[0], 0.0, u.pos[1]], 0.5, [0.0, 2.0]);
            let mask = self.enemy_mask(slot, teams);
            let _ = spatial.within(center, SIGHT_M, mask, |e| {
                if self.d2(slot, e) <= SIGHT_M * SIGHT_M && !self.forgotten(slot, e) {
                    list.push(e);
                }
                ControlFlow::Continue(())
            });
            if !list.is_empty() {
                let by = (self.tick as usize + slot) % list.len();
                list.rotate_left(by);
            }
        }
        lists
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Nearest first (one subtraction pair and a product).
    Cheap,
    /// Slot rows read like a game's valuation: health, kind weight, threat, distance falloff. No strings.
    Do27,
    /// Every candidate scores the same (tie rules decide).
    Flat,
}

const KIND_WEIGHT: [f32; 4] = [1.0, 1.5, 0.75, 2.0];

struct Host<'w> {
    world: &'w World,
    shape: Shape,
    /// Exhaustive's tie rule: false = first of equal best wins (`>`), true = last wins (`>=`).
    ties_last: bool,
    scored: u64,
}

impl<'w> Host<'w> {
    fn new(world: &'w World, shape: Shape) -> Self {
        Host {
            world,
            shape,
            ties_last: false,
            scored: 0,
        }
    }

    fn value(&self, unit: usize, c: usize) -> f32 {
        let d2 = self.world.d2(unit, c);
        let t = &self.world.units[c];
        match self.shape {
            Shape::Cheap => -d2,
            Shape::Do27 => {
                let weak = 1.0 - t.hp;
                let kind = KIND_WEIGHT[usize::from(t.kind)];
                (0.5 + weak) * kind * (1.0 + t.threat) / (1.0 + d2 * 0.0005)
            }
            Shape::Flat => 1.0,
        }
    }
}

impl TargetHost for Host<'_> {
    fn score(&mut self, unit: usize, candidate: usize) -> f32 {
        self.scored += 1;
        self.value(unit, candidate)
    }

    fn eligible(&mut self, unit: usize, candidate: usize) -> bool {
        self.world.units[candidate].alive && !self.world.forgotten(unit, candidate)
    }

    fn replaces(&mut self, challenger: (usize, f32), best: Option<(usize, f32)>) -> bool {
        match best {
            None => !challenger.1.is_nan(),
            Some(b) if self.ties_last => challenger.1 >= b.1,
            Some(b) => challenger.1 > b.1,
        }
    }
}

/// The plain loop a game runs today: every known enemy in list order, the game's tie rule.
fn brute_pick(host: &mut Host<'_>, unit: usize, known: &[usize]) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for &e in known {
        let s = host.value(unit, e);
        let replace = match best {
            None => !s.is_nan(),
            Some(b) if host.ties_last => s >= b.1,
            Some(b) => s > b.1,
        };
        if replace {
            best = Some((e, s));
        }
    }
    best.map(|(e, _)| e)
}

fn spatial_for(world: &World) -> DetSpatial {
    let mut spatial = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..BroadphaseConfig::default()
    });
    spatial.build(&world.rows()).expect("valid slots");
    spatial
}

fn ringed() -> AiRules {
    AiRules::for_version(2).expect("version 2 is known")
}

const RINGS: [f32; 3] = [80.0, 120.0, 250.0];

// ---------------------------------------------------------------------------
// Acceptance tests
// ---------------------------------------------------------------------------

#[test]
fn exhaustive_rule_equals_a_brute_force_scan() {
    for (teams, seed) in [(2u8, 0xA12u64), (4, 0xB34)] {
        let mut world = make_world(UNITS, teams, seed);
        let mut selector = TargetSelector::new(AiRules::default()).expect("version 1 rules are valid");
        let mut targets = vec![None; UNITS];
        let mut picks_with_target = 0u64;
        let mut deaths_seen = false;
        for tick in 0..TICKS {
            let mut spatial = spatial_for(&world);
            let known = world.known_lists(&mut spatial, teams);
            let units = world.units(teams);
            // Every 11th tick scores every enemy the same, so the tie rule decides every pick.
            let shape = if tick % 11 == 5 { Shape::Flat } else { Shape::Do27 };
            let mut host = Host::new(&world, shape);
            // Every 7th tick runs the other tie rule: the selector must follow the caller's rule either way.
            host.ties_last = tick % 7 == 3;
            let mut unit_targets: Vec<Option<usize>> = units.iter().map(|u| targets[u.slot]).collect();
            let report = selector.pick_tick(
                tick,
                &units,
                &mut unit_targets,
                |slot| known[slot].as_slice(),
                &mut spatial,
                &mut host,
            );
            let mut expected_scores = 0u64;
            for (u, got) in units.iter().zip(&unit_targets) {
                let want = brute_pick(&mut host, u.slot, &known[u.slot]);
                assert_eq!(*got, want, "teams {teams} tick {tick} unit slot {}", u.slot);
                expected_scores += known[u.slot].len() as u64;
                picks_with_target += u64::from(got.is_some());
                targets[u.slot] = *got;
            }
            assert_eq!(
                report.scores_evaluated, expected_scores,
                "every known enemy, every tick"
            );
            assert_eq!(report.picks, units.len() as u64);
            assert_eq!(report.kept_targets, 0);
            assert_eq!(report.forced_repicks, 0);
            deaths_seen |= units.len() < UNITS;
            world.step();
        }
        assert!(
            deaths_seen && picks_with_target > 100_000,
            "the run must exercise deaths and picks"
        );
    }
}

/// One `Ringed` run over the seeded world; `each` sees every tick's units, picks and report.
fn run_ringed(
    teams: u8,
    seed: u64,
    shape: Shape,
    mut each: impl FnMut(&World, &[TargetUnit], &[(Option<usize>, PickKind)], &TargetPickReport),
) {
    let mut world = make_world(UNITS, teams, seed);
    let mut selector = TargetSelector::new(ringed()).expect("valid");
    let mut targets = vec![None; UNITS];
    let mut kinds = Vec::new();
    for tick in 0..TICKS {
        let mut spatial = spatial_for(&world);
        let units = world.units(teams);
        let mut host = Host::new(&world, shape);
        kinds.clear();
        for u in &units {
            let pick = selector.pick(tick, *u, &mut targets[u.slot], &[], &mut spatial, &mut host);
            assert_eq!(pick.target, targets[u.slot]);
            kinds.push((pick.target, pick.kind));
        }
        let report = selector.take_report();
        assert_eq!(
            report.scores_evaluated, host.scored,
            "the report counts every score call"
        );
        each(&world, &units, &kinds, &report);
        // Dead units keep no target.
        for (slot, u) in world.units.iter().enumerate() {
            if !u.alive {
                targets[slot] = None;
            }
        }
        world.step();
    }
}

#[test]
fn ringed_rule_scores_at_most_n_k_over_r_per_tick() {
    let (k, r) = match ringed().target {
        TargetRule::Ringed {
            max_candidates,
            repick_ticks,
            ..
        } => (u64::from(max_candidates), u64::from(repick_ticks)),
        TargetRule::Exhaustive => unreachable!("version 2 is Ringed"),
    };
    assert_eq!((k, r), (8, 4), "the documented default");
    let mut total_scores = 0u64;
    let mut total_forced = 0u64;
    let mut total_kept = 0u64;
    run_ringed(2, 0xC56, Shape::Do27, |_, units, _, report| {
        let due = units
            .iter()
            .filter(|u| u64::from(u.entity_id) % r == report_tick(report) % r)
            .count() as u64;
        // n x k / r, plus k per forced re-pick: no unit scores more than k candidates per pick.
        // The ids are spread evenly over the phases, so at most UNITS / r units are due on any tick.
        let bound = (UNITS as u64).div_ceil(r) * k + report.forced_repicks * k;
        assert!(due <= (UNITS as u64).div_ceil(r));
        assert!(
            report.scores_evaluated <= bound,
            "tick {}: {} scores > bound {bound}",
            report_tick(report),
            report.scores_evaluated
        );
        assert!(report.scores_evaluated <= (due + report.forced_repicks) * k);
        assert_eq!(
            report.picks,
            due + report.forced_repicks,
            "scheduled picks are the due units"
        );
        total_scores += report.scores_evaluated;
        total_forced += report.forced_repicks;
        total_kept += report.kept_targets;
    });
    // The gap this closes: the scan scores every known enemy every tick (~405,000 at 900 units, 2 teams).
    let per_tick = total_scores / TICKS;
    assert!(per_tick <= (UNITS as u64 / r) * k + total_forced / TICKS * k + k);
    assert!(per_tick < 3_000, "Ringed at 900 units scores {per_tick} per tick");
    assert!(
        total_forced > 0 && total_kept > 0,
        "the run must exercise forced re-picks and kept targets"
    );
}

/// The report does not carry the tick; the tests track it beside the report.
fn report_tick(report: &TargetPickReport) -> u64 {
    report.tick
}

#[test]
fn ringed_rule_prefers_an_in_range_target() {
    for teams in [2u8, 4] {
        let mut checked = 0u64;
        let mut outer_picks = 0u64;
        run_ringed(
            teams,
            0xD78 + u64::from(teams),
            Shape::Do27,
            |world, units, picks, _| {
                for (u, &(target, kind)) in units.iter().zip(picks) {
                    if !matches!(kind, PickKind::Scheduled | PickKind::Forced) {
                        continue;
                    }
                    // Oracle: the innermost ring that holds any eligible enemy, by a plain scan.
                    let eligible = |e: usize| {
                        e != u.slot
                            && world.units[e].alive
                            && u.enemy_mask & team_group_mask(world.units[e].team) != 0
                            && !world.forgotten(u.slot, e)
                    };
                    let inner = RINGS.iter().position(|&r| {
                        (0..world.units.len()).any(|e| eligible(e) && world.d2(u.slot, e) <= r * r)
                    });
                    match (inner, target) {
                        (None, None) => {}
                        (None, Some(t)) => panic!("unit {} picked {t} with no enemy in any ring", u.slot),
                        (Some(i), None) => panic!("unit {} picked nothing with an enemy in ring {i}", u.slot),
                        (Some(i), Some(t)) => {
                            let r = RINGS[i];
                            assert!(eligible(t), "an ineligible pick");
                            assert!(
                                world.d2(u.slot, t) <= r * r,
                                "unit {} picked {t} outside ring {i} ({r} m) while an enemy is inside it",
                                u.slot
                            );
                            checked += 1;
                            outer_picks += u64::from(i > 0);
                        }
                    }
                }
            },
        );
        assert!(
            checked > 10_000 && outer_picks > 0,
            "teams {teams}: {checked} picks, {outer_picks} outer"
        );
    }
}

/// A hand-built scene: unit 0 (team 0) at the origin, enemies on team 1.
fn scene(enemies: &[[f32; 2]]) -> World {
    let mut units = vec![Unit {
        id: 4, // phase 0 under repick_ticks 4: due on ticks 0, 4, 8, ...
        team: 0,
        pos: [0.0, 0.0],
        vel: [0.0, 0.0],
        hp: 1.0,
        threat: 0.0,
        kind: 0,
        death_tick: u64::MAX,
        alive: true,
    }];
    for (i, p) in enemies.iter().enumerate() {
        units.push(Unit {
            id: 100 + i as u32,
            team: 1,
            pos: *p,
            hp: 1.0,
            ..units[0]
        });
    }
    World { units, tick: 0 }
}

/// A scene host: the score prefers lower slots slightly unless `flat`, and `forgotten` lists
/// (unit, enemy) pairs the unit no longer knows.
struct SceneHost<'w> {
    world: &'w World,
    forgotten: Vec<usize>,
    flat: bool,
}

impl TargetHost for SceneHost<'_> {
    fn score(&mut self, _unit: usize, candidate: usize) -> f32 {
        if self.flat { 1.0 } else { 10.0 - candidate as f32 }
    }
    fn eligible(&mut self, _unit: usize, candidate: usize) -> bool {
        self.world.units[candidate].alive && !self.forgotten.contains(&candidate)
    }
}

fn unit0() -> TargetUnit {
    TargetUnit {
        slot: 0,
        entity_id: 4,
        enemy_mask: team_group_mask(1),
    }
}

#[test]
fn an_invalid_target_is_repicked_on_the_same_tick() {
    // B at 10 m (slot 1), C at 50 m (slot 2), D at 200 m (slot 3).
    let mut world = scene(&[[10.0, 0.0], [0.0, 50.0], [200.0, 0.0]]);
    let mut selector = TargetSelector::new(ringed()).expect("valid");
    let mut target = None;
    let mut forgotten = Vec::new();
    let mut step = |world: &World, forgotten: &[usize], tick: u64, target: &mut Option<usize>| {
        let mut spatial = spatial_for(world);
        let mut host = SceneHost {
            world,
            forgotten: forgotten.to_vec(),
            flat: false,
        };
        selector.pick(tick, unit0(), target, &[], &mut spatial, &mut host)
    };

    let p = step(&world, &forgotten, 0, &mut target);
    assert_eq!(
        (p.kind, p.target),
        (PickKind::Scheduled, Some(1)),
        "tick 0 is the unit's phase"
    );
    let p = step(&world, &forgotten, 1, &mut target);
    assert_eq!((p.kind, p.target), (PickKind::Kept, Some(1)));

    // Kill B between picks: the re-pick happens on tick 2, not on the next phase (tick 4).
    world.units[1].alive = false;
    let p = step(&world, &forgotten, 2, &mut target);
    assert_eq!((p.kind, p.target), (PickKind::Forced, Some(2)), "dead target");

    // Forget C: the inner rings are now empty, D in the outer ring is picked at once.
    forgotten.push(2);
    let p = step(&world, &forgotten, 3, &mut target);
    assert_eq!(
        (p.kind, p.target),
        (PickKind::Forced, Some(3)),
        "forgotten target"
    );

    // Tick 4 is the phase: a scheduled pick keeps D (the only candidate).
    let p = step(&world, &forgotten, 4, &mut target);
    assert_eq!((p.kind, p.target), (PickKind::Scheduled, Some(3)));

    // Exactly on the outermost ring still counts as inside.
    world.units[3].pos = [250.0, 0.0];
    let p = step(&world, &forgotten, 5, &mut target);
    assert_eq!(
        (p.kind, p.target),
        (PickKind::Kept, Some(3)),
        "on the ring is inside"
    );

    // Move D beyond the outermost ring: forced re-pick on the same tick, nothing left to pick.
    world.units[3].pos = [250.5, 0.0];
    let p = step(&world, &forgotten, 6, &mut target);
    assert_eq!(
        (p.kind, p.target),
        (PickKind::Forced, None),
        "target beyond the outer ring"
    );

    // No target and not due: idle until the phase.
    let p = step(&world, &forgotten, 7, &mut target);
    assert_eq!((p.kind, p.target), (PickKind::Idle, None));
    // C is remembered again and comes back on the next phase.
    forgotten.clear();
    let p = step(&world, &forgotten, 8, &mut target);
    assert_eq!((p.kind, p.target), (PickKind::Scheduled, Some(2)));

    let report = selector.report();
    assert_eq!(report.forced_repicks, 3);
    assert_eq!(report.kept_targets, 2);
    assert_eq!(report.picks, 6, "three scheduled + three forced");
    assert_eq!(report.units, 9);
}

#[test]
fn floors_masks_and_the_unit_itself_bound_the_candidates() {
    // Slot 1: an ally at 5 m. Slot 2: an enemy at 10 m one floor up. Slot 3: an enemy at 50 m on the unit's
    // floor. Slot 4: an enemy at 60 m on a team the unit does not target.
    let world = scene(&[[5.0, 0.0], [10.0, 0.0], [0.0, 50.0], [0.0, 60.0]]);
    let rows_with = |upper_band: [f32; 2]| -> DetSpatial {
        let mut rows = world.rows();
        rows[1].group_mask = team_group_mask(0);
        rows[2].body.band = upper_band;
        rows[4].group_mask = team_group_mask(2);
        let mut spatial = DetSpatial::new(BroadphaseConfig {
            cell_size: 40.0,
            ..BroadphaseConfig::default()
        });
        spatial.build(&rows).expect("valid slots");
        spatial
    };
    let mut host = SceneHost {
        world: &world,
        forgotten: Vec::new(),
        flat: false,
    };
    let mut selector = TargetSelector::new(ringed()).expect("valid");
    let mut target = None;
    // The upper floor is out of reach (default `FloorRule::Bands`): slot 3 wins although slot 2 scores higher.
    let mut spatial = rows_with([10.0, 12.0]);
    let p = selector.pick(0, unit0(), &mut target, &[], &mut spatial, &mut host);
    assert_eq!(p.target, Some(3));
    // The unit itself is never a candidate, even with its own team in the mask.
    let mut all = unit0();
    all.enemy_mask = team_group_mask(0) | team_group_mask(1);
    let mut own = None;
    let p = selector.pick(0, all, &mut own, &[], &mut spatial, &mut host);
    assert_eq!(
        p.target,
        Some(1),
        "the ally is in the mask now, the unit itself never is"
    );
    // The upper enemy comes down to the unit's floor: it wins at the next phase, and the held target is kept
    // until then.
    let mut spatial = rows_with([0.0, 2.0]);
    let p = selector.pick(1, unit0(), &mut target, &[], &mut spatial, &mut host);
    assert_eq!((p.kind, p.target), (PickKind::Kept, Some(3)));
    let p = selector.pick(4, unit0(), &mut target, &[], &mut spatial, &mut host);
    assert_eq!((p.kind, p.target), (PickKind::Scheduled, Some(2)));
    // It climbs back up between picks: forced re-pick on that tick.
    let mut spatial = rows_with([10.0, 12.0]);
    let p = selector.pick(5, unit0(), &mut target, &[], &mut spatial, &mut host);
    assert_eq!(
        (p.kind, p.target),
        (PickKind::Forced, Some(3)),
        "a target on another floor is invalid"
    );
    // A unit whose own row is not in the index finds nothing (and does not panic).
    let mut lost = unit0();
    lost.slot = 99;
    let mut none = None;
    let p = selector.pick(0, lost, &mut none, &[], &mut spatial, &mut host);
    assert_eq!(p.target, None);
    // Exhaustive with an empty or all-NaN list picks nothing.
    let mut exhaustive = TargetSelector::new(AiRules::default()).expect("valid");
    struct NanHost;
    impl TargetHost for NanHost {
        fn score(&mut self, _: usize, _: usize) -> f32 {
            f32::NAN
        }
    }
    let mut t = Some(3);
    assert_eq!(
        exhaustive
            .pick(0, unit0(), &mut t, &[], &mut spatial, &mut NanHost)
            .target,
        None
    );
    assert_eq!(t, None, "Exhaustive overwrites the held target every tick");
    assert_eq!(
        exhaustive
            .pick(0, unit0(), &mut t, &[2, 3], &mut spatial, &mut NanHost)
            .target,
        None
    );
    assert!(
        (0..12).all(|tick| exhaustive.due(tick, 7)),
        "Exhaustive picks every tick"
    );
    assert_eq!(
        (0..12).filter(|&tick| selector.due(tick, 7)).count(),
        3,
        "Ringed every 4 ticks"
    );
}

#[test]
fn ties_go_to_the_lowest_slot_whatever_the_list_order() {
    // The reduction alone: highest score, then lowest slot, NaN never wins, any input order.
    let pairs = [
        (9usize, 2.0f32),
        (4, 3.0),
        (7, 3.0),
        (2, f32::NAN),
        (5, 3.0),
        (1, 1.0),
        (8, 0.0),
        (6, -0.0),
    ];
    let mut rng = AiRng::new(0x7135);
    for _ in 0..64 {
        let mut shuffled = pairs.to_vec();
        shuffle(&mut shuffled, &mut rng);
        assert_eq!(best_target(shuffled.iter().copied()), Some((4, 3.0)));
    }
    assert_eq!(best_target([(3usize, f32::NAN)]), None);
    assert_eq!(
        best_target([(3usize, -0.0f32), (1, 0.0)]).map(|b| b.0),
        Some(1),
        "-0 ties +0"
    );

    // The selector: flat scores, enemies in the inner ring at slots 1..=6 given in shuffled row orders and
    // shuffled known lists; the pick is always slot 1.
    let world = scene(&[
        [5.0, 0.0],
        [0.0, 6.0],
        [7.0, 0.0],
        [0.0, 8.0],
        [9.0, 0.0],
        [0.0, 10.0],
        [100.0, 0.0],
    ]);
    for round in 0..32 {
        let mut rows = world.rows();
        shuffle(&mut rows, &mut rng);
        let mut spatial = DetSpatial::new(BroadphaseConfig {
            cell_size: 40.0,
            ..BroadphaseConfig::default()
        });
        spatial.build(&rows).expect("valid slots");
        let mut known: Vec<usize> = (1..world.units.len()).collect();
        shuffle(&mut known, &mut rng);
        let mut selector = TargetSelector::new(ringed()).expect("valid");
        let mut target = None;
        let mut host = SceneHost {
            world: &world,
            forgotten: Vec::new(),
            flat: true,
        };
        let p = selector.pick(0, unit0(), &mut target, &known, &mut spatial, &mut host);
        assert_eq!(p.target, Some(1), "round {round}");
        // Forget the winner: the next lowest slot wins.
        host.forgotten.push(1);
        let mut target = None;
        let p = selector.pick(4, unit0(), &mut target, &known, &mut spatial, &mut host);
        assert_eq!(p.target, Some(2), "round {round}");
    }
}

#[test]
fn older_version_refuses_ringed_rules() {
    assert_eq!(TargetRule::for_version(1), Ok(TargetRule::Exhaustive));
    assert_eq!(
        TargetRule::for_version(2),
        Ok(TargetRule::Ringed {
            rings_m: RINGS.to_vec(),
            max_candidates: 8,
            repick_ticks: 4,
        })
    );
    let unknown = TargetRule::for_version(0).expect_err("version 0 is not a rules version");
    assert_eq!(unknown.code(), "ai_rules_unknown_version");
    assert_eq!(
        TargetRule::for_version(3).map_err(AiRulesError::code),
        Err("ai_rules_unknown_version")
    );
    assert_eq!(TargetRule::Exhaustive.min_version(), 1);
    assert_eq!(ringed().target.min_version(), 2);

    // A version-1 rules file that names Ringed is refused: Ringed needs the bump.
    let stale = AiRules {
        version: 1,
        target: ringed().target,
    };
    let err = stale.validate().expect_err("Ringed needs version 2");
    assert_eq!(err, AiRulesError::VersionTooOld { version: 1, needs: 2 });
    assert_eq!(err.code(), "ai_rules_version_too_old");
    assert!(
        TargetSelector::new(stale).is_err(),
        "the selector validates its rules"
    );

    // A host on version 2 refuses a peer that only knows version 1; the reverse is fine (v2 builds still
    // run Exhaustive), and equal versions join.
    let host = ringed();
    let refused = host
        .check_peer(1)
        .expect_err("a version-1 peer cannot run Ringed");
    assert_eq!(refused, AiRulesError::PeerTooOld { peer: 1, needs: 2 });
    assert_eq!(refused.code(), "ai_rules_peer_too_old");
    assert_eq!(host.check_peer(2), Ok(()));
    assert_eq!(AiRules::default().check_peer(2), Ok(()));
    assert_eq!(AiRules::default().check_peer(1), Ok(()));
    assert_eq!(
        AiRules::default().check_peer(0).map_err(AiRulesError::code),
        Err("ai_rules_peer_too_old")
    );

    // Malformed Ringed settings each have their own code.
    let bad = |rings_m: Vec<f32>, max_candidates: u16, repick_ticks: u32| {
        AiRules {
            version: 2,
            target: TargetRule::Ringed {
                rings_m,
                max_candidates,
                repick_ticks,
            },
        }
        .validate()
        .map_err(AiRulesError::code)
    };
    assert_eq!(bad(vec![], 8, 4), Err("ai_rules_bad_rings"));
    assert_eq!(bad(vec![80.0, 80.0], 8, 4), Err("ai_rules_bad_rings"));
    assert_eq!(bad(vec![120.0, 80.0], 8, 4), Err("ai_rules_bad_rings"));
    assert_eq!(bad(vec![0.0, 80.0], 8, 4), Err("ai_rules_bad_rings"));
    assert_eq!(bad(vec![f32::NAN], 8, 4), Err("ai_rules_bad_rings"));
    assert_eq!(bad(vec![f32::INFINITY], 8, 4), Err("ai_rules_bad_rings"));
    assert_eq!(
        bad((1..=9).map(|i| 10.0 * i as f32).collect(), 8, 4),
        Err("ai_rules_bad_rings")
    );
    assert_eq!(bad(vec![80.0], 0, 4), Err("ai_rules_bad_max_candidates"));
    assert_eq!(bad(vec![80.0], 8, 0), Err("ai_rules_bad_repick_ticks"));
    assert_eq!(bad(vec![80.0], 1, 1), Ok(()));
    for e in [
        AiRulesError::UnknownVersion { version: 9 },
        AiRulesError::VersionTooOld { version: 1, needs: 2 },
        AiRulesError::PeerTooOld { peer: 1, needs: 2 },
        AiRulesError::BadRings,
        AiRulesError::BadMaxCandidates,
        AiRulesError::BadRepickTicks,
    ] {
        let code = e.code();
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
        assert!(e.to_string().starts_with(code), "{e}");
    }
}

#[test]
fn rules_round_trip_serde() {
    for rules in [AiRules::default(), ringed()] {
        let json = serde_json::to_string(&rules).expect("serialize");
        let back: AiRules = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, rules, "{json}");
        assert_eq!(back.validate(), Ok(()));
    }
    // The stored shape is what a settings file holds.
    let json = serde_json::to_value(ringed()).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "version": 2,
            "target": { "Ringed": { "rings_m": [80.0, 120.0, 250.0], "max_candidates": 8, "repick_ticks": 4 } }
        })
    );
    assert_eq!(
        serde_json::to_value(AiRules::default()).expect("serialize"),
        serde_json::json!({ "version": 1, "target": "Exhaustive" })
    );
    // A hand-edited file that names Ringed under version 1 loads but does not validate.
    let stale: AiRules = serde_json::from_str(
        r#"{"version":1,"target":{"Ringed":{"rings_m":[50.0],"max_candidates":4,"repick_ticks":2}}}"#,
    )
    .expect("well-formed");
    assert_eq!(
        stale.validate().map_err(AiRulesError::code),
        Err("ai_rules_version_too_old")
    );

    let report = TargetPickReport {
        tick: 7,
        units: 900,
        picks: 230,
        scores_evaluated: 1800,
        candidates_seen: 4000,
        forced_repicks: 5,
        kept_targets: 660,
    };
    let back: TargetPickReport =
        serde_json::from_str(&serde_json::to_string(&report).expect("serialize")).expect("deserialize");
    assert_eq!(back, report);
}

/// FNV-1a over little-endian words.
fn fnv(words: impl IntoIterator<Item = u64>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Every pick of the seeded `Ringed` run (4 teams, DO27-shaped closure): tick, unit slot, target or `u64::MAX`,
/// pick kind.
fn ringed_run_hash(teams: u8, seed: u64) -> u64 {
    let mut words = Vec::new();
    run_ringed(teams, seed, Shape::Do27, |world, units, picks, report| {
        for (u, &(target, kind)) in units.iter().zip(picks) {
            words.push(world.tick);
            words.push(u.slot as u64);
            words.push(target.map_or(u64::MAX, |t| t as u64));
            words.push(kind as u64);
        }
        words.push(report.scores_evaluated);
        words.push(report.candidates_seen);
    });
    fnv(words)
}

#[test]
fn targets_bits_match_the_pinned_hash() {
    let got = [ringed_run_hash(4, 0x007A_26E7), ringed_run_hash(2, 0x007A_26E8)];
    eprintln!("targets pins: [{:#018x}, {:#018x}]", got[0], got[1]);
    assert_eq!(
        got, TARGETS_PINNED,
        "Ringed target picks moved: candidates, rings, cadence or ties"
    );
}

/// Recorded on x86-64 Linux (the workspace test profile). aarch64 (the Mac, the TEST Pixel 9 Pro) must match.
const TARGETS_PINNED: [u64; 2] = [0x455d_e2b7_8791_3fc9, 0x0ac4_f820_8c3a_3d91];

#[test]
fn a_warm_selector_allocates_nothing() {
    let world = make_world(UNITS, 2, 0xE9A);
    let mut spatial = spatial_for(&world);
    let units = world.units(2);
    let known = world.known_lists(&mut spatial, 2);
    for rules in [AiRules::default(), ringed()] {
        let mut selector = TargetSelector::new(rules).expect("valid");
        let mut targets = vec![None; units.len()];
        let mut host = Host::new(&world, Shape::Cheap);
        // Warm: the first ticks may size the scratch.
        for tick in 0..8 {
            selector.pick_tick(
                tick,
                &units,
                &mut targets,
                |s| known[s].as_slice(),
                &mut spatial,
                &mut host,
            );
        }
        let n = allocations_in(|| {
            for tick in 8..24 {
                selector.pick_tick(
                    tick,
                    &units,
                    &mut targets,
                    |s| known[s].as_slice(),
                    &mut spatial,
                    &mut host,
                );
            }
        });
        assert_eq!(
            n,
            0,
            "{:?}: a warm selector allocated {n} times",
            selector.rules().target
        );
    }
}

fn shuffle<T>(v: &mut [T], rng: &mut AiRng) {
    for i in (1..v.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}
