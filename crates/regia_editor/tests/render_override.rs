// SPDX-License-Identifier: MPL-2.0
//! Per-entity render overrides in the pane (Tincal engine request F5): a tint changes only the
//! targeted entity's pixels (rigid and skinned), removing it restores the frame exactly, ghost
//! blends with what is behind, x-ray shows through an occluder, unlit flattens the shading,
//! emissive adds light; no material is cloned and steady frames create no GPU buffers.

use std::sync::Arc;

use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_editor::render::{PaneFrame, PaneRequest, render_pane};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{
    CpuMesh, DebugViewMode, JointMatrix, LabMaterial, MeshRenderer, RenderOverride, SkinJoints,
};
use regia_scene::prelude::{Transform, Visibility};
use serde_json::json;

const W: u32 = 480;
/// The frame-path counters are process-wide: the tests of this file run one at a time so one
/// test's pipeline and buffer creation does not land in another's measurement.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
const H: u32 = 320;
/// Rigid cube centre (left) and skinned cube centre (right).
const LEFT: Vec3 = Vec3::new(-1.2, 0.5, 0.0);
const RIGHT: Vec3 = Vec3::new(1.2, 0.5, 0.0);

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
    material: Arc<LabMaterial>,
    rigid: Entity,
    skinned: Entity,
}

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
    let rigid = world
        .spawn((
            Transform::from_translation(LEFT),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
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
                Arc::clone(&material),
            ),
            SkinJoints {
                palette: vec![JointMatrix::translation(RIGHT.x, RIGHT.y, RIGHT.z)],
            },
        ))
        .id();
    Scene {
        app,
        material,
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
            ids: false,
            warm_frames: 0,
            debug_view: DebugViewMode::Lit,
            reference: None,
        },
    )
    .expect("pane")
}

fn save(frame: &PaneFrame, name: &str) {
    let dir = regia_core::brand::env_var("REGIA_VIEWPORT_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-viewport-shots"));
    let _ = std::fs::create_dir_all(&dir);
    let bytes: Vec<u8> = frame
        .image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    if let Some(buffer) = image::RgbaImage::from_raw(W, H, bytes) {
        let _ = buffer.save(dir.join(name));
    }
}

fn pixel(frame: &PaneFrame, point: Vec3) -> [i32; 3] {
    let p = frame
        .view
        .project(point, Vec2::new(W as f32, H as f32))
        .expect("in front");
    let c = frame.image.pixels[p.y as usize * W as usize + p.x as usize];
    [i32::from(c.r()), i32::from(c.g()), i32::from(c.b())]
}

/// Differing pixels left and right of the image centre.
fn changed(a: &PaneFrame, b: &PaneFrame) -> (usize, usize) {
    let max = a
        .image
        .pixels
        .iter()
        .zip(&b.image.pixels)
        .map(|(pa, pb)| {
            (i32::from(pa.r()) - i32::from(pb.r()))
                .abs()
                .max((i32::from(pa.g()) - i32::from(pb.g())).abs())
                .max((i32::from(pa.b()) - i32::from(pb.b())).abs())
        })
        .max()
        .unwrap_or(0);
    if max > 0 {
        eprintln!("max channel difference {max}");
    }
    let mut out = (0, 0);
    for (i, (pa, pb)) in a.image.pixels.iter().zip(&b.image.pixels).enumerate() {
        if pa != pb {
            if (i % W as usize) < W as usize / 2 {
                out.0 += 1;
            } else {
                out.1 += 1;
            }
        }
    }
    out
}

/// A visible face centre of the cube at `centre` (lifted toward the top so it is not an edge).
fn face_toward_eye(frame: &PaneFrame, centre: Vec3) -> Vec3 {
    [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z]
        .into_iter()
        .map(|n| centre + n * 0.5 + Vec3::Y * 0.15)
        .find(|p| (*p - centre).with_y(0.0).dot(frame.view.eye - *p) > 0.0)
        .expect("a side face toward the eye")
}

#[test]
fn tint_changes_only_its_entity_and_removal_restores_the_frame() {
    let _serial = serial();
    let device = device();
    let Scene {
        mut app,
        material,
        rigid,
        skinned,
    } = scene(&device);
    let world = app.world_mut();

    let base = frame(&device, world);
    let _steady = frame(&device, world);
    let steady = frame(&device, world);
    save(&base, "render_override_base.png");
    let top_left = LEFT + Vec3::new(0.1, 0.5, 0.2);
    let top_right = RIGHT + Vec3::new(0.1, 0.5, 0.2);

    // Red tint on the rigid cube.
    world
        .entity_mut(rigid)
        .insert(RenderOverride::tinted([1.0, 0.1, 0.1, 1.0]));
    let red = frame(&device, world);
    save(&red, "render_override_tint_rigid.png");
    eprintln!(
        "draws: instanced {} per_object {}",
        red.info.draws.instanced, red.info.draws.per_object
    );
    let (left, right) = changed(&base, &red);
    assert!(left > 500, "the tinted cube changed ({left} px)");
    assert_eq!(right, 0, "the other cube is untouched");
    let [r, g, b] = pixel(&red, top_left);
    assert!(r > g + 40 && r > b + 40, "red top face: {r} {g} {b}");
    assert_eq!(pixel(&red, top_right), pixel(&base, top_right));

    // Steady and re-tinted frames create no GPU buffers beyond what a plain steady frame does.
    let _warm = frame(&device, world);
    let tinted_steady = frame(&device, world);
    world
        .entity_mut(rigid)
        .insert(RenderOverride::tinted([0.1, 1.0, 0.1, 1.0]));
    let green = frame(&device, world);
    let [r, g, _] = pixel(&green, top_left);
    assert!(g > r + 40, "re-tinted green: {r} {g}");
    eprintln!(
        "buffers: steady {} tinted steady {} re-tint {}; bind groups: steady {} tinted {} re-tint {}",
        steady.info.counters.buffers,
        tinted_steady.info.counters.buffers,
        green.info.counters.buffers,
        steady.info.counters.bind_groups,
        tinted_steady.info.counters.bind_groups,
        green.info.counters.bind_groups
    );
    assert!(tinted_steady.info.counters.buffers <= steady.info.counters.buffers);
    assert!(
        green.info.counters.buffers <= steady.info.counters.buffers,
        "a tint change is an upload, not a buffer"
    );
    assert!(green.info.counters.bind_groups <= steady.info.counters.bind_groups);
    eprintln!(
        "pipelines: steady {} tinted steady {} re-tint {}",
        steady.info.counters.pipelines, tinted_steady.info.counters.pipelines, green.info.counters.pipelines
    );
    assert_eq!(green.info.counters.pipelines, 0, "no pipeline for a tint");

    // Removing it restores the original pixels exactly.
    world.entity_mut(rigid).remove::<RenderOverride>();
    let restored = frame(&device, world);
    assert_eq!(changed(&base, &restored), (0, 0), "removal restores the frame");

    // The skinned cube (per-object path) takes a tint too, alone.
    world
        .entity_mut(skinned)
        .insert(RenderOverride::tinted([0.1, 0.1, 1.0, 1.0]));
    let blue = frame(&device, world);
    save(&blue, "render_override_tint_skinned.png");
    let (left, right) = changed(&base, &blue);
    assert_eq!(left, 0, "the rigid cube is untouched");
    assert!(right > 500, "the skinned cube changed ({right} px)");
    let [r, _, b] = pixel(&blue, top_right);
    assert!(b > r + 40, "blue skinned top: {r} {b}");
    world.entity_mut(skinned).remove::<RenderOverride>();
    assert_eq!(changed(&base, &frame(&device, world)), (0, 0));

    // No material was cloned: both entities still draw the one shared material (the `Arc`
    // count is not a measure: the pane keeps recent extracted frames alive).
    for entity in [rigid, skinned] {
        let renderer = world.get::<MeshRenderer>(entity).expect("renderer");
        assert!(Arc::ptr_eq(&renderer.material, &material));
    }
}

#[test]
fn ghost_xray_unlit_and_emissive() {
    let _serial = serial();
    let device = device();
    let Scene { mut app, rigid, .. } = scene(&device);
    let world = app.world_mut();
    let base = frame(&device, world);
    let top = LEFT + Vec3::new(0.1, 0.5, 0.2);
    let side = face_toward_eye(&base, LEFT);
    let solid = pixel(&base, top);

    // The background behind the cube's top face.
    world.entity_mut(rigid).insert(Visibility::Hidden);
    let empty = frame(&device, world);
    world.entity_mut(rigid).insert(Visibility::Inherited);
    let behind = pixel(&empty, top);

    // Ghost: between the solid cube and what is behind it.
    world.entity_mut(rigid).insert(RenderOverride::ghost(0.35));
    let ghost = frame(&device, world);
    save(&ghost, "render_override_ghost.png");
    let g = pixel(&ghost, top);
    let dist = |a: [i32; 3], b: [i32; 3]| (0..3).map(|i| (a[i] - b[i]).abs()).sum::<i32>();
    assert_ne!(g, solid, "the ghost is not the solid cube");
    assert!(
        dist(g, behind) < dist(solid, behind),
        "ghost {g:?} is closer to the background {behind:?} than the solid {solid:?}"
    );

    // X-ray: an occluder in front of the cube hides it; x-ray shows it through.
    world.entity_mut(rigid).remove::<RenderOverride>();
    let toward_eye = (base.view.eye - LEFT).normalize();
    let (vertices, indices) = regia_render::box_mesh([0.6, 0.6, 0.6]);
    let wall_material = Arc::new(LabMaterial {
        base_color: [0.05, 0.05, 0.05, 1.0],
        ..LabMaterial::default()
    });
    world.spawn((
        Transform::from_translation(LEFT + toward_eye * 2.0),
        MeshRenderer::new(Arc::new(CpuMesh { vertices, indices }), wall_material),
    ));
    let hidden = frame(&device, world);
    world.entity_mut(rigid).insert(RenderOverride {
        tint: Some([1.0, 0.0, 0.0, 1.0]),
        unlit: true,
        ..RenderOverride::default()
    });
    let still_hidden = frame(&device, world);
    assert_eq!(
        pixel(&still_hidden, LEFT),
        pixel(&hidden, LEFT),
        "without x-ray the occluder wins"
    );
    world.entity_mut(rigid).insert(RenderOverride {
        tint: Some([1.0, 0.0, 0.0, 1.0]),
        unlit: true,
        xray: true,
        ..RenderOverride::default()
    });
    let xray = frame(&device, world);
    save(&xray, "render_override_xray.png");
    let [r, g, b] = pixel(&xray, LEFT);
    assert!(
        r > 150 && g < 60 && b < 60,
        "x-ray shows the red cube through the wall: {r} {g} {b}"
    );

    // Unlit: two differently lit faces read the same flat colour. (Occluder gone: a new scene.)
    let Scene { mut app, rigid, .. } = scene(&device);
    let world = app.world_mut();
    let lit = frame(&device, world);
    assert_ne!(pixel(&lit, top), pixel(&lit, side), "lit faces differ");
    world.entity_mut(rigid).insert(RenderOverride {
        unlit: true,
        ..RenderOverride::default()
    });
    let unlit = frame(&device, world);
    save(&unlit, "render_override_unlit.png");
    let (a, b) = (pixel(&unlit, top), pixel(&unlit, side));
    assert!(dist(a, b) <= 3, "unlit faces are flat: {a:?} {b:?}");

    // Emissive adds light (blue) on top of the lit colour.
    world.entity_mut(rigid).insert(RenderOverride {
        emissive: Some([0.0, 0.0, 1.0]),
        ..RenderOverride::default()
    });
    let glow = frame(&device, world);
    let (before, after) = (pixel(&lit, side), pixel(&glow, side));
    assert!(
        after[2] > before[2] + 30,
        "emissive brightens blue: {before:?} → {after:?}"
    );
}

#[test]
fn parts_and_children_inherit_and_the_agent_method_sets_and_clears() {
    let _serial = serial();
    let device = device();
    let Scene { mut app, rigid, .. } = scene(&device);
    let world = app.world_mut();
    // An override on a parent node reaches its mesh child. (The mesh-less parent draws as an
    // editor proxy cube, so the baseline is taken with it.)
    let parent = world.spawn(Transform::IDENTITY).id();
    world.entity_mut(rigid).insert(ChildOf(parent));
    let base = frame(&device, world);
    world
        .entity_mut(parent)
        .insert(RenderOverride::tinted([1.0, 0.1, 0.1, 1.0]));
    let inherited = frame(&device, world);
    let top = LEFT + Vec3::new(0.1, 0.5, 0.2);
    let [r, g, _] = pixel(&inherited, top);
    assert!(r > g + 40, "the child takes its parent's tint: {r} {g}");
    // An identity override on the child stops it. The parent's own proxy cube keeps the
    // parent's tint (proxy cubes take their entity's override, P-160), so the reference is the
    // same tinted parent with the child outside the hierarchy: only the child may differ.
    world.entity_mut(rigid).remove::<ChildOf>();
    let untied = frame(&device, world);
    let [r, g, _] = pixel(&untied, top);
    let [base_r, base_g, _] = pixel(&base, top);
    assert!(
        (r - base_r).abs() <= 3 && (g - base_g).abs() <= 3,
        "a child outside the hierarchy keeps its own color: {r} {g} vs {base_r} {base_g}"
    );
    world
        .entity_mut(rigid)
        .insert((ChildOf(parent), RenderOverride::default()));
    assert_eq!(changed(&untied, &frame(&device, world)), (0, 0));
    world.entity_mut(rigid).remove::<RenderOverride>();
    world.entity_mut(parent).remove::<RenderOverride>();

    let methods = regia_editor::viewport_agent::methods();
    let (_, handler) = methods
        .iter()
        .find(|(schema, _)| schema.name == "editor.set_render_override")
        .expect("registered");
    let out = handler(
        world,
        &json!({ "entity": rigid.to_bits(), "tint": [0.2, 1.0, 0.2, 1.0], "alpha": 0.5 }),
    )
    .expect("set");
    assert_eq!(out["override"]["alpha"].as_f64(), Some(0.5), "{out}");
    assert_eq!(out["override"]["blends"].as_bool(), Some(true), "{out}");
    assert_eq!(
        world.get::<RenderOverride>(rigid).copied(),
        Some(RenderOverride {
            tint: Some([0.2, 1.0, 0.2, 1.0]),
            alpha: Some(0.5),
            ..RenderOverride::default()
        })
    );
    let out = handler(world, &json!({ "entity": rigid.to_bits(), "clear": true })).expect("clear");
    assert!(out["override"].is_null(), "{out}");
    assert!(world.get::<RenderOverride>(rigid).is_none());
    assert!(handler(world, &json!({ "entity": rigid.to_bits(), "tint": [1, 2] })).is_err());
    assert!(handler(world, &json!({ "tint": [1, 1, 1, 1] })).is_err());
}
