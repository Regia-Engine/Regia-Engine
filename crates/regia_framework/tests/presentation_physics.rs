// SPDX-License-Identifier: MPL-2.0
//! Presentation physics next to an authoritative sim (`book/systems/sim-bridge.md`, "Presentation
//! physics"): the bridge's sim entities are kinematic hulls driven by their snapshot poses each
//! fixed step, the ground collider is built from the same `TerrainMap` and holes the draw uses,
//! and engine debris (no sim id, dynamic) collides with both while the bridge never writes it.
//!
//! Every test runs fixed 1/60 s steps with one fixed step per update; no wall clock.

use std::collections::HashMap;
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::sim_bridge::presentation::{
    DebrisBurst, HullShapes, PresentationGround, PresentationPhysicsPlugin, PresentationPhysicsStats,
    cube_chunk,
};
use regia_framework::sim_bridge::{
    BridgeInput, ExternalSim, KindRegistry, SimBridgePlugin, SimEntityMap, SimEntityView, SimId, SimSnapshot,
    SnapshotTick,
};
use regia_physics::{BodyKind, Collider, ColliderShape, DebrisTag, PhysicsPlugin, RigidBody};
use regia_scene::Transform;
use regia_terrain::{HoleEllipse, HolesMask, TerrainMap};

/// Half extents of the test hull: a 4 m × 0.5 m × 4 m deck.
const HULL_HALF: Vec3 = Vec3::new(2.0, 0.25, 2.0);
/// Half size of a debris cube.
const CHUNK_HALF: f32 = 0.25;
/// Stated bound: a resting chunk's center is within this of the surface top plus its half size
/// (the contact skin and the solver's allowed penetration are millimetres).
const REST_BOUND_M: f32 = 0.05;
/// Stated bound: how far a chunk riding the hull may lag or lead the hull's travel (the hull starts
/// moving at full speed in one step, so the chunk slips a few centimetres before friction holds it).
const RIDE_BOUND_M: f32 = 0.1;

/// Samples per side of the flat test ground (1 m apart).
const GROUND_SAMPLES: u32 = 33;
/// World width of the test ground, metres (centered on the origin).
const GROUND_WIDTH_M: f32 = 32.0;
/// Center `(x, z)` of the test ground's one hole. Off the X = Z diagonal on purpose.
const HOLE_CENTER: [f32; 2] = [8.0, 0.0];
/// Radius of the test ground's hole, metres.
const HOLE_RADIUS_M: f32 = 2.5;

// ---------------------------------------------------------------------------
// A scripted authoritative sim: one hull whose pose the test moves.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HullSnapshot {
    hulls: Vec<(u64, Vec3)>,
}

impl SimSnapshot for HullSnapshot {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for (id, translation) in &self.hulls {
            f(SimEntityView {
                id: *id,
                kind: "hull",
                team: 0,
                translation: *translation,
                rotation: Quat::IDENTITY,
            });
        }
    }
}

/// The sim owns the hull poses; the test edits them between frames as the authority would.
#[derive(Resource)]
struct HullSim {
    tick: u64,
    hulls: Vec<(u64, Vec3)>,
}

impl ExternalSim for HullSim {
    type Snapshot = HullSnapshot;
    type Event = String;

    fn submit_input(&mut self, _input: &BridgeInput) {}

    fn read_snapshot(&mut self, _frame_time: Duration, out: &mut HullSnapshot) -> SnapshotTick {
        self.tick += 1;
        out.hulls.clone_from(&self.hulls);
        SnapshotTick(self.tick)
    }

    fn drain_events(&mut self, _out: &mut Vec<String>) {}

    fn event_kind(event: &String) -> String {
        event.clone()
    }
}

/// Counts `Transform` inserts after spawn, split by whether the entity carries a sim id. The
/// bridge rewrites a sim entity's pose by inserting `Transform`; physics writes dynamic poses in
/// place. A debris chunk's own spawn is its first insert.
#[derive(Resource, Default)]
struct TransformInserts {
    per_entity: HashMap<Entity, u64>,
    sim_entity_inserts: u64,
}

fn count_transform_inserts(
    on: On<Insert<Transform>>,
    sim: Query<Has<SimId>>,
    mut counts: ResMut<TransformInserts>,
) {
    let entity = on.event().entity;
    *counts.per_entity.entry(entity).or_insert(0) += 1;
    if sim.get(entity).unwrap_or(false) {
        counts.sim_entity_inserts += 1;
    }
}

/// Flat ground at y = 0 with one hole, plus the presentation collider built from it.
fn sample_ground() -> (TerrainMap, HolesMask, PresentationGround) {
    let n = GROUND_SAMPLES;
    let heights = vec![0.0; (n * n) as usize];
    let map = TerrainMap::from_heights(n, n, heights, GROUND_WIDTH_M, 1.0, 0.0).expect("test ground map");
    let holes = HolesMask::new(vec![HoleEllipse {
        center: HOLE_CENTER,
        radius: [HOLE_RADIUS_M; 2],
        rotation_radians: 0.0,
    }])
    .expect("test ground hole");
    let ground = PresentationGround::from_terrain(&map, Some(&holes));
    (map, holes, ground)
}

fn app(hulls: Vec<(u64, Vec3)>, shapes: HullShapes, ground: PresentationGround) -> App {
    let kinds =
        KindRegistry::from_ron(r#"(kinds: { "hull": (presentation_scripts: []) })"#).expect("kinds ron");
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        PhysicsPlugin,
        SimBridgePlugin::<HullSim>::new(HullSim { tick: 0, hulls }).kinds(kinds),
    ));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.insert_resource(ground);
    app.add_plugins(PresentationPhysicsPlugin::new(shapes));
    app.init_resource::<TransformInserts>();
    app.add_observer(count_transform_inserts);
    app
}

fn box_hulls() -> HullShapes {
    HullShapes::single(
        "hull",
        ColliderShape::Box {
            half_extents: HULL_HALF,
        },
    )
}

fn steps(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn burst(app: &mut App, origin: Vec3) {
    app.world_mut().write_message(DebrisBurst {
        origin,
        chunks: vec![cube_chunk(Vec3::ZERO, CHUNK_HALF, 5.0)],
        speed: 0.0,
    });
}

/// Debris chunks: tagged by the spawner, never given a sim id.
type DebrisOnly = (With<DebrisTag>, Without<SimId>);

/// Every debris chunk (no sim id) with its translation, in entity order.
fn debris(app: &mut App) -> Vec<(Entity, Vec3)> {
    let world = app.world_mut();
    let mut q = world.query_filtered::<(Entity, &Transform), DebrisOnly>();
    let mut out: Vec<(Entity, Vec3)> = q.iter(world).map(|(e, t)| (e, t.translation)).collect();
    out.sort_by_key(|(e, _)| *e);
    out
}

/// Where the authority puts the hull `tick` steps into its move: 0.01 m per step along +X.
fn moved(start: Vec3, tick: u32) -> Vec3 {
    start + Vec3::new(tick as f32 * 0.01, 0.0, 0.0)
}

fn set_hull(app: &mut App, id: u64, at: Vec3) {
    let mut sim = app.world_mut().resource_mut::<HullSim>();
    if let Some(hull) = sim.hulls.iter_mut().find(|(i, _)| *i == id) {
        hull.1 = at;
    }
}

#[test]
fn debris_rests_on_a_bridged_kinematic_hull_and_rides_with_it() {
    let start = Vec3::new(0.0, 2.0, 0.0);
    let (_, _, ground) = sample_ground();
    let mut app = app(vec![(1, start)], box_hulls(), ground);
    // Frame 1 spawns the sim entity, frame 2 gives it a kinematic body.
    steps(&mut app, 3);
    let stats = *app.world().resource::<PresentationPhysicsStats>();
    assert_eq!(
        stats.hull_bodies, 1,
        "the bridged hull got one kinematic body: {stats:?}"
    );

    // Drop one cube 1.5 m above the deck and let it settle.
    burst(&mut app, start + Vec3::new(0.0, 2.0, 0.0));
    steps(&mut app, 240);
    let rest = debris(&mut app);
    assert_eq!(rest.len(), 1, "one debris chunk: {rest:?}");
    let top = start.y + HULL_HALF.y + CHUNK_HALF;
    let y = rest[0].1.y;
    assert!(
        (y - top).abs() <= REST_BOUND_M,
        "the chunk rests on the hull: center y {y} vs deck top + half size {top} (bound {REST_BOUND_M} m)"
    );

    // The authority moves the hull 1.2 m along +X at 0.6 m/s, then holds it.
    let x0 = rest[0].1.x;
    for tick in 1..=120 {
        set_hull(&mut app, 1, moved(start, tick));
        steps(&mut app, 1);
    }
    steps(&mut app, 60);
    let rode = debris(&mut app);
    assert_eq!(rode.len(), 1, "the chunk is still there: {rode:?}");
    let travelled = rode[0].1.x - x0;
    assert!(
        (travelled - 1.2).abs() <= RIDE_BOUND_M,
        "the chunk rides the hull: moved {travelled} m with the hull's 1.2 m (bound {RIDE_BOUND_M} m)"
    );
    assert!(
        (rode[0].1.y - top).abs() <= REST_BOUND_M,
        "the chunk is still on the deck: y {} vs {top}",
        rode[0].1.y
    );

    // The hull's pose is the snapshot's, never physics': its Transform is exactly where the sim put it.
    let hull_entity = app
        .world()
        .resource::<SimEntityMap>()
        .entity(1)
        .expect("hull entity");
    let hull_t = app
        .world()
        .get::<Transform>(hull_entity)
        .expect("hull transform")
        .translation;
    assert_eq!(hull_t, moved(start, 120), "the hull stays where the sim put it");
    // And the debris stays a dynamic body.
    let kind = app.world().get::<BodyKind>(rode[0].0).copied();
    assert_eq!(
        kind,
        Some(BodyKind::Dynamic),
        "debris without a sim id stays dynamic"
    );
}

#[test]
fn debris_over_a_terrain_hole_falls_through_and_debris_beside_it_rests() {
    // No sim entities near the drops: only the terrain collider matters.
    let (map, holes, ground) = sample_ground();
    let mut app = app(vec![(1, Vec3::new(0.0, 50.0, 0.0))], box_hulls(), ground);
    steps(&mut app, 3);
    let [hx, hz] = HOLE_CENTER;
    assert!(holes.contains(hx, hz), "the test hole is cut");
    // Over the hole, and the mirror spot across the hole's X = Z diagonal (where a transposed
    // collider would have put it), which must be solid ground.
    burst(&mut app, Vec3::new(hx, 1.0, hz));
    steps(&mut app, 1);
    burst(&mut app, Vec3::new(hz, 1.0, hx));
    steps(&mut app, 120);
    let chunks = debris(&mut app);
    assert_eq!(chunks.len(), 2, "two debris chunks: {chunks:?}");
    // Pick each chunk by where it was dropped (they fall straight down), not by entity order.
    let nearest = |x: f32, z: f32| {
        chunks
            .iter()
            .map(|(_, p)| *p)
            .min_by(|a, b| {
                let da = (a.x - x).powi(2) + (a.z - z).powi(2);
                let db = (b.x - x).powi(2) + (b.z - z).powi(2);
                da.total_cmp(&db)
            })
            .expect("a chunk")
    };
    let over_hole = nearest(hx, hz);
    let beside = nearest(hz, hx);
    assert!(
        over_hole.y < -5.0,
        "the chunk over the hole falls through the terrain collider: y {}",
        over_hole.y
    );
    let ground_y = map.sample_bilinear(hz, hx) + CHUNK_HALF;
    assert!(
        (beside.y - ground_y).abs() <= REST_BOUND_M,
        "the chunk off the hole rests on the ground: y {} vs {ground_y} (bound {REST_BOUND_M} m)",
        beside.y
    );
}

#[test]
fn the_bridge_never_writes_a_transform_without_a_sim_id() {
    let start = Vec3::new(0.0, 2.0, 0.0);
    let (_, _, ground) = sample_ground();
    let mut app = app(vec![(1, start)], box_hulls(), ground);
    steps(&mut app, 3);
    // One chunk on the hull, one on the ground beside it.
    app.world_mut().write_message(DebrisBurst {
        origin: start + Vec3::new(0.0, 2.0, 0.0),
        chunks: vec![
            cube_chunk(Vec3::ZERO, CHUNK_HALF, 5.0),
            cube_chunk(Vec3::new(-6.0, -3.0, 0.0), CHUNK_HALF, 5.0),
        ],
        speed: 0.0,
    });
    steps(&mut app, 60);
    for tick in 1..=120 {
        set_hull(&mut app, 1, moved(start, tick));
        steps(&mut app, 1);
    }
    let chunks = debris(&mut app);
    assert_eq!(chunks.len(), 2, "two debris chunks: {chunks:?}");
    let counts = app.world().resource::<TransformInserts>();
    // Positive control: the counter sees the bridge's pose writes on the moving hull.
    assert!(
        counts.sim_entity_inserts >= 120,
        "the bridge rewrote the hull's pose every moving frame: {} inserts",
        counts.sim_entity_inserts
    );
    for (entity, _) in &chunks {
        assert_eq!(
            counts.per_entity.get(entity).copied(),
            Some(1),
            "a debris chunk's Transform is inserted once (its spawn), never by the bridge"
        );
    }
    // The bridge's map holds the sim entity only.
    let map = app.world().resource::<SimEntityMap>();
    assert_eq!(map.iter().count(), 1, "the bridge tracks one entity");
    for (entity, _) in &chunks {
        assert_eq!(map.sim_id(*entity), None, "debris is not a sim entity");
    }
}

#[test]
fn a_dynamic_body_with_a_sim_id_is_refused() {
    let start = Vec3::new(0.0, 2.0, 0.0);
    let (_, _, ground) = sample_ground();
    let mut app = app(vec![(1, start)], box_hulls(), ground);
    steps(&mut app, 3);
    let hull_entity = app
        .world()
        .resource::<SimEntityMap>()
        .entity(1)
        .expect("hull entity");
    let before = app
        .world()
        .get::<Transform>(hull_entity)
        .expect("hull transform")
        .translation;
    assert_eq!(before, start, "the hull starts where the sim put it");
    app.world_mut()
        .entity_mut(hull_entity)
        .insert(RigidBody::dynamic());
    steps(&mut app, 1);
    let stats = *app.world().resource::<PresentationPhysicsStats>();
    assert!(
        stats.refused_writes >= 1,
        "a dynamic body on a sim id is counted: {stats:?}"
    );
    let after = app
        .world()
        .get::<Transform>(hull_entity)
        .expect("hull transform")
        .translation;
    assert_eq!(after, before, "physics did not write the sim entity's Transform");
    let kind = app.world().get::<RigidBody>(hull_entity).map(|body| body.kind);
    assert_ne!(
        kind,
        Some(BodyKind::Dynamic),
        "a sim entity is not left dynamic: {kind:?}"
    );
}

#[test]
fn a_sphere_hull_is_attached() {
    let (_, _, ground) = sample_ground();
    let shapes = HullShapes::single("hull", ColliderShape::Sphere { radius: 0.4 });
    let mut app = app(vec![(1, Vec3::new(0.0, 1.0, 0.0))], shapes, ground);
    steps(&mut app, 3);
    let hull_entity = app
        .world()
        .resource::<SimEntityMap>()
        .entity(1)
        .expect("hull entity");
    match app
        .world()
        .get::<Collider>(hull_entity)
        .map(|collider| &collider.shape)
    {
        Some(ColliderShape::Sphere { radius }) => assert_eq!(*radius, 0.4),
        other => panic!("expected a sphere hull, got {other:?}"),
    }
}

#[test]
fn hole_cells_are_the_cells_the_collider_cuts() {
    let (map, holes, ground) = sample_ground();
    let field = map.collision_heightfield(Some(&holes));
    let rows = field.nx;
    let cols = field.nz;
    let cut = |row: usize, col: usize| field.heights[row + col * rows].is_nan();
    let mut expected = Vec::new();
    for row in 0..rows.saturating_sub(1) {
        for col in 0..cols.saturating_sub(1) {
            if cut(row, col) || cut(row + 1, col) || cut(row, col + 1) || cut(row + 1, col + 1) {
                expected.push((row as u32, col as u32));
            }
        }
    }
    assert!(!expected.is_empty(), "the hole cuts cells");
    assert_eq!(ground.hole_cells, expected);
    match ground.collider.shape {
        ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } => {
            assert_eq!((nx, nz), (field.nx, field.nz));
            assert_eq!(scale, Vec3::from(field.scale));
            assert_eq!(heights.len(), field.heights.len());
            for (a, b) in heights.iter().zip(&field.heights) {
                assert_eq!(a.to_bits(), b.to_bits());
            }
        }
        other => panic!("expected a heightfield, got {other:?}"),
    }
}
