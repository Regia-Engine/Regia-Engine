// SPDX-License-Identifier: MPL-2.0
//! The time harness (ADR-8185, spec 120 § 9 and § 15.4): client prediction through the real
//! transport and its RTT estimator, with clocks that drift, delays that step and datagrams that
//! arrive inside a tick.
//!
//! The long sessions (`*_session_reports_its_numbers`) run 10 minutes of virtual time and print
//! one [`TimeReport`] line per client (`--nocapture` shows it): the numbers the prediction, clock
//! and lead lanes set their bars on. The harness's own tests check the harness, not the engine.
// The time harness runs ECS worlds (feature `ecs`, on by default).
#![cfg(feature = "ecs")]

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::bridge::NetFrame;
use regia_net::predict::ClientPrediction;
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::settings::StateChannel;
use regia_net::*;
use regia_net_soak::impair::{Direction, DropCause, GilbertElliott, LinkSpec};
use regia_net_soak::profiles::{CLOCK_DRIFT_PPM, Profile};
use regia_net_soak::timed::{TICK_US, TimeReport, TimedHub, TimedOptions, rtt_ramp};
use regia_scene::transform::Transform;

// ---- the test game (the hero of regia_net's tests/prediction.rs) -------------------------------

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.time_harness";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    x: i8,
    jump: bool,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        Self {
            jump: false,
            ..last.clone()
        }
    }
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Motion {
    vy: f32,
    grounded: bool,
}

/// Client: the input of each client tick.
#[derive(Resource, Clone, Copy)]
struct Script(fn(u32) -> Pad);

type Inputs<'w, 's> = Query<'w, 's, (&'static NetPlayer, &'static InputCommand<Pad>)>;
type HeroData = (
    &'static mut Transform,
    &'static mut Motion,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);

fn step_hero(cx: PredictionCx, inputs: Inputs, mut heroes: Query<HeroData>) {
    for (mut t, mut m, owner, role) in &mut heroes {
        if !cx.simulates(role) {
            continue;
        }
        let player = cx.owner_player(owner);
        let pad = inputs
            .iter()
            .find(|(p, _)| Some(p.id) == player)
            .map(|(_, c)| c.value.clone())
            .unwrap_or_default();
        t.translation.x += f32::from(pad.x) * 0.1;
        if pad.jump && m.grounded {
            m.vy = 5.0;
            m.grounded = false;
        }
        if !m.grounded {
            m.vy -= 9.8 / 60.0;
            t.translation.y += m.vy / 60.0;
            if t.translation.y <= 0.0 {
                t.translation.y = 0.0;
                m.vy = 0.0;
                m.grounded = true;
            }
        }
    }
}

/// The game, with `regia_net`'s default settings.
fn setup(world: &mut World) {
    register_replicated::<Motion>(world, ReplicateOptions::default());
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    predict::<Motion>(world, Tolerance::Exact).expect("motion");
    add_predicted_systems(world, step_hero);
}

/// The game with the lockstep hub's settings (`regia_net::testing`: one frame per tick, state on
/// the reliable channel), for the parity run.
fn hub_setup(world: &mut World) {
    let mut s = world.resource_mut::<NetSettings>();
    s.replication.send_every_ticks = 1;
    s.replication.state_channel = StateChannel::Reliable;
    setup(world);
}

fn feed(script: Res<Script>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<Pad>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

/// tests/prediction.rs's 600-tick recording: idle, walking left and right with jumps, idle.
fn recording(t: u32) -> Pad {
    if !(120..600).contains(&t) {
        return Pad::default();
    }
    Pad {
        x: if (t / 37).is_multiple_of(3) { -1 } else { 1 },
        jump: t.is_multiple_of(50),
    }
}

/// A whole session of play: the recording's pattern repeated, never idle for long.
fn play(t: u32) -> Pad {
    Pad {
        x: if (t / 37).is_multiple_of(3) { -1 } else { 1 },
        jump: t.is_multiple_of(50),
    }
}

/// A hub with `links.len()` clients, each with a hero it owns and predicts, spawned when all are
/// connected.
fn session(
    seed: u64,
    setup: fn(&mut World),
    links: Vec<LinkSpec>,
    script: fn(u32) -> Pad,
) -> (TimedHub, Vec<Entity>) {
    let options = TimedOptions {
        seed,
        ..TimedOptions::default()
    };
    let mut hub = TimedHub::new::<Game>(options, setup).expect("server");
    for link in links {
        let c = hub.add_client::<Game>(link).expect("client");
        hub.client(c).world.insert_resource(Script(script));
        hub.client(c).gameplay.add_systems(feed);
    }
    let n = hub.client_count();
    assert!(
        hub.run_until(10_000, |h| (0..n).all(|c| h.client_peer(c).is_some())),
        "every client connects"
    );
    let heroes = (0..n)
        .map(|c| {
            let peer = hub.client_peer(c).expect("peer");
            hub.server()
                .world
                .spawn((
                    Replicated {
                        transform: TransformReplication::PositionOnly,
                        ..Replicated::default()
                    },
                    Transform::default(),
                    Motion {
                        vy: 0.0,
                        grounded: true,
                    },
                    NetPredicted,
                    NetOwner(peer),
                ))
                .id()
        })
        .collect();
    (hub, heroes)
}

fn client_hero(hub: &mut TimedHub, c: usize) -> Option<Entity> {
    let w = &mut hub.client(c).world;
    let mut q = w.query_filtered::<(Entity, &NetRole), With<Motion>>();
    q.iter(w).find(|(_, r)| **r == NetRole::Predicted).map(|(e, _)| e)
}

fn pos(world: &World, e: Entity) -> Vec3 {
    world
        .get::<Transform>(e)
        .map(|t| t.translation)
        .unwrap_or(Vec3::NAN)
}

fn named_link(profile: &str, client: usize, duration_ms: u64) -> LinkSpec {
    Profile::named(profile)
        .unwrap_or_else(|| panic!("profile {profile}"))
        .client_link(client, duration_ms)
}

fn print(name: &str, reports: &[TimeReport]) {
    for r in reports {
        println!("time-harness {name}: {}", r.line());
        println!(
            "time-harness {name} json: {}",
            serde_json::to_string(r).unwrap_or_default()
        );
    }
}

const TEN_MIN_MS: u64 = 600_000;

// ---- parity -------------------------------------------------------------------------------------

/// The parity link: 100 ms each way, 5 % loss and reordering of up to 50 ms both ways (the
/// lockstep hub's 6 ticks, 5 % loss, 20 % reorder of 1-3 ticks).
fn parity_link() -> LinkSpec {
    let dir = Direction {
        latency_ms: 100,
        loss: 0.05,
        reorder: 0.2,
        reorder_ms: 50,
        ..Direction::clean()
    };
    LinkSpec {
        up: dir.clone(),
        down: dir,
        ..LinkSpec::default()
    }
}

/// tests/prediction.rs `no_false_corrections_under_latency_jitter_and_loss` on this harness: the
/// RTT comes from the transport's estimator (that test sets it by hand before the session),
/// reliable frames are resent by the transport (the lockstep hub guarantees them) and the client
/// runs at its own phase inside the tick. Its assertions hold over the report's window, which
/// starts once the lead had its 2 s to settle (spec 120 § 9.2): no false corrections, no resets,
/// no resyncs, both sides end bit-identical, and the 5 % loss went through the resend path.
/// The first 2 s are `startup_before_the_first_rtt_sample_keeps_prediction_clean`'s.
#[test]
fn parity_no_false_corrections_under_latency_jitter_and_loss() {
    let (mut hub, heroes) = session(11, hub_setup, vec![parity_link()], recording);
    let start = hub.server_tick();
    hub.run_until(60_000, |h| h.server_tick() >= start + 900);
    let report = hub.report(0);
    println!("parity: {}", report.line());
    assert!(report.predicted_ticks > 600, "{}", report.line());
    assert_eq!(report.corrections, 0, "{}", report.line());
    assert_eq!(report.resets, 0, "{}", report.line());
    assert_eq!(report.resyncs, 0, "{}", report.line());
    let ch = client_hero(&mut hub, 0).expect("predicted copy");
    let sp = pos(hub.server_world(), heroes[0]);
    let cp = pos(hub.client_world(0), ch);
    assert!(sp.x.abs() > 1.0, "the hero walked: {sp}");
    assert_eq!(
        sp.to_array().map(f32::to_bits),
        cp.to_array().map(f32::to_bits),
        "bit-identical end position"
    );
    assert_eq!(
        hub.server_world().get::<Motion>(heroes[0]),
        hub.client_world(0).get::<Motion>(ch)
    );
    assert!(report.server_reliable_resends > 0, "{}", report.line());
}

/// The parity test's assertions over the whole session, from the connect. Failed before N208
/// (ADR-8219): prediction started its clock at the first state frame after the connect, before
/// the transport's first RTT sample, so its lead started about 14 ticks short at this RTT; it
/// counted two resets and resynced once when the sample arrived (resets at 1,089 and 1,106 ms with
/// `rtt_ms: None`, the resync at 1,223 ms with a reported margin of -20). Spec 120 § 9.2: "the
/// first lead comes from the handshake RTT plus jitter, correct within 2 s". The clock now waits
/// for the transport's first sample (`predict::expect_transport_rtt`).
#[test]
fn startup_before_the_first_rtt_sample_keeps_prediction_clean() {
    let (mut hub, _) = session(11, hub_setup, vec![parity_link()], recording);
    let start = hub.server_tick();
    hub.run_until(60_000, |h| h.server_tick() >= start + 900);
    let stats = hub.client_world(0).resource::<ClientPrediction>().stats.clone();
    for e in &hub.report(0).events {
        println!("startup event: {e:?}");
    }
    assert_eq!(stats.corrections, 0, "{stats:?}");
    assert_eq!(stats.resets, 0, "{stats:?}");
    assert_eq!(stats.resyncs, 0, "{stats:?}");
}

// ---- the long sessions (numbers, no bars) -------------------------------------------------------

/// rtt-step (20 ms one way, 125 ms from 60 s, 20 ms again from 120 s: RTT 40 -> 250 -> 40 ms) for
/// 10 minutes through the transport's RTT estimator.
#[test]
fn rtt_step_session_reports_its_numbers() {
    let link = named_link("rtt-step", 0, TEN_MIN_MS);
    assert_eq!(link.phases.len(), 2, "two steps");
    let (mut hub, _) = session(0x5EED_0001, setup, vec![link], play);
    hub.run_ms(TEN_MIN_MS);
    let reports = hub.reports();
    print("rtt-step", &reports);
    let r = &reports[0];
    assert!(r.measured_ms >= TEN_MIN_MS - 5_000, "{}", r.line());
    assert!(r.predicted_ticks > 30_000, "{}", r.line());
    assert!(r.lead_error_ticks.samples > 1_000, "{}", r.line());
    assert_eq!(r.rtt_settles.len(), 2, "both steps are measured: {}", r.line());
    assert_eq!(r.disconnects, 0, "{}", r.line());
}

/// clock-drift (good-wifi; client 0's clock 0.5 % fast, client 1's 0.5 % slow) for 10 minutes.
#[test]
fn clock_drift_session_reports_its_numbers() {
    let links = vec![
        named_link("clock-drift", 0, TEN_MIN_MS),
        named_link("clock-drift", 1, TEN_MIN_MS),
    ];
    assert_eq!(links[0].clock_ppm, CLOCK_DRIFT_PPM);
    assert_eq!(links[1].clock_ppm, -CLOCK_DRIFT_PPM);
    let (mut hub, _) = session(0x5EED_0002, setup, links, play);
    let (s0, c0, c1) = (hub.server_updates(), hub.client_updates(0), hub.client_updates(1));
    hub.run_ms(TEN_MIN_MS);
    let reports = hub.reports();
    print("clock-drift", &reports);
    // The harness: each client ran its own clock's ticks (36,000 server ticks; +-180).
    let server = (hub.server_updates() - s0) as i64;
    let fast = (hub.client_updates(0) - c0) as i64;
    let slow = (hub.client_updates(1) - c1) as i64;
    assert!(
        (fast - server - server / 200).abs() <= 1,
        "fast {fast} server {server}"
    );
    assert!(
        (server - slow - server / 200).abs() <= 2,
        "slow {slow} server {server}"
    );
    for r in &reports {
        assert!(r.measured_ms >= TEN_MIN_MS - 5_000, "{}", r.line());
        assert!(r.lead_error_ticks.samples > 1_000, "{}", r.line());
        assert_eq!(r.disconnects, 0, "{}", r.line());
    }
}

/// rtt-step and a 0.5 % fast clock at once, for 10 minutes.
#[test]
fn rtt_step_with_clock_drift_session_reports_its_numbers() {
    let mut link = named_link("rtt-step", 0, TEN_MIN_MS);
    link.clock_ppm = CLOCK_DRIFT_PPM;
    let (mut hub, _) = session(0x5EED_0003, setup, vec![link], play);
    hub.run_ms(TEN_MIN_MS);
    let reports = hub.reports();
    print("rtt-step+clock-drift", &reports);
    let r = &reports[0];
    assert!(r.measured_ms >= TEN_MIN_MS - 5_000, "{}", r.line());
    assert_eq!(r.rtt_settles.len(), 2, "{}", r.line());
    assert_eq!(r.disconnects, 0, "{}", r.line());
}

/// high-rtt (RTT 450 ms on client 0, 650 ms on client 1, 1 % loss) for 2 minutes: spec 120 § 9.4's
/// scenario (whether prediction stays on: `resets`).
#[test]
fn high_rtt_session_reports_its_numbers() {
    let links = vec![
        named_link("high-rtt", 0, 120_000),
        named_link("high-rtt", 1, 120_000),
    ];
    let (mut hub, _) = session(0x5EED_0004, setup, links, play);
    hub.run_ms(120_000);
    let reports = hub.reports();
    print("high-rtt", &reports);
    for r in &reports {
        assert!(r.measured_ms >= 110_000, "{}", r.line());
    }
}

// ---- spec 120 § 9.4 and § 9.2 (N208, ADR-8219) -------------------------------------------------

const FIVE_MIN_MS: u64 = 300_000;

/// Server-side kicks the clients cannot predict: every 7 s the server moves each hero 0.5 m
/// sideways (`net.force_correction`), so every client takes a correction of that size through
/// whatever rollback depth its round trip needs.
const KICK_EVERY_MS: u64 = 7_000;
const KICK_M: f32 = 0.5;

fn run_with_kicks(hub: &mut TimedHub, heroes: &[Entity], ms: u64) -> u64 {
    let mut kicks = 0;
    let mut left = ms;
    while left > 0 {
        let chunk = left.min(KICK_EVERY_MS);
        hub.run_ms(chunk);
        left -= chunk;
        if left > 0 {
            for h in heroes {
                regia_net::predict::force_correction(&mut hub.server().world, *h, Vec3::Z * KICK_M)
                    .expect("server hero");
            }
            kicks += 1;
        }
    }
    kicks
}

/// Spec 120 § 9.4's bar: high-rtt (RTT 450 ms on client 0, 650 ms on client 1, 0-20 ms jitter,
/// 1 % loss) for 5 minutes. Prediction stays on (at most one reset after the start), the own pawn's
/// correction p95 is within the value measured on transatlantic + 20 %, and nothing snaps (no
/// correction over the snap distance). Both sessions take the same unpredictable server kicks
/// ([`run_with_kicks`]), so corrections happen at every round trip and must re-run and smooth
/// (the test game itself never mispredicts: 16-fold input redundancy covers 1 % loss). At the
/// base, 650 ms reset on 3,501 of 7,080 ticks in 2 minutes: every state was over the 32-tick cap.
#[test]
fn high_rtt_prediction_stays_on_for_five_minutes() {
    let (mut hub, heroes) = session(
        0x5EED_0005,
        setup,
        vec![named_link("transatlantic", 0, FIVE_MIN_MS)],
        play,
    );
    let kicks = run_with_kicks(&mut hub, &heroes, FIVE_MIN_MS);
    let transatlantic = hub.report(0);
    print("transatlantic", std::slice::from_ref(&transatlantic));
    assert!(
        transatlantic.corrections + 1 >= kicks,
        "every kick corrects: {kicks} kicks, {}",
        transatlantic.line()
    );
    let bar = transatlantic.correction_m.p95_abs * 1.2;
    let links = vec![
        named_link("high-rtt", 0, FIVE_MIN_MS),
        named_link("high-rtt", 1, FIVE_MIN_MS),
    ];
    let (mut hub, heroes) = session(0x5EED_0004, setup, links, play);
    let kicks = run_with_kicks(&mut hub, &heroes, FIVE_MIN_MS);
    let reports = hub.reports();
    print("high-rtt", &reports);
    let snap_m = f64::from(
        hub.client_world(0)
            .resource::<NetSettings>()
            .prediction
            .snap_distance_m,
    );
    for r in &reports {
        assert!(r.measured_ms >= FIVE_MIN_MS - 5_000, "{}", r.line());
        assert!(r.predicted_ticks > 15_000, "{}", r.line());
        assert_eq!(r.disconnects, 0, "{}", r.line());
        assert!(r.resets <= 1, "prediction stays on: {}", r.line());
        assert!(
            r.corrections + 1 >= kicks,
            "each kick is a correction that re-ran, not a reset: {kicks} kicks, {}",
            r.line()
        );
        assert!(
            r.correction_m.p95_abs <= bar,
            "correction p95 {} over transatlantic {} + 20 %: {}",
            r.correction_m.p95_abs,
            transatlantic.correction_m.p95_abs,
            r.line()
        );
        assert_eq!(r.snaps, 0, "{}", r.line());
        assert!(r.correction_m.max_abs <= snap_m, "{}", r.line());
    }
}

/// Spec 120 § 9.2: the input lead is right within 2 s of the connect, on transatlantic (three
/// seeds), on the parity link (100 ms each way, 5 % loss, reordering) and at 650 ms RTT: over the
/// first second of the report window (2-3 s after the connect) the median arrival margin at the
/// server is within one tick of the steady-state target (the median target of the last 10 s), and
/// so is the margin target the client steers to; and it got there without a resync or a reset from
/// the connect on. At the base the clock started on a guess before the transport's first round
/// trip and the first jitter was half the round trip: the parity link reset 3 times and resynced.
#[test]
fn startup_lead_on_transatlantic_is_right_within_two_seconds() {
    let runs = [
        (0x5EED_0006u64, named_link("transatlantic", 0, 40_000)),
        (0x5EED_0007, named_link("transatlantic", 0, 40_000)),
        (0x5EED_0008, named_link("transatlantic", 0, 40_000)),
        (11, parity_link()),
        (0x5EED_0009, named_link("high-rtt", 1, 40_000)),
    ];
    for (seed, link) in runs {
        let (mut hub, _) = session(seed, setup, vec![link], play);
        hub.run_ms(40_000);
        let samples = hub.lead_samples(0);
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
        };
        let last = samples.last().map_or(0, |s| s.0);
        let steady = median(
            samples
                .iter()
                .filter(|s| s.0 + 10_000 >= last)
                .map(|s| s.2)
                .collect(),
        );
        let early: Vec<&(u64, f64, f64)> = samples.iter().filter(|s| (2_000..3_000).contains(&s.0)).collect();
        assert!(early.len() > 30, "seed {seed:#x}: {} samples", early.len());
        let margin = median(early.iter().map(|s| s.1).collect());
        let target = median(early.iter().map(|s| s.2).collect());
        println!("startup seed {seed:#x}: margin {margin} target {target} steady {steady}");
        assert!(
            (margin - steady).abs() <= 1.0,
            "seed {seed:#x}: margin {margin} at 2-3 s, steady target {steady}"
        );
        assert!(
            (target - steady).abs() <= 1.0,
            "seed {seed:#x}: target {target} at 2-3 s, steady target {steady}"
        );
        let whole = hub.client_world(0).resource::<ClientPrediction>().stats.clone();
        assert_eq!(
            (whole.resets, whole.resyncs, whole.corrections),
            (0, 0, 0),
            "seed {seed:#x}, from the connect: {whole:?}"
        );
    }
}

// ---- the harness's own tests ---------------------------------------------------------------------

/// Prediction reads its round trip from the transport's estimator, which follows the path: no
/// test sets it. A 20 -> 125 ms one-way step moves the estimate from about 40 to about 250 ms.
#[test]
fn prediction_reads_the_transport_rtt_estimator_through_a_step() {
    let base = named_link("rtt-step", 0, 30_000);
    let link = LinkSpec {
        phases: rtt_ramp(&base, 20, 125, 15_000, 0, 1),
        ..base
    };
    let (mut hub, _) = session(5, setup, vec![link], play);
    hub.run_until_us(14_000_000);
    let before = hub.client_link_stats(0).expect("link").rtt_ms;
    let fed = hub.client_world(0).resource::<ClientPrediction>().transport_rtt;
    assert_eq!(
        fed.map(|f| f.0),
        Some(before),
        "prediction gets the estimator's value"
    );
    assert!((30.0..80.0).contains(&before), "before the step: {before}");
    hub.run_until_us(30_000_000);
    let after = hub.client_link_stats(0).expect("link").rtt_ms;
    let fed = hub.client_world(0).resource::<ClientPrediction>().transport_rtt;
    assert_eq!(fed.map(|f| f.0), Some(after));
    assert!((220.0..300.0).contains(&after), "after the step: {after}");
}

/// Reliable frames are not guaranteed by the harness: at 20 % loss each way the state still gets
/// through, by the transport's own resends.
#[test]
fn reliable_frames_go_through_the_transport_resend_path() {
    let dir = Direction {
        latency_ms: 30,
        loss: 0.2,
        ..Direction::clean()
    };
    let link = LinkSpec {
        up: dir.clone(),
        down: dir,
        ..LinkSpec::default()
    };
    let (mut hub, heroes) = session(6, hub_setup, vec![link], play);
    hub.run_ms(10_000);
    // The state flows server -> client on the reliable channel: the server's transport resends it.
    let server = hub.server_link_stats(0).expect("server's link to the client");
    assert!(server.reliable_resends > 0, "{server:?}");
    assert!(server.packets_lost > 0, "{server:?}");
    assert_eq!(hub.report(0).server_reliable_resends, server.reliable_resends);
    let stats = hub.net().stats(hub.client_endpoint(0));
    assert!(
        stats.drops_of(DropCause::Random) > 0,
        "the impairment layer dropped datagrams"
    );
    assert!(
        client_hero(&mut hub, 0).is_some(),
        "the hero's reliable state arrived"
    );
    let sp = pos(hub.server_world(), heroes[0]);
    assert!(sp.x.abs() > 1.0, "the server ran the client's inputs: {sp}");
}

/// Clients update at their own phase inside the server's tick, and datagrams arrive at the
/// microsecond the impairment layer gives them, not on a tick boundary.
#[test]
fn clients_run_inside_the_tick_and_datagrams_arrive_between_ticks() {
    let link = named_link("good-wifi", 0, 10_000);
    let mut hub = TimedHub::new::<Game>(TimedOptions::default(), setup).expect("server");
    let a = hub.add_client::<Game>(link.clone()).expect("a");
    let b = hub.add_client::<Game>(link).expect("b");
    hub.run_ms(1);
    let ra = hub.report(a);
    let rb = hub.report(b);
    assert_ne!(ra.phase_us % TICK_US, 0, "inside the tick");
    assert_ne!(ra.phase_us, rb.phase_us, "each client its own phase");
    // The impairment layer: a datagram sent at 1,234 us with 20 ms of latency is there at 21,234
    // us, not before (sub-millisecond, so sub-tick).
    let net = regia_net_soak::impair::ImpairedNet::new(1);
    let spec = LinkSpec {
        up: Direction {
            latency_ms: 20,
            ..Direction::clean()
        },
        ..LinkSpec::default()
    };
    let from: std::net::SocketAddr = ([10, 9, 0, 1], 1).into();
    let to: std::net::SocketAddr = ([10, 9, 0, 2], 1).into();
    let ea = net.add_endpoint(from, spec).expect("a");
    let eb = net.add_endpoint(to, LinkSpec::default()).expect("b");
    let (mut sa, mut sb) = (net.socket(ea), net.socket(eb));
    net.advance_to_us(1_234);
    sa.send_to(&[7], to).expect("send");
    let mut buf = [0u8; 8];
    net.advance_to_us(21_233);
    assert_eq!(sb.recv_from(&mut buf).expect("io"), None, "not before its time");
    net.advance_to_us(21_234);
    assert!(
        sb.recv_from(&mut buf).expect("io").is_some(),
        "at its microsecond"
    );
}

/// Gilbert-Elliott burst loss runs under the session: bursts are dropped, the client stays.
#[test]
fn burst_loss_runs_under_a_session() {
    let dir = Direction {
        latency_ms: 25,
        burst: Some(GilbertElliott {
            p_good_to_bad: 0.01,
            p_bad_to_good: 0.25,
            loss_good: 0.0,
            loss_bad: 0.8,
        }),
        ..Direction::clean()
    };
    let link = LinkSpec {
        up: dir.clone(),
        down: dir,
        ..LinkSpec::default()
    };
    let (mut hub, _) = session(7, setup, vec![link], play);
    hub.run_ms(20_000);
    let stats = hub.net().stats(hub.client_endpoint(0));
    assert!(stats.drops_of(DropCause::Burst) > 0, "{stats:?}");
    let r = hub.report(0);
    assert_eq!(r.disconnects, 0, "{}", r.line());
    assert!(r.predicted_ticks > 1_000, "{}", r.line());
}

/// The same seed gives the same numbers; another seed gives other ones.
#[test]
fn the_same_seed_gives_the_same_numbers() {
    let run = |seed: u64| {
        let links = vec![
            named_link("bad-wifi", 0, 20_000),
            named_link("clock-drift", 0, 20_000),
        ];
        let (mut hub, _) = session(seed, setup, links, play);
        hub.run_ms(15_000);
        hub.reports()
    };
    let a = run(21);
    assert_eq!(a, run(21));
    assert_ne!(a, run(22));
}

// ---- spec 120 § 9.1 on the prediction path (N207, ADR-8312) -------------------------------------

/// The § 9.1 bar on the prediction path: the predicted tick never steps backward over 2 minutes
/// of good-wifi, bad-wifi, mobile-4g and rtt-step (its 40 -> 250 ms step at 60 s), with the
/// transport's RTT estimator and clients inside the server's tick.
#[test]
fn the_predicted_tick_never_steps_backward_on_jittery_paths() {
    const TWO_MIN_MS: u64 = 120_000;
    for (i, profile) in ["good-wifi", "bad-wifi", "mobile-4g", "rtt-step"]
        .into_iter()
        .enumerate()
    {
        let link = named_link(profile, 0, TWO_MIN_MS);
        let (mut hub, _) = session(0x5EED_0207 + i as u64, setup, vec![link], play);
        hub.run_ms(TWO_MIN_MS);
        let reports = hub.reports();
        print(profile, &reports);
        let r = &reports[0];
        assert!(r.predicted_ticks > 5_000, "{profile}: {}", r.line());
        assert_eq!(r.disconnects, 0, "{profile}: {}", r.line());
        assert_eq!(
            r.backward_steps,
            0,
            "{profile}: the predicted tick stepped back: {}",
            r.line()
        );
    }
}
