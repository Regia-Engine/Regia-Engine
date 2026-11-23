// SPDX-License-Identifier: MPL-2.0
//! `VortexDef::radial` on the GPU (ADR-5629): the update kernel adds it to the built-in outward
//! `0.25 × strength`, so a negative value pulls particles toward the vortex axis and a positive
//! one pushes them out. Before ADR-5629 the kernel never read the field: all three pools below
//! were identical. Headless adapter (lavapipe in the gate), the pattern of `kill_volumes_gpu.rs`.

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{PoolParticle, VfxCamera, VfxGpu, VfxProfile};

const DT: f32 = 1.0 / 64.0;

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A still ring of particles around the +Y axis with a vortex of strength 1 and `radial`.
fn system(radial: f32) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "swirl",
    system: (budget: 100000),
    emitters: [(
        name: "ring",
        spawn: (mode: Rate(300.0), lifetime: (min: 3.0, max: 3.0)),
        init: (
            shape: Sphere(radius: (min: 2.0, max: 2.0), surface: true),
            size: (min: 0.1, max: 0.1),
        ),
        update: (vortex: Some((axis: (0.0, 1.0, 0.0), strength: 1.0, radial: {radial:?}))),
        render: (mode: Billboard),
    )],
)"#
    ))
    .expect("test system parses")
}

/// Plays `def` for `steps` frames on the GPU and returns the live pool.
fn gpu_pool(def: &VfxSystemDef, steps: u32) -> Vec<PoolParticle> {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 8192,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let mut library = VfxLibrary::new();
    library.add(def.clone()).expect("valid def");
    let mut scene = VfxScene::new(library);
    scene.spawn(&def.name, Vec3::ZERO, Vec3::Z, 7).expect("spawned");
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 10.0, -20.0), Vec3::ZERO);
    for _ in 0..steps {
        scene.update(
            DT,
            &LodInput {
                camera_pos: Vec3::ZERO,
            },
            gpu.capacity(),
        );
        scene.upload(&mut gpu);
        gpu.set_frame(&camera, DT, 64, 64);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        gpu.simulate(&mut encoder);
        gpu.record_stats_copy(&mut encoder);
        device.queue.submit([encoder.finish()]);
    }
    gpu.read_particles()
}

/// Mean distance of the pool from the +Y axis through the origin.
fn mean_axis_distance(pool: &[PoolParticle]) -> f32 {
    assert!(pool.len() > 100, "the ring spawned: {} particles", pool.len());
    let sum: f32 = pool
        .iter()
        .map(|p| (p.position[0] * p.position[0] + p.position[2] * p.position[2]).sqrt())
        .sum();
    sum / pool.len() as f32
}

#[test]
fn the_vortex_radial_term_pulls_in_and_pushes_out_on_the_gpu() {
    let steps = 90;
    let base = mean_axis_distance(&gpu_pool(&system(0.0), steps));
    let inward = mean_axis_distance(&gpu_pool(&system(-3.0), steps));
    let outward = mean_axis_distance(&gpu_pool(&system(3.0), steps));
    // Over ~1.4 s a 3 m/s² radial term moves the oldest particles metres; the means differ by
    // far more than float noise.
    assert!(
        inward + 0.2 < base,
        "radial -3 pulls toward the axis: {inward} vs {base}"
    );
    assert!(
        outward > base + 0.2,
        "radial +3 pushes away from the axis: {outward} vs {base}"
    );
}
