// SPDX-License-Identifier: MPL-2.0
//! Spec 73 § 13: client prediction and server reconciliation over the in-memory `LoopbackHub`
//! (fixed steps, latency in ticks; "latency 6 ticks" = 100 ms at 60 Hz).

use std::collections::{BTreeMap, BTreeSet};

use glam::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_net::interp::{NetInterpolation, install_interpolation, net_interpolate, view_tick_to_wire};
use regia_net::predict::{
    ClientPrediction, PredictionStats, ServerPrediction, force_correction, set_transport_rtt,
};
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::runtime::NetNode;
use regia_net::settings::PredictionSettings;
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::*;
use regia_scene::transform::Transform;

// ---- the test game ------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    x: i8,
    jump: bool,
    fire: bool,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        // One-shot fields do not repeat.
        Self {
            jump: false,
            fire: false,
            ..last.clone()
        }
    }
}

/// The hero's predicted movement state.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Motion {
    vy: f32,
    grounded: bool,
}

/// A carried shield: a second predicted entity of the same player.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Shield {
    side: f32,
}

/// A projectile spawned by predicted code.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Bolt {
    speed: f32,
}

/// The hero stands on the tick-driven platform.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Rider {
    height: f32,
}

/// Server only: refuse shots.
#[derive(Resource)]
struct Deny;

/// Client: first-time effects per tick.
#[derive(Resource, Default)]
struct Played(BTreeMap<u32, u32>);

#[derive(Message, Debug, Clone, Copy)]
struct Ping;

/// Client: the input of each client tick.
#[derive(Resource, Clone, Copy)]
struct Script(fn(u32) -> Pad);

/// Server: the commands consumed for player 1, by tick.
#[derive(Resource, Default)]
struct Consumed(Vec<(u32, Pad)>);

type Inputs<'w, 's> = Query<'w, 's, (&'static NetPlayer, &'static InputCommand<Pad>)>;

fn pad_of(inputs: &Inputs, player: Option<NetPlayerId>) -> Pad {
    let Some(player) = player else {
        return Pad::default();
    };
    inputs
        .iter()
        .find(|(p, _)| p.id == player)
        .map(|(_, c)| c.value.clone())
        .unwrap_or_default()
}

type HeroData = (
    &'static mut Transform,
    &'static mut Motion,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
    Option<&'static Rider>,
);
type PlatformFilter = (With<TickDrivenMotion>, Without<Motion>);
type ShieldData = (
    &'static mut Transform,
    &'static Shield,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);
type ShooterData = (
    &'static Transform,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);
type BoltData = (&'static mut Transform, &'static Bolt, Option<&'static NetRole>);
type BoltFilter = (Without<Motion>, Without<Shield>);

fn step_hero(
    cx: PredictionCx,
    inputs: Inputs,
    mut heroes: Query<HeroData, Without<TickDrivenMotion>>,
    platforms: Query<&Transform, PlatformFilter>,
) {
    let floor = platforms.iter().next().map(|t| t.translation.y);
    for (mut t, mut m, owner, role, rider) in &mut heroes {
        if !cx.simulates(role) {
            continue;
        }
        let pad = pad_of(&inputs, cx.owner_player(owner));
        t.translation.x += f32::from(pad.x) * 0.1;
        if let (Some(r), Some(y)) = (rider, floor) {
            t.translation.y = y + r.height;
            continue;
        }
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

fn step_shield(cx: PredictionCx, inputs: Inputs, mut shields: Query<ShieldData, Without<Motion>>) {
    for (mut t, s, owner, role) in &mut shields {
        if !cx.simulates(role) {
            continue;
        }
        let pad = pad_of(&inputs, cx.owner_player(owner));
        t.translation.x += f32::from(pad.x) * 0.1;
        t.translation.z = s.side;
    }
}

fn fire(
    mut cx: PredictionCx,
    inputs: Inputs,
    heroes: Query<ShooterData, With<Motion>>,
    deny: Option<Res<Deny>>,
) {
    let mut shots = Vec::new();
    for (t, owner, role) in &heroes {
        if !cx.simulates(role) {
            continue;
        }
        let Some(player) = cx.owner_player(owner) else {
            continue;
        };
        if pad_of(&inputs, Some(player)).fire {
            shots.push((player, t.translation));
        }
    }
    for (player, at) in shots {
        if cx.is_server() && deny.is_some() {
            continue;
        }
        cx.spawn_predicted(
            player,
            (
                Replicated {
                    transform: TransformReplication::PositionOnly,
                    ..Replicated::default()
                },
                Transform::from_translation(at),
                Bolt { speed: 0.5 },
                NetPredicted,
                NetOwner(player.peer),
            ),
        );
    }
}

fn step_bolt(cx: PredictionCx, mut bolts: Query<BoltData, BoltFilter>) {
    for (mut t, b, role) in &mut bolts {
        if cx.simulates(role) {
            t.translation.x += b.speed;
        }
    }
}

fn effects(mut cx: PredictionCx, played: Option<ResMut<Played>>) {
    if cx.first_time()
        && let Some(mut p) = played
    {
        *p.0.entry(cx.tick().0).or_insert(0) += 1;
    }
    cx.send_local_event(Ping);
}

fn setup(world: &mut World) {
    register_replicated::<Motion>(world, ReplicateOptions::default());
    register_replicated::<Shield>(world, ReplicateOptions::default());
    register_replicated::<Bolt>(world, ReplicateOptions::default());
    register_replicated::<Rider>(world, ReplicateOptions::default());
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    predict::<Motion>(world, Tolerance::Exact).expect("motion");
    add_predicted_systems(world, (step_hero, step_shield, fire, step_bolt, effects).chain());
    world.init_resource::<Messages<Ping>>();
}

fn feed(script: Res<Script>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<Pad>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

fn log_consumed(mut log: ResMut<Consumed>, q: Query<(&NetPlayer, &InputCommand<Pad>)>) {
    for (p, c) in &q {
        if p.id.peer == PeerId(1) {
            log.0.push((c.tick.0, c.value.clone()));
        }
    }
}

// ---- rig ----------------------------------------------------------------------------------------

struct Rig {
    hub: LoopbackHub,
    hero: Entity,
}

fn idle(_: u32) -> Pad {
    Pad::default()
}

fn walk(_: u32) -> Pad {
    Pad {
        x: 1,
        ..Pad::default()
    }
}

/// A dedicated server, one client at `delay` ticks each way, a hero owned and predicted by it.
fn rig(seed: u64, delay: u64, script: fn(u32) -> Pad, tweak: fn(&mut PredictionSettings)) -> Rig {
    rig_on(setup, seed, delay, script, tweak)
}

/// [`rig`] with another world setup (it must call `setup`).
fn rig_on(
    setup: fn(&mut World),
    seed: u64,
    delay: u64,
    script: fn(u32) -> Pad,
    tweak: fn(&mut PredictionSettings),
) -> Rig {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    hub.down = LinkConditions {
        delay_ticks: delay,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    let c = hub.add_client();
    for w in [&mut hub.server.world, &mut hub.clients[c].world] {
        tweak(&mut w.resource_mut::<NetSettings>().prediction);
    }
    let rtt_ticks = 2 * delay + 1;
    set_transport_rtt(&mut hub.clients[c].world, rtt_ticks as f64 * 1000.0 / 60.0, 0.0);
    hub.clients[c].world.insert_resource(Script(script));
    hub.clients[c].world.init_resource::<Played>();
    hub.clients[c].gameplay.add_systems(feed);
    hub.server.world.init_resource::<Consumed>();
    hub.server.gameplay.add_systems(log_consumed);
    let hero = hub
        .server
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
            NetOwner(PeerId(1)),
        ))
        .id();
    Rig { hub, hero }
}

fn no_tweak(_: &mut PredictionSettings) {}

fn stats(hub: &LoopbackHub) -> PredictionStats {
    hub.clients[0].world.resource::<ClientPrediction>().stats.clone()
}

fn client_hero(hub: &mut LoopbackHub) -> Option<Entity> {
    let w = &mut hub.clients[0].world;
    let mut q = w.query_filtered::<(Entity, &NetRole), With<Motion>>();
    q.iter(w).find(|(_, r)| **r == NetRole::Predicted).map(|(e, _)| e)
}

fn pos(world: &World, e: Entity) -> Vec3 {
    world
        .get::<Transform>(e)
        .map(|t| t.translation)
        .unwrap_or(Vec3::NAN)
}

fn player_stats(hub: &LoopbackHub, peer: u32) -> regia_net::predict::PlayerInputStats {
    hub.server
        .world
        .resource::<ServerPrediction>()
        .players
        .get(&NetPlayerId {
            peer: PeerId(peer),
            local: 0,
        })
        .cloned()
        .unwrap_or_default()
}

fn is_input_frame(d: &regia_net::runtime::Datagram) -> bool {
    d.bytes.first() == Some(&5)
}

/// A scripted 600-tick recording: idle, then walking left and right with jumps, then idle.
fn recording(t: u32) -> Pad {
    if !(120..600).contains(&t) {
        return Pad::default();
    }
    Pad {
        x: if (t / 37).is_multiple_of(3) { -1 } else { 1 },
        jump: t.is_multiple_of(50),
        fire: false,
    }
}

// ---- tests --------------------------------------------------------------------------------------

/// § 13 test 1: no false corrections under latency, jitter and loss; both sides end bit-identical.
#[test]
fn no_false_corrections_under_latency_jitter_and_loss() {
    let mut r = rig(11, 6, recording, no_tweak);
    // 2 ticks of jitter (reordering adds 1..=3 ticks) and 5 % loss both ways.
    for c in [&mut r.hub.down, &mut r.hub.up] {
        c.loss = 0.05;
        c.reorder = 0.2;
    }
    set_transport_rtt(
        &mut r.hub.clients[0].world,
        13.0 * 1000.0 / 60.0,
        2.0 * 1000.0 / 60.0,
    );
    r.hub.run(900);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    assert_eq!(s.resyncs, 0, "{s:?}");
    let ch = client_hero(&mut r.hub).expect("predicted copy");
    let sp = pos(&r.hub.server.world, r.hero);
    let cp = pos(&r.hub.clients[0].world, ch);
    assert!(sp.x.abs() > 1.0, "the hero walked: {sp}");
    assert_eq!(
        sp.to_array().map(f32::to_bits),
        cp.to_array().map(f32::to_bits),
        "bit-identical end position"
    );
    assert_eq!(
        r.hub.server.world.get::<Motion>(r.hero),
        r.hub.clients[0].world.get::<Motion>(ch)
    );
}

/// § 13 test 2: a forced 1 m error is one correction; the client follows the server after it and
/// the rendered offset decays below 1 mm within 400 ms of simulated time.
#[test]
fn one_forced_correction_smooths_out() {
    let mut r = rig(12, 6, walk, no_tweak);
    while r.hub.server_tick().0 < 300 {
        r.hub.step();
    }
    let applied =
        force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 1.0)).expect("server");
    assert_eq!(applied.0, 300);
    let mut steps = 0;
    while stats(&r.hub).corrections == 0 {
        r.hub.step();
        steps += 1;
        assert!(steps < 60, "the correction arrives");
    }
    let ch = client_hero(&mut r.hub).expect("copy");
    let off = r.hub.clients[0]
        .world
        .get::<NetVisualOffset>(ch)
        .copied()
        .expect("offset");
    assert!(off.translation.length() > 0.5, "smoothed, not snapped: {off:?}");
    // 400 ms = 24 ticks.
    r.hub.run(24);
    let off = r.hub.clients[0]
        .world
        .get::<NetVisualOffset>(ch)
        .copied()
        .unwrap_or_default();
    assert!(off.translation.length() < 1e-3, "{off:?}");
    r.hub.run(200);
    assert_eq!(stats(&r.hub).corrections, 1, "exactly one correction");
    // The client follows the server: its z is the server's from the correction on.
    let ch = client_hero(&mut r.hub).expect("copy");
    assert_eq!(
        pos(&r.hub.clients[0].world, ch).z,
        pos(&r.hub.server.world, r.hero).z
    );
}

/// § 13 test 3: a 3 m error snaps (no smoothing); a teleport snaps without a correction.
#[test]
fn large_errors_and_teleports_snap() {
    let mut r = rig(13, 6, walk, no_tweak);
    r.hub.run(200);
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 3.0)).expect("server");
    let before = stats(&r.hub).corrections;
    while stats(&r.hub).corrections == before {
        r.hub.step();
    }
    let ch = client_hero(&mut r.hub).expect("copy");
    let off = r.hub.clients[0]
        .world
        .get::<NetVisualOffset>(ch)
        .copied()
        .unwrap_or_default();
    assert!(off.is_zero(), "3 m snaps: {off:?}");
    let s = stats(&r.hub);
    assert_eq!(s.snaps, 1, "the snap is counted: {s:?}");
    assert!((s.last_correction_m - 3.0).abs() < 1e-3, "{s:?}");
    r.hub.run(60);
    let corrections = stats(&r.hub).corrections;
    r.hub
        .server
        .world
        .get_mut::<Transform>(r.hero)
        .expect("hero")
        .translation
        .y += 50.0;
    assert!(net_teleport(&mut r.hub.server.world, r.hero));
    r.hub.run(30);
    let s = stats(&r.hub);
    assert!(s.teleports >= 1, "{s:?}");
    assert_eq!(s.corrections, corrections, "a teleport is no correction");
    let off = r.hub.clients[0]
        .world
        .get::<NetVisualOffset>(ch)
        .copied()
        .unwrap_or_default();
    assert!(off.is_zero(), "{off:?}");
    r.hub.run(60);
    assert!((pos(&r.hub.clients[0].world, ch).y - 50.0).abs() < 1e-3);
}

/// § 13 test 4a: redundancy recovers 15 consecutive lost input packets (with a buffer margin that
/// covers the gap the server never misses a command).
#[test]
fn redundancy_covers_fifteen_lost_packets() {
    let mut r = rig(14, 6, walk, |s| {
        s.margin_target_ticks = 16;
        s.margin_max_ticks = 16;
    });
    r.hub.run(200);
    let missing = player_stats(&r.hub, 1).missing;
    r.hub.up_drop = Some(Box::new(|step, _, d| {
        (200..215).contains(&step) && is_input_frame(d)
    }));
    r.hub.run(200);
    let st = player_stats(&r.hub, 1);
    assert_eq!(st.missing, missing, "no command lost: {st:?}");
    assert_eq!(stats(&r.hub).corrections, 0);
}

fn switch_at_450(t: u32) -> Pad {
    Pad {
        x: if t < 450 { 1 } else { -1 },
        ..Pad::default()
    }
}

/// § 13 test 4b: 20 consecutive lost packets with the default margin make the server repeat; with
/// a constant input the repeats are right (no correction); when the input changes in the gap the
/// client corrects, only within the gap, and ends where the server is (ADR-1793 (4)).
#[test]
fn twenty_lost_packets_repeat_and_correct_only_when_input_changed() {
    for (script, changes) in [(walk as fn(u32) -> Pad, false), (switch_at_450, true)] {
        let mut r = rig(15, 6, script, no_tweak);
        while r.hub.clients[0]
            .world
            .resource::<ClientPrediction>()
            .client_tick()
            .0
            < 440
        {
            r.hub.step();
        }
        let gap_start = r.hub.step_count();
        r.hub.up_drop = Some(Box::new(move |step, _, d| {
            (gap_start..gap_start + 20).contains(&step) && is_input_frame(d)
        }));
        r.hub.run(60);
        let after_gap = stats(&r.hub).corrections;
        r.hub.run(200);
        let st = player_stats(&r.hub, 1);
        assert!(st.missing > 0, "the server repeated: {st:?}");
        let s = stats(&r.hub);
        if changes {
            assert!(s.corrections >= 1, "{s:?}");
            assert!(
                s.corrections <= st.missing,
                "at most one per repeated tick: {s:?} {st:?}"
            );
            assert_eq!(
                s.corrections, after_gap,
                "no correction after the stream recovered"
            );
        } else {
            assert_eq!(s.corrections, 0, "constant input: repeats are right: {s:?}");
        }
        let ch = client_hero(&mut r.hub).expect("copy");
        r.hub.clients[0].world.insert_resource(Script(idle));
        r.hub.run(100);
        assert_eq!(pos(&r.hub.clients[0].world, ch), pos(&r.hub.server.world, r.hero));
    }
}

fn held_jump(_: u32) -> Pad {
    Pad {
        x: 1,
        jump: true,
        fire: false,
    }
}

/// § 13 test 5: with `on_missing` clearing `jump`, a held-forward input repeats and a jump does not.
#[test]
fn missing_input_repeats_forward_not_jump() {
    let mut r = rig(16, 3, held_jump, no_tweak);
    r.hub.run(120);
    let stop = r.hub.step_count();
    r.hub.up_drop = Some(Box::new(move |step, _, d| step > stop && is_input_frame(d)));
    r.hub.run(80);
    let log = &r.hub.server.world.resource::<Consumed>().0;
    let last_real = log
        .iter()
        .rev()
        .find(|(_, p)| p.jump)
        .map(|(t, _)| *t)
        .expect("jumps arrived");
    let repeats: Vec<&Pad> = log
        .iter()
        .filter(|(t, _)| *t > last_real)
        .map(|(_, p)| p)
        .collect();
    assert!(repeats.len() > 40, "{}", repeats.len());
    assert!(
        repeats.iter().all(|p| p.x == 1 && !p.jump),
        "forward repeats, jump does not"
    );
}

/// § 13 test 6a: a start lead 20 ticks too large resyncs once.
#[test]
fn lead_too_large_resyncs_once() {
    let mut hub = rig(17, 6, walk, no_tweak).hub;
    hub.clients[0]
        .world
        .resource_mut::<ClientPrediction>()
        .start_offset_ticks = 20;
    hub.run(400);
    assert_eq!(stats(&hub).resyncs, 1, "{:?}", stats(&hub));
}

/// § 5.1: a start lead too short resyncs forward once, both by the negative-margin rule (6 ticks
/// short, inside the dilation window) and by the window rule (20 ticks short). The server's margin
/// is an average over the last `MARGIN_WINDOW` commands, so for a while after the jump it still
/// holds pre-jump samples; that stale part is the jump already made and must not count as a new
/// negative margin or a new error (the profile B two-machine run resynced +12, +9, -9).
#[test]
fn lead_too_small_resyncs_once() {
    for offset in [-6, -20] {
        let mut hub = rig(19, 6, walk, no_tweak).hub;
        hub.clients[0]
            .world
            .resource_mut::<ClientPrediction>()
            .start_offset_ticks = offset;
        hub.run(400);
        let s = stats(&hub);
        assert_eq!(s.resyncs, 1, "offset {offset}: {s:?}");
        let cp = hub.clients[0].world.resource::<ClientPrediction>();
        let settings = hub.clients[0].world.resource::<NetSettings>().prediction.clone();
        let target = cp.margin_target(&settings);
        let margin = cp.reported_margin().expect("feedback");
        assert!(
            (margin - target).abs() <= 1.0,
            "offset {offset}: margin {margin} target {target}"
        );
    }
}

/// § 5.1 under the conditioned profile B shape (reordering on top of the delay, 2 % loss): a start
/// lead 12 ticks short resyncs once and then settles; no chain of follow-up resyncs.
#[test]
fn lead_too_small_under_reorder_and_loss_resyncs_once() {
    let mut r = rig(23, 3, walk, no_tweak);
    for c in [&mut r.hub.down, &mut r.hub.up] {
        c.loss = 0.02;
        c.reorder = 0.05;
    }
    set_transport_rtt(&mut r.hub.clients[0].world, 7.0 * 1000.0 / 60.0, 10.0);
    r.hub.clients[0]
        .world
        .resource_mut::<ClientPrediction>()
        .start_offset_ticks = -12;
    r.hub.run(900);
    let s = stats(&r.hub);
    assert_eq!(s.resyncs, 1, "{s:?}");
}

/// § 5.1, both directions under the profile B shape and three conditioner seeds: 12 ticks short
/// (a forward jump) and 20 ticks long (a backward jump, after which the input stream holds until
/// it passes the newest tick sent) each resync once, and the margin then sits on its target.
#[test]
fn lead_errors_both_ways_resync_once_across_seeds() {
    for seed in [23, 24, 25] {
        for offset in [-12, 20] {
            let mut r = rig(seed, 3, walk, no_tweak);
            for c in [&mut r.hub.down, &mut r.hub.up] {
                c.loss = 0.02;
                c.reorder = 0.05;
            }
            set_transport_rtt(&mut r.hub.clients[0].world, 7.0 * 1000.0 / 60.0, 10.0);
            r.hub.clients[0]
                .world
                .resource_mut::<ClientPrediction>()
                .start_offset_ticks = offset;
            r.hub.run(900);
            let s = stats(&r.hub);
            assert_eq!(s.resyncs, 1, "seed {seed} offset {offset}: {s:?}");
            let cp = r.hub.clients[0].world.resource::<ClientPrediction>();
            let settings = r.hub.clients[0]
                .world
                .resource::<NetSettings>()
                .prediction
                .clone();
            let target = cp.margin_target(&settings);
            let margin = cp.reported_margin().expect("feedback");
            assert!(
                (margin - target).abs() <= 1.5,
                "seed {seed} offset {offset}: margin {margin} target {target}"
            );
        }
    }
}

/// § 13 test 6b: 3 ticks off converges within 120 ticks by dilation alone; no early or late input
/// after convergence. Too short by 3 would make the margin negative, and a negative margin for 10
/// reports resyncs by § 5.1 itself, so the short side is tested at 2 ticks (ADR-1793 (4)).
#[test]
fn small_lead_error_converges_by_dilation() {
    for offset in [3, -2] {
        let mut hub = rig(18, 6, walk, no_tweak).hub;
        hub.clients[0]
            .world
            .resource_mut::<ClientPrediction>()
            .start_offset_ticks = offset;
        // Prediction starts with the first state (a few ticks in); 120 ticks after that.
        hub.run(8 + 120);
        let cp = hub.clients[0].world.resource::<ClientPrediction>();
        let s = hub.clients[0].world.resource::<NetSettings>().prediction.clone();
        let target = cp.margin_target(&s);
        let margin = cp.reported_margin().expect("feedback");
        assert_eq!(cp.stats.resyncs, 0, "offset {offset}: {:?}", cp.stats);
        assert!(cp.stats.dilated != 0, "offset {offset}: dilation ran");
        assert!(
            (margin - target).abs() <= 1.0,
            "offset {offset}: margin {margin} target {target}"
        );
        let st = player_stats(&hub, 1);
        hub.run(300);
        let st2 = player_stats(&hub, 1);
        assert_eq!((st2.early, st2.late), (st.early, st.late), "offset {offset}");
        assert_eq!(st2.missing, st.missing, "offset {offset}");
    }
}

/// § 13 test 7: a client that sends two commands per tick moves no faster than one that sends one;
/// the excess are dropped as early.
#[test]
fn two_commands_per_tick_gain_no_speed() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 19, setup);
    hub.down.delay_ticks = 3;
    hub.up.delay_ticks = 3;
    let mut heroes = Vec::new();
    for i in 0..2 {
        let c = hub.add_client();
        set_transport_rtt(&mut hub.clients[c].world, 7.0 * 1000.0 / 60.0, 0.0);
        hub.clients[c].world.insert_resource(Script(walk));
        hub.clients[c].gameplay.add_systems(feed);
        if i == 1 {
            hub.clients[c]
                .world
                .resource_mut::<ClientPrediction>()
                .ticks_per_step = Some(2);
        }
        heroes.push(
            hub.server
                .world
                .spawn((
                    Replicated::default(),
                    Transform::default(),
                    Motion {
                        vy: 0.0,
                        grounded: true,
                    },
                    NetPredicted,
                    NetOwner(hub.clients[c].peer),
                ))
                .id(),
        );
    }
    hub.run(400);
    let honest = pos(&hub.server.world, heroes[0]).x;
    let abuser = pos(&hub.server.world, heroes[1]).x;
    assert!(honest > 30.0, "{honest}");
    assert!(abuser <= honest + 0.5, "honest {honest} abuser {abuser}");
    assert!(player_stats(&hub, 2).early > 0, "excess dropped as early");
    assert_eq!(player_stats(&hub, 1).early, 0);
}

/// § 13 test 8: a mismatch on one predicted entity rolls back every predicted entity of the player.
#[test]
fn rollback_moves_every_predicted_entity() {
    let mut r = rig(20, 6, walk, no_tweak);
    r.hub.server.world.spawn((
        Replicated::default(),
        Transform::default(),
        Shield { side: 0.5 },
        NetPredicted,
        NetOwner(PeerId(1)),
    ));
    r.hub.run(200);
    assert_eq!(stats(&r.hub).corrections, 0);
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.5)).expect("server");
    r.hub.run(30);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 1, "{s:?}");
    assert_eq!(s.last_rollback_entities, 2, "hero and shield move back together");
}

/// § 6.3 step 1 (ADR-2011): a predicted entity that reaches the client after its prediction clock
/// started (a respawn, or a spawn the server held back until the connection chose its wire) has no
/// history because it is new. It catches up to the client tick without counting a reset or a
/// correction.
#[test]
fn a_predicted_entity_spawned_after_the_clock_started_is_not_a_reset() {
    let mut r = rig(22, 6, walk, no_tweak);
    r.hub.run(200);
    let before = stats(&r.hub);
    assert_eq!((before.corrections, before.resets), (0, 0), "{before:?}");
    let shield = r
        .hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Transform::default(),
            Shield { side: 0.5 },
            NetPredicted,
            NetOwner(PeerId(1)),
        ))
        .id();
    r.hub.run(60);
    let s = stats(&r.hub);
    assert_eq!(s.resets, 0, "{s:?}");
    assert_eq!(s.corrections, 0, "{s:?}");
    let copy = {
        let w = &mut r.hub.clients[0].world;
        let mut q = w.query_filtered::<(Entity, &NetRole), With<Shield>>();
        q.iter(w)
            .find(|(_, role)| **role == NetRole::Predicted)
            .map(|(e, _)| e)
            .expect("predicted shield copy")
    };
    // Both sides walk it +0.1 per tick; the client runs ahead of the server, so a copy that caught
    // up to the client tick is ahead of the server's shield.
    let (server_x, client_x) = (
        pos(&r.hub.server.world, shield).x,
        pos(&r.hub.clients[0].world, copy).x,
    );
    assert!(
        client_x > server_x + 0.5,
        "caught up: server {server_x} client {client_x}"
    );
}

/// A reliable broadcast the server sends every tick, so each send tick also has a tick frame (as
/// a game with gameplay events does).
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Chatter;
impl NetMessage for Chatter {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for Chatter {}

fn chatter(mut net: regia_net::message::NetSender) {
    let _ = net.broadcast_session(Chatter);
}

/// The prediction setup on the state channel (spec 72 § 4) at a send interval of 3 ticks.
fn state_channel_setup(world: &mut World) {
    setup(world);
    regia_net::message::register_net_message::<Chatter>(world);
    let mut settings = world.resource_mut::<NetSettings>();
    settings.replication.state_channel = regia_net::settings::StateChannel::Unreliable;
    settings.replication.send_every_ticks = 3;
}

/// Audit wave 5 (fix030b M1): on the negotiating wire the clock starts on empty tick frames, and
/// the first state frame (which spawns the hero) is held back one tick while its tick frame (it
/// carries the tick's reliable message) arrives on time. That state's tick equals one an empty tick frame already set, and it used to
/// be taken for a repeat: the hero never adopted and the next state counted a reset. It adopts.
#[test]
fn a_first_state_that_arrives_after_its_tick_frame_adopts_without_a_reset() {
    use std::sync::{Arc, Mutex};
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 33, state_channel_setup);
    hub.negotiate = true;
    hub.down = LinkConditions {
        delay_ticks: 4,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    // (first state tick, tick frames delivered on time).
    let seen: Arc<Mutex<(Option<u32>, BTreeSet<u32>)>> = Arc::default();
    let record = seen.clone();
    hub.down_filter = Some(Box::new(move |d, _| {
        let tag = d.bytes.first().copied().unwrap_or(0);
        let mut r = regia_net::bits::BitReader::new(&d.bytes);
        let tick = r
            .read_bits(8)
            .ok()
            .and_then(|_| r.read_varint().ok())
            .map(|t| t as u32);
        let mut seen = record.lock().expect("lock");
        match (tag, tick) {
            (7, Some(t)) if seen.0.is_none() => {
                seen.0 = Some(t);
                regia_net::testing::FrameFate::Delay(1)
            }
            (1, Some(t)) => {
                seen.1.insert(t);
                regia_net::testing::FrameFate::Deliver
            }
            _ => regia_net::testing::FrameFate::Deliver,
        }
    }));
    let c = hub.add_client();
    set_transport_rtt(&mut hub.clients[c].world, 9.0 * 1000.0 / 60.0, 0.0);
    hub.clients[c].world.insert_resource(Script(walk));
    hub.clients[c].world.init_resource::<Played>();
    hub.clients[c].gameplay.add_systems(feed);
    hub.server.world.init_resource::<Consumed>();
    hub.server.gameplay.add_systems((log_consumed, chatter));
    hub.server.world.spawn((
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
        NetOwner(PeerId(1)),
    ));
    hub.run(200);
    let (first, ticks) = seen.lock().expect("lock").clone();
    let first = first.expect("a state frame was sent");
    assert!(
        ticks.contains(&first),
        "the scenario needs the tick frame of the first state's tick ({first}) to arrive first: {ticks:?}"
    );
    let s = stats(&hub);
    assert_eq!(s.resets, 0, "{s:?}");
    assert!(client_hero(&mut hub).is_some(), "the hero is predicted");
}

/// § 13 test 9, as spec 120 § 9.4 changed it (ADR-8219): a 40-tick margin target puts the lead
/// over the old 32-tick cap. The mismatch re-runs every tick to the client tick as one correction
/// (no reset), the history scales with the round trip, the correction smooths (no snap), and
/// prediction stays on afterwards: no reset, no further correction, the client on the server's
/// position. At the base it reset, re-ran nothing and cleared the history.
#[test]
fn resim_cap_resets() {
    let mut r = rig(21, 3, idle, |s| {
        s.margin_target_ticks = 40;
        s.margin_max_ticks = 40;
        s.dilation_window_ticks = 100;
    });
    r.hub.run(150);
    let before = stats(&r.hub);
    assert_eq!(before.corrections, 0, "{before:?}");
    assert_eq!(
        before.resets, 0,
        "a lead over the old cap starts clean: {before:?}"
    );
    force_correction(&mut r.hub.server.world, r.hero, Vec3::X).expect("server");
    r.hub.run(20);
    let s = stats(&r.hub);
    assert_eq!(s.resets, before.resets, "no reset: {s:?}");
    assert_eq!(s.corrections, 1, "{s:?}");
    assert!(s.resim_ticks_last > 32, "re-ran past the old cap: {s:?}");
    let (depth, cap) = {
        let w = &r.hub.clients[0].world;
        let settings = w.resource::<NetSettings>().prediction.clone();
        (
            w.resource::<ClientPrediction>().resim_depth(&settings),
            ClientPrediction::history_cap(&settings),
        )
    };
    assert!(
        s.resim_ticks_last <= depth && depth <= cap,
        "depth {depth} cap {cap}: {s:?}"
    );
    assert_eq!(s.snaps, 0, "smoothed: {s:?}");
    assert!((s.last_correction_m - 1.0).abs() < 1e-3, "{s:?}");
    r.hub.run(120);
    let after = stats(&r.hub);
    assert_eq!(after.resets, before.resets, "prediction stays on: {after:?}");
    assert_eq!(after.corrections, 1, "{after:?}");
    let ch = client_hero(&mut r.hub).expect("copy");
    let client_tick = r.hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .client_tick();
    assert!(
        r.hub.clients[0]
            .world
            .resource::<ClientPrediction>()
            .has_history(ch, client_tick),
        "history is kept"
    );
    assert_eq!(pos(&r.hub.clients[0].world, ch), pos(&r.hub.server.world, r.hero));
}

/// Spec 120 § 9.4: a rollback deeper than the RTT-scaled depth (here the round trip estimate is
/// 40 ticks short of the real lead) moves to the server's state and re-runs to the client tick:
/// one reset per such rollback, never a chain, and no snap below the snap distance. At the base the
/// clock started without catching up (40 > 32), every later state was over the cap and older than
/// the cleared history, so prediction reset on every state for the rest of the session.
#[test]
fn a_rollback_over_the_depth_resets_once_and_keeps_predicting() {
    let mut r = rig(24, 3, walk, |s| {
        s.dilation_window_ticks = 100;
        s.max_dilation = 0.0;
    });
    r.hub.clients[0]
        .world
        .resource_mut::<ClientPrediction>()
        .start_offset_ticks = 40;
    r.hub.run(150);
    let before = stats(&r.hub);
    assert_eq!(
        before.resets, 0,
        "the start catches up past the old cap: {before:?}"
    );
    assert_eq!(before.corrections, 0, "{before:?}");
    let depth = {
        let w = &r.hub.clients[0].world;
        let settings = w.resource::<NetSettings>().prediction.clone();
        w.resource::<ClientPrediction>().resim_depth(&settings)
    };
    let lead = {
        let cp = r.hub.clients[0].world.resource::<ClientPrediction>();
        cp.client_tick().0 - r.hub.server_tick().0
    };
    assert!(
        lead > depth,
        "the scenario needs a lead ({lead}) over the depth ({depth})"
    );
    force_correction(&mut r.hub.server.world, r.hero, Vec3::Z).expect("server");
    r.hub.run(20);
    let s = stats(&r.hub);
    assert_eq!(s.resets, 1, "one reset: {s:?}");
    assert!(s.resim_ticks_last > depth, "it re-ran to the client tick: {s:?}");
    assert_eq!(s.snaps, 0, "a 1 m correction smooths: {s:?}");
    let ch = client_hero(&mut r.hub).expect("copy");
    assert!(
        r.hub.clients[0].world.get::<NetVisualOffset>(ch).is_some(),
        "the correction is spread by the visual offset"
    );
    r.hub.run(300);
    let after = stats(&r.hub);
    assert_eq!(after.resets, 1, "prediction stays on: {after:?}");
    assert_eq!(after.corrections, 0, "{after:?}");
    assert_eq!(
        pos(&r.hub.clients[0].world, ch).z,
        pos(&r.hub.server.world, r.hero).z,
        "the client follows the server"
    );
}

/// Spec 120 § 9.4: a lead past the whole history (16 ticks here) cannot be compared, but the
/// client still moves to each server state and re-runs to the client tick from its input history:
/// every such state counts as a reset, the predicted hero stays at the client tick (no snap back to
/// the server's older state) and follows the server. At the base each of those resets snapped the
/// hero back a lead behind and re-ran nothing.
#[test]
fn a_lead_past_the_history_still_predicts() {
    let mut r = rig(25, 3, walk, |s| {
        s.history_ticks = 16;
        s.max_resim_ticks = 8;
        s.dilation_window_ticks = 100;
        s.max_dilation = 0.0;
    });
    r.hub.clients[0]
        .world
        .resource_mut::<ClientPrediction>()
        .start_offset_ticks = 20;
    r.hub.run(150);
    let s = stats(&r.hub);
    assert!(s.resets > 0, "nothing to compare with: {s:?}");
    assert!(s.resim_ticks_last > 16, "re-ran past the history: {s:?}");
    assert_eq!(s.snaps, 0, "{s:?}");
    // The predicted hero is a lead ahead of the server's (walking +x at 0.1 m per tick).
    let ch = client_hero(&mut r.hub).expect("copy");
    let ahead = pos(&r.hub.clients[0].world, ch).x - pos(&r.hub.server.world, r.hero).x;
    assert!(ahead > 2.0, "predicted ahead of the server by its lead: {ahead}");
}

/// Spec 120 § 9.2: with a transport link connected, the clock waits for the transport's first
/// round trip instead of starting on a guess, then starts at the round trip plus the margin target;
/// without one it starts on its fallback after `START_WAIT_FOR_RTT_STEPS`.
#[test]
fn the_clock_waits_for_the_transport_round_trip() {
    for sample in [true, false] {
        let mut r = rig(26, 6, walk, no_tweak);
        {
            let w = &mut r.hub.clients[0].world;
            w.resource_mut::<ClientPrediction>().transport_rtt = None;
            regia_net::predict::expect_transport_rtt(w);
        }
        r.hub.run(60);
        assert!(
            !r.hub.clients[0].world.resource::<ClientPrediction>().started,
            "no round trip yet: the clock waits"
        );
        if sample {
            set_transport_rtt(&mut r.hub.clients[0].world, 13.0 * 1000.0 / 60.0, 0.0);
            r.hub.run(10);
            let s = stats(&r.hub);
            assert_eq!((s.resets, s.resyncs), (0, 0), "{s:?}");
            assert!(r.hub.clients[0].world.resource::<ClientPrediction>().started);
        } else {
            r.hub
                .run(regia_net::predict::START_WAIT_FOR_RTT_STEPS as usize + 10);
            assert!(
                r.hub.clients[0].world.resource::<ClientPrediction>().started,
                "the fallback starts it"
            );
        }
    }
}

/// The send-time map that turns input acknowledgments into round trips stays bounded when no
/// acknowledgment comes back (it was pruned only on an ack).
#[test]
fn the_round_trip_map_is_bounded_without_acks() {
    let mut r = rig(27, 3, walk, no_tweak);
    r.hub.run(100);
    r.hub.up.loss = 1.0;
    r.hub.run(600);
    let held = r.hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .pending_rtt_samples();
    assert!(
        held <= regia_net::predict::INPUT_HISTORY_TICKS as usize + 1,
        "{held} send times held"
    );
}

/// § 13 test 10: a hero riding a tick-driven elevator for 600 ticks at 6-tick latency predicts with
/// zero corrections. (The physics-driven raft half is in `regia_framework/tests/net_character.rs`.)
#[test]
fn tick_driven_elevator_predicts_exactly() {
    let mut r = rig(22, 6, recording, no_tweak);
    r.hub.server.world.spawn((
        Replicated {
            transform: TransformReplication::None,
            ..Replicated::default()
        },
        TickDrivenMotion {
            keys: vec![
                (0.0, Transform::default()),
                (2.0, Transform::from_translation(Vec3::new(0.0, 6.0, 0.0))),
            ],
            start_tick: NetTick(0),
            mode: MotionMode::PingPong,
        },
        Transform::default(),
    ));
    r.hub
        .server
        .world
        .entity_mut(r.hero)
        .insert(Rider { height: 1.0 });
    r.hub.run(700);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    let ch = client_hero(&mut r.hub).expect("copy");
    let y = pos(&r.hub.clients[0].world, ch).y;
    assert!(y >= 1.0, "riding: {y}");
}

fn fire_once(t: u32) -> Pad {
    Pad {
        fire: t == 200,
        ..Pad::default()
    }
}

fn bolts(world: &mut World) -> Vec<(Entity, Option<NetId>)> {
    let mut q = world.query_filtered::<(Entity, Option<&NetId>), With<Bolt>>();
    q.iter(world).map(|(e, id)| (e, id.copied())).collect()
}

/// § 13 test 12: a predicted projectile binds to the server's spawn (one entity); a refused one is
/// despawned after the grace period with `NetPredictedSpawnRejected`.
#[test]
fn predicted_spawns_bind_or_are_rejected() {
    let mut r = rig(23, 6, fire_once, no_tweak);
    while r.hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .client_tick()
        .0
        < 201
    {
        r.hub.step();
    }
    let local = bolts(&mut r.hub.clients[0].world);
    assert_eq!(local.len(), 1, "spawned ahead of the server");
    assert!(local[0].1.is_none(), "not bound yet");
    r.hub.run(60);
    let after = bolts(&mut r.hub.clients[0].world);
    assert_eq!(after.len(), 1, "no duplicate: {after:?}");
    assert_eq!(after[0].0, local[0].0, "the local entity is kept");
    assert!(after[0].1.is_some(), "bound");
    assert_eq!(stats(&r.hub).bound_spawns, 1);
    assert_eq!(bolts(&mut r.hub.server.world).len(), 1);

    let mut r = rig(24, 6, fire_once, no_tweak);
    r.hub.server.world.insert_resource(Deny);
    let mut rejected = Vec::new();
    for _ in 0..300 {
        r.hub.step();
        rejected.extend(
            r.hub.clients[0]
                .world
                .resource::<regia_net::predict::PredictionEvents>()
                .rejected
                .iter()
                .copied(),
        );
    }
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0].key.tick, NetTick(200));
    assert!(bolts(&mut r.hub.clients[0].world).is_empty());
    assert!(bolts(&mut r.hub.server.world).is_empty());
    assert_eq!(stats(&r.hub).rejected_spawns, 1);
}

/// § 13 test 13: effects gated on `first_time()` (and local events) happen once per tick even with
/// three resimulations across it.
#[test]
fn first_time_effects_once_per_tick() {
    let mut r = rig(25, 6, walk, no_tweak);
    r.hub.run(150);
    for _ in 0..3 {
        force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.3)).expect("server");
        r.hub.run(5);
    }
    r.hub.run(40);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 3, "{s:?}");
    let played = &r.hub.clients[0].world.resource::<Played>().0;
    assert!(played.len() > 100);
    assert!(played.values().all(|n| *n == 1), "once per tick");
    let ticks = played.len();
    let pings = r.hub.clients[0].world.resource::<Messages<Ping>>().len();
    assert_eq!(pings, ticks, "local events dropped while resimulating");
}

/// § 13 test 14: with `trust_client_within_m: Some(0.05)` a 2 cm offset is adopted (no correction)
/// and a 10 cm offset is corrected.
#[test]
fn trusted_client_position_tolerance() {
    let mut r = rig(26, 6, walk, |s| s.trust_client_within_m = Some(0.05));
    r.hub.run(200);
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.02)).expect("server");
    r.hub.run(40);
    assert_eq!(stats(&r.hub).corrections, 0, "{:?}", stats(&r.hub));
    assert!(player_stats(&r.hub, 1).trusted >= 1);
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.10)).expect("server");
    r.hub.run(40);
    assert_eq!(stats(&r.hub).corrections, 1, "{:?}", stats(&r.hub));
}

/// Audit-net 3: the tolerance is a budget refilled per second, not a gain per tick. Repeated 2 cm
/// offsets on consecutive ticks are adopted only until the 5 cm budget is spent.
#[test]
fn trusted_position_adoption_is_bounded_over_time() {
    let mut r = rig(26, 6, walk, |s| s.trust_client_within_m = Some(0.05));
    r.hub.run(200);
    let before = player_stats(&r.hub, 1).trusted;
    for _ in 0..8 {
        force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.02)).expect("server");
        r.hub.run(1);
    }
    let adopted = player_stats(&r.hub, 1).trusted - before;
    assert!(adopted <= 3, "adopted {adopted} offsets of 2 cm within 8 ticks");
}

/// Two 4 cm reports one tick apart with a 5 cm per second budget: the first is adopted, the
/// second is not. One tick refills about 0.8 mm, which does not cover another 4 cm.
#[test]
fn two_four_centimetre_reports_one_tick_apart_spend_the_trust_budget() {
    let mut r = rig(26, 6, walk, |s| s.trust_client_within_m = Some(0.05));
    r.hub.run(200);
    let before = player_stats(&r.hub, 1).trusted;
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.04)).expect("server");
    r.hub.run(1);
    let after_first = player_stats(&r.hub, 1).trusted;
    assert_eq!(
        after_first - before,
        1,
        "the first 4 cm report is inside the budget"
    );
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.04)).expect("server");
    r.hub.run(1);
    assert_eq!(
        player_stats(&r.hub, 1).trusted,
        after_first,
        "the second 4 cm report is outside the leftover budget"
    );
}

/// Offline: the local command is consumed directly (no stream, no rollback) and predicted systems
/// run once per tick.
#[test]
fn offline_runs_the_predicted_schedule_directly() {
    let mut world = World::new();
    world.insert_resource(NetMode::Offline);
    setup(&mut world);
    regia_net::runtime::ensure_resources(&mut world);
    let hero = world
        .spawn((
            Transform::default(),
            Motion {
                vy: 0.0,
                grounded: true,
            },
        ))
        .id();
    world.spawn((
        NetPlayer {
            id: NetPlayerId {
                peer: PeerId::SERVER,
                local: 0,
            },
        },
        InputCommand {
            tick: NetTick(0),
            value: walk(0),
        },
    ));
    world.entity_mut(hero).insert(NetOwner(PeerId::SERVER));
    let mut gameplay = Schedule::default();
    for _ in 0..10 {
        regia_net::runtime::run_net_tick(&mut world, &mut gameplay);
    }
    assert!(
        (pos(&world, hero).x - 1.0).abs() < 1e-4,
        "{}",
        pos(&world, hero).x
    );
}

/// The input type's codec is part of the protocol hash.
#[test]
fn input_type_is_in_the_protocol_hash() {
    let mut a = World::new();
    setup(&mut a);
    let mut b = World::new();
    register_replicated::<Motion>(&mut b, ReplicateOptions::default());
    register_replicated::<Shield>(&mut b, ReplicateOptions::default());
    register_replicated::<Bolt>(&mut b, ReplicateOptions::default());
    register_replicated::<Rider>(&mut b, ReplicateOptions::default());
    regia_net::replication::register_replicated::<NetPredictionKey>(&mut b, ReplicateOptions::default());
    regia_net::replication::register_replicated::<TickDrivenMotion>(&mut b, ReplicateOptions::default());
    assert_ne!(
        regia_net::runtime::protocol_hash(&a),
        regia_net::runtime::protocol_hash(&b)
    );
    assert!(add_net_input::<Pad>(&mut a).is_err(), "one input type per game");
}

/// Spec 73 § 4 with spec 74 § 8: each command carries the view tick its client was showing, and
/// the server's consumption puts exactly that value on the player's controller (`NetViewTick`),
/// the tick spec 75 rewinds against. At a 100 ms round trip it stays inside the default
/// 250 ms (15-tick) rewind window and never runs backwards once the clock has settled.
#[test]
fn view_tick_travels_with_the_input_stream() {
    let Rig { mut hub, .. } = rig(73, 3, walk, no_tweak);
    install_interpolation(&mut hub.clients[0].world, 60.0, 1.0);
    hub.server
        .world
        .spawn((Replicated::default(), Transform::default()));
    let mut sampled = BTreeSet::new();
    let mut seen: Vec<(u32, NetViewTick)> = Vec::new();
    for step in 1..=600u64 {
        let now = step as f64 * 1000.0 / 60.0;
        let cw = &mut hub.clients[0].world;
        cw.resource_mut::<NetInterpolation>().now_ms = now;
        net_interpolate(cw, now);
        if let Some(t) = cw.resource::<NetInterpolation>().clock.interp_tick() {
            sampled.insert(view_tick_to_wire(t));
        }
        hub.step();
        let sw = &mut hub.server.world;
        let tick = sw.resource::<NetNode>().tick.0;
        let mut q = sw.query::<(&NetPlayer, &NetViewTick)>();
        let views: Vec<NetViewTick> = q.iter(sw).map(|(_, v)| *v).collect();
        assert!(views.len() <= 1, "one controller");
        if let Some(v) = views.first() {
            seen.push((tick, *v));
        }
    }
    assert!(seen.len() > 500, "view ticks from tick {:?}", seen.first());
    for &(tick, v) in &seen {
        assert!(
            sampled.contains(&view_tick_to_wire(v.tick)),
            "tick {tick}: view {} was never shown on the client",
            v.tick
        );
        let behind = f64::from(tick) - v.tick;
        assert!(
            behind > 0.0 && behind < 15.0,
            "tick {tick}: view {} is {behind} ticks back",
            v.tick
        );
        assert!(v.delay_ms > 0.0, "tick {tick}: delay {}", v.delay_ms);
    }
    let settled = &seen[120..];
    assert!(
        settled.windows(2).all(|w| w[1].1.tick >= w[0].1.tick),
        "the view tick ran backwards"
    );
}

// ---- one tolerance over several components (§ 9) ----------------------------------------------

fn loose_hero(
    predicted: &regia_net::PredictedValues<'_>,
    server: &regia_net::PredictedValues<'_>,
) -> Option<bool> {
    let (a, b) = (predicted.get::<Transform>()?, server.get::<Transform>()?);
    predicted.get::<Motion>()?;
    Some((a.translation.y - b.translation.y).abs() <= 0.5)
}

fn half_error(
    predicted: &regia_net::PredictedValues<'_>,
    server: &regia_net::PredictedValues<'_>,
) -> Option<f32> {
    let (a, b) = (predicted.get::<Motion>()?, server.get::<Motion>()?);
    (a != b).then_some(0.25)
}

fn setup_entity_tolerance(world: &mut World) {
    setup(world);
    regia_net::predict_entity_tolerance(
        world,
        regia_net::EntityTolerance {
            covers: vec![std::any::TypeId::of::<Transform>()],
            matches: loose_hero,
            error_m: half_error,
        },
    );
}

/// An entity tolerance decides for the components it covers instead of their own tolerance; the
/// entity's other components still count, and its `error_m` goes to the stats.
#[test]
fn an_entity_tolerance_decides_for_what_it_covers() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 31, setup_entity_tolerance);
    hub.down.delay_ticks = 3;
    hub.up.delay_ticks = 3;
    let c = hub.add_client();
    set_transport_rtt(&mut hub.clients[c].world, 7.0 * 1000.0 / 60.0, 0.0);
    hub.clients[c].world.insert_resource(Script(idle));
    hub.clients[c].world.init_resource::<Played>();
    hub.clients[c].gameplay.add_systems(feed);
    hub.server.world.init_resource::<Consumed>();
    let hero = hub
        .server
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
            NetOwner(PeerId(1)),
        ))
        .id();
    hub.run(120);
    // Far outside `Tolerance::position(0.01)`, inside the entity tolerance.
    force_correction(&mut hub.server.world, hero, Vec3::new(0.0, 0.3, 0.0)).expect("forced");
    hub.run(30);
    assert_eq!(stats(&hub).corrections, 0, "the entity tolerance accepted it");
    // `Motion` is not covered: its own exact tolerance still corrects.
    hub.server.world.get_mut::<Motion>(hero).expect("motion").vy = 0.0625;
    hub.run(30);
    let s = stats(&hub);
    assert_eq!(s.corrections, 1, "{s:?}");
    assert_eq!(s.max_error_m, 0.25, "the tolerance's error: {s:?}");
}

/// ADR-2413: per-entity reconcile ticks on the state channel. The hero is sent every send tick, the
/// shield (same player, also walking) only every third (`min_interval_ticks: 3`). A state tick that
/// does not carry the shield leaves its older server state in place: compared at that tick it would
/// be a false correction every time. Each entity is compared at its own tick instead.
#[test]
fn two_predicted_entities_reconcile_at_their_own_ticks() {
    fn slow_shield_setup(world: &mut World) {
        setup(world);
        {
            let mut s = world.resource_mut::<NetSettings>();
            s.replication.state_channel = regia_net::settings::StateChannel::Unreliable;
            s.replication.send_every_ticks = 1;
        }
        if world.resource::<NetMode>().is_server() {
            install_net_interest(world);
            let everywhere = NetProfile {
                relevance: RelevanceShape::World,
                linger_ticks: 0,
                ..NetProfile::default()
            };
            let slow = NetProfile {
                min_interval_ticks: 3,
                ..everywhere.clone()
            };
            let mut profiles = world.resource_mut::<NetProfiles>();
            profiles.insert("everywhere", everywhere).expect("profile");
            profiles.insert("slow", slow).expect("profile");
            profiles.set_default("everywhere");
        }
    }
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 41, slow_shield_setup);
    hub.down = LinkConditions {
        delay_ticks: 3,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    let c = hub.add_client();
    set_transport_rtt(&mut hub.clients[c].world, 7.0 * 1000.0 / 60.0, 0.0);
    hub.clients[c].world.insert_resource(Script(walk));
    hub.clients[c].world.init_resource::<Played>();
    hub.clients[c].gameplay.add_systems(feed);
    hub.server.world.init_resource::<Consumed>();
    hub.server.gameplay.add_systems(log_consumed);
    hub.server.world.spawn((
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
        NetOwner(PeerId(1)),
    ));
    hub.server.world.spawn((
        Replicated {
            profile: Some("slow".into()),
            ..Replicated::default()
        },
        Transform::default(),
        Shield { side: 0.5 },
        NetPredicted,
        NetOwner(PeerId(1)),
    ));
    hub.run(150);
    let hero = client_hero(&mut hub).expect("hero predicted");
    let shield = {
        let w = &mut hub.clients[c].world;
        let mut q = w.query_filtered::<(Entity, &NetRole), With<Shield>>();
        q.iter(w)
            .find(|(_, r)| **r == NetRole::Predicted)
            .map(|(e, _)| e)
            .expect("shield predicted")
    };
    let mut differed = 0;
    for _ in 0..60 {
        hub.step();
        let cp = hub.clients[c].world.resource::<ClientPrediction>();
        if let (Some(a), Some(b)) = (cp.reconcile_tick(hero), cp.reconcile_tick(shield))
            && a != b
        {
            assert!(a > b, "the hero is carried every tick: hero {a:?}, shield {b:?}");
            differed += 1;
        }
    }
    assert!(
        differed > 0,
        "the two entities never reconciled at different ticks"
    );
    let s = stats(&hub);
    assert!(s.deferred > 0, "{s:?}");
    assert_eq!(
        s.corrections, 0,
        "an uncarried entity is not a misprediction: {s:?}"
    );
    assert_eq!(s.resets, 0, "{s:?}");
}

// ---- spec conformance audit (lane spec-30-73) ---------------------------------------------------

/// Advances until the client counts one more correction (at most `limit` steps).
fn until_correction(hub: &mut LoopbackHub, limit: usize) {
    let before = stats(hub).corrections;
    for _ in 0..limit {
        hub.step();
        if stats(hub).corrections > before {
            return;
        }
    }
    panic!("no correction within {limit} steps: {:?}", stats(hub));
}

fn client_offset(hub: &mut LoopbackHub) -> NetVisualOffset {
    let ch = client_hero(hub).expect("copy");
    hub.clients[0]
        .world
        .get::<NetVisualOffset>(ch)
        .copied()
        .unwrap_or_default()
}

/// § 6.4: a correction whose rotation error is over 90° snaps (the visual offset is cleared, even
/// though its 1 m position error alone would be smoothed); one of 60° is smoothed, rotation
/// included.
#[test]
fn a_rotation_error_over_ninety_degrees_snaps() {
    for (degrees, snaps) in [(120.0f32, true), (60.0, false)] {
        let mut r = rig(31, 6, walk, no_tweak);
        r.hub
            .server
            .world
            .get_mut::<Replicated>(r.hero)
            .expect("hero")
            .transform = TransformReplication::Movement;
        r.hub.run(200);
        assert_eq!(stats(&r.hub).corrections, 0, "{:?}", stats(&r.hub));
        {
            let w = &mut r.hub.server.world;
            force_correction(w, r.hero, Vec3::new(0.0, 0.0, 1.0)).expect("server");
            w.get_mut::<Transform>(r.hero).expect("hero").rotation =
                Quat::from_rotation_y(degrees.to_radians());
        }
        until_correction(&mut r.hub, 60);
        let off = client_offset(&mut r.hub);
        if snaps {
            assert!(off.is_zero(), "{degrees}°: snaps: {off:?}");
        } else {
            assert!(off.translation.length() > 0.5, "{degrees}°: smoothed: {off:?}");
            let angle = off.rotation.angle_between(Quat::IDENTITY);
            assert!(
                angle > 0.5,
                "{degrees}°: the rotation is smoothed too: {angle} rad"
            );
        }
        // Either way the simulated pose is the server's at once.
        let ch = client_hero(&mut r.hub).expect("copy");
        let rot = r.hub.clients[0]
            .world
            .get::<Transform>(ch)
            .expect("copy")
            .rotation;
        assert!(
            rot.angle_between(Quat::from_rotation_y(degrees.to_radians())) < 0.01,
            "{degrees}°: simulated rotation {rot:?}"
        );
    }
}

fn walk_then_idle(t: u32) -> Pad {
    if t < 250 { walk(t) } else { idle(t) }
}

/// § 6.3 step 1: a server state for a tick older than the prediction history ring (here a 4-tick
/// ring under a 6-tick latency) has no history to compare with: it counts a reset, never a
/// correction, and the client still ends where the server is (ADR-1793 (6): the reset re-runs the
/// known inputs). The same run with the default 64-tick ring has neither.
#[test]
fn a_state_older_than_the_history_ring_resets() {
    let mut short = rig(32, 6, walk_then_idle, |s| s.history_ticks = 4);
    let mut normal = rig(32, 6, walk_then_idle, no_tweak);
    for r in [&mut short, &mut normal] {
        r.hub.run(400);
    }
    let s = stats(&short.hub);
    assert!(s.resets >= 50, "a reset per state older than the ring: {s:?}");
    assert_eq!(s.corrections, 0, "a reset is no correction: {s:?}");
    let n = stats(&normal.hub);
    assert_eq!((n.resets, n.corrections), (0, 0), "{n:?}");
    for r in [&mut short, &mut normal] {
        let ch = client_hero(&mut r.hub).expect("copy");
        let client = pos(&r.hub.clients[0].world, ch);
        let server = pos(&r.hub.server.world, r.hero);
        assert!(server.x > 10.0, "walked: {server}");
        assert!(
            client.distance(server) < 1e-3,
            "the client ends at the server's position: {client} vs {server}"
        );
    }
}

/// § 7: during resimulation `spawn_predicted` for a key that already exists returns the existing
/// entity. A correction whose rollback crosses the tick of a not-yet-bound predicted shot leaves
/// one local projectile, which then binds to the server's.
#[test]
fn a_resimulation_across_a_predicted_spawn_keeps_one_entity() {
    let mut r = rig(33, 6, fire_once, no_tweak);
    while r.hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .client_tick()
        .0
        < 202
    {
        r.hub.step();
    }
    let local = bolts(&mut r.hub.clients[0].world);
    assert_eq!(local.len(), 1, "spawned ahead of the server");
    assert!(local[0].1.is_none(), "not bound yet");
    let server_tick = r.hub.server.world.resource::<NetNode>().tick.0;
    assert!(
        server_tick < 200,
        "the server has not simulated the shot: {server_tick}"
    );
    // The rollback goes back to the server's state for this tick and re-runs past tick 200.
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 0.3)).expect("server");
    until_correction(&mut r.hub, 30);
    let s = stats(&r.hub);
    assert!(s.resim_ticks_last > 2, "the re-run crossed the spawn tick: {s:?}");
    let during = bolts(&mut r.hub.clients[0].world);
    assert_eq!(
        during.len(),
        1,
        "no second projectile from the re-run: {during:?}"
    );
    assert_eq!(during[0].0, local[0].0, "the same entity");
    r.hub.run(60);
    let after = bolts(&mut r.hub.clients[0].world);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].0, local[0].0, "the local entity is kept");
    assert!(after[0].1.is_some(), "bound");
    let s = stats(&r.hub);
    assert_eq!((s.bound_spawns, s.rejected_spawns), (1, 0), "{s:?}");
    // The re-run restored the existing projectile to its first recorded state (ADR-1793 (7)),
    // spawned at the hero's pose before the 0.3 m correction, which the server's projectile has:
    // binding costs it its own 0.3 m correction, never one of the re-run's travel (7.5 m before
    // the ADR-5649 addendum, when the restore found no record and the re-run moved it again).
    assert_eq!(s.corrections, 2, "the hero's and the projectile's: {s:?}");
    assert!((s.max_error_m - 0.3).abs() < 1e-4, "{s:?}");
}

/// A component the predicted schedule writes but nobody registered.
#[derive(Component, Debug, Default)]
struct Glow(u32);

fn step_glow(cx: PredictionCx, mut q: Query<(&mut Glow, Option<&NetRole>)>) {
    for (mut g, role) in &mut q {
        if cx.simulates(role) {
            g.0 += 1;
        }
    }
}

fn setup_glow(world: &mut World) {
    setup(world);
    add_predicted_systems(world, step_glow);
}

#[derive(Clone, Default)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `f` with this thread's `warn` lines captured.
fn capture_warnings(f: impl FnOnce()) -> String {
    let sink = LogSink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    sink.0
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// § 6.1: the debug-build checker warns `net.predict_writes_unregistered` once per component type
/// the predicted schedule writes without `predict::<C>()`, however many ticks write it; the
/// registered components never warn.
#[test]
fn an_unregistered_predicted_write_warns_once_per_type() {
    let mut r = rig_on(setup_glow, 34, 3, walk, no_tweak);
    r.hub.run(60);
    let ch = client_hero(&mut r.hub).expect("copy");
    r.hub.clients[0].world.entity_mut(ch).insert(Glow::default());
    let log = capture_warnings(|| r.hub.run(120));
    let glow = r.hub.clients[0].world.get::<Glow>(ch).map_or(0, |g| g.0);
    assert!(
        glow >= 100,
        "the predicted schedule wrote Glow every tick: {glow}"
    );
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("net.predict_writes_unregistered"))
        .collect();
    let expected = usize::from(cfg!(debug_assertions));
    assert_eq!(
        lines.iter().filter(|l| l.contains("Glow")).count(),
        expected,
        "{log}"
    );
    assert_eq!(lines.len(), expected, "only the unregistered type: {log}");
}

/// § 3: on a listen-server host the host's own command is consumed directly at its tick: no
/// input stream, no lead, no rollback. Its predicted hero moves one step per server tick from the
/// first tick on.
#[test]
fn a_listen_host_consumes_its_own_command_at_its_tick() {
    let mut hub = LoopbackHub::new(NetMode::ListenServer, 35, setup);
    hub.add_client();
    let hero = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Transform::default(),
            Motion {
                vy: 0.0,
                grounded: true,
            },
            NetPredicted,
            NetOwner(PeerId::SERVER),
        ))
        .id();
    let pad = hub
        .server
        .world
        .spawn((
            NetPlayer {
                id: NetPlayerId {
                    peer: PeerId::SERVER,
                    local: 0,
                },
            },
            InputCommand {
                tick: NetTick(0),
                value: walk(0),
            },
        ))
        .id();
    let start = pos(&hub.server.world, hero).x;
    for n in 1..=120u32 {
        // The tick this step simulates (`NetSet::Send` advances the node to the next one).
        let tick = hub.server.world.resource::<NetNode>().tick;
        hub.step();
        let w = &hub.server.world;
        let cmd = w.get::<InputCommand<Pad>>(pad).expect("host command");
        assert_eq!(cmd.tick, tick, "consumed at its own tick");
        let x = pos(w, hero).x - start;
        assert!(
            (x - n as f32 * 0.1).abs() < 1e-3,
            "one step per tick, no lead: tick {n}, x {x}"
        );
    }
    let host = hub.server.world.resource::<ServerPrediction>();
    assert!(
        !host.players.keys().any(|p| p.peer == PeerId::SERVER),
        "no input buffer for the host"
    );
    if let Some(cp) = hub.server.world.get_resource::<ClientPrediction>() {
        assert_eq!(cp.stats, PredictionStats::default(), "no rollback on the host");
        assert_eq!(cp.tracked_entities(), 0, "no prediction history on the host");
    }
}

/// § 7 with § 6.3: a predicted spawn the server confirms binds without a correction and keeps its
/// own prediction for the client tick. Before ADR-5649 the bound projectile took the server's
/// state for the bind tick (a lead behind its prediction) and a later state then counted a false
/// correction of the whole lead's travel.
#[test]
fn a_confirmed_predicted_spawn_binds_at_its_prediction_without_a_correction() {
    let mut r = rig(36, 6, fire_once, no_tweak);
    let mut bound_at = None;
    for _ in 0..320 {
        r.hub.step();
        let ct = r.hub.clients[0]
            .world
            .resource::<ClientPrediction>()
            .client_tick()
            .0;
        let local = bolts(&mut r.hub.clients[0].world);
        if ct < 200 {
            assert!(local.is_empty());
            continue;
        }
        assert_eq!(local.len(), 1, "one projectile at client tick {ct}: {local:?}");
        // Spawned at tick 200 and moved 0.5 every tick from that one on.
        let want = 0.5 * (ct - 199) as f32;
        let x = pos(&r.hub.clients[0].world, local[0].0).x;
        assert!(
            (x - want).abs() < 1e-4,
            "client tick {ct} (bound: {:?}): the projectile is at {x}, its prediction is {want}",
            local[0].1
        );
        let off = r.hub.clients[0]
            .world
            .get::<NetVisualOffset>(local[0].0)
            .copied()
            .unwrap_or_default();
        assert!(off.is_zero(), "client tick {ct}: no visual offset: {off:?}");
        if local[0].1.is_some() && bound_at.is_none() {
            bound_at = Some(ct);
        }
    }
    assert!(bound_at.is_some(), "the projectile bound");
    let s = stats(&r.hub);
    assert_eq!((s.bound_spawns, s.corrections, s.resets), (1, 0, 0), "{s:?}");
}

/// The tick of a server-to-client frame that can carry state, if it is one: a tick frame (tag 1),
/// or on the state channel a `STATE` frame (tag 7) or its entropy-coded form (tag 8). All three
/// start `tag u8 | tick varint`; with `REGIA_NET_TEST_STATE_CHANNEL=unreliable` the states ride the
/// `STATE` frames, so a filter that holds a tick's state must hold those too.
fn tick_frame_tick(d: &regia_net::runtime::Datagram) -> Option<u32> {
    if !matches!(d.bytes.first(), Some(1 | 7 | 8)) {
        return None;
    }
    let mut r = regia_net::bits::BitReader::new(&d.bytes);
    r.read_bits(8).ok()?;
    r.read_varint().ok().and_then(|t| u32::try_from(t).ok())
}

/// § 6.3 step 6: when several server states arrive in one frame, the client reconciles once,
/// against the newest. The tick frames (and on the state channel the `STATE` frames) of the four
/// ticks from a forced correction on are held so all four arrive in one step; each of them differs from the prediction, but one correction
/// re-runs only the ticks after the newest.
#[test]
fn several_states_in_one_frame_reconcile_once_against_the_newest() {
    use regia_net::testing::FrameFate;
    let mut r = rig(41, 6, walk, no_tweak);
    while r.hub.server_tick().0 < 300 {
        r.hub.step();
    }
    let applied =
        force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.0, 0.0, 1.0)).expect("server");
    let first = applied.0;
    let newest = first + 3;
    r.hub.down_filter = Some(Box::new(move |d, _| match tick_frame_tick(d) {
        Some(t) if (first..newest).contains(&t) => FrameFate::Delay(u64::from(newest - t)),
        _ => FrameFate::Deliver,
    }));
    let state_tick = |hub: &LoopbackHub| {
        hub.clients[0]
            .world
            .resource::<regia_net::replication::ClientReplication>()
            .state_tick()
            .map(|t| t.0)
    };
    let mut steps = 0;
    let before = loop {
        let before = r.hub.clients[0]
            .world
            .resource::<ClientPrediction>()
            .client_tick()
            .0;
        let applied_before = state_tick(&r.hub);
        r.hub.step();
        if stats(&r.hub).corrections > 0 {
            assert!(
                applied_before.is_some_and(|t| t < first),
                "no state of the held ticks arrived earlier: {applied_before:?}"
            );
            break before;
        }
        steps += 1;
        assert!(steps < 60, "the correction arrives");
    };
    assert_eq!(
        state_tick(&r.hub),
        Some(newest),
        "the four states arrived together"
    );
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 1, "{s:?}");
    assert_eq!(
        before - s.resim_ticks_last,
        newest,
        "the rollback went back to the newest state, not to {first}: {s:?}"
    );
    r.hub.down_filter = None;
    r.hub.run(120);
    let s = stats(&r.hub);
    assert_eq!((s.corrections, s.resets), (1, 0), "{s:?}");
    let ch = client_hero(&mut r.hub).expect("copy");
    assert_eq!(
        pos(&r.hub.clients[0].world, ch).z,
        pos(&r.hub.server.world, r.hero).z
    );
}
