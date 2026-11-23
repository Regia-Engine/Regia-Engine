// SPDX-License-Identifier: MPL-2.0
//! Building one terrain-collision tile must not copy the whole heightfield (audit G, finding 1).
//!
//! Its own binary: a counting global allocator. Two fields share the sample spacing and the tile
//! size, so the cut window and the rapier shape of that one tile are the same size. The bytes of
//! the update that builds the tile must not grow with the rest of the field.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_scene::Transform;
use regia_scene::level::{LevelId, LevelMember};
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_scene::stream_plan::{SourceId, SourceView};
use regia_scene::streaming::{
    StreamGates, StreamLevelLayout, StreamLevelLayouts, StreamViews, TerrainTileCollision,
};

struct Counting;

// The desktop app runs the tile system on a worker. A thread-local counter on the
// test thread never saw `build_tile`'s copy of the heightfield.
static BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        BYTES.fetch_add(new_size, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn bytes() -> usize {
    BYTES.load(Ordering::Relaxed)
}

fn level() -> LevelId {
    LevelId(Guid::from_u128(0x71E1))
}

/// Bytes allocated by the single update that builds the one tile under the view.
/// `n` samples on a side, 4 m apart, tiles of 64 m, so every field cuts the same window.
fn bytes_to_build_one_tile(n: usize) -> usize {
    let width = (n - 1) as f32 * 4.0;
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
            grid: StreamGrid::new(64.0).expect("grid"),
            bounds: [CellCoord::new(-20, -20), CellCoord::new(20, 20)],
            records: Default::default(),
        },
    );
    app.insert_resource(layouts);
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::HeightField {
                nx: n,
                nz: n,
                heights: vec![1.0; n * n],
                scale: Vec3::new(width, 1.0, width),
            },
            ..Collider::default()
        },
        TerrainTileCollision {
            tile_m: 64.0,
            view_radius_m: 10.0,
            max_tiles_per_step: 0,
        },
        LevelMember::from_level(level()),
    ));
    // Warm the plugin before the view holds a tile, so spawn and the first sync are not counted.
    app.update();
    app.update();
    app.world_mut().resource_mut::<StreamViews>().0 =
        vec![SourceView::at(SourceId::Pin(1), [-96.0, 0.0, -96.0])];
    let before = bytes();
    app.update();
    bytes() - before
}

#[test]
fn building_one_tile_does_not_copy_the_whole_heightfield() {
    let small = bytes_to_build_one_tile(65);
    let large = bytes_to_build_one_tile(257);
    let delta = large.saturating_sub(small);
    // The cut window is the same size on both fields. A clone of the source heights would add
    // about 4 * (257² − 65²) bytes. Staying under one small field means that clone is gone.
    let whole_small = 4 * 65 * 65;
    assert!(
        delta < whole_small,
        "building the same-sized tile grew by {delta} bytes from 65² to 257² (small update {small}, large update {large}); a full-field clone is about {} bytes",
        4 * (257 * 257 - 65 * 65)
    );
}
