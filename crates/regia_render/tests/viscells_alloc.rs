// SPDX-License-Identifier: MPL-2.0
//! The cell cull is allocation-free in steady state (ADR-7586): once its scratch and the report's
//! lists have grown, [`cull_unreachable_cells`] over a freshly extracted, steady frame allocates
//! nothing on the calling thread (a counting global allocator, read per thread), while it walks
//! the portals and culls a room's draws every frame.
//!
//! The binary installs a global allocator, so this test lives here only. CPU only, no wall clock.

#![allow(unsafe_code)] // the counting `GlobalAlloc` (ADR-5631), as in `view_visibility_alloc.rs`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;

use regia_app::prelude::System as _;
use regia_app::prelude::*;
use regia_render::viscells::{VisCell, VisCellStats, VisCells, cull_unreachable_cells};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, ExtractedFrame, LabVertex, LodViewport, MeshRenderer,
    extract_frame,
};
use regia_scene::Transform;
use regia_scene::propagate::propagate_transforms;

struct Counting;

thread_local! {
    /// The calling thread's allocations (other test threads and the harness do not count).
    static THREAD_ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn thread_allocs() -> u64 {
    THREAD_ALLOCS.with(|c| c.get())
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.5, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.5, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

/// Two rooms and a doorway; the camera in room A faces away from it. 200 meshes in each room
/// (and 100 without a cell): after warm-up, each frame's cull (a fresh extraction every frame, so
/// it really culls 200 draws each time) allocates nothing.
#[test]
fn a_steady_frame_with_cells_allocates_nothing() {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<VisCellStats>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.insert_resource(
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
                (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
             ],
             portals: [(a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)])])",
        )
        .expect("valid cells"),
    );
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((
        Transform {
            translation: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::PI),
            ..Transform::default()
        },
        Camera::default(),
    ));
    let mesh = triangle();
    let material = Arc::new(CpuMaterial::default());
    for i in 0..500 {
        let x = (i as f32 * 0.01) - 2.5;
        let (z, cell) = match i % 5 {
            0 | 1 => (2.0, Some(1)),
            2 | 3 => (-8.0, Some(2)),
            _ => (2.0, None),
        };
        let mut entity = app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(x, 1.0, z)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ));
        if let Some(cell) = cell {
            entity.insert(VisCell(cell));
        }
    }
    let mut system = IntoSystem::into_system(cull_unreachable_cells);
    let _ = system.initialize(app.world_mut());
    let mut allocations = 0;
    for frame in 0..13 {
        // A fresh extraction (it allocates; it is not counted).
        app.update();
        let before = thread_allocs();
        let _ = system.run((), app.world_mut());
        if frame >= 3 {
            allocations += thread_allocs() - before;
        }
    }
    let stats = app.world().resource::<VisCellStats>().clone();
    assert_eq!(stats.reachable, vec![1], "{stats:?}");
    assert_eq!(stats.culled_draws, 200, "room B's 200 draws: {stats:?}");
    assert_eq!(stats.cell_draws, 400);
    assert_eq!(allocations, 0, "10 steady culls allocated {allocations} times");
}

/// The doorway rule and the depth-cap flood (ADR-7639) stay allocation-free too: a corridor of 40
/// rooms whose boxes stop at the faces of 0.3 m walls, the camera in the first door's wall gap
/// looking down it. The walk starts in both rooms of the doorway, narrows 32 portals deep, then
/// floods the rest; after warm-up, 10 culls allocate nothing.
#[test]
fn a_steady_frame_in_a_doorway_past_the_depth_cap_allocates_nothing() {
    let rooms = 40;
    let mut text = String::from("(cells: [");
    for i in 1..=rooms {
        text.push_str(&format!(
            "(id: {i}, bounds: [(center: (0.0, 1.5, {:.1}), half_extents: (4.0, 1.5, 3.85))]),",
            -8.0 * (i - 1) as f32
        ));
    }
    text.push_str("], portals: [");
    for i in 1..rooms {
        let z = -8.0 * (i - 1) as f32 - 4.0;
        text.push_str(&format!(
            "(a: {i}, b: {}, quad: [(-1.0, 0.0, {z:.1}), (1.0, 0.0, {z:.1}), (1.0, 2.0, {z:.1}), (-1.0, 2.0, {z:.1})]),",
            i + 1
        ));
    }
    text.push_str("])");
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<VisCellStats>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.insert_resource(VisCells::from_ron(&text).expect("valid cells"));
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    // In the wall gap of the door between rooms 1 and 2, looking down −Z (the default view).
    app.world_mut().spawn((
        Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, -4.05)),
        Camera {
            far: 1000.0,
            ..Camera::default()
        },
    ));
    let mesh = triangle();
    let material = Arc::new(CpuMaterial::default());
    for i in 1..=rooms {
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, -8.0 * (i - 1) as f32)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            VisCell(i as u16),
        ));
    }
    let mut system = IntoSystem::into_system(cull_unreachable_cells);
    let _ = system.initialize(app.world_mut());
    let mut allocations = 0;
    for frame in 0..13 {
        app.update();
        let before = thread_allocs();
        let _ = system.run((), app.world_mut());
        if frame >= 3 {
            allocations += thread_allocs() - before;
        }
    }
    let stats = app.world().resource::<VisCellStats>().clone();
    let view = &stats.views[0];
    assert_eq!(view.eye_cells, vec![1, 2], "{view:?}");
    assert!(view.depth_capped, "{view:?}");
    assert_eq!(view.reachable, (1..=rooms as u16).collect::<Vec<_>>(), "{view:?}");
    assert_eq!(stats.culled_draws, 0, "{stats:?}");
    assert_eq!(allocations, 0, "10 steady culls allocated {allocations} times");
}

/// The outdoor frusta (plan row W-22, ADR-8413) stay allocation-free: room A has a window to the
/// outdoors at z +4 and the camera looks out of it, so every cull records one outdoor frustum and one
/// mouth into `ExtractedFrame::outdoor_portals`; after warm-up, 10 culls allocate nothing.
#[test]
fn a_steady_frame_looking_out_of_a_window_allocates_nothing() {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<VisCellStats>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.insert_resource(
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
             ],
             portals: [(a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)])])",
        )
        .expect("valid cells"),
    );
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((
        Transform {
            translation: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::PI),
            ..Transform::default()
        },
        Camera::default(),
    ));
    let mut system = IntoSystem::into_system(cull_unreachable_cells);
    let _ = system.initialize(app.world_mut());
    let mut allocations = 0;
    for frame in 0..13 {
        app.update();
        let before = thread_allocs();
        let _ = system.run((), app.world_mut());
        if frame >= 3 {
            allocations += thread_allocs() - before;
        }
    }
    let portals = &app.world().resource::<ExtractedFrame>().outdoor_portals;
    assert!(portals.is_restricted(), "{portals:?}");
    assert_eq!(
        (portals.frusta().len(), portals.mouths().len()),
        (1, 1),
        "{portals:?}"
    );
    assert_eq!(allocations, 0, "10 steady culls allocated {allocations} times");
}

/// Shadow views per cell (ADR-8874) stay allocation-free on a steady sun frame: the camera in
/// room A faces away from the door, a vertical sun casts shadows, and one rigid caster sits in
/// room B. After warm-up the cull seals that caster every frame and allocates nothing.
#[test]
fn a_steady_frame_culling_sun_shadows_allocates_nothing() {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<VisCellStats>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.insert_resource(
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
                (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
             ],
             portals: [(a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)])])",
        )
        .expect("valid cells"),
    );
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((
        Transform {
            translation: regia_core::math::Vec3::new(0.0, 1.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::PI),
            ..Transform::default()
        },
        Camera::default(),
    ));
    app.world_mut().spawn(DirectionalLight {
        direction: [0.0, -1.0, 0.0],
        color: [1.0, 1.0, 1.0],
        illuminance: 3000.0,
        cast_shadows: true,
        static_shadow: false,
    });
    app.world_mut().spawn((
        Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, -8.0)),
        MeshRenderer::new(triangle(), Arc::new(CpuMaterial::default())),
        VisCell(2),
    ));
    let mut system = IntoSystem::into_system(cull_unreachable_cells);
    let _ = system.initialize(app.world_mut());
    let mut allocations = 0;
    for frame in 0..13 {
        app.update();
        let before = thread_allocs();
        let _ = system.run((), app.world_mut());
        if frame >= 3 {
            allocations += thread_allocs() - before;
        }
    }
    let stats = app.world().resource::<VisCellStats>().clone();
    assert_eq!(stats.reachable, vec![1], "{stats:?}");
    assert!(stats.shadow_view_active, "the shadow-view cull ran: {stats:?}");
    assert_eq!(stats.shadow_view_culled_draws, 1, "{stats:?}");
    assert_eq!(stats.shadow_view_dropped_draws, 1, "{stats:?}");
    assert_eq!(
        allocations, 0,
        "10 steady sun-shadow culls allocated {allocations} times"
    );
}
