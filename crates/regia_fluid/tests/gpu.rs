// SPDX-License-Identifier: MPL-2.0
//! The GPU path against the CPU reference within the documented tolerance (`regia_fluid::gpu` § 4).
//! Needs a headless adapter (lavapipe in CI).
#![cfg(feature = "gpu")]

use glam::Vec3;
use regia_fluid::gpu::GpuFluid;
use regia_fluid::presets;
use regia_fluid::sim::{FluidParams, FluidSim};
use regia_render::RenderDevice;

fn dam_break() -> FluidSim {
    let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
    let params = FluidParams {
        bounds_min: lo,
        bounds_max: hi,
        cell_size: dx,
        dt: 1.0 / 120.0,
        max_substeps: 1,
        separation: 0.0,
        reseed: false,
        ..FluidParams::default()
    };
    let mut p = params;
    p.whitewater.enabled = false;
    presets::build("dam_break", p).expect("dam break")
}

fn mean(v: &[Vec3]) -> Vec3 {
    v.iter().fold(Vec3::ZERO, |s, x| s + *x) / v.len().max(1) as f32
}

#[test]
fn gpu_path_stays_within_tolerance_of_the_cpu_reference() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let mut cpu = dam_break();
    // One CPU step emits the block; both runs start from those particles.
    cpu.step();
    let mut gpu_sim = dam_break();
    gpu_sim.step();
    assert_eq!(cpu.state_digest(), gpu_sim.state_digest());
    let gpu = GpuFluid::new(&device.device, &device.queue, &gpu_sim).expect("upload");
    let steps = 40;
    gpu.step(steps).expect("gpu steps");
    for _ in 0..steps {
        cpu.step();
    }
    gpu.sync_into(&mut gpu_sim, u64::from(steps)).expect("read back");
    assert_eq!(gpu_sim.positions().len(), cpu.positions().len());
    assert!(gpu_sim.positions().iter().all(|p| p.is_finite()));

    let (v_cpu, v_gpu) = (cpu.liquid_volume(), gpu_sim.liquid_volume());
    let volume_error = (v_gpu - v_cpu).abs() / v_cpu;
    let (m_cpu, m_gpu) = (mean(cpu.velocities()), mean(gpu_sim.velocities()));
    let velocity_error = (m_gpu - m_cpu).length();
    let front = |s: &FluidSim| s.positions().iter().fold(0.0f32, |m, p| m.max(p.x));
    let front_error = (front(&gpu_sim) - front(&cpu)).abs();
    let dx = cpu.dims().dx;
    eprintln!(
        "gpu vs cpu after {steps} steps: volume {v_gpu} vs {v_cpu} ({volume_error}), mean velocity {m_gpu} vs \
         {m_cpu} (|d| {velocity_error}), front {} vs {} ({front_error} m)",
        front(&gpu_sim),
        front(&cpu)
    );
    assert!(volume_error < 0.02, "volume {v_gpu} vs {v_cpu}");
    assert!(
        velocity_error <= 0.05 * m_cpu.length() + 0.02,
        "mean velocity {m_gpu} vs {m_cpu}"
    );
    assert!(front_error <= dx, "front differs by {front_error} m");

    // The GPU path repeats itself on one device.
    let mut again = dam_break();
    again.step();
    let gpu2 = GpuFluid::new(&device.device, &device.queue, &again).expect("upload");
    gpu2.step(steps).expect("gpu steps");
    gpu2.sync_into(&mut again, u64::from(steps)).expect("read back");
    assert_eq!(again.positions(), gpu_sim.positions());
}

/// A tank with a paddle (a kinematic box, open below and above) moving at 0.5 m/s, settled for 5 CPU
/// steps first; both paths start from the same particles.
fn paddle_tank() -> FluidSim {
    let mut sim = paddle_tank_base();
    let mut moving = sim.moving_solids()[0];
    moving.linear_velocity = Vec3::new(0.5, 0.0, 0.0);
    sim.set_moving_solids(vec![moving]).expect("moving");
    sim
}

/// The GPU moving-solid path (rasterize kernel, solid face velocities, push-out) against the CPU
/// reference on the paddle tank after 40 steps: volume within 2 %, the mean velocity within
/// `0.1 |mean_cpu| + 0.01 m/s`, both pushed along +x, and the paddle in the same place.
#[test]
fn gpu_moving_solids_stay_within_tolerance_of_the_cpu_reference() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let mut cpu = paddle_tank();
    let mut gpu_sim = paddle_tank();
    assert_eq!(cpu.state_digest(), gpu_sim.state_digest());
    let gpu = GpuFluid::new(&device.device, &device.queue, &gpu_sim).expect("upload");
    let steps = 40;
    gpu.step(steps).expect("gpu steps");
    for _ in 0..steps {
        cpu.step();
    }
    gpu.sync_into(&mut gpu_sim, u64::from(steps)).expect("read back");
    assert!(gpu_sim.positions().iter().all(|p| p.is_finite()));
    let (v_cpu, v_gpu) = (cpu.liquid_volume(), gpu_sim.liquid_volume());
    let volume_error = (v_gpu - v_cpu).abs() / v_cpu;
    let (m_cpu, m_gpu) = (mean(cpu.velocities()), mean(gpu_sim.velocities()));
    let velocity_error = (m_gpu - m_cpu).length();
    let paddle_gap = (cpu.moving_solids()[0].position - gpu_sim.moving_solids()[0].position).length();
    eprintln!(
        "gpu vs cpu paddle after {steps} steps: volume {v_gpu} vs {v_cpu} ({volume_error}), mean velocity {m_gpu} vs \
         {m_cpu} (|d| {velocity_error}), paddle {paddle_gap} m apart"
    );
    assert!(volume_error < 0.02, "volume {v_gpu} vs {v_cpu}");
    assert!(
        m_cpu.x > 0.01 && m_gpu.x > 0.01,
        "both pushed along +x: {m_cpu} {m_gpu}"
    );
    assert!(
        velocity_error <= 0.1 * m_cpu.length() + 0.01,
        "mean velocity {m_gpu} vs {m_cpu}"
    );
    assert!(paddle_gap < 1e-4, "paddle {paddle_gap} m apart");
    // No particle ends inside the paddle on the GPU either.
    let paddle = gpu_sim.moving_solids()[0];
    let dx = gpu_sim.dims().dx;
    let inside = gpu_sim
        .positions()
        .iter()
        .filter(|p| paddle.distance(**p) < -0.25 * dx)
        .count();
    assert!(
        inside * 200 <= gpu_sim.positions().len(),
        "{inside} particles inside the paddle"
    );

    // Too many solids for the uniform are refused.
    let mut gpu = gpu;
    let many = vec![gpu_sim.moving_solids()[0]; regia_fluid::gpu::MAX_GPU_SOLIDS + 1];
    let e = gpu.set_moving_solids(&many).expect_err("too many");
    assert_eq!(e.code(), "fluid_gpu_too_many_solids");
}

/// FNV-1a over the readback bits, so a pinned run is one constant.
fn readback_hash(pos: &[Vec3], vel: &[Vec3]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let write = |h: &mut u64, bytes: &[u8]| {
        for b in bytes {
            *h = (*h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    for v in pos.iter().chain(vel) {
        for x in v.to_array() {
            write(&mut h, &x.to_bits().to_le_bytes());
        }
    }
    h
}

/// The paddle tank with the paddle spinning about y instead of shoving along x.
fn spinning_tank() -> FluidSim {
    let mut sim = paddle_tank_base();
    let mut moving = sim.moving_solids()[0];
    moving.linear_velocity = Vec3::ZERO;
    moving.angular_velocity = Vec3::new(0.0, 1.0, 0.0);
    sim.set_moving_solids(vec![moving]).expect("spinning");
    sim
}

/// A tank with a paddle (a kinematic box, open below and above) and settled liquid, like
/// `paddle_tank` but with the paddle at rest.
fn paddle_tank_base() -> FluidSim {
    use regia_fluid::sim::{Emitter, EmitterKind};
    use regia_fluid::solid::{MovingSolid, SolidShape};
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.8, 0.4, 0.1),
        cell_size: 0.025,
        dt: 1.0 / 120.0,
        max_substeps: 1,
        separation: 0.0,
        reseed: false,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("tank");
    sim.add_emitter(Emitter {
        shape: regia_fluid::sim::Shape::Box {
            center: Vec3::new(0.4, 0.1, 0.05),
            half_extents: Vec3::new(0.4, 0.1, 0.05),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    let paddle = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(0.03, 0.08, 0.2),
        },
        Vec3::new(0.3, 0.13, 0.05),
    );
    sim.set_moving_solids(vec![paddle]).expect("paddle");
    for _ in 0..5 {
        sim.step();
    }
    sim
}

/// (ADR-4753) A spinning solid on the GPU path: the kernels build the incremental rotation from
/// software sin/cos (WGSL's builtins are not bit-stable across devices). On the reference device
/// (lavapipe, a CPU adapter) the readback must match the pinned bits exactly. Another device may
/// differ in the last bits (WGSL division is 2.5 ULP; ADR-5598: an RTX 5090 reads back
/// `0x2245_ded4_33a7_a922`), so there only the same-device repeat below is exact.
#[test]
fn spinning_solid_readback_matches_pinned_particles() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let reference = device.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
    let mut sim = spinning_tank();
    let gpu = GpuFluid::new(&device.device, &device.queue, &sim).expect("upload");
    let steps = 40;
    gpu.step(steps).expect("gpu steps");
    gpu.sync_into(&mut sim, u64::from(steps)).expect("read back");
    assert!(sim.positions().iter().all(|p| p.is_finite()));
    let hash = readback_hash(sim.positions(), sim.velocities());
    eprintln!("spinning paddle readback after {steps} steps: hash {hash:#018x}");
    if reference {
        assert_eq!(hash, 0xff12_e498_1ff1_701b, "pinned readback");
    }

    // The same upload on the same device repeats itself bit for bit.
    let mut again = spinning_tank();
    let gpu2 = GpuFluid::new(&device.device, &device.queue, &again).expect("upload");
    gpu2.step(steps).expect("gpu steps");
    gpu2.sync_into(&mut again, u64::from(steps)).expect("read back");
    assert_eq!(readback_hash(again.positions(), again.velocities()), hash);
}

/// (ADR-4753) A particle faster than the p2g fixed-point scatter's i32 range: the shader
/// saturates the conversion, the upload clamps to the same ceiling, and the readback is the
/// saturated value on every device (WGSL's out-of-range float-to-i32 conversion is
/// indeterminate).
#[test]
fn fast_particle_p2g_stays_in_i32() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    // A long, one-cell-high domain: the particle scatters to exactly one face with weight 1.0
    // (it sits on the face sample) and crosses no wall in one step.
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(1200.0, 0.5, 0.5),
        cell_size: 0.5,
        dt: 1.0 / 60.0,
        max_substeps: 4,
        gravity: Vec3::ZERO,
        reseed: false,
        separation: 0.0,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params.clone()).expect("sim");
    sim.add_particles(&[(Vec3::new(10.0, 0.25, 0.25), Vec3::new(40000.0, 0.0, 0.0))]);

    let run = |sim: &FluidSim| {
        let gpu = GpuFluid::new(&device.device, &device.queue, sim).expect("upload");
        gpu.step(1).expect("gpu step");
        gpu.read_back().expect("read back")
    };
    // The saturate contract (ADR-4753): the scatter clamps into the i32 fixed-point range, so no
    // component reads back above the ceiling, the run repeats itself bit for bit, and the whole
    // readback is pinned — before the fix the float-to-i32 conversion of the out-of-range
    // scatter was device-defined, so two devices could disagree (the pre-fix readback on the
    // reference host is recorded in the lane report).
    let (pos, vel) = run(&sim);
    let (pos2, vel2) = run(&sim);
    assert!(pos.iter().chain(&pos2).all(|p| p.is_finite()));
    assert_eq!(pos, pos2, "two runs on one device agree");
    assert_eq!(vel, vel2);
    for v in vel.iter().chain(&vel2) {
        for x in v.to_array() {
            assert!(
                x.is_finite() && x.abs() <= regia_fluid::gpu::MAX_FIXEDPOINT_SPEED,
                "component {x} within the saturate ceiling"
            );
        }
    }
    eprintln!(
        "fast particle readback: pos {:?} vel {:?} bits {:08x?}",
        pos[0],
        vel[0],
        vel[0].to_array().map(f32::to_bits)
    );
    assert_eq!(
        pos[0].x, 10.0,
        "pinned position (the projected field holds the particle)"
    );
    // Re-pinned with the fluid-correctness fixes (audit G-fluid #3): the lone particle keeps its
    // saturated speed exactly. The old pin (0x4621febc, 10367.684 m/s) came from the old
    // `solid_liquid` rule (mean face open fraction under one half), which in this one-cell-high
    // domain counted both air neighbours of the particle's cell as liquid inside a solid, because
    // the domain walls close four of their six faces.
    assert_eq!(
        vel[0].x,
        regia_fluid::gpu::MAX_FIXEDPOINT_SPEED,
        "the lone particle keeps its speed"
    );
    assert_eq!(vel[0].x.to_bits(), 0x46ff_fe00, "pinned speed bits");
}

fn headless() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// (Audit G-fluid #8) Many coincident particles on one face sample: the fixed-point velocity sum
/// is far past `i32::MAX` (1000 particles x 100 m/s x 65536), so a 32-bit accumulator wraps and
/// the face velocity changes sign. Pure PIC (`flip_ratio` 0) reads the face velocity straight
/// back into the particles.
#[test]
fn clumped_face_velocity_does_not_wrap() {
    let device = headless();
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(20.0, 0.5, 0.5),
        cell_size: 0.5,
        dt: 1.0 / 60.0,
        max_substeps: 1,
        gravity: Vec3::ZERO,
        flip_ratio: 0.0,
        reseed: false,
        separation: 0.0,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("sim");
    // On a u-face sample: each particle scatters weight 1.0 to exactly one u face.
    let clump = vec![(Vec3::new(10.0, 0.25, 0.25), Vec3::new(100.0, 0.0, 0.0)); 1000];
    assert_eq!(sim.add_particles(&clump), clump.len());
    let gpu = GpuFluid::new(&device.device, &device.queue, &sim).expect("upload");
    gpu.step(1).expect("gpu step");
    let (pos, vel) = gpu.read_back().expect("read back");
    eprintln!("clumped face: vel {:?} pos {:?}", vel[0], pos[0]);
    for v in &vel {
        assert!(
            (v.x - 100.0).abs() < 1e-3 && v.y == 0.0 && v.z == 0.0,
            "particle velocity {v} (expected 100 m/s along x)"
        );
    }
}

/// A settled pool (5 CPU steps) around a still kinematic solid at `center`.
fn pool_with_solid(
    shape: regia_fluid::solid::SolidShape,
    center: Vec3,
    colliders: Vec<regia_fluid::solid::Collider>,
    fill_lo: f32,
) -> FluidSim {
    use regia_fluid::sim::{Emitter, EmitterKind, Shape};
    use regia_fluid::solid::MovingSolid;
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.4, 0.4, 0.1),
        cell_size: 0.025,
        dt: 1.0 / 120.0,
        max_substeps: 1,
        separation: 0.0,
        reseed: false,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::with_colliders(params, colliders).expect("pool");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.2, fill_lo + 0.1, 0.05),
            half_extents: Vec3::new(0.2, 0.1, 0.05),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    sim.set_moving_solids(vec![MovingSolid::kinematic(shape, center)])
        .expect("solid");
    for _ in 0..5 {
        sim.step();
    }
    sim
}

fn mean_speed(v: &[Vec3]) -> f32 {
    v.iter().map(|x| x.length()).sum::<f32>() / v.len().max(1) as f32
}

/// (Audit G-fluid #3) A still sphere smaller than its cell's corner nodes (radius 0.8 dx, centred
/// on a cell inside the pool): every face of that cell is fully open, but its centre is inside the
/// solid. The GPU must count it as liquid like the CPU does; as air, its `p = 0` sits inside the
/// pool and pulls the liquid toward the sphere.
#[test]
fn small_solid_cell_is_liquid() {
    use regia_fluid::solid::SolidShape;
    let device = headless();
    let dx = 0.025;
    // The centre of cell (8, 3, 1).
    let center = Vec3::new(8.5 * dx, 3.5 * dx, 1.5 * dx);
    let shape = SolidShape::Sphere { radius: 0.8 * dx };
    let mut cpu = pool_with_solid(shape, center, Vec::new(), 0.0);
    let mut gpu_sim = cpu.clone();
    let steps = 40;
    let gpu = GpuFluid::new(&device.device, &device.queue, &gpu_sim).expect("upload");
    gpu.step(steps).expect("gpu steps");
    gpu.sync_into(&mut gpu_sim, u64::from(steps)).expect("read back");
    for _ in 0..steps {
        cpu.step();
    }
    let (s_cpu, s_gpu) = (mean_speed(cpu.velocities()), mean_speed(gpu_sim.velocities()));
    let (v_cpu, v_gpu) = (cpu.liquid_volume(), gpu_sim.liquid_volume());
    eprintln!("small solid: mean speed gpu {s_gpu} cpu {s_cpu}, volume gpu {v_gpu} cpu {v_cpu}");
    assert!(gpu_sim.positions().iter().all(|p| p.is_finite()));
    assert!(
        s_gpu <= 2.0 * s_cpu + 0.01,
        "the pool around a still sphere stirs on the GPU: {s_gpu} m/s (cpu {s_cpu})"
    );
    assert!((v_gpu - v_cpu).abs() / v_cpu < 0.02, "volume {v_gpu} vs {v_cpu}");
}

/// (Audit G-fluid #9) A box sliding inside a static floor, entirely below the floor's surface: the
/// floor is the nearer solid on every face it shares with the box, so (as on the CPU) those
/// closed faces stay at rest and the liquid on the floor does not move with the hidden box.
#[test]
fn buried_box_does_not_drag_the_floor_liquid() {
    use regia_fluid::solid::{Collider, SolidShape};
    let device = headless();
    let floor = Collider::Box {
        center: Vec3::new(0.2, 0.05, 0.05),
        half_extents: Vec3::new(0.3, 0.05, 0.3),
    };
    let shape = SolidShape::Box {
        half_extents: Vec3::new(0.1, 0.02, 0.2),
    };
    let mut cpu = pool_with_solid(shape, Vec3::new(0.15, 0.075, 0.05), vec![floor], 0.1);
    let mut moving = cpu.moving_solids()[0];
    moving.linear_velocity = Vec3::new(0.5, 0.0, 0.0);
    cpu.set_moving_solids(vec![moving]).expect("sliding box");
    let mut gpu_sim = cpu.clone();
    let steps = 30;
    let gpu = GpuFluid::new(&device.device, &device.queue, &gpu_sim).expect("upload");
    gpu.step(steps).expect("gpu steps");
    gpu.sync_into(&mut gpu_sim, u64::from(steps)).expect("read back");
    for _ in 0..steps {
        cpu.step();
    }
    let (m_cpu, m_gpu) = (mean(cpu.velocities()), mean(gpu_sim.velocities()));
    eprintln!("buried box: mean velocity gpu {m_gpu} cpu {m_cpu}");
    assert!(gpu_sim.positions().iter().all(|p| p.is_finite()));
    assert!(
        m_cpu.x.abs() < 0.005,
        "cpu liquid moved with the buried box: {m_cpu}"
    );
    assert!(
        m_gpu.x.abs() < 0.005,
        "gpu liquid moved with the buried box: {m_gpu}"
    );
}

/// The GPU path computes no solid impulses, so after GPU steps are handed back the last CPU step's
/// impulses must not be reported as the last step's.
#[test]
fn gpu_sync_drops_the_last_cpu_impulses() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let mut sim = spinning_tank();
    // `spinning_tank` ends with `set_moving_solids`, which empties the impulses: one CPU step
    // with the paddle in the liquid records them again.
    sim.step();
    assert!(
        !sim.solid_impulses().is_empty(),
        "the CPU steps with a paddle in the liquid record an impulse"
    );
    let gpu = GpuFluid::new(&device.device, &device.queue, &sim).expect("upload");
    gpu.step(2).expect("gpu steps");
    gpu.sync_into(&mut sim, 2).expect("read back");
    assert!(
        sim.solid_impulses().is_empty(),
        "stale CPU impulses after a GPU sync: {:?}",
        sim.solid_impulses()
    );
}

/// A particle with a non-finite velocity or position next to one at rest: the rest neighbour stays
/// at rest. Before ADR-5730 the NaN velocity survived the upload (`f32::clamp` keeps NaN) and
/// lavapipe read the neighbour back at about 1997 m/s after one step. Since ADR-5730 the upload
/// drops both particles, so this test covers the public path only; the kernel's own `finite3`
/// guard is covered by `gpu::tests::kernel_guard_skips_a_non_finite_particle_the_upload_kept`,
/// which uploads them unfiltered (ADR-5875).
#[test]
fn gpu_ignores_a_non_finite_particle() {
    let device = headless();
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(0.4),
        cell_size: 0.1,
        dt: 1.0 / 60.0,
        max_substeps: 1,
        gravity: Vec3::ZERO,
        separation: 0.0,
        reseed: false,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("sim");
    sim.set_particles(
        vec![
            Vec3::new(0.15, 0.15, 0.15),
            Vec3::new(0.16, 0.15, 0.15),
            Vec3::new(f32::NAN, 0.15, 0.15),
        ],
        vec![Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ZERO],
    );
    let gpu = GpuFluid::new(&device.device, &device.queue, &sim).expect("upload");
    gpu.step(1).expect("gpu step");
    let (pos, vel) = gpu.read_back().expect("read back");
    let speed = vel[0].length();
    assert!(
        speed < 1.0 && pos[0].is_finite(),
        "non-finite particle launched its neighbour to vel {:?} pos {:?}",
        vel[0],
        pos[0]
    );
}

/// The upload drops a particle whose position or velocity is not finite, as the CPU solver does
/// at the end of a step, so no backend's shader ever sees the NaN (`f32::clamp` keeps NaN, and a
/// fast-math shader compiler can fold a `v == v` guard; `finite3` tests the exponent bits).
#[test]
fn gpu_upload_drops_a_non_finite_particle() {
    let device = headless();
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(0.4),
        cell_size: 0.1,
        dt: 1.0 / 60.0,
        max_substeps: 1,
        gravity: Vec3::ZERO,
        separation: 0.0,
        reseed: false,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("sim");
    sim.set_particles(
        vec![
            Vec3::new(0.15, 0.15, 0.15),
            Vec3::new(f32::NAN, 0.15, 0.15),
            Vec3::new(0.16, 0.15, 0.15),
        ],
        vec![Vec3::ZERO, Vec3::ZERO, Vec3::new(f32::INFINITY, 0.0, 0.0)],
    );
    let gpu = GpuFluid::new(&device.device, &device.queue, &sim).expect("upload");
    gpu.step(1).expect("gpu step");
    let (pos, vel) = gpu.read_back().expect("read back");
    assert_eq!(
        pos.len(),
        1,
        "a non-finite particle reached the GPU: {pos:?} {vel:?}"
    );
    assert!(pos[0].is_finite() && vel[0].is_finite());
}

/// `fluid_flip.wgsl` passes the engine shader gate: naga validation, the ADR-026 layout scan,
/// `spirv-val --target-env vulkan1.3` (`spirv_validate`) and module creation on the device.
#[test]
fn fluid_flip_wgsl_passes_spirv_validation() {
    regia_render::validate_wgsl_modules(&[("fluid_flip.wgsl", include_str!("../src/fluid_flip.wgsl"))])
        .expect("fluid_flip.wgsl validates");
}
