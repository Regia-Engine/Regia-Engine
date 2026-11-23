// SPDX-License-Identifier: MPL-2.0
//! The terrain collider follows runtime terrain changes (ADR-1595, DO-4): a named hole opened at
//! runtime removes the collider's cells under it (a ray falls into the tunnel mouth), closing it
//! restores them, and a runtime height patch reaches the collider through
//! `CollisionHeightfield::patch` over the patched band only, with the same heights a full
//! rebuild gives.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, QueryFilter, RigidBody};
use regia_scene::Transform;
use regia_terrain::{
    CollisionChunk, CollisionHeightfield, HeightPatch, HoleShape, HolesMask, NamedHole, PatchTarget,
    TerrainHeightPatches, TerrainMap,
};

/// Probe points: the mouth's centre, inside the patch block, flat ground away from both.
const PROBES: [[f32; 2]; 3] = [[0.0, 0.0], [12.0, 12.0], [-15.0, 10.0]];

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// 41 × 41 samples over 40 m, flat at 2 m, loaded from a 16-bit field like a game map.
fn map() -> TerrainMap {
    let n = 41u32;
    let hf = regia_asset::Heightfield {
        width: n,
        height: n,
        samples: vec![6554; (n * n) as usize],
    };
    TerrainMap::new(&hf, 40.0, 20.0, 0.0).expect("map")
}

fn shape(c: &CollisionHeightfield) -> ColliderShape {
    ColliderShape::HeightField {
        nx: c.nx,
        nz: c.nz,
        heights: c.heights.clone(),
        scale: Vec3::from(c.scale),
    }
}

/// Hit heights of rays straight down at [`PROBES`] after `app` steps twice.
fn probe(app: &mut App) -> [Option<f32>; 3] {
    let slot: Arc<Mutex<Option<[Option<f32>; 3]>>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let down = |[x, z]: [f32; 2]| {
            queries
                .raycast(Vec3::new(x, 30.0, z), Vec3::NEG_Y, 60.0, QueryFilter::new())
                .map(|hit| hit.point.y)
        };
        *out.lock().expect("slot") = Some(PROBES.map(down));
    });
    app.update();
    app.update();
    let mut guard = slot.lock().expect("slot");
    guard.take().expect("the probe ran")
}

fn mouth(open: bool) -> HolesMask {
    let mut mask = HolesMask::default();
    mask.set_named(NamedHole {
        id: "tunnel_mouth".into(),
        open,
        shape: HoleShape::Rect {
            center: [0.0, 0.0],
            half_extents: [4.0, 3.0],
            rotation_radians: 0.3,
        },
    })
    .expect("hole");
    mask
}

#[test]
fn a_named_hole_opens_and_closes_the_collider() {
    let map = map();
    for (open, expect_hole) in [(false, false), (true, true)] {
        let collider = map.collision_heightfield(Some(&mouth(open)));
        assert_eq!(collider.cut_samples > 0, expect_hole);
        let mut app = physics_app();
        app.world_mut().spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            Collider {
                shape: shape(&collider),
                ..Collider::default()
            },
        ));
        let [centre, block, flat] = probe(&mut app);
        if expect_hole {
            assert!(centre.is_none(), "a ray into the open mouth hit {centre:?}");
        } else {
            assert!(
                centre.is_some_and(|y| (y - 2.0).abs() < 0.01),
                "closed: ground at {centre:?}"
            );
        }
        assert!(block.is_some_and(|y| (y - 2.0).abs() < 0.01), "{block:?}");
        assert!(flat.is_some_and(|y| (y - 2.0).abs() < 0.01), "{flat:?}");
    }
}

/// A patch raises a block by 6 m: the collider patched over each band equals a full rebuild
/// (holes cut the same), and a ray lands on the new top. The open mouth stays cut.
#[test]
fn a_height_patch_reaches_the_collider_band_by_band() {
    let mut map = map();
    let holes = mouth(true);
    let mut collider = map.collision_heightfield(Some(&holes));
    let queue = TerrainHeightPatches::default();
    queue.set_target(PatchTarget::of(&map));
    queue.set_budget(40);
    // Samples 28..36 × 28..36: world x, z from 8 m to 15 m.
    queue
        .request(HeightPatch {
            col: 28,
            row: 28,
            width: 8,
            height: 8,
            heights: vec![8.0; 64],
        })
        .expect("patch");
    let mut bands = 0;
    let mut written = 0;
    while !queue.is_idle() {
        for band in queue.take_frame() {
            band.apply(&mut map).expect("same grid");
            written += collider.patch(&map, Some(&holes), &band.rect);
            bands += 1;
        }
    }
    assert_eq!(
        (bands, written),
        (2, 64),
        "two frames of 40 samples: 5 rows, then 3"
    );
    let rebuilt = map.collision_heightfield(Some(&holes));
    assert_eq!(collider.cut_samples, rebuilt.cut_samples);
    let same = collider
        .heights
        .iter()
        .zip(&rebuilt.heights)
        .all(|(a, b)| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()));
    assert!(same, "a patched collider equals a full rebuild");

    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: shape(&collider),
            ..Collider::default()
        },
    ));
    let [centre, block, flat] = probe(&mut app);
    assert!(centre.is_none(), "the mouth stays open: {centre:?}");
    assert!(
        block.is_some_and(|y| (y - 8.0).abs() < 0.01),
        "the patched block: {block:?}"
    );
    assert!(
        flat.is_some_and(|y| (y - 2.0).abs() < 0.01),
        "untouched ground: {flat:?}"
    );
}

fn chunk_collider(chunk: &CollisionChunk) -> (Transform, RigidBody, Collider) {
    (
        Transform::from_translation(Vec3::from(chunk.center)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::HeightField {
                nx: chunk.nx,
                nz: chunk.nz,
                heights: chunk.heights.clone(),
                scale: Vec3::from(chunk.scale),
            },
            ..Collider::default()
        },
    )
}

/// The terrain collider as 16-cell chunks: rays land where they land on the single collider,
/// on chunk seams too; a patch rebuilds only the chunks holding its samples, and the open
/// mouth stays cut.
#[test]
fn chunked_colliders_match_the_single_collider_and_rebuild_only_touched_chunks() {
    let mut map = map();
    let holes = mouth(true);
    let mut collider = map.collision_heightfield(Some(&holes));
    let [gx, gz] = collider.chunk_grid(16);
    assert_eq!([gx, gz], [3, 3]);
    let mut app = physics_app();
    let mut entities = Vec::new();
    for z in 0..gz {
        for x in 0..gx {
            let chunk = collider.chunk(16, [x, z]).expect("chunk");
            entities.push(app.world_mut().spawn(chunk_collider(&chunk)).id());
        }
    }
    let [centre, block, flat] = probe(&mut app);
    assert!(centre.is_none(), "the mouth is cut in the chunks: {centre:?}");
    assert!(block.is_some_and(|y| (y - 2.0).abs() < 0.01), "{block:?}");
    assert!(flat.is_some_and(|y| (y - 2.0).abs() < 0.01), "{flat:?}");

    // Raise samples 28..36 (world 8..15 m): chunks x 1..2, z 1..2 hold them (sample 32 is the
    // border of chunks 1 and 2).
    let queue = TerrainHeightPatches::default();
    queue.set_target(PatchTarget::of(&map));
    queue
        .request(HeightPatch {
            col: 28,
            row: 28,
            width: 8,
            height: 8,
            heights: vec![8.0; 64],
        })
        .expect("patch");
    let mut rebuilt = std::collections::BTreeSet::new();
    for band in queue.take_frame() {
        band.apply(&mut map).expect("same grid");
        collider.patch(&map, Some(&holes), &band.rect);
        rebuilt.extend(collider.chunks_touching(16, &band.rect));
    }
    assert_eq!(
        rebuilt.iter().copied().collect::<Vec<_>>(),
        vec![[1, 1], [1, 2], [2, 1], [2, 2]],
        "only the chunks holding the patch are rebuilt"
    );
    for [x, z] in &rebuilt {
        let chunk = collider.chunk(16, [*x, *z]).expect("chunk");
        let entity = entities[(*z * gx + *x) as usize];
        app.world_mut()
            .entity_mut(entity)
            .insert(chunk_collider(&chunk).2);
    }
    let [centre, block, flat] = probe(&mut app);
    assert!(centre.is_none(), "the mouth stays open: {centre:?}");
    assert!(
        block.is_some_and(|y| (y - 8.0).abs() < 0.01),
        "the patched block: {block:?}"
    );
    assert!(
        flat.is_some_and(|y| (y - 2.0).abs() < 0.01),
        "untouched chunk: {flat:?}"
    );
}
