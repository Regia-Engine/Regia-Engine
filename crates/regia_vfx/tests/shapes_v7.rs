// SPDX-License-Identifier: MPL-2.0
//! `.vfx.ron` v7 (ADR-2800) on the CPU: mesh-surface and skinned-mesh shapes, the along-normal
//! velocity, mesh errors, kill volumes in the CPU reference, and ribbon strips.

use std::sync::Arc;

use regia_core::math::{Mat4, Quat, Vec3};
use regia_vfx::format::{RenderMode, VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::mesh_shape::{VfxMesh, VfxMeshError};
use regia_vfx::probe::{ProbeSettings, probe_steps};
use regia_vfx::ribbon::strip_vertices;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::{LodInput, SpawnJob, VfxInstance};

const DT: f32 = 1.0 / 60.0;
const LOD: LodInput = LodInput {
    camera_pos: Vec3::new(0.0, 5.0, -20.0),
};

/// One emitter with `shape`, `velocity`, `spawn` and `extra` render/update text.
fn effect(shape: &str, velocity: &str, spawn: &str, update: &str, render: &str) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "t",
    system: (budget: 200000),
    emitters: [(
        name: "e",
        spawn: ({spawn}),
        init: (shape: {shape}, velocity: (mode: {velocity}), size: (min: 0.2, max: 0.2)),
        update: ({update}),
        render: ({render}),
    )],
)"#
    ))
    .expect("test effect parses")
}

/// Two triangles in the z = 0 plane: area 3 at x in [0, 3], area 1 at x in [10, 11].
fn two_triangles() -> VfxMesh {
    VfxMesh::from_triangles(
        &[
            [0.0, 0.0, 0.0],
            [3.0, 0.0, 0.0],
            [0.0, 2.0, 0.0],
            [10.0, 0.0, 0.0],
            [11.0, 0.0, 0.0],
            [10.0, 2.0, 0.0],
        ],
        &[0, 1, 2, 3, 4, 5],
    )
    .expect("valid mesh")
}

/// Inside a triangle in the z = 0 plane (with a tolerance).
fn on_triangle(p: Vec3, [a, b, c]: [Vec3; 3]) -> bool {
    let v0 = b - a;
    let v1 = c - a;
    let v2 = p - a;
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denom = d00 * d11 - d01 * d01;
    let v = (d11 * d20 - d01 * d21) / denom;
    let w = (d00 * d21 - d01 * d20) / denom;
    let u = 1.0 - v - w;
    let plane = (p - a).dot(v0.cross(v1).normalize()).abs();
    plane < 1e-4 && u >= -1e-4 && v >= -1e-4 && w >= -1e-4
}

/// The instance-local offset of a world point (the inverse of `origin + right x + up y + fwd z`).
fn local(instance: &VfxInstance, world: [f32; 3]) -> Vec3 {
    let (right, up, forward) = instance.axes();
    let d = Vec3::from_array(world) - instance.origin();
    Vec3::new(d.dot(right), d.dot(up), d.dot(forward))
}

fn spawn_jobs(def: &VfxSystemDef, instance: &mut VfxInstance, frames: u32) -> Vec<SpawnJob> {
    let mut all = Vec::new();
    for _ in 0..frames {
        instance.schedule(def, DT, &LOD, u32::MAX);
        instance.drain_jobs(&mut all);
    }
    all
}

#[test]
fn surface_sampling_follows_area_three_to_one_and_stays_on_the_mesh() {
    let def = effect(
        r#"MeshSurface(mesh: "two")"#,
        "Radial(speed: (min: 0.0, max: 0.0))",
        "mode: Burst(100000), lifetime: (min: 1.0, max: 1.0)",
        "",
        "mode: Billboard",
    );
    let mesh = Arc::new(two_triangles());
    let triangles = [mesh.triangle(0).expect("t0"), mesh.triangle(1).expect("t1")];
    let mut instance = VfxInstance::new(0, 5, Vec3::new(2.0, -1.0, 4.0), Vec3::new(0.3, 0.2, 1.0));
    instance.bind_mesh(0, Some(mesh));
    let jobs = spawn_jobs(&def, &mut instance, 1);
    assert_eq!(jobs.len(), 100_000);
    let mut big = 0u32;
    for job in &jobs {
        let p = local(&instance, job.position);
        let first = on_triangle(p, triangles[0]);
        assert!(first || on_triangle(p, triangles[1]), "{p} is not on the mesh");
        big += u32::from(first);
    }
    let ratio = f64::from(big) / f64::from(100_000 - big);
    assert!(
        (ratio - 3.0).abs() / 3.0 < 0.02,
        "area ratio 3:1, sampled {ratio}"
    );
}

#[test]
fn normal_velocity_runs_along_the_face_normal_in_world_space() {
    let def = effect(
        r#"MeshSurface(mesh: "two")"#,
        "Normal(speed: (min: 2.0, max: 2.0))",
        "mode: Burst(500), lifetime: (min: 1.0, max: 1.0)",
        "",
        "mode: Billboard",
    );
    let forward = Vec3::new(1.0, 1.0, 0.0).normalize();
    let mut instance = VfxInstance::new(0, 9, Vec3::ZERO, forward);
    instance.bind_mesh(0, Some(Arc::new(two_triangles())));
    let (_, _, fwd) = instance.axes();
    for job in spawn_jobs(&def, &mut instance, 1) {
        let v = Vec3::from_array(job.velocity);
        // The face normal is local +Z: the instance's forward, at 2 m/s.
        assert!((v - fwd * 2.0).length() < 1e-4, "{v} vs {}", fwd * 2.0);
    }
}

#[test]
fn a_skinned_sample_follows_a_rotated_joint() {
    let def = effect(
        r#"SkinnedMesh(mesh: "arm", max_per_frame: 50)"#,
        "Radial(speed: (min: 0.0, max: 0.0))",
        "mode: Rate(6000.0), lifetime: (min: 1.0, max: 1.0)",
        "",
        "mode: Billboard",
    );
    // One triangle on joint 1 (the forearm), one on joint 0 (the root).
    let mesh = two_triangles()
        .with_skin(
            &[
                [1, 0, 0, 0],
                [1, 0, 0, 0],
                [1, 0, 0, 0],
                [0, 0, 0, 0],
                [0, 0, 0, 0],
                [0, 0, 0, 0],
            ],
            &[[1.0, 0.0, 0.0, 0.0]; 6],
        )
        .expect("skin");
    let rest = [mesh.triangle(0).expect("t0"), mesh.triangle(1).expect("t1")];
    let mut library = VfxLibrary::new();
    library.add_mesh("arm", mesh);
    let name = def.name.clone();
    library.add(def).expect("valid");
    let mut scene = VfxScene::new(library);
    let id = scene.spawn(&name, Vec3::ZERO, Vec3::Z, 3).expect("spawned");
    // Joint 1 turns 90 degrees about Y.
    let turn = Mat4::from_quat(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
    assert!(scene.set_skin_palette(id, &[Mat4::IDENTITY, turn]));
    let rotated = rest[0].map(|p| turn.transform_point3(p));
    let mut on_turned = 0;
    let mut on_root = 0;
    for _ in 0..4 {
        scene.update(DT, &LOD, 1_000_000);
        let instance = scene.instance_mut(id).expect("live");
        let mut jobs = Vec::new();
        instance.drain_jobs(&mut jobs);
        assert!(jobs.len() <= 50, "capped per frame: {}", jobs.len());
        for job in &jobs {
            let p = local(instance, job.position);
            if on_triangle(p, rotated) {
                on_turned += 1;
            } else {
                assert!(
                    on_triangle(p, rest[1]),
                    "{p} is on neither the turned nor the root triangle"
                );
                on_root += 1;
            }
        }
    }
    assert_eq!(on_turned + on_root, 200, "50 per frame for 4 frames");
    assert!(
        on_turned > on_root,
        "the bigger triangle (area 3) wins: {on_turned} vs {on_root}"
    );
}

#[test]
fn a_missing_empty_or_flat_mesh_spawns_nothing_and_reports_why() {
    let def = effect(
        r#"MeshSurface(mesh: "nowhere")"#,
        "Radial(speed: (min: 1.0, max: 1.0))",
        "mode: Rate(600.0), lifetime: (min: 1.0, max: 1.0)",
        "",
        "mode: Billboard",
    );
    let mut library = VfxLibrary::new();
    let err = library
        .shape_mesh(&def.emitters[0].init.shape)
        .expect_err("not registered");
    assert_eq!(err, VfxMeshError::Missing("nowhere".into()));
    assert_eq!(err.code(), "missing_mesh");
    // An empty and a zero-area mesh asset are refused and leave nothing registered.
    let empty = regia_asset::CpuMesh::default();
    assert_eq!(library.add_cpu_mesh("nowhere", &empty), Err(VfxMeshError::Empty));
    let flat = regia_asset::CpuMesh {
        positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]],
        indices: vec![0, 1, 2],
        ..Default::default()
    };
    let refused = library.add_cpu_mesh("nowhere", &flat).expect_err("flat");
    assert_eq!(
        (refused.clone(), refused.code()),
        (VfxMeshError::ZeroArea, "bad_mesh")
    );
    assert!(library.mesh("nowhere").is_none());
    let name = def.name.clone();
    library.add(def).expect("the effect itself is valid");
    let mut scene = VfxScene::new(library);
    let id = scene.spawn(&name, Vec3::ZERO, Vec3::Z, 1).expect("spawned");
    for _ in 0..30 {
        scene.update(DT, &LOD, 100_000);
        assert_eq!(scene.last_scheduled(), 0);
    }
    let instance = scene.instance(id).expect("still live");
    assert_eq!(instance.live_particles(), 0);
    assert!(
        instance.mesh_misses(0) >= 290,
        "{} refused spawns",
        instance.mesh_misses(0)
    );
}

#[test]
fn the_cpu_reference_kills_inside_a_box_and_outside_a_sphere() {
    // Particles rise through a box above the origin: none survive above its top.
    let boxed = effect(
        "Point",
        "Directional(dir: (0.0, 0.0, 1.0), speed: (min: 4.0, max: 4.0))",
        "mode: Rate(120.0), lifetime: (min: 3.0, max: 3.0)",
        "kill_volume: Some((shape: Box(half_extents: (1.0, 1.0, 0.5)), center: (0.0, 0.0, 3.0)))",
        "mode: Billboard",
    );
    let settings = ProbeSettings {
        ground: None,
        ..ProbeSettings::default()
    };
    let (states, _) = probe_steps(&boxed, Vec3::ZERO, Vec3::Y, settings, 120);
    assert!(!states.is_empty());
    for s in &states {
        assert!(
            s.position.y < 2.5,
            "a particle went through the box: {}",
            s.position
        );
    }
    // Outside a 2 m sphere: everything that left it is dead.
    let sphere = effect(
        "Sphere(radius: (min: 0.0, max: 1.0))",
        "Radial(speed: (min: 1.0, max: 3.0))",
        "mode: Rate(300.0), lifetime: (min: 3.0, max: 3.0)",
        "kill_volume: Some((shape: Sphere(radius: 2.0), kill: Outside))",
        "mode: Billboard",
    );
    let origin = Vec3::new(5.0, 1.0, -2.0);
    let (states, _) = probe_steps(&sphere, origin, Vec3::Z, settings, 120);
    assert!(!states.is_empty());
    for s in &states {
        assert!(
            s.position.distance(origin) <= 2.0 + 1e-4,
            "{} escaped",
            s.position
        );
    }
}

/// A ribbon emitter with `max_points`, Burst `n` once.
fn ribbon(n: u32, max_points: u32, link: &str) -> VfxSystemDef {
    effect(
        "Sphere(radius: (min: 0.0, max: 3.0))",
        "Radial(speed: (min: 0.0, max: 0.5))",
        &format!("mode: Burst({n}), lifetime: (min: 10.0, max: 10.0)"),
        "",
        &format!(
            "mode: Ribbon, blend: Alpha, ribbon: (link: {link}, max_points: {max_points}, \
             width: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.2)], texture: Tile(0.5))"
        ),
    )
}

#[test]
fn a_ribbon_draws_six_vertices_per_segment_and_respects_its_point_cap() {
    let def = ribbon(40, 16, "Emitter");
    assert_eq!(def.emitters[0].render.mode, RenderMode::Ribbon);
    let mut library = VfxLibrary::new();
    library.add(def).expect("valid");
    let mut scene = VfxScene::new(library);
    scene.spawn("t", Vec3::ZERO, Vec3::Z, 2).expect("spawned");
    scene.update(DT, &LOD, 100_000);
    let stats = scene.ribbon_stats();
    assert_eq!(stats.points, 16, "the cap keeps the newest 16 of 40");
    assert_eq!(stats.strips, 1);
    assert_eq!(scene.ribbon_vertices().len(), strip_vertices(16));
    assert_eq!(stats.vertices as usize, 6 * 15);
    // Spawn groups: two instances' bursts make two strips, each its own group.
    let mut library = VfxLibrary::new();
    library.add(ribbon(10, 64, "SpawnGroup")).expect("valid");
    let mut scene = VfxScene::new(library);
    scene.spawn("t", Vec3::ZERO, Vec3::Z, 2).expect("spawned");
    scene.spawn("t", Vec3::X, Vec3::Z, 3).expect("spawned");
    scene.update(DT, &LOD, 100_000);
    assert_eq!(scene.ribbon_stats().strips, 2);
    assert_eq!(scene.ribbon_vertices().len(), 2 * strip_vertices(10));
    // The GPU never sees a ribbon particle.
    assert!(
        scene
            .ribbon_vertices()
            .iter()
            .all(|v| v.color[3] >= 0.0 && v.position.iter().all(|c| c.is_finite()))
    );
}

#[test]
fn ribbons_stop_at_the_trail_vertex_cap_without_a_panic() {
    let cap = regia_vfx::gpu::layout::MAX_TRAIL_VERTICES as usize;
    let per_strip = strip_vertices(1024);
    let instances = cap / per_strip + 2;
    let mut library = VfxLibrary::new();
    library.add(ribbon(1024, 1024, "Emitter")).expect("valid");
    let mut scene = VfxScene::new(library);
    for i in 0..instances {
        scene
            .spawn("t", Vec3::new(i as f32, 0.0, 0.0), Vec3::Z, i as u64)
            .expect("spawned");
    }
    for _ in 0..3 {
        scene.update(DT, &LOD, 10_000_000);
        let stats = scene.ribbon_stats();
        assert!(scene.ribbon_vertices().len() <= cap);
        assert_eq!(scene.ribbon_vertices().len(), (cap / per_strip) * per_strip);
        assert_eq!(stats.strips as usize, instances);
        assert_eq!(
            stats.dropped_vertices as usize,
            (instances - cap / per_strip) * per_strip
        );
    }
}

/// Ribbons are built under significance caps exactly as without them (rc1 review finding 2: the
/// ranked schedule never rebuilt the strips, so a capped scene drew no trails, or the last
/// uncapped frame's trails forever when caps came on mid-play).
#[test]
fn ribbons_build_under_significance_caps_as_without() {
    use regia_vfx::significance::SignificanceCaps;
    let caps = SignificanceCaps {
        max_instances: Some(8),
        ..Default::default()
    };
    let run = |capped: bool| {
        let mut library = VfxLibrary::new();
        library.add(ribbon(40, 16, "Emitter")).expect("valid");
        let mut scene = VfxScene::new(library);
        if capped {
            scene.significance_mut().set_caps(caps.clone());
        }
        scene.spawn("t", Vec3::ZERO, Vec3::Z, 2).expect("spawned");
        let mut frames = Vec::new();
        for _ in 0..4 {
            scene.update(DT, &LOD, 100_000);
            frames.push((
                scene.ribbon_stats(),
                bytemuck::cast_slice::<_, u8>(scene.ribbon_vertices()).to_vec(),
            ));
        }
        frames
    };
    let plain = run(false);
    let capped = run(true);
    assert_eq!(plain[0].0.strips, 1, "the uncapped control draws its ribbon");
    for (frame, (p, c)) in plain.iter().zip(&capped).enumerate() {
        assert_eq!(p.0, c.0, "frame {frame}: ribbon stats");
        assert_eq!(p.1.len(), c.1.len(), "frame {frame}: vertex bytes");
        assert!(
            p.1 == c.1,
            "frame {frame}: the capped strip matches the uncapped one"
        );
    }

    // Caps switched on mid-play: the strips keep following the particles, and once the instance
    // is gone they are gone too (not the last uncapped frame's vertices, frozen).
    let mut library = VfxLibrary::new();
    library.add(ribbon(40, 16, "Emitter")).expect("valid");
    let mut scene = VfxScene::new(library);
    scene.spawn("t", Vec3::ZERO, Vec3::Z, 2).expect("spawned");
    scene.update(DT, &LOD, 100_000);
    let before = bytemuck::cast_slice::<_, u8>(scene.ribbon_vertices()).to_vec();
    assert!(!before.is_empty());
    scene.significance_mut().set_caps(caps);
    scene.update(DT, &LOD, 100_000);
    assert_eq!(scene.ribbon_stats().strips, 1);
    assert!(
        bytemuck::cast_slice::<_, u8>(scene.ribbon_vertices()) != before.as_slice(),
        "the strip moved with its particles"
    );
    scene.stop_all();
    for _ in 0..(12.0 / DT) as usize {
        scene.update(DT, &LOD, 100_000);
    }
    assert_eq!(scene.live_instances(), 0, "the ribbon instance retired");
    assert!(
        scene.ribbon_vertices().is_empty(),
        "no frozen trail after the instance is gone"
    );
    assert_eq!(scene.ribbon_stats().strips, 0);
}
