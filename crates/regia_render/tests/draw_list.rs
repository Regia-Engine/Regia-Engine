// SPDX-License-Identifier: MPL-2.0
//! Keyed draws without one entity per draw (plan row P-03 drawlist-core, ADR-8087).
//!
//! - `drawlist_9k_keyed_draws_spawn_no_entities` (the row's `9k_keyed_draws_spawn_no_entities`;
//!   a Rust name cannot start with a digit): 9,000 keyed draws in the `DrawList` resource reach
//!   the extracted frame and the frame scene, one mesh each, with no entity spawned and no
//!   command queued; moving all of them keeps every object key (the temporal history), and a sync
//!   pass that submits half of them drops the other half from the next frame.
//! - Entity meshes still extract first, the list's draws follow in key order whatever order the
//!   host inserted them in, and a draw-list layer outside the camera's mask is filtered like an
//!   entity's.
//! - Without the resource, extraction runs as before.
//! - On lavapipe, a cube drawn from the list renders byte-identical to the same cube drawn by a
//!   `MeshRenderer` entity at the same transform (fresh renderers, first frame).
//!
//! The ECS tests are CPU only; the image test needs a headless device (lavapipe).

use std::collections::HashSet;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DrawKey, DrawList, ExtractedFrame, FrameRenderer, KeyedDraw, LabVertex,
    MeshRenderer, RenderDevice, extract_frame,
};
use regia_scene::Transform;
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

/// An app with propagation in `PostUpdate`, extraction in `Render`, an empty `DrawList` and a
/// window camera at the origin looking down −Z.
fn app() -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<DrawList>();
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((
        Transform::default(),
        Camera {
            exposure_ev: Some(10.0),
            ..Camera::default()
        },
    ));
    app
}

fn entity_count(app: &mut App) -> usize {
    let world = app.world_mut();
    world.query::<Entity>().iter(world).count()
}

/// The object key the frame scene gives each extracted mesh, in extraction order.
fn object_keys(app: &App) -> Vec<u64> {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .map(|m| m.entity.to_bits() ^ m.object_key_xor)
        .collect()
}

/// Key `i` of the 9k scene: 3,000 owners with three parts each.
fn key(i: u64) -> DrawKey {
    DrawKey::pair(i / 3, i % 3)
}

/// Where draw `i` sits: a 100 × 90 grid of cubes 20 m in front of the camera.
fn place(i: u64, lift: f32) -> Mat4 {
    let (x, y) = ((i % 100) as f32, (i / 100) as f32);
    Mat4::from_translation(Vec3::new(x * 0.4 - 20.0, y * 0.4 - 18.0 + lift, -20.0))
}

#[test]
fn drawlist_9k_keyed_draws_spawn_no_entities() {
    const N: u64 = 9_000;
    let mut app = app();
    app.update();
    let entities_before = entity_count(&mut app);
    let (mesh, mat) = (cube(), material([0.7, 0.7, 0.7, 1.0]));
    {
        let mut list = app.world_mut().resource_mut::<DrawList>();
        for i in 0..N {
            list.insert(
                key(i),
                KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&mat), place(i, 0.0)),
            );
        }
        assert_eq!(list.len(), N as usize);
    }
    app.update();
    assert_eq!(
        entity_count(&mut app),
        entities_before,
        "keyed draws spawn no entity"
    );
    let first = object_keys(&app);
    assert_eq!(first.len(), N as usize, "every keyed draw is extracted once");
    let expected: HashSet<u64> = (0..N).map(|i| key(i).object_key()).collect();
    assert_eq!(expected.len(), N as usize, "9k keys give 9k object keys");
    let got: HashSet<u64> = first.iter().copied().collect();
    assert_eq!(got, expected, "each draw's object key is its key's");
    {
        let extracted = app.world().resource::<ExtractedFrame>();
        assert!(
            extracted.meshes.iter().all(|m| m.entity == Entity::PLACEHOLDER),
            "no draw names a live entity"
        );
        assert!(
            extracted
                .meshes
                .iter()
                .all(|m| Arc::ptr_eq(&m.mesh, &mesh) && Arc::ptr_eq(&m.material, &mat)),
            "the draws share the host's mesh and material (one cache entry each)"
        );
        let scene = extracted.frame_scene(0, 64, 48);
        assert_eq!(
            scene.meshes.len(),
            N as usize,
            "the frame scene draws all of them"
        );
        let scene_keys: HashSet<u64> = scene.meshes.iter().map(|m| m.object_key).collect();
        assert_eq!(scene_keys, expected);
        let stats = app.world().resource::<DrawList>().stats();
        assert_eq!((stats.items, stats.extracted, stats.hidden), (9_000, 9_000, 0));
    }

    // Every draw moves: same object keys in the same order, new matrices, still no entity.
    {
        let mut list = app.world_mut().resource_mut::<DrawList>();
        for i in 0..N {
            assert!(list.set_transform(key(i), place(i, 1.0)));
        }
    }
    app.update();
    assert_eq!(entity_count(&mut app), entities_before);
    assert_eq!(object_keys(&app), first, "moving keeps every object key");
    {
        let extracted = app.world().resource::<ExtractedFrame>();
        let wanted: Vec<[[f32; 4]; 4]> = {
            let mut by_key: Vec<(DrawKey, u64)> = (0..N).map(|i| (key(i), i)).collect();
            by_key.sort_unstable();
            by_key
                .into_iter()
                .map(|(_, i)| place(i, 1.0).to_cols_array_2d())
                .collect()
        };
        let models: Vec<[[f32; 4]; 4]> = extracted.meshes.iter().map(|m| m.model).collect();
        assert_eq!(models, wanted, "the moved matrices, in key order");
    }

    // A sync pass that submits the even draws drops the odd ones from the next frame.
    let report = app
        .world_mut()
        .resource_mut::<DrawList>()
        .sync((0..N).filter(|i| i % 2 == 0).map(|i| {
            (
                key(i),
                KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&mat), place(i, 1.0)),
            )
        }));
    assert_eq!(
        (report.inserted, report.updated, report.removed),
        (0, 4_500, 4_500)
    );
    app.update();
    assert_eq!(entity_count(&mut app), entities_before);
    let half: HashSet<u64> = object_keys(&app).into_iter().collect();
    let even: HashSet<u64> = (0..N)
        .filter(|i| i % 2 == 0)
        .map(|i| key(i).object_key())
        .collect();
    assert_eq!(half, even, "only the submitted draws remain");
}

#[test]
fn entity_meshes_extract_first_and_the_list_follows_in_key_order() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)),
            MeshRenderer::new(cube(), material([0.8, 0.2, 0.2, 1.0])),
        ))
        .id();
    let (mesh, mat) = (cube(), material([0.2, 0.8, 0.2, 1.0]));
    {
        let mut list = app.world_mut().resource_mut::<DrawList>();
        for raw in [30u64, 10, 20] {
            list.insert(
                DrawKey(raw),
                KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&mat), place(raw, 0.0)),
            );
        }
        // Hidden and off-mask draws stay keyed but do not reach the views.
        list.insert(
            DrawKey(40),
            KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&mat), place(40, 0.0)).with_visible(false),
        );
        list.insert(
            DrawKey(50),
            KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&mat), place(50, 0.0)).with_layer(7),
        );
    }
    let mut cameras = app.world_mut().query::<&mut Camera>();
    for mut camera in cameras.iter_mut(app.world_mut()) {
        camera.layer_mask = 1;
    }
    app.update();
    let keys = object_keys(&app);
    assert_eq!(
        keys,
        vec![
            entity.to_bits(),
            DrawKey(10).object_key(),
            DrawKey(20).object_key(),
            DrawKey(30).object_key(),
            DrawKey(50).object_key(),
        ],
        "the entity first, then the list in key order; the hidden draw is not extracted"
    );
    let extracted = app.world().resource::<ExtractedFrame>();
    let scene = extracted.frame_scene(0, 64, 48);
    let drawn: Vec<u64> = scene.meshes.iter().map(|m| m.object_key).collect();
    assert_eq!(
        drawn,
        keys[..4].to_vec(),
        "layer 7 is outside the camera's mask, as for an entity"
    );
}

#[test]
fn extraction_without_a_draw_list_is_unchanged() {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, extract_frame);
    app.world_mut().spawn((Transform::default(), Camera::default()));
    let entity = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)),
            MeshRenderer::new(cube(), material([0.8, 0.8, 0.8, 1.0])),
        ))
        .id();
    app.update();
    assert_eq!(object_keys(&app), vec![entity.to_bits()]);
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
fn a_draw_list_cube_renders_like_a_mesh_entity() {
    let device = device();
    let at = Vec3::new(0.6, -0.2, -4.0);
    let color = [0.9, 0.3, 0.1, 1.0];
    let mut by_entity = app();
    by_entity.world_mut().spawn((
        Transform::from_translation(at),
        MeshRenderer::new(cube(), material(color)),
    ));
    let mut by_list = app();
    by_list.world_mut().resource_mut::<DrawList>().insert(
        DrawKey(1),
        KeyedDraw::new(cube(), material(color), Mat4::from_translation(at)),
    );
    let mut empty = app();
    let entity_image = render_first_frame(&device, &mut by_entity);
    let list_image = render_first_frame(&device, &mut by_list);
    let empty_image = render_first_frame(&device, &mut empty);
    assert_eq!(entity_image.len(), list_image.len());
    let differing = entity_image
        .iter()
        .zip(&list_image)
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "the listed cube differs from the entity cube in {differing} bytes"
    );
    let drawn = list_image
        .iter()
        .zip(&empty_image)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        drawn > 64,
        "the listed cube draws ({drawn} bytes differ from an empty world)"
    );
}

/// Hand-run cost of the list's two per-frame paths for the 9k scene, printed, never asserted:
/// `cargo test -p regia_render --release --test draw_list -- --ignored --nocapture draw_list_cost_9k`.
#[test]
#[ignore = "hand-run timing: prints the cost of extraction and a sync pass, asserts nothing timed"]
fn draw_list_cost_9k() {
    const N: u64 = 9_000;
    const RUNS: usize = 101;
    let (mesh, mat) = (cube(), material([0.7, 0.7, 0.7, 1.0]));
    let mut list = DrawList::new();
    let draws = |lift: f32| {
        let (mesh, mat) = (Arc::clone(&mesh), Arc::clone(&mat));
        (0..N).map(move |i| {
            (
                key(i),
                KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&mat), place(i, lift)),
            )
        })
    };
    list.sync(draws(0.0));
    let mut out = Vec::new();
    let mut extract_us = Vec::with_capacity(RUNS);
    let mut sync_us = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        let start = std::time::Instant::now();
        list.sync(draws(run as f32 * 0.01));
        sync_us.push(start.elapsed().as_secs_f64() * 1e6);
        out.clear();
        let start = std::time::Instant::now();
        let n = list.extend_extracted(&mut out);
        extract_us.push(start.elapsed().as_secs_f64() * 1e6);
        assert_eq!(n, N as usize);
    }
    extract_us.sort_by(f64::total_cmp);
    sync_us.sort_by(f64::total_cmp);
    println!(
        "draw_list 9k: extract median {:.0} us (max {:.0}), sync pass median {:.0} us (max {:.0}), {RUNS} runs",
        extract_us[RUNS / 2],
        extract_us[RUNS - 1],
        sync_us[RUNS / 2],
        sync_us[RUNS - 1]
    );
}
