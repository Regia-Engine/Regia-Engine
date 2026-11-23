// SPDX-License-Identifier: MPL-2.0
//! M2 acceptance (`docs/specs/10-core.md` § 4): a headless `App` with `MinimalPlugins` runs 10,000
//! frames of a test system deterministically.

use std::time::Duration;

use regia_app::prelude::*;

const FRAMES: u64 = 10_000;
const BODIES: u32 = 1_000;
/// FNV-1a over the final state, recorded from the first run on the reference machine. The test
/// uses only IEEE add/sub/mul on `f32`, so the value is the same on every platform.
const EXPECTED_CHECKSUM: u64 = 0xadec_a312_8976_530d;

#[derive(Component, Clone, Copy)]
struct Body {
    pos: [f32; 3],
    vel: [f32; 3],
}

#[derive(Component, Clone, Copy)]
struct Id(u32);

/// Numerical Recipes LCG: deterministic and platform-independent.
#[derive(Resource)]
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 2_000) as f32 / 1_000.0 - 1.0
    }
}

#[derive(Resource, Default)]
struct Counters {
    fixed_steps: u64,
    next_id: u32,
}

fn spawn_bodies(mut commands: Commands, mut rng: ResMut<Lcg>, mut counters: ResMut<Counters>) {
    for _ in 0..BODIES {
        let body = Body {
            pos: [rng.unit() * 50.0, 10.0 + rng.unit() * 5.0, rng.unit() * 50.0],
            vel: [rng.unit(), rng.unit() * 3.0, rng.unit()],
        };
        commands.spawn((body, Id(counters.next_id)));
        counters.next_id += 1;
    }
}

fn integrate(fixed: Res<FixedTime>, mut counters: ResMut<Counters>, mut bodies: Query<&mut Body>) {
    counters.fixed_steps += 1;
    let dt = fixed.step().as_secs_f32();
    for mut body in &mut bodies {
        body.vel[1] -= 9.81 * dt;
        for axis in 0..3 {
            body.pos[axis] += body.vel[axis] * dt;
        }
        if body.pos[1] < 0.0 {
            body.pos[1] = -body.pos[1];
            body.vel[1] = -body.vel[1] * 0.9;
        }
    }
}

fn kick_and_spawn(
    time: Res<Time>,
    mut commands: Commands,
    mut rng: ResMut<Lcg>,
    mut counters: ResMut<Counters>,
    mut bodies: Query<(&Id, &mut Body)>,
) {
    let target = rng.next() % counters.next_id;
    let kick = [rng.unit(), rng.unit() * 4.0, rng.unit()];
    for (id, mut body) in &mut bodies {
        if id.0 == target {
            for (v, k) in body.vel.iter_mut().zip(kick) {
                *v += k;
            }
        }
    }
    if time.frame_count().is_multiple_of(500) {
        commands.spawn((
            Body {
                pos: [0.0, 20.0, 0.0],
                vel: [rng.unit(), 0.0, rng.unit()],
            },
            Id(counters.next_id),
        ));
        counters.next_id += 1;
    }
}

fn checksum(app: &mut App) -> u64 {
    let world = app.world_mut();
    let mut state: Vec<(u32, Body)> = world
        .query::<(&Id, &Body)>()
        .iter(world)
        .map(|(id, body)| (id.0, *body))
        .collect();
    state.sort_by_key(|(id, _)| *id);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (id, body) in &state {
        feed(&id.to_le_bytes());
        for v in body.pos.iter().chain(&body.vel) {
            feed(&v.to_bits().to_le_bytes());
        }
    }
    let counters = world.resource::<Counters>();
    feed(&counters.fixed_steps.to_le_bytes());
    let time = world.resource::<Time>();
    feed(&time.frame_count().to_le_bytes());
    feed(&time.elapsed().as_nanos().to_le_bytes());
    hash
}

fn run_once() -> (u64, u64, usize) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )))
        .insert_resource(Lcg(0x5EED))
        .init_resource::<Counters>()
        .add_systems(Startup, spawn_bodies)
        .add_systems(FixedUpdate, integrate)
        .add_systems(Update, kick_and_spawn);
    app.finish_plugins();
    for _ in 0..FRAMES {
        app.update();
    }
    let steps = app.world().resource::<Counters>().fixed_steps;
    let bodies = app.world_mut().query::<&Body>().iter(app.world()).count();
    (checksum(&mut app), steps, bodies)
}

#[test]
fn ten_thousand_frames_are_bit_identical() {
    let (first, steps, bodies) = run_once();
    let (second, _, _) = run_once();
    println!("checksum {first:#018x}, {steps} fixed steps, {bodies} bodies");
    assert_eq!(first, second, "two runs diverged");
    assert_eq!(steps, FRAMES, "one fixed step per 1/60 s frame");
    assert_eq!(bodies, BODIES as usize + 20);
    assert_eq!(
        first, EXPECTED_CHECKSUM,
        "state changed from the recorded checksum"
    );
}

#[test]
fn minimal_plugins_runner_exits_on_request() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(1)))
        .add_systems(Last, |time: Res<Time>, mut exit: MessageWriter<AppExit>| {
            if time.frame_count() == FRAMES {
                exit.write(AppExit::Success);
            }
        });
    assert_eq!(app.run(), AppExit::Success);
}
