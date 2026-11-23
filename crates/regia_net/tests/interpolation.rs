// SPDX-License-Identifier: MPL-2.0
//! Spec 74 § 13: snapshot interpolation, through the pure core with simulated arrival times and
//! end to end over the `LoopbackHub`. No wall clock: every arrival and frame time is computed.

use std::collections::BTreeMap;

use glam::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_net::interp::{
    InterpClock, InterpProfile, InterpQueryError, InterpolationMode, NetHiddenUntilShown, NetInterpolation,
    NetPendingDespawn, NetSnapshotBuffer, NetStarved, NetStepQueue, NetViewTick, ProxyState,
    ProxyTimelineState, SampleParams, SnapshotBuffer, ViewTickWire, install_interpolation,
    interpolation_report, net_interpolate, proxy_timeline, register_interpolated, set_interp_delay,
    set_view_tick,
};
use regia_net::replication::{NetEvents, NetVelocity, register_replicated};
use regia_net::runtime::NetEntities;
use regia_net::settings::InterpolationSettings;
use regia_net::testing::{LoopbackHub, SplitMix64};
use regia_net::*;
use regia_scene::components::Visibility;
use regia_scene::transform::Transform;

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;

// ---- pure-core simulation ------------------------------------------------------------------------

/// One proxy on one client: states arrive at simulated times; frames sample at simulated times.
struct Sim {
    clock: InterpClock,
    buf: SnapshotBuffer,
    profile: InterpProfile,
    params: SampleParams,
    queue: Vec<(f64, ProxyState)>,
    last_frame: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    now: f64,
    interp: f64,
    pos: Vec3,
    state: ProxyTimelineState,
    recovered: bool,
    snapped: bool,
}

impl Sim {
    fn new(send_interval: f64) -> Self {
        let settings = InterpolationSettings::default();
        let params = SampleParams {
            tick_hz: HZ,
            send_interval_ticks: send_interval,
            recover_half_life_ms: settings.recover_half_life_ms,
            newest_tick: None,
        };
        Self {
            clock: InterpClock::new(settings, HZ, send_interval),
            buf: SnapshotBuffer::new(32),
            profile: InterpProfile::default(),
            params,
            queue: Vec::new(),
            last_frame: None,
        }
    }

    fn send(&mut self, arrival_ms: f64, state: ProxyState) {
        self.queue.push((arrival_ms, state));
    }

    fn frame(&mut self, now: f64) -> Option<Frame> {
        self.queue.sort_by(|a, b| a.0.total_cmp(&b.0));
        while self.queue.first().is_some_and(|(at, _)| *at <= now) {
            let (at, s) = self.queue.remove(0);
            self.clock.on_state(s.tick, at);
            self.buf.push(s);
        }
        let frame_ms = self.last_frame.map_or(0.0, |l| now - l);
        self.last_frame = Some(now);
        let interp = self.clock.advance(now)?;
        let s = self.buf.sample(interp, &self.profile, &self.params, frame_ms)?;
        Some(Frame {
            now,
            interp,
            pos: s.pose.translation,
            state: s.state,
            recovered: s.recovered,
            snapped: s.snapped,
        })
    }
}

fn state(tick: u64, pos: Vec3, vel: Option<Vec3>) -> ProxyState {
    ProxyState {
        tick,
        translation: pos,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
        velocity: vel,
        teleport: false,
        steady: false,
    }
}

fn line(tick: f64) -> Vec3 {
    Vec3::new((3.0 * tick / HZ) as f32, 1.0, 0.0)
}

/// § 13 test 1: constant motion at a 30 Hz send rate, no jitter, frames at 60, 90 and 144 Hz:
/// the shown position equals the analytic position at `interp_tick` within 1 mm.
#[test]
fn smooth_constant_motion_at_any_frame_rate() {
    for fps in [60.0, 90.0, 144.0] {
        let mut sim = Sim::new(2.0);
        for tick in (0..=600u64).step_by(2) {
            let at = tick as f64 * MSPT + 40.0;
            sim.send(at, state(tick, line(tick as f64), Some(Vec3::new(3.0, 0.0, 0.0))));
        }
        let frame_ms = 1000.0 / fps;
        let mut checked = 0;
        for i in 0..(9.0 * fps) as usize {
            let now = 40.0 + i as f64 * frame_ms;
            let Some(f) = sim.frame(now) else {
                continue;
            };
            if now < 1000.0 {
                continue;
            }
            let err = f.pos.distance(line(f.interp));
            assert!(err < 0.001, "{fps} Hz at {now} ms: error {err} m");
            assert_eq!(f.state, ProxyTimelineState::Interpolating, "{fps} Hz at {now} ms");
            checked += 1;
        }
        assert!(checked > 300, "{fps} Hz: {checked} frames checked");
    }
}

/// § 13 test 2: ±30 ms arrival jitter: the adaptive delay settles within 2 s and no frame starves
/// (or extrapolates) over 60 s.
#[test]
fn jitter_settles_and_never_starves() {
    let mut rng = SplitMix64::new(74);
    let mut sim = Sim::new(2.0);
    for tick in (0..=62 * 60u64).step_by(2) {
        let jitter = (rng.next_f64() * 2.0 - 1.0) * 30.0;
        let at = tick as f64 * MSPT + 60.0 + jitter;
        sim.send(at, state(tick, line(tick as f64), Some(Vec3::new(3.0, 0.0, 0.0))));
    }
    let mut frames = 0;
    for i in 0..(61 * 60) {
        let now = 100.0 + i as f64 * MSPT;
        let Some(f) = sim.frame(now) else {
            continue;
        };
        if now >= 2100.0 {
            let delay = sim.clock.delay_ms(now).expect("delay");
            let target = sim.clock.target_delay_ms();
            assert!(
                (delay - target).abs() < 12.0,
                "settled by 2 s: delay {delay:.1} target {target:.1} at {now:.0}"
            );
        }
        // Before the first state is shown the proxy holds it (start-up, not starvation).
        if now < 300.0 {
            continue;
        }
        assert!(
            matches!(f.state, ProxyTimelineState::Interpolating),
            "frame at {now:.0} ms is {:?}",
            f.state
        );
        frames += 1;
    }
    assert!(frames > 3500);
    let jitter = sim.clock.jitter_ms();
    assert!(
        (10.0..20.0).contains(&jitter),
        "uniform ±30 ms has a mean deviation near 15: {jitter}"
    );
}

/// § 13 test 3: a jump in jitter raises the delay within 0.5 s; a calm period lowers it at no
/// more than a 2 % clock change; a 2 s stall triggers one resync and `interp_tick` stays monotonic.
#[test]
fn delay_adapts_up_fast_down_slowly_and_stalls_resync_once() {
    let mut rng = SplitMix64::new(3);
    let mut sim = Sim::new(2.0);
    // 0-3 s calm, 3-8 s ±60 ms jitter, 8-20 s calm.
    for tick in (0..=30 * 60u64).step_by(2) {
        let t_ms = tick as f64 * MSPT;
        let j = if (3000.0..8000.0).contains(&t_ms) {
            (rng.next_f64() * 2.0 - 1.0) * 60.0
        } else {
            0.0
        };
        sim.send(
            t_ms + 50.0 + j,
            state(tick, line(tick as f64), Some(Vec3::new(3.0, 0.0, 0.0))),
        );
    }
    let mut prev_interp = f64::MIN;
    let mut delay_at_jump = None;
    let mut rose_by = 0.0f64;
    let mut slowed = false;
    let mut lowering_rates: Vec<f64> = Vec::new();
    let mut delay_at_8s = 0.0;
    let mut delay_at_20s = 0.0;
    for i in 0..(20 * 60) {
        let now = 50.0 + i as f64 * MSPT;
        let Some(f) = sim.frame(now) else {
            continue;
        };
        assert!(f.interp >= prev_interp, "monotonic");
        prev_interp = f.interp;
        let delay = sim.clock.delay_ms(now).expect("delay");
        let rate = sim.clock.clock_rate();
        assert!(
            (0.9 - 1e-9..=1.02 + 1e-9).contains(&rate),
            "rate {rate} within -10 % / +2 %"
        );
        if now >= 3000.0 && delay_at_jump.is_none() {
            delay_at_jump = Some(delay);
        }
        if let Some(d0) = delay_at_jump
            && now < 3500.0
        {
            rose_by = rose_by.max(delay - d0);
            slowed |= rate < 1.0;
        }
        if (8000.0..8020.0).contains(&now) {
            delay_at_8s = delay;
        }
        if now >= 8000.0 {
            lowering_rates.push(rate);
        }
        delay_at_20s = delay;
    }
    assert!(slowed, "the clock slows within 0.5 s of the jitter jump");
    assert!(rose_by > 5.0, "the delay rose by {rose_by:.1} ms within 0.5 s");
    assert!(
        lowering_rates.iter().all(|r| *r <= 1.02 + 1e-9),
        "lowering never exceeds +2 %"
    );
    assert!(
        delay_at_20s < delay_at_8s - 10.0,
        "calm lowers the delay: {delay_at_8s:.1} -> {delay_at_20s:.1}"
    );
    assert_eq!(sim.clock.resyncs(), 0, "no resync without a stall");

    // A 2 s stall of the client: no frames, and the packets of the stall arrive in one burst.
    let resume = 50.0 + 20.0 * 60.0 * MSPT + 2000.0;
    let mut pending: Vec<(f64, ProxyState)> = std::mem::take(&mut sim.queue);
    for (at, _) in pending.iter_mut() {
        if *at < resume {
            *at = at.max(resume);
        }
    }
    sim.queue = pending;
    for i in 0..300 {
        let now = resume + i as f64 * MSPT;
        if let Some(f) = sim.frame(now) {
            assert!(f.interp >= prev_interp, "monotonic across the resync");
            prev_interp = f.interp;
        }
    }
    assert_eq!(sim.clock.resyncs(), 1, "a 2 s stall resyncs exactly once");
}

fn circle(tick: f64) -> (Vec3, Vec3) {
    // Radius 10 m at 5 m/s.
    let w = 0.5;
    let a = w * tick / HZ;
    (
        Vec3::new((10.0 * a.cos()) as f32, 0.0, (10.0 * a.sin()) as f32),
        Vec3::new((-5.0 * a.sin()) as f32, 0.0, (5.0 * a.cos()) as f32),
    )
}

fn run_loss(lost: &[u64]) -> Vec<Frame> {
    let mut sim = Sim::new(2.0);
    for tick in (0..=360u64).step_by(2) {
        if lost.contains(&tick) {
            continue;
        }
        let (p, v) = circle(tick as f64);
        sim.send(tick as f64 * MSPT + 30.0, state(tick, p, Some(v)));
    }
    (0..330)
        .filter_map(|i| sim.frame(30.0 + i as f64 * MSPT))
        .collect()
}

/// § 13 test 4: one lost state is covered by the two-interval delay; three consecutive lost states
/// extrapolate and recover with a decaying offset (no jump over 1 cm per frame).
#[test]
fn loss_extrapolates_and_recovers_without_jumps() {
    let one = run_loss(&[200]);
    assert!(
        one.iter()
            .all(|f| f.state != ProxyTimelineState::Extrapolating && f.state != ProxyTimelineState::Starved),
        "one lost state never starves"
    );
    let three = run_loss(&[200, 202, 204]);
    let extrapolated = three
        .iter()
        .filter(|f| f.state == ProxyTimelineState::Extrapolating)
        .count();
    assert!(extrapolated > 0, "three lost states extrapolate");
    assert!(three.iter().any(|f| f.recovered), "and recover");
    assert!(three.iter().all(|f| !f.snapped));
    // From the first shown state on (start-up holds the first state until its tick is shown).
    let start = three
        .iter()
        .position(|f| f.state == ProxyTimelineState::Interpolating)
        .unwrap_or(0);
    for w in three[start..].windows(2) {
        let shown = w[1].pos - w[0].pos;
        let truth = circle(w[1].interp).0 - circle(w[0].interp).0;
        let jump = (shown - truth).length();
        assert!(jump < 0.01, "jump {jump} m at {:.0} ms", w[1].now);
    }
}

/// § 13 test 4 (gap): an entity that rests for 2 s then moves stays exactly at rest until one send
/// interval before the new state, then blends over one interval.
#[test]
fn gap_rule_holds_rest_then_blends_one_interval() {
    let mut sim = Sim::new(2.0);
    let p0 = Vec3::new(1.0, 0.0, 2.0);
    for tick in (0..=60u64).step_by(2) {
        sim.send(tick as f64 * MSPT + 20.0, state(tick, p0, Some(Vec3::ZERO)));
    }
    let p1 = p0 + Vec3::new(0.1, 0.0, 0.0);
    sim.send(
        180.0 * MSPT + 20.0,
        state(180, p1, Some(Vec3::new(3.0, 0.0, 0.0))),
    );
    sim.send(
        182.0 * MSPT + 20.0,
        state(182, p1 + Vec3::new(0.1, 0.0, 0.0), Some(Vec3::new(3.0, 0.0, 0.0))),
    );
    let mut blended = 0;
    for i in 0..240 {
        let Some(f) = sim.frame(20.0 + i as f64 * MSPT) else {
            continue;
        };
        if f.interp < 182.0 {
            // (Past the last state the moving entity extrapolates, as it should.)
            assert_ne!(
                f.state,
                ProxyTimelineState::Starved,
                "a stopped entity is not starved"
            );
            assert_ne!(f.state, ProxyTimelineState::Extrapolating);
        }
        if f.interp <= 178.0 {
            assert_eq!(f.pos, p0, "no drift during the rest (interp {:.2})", f.interp);
        } else if f.interp < 180.0 {
            let t = ((f.interp - 178.0) / 2.0) as f32;
            assert!(
                f.pos.distance(p0.lerp(p1, t)) < 1e-4,
                "blend over the last interval"
            );
            blended += 1;
        }
    }
    assert!(blended > 0);
}

/// § 13 test 5: with the stream cut, a moving entity extrapolates for 100 ms, then holds starved;
/// a stopped entity with no updates is held without starving.
#[test]
fn starvation_extrapolates_100ms_then_holds() {
    let mut sim = Sim::new(2.0);
    let v = Vec3::new(3.0, 0.0, 0.0);
    for tick in (0..=60u64).step_by(2) {
        sim.send(tick as f64 * MSPT, state(tick, line(tick as f64), Some(v)));
    }
    let mut held = None;
    let mut seen_starved = false;
    for i in 0..200 {
        let Some(f) = sim.frame(i as f64 * MSPT) else {
            continue;
        };
        let past_ms = (f.interp - 60.0) * MSPT;
        match f.state {
            ProxyTimelineState::Extrapolating => {
                assert!(
                    past_ms > 0.0 && past_ms <= 100.0 + 1e-6,
                    "extrapolating {past_ms} ms past"
                );
                assert!(
                    f.pos.distance(line(f.interp)) < 1e-3,
                    "extrapolation follows the velocity"
                );
            }
            ProxyTimelineState::Starved => {
                assert!(past_ms > 100.0);
                seen_starved = true;
                let at_limit = line(60.0 + 100.0 / MSPT);
                assert!(f.pos.distance(at_limit) < 1e-3, "held at the limit");
                if let Some(h) = held {
                    assert_eq!(f.pos, h, "held");
                }
                held = Some(f.pos);
            }
            _ => {}
        }
    }
    assert!(seen_starved);

    let mut stopped = Sim::new(2.0);
    for tick in (0..=20u64).step_by(2) {
        stopped.send(tick as f64 * MSPT, state(tick, Vec3::ONE, Some(Vec3::ZERO)));
    }
    for i in 0..300 {
        if let Some(f) = stopped.frame(i as f64 * MSPT) {
            assert_ne!(
                f.state,
                ProxyTimelineState::Starved,
                "a stopped entity never starves"
            );
            assert_ne!(f.state, ProxyTimelineState::Extrapolating);
        }
    }
}

fn pair_frames(b: ProxyState) -> Vec<(f64, Vec3, bool)> {
    let mut buf = SnapshotBuffer::new(8);
    buf.push(state(10, Vec3::ZERO, None));
    buf.push(b);
    let p = SampleParams::default();
    (0..=20)
        .map(|i| {
            let it = 10.0 + i as f64 * 0.1;
            let s = buf
                .sample(it, &InterpProfile::default(), &p, 1.6)
                .expect("sample");
            (it, s.pose.translation, s.snapped)
        })
        .collect()
}

/// § 13 test 6: a teleport bit produces no in-between frames; a 6 m jump snaps; a 4 m one blends.
#[test]
fn snaps_and_teleports_have_no_in_between_frames() {
    let mut tele = state(12, Vec3::new(1.0, 0.0, 0.0), None);
    tele.teleport = true;
    for (name, b) in [
        ("teleport", tele),
        ("6 m", state(12, Vec3::new(6.0, 0.0, 0.0), None)),
    ] {
        let frames = pair_frames(b);
        let mut snaps = 0;
        for (it, pos, snapped) in frames {
            if it < 12.0 {
                assert_eq!(pos, Vec3::ZERO, "{name}: shows a until b's tick");
            } else {
                assert_eq!(pos, b.translation, "{name}: then b");
            }
            snaps += usize::from(snapped);
        }
        assert_eq!(snaps, 1, "{name}: one snap");
    }
    let frames = pair_frames(state(12, Vec3::new(4.0, 0.0, 0.0), None));
    let mid = frames
        .iter()
        .find(|(it, _, _)| (*it - 11.0).abs() < 1e-9)
        .expect("mid frame");
    assert!(mid.1.distance(Vec3::new(2.0, 0.0, 0.0)) < 1e-5, "4 m blends");
    assert!(frames.iter().all(|f| !f.2));
}

/// § 13 test 7: a ballistic arc at a 30 Hz send rate: Hermite stays within 2 cm of the analytic
/// arc and Linear is worse (why the option exists).
#[test]
fn hermite_follows_a_ballistic_arc_better_than_linear() {
    let g = Vec3::new(0.0, -9.81, 0.0);
    let v0 = Vec3::new(20.0, 15.0, 0.0);
    let arc = |tick: f64| {
        let t = (tick / HZ) as f32;
        (v0 * t + 0.5 * g * t * t, v0 + g * t)
    };
    let mut worst = [0.0f32; 2];
    for (k, mode) in [InterpolationMode::Hermite, InterpolationMode::Linear]
        .into_iter()
        .enumerate()
    {
        let mut buf = SnapshotBuffer::new(64);
        for tick in (0..=120u64).step_by(2) {
            let (p, v) = arc(tick as f64);
            buf.push(state(tick, p, Some(v)));
        }
        let profile = InterpProfile {
            interpolation: mode,
            snap_distance_m: 50.0,
            ..InterpProfile::default()
        };
        let p = SampleParams::default();
        for i in 0..1100 {
            let it = 2.0 + i as f64 * 0.1;
            let s = buf.sample(it, &profile, &p, 1.6).expect("sample");
            worst[k] = worst[k].max(s.pose.translation.distance(arc(it).0));
        }
    }
    assert!(worst[0] < 0.02, "Hermite error {} m", worst[0]);
    assert!(
        worst[1] > worst[0] * 10.0,
        "Linear {} m is worse than Hermite {} m",
        worst[1],
        worst[0]
    );
}

/// § 13 test 10: the view tick a command carries round-trips within 1/256 tick, and the server's
/// `NetViewTick` takes it; a repeated command keeps the previous value.
#[test]
fn view_tick_reaches_the_server_within_a_256th() {
    let mut world = World::new();
    let player = world.spawn_empty().id();
    for i in 0..2000 {
        let interp = 100.0 + i as f64 * 0.37;
        let w = ViewTickWire::new(interp, 70.0);
        let mut bw = BitWriter::new();
        w.encode(&mut bw);
        let bytes = bw.finish();
        let back = ViewTickWire::decode(&mut BitReader::new(&bytes)).expect("decode");
        set_view_tick(&mut world, player, Some(back));
        let v = world.get::<NetViewTick>(player).copied().expect("view tick");
        assert!((v.tick - interp).abs() <= 1.0 / 256.0, "{} vs {interp}", v.tick);
        assert_eq!(v.delay_ms, 70.0);
    }
    let before = world.get::<NetViewTick>(player).copied();
    set_view_tick(&mut world, player, None);
    assert_eq!(
        world.get::<NetViewTick>(player).copied(),
        before,
        "a repeated command keeps it"
    );
}

// ---- end to end ----------------------------------------------------------------------------------

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Crouch(bool);

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Glow(f32);

fn lerp_glow(a: &Glow, b: &Glow, t: f32) -> Glow {
    Glow(a.0 + (b.0 - a.0) * t)
}

fn setup(world: &mut World) {
    register_replicated::<Crouch>(world, ReplicateOptions::default());
    register_replicated::<Glow>(world, ReplicateOptions::default());
    register_interpolated::<Glow>(world, lerp_glow);
    register_replicated::<Count>(world, ReplicateOptions::default());
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Count {
    n: u32,
    m: u32,
}

/// A proxy that becomes owned while step values are queued: the queue is applied in order before
/// newer state lands, so field deltas keep decoding against the newest received value (no stale
/// or corrupted components), and the owned copy is written on arrival from then on.
#[test]
fn proxy_that_becomes_owned_keeps_exact_component_values() {
    let mut e2e = E2e::new(InterpProfile::default());
    let server = spawn_mover(&mut e2e);
    if let Ok(mut ent) = e2e.hub.server.world.get_entity_mut(server) {
        ent.insert(Count::default());
    }
    let peer = e2e.hub.clients[e2e.client].peer;
    for step in 0..120u32 {
        if let Some(mut c) = e2e.hub.server.world.get_mut::<Count>(server) {
            // One field changes every tick, the other every third tick: deltas carry one or both.
            c.n = step;
            if step.is_multiple_of(3) {
                c.m = step * 7;
            }
        }
        move_by(&mut e2e, server, 0.05);
        if step == 60 {
            regia_net::model::set_possession_owner(&mut e2e.hub.server.world, server, Some(peer));
        }
        e2e.step();
        let Some(copy) = e2e.copy_of(server) else {
            continue;
        };
        let cw = &e2e.hub.clients[e2e.client].world;
        let owned = cw.get::<NetOwnedLocally>(copy).is_some();
        let shown = cw.get::<Count>(copy).cloned().unwrap_or_default();
        if owned && step > 70 {
            // Owned: the newest received value (only the link delay behind the server), with
            // consistent fields.
            assert!(step - shown.n <= 5, "step {step}: owned copy shows {}", shown.n);
            assert_eq!(shown.m, (shown.n - shown.n % 3) * 7, "step {step}");
            assert!(cw.get::<NetStepQueue>(copy).is_none(), "no queue once owned");
        } else if !owned && step > 20 {
            // Proxy: behind, but always a value the server had (m consistent with n).
            assert!(shown.n < step - 3, "a proxy shows the past");
            assert_eq!(
                shown.m,
                (shown.n - shown.n % 3) * 7,
                "consistent fields at step {step}"
            );
        }
    }
    let copy = e2e.copy_of(server).expect("copy");
    assert!(
        e2e.hub.clients[e2e.client]
            .world
            .get::<NetOwnedLocally>(copy)
            .is_some()
    );
}

struct E2e {
    hub: LoopbackHub,
    client: usize,
    step: u64,
    /// Server tick of each sent state -> x the server had.
    sent_x: BTreeMap<u64, f32>,
}

impl E2e {
    fn new(profile: InterpProfile) -> Self {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
        hub.down.delay_ticks = 3;
        let client = hub.add_client();
        let w = &mut hub.clients[client].world;
        install_interpolation(w, HZ, 1.0);
        w.resource_mut::<NetInterpolation>().default_profile = profile;
        Self {
            hub,
            client,
            step: 0,
            sent_x: BTreeMap::new(),
        }
    }

    fn cw(&mut self) -> &mut World {
        &mut self.hub.clients[self.client].world
    }

    /// One fixed step (arrivals stamped at the step's time) and one frame at the same time.
    fn step(&mut self) -> f64 {
        self.step += 1;
        let now = self.step as f64 * MSPT;
        self.cw().resource_mut::<NetInterpolation>().now_ms = now;
        self.hub.step();
        let tick = u64::from(self.hub.server_tick().0).saturating_sub(1);
        let mut q = self.hub.server.world.query_filtered::<&Transform, With<Mover>>();
        if let Some(t) = q.iter(&self.hub.server.world).next() {
            self.sent_x.insert(tick, t.translation.x);
        }
        net_interpolate(self.cw(), now);
        now
    }

    fn copy_of(&self, server: Entity) -> Option<Entity> {
        let id = self.hub.server.world.get::<NetId>(server).copied()?;
        self.hub.clients[self.client]
            .world
            .resource::<NetEntities>()
            .entity(id)
    }

    fn interp(&self) -> f64 {
        self.hub.clients[self.client]
            .world
            .resource::<NetInterpolation>()
            .clock
            .interp_tick()
            .unwrap_or(f64::MIN)
    }

    fn expected_x(&self, interp: f64) -> Option<f32> {
        let a = self.sent_x.range(..=interp.floor() as u64).next_back()?;
        let b = self.sent_x.range(interp.ceil() as u64..).next()?;
        if a.0 == b.0 {
            return Some(*a.1);
        }
        let t = ((interp - *a.0 as f64) / (*b.0 - *a.0) as f64) as f32;
        Some(a.1 + (b.1 - a.1) * t)
    }
}

#[derive(Component)]
struct Mover;

fn spawn_mover(e2e: &mut E2e) -> Entity {
    e2e.hub
        .server
        .world
        .spawn((
            Mover,
            Replicated::default(),
            Transform::default(),
            NetVelocity(Vec3::new(3.0, 0.0, 0.0)),
            Crouch(false),
            Glow(0.0),
        ))
        .id()
}

fn move_by(e2e: &mut E2e, e: Entity, dx: f32) {
    if let Some(mut t) = e2e.hub.server.world.get_mut::<Transform>(e) {
        t.translation.x += dx;
    }
}

/// End to end: a moving proxy is shown behind the server, on the analytic path of what the server
/// sent, smoothly; a step flag applies exactly when its tick is shown; an interpolated component
/// blends; a stream cut extrapolates then starves with `NetStarved`; the agent data reads it.
#[test]
fn loopback_proxy_is_behind_smooth_and_steps_on_time() {
    let mut e2e = E2e::new(InterpProfile::default());
    let server = spawn_mover(&mut e2e);
    let mut crouch_tick: Option<u64> = None;
    let mut crouch_checked = 0;
    let mut smooth_checked = 0;
    let mut prev_glow = -1.0f32;
    for step in 0..240u64 {
        move_by(&mut e2e, server, 0.05);
        if let Some(mut g) = e2e.hub.server.world.get_mut::<Glow>(server) {
            g.0 += 1.0;
        }
        if step == 100
            && let Some(mut c) = e2e.hub.server.world.get_mut::<Crouch>(server)
        {
            c.0 = true;
        }
        e2e.step();
        let Some(copy) = e2e.copy_of(server) else {
            continue;
        };
        let interp = e2e.interp();
        let cw = &e2e.hub.clients[e2e.client].world;
        if let Some(q) = cw.get::<NetStepQueue>(copy)
            && let Some(t) = q.ticks().first()
        {
            crouch_tick.get_or_insert(*t);
        }
        if let Some(t) = crouch_tick {
            let shown = cw.get::<Crouch>(copy).is_some_and(|c| c.0);
            assert_eq!(
                shown,
                interp >= t as f64,
                "crouch applies exactly at tick {t} (interp {interp:.2})"
            );
            crouch_checked += 1;
        }
        if step > 30 {
            let x = cw.get::<Transform>(copy).expect("transform").translation.x;
            let server_x = e2e
                .hub
                .server
                .world
                .get::<Transform>(server)
                .expect("t")
                .translation
                .x;
            assert!(x < server_x - 0.1, "the proxy is shown behind the server");
            if let Some(want) = e2e.expected_x(interp) {
                assert!((x - want).abs() < 0.001, "step {step}: shown {x} want {want}");
                smooth_checked += 1;
            }
            let g = cw.get::<Glow>(copy).expect("glow").0;
            assert!(g > prev_glow, "the interpolated component blends forward");
            prev_glow = g;
            assert!(cw.get::<NetSnapshotBuffer>(copy).is_some());
        }
    }
    assert!(
        crouch_checked > 50 && smooth_checked > 150,
        "{crouch_checked} / {smooth_checked}"
    );
    let report = interpolation_report(&e2e.hub.clients[e2e.client].world).expect("report");
    assert!(report.delay_ms > 0.0 && report.target_delay_ms >= 33.0);
    assert_eq!(report.starved, 0);
    let copy = e2e.copy_of(server).expect("copy");
    let tl = proxy_timeline(&e2e.hub.clients[e2e.client].world, copy).expect("timeline");
    assert_eq!(tl.state, "interpolating");
    assert!(tl.bracket.is_some() && !tl.buffered.is_empty());

    // Stream cut: the server stops moving it (nothing changes, nothing is sent) while its last
    // state still carries a velocity: extrapolate, then starve.
    let mut starved_at = None;
    for i in 0..30 {
        e2e.step();
        if e2e.hub.clients[e2e.client]
            .world
            .get::<NetStarved>(copy)
            .is_some()
        {
            starved_at.get_or_insert(i);
        }
    }
    assert!(starved_at.is_some(), "NetStarved after the extrapolation limit");
    let counters = e2e.hub.clients[e2e.client]
        .world
        .resource::<NetInterpolation>()
        .counters;
    assert!(counters.frames_extrapolated > 0 && counters.frames_starved > 0);
    // Data resumes: NetStarved goes, and the recovery is counted.
    for _ in 0..30 {
        move_by(&mut e2e, server, 0.05);
        e2e.step();
    }
    let cw = &e2e.hub.clients[e2e.client].world;
    assert!(
        cw.get::<NetStarved>(copy).is_none(),
        "NetStarved removed when data resumes"
    );
    assert!(cw.resource::<NetInterpolation>().counters.recoveries > 0);
}

/// § 13 test 9: a spawned proxy stays hidden until its spawn tick is shown; a despawned one stays
/// (with `NetPendingDespawn`) until its despawn tick is shown; `appear_immediately` and
/// `interpolation: None` bypass both.
#[test]
fn spawn_and_despawn_follow_the_shown_time() {
    let mut e2e = E2e::new(InterpProfile::default());
    let _anchor = spawn_mover(&mut e2e);
    for _ in 0..40 {
        e2e.step();
    }
    let late = e2e
        .hub
        .server
        .world
        .spawn((Replicated::default(), Transform::default()))
        .id();
    let mut hidden_frames = 0;
    let mut revealed = false;
    for _ in 0..30 {
        e2e.step();
        let Some(copy) = e2e.copy_of(late) else {
            continue;
        };
        let interp = e2e.interp();
        let cw = &e2e.hub.clients[e2e.client].world;
        match cw.get::<NetHiddenUntilShown>(copy) {
            Some(h) => {
                assert!(interp < f64::from(h.tick.0), "hidden only before the spawn tick");
                assert_eq!(cw.get::<Visibility>(copy), Some(&Visibility::Hidden));
                hidden_frames += 1;
            }
            None => {
                assert_ne!(cw.get::<Visibility>(copy), Some(&Visibility::Hidden));
                revealed = true;
            }
        }
    }
    assert!(
        hidden_frames > 0 && revealed,
        "{hidden_frames} hidden frames, then shown"
    );

    let copy = e2e.copy_of(late).expect("copy");
    assert!(e2e.hub.server.world.despawn(late));
    let mut pending_frames = 0;
    let mut despawning_seen = false;
    for _ in 0..30 {
        e2e.step();
        let cw = &e2e.hub.clients[e2e.client].world;
        despawning_seen |= !cw.resource::<NetEvents>().despawning.is_empty();
        match cw.get_entity(copy) {
            Ok(ent) => {
                if let Some(p) = ent.get::<NetPendingDespawn>() {
                    assert!(e2e.interp() < f64::from(p.tick.0));
                    pending_frames += 1;
                }
            }
            Err(_) => break,
        }
    }
    assert!(despawning_seen, "NetDespawning on receipt");
    assert!(pending_frames > 0, "kept until the despawn tick is shown");
    assert!(
        e2e.hub.clients[e2e.client].world.get_entity(copy).is_err(),
        "despawned once shown"
    );

    for profile in [
        InterpProfile {
            appear_immediately: true,
            ..InterpProfile::default()
        },
        InterpProfile {
            interpolation: InterpolationMode::None,
            ..InterpProfile::default()
        },
    ] {
        let mut e2e = E2e::new(profile);
        let _anchor = spawn_mover(&mut e2e);
        for _ in 0..40 {
            e2e.step();
        }
        let e = e2e
            .hub
            .server
            .world
            .spawn((Replicated::default(), Transform::default()))
            .id();
        let mut copy = None;
        for _ in 0..10 {
            e2e.step();
            if let Some(c) = e2e.copy_of(e) {
                copy = Some(c);
                break;
            }
        }
        let copy = copy.expect("copy");
        let cw = &e2e.hub.clients[e2e.client].world;
        assert!(
            cw.get::<NetHiddenUntilShown>(copy).is_none(),
            "{profile:?}: shown at once"
        );
        assert_ne!(cw.get::<Visibility>(copy), Some(&Visibility::Hidden));
        if profile.interpolation == InterpolationMode::None {
            assert!(
                cw.get::<NetSnapshotBuffer>(copy).is_none(),
                "None buffers nothing"
            );
            assert!(e2e.hub.server.world.despawn(e));
            let mut gone_after = None;
            for i in 0..10 {
                e2e.step();
                let cw = &e2e.hub.clients[e2e.client].world;
                assert!(cw.get::<NetPendingDespawn>(copy).is_none(), "None never waits");
                if cw.get_entity(copy).is_err() {
                    gone_after = Some(i);
                    break;
                }
            }
            assert!(gone_after.is_some(), "None despawns on receipt");
        }
    }
}

/// § 13 test 11 (data side): the agent queries answer on a client and produce every error code.
#[test]
fn agent_data_and_error_codes() {
    let mut e2e = E2e::new(InterpProfile::default());
    let server = spawn_mover(&mut e2e);
    for _ in 0..20 {
        move_by(&mut e2e, server, 0.05);
        e2e.step();
    }
    let copy = e2e.copy_of(server).expect("copy");
    let cw = &mut e2e.hub.clients[e2e.client].world;
    assert!(interpolation_report(cw).is_ok());
    assert!(proxy_timeline(cw, copy).is_ok());
    let local = cw.spawn_empty().id();
    assert_eq!(proxy_timeline(cw, local), Err(InterpQueryError::NotProxy));
    assert_eq!(
        set_interp_delay(cw, Some(1500.0)),
        Err(InterpQueryError::DelayOutOfRange)
    );
    assert_eq!(
        set_interp_delay(cw, Some(-1.0)),
        Err(InterpQueryError::DelayOutOfRange)
    );
    assert_eq!(set_interp_delay(cw, Some(120.0)), Ok(120.0));
    assert_eq!(interpolation_report(cw).map(|r| r.target_delay_ms), Ok(120.0));
    let adaptive = set_interp_delay(cw, None).expect("adaptive");
    assert!((33.0..=250.0).contains(&adaptive));
    assert_eq!(
        interpolation_report(&e2e.hub.server.world),
        Err(InterpQueryError::NotClient)
    );
    let cw = &mut e2e.hub.clients[e2e.client].world;
    cw.remove_resource::<NetInterpolation>();
    assert_eq!(interpolation_report(cw), Err(InterpQueryError::NotRunning));
    assert_eq!(
        InterpQueryError::NotRunning.code(),
        "net_not_running",
        "codes are documented in book/agents/net-interpolation.md"
    );
}

// ---- spec conformance audit (lane spec-30-73) ---------------------------------------------------

/// § 14 / § 4: an Android pause. The app's real-time clock is reset on `resumed` (no frame gap),
/// while the server ran on for 10 s. The first arrivals after the pause are 10 s "early" for the
/// old arrival estimate: the clock resyncs exactly once by jumping forward (the delay must
/// shrink), never backward, and the proxy shows the new data at the target delay right away.
#[test]
fn an_android_resume_resyncs_once_and_shows_current_data() {
    let mut sim = Sim::new(2.0);
    let vel = Some(Vec3::new(3.0, 0.0, 0.0));
    for tick in (0..=180u64).step_by(2) {
        sim.send(tick as f64 * MSPT + 50.0, state(tick, line(tick as f64), vel));
    }
    let mut prev = f64::MIN;
    let mut now = 0.0;
    for i in 0..180 {
        now = 50.0 + i as f64 * MSPT;
        if let Some(f) = sim.frame(now) {
            assert!(f.interp >= prev, "monotonic");
            prev = f.interp;
        }
    }
    assert_eq!(sim.clock.resyncs(), 0);
    // Paused for 10 s: the server's 600 ticks of packets are lost; local time does not move.
    let resumed = now + MSPT;
    let first = 180 + 600;
    for k in (0..=240u64).step_by(2) {
        let tick = first + k;
        sim.send(
            resumed + k as f64 * MSPT + 50.0,
            state(tick, line(tick as f64), vel),
        );
    }
    let mut checked = 0;
    for i in 0..240 {
        let now = resumed + i as f64 * MSPT;
        let Some(f) = sim.frame(now) else {
            continue;
        };
        assert!(
            f.interp >= prev,
            "monotonic across the resume: {} < {prev}",
            f.interp
        );
        prev = f.interp;
        if now >= resumed + 300.0 {
            assert!(
                f.interp >= first as f64,
                "shows post-pause ticks at {now:.0}: {}",
                f.interp
            );
            assert_eq!(f.state, ProxyTimelineState::Interpolating, "at {now:.0}");
            let err = f.pos.distance(line(f.interp));
            assert!(err < 0.001, "on the line at {now:.0}: {err} m");
            let delay = sim.clock.delay_ms(now).expect("delay");
            let target = sim.clock.target_delay_ms();
            assert!(
                (delay - target).abs() < 5.0,
                "delay {delay:.1} target {target:.1}"
            );
            checked += 1;
        }
    }
    assert!(checked > 150, "{checked}");
    assert_eq!(sim.clock.resyncs(), 1, "one resync for the pause");
}

/// § 4: when a resync must grow the delay (here a dev-build override from the adaptive ~67 ms to
/// 900 ms, an error over `resync_error_ms`), `interp_tick` holds until the target catches up with
/// it, then runs at real-time speed again; it never goes backward.
#[test]
fn a_resync_that_grows_the_delay_holds_the_clock() {
    let mut sim = Sim::new(2.0);
    for tick in (0..=600u64).step_by(2) {
        sim.send(tick as f64 * MSPT + 50.0, state(tick, line(tick as f64), None));
    }
    let mut now = 0.0;
    let mut prev = f64::MIN;
    for i in 0..120 {
        now = 50.0 + i as f64 * MSPT;
        if let Some(f) = sim.frame(now) {
            prev = f.interp;
        }
    }
    let before = sim.clock.delay_ms(now).expect("delay");
    assert!(before < 100.0, "adaptive delay {before}");
    sim.clock.set_delay_override(Some(900.0));
    let mut held = 0;
    let mut moving_after_hold = 0;
    for i in 1..=180 {
        let now = 50.0 + (119 + i) as f64 * MSPT;
        let f = sim.frame(now).expect("frame");
        assert!(f.interp >= prev, "monotonic");
        if f.interp == prev {
            assert_eq!(moving_after_hold, 0, "the hold is one stretch");
            held += 1;
        } else if held > 0 {
            let step_ticks = f.interp - prev;
            assert!(
                (step_ticks - 1.0).abs() < 0.11,
                "real-time speed after the hold (within the 10 % adaptation): {step_ticks}"
            );
            moving_after_hold += 1;
        }
        prev = f.interp;
    }
    // (900 - ~67) ms of hold at 60 frames per second.
    assert!((45..=55).contains(&held), "held {held} frames");
    assert!(moving_after_hold > 100, "{moving_after_hold}");
    assert_eq!(sim.clock.resyncs(), 1);
    let after = sim.clock.delay_ms(50.0 + 299.0 * MSPT).expect("delay");
    assert!((after - 900.0).abs() < 20.0, "delay {after}");
}

/// § 3: a step component's `NetChanged` notice fires in the frame its value is applied (when
/// `interp_tick` reaches the value's tick), not in the frame its packet arrives.
#[test]
fn a_step_components_change_notice_fires_when_it_is_shown() {
    use regia_net::replication::NetChangedEvents;
    let mut e2e = E2e::new(InterpProfile::default());
    let server = spawn_mover(&mut e2e);
    let mut queued_at = None;
    let mut applied_at = None;
    let mut notices: Vec<u64> = Vec::new();
    for step in 0..160u64 {
        move_by(&mut e2e, server, 0.05);
        if step == 100
            && let Some(mut c) = e2e.hub.server.world.get_mut::<Crouch>(server)
        {
            c.0 = true;
        }
        e2e.step();
        let Some(copy) = e2e.copy_of(server) else {
            continue;
        };
        let cw = &e2e.hub.clients[e2e.client].world;
        if step < 60 {
            continue;
        }
        if queued_at.is_none()
            && cw
                .get::<NetStepQueue>(copy)
                .is_some_and(|q| !q.ticks().is_empty())
        {
            queued_at = Some(step);
        }
        if applied_at.is_none() && cw.get::<Crouch>(copy).is_some_and(|c| c.0) {
            applied_at = Some(step);
        }
        let fired = cw
            .get_resource::<NetChangedEvents<Crouch>>()
            .is_some_and(|ev| ev.iter().any(|n| n.entity == copy));
        if fired {
            notices.push(step);
        }
    }
    let (queued, applied) = (
        queued_at.expect("the value was queued"),
        applied_at.expect("applied"),
    );
    assert!(
        applied > queued,
        "the packet arrives before its tick is shown: {queued} / {applied}"
    );
    assert_eq!(
        notices,
        vec![applied],
        "one notice, in the frame the value is applied"
    );
}

#[derive(Resource, Default)]
struct OutOfView(bool);

fn in_view(world: &World, _e: Entity, _peer: PeerId) -> bool {
    !world.get_resource::<OutOfView>().is_some_and(|o| o.0)
}

/// § 6: a level-bound proxy that leaves the client's relevance is kept (`NetOutOfScope`, spec 71
/// § 4.5); it stops interpolating and holds the pose it was showing, though newer states sit in its
/// buffer. Back in relevance it interpolates again.
#[test]
fn an_out_of_scope_level_proxy_holds_its_last_shown_pose() {
    use regia_core::id::Guid;
    use regia_net::replication::{NetOutOfScope, NetRelevance};
    use regia_scene::components::EntityGuid;
    use regia_scene::level::LevelMember;
    let mut e2e = E2e::new(InterpProfile::default());
    let guid = EntityGuid(Guid::from_u128(0x74));
    // The client loaded the same level: its own entity binds to the server's.
    let copy = e2e
        .cw()
        .spawn((Replicated::default(), Transform::default(), LevelMember(7), guid))
        .id();
    e2e.hub.server.world.insert_resource(NetRelevance {
        filter: Some(in_view),
    });
    let server = e2e
        .hub
        .server
        .world
        .spawn((
            Mover,
            Replicated::default(),
            Transform::default(),
            NetVelocity(Vec3::new(3.0, 0.0, 0.0)),
            LevelMember(7),
            guid,
        ))
        .id();
    for _ in 0..60 {
        move_by(&mut e2e, server, 0.05);
        e2e.step();
    }
    assert_eq!(
        e2e.copy_of(server),
        Some(copy),
        "bound to the client's level entity"
    );
    let x_of = |e2e: &E2e| {
        e2e.hub.clients[e2e.client]
            .world
            .get::<Transform>(copy)
            .expect("copy")
            .translation
            .x
    };
    let moving = x_of(&e2e);
    assert!(moving > 0.5, "interpolating before: {moving}");
    e2e.hub.server.world.insert_resource(OutOfView(true));
    let mut held: Option<f32> = None;
    let mut held_frames = 0;
    for _ in 0..40 {
        move_by(&mut e2e, server, 0.05);
        e2e.step();
        let out = e2e.hub.clients[e2e.client]
            .world
            .get::<NetOutOfScope>(copy)
            .is_some();
        if out {
            let x = x_of(&e2e);
            match held {
                None => held = Some(x),
                Some(h) => assert_eq!(x, h, "holds the pose it was showing"),
            }
            held_frames += 1;
        }
    }
    let held = held.expect("the copy went out of scope and was kept");
    assert!(held_frames > 30, "{held_frames}");
    let buffered = e2e.hub.clients[e2e.client]
        .world
        .get::<NetSnapshotBuffer>(copy)
        .and_then(|b| b.0.newest().map(|s| s.translation.x))
        .expect("buffer");
    assert!(
        buffered > held,
        "newer states were buffered but not shown: {buffered} / {held}"
    );
    e2e.hub.server.world.insert_resource(OutOfView(false));
    for _ in 0..40 {
        move_by(&mut e2e, server, 0.05);
        e2e.step();
    }
    let cw = &e2e.hub.clients[e2e.client].world;
    assert!(cw.get::<NetOutOfScope>(copy).is_none(), "back in scope");
    assert!(x_of(&e2e) > held + 0.5, "interpolates again");
}

#[test]
fn interrupted_proxy_holds_transform_and_interpolated_component_until_restored() {
    let mut e2e = E2e::new(InterpProfile::default());
    let server = spawn_mover(&mut e2e);
    for _ in 0..60 {
        move_by(&mut e2e, server, 1.0);
        e2e.step();
    }
    let copy = e2e.copy_of(server).expect("copy");
    let position = e2e.cw().get::<Transform>(copy).expect("transform").translation;
    let glow = e2e.cw().get::<Glow>(copy).expect("glow").0;
    e2e.cw().entity_mut(copy).insert(regia_net::NetInterrupted);
    for tick in 0..60 {
        move_by(&mut e2e, server, 1.0);
        e2e.hub
            .server
            .world
            .entity_mut(server)
            .insert(Glow(tick as f32 + 1.0));
        e2e.step();
        assert_eq!(
            e2e.cw().get::<Transform>(copy).expect("transform").translation,
            position
        );
        assert_eq!(e2e.cw().get::<Glow>(copy).expect("glow").0, glow);
    }
    e2e.cw().entity_mut(copy).remove::<regia_net::NetInterrupted>();
    e2e.step();
    assert!(e2e.cw().get::<Transform>(copy).expect("transform").translation.x > position.x);
    assert!(e2e.cw().get::<Glow>(copy).expect("glow").0 > glow);
}

// ---- spec 120 § 9.7 (row R6): loss term and budget-deferred proxies ----------------------------

/// A 30 Hz state stream (ticks every 2) on a 60 Hz server, delivered 40 ms after send except for
/// the `lost` ticks; `late` ticks arrive `late_ms` after the next frame instead (reordered).
fn feed_clock(clock: &mut InterpClock, until_tick: u64, lost: &[u64], late: &[(u64, f64)]) {
    let mut pending: Vec<(f64, u64)> = Vec::new();
    for tick in (0..=until_tick).step_by(2) {
        if lost.contains(&tick) {
            continue;
        }
        let extra = late.iter().find(|(t, _)| *t == tick).map_or(0.0, |(_, ms)| *ms);
        pending.push((tick as f64 * MSPT + 40.0 + extra, tick));
    }
    pending.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (at, tick) in pending {
        clock.on_state(tick, at);
    }
}

/// Spec 120 § 9.7: the target delay is `2 send intervals + 2 x jitter + the loss term`, the loss
/// term being how much the measured burst-loss gap exceeds the two intervals (which cover one lost
/// frame). A run of three lost frames raises it by two send intervals; a frame that was only reordered (it arrives later) is no
/// loss; a gap longer than `max_delay_ms` (an outage) is not counted; the term is bounded by
/// `max_delay_ms` and forgotten after `LOSS_MEMORY_MS` of loss-free stream.
#[test]
fn burst_loss_runs_raise_the_jitter_buffer_delay() {
    let send_ms = 2.0 * MSPT;
    let settings = InterpolationSettings::default();
    let base = {
        let mut c = InterpClock::new(settings.clone(), HZ, 2.0);
        feed_clock(&mut c, 600, &[], &[]);
        c.target_delay_ms()
    };
    assert!(
        (base - 2.0 * send_ms).abs() < 1.0,
        "clean stream: two send intervals ({base:.1} ms)"
    );

    // Three frames in a row lost: a four-interval hole; the two-interval delay already covers one
    // lost frame, so the term adds two send intervals and the delay spans the hole.
    let mut c = InterpClock::new(settings.clone(), HZ, 2.0);
    feed_clock(&mut c, 600, &[300, 302, 304], &[]);
    let lossy = c.target_delay_ms();
    assert!(
        (lossy - (base + 2.0 * send_ms)).abs() < 1.0,
        "three lost frames add two send intervals: {base:.1} -> {lossy:.1} ms"
    );
    // One lost frame is already covered.
    let mut c = InterpClock::new(settings.clone(), HZ, 2.0);
    feed_clock(&mut c, 600, &[300], &[]);
    assert!(
        (c.target_delay_ms() - base).abs() < 1.0,
        "one lost frame: {:.1} ms",
        c.target_delay_ms()
    );

    // Reordered, not lost: the late frame fills the hole.
    let mut c = InterpClock::new(settings.clone(), HZ, 2.0);
    feed_clock(&mut c, 600, &[], &[(300, 50.0)]);
    let reordered = c.target_delay_ms();
    assert!(
        (reordered - base).abs() < 1.0,
        "reordering is not loss: {reordered:.1} ms"
    );

    // An outage longer than any allowed delay is not a loss run (extrapolation and resync handle it).
    let mut c = InterpClock::new(settings.clone(), HZ, 2.0);
    let outage: Vec<u64> = (300..360).step_by(2).collect();
    feed_clock(&mut c, 600, &outage, &[]);
    assert!(
        (c.target_delay_ms() - base).abs() < 1.0,
        "an outage is not a loss run"
    );

    // Bounded by max_delay_ms.
    let mut c = InterpClock::new(settings.clone(), HZ, 2.0);
    let long: Vec<u64> = (300..312).step_by(2).collect();
    feed_clock(&mut c, 600, &long, &[]);
    assert!(c.target_delay_ms() <= settings.max_delay_ms + 1e-9);
    assert!(
        (c.target_delay_ms() - (base + 5.0 * send_ms)).abs() < 1.0,
        "six lost frames add five send intervals: {:.1}",
        c.target_delay_ms()
    );

    // Forgotten after ten seconds of loss-free stream.
    let mut c = InterpClock::new(settings, HZ, 2.0);
    feed_clock(&mut c, 600 + 11 * 60, &[300, 302, 304], &[]);
    assert!((c.target_delay_ms() - base).abs() < 1.0, "old loss is forgotten");
}

/// One velocity-less proxy whose states the budget sends every `every` ticks (up to `last_sent`)
/// while the state frames themselves arrive every 2 ticks. Returns `(interp, shown, state)` per
/// 60 Hz frame.
fn deferred_run(
    every: u64,
    until: u64,
    last_sent: u64,
    pos: impl Fn(u64) -> Vec3,
) -> Vec<(f64, Vec3, ProxyTimelineState)> {
    let mut ni = NetInterpolation::new(InterpolationSettings::default(), HZ, 2.0);
    let mut buf = SnapshotBuffer::new(32);
    let profile = InterpProfile::default();
    let mut arrivals: Vec<(f64, u64)> = (0..=until)
        .step_by(2)
        .map(|t| (t as f64 * MSPT + 30.0, t))
        .collect();
    arrivals.reverse();
    let mut out = Vec::new();
    let mut last = None;
    for i in 0..(until as usize + 60) {
        let now = i as f64 * MSPT;
        while arrivals.last().is_some_and(|(at, _)| *at <= now) {
            let Some((_, tick)) = arrivals.pop() else { break };
            ni.clock.on_state(tick, now);
            if tick.is_multiple_of(every) && tick <= last_sent {
                buf.push(state(tick, pos(tick), None));
            }
        }
        let frame_ms = last.map_or(0.0, |l| now - l);
        last = Some(now);
        let Some(interp) = ni.clock.advance(now) else {
            continue;
        };
        let params = ni.sample_params();
        if let Some(s) = buf.sample(interp, &profile, &params, frame_ms) {
            out.push((interp, s.pose.translation, s.state));
        }
    }
    out
}

/// R6 (pure core): a constant-speed entity without `NetVelocity` that the budget sends every 8
/// ticks (every 4th state frame) is shown at no less than 80 % of its speed on every frame: its
/// own arrival spacing, not the send interval, sets its hold and blend, and past its newest state
/// it extrapolates from its last two states until the next one is due (base: holds 6 of every 8
/// ticks).
#[test]
fn a_budget_deferred_entity_without_velocity_keeps_its_speed() {
    let v = 4.0f32;
    let line_v = |t: u64| Vec3::new(v * t as f32 / HZ as f32, 0.0, 0.0);
    let frames = deferred_run(8, 1200, 1200, line_v);
    let mut checked = 0;
    for w in frames.windows(2) {
        let (i0, p0, _) = w[0];
        let (i1, p1, s1) = w[1];
        if i0 < 120.0 || i1 > 1150.0 || i1 - i0 < 1e-6 {
            continue;
        }
        let ratio = f64::from(p1.distance(p0)) / (f64::from(v) * (i1 - i0) / HZ);
        assert!(
            ratio >= 0.8,
            "interp {i1:.2}: shown speed {:.1} % of true ({s1:?})",
            ratio * 100.0
        );
        // And on the true path (constant speed extrapolates exactly).
        assert!(
            p1.distance(Vec3::new(v * (i1 / HZ) as f32, 0.0, 0.0)) < 0.01,
            "on the path at {i1:.2}"
        );
        checked += 1;
    }
    assert!(checked > 900, "{checked} frames checked");
}

/// R6 guard: a velocity-less entity that stops is shown at its last state once its next update is
/// overdue (the stream continues without it): at most one deferral interval of overshoot, which
/// decays back, and no lasting offset.
#[test]
fn a_deferred_entity_without_velocity_that_stops_settles_on_its_last_state() {
    let stop = 600u64;
    let pos = |t: u64| Vec3::new(4.0 * t.min(stop) as f32 / HZ as f32, 0.0, 0.0);
    // The server sends it only while it changes: the last state is the first at the stop.
    let frames = deferred_run(8, 1200, stop, pos);
    let at_stop = pos(stop);
    let max_over = frames
        .iter()
        .map(|(_, p, _)| p.x - at_stop.x)
        .fold(0.0f32, f32::max);
    assert!(
        max_over <= 4.0 * 8.0 / 60.0 + 1e-3,
        "overshoot {max_over} m is at most one deferral interval"
    );
    let (_, last, state) = frames.last().copied().expect("frames");
    assert!(
        last.distance(at_stop) < 1e-3,
        "settles on the last state: {last:?} ({state:?})"
    );
    assert_ne!(
        state,
        ProxyTimelineState::Starved,
        "a stopped entity is not starved"
    );
}

/// R6 guard: a resting velocity-less entity the budget sends every 8 ticks that starts moving holds
/// its rest exactly and never jumps: its next state arrives only about one delay before its tick,
/// so the rest's blend stays on the send interval (a blend over its 8-tick own interval would hold
/// and then jump half way when the state arrives).
#[test]
fn a_deferred_entity_at_rest_starts_moving_without_a_jump() {
    let p0 = Vec3::new(1.0, 0.0, 0.0);
    let start = 400u64;
    let pos = |t: u64| {
        if t < start {
            p0
        } else {
            p0 + Vec3::new(3.0 * (t - start + 8) as f32 / HZ as f32, 0.0, 0.0)
        }
    };
    let frames = deferred_run(8, 800, 800, pos);
    let mut moved = 0;
    for w in frames.windows(2) {
        let (i0, s0, _) = w[0];
        let (i1, s1, _) = w[1];
        if i1 > 200.0 && i1 <= (start - 2) as f64 {
            assert_eq!(s1, p0, "rest is held exactly (interp {i1:.2})");
        }
        if i0 > 200.0 && i1 < 790.0 {
            // At most the 0.4 m first step over one send interval (2 ticks) plus 3 m/s.
            let per_tick = f64::from(s1.distance(s0)) / (i1 - i0).max(1e-6);
            assert!(
                per_tick <= 0.2 + 1e-3,
                "no jump at {i1:.2}: {per_tick:.3} m per tick"
            );
            if s1 != p0 {
                moved += 1;
            }
        }
    }
    assert!(moved > 100, "it moves after the rest: {moved} frames");
}
