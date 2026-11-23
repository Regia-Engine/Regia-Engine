// SPDX-License-Identifier: MPL-2.0
//! Spec 73 § 9, § 6.6, § 6.7 and § 13 tests 10, 11, 15: characters in the predicted schedule over
//! the in-memory `LoopbackHub` (fixed steps, latency in ticks).
//!
//! The moving-base rigs use a small deck backend: flat ground plus replicated rafts, reporting the
//! raft as the ground entity and carrying the rider like the rapier backend does. Rapier itself is
//! exercised at the bottom of the file and by `character_physics`.

use std::collections::BTreeMap;
use std::time::Duration;

use regia_animation::clip::{JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::eval::ClipLibrary;
use regia_animation::montage::Montage;
use regia_animation::{AnimationClip, MontagePlayer};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_framework::character::{
    Character, CharacterCollisionQuery, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState,
    CharacterSteppedExternally, CollisionQuery, MoveResult, MovementMode, RootMotionApply, RootMotionDriven,
    RotationMode, SampledRootMotion, is_anchored, step_character, use_rapier_character_controller,
};
use regia_framework::net_character::{
    CharacterCommand, CharacterNetInput, NetCharacter, NetCharacterBase, NetCharacterPlugin, NetMontagePlay,
    install_net_character, net_character_rows, net_character_step,
};
use regia_net::predict::{
    ClientPrediction, InputCommand, NetVisualOffset, PredictionState, PredictionStats, add_predicted_systems,
    force_correction, set_transport_rtt,
};
use regia_net::replication::{NetPredicted, ReplicateOptions, register_replicated};
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::{
    MotionMode, NetId, NetInput, NetMode, NetOwner, NetPlayer, NetPlugin, NetResimulating, NetRole,
    NetSettings, NetTick, PeerId, PredictionCx, Replicate, Replicated, TickDrivenMotion,
    TransformReplication,
};
use regia_physics::{Collider, ColliderShape, RigidBody, Velocity};
use regia_scene::Transform;

const STAND_OFF: f32 = 1.2; // capsule radius + half height of the default movement settings
const DECK_SNAP: f32 = 0.3;
const DT: f32 = 1.0 / 60.0;
/// The unreliable-mode end-position bound of `no_false_corrections_with_the_real_step` (ADR-8763):
/// the divergence observed with this rig and seed is 4.4e-5 m (one repeated input command at
/// server tick 505), so 1e-3 m keeps an order of magnitude of headroom while no longer reading as
/// "anything under a centimeter passes" (ADR-8850 tightens the bound; it never loosens it).
const UNRELIABLE_END_TOLERANCE_M: f32 = 1e-3;

// ---- the deck backend ---------------------------------------------------------------------------

/// A square raft whose deck is at its transform's height.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Raft {
    half: f32,
    /// The velocity the backend carries riders with (a kinematic body's velocity in rapier).
    velocity: Vec3,
}

/// Client trace: the move the backend received on every run of every tick.
#[derive(Resource, Default)]
struct Trace {
    first: BTreeMap<u32, Vec3>,
    again: BTreeMap<u32, Vec<Vec3>>,
}

struct DeckQuery;

fn deck_under(rafts: &[(Entity, Vec3, f32, Vec3)], p: Vec3) -> Option<(Entity, Vec3, f32, Vec3)> {
    rafts
        .iter()
        .find(|(_, at, half, _)| {
            (p.x - at.x).abs() <= *half
                && (p.z - at.z).abs() <= *half
                && (p.y - STAND_OFF - at.y).abs() <= DECK_SNAP
        })
        .copied()
}

impl CollisionQuery for DeckQuery {
    fn move_shape(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        _radius: f32,
        _half_height: f32,
        delta: Vec3,
        _dt: f32,
        _carry_dt: f32,
        _max_slope_angle: f32,
        _step_height: f32,
        _snap_to_ground: f32,
    ) -> MoveResult {
        if let Some(state) = world.get_resource::<PredictionState>().copied()
            && !state.server
            && let Some(mut trace) = world.get_resource_mut::<Trace>()
        {
            if state.resimulating {
                trace.again.entry(state.tick.0).or_default().push(delta);
            } else {
                trace.first.insert(state.tick.0, delta);
            }
        }
        let rafts: Vec<(Entity, Vec3, f32, Vec3)> = {
            let mut q = world.query::<(Entity, &Transform, &Raft)>();
            q.iter(world)
                .map(|(e, t, r)| (e, t.translation, r.half, r.velocity))
                .collect()
        };
        let start = deck_under(&rafts, position);
        let carry = if is_anchored(world, entity) {
            Vec3::ZERO
        } else {
            start.map_or(Vec3::ZERO, |(_, _, _, v)| v * DT)
        };
        let mut target = position + delta + carry;
        let ground_velocity = start.map_or(Vec3::ZERO, |(_, _, _, v)| v);
        let mut ground_entity = None;
        let mut grounded = false;
        // A rider stays on the deck; a faller lands when it reaches it.
        let on_deck = deck_under(&rafts, target)
            .filter(|(_, at, _, _)| delta.y <= 0.0 && (start.is_some() || target.y <= at.y + STAND_OFF));
        if let Some((entity, at, _, _)) = on_deck {
            target.y = at.y + STAND_OFF;
            grounded = true;
            ground_entity = Some(entity);
        } else if target.y <= STAND_OFF {
            target.y = STAND_OFF;
            grounded = true;
        }
        MoveResult {
            position: target,
            grounded,
            ground_normal: Vec3::Y,
            ground_velocity,
            ground_entity,
            ..MoveResult::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---- the rig ------------------------------------------------------------------------------------

/// Client: the input of each client tick.
#[derive(Resource, Clone, Copy)]
struct Script(fn(u32) -> CharacterNetInput);

/// Server: the raft's velocity at each tick.
#[derive(Resource, Clone, Copy)]
struct RaftPath(fn(u32) -> Vec3);

fn feed(script: Res<Script>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<CharacterNetInput>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

fn drive_raft(
    path: Option<Res<RaftPath>>,
    node: Res<regia_net::runtime::NetNode>,
    mut rafts: Query<(&mut Transform, &mut Raft)>,
) {
    let Some(path) = path else {
        return;
    };
    let velocity = (path.0)(node.tick.0);
    for (mut t, mut r) in &mut rafts {
        t.translation += velocity * DT;
        r.velocity = velocity;
    }
}

fn setup(world: &mut World) {
    world.insert_resource(CharacterCollisionQuery {
        query: Box::new(DeckQuery),
    });
    register_replicated::<Raft>(world, ReplicateOptions::default());
    install_net_character::<CharacterNetInput>(world).expect("net character");
}

struct Rig {
    hub: LoopbackHub,
    hero: Entity,
    raft: Entity,
}

fn hero_bundle(at: Vec3) -> impl Bundle {
    (
        Replicated {
            transform: TransformReplication::Movement,
            ..Replicated::default()
        },
        Transform::from_translation(at),
        NetCharacter::default(),
        CharacterState {
            grounded: true,
            ..CharacterState::default()
        },
        NetPredicted,
        NetOwner(PeerId(1)),
    )
}

fn raft_bundle(at: Vec3, half: f32) -> impl Bundle {
    (
        Replicated {
            transform: TransformReplication::PositionOnly,
            ..Replicated::default()
        },
        Transform::from_translation(at),
        Raft {
            half,
            velocity: Vec3::ZERO,
        },
    )
}

/// A dedicated server, one client at `delay` ticks each way, a hero on a raft (or on the floor
/// beside it when `aboard` is false).
fn rig(seed: u64, delay: u64, script: fn(u32) -> CharacterNetInput, aboard: bool) -> Rig {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    hub.down = LinkConditions {
        delay_ticks: delay,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    let c = hub.add_client();
    set_transport_rtt(
        &mut hub.clients[c].world,
        (2 * delay + 1) as f64 * 1000.0 / 60.0,
        0.0,
    );
    hub.clients[c].world.insert_resource(Script(script));
    hub.clients[c].world.init_resource::<Trace>();
    hub.clients[c].gameplay.add_systems(feed);
    hub.server.gameplay.add_systems(drive_raft);
    let raft = hub
        .server
        .world
        .spawn(raft_bundle(Vec3::new(0.0, 1.0, 0.0), 5.0))
        .id();
    let start = if aboard {
        Vec3::new(1.0, 1.0 + STAND_OFF, 0.0)
    } else {
        Vec3::new(30.0, STAND_OFF, 0.0)
    };
    let hero = hub.server.world.spawn(hero_bundle(start)).id();
    Rig { hub, hero, raft }
}

fn stats(hub: &LoopbackHub) -> PredictionStats {
    hub.clients[0].world.resource::<ClientPrediction>().stats.clone()
}

fn copy_of(hub: &mut LoopbackHub, client: usize, role: NetRole) -> Option<Entity> {
    let w = &mut hub.clients[client].world;
    let mut q = w.query_filtered::<(Entity, &NetRole), With<NetCharacter>>();
    q.iter(w).find(|(_, r)| **r == role).map(|(e, _)| e)
}

fn raft_copy(hub: &mut LoopbackHub) -> Option<Entity> {
    let w = &mut hub.clients[0].world;
    let mut q = w.query_filtered::<Entity, With<Raft>>();
    q.iter(w).next()
}

fn pos(world: &World, e: Entity) -> Vec3 {
    world
        .get::<Transform>(e)
        .map(|t| t.translation)
        .unwrap_or(Vec3::NAN)
}

/// What is drawn: the simulated pose plus the correction offset.
fn rendered(world: &World, e: Entity) -> Vec3 {
    let offset = world.get::<NetVisualOffset>(e).copied().unwrap_or_default();
    pos(world, e) + offset.translation
}

fn base_of(world: &World, e: Entity) -> NetCharacterBase {
    world.get::<NetCharacterBase>(e).cloned().unwrap_or_default()
}

fn idle(_: u32) -> CharacterNetInput {
    CharacterNetInput::default()
}

/// Walks one way and back with jumps, the way a player moves on open ground.
fn recording(t: u32) -> CharacterNetInput {
    if !(120..600).contains(&t) {
        return CharacterNetInput::default();
    }
    CharacterNetInput {
        move_dir: Vec2::new(if (t / 37).is_multiple_of(3) { -0.7 } else { 0.7 }, 0.5),
        yaw: (t as f32 * 0.01).sin(),
        jump: t.is_multiple_of(90),
        sprint: (t / 53).is_multiple_of(2),
        ..CharacterNetInput::default()
    }
}

/// Paces around the deck without leaving it, then stands.
fn pacing(t: u32) -> CharacterNetInput {
    CharacterNetInput {
        move_dir: if !(120..700).contains(&t) {
            Vec2::ZERO
        } else if (t / 40).is_multiple_of(2) {
            Vec2::new(0.5, 0.0)
        } else {
            Vec2::new(-0.5, 0.0)
        },
        ..CharacterNetInput::default()
    }
}

/// A raft that only the server can know: it speeds up, turns hard and stops.
fn wandering_raft(t: u32) -> Vec3 {
    match t {
        0..=119 => Vec3::ZERO,
        120..=239 => Vec3::new(3.0, 0.0, 0.0),
        240..=299 => Vec3::new(-2.0, 0.5, 4.0),
        300..=419 => Vec3::new(1.0, -0.25, -5.0),
        420..=539 => Vec3::new(0.0, 0.0, 2.0 * ((t as f32) * 0.1).sin()),
        _ => Vec3::ZERO,
    }
}

// ---- § 13 test 1 with the real step ------------------------------------------------------------

/// A character walks and jumps a 600-tick recording at 6 ticks of latency, jitter and 5 % loss:
/// no correction, and client and server end on the same bits.
#[test]
fn no_false_corrections_with_the_real_step() {
    let mut r = rig(11, 6, recording, false);
    r.hub.down.reorder = 0.1;
    r.hub.up.reorder = 0.1;
    r.hub.down.loss = 0.05;
    r.hub.up.loss = 0.05;
    r.hub.run(760);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    assert_eq!(s.resets, 0, "{s:?}");
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let (a, b) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    if regia_net::testing::every_test_on_state_channel() {
        // ADR-8763: on the state channel the hub's seeded loss lands differently (state frames and
        // their acknowledgments draw from the same stream), and with this seed one lost input
        // window falls at server tick 505, mid-move: the server repeats tick 504's command (spec 73
        // § 5.2) and ends 4.4e-5 m from the prediction, under the correction tolerance, so by
        // design nothing corrects it. `no_drift_from_the_state_channel_without_input_loss` keeps
        // the bit-identical check on the state channel with the input path lossless.
        let missing: u64 = r
            .hub
            .server
            .world
            .resource::<regia_net::predict::ServerPrediction>()
            .players
            .values()
            .map(|p| p.missing)
            .sum();
        assert!(
            a == b || (missing > 0 && a.distance(b) <= UNRELIABLE_END_TOLERANCE_M),
            "end positions {a} and {b} differ by more than the tolerance, or without a repeated input ({missing} missing)"
        );
    } else {
        assert_eq!(a, b, "bit-identical end position");
    }
    assert!(a.x != 30.0 || a.z != 0.0, "it walked: {a}");
}

/// ADR-8763: the same walk with loss, jitter and reordering on the server-to-client link only
/// (every input command arrives): the state channel and the reliable stream both end on the same
/// bits, so state delivery adds no drift of its own.
#[test]
fn no_drift_from_the_state_channel_without_input_loss() {
    let mut r = rig(11, 6, recording, false);
    r.hub.down.reorder = 0.1;
    r.hub.down.loss = 0.05;
    r.hub.down.duplicate = 0.02;
    r.hub.run(760);
    let s = stats(&r.hub);
    assert_eq!((s.corrections, s.resets), (0, 0), "{s:?}");
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let (a, b) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    assert_eq!(a, b, "bit-identical end position");
    assert!(a.x != 30.0 || a.z != 0.0, "it walked: {a}");
}

/// The rollback path of `CharacterMovement::mode`: a correction while the prediction is in the air
/// and the server on the ground restores the mode with the state.
#[test]
fn a_correction_restores_the_movement_mode() {
    fn jump_once(t: u32) -> CharacterNetInput {
        CharacterNetInput {
            jump: t == 300,
            ..CharacterNetInput::default()
        }
    }
    let mut r = rig(12, 6, jump_once, false);
    let mut seen_falling = false;
    for _ in 0..420 {
        r.hub.step();
        if let Some(ch) = copy_of(&mut r.hub, 0, NetRole::Predicted) {
            let w = &r.hub.clients[0].world;
            let (m, s) = (
                w.get::<CharacterMovement>(ch).expect("movement"),
                w.get::<CharacterState>(ch).expect("state"),
            );
            seen_falling |= s.mode == MovementMode::Falling;
            assert_eq!(
                m.mode, s.mode,
                "the settings mode follows the state at every step"
            );
        }
    }
    assert!(seen_falling);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
}

// ---- moving bases (§ 6.6, § 13 test 10) ---------------------------------------------------------

/// § 13 test 10, tick-driven half with the real step and the deck backend: a rider of an elevator
/// predicts with no correction over 600 ticks.
#[test]
fn riding_a_tick_driven_elevator_with_the_real_step() {
    let mut r = rig(21, 6, pacing, true);
    r.hub.server.world.entity_mut(r.raft).insert((
        Replicated {
            transform: TransformReplication::None,
            ..Replicated::default()
        },
        TickDrivenMotion {
            keys: vec![
                (0.0, Transform::from_translation(Vec3::new(0.0, 1.0, 0.0))),
                (5.0, Transform::from_translation(Vec3::new(0.0, 4.0, 3.0))),
            ],
            start_tick: NetTick(0),
            mode: MotionMode::PingPong,
        },
    ));
    r.hub.run(760);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    assert!(
        base_of(&r.hub.clients[0].world, ch).base.is_some(),
        "still riding"
    );
}

/// § 13 test 10, raft half: the raft's motion is known only to the server and reaches the client
/// 6 ticks late, turning hard on the way. Riding and pacing on it needs no correction, because
/// both sides agree on the position on the raft; the world position differs by the raft's lag and
/// the rider stays on the raft as the client draws it.
#[test]
fn a_late_raft_is_not_a_misprediction() {
    let mut r = rig(22, 6, pacing, true);
    r.hub.server.world.insert_resource(RaftPath(wandering_raft));
    let mut worst_lag = 0.0f32;
    let mut worst_on_deck = 0.0f32;
    for _ in 0..700 {
        r.hub.step();
        let (Some(ch), Some(raft)) = (copy_of(&mut r.hub, 0, NetRole::Predicted), raft_copy(&mut r.hub))
        else {
            continue;
        };
        let world = &r.hub.clients[0].world;
        let server_raft = pos(&r.hub.server.world, r.raft);
        worst_lag = worst_lag.max(server_raft.distance(pos(world, raft)));
        let rider = base_of(world, ch);
        if rider.base.is_some() {
            // Drawn on the deck the client sees, whatever the raft did.
            let on_deck = pos(world, ch) - pos(world, raft);
            worst_on_deck = worst_on_deck.max((on_deck.y - STAND_OFF).abs());
            assert!(
                on_deck.x.abs() <= 5.5 && on_deck.z.abs() <= 5.5,
                "on deck: {on_deck}"
            );
        }
    }
    assert!(worst_lag > 0.3, "the client's raft really is late: {worst_lag}");
    // A tick the lead control skips leaves the rider one raft step behind for that one frame.
    assert!(
        worst_on_deck <= 0.5 * DT + 1e-3,
        "height on the deck: {worst_on_deck}"
    );
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    assert_eq!(s.resets, 0, "{s:?}");
    r.hub.run(120);
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let (a, b) = (
        base_of(&r.hub.server.world, r.hero),
        base_of(&r.hub.clients[0].world, ch),
    );
    assert_eq!(a.base, b.base);
    assert!(a.local.distance(b.local) < 1e-3, "{a:?} {b:?}");
    let (sp, cp) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    assert!(
        sp.distance(cp) < 1e-3,
        "the raft stopped, the worlds agree: {sp} {cp}"
    );
}

/// The rider is shoved on a moving raft: a misprediction of its place on the base. The client
/// corrects once, the error it reports is the shove (not the raft's lag), the simulated pose jumps
/// and the drawn pose does not, and both sides end at the same place on the raft.
#[test]
fn a_misprediction_on_a_moving_base_reconciles_without_a_pop() {
    fn steady(t: u32) -> Vec3 {
        if t < 120 {
            Vec3::ZERO
        } else {
            Vec3::new(3.0, 0.0, 1.0)
        }
    }
    let mut r = rig(23, 6, idle, true);
    r.hub.server.world.insert_resource(RaftPath(steady));
    r.hub.run(300);
    assert_eq!(stats(&r.hub).corrections, 0);
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    force_correction(&mut r.hub.server.world, r.hero, Vec3::new(1.0, 0.0, 0.0)).expect("forced");

    let raft_step = Vec3::new(3.0, 0.0, 1.0).length() * DT;
    let world = &r.hub.clients[0].world;
    let (mut last_sim, mut last_drawn) = (pos(world, ch), rendered(world, ch));
    let (mut worst_sim, mut worst_drawn) = (0.0f32, 0.0f32);
    for _ in 0..60 {
        r.hub.step();
        let world = &r.hub.clients[0].world;
        let (sim, drawn) = (pos(world, ch), rendered(world, ch));
        worst_sim = worst_sim.max(sim.distance(last_sim));
        worst_drawn = worst_drawn.max(drawn.distance(last_drawn));
        (last_sim, last_drawn) = (sim, drawn);
    }
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 1, "{s:?}");
    assert_eq!(s.resets, 0, "{s:?}");
    assert!(
        (s.max_error_m - 1.0).abs() < 0.02,
        "the shove, not the lag: {}",
        s.max_error_m
    );
    assert!(worst_sim > 0.9, "the simulation jumped: {worst_sim}");
    // One step of a 40 ms half-life at 60 Hz removes 25 % of the offset; the raft keeps moving.
    assert!(worst_drawn < 0.26 + raft_step + 0.01, "no pop: {worst_drawn}");
    let offset = r.hub.clients[0]
        .world
        .get::<NetVisualOffset>(ch)
        .copied()
        .unwrap_or_default();
    assert!(offset.translation.length() < 1e-3, "{offset:?}");
    let (a, b) = (
        base_of(&r.hub.server.world, r.hero),
        base_of(&r.hub.clients[0].world, ch),
    );
    assert!(a.local.distance(b.local) < 1e-3, "{a:?} {b:?}");
    assert!(
        (a.local.x - 2.0).abs() < 1e-3,
        "the shove stayed on the server: {a:?}"
    );
}

/// The raft goes away under the rider: the server's rider falls to the floor from where the
/// server had it, the client predicted riding. One correction per wrong claim, no reset, and the
/// client ends where the server is; the rider's drawn pose stays inside the smoothing.
#[test]
fn losing_the_base_reconciles_to_the_server() {
    fn slow(t: u32) -> Vec3 {
        if t < 120 {
            Vec3::ZERO
        } else {
            Vec3::new(1.0, 0.0, 0.0)
        }
    }
    let mut r = rig(24, 6, idle, true);
    r.hub.server.world.insert_resource(RaftPath(slow));
    r.hub.run(300);
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    assert!(base_of(&r.hub.clients[0].world, ch).base.is_some());
    r.hub.server.world.despawn(r.raft);
    r.hub.run(180);
    let s = stats(&r.hub);
    assert!(s.corrections >= 1, "{s:?}");
    assert_eq!(s.resets, 0, "{s:?}");
    let (sp, cp) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    assert!(sp.distance(cp) < 1e-3, "{sp} {cp}");
    assert!((sp.y - STAND_OFF).abs() < 1e-3, "on the floor: {sp}");
    assert_eq!(base_of(&r.hub.clients[0].world, ch).base, None);
}

/// A rotating raft: the rider's position on it is rotation-relative, so it circles with the raft
/// (`Transform::rotation` of the raft travels with `TransformReplication::Full`).
#[test]
fn a_turning_base_carries_the_rider_around() {
    let mut r = rig(25, 3, idle, true);
    r.hub.server.world.entity_mut(r.raft).insert(Replicated {
        transform: TransformReplication::Full,
        ..Replicated::default()
    });
    r.hub.run(120);
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let raft = raft_copy(&mut r.hub).expect("raft");
    for _ in 0..90 {
        if let Some(mut t) = r.hub.server.world.get_mut::<Transform>(r.raft) {
            t.rotation = (Quat::from_rotation_y(0.01) * t.rotation).normalize();
        }
        r.hub.step();
    }
    r.hub.run(60);
    assert_eq!(stats(&r.hub).corrections, 0);
    let world = &r.hub.clients[0].world;
    let on_deck = pos(world, ch) - pos(world, raft);
    let expected = Quat::from_rotation_y(0.9) * Vec3::new(1.0, STAND_OFF, 0.0);
    assert!(on_deck.distance(expected) < 0.05, "{on_deck} vs {expected}");
    let server = pos(&r.hub.server.world, r.hero);
    assert!(server.distance(pos(world, ch)) < 0.02, "{server}");
}

/// Jumping on a moving raft leaves the base's frame and comes back to it, which are real
/// differences between the two worlds: a few corrections, no reset, a smoothed drawn pose, and
/// the same place on the raft at the end.
#[test]
fn a_jump_on_a_moving_raft_is_corrected_and_smoothed() {
    fn jump_once(t: u32) -> CharacterNetInput {
        CharacterNetInput {
            jump: t == 300,
            ..CharacterNetInput::default()
        }
    }
    fn steady(t: u32) -> Vec3 {
        if t < 120 {
            Vec3::ZERO
        } else {
            Vec3::new(2.0, 0.0, 0.0)
        }
    }
    let mut r = rig(26, 6, jump_once, true);
    r.hub.server.world.insert_resource(RaftPath(steady));
    r.hub.run(280);
    assert_eq!(stats(&r.hub).corrections, 0);
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let raft = raft_copy(&mut r.hub).expect("raft");
    let mut left_the_deck = false;
    let mut landed = false;
    let mut last = rendered(&r.hub.clients[0].world, ch);
    let mut worst = 0.0f32;
    for _ in 0..240 {
        r.hub.step();
        let world = &r.hub.clients[0].world;
        let drawn = rendered(world, ch);
        worst = worst.max(drawn.distance(last));
        last = drawn;
        let on_base = base_of(world, ch).base.is_some();
        left_the_deck |= !on_base;
        landed |= left_the_deck && on_base;
    }
    assert!(left_the_deck && landed, "it jumped and came back");
    let s = stats(&r.hub);
    assert!((1..=4).contains(&s.corrections), "{s:?}");
    assert_eq!(s.resets, 0, "{s:?}");
    // The lag is 13 ticks at 2 m/s, about 0.43 m: a quarter of it per step, plus the motion.
    assert!(worst < 0.25 * 0.5 + 8.0 * DT, "no pop: {worst}");
    let (a, b) = (
        base_of(&r.hub.server.world, r.hero),
        base_of(&r.hub.clients[0].world, ch),
    );
    assert_eq!(a.base, b.base);
    assert!(a.local.distance(b.local) < 1e-3, "{a:?} {b:?}");
    let world = &r.hub.clients[0].world;
    let on_deck = pos(world, ch) - pos(world, raft);
    assert!((on_deck.y - STAND_OFF).abs() <= 1e-3, "{on_deck}");
}

/// A `Transform` write on a rider by gameplay moves its place on the base instead of being undone.
#[test]
fn a_rider_keeps_a_gameplay_move() {
    let mut r = rig(27, 3, idle, true);
    r.hub.run(120);
    let before = base_of(&r.hub.server.world, r.hero);
    r.hub
        .server
        .world
        .get_mut::<Transform>(r.hero)
        .expect("transform")
        .translation
        .z += 0.5;
    r.hub.run(60);
    let after = base_of(&r.hub.server.world, r.hero);
    assert_eq!(after.base, before.base);
    assert!(
        (after.local.z - before.local.z - 0.5).abs() < 1e-4,
        "{before:?} {after:?}"
    );
    assert_eq!(stats(&r.hub).corrections, 1);
}

// ---- the plugin's other edges -------------------------------------------------------------------

/// A second client sees the character as a proxy: nothing steps it there, in either system.
#[test]
fn a_proxy_character_is_not_stepped() {
    let mut r = rig(31, 3, pacing, false);
    let second = r.hub.add_client();
    r.hub.clients[second].world.insert_resource(Script(idle));
    r.hub.run(300);
    let proxy = copy_of(&mut r.hub, second, NetRole::Proxy).expect("proxy");
    let w = &r.hub.clients[second].world;
    assert!(w.get::<CharacterSteppedExternally>(proxy).is_some());
    let state = w.get::<CharacterState>(proxy).expect("state");
    assert!(state.grounded, "the server's state arrived");
    let at = pos(w, proxy);
    let server = pos(&r.hub.server.world, r.hero);
    assert!(
        at.distance(server) < 2.0,
        "follows the server, not its own step: {at} {server}"
    );
}

/// A character without an owner is the server's: it reads its own input, clients only see it.
#[test]
fn a_server_owned_character_reads_its_own_input() {
    let mut r = rig(32, 3, idle, false);
    let npc = r
        .hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::Movement,
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(-20.0, STAND_OFF, 0.0)),
            NetCharacter::default(),
            CharacterInput {
                move_dir: Vec3::X,
                ..CharacterInput::default()
            },
        ))
        .id();
    r.hub.run(120);
    let x = pos(&r.hub.server.world, npc).x;
    assert!(x > -15.0, "it walked: {x}");
}

/// `UseControllerYaw`: the character faces the command's yaw on both sides, no correction.
#[test]
fn a_controller_yaw_character_faces_the_look_angle() {
    fn look(t: u32) -> CharacterNetInput {
        CharacterNetInput {
            yaw: t as f32 * 0.02,
            move_dir: Vec2::new(0.0, 1.0),
            ..CharacterNetInput::default()
        }
    }
    let mut r = rig(33, 6, look, false);
    r.hub
        .server
        .world
        .get_mut::<CharacterMovement>(r.hero)
        .expect("movement")
        .rotation_mode = RotationMode::UseControllerYaw;
    fn use_look(mut q: Query<&mut CharacterMovement, Added<NetCharacter>>) {
        for mut m in &mut q {
            m.rotation_mode = RotationMode::UseControllerYaw;
        }
    }
    r.hub.clients[0].gameplay.add_systems(use_look);
    r.hub.run(400);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let tick = r.hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .client_tick()
        .0;
    let expected = Quat::from_rotation_y(look(tick).quantized().yaw);
    let facing = r.hub.clients[0].world.get::<Transform>(ch).expect("t").rotation;
    assert!(facing.angle_between(expected) < 1e-3, "faces the look: {facing}");
}

// ---- root motion (§ 6.7, § 13 test 11) ----------------------------------------------------------

/// A game's own input type: the mapping of [`CharacterCommand`], and a button that starts a montage.
#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    forward: i8,
    vault: bool,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        Self {
            vault: false,
            ..last.clone()
        }
    }
}

impl CharacterCommand for Pad {
    fn character_input(&self) -> (CharacterInput, f32) {
        (
            CharacterInput {
                move_dir: Vec3::X * f32::from(self.forward),
                ..CharacterInput::default()
            },
            0.0,
        )
    }
}

#[derive(Resource, Clone, Copy)]
struct PadScript(fn(u32) -> Pad);

fn feed_pad(script: Res<PadScript>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<Pad>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

/// The predicted code that starts the montage, on the server and in every re-run.
fn start_vault(
    cx: PredictionCx,
    inputs: Query<(&NetPlayer, &InputCommand<Pad>)>,
    mut heroes: Query<(&mut NetMontagePlay, Option<&NetOwner>, Option<&NetRole>)>,
) {
    for (mut play, owner, role) in &mut heroes {
        if !cx.simulates(role) {
            continue;
        }
        let player = cx.owner_player(owner);
        let vault = inputs.iter().any(|(p, c)| Some(p.id) == player && c.value.vault);
        if vault && !play.is_playing() {
            play.start("vault", 0, cx.tick(), 1.0);
        }
    }
}

fn setup_pad(world: &mut World) {
    world.insert_resource(CharacterCollisionQuery {
        query: Box::new(DeckQuery),
    });
    install_net_character::<Pad>(world).expect("net character");
    add_predicted_systems(world, start_vault.before(net_character_step::<Pad>));
}

/// A one-second clip that walks the root 3 m along -Z.
fn vault_player() -> MontagePlayer {
    let mut clip = AnimationClip::empty("vault_clip", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 0.25, 1.0],
            values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -0.3), Vec3::new(0.0, 0.0, -3.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::XZ,
    });
    let mut library = ClipLibrary::default();
    library.insert(clip);
    let mut player = MontagePlayer::new(Vec::new(), library);
    player.add_montage(Montage::single("vault", "full_body", "vault_clip"));
    player
}

/// Client: the character copy gets its rig when it appears.
fn equip_client(mut commands: Commands, new: Query<Entity, Added<NetCharacter>>) {
    for e in &new {
        commands.entity(e).insert((
            vault_player(),
            RootMotionDriven {
                rotation: false,
                ..RootMotionDriven::default()
            },
        ));
    }
}

struct VaultRig {
    hub: LoopbackHub,
    hero: Entity,
}

fn vault_rig(seed: u64, script: fn(u32) -> Pad) -> VaultRig {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup_pad);
    hub.down = LinkConditions {
        delay_ticks: 6,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    let c = hub.add_client();
    set_transport_rtt(&mut hub.clients[c].world, 13.0 * 1000.0 / 60.0, 0.0);
    hub.clients[c].world.insert_resource(PadScript(script));
    hub.clients[c].world.init_resource::<Trace>();
    hub.clients[c].gameplay.add_systems((feed_pad, equip_client));
    let hero = hub
        .server
        .world
        .spawn((
            hero_bundle(Vec3::new(30.0, STAND_OFF, 0.0)),
            vault_player(),
            RootMotionDriven {
                rotation: false,
                ..RootMotionDriven::default()
            },
        ))
        .id();
    VaultRig { hub, hero }
}

fn vault_at_300(t: u32) -> Pad {
    Pad {
        forward: 0,
        vault: t == 300,
    }
}

/// § 13 test 11: a montage started from predicted code moves the server and the client by the
/// same root motion, with no correction and with the input locked out while it drives.
#[test]
fn root_motion_is_identical_on_server_and_client() {
    let mut r = vault_rig(41, vault_at_300);
    r.hub.run(520);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let (a, b) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    assert_eq!(a, b, "same bits");
    // The clip's whole 3 m, plus the slide of braking from its speed when it ends.
    assert!(a.z < -3.0 && a.z > -3.3, "{a}");
    assert!(
        !r.hub
            .server
            .world
            .get::<NetMontagePlay>(r.hero)
            .expect("play")
            .is_playing(),
        "cleared at the end"
    );
    let trace = r.hub.clients[0].world.resource::<Trace>();
    let moved: f32 = trace.first.values().map(|d| -d.z).sum();
    assert!(
        (moved + a.z).abs() < 1e-3,
        "the backend was asked for it all: {moved}"
    );
}

/// § 13 test 11, resimulation: corrections on both sides of the montage's start re-run it, and
/// every re-run tick asks the backend for exactly the bits of the first run.
#[test]
fn root_motion_replays_bit_for_bit_across_its_start() {
    let mut r = vault_rig(42, vault_at_300);
    let mut forced = 0;
    for _ in 0..520 {
        r.hub.step();
        let tick = r
            .hub
            .server
            .world
            .resource::<regia_net::runtime::NetNode>()
            .tick
            .0;
        // The server is about 13 ticks behind the client tick: these corrections arrive with the
        // start of the montage inside the re-run span, then with it in the past.
        if matches!(tick, 280 | 292 | 298 | 310 | 340) {
            force_correction(&mut r.hub.server.world, r.hero, Vec3::new(0.05, 0.0, 0.0)).expect("forced");
            forced += 1;
        }
    }
    let s = stats(&r.hub);
    assert_eq!(s.corrections, forced, "{s:?}");
    let trace = r.hub.clients[0].world.resource::<Trace>();
    let mut compared = 0;
    let mut across_start = 0;
    for (tick, again) in &trace.again {
        let Some(first) = trace.first.get(tick) else {
            continue;
        };
        for delta in again {
            assert_eq!(delta.to_array(), first.to_array(), "tick {tick}");
            compared += 1;
            across_start += u32::from((300..=301).contains(tick));
        }
    }
    assert!(compared > 40, "{compared}");
    assert!(across_start > 0, "a re-run crossed the start");
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    let (a, b) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    assert_eq!(a, b, "same bits at the end");
    assert!((a.x - 30.25).abs() < 1e-3, "every shove kept: {a}");
}

/// `prediction.verify_resim`: every tick of a montage re-run from the same start gives the same
/// result, through the real re-run path.
#[test]
fn verify_resim_finds_the_montage_step_deterministic() {
    let mut r = vault_rig(44, vault_at_300);
    for w in [&mut r.hub.server.world, &mut r.hub.clients[0].world] {
        w.resource_mut::<NetSettings>().prediction.verify_resim = true;
    }
    r.hub.run(520);
    let s = stats(&r.hub);
    assert_eq!(s.verify_failures, 0, "{s:?}");
    assert_eq!(s.corrections, 0, "{s:?}");
}

/// A montage the client predicted and the server never started: the correction undoes it and the
/// rig stops showing it.
#[test]
fn a_refused_montage_is_rolled_back() {
    let mut r = vault_rig(43, vault_at_300);
    fn refuse(mut q: Query<&mut NetMontagePlay>) {
        for mut p in &mut q {
            if p.is_playing() {
                *p = NetMontagePlay::default();
            }
        }
    }
    r.hub.server.gameplay.add_systems(refuse);
    r.hub.run(340);
    let ch = copy_of(&mut r.hub, 0, NetRole::Predicted).expect("copy");
    r.hub.run(180);
    let s = stats(&r.hub);
    assert!(s.corrections >= 1, "{s:?}");
    let (a, b) = (pos(&r.hub.server.world, r.hero), pos(&r.hub.clients[0].world, ch));
    assert!(a.distance(b) < 1e-3, "{a} {b}");
    assert!(
        !r.hub.clients[0]
            .world
            .get::<NetMontagePlay>(ch)
            .expect("play")
            .is_playing()
    );
}

// ---- step_character (§ 9, § 13 test 15) ---------------------------------------------------------

fn character_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CharacterPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 62.5,
        )));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed);
    app
}

/// `step_character` is the movement system's step: the same inputs give the same bits.
#[test]
fn step_character_is_the_movement_systems_step() {
    let mut app = character_app();
    let bundle = (
        Transform::from_translation(Vec3::new(0.0, 3.0, 0.0)),
        Character,
        CharacterMovement::default(),
        CharacterInput::default(),
        CharacterState::default(),
    );
    let by_system = app.world_mut().spawn(bundle.clone()).id();
    let by_hand = app.world_mut().spawn((bundle, CharacterSteppedExternally)).id();
    let query = regia_framework::character::FlatPlaneQuery;
    let input_at = |t: u32| CharacterInput {
        move_dir: if t < 60 { Vec3::X } else { Vec3::Z * 0.5 },
        jump: t.is_multiple_of(47),
        crouch: (150..170).contains(&t),
        sprint: (80..120).contains(&t),
    };
    for t in 0..240 {
        let input = input_at(t);
        *app.world_mut()
            .get_mut::<CharacterInput>(by_system)
            .expect("input") = input.clone();
        app.update();
        let result = step_character(app.world_mut(), &query, by_hand, &input, 1.0 / 62.5);
        let world = app.world();
        assert_eq!(
            world.get::<Transform>(by_hand),
            world.get::<Transform>(by_system),
            "tick {t}"
        );
        let (a, b) = (
            world.get::<CharacterState>(by_hand).expect("state"),
            world.get::<CharacterState>(by_system).expect("state"),
        );
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "tick {t}");
        assert_eq!(
            result.position,
            world.get::<Transform>(by_hand).expect("t").translation
        );
        assert_eq!(result.ground_entity, None, "the flat stub reports no entity");
    }
}

/// An entity that is no character gets the default result and stays as it is.
#[test]
fn step_character_on_a_non_character_does_nothing() {
    let mut app = character_app();
    let e = app.world_mut().spawn(Transform::from_translation(Vec3::ONE)).id();
    let query = regia_framework::character::FlatPlaneQuery;
    let result = step_character(app.world_mut(), &query, e, &CharacterInput::default(), 0.016);
    assert_eq!(result.position, Vec3::ZERO);
    assert_eq!(
        app.world().get::<Transform>(e),
        Some(&Transform::from_translation(Vec3::ONE))
    );
}

/// `SampledRootMotion` replaces frame-time motion, is mapped by `Transform`s (a stale
/// `GlobalTransform` on the rig changes nothing), and `NONE` leaves the input in charge.
#[test]
fn sampled_root_motion_reads_no_frame_state() {
    let mut app = character_app();
    let rig = app
        .world_mut()
        .spawn((
            Transform::from_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)),
            regia_scene::GlobalTransform::default(),
        ))
        .id();
    let e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, STAND_OFF, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            CharacterSteppedExternally,
            RootMotionDriven {
                rig: Some(rig),
                rotation: false,
                ..RootMotionDriven::default()
            },
        ))
        .id();
    let query = regia_framework::character::FlatPlaneQuery;
    app.world_mut().entity_mut(e).insert(SampledRootMotion {
        translation: Vec3::new(0.0, 0.0, -1.0),
        rotation: Quat::IDENTITY,
        seconds: 0.02,
    });
    step_character(app.world_mut(), &query, e, &CharacterInput::default(), 0.02);
    let p = pos(app.world(), e);
    assert!(
        p.distance(Vec3::new(-1.0, STAND_OFF, 0.0)) < 1e-4,
        "the rig's turn applies: {p}"
    );
    assert!(app.world().get::<SampledRootMotion>(e).is_none(), "consumed");
    app.world_mut().entity_mut(e).insert(SampledRootMotion::NONE);
    let input = CharacterInput {
        move_dir: Vec3::Z,
        ..CharacterInput::default()
    };
    step_character(app.world_mut(), &query, e, &input, 0.02);
    assert!(pos(app.world(), e).z > 0.0, "input drives while nothing plays");
}

// ---- rapier (§ 9) -------------------------------------------------------------------------------

fn rapier_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 62.5,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(MinimalPlugins)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(CharacterPlugin);
    use_rapier_character_controller(&mut app);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(50.0, 0.5, 50.0),
            },
            ..Collider::default()
        },
    ));
    app
}

/// A dynamic box with a character walking into it (`None`: no character); returns the box's
/// fastest speed along the push.
fn pushed_speed(resimulating: Option<bool>) -> f32 {
    let mut app = rapier_app();
    let crate_box = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(1.0, 0.5, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    if let Some(resimulating) = resimulating {
        app.insert_resource(NetResimulating(resimulating));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(-0.1, STAND_OFF, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput {
                move_dir: Vec3::X,
                ..CharacterInput::default()
            },
            CharacterState::default(),
        ));
    }
    let mut fastest = 0.0f32;
    for _ in 0..40 {
        app.update();
        // Along the push: the box also settles on the floor.
        let speed = app
            .world()
            .get::<Velocity>(crate_box)
            .map_or(0.0, |v| v.linear.x.abs());
        fastest = fastest.max(speed);
    }
    fastest
}

/// § 9: a re-run tick pushes nothing; the first run does.
#[test]
fn resimulation_applies_no_push_impulses() {
    let alone = pushed_speed(None);
    let pushed = pushed_speed(Some(false));
    assert!(
        pushed > 0.05 && pushed > 10.0 * alone,
        "the first run pushes: {pushed}, alone {alone}"
    );
    assert_eq!(
        pushed_speed(Some(true)),
        alone,
        "a re-run leaves the box as if nobody walked into it"
    );
}

/// § 9: the rapier backend reports what the character stands on.
#[test]
fn rapier_reports_the_ground_entity() {
    let mut app = rapier_app();
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(3.0, 0.1, 3.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    app.update();
    app.update();
    let e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.1 + STAND_OFF + 0.02, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterState::default(),
            CharacterSteppedExternally,
        ))
        .id();
    let query = regia_framework::character::RapierCharacterQuery;
    let mut last = MoveResult::default();
    for _ in 0..10 {
        app.update();
        last = step_character(app.world_mut(), &query, e, &CharacterInput::default(), 1.0 / 62.5);
    }
    assert!(last.grounded, "{last:?}");
    assert_eq!(last.ground_entity, Some(platform), "{last:?}");
}

// ---- the plugin in an app -----------------------------------------------------------------------

/// `NetCharacterPlugin` in an offline app: the character moves once per tick (not by the movement
/// system as well), the rig shows the montage, and the report lists it.
#[test]
fn the_plugin_runs_offline() {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(NetPlugin {
        mode: NetMode::Offline,
    })
    .add_plugins(CharacterPlugin)
    .add_plugins(NetCharacterPlugin);
    let e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, STAND_OFF, 0.0)),
            NetCharacter::default(),
            CharacterInput {
                move_dir: Vec3::X,
                ..CharacterInput::default()
            },
            vault_player(),
            RootMotionDriven {
                mode: RootMotionApply::Off,
                ..RootMotionDriven::default()
            },
        ))
        .id();
    for _ in 0..30 {
        app.update();
    }
    let by_net = pos(app.world(), e).x;
    assert!(by_net > 0.0, "{by_net}");
    // 30 ticks of acceleration at 20 m/s², capped by the walk speed: a double step would be twice.
    let single = (0..30).fold((0.0f32, 0.0f32), |(x, v), _| {
        let v = (v + 20.0 * DT).min(6.0);
        (x + v * DT, v)
    });
    assert!((by_net - single.0).abs() < 0.2, "{by_net} vs {}", single.0);
    app.world_mut()
        .get_mut::<NetMontagePlay>(e)
        .expect("play")
        .start("vault", 0, NetTick(0), 1.0);
    app.update();
    assert!(app.world().get::<MontagePlayer>(e).expect("player").is_active());
    let rows = net_character_rows(app.world_mut());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity, e);
    assert_eq!(rows[0].role, "authority");
    assert_eq!(
        rows[0].montage.as_ref().map(|m| m.montage.as_str()),
        Some("vault")
    );
    assert_eq!(rows[0].net_id, app.world().get::<NetId>(e).copied());
}

/// § 6.6 on real rapier: a client's proxy raft (dynamic in the prefab, kinematic as a proxy) moves
/// by replicated `Transform` writes, and a rider stays where it stands on the deck: the anchor
/// replaces the backend's carry instead of adding to it. Rising and sinking keep it on the deck.
#[test]
fn a_rapier_rider_keeps_its_place_on_a_proxy_base() {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(NetPlugin {
            mode: NetMode::Offline,
        })
        .add_plugins(CharacterPlugin)
        .add_plugins(NetCharacterPlugin);
    use_rapier_character_controller(&mut app);
    let raft = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 0.1, 6.0),
                },
                ..Collider::default()
            },
            NetRole::Proxy,
            NetId(7),
        ))
        .id();
    app.world_mut()
        .resource_mut::<regia_net::runtime::NetEntities>()
        .insert(NetId(7), raft);
    let rider = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(1.0, 2.1 + STAND_OFF + 0.02, 0.0)),
            NetCharacter::default(),
        ))
        .id();
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(
        app.world().get::<RigidBody>(raft).expect("body").kind,
        regia_physics::BodyKind::Kinematic,
        "a proxy follows"
    );
    let settled = base_of(app.world(), rider);
    assert_eq!(settled.base, Some(NetId(7)), "standing on the raft");
    let path = [
        Vec3::new(3.0, 0.0, 0.0),
        Vec3::new(-2.0, 0.0, 4.0),
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
    ];
    for velocity in path {
        for _ in 0..60 {
            if let Some(mut t) = app.world_mut().get_mut::<Transform>(raft) {
                t.translation += velocity * DT;
            }
            app.update();
        }
        let now = base_of(app.world(), rider);
        assert_eq!(now.base, Some(NetId(7)), "still riding at {velocity}");
        assert!(
            now.local.distance(settled.local) < 0.03,
            "at {velocity}: {:?} left {:?}",
            now.local,
            settled.local
        );
        let state = app.world().get::<CharacterState>(rider).expect("state");
        assert!(state.grounded, "at {velocity}");
    }
}

/// § 6.6 on real rapier, when the rider's sweep stops at something: a railing that moves with the
/// proxy raft. rapier's controller carries a character with kinematic ground on its own once its
/// cast stopped at a contact; that carry must not come on top of the anchor. A rider pressing
/// against the railing keeps its place while the raft slides, rises and sinks (ADR-5972).
#[test]
fn a_rapier_rider_at_a_railing_keeps_its_place_on_a_moving_proxy_base() {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(NetPlugin {
            mode: NetMode::Offline,
        })
        .add_plugins(CharacterPlugin)
        .add_plugins(NetCharacterPlugin);
    use_rapier_character_controller(&mut app);
    let raft = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 0.1, 6.0),
                },
                ..Collider::default()
            },
            NetRole::Proxy,
            NetId(7),
        ))
        .id();
    app.world_mut()
        .resource_mut::<regia_net::runtime::NetEntities>()
        .insert(NetId(7), raft);
    // The railing is its own kinematic body, moved with the raft below; its inner face is at
    // z = 1 and it starts above the deck.
    let railing = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.1 + 0.05 + 0.75, 1.25)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 0.75, 0.25),
                },
                ..Collider::default()
            },
        ))
        .id();
    let rider = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(1.0, 2.1 + STAND_OFF + 0.02, 0.0)),
            NetCharacter::default(),
            CharacterInput {
                move_dir: Vec3::Z,
                ..CharacterInput::default()
            },
        ))
        .id();
    for _ in 0..90 {
        app.update();
    }
    let pressed = base_of(app.world(), rider);
    assert_eq!(pressed.base, Some(NetId(7)), "standing on the raft");
    assert!(
        (pressed.local.z - (1.0 - 0.3 - 0.02)).abs() < 0.02 && (pressed.local.x - 1.0).abs() < 0.02,
        "walked straight to the railing: {:?}",
        pressed.local
    );
    let path = [
        Vec3::new(3.0, 0.0, 0.0),
        Vec3::new(-2.0, 0.0, 4.0),
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
    ];
    for velocity in path {
        for _ in 0..60 {
            for part in [raft, railing] {
                if let Some(mut t) = app.world_mut().get_mut::<Transform>(part) {
                    t.translation += velocity * DT;
                }
            }
            app.update();
        }
        let now = base_of(app.world(), rider);
        assert_eq!(now.base, Some(NetId(7)), "still riding at {velocity}");
        assert!(
            now.local.distance(pressed.local) < 0.03,
            "at {velocity}: {:?} left {:?}",
            now.local,
            pressed.local
        );
        let state = app.world().get::<CharacterState>(rider).expect("state");
        assert!(state.grounded, "at {velocity}");
    }
}

// ---- two apps over a delay line: the real schedules, rapier on both sides ----------------------

/// A replicated hull. Its body is dynamic and weightless on the server, which steers it with
/// velocities; the client's copy gets the same body, and the plugin makes that one follow.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hull {
    half: f32,
}

fn hull_body() -> (RigidBody, Collider) {
    (
        RigidBody {
            gravity_scale: 0.0,
            ..RigidBody::dynamic()
        },
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(6.0, 0.1, 6.0),
            },
            ..Collider::default()
        },
    )
}

fn equip_hull(mut commands: Commands, new: Query<Entity, Added<Hull>>) {
    for e in &new {
        commands.entity(e).insert(hull_body());
    }
}

fn steer_hull(node: Res<regia_net::runtime::NetNode>, mut hulls: Query<&mut Velocity, With<Hull>>) {
    let velocity = match node.tick.0 {
        0..=299 => Vec3::ZERO,
        300..=419 => Vec3::new(3.0, 0.0, 0.0),
        420..=479 => Vec3::new(-2.0, 0.0, 4.0),
        480..=599 => Vec3::new(1.0, 0.0, -5.0),
        _ => Vec3::ZERO,
    };
    for mut v in &mut hulls {
        v.linear = velocity;
        v.angular = Vec3::ZERO;
    }
}

fn feed_app(cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<CharacterNetInput>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = pacing(next);
    }
}

fn net_app(mode: NetMode, peer: Option<PeerId>) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.insert_resource(regia_net::LocalPeer(peer))
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(NetPlugin { mode })
        .add_plugins(CharacterPlugin)
        .add_plugins(NetCharacterPlugin);
    use_rapier_character_controller(&mut app);
    register_replicated::<Hull>(app.world_mut(), ReplicateOptions::default());
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(500.0, 0.5, 500.0),
            },
            ..Collider::default()
        },
    ));
    app
}

/// Server and client apps with a 6-tick delay line between them.
struct AppPair {
    server: App,
    client: App,
    line: Vec<(u64, bool, regia_net::runtime::Datagram)>,
    step: u64,
}

impl AppPair {
    fn step(&mut self) {
        const DELAY: u64 = 6;
        self.step += 1;
        let now = self.step;
        let (due, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut self.line)
            .into_iter()
            .partition(|(at, _, _)| *at <= now);
        self.line = wait;
        for (_, to_server, d) in due {
            let app = if to_server {
                &mut self.server
            } else {
                &mut self.client
            };
            app.world_mut()
                .resource_mut::<regia_net::runtime::NetNode>()
                .inbox
                .push(d);
        }
        self.server.update();
        let out = std::mem::take(
            &mut self
                .server
                .world_mut()
                .resource_mut::<regia_net::runtime::NetNode>()
                .outbox,
        );
        self.line.extend(out.into_iter().map(|d| (now + DELAY, false, d)));
        self.client.update();
        let out = std::mem::take(
            &mut self
                .client
                .world_mut()
                .resource_mut::<regia_net::runtime::NetNode>()
                .outbox,
        );
        self.line.extend(out.into_iter().map(|mut d| {
            d.peer = PeerId(1);
            (now + DELAY, true, d)
        }));
    }
}

/// § 13 test 10 end to end: `NetPlugin`, `PhysicsPlugin` and `NetCharacterPlugin` in a server app and
/// a client app, a rider on a hull that physics moves on the server and that reaches the client 6
/// ticks late. The rider paces on it with no correction; a shove is one correction that does not
/// pop; both sides end at the same place on the hull.
#[test]
fn two_apps_ride_a_physics_hull_on_rapier() {
    let mut server = net_app(NetMode::DedicatedServer, Some(PeerId::SERVER));
    let mut client = net_app(NetMode::Client, Some(PeerId(1)));
    regia_net::runtime::server_add_peer(server.world_mut(), PeerId(1));
    set_transport_rtt(client.world_mut(), 13.0 * 1000.0 / 60.0, 0.0);
    server.add_systems(FixedUpdate, steer_hull.before(regia_physics::PhysicsStep));
    client.add_systems(
        FixedUpdate,
        equip_hull
            .after(regia_net::NetSet::Apply)
            .before(regia_physics::PhysicsStep),
    );
    client.add_systems(FixedUpdate, feed_app.before(regia_net::NetSet::Predict));
    let hull = server
        .world_mut()
        .spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            Hull { half: 6.0 },
            hull_body(),
            Velocity::default(),
        ))
        .id();
    let hero = server
        .world_mut()
        .spawn(hero_bundle(Vec3::new(1.0, 2.1 + STAND_OFF + 0.02, 0.0)))
        .id();
    let mut pair = AppPair {
        server,
        client,
        line: Vec::new(),
        step: 0,
    };
    for _ in 0..300 {
        pair.step();
    }
    let corrections_at_rest = pair
        .client
        .world()
        .resource::<ClientPrediction>()
        .stats
        .corrections;
    let ch = {
        let w = pair.client.world_mut();
        let mut q = w.query_filtered::<(Entity, &NetRole), With<NetCharacter>>();
        q.iter(w)
            .find(|(_, r)| **r == NetRole::Predicted)
            .map(|(e, _)| e)
            .expect("predicted copy")
    };
    assert!(
        base_of(pair.client.world(), ch).base.is_some(),
        "the copy stands on the hull"
    );
    let copy = {
        let w = pair.client.world_mut();
        let mut q = w.query_filtered::<Entity, With<Hull>>();
        q.iter(w).next().expect("hull copy")
    };
    assert_eq!(
        pair.client.world().get::<RigidBody>(copy).expect("body").kind,
        regia_physics::BodyKind::Kinematic,
        "the client's hull follows"
    );

    let mut worst_lag = 0.0f32;
    for _ in 0..460 {
        pair.step();
        let lag = pos(pair.server.world(), hull).distance(pos(pair.client.world(), copy));
        worst_lag = worst_lag.max(lag);
    }
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert!(worst_lag > 0.3, "the client's hull is late: {worst_lag}");
    assert_eq!(stats.corrections, corrections_at_rest, "{stats:?}");
    assert_eq!(stats.resets, 0, "{stats:?}");
    let (a, b) = (
        base_of(pair.server.world(), hero),
        base_of(pair.client.world(), ch),
    );
    assert_eq!(a.base, b.base);
    assert!(a.local.distance(b.local) < 0.01, "{a:?} {b:?}");
    let (sp, cp) = (pos(pair.server.world(), hero), pos(pair.client.world(), ch));
    assert!(
        sp.distance(cp) < 0.02,
        "the hull stopped, the worlds agree: {sp} {cp}"
    );

    force_correction(pair.server.world_mut(), hero, Vec3::new(1.0, 0.0, 0.0)).expect("forced");
    let mut last = rendered(pair.client.world(), ch);
    let mut worst = 0.0f32;
    for _ in 0..60 {
        pair.step();
        let drawn = rendered(pair.client.world(), ch);
        worst = worst.max(drawn.distance(last));
        last = drawn;
    }
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert_eq!(stats.corrections, corrections_at_rest + 1, "{stats:?}");
    assert!(worst < 0.3, "no pop: {worst}");
    let (a, b) = (
        base_of(pair.server.world(), hero),
        base_of(pair.client.world(), ch),
    );
    assert!(a.local.distance(b.local) < 0.01, "{a:?} {b:?}");
}

// ---- remote animation from replicated state (ADR-2551) ---------------------------------------

mod remote_animation {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use regia_animation::components::{Animator, SkinnedMesh, spawn_skeleton};
    use regia_animation::events::{AnimNotify, AnimStateEvent};
    use regia_animation::pose::Pose;
    use regia_animation::pose_eval::demo::{AIM, JUMP, SPEED, demo_net_character};
    use regia_animation::pose_eval::{
        AnimStateSnapshot, ParamLayout, PoseRequest, capture_state, evaluate_pose,
    };
    use regia_animation::skeleton::Skeleton;
    use regia_animation::systems::evaluate_animators;
    use regia_animation::{MontageEvent, ScriptedWarpEvent, SocketTargets};
    use regia_app::Time;
    use regia_core::math::Vec3;
    use regia_ecs::prelude::*;
    use regia_framework::net_character::{
        NetAnimShown, NetAnimState, install_net_animation, net_anim_capture, net_anim_drive,
    };
    use regia_net::interp::{NetInterpolation, install_interpolation, net_interpolate};
    use regia_net::predict::tick_step_seconds_of;
    use regia_net::runtime::NetEntities;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetId, NetMode, Replicated};
    use regia_scene::Transform;

    const HZ: f64 = 60.0;
    const MSPT: f64 = 1000.0 / HZ;
    const STEP: Duration = Duration::from_nanos(16_666_667);

    /// The client's driven pose against the server's own state evaluated at the shown time
    /// (rig space, every joint): equal up to float noise.
    const EXACT_M: f32 = 1e-5;
    /// The client's pose against a linear blend of the server's poses of the two ticks around the
    /// shown time. The blend is a chord of the animation between two ticks; the client shows the
    /// animation itself, which bends away from the chord in a fast swing (measured worst: 3.4 cm,
    /// three quarters of a tick into the jump's run at 1.5 times speed).
    const BLEND_TOLERANCE_M: f32 = 0.05;

    fn setup(world: &mut World) {
        install_net_animation(world);
    }

    fn server_gameplay(world: &mut World) {
        net_anim_capture(world);
    }

    /// What the animation systems need on a bare world.
    fn animation_world(world: &mut World) {
        world.init_resource::<Time>();
        world.init_resource::<SocketTargets>();
        world.init_resource::<Messages<AnimNotify>>();
        world.init_resource::<Messages<AnimStateEvent>>();
        world.init_resource::<Messages<MontageEvent>>();
        world.init_resource::<Messages<ScriptedWarpEvent>>();
    }

    fn rig(world: &mut World) -> (SkinnedMesh, Animator, Arc<Skeleton>) {
        let (skeleton, graph, clips) = demo_net_character().expect("demo rig");
        let skeleton = Arc::new(skeleton);
        (
            spawn_skeleton(world, skeleton.clone()),
            Animator::graph(graph, clips),
            skeleton,
        )
    }

    fn joint_positions(skeleton: &Skeleton, pose: &Pose) -> Vec<Vec3> {
        skeleton
            .globals(pose, None)
            .iter()
            .map(|m| m.transform_point3(Vec3::ZERO))
            .collect()
    }

    fn max_distance(a: &[Vec3], b: &[Vec3]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| x.distance(*y))
            .fold(0.0f32, f32::max)
    }

    /// Spec 74, ADR-2551: two in-process peers (a server and an interpolating client, 3 ticks of
    /// latency each way). The server's character walks, runs, jumps twice and raises an eased
    /// upper-body layer; the client's copy of the rig is driven only by the replicated
    /// [`NetAnimState`]. On every frame that re-anchors the client's animator (each new server
    /// tick shown), its evaluated joints equal (`EXACT_M`) the server's own state of that tick
    /// evaluated at the shown time, and the words that crossed the link equal the server's
    /// snapshot. On every frame the joints are within `BLEND_TOLERANCE_M` of the blend of the
    /// server's two tick poses around the shown time, and exactly on it when the client's frames
    /// fall on the server's ticks.
    #[test]
    fn a_remote_character_animates_from_its_interpolated_state() {
        // Client frames on the tick, and a quarter, half and three quarters of a tick after it
        // (the shown tick is then fractional and the re-anchor carries a lead).
        for offset in [0.0, 0.25, 0.5, 0.75] {
            remote_run(offset);
        }
    }

    /// One run of [`a_remote_character_animates_from_its_interpolated_state`] with the client's
    /// frames `offset` ticks after the server's.
    fn remote_run(offset: f64) {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
        hub.down.delay_ticks = 3;
        hub.server.gameplay.add_systems(server_gameplay);
        let client = hub.add_client();
        install_interpolation(&mut hub.clients[client].world, HZ, 1.0);
        animation_world(&mut hub.server.world);
        animation_world(&mut hub.clients[client].world);
        let mut frame_s = Schedule::default();
        frame_s.add_systems(evaluate_animators);
        let mut frame_c = Schedule::default();
        frame_c.add_systems((net_anim_drive, evaluate_animators).chain());

        let (binding, animator, skeleton) = rig(&mut hub.server.world);
        let server = hub
            .server
            .world
            .spawn((
                Replicated::default(),
                Transform::default(),
                binding,
                animator,
                NetAnimState::default(),
            ))
            .id();
        // Server tick -> joint positions of the pose the server evaluated for it, and the
        // server's own capture of the state that evaluation started from.
        let mut server_joints: BTreeMap<u64, Vec<Vec3>> = BTreeMap::new();
        let mut server_states: BTreeMap<u32, AnimStateSnapshot> = BTreeMap::new();
        let (mut blended, mut exact, mut reanchors) = (0, 0, 0);
        let (mut worst_blend, mut worst_exact) = (0.0f32, 0.0f32);
        let mut last_shown = None;
        let mut prev_interp: Option<f64> = None;
        for step in 1..=300u64 {
            let f = step as f32;
            if let Some(mut a) = hub.server.world.get_mut::<Animator>(server) {
                a.set(SPEED, (f * 0.03).min(3.2));
                a.set(AIM, if (120..200).contains(&step) { 1.0 } else { 0.0 });
                if step == 90 || step == 220 {
                    a.trigger(JUMP);
                }
            }
            let now = step as f64 * MSPT;
            hub.clients[client]
                .world
                .resource_mut::<NetInterpolation>()
                .now_ms = now;
            hub.step();
            let tick = u64::from(hub.server_tick().0).saturating_sub(1);
            if let Ok(state) = capture_state(hub.server.world.get::<Animator>(server).expect("animator")) {
                server_states.insert(tick as u32, state);
            }
            // The frame after the tick: the server evaluates the tick's pose.
            hub.server.world.resource_mut::<Time>().advance(STEP);
            frame_s.run(&mut hub.server.world);
            let pose = hub
                .server
                .world
                .get::<Animator>(server)
                .expect("animator")
                .evaluated_pose()
                .clone();
            server_joints.insert(tick, joint_positions(&skeleton, &pose));

            let cw = &mut hub.clients[client].world;
            let Some(id) = hub.server.world.get::<NetId>(server).copied() else {
                continue;
            };
            let copy = cw.resource::<NetEntities>().entity(id);
            if let Some(copy) = copy
                && cw.get::<Animator>(copy).is_none()
            {
                // The client's prefab brings the same rig.
                let (binding, animator, _) = rig(cw);
                cw.entity_mut(copy).insert((binding, animator));
            }
            cw.resource_mut::<Time>().advance(STEP);
            frame_c.run(cw);
            // The drive used the state shown at the previous frame's interpolation tick.
            let (Some(copy), Some(shown_at)) = (copy, prev_interp) else {
                net_interpolate(cw, now + offset * MSPT);
                prev_interp = cw.resource::<NetInterpolation>().clock.interp_tick();
                continue;
            };
            let client_joints = cw
                .get::<Animator>(copy)
                .map(|a| joint_positions(&skeleton, a.evaluated_pose()));
            let shown = cw.get::<NetAnimShown>(copy).map(|s| s.tick);
            if let (Some(tick_shown), Some(client_joints)) = (shown, client_joints.as_ref())
                && shown != last_shown
            {
                // Re-anchored this frame on `tick_shown`: the reference is the server's own state
                // of that tick, evaluated with the parameters and lead the drive used.
                last_shown = shown;
                reanchors += 1;
                let shown_state = cw.get::<NetAnimState>(copy).cloned().expect("shown state");
                let server_state = server_states.get(&tick_shown).expect("server captured the tick");
                assert_eq!(
                    shown_state.words.as_slice(),
                    server_state.words(),
                    "the words crossed the link intact"
                );
                let animator = cw.get::<Animator>(copy).expect("animator");
                let params = animator
                    .graph_def()
                    .map(ParamLayout::declared)
                    .expect("graph")
                    .decode(&shown_state.params);
                let lead = shown_state.past * tick_step_seconds_of(cw);
                let request = PoseRequest {
                    skeleton: &skeleton,
                    source: &animator.source,
                    state: server_state,
                    params: &params,
                    delta: lead + cw.resource::<Time>().delta_secs(),
                    rig: copy,
                    socket_targets: None,
                };
                let mut reference = Pose::default();
                evaluate_pose(&request, &mut reference).expect("reference");
                let err = max_distance(client_joints, &joint_positions(&skeleton, &reference));
                worst_exact = worst_exact.max(err);
                assert!(
                    err < EXACT_M,
                    "offset {offset}: step {step}: client pose {err} m from the server's state of tick {tick_shown} at {shown_at}"
                );
                exact += 1;
            }
            let lo = server_joints.range(..=shown_at.floor() as u64).next_back();
            let hi = server_joints.range(shown_at.ceil() as u64..).next();
            if let (Some(_), Some(client_joints), Some((ta, a)), Some((tb, b))) =
                (shown, client_joints, lo, hi)
            {
                let t = if tb > ta {
                    ((shown_at - *ta as f64) / (*tb - *ta) as f64) as f32
                } else {
                    0.0
                };
                let blend: Vec<Vec3> = a.iter().zip(b).map(|(x, y)| x.lerp(*y, t)).collect();
                let err = max_distance(&client_joints, &blend);
                worst_blend = worst_blend.max(err);
                let bound = if offset <= 0.0 { EXACT_M } else { BLEND_TOLERANCE_M };
                assert!(
                    err < bound,
                    "offset {offset}: step {step}: client pose {err} m from the blend of the server's poses at {shown_at}"
                );
                blended += 1;
            }
            net_interpolate(cw, now + offset * MSPT);
            prev_interp = cw.resource::<NetInterpolation>().clock.interp_tick();
        }
        eprintln!(
            "remote animation, offset {offset}: {exact} re-anchored frames, worst {worst_exact} m from the server's \
             state; {blended} frames, worst {worst_blend} m from the blend of the server's tick poses"
        );
        assert!(
            blended > 200,
            "the client showed the character for most of the run: {blended}"
        );
        assert!(
            reanchors > 200 && exact == reanchors,
            "the animator re-anchored on each new server tick: {reanchors}"
        );
    }

    /// Without the drive, the client's rig runs on its own clock: the same run is out of
    /// tolerance (the test above is not passing by accident).
    #[test]
    fn without_the_drive_the_client_pose_drifts() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
        hub.down.delay_ticks = 3;
        hub.server.gameplay.add_systems(server_gameplay);
        let client = hub.add_client();
        install_interpolation(&mut hub.clients[client].world, HZ, 1.0);
        animation_world(&mut hub.server.world);
        animation_world(&mut hub.clients[client].world);
        let mut frame = Schedule::default();
        frame.add_systems(evaluate_animators);
        let mut frame_c = Schedule::default();
        frame_c.add_systems(evaluate_animators);
        let (binding, animator, skeleton) = rig(&mut hub.server.world);
        let server = hub
            .server
            .world
            .spawn((
                Replicated::default(),
                Transform::default(),
                binding,
                animator,
                NetAnimState::default(),
            ))
            .id();
        let mut worst = 0.0f32;
        for step in 1..=200u64 {
            if let Some(mut a) = hub.server.world.get_mut::<Animator>(server) {
                a.set(SPEED, 3.2);
                if step == 90 {
                    a.trigger(JUMP);
                }
            }
            hub.step();
            hub.server.world.resource_mut::<Time>().advance(STEP);
            frame.run(&mut hub.server.world);
            let id = hub.server.world.get::<NetId>(server).copied().expect("id");
            let sp = joint_positions(
                &skeleton,
                hub.server
                    .world
                    .get::<Animator>(server)
                    .expect("a")
                    .evaluated_pose(),
            );
            let cw = &mut hub.clients[client].world;
            let Some(copy) = cw.resource::<NetEntities>().entity(id) else {
                continue;
            };
            if cw.get::<Animator>(copy).is_none() {
                let (binding, animator, _) = rig(cw);
                cw.entity_mut(copy).insert((binding, animator));
            }
            cw.resource_mut::<Time>().advance(STEP);
            frame_c.run(cw);
            let cp = joint_positions(&skeleton, cw.get::<Animator>(copy).expect("a").evaluated_pose());
            worst = worst.max(
                sp.iter()
                    .zip(&cp)
                    .map(|(a, b)| a.distance(*b))
                    .fold(0.0, f32::max),
            );
        }
        assert!(
            worst > BLEND_TOLERANCE_M,
            "an undriven rig drifts from the server: {worst} m"
        );
    }
}
