// SPDX-License-Identifier: MPL-2.0
//! `ViewVisibility` is written allocation-free in steady state (spec 14 § A1, ADR-7200): once the
//! scratch lists and the per-mesh bounds have grown, [`update_view_visibility`] over a steady
//! frame allocates nothing on the calling thread (a counting global allocator, read per thread).
//!
//! The binary installs a global allocator, so this test lives here only. CPU only (no device), no
//! wall clock.

#![allow(unsafe_code)] // the counting `GlobalAlloc` (ADR-5631), as in `post_volumes_steady.rs`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;

use regia_app::prelude::System as _;
use regia_app::prelude::*;
use regia_render::{
    Camera, CpuMaterial, CpuMesh, ExtractedFrame, LabVertex, LodViewport, MeshRenderer, ViewVisibilityStats,
    extract_frame, update_view_visibility,
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

/// 300 meshes (half ahead of the camera, half behind it) on two shared mesh allocations: after
/// three warm-up frames the `ViewVisibility` write of a steady frame allocates nothing and
/// changes nothing.
#[test]
fn a_steady_view_visibility_write_allocates_nothing() {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((Transform::default(), Camera::default()));
    let meshes = [triangle(), triangle()];
    let material = Arc::new(CpuMaterial::default());
    for i in 0..300 {
        let z = if i % 2 == 0 { -5.0 } else { 5.0 };
        let x = (i as f32 * 0.01) - 1.5;
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, z)),
            MeshRenderer::new(Arc::clone(&meshes[i % 2]), Arc::clone(&material)),
        ));
    }
    for _ in 0..3 {
        app.update();
    }
    let mut system = IntoSystem::into_system(update_view_visibility);
    let _ = system.initialize(app.world_mut());
    // Warm the system's own scratch (first run grows it).
    for _ in 0..3 {
        let _ = system.run((), app.world_mut());
    }
    let before = thread_allocs();
    for _ in 0..10 {
        let _ = system.run((), app.world_mut());
    }
    let allocations = thread_allocs() - before;
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert_eq!(
        stats.visible, 150,
        "half the meshes are ahead of the camera: {stats:?}"
    );
    assert_eq!(stats.writes, 0, "a steady frame changes nothing: {stats:?}");
    assert_eq!(allocations, 0, "10 steady writes allocated {allocations} times");
}

/// The same steady write with skinned meshes (posed bounds, ADR-7255) and a published occlusion
/// result folded in: after warm-up it allocates nothing, and the counts are the frustum's minus
/// the occluded ones.
#[test]
fn a_steady_write_with_skinned_posed_bounds_and_occlusion_allocates_nothing() {
    use regia_render::{JointMatrix, OcclusionFeedback, SkinJoints};
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<OcclusionFeedback>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((Transform::default(), Camera::default()));
    let rigid = triangle();
    let skinned = {
        let mut vertices = triangle().vertices.clone();
        for vertex in &mut vertices {
            vertex.joints = [0; 4];
            vertex.weights = [1.0, 0.0, 0.0, 0.0];
        }
        Arc::new(CpuMesh {
            vertices,
            indices: vec![0, 1, 2],
        })
    };
    let material = Arc::new(CpuMaterial::default());
    let mut ahead = Vec::new();
    for i in 0..200 {
        let x = (i as f32 * 0.01) - 1.0;
        // Rigid: alternately ahead and behind.
        let z = if i % 2 == 0 { -5.0 } else { 5.0 };
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, z)),
                MeshRenderer::new(Arc::clone(&rigid), Arc::clone(&material)),
            ))
            .id();
        if i % 2 == 0 {
            ahead.push(entity);
        }
        // Skinned: the bind pose ahead, the pose 10 m back (behind) for odd `i`, in place for even.
        let pose = if i % 2 == 0 { 0.0 } else { 10.0 };
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, -5.0)),
            MeshRenderer::new(Arc::clone(&skinned), Arc::clone(&material)),
            SkinJoints {
                palette: vec![JointMatrix::translation(0.0, 0.0, pose)],
            },
        ));
    }
    // A host-published occlusion result: 30 of the rigid meshes ahead are occluded.
    let occluded: Vec<u64> = ahead.iter().take(30).map(|e| e.to_bits()).collect();
    app.world().resource::<OcclusionFeedback>().publish(&occluded);
    for _ in 0..3 {
        app.update();
    }
    let mut system = IntoSystem::into_system(update_view_visibility);
    let _ = system.initialize(app.world_mut());
    for _ in 0..3 {
        let _ = system.run((), app.world_mut());
    }
    let before = thread_allocs();
    for _ in 0..10 {
        let _ = system.run((), app.world_mut());
    }
    let allocations = thread_allocs() - before;
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert_eq!(
        (stats.visible, stats.occluded),
        (100 - 30 + 100, 30),
        "rigid ahead minus the occluded, plus the skinned posed ahead: {stats:?}"
    );
    assert_eq!(stats.writes, 0, "a steady frame changes nothing: {stats:?}");
    assert_eq!(allocations, 0, "10 steady writes allocated {allocations} times");
}

/// ADR-7286: two views (split screen), the left camera looking ahead and the right one looking
/// back. The first view's occlusion result hides 30 meshes ahead, which the second view does not
/// see: they are not visible, the 100 meshes behind (the second view's) are, and the steady write
/// still allocates nothing.
#[test]
fn a_steady_write_with_two_views_and_occlusion_allocates_nothing() {
    use regia_render::{OcclusionFeedback, ViewVisibilityPerView};
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<ViewVisibilityPerView>();
    app.init_resource::<OcclusionFeedback>();
    app.insert_resource(LodViewport {
        width: 128,
        height: 48,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    // View 0 (spawned first): looks down -z. View 1: turned around, looks down +z.
    app.world_mut().spawn((
        Transform::default(),
        Camera {
            viewport: [0.0, 0.0, 0.5, 1.0],
            ..Camera::default()
        },
    ));
    app.world_mut().spawn((
        Transform::from_rotation(regia_core::math::Quat::from_rotation_y(std::f32::consts::PI)),
        Camera {
            viewport: [0.5, 0.0, 0.5, 1.0],
            ..Camera::default()
        },
    ));
    let mesh = triangle();
    let material = Arc::new(CpuMaterial::default());
    let mut ahead = Vec::new();
    for i in 0..200 {
        let z = if i % 2 == 0 { -5.0 } else { 5.0 };
        let x = (i as f32 * 0.01) - 1.0;
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, z)),
                MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            ))
            .id();
        if i % 2 == 0 {
            ahead.push(entity);
        }
    }
    let occluded: Vec<u64> = ahead.iter().take(30).map(|e| e.to_bits()).collect();
    let feedback = app.world().resource::<OcclusionFeedback>().clone();
    feedback.publish(&occluded);
    // ADR-7329: each view's Hi-Z pyramid, as the renderer reports it (one per view).
    const PYRAMID: u64 = 64 * 48 * 4 * 4 / 3;
    feedback.note_view_pyramid(0, PYRAMID);
    feedback.note_view_pyramid(1, PYRAMID);
    for _ in 0..3 {
        app.update();
    }
    let mut system = IntoSystem::into_system(update_view_visibility);
    let _ = system.initialize(app.world_mut());
    let mut rings = Vec::new();
    for _ in 0..3 {
        let _ = system.run((), app.world_mut());
        feedback.view_stats_into(&mut rings);
    }
    let before = thread_allocs();
    for _ in 0..10 {
        let _ = system.run((), app.world_mut());
        // The renderer re-reports an unchanged pyramid each frame; the agent reads the views.
        feedback.note_view_pyramid(0, PYRAMID);
        feedback.note_view_pyramid(1, PYRAMID);
        feedback.view_stats_into(&mut rings);
    }
    let allocations = thread_allocs() - before;
    assert_eq!(
        (feedback.view_pyramid_bytes(0), feedback.view_pyramid_bytes(1)),
        (PYRAMID, PYRAMID),
        "a pyramid per view"
    );
    assert_eq!(feedback.stats().pyramid_bytes, 2 * PYRAMID);
    assert_eq!(
        rings.first().map(|ring| ring.stats.pyramid_bytes),
        Some(PYRAMID),
        "view 0's ring reports its pyramid: {rings:?}"
    );
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert_eq!(stats.views, 2, "{stats:?}");
    assert_eq!(
        (stats.visible, stats.occluded),
        (100 - 30 + 100, 30),
        "ahead minus the first view's occluded, plus the second view's: {stats:?}"
    );
    assert_eq!(stats.writes, 0, "a steady frame changes nothing: {stats:?}");
    assert_eq!(allocations, 0, "10 steady writes allocated {allocations} times");
}
