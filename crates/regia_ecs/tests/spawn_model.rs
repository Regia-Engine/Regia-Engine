// SPDX-License-Identifier: MPL-2.0
//! Deterministic operation model for component arrival tracking.

use regia_ecs::prelude::*;
use regia_ecs::spawn_order::{SpawnOrder, track_spawn_order};

#[derive(Component)]
struct First(u32);

#[derive(Component)]
struct Second(u32);

#[derive(Component)]
struct Unrelated(u32);

#[derive(Clone, Copy)]
struct ModelEntity {
    entity: Entity,
    live: bool,
    first: bool,
    second: bool,
}

#[derive(Clone, Copy)]
enum Operation {
    SpawnDirect,
    SpawnDeferred,
    AddFirst,
    AddSecond,
    ReplaceFirst,
    ReplaceSecond,
    RemoveFirst,
    RemoveSecond,
    Despawn,
    MutateUnrelated,
    TrackAgain,
}

fn check<C: Component>(
    world: &World,
    expected: &[Entity],
    arrivals: &[(Entity, u64)],
    entities: &[ModelEntity],
) {
    let order = world.resource::<SpawnOrder<C>>();
    assert_eq!(order.len(), expected.len());
    for item in entities.iter().filter(|item| item.live) {
        let expected_sequence = arrivals
            .iter()
            .find_map(|(entity, sequence)| (*entity == item.entity).then_some(*sequence));
        assert_eq!(order.get(item.entity), expected_sequence);
    }
    let mut sorted = expected.to_vec();
    order.sort_by_spawn(&mut sorted, |entity| *entity);
    assert_eq!(sorted, expected);
    for &entity in expected {
        assert!(world.get_entity(entity).is_ok());
    }
}

#[test]
fn fixed_seed_component_arrival_model_matches_every_prefix() {
    let mut world = World::new();
    assert!(track_spawn_order::<First>(&mut world));
    assert!(track_spawn_order::<Second>(&mut world));

    let mut entities = Vec::<ModelEntity>::new();
    let mut first_order = Vec::<Entity>::new();
    let mut second_order = Vec::<Entity>::new();
    let mut first_arrivals = Vec::<(Entity, u64)>::new();
    let mut second_arrivals = Vec::<(Entity, u64)>::new();
    let mut first_next = 0_u64;
    let mut second_next = 0_u64;
    let mut random = 0x9e37_79b9_u32;

    for step in 0..512_u32 {
        random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let operation = match (random >> 24) % 11 {
            0 => Operation::SpawnDirect,
            1 => Operation::SpawnDeferred,
            2 => Operation::AddFirst,
            3 => Operation::AddSecond,
            4 => Operation::ReplaceFirst,
            5 => Operation::ReplaceSecond,
            6 => Operation::RemoveFirst,
            7 => Operation::RemoveSecond,
            8 => Operation::Despawn,
            9 => Operation::MutateUnrelated,
            _ => Operation::TrackAgain,
        };
        let live_indices: Vec<usize> = entities
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.live.then_some(index))
            .collect();
        let selected =
            (!live_indices.is_empty()).then(|| live_indices[(random as usize) % live_indices.len()]);

        match operation {
            Operation::SpawnDirect => {
                let (entity, with_first) = if random & 2 == 0 {
                    (world.spawn(First(step)).id(), true)
                } else {
                    (world.spawn_empty().id(), false)
                };
                if with_first {
                    first_order.push(entity);
                    first_arrivals.push((entity, first_next));
                    first_next += 1;
                }
                entities.push(ModelEntity {
                    entity,
                    live: true,
                    first: with_first,
                    second: false,
                });
            }
            Operation::SpawnDeferred => {
                let mut queue = bevy_ecs::world::CommandQueue::default();
                let mut commands = Commands::new(&mut queue, &world);
                let (entity, with_first) = if random & 1 == 0 {
                    (commands.spawn(First(step)).id(), true)
                } else {
                    (commands.spawn_empty().id(), false)
                };
                queue.apply(&mut world);
                if with_first {
                    first_order.push(entity);
                    first_arrivals.push((entity, first_next));
                    first_next += 1;
                }
                entities.push(ModelEntity {
                    entity,
                    live: true,
                    first: with_first,
                    second: false,
                });
            }
            Operation::AddFirst
            | Operation::AddSecond
            | Operation::ReplaceFirst
            | Operation::ReplaceSecond
            | Operation::RemoveFirst
            | Operation::RemoveSecond
            | Operation::Despawn
            | Operation::MutateUnrelated => {
                if let Some(index) = selected {
                    let item = &mut entities[index];
                    match operation {
                        Operation::AddFirst => {
                            if !item.first {
                                item.first = true;
                                first_order.push(item.entity);
                                first_arrivals.push((item.entity, first_next));
                                first_next += 1;
                            }
                            world.entity_mut(item.entity).insert(First(step));
                            assert_eq!(world.get::<First>(item.entity).map(|value| value.0), Some(step));
                        }
                        Operation::AddSecond => {
                            if !item.second {
                                item.second = true;
                                second_order.push(item.entity);
                                second_arrivals.push((item.entity, second_next));
                                second_next += 1;
                            }
                            world.entity_mut(item.entity).insert(Second(step));
                            assert_eq!(world.get::<Second>(item.entity).map(|value| value.0), Some(step));
                        }
                        Operation::ReplaceFirst => {
                            if item.first {
                                world.entity_mut(item.entity).insert(First(step));
                                assert_eq!(world.get::<First>(item.entity).map(|value| value.0), Some(step));
                            }
                        }
                        Operation::ReplaceSecond => {
                            if item.second {
                                world.entity_mut(item.entity).insert(Second(step));
                                assert_eq!(world.get::<Second>(item.entity).map(|value| value.0), Some(step));
                            }
                        }
                        Operation::RemoveFirst => {
                            if item.first {
                                item.first = false;
                                first_order.retain(|entity| *entity != item.entity);
                                first_arrivals.retain(|(entity, _)| *entity != item.entity);
                                world.entity_mut(item.entity).remove::<First>();
                            }
                        }
                        Operation::RemoveSecond => {
                            if item.second {
                                item.second = false;
                                second_order.retain(|entity| *entity != item.entity);
                                second_arrivals.retain(|(entity, _)| *entity != item.entity);
                                world.entity_mut(item.entity).remove::<Second>();
                            }
                        }
                        Operation::Despawn => {
                            assert!(world.despawn(item.entity));
                            item.live = false;
                            if item.first {
                                first_order.retain(|entity| *entity != item.entity);
                                first_arrivals.retain(|(entity, _)| *entity != item.entity);
                            }
                            if item.second {
                                second_order.retain(|entity| *entity != item.entity);
                                second_arrivals.retain(|(entity, _)| *entity != item.entity);
                            }
                        }
                        Operation::MutateUnrelated => {
                            world.entity_mut(item.entity).insert(Unrelated(step));
                            assert_eq!(
                                world.get::<Unrelated>(item.entity).map(|value| value.0),
                                Some(step)
                            );
                        }
                        _ => unreachable!(),
                    }
                }
            }
            Operation::TrackAgain => {
                assert!(track_spawn_order::<First>(&mut world));
                assert!(track_spawn_order::<Second>(&mut world));
            }
        }

        check::<First>(&world, &first_order, &first_arrivals, &entities);
        check::<Second>(&world, &second_order, &second_arrivals, &entities);
        for item in entities.iter().filter(|item| !item.live) {
            assert_eq!(world.resource::<SpawnOrder<First>>().get(item.entity), None);
            assert_eq!(world.resource::<SpawnOrder<Second>>().get(item.entity), None);
        }
    }

    let mut reuse_world = World::new();
    assert!(track_spawn_order::<First>(&mut reuse_world));
    let stale = reuse_world.spawn(First(0)).id();
    let Some(reused) = reuse_world.despawn_no_free(stale) else {
        panic!("the known live entity should be despawned");
    };
    assert!(reuse_world.spawn_at(reused, First(1)).is_ok());
    assert_eq!(
        reused.index(),
        stale.index(),
        "despawned entity slot should be reused"
    );
    assert_ne!(reused, stale, "the reused slot has a new generation");
    let order = reuse_world.resource::<SpawnOrder<First>>();
    assert_eq!(order.get(stale), None);
    assert_eq!(order.get(reused), Some(1));
}

#[test]
fn distinct_trackers_and_late_failed_tracking_keep_remove_hook_ownership() {
    let mut world = World::new();
    assert!(track_spawn_order::<First>(&mut world));
    assert!(track_spawn_order::<Second>(&mut world));
    let entity = world.spawn((First(1), Second(2))).id();
    assert_eq!(world.resource::<SpawnOrder<First>>().get(entity), Some(0));
    assert_eq!(world.resource::<SpawnOrder<Second>>().get(entity), Some(0));

    world
        .register_component_hooks::<Unrelated>()
        .on_add(|_world, _context| {});
    assert!(!track_spawn_order::<Unrelated>(&mut world));
    assert!(
        world
            .register_component_hooks::<Unrelated>()
            .try_on_remove(|_world, _context| {})
            .is_some()
    );
    world.entity_mut(entity).remove::<First>();
    assert_eq!(world.resource::<SpawnOrder<First>>().get(entity), None);
    assert_eq!(world.resource::<SpawnOrder<Second>>().get(entity), Some(0));
}
