// SPDX-License-Identifier: MPL-2.0
//! GPU particle determinism, alpha-sort order and pool budget (ADR-545). Headless adapter:
//! lavapipe in the gate.
//!
//! - The bitonic alpha sort is a total order on `(key, slot)`: any input order of the same
//!   pairs sorts to the same list, ties included, and a particle with the largest key stays
//!   ahead of the padding.
//! - The emit kernel places spawn job `j` at `(ring cursor + j) % capacity`: the pool layout
//!   is a function of the spawn jobs, and a frame with more jobs than slots keeps its last
//!   `capacity` jobs and counts the rest as dropped.
//! - The mesh list sorts back to front in its own region and leaves the alpha list alone.
//! - Two contexts fed the same scene simulate to bit-identical pools and sorted lists, and
//!   the CPU budgets keep the ring from overwriting a live particle.

use regia_core::math::Vec3;
use regia_render::RenderDevice;

use super::{EmitterGpu, STALE_UPLOADS, SortAlgorithm, VfxCamera, VfxGpu, VfxProfile};
use crate::format::{
    BlendMode, EmitterDef, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef, SpawnMode, SpawnModule,
    SystemModule, UpdateModule, VelocityDef, VelocityMode, VfxSystemDef,
};
use crate::scene::{VfxLibrary, VfxScene};
use crate::sim::{LodInput, SpawnJob};

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Reads `bytes` of `buffer` from `offset` as words.
fn read_words(device: &RenderDevice, buffer: &wgpu::Buffer, offset: u64, bytes: u64) -> Vec<u32> {
    let d = &device.device;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vfx determinism readback"),
        size: bytes.max(4),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    if bytes > 0 {
        encoder.copy_buffer_to_buffer(buffer, offset, &staging, 0, bytes);
    }
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map").expect("map ok");
    let data = staging.slice(..).get_mapped_range().expect("range");
    let words: Vec<u32> = bytemuck::cast_slice(&data[..]).to_vec();
    drop(data);
    staging.unmap();
    words.into_iter().take((bytes / 4) as usize).collect()
}

/// xorshift32 stream: deterministic test data without a dependency.
struct XorShift(u32);

impl XorShift {
    fn next_u32(&mut self) -> u32 {
        let mut s = self.0;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.0 = s;
        s
    }
}

/// Runs the bitonic alpha sort on `pairs` (written as the alpha list) and reads the sorted
/// live prefix back.
fn bitonic(gpu: &mut VfxGpu, device: &RenderDevice, pairs: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let count = pairs.len() as u32;
    let sort_n = gpu.sort_n;
    gpu.queue
        .write_buffer(&gpu.sim.lists, u64::from(sort_n) * 8, bytemuck::cast_slice(pairs));
    gpu.queue
        .write_buffer(&gpu.sim.counters, 4, bytemuck::cast_slice(&[count]));
    gpu.set_alpha_bound(count);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        gpu.record_bitonic(&mut pass);
    }
    device.queue.submit([encoder.finish()]);
    let words = read_words(
        device,
        &gpu.sim.lists,
        u64::from(sort_n) * 8,
        u64::from(count) * 8,
    );
    words.as_chunks::<2>().0.to_vec()
}

/// Equal keys sort by slot whatever order the update kernel gathered them in, and the
/// largest possible key (a particle within 2 cm of the eye) stays inside the live prefix,
/// ahead of the padding sentinels.
#[test]
fn bitonic_order_does_not_depend_on_gather_order() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 20_000,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    for count in [300u32, 512, 3000, 9001] {
        let mut rng = XorShift(0x5eed_0001 ^ count);
        // Five distinct keys: almost every pair ties with hundreds of others.
        let mut pairs: Vec<[u32; 2]> = (0..count).map(|i| [rng.next_u32() % 5, i * 2 + 1]).collect();
        pairs[0][0] = u32::MAX;
        let mut expected = pairs.clone();
        expected.sort_unstable();
        let gathered = bitonic(&mut gpu, &device, &pairs);
        pairs.reverse();
        let reversed = bitonic(&mut gpu, &device, &pairs);
        pairs.rotate_left(count as usize / 3);
        let rotated = bitonic(&mut gpu, &device, &pairs);
        assert_eq!(gathered, expected, "{count}: not sorted by (key, slot)");
        assert_eq!(reversed, expected, "{count}: reversed input sorted differently");
        assert_eq!(rotated, expected, "{count}: rotated input sorted differently");
    }
}

/// The mesh-list sort orders its region by `(key, slot)` with the mesh counter, and does not
/// touch the alpha region next to it.
#[test]
fn mesh_list_sorts_in_its_own_region() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 4000,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let sort_n = gpu.sort_n;
    let mut rng = XorShift(0x0de8_1234);
    let alpha: Vec<[u32; 2]> = (0..sort_n).map(|i| [rng.next_u32(), i]).collect();
    for count in [37u32, 700, 2500] {
        let mut mesh: Vec<[u32; 2]> = (0..count).map(|i| [rng.next_u32() % 9, 3 * i]).collect();
        gpu.queue.write_buffer(
            &gpu.sim.lists,
            u64::from(sort_n) * 8,
            bytemuck::cast_slice(&alpha),
        );
        gpu.queue.write_buffer(
            &gpu.sim.lists,
            u64::from(sort_n) * 16,
            bytemuck::cast_slice(&mesh),
        );
        gpu.queue
            .write_buffer(&gpu.sim.counters, 0, bytemuck::cast_slice(&[0u32, 5, count, 0]));
        gpu.set_mesh_bound(count);
        assert!(gpu.mesh_sort_keys() >= count);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            gpu.record_bitonic_region(&mut pass, gpu.mesh_sort_offset, gpu.mesh_sort_n);
        }
        device.queue.submit([encoder.finish()]);
        let sorted = read_words(
            &device,
            &gpu.sim.lists,
            u64::from(sort_n) * 16,
            u64::from(count) * 8,
        );
        mesh.sort_unstable();
        assert_eq!(
            sorted.as_chunks::<2>().0,
            &mesh[..],
            "{count}: mesh list not sorted"
        );
        let untouched = read_words(
            &device,
            &gpu.sim.lists,
            u64::from(sort_n) * 8,
            u64::from(sort_n) * 8,
        );
        assert_eq!(
            untouched.as_chunks::<2>().0,
            &alpha[..],
            "{count}: the mesh sort wrote the alpha list"
        );
    }
    gpu.set_mesh_bound(0);
    assert_eq!(gpu.mesh_sort_keys(), 0, "no mesh particles: no mesh sort");
}

/// One frame of the bare pipeline: stage `jobs` for emitter record 0, simulate, copy the
/// stats, submit.
fn run_frame(gpu: &mut VfxGpu, device: &RenderDevice, jobs: &[SpawnJob]) {
    run_frame_emit(gpu, device, 0, jobs);
}

/// [`run_frame`] for jobs of an emitter record other than 0.
fn run_frame_emit(gpu: &mut VfxGpu, device: &RenderDevice, emitter: u32, jobs: &[SpawnJob]) {
    gpu.note_upload();
    gpu.push_spawn_jobs(emitter, jobs);
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, -10.0), Vec3::ZERO);
    gpu.set_frame(&camera, 1.0 / 60.0, 64, 64);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    gpu.simulate(&mut encoder);
    gpu.record_stats_copy(&mut encoder);
    device.queue.submit([encoder.finish()]);
}

/// Motionless jobs tagged by their x coordinate.
fn tagged_jobs(first_tag: u32, n: u32) -> Vec<SpawnJob> {
    (0..n)
        .map(|j| SpawnJob {
            position: [(first_tag + j) as f32, 0.0, 0.0],
            velocity: [0.0; 3],
            lifetime: 30.0,
            size: 0.1,
            rotation: 0.0,
            spin: 0.0,
            color: [1.0; 4],
            phase: 0.0,
        })
        .collect()
}

/// Job `j` of a frame lands in slot `(cursor + j) % capacity`; a frame with more jobs than
/// slots keeps its last `capacity` jobs. `spawned` and `dropped` count exactly.
#[test]
fn emit_places_jobs_at_the_ring_cursor() {
    let device = device();
    let capacity = 1000u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    gpu.write_emitter(0, EmitterGpu::default_emitter());
    let tag_of_slot = |gpu: &VfxGpu| -> Vec<f32> {
        let words = read_words(&device, &gpu.sim.pos_age, 0, u64::from(capacity) * 16);
        words
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| f32::from_bits(w[0]))
            .collect()
    };

    // Frame 1: 600 jobs from cursor 0.
    run_frame(&mut gpu, &device, &tagged_jobs(0, 600));
    let tags = tag_of_slot(&gpu);
    for j in 0..600u32 {
        assert_eq!(tags[j as usize], j as f32, "frame 1: job {j} not in slot {j}");
    }
    let stats = gpu.read_pool_stats().expect("stats");
    assert_eq!((stats.spawned, stats.dropped, stats.alive), (600, 0, 600));

    // Frame 2: 600 jobs from cursor 600; the last 200 wrap over frame 1's live slots 0..200.
    run_frame(&mut gpu, &device, &tagged_jobs(1000, 600));
    let tags = tag_of_slot(&gpu);
    for j in 0..600u32 {
        let slot = ((600 + j) % capacity) as usize;
        assert_eq!(
            tags[slot],
            (1000 + j) as f32,
            "frame 2: job {j} not in slot {slot}"
        );
    }
    let stats = gpu.read_pool_stats().expect("stats");
    assert_eq!((stats.spawned, stats.dropped, stats.alive), (1200, 200, 1000));

    // Frame 3: 2500 jobs from cursor 200: jobs 0..1500 cannot land (dropped), jobs
    // 1500..2500 overwrite all 1000 live slots.
    run_frame(&mut gpu, &device, &tagged_jobs(5000, 2500));
    let tags = tag_of_slot(&gpu);
    for j in 1500..2500u32 {
        let slot = ((200 + j) % capacity) as usize;
        assert_eq!(
            tags[slot],
            (5000 + j) as f32,
            "frame 3: job {j} not in slot {slot}"
        );
    }
    let stats = gpu.read_pool_stats().expect("stats");
    assert_eq!(
        (stats.spawned, stats.dropped, stats.alive),
        (3700, 200 + 1500 + 1000, 1000)
    );
}

/// A smoke column (alpha, sorted, curl and gravity) and sparks (additive, drag) on a moving
/// emitter, budgeted below the pool.
pub(crate) fn scene_library() -> VfxLibrary {
    let emitter = |name: &str, blend: BlendMode, rate: f32, life: (f32, f32)| EmitterDef {
        name: name.into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(rate),
            lifetime: RangeDef {
                min: life.0,
                max: life.1,
            },
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Sphere {
                radius: RangeDef { min: 0.1, max: 0.6 },
                surface: false,
            },
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 1.0, 0.0),
                    spread: 40.0,
                    speed: RangeDef { min: 0.5, max: 4.0 },
                },
                inherit: 0.3,
            },
            size: RangeDef { min: 0.2, max: 0.6 },
            rotation: RangeDef { min: 0.0, max: 6.0 },
            spin: RangeDef { min: -1.0, max: 1.0 },
            color: Vec::new(),
        },
        update: UpdateModule {
            gravity: (0.0, -2.0, 0.0),
            drag: 0.4,
            curl: Some((1.5, 0.7, 0.5)),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: Default::default(),
            flipbook: None,
            blend,
            softness: 0.0,
            alpha_scale: 1.0,
            stretch: Default::default(),
            light: None,
            emissive: 1.0,
            lit: false,
            self_shadow: 0.65,
            screen_floor_px: 0.0,
            heat_haze: 0.0,
            ribbon: Default::default(),
            mesh: None,
        },
    };
    let mut debris = emitter("debris", BlendMode::Alpha, 100.0, (1.0, 1.0));
    debris.render.mode = RenderMode::Mesh;
    let mut library = VfxLibrary::new();
    library
        .add(VfxSystemDef {
            version: crate::format::VFX_FORMAT_VERSION,
            name: "determinism".into(),
            system: SystemModule {
                bounds: None,
                budget: 1500,
                lod: None,
            },
            emitters: vec![
                emitter("smoke", BlendMode::Alpha, 400.0, (1.0, 1.0)),
                emitter("sparks", BlendMode::Additive, 600.0, (1.0, 1.0)),
                debris,
            ],
        })
        .expect("valid test effect");
    library
}

/// Everything one run leaves on the GPU: the four particle pools, the sorted alpha and mesh
/// lists and the ring stats.
#[derive(Debug, PartialEq)]
struct PoolSnapshot {
    pools: Vec<Vec<u32>>,
    alpha_list: Vec<u32>,
    mesh_list: Vec<u32>,
    dropped: u32,
}

/// Runs the scene → upload → simulate chain for `frames` on a fresh context sorting the
/// alpha list with `algorithm`. Checks the ring never overwrote a live particle on the way.
fn run_scene(device: &RenderDevice, frames: u32, algorithm: SortAlgorithm) -> PoolSnapshot {
    let capacity = 2000u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(device, profile).expect("vfx gpu");
    gpu.set_sort_algorithm(algorithm);
    let mut scene = VfxScene::new(scene_library());
    let id = scene
        .spawn("determinism", Vec3::ZERO, Vec3::Z, 0xd27)
        .expect("spawn");
    let dt = 1.0 / 60.0;
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, -12.0), Vec3::new(0.0, 2.0, 0.0));
    let lod = LodInput {
        camera_pos: camera.eye,
    };
    for frame in 0..frames {
        let t = frame as f32 * dt;
        if let Some(instance) = scene.instance_mut(id) {
            instance.set_transform(Vec3::new(t.sin() * 3.0, 0.0, t * 2.0), Vec3::Z, dt);
        }
        scene.update_budgets(dt, &lod, capacity, 1200);
        assert!(
            scene.estimated_live() <= 1500,
            "frame {frame}: system budget exceeded"
        );
        scene.upload(&mut gpu);
        gpu.set_frame(&camera, dt, 128, 72);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        gpu.simulate(&mut encoder);
        gpu.record_stats_copy(&mut encoder);
        device.queue.submit([encoder.finish()]);
        let stats = gpu.read_pool_stats().expect("stats");
        assert!(stats.alive <= capacity);
        assert_eq!(
            stats.dropped, 0,
            "frame {frame}: the ring overwrote a live particle"
        );
    }
    let stats = gpu.read_pool_stats().expect("stats");
    assert!(
        stats.alpha > 0 && stats.additive > 0 && stats.mesh > 0,
        "every list drawn: {stats:?}"
    );
    let bytes = u64::from(capacity) * 16;
    let pools = [
        &gpu.sim.pos_age,
        &gpu.sim.vel_life,
        &gpu.sim.attrib,
        &gpu.sim.color0,
    ]
    .into_iter()
    .map(|buffer| read_words(device, buffer, 0, bytes))
    .collect();
    let alpha_list = read_words(
        device,
        &gpu.sim.lists,
        u64::from(gpu.sort_n) * 8,
        u64::from(stats.alpha) * 8,
    );
    let mesh_list = read_words(
        device,
        &gpu.sim.lists,
        u64::from(gpu.sort_n) * 16,
        u64::from(stats.mesh) * 8,
    );
    for (name, list) in [("alpha", &alpha_list), ("mesh", &mesh_list)] {
        assert!(
            list.as_chunks::<2>().0.windows(2).all(|w| w[0] < w[1]),
            "{name} list not strictly ordered by (key, slot)"
        );
    }
    PoolSnapshot {
        pools,
        alpha_list,
        mesh_list,
        dropped: stats.dropped,
    }
}

/// ADR-2633: the pool and the scene captured partway through play the same frames again after they
/// are put back, spawns of the second stretch included. A render-out job with the world held leaves
/// live effects this way, so the next held job draws the same particles.
#[test]
fn a_restored_pool_and_scene_play_the_same_frames_again() {
    let device = device();
    let capacity = 2000u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let mut scene = VfxScene::new(scene_library());
    scene
        .spawn("determinism", Vec3::ZERO, Vec3::Z, 0xd27)
        .expect("spawn");
    let dt = 1.0 / 60.0;
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, -12.0), Vec3::new(0.0, 2.0, 0.0));
    let lod = LodInput {
        camera_pos: camera.eye,
    };
    let run = |gpu: &mut VfxGpu, scene: &mut VfxScene, frames: u32| {
        for _ in 0..frames {
            scene.update_budgets(dt, &lod, capacity, 1200);
            scene.upload(gpu);
            gpu.set_frame(&camera, dt, 128, 72);
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            gpu.simulate(&mut encoder);
            device.queue.submit([encoder.finish()]);
        }
    };
    let pools = |gpu: &VfxGpu| -> Vec<Vec<u32>> {
        [
            &gpu.sim.pos_age,
            &gpu.sim.vel_life,
            &gpu.sim.attrib,
            &gpu.sim.color0,
        ]
        .into_iter()
        .map(|buffer| read_words(&device, buffer, 0, u64::from(capacity) * 16))
        .collect()
    };
    run(&mut gpu, &mut scene, 40);
    let at_capture = pools(&gpu);
    let live_at_capture = scene.estimated_live();
    let pool = gpu.capture_pool();
    let instances = scene.capture_instances();
    run(&mut gpu, &mut scene, 30);
    let first = pools(&gpu);
    assert!(first != at_capture, "the second stretch moved the particles");
    gpu.restore_pool(pool);
    assert!(scene.restore_instances(instances));
    assert_eq!(scene.estimated_live(), live_at_capture);
    assert!(
        pools(&gpu) == at_capture,
        "the pool went back to what was captured"
    );
    run(&mut gpu, &mut scene, 30);
    assert!(pools(&gpu) == first, "the same frames were played again");
}

/// ADR-2633: a reset pool is empty and the ring and clock start over, as in a new context.
#[test]
fn a_reset_pool_is_a_new_pool() {
    let device = device();
    let capacity = 256u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let slot = gpu.allocate_emitters(1).expect("slot");
    gpu.write_emitter(slot, EmitterGpu::default_emitter());
    run_frame(&mut gpu, &device, &tagged_jobs(1, 16));
    let words = |gpu: &VfxGpu| read_words(&device, &gpu.sim.pos_age, 0, u64::from(capacity) * 16);
    assert!(words(&gpu).iter().any(|w| *w != 0), "the frame spawned");
    gpu.reset_pool();
    assert!(words(&gpu).iter().all(|w| *w == 0), "the pool is empty");
    assert_eq!(gpu.time, 0.0);
    assert_eq!(gpu.spawn_cursor, 0);
    // ADR-2687: an empty pool is not kept alive by a spawn time from before the reset.
    assert!(!gpu.particles_may_live(), "the reset pool holds no particle");
    assert_eq!(
        gpu.allocate_emitters(1),
        Some(slot),
        "the allocator starts over, so the first slot is free again"
    );
}

/// ADR-2687: a pool put back says whether a particle may live as it did when it was captured. The
/// last spawn time went back with the clock: before, a pool captured empty and put back after a
/// stretch that spawned stayed "alive" (never idle) until the clock caught up with that spawn.
#[test]
fn a_restored_pool_knows_whether_a_particle_may_live() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 256,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    gpu.set_max_lifetime(0.5);
    let slot = gpu.allocate_emitters(1).expect("slot");
    gpu.write_emitter(slot, EmitterGpu::default_emitter());
    run_frame(&mut gpu, &device, &[]);
    let pool = gpu.capture_pool();
    assert!(!gpu.particles_may_live());
    run_frame(&mut gpu, &device, &tagged_jobs(1, 8));
    assert!(gpu.particles_may_live(), "the stretch spawned");
    gpu.restore_pool(pool);
    assert!(!gpu.particles_may_live(), "the captured pool had nothing alive");
    assert!(gpu.restart_idle_clock(), "so its clock may start over");
}

/// ADR-2687 (audit renderout-state findings 1 and 2): the particle clock (the heat-haze phase) and
/// the spawn ring (the slots new particles take, the last tie-break of the blend sort) start over
/// when no particle can be alive, so a render-out job's new effects take the same slots and phase
/// in every job. A pool with a particle that may still live keeps both.
#[test]
fn an_idle_pool_restarts_its_clock_and_ring_and_a_live_one_keeps_them() {
    let device = device();
    let capacity = 256u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    gpu.set_max_lifetime(0.25);
    let slot = gpu.allocate_emitters(1).expect("slot");
    gpu.write_emitter(slot, EmitterGpu::default_emitter());
    run_frame(&mut gpu, &device, &tagged_jobs(1, 16));
    assert_eq!(gpu.spawn_cursor(), 16);
    assert!(!gpu.restart_idle_clock(), "a particle may still live");
    assert_eq!(gpu.spawn_cursor(), 16, "a live pool keeps its ring");
    // Past the longest lifetime (and the live margin) nothing can be alive.
    for _ in 0..90 {
        run_frame(&mut gpu, &device, &[]);
    }
    assert!(gpu.sim_clock() > 1.0);
    assert!(gpu.restart_idle_clock());
    assert_eq!(gpu.sim_clock(), 0.0);
    assert_eq!(gpu.spawn_cursor(), 0);
    // The next spawn lands where a new context's first spawn lands.
    let mut fresh = VfxGpu::new(&device, profile).expect("vfx gpu");
    fresh.set_max_lifetime(0.25);
    let fresh_slot = fresh.allocate_emitters(1).expect("slot");
    fresh.write_emitter(fresh_slot, EmitterGpu::default_emitter());
    run_frame(&mut gpu, &device, &tagged_jobs(100, 4));
    run_frame(&mut fresh, &device, &tagged_jobs(100, 4));
    assert_eq!(gpu.sim_clock(), fresh.sim_clock());
    assert_eq!(gpu.spawn_cursor(), fresh.spawn_cursor());
    let first_slots = |gpu: &VfxGpu| read_words(&device, &gpu.sim.pos_age, 0, 4 * 16);
    assert!(
        first_slots(&gpu) == first_slots(&fresh),
        "the new particles took a new context's slots"
    );
}

/// ADR-2814: whether a particle can be alive at a job's start goes by the lifetimes spawned, not by
/// the library's longest. A short job with a short effect leaves nothing alive at the next start;
/// judged by the library, that job continued on another clock and ring.
#[test]
fn a_pool_restarts_by_the_lifetimes_it_spawned_not_the_librarys_longest() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 256,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    gpu.set_max_lifetime(3.0);
    let slot = gpu.allocate_emitters(1).expect("slot");
    gpu.write_emitter(slot, EmitterGpu::default_emitter());
    let short: Vec<SpawnJob> = tagged_jobs(1, 16)
        .into_iter()
        .map(|job| SpawnJob {
            lifetime: 0.25,
            ..job
        })
        .collect();
    run_frame(&mut gpu, &device, &short);
    // A quarter second on: the particles are at the end of their lifetime, within the margin.
    for _ in 0..15 {
        run_frame(&mut gpu, &device, &[]);
    }
    assert!(!gpu.restart_idle_clock(), "a particle may still live");
    // Well inside the library's 3 s, every particle of this spawn is gone.
    for _ in 0..30 {
        run_frame(&mut gpu, &device, &[]);
    }
    assert!(gpu.sim_clock() < 1.0);
    assert!(
        gpu.particles_may_live(),
        "the layer stays live by the library's bound"
    );
    assert!(gpu.restart_idle_clock(), "nothing this pool spawned is alive");
    assert_eq!(gpu.sim_clock(), 0.0);
    assert_eq!(gpu.spawn_cursor(), 0);
}

/// Two contexts fed the same scene end with bit-identical particle pools and the same
/// sorted alpha list.
#[test]
fn same_scene_simulates_bit_identically() {
    let device = device();
    let a = run_scene(&device, 90, SortAlgorithm::Bitonic);
    let b = run_scene(&device, 90, SortAlgorithm::Bitonic);
    assert_eq!(a.dropped, 0);
    assert!(a == b, "two runs of the same scene left different GPU pools");
}

/// The opt-in radix sort (with its slot tie-break) puts the alpha list in exactly the
/// bitonic order: the same `(key, slot)` total order.
#[test]
fn radix_and_bitonic_agree_on_the_alpha_order() {
    let device = device();
    let bitonic = run_scene(&device, 60, SortAlgorithm::Bitonic);
    let radix = run_scene(&device, 60, SortAlgorithm::Radix);
    assert!(!bitonic.alpha_list.is_empty());
    assert!(
        bitonic.alpha_list == radix.alpha_list,
        "radix and bitonic alpha orders differ"
    );
    assert!(bitonic == radix, "the sort changed the simulation");
}

/// Spec 78 § 12.2, ADR-2022: a floating-origin rebase moves every live particle by `-shift` once,
/// on the next frame, before that frame's spawns, which are already in the new space. Motionless
/// particles keep their tag in `y` so each one can be traced.
#[test]
fn a_rebase_moves_the_live_pool_once_and_not_the_new_spawns() {
    let device = device();
    let capacity = 256u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    // No gravity, no drag: positions only move by the shift.
    let mut still = EmitterGpu::default_emitter();
    still.gravity_drag = [0.0; 4];
    gpu.write_emitter(0, still);
    let positions = |gpu: &VfxGpu, slots: std::ops::Range<u32>| -> Vec<[f32; 3]> {
        let words = read_words(&device, &gpu.sim.pos_age, 0, u64::from(capacity) * 16);
        words.as_chunks::<4>().0[slots.start as usize..slots.end as usize]
            .iter()
            .map(|w| [f32::from_bits(w[0]), f32::from_bits(w[1]), f32::from_bits(w[2])])
            .collect()
    };
    let jobs = |x: f32, n: u32| -> Vec<SpawnJob> {
        tagged_jobs(0, n)
            .into_iter()
            .enumerate()
            .map(|(i, mut job)| {
                job.position = [x + i as f32, i as f32, 3000.5];
                job
            })
            .collect()
    };
    // Frame 1: ten particles about 4 km out.
    run_frame(&mut gpu, &device, &jobs(4100.25, 10));
    // The rebase: three cells of 1024 m on X and Z.
    let shift = Vec3::new(3072.0, 0.0, 3072.0);
    gpu.shift_origin(shift);
    assert_eq!(gpu.pending_shift(), shift);
    // Frame 2 spawns five more, already in the new space.
    run_frame(&mut gpu, &device, &jobs(1028.25, 5));
    assert_eq!(gpu.pending_shift(), Vec3::ZERO, "taken by the frame");
    let old = positions(&gpu, 0..10);
    for (i, p) in old.iter().enumerate() {
        assert_eq!(
            *p,
            [4100.25 + i as f32 - 3072.0, i as f32, 3000.5 - 3072.0],
            "old particle {i}"
        );
    }
    let new = positions(&gpu, 10..15);
    for (i, p) in new.iter().enumerate() {
        assert_eq!(
            *p,
            [1028.25 + i as f32, i as f32, 3000.5],
            "new particle {i}: moved twice"
        );
    }
    // Frame 3: no rebase, nothing moves.
    run_frame(&mut gpu, &device, &[]);
    assert_eq!(positions(&gpu, 0..10), old);
    assert_eq!(positions(&gpu, 10..15), new);
    // Bad input queues nothing.
    gpu.shift_origin(Vec3::new(f32::NAN, 0.0, 0.0));
    assert_eq!(gpu.pending_shift(), Vec3::ZERO);
}

/// A held host (game time paused) keeps the spawn jobs it staged for the next drawn frame over
/// any number of updates; running again with nothing drawn drops them after [`STALE_UPLOADS`]
/// as before (ADR-3287: a paused headless app on a fast machine lost a blast's fire and smoke
/// between the blast and its screenshot).
#[test]
fn held_time_keeps_staged_spawn_jobs_for_the_next_drawn_frame() {
    let device = device();
    let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
    gpu.write_emitter(0, EmitterGpu::default_emitter());
    gpu.note_upload();
    gpu.push_spawn_jobs(0, &tagged_jobs(0, 16));
    gpu.set_time_held(true);
    for _ in 0..STALE_UPLOADS * 4 {
        gpu.note_upload();
    }
    assert_eq!(gpu.queued_spawn_jobs(), 16, "held updates kept the jobs");
    gpu.set_time_held(false);
    for _ in 0..=STALE_UPLOADS {
        gpu.note_upload();
    }
    assert_eq!(gpu.queued_spawn_jobs(), 0, "undrawn running updates drop them");
}

/// ADR-3307: a spawn queued after many updates with no drawn frame (a headless app between agent
/// screenshots, a window that was hidden) is kept until it has itself waited [`STALE_UPLOADS`]
/// uploads. Before, the count ran from the last drawn frame, so such a spawn was dropped one update
/// after it was queued (the destruction lab's fireball, spawned by `lab.fire` a few updates before
/// `seeing.screenshot`, never drew on a loaded host).
#[test]
fn a_spawn_after_undrawn_updates_waits_its_own_stale_count() {
    let device = device();
    let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
    let slot = gpu.allocate_emitters(1).expect("slot");
    gpu.write_emitter(slot, EmitterGpu::default_emitter());
    assert!(!gpu.is_idle(), "a new context has warm frames left");
    // Updates with nothing queued and no frame drawn.
    for _ in 0..3 * STALE_UPLOADS {
        gpu.note_upload();
    }
    gpu.note_upload();
    gpu.push_spawn_jobs(slot, &tagged_jobs(1, 4));
    for _ in 1..STALE_UPLOADS {
        gpu.note_upload();
        assert_eq!(gpu.queued_spawn_jobs(), 4, "a fresh spawn is not stale");
    }
    // Its own count is up: it is dropped, and the next spawn starts a new count.
    gpu.note_upload();
    assert_eq!(
        gpu.queued_spawn_jobs(),
        0,
        "jobs that waited too long are dropped"
    );
    gpu.note_upload();
    gpu.push_spawn_jobs(slot, &tagged_jobs(10, 2));
    gpu.note_upload();
    assert_eq!(gpu.queued_spawn_jobs(), 2);
    // A drawn frame takes them.
    run_frame(&mut gpu, &device, &[]);
    assert_eq!(gpu.queued_spawn_jobs(), 0);
}

/// deep-zai-vfx finding 3: `push_spawn_jobs` is the one emitter-slot writer that did not
/// bounds-check. A slot at or past [`MAX_EMITTERS`] reaches the update and draw kernels as
/// `emitters[u32(at.w)]` — an out-of-bounds WGSL storage read (garbage or device loss). The
/// sibling writers (`write_emitter`, `write_emitter_events`) drop out-of-range slots; the spawn
/// batch is dropped the same way.
#[test]
fn push_spawn_jobs_drops_batches_with_an_out_of_range_emitter_slot() {
    let device = device();
    let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
    for slot in [MAX_EMITTERS, MAX_EMITTERS + 44, u32::MAX] {
        gpu.push_spawn_jobs(slot, &tagged_jobs(0, 4));
        assert_eq!(gpu.queued_spawn_jobs(), 0, "slot {slot} is out of range");
    }
    // The last in-range slot still queues its batch.
    gpu.push_spawn_jobs(MAX_EMITTERS - 1, &tagged_jobs(8, 2));
    assert_eq!(gpu.queued_spawn_jobs(), 2, "MAX_EMITTERS - 1 is in range");
}

// ---------------------------------------------------------------------------------------------
// Event determinism (fix-vfx1-determinism findings 1 and 4)
// ---------------------------------------------------------------------------------------------

use crate::events::{TRIGGER_SPAWN, child_seed, event_seed, link_seed, sample_child};
use crate::format::MAX_EVENTS_PER_EMITTER;
use crate::gpu::layout::{ChildInitGpu, EventLinkGpu, MAX_EMITTERS};

/// A parent emitter record that fires `TRIGGER_SPAWN` on its first update, with `links` for
/// emitter record 0 and a point-shape child init at emitter record `child_slot`.
fn event_scene_setup(
    gpu: &mut VfxGpu,
    links: [EventLinkGpu; MAX_EVENTS_PER_EMITTER],
    child_slot: u32,
    child_init: ChildInitGpu,
) {
    let mut parent = EmitterGpu::default_emitter();
    parent.events = [TRIGGER_SPAWN, 0, 0, 0];
    gpu.write_emitter(0, parent);
    gpu.write_emitter(child_slot, EmitterGpu::default_emitter());
    // The kernel reads `child_inits[link.child_slot]`: the links live on the parent's record,
    // the init on the child's.
    gpu.write_emitter_events(0, links, ChildInitGpu::default());
    gpu.write_emitter_events(
        child_slot,
        [EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER],
        child_init,
    );
    gpu.set_events_active(true);
}

/// One link row: `trigger` with `count` children of emitter record `child_slot`, always firing.
fn spawn_link(trigger: u32, count: u32, child_slot: u32) -> EventLinkGpu {
    EventLinkGpu {
        trigger_count: trigger | (count << 8),
        child_slot,
        probability: 1.0,
        inherit: 0.0,
    }
}

/// A point-shape child init: children stay exactly at the parent's position with zero velocity.
fn point_child_init(life: f32) -> ChildInitGpu {
    ChildInitGpu {
        shape: [0.0, 0.0, 0.0, 0.0],
        life_size: [life, life, 0.1, 0.1],
        ..ChildInitGpu::default()
    }
}

/// Jobs at rest, one per (exactly representable) x position on the X axis.
fn rest_jobs(positions: &[f32]) -> Vec<SpawnJob> {
    positions.iter().map(|&x| rest_job_at([x, 0.0, 0.0])).collect()
}

/// One job at rest at `position`.
fn rest_job_at(position: [f32; 3]) -> SpawnJob {
    SpawnJob {
        position,
        velocity: [0.0; 3],
        lifetime: 30.0,
        size: 0.1,
        rotation: 0.0,
        spin: 0.0,
        color: [1.0; 4],
        phase: 0.0,
    }
}

/// Reads `(position, velocity)` words of pool slots `slots`, interleaved per slot.
fn pool_pos_vel(device: &RenderDevice, gpu: &VfxGpu, slots: std::ops::Range<u32>) -> Vec<u32> {
    let bytes = u64::from(slots.end - slots.start) * 16;
    let pos = read_words(device, &gpu.sim.pos_age, u64::from(slots.start) * 16, bytes);
    let vel = read_words(device, &gpu.sim.vel_life, u64::from(slots.start) * 16, bytes);
    pos.chunks(4)
        .zip(vel.chunks(4))
        .flat_map(|(p, v)| p.iter().chain(v).copied())
        .collect()
}

/// Finding 1 (event keep-set): an over-capacity frame keeps exactly the first
/// `event_capacity` fired events in slot order (the budget counts events, not staging ranks),
/// drops the rest, and two identical contexts end bit-identical — the keep-set and the child
/// layout never followed thread scheduling.
#[test]
fn event_keep_set_is_deterministic_and_in_slot_order() {
    let device = device();
    let capacity = 64u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        event_capacity: 48, // events: 48 of the 64 fired are kept
        ..VfxProfile::desktop_high()
    };
    let run = |device: &RenderDevice| {
        let mut gpu = VfxGpu::new(device, profile).expect("vfx gpu");
        let links = [
            spawn_link(TRIGGER_SPAWN, 1, 1),
            EventLinkGpu::default(),
            EventLinkGpu::default(),
            EventLinkGpu::default(),
        ];
        event_scene_setup(&mut gpu, links, 1, point_child_init(10.0));
        let positions: Vec<f32> = (0..64).map(|j| 1000.0 + j as f32).collect();
        // Frame 1: 64 parents fire 64 spawn events; every one is staged.
        run_frame(&mut gpu, device, &rest_jobs(&positions));
        let stats1 = gpu.read_pool_stats().expect("stats");
        assert_eq!(stats1.events_fired, 64, "every parent fired");
        assert_eq!(stats1.events_overflow, 0, "staging loses nothing");
        assert_eq!(stats1.children_spawned, 0, "children come on the next frame");
        // Frame 2: the prepare compacts the first 48 events (one point child each, at ring
        // slot 0..48 in slot order) and counts the 16 past the budget lost — the
        // over-budget drop is counted where it happens, in prepare.
        run_frame(&mut gpu, device, &[]);
        let stats2 = gpu.read_pool_stats().expect("stats");
        assert_eq!(
            stats2.events_overflow, 16,
            "only the 16 events past the budget are lost"
        );
        assert_eq!(stats2.children_spawned, 48, "one child per kept event");
        assert_eq!(stats2.children_dropped, 0);
        // The child ring mirrors the first 48 parent positions exactly.
        let child_ring = pool_pos_vel(device, &gpu, capacity..capacity + 48);
        for (k, w) in child_ring.chunks(8).enumerate() {
            let pos = &w[..4];
            let vel = &w[4..];
            assert_eq!(f32::from_bits(pos[0]), 1000.0 + k as f32, "child {k} parent slot");
            assert_eq!(pos[1], 0.0f32.to_bits(), "child {k} position y");
            assert_eq!(pos[2], 0.0f32.to_bits(), "child {k} position z");
            for (axis, w) in vel[..3].iter().enumerate() {
                assert_eq!(*w, 0.0f32.to_bits(), "child {k} velocity {axis}");
            }
        }
        (
            pool_pos_vel(device, &gpu, 0..capacity),
            child_ring,
            (stats1, stats2),
        )
    };
    let (pool_a, ring_a, stats_a) = run(&device);
    let (pool_b, ring_b, stats_b) = run(&device);
    assert_eq!(stats_a, stats_b, "the counts match across contexts");
    assert_eq!(pool_a, pool_b, "two runs of the same frame leave different pools");
    assert_eq!(ring_a, ring_b, "two runs place the children differently");
}

/// A cleared event epoch is not a staged event.
///
/// Steps start at 1, so epoch 0 means the rank was never written. On the first advancing
/// frame the previous step is 0, and `event_prepare` used to keep every such rank. A zero
/// event payload is parent slot 0 with trigger 0, so a trigger-0 link on emitter 0 spawned
/// children on a frame where no particle fired.
#[test]
fn cleared_event_epochs_do_not_spawn_children() {
    let device = device();
    let event_capacity = 48u32;
    let profile = VfxProfile {
        particle_capacity: 64,
        event_capacity,
        child_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let links = [
        spawn_link(0, 1, 1),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
    ];
    event_scene_setup(&mut gpu, links, 1, point_child_init(10.0));
    // Fresh allocations are not specified to be zero, so write the cleared state the shader
    // comment assumes: epoch 0, empty event payload, a dead pool, and zero counters.
    let epochs = vec![0u32; event_capacity as usize];
    gpu.queue
        .write_buffer(&gpu.sim.event_epoch, 0, bytemuck::cast_slice(&epochs));
    let event_words = event_capacity as usize * (size_of::<crate::gpu::layout::EventGpu>() / 4);
    let events = vec![0u32; event_words];
    gpu.queue
        .write_buffer(&gpu.sim.events, 0, bytemuck::cast_slice(&events));
    let pool_bytes = u64::from(gpu.capacity + gpu.child_capacity()) * 16;
    let zeros = vec![0u8; pool_bytes as usize];
    gpu.queue.write_buffer(&gpu.sim.pos_age, 0, &zeros);
    gpu.queue.write_buffer(&gpu.sim.vel_life, 0, &zeros);
    let stats_zero = [0u32; crate::gpu::layout::STATS_COUNT as usize];
    gpu.queue
        .write_buffer(&gpu.sim.stats, 0, bytemuck::cast_slice(&stats_zero));
    run_frame(&mut gpu, &device, &[]);
    let stats = gpu.read_pool_stats().expect("stats");
    assert_eq!(stats.events_fired, 0, "no particle fired");
    assert_eq!(
        stats.children_spawned, 0,
        "a cleared epoch is not last frame's event"
    );
}

/// The event buffers hold `3 * (particle_capacity + child_capacity)` staged ranks. The event
/// budget counts fired events and may be larger than that span: a small pool that keeps the
/// default budget. Building such a pool must not write past the epoch buffer. integ-v048 merged
/// the build-time epoch stamp, sized by `event_capacity` while that was also the buffer length,
/// with the staging layout that made the buffer one epoch per staged rank; the stamp of this
/// profile was 8192 epochs into a buffer of 96, a validation error that the engine's device
/// hook turns into a panic.
#[test]
fn a_pool_smaller_than_its_event_budget_builds_and_spawns_children() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 16,
        child_capacity: 16,
        event_capacity: 8_192,
        ..VfxProfile::desktop_high()
    };
    // Error scopes are per thread (wgpu 30), so another test's device work cannot land here.
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let built = VfxGpu::new(&device, profile);
    let error = regia_core::tasks::block_on(scope.pop());
    assert!(
        error.is_none(),
        "building a 16 + 16 slot pool with an event budget of 8192 raised: {error:?}"
    );
    let mut gpu = built.expect("vfx gpu");
    assert_eq!(gpu.event_capacity(), 8_192);
    assert_eq!(
        gpu.sim.event_epoch.size(),
        3 * (16 + 16) * 4,
        "one epoch per staged rank"
    );
    // The pool works: eight parents fire, and each event spawns its child on the next frame.
    let links = [
        spawn_link(TRIGGER_SPAWN, 1, 1),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
    ];
    event_scene_setup(&mut gpu, links, 1, point_child_init(10.0));
    let positions: Vec<f32> = (0..8).map(|j| 1000.0 + j as f32).collect();
    run_frame(&mut gpu, &device, &rest_jobs(&positions));
    let fired = gpu.read_pool_stats().expect("stats");
    assert_eq!(fired.events_fired, 8, "every parent fired");
    run_frame(&mut gpu, &device, &[]);
    let spawned = gpu.read_pool_stats().expect("stats");
    assert_eq!(spawned.events_overflow, 0);
    assert_eq!(spawned.children_spawned, 8, "one child per event");
}

/// Every staged rank starts with the invalid epoch, not only the first `event_capacity` of
/// them: the stamp that keeps a never-written rank from reading as an event of step 0 has to
/// cover the buffer the staging layout allocates. With 64 + 64 slots that is 384 ranks, and a
/// budget of 48 events stamped 48 of them.
#[test]
fn every_staged_rank_starts_with_an_invalid_epoch() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 64,
        event_capacity: 48,
        child_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let bytes = gpu.sim.event_epoch.size();
    assert_eq!(bytes, 3 * (64 + 64) * 4, "one epoch per staged rank");
    let epochs = read_words(&device, &gpu.sim.event_epoch, 0, bytes);
    let unstamped: Vec<usize> = epochs
        .iter()
        .enumerate()
        .filter(|(_, epoch)| **epoch != u32::MAX)
        .map(|(rank, _)| rank)
        .collect();
    assert!(
        unstamped.is_empty(),
        "{} of {} ranks do not hold the invalid epoch, the first is rank {:?}",
        unstamped.len(),
        epochs.len(),
        unstamped.first()
    );
}

/// One frame at the same simulated step again (`dt` 0: a paused or held host), with no jobs.
fn run_held_frame(gpu: &mut VfxGpu, device: &RenderDevice) {
    gpu.note_upload();
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, -10.0), Vec3::ZERO);
    gpu.set_frame(&camera, 0.0, 64, 64);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    gpu.simulate(&mut encoder);
    gpu.record_stats_copy(&mut encoder);
    device.queue.submit([encoder.finish()]);
}

/// A cleared rank beside a rank that fired is not an event.
///
/// `event_prepare` scans the ranks of the slots whose mask bit is set. A slot that fired one
/// event has two more ranks, and in a cleared buffer they hold epoch 0. A held frame at step 1
/// consumes the events of step 1 with the previous step 0, so without the epoch-0 check those
/// two ranks read as events with a zero payload: parent 0, trigger 0, and a trigger-0 link
/// spawns for each. [`cleared_event_epochs_do_not_spawn_children`] no longer reaches that check:
/// it fires nothing, so the scan it was written for (every rank under the budget) visits no
/// rank at all since the mask scan replaced it (integ-v048).
#[test]
fn a_cleared_rank_beside_a_fired_event_is_not_an_event() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 64,
        event_capacity: 48,
        child_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    // The spawn link of the real events, and a trigger-0 link a zero payload would match.
    let links = [
        spawn_link(TRIGGER_SPAWN, 1, 1),
        spawn_link(0, 1, 1),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
    ];
    event_scene_setup(&mut gpu, links, 1, point_child_init(10.0));
    // The cleared state over every staged rank: epoch 0 and an empty payload.
    let epoch_bytes = gpu.sim.event_epoch.size() as usize;
    gpu.queue
        .write_buffer(&gpu.sim.event_epoch, 0, &vec![0u8; epoch_bytes]);
    let event_bytes = gpu.sim.events.size() as usize;
    gpu.queue
        .write_buffer(&gpu.sim.events, 0, &vec![0u8; event_bytes]);
    // Step 1: four parents fire one spawn event each (rank 0 of their slot).
    run_frame(&mut gpu, &device, &rest_jobs(&[1000.0, 1001.0, 1002.0, 1003.0]));
    let fired = gpu.read_pool_stats().expect("stats");
    assert_eq!(fired.events_fired, 4, "every parent fired");
    assert_eq!(fired.children_spawned, 0, "children come on the next frame");
    // Step 1 again: the held frame consumes the four events. The eight cleared ranks beside
    // them are not events.
    run_held_frame(&mut gpu, &device);
    let held = gpu.read_pool_stats().expect("stats");
    assert_eq!(
        held.events_fired, 4,
        "the totals count from the start: a held frame fires nothing new"
    );
    assert_eq!(
        held.children_spawned, 4,
        "one child per fired event, none for a cleared rank"
    );
    assert_eq!(held.children_dropped, 0);
    assert_eq!(held.events_overflow, 0);
}

/// A staged event whose parent slot is past the emitter table is not an event (ADR-5976).
///
/// `event_prepare` indexes the link table with the event's parent slot. integ's 3dc9cbd37 skips
/// a parent at or past `MAX_EMITTERS` (and stamps the rank invalid) instead of reading past the
/// table; the fixrev5-vfx-events-vt rewrite of the kernel did not have that check. What an
/// out-of-range read returns depends on the adapter's bounds-check policy, but without the check
/// the event is compacted whatever it reads, so the consumed count tells the two apart on every
/// adapter. A control context with the same scene consumes and spawns for all four events.
#[test]
fn an_event_parent_past_the_emitter_table_is_not_compacted() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 64,
        event_capacity: 48,
        child_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let consumed_after_held = |parent_slot: Option<u32>| {
        let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
        let links = [
            spawn_link(TRIGGER_SPAWN, 1, 1),
            EventLinkGpu::default(),
            EventLinkGpu::default(),
            EventLinkGpu::default(),
        ];
        event_scene_setup(&mut gpu, links, 1, point_child_init(10.0));
        // Step 1: four parents at ordinals 0..4 fire one spawn event each, at rank `3 * k`.
        run_frame(&mut gpu, &device, &rest_jobs(&[1000.0, 1001.0, 1002.0, 1003.0]));
        assert_eq!(gpu.read_pool_stats().expect("stats").events_fired, 4);
        if let Some(slot) = parent_slot {
            // Rewrite each staged event's parent (the trigger bits stay `TRIGGER_SPAWN`).
            let event_bytes = size_of::<crate::gpu::layout::EventGpu>() as u64;
            let word = TRIGGER_SPAWN | (slot << 8);
            for k in 0..4u64 {
                let offset = 3 * k * event_bytes + (event_bytes - 4);
                gpu.queue
                    .write_buffer(&gpu.sim.events, offset, bytemuck::bytes_of(&word));
            }
        }
        // Step 1 again: the held frame's prepare consumes the four staged ranks.
        run_held_frame(&mut gpu, &device);
        let offset = u64::from(crate::gpu::layout::STATS_EVENT_CONSUME) * 4;
        let consumed = read_words(&device, &gpu.sim.stats, offset, 4)[0];
        let stats = gpu.read_pool_stats().expect("stats");
        (consumed, stats.children_spawned)
    };
    assert_eq!(
        consumed_after_held(None),
        (4, 4),
        "control: the four events are consumed and each spawns one child"
    );
    let parent = crate::gpu::layout::MAX_EMITTERS + 44;
    assert_eq!(
        consumed_after_held(Some(parent)),
        (0, 0),
        "an event of parent slot {parent} is past the link table and must not be compacted"
    );
}

/// Muse review of fix-vfx1-determinism (finding 1): the keep-set is by fired count, not window
/// position — a window whose early slots are quiet keeps every event a late slot fires. The
/// rank staging this replaced dropped all of these: the first firing ordinal is 120, rank 360,
/// past any sane budget.
#[test]
fn sparse_window_keeps_late_ordinal_events() {
    let device = device();
    let capacity = 128u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        child_capacity: 32,
        event_capacity: 8,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    // Emitter 0: plain particles, no event triggers — they fill the window and fire nothing.
    gpu.write_emitter(0, EmitterGpu::default_emitter());
    // Emitter 1: fires OnSpawn with one point child per event.
    let mut parent = EmitterGpu::default_emitter();
    parent.events = [TRIGGER_SPAWN, 0, 0, 0];
    gpu.write_emitter(1, parent);
    let links = [
        spawn_link(TRIGGER_SPAWN, 1, 2),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
    ];
    gpu.write_emitter_events(1, links, ChildInitGpu::default());
    gpu.write_emitter(2, EmitterGpu::default_emitter());
    gpu.write_emitter_events(
        2,
        [EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER],
        point_child_init(10.0),
    );
    gpu.set_events_active(true);
    // Frame 1: 120 quiet particles at window ordinals 0..120.
    run_frame_emit(&mut gpu, &device, 0, &tagged_jobs(0, 120));
    // Frame 2: 6 event parents at ordinals 120..126 — six fired events, under the budget of 8.
    run_frame_emit(&mut gpu, &device, 1, &tagged_jobs(1000, 6));
    let stats1 = gpu.read_pool_stats().expect("stats");
    assert_eq!(stats1.events_fired, 6, "only the emitter-1 parents fire");
    assert_eq!(
        stats1.events_overflow, 0,
        "six fired events fit the budget of 8 however late their ordinals"
    );
    // Frame 3: all six events spawn a child, in slot order.
    run_frame(&mut gpu, &device, &[]);
    let stats2 = gpu.read_pool_stats().expect("stats");
    assert_eq!(stats2.children_spawned, 6, "every late-ordinal event is kept");
    assert_eq!(stats2.children_dropped, 0);
    let child_ring = pool_pos_vel(&device, &gpu, capacity..capacity + 6);
    for (k, w) in child_ring.chunks(8).enumerate() {
        assert_eq!(
            f32::from_bits(w[0]),
            1000.0 + k as f32,
            "child {k} is the parent at late ordinal {}",
            120 + k
        );
    }
}

/// Finding 1 (child budget): links are reserved in event order with rollback — a link that
/// does not fit consumes nothing, so a later smaller link still spawns. Counts and ring
/// placement are identical on every run.
#[test]
fn child_ring_budget_rolls_back_rejected_links() {
    let device = device();
    let capacity = 16u32;
    let ring = 256u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        child_capacity: ring,
        event_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let run = |device: &RenderDevice| {
        let mut gpu = VfxGpu::new(device, profile).expect("vfx gpu");
        // One parent, three firing links: 150 + 150 + 60 children, ring 256.
        // Deterministic order: 150 fits, 150 rolls back (300 > 256), 60 fits (210 <= 256).
        let links = [
            spawn_link(TRIGGER_SPAWN, 150, 1),
            spawn_link(TRIGGER_SPAWN, 150, 1),
            spawn_link(TRIGGER_SPAWN, 60, 1),
            EventLinkGpu::default(),
        ];
        event_scene_setup(&mut gpu, links, 1, point_child_init(10.0));
        run_frame(&mut gpu, device, &rest_jobs(&[7.0]));
        run_frame(&mut gpu, device, &[]);
        let stats = gpu.read_pool_stats().expect("stats");
        assert_eq!(
            stats.children_spawned, 210,
            "150 + 60 spawn; the rejected 150 rolled back instead of consuming the ring"
        );
        assert_eq!(stats.children_dropped, 150, "the rejected link is counted once");
        pool_pos_vel(device, &gpu, capacity..capacity + ring)
    };
    let a = run(&device);
    let b = run(&device);
    assert_eq!(a, b, "two runs placed the children differently");
}

/// Finding 4: a cone child spawned by the GPU matches the CPU mirror
/// (`events::sample_child`) bit for bit — the WGSL det_sin_cos twin reproduces the Rust
/// `det_sin_cos_f32` exactly, unlike WGSL `sin`/`cos`.
#[test]
fn cone_child_matches_the_cpu_mirror_bit_for_bit() {
    let device = device();
    let capacity = 16u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        child_capacity: 64,
        event_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let cone = ChildInitGpu {
        shape: [3.0, 0.0, 0.5, 2.0], // cone, length 0.5..2
        shape2: [
            0.0,
            0.0,
            0.0,
            regia_core::math::det_tan(0.6f32.to_radians().clamp(0.0, 1.55)),
        ],
        velocity: [0.0, 1.0, 0.0, 0.0], // radial, speed 0
        life_size: [1.0, 1.0, 0.2, 0.2],
        ..ChildInitGpu::default()
    };
    let links = [
        spawn_link(TRIGGER_SPAWN, 1, 1),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
    ];
    event_scene_setup(&mut gpu, links, 1, cone);
    let parent = [1.5, -2.5, 3.25];
    run_frame(&mut gpu, &device, &[rest_job_at(parent)]);
    run_frame(&mut gpu, &device, &[]);
    // The event of the parent in slot 0, fired at step 1, link 0, child 0.
    let ev_seed = event_seed(0, 1, TRIGGER_SPAWN);
    let seed = child_seed(link_seed(ev_seed, 0), 0);
    let expected = sample_child(
        &cone,
        seed,
        Vec3::new(parent[0], parent[1], parent[2]),
        Vec3::ZERO,
        0.0,
    );
    let ring = pool_pos_vel(&device, &gpu, capacity..capacity + 1);
    let pos = &ring[..4];
    let vel = &ring[4..];
    assert_eq!(pos[0], expected.position.x.to_bits(), "child x");
    assert_eq!(pos[1], expected.position.y.to_bits(), "child y");
    assert_eq!(pos[2], expected.position.z.to_bits(), "child z");
    assert_eq!(vel[0], expected.velocity.x.to_bits(), "velocity x");
    assert_eq!(vel[1], expected.velocity.y.to_bits(), "velocity y");
    assert_eq!(vel[2], expected.velocity.z.to_bits(), "velocity z");
}

/// An unchanged emitter record uploads nothing, and one contiguous dirty run is one
/// `write_buffer`.
#[test]
fn emitter_writes_skip_unchanged_records_and_coalesce_a_run() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 64,
        decal_capacity: 4,
        child_capacity: 16,
        event_capacity: 16,
        sort: false,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("gpu");
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, 6.0), Vec3::ZERO);
    let mut record = EmitterGpu::default_emitter();
    record.origin = [1.0, 2.0, 3.0, 1.0];
    for slot in 0..3 {
        gpu.write_emitter(slot, record);
    }
    gpu.set_frame(&camera, 1.0 / 60.0, 64, 64);
    assert_eq!(
        gpu.emitter_writes(),
        1,
        "three contiguous dirty slots must be one write_buffer, got {}",
        gpu.emitter_writes()
    );
    for slot in 0..3 {
        gpu.write_emitter(slot, record);
    }
    gpu.set_frame(&camera, 1.0 / 60.0, 64, 64);
    assert_eq!(
        gpu.emitter_writes(),
        1,
        "writing the same records again must not upload"
    );
    record.origin[0] = 4.0;
    gpu.write_emitter(0, record);
    gpu.write_emitter(2, record);
    gpu.set_frame(&camera, 1.0 / 60.0, 64, 64);
    assert_eq!(
        gpu.emitter_writes(),
        3,
        "a gap between dirty slots is two writes (1 + 2)"
    );
}

/// An event-child emitter that has spawned nothing must not sort or soft-draw the child ring.
///
/// The parent is in the system so validation accepts the `Event` child. `upload` does not
/// schedule, so no parent particle exists and the reservation stays empty.
#[test]
fn idle_event_child_does_not_sort_or_soft_draw_the_ring() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 64,
        decal_capacity: 4,
        sort: true,
        light_budget: 1,
        screen_floor_px: 1.0,
        smoke_budget: 32,
        soft_particle_res: crate::gpu::SoftParticleRes::Half,
        event_capacity: 64,
        child_capacity: 4096,
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("gpu");
    gpu.set_soft_pass(true);
    let def = crate::format::VfxSystemDef::from_ron(
        r#"(
        version: 7,
        name: "event-only",
        system: (budget: 64),
        emitters: [
            (
                name: "parent",
                spawn: (mode: Burst(1), lifetime: (min: 0.5, max: 0.5)),
                init: (
                    shape: Sphere(radius: (min: 0.1, max: 0.1)),
                    size: (min: 1.0, max: 1.0),
                ),
                update: (events: [(on: Death, child: "child", count: 2, probability: 1.0, inherit_velocity: 0.0)]),
                render: (mode: Billboard, blend: Additive),
            ),
            (
                name: "child",
                spawn: (mode: Event, lifetime: (min: 1.0, max: 1.0)),
                init: (
                    shape: Sphere(radius: (min: 0.1, max: 0.1)),
                    size: (min: 1.0, max: 1.0),
                ),
                render: (mode: Billboard, blend: Alpha, softness: 1.0, emissive: 0.0),
            ),
        ],
    )"#,
    )
    .expect("event-child system");
    let mut library = VfxLibrary::new();
    library.add(def).expect("library");
    let mut scene = VfxScene::new(library);
    scene
        .spawn("event-only", Vec3::ZERO, Vec3::Z, 1)
        .expect("instance");
    scene.upload(&mut gpu);
    assert_eq!(
        gpu.sort_live_keys(),
        512,
        "no live alpha particles: the sort covers the 512-key floor, not the {}-slot child ring ({})",
        profile.child_capacity,
        gpu.sort_live_keys()
    );
    assert!(
        gpu.sort_live_keys() < profile.child_capacity,
        "sort keys {} cover the child ring",
        gpu.sort_live_keys()
    );
    assert!(
        !gpu.soft_pass_active(),
        "no live soft particles: the soft layer records no draw"
    );
}

/// Finding 7 (Muse review of fix-vfx1-determinism; strengthened for rev-kimi-19 finding 5): a
/// filled-sphere child exercises the WGSL `det_cbrt` twin (volume fill), which the cone test
/// above does not, so an edit to the shader's cube-root loop must fail here, not just on the
/// CPU pin. Sixteen children (sixteen cube-root draws) spawn from a parent at the origin, so
/// each child's position *is* its local offset (no parent-magnitude ulp hides a radius
/// change), and on a held frame, so the update does not move them before the read-back.
///
/// - **The cube root, bit for bit on a software adapter.** The radius range is `[1, 1]`, so
///   `lerp` returns exactly 1 and the radius is `det_cbrt(u)` itself. The z offset is
///   `(2u − 1) · det_cbrt(u)`: `2u − 1` is exact for the 24-bit `child_unit`, `det_cbrt` is the
///   deterministic twin (divisions and adds only), and one multiply rounds it, with no
///   mul-add for a driver to contract. Where f32 division is correctly rounded (lavapipe, the
///   gate's reference adapter), z must match the CPU mirror exactly, and a cube root that
///   differs by an ulp shows: the `det_cbrt` → `pow(u, 1/3)` mutation fails here (child 0,
///   1 ulp). WGSL allows f32 division 2.5 ULP, and `det_cbrt`'s last Newton step divides
///   twice (`u / (y·y)`, then by 3). An RTX 5090 lands it up to 2 ulp from the CPU, so a
///   hardware adapter is held to 3 ulp instead.
/// - **x and y.** They go through `flat = sqrt(1 − z²)` and `det_sin_cos`. On a software
///   adapter they are held to 1 ulp of `|local|` (lavapipe measured 0). On hardware the bound
///   is `4 + 1 / flat` ulp. WGSL does not round `sqrt` correctly (`inverseSqrt`'s 2 ULP), and a
///   driver that contracts `1 − z²` into an fma moves it by up to 2⁻²⁴, which the sqrt
///   amplifies by `1 / (2·flat)` near the poles (an RTX 5090 measured 4.5 ulp on a child with
///   a small `flat`).
/// - **The velocity, per axis.** The radial speed is nonzero, so this compares
///   `normalize(local) · speed`, not a zero vector. WGSL does not round `normalize` correctly
///   either (`inverseSqrt` plus a 2.5-ULP divide): 4 ulp of the speed on a software adapter
///   (lavapipe measured 2), and `4 + 1 / flat` on hardware, since the direction inherits the
///   offset's error (the 5090 measured 3.5).
#[test]
fn filled_sphere_child_matches_the_cpu_mirror_within_driver_ulp() {
    let device = device();
    let capacity = 16u32;
    let children = 16u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        child_capacity: 64,
        event_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let sphere = ChildInitGpu {
        shape: [1.0, 0.0, 1.0, 1.0],    // sphere, filled (surface 0), radius exactly 1
        velocity: [0.0, 1.0, 2.5, 2.5], // radial, speed 2.5
        life_size: [1.0, 1.0, 0.2, 0.2],
        // Identity basis with unit scales: `right.w` is the offset scale and `up.w` the
        // speed scale (`spawn_child`), so the child lands at parent + local.
        right: [1.0, 0.0, 0.0, 1.0],
        up: [0.0, 1.0, 0.0, 1.0],
        forward: [0.0, 0.0, 1.0, 0.0],
        ..ChildInitGpu::default()
    };
    let links = [
        spawn_link(TRIGGER_SPAWN, children, 1),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
        EventLinkGpu::default(),
    ];
    event_scene_setup(&mut gpu, links, 1, sphere);
    run_frame(&mut gpu, &device, &[rest_job_at([0.0; 3])]);
    // The children spawn on a held frame (dt 0: the same step consumes its own staging). Its
    // update then leaves them bit-exact where `child_emit` put them: `p + v * 0` and
    // `(v + g * 0) / (1 + k * 0)` are exact, where a stepping frame would move each child by
    // `v * dt` before the read-back.
    run_frame_dt(&mut gpu, &device, &[], 0.0);
    // The event of the parent in slot 0, fired at step 1, link 0; child c is ring slot c.
    let ev_seed = event_seed(0, 1, TRIGGER_SPAWN);
    let ring = pool_pos_vel(&device, &gpu, capacity..capacity + children);
    // Software adapters (lavapipe) round f32 division correctly: the cube root is exact there.
    let software = device.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
    let z_tolerance = if software { 0 } else { 3 };
    // One f32 step at `scale`: the tolerance unit at the local (or speed) magnitude.
    let step_at = |scale: f32| scale.next_up() - scale;
    // Deviation of `got` from `want` in f32 steps at `scale`.
    let steps = |got: f32, want: f32, scale: f32| (got - want).abs() / step_at(scale);
    let mut worst = [0.0f32; 2];
    for (c, row) in ring.chunks(8).enumerate() {
        let seed = child_seed(link_seed(ev_seed, 0), c as u32);
        let expected = sample_child(&sphere, seed, Vec3::ZERO, Vec3::ZERO, 0.0);
        let want_local = expected.position;
        let r = want_local.length();
        // The radius is `det_cbrt(u)`: strictly inside the unit sphere (without the volume
        // fill it would be exactly 1).
        assert!(r > 0.0 && r < 1.0, "child {c}: filled radius {r} outside (0, 1)");
        let got_local = Vec3::new(
            f32::from_bits(row[0]),
            f32::from_bits(row[1]),
            f32::from_bits(row[2]),
        );
        let got_vel = Vec3::new(
            f32::from_bits(row[4]),
            f32::from_bits(row[5]),
            f32::from_bits(row[6]),
        );
        let z_ulps = (i64::from(row[2] as i32) - i64::from(want_local.z.to_bits() as i32)).unsigned_abs();
        assert!(
            z_ulps <= z_tolerance,
            "child {c}: z = (2u - 1) * det_cbrt(u) is {z_ulps} ulp from the CPU mirror (allowed \
             {z_tolerance} on this adapter): {:e} vs {:e}",
            got_local.z,
            want_local.z
        );
        // The horizontal share of the direction (`sqrt(1 − z²)`), floored so the bound stays
        // finite at a pole.
        let flat = (want_local.x.hypot(want_local.y) / r).max(1e-3);
        let (offset_tolerance, speed_tolerance) = if software {
            (1.0, 4.0)
        } else {
            (4.0 + 1.0 / flat, 4.0 + 1.0 / flat)
        };
        for axis in 0..3 {
            let offset = steps(got_local[axis], want_local[axis], r);
            worst[0] = worst[0].max(offset);
            assert!(
                offset <= offset_tolerance,
                "child {c}: local axis {axis} off by {offset} ulp of |local| (allowed \
                 {offset_tolerance}): {:e} vs {:e}",
                got_local[axis],
                want_local[axis]
            );
            let speed = steps(got_vel[axis], expected.velocity[axis], 2.5);
            worst[1] = worst[1].max(speed);
            assert!(
                speed <= speed_tolerance,
                "child {c}: velocity axis {axis} off by {speed} ulp of the speed (allowed \
                 {speed_tolerance}): {:e} vs {:e}",
                got_vel[axis],
                expected.velocity[axis]
            );
        }
    }
    eprintln!("filled sphere: worst offset, velocity deviations in ulp: {worst:?}");
}

// ---------------------------------------------------------------------------------------------
// Event prepare scan cost and window shrink (rev-kimi-19, fixrev-22-k findings 1 and 2)
// ---------------------------------------------------------------------------------------------

/// The cumulative `STATS_EVENT_SCAN` word: fired-slot mask bits `event_prepare` has visited.
fn event_scan_bits(device: &RenderDevice, gpu: &VfxGpu) -> u32 {
    let offset = u64::from(super::layout::STATS_EVENT_SCAN) * 4;
    read_words(device, &gpu.sim.stats, offset, 4)[0]
}

/// One frame stepping `dt` seconds with `jobs` per emitter record (in push order).
fn run_frame_dt(gpu: &mut VfxGpu, device: &RenderDevice, jobs: &[(u32, Vec<SpawnJob>)], dt: f32) {
    gpu.note_upload();
    for (emitter, list) in jobs {
        gpu.push_spawn_jobs(*emitter, list);
    }
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, -10.0), Vec3::ZERO);
    gpu.set_frame(&camera, dt, 64, 64);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    gpu.simulate(&mut encoder);
    gpu.record_stats_copy(&mut encoder);
    device.queue.submit([encoder.finish()]);
}

/// `n` motionless jobs at distinct x positions living `life` seconds.
fn short_jobs(first_tag: u32, n: u32, life: f32) -> Vec<SpawnJob> {
    tagged_jobs(first_tag, n)
        .into_iter()
        .map(|job| SpawnJob {
            lifetime: life,
            ..job
        })
        .collect()
}

/// rev-kimi-19 finding 1: `event_prepare` clears the fired-slot mask words it consumes, so its
/// scan visits exactly the slots that fired since the last prepare. Before the fix the bits
/// were never cleared: a slot that once fired stayed set in its parity forever, and a single
/// late event re-scanned every stale bit below it (here 32 stale bits for 1 event; up to
/// ~116k slots × 3 epoch reads per frame at desktop_high).
#[test]
fn event_prepare_scans_only_the_slots_that_fired() {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 64,
        child_capacity: 32,
        event_capacity: 64,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    // Emitter 0 fires OnSpawn and has no link: every event is compacted, none spawns a child.
    let mut parent = EmitterGpu::default_emitter();
    parent.events = [TRIGGER_SPAWN, 0, 0, 0];
    gpu.write_emitter(0, parent);
    gpu.write_emitter_events(
        0,
        [EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER],
        ChildInitGpu::default(),
    );
    gpu.set_events_active(true);
    // No max lifetime: the update window is the whole pool, so slot s is window ordinal s.
    let mut scans = Vec::new();
    // Step 1: 32 parents fire at slots 0..32 (parity 1).
    run_frame_dt(&mut gpu, &device, &[(0, tagged_jobs(0, 32))], 1.0 / 60.0);
    scans.push(event_scan_bits(&device, &gpu));
    // Step 2: prepare consumes the 32 events; nothing fires.
    run_frame_dt(&mut gpu, &device, &[], 1.0 / 60.0);
    scans.push(event_scan_bits(&device, &gpu));
    // Step 3: one parent fires at slot 32 (parity 1 again, above the 32 consumed bits).
    run_frame_dt(&mut gpu, &device, &[(0, tagged_jobs(100, 1))], 1.0 / 60.0);
    scans.push(event_scan_bits(&device, &gpu));
    // Step 4: prepare consumes that one event.
    run_frame_dt(&mut gpu, &device, &[], 1.0 / 60.0);
    scans.push(event_scan_bits(&device, &gpu));
    let stats = gpu.read_pool_stats().expect("stats");
    assert_eq!(stats.events_fired, 33);
    assert_eq!(stats.events_overflow, 0);
    let per_frame: Vec<u32> = std::iter::once(scans[0])
        .chain(scans.windows(2).map(|w| w[1] - w[0]))
        .collect();
    assert_eq!(
        per_frame,
        [0, 32, 0, 1],
        "bits visited per prepare (cumulative {scans:?}): the scan must cost one bit per \
         fired slot, not re-visit the 32 consumed ones"
    );
}

/// rev-kimi-19 finding 2: events staged against a larger window are still compacted (or
/// counted in `events_overflow`) when the main window shrinks before the next prepare. The
/// children fire at ordinals `window + ring_index` = 104..128 in a 64-slot window; the next
/// frame the window shrinks to one slot (no spawns, `life + 0.25 s` elapsed), and the old
/// `min(highest, pool_slots())` clamp (65) dropped all 24 without counting them.
#[test]
fn a_shrinking_window_keeps_child_events_staged_against_the_old_one() {
    let device = device();
    let capacity = 128u32;
    let ring = 64u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        child_capacity: ring,
        event_capacity: 256,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    gpu.set_max_lifetime(1.0);
    let fires_into = |gpu: &mut VfxGpu, slot: u32, child: u32| {
        let mut record = EmitterGpu::default_emitter();
        record.events = [TRIGGER_SPAWN, 0, 0, 0];
        gpu.write_emitter(slot, record);
        let links = [
            spawn_link(TRIGGER_SPAWN, 1, child),
            EventLinkGpu::default(),
            EventLinkGpu::default(),
            EventLinkGpu::default(),
        ];
        (slot, links)
    };
    // Record 3: a filler parent whose children (record 2) only advance the child-ring cursor.
    // Record 0: the real parent; its children (record 1) fire OnSpawn into record 2.
    let (s3, links3) = fires_into(&mut gpu, 3, 2);
    gpu.write_emitter_events(s3, links3, ChildInitGpu::default());
    let (s0, links0) = fires_into(&mut gpu, 0, 1);
    gpu.write_emitter_events(s0, links0, ChildInitGpu::default());
    let (s1, links1) = fires_into(&mut gpu, 1, 2);
    gpu.write_emitter_events(s1, links1, point_child_init(10.0));
    gpu.write_emitter(2, EmitterGpu::default_emitter());
    gpu.write_emitter_events(
        2,
        [EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER],
        point_child_init(10.0),
    );
    gpu.set_events_active(true);
    let dt = 1.0 / 60.0;
    // F1: 40 filler parents fire (window ordinals 0..40).
    run_frame_dt(&mut gpu, &device, &[(3, short_jobs(0, 40, 0.5))], dt);
    // F2: their 40 children take ring 0..40; 24 real parents fire at ordinals 40..64.
    run_frame_dt(&mut gpu, &device, &[(0, short_jobs(100, 24, 0.5))], dt);
    // F3: the 24 children land at ring 40..64 and fire at ordinals 64 + 40..64 = 104..128.
    run_frame_dt(&mut gpu, &device, &[], dt);
    let staged = gpu.read_pool_stats().expect("stats");
    assert_eq!(staged.events_fired, 40 + 24 + 24);
    assert_eq!(staged.children_spawned, 40 + 24);
    // F4: no spawns for longer than life + 0.25 s: the window shrinks to one slot.
    run_frame_dt(&mut gpu, &device, &[], 2.0);
    assert_eq!(gpu.update_window.1, 1, "the main window shrank");
    run_frame_dt(&mut gpu, &device, &[], dt);
    let stats = gpu.read_pool_stats().expect("stats");
    assert_eq!(stats.events_fired, 88, "nothing fires after F3");
    assert_eq!(
        stats.children_spawned + stats.events_overflow,
        stats.events_fired,
        "every fired event spawned its child or was counted lost: {stats:?}"
    );
    assert_eq!(stats.events_overflow, 0, "the budget of 256 holds all 88");
}

/// The three draw lists of one run, read after every frame.
#[derive(Debug, PartialEq)]
struct GatherRun {
    /// Per frame: the additive, alpha and mesh lists (`(key, slot)` words) in draw order.
    frames: Vec<[Vec<u32>; 3]>,
}

/// Runs the determinism scene for `frames` on a fresh context with `base`'s sort setting and
/// reads every list after every frame. Checks on the way that each unsorted list (the additive
/// list always; the alpha and mesh lists when the profile does not sort) is in pool-slot order:
/// the update window's first slot first, around the ring. Sorted lists are strictly ordered by
/// `(key, slot)`.
fn run_gather_scene(device: &RenderDevice, frames: u32, base: VfxProfile) -> GatherRun {
    let capacity = 2000u32;
    let profile = VfxProfile {
        particle_capacity: capacity,
        ..base
    };
    let mut gpu = VfxGpu::new(device, profile).expect("vfx gpu");
    let mut scene = VfxScene::new(scene_library());
    let id = scene
        .spawn("determinism", Vec3::ZERO, Vec3::Z, 0xd27)
        .expect("spawn");
    let dt = 1.0 / 60.0;
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, -12.0), Vec3::new(0.0, 2.0, 0.0));
    let lod = LodInput {
        camera_pos: camera.eye,
    };
    let mut out = Vec::new();
    for frame in 0..frames {
        let t = frame as f32 * dt;
        if let Some(instance) = scene.instance_mut(id) {
            instance.set_transform(Vec3::new(t.sin() * 3.0, 0.0, t * 2.0), Vec3::Z, dt);
        }
        scene.update_budgets(dt, &lod, capacity, 1200);
        scene.upload(&mut gpu);
        gpu.set_frame(&camera, dt, 128, 72);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        gpu.simulate(&mut encoder);
        gpu.record_stats_copy(&mut encoder);
        device.queue.submit([encoder.finish()]);
        let stats = gpu.read_pool_stats().expect("stats");
        let (first, _) = gpu.update_window();
        let lists = [(0u64, stats.additive), (1, stats.alpha), (2, stats.mesh)].map(|(region, count)| {
            read_words(
                device,
                &gpu.sim.lists,
                region * u64::from(gpu.sort_n) * 8,
                u64::from(count) * 8,
            )
        });
        for (index, (name, list)) in ["additive", "alpha", "mesh"].iter().zip(&lists).enumerate() {
            let pairs = list.as_chunks::<2>().0;
            if profile.sort && index > 0 {
                assert!(
                    pairs.windows(2).all(|w| w[0] < w[1]),
                    "frame {frame}: the sorted {name} list is not strictly ordered by (key, slot)"
                );
                continue;
            }
            let ordinals: Vec<u32> = pairs
                .iter()
                .map(|pair| (pair[1] + capacity - first % capacity) % capacity)
                .collect();
            assert!(
                ordinals.windows(2).all(|w| w[0] < w[1]),
                "frame {frame}: the unsorted {name} list is not in pool-slot order (window starts at {first})"
            );
        }
        out.push(lists);
    }
    let last = out.last().expect("frames");
    assert!(
        last.iter().all(|list| !list.is_empty()),
        "every list drawn: {} additive, {} alpha, {} mesh words",
        last[0].len(),
        last[1].len(),
        last[2].len()
    );
    GatherRun { frames: out }
}

/// Two runs of [`run_gather_scene`] gather the same three lists, entry for entry, on every frame.
fn assert_same_lists(profile: VfxProfile) {
    let device = device();
    let a = run_gather_scene(&device, 90, profile);
    let b = run_gather_scene(&device, 90, profile);
    for (frame, (x, y)) in a.frames.iter().zip(&b.frames).enumerate() {
        for (name, (p, q)) in ["additive", "alpha", "mesh"].iter().zip(x.iter().zip(y)) {
            assert!(p == q, "frame {frame}: two runs gathered different {name} lists");
        }
    }
}

/// ADR-5991: a profile that does not sort (Mobile High, the crash effects' Lite tier) draws its
/// lists in gather order, so that order must not depend on thread scheduling. Two contexts fed the
/// same scene gather the same additive, alpha and mesh lists, entry for entry, on every frame
/// (`vfx_lab --crash-sheets` drew different Lite-tier bytes run to run while the update kernel
/// appended with an atomic counter).
#[test]
fn an_unsorted_profile_gathers_the_same_lists_every_run() {
    let profile = VfxProfile::mobile_high();
    assert!(!profile.sort, "Mobile High draws its lists unsorted");
    assert_same_lists(profile);
}

/// ADR-5991: a sorting profile (Desktop High) never sorts its additive list either, so the same
/// rule holds for it: two runs gather the same additive list (and the same sorted alpha and mesh
/// lists) on every frame.
#[test]
fn a_sorted_profile_gathers_the_same_additive_list_every_run() {
    let profile = VfxProfile::desktop_high();
    assert!(profile.sort, "Desktop High sorts its alpha and mesh lists");
    assert_same_lists(profile);
}

/// ADR-5991: overlapping additive particles on a sorting profile draw the same bytes twice. Their
/// blend sums round in the HDR target, so the sum depends on the draw order: with the atomic
/// append, two runs differed by a few pixels on most frames.
#[test]
fn sorted_profile_additive_overlap_renders_the_same_bytes_twice() {
    let device = device();
    let render = || {
        let profile = VfxProfile {
            particle_capacity: 4000,
            ..VfxProfile::desktop_high()
        };
        let mut harness = crate::offscreen::VfxHarness::new(&device, profile).expect("harness");
        let mut scene = VfxScene::new(scene_library());
        let id = scene
            .spawn("determinism", Vec3::ZERO, Vec3::Z, 0xd27)
            .expect("spawn");
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, -5.0), Vec3::new(0.0, 1.5, 0.0));
        let lod = LodInput {
            camera_pos: camera.eye,
        };
        let dt = 1.0 / 60.0;
        let mut frames = Vec::new();
        for frame in 0..120u32 {
            let t = frame as f32 * dt;
            if let Some(instance) = scene.instance_mut(id) {
                instance.set_transform(Vec3::new(t.sin() * 0.5, 0.0, 0.0), Vec3::Z, dt);
            }
            scene.update(dt, &lod, profile.particle_capacity);
            scene.upload(harness.gpu());
            let rendered = harness.render(&device, &camera, 160, 90, dt).expect("render");
            frames.push(rendered.pixels);
        }
        frames
    };
    let a = render();
    let b = render();
    let differing: Vec<(usize, usize)> = a
        .iter()
        .zip(&b)
        .enumerate()
        .map(|(frame, (x, y))| {
            let pixels = x
                .as_chunks::<4>()
                .0
                .iter()
                .zip(y.as_chunks::<4>().0)
                .filter(|(p, q)| p != q)
                .count();
            (frame, pixels)
        })
        .filter(|&(_, pixels)| pixels > 0)
        .collect();
    assert!(
        differing.is_empty(),
        "{} of 120 frames differ between two runs (frame, pixels): {:?}",
        differing.len(),
        &differing[..differing.len().min(8)]
    );
}

/// ADR-10105, the pause-freeze bar for GPU particles: a paused game uploads every frame with a
/// zero step and `time_held` (what `upload_runtime_frame` does from `Time`). Over 300 such frames
/// no particle moves or ages, none is born or dies, and the first step after the pause leaves the
/// pool bit-identical to one that never paused (no per-frame counter advanced while held).
#[test]
fn a_held_zero_step_pool_neither_moves_nor_ages_and_resumes_bit_identically() {
    const SLOTS: u32 = 24;
    let moving = |first: u32| -> Vec<SpawnJob> {
        tagged_jobs(first, SLOTS)
            .into_iter()
            .enumerate()
            .map(|(i, job)| SpawnJob {
                velocity: [1.0 + i as f32 * 0.25, 0.5, -0.75],
                lifetime: 30.0,
                ..job
            })
            .collect()
    };
    let device = device();
    let run = |pause: bool| {
        let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
        gpu.write_emitter(0, EmitterGpu::default_emitter());
        run_frame_dt(&mut gpu, &device, &[(0, moving(0))], 1.0 / 60.0);
        for _ in 0..4 {
            run_frame_dt(&mut gpu, &device, &[], 1.0 / 60.0);
        }
        if pause {
            gpu.set_time_held(true);
            run_frame_dt(&mut gpu, &device, &[], 0.0);
            let held = pool_pos_vel(&device, &gpu, 0..SLOTS);
            assert!(held.iter().any(|&w| w != 0), "the pool has live particles");
            for frame in 1..=300u32 {
                run_frame_dt(&mut gpu, &device, &[], 0.0);
                if frame.is_multiple_of(50) {
                    assert_eq!(
                        pool_pos_vel(&device, &gpu, 0..SLOTS),
                        held,
                        "paused frame {frame}: a particle moved or aged"
                    );
                }
            }
            assert_eq!(pool_pos_vel(&device, &gpu, 0..SLOTS), held);
            gpu.set_time_held(false);
        }
        run_frame_dt(&mut gpu, &device, &[], 1.0 / 60.0);
        pool_pos_vel(&device, &gpu, 0..SLOTS)
    };
    let paused = run(true);
    let control = run(false);
    assert_eq!(
        paused, control,
        "after the pause the pool is where a never-paused pool is"
    );
}
