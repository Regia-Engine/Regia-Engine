// SPDX-License-Identifier: MPL-2.0
//! Tests of the threaded fixed-step host (ADR-8083). No wall-clock budgets: manual pacing and
//! counters drive every step; the realtime tests only wait for progress.

use super::*;
use crate::sim_bridge::{SimBridgePlugin, SimEntityView, SimEvent, SimId};
use regia_core::math::{Quat, Vec2, Vec3};
use regia_scene::Transform;
use std::sync::atomic::AtomicUsize;

// ---------------------------------------------------------------------------
// Test sims
// ---------------------------------------------------------------------------

/// Every stripe of a frame holds the frame's tick: a frame written while read would show a mix.
#[derive(Default, Clone)]
struct StripeFrame {
    tick: u64,
    stripes: Vec<u64>,
    axes: Vec2,
    edges: Vec<String>,
}

struct StripeSim {
    tick: u64,
    width: usize,
    axes: Vec2,
    edges: Vec<String>,
    events_per_tick: u64,
    panic_at: Option<u64>,
}

impl StripeSim {
    fn new(width: usize) -> Self {
        Self {
            tick: 0,
            width,
            axes: Vec2::ZERO,
            edges: Vec::new(),
            events_per_tick: 0,
            panic_at: None,
        }
    }
}

impl FixedStepSim for StripeSim {
    type Frame = StripeFrame;
    type Event = u64;

    fn step(&mut self, input: &BridgeInput) {
        self.tick += 1;
        if self.panic_at == Some(self.tick) {
            panic!("stripe sim fails at tick {}", self.tick);
        }
        self.axes = input.move_axes;
        self.edges.clone_from(&input.edges);
    }

    fn tick(&self) -> u64 {
        self.tick
    }

    fn write_frame(&self, out: &mut StripeFrame) {
        out.tick = self.tick;
        out.stripes.clear();
        out.stripes.resize(self.width, self.tick);
        out.axes = self.axes;
        out.edges.clone_from(&self.edges);
    }

    fn drain_events(&mut self, out: &mut Vec<u64>) {
        out.extend((0..self.events_per_tick).map(|i| self.tick * 10 + i));
    }
}

fn manual() -> SimHostSettings {
    SimHostSettings {
        pacing: HostPacing::Manual,
        ..SimHostSettings::default()
    }
}

fn realtime_1ms() -> SimHostSettings {
    SimHostSettings {
        pacing: HostPacing::Realtime {
            tick: Duration::from_millis(1),
            catch_up: 0,
        },
        ..SimHostSettings::default()
    }
}

/// Wait until the newest frame reaches `tick`; returns the pair that did.
fn wait_tick<S: FixedStepSim>(host: &ThreadedSimHost<S>, tick: u64) -> PublishedFrames<S::Frame> {
    let mut pair = host.published();
    while pair.curr_tick < tick {
        pair = host
            .wait_published(pair.sequence)
            .expect("the host keeps publishing");
    }
    pair
}

/// Wait until the published pair is still (held).
fn wait_still<S: FixedStepSim>(host: &ThreadedSimHost<S>) -> PublishedFrames<S::Frame> {
    let mut pair = host.published();
    while !pair.is_still() {
        pair = host
            .wait_published(pair.sequence)
            .expect("the host keeps publishing");
    }
    pair
}

// ---------------------------------------------------------------------------
// The acceptance test
// ---------------------------------------------------------------------------

/// What one reader remembers of the last pair it saw.
struct Seen {
    sequence: u64,
    prev: *const StripeFrame,
    curr: *const StripeFrame,
    prev_tick: u64,
    curr_tick: u64,
}

/// Checks one sampled pair against the publish contract; returns the violations.
fn check_pair(pair: &PublishedFrames<StripeFrame>, last: Option<&Seen>, width: usize) -> Vec<String> {
    let mut bad = Vec::new();
    for (name, frame, tick) in [
        ("prev", &pair.prev, pair.prev_tick),
        ("curr", &pair.curr, pair.curr_tick),
    ] {
        if frame.tick != tick {
            bad.push(format!(
                "seq {}: {name} frame tick {} != published {tick}",
                pair.sequence, frame.tick
            ));
        }
        if frame.stripes.len() != width || frame.stripes.iter().any(|&s| s != frame.tick) {
            bad.push(format!(
                "seq {}: {name} frame {} is half written",
                pair.sequence, frame.tick
            ));
        }
    }
    if pair.is_still() {
        if pair.prev_tick != pair.curr_tick || pair.prev_at != pair.curr_at {
            bad.push(format!(
                "seq {}: a still pair with two ticks or two times",
                pair.sequence
            ));
        }
    } else {
        if pair.curr_tick != pair.prev_tick + 1 {
            bad.push(format!(
                "seq {}: prev {} and curr {} are not consecutive",
                pair.sequence, pair.prev_tick, pair.curr_tick
            ));
        }
        if pair.curr_at < pair.prev_at {
            bad.push(format!("seq {}: curr published before prev", pair.sequence));
        }
    }
    if let Some(last) = last {
        if pair.sequence < last.sequence || pair.curr_tick < last.curr_tick {
            bad.push(format!(
                "seq {} after seq {}: the pair went backwards",
                pair.sequence, last.sequence
            ));
        }
        let same = Arc::as_ptr(&pair.prev) == last.prev
            && Arc::as_ptr(&pair.curr) == last.curr
            && pair.prev_tick == last.prev_tick
            && pair.curr_tick == last.curr_tick;
        if pair.sequence == last.sequence && !same {
            bad.push(format!(
                "seq {}: one sequence, two different pairs (torn)",
                pair.sequence
            ));
        }
    }
    bad
}

/// The P-01 acceptance test (ADR-8083): readers sampling as fast as they can while the host steps
/// 30 000 ticks and is held and released 20 times never see a torn pair: a frame from another
/// publish, a half-written frame, non-consecutive ticks while stepping, or one sequence number
/// with two different pairs.
#[test]
fn host_never_publishes_a_torn_frame() {
    const TOTAL: u64 = 30_000;
    const ROUNDS: u64 = 20;
    const WIDTH: usize = 64;
    let host = ThreadedSimHost::start(StripeSim::new(WIDTH), manual()).expect("start");
    let handle = host.handle();
    let done = AtomicBool::new(false);
    let (violations, samples) = std::thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    let mut bad = Vec::new();
                    let mut last: Option<Seen> = None;
                    let mut samples = 0u64;
                    loop {
                        let finished = done.load(Ordering::Acquire);
                        let pair = host.published();
                        samples += 1;
                        bad.extend(check_pair(&pair, last.as_ref(), WIDTH));
                        last = Some(Seen {
                            sequence: pair.sequence,
                            prev: Arc::as_ptr(&pair.prev),
                            curr: Arc::as_ptr(&pair.curr),
                            prev_tick: pair.prev_tick,
                            curr_tick: pair.curr_tick,
                        });
                        if finished || bad.len() > 16 {
                            break;
                        }
                        // Sample as often as the scheduler allows without starving the host of
                        // the publish lock on a loaded machine.
                        std::thread::yield_now();
                    }
                    (bad, samples)
                })
            })
            .collect();
        // Each round grants 1 500 ticks and holds once the host has stepped past both the round's
        // first thousand and the last hold, so every hold lands on a moving pair (often while the
        // host still has grants to step) and publishes exactly one still pair.
        let mut last_still = 0;
        for round in 1..=ROUNDS {
            handle.grant_ticks(TOTAL / ROUNDS).expect("grant");
            wait_tick(&host, (round * 1_000).max(last_still + 1));
            handle.set_held(true);
            let still = wait_still(&host);
            assert_eq!(still.prev_tick, still.curr_tick);
            last_still = still.curr_tick;
            handle.set_held(false);
        }
        let last = wait_tick(&host, TOTAL);
        assert_eq!(last.curr_tick, TOTAL);
        done.store(true, Ordering::Release);
        readers
            .into_iter()
            .fold((Vec::new(), 0u64), |(mut all, n), reader| {
                let (bad, samples) = reader.join().expect("reader");
                all.extend(bad);
                (all, n + samples)
            })
    });
    assert!(violations.is_empty(), "torn pairs seen: {violations:#?}");
    assert!(samples > 0);
    let status = handle.status();
    assert_eq!(status.ticks, TOTAL);
    assert_eq!(status.curr_tick, TOTAL);
    // One publish per tick plus one still pair per hold.
    assert_eq!(status.publishes, TOTAL + ROUNDS);
}

// ---------------------------------------------------------------------------
// Stepping, holding and input
// ---------------------------------------------------------------------------

#[test]
fn manual_pacing_steps_exactly_the_granted_ticks() {
    let host = ThreadedSimHost::start(StripeSim::new(4), manual()).expect("start");
    let boot = host.published();
    assert_eq!((boot.sequence, boot.prev_tick, boot.curr_tick), (0, 0, 0));
    assert!(boot.is_still(), "the boot pair is still");
    let handle = host.handle();
    assert_eq!(handle.grant_ticks(7), Ok(7));
    let pair = wait_tick(&host, 7);
    assert_eq!((pair.prev_tick, pair.curr_tick, pair.sequence), (6, 7, 7));
    let status = handle.status();
    assert_eq!((status.ticks, status.granted, status.manual), (7, 0, true));
    assert_eq!(
        host.into_sim().map(|sim| sim.tick),
        Ok(7),
        "no step past the grant"
    );
}

#[test]
fn a_held_host_publishes_a_still_pair_and_keeps_its_grants() {
    let host = ThreadedSimHost::start(StripeSim::new(4), manual()).expect("start");
    let handle = host.handle();
    handle.grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    assert!(!handle.set_held(true));
    let still = host.wait_published(2).expect("still pair");
    assert!(still.is_still());
    assert_eq!((still.prev_tick, still.curr_tick, still.sequence), (2, 2, 3));
    assert_eq!(still.prev_at, still.curr_at, "a still pair does not drift");
    assert_eq!(still.alpha(Instant::now(), DEFAULT_TICK), 1.0);
    handle.grant_ticks(5).expect("grant while held");
    let status = handle.status();
    assert!(status.held);
    assert_eq!(
        (status.ticks, status.granted),
        (2, 5),
        "a held host does not step"
    );
    assert!(handle.set_held(false));
    let pair = wait_tick(&host, 7);
    assert_eq!((pair.prev_tick, pair.curr_tick, pair.sequence), (6, 7, 8));
}

#[test]
fn edges_queue_in_order_and_axes_take_the_latest() {
    let host = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let handle = host.handle();
    host.push_input(&BridgeInput {
        move_axes: Vec2::new(0.5, 0.0),
        edges: vec!["a".into()],
    });
    host.push_input(&BridgeInput {
        move_axes: Vec2::new(0.0, 1.0),
        edges: vec!["b".into(), "c".into()],
    });
    handle.grant_ticks(1).expect("grant");
    let pair = wait_tick(&host, 1);
    assert_eq!(
        pair.curr.edges,
        ["a", "b", "c"],
        "every edge reaches the next tick, in order"
    );
    assert_eq!(pair.curr.axes, Vec2::new(0.0, 1.0));
    handle.grant_ticks(1).expect("grant");
    let pair = wait_tick(&host, 2);
    assert!(pair.curr.edges.is_empty(), "an edge reaches one tick only");
    assert_eq!(pair.curr.axes, Vec2::new(0.0, 1.0), "axes hold until replaced");
}

#[test]
fn a_held_host_drops_offered_input() {
    let settings = SimHostSettings {
        start_held: true,
        ..manual()
    };
    let host = ThreadedSimHost::start(StripeSim::new(1), settings).expect("start");
    let handle = host.handle();
    assert!(handle.held());
    host.push_input(&BridgeInput {
        move_axes: Vec2::ONE,
        edges: vec!["a".into(), "b".into()],
    });
    assert_eq!(handle.status().edges_dropped, 2);
    handle.set_held(false);
    host.push_input(&BridgeInput {
        move_axes: Vec2::ZERO,
        edges: vec!["c".into()],
    });
    handle.grant_ticks(1).expect("grant");
    let pair = wait_tick(&host, 1);
    assert_eq!(pair.curr.edges, ["c"]);
}

#[test]
fn a_full_bus_drops_and_counts_extra_edges() {
    let host = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let edges: Vec<String> = (0..MAX_PENDING_EDGES + 3).map(|i| i.to_string()).collect();
    host.push_input(&BridgeInput {
        move_axes: Vec2::ZERO,
        edges,
    });
    let handle = host.handle();
    assert_eq!(handle.status().edges_dropped, 3);
    handle.grant_ticks(1).expect("grant");
    let pair = wait_tick(&host, 1);
    assert_eq!(pair.curr.edges.len(), MAX_PENDING_EDGES);
    assert_eq!(
        pair.curr.edges.first().map(String::as_str),
        Some("0"),
        "the oldest edges are kept"
    );
}

#[test]
fn the_event_queue_is_bounded_and_drops_the_oldest() {
    let settings = SimHostSettings {
        event_cap: 4,
        ..manual()
    };
    let mut sim = StripeSim::new(1);
    sim.events_per_tick = 3;
    let host = ThreadedSimHost::start(sim, settings).expect("start");
    host.handle().grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    let mut events = Vec::new();
    host.take_events(&mut events);
    assert_eq!(events, [12, 20, 21, 22]);
    assert_eq!(host.handle().status().events_dropped, 2);
    events.clear();
    host.take_events(&mut events);
    assert!(events.is_empty(), "events drain once");
}

#[test]
fn stopping_joins_and_wakes_waiters() {
    let host = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let handle = host.handle();
    handle.grant_ticks(3).expect("grant");
    wait_tick(&host, 3);
    assert_eq!(host.stop(), Ok(()));
    assert_eq!(host.stop(), Ok(()), "stop is idempotent");
    assert_eq!(
        host.wait_published(3).map(|p| p.sequence),
        Err(SimHostError::Stopped)
    );
    assert_eq!(handle.grant_ticks(1), Err(SimHostError::Stopped));
    let status = handle.status();
    assert!(!status.running && !status.panicked);
    assert_eq!(host.into_sim().map(|sim| sim.tick), Err(SimHostError::Stopped));
}

#[test]
fn a_panicking_sim_is_reported_not_hung() {
    let mut sim = StripeSim::new(1);
    sim.panic_at = Some(3);
    let host = ThreadedSimHost::start(sim, manual()).expect("start");
    let handle = host.handle();
    handle.grant_ticks(10).expect("grant");
    let mut sequence = 0;
    let err = loop {
        match host.wait_published(sequence) {
            Ok(pair) => sequence = pair.sequence,
            Err(err) => break err,
        }
    };
    assert_eq!(err, SimHostError::SimPanicked);
    assert_eq!(sequence, 2, "ticks 1 and 2 published before the panic");
    let status = handle.status();
    assert!(status.panicked && !status.running);
    assert_eq!(handle.grant_ticks(1), Err(SimHostError::SimPanicked));
    assert_eq!(
        host.into_sim().map(|sim| sim.tick),
        Err(SimHostError::SimPanicked)
    );
}

#[test]
fn grants_need_a_manual_host_and_a_sane_count() {
    let host = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let handle = host.handle();
    assert_eq!(handle.grant_ticks(0), Err(SimHostError::BadGrant(0)));
    assert_eq!(
        handle.grant_ticks(MAX_GRANT + 1),
        Err(SimHostError::BadGrant(MAX_GRANT + 1))
    );
    let settings = SimHostSettings {
        start_held: true,
        ..realtime_1ms()
    };
    let realtime = ThreadedSimHost::start(StripeSim::new(1), settings).expect("start");
    assert_eq!(realtime.handle().grant_ticks(1), Err(SimHostError::NotManual));
    let status = realtime.handle().status();
    assert!(!status.manual);
    assert_eq!(status.tick_ns, 1_000_000);
}

#[test]
fn a_zero_realtime_tick_is_refused() {
    let settings = SimHostSettings {
        pacing: HostPacing::Realtime {
            tick: Duration::ZERO,
            catch_up: 0,
        },
        ..SimHostSettings::default()
    };
    let err = ThreadedSimHost::start(StripeSim::new(1), settings).err();
    assert!(matches!(err, Some(SimHostError::InvalidSettings(_))), "{err:?}");
}

/// Frames built by `Default` for this sim only (one counter per test, so parallel tests do not
/// share it).
static RECYCLE_DEFAULTS: AtomicUsize = AtomicUsize::new(0);

struct RecycleFrame(u64);

impl Default for RecycleFrame {
    fn default() -> Self {
        RECYCLE_DEFAULTS.fetch_add(1, Ordering::Relaxed);
        Self(0)
    }
}

struct RecycleSim(u64);

impl FixedStepSim for RecycleSim {
    type Frame = RecycleFrame;
    type Event = ();

    fn step(&mut self, _input: &BridgeInput) {
        self.0 += 1;
    }

    fn tick(&self) -> u64 {
        self.0
    }

    fn write_frame(&self, out: &mut RecycleFrame) {
        out.0 = self.0;
    }
}

#[test]
fn a_steady_host_recycles_its_frame_buffers() {
    let host = ThreadedSimHost::start(RecycleSim(0), manual()).expect("start");
    host.handle().grant_ticks(1_000).expect("grant");
    // Hold no pair while the host runs: the dropped `prev` is then always free to reuse.
    let pair = host.wait_published(999).expect("1000 ticks");
    assert_eq!(pair.curr.0, 1_000);
    let built = RECYCLE_DEFAULTS.load(Ordering::Relaxed);
    assert!(
        built <= 3,
        "{built} frames built for 1000 ticks: the host allocates per tick"
    );
}

// ---------------------------------------------------------------------------
// Realtime clock
// ---------------------------------------------------------------------------

#[test]
fn a_realtime_host_steps_on_its_own() {
    let host = ThreadedSimHost::start(StripeSim::new(1), realtime_1ms()).expect("start");
    let pair = wait_tick(&host, 5);
    assert!(pair.curr_tick >= 5);
    assert_eq!(host.stop(), Ok(()));
    assert!(host.handle().status().ticks >= 5);
}

#[test]
fn a_stale_presentation_stamp_pauses_a_realtime_host() {
    let settings = SimHostSettings {
        client_stale_after: Some(Duration::from_secs(3_600)),
        ..realtime_1ms()
    };
    let host = ThreadedSimHost::start(StripeSim::new(1), settings).expect("start");
    let handle = host.handle();
    // Wait (on the counter) until the host has woken and refused to step a few times.
    let mut polls = 0u32;
    while handle.status().client_waits < 3 {
        polls += 1;
        assert!(polls < 60_000, "the host never woke");
        std::thread::sleep(Duration::from_millis(1));
    }
    let status = handle.status();
    assert_eq!((status.ticks, status.publishes), (0, 0), "no stamp, no step");
    handle.stamp_client();
    let pair = host.wait_published(0).expect("a fresh stamp releases the host");
    assert!(pair.curr_tick >= 1);
}

#[test]
fn plan_wake_sleeps_catches_up_then_drops_the_debt() {
    let base = Instant::now();
    let tick = Duration::from_millis(10);
    let on_time = plan_wake(base, base + tick, 0, 2, tick);
    assert_eq!(on_time.sleep, Some(tick));
    assert_eq!(
        (on_time.next, on_time.behind, on_time.dropped),
        (base + tick, 0, 0)
    );

    let late = base + Duration::from_millis(35);
    let first = plan_wake(late, base + tick, 0, 2, tick);
    assert_eq!(
        (first.sleep, first.next, first.behind, first.dropped),
        (None, base + tick, 1, 0)
    );
    let second = plan_wake(late, base + tick, 1, 2, tick);
    assert_eq!((second.behind, second.dropped), (2, 0));
    let third = plan_wake(late, base + tick, 2, 2, tick);
    assert_eq!((third.sleep, third.next, third.behind), (None, late, 0));
    assert_eq!(third.dropped, 2, "25 ms late at 10 ms a tick drops 2 whole ticks");

    let no_catch_up = plan_wake(late, base + tick, 0, 0, tick);
    assert_eq!((no_catch_up.next, no_catch_up.dropped), (late, 2));
}

#[test]
fn client_freshness_has_an_inclusive_limit() {
    let limit = Duration::from_secs(1);
    assert!(!client_is_fresh(0, 10, limit), "no stamp yet");
    assert!(client_is_fresh(5, 1_005, limit));
    assert!(!client_is_fresh(5, 1_006, limit));
}

#[test]
fn blend_alpha_places_the_render_time_on_the_pair() {
    let base = Instant::now();
    let tick = Duration::from_millis(10);
    let curr_at = base + tick;
    assert_eq!(
        blend_alpha(base, base, base + tick, tick),
        1.0,
        "a still pair draws curr"
    );
    assert_eq!(
        blend_alpha(curr_at, base, curr_at, tick),
        1.0,
        "a reversed pair draws curr"
    );
    let half = blend_alpha(base, curr_at, base + Duration::from_millis(15), tick);
    assert!((half - 0.5).abs() < 1e-6, "{half}");
    assert_eq!(
        blend_alpha(base, curr_at, base + Duration::from_millis(5), tick),
        0.0
    );
    assert_eq!(
        blend_alpha(base, curr_at, base + Duration::from_secs(1), tick),
        1.0
    );
}

// ---------------------------------------------------------------------------
// Through the sim bridge
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct DiscFrame {
    discs: Vec<(u64, Vec3)>,
    edges_seen: u64,
}

impl SimSnapshot for DiscFrame {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for &(id, translation) in &self.discs {
            f(SimEntityView {
                id,
                kind: "disc",
                team: 0,
                translation,
                rotation: Quat::IDENTITY,
            });
        }
    }
}

struct DiscSim {
    tick: u64,
    edges_seen: u64,
}

impl FixedStepSim for DiscSim {
    type Frame = DiscFrame;
    type Event = String;

    fn step(&mut self, input: &BridgeInput) {
        self.tick += 1;
        self.edges_seen += input.edges.len() as u64;
    }

    fn tick(&self) -> u64 {
        self.tick
    }

    fn write_frame(&self, out: &mut DiscFrame) {
        out.discs.clear();
        out.discs.push((1, Vec3::new(self.tick as f32, 0.0, 0.0)));
        out.edges_seen = self.edges_seen;
    }

    fn drain_events(&mut self, out: &mut Vec<String>) {
        out.push(format!("tick:{}", self.tick));
    }
}

#[test]
fn a_threaded_host_drives_the_sim_bridge() {
    let host = ThreadedSimHost::start(
        DiscSim {
            tick: 0,
            edges_seen: 0,
        },
        manual(),
    )
    .expect("start");
    let handle = host.handle();
    handle.grant_ticks(3).expect("grant");
    wait_tick(&host, 3);

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SimBridgePlugin::new(host)));
    app.insert_resource(handle.clone());
    app.update();

    let world = app.world_mut();
    let mut discs = world.query::<(&SimId, &Transform)>();
    let found: Vec<(u64, Vec3)> = discs.iter(world).map(|(id, t)| (id.0, t.translation)).collect();
    assert_eq!(
        found,
        [(1, Vec3::new(3.0, 0.0, 0.0))],
        "the bridge reads the newest frame"
    );
    let messages = world.resource::<Messages<SimEvent<String>>>();
    let mut cursor = messages.get_cursor();
    let kinds: Vec<&str> = cursor.read(messages).map(|event| event.0.as_str()).collect();
    assert_eq!(
        kinds,
        ["tick:1", "tick:2", "tick:3"],
        "every tick's events, once, in order"
    );

    app.world_mut().resource_mut::<BridgeInput>().edges = vec!["jump".into()];
    app.update();
    handle.grant_ticks(1).expect("grant");
    let pair = wait_tick(app.world().resource::<ThreadedSimHost<DiscSim>>(), 4);
    assert!(
        pair.curr.edges_seen >= 1,
        "an edge submitted through the bridge reaches the sim"
    );
}

// ---------------------------------------------------------------------------
// The floating-point environment (ADR-8526)
// ---------------------------------------------------------------------------

/// Records the thread's floating-point environment as `step` sees it; can plant FTZ at a tick to
/// model foreign code changing it on the sim thread.
#[derive(Default)]
struct FpSim {
    tick: u64,
    seen: Vec<regia_detsim::fpu::FpEnvReport>,
    plant_at: Option<u64>,
}

impl FixedStepSim for FpSim {
    type Frame = StripeFrame;
    type Event = ();

    fn step(&mut self, _input: &BridgeInput) {
        self.tick += 1;
        self.seen.push(regia_detsim::fpu::read_fp_env());
        if self.plant_at == Some(self.tick) {
            regia_detsim::fpu::set_flush_to_zero_for_tests();
        }
    }

    fn tick(&self) -> u64 {
        self.tick
    }

    fn write_frame(&self, out: &mut StripeFrame) {
        out.tick = self.tick;
    }
}

/// DC-3.8: a sim thread spawned from a thread that runs with flush-to-zero (an audio or video
/// library's setting) must still step under the IEEE default. POSIX threads inherit the
/// floating-point environment, so without the host's thread-start guard the sim's first step sees
/// FTZ.
#[test]
fn host_thread_starts_with_the_default_fp_env() {
    let planted = regia_detsim::fpu::set_flush_to_zero_for_tests();
    let started = ThreadedSimHost::start(FpSim::default(), manual());
    // Put this test thread back before anything else runs on it.
    let parent = regia_detsim::fpu::ensure_default_fp_env();
    assert!(planted.was_default(), "the test thread started at the default");
    assert_eq!(
        parent.flush_to_zero, parent.supported,
        "FTZ was planted on the parent: {parent:?}"
    );
    let host = started.expect("start");
    let handle = host.handle();
    handle.grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    let status = handle.status();
    let sim = host.into_sim().expect("sim");
    assert_eq!(sim.seen.len(), 2);
    for (tick, report) in sim.seen.iter().enumerate() {
        assert!(
            report.was_default(),
            "tick {} stepped under a non-default environment: {report:?}",
            tick + 1
        );
    }
    assert_eq!(status.fp_env_supported, parent.supported);
    if cfg!(unix) && parent.supported {
        assert!(
            status.fp_env_reset,
            "the thread inherited FTZ and the guard reset it: {status:?}"
        );
        assert_ne!(status.fp_env_at_start, 0);
    }
}

#[test]
fn a_changed_fp_env_is_reset_and_counted() {
    let settings = SimHostSettings {
        fp_check_every: 1,
        ..manual()
    };
    let sim = FpSim {
        plant_at: Some(2),
        ..FpSim::default()
    };
    let host = ThreadedSimHost::start(sim, settings).expect("start");
    let handle = host.handle();
    handle.grant_ticks(4).expect("grant");
    wait_tick(&host, 4);
    let status = handle.status();
    let sim = host.into_sim().expect("sim");
    assert!(
        sim.seen.iter().all(|report| report.was_default()),
        "{:?}",
        sim.seen
    );
    assert_eq!(status.fp_checks, 4, "one check before every step");
    let expected = u64::from(regia_detsim::fpu::read_fp_env().supported);
    assert_eq!(status.fp_env_changed, expected, "the planted FTZ is found once");

    // With the periodic check off, the change survives to the next step (the guard ran at start
    // only), which is what the setting trades for speed.
    let settings = SimHostSettings {
        fp_check_every: 0,
        ..manual()
    };
    let sim = FpSim {
        plant_at: Some(1),
        ..FpSim::default()
    };
    let host = ThreadedSimHost::start(sim, settings).expect("start");
    host.handle().grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    let status = host.handle().status();
    let sim = host.into_sim().expect("sim");
    assert_eq!((status.fp_checks, status.fp_env_changed), (0, 0));
    assert_eq!(sim.seen[1].flush_to_zero, expected == 1);
}

#[test]
fn the_default_fp_check_interval_follows_the_build() {
    let every = SimHostSettings::default().fp_check_every;
    if cfg!(debug_assertions) {
        assert_eq!(every, 1, "debug builds check before every step");
    } else {
        assert_eq!(every, DEFAULT_FP_CHECK_EVERY);
    }
}

// ---------------------------------------------------------------------------
// Lockstep pacing, snapshots and catch-up (ADR-8526)
// ---------------------------------------------------------------------------

/// The published frame of [`LockSim`].
#[derive(Default, Clone)]
struct LockFrame {
    tick: u64,
    hash: u64,
}

/// A small lockstep sim: one body driven by every seat's input, a hash chained over every tick.
/// Plain float arithmetic only (no std float functions on the step path).
#[derive(Debug, Default, Clone, PartialEq)]
struct LockSim {
    tick: u64,
    pos: f32,
    vel: f32,
    hash: u64,
    /// Every applied tick with its inputs (not saved: a restore starts a new log).
    seen: Vec<(u64, Vec<i32>)>,
}

impl LockSim {
    fn mix(&mut self, word: u64) {
        self.hash = (self.hash ^ word).wrapping_mul(0x0100_0000_01b3).rotate_left(17);
    }
}

impl FixedStepSim for LockSim {
    type Frame = LockFrame;
    type Event = ();

    fn step(&mut self, _input: &BridgeInput) {
        panic!("a lockstep host never steps merged input");
    }

    fn tick(&self) -> u64 {
        self.tick
    }

    fn write_frame(&self, out: &mut LockFrame) {
        out.tick = self.tick;
        out.hash = self.hash;
    }
}

impl LockstepSim for LockSim {
    type Inputs = Vec<i32>;

    fn step_inputs(&mut self, tick: u64, inputs: &Vec<i32>) {
        assert_eq!(tick, self.tick + 1, "frames arrive in tick order");
        self.tick = tick;
        for &seat in inputs {
            self.vel += seat as f32 * 0.25;
        }
        self.vel *= 0.9;
        self.pos += self.vel * (1.0 / 60.0);
        self.mix(tick);
        self.mix(u64::from(self.pos.to_bits()));
        self.mix(u64::from(self.vel.to_bits()));
        self.seen.push((tick, inputs.clone()));
    }
}

impl DetState for LockSim {
    const STATE_TAG: [u8; 8] = *b"lockhost";
    const STATE_VERSION: u32 = 1;

    fn save(&self, w: &mut regia_detsim::state::DetWriter) {
        w.u64(self.tick);
        w.f32(self.pos);
        w.f32(self.vel);
        w.u64(self.hash);
    }

    fn load(r: &mut regia_detsim::state::DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(Self {
            tick: r.u64()?,
            pos: r.f32()?,
            vel: r.f32()?,
            hash: r.u64()?,
            seen: Vec::new(),
        })
    }
}

/// A deterministic two-seat frame for `tick`.
fn frame_for(tick: u64) -> Vec<i32> {
    vec![(tick * 7 % 5) as i32 - 2, (tick * 3 % 4) as i32 - 1]
}

/// The uninterrupted reference: ticks `1..=to` stepped in order on one thread.
fn reference(to: u64) -> LockSim {
    let mut sim = LockSim::default();
    for tick in 1..=to {
        sim.step_inputs(tick, &frame_for(tick));
    }
    sim
}

fn lockstep_by_grants() -> SimHostSettings {
    SimHostSettings {
        pacing: HostPacing::Lockstep { tick: None },
        ..SimHostSettings::default()
    }
}

fn push_range(feed: &LockstepFeed<Vec<i32>>, ticks: std::ops::RangeInclusive<u64>) {
    for tick in ticks {
        feed.push_frame(tick, frame_for(tick)).expect("frame");
    }
}

/// DC-3.8: a lockstep host steps exact per-tick frames, each once and in order. A gap stalls (and
/// is counted) until the frame arrives; a duplicate or an old frame is refused; bridge input is
/// never merged in.
#[test]
fn lockstep_pacing_steps_each_frame_once_in_order() {
    let (host, feed) =
        ThreadedSimHost::start_lockstep(LockSim::default(), lockstep_by_grants()).expect("start");
    let handle = host.handle();
    assert!(handle.status().lockstep);
    feed.push_frame(1, frame_for(1)).expect("1");
    feed.push_frame(2, frame_for(2)).expect("2");
    assert_eq!(
        feed.push_frame(4, frame_for(4)),
        Ok(3),
        "a frame past a gap waits in the queue"
    );
    assert_eq!(
        feed.push_frame(2, vec![9, 9]),
        Err(SimHostError::FrameOutOfOrder { tick: 2, next: 1 }),
        "a duplicate is refused"
    );
    host.push_input(&BridgeInput {
        move_axes: Vec2::ONE,
        edges: vec!["fire".into()],
    });

    handle.grant_ticks(3).expect("grant");
    let stalled = handle
        .wait_status(|s| s.curr_tick == 2 && s.frame_stalls == 1)
        .expect("the third wake stalls at the gap");
    assert_eq!((stalled.ticks, stalled.frames_queued), (2, 1));

    feed.push_frame(3, frame_for(3)).expect("3 fills the gap");
    let filled = handle
        .wait_status(|s| s.curr_tick == 3)
        .expect("the stalled wake steps the filled gap");
    assert_eq!(filled.frames_queued, 1, "one tick per wake: frame 4 waits");
    assert_eq!(
        feed.push_frame(3, frame_for(3)),
        Err(SimHostError::FrameOutOfOrder { tick: 3, next: 4 })
    );
    assert_eq!(
        feed.push_frame(1, frame_for(1)),
        Err(SimHostError::FrameOutOfOrder { tick: 1, next: 4 })
    );
    handle.grant_ticks(1).expect("grant");
    let done = handle.wait_status(|s| s.curr_tick == 4).expect("tick 4");
    assert_eq!(done.ticks, 4);
    assert_eq!(done.frames_refused, 3);
    assert_eq!(done.frame_stalls, 1);
    assert_eq!(done.edges_dropped, 1, "bridge input is dropped, never merged");
    assert!(done.max_step_ns >= done.last_step_ns);
    assert_eq!(feed.next_tick(), 5);

    let sim = host.into_sim().expect("sim");
    let order: Vec<u64> = sim.seen.iter().map(|(tick, _)| *tick).collect();
    assert_eq!(order, [1, 2, 3, 4], "each frame once, in order");
    assert!(sim.seen.iter().all(|(tick, inputs)| *inputs == frame_for(*tick)));
    assert_eq!(sim.hash, reference(4).hash);
}

#[test]
fn a_frame_past_the_queue_cap_is_refused() {
    let settings = SimHostSettings {
        frame_queue_cap: 4,
        ..lockstep_by_grants()
    };
    let (host, feed) = ThreadedSimHost::start_lockstep(LockSim::default(), settings).expect("start");
    assert_eq!(feed.push_frame(4, frame_for(4)), Ok(1));
    assert_eq!(
        feed.push_frame(5, frame_for(5)),
        Err(SimHostError::FrameQueueFull { tick: 5, cap: 4 })
    );
    assert_eq!(host.handle().status().frames_refused, 1);
}

#[test]
fn pacing_and_constructor_must_agree() {
    let err = ThreadedSimHost::start(StripeSim::new(1), lockstep_by_grants()).err();
    assert!(matches!(err, Some(SimHostError::InvalidSettings(_))), "{err:?}");
    let err = ThreadedSimHost::start_lockstep(LockSim::default(), manual()).err();
    assert!(matches!(err, Some(SimHostError::InvalidSettings(_))), "{err:?}");
    let zero = SimHostSettings {
        pacing: HostPacing::Lockstep {
            tick: Some(Duration::ZERO),
        },
        ..SimHostSettings::default()
    };
    let err = ThreadedSimHost::start_lockstep(LockSim::default(), zero).err();
    assert!(matches!(err, Some(SimHostError::InvalidSettings(_))), "{err:?}");

    let merged = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let handle = merged.handle();
    assert!(!handle.snapshots_enabled());
    assert_eq!(
        handle.request_snapshot_now().err(),
        Some(SimHostError::NoSnapshots)
    );
    assert_eq!(handle.restore(Vec::new()).err(), Some(SimHostError::NoSnapshots));
    assert_eq!(handle.catch_up(4), Err(SimHostError::NotLockstep));
}

/// DC-3.8: a snapshot taken on the host thread, restored on the host thread, resumes bit for bit:
/// the hash after the restore plus N frames equals an uninterrupted run's. The restore republishes
/// a still boot pair at the restored tick, keeps a hold, refuses bad bytes without touching the
/// sim, and a late joiner starts from a snapshot of another host.
#[test]
fn snapshot_then_restore_republishes_and_resumes_bit_identically() {
    const MID: u64 = 60;
    const LATE: u64 = 90;
    const END: u64 = 120;
    let (host, feed) =
        ThreadedSimHost::start_lockstep(LockSim::default(), lockstep_by_grants()).expect("start");
    let handle = host.handle();
    assert!(handle.snapshots_enabled());
    push_range(&feed, 1..=MID);
    handle.grant_ticks(MID).expect("grant");
    wait_until_tick(&host, MID);
    let mid = handle
        .request_snapshot_now()
        .expect("request")
        .wait()
        .expect("snapshot");
    assert_eq!(mid.tick, MID);
    assert_eq!(mid.header.tick, MID);
    assert_eq!(mid.header.tag, *b"lockhost");
    let late = handle.request_snapshot(LATE).expect("request at a later tick");
    assert!(
        late.try_wait().is_none(),
        "answered when the sim reaches tick {LATE}"
    );

    push_range(&feed, MID + 1..=END);
    handle.grant_ticks(END - MID).expect("grant");
    let end = wait_until_tick(&host, END);
    assert_eq!(
        end.curr.hash,
        reference(END).hash,
        "the host matches the reference run"
    );
    let late = late.wait().expect("snapshot at tick 90");
    assert_eq!(late.tick, LATE);
    assert_eq!(
        handle.request_snapshot(10).expect("request").wait(),
        Err(SimHostError::SnapshotTickPassed { tick: 10, curr: END })
    );

    // A flipped body byte is refused by the frame hash; the sim is unchanged.
    let mut bad = mid.bytes.clone();
    if let Some(last) = bad.last_mut() {
        *last ^= 0x40;
    }
    let refused = handle.restore(bad).expect("request").wait();
    assert!(
        matches!(refused, Err(SimHostError::RestoreFailed(_))),
        "{refused:?}"
    );
    assert_eq!(handle.status().curr_tick, END);

    // Restore while held: the hold stays, and the pair is a still boot pair at the restored tick.
    handle.set_held(true);
    let restored = handle.restore(mid.bytes.clone()).expect("request").wait();
    assert_eq!(restored, Ok(MID));
    let pair = host.published();
    assert!(pair.is_still());
    assert_eq!((pair.prev_tick, pair.curr_tick), (MID, MID));
    assert_eq!(pair.curr.hash, reference(MID).hash);
    let status = handle.status();
    assert!(status.held);
    assert_eq!((status.curr_tick, status.restores, status.snapshots), (MID, 1, 2));
    assert_eq!(
        feed.next_tick(),
        MID + 1,
        "the feed takes frames after the restored tick"
    );
    handle.set_held(false);

    push_range(&feed, MID + 1..=END);
    handle.grant_ticks(END - MID).expect("grant");
    wait_until_tick(&host, END);
    let resumed = host.into_sim().expect("sim");
    let uninterrupted = reference(END);
    assert_eq!(resumed.hash, uninterrupted.hash);
    assert_eq!(resumed.pos.to_bits(), uninterrupted.pos.to_bits());
    assert_eq!(resumed.vel.to_bits(), uninterrupted.vel.to_bits());

    // A late joiner: a fresh host restores another host's snapshot and runs the frames after it.
    let (joiner, join_feed) =
        ThreadedSimHost::start_lockstep(LockSim::default(), lockstep_by_grants()).expect("start");
    let join = joiner.handle();
    assert_eq!(join.restore(late.bytes).expect("request").wait(), Ok(LATE));
    push_range(&join_feed, LATE + 1..=END);
    join.grant_ticks(END - LATE).expect("grant");
    wait_until_tick(&joiner, END);
    assert_eq!(joiner.into_sim().expect("sim").hash, uninterrupted.hash);
}

/// Wait until the published `curr_tick` equals `tick` (a restore may move it backwards, so this
/// waits on the status, not on a growing tick).
fn wait_until_tick<S: FixedStepSim>(host: &ThreadedSimHost<S>, tick: u64) -> PublishedFrames<S::Frame> {
    host.handle()
        .wait_status(|s| s.curr_tick == tick)
        .expect("the host reaches the tick");
    host.published()
}

/// Spec 120 § 7.4: a catching-up host steps queued frames faster than real time but never more
/// than its per-wake budget, publishes every tick, and drops back to one tick per wake once it has
/// stepped every queued frame.
#[test]
fn catch_up_obeys_the_per_frame_tick_budget() {
    let (host, feed) =
        ThreadedSimHost::start_lockstep(LockSim::default(), lockstep_by_grants()).expect("start");
    let handle = host.handle();
    assert_eq!(handle.catch_up(0), Err(SimHostError::BadCatchUp(0)));
    assert_eq!(
        handle.catch_up(MAX_CATCH_UP + 1),
        Err(SimHostError::BadCatchUp(MAX_CATCH_UP + 1))
    );
    push_range(&feed, 1..=100);
    handle.catch_up(8).expect("catch up");
    handle.grant_ticks(3).expect("three wakes");
    let burst = handle
        .wait_status(|s| s.ticks == 24 && s.granted == 0)
        .expect("three wakes of eight");
    assert_eq!(burst.catch_up_ticks, 3 * 7, "seven extra ticks per wake");
    assert_eq!(burst.catch_up_budget, 8, "still behind: catch-up continues");
    assert_eq!(burst.publishes, 24, "every caught-up tick is published");

    // 76 frames left: nine wakes of eight and one of four reach tick 100, then catch-up ends.
    handle.grant_ticks(20).expect("more wakes");
    let caught = handle
        .wait_status(|s| s.curr_tick == 100 && s.catch_up_budget == 0 && s.frame_stalls == 1)
        .expect("caught up");
    assert_eq!(caught.ticks, 100);
    assert_eq!(caught.catch_up_ticks, 3 * 7 + 9 * 7 + 3);

    // Back to one tick per wake.
    push_range(&feed, 101..=103);
    let live = handle.wait_status(|s| s.curr_tick == 103).expect("live ticks");
    assert_eq!(
        live.catch_up_ticks, caught.catch_up_ticks,
        "no extra ticks at the live edge"
    );
    let sim = host.into_sim().expect("sim");
    assert_eq!(sim.hash, reference(103).hash);
}

/// A snapshot or restore asked for while the host waits (for a grant, a frame or a release) runs
/// at once on the host thread; a stopped host answers `Stopped` instead of hanging.
#[test]
fn commands_run_while_the_host_waits_and_fail_once_it_stops() {
    let (host, feed) =
        ThreadedSimHost::start_lockstep(LockSim::default(), lockstep_by_grants()).expect("start");
    let handle = host.handle();
    // Waiting for a grant.
    let boot = handle
        .request_snapshot_now()
        .expect("request")
        .wait()
        .expect("snapshot");
    assert_eq!(boot.tick, 0);
    // Stalled on a missing frame.
    handle.grant_ticks(1).expect("grant");
    handle.wait_status(|s| s.frame_stalls == 1).expect("stall");
    assert_eq!(
        handle
            .request_snapshot_now()
            .expect("request")
            .wait()
            .map(|s| s.tick),
        Ok(0)
    );
    feed.push_frame(1, frame_for(1)).expect("frame");
    handle
        .wait_status(|s| s.curr_tick == 1)
        .expect("the interrupted wake still steps");
    let pending = handle.request_snapshot(50).expect("request");
    host.stop().expect("stop");
    assert_eq!(pending.wait(), Err(SimHostError::Stopped));
    assert_eq!(handle.request_snapshot_now().err(), Some(SimHostError::Stopped));
    assert_eq!(feed.push_frame(2, frame_for(2)), Err(SimHostError::Stopped));
}

// ---------------------------------------------------------------------------
// Android suspend (ADR-8526)
// ---------------------------------------------------------------------------

fn lifecycle_app(sim: ThreadedSimHost<DiscSim>, handle: &SimHostHandle) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SimBridgePlugin::new(sim)));
    // What `PlatformPlugin` installs; the runner calls `apply_transition` for the OS event.
    app.init_resource::<regia_platform::AppLifecycle>()
        .add_message::<regia_platform::LifecycleEvent>();
    app.insert_resource(handle.clone());
    app.update();
    app
}

/// DC-3.8: a suspended app must not step its sim unseen. The bridge plugin's lifecycle hook holds
/// the host on `Suspended` and releases it on `Resumed`, unless the game had held it already.
#[test]
fn suspend_holds_the_host() {
    let host = ThreadedSimHost::start(
        DiscSim {
            tick: 0,
            edges_seen: 0,
        },
        manual(),
    )
    .expect("start");
    let handle = host.handle();
    handle.grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    let mut app = lifecycle_app(host, &handle);

    regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Suspended);
    assert!(handle.held(), "Suspended holds the host");
    handle.grant_ticks(3).expect("grant while suspended");
    let still = handle.wait_status(|s| s.publishes == 3).expect("a still pair");
    assert_eq!(
        (still.ticks, still.granted),
        (2, 3),
        "a suspended host does not step"
    );

    regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Resumed);
    assert!(!handle.held(), "Resumed releases the hold the suspend took");
    let resumed = handle.wait_status(|s| s.curr_tick == 5).expect("steps again");
    assert_eq!(resumed.granted, 0);

    // A host the game held (a lobby) stays held across a suspend and resume.
    handle.set_held(true);
    regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Suspended);
    regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Resumed);
    assert!(handle.held(), "the game's own hold survives");
}

/// ADR-10105: the gameplay pause holds a threaded host (it steps on its own thread, so nothing
/// else would stop it) and unpausing releases it; a hold the game took itself survives the pause.
#[test]
fn the_gameplay_pause_holds_the_host() {
    let host = ThreadedSimHost::start(
        DiscSim {
            tick: 0,
            edges_seen: 0,
        },
        manual(),
    )
    .expect("start");
    let handle = host.handle();
    handle.grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    let mut app = lifecycle_app(host, &handle);

    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    assert!(handle.held(), "the pause holds the host");
    handle.grant_ticks(3).expect("grant while paused");
    for _ in 0..30 {
        app.update();
    }
    let status = handle.status();
    assert_eq!(
        (status.ticks, status.granted),
        (2, 3),
        "a paused host does not step"
    );

    app.world_mut().resource_mut::<Time>().paused = false;
    app.update();
    assert!(!handle.held(), "unpausing releases the hold the pause took");
    let resumed = handle.wait_status(|s| s.curr_tick == 5).expect("steps again");
    assert_eq!(
        resumed.granted, 0,
        "the held grants run after the pause, none lost"
    );

    // The game's own hold (a lobby) stays across a pause and its end.
    handle.set_held(true);
    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    app.world_mut().resource_mut::<Time>().paused = false;
    app.update();
    assert!(handle.held(), "the game's own hold survives");
}

#[test]
fn a_realtime_lockstep_host_steps_frames_as_they_arrive() {
    let settings = SimHostSettings {
        pacing: HostPacing::Lockstep {
            tick: Some(Duration::from_millis(1)),
        },
        ..SimHostSettings::default()
    };
    let (host, feed) = ThreadedSimHost::start_lockstep(LockSim::default(), settings).expect("start");
    let handle = host.handle();
    assert!(!handle.status().manual);
    push_range(&feed, 1..=5);
    handle
        .wait_status(|s| s.curr_tick == 5 && s.frame_stalls >= 1)
        .expect("five frames, then a stall at the live edge");
    push_range(&feed, 6..=8);
    let status = handle.wait_status(|s| s.curr_tick == 8).expect("eight");
    assert_eq!((status.ticks, status.frames_queued), (8, 0));
    assert_eq!(host.into_sim().expect("sim").hash, reference(8).hash);
}

// ---------------------------------------------------------------------------
// The pair read (ADR-8456)
// ---------------------------------------------------------------------------

#[test]
fn a_manual_host_pair_read_draws_curr() {
    let mut host = ThreadedSimHost::start(
        DiscSim {
            tick: 0,
            edges_seen: 0,
        },
        manual(),
    )
    .expect("start");
    host.handle().grant_ticks(2).expect("grant");
    wait_tick(&host, 2);
    let (mut prev, mut curr) = (DiscFrame::default(), DiscFrame::default());
    let pair = host
        .read_pair(Duration::ZERO, DEFAULT_TICK, &mut prev, &mut curr)
        .expect("a host publishes a pair");
    // Manual pacing has no clock to blend on: the newest frame, every time.
    assert_eq!(pair.alpha, 1.0);
    assert_eq!(pair.curr_tick.0, 2);
    assert_eq!(pair.prev_tick.0, 1);
    assert_eq!(curr.discs, [(1, Vec3::new(2.0, 0.0, 0.0))]);
}

#[test]
fn a_realtime_pair_read_is_one_consistent_pair() {
    let mut host = ThreadedSimHost::start(
        DiscSim {
            tick: 0,
            edges_seen: 0,
        },
        realtime_1ms(),
    )
    .expect("start");
    wait_tick(&host, 3);
    let (mut prev, mut curr) = (DiscFrame::default(), DiscFrame::default());
    for _ in 0..200 {
        let pair = host
            .read_pair(Duration::ZERO, Duration::from_millis(1), &mut prev, &mut curr)
            .expect("a host publishes a pair");
        assert!((0.0..=1.0).contains(&pair.alpha), "{}", pair.alpha);
        assert_eq!(
            curr.discs[0].1.x, pair.curr_tick.0 as f32,
            "curr is the pair's newest frame"
        );
        if pair.alpha < crate::sim_blend::BLEND_CURR_AT {
            // The bridge reads `prev` only below the `curr` threshold: then it is the older frame
            // of the same publish.
            assert_eq!(pair.prev_tick.0 + 1, pair.curr_tick.0);
            assert_eq!(prev.discs[0].1.x, pair.prev_tick.0 as f32);
        }
    }
}

/// A hold that lands between a manual host taking a grant and stepping it must keep that grant.
/// P-02 found the P-01 host dropping it: `host_never_publishes_a_torn_frame` then waited forever for
/// its last tick on a loaded machine (the sim thread asleep, every grant used up or lost). Each round
/// races a hold against the host taking a fresh grant, sweeping the delay between them; after the
/// thread is joined, every tick ever granted must be stepped or still pending.
#[test]
fn a_hold_never_eats_a_granted_tick() {
    const ROUNDS: u64 = 20_000;
    let host = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let handle = host.handle();
    for round in 0..ROUNDS {
        handle.grant_ticks(1).expect("grant");
        for _ in 0..round % 64 {
            std::thread::yield_now();
        }
        handle.set_held(true);
        handle.set_held(false);
    }
    let sim = host.into_sim().expect("the host joins");
    let status = handle.status();
    assert_eq!(
        sim.tick + status.granted,
        ROUNDS,
        "a hold dropped {} granted ticks ({status:?})",
        ROUNDS.saturating_sub(sim.tick + status.granted)
    );
}

/// The same hold, placed deterministically: the host raises it right after taking the grant, then
/// the host is stopped and joined. The grant must still be pending (on the P-01 host it was gone).
#[test]
fn a_hold_after_a_grant_is_taken_keeps_the_grant() {
    let host = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    let handle = host.handle();
    host.shared
        .control
        .hold_after_grant
        .store(true, Ordering::Release);
    handle.grant_ticks(1).expect("grant");
    // The host thread sets the hold once it has taken the grant.
    while !handle.held() {
        std::thread::yield_now();
    }
    let sim = host.into_sim().expect("the host joins");
    let status = handle.status();
    assert_eq!(sim.tick, 0, "a held host does not step");
    assert_eq!(status.granted, 1, "the hold kept the grant ({status:?})");
}

// ---------------------------------------------------------------------------
// Desync rows of an external sim (ADR-8547)
// ---------------------------------------------------------------------------

/// A counter sim with a per-system fold: `clock` holds the tick, `bank` a value nudged at `bump_at`.
struct FoldSim {
    tick: u64,
    bank: u32,
    bump_at: Option<u64>,
}

impl FixedStepSim for FoldSim {
    type Frame = u64;
    type Event = ();
    fn step(&mut self, _input: &BridgeInput) {
        self.tick += 1;
        if self.bump_at == Some(self.tick) {
            self.bank += 1;
        }
    }
    fn tick(&self) -> u64 {
        self.tick
    }
    fn write_frame(&self, out: &mut u64) {
        *out = self.tick;
    }
}

impl SystemFold<2> for FoldSim {
    const SYSTEMS: regia_detsim::hash::Systems<2> = regia_detsim::hash::Systems::new(["clock", "bank"]);
    fn fold_systems(&self, mix: &mut [regia_detsim::hash::Mix; 2]) {
        mix[0].u64(self.tick);
        mix[1].u64(u64::from(self.bank));
    }
}

fn run_fold_host(bump_at: Option<u64>, ticks: u64) -> (ThreadedSimHost<FoldSim>, SimHostHandle) {
    let host = ThreadedSimHost::start_with_desync::<2>(
        FoldSim {
            tick: 0,
            bank: 0,
            bump_at,
        },
        manual(),
        false,
    )
    .expect("start");
    let handle = host.handle();
    handle.grant_ticks(ticks).expect("grant");
    let mut pair = host.published();
    while pair.curr_tick < ticks {
        pair = host.wait_published(pair.sequence).expect("steps");
    }
    (host, handle)
}

#[test]
fn a_folding_sim_records_a_row_per_step_before_the_publish() {
    let (host, handle) = run_fold_host(None, 5);
    let desync = handle.desync().expect("recording");
    let rows = desync.rows(None, 96);
    assert_eq!(rows.systems, ["clock", "bank"]);
    // The boot state (tick 0) and every published tick: the row is there before its frame.
    assert_eq!(
        rows.rows.iter().map(|r| r.tick).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5]
    );
    assert_eq!(rows.recorded, 6);
    let sim = host.into_sim().expect("joined");
    let now = regia_detsim::hash::system_hashes(&sim);
    assert_eq!(rows.rows[5].words, now.words.to_vec());

    // Two hosts, one nudged at tick 3: the diagnosis names `bank` at 3.
    let (_, other) = run_fold_host(Some(3), 5);
    let theirs = other.desync().expect("recording").rows(None, 96).rows;
    let d = desync.diagnose(&rows.rows, &theirs).expect("same width");
    assert_eq!(d.canonical.list(), "bank@3");
    assert_eq!(d.canonical.last_agreed, Some(2));

    // A host started without a fold carries no rows.
    let plain = ThreadedSimHost::start(StripeSim::new(1), manual()).expect("start");
    assert!(plain.handle().desync().is_none());
}

#[test]
fn the_world_reads_a_host_ring_without_an_ecs_hash() {
    let (_host, handle) = run_fold_host(None, 3);
    let mut world = World::new();
    world.insert_resource(handle);
    let rows = crate::desync_hash::desync_rows(&world, Some(3), 2).expect("host rows");
    assert_eq!(rows.source, crate::desync_hash::DesyncSource::SimHost);
    assert_eq!(rows.rows.iter().map(|r| r.tick).collect::<Vec<_>>(), [2, 3]);
    let report = crate::desync_hash::compare_desync_rows(&world, &rows.rows, &rows.rows)
        .expect("host rows")
        .expect("same width");
    assert!(report.systems.is_empty());
}

// ---------------------------------------------------------------------------
// A restore restarts the desync ring (ADR-8621)
// ---------------------------------------------------------------------------

impl DetState for FoldSim {
    const STATE_TAG: [u8; 8] = *b"foldsim\0";
    const STATE_VERSION: u32 = 1;

    fn save(&self, w: &mut regia_detsim::state::DetWriter) {
        w.u64(self.tick);
        w.u64(u64::from(self.bank));
        w.u64(self.bump_at.unwrap_or(0));
    }

    fn load(r: &mut regia_detsim::state::DetReader<'_>) -> Result<Self, DetStateError> {
        let tick = r.u64()?;
        let bank = u32::try_from(r.u64()?).map_err(|_| DetStateError::bad_value("bank"))?;
        let bump_at = r.u64()?;
        Ok(Self {
            tick,
            bank,
            bump_at: (bump_at != 0).then_some(bump_at),
        })
    }
}

fn snapshot_fold_host(bump_at: Option<u64>) -> ThreadedSimHost<FoldSim> {
    ThreadedSimHost::start_with_snapshots_and_desync::<2>(
        FoldSim {
            tick: 0,
            bank: 0,
            bump_at,
        },
        manual(),
        false,
    )
    .expect("start")
}

fn step_to<S: FixedStepSim>(host: &ThreadedSimHost<S>, tick: u64) {
    let handle = host.handle();
    let now = handle.status().curr_tick;
    handle.grant_ticks(tick - now).expect("grant");
    wait_until_tick(host, tick);
}

fn ring_ticks(handle: &SimHostHandle) -> Vec<u64> {
    let rows = handle.desync().expect("recording").rows(None, 96);
    rows.rows.iter().map(|row| row.tick).collect()
}

/// A peer that split at tick 2 resyncs from a good peer's snapshot at tick 6. Its ring must not keep
/// the rows of the timeline it left (ticks 0-5 included): compared with the good peer it agrees.
#[test]
fn a_restore_restarts_the_desync_ring_at_the_restored_tick() {
    let good = snapshot_fold_host(None);
    let split = snapshot_fold_host(Some(2));
    let (good_h, split_h) = (good.handle(), split.handle());
    step_to(&good, 6);
    let snap = good_h
        .request_snapshot_now()
        .expect("request")
        .wait()
        .expect("snapshot");
    assert_eq!(snap.tick, 6);
    step_to(&split, 8);
    let before = split_h.desync().expect("recording").rows(None, 96);
    let d = split_h
        .desync()
        .expect("recording")
        .diagnose(
            &before.rows,
            &good_h.desync().expect("recording").rows(None, 96).rows,
        )
        .expect("same width");
    assert_eq!(d.canonical.list(), "bank@2", "the split before the resync");

    assert_eq!(
        split_h.restore(snap.bytes.clone()).expect("request").wait(),
        Ok(6)
    );
    assert_eq!(
        ring_ticks(&split_h),
        [6],
        "the ring restarts at the restored tick"
    );
    step_to(&split, 10);
    step_to(&good, 10);
    assert_eq!(ring_ticks(&split_h), [6, 7, 8, 9, 10]);
    let ours = split_h.desync().expect("recording").rows(None, 96);
    assert_eq!(ours.newest, Some(10));
    assert_eq!(
        ours.recorded,
        9 + 1 + 4,
        "the count keeps counting across the restore"
    );
    let theirs = good_h.desync().expect("recording").rows(None, 96);
    let d = split_h
        .desync()
        .expect("recording")
        .diagnose(&ours.rows, &theirs.rows)
        .expect("same width");
    assert!(d.same(), "{d:?}");
    assert_eq!((d.canonical.compared, d.canonical.last_agreed), (5, Some(10)));

    // A refused restore leaves the ring alone.
    assert!(matches!(
        split_h.restore(vec![1, 2, 3]).expect("request").wait(),
        Err(SimHostError::RestoreFailed(_))
    ));
    assert_eq!(ring_ticks(&split_h), [6, 7, 8, 9, 10]);
}

impl SystemFold<2> for LockSim {
    const SYSTEMS: regia_detsim::hash::Systems<2> = regia_detsim::hash::Systems::new(["motion", "log"]);
    fn fold_systems(&self, mix: &mut [regia_detsim::hash::Mix; 2]) {
        mix[0].f32(self.pos);
        mix[0].f32(self.vel);
        mix[1].u64(self.hash);
    }
}

#[test]
fn a_lockstep_restore_restarts_the_desync_ring() {
    let (host, feed) =
        ThreadedSimHost::start_lockstep_with_desync::<2>(LockSim::default(), lockstep_by_grants(), true)
            .expect("start");
    let handle = host.handle();
    push_range(&feed, 1..=20);
    step_to(&host, 10);
    let mid = handle
        .request_snapshot_now()
        .expect("request")
        .wait()
        .expect("snapshot");
    step_to(&host, 20);
    assert_eq!(ring_ticks(&handle).len(), 21, "the boot row and 20 steps");
    assert_eq!(handle.restore(mid.bytes).expect("request").wait(), Ok(10));
    assert_eq!(ring_ticks(&handle), [10]);
    push_range(&feed, 11..=15);
    step_to(&host, 15);
    assert_eq!(ring_ticks(&handle), [10, 11, 12, 13, 14, 15]);
    let rows = handle.desync().expect("recording").rows(None, 96);
    assert!(rows.raw, "a raw ring stays raw across the restore");

    // The rows match a host that ran straight to 15.
    let (straight, feed2) =
        ThreadedSimHost::start_lockstep_with_desync::<2>(LockSim::default(), lockstep_by_grants(), true)
            .expect("start");
    push_range(&feed2, 1..=15);
    step_to(&straight, 15);
    let theirs = straight.handle().desync().expect("recording").rows(None, 96);
    let d = handle
        .desync()
        .expect("recording")
        .diagnose(&rows.rows, &theirs.rows)
        .expect("same width");
    assert!(d.same(), "{d:?}");
    assert_eq!(d.canonical.compared, 6);
}

/// KI-GP-005 (ADR-10062): a `DetInputSim` runs on the lockstep host through `DetInputHost`; the
/// host steps the same states and hashes as `advance` in a plain loop, a host snapshot restores
/// into an adapter at its tick with the same state, and the adapter's tag keeps bare sim snapshots
/// and adapter snapshots apart.
#[test]
fn a_det_input_sim_runs_on_the_lockstep_host_through_the_adapter() {
    use regia_detsim::hash::{Mix, SystemFold, Systems};
    use regia_detsim::replay::{DetInputSim, TickInputs, advance};
    use regia_detsim::state::{DetReader, DetWriter};

    #[derive(Default, Debug, Clone, PartialEq)]
    struct Tally {
        sums: [i64; 2],
    }
    impl DetState for Tally {
        const STATE_TAG: [u8; 8] = *b"tally\0\0\0";
        const STATE_VERSION: u32 = 1;
        fn save(&self, w: &mut DetWriter) {
            w.i64(self.sums[0]);
            w.i64(self.sums[1]);
        }
        fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
            Ok(Tally {
                sums: [r.i64()?, r.i64()?],
            })
        }
    }
    impl SystemFold<2> for Tally {
        const SYSTEMS: Systems<2> = Systems::new(["left", "right"]);
        fn fold_systems(&self, mix: &mut [Mix; 2]) {
            mix[0].fold(&self.sums[0]);
            mix[1].fold(&self.sums[1]);
        }
    }
    impl DetInputSim<2> for Tally {
        type Input = i32;
        fn step(&mut self, tick: u64, inputs: &TickInputs<i32>) {
            for entry in inputs.seats() {
                if let (Some(v), Some(sum)) = (entry.input, self.sums.get_mut(usize::from(entry.seat))) {
                    *sum = sum.wrapping_mul(3).wrapping_add(i64::from(v) + tick as i64);
                }
            }
        }
    }
    /// The sim's tick `tick`: seat 0 every tick, seat 1 on every third.
    fn inputs(tick: u64) -> TickInputs<i32> {
        TickInputs::from_sorted(
            tick,
            [
                (0, Some(1 + (tick % 5) as i32)),
                (1, tick.is_multiple_of(3).then_some(-2)),
            ],
        )
        .expect("sorted")
    }
    #[derive(Default)]
    struct Sums([i64; 2]);
    impl DetInputFrame<Tally> for Sums {
        fn write(&mut self, sim: &Tally, _tick: u64) {
            self.0 = sim.sums;
        }
    }

    let mut plain = Tally::default();
    let mut plain_hashes = Vec::new();
    for tick in 0..12u64 {
        plain_hashes.push(advance(&mut plain, &inputs(tick)));
    }

    let adapter: DetInputHost<Tally, Sums, 2> = DetInputHost::new(Tally::default(), 0);
    let (host, feed) = ThreadedSimHost::start_lockstep(adapter, lockstep_by_grants()).expect("start");
    let handle = host.handle();
    for host_tick in 1..=12u64 {
        feed.push_frame(host_tick, inputs(host_tick - 1)).expect("frame");
    }
    handle.grant_ticks(7).expect("grant");
    handle.wait_status(|s| s.curr_tick == 7).expect("seven ticks");
    let snap = handle
        .request_snapshot_now()
        .expect("request")
        .wait_timeout(Duration::from_secs(10))
        .expect("snapshot");
    assert_eq!(snap.tick, 7);
    assert_eq!(snap.header.tag, adapter_tag(Tally::STATE_TAG));
    handle.grant_ticks(5).expect("grant");
    handle.wait_status(|s| s.curr_tick == 12).expect("twelve ticks");
    assert_eq!(
        host.published().curr.0,
        plain.sums,
        "the frame writer published the sim"
    );

    let done = host.into_sim().expect("sim");
    assert_eq!(done.last_hashes(), plain_hashes.last().copied());
    let (sim, tick) = done.into_parts();
    assert_eq!((sim, tick), (plain, 12));

    // The snapshot taken at host tick 7 is the plain loop's state after its tick 6.
    let (at, restored) =
        state::restore::<DetInputHost<Tally, Sums, 2>>(&snap.bytes).expect("adapter restore");
    let mut seven = Tally::default();
    for tick in 0..7u64 {
        advance(&mut seven, &inputs(tick));
    }
    assert_eq!((at, restored.sim()), (7, &seven));
    assert_eq!(restored.last_hashes(), None);
    let bare = state::restore::<Tally>(&snap.bytes)
        .map(|_| ())
        .expect_err("an adapter snapshot is not a bare sim");
    assert_eq!(bare.code(), "bad_tag");
}
