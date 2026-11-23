// SPDX-License-Identifier: MPL-2.0
//! Per-tile terrain collision (spec 78 § 13.1, ADR-2317): a heightfield with
//! `TerrainTileCollision` is a collider only where spawned cells and sources need it, a tile shared by
//! cells stays while any of them holds it, and the `terrain_collision` gate answers per cell.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_physics::{
    Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, QueryFilter, RigidBody, TerrainTileStats,
};
use regia_scene::Transform;
use regia_scene::level::{LevelId, LevelMember};
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_scene::stream_plan::{SourceId, SourceView};
use regia_scene::streaming::{
    StreamGates, StreamLevelLayout, StreamLevelLayouts, StreamViews, TERRAIN_COLLISION_GATE,
    TerrainTileCollision,
};

/// Field: 65 × 65 samples 4 m apart (256 m), flat at y = 1, centered on the origin.
const N: usize = 65;
const WIDTH: f32 = 256.0;

fn level() -> LevelId {
    LevelId(Guid::from_u128(0x7E11))
}

type Hits = Arc<Mutex<Vec<Option<f32>>>>;

/// A physics app with a tiled terrain, cells of `cell_m` (records at the given cells) and a probe
/// that casts a ray down at each of `probes` every step.
fn app(cell_m: f64, records: &[CellCoord], view_radius_m: f32, probes: Vec<[f32; 2]>) -> (App, Hits) {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app.init_resource::<StreamGates>();
    app.init_resource::<StreamViews>();
    let mut layouts = StreamLevelLayouts::default();
    layouts.0.insert(
        level(),
        StreamLevelLayout {
            grid: StreamGrid::new(cell_m).expect("grid"),
            bounds: [CellCoord::new(-4, -4), CellCoord::new(3, 3)],
            records: records.iter().copied().collect(),
        },
    );
    app.insert_resource(layouts);
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::HeightField {
                nx: N,
                nz: N,
                heights: vec![1.0; N * N],
                scale: Vec3::new(WIDTH, 1.0, WIDTH),
            },
            ..Collider::default()
        },
        TerrainTileCollision {
            tile_m: 64.0,
            view_radius_m,
            max_tiles_per_step: 0,
        },
        LevelMember::from_level(level()),
    ));
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let slot = hits.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let found = probes
            .iter()
            .map(|p| {
                queries
                    .raycast(Vec3::new(p[0], 10.0, p[1]), Vec3::NEG_Y, 20.0, QueryFilter::new())
                    .map(|hit| hit.point.y)
            })
            .collect();
        *slot.lock().expect("slot") = found;
    });
    (app, hits)
}

fn steps(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn stats(app: &App) -> TerrainTileStats {
    *app.world().resource::<TerrainTileStats>()
}

fn spawned(app: &mut App, cell: CellCoord) {
    app.world_mut()
        .resource_mut::<StreamGates>()
        .note_spawned(level(), cell);
}

fn unloaded(app: &mut App, cell: CellCoord) {
    app.world_mut()
        .resource_mut::<StreamGates>()
        .forget_cell(level(), cell);
}

fn terrain_ready(app: &App, cell: CellCoord) -> bool {
    !app.world()
        .resource::<StreamGates>()
        .missing(level(), cell)
        .contains(&TERRAIN_COLLISION_GATE)
}

fn hit(hits: &Hits, i: usize) -> Option<f32> {
    hits.lock().expect("hits").get(i).copied().flatten()
}

#[test]
fn only_the_tiles_of_spawned_cells_collide_and_the_gate_marks_them() {
    // Cell (0, 0) is [0, 64]²: tile (2, 2). A probe inside it and one far away.
    let cell = CellCoord::new(0, 0);
    let (mut app, hits) = app(64.0, &[cell], 0.0, vec![[32.0, 32.0], [-100.0, -100.0]]);
    steps(&mut app, 2);
    assert_eq!(stats(&app).terrains, 1);
    assert_eq!(stats(&app).resident_tiles, 0, "nothing holds a tile yet");
    assert!(hit(&hits, 0).is_none());

    spawned(&mut app, cell);
    steps(&mut app, 2);
    assert!(terrain_ready(&app, cell));
    assert_eq!(stats(&app).resident_tiles, 1);
    assert_eq!(stats(&app).held_cells, 1);
    let ground = hit(&hits, 0).expect("the cell's ground collides");
    assert!((ground - 1.0).abs() < 1.0e-3, "{ground}");
    assert!(hit(&hits, 1).is_none(), "a tile nothing holds collides");

    unloaded(&mut app, cell);
    steps(&mut app, 2);
    assert_eq!(stats(&app).resident_tiles, 0);
    assert_eq!(stats(&app).removed, 1);
    assert!(hit(&hits, 0).is_none());
}

#[test]
fn a_tile_shared_by_cells_stays_while_any_of_them_holds_it() {
    // 32 m cells: (0, 0) and (1, 0) both lie in tile (2, 2).
    let (a, b) = (CellCoord::new(0, 0), CellCoord::new(1, 0));
    let (mut app, hits) = app(32.0, &[a, b], 0.0, vec![[16.0, 16.0]]);
    // The gate exists from the first step (a streamed level's open registers it too).
    steps(&mut app, 1);
    spawned(&mut app, a);
    spawned(&mut app, b);
    steps(&mut app, 2);
    assert!(terrain_ready(&app, a) && terrain_ready(&app, b));
    assert_eq!(stats(&app).resident_tiles, 1);
    unloaded(&mut app, a);
    steps(&mut app, 2);
    assert_eq!(stats(&app).resident_tiles, 1, "cell (1, 0) still holds the tile");
    assert!(
        hit(&hits, 0).is_some(),
        "the shared tile still collides under cell (0, 0)"
    );
    unloaded(&mut app, b);
    steps(&mut app, 2);
    assert_eq!(stats(&app).resident_tiles, 0);
}

#[test]
fn sources_hold_the_ground_of_cells_with_no_content() {
    // No records: every cell is empty. A source at (-100, -100) with a 10 m radius holds tile (0, 0),
    // which covers the empty cell (-2, -2) = [-128, -64]².
    let empty = CellCoord::new(-2, -2);
    let (mut app, hits) = app(64.0, &[], 10.0, vec![[-100.0, -100.0], [100.0, 100.0]]);
    app.world_mut().resource_mut::<StreamViews>().0 =
        vec![SourceView::at(SourceId::Pin(1), [-100.0, 0.0, -100.0])];
    steps(&mut app, 2);
    assert_eq!(stats(&app).resident_tiles, 1);
    assert!(app.world().resource::<StreamGates>().is_empty_cell_ready(
        level(),
        empty,
        regia_scene::streaming::Readiness::SimReady
    ));
    assert!(hit(&hits, 0).is_some());
    assert!(hit(&hits, 1).is_none());

    // The source leaves: the tile goes and the empty cell is not ready any more.
    app.world_mut().resource_mut::<StreamViews>().0 =
        vec![SourceView::at(SourceId::Pin(1), [100.0, 0.0, 100.0])];
    steps(&mut app, 2);
    assert!(!app.world().resource::<StreamGates>().is_empty_cell_ready(
        level(),
        empty,
        regia_scene::streaming::Readiness::SimReady
    ));
    assert!(hit(&hits, 0).is_none());
    assert!(hit(&hits, 1).is_some());
}

#[test]
fn the_per_step_limit_builds_tiles_in_order_over_several_steps() {
    // The 256 m cell (-1, -1) = [-256, 0]² holds the four tiles of the field's corner; one per step.
    let cell = CellCoord::new(-1, -1);
    let (mut app, _hits) = app(256.0, &[cell], 0.0, vec![]);
    {
        let world = app.world_mut();
        let mut q = world.query::<&mut TerrainTileCollision>();
        for mut tiles in q.iter_mut(world) {
            tiles.max_tiles_per_step = 1;
        }
    }
    steps(&mut app, 1);
    spawned(&mut app, cell);
    let mut resident = Vec::new();
    for _ in 0..6 {
        steps(&mut app, 1);
        resident.push(stats(&app).resident_tiles);
    }
    assert_eq!(resident.last(), Some(&4));
    assert!(
        resident.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] <= 1),
        "{resident:?}"
    );
    assert_eq!(stats(&app).pending_tiles, 0);
    assert!(terrain_ready(&app, cell));
}

/// Audit g-physics2 finding 5: unloading a tile under a body in contact reports the contact's end
/// (`CollisionEnded` naming the terrain), as removing any other collider does. The tile's rapier
/// collider used to go without the record `PhysicsWorld::remove_collider` keeps for the next
/// step's `Stopped` events, so the end was dropped and gameplay contact state stayed open.
#[test]
fn unloading_a_tile_under_a_touching_body_reports_the_contact_end() {
    use regia_ecs::ecs::message::Messages;
    use regia_ecs::prelude::{Entity, With};
    use regia_physics::{ActiveEvents, CollisionEnded, CollisionStarted};

    let cell = CellCoord::new(0, 0);
    let (mut app, _hits) = app(64.0, &[cell], 0.0, vec![]);
    let terrain = {
        let world = app.world_mut();
        let mut q = world.query_filtered::<(Entity, &mut Collider), With<TerrainTileCollision>>();
        let (entity, mut collider) = q.single_mut(world).expect("one terrain");
        collider.active_events = ActiveEvents {
            collision: true,
            contact_force: false,
        };
        entity
    };
    let crate_e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(32.0, 1.6, 32.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..Collider::default()
            },
        ))
        .id();
    let pair = |a: Entity, b: Entity| (a, b) == (terrain, crate_e) || (a, b) == (crate_e, terrain);
    let mut events: Vec<bool> = Vec::new();
    let run = |app: &mut App, n: usize, events: &mut Vec<bool>| {
        for _ in 0..n {
            app.update();
            let started = app.world().resource::<Messages<CollisionStarted>>();
            events.extend(
                started
                    .iter_current_update_messages()
                    .filter(|m| pair(m.entity_a, m.entity_b))
                    .map(|_| true),
            );
            let ended = app.world().resource::<Messages<CollisionEnded>>();
            events.extend(
                ended
                    .iter_current_update_messages()
                    .filter(|m| pair(m.entity_a, m.entity_b))
                    .map(|_| false),
            );
        }
    };
    steps(&mut app, 1);
    spawned(&mut app, cell);
    run(&mut app, 40, &mut events);
    assert_eq!(stats(&app).resident_tiles, 1);
    assert_eq!(
        events.last(),
        Some(&true),
        "the crate rests on the tile: {events:?}"
    );

    events.clear();
    unloaded(&mut app, cell);
    run(&mut app, 10, &mut events);
    assert_eq!(stats(&app).resident_tiles, 0);
    assert_eq!(events, vec![false], "exactly one end for the unloaded tile");
}

/// Corr-physics audit: a tile whose window is all holes (NaN samples) can never be built, and it
/// kept every cell over it from being marked `terrain_collision`, so the streaming runtime waited
/// on that cell for good. A tile with nothing to build counts as done.
#[test]
fn a_cell_over_an_all_hole_tile_is_still_marked() {
    // Cell (0, 0) is [0, 64]²: tile (2, 2), samples 32..=48 on both axes (4 m apart from -128).
    let cell = CellCoord::new(0, 0);
    let (mut app, hits) = app(64.0, &[cell], 0.0, vec![[32.0, 32.0]]);
    let mut terrains = app.world_mut().query::<&mut Collider>();
    for mut collider in terrains.iter_mut(app.world_mut()) {
        if let ColliderShape::HeightField { heights, .. } = &mut collider.shape {
            for z in 32..=48 {
                for x in 32..=48 {
                    heights[x + z * N] = f32::NAN;
                }
            }
        }
    }
    steps(&mut app, 2);
    spawned(&mut app, cell);
    steps(&mut app, 3);
    assert!(
        terrain_ready(&app, cell),
        "the cell over the hole tile was never marked"
    );
    assert!(hit(&hits, 0).is_none(), "a hole has no ground");
    assert_eq!(stats(&app).resident_tiles, 0, "nothing was built");
}
