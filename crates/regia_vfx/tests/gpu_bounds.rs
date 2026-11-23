// SPDX-License-Identifier: MPL-2.0
//! GPU particle bounds (P-225 (2), ADR-8355): the reduction over the live pool on a headless
//! adapter (lavapipe in the gate) against every particle read back, and against the CPU measured
//! bounds.

use regia_core::math::{Aabb, Vec3};
use regia_render::RenderDevice;
use regia_vfx::bounds::{measured_bounds, particle_extent};
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::probe::ProbeSettings;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxGpu, VfxProfile};

const DT: f32 = 1.0 / 64.0;
const ORIGIN: Vec3 = Vec3::new(2.0, 1.0, -3.0);

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A billboard cloud and a stretched streak emitter; `update` goes into both.
fn system(update: &str) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "b",
    system: (budget: 100000),
    emitters: [
        (
            name: "cloud",
            spawn: (mode: Burst(300), lifetime: (min: 3.0, max: 3.0)),
            init: (
                shape: Sphere(radius: (min: 0.2, max: 1.5)),
                velocity: (mode: Radial(speed: (min: 0.5, max: 3.0))),
                size: (min: 0.2, max: 1.4),
            ),
            update: ({update}),
            render: (mode: Billboard),
        ),
        (
            name: "streaks",
            spawn: (mode: Burst(120), lifetime: (min: 3.0, max: 3.0)),
            init: (
                shape: Point,
                velocity: (mode: Directional(dir: (0.0, 1.0, 0.0), spread: 40.0, speed: (min: 4.0, max: 9.0))),
                size: (min: 0.1, max: 0.3),
            ),
            update: ({update}),
            render: (mode: Stretched, stretch: (velocity_scale: 0.2, min_length: 0.05)),
        ),
    ],
)"#
    ))
    .expect("fixture parses")
}

/// What [`play`] returns: the GPU bounds of the instance's emitters, of every other record, and
/// the particles read back as `(emitter index, centre, extent)`.
type Played = (Vec<Option<Aabb>>, Vec<Option<Aabb>>, Vec<(usize, Vec3, f32)>);

/// Plays `def` for `frames` steps on a fresh context and reads its bounds and particles.
fn play(def: &VfxSystemDef, frames: u32) -> Played {
    let device = device();
    let profile = VfxProfile {
        particle_capacity: 8192,
        ..VfxProfile::desktop_high()
    };
    let mut gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
    let mut library = VfxLibrary::new();
    library.add(def.clone()).expect("valid def");
    let mut scene = VfxScene::new(library);
    let id = scene.spawn(&def.name, ORIGIN, Vec3::Z, 7).expect("spawned");
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 20.0, 30.0), ORIGIN);
    for _ in 0..frames {
        scene.update(DT, &LodInput { camera_pos: ORIGIN }, gpu.capacity());
        scene.upload(&mut gpu);
        gpu.set_frame(&camera, DT, 64, 64);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        gpu.simulate(&mut encoder);
        device.queue.submit([encoder.finish()]);
    }
    let (base, count) = scene.gpu_emitter_run(id).expect("the instance holds records");
    assert_eq!(count as usize, def.emitters.len());
    let records = gpu.read_particle_bounds().expect("bounds read back");
    let bounds = (0..count).map(|i| records[(base + i) as usize]).collect();
    let others = records
        .iter()
        .enumerate()
        .filter(|(slot, _)| !(base..base + count).contains(&(*slot as u32)))
        .map(|(_, b)| *b)
        .collect();
    let particles = gpu
        .read_particles()
        .into_iter()
        .filter(|p| (base..base + count).contains(&p.emitter_slot))
        .map(|p| {
            let index = (p.emitter_slot - base) as usize;
            let e = &def.emitters[index];
            let mode = regia_vfx::bounds::render_mode_code(e.render.mode);
            let velocity = Vec3::from_array(p.velocity);
            let stretch = [e.render.stretch.velocity_scale, e.render.stretch.min_length];
            // No size curve in these fixtures: the drawn size is the base size.
            let r = particle_extent(mode, p.size, velocity.length(), stretch);
            (index, Vec3::from_array(p.position), r)
        })
        .collect();
    (bounds, others, particles)
}

/// Acceptance (P-225 (2)): every live particle, grown by its drawn extent, lies inside its
/// emitter's GPU bounds, and the bounds are the tight hull (each face touches a particle), with
/// every motion module on.
#[test]
fn gpu_bounds_contain_every_particle() {
    let def = system(
        "gravity: (0.0, -2.0, 0.0), drag: 0.3, curl: Some((6.0, 0.8, 1.0)), \
         vortex: Some((axis: (0.0, 1.0, 0.0), strength: 2.0)), \
         attractor: Some((position: (0.0, 3.0, 0.0), strength: 6.0))",
    );
    let (bounds, others, particles) = play(&def, 48);
    assert!(particles.len() > 300, "{} live particles", particles.len());
    let eps = 1e-4;
    for (index, emitter) in def.emitters.iter().enumerate() {
        let mine: Vec<_> = particles.iter().filter(|p| p.0 == index).collect();
        assert!(!mine.is_empty(), "{}: no live particle", emitter.name);
        let b = bounds[index].unwrap_or_else(|| panic!("{}: no GPU bounds", emitter.name));
        let mut hull = Aabb::EMPTY;
        for &&(_, c, r) in &mine {
            let lo = c - Vec3::splat(r);
            let hi = c + Vec3::splat(r);
            assert!(
                lo.cmpge(b.min - Vec3::splat(eps)).all() && hi.cmple(b.max + Vec3::splat(eps)).all(),
                "{}: particle {c:?} r {r} outside {b:?}",
                emitter.name
            );
            hull.extend(lo);
            hull.extend(hi);
        }
        assert!(
            (hull.min - b.min).abs().max_element() < 1e-3,
            "{}: {hull:?} vs {b:?}",
            emitter.name
        );
        assert!(
            (hull.max - b.max).abs().max_element() < 1e-3,
            "{}: {hull:?} vs {b:?}",
            emitter.name
        );
    }
    // Every record with nothing alive reports no bounds.
    assert_eq!(
        others.len(),
        regia_vfx::gpu::layout::MAX_EMITTERS as usize - def.emitters.len()
    );
    assert!(
        others.iter().all(Option::is_none),
        "a record without particles has bounds"
    );
}

/// The CPU measured bounds follow the same forces as the kernel: with gravity, drag, vortex and
/// attractor (curl left out: its lattice noise amplifies float differences) they match the GPU
/// reduction within a few centimetres after 0.75 s.
#[test]
fn cpu_measured_bounds_match_the_gpu_reduction() {
    let def = system(
        "gravity: (0.0, -2.0, 0.0), drag: 0.3, vortex: Some((axis: (0.0, 1.0, 0.0), strength: 2.0)), \
         attractor: Some((position: (0.0, 3.0, 0.0), strength: 6.0))",
    );
    let (gpu, _, _) = play(&def, 48);
    let settings = ProbeSettings {
        dt: DT,
        seed: 7,
        ground: None,
        ..ProbeSettings::default()
    };
    let cpu = measured_bounds(&def, ORIGIN, Vec3::Z, settings, 48);
    for (index, emitter) in def.emitters.iter().enumerate() {
        let g = gpu[index].expect("gpu bounds");
        let c = cpu.emitters[index].expect("cpu bounds");
        assert!(
            (g.min - c.min).abs().max_element() < 0.05 && (g.max - c.max).abs().max_element() < 0.05,
            "{}: gpu {g:?} cpu {c:?}",
            emitter.name
        );
    }
}
