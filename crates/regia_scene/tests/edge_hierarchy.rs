// SPDX-License-Identifier: MPL-2.0
//! Deep and hostile hierarchies for propagation and the hierarchy helpers, and the sky-body
//! math with non-finite input. Deterministic: counters and values only.

use bevy_ecs::hierarchy::ChildOf;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::hierarchy::{despawn_tree, would_create_cycle};
use regia_scene::level::SkyBody;
use regia_scene::propagate::{PropagationCache, propagate_transforms};
use regia_scene::{GlobalTransform, Transform};

/// A 20,000-deep chain propagates without recursion trouble and composes to the sum; a second
/// run touches nothing; moving the root recomputes the chain once.
#[test]
fn a_very_deep_chain_propagates_iteratively() {
    const DEPTH: usize = 20_000;
    let mut world = World::new();
    let mut parent = world.spawn(Transform::from_translation(Vec3::X)).id();
    let root = parent;
    for _ in 0..DEPTH {
        parent = world
            .spawn((Transform::from_translation(Vec3::X), ChildOf(parent)))
            .id();
    }
    propagate_transforms(&mut world);
    let leaf = world
        .get::<GlobalTransform>(parent)
        .expect("leaf global")
        .translation();
    assert_eq!(leaf, Vec3::new((DEPTH + 1) as f32, 0.0, 0.0));
    propagate_transforms(&mut world);
    propagate_transforms(&mut world);
    let stats = world.resource::<PropagationCache>().last_run();
    assert_eq!((stats.changed, stats.recomputed), (0, 0), "{stats:?}");
    world.get_mut::<Transform>(root).expect("root").translation = Vec3::new(2.0, 0.0, 0.0);
    propagate_transforms(&mut world);
    let stats = world.resource::<PropagationCache>().last_run();
    assert_eq!(stats.recomputed, DEPTH + 1, "{stats:?}");
    assert_eq!(
        world
            .get::<GlobalTransform>(parent)
            .expect("leaf")
            .translation()
            .x,
        (DEPTH + 2) as f32
    );
    // The cycle check walks the same ancestry without recursion too.
    assert!(would_create_cycle(&world, root, parent));
    assert!(!would_create_cycle(&world, parent, root));
}

/// Despawning the top of a deep chain removes every link in it, on the test's own thread.
///
/// `World::despawn` follows the relationship recursively, one stack frame set per link: on Windows
/// a test thread's 2 MiB stack overflowed at 5,000 links (the pass of v0.2.8,
/// `STATUS_STACK_OVERFLOW`). `despawn_tree` walks the chain with an explicit stack, so this runs on
/// the default test stack at twenty times that depth (stability-smalls, ADR-2306).
#[test]
fn despawning_a_deep_chain_removes_all_of_it() {
    const DEPTH: usize = 100_000;
    let mut world = World::new();
    let root = world.spawn(Transform::IDENTITY).id();
    let mut parent = root;
    for _ in 0..DEPTH {
        parent = world.spawn((Transform::IDENTITY, ChildOf(parent))).id();
    }
    let outsider = world.spawn(Transform::IDENTITY).id();
    assert!(despawn_tree(&mut world, root));
    assert!(world.get_entity(parent).is_err());
    assert!(world.get_entity(outsider).is_ok());
    let mut left = world.query::<&Transform>();
    assert_eq!(left.iter(&world).count(), 1);
}

/// The `despawn_recursive` command takes the same iterative path: a deep chain despawned through
/// `Commands` on the default test stack.
#[test]
fn despawn_recursive_command_on_a_deep_chain() {
    use regia_scene::hierarchy::HierarchyCommands;
    const DEPTH: usize = 100_000;
    let mut world = World::new();
    let root = world.spawn(Transform::IDENTITY).id();
    let mut parent = root;
    for _ in 0..DEPTH {
        parent = world.spawn((Transform::IDENTITY, ChildOf(parent))).id();
    }
    let mut queue = bevy_ecs::world::CommandQueue::default();
    {
        let mut commands = Commands::new(&mut queue, &world);
        commands.entity(root).despawn_recursive();
    }
    queue.apply(&mut world);
    assert!(world.get_entity(root).is_err());
    assert!(world.get_entity(parent).is_err());
    let mut left = world.query::<&Transform>();
    assert_eq!(left.iter(&world).count(), 0);
}

/// A NaN translation does not panic the propagation, does not poison siblings, and the finite
/// entities settle (stop being recomputed) even though NaN never equals itself.
#[test]
fn a_nan_transform_does_not_poison_or_stall_its_neighbours() {
    let mut world = World::new();
    let bad = world
        .spawn(Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)))
        .id();
    let good = world.spawn(Transform::from_translation(Vec3::Y)).id();
    let under_good = world
        .spawn((Transform::from_translation(Vec3::Z), ChildOf(good)))
        .id();
    propagate_transforms(&mut world);
    assert!(
        world
            .get::<GlobalTransform>(bad)
            .expect("bad")
            .translation()
            .x
            .is_nan()
    );
    assert_eq!(
        world
            .get::<GlobalTransform>(under_good)
            .expect("child")
            .translation(),
        Vec3::new(0.0, 1.0, 1.0)
    );
    propagate_transforms(&mut world);
    propagate_transforms(&mut world);
    let stats = world.resource::<PropagationCache>().last_run();
    // The NaN entity is the only one still reported; the finite subtree is clean.
    assert!(stats.recomputed <= 1, "{stats:?}");
    assert_eq!(
        world
            .get::<GlobalTransform>(under_good)
            .expect("child")
            .translation(),
        Vec3::new(0.0, 1.0, 1.0)
    );
}

/// The sky-body maths never returns NaN or a negative value for non-finite or extreme input (an
/// infinite sun is the one input allowed an infinite answer).
#[test]
fn sky_body_math_is_total() {
    let body = SkyBody::jupiter_from_europa();
    for sun in [f32::NAN, f32::INFINITY, -5.0, 0.0, f32::MAX] {
        for dir in [
            [0.0; 3],
            [f32::NAN, 0.0, 0.0],
            [f32::INFINITY, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ] {
            let lux = body.planet_shine_lux(sun, dir);
            assert!(!lux.is_nan() && lux >= 0.0, "sun {sun} dir {dir:?} -> {lux}");
            if sun.is_finite() {
                assert!(lux.is_finite(), "sun {sun} dir {dir:?} -> {lux}");
            }
        }
    }
    let nan_body = SkyBody {
        angular_diameter_deg: f32::NAN,
        tint: [f32::NAN; 3],
        direction: [f32::NAN; 3],
        ..body
    };
    let lux = nan_body.planet_shine_lux(4400.0, [0.0, 1.0, 0.0]);
    assert!(lux.is_finite() && lux >= 0.0, "{lux}");
    assert_eq!(
        SkyBody {
            fill_lux: Some(-3.0),
            ..body
        }
        .fill_illuminance(4400.0, [0.0, 1.0, 0.0]),
        0.0
    );
    for (r, d) in [
        (f32::NAN, 10.0),
        (10.0, f32::NAN),
        (f32::INFINITY, f32::INFINITY),
        (f32::INFINITY, 1.0),
        (0.0, 10.0),
        (-1.0, 10.0),
        (10.0, 10.0),
        (10.0, 5.0),
    ] {
        assert_eq!(SkyBody::physical_diameter_deg(r, d), 180.0, "{r} {d}");
    }
    assert_eq!(SkyBody::physical_diameter_deg(1.0, f32::INFINITY), 0.0);
    let jupiter = SkyBody::physical_diameter_deg(71_492.0, 671_100.0);
    assert!((jupiter - 12.2).abs() < 0.05, "{jupiter}");
}
