// SPDX-License-Identifier: MPL-2.0
//! Hidden things stop drawing (spec 14 § A1, spec 37 § 13, ADR-7200).
//!
//! - A `Visibility::Hidden` mesh is absent from the extracted frame, and so is a mesh child of a
//!   hidden parent (the child has no `Visibility` of its own).
//! - Turning it visible draws it the next frame.
//! - `ViewVisibility` is true for a mesh on screen and false for one behind the camera after one
//!   frame, false for a hidden one, and a steady frame writes nothing.
//! - On lavapipe, a frame with the hidden mesh equals a frame of a world that never had it (same
//!   frame number, fresh renderers), and differs from a frame where it shows.
//! - On lavapipe, a hidden caster outside the view leaves no shadow on the ground in view under a
//!   shadow-casting sun, where the same caster shown does (w9-visibility-backport, ADR-7588).
//!
//! - Known issues of v0.5.1, proved by ignored tests that assert the intended behavior (they fail
//!   on v0.5.1 and pass once the v0.6 line's ADR-7255 lands; `--ignored` runs them, ADR-7670):
//!   a skinned draw passes the frustum test whole (its posed bound is not used), and
//!   `ViewVisibility` does not read GPU occlusion back (a mesh behind a wall reads visible).
//!
//! - A skinned mesh is culled with its posed bounds (the joint palette moves each joint's
//!   bind-space box, ADR-248), not its bind pose: posed off screen it is not view-visible even when
//!   its bind pose is on screen, and posed on screen it is view-visible and draws even when its bind
//!   pose is behind the camera (lavapipe: the image differs from a world without it).
//!
//! The ECS tests are CPU only; the image tests need a headless device (lavapipe).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, ExtractedFrame, FrameRenderer, JointMatrix, LabVertex,
    LodViewport, MeshRenderer, RenderDevice, SkinJoints, ViewVisibilityStats, extract_frame,
    update_view_visibility,
};
use regia_scene::Transform;
use regia_scene::components::{InheritedVisibility, ViewVisibility, Visibility};
use regia_scene::propagate::propagate_transforms;

/// A unit cube (±0.5 m) around the origin.
fn cube() -> Arc<CpuMesh> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                0.5 * (n[0] + su * u[0] + sv * v[0]),
                0.5 * (n[1] + su * u[1] + sv * v[1]),
                0.5 * (n[2] + su * u[2] + sv * v[2]),
            ];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                n,
                [0.5 * (su + 1.0), 0.5 * (sv + 1.0)],
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

fn material(color: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: color,
        ..CpuMaterial::default()
    })
}

/// An app with propagation in `PostUpdate` and extraction plus the `ViewVisibility` write in
/// `Render`, a window camera at the origin looking down −Z, and a 64×48 viewport.
fn app() -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, (extract_frame, update_view_visibility).chain());
    app.world_mut().spawn((
        Transform::default(),
        Camera {
            exposure_ev: Some(10.0),
            ..Camera::default()
        },
    ));
    app
}

fn mesh_at(app: &mut App, at: Vec3Like, color: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(at.0, at.1, at.2)),
            MeshRenderer::new(cube(), material(color)),
        ))
        .id()
}

type Vec3Like = (f32, f32, f32);

fn drawn(app: &App, entity: Entity) -> bool {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .any(|m| m.entity == entity)
}

fn view_visible(app: &App, entity: Entity) -> bool {
    app.world().get::<ViewVisibility>(entity).is_some_and(|v| v.0)
}

#[test]
fn a_hidden_mesh_is_absent_from_the_extracted_frame_and_shows_the_next_frame() {
    let mut app = app();
    let control = mesh_at(&mut app, (-1.5, 0.0, -5.0), [0.8, 0.8, 0.8, 1.0]);
    let subject = mesh_at(&mut app, (1.5, 0.0, -5.0), [0.9, 0.1, 0.1, 1.0]);
    app.world_mut().entity_mut(subject).insert(Visibility::Hidden);
    app.update();
    assert!(drawn(&app, control), "the control draws");
    assert!(!drawn(&app, subject), "a Hidden mesh is not extracted");
    assert!(!view_visible(&app, subject), "a Hidden mesh is not view-visible");

    app.world_mut().entity_mut(subject).insert(Visibility::Visible);
    app.update();
    assert!(drawn(&app, subject), "turned visible, it draws the next frame");
    assert!(view_visible(&app, subject));
}

#[test]
fn a_mesh_child_of_a_hidden_parent_is_hidden() {
    let mut app = app();
    let parent = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, -5.0)),
            Visibility::Hidden,
        ))
        .id();
    // The child has no `Visibility`: `MeshRenderer` brings `InheritedVisibility`.
    let child = app
        .world_mut()
        .spawn((
            Transform::default(),
            MeshRenderer::new(cube(), material([0.2, 0.8, 0.2, 1.0])),
            ChildOf(parent),
        ))
        .id();
    app.update();
    assert_eq!(
        app.world().get::<InheritedVisibility>(child).map(|v| v.0),
        Some(false),
        "the child resolves hidden"
    );
    assert!(!drawn(&app, child), "a child of a hidden parent is not extracted");

    app.world_mut().entity_mut(parent).insert(Visibility::Inherited);
    app.update();
    assert!(
        drawn(&app, child),
        "the parent shown, the child draws the next frame"
    );
}

#[test]
fn view_visibility_is_true_on_screen_and_false_behind_the_camera_after_one_frame() {
    let mut app = app();
    let ahead = mesh_at(&mut app, (0.0, 0.0, -5.0), [0.8, 0.8, 0.8, 1.0]);
    let behind = mesh_at(&mut app, (0.0, 0.0, 5.0), [0.8, 0.8, 0.8, 1.0]);
    app.update();
    assert!(
        drawn(&app, behind),
        "behind the camera is still extracted (culling is per view)"
    );
    assert!(view_visible(&app, ahead), "on screen after one frame");
    assert!(!view_visible(&app, behind), "behind the camera after one frame");
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert_eq!((stats.views, stats.visible), (1, 1), "{stats:?}");
    assert!(stats.writes >= 1, "{stats:?}");

    // A steady frame writes nothing and marks nothing changed.
    app.update();
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert_eq!(stats.writes, 0, "a steady frame writes nothing: {stats:?}");

    // The camera turns around: the two swap on the next frame.
    let mut q = app.world_mut().query_filtered::<&mut Transform, With<Camera>>();
    for mut transform in q.iter_mut(app.world_mut()) {
        transform.rotation = regia_core::math::Quat::from_rotation_y(std::f32::consts::PI);
    }
    app.update();
    assert!(!view_visible(&app, ahead));
    assert!(view_visible(&app, behind));
}

#[test]
fn a_mesh_on_a_layer_no_camera_sees_is_not_view_visible() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, -5.0)),
            MeshRenderer {
                layer: 7,
                ..MeshRenderer::new(cube(), material([0.8, 0.8, 0.8, 1.0]))
            },
        ))
        .id();
    let mut q = app.world_mut().query::<&mut Camera>();
    for mut camera in q.iter_mut(app.world_mut()) {
        camera.layer_mask = 1;
    }
    app.update();
    assert!(
        !view_visible(&app, entity),
        "layer 7 is outside the camera's mask"
    );
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The scene's first frame, rendered by a fresh renderer (frame number 1 for every world).
fn render_first_frame(device: &RenderDevice, app: &mut App) -> Vec<u8> {
    app.update();
    let extracted = app.world().resource::<ExtractedFrame>();
    let scene = extracted.frame_scene(0, 64, 48);
    FrameRenderer::new()
        .render_offscreen(device, &scene)
        .expect("frame")
        .pixels
}

#[test]
fn a_hidden_mesh_renders_like_a_world_that_never_had_it() {
    let device = device();
    let build = |subject: Option<Visibility>| {
        let mut app = app();
        mesh_at(&mut app, (-1.2, 0.0, -4.0), [0.8, 0.8, 0.8, 1.0]);
        if let Some(visibility) = subject {
            let entity = mesh_at(&mut app, (1.2, 0.0, -4.0), [0.9, 0.1, 0.1, 1.0]);
            app.world_mut().entity_mut(entity).insert(visibility);
        }
        app
    };
    let hidden = render_first_frame(&device, &mut build(Some(Visibility::Hidden)));
    let never = render_first_frame(&device, &mut build(None));
    let shown = render_first_frame(&device, &mut build(Some(Visibility::Visible)));
    assert_eq!(hidden.len(), never.len());
    let differing = hidden.iter().zip(&never).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the hidden mesh left {differing} bytes on the image"
    );
    let shown_diff = shown.iter().zip(&never).filter(|(a, b)| a != b).count();
    assert!(
        shown_diff > 64,
        "the shown mesh draws ({shown_diff} bytes differ): the test sees it"
    );
}

/// The unit cube with every vertex weighted fully to joint 0 (a one-joint skin).
fn skinned_cube() -> Arc<CpuMesh> {
    Arc::new(skinned_cube_data())
}

fn skinned_cube_data() -> CpuMesh {
    let rigid = cube();
    let mut vertices = rigid.vertices.clone();
    for vertex in &mut vertices {
        vertex.joints = [0; 4];
        vertex.weights = [1.0, 0.0, 0.0, 0.0];
    }
    CpuMesh {
        vertices,
        indices: rigid.indices.clone(),
    }
}

/// A one-joint skinned cube whose entity (its bind pose) sits at `at` and whose joint moves it by
/// `pose` metres: the posed cube is at `at + pose`.
fn skinned_at(app: &mut App, at: Vec3Like, pose: Vec3Like, color: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(at.0, at.1, at.2)),
            MeshRenderer::new(skinned_cube(), material(color)),
            SkinJoints {
                palette: vec![JointMatrix::translation(pose.0, pose.1, pose.2)],
            },
        ))
        .id()
}

#[test]
fn a_skinned_mesh_posed_off_screen_is_not_view_visible_though_its_bind_pose_is_on_screen() {
    let mut app = app();
    // Bind pose 5 m ahead; the joint carries the whole cube 10 m back, behind the camera.
    let away = skinned_at(&mut app, (0.0, 0.0, -5.0), (0.0, 0.0, 10.0), [0.8, 0.8, 0.8, 1.0]);
    // The control: bind pose ahead, posed in place.
    let stays = skinned_at(&mut app, (1.0, 0.0, -5.0), (0.0, 0.0, 0.0), [0.8, 0.8, 0.8, 1.0]);
    app.update();
    assert!(
        drawn(&app, away),
        "a skinned mesh is extracted (culling is per view)"
    );
    assert!(view_visible(&app, stays), "the control is posed on screen");
    assert!(
        !view_visible(&app, away),
        "posed behind the camera, the skinned mesh is not view-visible (its bind pose is on screen)"
    );

    // The pose comes back on screen: visible the next frame.
    if let Some(mut joints) = app.world_mut().get_mut::<SkinJoints>(away) {
        joints.palette[0] = JointMatrix::translation(0.0, 0.0, 0.0);
    }
    app.update();
    assert!(view_visible(&app, away), "posed on screen again");
}

#[test]
fn a_skinned_mesh_posed_on_screen_is_view_visible_though_its_bind_pose_is_behind_the_camera() {
    let mut app = app();
    // Bind pose 5 m behind the camera; the joint carries the limb 10 m forward, 5 m ahead.
    let limb = skinned_at(&mut app, (0.0, 0.0, 5.0), (0.0, 0.0, -10.0), [0.9, 0.1, 0.1, 1.0]);
    app.update();
    assert!(
        view_visible(&app, limb),
        "posed on screen, the skinned mesh is view-visible (its bind pose is behind the camera)"
    );
}

#[test]
fn a_skinned_mesh_without_a_safe_bound_stays_view_visible() {
    let mut app = app();
    // The vertices weight joint 0, the palette is empty: no bound (the renderer refuses the
    // draw), so culling cannot prove it off screen and keeps it (conservative).
    let entity = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 5.0)),
            MeshRenderer::new(skinned_cube(), material([0.8, 0.8, 0.8, 1.0])),
            SkinJoints { palette: Vec::new() },
        ))
        .id();
    // Negative weights: no bound either.
    let negative = {
        let mut mesh = skinned_cube_data();
        mesh.vertices[0].weights = [1.5, -0.5, 0.0, 0.0];
        app.world_mut()
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 5.0)),
                MeshRenderer::new(Arc::new(mesh), material([0.8, 0.8, 0.8, 1.0])),
                SkinJoints {
                    palette: vec![JointMatrix::identity(); 2],
                },
            ))
            .id()
    };
    app.update();
    assert!(drawn(&app, negative));
    assert!(
        view_visible(&app, negative),
        "negative weights have no safe bound: kept visible"
    );
    // A rigid draw is what an empty palette extracts to (`joints` empty), culled by its mesh
    // sphere: behind the camera, not visible.
    assert!(
        !view_visible(&app, entity),
        "an empty palette draws rigid, behind the camera"
    );
}

#[test]
fn a_skinned_limb_posed_on_screen_draws() {
    let device = device();
    let build = |limb: bool| {
        let mut app = app();
        mesh_at(&mut app, (-1.2, 0.0, -4.0), [0.8, 0.8, 0.8, 1.0]);
        if limb {
            // Bind pose behind the camera, posed 4 m ahead and to the right.
            skinned_at(&mut app, (1.2, 0.0, 6.0), (0.0, 0.0, -10.0), [0.9, 0.1, 0.1, 1.0]);
        }
        app
    };
    let mut with_limb = build(true);
    let shown = render_first_frame(&device, &mut with_limb);
    let entity = {
        let mut q = with_limb.world_mut().query_filtered::<Entity, With<SkinJoints>>();
        q.iter(with_limb.world()).next()
    };
    let never = render_first_frame(&device, &mut build(false));
    assert!(
        entity.is_some_and(|e| view_visible(&with_limb, e)),
        "the posed limb is view-visible"
    );
    assert_eq!(shown.len(), never.len());
    let differing = shown.iter().zip(&never).filter(|(a, b)| a != b).count();
    assert!(
        differing > 64,
        "the posed limb draws ({differing} bytes differ from a world without it)"
    );
}

#[test]
fn a_dual_quaternion_skin_posed_off_screen_stays_view_visible() {
    use regia_render::{ExtractedSkinning, SkinningOptions, extract_skinning};
    // Dual-quaternion blending rotates vertices off the joints' convex hull, so the palette bound
    // does not hold it: it passes the frustum whole (conservative), as before ADR-7255.
    let mut app = app();
    app.init_resource::<ExtractedSkinning>();
    app.add_systems(
        Render,
        extract_skinning
            .after(extract_frame)
            .before(update_view_visibility),
    );
    let dq = skinned_at(&mut app, (0.0, 0.0, -5.0), (0.0, 0.0, 10.0), [0.8, 0.8, 0.8, 1.0]);
    app.world_mut()
        .entity_mut(dq)
        .insert(SkinningOptions::dual_quaternion());
    let linear = skinned_at(&mut app, (1.0, 0.0, -5.0), (0.0, 0.0, 10.0), [0.8, 0.8, 0.8, 1.0]);
    app.update();
    assert!(
        view_visible(&app, dq),
        "a dual-quaternion skin has no palette bound: kept visible"
    );
    assert!(
        !view_visible(&app, linear),
        "the linear skin posed off screen is culled"
    );
}

/// A box a hidden mesh would shadow: a shadow-casting sun from (almost) straight above, no ambient
/// (so the shadow is not washed out), a ground slab whose top is 1 m below the eye, and a caster
/// 6 m up over the ground in view. The caster is outside the 45° view, so any difference it makes
/// to the image is its shadow.
#[test]
fn a_hidden_caster_out_of_view_casts_no_shadow() {
    let device = device();
    let build = |caster: Option<Visibility>| {
        let mut app = app();
        let direction = regia_core::math::Vec3::new(0.02, -1.0, 0.05).normalize();
        app.world_mut().spawn((
            Transform::default(),
            DirectionalLight {
                direction: direction.to_array(),
                color: [1.0; 3],
                illuminance: 2_000.0,
                cast_shadows: true,
                static_shadow: false,
            },
        ));
        app.world_mut().spawn((
            Transform {
                translation: regia_core::math::Vec3::new(0.0, -1.25, -10.0),
                scale: regia_core::math::Vec3::new(30.0, 0.5, 30.0),
                ..Transform::default()
            },
            MeshRenderer::new(cube(), material([0.8, 0.8, 0.8, 1.0])),
        ));
        if let Some(visibility) = caster {
            let entity = app
                .world_mut()
                .spawn((
                    Transform {
                        translation: regia_core::math::Vec3::new(0.0, 6.0, -6.0),
                        scale: regia_core::math::Vec3::new(3.0, 0.5, 3.0),
                        ..Transform::default()
                    },
                    MeshRenderer::new(cube(), material([0.9, 0.1, 0.1, 1.0])),
                ))
                .id();
            app.world_mut().entity_mut(entity).insert(visibility);
        }
        app
    };
    let render = |mut app: App| {
        app.update();
        let mut extracted = app.world_mut().resource_mut::<ExtractedFrame>();
        extracted.environment.ambient_lux = 0.0;
        assert!(extracted.directional_shadows, "the sun casts shadows");
        let scene = extracted.frame_scene(0, 64, 48);
        FrameRenderer::new()
            .render_offscreen(&device, &scene)
            .expect("frame")
            .pixels
    };
    let hidden = render(build(Some(Visibility::Hidden)));
    let never = render(build(None));
    let shown = render(build(Some(Visibility::Visible)));
    assert_eq!(hidden.len(), never.len());
    let differing = hidden.iter().zip(&never).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the hidden caster left {differing} bytes (its shadow) on the image"
    );
    // The control: shown, the caster darkens the ground it shadows (it is not itself in view).
    let darker = shown.iter().zip(&never).filter(|(a, b)| a < b).count();
    let lighter = shown.iter().zip(&never).filter(|(a, b)| a > b).count();
    assert!(
        darker > 64,
        "the shown caster shadows the ground ({darker} bytes darker, {lighter} lighter): the test sees it"
    );
}

/// Known issue (v0.5.1, ADR-7200, ADR-7670), pinned: GPU occlusion is decided on the device and
/// not read back, so `ViewVisibility` is frustum-only and a mesh in the frustum but fully behind a
/// wall reads visible (conservative: gameplay never skips an entity the GPU drew). This holds with
/// two-phase occlusion culling switched on, and it stays true on the v0.6 line for a world with no
/// occlusion read-back connected (ADR-7255 (2) folds a one-frame-late result in only when a renderer
/// publishes one).
#[test]
fn known_issue_a_mesh_behind_a_wall_reads_view_visible_without_an_occlusion_readback() {
    let mut app = app();
    app.insert_resource(regia_render::RenderSettings {
        two_phase_occlusion: Some(true),
        ..regia_render::RenderSettings::default()
    });
    // A wall 3 m ahead, much wider than the view; a small cube 10 m ahead, right behind it.
    let wall = app
        .world_mut()
        .spawn((
            Transform {
                translation: regia_core::math::Vec3::new(0.0, 0.0, -3.0),
                scale: regia_core::math::Vec3::new(40.0, 40.0, 0.5),
                ..Transform::default()
            },
            MeshRenderer::new(cube(), material([0.8, 0.8, 0.8, 1.0])),
        ))
        .id();
    let behind = mesh_at(&mut app, (0.0, 0.0, -10.0), [0.9, 0.1, 0.1, 1.0]);
    for _ in 0..3 {
        app.update();
    }
    assert!(
        app.world().resource::<ExtractedFrame>().two_phase_occlusion,
        "the frame asks for two-phase occlusion culling"
    );
    assert!(view_visible(&app, wall), "the wall is on screen");
    assert!(
        view_visible(&app, behind),
        "frustum-only: the mesh behind the wall reads visible (no occlusion read-back)"
    );
}
