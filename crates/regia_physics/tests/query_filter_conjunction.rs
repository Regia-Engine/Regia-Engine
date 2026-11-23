// SPDX-License-Identifier: MPL-2.0
//! Combined `QueryFilter` oracle: matrix row, explicit mask, entity exclusion, sensor
//! exclusion, and predicate must all constrain the same query. Expected answers are derived
//! from the table below and elementary axis-aligned box geometry.
//!
//! Provenance: the reference model and geometry in this test are original elementary logic;
//! query behavior is specified by Regia `specs/15` § 3 and exercised through Rapier's public API.

use std::collections::BTreeSet;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, CollisionLayer, PhysicsLayers, PhysicsPlugin, PhysicsWorld, QueryFilter,
    RigidBody, WorldQueries,
};
use regia_scene::Transform;

#[derive(Clone, Copy)]
struct Target {
    entity: Entity,
    layer: u16,
    sensor: bool,
    centre_x: f32,
    half_x: f32,
    predicate: bool,
}

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

fn layers(query_rows: [u32; 3]) -> PhysicsLayers {
    let all = 0b111;
    PhysicsLayers {
        names: vec!["a".into(), "b".into(), "c".into()],
        collision_rows: [all; 32],
        query_rows: [
            query_rows[0],
            query_rows[1],
            query_rows[2],
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
    }
}

fn world_queries(app: &App) -> WorldQueries<'_> {
    WorldQueries {
        world: app.world().resource::<PhysicsWorld>(),
        layers: Some(app.world().resource::<PhysicsLayers>()),
    }
}

fn entity_set(entities: impl IntoIterator<Item = Entity>) -> BTreeSet<Entity> {
    entities.into_iter().collect()
}

fn expected(
    targets: &[Target],
    row: u32,
    mask: Option<u32>,
    excluded: Option<Entity>,
    exclude_sensors: bool,
    predicate_on: bool,
) -> Vec<Target> {
    targets
        .iter()
        .copied()
        .filter(|target| row & (1 << target.layer) != 0)
        .filter(|target| mask.is_none_or(|mask| mask & (1 << target.layer) != 0))
        .filter(|target| excluded != Some(target.entity))
        .filter(|target| !exclude_sensors || !target.sensor)
        .filter(|target| !predicate_on || target.predicate)
        .collect()
}

fn filter<'a>(
    from_layer: u16,
    mask: Option<u32>,
    excluded: Option<&'a [Entity]>,
    exclude_sensors: bool,
    predicate: Option<&'a dyn Fn(Entity) -> bool>,
) -> QueryFilter<'a> {
    let mut filter = QueryFilter::new().from_layer(CollisionLayer::new(from_layer));
    if let Some(mask) = mask {
        filter = filter.layers(mask);
    }
    if let Some(excluded) = excluded {
        filter = filter.exclude_entities(excluded);
    }
    if exclude_sensors {
        filter = filter.exclude_sensors();
    }
    if let Some(predicate) = predicate {
        filter = filter.predicate(predicate);
    }
    filter
}

#[test]
fn query_filter_conditions_are_conjoined_without_call_state_leakage() {
    let mut app = app();
    // All collider matrix rows admit every layer; only the independent query matrix
    // controls visibility. Boxes are separated along X and have distinct ray distances.
    let specs = [
        (0, false, true),
        (1, true, false),
        (2, false, true),
        (0, true, false),
        (1, false, true),
        (2, true, true),
    ];
    let mut targets = Vec::new();
    for (index, (layer, sensor, predicate)) in specs.into_iter().enumerate() {
        let centre_x = index as f32 * 3.0;
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(centre_x, 0.0, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(0.5),
                    },
                    sensor,
                    layer: CollisionLayer::new(layer),
                    ..Collider::default()
                },
            ))
            .id();
        targets.push(Target {
            entity,
            layer,
            sensor,
            centre_x,
            half_x: 0.5,
            predicate,
        });
    }
    app.insert_resource(layers([0b101, 0b110, 0b011]));
    for _ in 0..3 {
        app.update();
    }

    // Small Boolean product: 3 query rows × 3 explicit masks (including absent) ×
    // 2 entity-exclusion choices × 2 sensor choices × 2 predicate choices = 72 cases.
    let masks = [None, Some(0b011), Some(0b110)];
    for row_index in 0..3u16 {
        for mask in masks {
            for excluded_index in [None, Some(2usize)] {
                for exclude_sensors in [false, true] {
                    for predicate_on in [false, true] {
                        let excluded = excluded_index.map(|index| [targets[index].entity]);
                        let predicate = |entity| {
                            targets
                                .iter()
                                .find(|target| target.entity == entity)
                                .is_some_and(|target| target.predicate)
                        };
                        let selected = filter(
                            row_index,
                            mask,
                            excluded.as_ref().map(|entities| &entities[..]),
                            exclude_sensors,
                            predicate_on.then_some(&predicate as &dyn Fn(Entity) -> bool),
                        );
                        let row = app.world().resource::<PhysicsLayers>().query_rows[row_index as usize];
                        let expected = expected(
                            &targets,
                            row,
                            mask,
                            excluded_index.map(|index| targets[index].entity),
                            exclude_sensors,
                            predicate_on,
                        );
                        let expected_set = entity_set(expected.iter().map(|target| target.entity));
                        let queries = world_queries(&app);

                        // An enclosing AABB-free query shape intersects every candidate box.
                        let overlaps = queries.overlap(
                            &ColliderShape::Box {
                                half_extents: Vec3::new(20.0, 2.0, 2.0),
                            },
                            Transform::from_translation(Vec3::new(7.5, 0.0, 0.0)),
                            selected,
                        );
                        assert_eq!(
                            entity_set(overlaps),
                            expected_set,
                            "overlap row={row_index} mask={mask:?} excluded={excluded_index:?} sensors={exclude_sensors} predicate={predicate_on}"
                        );

                        // Recreate the filter after overlap (filter values are consumed).
                        let selected = filter(
                            row_index,
                            mask,
                            excluded.as_ref().map(|entities| &entities[..]),
                            exclude_sensors,
                            predicate_on.then_some(&predicate as &dyn Fn(Entity) -> bool),
                        );
                        let ray_origin = Vec3::new(-2.0, 0.0, 0.0);
                        let ray = queries.raycast_all(ray_origin, Vec3::X, 30.0, selected);
                        let mut analytic = expected.clone();
                        analytic.sort_by(|a, b| (a.centre_x - a.half_x).total_cmp(&(b.centre_x - b.half_x)));
                        assert_eq!(
                            ray.iter().map(|hit| hit.entity).collect::<Vec<_>>(),
                            analytic.iter().map(|target| target.entity).collect::<Vec<_>>(),
                            "ray entity order row={row_index} mask={mask:?} excluded={excluded_index:?} sensors={exclude_sensors} predicate={predicate_on}"
                        );
                        for (hit, target) in ray.iter().zip(&analytic) {
                            let distance = target.centre_x - target.half_x - ray_origin.x;
                            assert!(
                                (hit.distance - distance).abs() < 1.0e-4,
                                "analytic ray distance: {hit:?} vs {distance}"
                            );
                        }
                        let selected = filter(
                            row_index,
                            mask,
                            excluded.as_ref().map(|entities| &entities[..]),
                            exclude_sensors,
                            predicate_on.then_some(&predicate as &dyn Fn(Entity) -> bool),
                        );
                        let nearest_ray = queries.raycast(ray_origin, Vec3::X, 30.0, selected);
                        if let Some(nearest) = analytic.first() {
                            let hit = nearest_ray.expect("eligible box is on the ray");
                            let distance = nearest.centre_x - nearest.half_x - ray_origin.x;
                            assert_eq!(hit.entity, nearest.entity);
                            assert!((hit.distance - distance).abs() < 1.0e-4);
                        } else {
                            assert!(nearest_ray.is_none(), "no eligible AABB is on the ray");
                        }
                        if let Some(nearest) = analytic.first() {
                            let selected = filter(
                                row_index,
                                mask,
                                excluded.as_ref().map(|entities| &entities[..]),
                                exclude_sensors,
                                predicate_on.then_some(&predicate as &dyn Fn(Entity) -> bool),
                            );
                            let closest = queries
                                .closest_point(ray_origin, 30.0, selected)
                                .expect("eligible box has a closest point");
                            let distance = nearest.centre_x - nearest.half_x - ray_origin.x;
                            assert_eq!(
                                closest.entity, nearest.entity,
                                "closest point picks nearest accepted AABB"
                            );
                            assert!((closest.point.x - (nearest.centre_x - nearest.half_x)).abs() < 1.0e-4);
                            assert!((closest.point.distance(ray_origin) - distance).abs() < 1.0e-4);
                        } else {
                            let selected = filter(
                                row_index,
                                mask,
                                excluded.as_ref().map(|entities| &entities[..]),
                                exclude_sensors,
                                predicate_on.then_some(&predicate as &dyn Fn(Entity) -> bool),
                            );
                            assert!(queries.closest_point(ray_origin, 30.0, selected).is_none());
                        }

                        // A restrictive query followed by an unrestricted query in the
                        // same frame must not retain any part of the prior filter.
                        let unrestricted = queries.raycast_all(ray_origin, Vec3::X, 30.0, QueryFilter::new());
                        assert_eq!(unrestricted.len(), targets.len());
                        assert_eq!(
                            entity_set(unrestricted.iter().map(|hit| hit.entity)),
                            entity_set(targets.iter().map(|target| target.entity)),
                        );
                    }
                }
            }
        }
    }

    // Changing only the query matrix resource changes results immediately, including
    // after a restrictive call, while explicit masks remain intersected with the new row.
    app.world_mut().resource_mut::<PhysicsLayers>().query_rows[0] = 1 << 1;
    let queries = world_queries(&app);
    let expected = expected(&targets, 1 << 1, Some(0b111), None, false, false);
    let result = queries.raycast_all(
        Vec3::new(-2.0, 0.0, 0.0),
        Vec3::X,
        30.0,
        QueryFilter::new()
            .from_layer(CollisionLayer::new(0))
            .layers(0b111),
    );
    assert_eq!(
        entity_set(result.iter().map(|hit| hit.entity)),
        entity_set(expected.iter().map(|target| target.entity)),
    );
    let overlap = queries.overlap(
        &ColliderShape::Box {
            half_extents: Vec3::new(20.0, 2.0, 2.0),
        },
        Transform::from_translation(Vec3::new(7.5, 0.0, 0.0)),
        QueryFilter::new()
            .from_layer(CollisionLayer::new(0))
            .layers(0b111),
    );
    assert_eq!(
        entity_set(overlap),
        entity_set(expected.iter().map(|target| target.entity)),
    );
    let closest = queries
        .closest_point(
            Vec3::new(-2.0, 0.0, 0.0),
            30.0,
            QueryFilter::new().from_layer(CollisionLayer::new(0)),
        )
        .expect("matrix row admits eligible layer-1 boxes");
    assert_eq!(closest.entity, expected[0].entity);
    assert!((closest.point.x - (expected[0].centre_x - expected[0].half_x)).abs() < 1.0e-4);
}
