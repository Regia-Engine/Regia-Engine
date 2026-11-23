// SPDX-License-Identifier: MPL-2.0
//! Schedule ticks with a measured allocation count (spec 10 § 4, ADR-2704). The hand-run tests are
//! `#[ignore]`: the gate never runs those, and they assert only that the work happened, never a time.
//! `diagnostics_record_frame_steady_window_does_not_allocate` is the exception: it is a gate test,
//! and a steady frame-time window must allocate nothing. The ignored tests print the
//! `docs/06-testing-and-ci.md` § 9 lines, and `cargo xtask perf-budgets` reads them through
//! `docs/perf/budgets.ron`:
//!
//! ```text
//! cargo test -p regia_app --release --test perf_micro_schedule -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `template_shape` is the *shape* of the third_person template's world: its five entities, its four
//! `Update` systems doing the template's queries, and one `FixedUpdate` step of a character. It is not
//! the template: `regia_app` sits below physics, input and the camera rig, so the plugins' own systems
//! are not in it and this says nothing about the `docs/06` § 4 "CPU main thread" budget. It catches a
//! regression in what the app and the ECS add to a tick of that shape, and `benches/schedule.rs` stays
//! the criterion number for the 200 empty systems budget.
//!
//! Its own test binary: it installs a counting global allocator. The shared counter (ADR-3291) costs a
//! few thread-local and relaxed atomic adds per allocation in the timed tests too, which is small
//! next to the allocation itself.

use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests and bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

#[derive(Component, Clone, Copy, Default)]
struct Position([f32; 3]);

#[derive(Component, Clone, Copy, Default)]
struct MoveInput {
    direction: [f32; 3],
    jump: bool,
}

#[derive(Component, Clone, Copy, Default)]
struct Rig {
    yaw: f32,
}

#[derive(Component, Clone, Copy)]
struct Chase(Entity);

#[derive(Component)]
struct Player;

#[derive(Resource, Default)]
struct Checkpoint {
    position: [f32; 3],
    yaw: f32,
}

fn write_input(rigs: Query<&Rig>, mut players: Query<&mut MoveInput, With<Player>>) {
    let yaw = rigs.iter().next().map_or(0.0, |rig| rig.yaw);
    for mut input in &mut players {
        input.direction = [yaw.sin(), 0.0, yaw.cos()];
        input.jump = false;
    }
}

fn write_look(mut rigs: Query<&mut Rig>) {
    for mut rig in &mut rigs {
        rig.yaw = (rig.yaw + 0.01) % std::f32::consts::TAU;
    }
}

fn track(players: Query<&Position, With<Player>>, rigs: Query<&Rig>, mut checkpoint: ResMut<Checkpoint>) {
    if let Some(position) = players.iter().next() {
        checkpoint.position = position.0;
    }
    if let Some(rig) = rigs.iter().next() {
        checkpoint.yaw = rig.yaw;
    }
}

fn restore(checkpoint: Res<Checkpoint>, mut players: Query<&mut Position, With<Player>>) {
    if checkpoint.is_changed() && checkpoint.yaw < 0.0 {
        for mut position in &mut players {
            position.0 = checkpoint.position;
        }
    }
}

fn step_character(mut players: Query<(&mut Position, &MoveInput)>) {
    for (mut position, input) in &mut players {
        for (axis, direction) in position.0.iter_mut().zip(input.direction) {
            *axis += direction / 60.0;
        }
    }
}

fn follow(
    targets: Query<&Position, With<Player>>,
    mut chasers: Query<(&Chase, &mut Position), Without<Player>>,
) {
    for (chase, mut position) in &mut chasers {
        if let Ok(target) = targets.get(chase.0) {
            position.0 = target.0;
        }
    }
}

fn manual_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    app
}

/// The template's world and systems, in the shape the file header describes.
fn template_shape() -> App {
    let mut app = manual_app();
    app.init_resource::<Checkpoint>()
        .add_systems(Update, (write_input, write_look, track, restore, follow))
        .add_systems(FixedUpdate, step_character);
    let world = app.world_mut();
    for x in [0.0, 3.0, -3.0] {
        world.spawn(Position([x, 0.0, 0.0]));
    }
    let player = world
        .spawn((Player, Position([0.0, 1.2, 5.0]), MoveInput::default()))
        .id();
    world.spawn((Position::default(), Rig::default(), Chase(player)));
    app.finish_plugins();
    // Build the schedules, and let the fixed step settle, before measuring.
    for _ in 0..8 {
        app.update();
    }
    app
}

fn empty_system() {}

fn with_empty_systems(count: usize) -> App {
    let mut app = manual_app();
    for _ in 0..count {
        app.add_systems(Update, empty_system);
    }
    app.finish_plugins();
    for _ in 0..8 {
        app.update();
    }
    app
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

/// Allocations and requested bytes of 240 ticks on this thread, after 240 warm ones, on a fresh app
/// with every schedule on the single-threaded executor. Runs twice and asserts the same answer: a count
/// is only worth pinning when it repeats. Two windows of one app need not agree, because the fixed
/// clock's accumulator does not step every tick alike.
fn report_tick_count(name: &str, make: fn() -> App) {
    const TICKS: u64 = 240;
    let measure = || {
        let mut app = make();
        single_threaded(&mut app);
        for _ in 0..TICKS {
            app.update();
        }
        let start = alloc_counter::thread_stats();
        for _ in 0..TICKS {
            app.update();
        }
        {
            let used = alloc_counter::thread_stats().since(start);
            (used.allocs, used.bytes)
        }
    };
    let (allocs, bytes) = measure();
    assert_eq!((allocs, bytes), measure(), "{name}: the count does not repeat");
    println!("perf-count {name}: ops={TICKS} allocs={allocs} bytes={bytes}");
    assert_eq!((allocs, bytes), (0, 0), "{name}: a steady tick still allocates");
}

/// A full frame-time window, then another window of the same length, allocates nothing.
/// The schedule-tick count above includes the rest of a frame; this one is only
/// `Diagnostics::record_frame`.
#[test]
fn diagnostics_record_frame_steady_window_does_not_allocate() {
    const WINDOW: usize = 300;
    let mut diagnostics = Diagnostics::default();
    for _ in 0..WINDOW {
        diagnostics.record_frame(Duration::from_millis(16), WINDOW);
    }
    let start = alloc_counter::thread_stats();
    for _ in 0..WINDOW {
        diagnostics.record_frame(Duration::from_millis(16), WINDOW);
    }
    let used = alloc_counter::thread_stats().since(start);
    assert_eq!((used.allocs, used.bytes), (0, 0), "a full window still allocates");
}

fn tick_samples(app: &mut App, frames: u32) -> Vec<Duration> {
    let start_frame = app.world().resource::<Time>().frame_count();
    let samples = (0..frames)
        .map(|_| {
            let start = Instant::now();
            app.update();
            start.elapsed()
        })
        .collect();
    assert_eq!(
        app.world().resource::<Time>().frame_count(),
        start_frame + u64::from(frames),
        "every tick ran"
    );
    samples
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_schedule_tick() {
    report(
        "schedule_tick_template_shape",
        tick_samples(&mut template_shape(), 2_000),
    );
    report(
        "schedule_tick_200_empty_systems",
        tick_samples(&mut with_empty_systems(200), 2_000),
    );
}

/// Every schedule on the single-threaded executor. The multi-threaded one moves systems between the
/// main thread and the workers as the host's load dictates, so what the main thread allocates does
/// not repeat from one run to the next.
fn single_threaded(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

/// Old path. `black_box` on the world keeps the scan from being lifted out of the caller.
#[inline(never)]
fn time_count_spawned(world: &World) -> u32 {
    std::hint::black_box(world).entities().count_spawned()
}

/// Same walk as the diagnostics entity count: one `Archetype::len` per archetype.
/// `black_box` keeps the caller from treating the function as a pure load of an unchanged world.
#[inline(never)]
fn time_archetype_sum(world: &World) -> u32 {
    let world = std::hint::black_box(world);
    let total = world
        .archetypes()
        .iter()
        .map(|archetype| u64::from(archetype.len()))
        .sum::<u64>();
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// High-water entity index versus the archetype sum. Prints both; asserts neither a time nor
/// which one is faster. 100 000 spawns, then all but one despawned.
#[test]
#[ignore = "hand-run timing of the entity-count scan (no time assert)"]
fn perf_entity_count_high_water_scan() {
    let mut app = manual_app();
    let world = app.world_mut();
    #[derive(Component)]
    struct Marker;
    let spawned: Vec<_> = (0..100_000).map(|_| world.spawn(Marker).id()).collect();
    for entity in spawned.into_iter().skip(1) {
        world.despawn(entity);
    }
    let high_water = world.entities().len();
    let archetypes = world.archetypes().len();
    let live = world.entities().count_spawned();
    const N: u32 = 2_000;
    let mut sink = 0u32;
    let start = Instant::now();
    for _ in 0..N {
        sink = sink.wrapping_add(std::hint::black_box(time_count_spawned(world)));
    }
    let scan = start.elapsed();
    let start = Instant::now();
    for _ in 0..N {
        sink = sink.wrapping_add(std::hint::black_box(time_archetype_sum(world)));
    }
    let summed = start.elapsed();
    println!(
        "perf-entity-count high_water={high_water} archetypes={archetypes} live={live} \
         count_spawned={scan:?}/{N} archetype_sum={summed:?}/{N} sink={sink}"
    );
    assert_eq!(live, time_archetype_sum(world));
    assert!(u64::from(high_water) > u64::try_from(archetypes).unwrap_or(u64::MAX) + 1_000);
}

/// Allocation counts: the same on every machine, so `perf-budgets --counts-only` can gate on them.
#[test]
#[ignore = "hand-run allocation counts (docs/06 § 9)"]
fn perf_count_schedule_tick() {
    report_tick_count("schedule_tick_template_shape_allocs", template_shape);
    report_tick_count("schedule_tick_200_empty_systems_allocs", || {
        with_empty_systems(200)
    });
}
