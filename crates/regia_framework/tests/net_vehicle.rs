// SPDX-License-Identifier: MPL-2.0
//! ADR-2784: vehicles in multiplayer, over the in-memory `LoopbackHub` (no sockets, fixed steps, the
//! link's delay and loss under a seeded generator), with real rapier on every process.
//!
//! The acceptance of the lane's brief, at 100 ms of delay each way and 2 % loss: an owner driving a
//! scripted course stays within 0.25 m of the server at 20 m/s, and the drawn pose never jumps over
//! 0.1 m per step except across the snap threshold; a third peer's interpolated car stays within
//! 0.5 m of the server's position delayed by the interpolation time; the server's state is
//! bit-identical to a run with no clients; a seat change at speed loses no input and leaves one
//! owner; a malformed input is refused with a documented code.

use std::collections::BTreeMap;

use regia_app::FixedTime;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::system::RunSystemOnce;
use regia_framework::character::{CharacterState, FlatPlaneQuery, MovementMode};
use regia_framework::net_character::{CharacterNetInput, NetCharacter};
use regia_framework::net_vehicle::{
    CODE_INPUT_RANGE, CODE_INPUT_SHIFT, DriverNetInput, NetSeated, NetVehicle, NetVehiclePlugin,
    NetVehicleSmoothing, NetVehicleState, NetVehicleTunings, NetVehicleVisual, SEATED_MODE_ID,
    VehicleCommand, VehicleNetInput, VehicleSeat, VehicleSeatLog, VehicleSeatRequest, VehicleSeats,
    VehicleShift, enter_seat, exit_seat, install_net_driver, install_net_vehicle, net_vehicle_rows,
    net_vehicle_server_apply, net_vehicle_step, spawn_net_vehicle,
};
use regia_net::bits::{BitWriter, DecodeError};
use regia_net::interp::{NetInterpolation, install_interpolation, net_interpolate};
use regia_net::message::Channel;
use regia_net::predict::{
    ClientPrediction, InputCommand, NetVisualOffset, PredictionState, ServerPrediction,
    add_predicted_systems, set_transport_rtt,
};
use regia_net::replication::{ConnWire, NetPredicted, ServerReplication};
use regia_net::runtime::{Datagram, NetNode};
use regia_net::settings::StateChannel;
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::{
    NetId, NetMode, NetOwner, NetPlayer, NetRole, NetSettings, PeerId, Replicated, TransformReplication,
};
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehicleInput, drive_vehicles};
use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};
use regia_physics::vehicle_snapshot::add_static_box;
use regia_physics::vehicle_tire::{SlipTire, TireModel};
use regia_physics::{BodyBudget, Gravity, PhysicsSettings, PhysicsWorld, step_physics};
use regia_scene::Transform;

const HZ: f64 = 60.0;
const DT: f32 = 1.0 / 60.0;
/// 100 ms each way.
const DELAY_TICKS: u64 = 6;
const LOSS: f64 = 0.02;
const SIMPLE: &str = "simple";
const GEARED: &str = "geared";
const SLIP: &str = "slip";

// ---- the level ----------------------------------------------------------------------------------

/// A big flat track with shallow bumps, so the suspension works, and a wall far off the course.
fn level(world: &mut World) {
    world.insert_resource(FixedTime::default());
    world.init_resource::<PhysicsSettings>();
    world.init_resource::<Gravity>();
    world.init_resource::<BodyBudget>();
    let mut physics = PhysicsWorld::default();
    add_static_box(
        &mut physics,
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(4000.0, 1.0, 4000.0),
    );
    for i in 0..40 {
        let z = -40.0 - i as f32 * 20.0;
        add_static_box(&mut physics, Vec3::new(0.0, -0.46, z), Vec3::new(30.0, 0.5, 1.0));
    }
    world.insert_resource(physics);
    let mut tunings = NetVehicleTunings::default();
    tunings.insert(SIMPLE, VehicleConfig::default());
    tunings.insert(
        GEARED,
        VehicleConfig {
            drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
            ..VehicleConfig::default()
        },
    );
    let mut slip = VehicleConfig::default();
    for wheel in &mut slip.wheels {
        wheel.tire_model = TireModel::Slip(SlipTire::default());
    }
    tunings.insert(SLIP, slip);
    world.insert_resource(tunings);
}

fn setup(world: &mut World) {
    world.resource_mut::<NetSettings>().replication.send_every_ticks = 2;
    level(world);
    install_net_vehicle::<VehicleNetInput>(world).expect("net vehicle");
    add_predicted_systems(
        world,
        log_tick::<VehicleNetInput>.after(net_vehicle_step::<VehicleNetInput>),
    );
}

fn driver_setup(world: &mut World) {
    world.resource_mut::<NetSettings>().replication.send_every_ticks = 2;
    level(world);
    world.insert_resource(regia_framework::character::CharacterCollisionQuery {
        query: Box::new(FlatPlaneQuery),
    });
    install_net_driver::<DriverNetInput>(world).expect("net driver");
    add_predicted_systems(
        world,
        log_tick::<DriverNetInput>
            .after(net_vehicle_step::<DriverNetInput>)
            .before(regia_framework::net_vehicle::NetVehicleSeatSet),
    );
}

// ---- what each process records ------------------------------------------------------------------

/// One tick of one vehicle as this process left it.
#[derive(Debug, Clone, Copy)]
struct Row {
    pose: Transform,
    drawn: Vec3,
    speed: f32,
    velocity: Vec3,
    snaps: u64,
    input: VehicleNetInput,
    owner: Option<PeerId>,
    digest: u64,
}

/// Every first run of a tick, per process, by tick. On a client only the predicted vehicle.
#[derive(Resource, Default)]
struct Log {
    rows: BTreeMap<u32, Row>,
    /// Which car it records (on a server: all ticks of this one).
    car: Option<Entity>,
}

fn log_tick<I: VehicleCommand>(world: &mut World) {
    let Some(state) = world.get_resource::<PredictionState>().copied() else {
        return;
    };
    if state.resimulating {
        return;
    }
    if !world.contains_resource::<Log>() {
        return;
    }
    let car = if state.server {
        world.resource::<Log>().car
    } else {
        let mut q = world.query::<(Entity, &NetVehicle, &NetRole)>();
        q.iter(world)
            .find(|(_, _, role)| **role == NetRole::Predicted)
            .map(|(e, _, _)| e)
    };
    let Some(car) = car else {
        return;
    };
    let Some(pose) = world.get::<Transform>(car).copied() else {
        return;
    };
    let state_row = world.get::<NetVehicleState>(car).cloned().unwrap_or_default();
    let visual = world.get::<NetVehicleVisual>(car).copied().unwrap_or_default();
    let input = if state.server {
        let applied = world.get::<VehicleInput>(car).copied().unwrap_or_default();
        VehicleNetInput::from_vehicle_input(applied)
    } else {
        let mut q = world.query::<(&NetPlayer, &InputCommand<I>)>();
        q.iter(world)
            .next()
            .map_or_else(VehicleNetInput::default, |(_, c)| c.value.vehicle_command())
    };
    let digest = world
        .get::<VehicleController>(car)
        .zip(world.get_resource::<PhysicsWorld>())
        .and_then(|(c, physics)| c.vehicle.snapshot(physics).ok())
        .map_or(0, |s| s.digest());
    let row = Row {
        pose,
        drawn: visual.rendered(&pose).translation,
        speed: state_row.linear_velocity.length(),
        velocity: state_row.linear_velocity,
        snaps: visual.snaps,
        input,
        owner: world.get::<NetOwner>(car).map(|o| o.0),
        digest,
    };
    world.resource_mut::<Log>().rows.insert(state.tick.0, row);
}

// ---- the scripted course ------------------------------------------------------------------------

/// Full throttle to about 22 m/s, then just enough to hold it, with a slow weave across the bumps.
fn course(t: u32) -> VehicleNetInput {
    VehicleNetInput {
        throttle: if t < 270 { 1.0 } else { 0.02 },
        brake: 0.0,
        steer: if t > 340 {
            ((t as f32) * 0.02).sin() * 0.12
        } else {
            0.0
        },
        ..VehicleNetInput::default()
    }
}

/// A geared car on the default tune pulls harder at the bottom and shifts on the way: longer to 20 m/s.
fn geared_course(t: u32) -> VehicleNetInput {
    VehicleNetInput {
        throttle: if t < 620 { 1.0 } else { 0.1 },
        steer: if t > 660 {
            ((t as f32) * 0.02).sin() * 0.12
        } else {
            0.0
        },
        ..VehicleNetInput::default()
    }
}

#[derive(Resource, Clone, Copy)]
struct Script(fn(u32) -> VehicleNetInput);

fn feed(script: Res<Script>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<VehicleNetInput>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

// ---- the rig ------------------------------------------------------------------------------------

struct Rig {
    hub: LoopbackHub,
    car: Entity,
}

fn conditions(delay: u64, loss: f64) -> LinkConditions {
    LinkConditions {
        delay_ticks: delay,
        loss,
        ..LinkConditions::default()
    }
}

fn add_client(hub: &mut LoopbackHub, unreliable: bool, delay: u64) -> usize {
    let c = hub.add_client();
    set_transport_rtt(
        &mut hub.clients[c].world,
        (2 * delay + 1) as f64 * 1000.0 / HZ,
        0.0,
    );
    if unreliable {
        let peer = hub.clients[c].peer;
        hub.clients[c]
            .world
            .resource_mut::<NetSettings>()
            .replication
            .state_channel = StateChannel::Unreliable;
        hub.server
            .world
            .resource_mut::<ServerReplication>()
            .set_wire(peer, ConnWire::Acked);
    }
    c
}

/// A server, `clients` clients at `delay` ticks each way with `loss`, a car owned by client 0.
fn rig(seed: u64, tuning: &str, delay: u64, loss: f64, clients: usize, unreliable: bool) -> Rig {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    hub.down = conditions(delay, loss);
    hub.up = hub.down;
    if unreliable {
        hub.server
            .world
            .resource_mut::<NetSettings>()
            .replication
            .state_channel = StateChannel::Unreliable;
    }
    for _ in 0..clients {
        let c = add_client(&mut hub, unreliable, delay);
        hub.clients[c]
            .world
            .insert_resource(Script(if tuning == GEARED { geared_course } else { course }));
        hub.clients[c].world.init_resource::<Log>();
        hub.clients[c].gameplay.add_systems(feed);
    }
    hub.server.gameplay.add_systems(
        (
            net_vehicle_server_apply::<VehicleNetInput>,
            drive_vehicles,
            step_physics,
        )
            .chain(),
    );
    let owner = (clients > 0).then_some(PeerId(1));
    let car = spawn_net_vehicle(
        &mut hub.server.world,
        tuning,
        (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
        owner,
    )
    .expect("car");
    hub.server.world.insert_resource(Log {
        car: Some(car),
        ..Log::default()
    });
    Rig { hub, car }
}

fn log_of(world: &World) -> &BTreeMap<u32, Row> {
    &world.resource::<Log>().rows
}

/// Compares the owner's tick log with the server's: error at each common tick, and the drawn pose's
/// visible jump per tick (its motion less what the velocity explains), outside snaps.
fn owner_against_server(server: &World, client: &World, from: u32) -> (f32, f32, f32, u32) {
    let (s, c) = (log_of(server), log_of(client));
    let (mut max_error, mut max_jump, mut top_speed, mut common) = (0.0f32, 0.0f32, 0.0f32, 0u32);
    let mut previous: Option<(u32, Row)> = None;
    for (tick, row) in c.iter().filter(|(t, _)| **t >= from) {
        if let Some(truth) = s.get(tick) {
            max_error = max_error.max(row.pose.translation.distance(truth.pose.translation));
            top_speed = top_speed.max(truth.speed);
            common += 1;
        }
        if let Some((last, before)) = previous
            && last + 1 == *tick
            && before.snaps == row.snaps
        {
            let jump = (row.drawn - before.drawn - row.velocity * DT).length();
            max_jump = max_jump.max(jump);
        }
        previous = Some((*tick, *row));
    }
    (max_error, max_jump, top_speed, common)
}

// ---- acceptance: the owner ----------------------------------------------------------------------

fn owner_stays_on_the_server(tuning: &str, unreliable: bool) {
    let mut r = rig(41, tuning, DELAY_TICKS, LOSS, 1, unreliable);
    r.hub.run(1500);
    let (error, jump, speed, common) =
        owner_against_server(&r.hub.server.world, &r.hub.clients[0].world, 120);
    let stats = r.hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .stats
        .clone();
    eprintln!(
        "{tuning} unreliable={unreliable}: max error {error:.4} m, max jump {jump:.4} m, top speed {speed:.2} m/s, \
         {common} ticks compared; corrections {}, resets {}, resyncs {}",
        stats.corrections, stats.resets, stats.resyncs
    );
    assert!(common > 1000, "the run compared the course: {common}");
    assert!(speed >= 20.0, "the course reaches 20 m/s: {speed}");
    assert!(error <= 0.25, "{error} m from the server");
    assert!(jump <= 0.1, "{jump} m on screen in one tick");
    let rows = net_vehicle_rows(&mut r.hub.clients[0].world);
    assert_eq!(rows[0].mode, "predicted");
    assert_eq!(rows[0].owner, Some(1));
    assert_eq!(rows[0].fault, None);
}

#[test]
fn an_owner_stays_on_the_server_at_20_ms_on_the_reliable_stream() {
    owner_stays_on_the_server(SIMPLE, false);
}

#[test]
fn an_owner_stays_on_the_server_at_20_ms_on_the_state_channel() {
    owner_stays_on_the_server(SIMPLE, true);
}

#[test]
fn a_geared_owner_stays_on_the_server_at_20_ms() {
    owner_stays_on_the_server(GEARED, true);
}

/// The wire carries no tire state: a correction lets go of the stick anchors, and a rolling slip
/// tire has none.
#[test]
fn a_slip_tire_owner_stays_on_the_server_at_20_ms() {
    owner_stays_on_the_server(SLIP, true);
}

// ---- acceptance: corrections --------------------------------------------------------------------

/// Server-only: at `tick`, shoves the chassis sideways, which no client can predict.
#[derive(Resource, Clone, Copy)]
struct Shove {
    tick: u32,
    velocity: Vec3,
    teleport: Vec3,
    done: bool,
}

fn shove(
    mut shove: ResMut<Shove>,
    node: Res<NetNode>,
    mut physics: ResMut<PhysicsWorld>,
    cars: Query<&VehicleController>,
) {
    if shove.done || node.tick.0 < shove.tick {
        return;
    }
    shove.done = true;
    for car in &cars {
        if let Some(body) = physics.bodies.get_mut(car.vehicle.chassis().to_handle()) {
            let v = regia_physics::convert::from_rapier_vec3(body.linvel()) + shove.velocity;
            body.set_linvel(regia_physics::convert::to_rapier_vec3(v), true);
            let (at, rot) = regia_physics::convert::from_rapier_pose(body.position());
            body.set_position(
                regia_physics::convert::to_rapier_pose(at + shove.teleport, rot),
                true,
            );
        }
    }
}

fn shoved_rig(velocity: Vec3, teleport: Vec3) -> Rig {
    let mut r = rig(43, SIMPLE, DELAY_TICKS, LOSS, 1, true);
    r.hub.server.world.insert_resource(Shove {
        tick: 700,
        velocity,
        teleport,
        done: false,
    });
    r.hub.server.gameplay.add_systems(shove.before(drive_vehicles));
    r
}

/// A shove along the course at speed, which side friction does not eat: the simulated pose corrects by
/// more than a meter, the drawn pose pays it back under the limit, no snap, and the prediction is back
/// on the server after it.
#[test]
fn a_correction_is_smoothed_on_screen() {
    let mut r = shoved_rig(Vec3::new(0.0, 0.0, -6.0), Vec3::ZERO);
    let mut largest = 0.0f32;
    for _ in 0..1200 {
        r.hub.step();
        let w = &mut r.hub.clients[0].world;
        let mut q = w.query::<&NetVehicleVisual>();
        if let Some(visual) = q.iter(w).next() {
            largest = largest.max(visual.last_correction_m);
        }
    }
    let client = &r.hub.clients[0].world;
    let (_, jump, _, _) = owner_against_server(&r.hub.server.world, client, 120);
    let stats = client.resource::<ClientPrediction>().stats.clone();
    assert!(stats.corrections > 0, "the shove was corrected: {stats:?}");
    assert!(jump <= 0.1, "{jump} m on screen in one tick");
    let visual = {
        let w = &mut r.hub.clients[0].world;
        let mut q = w.query::<&NetVehicleVisual>();
        *q.iter(w).next().expect("visual")
    };
    eprintln!(
        "shove: {} corrections, largest correction {largest:.3} m, last {:.3} m, max jump {jump:.4} m, snaps {}",
        visual.corrections, visual.last_correction_m, visual.snaps
    );
    assert!(visual.corrections > 0);
    assert!(
        largest > 1.0,
        "the shove moved it by more than a meter: {largest} m"
    );
    assert_eq!(visual.snaps, 0);
    assert!(visual.offset.length() < 0.05, "paid back by the end: {visual:?}");
    // Long after the shove the owner is on the server again.
    let (error, _, _, common) = owner_against_server(&r.hub.server.world, &r.hub.clients[0].world, 900);
    assert!(common > 200);
    assert!(error <= 0.25, "{error} m from the server after the correction");
    // regia_net's own offset, which visual children follow, stays on the drawn pose.
    let w = &mut r.hub.clients[0].world;
    let mut q = w.query::<&NetVisualOffset>();
    assert!(q.iter(w).all(|o| o.translation.length() < 0.1));
}

/// Past the threshold the drawn pose goes straight to the new one, and that is counted.
#[test]
fn a_large_correction_snaps() {
    let mut r = shoved_rig(Vec3::ZERO, Vec3::new(30.0, 0.0, 0.0));
    r.hub.run(1000);
    let w = &mut r.hub.clients[0].world;
    let mut q = w.query::<&NetVehicleVisual>();
    let visual = *q.iter(w).next().expect("visual");
    assert!(visual.snaps >= 1, "{visual:?}");
    assert!(visual.offset.length() < 0.05, "{visual:?}");
    let (_, jump, _, _) = owner_against_server(&r.hub.server.world, &r.hub.clients[0].world, 120);
    assert!(jump <= 0.1, "{jump} m on screen in one tick, outside the snap");
    let (error, _, _, _) = owner_against_server(&r.hub.server.world, &r.hub.clients[0].world, 850);
    assert!(error <= 0.25, "{error} m after the snap");
}

// ---- acceptance: the third peer -----------------------------------------------------------------

fn third_peer(unreliable: bool) -> (f32, f64, u32) {
    let mut r = rig(47, SIMPLE, DELAY_TICKS, LOSS, 2, unreliable);
    let w = &mut r.hub.clients[1].world;
    install_interpolation(w, HZ, 2.0);
    let (mut worst, mut compared, mut delay_sum) = (0.0f32, 0u32, 0.0f64);
    let mut server: BTreeMap<u64, Vec3> = BTreeMap::new();
    let mut top_speed = 0.0f32;
    for step in 1..=1500u32 {
        let now = f64::from(step) * 1000.0 / HZ;
        r.hub.clients[1].world.resource_mut::<NetInterpolation>().now_ms = now;
        r.hub.step();
        let tick = u64::from(r.hub.server_tick().0).saturating_sub(1);
        let at = r
            .hub
            .server
            .world
            .get::<Transform>(r.car)
            .expect("car")
            .translation;
        server.insert(tick, at);
        top_speed = top_speed.max(
            r.hub
                .server
                .world
                .get::<NetVehicleState>(r.car)
                .expect("state")
                .linear_velocity
                .length(),
        );
        let w = &mut r.hub.clients[1].world;
        net_interpolate(w, now);
        let Some(interp) = w.resource::<NetInterpolation>().clock.interp_tick() else {
            continue;
        };
        let mut q = w.query_filtered::<(&Transform, &NetRole), With<NetVehicle>>();
        let Some((proxy, role)) = q.iter(w).next() else {
            continue;
        };
        assert_eq!(*role, NetRole::Proxy);
        let proxy = proxy.translation;
        let delay = w
            .resource::<NetInterpolation>()
            .clock
            .delay_ms(now)
            .unwrap_or(0.0);
        let (lo, hi) = (interp.floor() as u64, interp.ceil() as u64);
        let (Some(a), Some(b)) = (server.get(&lo), server.get(&hi)) else {
            continue;
        };
        if step < 120 {
            continue;
        }
        let expected = a.lerp(*b, (interp - lo as f64) as f32);
        worst = worst.max(proxy.distance(expected));
        compared += 1;
        delay_sum += delay;
    }
    assert!(top_speed >= 20.0, "{top_speed}");
    (worst, delay_sum / f64::from(compared.max(1)), compared)
}

#[test]
fn a_third_peer_interpolates_within_half_a_meter() {
    for unreliable in [false, true] {
        let (worst, delay, compared) = third_peer(unreliable);
        eprintln!(
            "third peer unreliable={unreliable}: worst {worst:.4} m, mean delay {delay:.1} ms, {compared} frames"
        );
        assert!(compared > 1000, "{compared}");
        assert!(
            worst <= 0.5,
            "{worst} m from the server at the interpolation tick"
        );
    }
}

// ---- acceptance: the server does not care who watches -------------------------------------------

/// The server's run of a car with an owner over a lossy link and a watcher, replayed with its applied
/// inputs on a server with no clients: the same bits at every tick.
#[test]
fn the_server_state_is_bit_identical_with_no_clients() {
    server_is_identical_with_no_clients(SIMPLE);
}

#[test]
fn a_slip_tire_server_is_bit_identical_with_no_clients() {
    server_is_identical_with_no_clients(SLIP);
}

fn server_is_identical_with_no_clients(tuning: &str) {
    let mut with = rig(53, tuning, DELAY_TICKS, LOSS, 2, true);
    with.hub.run(1200);
    let recorded: BTreeMap<u32, Row> = log_of(&with.hub.server.world).clone();
    assert!(recorded.len() > 1000);
    assert!(
        recorded.values().any(|r| r.input.throttle > 0.0),
        "the owner's input reached the car"
    );

    let mut alone = rig(53, tuning, DELAY_TICKS, LOSS, 0, true);
    #[derive(Resource)]
    struct Replay(BTreeMap<u32, VehicleInput>);
    fn replay(replay: Res<Replay>, node: Res<NetNode>, mut cars: Query<&mut VehicleInput>) {
        if let Some(input) = replay.0.get(&node.tick.0) {
            for mut car in &mut cars {
                *car = *input;
            }
        }
    }
    alone.hub.server.world.insert_resource(Replay(
        recorded
            .iter()
            .map(|(t, r)| {
                (
                    *t,
                    VehicleInput {
                        throttle: r.input.throttle,
                        brake: r.input.brake,
                        steer: r.input.steer,
                    },
                )
            })
            .collect(),
    ));
    alone
        .hub
        .server
        .gameplay
        .add_systems(replay.before(drive_vehicles));
    alone.hub.run(1200);
    let solo = log_of(&alone.hub.server.world);
    let mut compared = 0;
    for (tick, row) in &recorded {
        let Some(other) = solo.get(tick) else {
            continue;
        };
        assert_eq!(row.digest, other.digest, "tick {tick}");
        assert_eq!(row.pose.translation, other.pose.translation, "tick {tick}");
        assert_eq!(row.pose.rotation, other.pose.rotation, "tick {tick}");
        compared += 1;
    }
    assert!(compared > 1000, "{compared}");
}

// ---- acceptance: a malformed input --------------------------------------------------------------

/// An input frame by hand: one player, one command with this axis.
fn input_frame(tick: u32, axis: u64, shift: u64) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(5, 8); // the input frame's tag
    w.write_varint(u64::from(tick));
    w.write_varint(1);
    w.write_bits(0, 3);
    w.write_varint(1);
    w.write_varint(u64::from(tick));
    w.write_bool(false);
    w.write_bool(false);
    w.write_bits(axis, 8);
    w.write_bits(0, 8);
    w.write_bits(0, 8);
    w.write_bits(shift, 2);
    w.finish()
}

/// A frame with an axis the encoder cannot write costs the sender its connection
/// (`MalformedPacket`), and the car is untouched.
#[test]
fn a_malformed_input_is_refused() {
    for (axis, shift) in [(0x80, 0), (0, 3)] {
        let mut r = rig(59, SIMPLE, 1, 0.0, 1, false);
        r.hub.run(120);
        let tick = r.hub.server_tick().0 + 4;
        let before = *r.hub.server.world.get::<VehicleInput>(r.car).expect("input");
        r.hub.server.world.resource_mut::<NetNode>().inbox.push(Datagram {
            peer: PeerId(1),
            channel: Channel::Unreliable,
            bytes: input_frame(tick, axis, shift),
        });
        r.hub.run(2);
        assert!(
            !r.hub.clients[0].connected,
            "axis {axis} shift {shift}: the sender is gone"
        );
        let after = *r.hub.server.world.get::<VehicleInput>(r.car).expect("input");
        assert_eq!(after.throttle.signum(), before.throttle.signum());
        assert!(after.throttle >= 0.0, "the bad value never reached the car");
    }
}

/// The well-formed frame beside it goes through: the refusal is about the value, not the framing.
#[test]
fn a_well_formed_frame_by_hand_goes_through() {
    let mut r = rig(59, SIMPLE, 1, 0.0, 1, false);
    r.hub.run(120);
    let tick = r.hub.server_tick().0 + 30;
    r.hub.server.world.resource_mut::<NetNode>().inbox.push(Datagram {
        peer: PeerId(1),
        channel: Channel::Unreliable,
        bytes: input_frame(tick, 40, 0),
    });
    r.hub.run(2);
    assert!(r.hub.clients[0].connected);
}

/// The two refusal codes are what `DecodeError` carries.
#[test]
fn the_refusal_codes_are_documented_constants() {
    assert_eq!(CODE_INPUT_RANGE, "vehicle_input_range");
    assert_eq!(CODE_INPUT_SHIFT, "vehicle_input_shift");
    assert!(
        DecodeError::Invalid(CODE_INPUT_RANGE)
            .to_string()
            .contains(CODE_INPUT_RANGE)
    );
}

// ---- bytes per second ---------------------------------------------------------------------------

/// Everything a peer received per second over the 20 m/s stretch, with `cars` cars.
fn measured_bytes(tuning: &str, cars: usize) -> (f64, f64) {
    let mut r = rig(61, tuning, 0, 0.0, 1, true);
    for i in 1..cars {
        spawn_net_vehicle(
            &mut r.hub.server.world,
            tuning,
            (Vec3::new(10.0 * i as f32, 1.0, 0.0), Quat::IDENTITY),
            Some(PeerId(1)),
        )
        .expect("car");
    }
    r.hub.run(900);
    let log = &r.hub.client_logs[0];
    let (from, to) = (600u64, 900u64);
    let bytes: usize = log
        .iter()
        .filter(|(at, _)| (from..to).contains(at))
        .map(|(_, d)| d.bytes.len())
        .sum();
    let total = bytes as f64 * HZ / (to - from) as f64;
    let row = net_vehicle_rows(&mut r.hub.server.world)
        .into_iter()
        .find_map(|row| row.bytes_per_second)
        .unwrap_or(0.0);
    (total / cars as f64, row)
}

/// The number ADR-2784 and the book quote: bytes per second per vehicle at 30 Hz with every field
/// changing at 20 m/s. A deterministic count, so it can hold a budget.
#[test]
fn bytes_per_second_per_vehicle_at_30_hz() {
    for (tuning, budget) in [(SIMPLE, 2_600.0), (GEARED, 3_800.0)] {
        let (one, one_row) = measured_bytes(tuning, 1);
        let (four, _) = measured_bytes(tuning, 4);
        eprintln!(
            "{tuning}: {one:.0} B/s with one car, {four:.0} B/s per car with four; \
             the pipeline's own count for one car {one_row:.0} B/s"
        );
        assert!(one <= budget, "{tuning}: {one} B/s");
        // The row counts the vehicle's records; the total adds the connection's own frames.
        assert!(
            one_row <= one && one_row >= one * 0.6,
            "{tuning}: the row's {one_row} B/s against {one} B/s measured"
        );
        assert!(four <= budget, "{tuning}: {four} B/s per car with four");
    }
}

// ---- seats --------------------------------------------------------------------------------------

/// Two players, a car at speed, a driver and a passenger; `swap_at`: the driver leaves and the
/// passenger takes the wheel.
struct SeatRig {
    hub: LoopbackHub,
    car: Entity,
    a: Entity,
    b: Entity,
}

/// What each client's player would do with the car, different on each, so the server shows whose
/// input it used.
#[derive(Resource, Clone, Copy)]
struct PeerScript(f32);

fn peer_command(script: f32, t: u32) -> DriverNetInput {
    DriverNetInput {
        character: CharacterNetInput::default(),
        vehicle: VehicleNetInput {
            throttle: script,
            // The low bits name the tick, so a dropped or repeated command shows up.
            steer: ((t % 16) as f32 - 8.0) / 127.0,
            ..VehicleNetInput::default()
        },
    }
}

fn feed_driver(
    script: Res<PeerScript>,
    cp: Res<ClientPrediction>,
    mut q: Query<&mut InputCommand<DriverNetInput>>,
) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = peer_command(script.0, next);
    }
}

fn seat_rig(seed: u64) -> SeatRig {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, driver_setup);
    hub.down = conditions(DELAY_TICKS, LOSS);
    hub.up = hub.down;
    hub.server
        .world
        .resource_mut::<NetSettings>()
        .replication
        .state_channel = StateChannel::Unreliable;
    for script in [1.0f32, 0.75] {
        let c = add_client(&mut hub, true, DELAY_TICKS);
        hub.clients[c].world.insert_resource(PeerScript(script));
        hub.clients[c].world.init_resource::<Log>();
        hub.clients[c].gameplay.add_systems(feed_driver);
    }
    hub.server.gameplay.add_systems(
        (
            net_vehicle_server_apply::<DriverNetInput>,
            drive_vehicles,
            step_physics,
        )
            .chain(),
    );
    let car = spawn_net_vehicle(
        &mut hub.server.world,
        SIMPLE,
        (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
        None,
    )
    .expect("car");
    hub.server.world.entity_mut(car).insert(VehicleSeats {
        seats: vec![
            VehicleSeat {
                offset: Vec3::new(-0.4, 0.3, 0.0),
                exit_offset: Vec3::new(-2.0, 0.5, 0.0),
                driver: true,
                occupant: None,
            },
            VehicleSeat {
                offset: Vec3::new(0.4, 0.3, 0.0),
                exit_offset: Vec3::new(2.0, 0.5, 0.0),
                driver: false,
                occupant: None,
            },
        ],
    });
    let mut character = |peer: u32, x: f32| {
        hub.server
            .world
            .spawn((
                Replicated {
                    transform: TransformReplication::Movement,
                    ..Replicated::default()
                },
                Transform::from_translation(Vec3::new(x, 1.2, 0.0)),
                NetCharacter::default(),
                CharacterState {
                    grounded: true,
                    ..CharacterState::default()
                },
                NetPredicted,
                NetOwner(PeerId(peer)),
            ))
            .id()
    };
    let (a, b) = (character(1, -1.0), character(2, 1.0));
    hub.server.world.insert_resource(Log {
        car: Some(car),
        ..Log::default()
    });
    SeatRig { hub, car, a, b }
}

fn owners(world: &mut World) -> Vec<(Entity, PeerId)> {
    let mut q = world.query_filtered::<(Entity, &NetOwner), With<NetVehicle>>();
    q.iter(world).map(|(e, o)| (e, o.0)).collect()
}

/// The brief's seat test: two players seated, up to speed, then the driver gets out and the passenger
/// takes the wheel in the same tick. From that tick the car runs on the new driver's commands with no
/// gap and no repeat, there is never more or less than one owner, and the new driver predicts.
#[test]
fn a_seat_change_at_speed_loses_no_input_and_leaves_one_owner() {
    let mut r = seat_rig(67);
    r.hub.run(60);
    let world = &mut r.hub.server.world;
    enter_seat(world, r.car, r.a, 0).expect("driver in");
    enter_seat(world, r.car, r.b, 1).expect("passenger in");
    assert_eq!(owners(world), vec![(r.car, PeerId(1))]);
    assert_eq!(
        enter_seat(world, r.car, r.b, 0).expect_err("taken").code(),
        "vehicle_seat_taken"
    );
    // Up to speed under the first driver.
    let mut speed = 0.0;
    for _ in 0..400 {
        r.hub.step();
        speed = r
            .hub
            .server
            .world
            .get::<NetVehicleState>(r.car)
            .expect("state")
            .linear_velocity
            .length();
        assert_eq!(owners(&mut r.hub.server.world).len(), 1);
    }
    assert!(speed >= 15.0, "at speed: {speed}");
    // Everything the server consumed so far was on time.
    let missing_before = missing_inputs(&r.hub.server.world);
    let world = &mut r.hub.server.world;
    let swap_tick = world.resource::<NetNode>().tick.0;
    exit_seat(world, r.a).expect("driver out");
    enter_seat(world, r.car, r.b, 0).expect("passenger to the wheel");
    assert_eq!(
        owners(world),
        vec![(r.car, PeerId(2))],
        "one owner, the new driver"
    );
    // The seats say so too, and the old driver stands outside at speed.
    let seats = world.get::<VehicleSeats>(r.car).expect("seats").clone();
    let b_id = *world.get::<NetId>(r.b).expect("id");
    assert_eq!(seats.seats[0].occupant, Some(b_id));
    assert_eq!(seats.seats[1].occupant, None);
    assert_eq!(world.get::<NetSeated>(r.a).expect("seated").vehicle, None);
    let a_state = world.get::<CharacterState>(r.a).expect("state");
    assert_eq!(a_state.mode, MovementMode::Falling);
    assert!(a_state.velocity.length() >= 15.0, "it leaves at the car's speed");

    for _ in 0..300 {
        r.hub.step();
        assert_eq!(owners(&mut r.hub.server.world).len(), 1, "never two, never none");
    }
    assert_eq!(owners(&mut r.hub.server.world), vec![(r.car, PeerId(2))]);

    // Every tick from the swap on ran on the new driver's own command for that tick.
    let server = log_of(&r.hub.server.world);
    let wire = peer_command(0.75, 0).vehicle.quantized().throttle;
    let mut compared = 0;
    for (tick, row) in server.range(swap_tick..) {
        let expected = peer_command(0.75, *tick).vehicle.quantized();
        assert_eq!(row.owner, Some(PeerId(2)), "tick {tick}");
        assert_eq!(row.input.throttle, wire, "tick {tick}: the new driver's pedal");
        assert_eq!(
            row.input.steer, expected.steer,
            "tick {tick}: that very tick's command"
        );
        compared += 1;
    }
    assert!(compared > 250, "{compared}");
    // And before it, the old driver's.
    for (tick, row) in server.range(swap_tick.saturating_sub(100)..swap_tick) {
        let expected = peer_command(1.0, *tick).vehicle.quantized();
        assert_eq!(row.owner, Some(PeerId(1)), "tick {tick}");
        assert_eq!(row.input.steer, expected.steer, "tick {tick}");
    }
    assert_eq!(
        missing_inputs(&r.hub.server.world),
        missing_before,
        "no command was missing across the change"
    );

    // Both clients know: the new driver predicts, the old one interpolates.
    let rows = net_vehicle_rows(&mut r.hub.clients[1].world);
    assert_eq!(rows[0].mode, "predicted");
    assert_eq!(rows[0].owner, Some(2));
    let rows = net_vehicle_rows(&mut r.hub.clients[0].world);
    assert_eq!(rows[0].mode, "interpolated");
    assert_eq!(rows[0].owner, Some(2));

    // The new driver's prediction stays on the server from a moment after it took over.
    let (error, jump, _, common) =
        owner_against_server(&r.hub.server.world, &r.hub.clients[1].world, swap_tick + 60);
    eprintln!("after the seat change: max error {error:.4} m, max jump {jump:.4} m, {common} ticks");
    // The 0.23 m here is one wheel ray on the server grazing a strip's vertical face, which the client's
    // ray, 4 mm away, misses (ADR-3080, Consequences). Measured without strips, this run stays under 0.035 m.
    assert!(common > 150, "{common}");
    assert!(error <= 0.25, "{error}");
    assert!(jump <= 0.1, "{jump}");
}

fn missing_inputs(world: &World) -> u64 {
    world
        .resource::<ServerPrediction>()
        .players
        .values()
        .map(|stats| stats.missing)
        .sum()
}

/// A seated character rides its seat on every side: on the server and on its owner's client, in the
/// seat's place on the car, with the seated mode, and no correction comes of it.
#[test]
fn a_seated_character_rides_the_seat() {
    let mut r = seat_rig(71);
    r.hub.run(60);
    let world = &mut r.hub.server.world;
    enter_seat(world, r.car, r.a, 0).expect("driver in");
    enter_seat(world, r.car, r.b, 1).expect("passenger in");
    r.hub.run(300);
    let world = &r.hub.server.world;
    let (car_at, a_at) = (
        *world.get::<Transform>(r.car).expect("car"),
        world.get::<Transform>(r.a).expect("a").translation,
    );
    let expected = car_at.translation + car_at.rotation * Vec3::new(-0.4, 0.3, 0.0);
    assert!(a_at.distance(expected) < 0.75, "{a_at} {expected}");
    assert_eq!(
        world.get::<CharacterState>(r.a).expect("state").mode,
        MovementMode::Custom(SEATED_MODE_ID)
    );
    // The passenger's client: its character sits on the car it interpolates, by the seat.
    let w = &mut r.hub.clients[1].world;
    let mut q = w.query::<(&NetSeated, &NetRole, &Transform)>();
    let (seated, at) = q
        .iter(w)
        .find(|(_, role, _)| **role == NetRole::Predicted)
        .map(|(seated, _, at)| (seated.clone(), at.translation))
        .expect("own");
    assert!(seated.vehicle.is_some());
    assert_eq!(seated.seat, 1);
    let mut cars = w.query_filtered::<&Transform, With<NetVehicle>>();
    let car = cars.iter(w).next().expect("car copy").translation;
    assert!(
        at.distance(car) < 1.0,
        "the passenger sits on the car it sees: {at} {car}"
    );
}

/// Refusals: a stranger out of reach, a seat that does not exist, leaving with no seat.
#[test]
fn seat_refusals_have_their_codes() {
    let mut r = seat_rig(73);
    r.hub.run(20);
    let world = &mut r.hub.server.world;
    assert_eq!(
        enter_seat(world, r.car, r.a, 9).expect_err("no seat").code(),
        "vehicle_seat_no_such_seat"
    );
    assert_eq!(
        exit_seat(world, r.a).expect_err("not seated").code(),
        "vehicle_seat_not_seated"
    );
    world.get_mut::<Transform>(r.a).expect("a").translation = Vec3::new(500.0, 1.2, 0.0);
    assert_eq!(
        enter_seat(world, r.car, r.a, 0).expect_err("far").code(),
        "vehicle_seat_too_far"
    );
    assert_eq!(
        enter_seat(world, r.a, r.b, 0).expect_err("not a car").code(),
        "vehicle_seat_no_vehicle"
    );
    assert!(owners(world).is_empty(), "nothing changed");
}

/// The request message end to end: the client asks, the server seats it, and a bad request is counted
/// with its code rather than kicking anyone.
#[test]
fn a_client_requests_its_seat_over_the_wire() {
    let mut r = seat_rig(79);
    r.hub.run(60);
    let id = *r.hub.server.world.get::<NetId>(r.car).expect("id");
    let send = |world: &mut World, request: VehicleSeatRequest| {
        world
            .run_system_once(move |mut sender: regia_net::NetSender| {
                sender.send_session_to_server(request.clone()).expect("send");
            })
            .expect("system");
    };
    send(
        &mut r.hub.clients[0].world,
        VehicleSeatRequest {
            vehicle: Some(id),
            seat: 7,
            enter: true,
        },
    );
    r.hub.run(40);
    {
        let log = r.hub.server.world.resource::<VehicleSeatLog>();
        assert_eq!(log.refused, 1);
        assert_eq!(log.last_refusal, Some("vehicle_seat_no_such_seat"));
    }
    send(
        &mut r.hub.clients[0].world,
        VehicleSeatRequest {
            vehicle: Some(id),
            seat: 0,
            enter: true,
        },
    );
    r.hub.run(40);
    assert_eq!(r.hub.server.world.resource::<VehicleSeatLog>().accepted, 1);
    assert_eq!(owners(&mut r.hub.server.world), vec![(r.car, PeerId(1))]);
    send(
        &mut r.hub.clients[0].world,
        VehicleSeatRequest {
            enter: false,
            ..VehicleSeatRequest::default()
        },
    );
    r.hub.run(40);
    assert_eq!(r.hub.server.world.resource::<VehicleSeatLog>().accepted, 2);
    assert!(
        owners(&mut r.hub.server.world).is_empty(),
        "an empty driver's seat leaves no owner"
    );
    let input = *r.hub.server.world.get::<VehicleInput>(r.car).expect("input");
    assert_eq!(input, VehicleInput::default(), "and released controls");
}

/// What the session's default hand-off does when a peer is gone (`OwnedHandOff::Server`): it strips
/// `NetOwner` from everything the peer owned. `despawn_character`: the game's rule despawns the pawn.
fn hand_off_to_server(world: &mut World, entities: &[Entity], despawn_character: Option<Entity>) {
    for entity in entities {
        world.entity_mut(*entity).remove::<NetOwner>();
    }
    if let Some(character) = despawn_character {
        world.despawn(character);
    }
}

/// rc1-rev-net-k 1: a driver whose peer leaves with the throttle held. The session hands the car and
/// the pawn to the server; the car must release its controls and its driver's seat, and another
/// player must be able to take the wheel. Before the fix the car kept the last pedals for ever.
#[test]
fn a_driver_who_leaves_releases_the_car_and_its_seat() {
    let mut r = seat_rig(89);
    r.hub.run(60);
    enter_seat(&mut r.hub.server.world, r.car, r.a, 0).expect("driver in");
    enter_seat(&mut r.hub.server.world, r.car, r.b, 1).expect("passenger in");
    r.hub.run(120);
    let held = *r.hub.server.world.get::<VehicleInput>(r.car).expect("input");
    assert!(held.throttle > 0.5, "driving: {held:?}");
    hand_off_to_server(&mut r.hub.server.world, &[r.car, r.a], None);
    r.hub.run(30);
    let world = &mut r.hub.server.world;
    let input = *world.get::<VehicleInput>(r.car).expect("input");
    assert_eq!(input, VehicleInput::default(), "the controls are released");
    assert_eq!(
        world.get::<NetVehicle>(r.car).expect("net").owner_peer,
        None,
        "clients read no owner"
    );
    let seats = world.get::<VehicleSeats>(r.car).expect("seats").clone();
    assert_eq!(seats.seats[0].occupant, None, "the driver's seat is free");
    assert_eq!(world.get::<NetSeated>(r.a).expect("seated").vehicle, None);
    // The passenger moves over to the wheel.
    enter_seat(world, r.car, r.b, 0).expect("someone else takes the wheel");
    assert_eq!(owners(world), vec![(r.car, PeerId(2))]);
}

/// The same with a game rule that despawns the departed player's pawn: the seat it held is freed.
#[test]
fn a_driver_whose_pawn_is_despawned_frees_the_seat() {
    let mut r = seat_rig(97);
    r.hub.run(60);
    enter_seat(&mut r.hub.server.world, r.car, r.a, 0).expect("driver in");
    enter_seat(&mut r.hub.server.world, r.car, r.b, 1).expect("passenger in");
    r.hub.run(120);
    hand_off_to_server(&mut r.hub.server.world, &[r.car], Some(r.a));
    r.hub.run(30);
    let world = &mut r.hub.server.world;
    assert_eq!(
        *world.get::<VehicleInput>(r.car).expect("input"),
        VehicleInput::default()
    );
    let seats = world.get::<VehicleSeats>(r.car).expect("seats").clone();
    assert_eq!(
        seats.seats[0].occupant, None,
        "no seat held by a pawn that is gone"
    );
    enter_seat(world, r.car, r.b, 0).expect("someone else takes the wheel");
    assert_eq!(owners(world), vec![(r.car, PeerId(2))]);
}

/// w7-runtime-smalls-z item 7 (ADR-7399 Consequences): a passenger whose pawn is despawned (its peer
/// left and the game's rule removed the pawn) frees its seat; the driver keeps the car, and another
/// client's character can take the freed seat. Before, the seat stayed held by the gone pawn.
#[test]
fn a_passenger_whose_pawn_is_despawned_frees_the_seat() {
    let mut r = seat_rig(103);
    r.hub.run(60);
    enter_seat(&mut r.hub.server.world, r.car, r.a, 0).expect("driver in");
    enter_seat(&mut r.hub.server.world, r.car, r.b, 1).expect("passenger in");
    r.hub.run(120);
    hand_off_to_server(&mut r.hub.server.world, &[], Some(r.b));
    r.hub.run(30);
    let world = &mut r.hub.server.world;
    let seats = world.get::<VehicleSeats>(r.car).expect("seats").clone();
    assert_eq!(
        seats.seats[1].occupant, None,
        "no seat held by a pawn that is gone"
    );
    let a_id = *world.get::<NetId>(r.a).expect("id");
    assert_eq!(seats.seats[0].occupant, Some(a_id), "the driver stays");
    assert_eq!(owners(world), vec![(r.car, PeerId(1))], "and keeps the car");
    // Peer 1's character takes the freed seat (it moves over from the wheel).
    enter_seat(world, r.car, r.a, 1).expect("the freed seat can be taken");
    assert_eq!(
        world.get::<VehicleSeats>(r.car).expect("seats").seats[1].occupant,
        Some(a_id)
    );
}

/// A passenger handed to a live peer (a game's `OwnedHandOff` rule) is still there, so it keeps its
/// seat: only seats whose occupant no longer exists are freed.
#[test]
fn a_passenger_handed_to_a_live_peer_keeps_the_seat() {
    let mut r = seat_rig(107);
    r.hub.run(60);
    enter_seat(&mut r.hub.server.world, r.car, r.a, 0).expect("driver in");
    enter_seat(&mut r.hub.server.world, r.car, r.b, 1).expect("passenger in");
    r.hub.run(120);
    r.hub.server.world.entity_mut(r.b).insert(NetOwner(PeerId(1)));
    r.hub.run(30);
    let world = &mut r.hub.server.world;
    let b_id = *world.get::<NetId>(r.b).expect("id");
    let seats = world.get::<VehicleSeats>(r.car).expect("seats").clone();
    assert_eq!(
        seats.seats[1].occupant,
        Some(b_id),
        "the passenger keeps its seat"
    );
    assert_eq!(world.get::<NetSeated>(r.b).expect("seated").seat, 1);
    assert_eq!(
        enter_seat(world, r.car, r.a, 1).expect_err("taken").code(),
        "vehicle_seat_taken"
    );
    assert_eq!(owners(world), vec![(r.car, PeerId(1))]);
}

/// Counts `net.vehicle_owner_left` events (target `net`) fired while `run` executes. The hub is
/// single-threaded, so a thread-local subscriber sees every server tick.
fn owner_left_events(run: impl FnOnce()) -> usize {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::SubscriberExt;

    struct Message(String);
    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }
    struct OwnerLeft(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for OwnerLeft {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if event.metadata().target() != "net" {
                return;
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            if message.0.trim_matches('"') == "net.vehicle_owner_left" {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(OwnerLeft(Arc::clone(&seen)));
    tracing::subscriber::with_default(subscriber, run);
    seen.load(Ordering::Relaxed)
}

/// w9-net-vehicle-steam-z 1: an ordinary seat exit (the driver gets out and stays in the game) is
/// not an owner departure. `release_departed_drivers` must not run `release_vehicle` for it — no
/// `net.vehicle_owner_left` log — while a peer that actually leaves releases once, as before.
#[test]
fn an_ordinary_seat_exit_is_not_an_owner_departure() {
    let mut r = seat_rig(103);
    r.hub.run(60);
    enter_seat(&mut r.hub.server.world, r.car, r.a, 0).expect("driver in");
    r.hub.run(120);

    // The driver steps out while connected: no departure.
    let events = owner_left_events(|| {
        exit_seat(&mut r.hub.server.world, r.a).expect("driver out");
        r.hub.run(10);
    });
    assert_eq!(events, 0, "an ordinary exit is not an owner departure");
    assert!(owners(&mut r.hub.server.world).is_empty());
    assert_eq!(
        *r.hub.server.world.get::<VehicleInput>(r.car).expect("input"),
        VehicleInput::default(),
        "the seat change itself releases the controls"
    );

    // Back in, then the peer leaves with the throttle held: exactly one release.
    let at = r
        .hub
        .server
        .world
        .get::<Transform>(r.car)
        .expect("car")
        .translation;
    r.hub
        .server
        .world
        .get_mut::<Transform>(r.a)
        .expect("driver")
        .translation = at;
    enter_seat(&mut r.hub.server.world, r.car, r.a, 0).expect("driver in again");
    r.hub.run(30);
    let held = *r.hub.server.world.get::<VehicleInput>(r.car).expect("input");
    assert!(held.throttle > 0.5, "driving: {held:?}");
    let events = owner_left_events(|| {
        hand_off_to_server(&mut r.hub.server.world, &[r.car, r.a], None);
        r.hub.run(10);
    });
    assert_eq!(events, 1, "a real departure releases once");
    assert_eq!(
        *r.hub.server.world.get::<VehicleInput>(r.car).expect("input"),
        VehicleInput::default()
    );
    let seats = r
        .hub
        .server
        .world
        .get::<VehicleSeats>(r.car)
        .expect("seats")
        .clone();
    assert_eq!(seats.seats[0].occupant, None, "the seat is freed");
}

/// A seatless car owned straight by a peer (`spawn_net_vehicle` with an owner): the hand-off releases
/// its controls too, while a car the server drives keeps whatever input the server writes.
#[test]
fn a_seatless_owner_who_leaves_releases_the_car() {
    let mut r = rig(101, SIMPLE, 1, 0.0, 1, false);
    let ai_car = spawn_net_vehicle(
        &mut r.hub.server.world,
        SIMPLE,
        (Vec3::new(20.0, 1.0, 0.0), Quat::IDENTITY),
        None,
    )
    .expect("server car");
    let ai_input = VehicleInput {
        throttle: 0.5,
        ..VehicleInput::default()
    };
    r.hub.server.world.entity_mut(ai_car).insert(ai_input);
    r.hub.run(200);
    let held = *r.hub.server.world.get::<VehicleInput>(r.car).expect("input");
    assert_ne!(held, VehicleInput::default(), "driving: {held:?}");
    hand_off_to_server(&mut r.hub.server.world, &[r.car], None);
    r.hub.run(10);
    let world = &r.hub.server.world;
    assert_eq!(
        *world.get::<VehicleInput>(r.car).expect("input"),
        VehicleInput::default()
    );
    assert_eq!(world.get::<NetVehicle>(r.car).expect("net").owner_peer, None);
    assert_eq!(
        *world.get::<VehicleInput>(ai_car).expect("input"),
        ai_input,
        "a server-driven car keeps the server's input"
    );
}

// ---- the rest ------------------------------------------------------------------------------------

/// An unknown tuning spawns nothing on the server and is reported on a client, never a panic.
#[test]
fn an_unknown_tuning_is_reported() {
    let mut r = rig(83, SIMPLE, 1, 0.0, 1, false);
    assert_eq!(
        spawn_net_vehicle(
            &mut r.hub.server.world,
            "nobody",
            (Vec3::ZERO, Quat::IDENTITY),
            None,
        )
        .expect_err("unknown")
        .code(),
        "vehicle_tuning_unknown"
    );
    r.hub.clients[0]
        .world
        .resource_mut::<NetVehicleTunings>()
        .by_name
        .clear();
    r.hub.run(60);
    let rows = net_vehicle_rows(&mut r.hub.clients[0].world);
    assert_eq!(rows[0].fault, Some("vehicle_tuning_unknown"));
}

/// Smoothing settings are the host's: zero half life shows a correction at once, and does not snap.
#[test]
fn smoothing_settings_are_honored() {
    let mut r = shoved_rig(Vec3::new(3.0, 0.0, 0.0), Vec3::ZERO);
    r.hub.clients[0].world.insert_resource(NetVehicleSmoothing {
        half_life_ms: 0.0,
        ..NetVehicleSmoothing::default()
    });
    r.hub.run(900);
    let w = &mut r.hub.clients[0].world;
    let mut q = w.query::<&NetVehicleVisual>();
    let visual = *q.iter(w).next().expect("visual");
    assert!(visual.corrections > 0);
    assert_eq!(visual.snaps, 0);
}

/// `net.vehicles`' row on the server has real byte counts once the pipeline runs.
#[test]
fn the_server_row_says_what_it_can() {
    let mut r = rig(89, SIMPLE, 1, 0.0, 1, true);
    r.hub.run(120);
    let rows = net_vehicle_rows(&mut r.hub.server.world);
    assert_eq!(rows[0].mode, "server");
    assert_eq!(rows[0].owner, Some(1));
    assert!(rows[0].net_id.is_some());
    assert_eq!(rows[0].tuning, SIMPLE);
    assert_eq!(rows[0].bytes_source, Some("codec"));
    assert!(rows[0].bytes_per_second.is_some_and(|b| b > 0.0));
}

/// With the relevance pipeline on, the row reads its counters.
#[test]
fn the_server_row_reads_the_pipeline_when_it_runs() {
    let mut r = rig(97, SIMPLE, 1, 0.0, 1, true);
    regia_net::relevance::install_net_interest(&mut r.hub.server.world);
    r.hub.run(240);
    let rows = net_vehicle_rows(&mut r.hub.server.world);
    assert_eq!(rows[0].bytes_source, Some("pipeline"));
    assert!(rows[0].bytes_per_second.is_some_and(|b| b > 0.0), "{:?}", rows[0]);
}

// ---- a shift ------------------------------------------------------------------------------------

/// A driver's shift is predicted: the client's gear changes at its own tick, the server's at the same
/// tick, and the two agree when the state comes back.
#[test]
fn a_shift_is_predicted() {
    fn shifting(t: u32) -> VehicleNetInput {
        VehicleNetInput {
            throttle: 1.0,
            shift: if t == 400 {
                VehicleShift::Up
            } else {
                VehicleShift::None
            },
            ..VehicleNetInput::default()
        }
    }
    let mut r = rig(101, GEARED, DELAY_TICKS, 0.0, 1, true);
    r.hub.clients[0].world.insert_resource(Script(shifting));
    r.hub.run(700);
    let gear_of = |world: &mut World| {
        let mut q = world.query_filtered::<&NetVehicleState, With<NetVehicle>>();
        let state = q.iter(world).next().expect("car");
        let core = state.drivetrain.as_ref().expect("geared");
        (core.gear, core.automatic)
    };
    let server = gear_of(&mut r.hub.server.world);
    let client = gear_of(&mut r.hub.clients[0].world);
    assert!(!server.1, "the driver took the gears");
    assert_eq!(server, client);
    let (error, jump, _, _) = owner_against_server(&r.hub.server.world, &r.hub.clients[0].world, 120);
    assert!(error <= 0.25, "{error}");
    assert!(jump <= 0.1, "{jump}");
}

// ---- real apps ----------------------------------------------------------------------------------

fn net_app(mode: NetMode, peer: Option<PeerId>) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, HZ);
    app.insert_resource(regia_net::LocalPeer(peer))
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(regia_physics::vehicle::VehiclePlugin)
        .add_plugins(regia_net::NetPlugin { mode })
        .add_plugins(NetVehiclePlugin);
    let world = app.world_mut();
    world
        .resource_mut::<NetVehicleTunings>()
        .insert(SIMPLE, VehicleConfig::default());
    world.init_resource::<Log>();
    add_predicted_systems(
        world,
        log_tick::<VehicleNetInput>.after(net_vehicle_step::<VehicleNetInput>),
    );
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, -1.0, 0.0)),
        regia_physics::RigidBody::static_body(),
        regia_physics::Collider {
            shape: regia_physics::ColliderShape::Box {
                half_extents: Vec3::new(4000.0, 1.0, 4000.0),
            },
            ..regia_physics::Collider::default()
        },
    ));
    app
}

fn feed_app(cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<VehicleNetInput>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = course(next);
    }
}

/// The plugins in a real server app and a real client app, 6 ticks apart: the system order the hub
/// rig assembles by hand is the one the plugin installs, and a client's copy gets its kinematic body.
#[test]
fn two_apps_drive_a_car_on_rapier() {
    const DELAY: u64 = 6;
    let mut server = net_app(NetMode::DedicatedServer, Some(PeerId::SERVER));
    let mut client = net_app(NetMode::Client, Some(PeerId(1)));
    regia_net::runtime::server_add_peer(server.world_mut(), PeerId(1));
    set_transport_rtt(client.world_mut(), 13.0 * 1000.0 / HZ, 0.0);
    client.add_systems(FixedUpdate, feed_app.before(regia_net::NetSet::Predict));
    // The ground reaches the rapier world on the first step: spawn the car after it.
    server.update();
    client.update();
    let car = spawn_net_vehicle(
        server.world_mut(),
        SIMPLE,
        (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
        Some(PeerId(1)),
    )
    .expect("car");
    server.world_mut().resource_mut::<Log>().car = Some(car);
    let mut line: Vec<(u64, bool, Datagram)> = Vec::new();
    for now in 0..900u64 {
        let (due, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut line)
            .into_iter()
            .partition(|(at, _, _)| *at <= now);
        line = wait;
        for (_, to_server, d) in due {
            let app = if to_server { &mut server } else { &mut client };
            app.world_mut().resource_mut::<NetNode>().inbox.push(d);
        }
        server.update();
        let out = std::mem::take(&mut server.world_mut().resource_mut::<NetNode>().outbox);
        line.extend(out.into_iter().map(|d| (now + DELAY, false, d)));
        client.update();
        let out = std::mem::take(&mut client.world_mut().resource_mut::<NetNode>().outbox);
        line.extend(out.into_iter().map(|mut d| {
            d.peer = PeerId(1);
            (now + DELAY, true, d)
        }));
    }
    let (error, jump, speed, common) = owner_against_server(server.world(), client.world(), 120);
    eprintln!(
        "two apps: max error {error:.4} m, max jump {jump:.4} m, top speed {speed:.2} m/s, {common} ticks"
    );
    assert!(common > 500, "{common}");
    assert!(speed >= 20.0, "{speed}");
    assert!(error <= 0.25, "{error}");
    assert!(jump <= 0.1, "{jump}");
    // The copy's kinematic chassis follows it in the client's shared world.
    let world = client.world_mut();
    let mut q = world.query_filtered::<(&Transform, &regia_physics::RigidBodyHandle), With<NetVehicle>>();
    let (pose, handle) = q
        .iter(world)
        .next()
        .map(|(t, h)| (*t, *h))
        .expect("the copy has a body");
    let physics = world.resource::<PhysicsWorld>();
    let body = physics.bodies.get(handle.to_handle()).expect("body");
    assert!(body.is_kinematic());
    let (at, _) = regia_physics::convert::from_rapier_pose(body.position());
    assert!(at.distance(pose.translation) < 1.0, "{at} {}", pose.translation);
}
