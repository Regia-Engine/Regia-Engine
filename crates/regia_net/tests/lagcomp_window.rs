// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 9.6 (ADR-8315): each shooter's rewind window comes from the server's own
//! measurements, and the view tick a command carries is checked against it when the command is
//! consumed. A server world driven tick by tick; no wall clock.

use regia_core::math::Ray;
use regia_ecs::prelude::*;
use regia_net::interp::{ViewTickWire, set_view_tick};
use regia_net::lagcomp::{
    LagCompWindows, MAX_REWIND_CEILING_MS, VIEW_JITTER_FACTOR, VIEW_SLACK_MS, install_lag_compensation,
    lagcomp_record, set_shooter_rtt,
};
use regia_net::predict::ServerPrediction;
use regia_net::runtime::{NetNode, ensure_resources, server_add_peer, server_remove_peer};
use regia_net::*;
use regia_scene::transform::Transform;

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;
const PEER: PeerId = PeerId(1);
/// The round trip and its variation the server measured for `PEER`: a 5 ms client.
const RTT_MS: f64 = 5.0;
const JITTER_MS: f64 = 0.5;
/// Its commands arrive two ticks early (the default margin target).
const MARGIN: f32 = 2.0;
/// Its interpolation delay at the defaults: two sends of two ticks (66.7 ms).
const DELAY_MS: f64 = 4.0 * MSPT;

fn shooter() -> NetPlayerId {
    NetPlayerId { peer: PEER, local: 0 }
}

struct Server {
    world: World,
    player: Entity,
    target: Entity,
    tick: u32,
}

impl Server {
    /// A dedicated server with one remote shooter, a target that never moves, and `rtt` measured
    /// for the shooter's peer (`None`: never measured).
    fn new(rtt: Option<(f64, f64)>) -> Self {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        server_add_peer(&mut world, PEER);
        if let Some((rtt, jitter)) = rtt {
            set_shooter_rtt(&mut world, PEER, rtt, jitter);
        }
        world.init_resource::<ServerPrediction>();
        world
            .resource_mut::<ServerPrediction>()
            .players
            .entry(shooter())
            .or_default()
            .margin = MARGIN;
        let target = world
            .spawn((
                NetId(1),
                Transform::from_translation(glam::Vec3::ZERO),
                HitShapes {
                    shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body")],
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        let player = world.spawn(NetPlayer { id: shooter() }).id();
        Self {
            world,
            player,
            target,
            // Far enough in that the oldest view in these tests is still a positive tick.
            tick: 1_000,
        }
    }

    fn now(&self) -> f64 {
        f64::from(self.tick)
    }

    /// Consumes the shooter's command for the current tick carrying `view` and `delay_ms`; returns
    /// the view tick the server kept.
    fn command(&mut self, view: f64, delay_ms: f64) -> f64 {
        self.world.resource_mut::<NetNode>().tick = NetTick(self.tick);
        set_view_tick(
            &mut self.world,
            self.player,
            Some(ViewTickWire::new(view, delay_ms)),
        );
        self.world
            .get::<NetViewTick>(self.player)
            .map_or(f64::NAN, |v| v.tick)
    }

    /// Records the current tick and moves on.
    fn end_tick(&mut self) {
        self.world.resource_mut::<NetNode>().tick = NetTick(self.tick);
        lagcomp_record(&mut self.world);
        self.tick += 1;
    }

    /// An honest command: the view an interpolation clock `DELAY_MS` behind shows over this link.
    fn honest(&mut self) -> f64 {
        let age_ms = RTT_MS + f64::from(MARGIN) * MSPT + DELAY_MS;
        let view = self.now() - age_ms / MSPT;
        let kept = self.command(view, DELAY_MS);
        assert!(
            (kept - view).abs() <= 1.0 / 256.0,
            "tick {}: an honest view {view} is kept as carried, got {kept}",
            self.tick
        );
        kept
    }

    /// `seconds` of honest play.
    fn play(&mut self, seconds: u32) {
        for _ in 0..seconds * 60 {
            self.honest();
            self.end_tick();
        }
    }

    fn window(&self) -> regia_net::lagcomp::ShooterWindow {
        *self
            .world
            .resource::<LagCompWindows>()
            .get(shooter())
            .expect("the shooter has a window")
    }
}

/// The acceptance row of N210: a 5 ms client claiming a 250 ms-old view (and a 200 ms
/// interpolation delay to go with it) is clamped to its own expected age, the rewound query runs
/// there, the clamp is counted, and a hit claim at the 250 ms-old tick is refused.
#[test]
fn a_5ms_client_claiming_a_250ms_old_view_is_clamped_to_its_own_expected_age() {
    let mut s = Server::new(Some((RTT_MS, JITTER_MS)));
    // Long enough for the server's bound on the interpolation delay to come down from the most a
    // client may use (250 ms) to this link's target (2 % of real time: about 9 s).
    s.play(20);
    let prev = s.honest();
    s.end_tick();
    let before = s.window();
    assert_eq!(before.clamped(), 0, "honest play is never clamped: {before:?}");
    assert!(before.measured);
    let stale = s.now() - 250.0 / MSPT;
    let kept = s.command(stale, 200.0);
    let w = s.window();
    let expected_ms = RTT_MS + f64::from(MARGIN) * MSPT + w.interp_ms + MSPT;
    assert!(
        (w.expected_age_ms - expected_ms).abs() < 1e-9,
        "expected age = RTT + lead + interpolation + 1 tick: {w:?}"
    );
    assert!(
        w.interp_ms < 100.0,
        "the 200 ms delay the client claims is not believed: {w:?}"
    );
    let allowance = VIEW_SLACK_MS + VIEW_JITTER_FACTOR * JITTER_MS;
    assert_eq!(w.allowance_ms, allowance);
    let floor = s.now() - (expected_ms + allowance) / MSPT;
    // The nearest allowed tick: the window's edge, or the allowance behind the previous view when
    // that is later.
    let allowed = floor.max(prev - allowance / MSPT);
    assert!(
        (kept - allowed).abs() < 1e-9,
        "the 250 ms-old view {stale} is clamped to the shooter's own window (edge {floor}, previous \
         view {prev}), kept {kept}"
    );
    assert!(kept >= floor);
    assert!(
        (s.now() - kept) * MSPT < 200.0,
        "a 5 ms client gets far less than 250 ms of rewind: {} ms",
        (s.now() - kept) * MSPT
    );
    assert_eq!(w.clamped_old, 1, "the clamp is counted: {w:?}");
    let lc = LagCompensation::new(&s.world).expect("server");
    assert_eq!(
        lc.rewind_tick(shooter()).expect("shooter").0,
        kept,
        "queries rewind to the clamped tick"
    );
    let claim = HitClaim {
        target: s.target,
        tag: "body".into(),
        origin: glam::Vec3::new(0.0, 0.0, -5.0),
        direction: glam::Vec3::Z,
        view_tick: stale,
        weapon_seq: 1,
    };
    assert_eq!(
        lc.validate_claim(shooter(), &claim, glam::Vec3::new(0.0, 0.0, -5.0))
            .expect("shooter"),
        ClaimVerdict::Invalid(ClaimInvalid::ViewTickOutOfRange),
        "a claim at the 250 ms-old tick is refused"
    );
    let ray = Ray::new(glam::Vec3::new(0.0, 0.0, -5.0), glam::Vec3::Z);
    assert!(
        lc.raycast(shooter(), ray, 100.0, &HitFilter::default())
            .expect("query")
            .is_some(),
        "the rewound query still runs"
    );
    let all = s.world.resource::<LagCompWindows>().all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].clamped_old, 1, "lagcomp.stats `windows` shows it: {all:?}");
}

/// A view tick that runs backward past the allowance is clamped to the allowance and counted;
/// small steps back (jitter) are not. The views here are young (one tick old), so the steps back
/// stay inside the window and only the backward check sees them.
#[test]
fn a_view_tick_running_backward_is_clamped_and_counted() {
    let mut s = Server::new(Some((RTT_MS, JITTER_MS)));
    s.play(2);
    let fresh = s.now() - 1.0;
    let prev = s.command(fresh, DELAY_MS);
    assert!((prev - fresh).abs() <= 1.0 / 256.0);
    s.end_tick();
    let allowance_ticks = (VIEW_SLACK_MS + VIEW_JITTER_FACTOR * JITTER_MS) / MSPT;
    // One tick back from the previous view: inside the allowance, kept as carried.
    let small = s.command(prev - 1.0, DELAY_MS);
    assert!(
        (small - (prev - 1.0)).abs() <= 1.0 / 256.0,
        "{small} vs {}",
        prev - 1.0
    );
    assert_eq!(s.window().clamped(), 0);
    s.end_tick();
    // Four and a half ticks back from that: past the allowance (about 3 ticks), inside the window.
    let back = s.command(small - 4.5, DELAY_MS);
    let w = s.window();
    assert!(
        s.now() - (small - 4.5) < w.window_ms / MSPT,
        "the step back stays inside the window: {w:?}"
    );
    assert!(
        (back - (small - allowance_ticks)).abs() < 1e-9,
        "clamped to the allowance behind the previous view: kept {back}, previous {small}"
    );
    assert_eq!(w.clamped_backward, 1, "{w:?}");
    assert_eq!(w.clamped_old, 0, "{w:?}");
    s.end_tick();
    // Honest play goes on unclamped.
    s.play(1);
    assert_eq!(s.window().clamped(), 1);
}

/// A view tick in the future is the present.
#[test]
fn a_view_tick_in_the_future_is_clamped_to_the_present() {
    let mut s = Server::new(Some((RTT_MS, JITTER_MS)));
    s.play(1);
    let kept = s.command(s.now() + 4.0, DELAY_MS);
    assert_eq!(kept, s.now());
    assert_eq!(s.window().clamped_future, 1);
}

/// A shooter whose round trip the server never measured gets the project maximum (default
/// 1,000 ms), so a long view is still judged where it was seen; the backward check still runs.
#[test]
fn an_unmeasured_shooter_gets_the_project_maximum() {
    let mut s = Server::new(None);
    s.play(1);
    let w = s.window();
    assert!(!w.measured);
    assert_eq!(w.window_ms, MAX_REWIND_CEILING_MS);
    let old = s.now() - 900.0 / MSPT;
    // Far behind the previous view: the backward check clamps it.
    let kept = s.command(old, DELAY_MS);
    assert!(kept > old);
    assert_eq!(s.window().clamped_backward, 1);
}

/// The window is the measured one at 450 ms and 650 ms round trips (the high-rtt profile), and a
/// view of that age is kept exactly (the 250 ms default before spec 120 clamped it).
#[test]
fn high_round_trips_keep_their_view_ticks() {
    for rtt in [450.0, 650.0] {
        let mut s = Server::new(Some((rtt, 20.0)));
        for _ in 0..600 {
            let view = s.now() - (rtt + f64::from(MARGIN) * MSPT + DELAY_MS) / MSPT;
            let kept = s.command(view, DELAY_MS);
            assert!(
                (kept - view).abs() <= 1.0 / 256.0,
                "rtt {rtt}: {view} kept as {kept}"
            );
            s.end_tick();
        }
        let w = s.window();
        assert_eq!(w.clamped(), 0, "rtt {rtt}: {w:?}");
        assert!(w.window_ms > rtt && w.window_ms <= MAX_REWIND_CEILING_MS, "{w:?}");
    }
}

/// A peer that leaves takes its window and its round trip with it.
#[test]
fn a_peer_that_leaves_is_forgotten() {
    let mut s = Server::new(Some((RTT_MS, JITTER_MS)));
    s.play(1);
    assert!(s.world.resource::<LagCompWindows>().get(shooter()).is_some());
    server_remove_peer(&mut s.world, PEER);
    assert!(s.world.resource::<LagCompWindows>().get(shooter()).is_none());
    assert!(s.world.resource::<LagCompWindows>().all().is_empty());
}

/// Off a server (or for an entity that is not a player) the carried tick is kept as is.
#[test]
fn clients_and_non_players_keep_the_carried_tick() {
    let mut world = World::new();
    let e = world.spawn_empty().id();
    set_view_tick(&mut world, e, Some(ViewTickWire::new(5.0, 70.0)));
    assert!((world.get::<NetViewTick>(e).expect("view").tick - 5.0).abs() < 1e-9);
    let mut s = Server::new(Some((RTT_MS, JITTER_MS)));
    let other = s.world.spawn_empty().id();
    s.world.resource_mut::<NetNode>().tick = NetTick(100);
    set_view_tick(&mut s.world, other, Some(ViewTickWire::new(1.0, 70.0)));
    assert!((s.world.get::<NetViewTick>(other).expect("view").tick - 1.0).abs() < 1e-9);
}
