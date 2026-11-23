// SPDX-License-Identifier: MPL-2.0
//! ADR-3080: the measurement that chose how a client predicts a vehicle.
//!
//! One scripted course at 100 ms each way (6 ticks of delay at 60 Hz, a lead of 8 ticks, so a server
//! state for tick `T - 14` is what a client knows at tick `T`), with one server-only shove that no
//! client can predict. Three designs, against the server's pose of the same tick:
//!
//! - (a) a private physics world per vehicle: prediction by the real step, re-run after a correction;
//! - (b) a reduced model: corrected to the server's state and re-integrated by a bicycle model;
//! - (c) no prediction: the server's state, shown when it arrives, with smoothing.
//!
//! Prints error, per-step visible jump, the cost of a step and of a correction, and the cost of the
//! private world against the static collider count.
//!
//! `cargo run --release -p regia_framework --features net --example net_vehicle_bench -- 2000`

use std::collections::BTreeMap;
use std::time::Instant;

use regia_core::math::{Quat, Vec3};
use regia_framework::net_vehicle::NetVehicleState;
use regia_net::bits::BitReader;
use regia_net::codec::{CodecContext, FieldCodec, ReplicateCodec, Rotation, encode_to_vec};
use regia_physics::PhysicsWorld;
use regia_physics::vehicle::{VehicleConfig, VehicleInput};
use regia_physics::vehicle_snapshot::{VehicleCore, VehicleSimSettings, VehicleSimWorld, add_static_box};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;
/// A client knows the server's tick `T - KNOWN_BEHIND` at its tick `T`: 6 ticks of delay and a lead
/// of `6 + 2`.
const KNOWN_BEHIND: usize = 14;
/// Option (c) shows the newest state 6 ticks late plus an interpolation delay of 6 ticks (100 ms).
const SHOWN_BEHIND: usize = 12;
const COURSE_TICKS: usize = 1500;
const SHOVE_TICK: usize = 700;
/// Half life of the offset all three designs draw through, ticks.
const SMOOTH_KEEP: f32 = 0.87;

/// The acceptance course of `tests/net_vehicle.rs`: to about 21 m/s, then a slow weave.
fn input(t: usize) -> VehicleInput {
    VehicleInput {
        throttle: if t < 270 { 1.0 } else { 0.02 },
        brake: 0.0,
        steer: if t > 340 {
            ((t as f32) * 0.02).sin() * 0.12
        } else {
            0.0
        },
    }
}

fn statics(count: usize) -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    add_static_box(
        &mut physics,
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(5000.0, 1.0, 5000.0),
    );
    for i in 0..count {
        // Out of the car's way, so its cost is the world's and not a crash's.
        let x = 2000.0 + (i % 100) as f32 * 8.0;
        let z = 2000.0 + (i / 100) as f32 * 8.0;
        add_static_box(&mut physics, Vec3::new(x, 3.0, z), Vec3::splat(3.0));
    }
    physics
}

fn new_sim(shared: &PhysicsWorld) -> VehicleSimWorld {
    VehicleSimWorld::new(
        VehicleConfig::default(),
        shared,
        VehicleSimSettings::default(),
        (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
        DT,
    )
    .expect("sim")
}

fn core_of(sim: &VehicleSimWorld) -> VehicleCore {
    sim.vehicle().core(sim.physics()).expect("core")
}

/// A state through the wire: 1 mm position grid, 10-bit smallest-three rotation, the vehicle codec.
fn through_wire(core: &VehicleCore) -> VehicleCore {
    let cx = CodecContext::detached();
    let state = NetVehicleState::from_core(core, 0.0);
    let bytes = encode_to_vec(&state, &cx);
    let state = NetVehicleState::decode(&mut BitReader::new(&bytes), &cx).expect("state");
    let mut w = regia_net::bits::BitWriter::new();
    FieldCodec::<Quat>::encode(&Rotation::default(), &core.rotation, &mut w, &cx);
    let bytes = w.finish();
    let rotation =
        FieldCodec::<Quat>::decode(&Rotation::default(), &mut BitReader::new(&bytes), &cx).expect("rotation");
    let pose = Transform {
        translation: (core.translation * 1000.0).round() / 1000.0,
        rotation,
        scale: Vec3::ONE,
    };
    state.to_core(&pose)
}

/// The server's run: every tick's core, as simulated and with the unpredictable shove applied.
fn server_run() -> Vec<VehicleCore> {
    let shared = statics(0);
    let mut sim = new_sim(&shared);
    let mut out = Vec::with_capacity(COURSE_TICKS);
    for t in 0..COURSE_TICKS {
        if t == SHOVE_TICK {
            let mut core = core_of(&sim);
            core.linear_velocity += core.rotation * Vec3::new(0.0, 0.0, -6.0);
            sim.apply_core(&core).expect("shove");
        }
        sim.step(input(t), DT, None).expect("step");
        out.push(core_of(&sim));
    }
    out
}

#[derive(Default, Debug)]
struct Score {
    max_error_m: f32,
    rms_sum: f32,
    samples: u32,
    max_jump_m: f32,
    corrections: u32,
    max_error_cruising_m: f32,
}

impl Score {
    fn note(&mut self, tick: usize, error: f32, jump: f32) {
        self.max_error_m = self.max_error_m.max(error);
        self.rms_sum += error * error;
        self.samples += 1;
        self.max_jump_m = self.max_jump_m.max(jump);
        // Away from the shove, which no design can predict.
        if !(SHOVE_TICK..SHOVE_TICK + 120).contains(&tick) {
            self.max_error_cruising_m = self.max_error_cruising_m.max(error);
        }
    }

    fn print(&self, name: &str) {
        println!(
            "{name}: error vs server, cruising max {:.3} m; overall max {:.3} m, rms {:.3} m; \
             visible jump max {:.3} m/step; corrections {}",
            self.max_error_cruising_m,
            self.max_error_m,
            (self.rms_sum / self.samples.max(1) as f32).sqrt(),
            self.max_jump_m,
            self.corrections,
        );
    }
}

/// What every design draws: a simulated position, and an offset that pays back corrections.
#[derive(Default)]
struct Draw {
    offset: Vec3,
    last: Option<Vec3>,
    last_sim: Option<Vec3>,
}

impl Draw {
    /// `corrected_from` is the position before this tick's correction; returns the visible jump.
    fn show(&mut self, sim: Vec3, corrected_from: Option<(Vec3, Vec3)>, motion: Vec3) -> f32 {
        if let Some((old, new)) = corrected_from {
            self.offset += old - new;
        }
        self.offset *= SMOOTH_KEEP;
        let drawn = sim + self.offset;
        let jump = self.last.map_or(0.0, |last| (drawn - last - motion).length());
        self.last = Some(drawn);
        self.last_sim = Some(sim);
        jump
    }
}

fn option_a(server: &[VehicleCore]) -> (Score, f64, f64) {
    let shared = statics(0);
    let mut sim = new_sim(&shared);
    let mut score = Score::default();
    let mut history: BTreeMap<usize, VehicleCore> = BTreeMap::new();
    let mut draw = Draw::default();
    let (mut step_ns, mut steps, mut fix_ns, mut fixes) = (0u128, 0u64, 0u128, 0u64);
    for t in 0..COURSE_TICKS {
        let before = core_of(&sim).translation;
        let started = Instant::now();
        sim.step(input(t), DT, None).expect("step");
        step_ns += started.elapsed().as_nanos();
        steps += 1;
        history.insert(t, core_of(&sim));
        let mut corrected = None;
        if t >= KNOWN_BEHIND {
            let s = t - KNOWN_BEHIND;
            let truth = &server[s];
            let predicted = &history[&s];
            if predicted.translation.distance(truth.translation) > 0.02
                || predicted.linear_velocity.distance(truth.linear_velocity) > 0.1
            {
                let old = core_of(&sim).translation;
                let started = Instant::now();
                sim.apply_core(&through_wire(truth)).expect("correct");
                for r in s + 1..=t {
                    sim.step(input(r), DT, None).expect("re-run");
                    history.insert(r, core_of(&sim));
                }
                fix_ns += started.elapsed().as_nanos();
                fixes += 1;
                score.corrections += 1;
                corrected = Some((old, core_of(&sim).translation));
            }
            history.retain(|k, _| *k > s);
        }
        let now = core_of(&sim).translation;
        let motion = if corrected.is_some() {
            server[t].linear_velocity * DT
        } else {
            now - before
        };
        let jump = draw.show(now, corrected, motion);
        score.note(t, now.distance(server[t].translation), jump);
    }
    (
        score,
        step_ns as f64 / steps as f64 / 1000.0,
        fix_ns as f64 / fixes.max(1) as f64 / 1000.0,
    )
}

/// The reduced model: position, heading, speed. Wheelbase, engine and resistance from the tuning; no
/// suspension, no tire slip, no roll.
#[derive(Clone, Copy)]
struct Reduced {
    at: Vec3,
    yaw: f32,
    speed: f32,
}

impl Reduced {
    fn from_core(core: &VehicleCore) -> Self {
        let forward = core.rotation * Vec3::NEG_Z;
        Self {
            at: core.translation,
            yaw: forward.x.atan2(-forward.z),
            speed: core.linear_velocity.dot(forward),
        }
    }

    fn step(&mut self, input: VehicleInput, config: &VehicleConfig) {
        let drive_wheels = config.wheels.iter().filter(|w| w.drive).count() as f32;
        let force = input.throttle * config.max_engine_force * drive_wheels
            - config.rolling_resistance * (config.wheels.len() as f32 - drive_wheels);
        self.speed += force / config.chassis_mass * DT;
        let wheelbase = 2.6;
        self.yaw += self.speed / wheelbase * (input.steer * config.max_steer_angle).tan() * DT;
        self.at += Vec3::new(self.yaw.sin(), 0.0, -self.yaw.cos()) * self.speed * DT;
    }
}

fn option_b(server: &[VehicleCore]) -> (Score, f64, f64) {
    let config = VehicleConfig::default();
    let mut score = Score::default();
    let mut draw = Draw::default();
    let mut model = Reduced::from_core(&server[0]);
    let (mut step_ns, mut steps, mut fix_ns, mut fixes) = (0u128, 0u64, 0u128, 0u64);
    for t in 0..COURSE_TICKS {
        let started = Instant::now();
        model.step(input(t), &config);
        step_ns += started.elapsed().as_nanos();
        steps += 1;
        let mut corrected = None;
        // Every server state corrects: the reduced model never agrees with the real step.
        if t >= KNOWN_BEHIND && (t - KNOWN_BEHIND).is_multiple_of(2) {
            let s = t - KNOWN_BEHIND;
            let old = model.at;
            let started = Instant::now();
            let mut fresh = Reduced::from_core(&through_wire(&server[s]));
            for r in s + 1..=t {
                fresh.step(input(r), &config);
            }
            fix_ns += started.elapsed().as_nanos();
            fixes += 1;
            if old.distance(fresh.at) > 0.02 {
                score.corrections += 1;
                corrected = Some((old, fresh.at));
            }
            model = fresh;
        }
        let jump = draw.show(model.at, corrected, server[t].linear_velocity * DT);
        score.note(t, model.at.distance(server[t].translation), jump);
    }
    (
        score,
        step_ns as f64 / steps as f64 / 1000.0,
        fix_ns as f64 / fixes.max(1) as f64 / 1000.0,
    )
}

fn option_c(server: &[VehicleCore]) -> Score {
    let mut score = Score::default();
    let mut draw = Draw::default();
    for t in 0..COURSE_TICKS {
        let shown = server[t.saturating_sub(SHOWN_BEHIND)].translation;
        let jump = draw.show(
            shown,
            None,
            server[t.saturating_sub(SHOWN_BEHIND)].linear_velocity * DT,
        );
        score.note(t, shown.distance(server[t].translation), jump);
    }
    score
}

/// What the private world costs by the size of the level.
fn world_costs(rounds: usize) {
    println!("private world against static colliders (build once; per step; one 14-tick correction):");
    for count in [0usize, 100, 1000, 10_000] {
        let shared = statics(count);
        let started = Instant::now();
        let mut sim = new_sim(&shared);
        let build = started.elapsed();
        // Settle, then time steps at speed.
        for t in 0..300 {
            sim.step(input(t), DT, None).expect("step");
        }
        let started = Instant::now();
        for t in 0..rounds {
            sim.step(input(300 + t % 300), DT, None).expect("step");
        }
        let per_step = started.elapsed().as_secs_f64() * 1.0e6 / rounds as f64;
        let core = core_of(&sim);
        let started = Instant::now();
        let re_runs = rounds / 14 + 1;
        for _ in 0..re_runs {
            sim.apply_core(&core).expect("rollback");
            for r in 0..14 {
                sim.step(input(300 + r), DT, None).expect("re-run");
            }
        }
        let per_fix = started.elapsed().as_secs_f64() * 1.0e6 / re_runs as f64;
        println!(
            "  {count:>6} statics: build {:>9.1} us, step {per_step:>7.2} us, correction {per_fix:>9.1} us",
            build.as_secs_f64() * 1.0e6
        );
    }
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2000);
    report(rounds);
}

/// `cargo test --example net_vehicle_bench -- --nocapture`: the same report on a host that may run
/// tests and not benches. The errors, jumps and correction counts are deterministic; the times there
/// only show orders of magnitude.
#[test]
fn report_runs() {
    report(200);
}

fn report(rounds: usize) {
    let server = server_run();
    let top = server
        .iter()
        .map(|c| c.linear_velocity.length())
        .fold(0.0, f32::max);
    println!(
        "course: {COURSE_TICKS} ticks, top speed {top:.1} m/s, a client knows the server {KNOWN_BEHIND} ticks back"
    );
    let (a, a_step, a_fix) = option_a(&server);
    a.print("(a) private world   ");
    println!("    step {a_step:.2} us, correction ({KNOWN_BEHIND} re-run ticks) {a_fix:.1} us");
    let (b, b_step, b_fix) = option_b(&server);
    b.print("(b) reduced model   ");
    println!("    step {b_step:.3} us, correction ({KNOWN_BEHIND} re-run ticks) {b_fix:.2} us");
    let c = option_c(&server);
    c.print("(c) server + smooth ");
    println!(
        "    no client step; the car answers input {} ms late",
        1000 * (KNOWN_BEHIND + SHOWN_BEHIND - 8) / 60
    );
    world_costs(rounds);
}
