// SPDX-License-Identifier: MPL-2.0
//! Traversal in the predicted schedule (`docs/specs/traversal.md` § 8): two rapier apps with a delay
//! line. A mantle predicts with no correction; a correction forced during a mantle converges to the
//! server, and the capsule is never inside the wall.

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_framework::character::{
    CharacterMovement, CharacterPlugin, CharacterState, MovementMode, use_rapier_character_controller,
};
use regia_framework::net_character::{CharacterNetInput, NetCharacter, NetCharacterPlugin};
use regia_framework::traversal::{
    CharacterTraversal, FlatWater, Ladder, TraversalKind, TraversalPlugin, TraversalSettings, TraversalWater,
};
use regia_net::predict::{ClientPrediction, InputCommand, force_correction, set_transport_rtt};
use regia_net::replication::NetPredicted;
use regia_net::runtime::{Datagram, NetNode};
use regia_net::{NetMode, NetOwner, NetPlugin, NetRole, PeerId, Replicated, TransformReplication};
use regia_physics::{
    Collider, ColliderShape, PhysicsLayers, PhysicsWorld, QueryFilter, RigidBody, WorldQueries,
};
use regia_scene::Transform;

const STAND: f32 = 1.2;

/// The client's script: what to press at each tick.
#[derive(Resource, Clone, Copy)]
struct Script(fn(u32) -> CharacterNetInput);

fn feed(script: Res<Script>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<CharacterNetInput>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

/// Both sides hold the same tuning; a character arriving on the client gets it here.
fn equip(mut commands: Commands, new: Query<Entity, Added<NetCharacter>>) {
    for e in &new {
        commands.entity(e).insert(TraversalSettings::default());
    }
}

fn solid(app: &mut App, half: Vec3, at: Vec3) {
    app.world_mut().spawn((
        Transform::from_translation(at),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box { half_extents: half },
            ..Collider::default()
        },
    ));
}

/// `origin`: the app's floating origin. Its level is the same world, at local positions minus the origin.
fn net_app(mode: NetMode, peer: Option<PeerId>, origin: Vec3) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.insert_resource(regia_net::LocalPeer(peer))
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(NetPlugin { mode })
        .add_plugins(CharacterPlugin)
        .add_plugins(TraversalPlugin)
        .add_plugins(NetCharacterPlugin);
    use_rapier_character_controller(&mut app);
    // The floor, a block with a 1.5 m ledge at x = 2, a deep pool beyond and a ladder.
    if origin != Vec3::ZERO {
        app.insert_resource(regia_scene::streaming::OriginOffset([
            f64::from(origin.x),
            f64::from(origin.y),
            f64::from(origin.z),
        ]));
    }
    solid(
        &mut app,
        Vec3::new(100.0, 0.5, 100.0),
        Vec3::new(0.0, -0.5, 0.0) - origin,
    );
    solid(
        &mut app,
        Vec3::new(5.0, 0.75, 4.0),
        Vec3::new(7.0, 0.75, 0.0) - origin,
    );
    app
}

/// Server and client apps with a 6-tick delay line between them.
struct Pair {
    server: App,
    client: App,
    line: Vec<(u64, bool, Datagram)>,
    step: u64,
}

impl Pair {
    fn new(script: fn(u32) -> CharacterNetInput, extra: fn(&mut App)) -> (Self, Entity) {
        Self::with_client_origin(script, extra, Vec3::ZERO)
    }

    fn with_client_origin(
        script: fn(u32) -> CharacterNetInput,
        extra: fn(&mut App),
        origin: Vec3,
    ) -> (Self, Entity) {
        let mut server = net_app(NetMode::DedicatedServer, Some(PeerId::SERVER), Vec3::ZERO);
        let mut client = net_app(NetMode::Client, Some(PeerId(1)), origin);
        extra(&mut server);
        extra(&mut client);
        regia_net::runtime::server_add_peer(server.world_mut(), PeerId(1));
        set_transport_rtt(client.world_mut(), 13.0 * 1000.0 / 60.0, 0.0);
        client.insert_resource(Script(script));
        client.add_systems(
            FixedUpdate,
            equip
                .after(regia_net::NetSet::Apply)
                .before(regia_net::NetSet::Predict),
        );
        client.add_systems(FixedUpdate, feed.before(regia_net::NetSet::Predict));
        let hero = server
            .world_mut()
            .spawn((
                Replicated {
                    transform: TransformReplication::Movement,
                    ..Replicated::default()
                },
                Transform::from_translation(Vec3::new(1.4, STAND + 0.02, 0.0)),
                NetCharacter::default(),
                CharacterState {
                    grounded: true,
                    ..CharacterState::default()
                },
                TraversalSettings::default(),
                NetPredicted,
                NetOwner(PeerId(1)),
            ))
            .id();
        (
            Self {
                server,
                client,
                line: Vec::new(),
                step: 0,
            },
            hero,
        )
    }

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
            app.world_mut().resource_mut::<NetNode>().inbox.push(d);
        }
        self.server.update();
        let out = std::mem::take(&mut self.server.world_mut().resource_mut::<NetNode>().outbox);
        self.line.extend(out.into_iter().map(|d| (now + DELAY, false, d)));
        self.client.update();
        let out = std::mem::take(&mut self.client.world_mut().resource_mut::<NetNode>().outbox);
        self.line.extend(out.into_iter().map(|mut d| {
            d.peer = PeerId(1);
            (now + DELAY, true, d)
        }));
    }

    fn copy(&mut self) -> Option<Entity> {
        let w = self.client.world_mut();
        let mut q = w.query_filtered::<(Entity, &NetRole), With<NetCharacter>>();
        q.iter(w).find(|(_, r)| **r == NetRole::Predicted).map(|(e, _)| e)
    }

    fn corrections(&self) -> u64 {
        self.client
            .world()
            .resource::<ClientPrediction>()
            .stats
            .corrections
    }
}

fn pos(world: &World, e: Entity) -> Vec3 {
    world.get::<Transform>(e).expect("transform").translation
}

fn embedded(world: &World, e: Entity, at: Vec3) -> bool {
    let queries = WorldQueries {
        world: world.resource::<PhysicsWorld>(),
        layers: world.get_resource::<PhysicsLayers>(),
    };
    let movement = world.get::<CharacterMovement>(e).expect("movement");
    let shape = ColliderShape::Capsule {
        half_height: movement.half_height,
        radius: movement.radius - 0.015,
        axis: regia_physics::ShapeAxis::Y,
    };
    let exclude = [e];
    !queries
        .overlap(
            &shape,
            Transform::from_translation(at),
            QueryFilter::new().exclude_sensors().exclude_entities(&exclude),
        )
        .is_empty()
}

/// At tick 200 and on: into the wall, jump once.
fn mantle_at_200(t: u32) -> CharacterNetInput {
    if t < 200 {
        return CharacterNetInput::default();
    }
    // The stick's +y is forward along the yaw: yaw -90 degrees looks down +X.
    CharacterNetInput {
        move_dir: if t < 203 { Vec2::Y } else { Vec2::ZERO },
        yaw: -std::f32::consts::FRAC_PI_2,
        jump: t == 201,
        ..CharacterNetInput::default()
    }
}

fn no_extra(_: &mut App) {}

#[test]
fn a_mantle_predicts_with_no_correction_and_both_sides_end_on_the_ledge() {
    let (mut pair, hero) = Pair::new(mantle_at_200, no_extra);
    for _ in 0..150 {
        pair.step();
    }
    let at_rest = pair.corrections();
    let mut copy_mantled = false;
    let mut server_mantled = false;
    for _ in 0..240 {
        pair.step();
        server_mantled |= pair
            .server
            .world()
            .get::<CharacterTraversal>(hero)
            .is_some_and(|t| t.kind == TraversalKind::Mantle);
        if let Some(c) = pair.copy() {
            let t = *pair
                .client
                .world()
                .get::<CharacterTraversal>(c)
                .expect("traversal copy");
            copy_mantled |= t.kind == TraversalKind::Mantle;
            if t.kind == TraversalKind::Mantle {
                let mode = pair.client.world().get::<CharacterState>(c).expect("state").mode;
                assert_eq!(mode, MovementMode::TRAVERSING);
            }
            assert!(!embedded(pair.client.world(), c, pos(pair.client.world(), c)));
        }
    }
    assert!(server_mantled, "the server ran it");
    assert!(copy_mantled, "the client predicted it");
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert_eq!(stats.corrections, at_rest, "{stats:?}");
    let c = pair.copy().expect("copy");
    let (s, k) = (pos(pair.server.world(), hero), pos(pair.client.world(), c));
    assert!(s.distance(k) < 0.02, "agree: {s} {k}");
    assert!(
        s.x > 2.0 && (s.y - (1.5 + STAND)).abs() < 0.08,
        "on the ledge: {s}"
    );
    let state = pair.server.world().get::<CharacterState>(hero).expect("state");
    assert!(state.grounded && state.mode == MovementMode::Walking, "{state:?}");
}

#[test]
fn a_correction_forced_during_a_mantle_converges_with_no_capsule_embedded() {
    let (mut pair, hero) = Pair::new(mantle_at_200, no_extra);
    for _ in 0..150 {
        pair.step();
    }
    // Run until the server is partway up the wall, then move its copy, as a knock would.
    let mut guard = 0;
    while pair
        .server
        .world()
        .get::<CharacterTraversal>(hero)
        .is_none_or(|t| t.kind != TraversalKind::Mantle || t.step < 8)
    {
        pair.step();
        guard += 1;
        assert!(guard < 300, "the mantle started");
    }
    let before = pair.corrections();
    force_correction(pair.server.world_mut(), hero, Vec3::new(-0.3, 0.0, 0.0)).expect("forced");

    let c = pair.copy().expect("copy");
    for _ in 0..240 {
        pair.step();
        let (sw, cw) = (pair.server.world(), pair.client.world());
        assert!(!embedded(sw, hero, pos(sw, hero)), "server: {}", pos(sw, hero));
        assert!(!embedded(cw, c, pos(cw, c)), "client: {}", pos(cw, c));
    }
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert!(stats.corrections > before, "a correction ran: {stats:?}");
    let (s, k) = (pos(pair.server.world(), hero), pos(pair.client.world(), c));
    assert!(s.distance(k) < 0.02, "converged: {s} {k}");
    let (ts, tc) = (
        *pair
            .server
            .world()
            .get::<CharacterTraversal>(hero)
            .expect("server"),
        *pair.client.world().get::<CharacterTraversal>(c).expect("client"),
    );
    assert_eq!(ts.kind, tc.kind);
    assert_eq!(ts.kind, TraversalKind::None, "the move is over");
    let (ms, mc) = (
        pair.server
            .world()
            .get::<CharacterMovement>(hero)
            .expect("server")
            .mode,
        pair.client
            .world()
            .get::<CharacterMovement>(c)
            .expect("client")
            .mode,
    );
    assert_eq!(ms, mc, "the settings mode follows");
    // Settled: no correction storm afterwards.
    let settled = pair.corrections();
    for _ in 0..120 {
        pair.step();
    }
    assert_eq!(pair.corrections(), settled, "no more corrections at rest");
}

/// A ladder stands at the wall of a second block, reached by walking.
fn with_ladder(app: &mut App) {
    solid(app, Vec3::new(4.0, 1.5, 4.0), Vec3::new(4.0, 1.5, -12.0));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(1.4, 0.0, -8.0)),
        Ladder {
            height: 3.0,
            user_id: 5,
            ..Ladder::default()
        },
    ));
}

fn climb_from_100(t: u32) -> CharacterNetInput {
    if t < 100 {
        return CharacterNetInput::default();
    }
    // Yaw 0 looks down -Z, toward the ladder at z = -8.
    CharacterNetInput {
        move_dir: Vec2::Y,
        ..CharacterNetInput::default()
    }
}

#[test]
fn a_ladder_climb_predicts_with_no_correction() {
    let (mut pair, hero) = Pair::new(climb_from_100, with_ladder);
    for _ in 0..90 {
        pair.step();
    }
    let at_rest = pair.corrections();
    let mut climbed = false;
    let mut on_top = false;
    for _ in 0..600 {
        pair.step();
        let world = pair.server.world();
        let kind = world.get::<CharacterTraversal>(hero).map(|t| t.kind);
        climbed |= kind == Some(TraversalKind::Ladder);
        let grounded = world.get::<CharacterState>(hero).is_some_and(|s| s.grounded);
        on_top |= kind == Some(TraversalKind::None) && grounded && pos(world, hero).y > 3.0 + STAND - 0.1;
    }
    assert!(climbed, "the server climbed: {}", pos(pair.server.world(), hero));
    assert!(
        on_top,
        "it stood on the upper floor: {}",
        pos(pair.server.world(), hero)
    );
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert_eq!(stats.corrections, at_rest, "{stats:?}");
}

/// A flat water level both sides know, under the start point.
fn with_water(app: &mut App) {
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: 3.0,
        min: [-50.0, -50.0],
        max: [50.0, 50.0],
    })));
}

#[test]
fn a_swim_predicts_with_no_correction_at_the_float_line() {
    fn wander(t: u32) -> CharacterNetInput {
        CharacterNetInput {
            move_dir: if (150..300).contains(&t) {
                Vec2::Y
            } else {
                Vec2::ZERO
            },
            yaw: std::f32::consts::FRAC_PI_2,
            crouch: (350..390).contains(&t),
            ..CharacterNetInput::default()
        }
    }
    let (mut pair, hero) = Pair::new(wander, with_water);
    for _ in 0..100 {
        pair.step();
    }
    let at_rest = pair.corrections();
    for _ in 0..500 {
        pair.step();
    }
    let t = *pair
        .server
        .world()
        .get::<CharacterTraversal>(hero)
        .expect("traversal");
    assert_eq!(t.kind, TraversalKind::Swim);
    let s = pos(pair.server.world(), hero);
    assert!((s.y - 2.5).abs() < 0.05, "at the line: {s}");
    assert!(s.x < 1.4, "swam: {s}");
    let c = pair.copy().expect("copy");
    let k = pos(pair.client.world(), c);
    assert!(s.distance(k) < 0.02, "agree: {s} {k}");
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert_eq!(stats.resets, 0, "{stats:?}");
    assert_eq!(stats.corrections, at_rest, "{stats:?}");
}

/// A correction that rewinds across the tick a mantle started re-runs the start: the move still happens,
/// and the host hears of it once.
#[test]
fn a_rerun_of_a_mantle_start_sends_no_second_event() {
    use regia_framework::traversal::TraversalEvent;

    let (mut pair, hero) = Pair::new(mantle_at_200, no_extra);
    // Stop just before the client's start reaches the server, so the server's moved state lands on a
    // client that has already started.
    let mut cursor = pair
        .client
        .world()
        .resource::<Messages<TraversalEvent>>()
        .get_cursor();
    let mut started = 0;
    let mut forced = false;
    let mut count = |pair: &mut Pair, cursor: &mut regia_ecs::ecs::message::MessageCursor<TraversalEvent>| {
        let messages = pair.client.world().resource::<Messages<TraversalEvent>>();
        started += cursor
            .read(messages)
            .filter(|e| {
                matches!(
                    e,
                    TraversalEvent::Started {
                        kind: TraversalKind::Mantle,
                        ..
                    }
                )
            })
            .count();
    };
    for _ in 0..420 {
        pair.step();
        count(&mut pair, &mut cursor);
        let client_started = pair.copy().is_some_and(|c| {
            pair.client
                .world()
                .get::<CharacterTraversal>(c)
                .is_some_and(|t| t.kind == TraversalKind::Mantle)
        });
        let server_started = pair
            .server
            .world()
            .get::<CharacterTraversal>(hero)
            .is_some_and(|t| t.kind != TraversalKind::None);
        if client_started && !server_started && !forced {
            forced = true;
            force_correction(pair.server.world_mut(), hero, Vec3::new(-0.05, 0.0, 0.0)).expect("forced");
        }
    }
    assert!(forced, "the client ran ahead of the server");
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert!(stats.corrections >= 1, "a rollback ran: {stats:?}");
    assert_eq!(started, 1, "one start for the host");
    let c = pair.copy().expect("copy");
    let (s, k) = (pos(pair.server.world(), hero), pos(pair.client.world(), c));
    assert!(s.distance(k) < 0.02, "converged: {s} {k}");
    assert!(
        s.x > 2.0 && (s.y - (1.5 + STAND)).abs() < 0.08,
        "the move happened: {s}"
    );
}

/// `prediction.verify_resim` runs every tick twice from the same start through the real re-run path and
/// counts any difference: each kind of move lands on the same bits.
#[test]
fn verify_resim_finds_every_move_deterministic() {
    fn swim_and_dive(t: u32) -> CharacterNetInput {
        CharacterNetInput {
            move_dir: if (150..300).contains(&t) {
                Vec2::Y
            } else {
                Vec2::ZERO
            },
            yaw: std::f32::consts::FRAC_PI_2,
            crouch: (350..390).contains(&t),
            ..CharacterNetInput::default()
        }
    }
    type Case = (&'static str, fn(u32) -> CharacterNetInput, fn(&mut App), usize);
    let cases: [Case; 3] = [
        ("mantle", mantle_at_200, no_extra, 420),
        ("ladder", climb_from_100, with_ladder, 600),
        ("swim", swim_and_dive, with_water, 500),
    ];
    for (name, script, extra, steps) in cases {
        let (mut pair, hero) = Pair::new(script, extra);
        for world in [pair.server.world_mut(), pair.client.world_mut()] {
            world
                .resource_mut::<regia_net::NetSettings>()
                .prediction
                .verify_resim = true;
        }
        let mut moved = false;
        for _ in 0..steps {
            pair.step();
            moved |= pair
                .server
                .world()
                .get::<CharacterTraversal>(hero)
                .is_some_and(|t| t.kind != TraversalKind::None);
        }
        assert!(moved, "{name}: the move ran");
        let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
        assert_eq!(stats.verify_failures, 0, "{name}: {stats:?}");
    }
}

/// A peer with another floating origin predicts a mantle with no correction: the state crosses the wire
/// with its frame, and the comparison is in the shared world frame.
///
/// Ignored until `regia_net` converts a predicted root's `Transform` to the local frame: today a plain
/// predicted character with no traversal stays at the server's raw position on such a client and corrects
/// at rest, so this test cannot pass whatever traversal does. The frame logic itself is covered by
/// `traversal::net` unit tests and `an_origin_rebase_during_a_move_does_not_yank_the_character`.
#[test]
#[ignore = "blocked by regia_net: a predicted root's Transform is not converted to the client's origin"]
fn a_client_with_another_origin_predicts_a_mantle_with_no_correction() {
    let origin = Vec3::new(1024.0, 0.0, -512.0);
    let (mut pair, hero) = Pair::with_client_origin(mantle_at_200, no_extra, origin);
    for _ in 0..150 {
        pair.step();
    }
    let at_rest = pair.corrections();
    let mut copy_mantled = false;
    for _ in 0..240 {
        pair.step();
        if let Some(c) = pair.copy() {
            let world = pair.client.world();
            copy_mantled |= world
                .get::<CharacterTraversal>(c)
                .is_some_and(|t| t.kind == TraversalKind::Mantle);
            assert!(!embedded(world, c, pos(world, c)), "{}", pos(world, c));
        }
    }
    let c = pair.copy().expect("copy");
    let (s, k) = (pos(pair.server.world(), hero), pos(pair.client.world(), c));
    let stats = pair.client.world().resource::<ClientPrediction>().stats.clone();
    assert!(
        copy_mantled,
        "the client predicted it: server {s}, client {k}, {:?}, {stats:?}",
        pair.client.world().get::<CharacterTraversal>(c)
    );
    assert_eq!(stats.corrections, at_rest, "{stats:?}");
    assert!(s.distance(k + origin) < 0.02, "one world: {s} {k}");
    assert!(
        s.x > 2.0 && (s.y - (1.5 + STAND)).abs() < 0.08,
        "on the ledge: {s}"
    );
}
