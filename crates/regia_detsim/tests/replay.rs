// SPDX-License-Identifier: MPL-2.0
//! Input logs and offline replay (DC-3.7b, DC-3.5b).
//!
//! - `replay_of_a_recorded_run_matches_every_tick`: the S-18 AI arena and a crowd soak (ids,
//!   grid broadphase, seats joining and leaving, 2,000 ticks) are recorded live through
//!   `replay::step_recorded`; replaying the encoded log from the snapshot recomputes the recorded
//!   hash at every tick, from the start and from a mid-run checkpoint.
//! - `a_flipped_input_is_named_at_its_tick_and_system`: one input changed in a recorded log is
//!   reported at its tick, with the system it moved, and the tick before as the last that agreed;
//!   also through the bytes-in `DesyncReplay` hook.
//! - `catch_up_reaches_the_live_tick_in_bounded_calls`: a late joiner restored from a snapshot
//!   catches a live host that keeps stepping, never more than its budget per call, in the number
//!   of calls the budget allows, and ends on the live hash.
//! - `input_log_scenario_is_a_golden`: a recorded log plus its snapshot is a golden scenario; the
//!   engine's `input_log_arena` matches its committed golden (recorded on x86_64-linux; the
//!   aarch64 legs run through `cargo xtask det-golden`) and its committed log matches its recipe.
//! - `input_log_reader_refuses_bad_input`: every refusal has its code, and 20,000 seeded mutations
//!   never panic and stay under an allocation bound set by the input length (counting allocator).
//!
//! `REGIA_BLESS=1` rewrites `goldens/input_log_arena.detlog` from its recipe (only when the change
//! in bits is intended; say why in the commit message).

#![allow(unsafe_code)]
// clippy 1.99 reports `missing_const_for_thread_local` on aarch64-linux-android only, at the
// `thread_local!` below whose initializers are already `const { }` (the false positive
// `hash_alloc.rs` and `det_state_reader.rs` record, ADR-8047). `expect` fails the build once it stops.
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use regia_detsim::arena::{ArenaOrder, ArenaScenario, ArenaSettings};
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase, FloorRule};
use regia_detsim::float::{det_clamp, det_max_f64};
use regia_detsim::golden::scenarios::{self, INPUT_LOG_ARENA, INPUT_LOG_ARENA_LOG, SplitMix64};
use regia_detsim::golden::{
    self, DetRegistry, DetRun, DetScenario, GoldenMode, InputLogScenario, REPORT_PREFIX, TickRate,
};
use regia_detsim::hash::{Mix, SystemFold, Systems};
use regia_detsim::ids::DetIdAllocator;
use regia_detsim::replay::{
    self, CatchUp, DesyncReplayRegistry, DetInputSim, INPUT_LOG_TAG, INPUT_LOG_VERSION, InputLog,
    MAX_SEATS_PER_TICK, ReplayHook, TickInputs,
};
use regia_detsim::state::{
    self, DetReadLimits, DetReader, DetState, DetStateError, DetWriter, HEADER_LEN, SnapshotKind,
    SnapshotRing, SnapshotRingSettings,
};
use regia_detsim::{math, tick};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------------
// Counting allocator (the pattern of det_state_reader.rs): this thread's live bytes and peak.
// ---------------------------------------------------------------------------------------------

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn note(delta: i64) {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    if !ARMED.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

// SAFETY: forwards every call to the system allocator unchanged; the only addition is thread-local
// counter updates, which neither allocate nor touch the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as i64);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as i64);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size as i64 - layout.size() as i64);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(-(layout.size() as i64));
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Peak live bytes on this thread while `f` runs.
fn measured<R>(f: impl FnOnce() -> R) -> (i64, R) {
    LIVE.with(|c| c.set(0));
    PEAK.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (PEAK.with(Cell::get), out)
}

/// A fresh directory under the system temp dir: pid plus a counter, so parallel tests never share.
fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-replay-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// ---------------------------------------------------------------------------------------------
// The crowd soak: ids, the grid broadphase and seats that join, steer, spawn and despawn.
// ---------------------------------------------------------------------------------------------

/// One seat's input to the crowd soak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CrowdInput {
    /// Push the seat's agents by `steer / 16` m/s per tick on x and z.
    steer: [i8; 2],
    /// Agents to spawn at the seat's corner (at most 3 a tick).
    spawn: u8,
    /// Free the seat's oldest agent.
    despawn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Agent {
    id: u16,
    owner: u16,
    body: BroadBody,
    vel: [f32; 2],
}

/// The crowd: agents kept in id order (the walk order), a grid that is a cache, contact count.
struct CrowdSoak {
    ids: DetIdAllocator<u16>,
    agents: Vec<Agent>,
    config: BroadphaseConfig,
    grid: DetBroadphase,
    contacts: u64,
    tick: u64,
}

impl CrowdSoak {
    fn new() -> Self {
        let config = BroadphaseConfig {
            cell_size: 1.5,
            floor_rule: FloorRule::Bands,
            ..BroadphaseConfig::default()
        };
        CrowdSoak {
            ids: DetIdAllocator::new(1_000),
            agents: Vec::new(),
            config,
            grid: DetBroadphase::new(config),
            contacts: 0,
            tick: 0,
        }
    }

    fn corner(seat: u16) -> [f32; 2] {
        let x = if seat & 1 == 0 { -18.0 } else { 18.0 };
        let z = if seat & 2 == 0 { -18.0 } else { 18.0 };
        [x, z]
    }
}

impl DetState for CrowdSoak {
    const STATE_TAG: [u8; 8] = *b"crowdsk\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        w.state(&self.ids);
        w.seq(&self.agents, |w, a| {
            w.u16(a.id);
            w.u16(a.owner);
            w.state(&a.body);
            w.f32(a.vel[0]);
            w.f32(a.vel[1]);
        });
        w.state(&self.config);
        w.u64(self.contacts);
        w.u64(self.tick);
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        let ids: DetIdAllocator<u16> = r.state()?;
        let agents = r.seq(|r| {
            Ok(Agent {
                id: r.u16()?,
                owner: r.u16()?,
                body: r.state()?,
                vel: [r.f32()?, r.f32()?],
            })
        })?;
        if agents.windows(2).any(|w| w[0].id >= w[1].id) || agents.iter().any(|a| !ids.is_live(a.id)) {
            return Err(DetStateError::bad_value(
                "crowd agents out of id order or not live",
            ));
        }
        let config: BroadphaseConfig = r.state()?;
        Ok(CrowdSoak {
            ids,
            agents,
            config,
            grid: DetBroadphase::new(config),
            contacts: r.u64()?,
            tick: r.u64()?,
        })
    }
}

impl SystemFold<3> for CrowdSoak {
    const SYSTEMS: Systems<3> = Systems::new(["bodies", "contacts", "ids"]);
    fn fold_systems(&self, mix: &mut [Mix; 3]) {
        let [bodies, contacts, ids] = mix;
        for a in &self.agents {
            bodies.f32(a.body.pos[0]);
            bodies.f32(a.body.pos[2]);
            bodies.f32(a.vel[0]);
            bodies.f32(a.vel[1]);
            ids.u64(u64::from(a.id));
            ids.u64(u64::from(a.owner));
            ids.u64(u64::from(self.ids.generation(a.id)));
        }
        contacts.u64(self.contacts);
        ids.u64(self.ids.live_count() as u64);
        ids.u64(self.ids.free_count() as u64);
        ids.u64(self.tick);
    }
}

impl DetInputSim<3> for CrowdSoak {
    type Input = CrowdInput;

    fn step(&mut self, tick: u64, inputs: &TickInputs<CrowdInput>) {
        let dt = tick::tick_dt();
        // Seats in ascending order: despawn, spawn, steer.
        for entry in inputs.seats() {
            let Some(input) = entry.input else { continue };
            let seat = entry.seat;
            if input.despawn
                && let Some(i) = self.agents.iter().position(|a| a.owner == seat)
            {
                let gone = self.agents.remove(i);
                self.ids.free(gone.id);
            }
            for k in 0..input.spawn.min(3) {
                let Some(id) = self.ids.alloc() else { break };
                let [x, z] = Self::corner(seat);
                let jitter = (k as f32 - 1.0) * 0.7;
                let agent = Agent {
                    id,
                    owner: seat,
                    body: BroadBody::new([x + jitter, 0.0, z - jitter], 0.45, [0.0, 1.8]),
                    vel: [0.0, 0.0],
                };
                let at = self.agents.partition_point(|a| a.id < id);
                self.agents.insert(at, agent);
            }
            let push = [f32::from(input.steer[0]) / 16.0, f32::from(input.steer[1]) / 16.0];
            for a in self.agents.iter_mut().filter(|a| a.owner == seat) {
                a.vel[0] = det_clamp(a.vel[0] + push[0] * dt, -4.0, 4.0);
                a.vel[1] = det_clamp(a.vel[1] + push[1] * dt, -4.0, 4.0);
            }
        }
        for a in &mut self.agents {
            a.body.pos[0] = det_clamp(a.body.pos[0] + a.vel[0] * dt, -20.0, 20.0);
            a.body.pos[2] = det_clamp(a.body.pos[2] + a.vel[1] * dt, -20.0, 20.0);
        }
        let mut bodies: Vec<BroadBody> = self.agents.iter().map(|a| a.body).collect();
        let mut contacts = 0u64;
        self.grid.for_each_contact(&mut bodies, |bodies, c| {
            contacts += 1;
            let dist = math::sqrt(c.dist_sq);
            let push = if dist > 1.0e-6 {
                (c.min_dist - dist) * 0.5 / dist
            } else {
                0.0
            };
            bodies[c.a].pos[0] -= c.dx * push;
            bodies[c.a].pos[2] -= c.dz * push;
            bodies[c.b].pos[0] += c.dx * push;
            bodies[c.b].pos[2] += c.dz * push;
        });
        for (a, b) in self.agents.iter_mut().zip(bodies) {
            a.body = b;
        }
        self.contacts += contacts;
        self.tick = tick + 1;
    }
}

/// Seeded crowd inputs for `tick`: four seats; seat 3 joins at tick 300 and seat 1 leaves at 1,400
/// (not listed outside those spans); each listed seat has an input 9 times in 10.
fn crowd_inputs(rng: &mut SplitMix64, tick: u64) -> TickInputs<CrowdInput> {
    let mut inputs = TickInputs::new(tick);
    for seat in 0..4u16 {
        if (seat == 3 && tick < 300) || (seat == 1 && tick >= 1_400) {
            continue;
        }
        let roll = rng.next_u64();
        let input = (!roll.is_multiple_of(10)).then(|| CrowdInput {
            steer: [(roll >> 8) as i8 % 24, (roll >> 16) as i8 % 24],
            spawn: if (roll >> 24).is_multiple_of(23) {
                1 + ((roll >> 32) % 3) as u8
            } else {
                0
            },
            despawn: (roll >> 40).is_multiple_of(31),
        });
        inputs.push(seat, input).expect("seats ascend");
    }
    inputs
}

/// Seeded arena orders for `tick`: both seats, each present 7 times in 8.
fn arena_inputs(rng: &mut SplitMix64, tick: u64) -> TickInputs<ArenaOrder> {
    let mut inputs = TickInputs::new(tick);
    for seat in 0..2u16 {
        let roll = rng.next_u64();
        let order = (!roll.is_multiple_of(8)).then_some(ArenaOrder {
            hold: (roll >> 8).is_multiple_of(6),
            sprint: (roll >> 16).is_multiple_of(4),
        });
        inputs.push(seat, order).expect("seats ascend");
    }
    inputs
}

fn arena() -> ArenaScenario {
    let mut sim = ArenaScenario::new(ArenaSettings {
        units_per_team: 5,
        ..ArenaSettings::default()
    })
    .expect("settings");
    DetScenario::reset(&mut sim, &DetRun::new(21, 1, TickRate::DEFAULT));
    sim
}

/// A live recording: the log (snapshot at `start`, hashes every tick), the world hash after each
/// tick, and the checkpoints the host kept.
struct Recording<I> {
    log: InputLog<I>,
    live: Vec<u64>,
    ring: SnapshotRing,
}

fn record<S: DetInputSim<N>, const N: usize>(
    mut sim: S,
    start: u64,
    ticks: u64,
    mut inputs: impl FnMut(u64) -> TickInputs<S::Input>,
) -> (S, Recording<S::Input>) {
    let mut log = InputLog::for_sim::<S, N>(start);
    log.set_start_snapshot(state::snapshot(&sim, start).expect("snapshot"))
        .expect("start snapshot");
    let mut ring = SnapshotRing::new(SnapshotRingSettings {
        capacity_ticks: 10_000,
        every: 250,
        max_bytes: 64 << 20,
    })
    .expect("ring");
    let mut live = Vec::new();
    for t in start..start + ticks {
        let h = replay::step_recorded(&mut sim, inputs(t), &mut log, true).expect("record");
        live.push(h.world);
        ring.record(t + 1, &sim).expect("checkpoint");
    }
    (sim, Recording { log, live, ring })
}

#[test]
fn replay_of_a_recorded_run_matches_every_tick() {
    // The S-18 arena, from a snapshot taken at tick 30 of a seed-only start.
    let mut sim = arena();
    for t in 0..30u64 {
        replay::advance(&mut sim, &TickInputs::new(t));
    }
    let mut rng = SplitMix64::new(0xa7e4a);
    let (live_arena, rec) = record::<ArenaScenario, 3>(sim, 30, 900, |t| arena_inputs(&mut rng, t));
    let bytes = rec.log.encode().expect("encode");
    let report =
        replay::replay_bytes::<ArenaScenario, 3>(&bytes, None, None, DetReadLimits::DEFAULT).expect("replay");
    assert!(report.passed(), "{}", report.verdict_line());
    assert_eq!((report.start_tick, report.end_tick), (30, 930));
    assert_eq!((report.ticks_run, report.hashes_compared), (900, 900));
    assert_eq!(report.last_agreed, Some(929));
    assert_eq!(report.final_world, rec.live.last().copied());
    assert_eq!(
        report.final_world,
        Some(<ArenaScenario as DetInputSim<3>>::hash(&live_arena))
    );
    let stats = live_arena.stats();
    assert!(stats.attacks > 0, "the run reached combat: {stats:?}");

    // The same log stepped tick by tick from the snapshot gives the live hash at every tick.
    let decoded = InputLog::<ArenaOrder>::decode(&bytes).expect("decode");
    assert_eq!(decoded, rec.log, "the log round-trips");
    let (_, mut again) =
        state::restore::<ArenaScenario>(decoded.start_snapshot().expect("carried")).expect("restore");
    for (entry, want) in decoded.entries().iter().zip(&rec.live) {
        assert_eq!(
            replay::advance(&mut again, &entry.inputs).world,
            *want,
            "tick {}",
            entry.inputs.tick()
        );
    }

    // Partial replay up to a boundary.
    let report = replay::replay_bytes::<ArenaScenario, 3>(&bytes, None, Some(400), DetReadLimits::DEFAULT)
        .expect("replay");
    assert!(report.passed());
    assert_eq!(
        (report.ticks_run, report.end_tick, report.last_agreed),
        (370, 400, Some(399))
    );

    // The crowd soak: 2,000 ticks, four seats joining and leaving, spawns and despawns.
    let mut rng = SplitMix64::new(0xc20d_50a4);
    let (live_crowd, rec) = record::<CrowdSoak, 3>(CrowdSoak::new(), 0, 2_000, |t| crowd_inputs(&mut rng, t));
    assert!(
        live_crowd.contacts > 1_000,
        "the crowd collides: {}",
        live_crowd.contacts
    );
    assert!(live_crowd.ids.free_count() > 0 || live_crowd.ids.live_count() > 0);
    let bytes = rec.log.encode().expect("encode");
    let report =
        replay::replay_bytes::<CrowdSoak, 3>(&bytes, None, None, DetReadLimits::DEFAULT).expect("replay");
    assert!(report.passed(), "{}", report.verdict_line());
    assert_eq!((report.ticks_run, report.hashes_compared), (2_000, 2_000));
    assert_eq!(
        report.final_world,
        Some(<CrowdSoak as DetInputSim<3>>::hash(&live_crowd))
    );

    // From a mid-run checkpoint: the bundle shape (newest checkpoint at or before a tick, plus the
    // log from there).
    let (at, checkpoint) = rec.ring.latest_at_or_before(1_234).expect("checkpoint");
    assert_eq!(at, 1_000);
    let tail = rec.log.suffix(at).expect("suffix");
    assert_eq!((tail.start_tick(), tail.len()), (1_000, 1_000));
    let report =
        replay::replay_from_snapshot::<CrowdSoak, 3>(Some(checkpoint), &tail, None, DetReadLimits::DEFAULT)
            .expect("replay");
    assert!(report.passed(), "{}", report.verdict_line());
    assert_eq!(
        (report.start_tick, report.end_tick, report.hashes_compared),
        (1_000, 2_000, 1_000)
    );
    assert_eq!(report.final_world, rec.live.last().copied());
    println!("{}", report.verdict_line());
}

#[test]
fn a_flipped_input_is_named_at_its_tick_and_system() {
    // Arena: seat 0 holds team 0 at tick 45 instead of moving. Early in the run every unit is far
    // from the enemy and moving, so only positions ("units") move at that tick.
    let mut sim = arena();
    for t in 0..20u64 {
        replay::advance(&mut sim, &TickInputs::new(t));
    }
    let (_, rec) = record::<ArenaScenario, 3>(sim, 20, 200, |t| {
        TickInputs::from_sorted(t, [(0, None), (1, None)]).expect("seats")
    });
    let mut flipped = rec.log.clone();
    let entry = flipped.entry_mut(45).expect("tick 45");
    assert!(entry.inputs.replace(
        0,
        Some(ArenaOrder {
            hold: true,
            sprint: false
        })
    ));
    let report =
        replay::replay_from_snapshot::<ArenaScenario, 3>(None, &flipped, None, DetReadLimits::DEFAULT)
            .expect("replay");
    assert_eq!(report.first_diverging_tick, Some(45), "{}", report.verdict_line());
    assert_eq!(report.system, Some("units"));
    assert_eq!(report.last_agreed, Some(44));
    assert_eq!(
        (report.ticks_run, report.end_tick),
        (26, 46),
        "the replay stops at the split"
    );
    assert_eq!(
        report.verdict_line(),
        "replay verdict diverged start 20 end 46 ticks 26 compared 26 first 45 system units last_agreed 44"
    );
    // Sprinting instead names the same system at the same tick.
    let mut sprint = rec.log.clone();
    sprint.entry_mut(45).expect("tick 45").inputs.replace(
        1,
        Some(ArenaOrder {
            hold: false,
            sprint: true,
        }),
    );
    let report =
        replay::replay_from_snapshot::<ArenaScenario, 3>(None, &sprint, None, DetReadLimits::DEFAULT)
            .expect("replay");
    assert_eq!(
        (report.first_diverging_tick, report.system),
        (Some(45), Some("units"))
    );

    // Crowd: an extra spawn at tick 777 changes ids (allocator, owners) and bodies; `bodies` comes
    // first in the system order, and both are listed.
    let mut rng = SplitMix64::new(0x5eed);
    let (_, rec) = record::<CrowdSoak, 3>(CrowdSoak::new(), 0, 1_000, |t| crowd_inputs(&mut rng, t));
    let mut flipped = rec.log.clone();
    let entry = flipped.entry_mut(777).expect("tick 777");
    // The same input with a different spawn count (whatever seat 2 sent there).
    let sent = entry.inputs.get(2).copied().unwrap_or(CrowdInput {
        steer: [0, 0],
        spawn: 0,
        despawn: false,
    });
    let more = CrowdInput {
        spawn: if sent.spawn == 3 { 1 } else { 3 },
        ..sent
    };
    assert!(entry.inputs.replace(2, Some(more)));
    let bytes = flipped.encode().expect("encode");
    let mut hooks = DesyncReplayRegistry::new();
    hooks
        .register(Box::new(ReplayHook::<CrowdSoak, 3>::new()))
        .expect("register");
    hooks
        .register(Box::new(ReplayHook::<ArenaScenario, 3>::new()))
        .expect("register");
    assert_eq!(
        hooks
            .register(Box::new(ReplayHook::<CrowdSoak, 3>::new()))
            .err()
            .map(|e| e.code()),
        Some("duplicate_sim")
    );
    assert_eq!(hooks.tags(), vec![ArenaScenario::STATE_TAG, CrowdSoak::STATE_TAG]);
    let hook = hooks.get(CrowdSoak::STATE_TAG).expect("hook");
    assert_eq!(hook.system_names(), ["bodies", "contacts", "ids"]);
    let report = hooks
        .replay(None, &bytes, None, DetReadLimits::DEFAULT)
        .expect("replay");
    assert_eq!(
        report.first_diverging_tick,
        Some(777),
        "{}",
        report.verdict_line()
    );
    assert_eq!(report.system, Some("bodies"));
    let named: Vec<&str> = report.systems.iter().map(|d| d.system).collect();
    assert!(named.contains(&"ids"), "{named:?}");
    assert!(report.systems.iter().all(|d| d.first_tick == 777));
    assert_eq!(report.last_agreed, Some(776));

    // Only the world hash recorded (a peer that kept its wire checksum only): the tick is named,
    // the system is not.
    let mut world_only = flipped.clone();
    for t in 0..1_000 {
        world_only.entry_mut(t).expect("tick").hash.systems = None;
    }
    let report =
        replay::replay_from_snapshot::<CrowdSoak, 3>(None, &world_only, None, DetReadLimits::DEFAULT)
            .expect("replay");
    assert_eq!((report.first_diverging_tick, report.system), (Some(777), None));

    // Sparse hashes: the split is found at the first recorded tick at or after the flip.
    let mut sparse = flipped.clone();
    for t in 0..1_000 {
        if t % 50 != 49 {
            sparse.entry_mut(t).expect("tick").hash = Default::default();
        }
    }
    let report = replay::replay_from_snapshot::<CrowdSoak, 3>(None, &sparse, None, DetReadLimits::DEFAULT)
        .expect("replay");
    assert_eq!(
        (report.first_diverging_tick, report.last_agreed),
        (Some(799), Some(749))
    );
    assert_eq!(report.hashes_compared, 16);
}

#[test]
fn catch_up_reaches_the_live_tick_in_bounded_calls() {
    let budget = 5u32;
    let mut rng = SplitMix64::new(0xca7_c40b);
    let mut host = CrowdSoak::new();
    let mut frames: Vec<TickInputs<CrowdInput>> = Vec::new();
    // The host runs 600 ticks before the seat joins, keeping a checkpoint every 100.
    let mut ring = SnapshotRing::new(SnapshotRingSettings {
        capacity_ticks: 1_000,
        every: 100,
        max_bytes: 16 << 20,
    })
    .expect("ring");
    for t in 0..600u64 {
        let inputs = crowd_inputs(&mut rng, t);
        replay::advance(&mut host, &inputs);
        frames.push(inputs);
        ring.record(t + 1, &host).expect("checkpoint");
    }
    // The joiner gets a checkpoint one hundred ticks old (the newest is at 600) and the frames since.
    assert_eq!(ring.latest_at_or_before(600).map(|(t, _)| t), Some(600));
    let (k, bytes) = ring
        .latest_at_or_before(599)
        .map(|(t, b)| (t, b.to_vec()))
        .expect("older");
    assert_eq!(k, 500, "a checkpoint one hundred ticks behind");
    let (tick, mut joiner) = state::restore::<CrowdSoak>(&bytes).expect("restore");
    let mut queue = CatchUp::new(tick);
    for f in &frames[k as usize..] {
        queue.push(f.clone()).expect("in order");
    }
    // A frame out of order is refused and nothing is queued.
    assert_eq!(
        queue
            .push(crowd_inputs(&mut SplitMix64::new(1), 9_999))
            .err()
            .map(|e| e.code()),
        Some("tick_gap")
    );
    assert_eq!(queue.pending(), 100);
    // Budget 0 steps nothing.
    let none = replay::catch_up(&mut joiner, &mut queue, 0);
    assert_eq!((none.stepped, none.tick, none.pending), (0, 500, 100));

    // Each frame the host steps one tick and sends it; the joiner steps at most `budget`.
    let mut calls = 0u32;
    let mut live_tick = 600u64;
    loop {
        let inputs = crowd_inputs(&mut rng, live_tick);
        replay::advance(&mut host, &inputs);
        queue.push(inputs).expect("in order");
        live_tick += 1;
        let step = replay::catch_up(&mut joiner, &mut queue, budget);
        calls += 1;
        assert!(step.stepped <= budget, "a call stepped {} ticks", step.stepped);
        assert_eq!(step.tick, queue.tick());
        if step.caught_up() {
            assert_eq!(step.tick, live_tick);
            break;
        }
        assert!(calls <= 1_000, "never caught up");
    }
    // 100 ticks behind, gaining `budget - 1` a call (the host adds one): at most ceil(100 / 4).
    let bound = 100u32.div_ceil(budget - 1);
    assert!(calls <= bound, "{calls} calls, bound {bound}");
    assert_eq!(calls, 25);
    assert_eq!(
        <CrowdSoak as DetInputSim<3>>::hash(&joiner),
        <CrowdSoak as DetInputSim<3>>::hash(&host)
    );
    assert_eq!(
        state::to_bytes(&joiner).expect("save"),
        state::to_bytes(&host).expect("save")
    );
    assert_eq!(queue.calls(), u64::from(calls) + 1);
    assert_eq!(queue.stepped(), 100 + u64::from(calls));
}

#[test]
fn input_log_scenario_is_a_golden() {
    // The committed log is its recipe's output (REGIA_BLESS=1 rewrites it).
    let recipe = scenarios::record_input_log_arena().expect("recipe");
    if GoldenMode::from_env() == GoldenMode::Bless {
        let path = golden::engine_golden_dir().join(format!("{INPUT_LOG_ARENA}.detlog"));
        let tmp = path.with_extension("detlog.tmp");
        std::fs::write(&tmp, &recipe).expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
    } else {
        assert!(
            recipe == INPUT_LOG_ARENA_LOG,
            "goldens/{INPUT_LOG_ARENA}.detlog differs from its recipe ({} vs {} bytes)",
            INPUT_LOG_ARENA_LOG.len(),
            recipe.len()
        );
    }
    let header = replay::read_input_log_header(INPUT_LOG_ARENA_LOG, DetReadLimits::DEFAULT).expect("header");
    assert_eq!(header.sim_tag, ArenaScenario::STATE_TAG);
    assert_eq!(
        (header.start_tick, header.system_count, header.has_snapshot),
        (40, 3, true)
    );
    // It replays clean against its own recorded hashes.
    let report =
        replay::replay_bytes::<ArenaScenario, 3>(INPUT_LOG_ARENA_LOG, None, None, DetReadLimits::DEFAULT)
            .expect("replay");
    assert!(report.passed(), "{}", report.verdict_line());
    assert_eq!((report.ticks_run, report.hashes_compared), (600, 30));

    // Registered with the engine scenarios, checked against its committed golden.
    let registry = DetRegistry::with_engine_scenarios();
    let entry = registry.get(INPUT_LOG_ARENA).expect("registered");
    let mut scenario = entry.create();
    assert_eq!(scenario.default_run(), DetRun::new(0, 600, TickRate::DEFAULT));
    let report = golden::check_golden(scenario.as_mut(), entry.golden_dir(), GoldenMode::from_env());
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");

    // The golden trace is the replay's world and system hashes: a game's own recorded run becomes
    // a golden the same way, in its own directory.
    let dir = unique_temp_dir("golden");
    let mut rng = SplitMix64::new(77);
    let mut start = CrowdSoak::new();
    for t in 0..100u64 {
        replay::advance(&mut start, &crowd_inputs(&mut rng, t));
    }
    let (_, rec) = record::<CrowdSoak, 3>(start, 100, 400, |t| crowd_inputs(&mut rng, t));
    let log_bytes = rec.log.encode().expect("encode");
    let mut games = DetRegistry::new();
    let game_bytes = log_bytes.clone();
    games
        .register(dir.clone(), move || {
            Box::new(
                InputLogScenario::<CrowdSoak, 3>::from_bytes(
                    "crowd_recorded",
                    None,
                    &game_bytes,
                    DetReadLimits::DEFAULT,
                )
                .expect("scenario"),
            ) as Box<dyn DetScenario>
        })
        .expect("register");
    let entry = games.get("crowd_recorded").expect("registered");
    let blessed = golden::check_golden(entry.create().as_mut(), &dir, GoldenMode::Bless);
    assert!(blessed.passed(), "{blessed}");
    let checked = golden::check_golden(entry.create().as_mut(), &dir, GoldenMode::Check);
    assert!(checked.passed(), "{checked}");

    // A flipped input in the log moves the golden at its tick, and the field names the system.
    let mut flipped = rec.log.clone();
    flipped.entry_mut(333).expect("tick").inputs.replace(
        0,
        Some(CrowdInput {
            steer: [0, 0],
            spawn: 2,
            despawn: false,
        }),
    );
    let mut clean =
        InputLogScenario::<CrowdSoak, 3>::new("crowd_recorded", None, rec.log.clone()).expect("clean");
    let mut moved = InputLogScenario::<CrowdSoak, 3>::new("crowd_recorded", None, flipped).expect("flipped");
    let run = clean.default_run();
    let divergence = golden::compare(&mut clean, &mut moved, &run, None)
        .expect("no injection")
        .expect("a flipped input diverges");
    assert_eq!(divergence.tick, 333 - 100);
    let fields: Vec<&str> = divergence.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields.first(), Some(&"world"), "{divergence}");
    assert!(
        fields.contains(&"bodies") && fields.contains(&"ids"),
        "{divergence}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The decode-and-replay outcome of `bytes` as a code (`"ok"` when it decodes and replays).
fn outcome(bytes: &[u8], limits: DetReadLimits) -> &'static str {
    match InputLog::<ArenaOrder>::decode_with(bytes, limits) {
        Ok(log) => match replay::replay_from_snapshot::<ArenaScenario, 3>(None, &log, None, limits) {
            Ok(_) => "ok",
            Err(e) => e.code(),
        },
        Err(e) => e.code(),
    }
}

/// A short arena log (snapshot carried, hashes every tick) for the reader tests.
fn small_log() -> InputLog<ArenaOrder> {
    let mut rng = SplitMix64::new(0x10e);
    record::<ArenaScenario, 3>(arena(), 0, 40, |t| arena_inputs(&mut rng, t))
        .1
        .log
}

/// An input log body written by hand: sim identity, no snapshot, then `entries`.
fn body(system_count: u32, entries: &[(&[(u16, u8)], u8)]) -> Vec<u8> {
    let mut w = DetWriter::new();
    w.raw(&ArenaScenario::STATE_TAG);
    w.u32(ArenaScenario::STATE_VERSION);
    w.u32(system_count);
    w.bytes(&[]);
    w.length(entries.len());
    for (seats, flags) in entries {
        w.length(seats.len());
        for &(seat, presence) in *seats {
            w.u16(seat);
            w.u8(presence);
            if presence == 1 {
                w.put(&ArenaOrder::default());
            }
        }
        w.u8(*flags);
    }
    w.into_bytes().expect("body")
}

fn framed(body: &[u8]) -> Vec<u8> {
    state::frame_bytes(SnapshotKind::InputLog, INPUT_LOG_TAG, INPUT_LOG_VERSION, 0, body).expect("frame")
}

fn mutate(rng: &mut SplitMix64, bytes: &mut Vec<u8>) {
    let len = bytes.len().max(1) as u64;
    match rng.next_u64() % 6 {
        0 => {
            let i = (rng.next_u64() % len) as usize;
            if let Some(b) = bytes.get_mut(i) {
                *b ^= 1 << (rng.next_u64() % 8);
            }
        }
        1 => {
            let i = (rng.next_u64() % len) as usize;
            if let Some(b) = bytes.get_mut(i) {
                *b = rng.next_u64() as u8;
            }
        }
        2 => {
            let cut = (rng.next_u64() % len) as usize;
            bytes.truncate(cut);
        }
        3 => {
            let i = (rng.next_u64() % len) as usize;
            let n = 1 + (rng.next_u64() % 8) as usize;
            for _ in 0..n {
                bytes.insert(i.min(bytes.len()), rng.next_u64() as u8);
            }
        }
        4 => {
            // A forged length: a large u32 at a random 4-byte position.
            let i = (rng.next_u64() % len) as usize;
            let forged = match rng.next_u64() % 3 {
                0 => u32::MAX,
                1 => 1 << 24,
                _ => (rng.next_u64() % 100_000) as u32,
            };
            for (k, b) in forged.to_le_bytes().into_iter().enumerate() {
                if let Some(slot) = bytes.get_mut(i + k) {
                    *slot = b;
                }
            }
        }
        _ => {
            let i = (rng.next_u64() % len) as usize;
            let j = (rng.next_u64() % len) as usize;
            if i < bytes.len() && j < bytes.len() {
                bytes.swap(i, j);
            }
        }
    }
}

#[test]
fn input_log_reader_refuses_bad_input() {
    let limits = DetReadLimits::DEFAULT;
    let log = small_log();
    let bytes = log.encode().expect("encode");
    assert_eq!(outcome(&bytes, limits), "ok");

    // The frame: truncated, trailing, damaged, foreign, wrong version, oversized.
    for cut in [0, 3, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + 1, bytes.len() - 1] {
        assert_eq!(outcome(&bytes[..cut], limits), "truncated", "cut at {cut}");
    }
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(outcome(&long, limits), "trailing_bytes");
    let mut damaged = bytes.clone();
    damaged[HEADER_LEN + 20] ^= 4;
    assert_eq!(outcome(&damaged, limits), "bad_hash");
    let snapshot = log.start_snapshot().expect("carried").to_vec();
    assert_eq!(
        outcome(&snapshot, limits),
        "bad_value",
        "a state snapshot is not an input log"
    );
    let other_tag = state::frame_bytes(
        SnapshotKind::InputLog,
        *b"inputlg2",
        INPUT_LOG_VERSION,
        0,
        &bytes[HEADER_LEN..],
    )
    .expect("frame");
    assert_eq!(outcome(&other_tag, limits), "bad_tag");
    let v2 = state::frame_bytes(
        SnapshotKind::InputLog,
        INPUT_LOG_TAG,
        INPUT_LOG_VERSION + 1,
        0,
        &bytes[HEADER_LEN..],
    )
    .expect("frame");
    assert_eq!(outcome(&v2, limits), "bad_version");
    let tight = DetReadLimits {
        max_bytes: bytes.len() - 1,
        ..limits
    };
    assert_eq!(outcome(&bytes, tight), "length_limit");
    assert_eq!(
        outcome(&bytes, DetReadLimits { max_len: 2, ..limits }),
        "length_limit"
    );

    // The body.
    assert_eq!(
        outcome(&framed(&body(3, &[(&[(0, 1), (1, 0)], 0)])), limits),
        "no_snapshot"
    );
    assert_eq!(
        outcome(&framed(&body(3, &[(&[(1, 0), (0, 0)], 0)])), limits),
        "seat_order"
    );
    assert_eq!(
        outcome(&framed(&body(3, &[(&[(4, 0), (4, 0)], 0)])), limits),
        "seat_order"
    );
    assert_eq!(
        outcome(&framed(&body(3, &[(&[(0, 2)], 0)])), limits),
        "bad_value",
        "presence byte 2"
    );
    assert_eq!(
        outcome(&framed(&body(3, &[(&[], 0x80)])), limits),
        "bad_value",
        "unknown hash flag"
    );
    assert_eq!(
        outcome(&framed(&body(65, &[])), limits),
        "bad_value",
        "more than 64 systems"
    );
    assert_eq!(
        outcome(&framed(&body(3, &[(&[], 2)])), limits),
        "truncated",
        "system words missing"
    );
    let mut many = DetWriter::new();
    many.raw(&ArenaScenario::STATE_TAG);
    many.u32(1);
    many.u32(3);
    many.bytes(&[]);
    many.length(1);
    many.length(MAX_SEATS_PER_TICK + 1);
    assert_eq!(
        outcome(&framed(&many.into_bytes().expect("body")), limits),
        "length_limit",
        "too many seats"
    );
    // A carried snapshot of another tick or another sim.
    let mut wrong_tick = InputLog::for_sim::<ArenaScenario, 3>(5);
    assert_eq!(
        wrong_tick
            .set_start_snapshot(snapshot.clone())
            .err()
            .map(|e| e.code()),
        Some("tick_gap")
    );
    let crowd = state::snapshot(&CrowdSoak::new(), 0).expect("snapshot");
    assert_eq!(
        InputLog::for_sim::<ArenaScenario, 3>(0)
            .set_start_snapshot(crowd)
            .err()
            .map(|e| e.code()),
        Some("bad_tag")
    );

    // Replay refusals.
    assert_eq!(
        replay::replay_bytes::<CrowdSoak, 3>(&bytes, None, None, limits)
            .err()
            .map(|e| e.code()),
        Some("wrong_sim")
    );
    let mut sim = arena();
    assert_eq!(
        replay::replay(&mut sim, &log, Some(41)).err().map(|e| e.code()),
        Some("bad_until")
    );
    assert_eq!(log.suffix(41).err().map(|e| e.code()), Some("bad_until"));
    let mut gap = InputLog::for_sim::<ArenaScenario, 3>(0);
    assert_eq!(
        gap.push(replay::LogEntry {
            inputs: TickInputs::new(1),
            hash: Default::default()
        })
        .err()
        .map(|e| e.code()),
        Some("tick_gap")
    );
    assert_eq!(
        gap.push(replay::LogEntry {
            inputs: TickInputs::new(0),
            hash: replay::RecordedHash {
                world: None,
                systems: Some(vec![1, 2])
            }
        })
        .err()
        .map(|e| e.code()),
        Some("system_count")
    );
    let unknown = DesyncReplayRegistry::new().replay(None, &bytes, None, limits);
    assert_eq!(unknown.err().map(|e| e.code()), Some("unknown_sim"));
    let older = state::snapshot(&arena(), 3).expect("snapshot");
    assert_eq!(
        replay::replay_from_snapshot::<ArenaScenario, 3>(Some(&older), &log, None, limits)
            .err()
            .map(|e| e.code()),
        Some("tick_gap")
    );

    // 20,000 seeded mutations of real logs (the arena log above and the committed fixture): never a
    // panic; every decode's peak allocation stays under 64 bytes per input byte plus 64 KiB (the
    // entry vector preallocates at most 4 KiB and from then grows with decoded entries; an entry of
    // at least 5 input bytes is under 100 bytes decoded, doubled by growth).
    let corpus = [bytes, INPUT_LOG_ARENA_LOG.to_vec()];
    let mut rng = SplitMix64::new(0x1095_7a7e);
    let mut outcomes: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut worst = 0f64;
    for i in 0..20_000u32 {
        let original = &corpus[i as usize % corpus.len()];
        let reframed = !rng.next_u64().is_multiple_of(4);
        let bytes = if reframed {
            let mut body = original[HEADER_LEN..].to_vec();
            for _ in 0..1 + rng.next_u64() % 3 {
                mutate(&mut rng, &mut body);
            }
            let header = state::read_header(original, limits).expect("header");
            state::frame_bytes(header.kind, header.tag, header.version, header.tick, &body).expect("frame")
        } else {
            let mut bytes = original.clone();
            mutate(&mut rng, &mut bytes);
            bytes
        };
        let (peak, result) = measured(|| {
            catch_unwind(AssertUnwindSafe(|| {
                InputLog::<ArenaOrder>::decode_with(&bytes, limits)
            }))
        });
        let Ok(decoded) = result else {
            panic!("mutation {i} panicked in decode (reframed {reframed})");
        };
        let bound = 64 * bytes.len() as i64 + 64 * 1024;
        assert!(
            peak <= bound,
            "mutation {i} peaked at {peak} bytes for {} input bytes",
            bytes.len()
        );
        worst = det_max_f64(worst, peak as f64 / bytes.len().max(1) as f64);
        let code = match decoded {
            Err(e) => e.code(),
            // A log that decodes also replays without a panic (every eighth, to keep the run short).
            Ok(log) if i % 8 == 0 => {
                match catch_unwind(AssertUnwindSafe(|| {
                    replay::replay_from_snapshot::<ArenaScenario, 3>(None, &log, None, limits)
                })) {
                    Ok(Ok(r)) if r.passed() => "ok",
                    Ok(Ok(_)) => "diverged",
                    Ok(Err(e)) => e.code(),
                    Err(_) => panic!("mutation {i} panicked in replay"),
                }
            }
            Ok(_) => "decoded",
        };
        *outcomes.entry(code).or_default() += 1;
    }
    for expected in ["bad_hash", "truncated", "bad_value", "seat_order"] {
        assert!(
            outcomes.get(expected).copied().unwrap_or(0) > 0,
            "no {expected} outcome: {outcomes:?}"
        );
    }
    assert!(
        outcomes.get("decoded").copied().unwrap_or(0)
            + outcomes.get("ok").copied().unwrap_or(0)
            + outcomes.get("diverged").copied().unwrap_or(0)
            > 0,
        "no mutation decoded: {outcomes:?}"
    );
    println!("input-log mutation outcomes: {outcomes:?}; worst peak {worst:.1} bytes per input byte");
}
