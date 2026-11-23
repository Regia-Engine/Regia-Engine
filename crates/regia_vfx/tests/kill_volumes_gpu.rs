// SPDX-License-Identifier: MPL-2.0
//! `.vfx.ron` v7 kill volumes (ADR-2800) on the GPU: the update kernel's rule against the CPU
//! reference ([`regia_vfx::probe::probe_steps`]) on a seeded stream, on a headless adapter
//! (lavapipe in the gate), the pattern of `events_gpu.rs`.

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::probe::{ProbeSettings, probe_steps};
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{PoolParticle, VfxCamera, VfxGpu, VfxProfile};

const DT: f32 = 1.0 / 64.0;

/// GPU against CPU, metres and m/s (the `events_gpu.rs` tolerance).
const TOLERANCE: f32 = 5e-3;

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 8192,
        ..VfxProfile::desktop_high()
    }
}

/// A seeded fountain through a kill volume: `volume` is the `kill_volume` value.
fn system(volume: &str) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "kv",
    system: (budget: 100000),
    emitters: [(
        name: "fountain",
        spawn: (mode: Rate(400.0), lifetime: (min: 1.5, max: 2.5)),
        init: (
            shape: Sphere(radius: (min: 0.0, max: 0.6)),
            velocity: (mode: Directional(dir: (0.0, 0.0, 1.0), spread: 35.0, speed: (min: 3.0, max: 6.0))),
            size: (min: 0.1, max: 0.1),
        ),
        update: (gravity: (0.0, -3.0, 0.0), drag: 0.1, kill_volume: Some({volume})),
        render: (mode: Billboard),
    )],
)"#
    ))
    .expect("test system parses")
}

/// Plays `def` for `steps` frames on the GPU and returns its pool and the emitter base slot.
fn gpu_pool(def: &VfxSystemDef, origin: Vec3, forward: Vec3, steps: u32) -> (Vec<PoolParticle>, u32) {
    let device = device();
    let mut gpu = VfxGpu::new(&device, profile()).expect("vfx gpu");
    let base = regia_vfx::gpu::layout::MAX_EMITTERS - def.emitters.len() as u32;
    let mut library = VfxLibrary::new();
    library.add(def.clone()).expect("valid def");
    let mut scene = VfxScene::new(library);
    scene.spawn(&def.name, origin, forward, 7).expect("spawned");
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 40.0, -60.0), Vec3::ZERO);
    for _ in 0..steps {
        scene.update(DT, &LodInput { camera_pos: origin }, gpu.capacity());
        scene.upload(&mut gpu);
        gpu.set_frame(&camera, DT, 64, 64);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        gpu.simulate(&mut encoder);
        gpu.record_stats_copy(&mut encoder);
        device.queue.submit([encoder.finish()]);
    }
    (gpu.read_particles(), base)
}

/// Every GPU particle has a CPU particle within `TOLERANCE` (nearest match), counts equal.
fn assert_pools_match(gpu: &[PoolParticle], base: u32, cpu: &[regia_vfx::probe::ProbeState]) {
    assert_eq!(gpu.len(), cpu.len(), "live particle counts differ");
    let mut unmatched: Vec<[f32; 6]> = cpu
        .iter()
        .map(|p| {
            let (a, b) = (p.position, p.velocity);
            [a.x, a.y, a.z, b.x, b.y, b.z]
        })
        .collect();
    for p in gpu {
        assert_eq!(p.emitter_slot, base);
        let row = [
            p.position[0],
            p.position[1],
            p.position[2],
            p.velocity[0],
            p.velocity[1],
            p.velocity[2],
        ];
        let distance = |o: &[f32; 6]| row.iter().zip(o).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max);
        let Some((index, d)) = unmatched
            .iter()
            .enumerate()
            .map(|(i, o)| (i, distance(o)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
        else {
            panic!("no CPU particle left for {row:?}");
        };
        assert!(d <= TOLERANCE, "gpu {row:?} is {d} from its nearest cpu particle");
        unmatched.swap_remove(index);
    }
}

fn check(volume: &str, origin: Vec3, forward: Vec3) {
    let def = system(volume);
    let none = system("(shape: Sphere(radius: 1000.0), kill: Outside)");
    let settings = ProbeSettings {
        dt: DT,
        seed: 7,
        ground: None,
        ..ProbeSettings::default()
    };
    for steps in [60u32, 110] {
        let (cpu, _) = probe_steps(&def, origin, forward, settings, steps);
        let (free, _) = probe_steps(&none, origin, forward, settings, steps);
        assert!(
            cpu.len() + 20 < free.len(),
            "the volume must kill something: {} vs {} without it",
            cpu.len(),
            free.len()
        );
        let (gpu, base) = gpu_pool(&def, origin, forward, steps);
        assert_pools_match(&gpu, base, &cpu);
    }
}

#[test]
fn gpu_and_cpu_agree_on_a_box_that_kills_inside() {
    check(
        "(shape: Box(half_extents: (3.0, 3.0, 0.3)), center: (0.0, 0.0, 1.2))",
        Vec3::new(1.0, 0.5, -2.0),
        Vec3::new(0.0, 1.0, 0.2),
    );
}

#[test]
fn gpu_and_cpu_agree_on_a_sphere_that_kills_outside() {
    check(
        "(shape: Sphere(radius: 1.6), center: (0.0, 0.0, 0.5), kill: Outside)",
        Vec3::new(-3.0, 2.0, 1.0),
        Vec3::Y,
    );
}

/// `.vfx.ron` v7 skinned-mesh spawns: positions sampled on the CPU from the current palette reach
/// the GPU pool through the emit stage unchanged (no forces, zero speed), capped per frame.
#[test]
fn skinned_spawns_reach_the_gpu_pool_through_the_emit_stage() {
    use regia_core::math::{Mat4, Quat};
    let def = VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "skin",
    emitters: [(
        name: "motes",
        spawn: (mode: Rate(6000.0), lifetime: (min: 5.0, max: 5.0)),
        init: (shape: SkinnedMesh(mesh: "arm", max_per_frame: 40), velocity: (mode: Normal(speed: (min: 0.0, max: 0.0))), size: (min: 0.1, max: 0.1)),
        render: (mode: Billboard),
    )],
)"#
    ))
    .expect("skinned effect parses");
    let mesh = || {
        regia_vfx::mesh_shape::VfxMesh::from_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[0, 1, 2],
        )
        .and_then(|m| m.with_skin(&[[0, 0, 0, 0]; 3], &[[1.0, 0.0, 0.0, 0.0]; 3]))
        .expect("skinned triangle")
    };
    let palette = [Mat4::from_rotation_translation(
        Quat::from_rotation_z(0.7),
        Vec3::new(0.0, 2.0, 0.0),
    )];
    let scene_with = || {
        let mut library = VfxLibrary::new();
        library.add_mesh("arm", mesh());
        library.add(def.clone()).expect("valid");
        let mut scene = VfxScene::new(library);
        let id = scene
            .spawn("skin", Vec3::new(1.0, 0.0, 0.0), Vec3::Z, 5)
            .expect("spawned");
        assert!(scene.set_skin_palette(id, &palette));
        (scene, id)
    };
    // The CPU's jobs for two frames.
    let (mut cpu, id) = scene_with();
    let lod = LodInput {
        camera_pos: Vec3::ZERO,
    };
    let mut expected = Vec::new();
    for _ in 0..2 {
        cpu.update(DT, &lod, 100_000);
        if let Some(instance) = cpu.instance_mut(id) {
            instance.drain_jobs(&mut expected);
        }
    }
    assert_eq!(expected.len(), 80, "40 per frame");
    // The GPU pool after the same two frames.
    let device = device();
    let mut gpu = VfxGpu::new(&device, profile()).expect("vfx gpu");
    let (mut scene, _) = scene_with();
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 4.0, -8.0), Vec3::ZERO);
    for _ in 0..2 {
        scene.update(DT, &lod, gpu.capacity());
        scene.upload(&mut gpu);
        gpu.set_frame(&camera, DT, 64, 64);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        gpu.simulate(&mut encoder);
        device.queue.submit([encoder.finish()]);
    }
    let pool = gpu.read_particles();
    assert_eq!(pool.len(), expected.len());
    let mut remaining: Vec<[f32; 3]> = expected.iter().map(|j| j.position).collect();
    for p in &pool {
        let at = remaining
            .iter()
            .position(|q| q.iter().zip(&p.position).all(|(a, b)| (a - b).abs() < 1e-5))
            .unwrap_or_else(|| panic!("gpu particle {:?} is not a CPU spawn", p.position));
        remaining.swap_remove(at);
    }
}
