// SPDX-License-Identifier: MPL-2.0
//! Surface picking through the pane (Tincal engine request F7): pixel → id buffer entity →
//! triangle, hit point and normal, for a mesh under a rotated parent node (node transforms) and
//! a CPU-skinned mesh; background and bind-pose pixels miss; the agent method agrees.

use std::sync::Arc;

use regia_core::math::{Mat4, Quat, Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_editor::picking::FaceCull;
use regia_editor::render::{PaneFrame, PaneRequest, render_pane};
use regia_editor::viewport::{EditorCamera, pick_hit_in_frame};
use regia_editor::views::ViewKind;
use regia_render::{CpuMesh, DebugViewMode, JointMatrix, LabMaterial, MeshRenderer, SkinJoints};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

const W: u32 = 480;
const H: u32 = 320;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

fn camera() -> EditorCamera {
    EditorCamera {
        target: Vec3::new(0.0, 0.5, 0.0),
        distance: 6.0,
        yaw: 0.4,
        pitch: 0.35,
        ..EditorCamera::default()
    }
}

struct Scene {
    app: regia_app::App,
    /// Rotated, moved parent node (no mesh).
    parent: Entity,
    /// The rigid cube: a child of `parent`, lifted 0.5 m in the parent's space.
    rigid: Entity,
    /// A skinned cube drawn at (1.2, 0.5, 0) by its palette; its bind pose sits at the origin.
    skinned: Entity,
}

/// The parent's turn about Y.
const PARENT_YAW: f32 = 0.5;

fn scene(device: &regia_render::RenderDevice) -> Scene {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(camera());
    app.world_mut().insert_resource(device.clone());
    let world = app.world_mut();
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let material = Arc::new(LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        ..LabMaterial::default()
    });
    let parent = world
        .spawn(Transform {
            translation: Vec3::new(-1.2, 0.0, 0.0),
            rotation: Quat::from_rotation_y(PARENT_YAW),
            ..Transform::IDENTITY
        })
        .id();
    let rigid = world
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.5, 0.0)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            ChildOf(parent),
        ))
        .id();
    let mut skinned_vertices = mesh.vertices.clone();
    for v in &mut skinned_vertices {
        v.joints = [0; 4];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    let skinned = world
        .spawn((
            Transform::IDENTITY,
            MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: skinned_vertices,
                    indices: mesh.indices.clone(),
                }),
                material,
            ),
            SkinJoints {
                palette: vec![JointMatrix::translation(1.2, 0.5, 0.0)],
            },
        ))
        .id();
    Scene {
        app,
        parent,
        rigid,
        skinned,
    }
}

fn frame(device: &regia_render::RenderDevice, world: &mut World) -> PaneFrame {
    render_pane(
        device,
        world,
        &PaneRequest {
            pane: 0,
            kind: ViewKind::Perspective,
            camera: camera(),
            width: W,
            height: H,
            ids: true,
            warm_frames: 0,
            debug_view: DebugViewMode::Lit,
            reference: None,
        },
    )
    .expect("pane")
}

/// The pixel a world point lands on.
fn pixel_of(frame: &PaneFrame, point: Vec3) -> Vec2 {
    let p = frame
        .view
        .project(point, Vec2::new(W as f32, H as f32))
        .expect("in front");
    Vec2::new(p.x.floor(), p.y.floor())
}

/// World size of one pixel at `point` (the hit lands within the pixel's footprint).
fn pixel_size(frame: &PaneFrame, point: Vec3) -> f32 {
    2.0 * (point - frame.view.eye).length() * (frame.view.fov_y * 0.5).tan() / H as f32
}

fn close(a: Vec3, b: Vec3, tolerance: f32) -> bool {
    (a - b).length() <= tolerance
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn pixel_to_entity_to_triangle_point_and_normal() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let Scene {
        mut app,
        parent,
        rigid,
        skinned,
    } = scene(&device);
    let world = app.world_mut();
    let frame = frame(&device, world);
    assert!(frame.ids.is_some(), "id buffer rendered");
    let parent_model =
        Mat4::from_rotation_translation(Quat::from_rotation_y(PARENT_YAW), Vec3::new(-1.2, 0.0, 0.0));
    let rigid_model = parent_model * Mat4::from_translation(Vec3::new(0.0, 0.5, 0.0));

    // The rigid cube's top face, a little off centre (away from the diagonal edge).
    let top = rigid_model.transform_point3(Vec3::new(0.1, 0.5, 0.2));
    let px = pixel_of(&frame, top);
    let hit = pick_hit_in_frame(world, &frame, px, FaceCull::None).expect("hit on the rigid cube");
    assert_eq!(hit.entity, rigid);
    assert_eq!(hit.owner, rigid);
    let tol = 2.0 * pixel_size(&frame, top);
    assert!(
        (hit.position.y - 1.0).abs() < 1e-3,
        "on the top face plane: {:?}",
        hit.position
    );
    assert!(
        close(hit.position, top, tol),
        "{:?} vs {top:?} (±{tol})",
        hit.position
    );
    assert!(close(hit.normal, Vec3::Y, 1e-3), "{:?}", hit.normal);
    assert!(close(hit.smooth_normal, Vec3::Y, 1e-3));
    assert!(hit.front_face);
    assert!((hit.barycentric.element_sum() - 1.0).abs() < 1e-5);
    let mesh = Arc::clone(&world.get::<MeshRenderer>(rigid).expect("mesh").mesh);
    let tri = hit.triangle as usize;
    assert_eq!(
        hit.indices,
        [
            mesh.indices[3 * tri],
            mesh.indices[3 * tri + 1],
            mesh.indices[3 * tri + 2]
        ]
    );
    let corner = |i: u32| rigid_model.transform_point3(Vec3::from_array(mesh.vertices[i as usize].position));
    let rebuilt = hit.barycentric.x * corner(hit.indices[0])
        + hit.barycentric.y * corner(hit.indices[1])
        + hit.barycentric.z * corner(hit.indices[2]);
    assert!(close(rebuilt, hit.position, 1e-4));
    // The same through the id buffer directly.
    let ids = frame.ids.as_ref().expect("ids");
    let direct = ids
        .hit(world, &frame.view, px.x as u32, px.y as u32)
        .expect("IdBuffer::hit");
    assert_eq!(direct.entity, rigid);
    assert!(close(direct.position, hit.position, 1e-5));

    // A side face of the rotated cube: its normal turns with the parent node.
    let eye = frame.view.eye;
    let side = [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z]
        .into_iter()
        .map(|n| {
            (
                n,
                rigid_model.transform_point3(n * 0.5 + Vec3::new(0.0, 0.1, 0.0) + n.cross(Vec3::Y) * 0.15),
            )
        })
        .find(|(n, p)| parent_model.transform_vector3(*n).dot(eye - *p) > 0.5)
        .expect("a side face toward the eye");
    let hit = pick_hit_in_frame(world, &frame, pixel_of(&frame, side.1), FaceCull::None).expect("side hit");
    assert_eq!(hit.entity, rigid);
    let side_normal = parent_model.transform_vector3(side.0);
    assert!(
        close(hit.normal, side_normal, 1e-3),
        "{:?} vs {side_normal:?}",
        hit.normal
    );

    // The skinned cube, where its palette draws it.
    let skinned_top = Vec3::new(1.2 + 0.1, 1.0, 0.2);
    let hit =
        pick_hit_in_frame(world, &frame, pixel_of(&frame, skinned_top), FaceCull::None).expect("skinned hit");
    assert_eq!(hit.entity, skinned);
    assert!(close(hit.normal, Vec3::Y, 1e-3));
    assert!(close(
        hit.position,
        skinned_top,
        2.0 * pixel_size(&frame, skinned_top)
    ));

    // Where the skinned cube's bind pose would be (the id pass draws skinned meshes in bind
    // pose), nothing is drawn: no hit.
    let bind_pose = Vec3::new(0.0, 0.0, 0.0);
    let px = pixel_of(&frame, bind_pose + Vec3::new(0.05, 0.4, 0.05));
    if let Some(hit) = pick_hit_in_frame(world, &frame, px, FaceCull::None) {
        assert_ne!(
            hit.entity, skinned,
            "the skinned mesh is not where its bind pose is"
        );
    }

    // Background (top-left corner: sky).
    assert!(pick_hit_in_frame(world, &frame, Vec2::new(1.0, 1.0), FaceCull::None).is_none());
    let _ = parent;
}

#[test]
fn back_face_culling_and_the_agent_method() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let Scene { mut app, rigid, .. } = scene(&device);
    let world = app.world_mut();
    let frame = frame(&device, world);
    let parent_model =
        Mat4::from_rotation_translation(Quat::from_rotation_y(PARENT_YAW), Vec3::new(-1.2, 0.0, 0.0));
    let top = parent_model.transform_point3(Vec3::new(0.1, 1.0, 0.2));
    let px = pixel_of(&frame, top);
    // Front faces only still finds the top face (it faces the eye).
    let front = pick_hit_in_frame(world, &frame, px, FaceCull::Back).expect("front face");
    assert!(front.front_face);

    let methods = regia_editor::viewport_agent::methods();
    let call = |world: &mut World, name: &str, params: Value| -> Value {
        let (_, handler) = methods
            .iter()
            .find(|(schema, _)| schema.name == name)
            .unwrap_or_else(|| panic!("{name} is registered"));
        handler(world, &params).unwrap_or_else(|err| panic!("{name}: {err:?}"))
    };
    let out = call(
        world,
        "editor.pick_hit",
        json!({ "x": px.x, "y": px.y, "width": W, "height": H }),
    );
    assert_eq!(out["entity"].as_u64(), Some(rigid.to_bits()), "{out}");
    let position: Vec<f64> = serde_json::from_value(out["position"].clone()).expect("position");
    assert!((position[1] - 1.0).abs() < 1e-3, "{out}");
    let normal: Vec<f64> = serde_json::from_value(out["normal"].clone()).expect("normal");
    assert!((normal[1] - 1.0).abs() < 1e-3, "{out}");
    assert!(
        out["triangle"].is_u64() && out["front_face"].as_bool() == Some(true),
        "{out}"
    );
    let miss = call(
        world,
        "editor.pick_hit",
        json!({ "x": 1, "y": 1, "width": W, "height": H }),
    );
    assert!(miss["entity"].is_null(), "{miss}");
    let (_, handler) = methods
        .iter()
        .find(|(schema, _)| schema.name == "editor.pick_hit")
        .expect("registered");
    assert!(handler(world, &json!({ "x": 1, "y": 1, "cull": "sideways" })).is_err());
}
