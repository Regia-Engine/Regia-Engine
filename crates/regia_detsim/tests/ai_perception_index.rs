// SPDX-License-Identifier: MPL-2.0
//! Perception on the spatial index and borrowed AI world views (gap AI-4, ADR-8518).
//!
//! The indexed sighting refresh (`SightingsBoard::refresh_with` over a `PerceptionIndex`) must write the
//! boards the linear all-pairs scan (`SightingsBoard::refresh_linear`) writes, bit for bit, and ask the
//! line of sight the same questions in the same order. `AiWorldView` borrows the per-team facts built
//! once per tick and allocates nothing.
//!
//! Counting allocator adapted from Regia's spatial_alloc.rs (ADR-8318).

#![allow(unsafe_code)]
// clippy 1.99 reports `missing_const_for_thread_local` on aarch64-linux-android only, at the
// `thread_local!` below whose initializers are already `const { }` (a false positive in the
// expansion; Linux and Windows clippy are clean). `expect` fails the build once it stops (ADR-8047).
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use regia_detsim::ai::{
    AiTickWorld, AiUnitFacts, KnownEnemy, LosFn, PerceptEnemy, PerceptUnit, PerceptionIndex,
    PerceptionIndexSettings, PerceptionParams, RADAR_RANGE_M, RosterSettings, SightingsBoard, TeamRosters,
};

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

// SAFETY: forwards every call to the system allocator unchanged; the only addition is a
// thread-local counter update, which neither allocates nor touches the returned memory.
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

/// Allocations made on this thread while `f` runs.
fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (COUNT.with(Cell::get), out)
}

/// Seeded xorshift64 (test input only).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform in `[0, 1)` from 24 bits (exact in f32).
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Debug, Clone, Copy)]
struct Body {
    id: u32,
    team: u8,
    pos: [f32; 3],
    vel: [f32; 2],
    sight: f32,
    radar_scale: f32,
    lit: bool,
}

/// A seeded battle: `n` bodies on `teams` teams over a `side` x `side` metre square.
struct Battle {
    rng: Rng,
    bodies: Vec<Body>,
    side: f32,
}

const SIGHTS: [f32; 4] = [150.0, 220.0, 300.0, 90.0];

impl Battle {
    fn new(seed: u64, n: usize, teams: u8, side: f32) -> Self {
        let mut rng = Rng(seed);
        let bodies = (0..n)
            .map(|i| {
                let team = (rng.below(u64::from(teams))) as u8;
                Body {
                    id: (i as u32) * 3 + 7,
                    team,
                    pos: [rng.unit() * side, rng.unit() * 4.0, rng.unit() * side],
                    vel: [rng.unit() * 6.0 - 3.0, rng.unit() * 6.0 - 3.0],
                    sight: SIGHTS[rng.below(4) as usize],
                    radar_scale: if rng.below(5) == 0 { 0.5 } else { 1.0 },
                    lit: rng.below(7) == 0,
                }
            })
            .collect();
        Battle { rng, bodies, side }
    }

    /// One tick of motion, deaths, lights and sensor damage.
    fn step(&mut self, deaths_per_mille: u64) {
        let side = self.side;
        let mut keep = Vec::with_capacity(self.bodies.len());
        for mut b in self.bodies.drain(..) {
            if self.rng.below(1000) < deaths_per_mille {
                continue;
            }
            b.pos[0] += b.vel[0];
            b.pos[2] += b.vel[1];
            if b.pos[0] < 0.0 || b.pos[0] > side {
                b.vel[0] = -b.vel[0];
            }
            if b.pos[2] < 0.0 || b.pos[2] > side {
                b.vel[1] = -b.vel[1];
            }
            if self.rng.below(50) == 0 {
                b.lit = !b.lit;
            }
            if self.rng.below(200) == 0 {
                b.radar_scale = if b.radar_scale < 1.0 { 1.0 } else { 0.5 };
            }
            keep.push(b);
        }
        self.bodies = keep;
    }

    fn units(&self) -> Vec<PerceptUnit> {
        self.bodies
            .iter()
            .map(|b| PerceptUnit {
                id: b.id,
                team: b.team,
                pos: b.pos,
                sight_range_m: b.sight,
                radar_scale: b.radar_scale,
            })
            .collect()
    }

    fn enemies(&self) -> Vec<PerceptEnemy> {
        // Bodies keep ascending id order (built ascending, removals keep order).
        self.bodies
            .iter()
            .map(|b| PerceptEnemy {
                id: b.id,
                team: b.team,
                pos: b.pos,
                vel: b.vel,
                dps: 10.0 + (b.id % 7) as f32,
                band: [0.0, 100.0 + (b.id % 5) as f32 * 40.0],
                lit: b.lit,
            })
            .collect()
    }
}

fn mixed_params(teams: usize) -> Vec<PerceptionParams> {
    let intervals = [1, 4, 12, 30, 2, 7];
    (0..teams)
        .map(|t| PerceptionParams {
            los_recheck_interval_ticks: intervals[t % intervals.len()],
            confidence_decay: 0.96 - 0.02 * t as f32,
            forget_confidence: 0.35 + 0.05 * t as f32,
            ..PerceptionParams::default()
        })
        .collect()
}

/// A deterministic blocker pattern: some pairs are clear, some blocked.
fn blocked(from: [f32; 3], to: [f32; 3]) -> bool {
    let a = (from[0] * 0.05) as i64;
    let b = (to[2] * 0.05) as i64;
    (a ^ b) & 3 == 0
}

fn pos_bits(p: [f32; 3]) -> [u32; 3] {
    [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()]
}

fn board_bits(board: &SightingsBoard) -> Vec<u64> {
    let mut out = vec![board.by_team.len() as u64, u64::from(board.radar_range.to_bits())];
    for row in &board.by_team {
        out.push(row.len() as u64);
        for k in row {
            out.extend(known_bits(k));
        }
    }
    out
}

fn known_bits(k: &KnownEnemy) -> Vec<u64> {
    let s = &k.sighting;
    vec![
        u64::from(k.entity),
        u64::from(s.entity),
        u64::from(s.last_pos[0].to_bits()),
        u64::from(s.last_pos[1].to_bits()),
        u64::from(s.last_pos[2].to_bits()),
        u64::from(s.last_vel[0].to_bits()),
        u64::from(s.last_vel[1].to_bits()),
        s.last_seen_tick,
        s.first_seen_tick,
        u64::from(s.confidence.to_bits()),
        s.source as u64,
        u64::from(k.threat.dps.to_bits()),
        u64::from(k.threat.range_band[0].to_bits()),
        u64::from(k.threat.range_band[1].to_bits()),
        u64::from(k.threat.can_reach_me),
        u64::from(k.threat.incoming),
    ]
}

fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    regia_detsim::math::sqrt(dx * dx + dz * dz)
}

#[test]
fn indexed_refresh_equals_the_linear_refresh_every_tick() {
    const TEAMS: usize = 4;
    let mut battle = Battle::new(0x05EE_DA14_0900, 900, TEAMS as u8, 3000.0);
    // Edge rows: a unit on a team past the board, one at a non-finite spot, one far off the map.
    battle.bodies[3].team = 5;
    battle.bodies[11].pos = [f32::NAN, 0.0, 10.0];
    battle.bodies[11].vel = [0.0, 0.0];
    battle.bodies[17].pos = [1.0e7, 0.0, -1.0e7];
    battle.bodies[17].vel = [0.0, 0.0];
    let params = mixed_params(TEAMS);
    let mut linear = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let mut indexed = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let mut scratch = PerceptionIndex::new(PerceptionIndexSettings::default());
    let calls_linear = RefCell::new(Vec::<[u32; 6]>::new());
    let calls_indexed = RefCell::new(Vec::<[u32; 6]>::new());
    let los_linear = LosFn(|from: [f32; 3], to: [f32; 3]| {
        let (f, t) = (pos_bits(from), pos_bits(to));
        calls_linear
            .borrow_mut()
            .push([f[0], f[1], f[2], t[0], t[1], t[2]]);
        !blocked(from, to)
    });
    let los_indexed = LosFn(|from: [f32; 3], to: [f32; 3]| {
        let (f, t) = (pos_bits(from), pos_bits(to));
        calls_indexed
            .borrow_mut()
            .push([f[0], f[1], f[2], t[0], t[1], t[2]]);
        !blocked(from, to)
    });
    let mut total_calls = 0usize;
    let mut total_rows = 0usize;
    for tick in 0..600u64 {
        let units = battle.units();
        let enemies = battle.enemies();
        calls_linear.borrow_mut().clear();
        calls_indexed.borrow_mut().clear();
        linear.refresh_linear(los_linear, tick, &units, &enemies, &params);
        indexed.refresh_with(&mut scratch, los_indexed, tick, &units, &enemies, &params);
        assert_eq!(
            *calls_linear.borrow(),
            *calls_indexed.borrow(),
            "tick {tick}: line-of-sight call sequence differs"
        );
        assert_eq!(
            board_bits(&linear),
            board_bits(&indexed),
            "tick {tick}: boards differ"
        );
        total_calls += calls_linear.borrow().len();
        total_rows += linear.by_team.iter().map(Vec::len).sum::<usize>();
        battle.step(1);
    }
    // The run exercised the paths it claims to (not an empty world).
    assert!(total_calls > 10_000, "only {total_calls} line-of-sight calls");
    assert!(total_rows > 100_000, "only {total_rows} board rows");
    assert!(
        battle.bodies.len() < 900 && battle.bodies.len() > 300,
        "{} alive",
        battle.bodies.len()
    );
    let stats = scratch.stats();
    assert_eq!(stats.refreshes, 600);
    assert_eq!(stats.los_calls as usize, total_calls);
}

#[test]
fn plain_refresh_is_the_indexed_refresh() {
    // `refresh` (S-10's entry point, which DO27's tests and the pins call) runs the indexed path with a
    // fresh scratch: same boards as the linear scan on a dense, a sparse and a degenerate world.
    for (seed, n, side) in [(1u64, 300usize, 400.0f32), (2, 600, 6000.0), (3, 40, 0.0)] {
        let mut battle = Battle::new(seed, n, 3, side);
        let params = mixed_params(3);
        let mut a = SightingsBoard::new(3, RADAR_RANGE_M);
        let mut b = SightingsBoard::new(3, RADAR_RANGE_M);
        let los = LosFn(|f: [f32; 3], t: [f32; 3]| !blocked(f, t));
        for tick in 0..40 {
            let (units, enemies) = (battle.units(), battle.enemies());
            a.refresh_linear(los, tick, &units, &enemies, &params);
            b.refresh(los, tick, &units, &enemies, &params);
            assert_eq!(board_bits(&a), board_bits(&b), "seed {seed} tick {tick}");
            battle.step(2);
        }
    }
}

#[test]
fn los_is_asked_only_for_in_range_pairs() {
    const TEAMS: usize = 3;
    let mut battle = Battle::new(0xC0FFEE, 600, TEAMS as u8, 2500.0);
    let params = mixed_params(TEAMS);
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let mut scratch = PerceptionIndex::new(PerceptionIndexSettings::default());
    let calls = RefCell::new(Vec::<([f32; 3], [f32; 3])>::new());
    let los = LosFn(|from: [f32; 3], to: [f32; 3]| {
        calls.borrow_mut().push((from, to));
        !blocked(from, to)
    });
    let mut asked = 0usize;
    for tick in 0..120u64 {
        let units = battle.units();
        let enemies = battle.enemies();
        // Observer eye -> its sight range (positions are distinct in this seeded world).
        let sight: BTreeMap<[u32; 3], (f32, u8, u32)> = units
            .iter()
            .map(|u| (pos_bits(u.pos), (u.sight_range_m, u.team, u.id)))
            .collect();
        let interval = |team: u8| params[team as usize].los_recheck_interval_ticks.max(1) as u64;
        calls.borrow_mut().clear();
        let before = scratch.stats().los_calls;
        board.refresh_with(&mut scratch, los, tick, &units, &enemies, &params);
        for (from, to) in calls.borrow().iter() {
            let (range, team, id) = sight[&pos_bits(*from)];
            assert!(
                dist_xz(*from, *to) <= range,
                "tick {tick}: LOS asked for a pair {} m apart, sight {range}",
                dist_xz(*from, *to)
            );
            assert_eq!(
                tick % interval(team),
                u64::from(id) % interval(team),
                "observer off its phase"
            );
        }
        assert_eq!(scratch.stats().los_calls - before, calls.borrow().len() as u64);
        asked += calls.borrow().len();
        battle.step(0);
    }
    assert!(asked > 1000, "only {asked} LOS calls");
}

#[test]
fn refresh_work_is_bounded_by_density() {
    // A sparse field (900 units over 8 km x 8 km): the all-pairs scan reads every ally of every other
    // team for every enemy; the indexed refresh reads only the allies in the cells a query box covers.
    const TEAMS: usize = 4;
    let side = 8000.0f32;
    let mut battle = Battle::new(0xDE5_1717, 900, TEAMS as u8, side);
    let params = mixed_params(TEAMS);
    let settings = PerceptionIndexSettings::default();
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let mut scratch = PerceptionIndex::new(settings);
    let los = LosFn(|f: [f32; 3], t: [f32; 3]| !blocked(f, t));
    let reach = RADAR_RANGE_M + board.lit_bonus_m;
    for tick in 0..30u64 {
        let units = battle.units();
        let enemies = battle.enemies();
        scratch.reset_stats();
        board.refresh_with(&mut scratch, los, tick, &units, &enemies, &params);
        let stats = scratch.stats();
        // Density bound: each query reads at most the team's units in its box of whole cells (padded by
        // one cell a side), at the field's density, with 2x slack for clustering of a uniform draw.
        let density_per_team = units.len() as f32 / TEAMS as f32 / (side * side);
        let box_side = |r: f32| 2.0 * r + 2.0 * settings.cell_size_m + 2.0 * settings.cell_size_m;
        let per_query_max = density_per_team * box_side(reach) * box_side(reach);
        let bound = 2.0 * stats.queries as f32 * per_query_max;
        assert!(
            (stats.candidates as f32) <= bound,
            "tick {tick}: {} candidates over {} queries, bound {bound}",
            stats.candidates,
            stats.queries
        );
        // And far below what the linear scan reads.
        assert!(
            stats.candidates * 4 < stats.scan_pairs,
            "tick {tick}: {} candidates vs {} all-pairs reads",
            stats.candidates,
            stats.scan_pairs
        );
        battle.step(0);
    }
}

#[test]
fn views_borrow_and_allocate_nothing_after_warmup() {
    const TEAMS: usize = 4;
    let battle = Battle::new(0x000A_110C, 900, TEAMS as u8, 2000.0);
    let units = battle.units();
    let enemies = battle.enemies();
    let params = mixed_params(TEAMS);
    let mut board = SightingsBoard::new(TEAMS, RADAR_RANGE_M);
    let mut scratch = PerceptionIndex::new(PerceptionIndexSettings::default());
    let mut rosters = TeamRosters::new(RosterSettings::default());
    let los = LosFn(|f: [f32; 3], t: [f32; 3]| !blocked(f, t));
    let mut enemy_slots: Vec<u32> = Vec::with_capacity(units.len());
    // Warm up: every phase of every team's re-check cadence, so every board row exists.
    for tick in 0..120 {
        rosters
            .build(&units, |u: &PerceptUnit| Some(u.team))
            .expect("teams fit the default limit");
        board.refresh_with(&mut scratch, los, tick, &units, &enemies, &params);
    }
    let (count, checksum) = allocations(|| {
        let mut checksum = 0u64;
        for tick in 120..180 {
            rosters
                .build(&units, |u: &PerceptUnit| Some(u.team))
                .expect("teams fit the default limit");
            board.refresh_with(&mut scratch, los, tick, &units, &enemies, &params);
            let world = AiTickWorld::new(tick, &rosters, &board);
            for (slot, u) in units.iter().enumerate() {
                let view = world.view(AiUnitFacts {
                    entity: u.id,
                    slot: slot as u32,
                    team: u.team,
                    pos: u.pos,
                });
                checksum = checksum.wrapping_add(view.allies().len() as u64);
                checksum = checksum.wrapping_add(view.known_enemies().len() as u64);
                if let Some(first) = view.known_enemies().first() {
                    let found = view.known_enemy(first.entity).map(|k| k.entity);
                    checksum = checksum.wrapping_add(u64::from(found.unwrap_or(0)));
                }
                view.enemy_slots_into(&mut enemy_slots);
                checksum = checksum.wrapping_add(enemy_slots.len() as u64);
                checksum = checksum.wrapping_add(u64::from(view.visible_threat().to_bits()));
            }
        }
        checksum
    });
    assert_eq!(count, 0, "views or the indexed refresh allocated after warm-up");
    assert_ne!(checksum, 0);

    // The views borrow the shared rows: same memory, not copies.
    let world = AiTickWorld::new(180, &rosters, &board);
    for team in 0..TEAMS as u8 {
        let unit = units
            .iter()
            .position(|u| u.team == team)
            .expect("every team has a unit");
        let view = world.view(AiUnitFacts {
            entity: units[unit].id,
            slot: unit as u32,
            team,
            pos: units[unit].pos,
        });
        assert!(std::ptr::eq(view.allies(), rosters.team(team)));
        assert!(std::ptr::eq(view.known_enemies(), board.for_team(team)));
        assert!(!view.known_enemies().is_empty());
        assert_eq!(
            view.visible_threat().to_bits(),
            board.visible_threat(team).to_bits()
        );
        let mut expect = Vec::new();
        rosters.others_into(team, &mut expect);
        view.enemy_slots_into(&mut enemy_slots);
        assert_eq!(enemy_slots, expect);
    }
    // A view is a handful of words whatever the world's size.
    assert!(std::mem::size_of::<regia_detsim::ai::AiWorldView<'_>>() <= 96);
}

#[test]
fn counter_sees_a_planted_allocation() {
    let (count, values) = allocations(|| vec![1_u8; 16]);
    assert_eq!(count, 1);
    assert_eq!(values.len(), 16);
}
