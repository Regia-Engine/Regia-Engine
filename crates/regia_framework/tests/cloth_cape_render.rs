// SPDX-License-Identifier: MPL-2.0
//! Spec 55 test 19 (the screenshot half): a caped character on the headless GPU (lavapipe).
//! A skinned cape authored standing up off the shoulders draws where the pose puts it at blend 0
//! and hangs down at blend 1, through the whole path: cooked asset, `ClothGluePlugin`, skinned
//! pose sync, body proxy, section publish, extraction and the frame. The pixels are checked at
//! points projected from the scene, so no golden image is needed; `REGIA_EVIDENCE_DIR=<dir>`
//! writes `cloth-cape-<name>.png` for the lead's golden.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::ecs::system::RunSystemOnce;
use regia_framework::cloth::{
    ClothBodyProxy, ClothGluePlugin, ClothGlueState, ClothOutput, ClothSim, ProxyShape,
};
use regia_physics::cloth::asset::{ClothSource, SimMeshInput};
use regia_physics::cloth::{ClothAsset, ClothSet};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, ExtractedFrame, FrameRenderer, LabVertex, MeshRenderer, RenderDevice,
};
use regia_scene::{GlobalTransform, Transform};

const SIZE: u32 = 160;
const CAMERA_Z: f32 = 5.0;
const CAMERA_Y: f32 = 1.0;
/// Half-width of the cape in meters.
const HALF_WIDTH: f32 = 0.5;
/// Height of the shoulder line the cape's top row is bound to.
const SHOULDER: f32 = 1.0;

/// The cape: 11 x 6 vertices, its first row across the shoulders, the rest standing up and
/// leaning back a little (so the simulation falls backward, away from the body).
fn cape_mesh() -> SimMeshInput {
    let (cols, rows) = (11u32, 6u32);
    let mut mesh = SimMeshInput::default();
    for r in 0..rows {
        for c in 0..cols {
            mesh.positions.push(Vec3::new(
                c as f32 * 0.1 - HALF_WIDTH,
                SHOULDER + r as f32 * 0.09,
                -0.3 - r as f32 * 0.04,
            ));
            mesh.uvs.push([c as f32 / 10.0, r as f32 / 5.0]);
            mesh.joints.push([0; 4]);
            mesh.weights.push([1.0, 0.0, 0.0, 0.0]);
        }
    }
    for r in 0..rows - 1 {
        for c in 0..cols - 1 {
            let i = r * cols + c;
            mesh.triangles.push([i, i + 1, i + cols]);
            mesh.triangles.push([i + 1, i + cols + 1, i + cols]);
        }
    }
    mesh
}

fn cape_asset() -> Arc<ClothAsset> {
    let entries: Vec<String> = (0..11).map(|i| format!("({i}, 0.0)")).collect();
    let text = format!(
        r#"(format_version: 1, name: "cape", skeleton: Some("hero.glb"), sim_mesh: (source: "cape.glb", node: "Cape"),
            params: (substeps: 8, damping: 1.0),
            maps: {{ "reach": Sparse(default: 4.0, entries: [{}]) }})"#,
        entries.join(", ")
    );
    let source = ClothSource::from_ron_str(&text).expect("parses");
    let asset = ClothAsset::cook(&source, &cape_mesh(), None, &[]).expect("cooks");
    // Through the cooked payload, as a game gets it.
    Arc::new(ClothAsset::from_bytes(&asset.to_bytes().expect("payload")).expect("loads"))
}

fn unlit(color: [f32; 3]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        unlit: true,
        double_sided: true,
        ..CpuMaterial::default()
    })
}

/// A box of half extents `half` around `center`, one normal per vertex is enough for an unlit
/// test body.
fn body_mesh(center: Vec3, half: Vec3) -> Arc<CpuMesh> {
    let mut vertices = Vec::new();
    for k in 0..8u32 {
        let p = center
            + Vec3::new(
                if k & 1 == 0 { -half.x } else { half.x },
                if k & 2 == 0 { -half.y } else { half.y },
                if k & 4 == 0 { -half.z } else { half.z },
            );
        vertices.push(LabVertex::from_pos_normal_uv(
            p.to_array(),
            [0.0, 0.0, 1.0],
            [0.0, 0.0],
        ));
    }
    let indices = vec![
        0, 2, 1, 1, 2, 3, 4, 5, 6, 5, 7, 6, 0, 1, 4, 1, 5, 4, 2, 6, 3, 3, 6, 7, 0, 4, 2, 2, 4, 6, 1, 3, 5, 3,
        7, 5,
    ];
    Arc::new(CpuMesh { vertices, indices })
}

struct Scene {
    app: App,
    cloth: Entity,
    joint: Entity,
}

fn scene() -> Scene {
    let mut app = App::new();
    app.add_plugins((
        regia_app::MinimalPlugins,
        regia_scene::ScenePlugin,
        AnimationPlugin,
        ClothGluePlugin,
    ));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    let skeleton = Arc::new(
        Skeleton::from_local_bind(vec!["spine".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton"),
    );
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let joint = binding.joints[0];
    // The body proxy rides on the joint, like a capsule on a spine bone.
    app.world_mut().entity_mut(joint).insert(ClothBodyProxy {
        name: "torso".into(),
        shape: ProxyShape::Capsule {
            a: Vec3::new(0.0, 0.2, 0.0),
            b: Vec3::new(0.0, SHOULDER - 0.1, 0.0),
            radius: 0.2,
        },
        friction: 0.3,
    });
    let owner = app.world_mut().spawn((Transform::IDENTITY, binding)).id();
    // The body follows the same joint as the cape.
    let body = body_mesh(
        Vec3::new(0.0, SHOULDER * 0.5, 0.0),
        Vec3::new(0.15, SHOULDER * 0.5, 0.15),
    );
    app.world_mut().spawn((
        MeshRenderer::new(body, unlit([0.5, 0.5, 0.5])),
        Transform::IDENTITY,
        ChildOf(joint),
    ));
    let section_mesh = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let section = app
        .world_mut()
        .spawn((
            MeshRenderer::new(section_mesh, unlit([0.9, 0.05, 0.05])),
            Transform::IDENTITY,
        ))
        .id();
    let mut sim = ClothSim::new(cape_asset());
    sim.skinned_mesh = Some(owner);
    sim.section = Some(section);
    sim.blend = 0.0;
    let cloth = app.world_mut().spawn((sim, Transform::IDENTITY)).id();
    let camera_at = Vec3::new(0.0, CAMERA_Y, CAMERA_Z);
    app.world_mut().spawn((
        Camera::default(),
        Transform::from_translation(camera_at),
        GlobalTransform(Affine3A::from_translation(camera_at)),
    ));
    Scene { app, cloth, joint }
}

/// Renders the scene on a fresh renderer (no history from the previous capture).
fn capture(app: &mut App, device: &RenderDevice, name: &str) -> Vec<u8> {
    let world = app.world_mut();
    world.init_resource::<ExtractedFrame>();
    world
        .run_system_once(regia_render::extract_frame)
        .expect("extract");
    let extracted = world.resource::<ExtractedFrame>().clone();
    let scene = extracted.frame_scene(0, SIZE, SIZE);
    let mut renderer = FrameRenderer::new();
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    if let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") {
        std::fs::create_dir_all(&dir).expect("evidence dir");
        let path = std::path::Path::new(&dir).join(format!("cloth-cape-{name}.png"));
        let file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create png"));
        let mut encoder = png::Encoder::new(file, SIZE, SIZE);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&pixels).expect("png data");
    }
    pixels
}

/// The pixel a world point lands on (camera on +Z looking down -Z). The default 45 degree view is
/// horizontal-plus: a square frame shows the width of a 16:9 one, so its height too.
fn pixel(p: Vec3) -> (i32, i32) {
    let half = (std::f32::consts::FRAC_PI_4 * 0.5).tan() * (16.0 / 9.0) * (CAMERA_Z - p.z);
    let x = (p.x / half + 1.0) * 0.5 * SIZE as f32;
    let y = (1.0 - (p.y - CAMERA_Y) / half) * 0.5 * SIZE as f32;
    (x as i32, y as i32)
}

/// How many cape-red pixels sit in the 9 x 9 window around a world point.
fn cape_pixels(pixels: &[u8], p: Vec3) -> usize {
    let (cx, cy) = pixel(p);
    let mut count = 0;
    for y in cy - 4..=cy + 4 {
        for x in cx - 4..=cx + 4 {
            if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
                continue;
            }
            let at = (y as usize * SIZE as usize + x as usize) * 4;
            let (r, g, b) = (pixels[at] as i32, pixels[at + 1] as i32, pixels[at + 2] as i32);
            if r > 100 && r > g * 2 && r > b * 2 {
                count += 1;
            }
        }
    }
    count
}

fn run(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

#[test]
fn a_caped_character_draws_the_pose_at_blend_zero_and_the_simulation_at_blend_one() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let Scene {
        mut app,
        cloth,
        joint,
    } = scene();
    // Where the cape's right side stands in the pose, and where it hangs when simulated.
    let up = Vec3::new(0.25, SHOULDER + 0.26, -0.42);
    let down = Vec3::new(0.3, SHOULDER - 0.3, -0.3);

    run(&mut app, 60);
    let pose = capture(&mut app, &device, "blend0");
    assert!(
        cape_pixels(&pose, up) > 40,
        "blend 0 draws the cape where the pose puts it"
    );
    assert_eq!(cape_pixels(&pose, down), 0, "nothing hangs at blend 0");

    // The published mesh is the pose too, with a new `Arc` per frame.
    let section = app
        .world()
        .get::<ClothSim>(cloth)
        .expect("sim")
        .section
        .expect("section");
    let first = app
        .world()
        .get::<MeshRenderer>(section)
        .expect("section")
        .mesh
        .clone();
    assert_eq!(first.vertices.len(), 66);
    assert_eq!(first.indices.len(), 50 * 2 * 3);
    let top = first
        .vertices
        .iter()
        .map(|v| v.position[1])
        .fold(f32::MIN, f32::max);
    assert!(
        (top - (SHOULDER + 0.45)).abs() < 1e-4,
        "the pose keeps the authored height ({top})"
    );
    run(&mut app, 1);
    let next = app
        .world()
        .get::<MeshRenderer>(section)
        .expect("section")
        .mesh
        .clone();
    assert!(!Arc::ptr_eq(&first, &next));

    app.world_mut().get_mut::<ClothSim>(cloth).expect("sim").blend = 1.0;
    run(&mut app, 300);
    let simulated = capture(&mut app, &device, "blend1");
    assert_eq!(cape_pixels(&simulated, up), 0, "blend 1 leaves the pose");
    assert!(
        cape_pixels(&simulated, down) > 40,
        "blend 1 hangs from the shoulders"
    );

    // The solver did the work: nothing dropped, the top row is on its skinned target, every
    // free particle clear of the body proxy.
    let state = app.world().get::<ClothGlueState>(cloth).expect("state");
    assert!(state.last_error.is_none());
    let set = app.world().resource::<ClothSet>();
    let solver = set.get(state.solver()).expect("solver");
    assert_eq!(solver.stats().dropped_steps, 0);
    assert_eq!(solver.colliders().len(), 1, "the joint's proxy");
    assert!(
        solver.particles()[0]
            .position
            .distance(Vec3::new(-HALF_WIDTH, SHOULDER, -0.3))
            < 1e-5
    );

    // The per-frame publish does not grow the renderer: one renderer drawing frame after frame
    // gives every retired mesh back (spec 55 § 2.2, ADR-2083).
    let mut renderer = FrameRenderer::new();
    let mut used = Vec::new();
    for _ in 0..12 {
        run(&mut app, 1);
        let world = app.world_mut();
        world
            .run_system_once(regia_render::extract_frame)
            .expect("extract");
        let extracted = world.resource::<ExtractedFrame>().clone();
        renderer.release_meshes(&extracted.released_mesh_keys);
        let scene = extracted.frame_scene(0, SIZE, SIZE);
        renderer.render_offscreen(&device, &scene).expect("frame");
        let (vertices, indices) = renderer.mesh_arena_stats();
        used.push((
            vertices.chunks,
            vertices.bytes - vertices.free_bytes,
            indices.chunks,
            indices.bytes - indices.free_bytes,
        ));
    }
    assert_eq!(used[4], used[11], "the mesh arenas hold steady under the publish");

    // Walking: the joint carries the whole character sideways, the cape comes along.
    let walked = Vec3::new(0.6, 0.0, 0.0);
    app.world_mut()
        .get_mut::<Transform>(joint)
        .expect("joint")
        .translation = walked;
    run(&mut app, 240);
    let moved = capture(&mut app, &device, "walked");
    assert!(
        cape_pixels(&moved, down + walked) > 40,
        "the cape followed the joint"
    );
    assert_eq!(
        cape_pixels(&moved, Vec3::new(-down.x, down.y, down.z)),
        0,
        "and left the place it hung"
    );
    let output = app.world().get::<ClothOutput>(cloth).expect("output");
    let pinned = output.cur[0].position;
    assert!(
        pinned.distance(Vec3::new(-HALF_WIDTH, SHOULDER, -0.3) + walked) < 1e-4,
        "{pinned}"
    );
}
