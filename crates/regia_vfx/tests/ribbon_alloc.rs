// SPDX-License-Identifier: MPL-2.0
//! A ribbon emitter's frame allocates nothing after warm-up (ADR-2800): the scene update that
//! spawns, moves the spawns into the CPU ribbon track, steps it and rebuilds the strip, counted
//! by a global allocator (the pattern of `regia_physics/tests/cloth_alloc.rs`, ADR-1672). Its
//! own test binary; the counter is per thread, so the harness's threads do not count.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_core::math::Vec3;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

// SAFETY: forwards to the system allocator unchanged; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract is passed through.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed through.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract is passed through.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn a_ribbon_frame_allocates_nothing_after_warm_up() {
    let def = VfxSystemDef::from_ron(&format!(
        r##"(
    version: {VFX_FORMAT_VERSION},
    name: "trail",
    emitters: [(
        name: "ribbon",
        spawn: (mode: Rate(90.0), lifetime: (min: 0.6, max: 1.0)),
        init: (shape: Sphere(radius: (min: 0.0, max: 0.1)), velocity: (mode: Radial(speed: (min: 0.2, max: 0.6))), size: (min: 0.3, max: 0.3)),
        update: (gravity: (0.0, 1.0, 0.0), drag: 0.5, vortex: Some((axis: (0.0, 1.0, 0.0), strength: 1.0)),
                 kill_volume: Some((shape: Box(half_extents: (5.0, 5.0, 5.0)), kill: Outside)),
                 size_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.2)]),
        render: (mode: Ribbon, blend: Additive, ribbon: (max_points: 48, width: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.0)],
                 color: [(t: 0.0, color: "#ffffff"), (t: 1.0, color: "#ff400000")], texture: Stretch)),
    )],
)"##
    ))
    .expect("ribbon effect parses");
    let mut library = VfxLibrary::new();
    library.add(def).expect("valid");
    let mut scene = VfxScene::new(library);
    let id = scene.spawn("trail", Vec3::ZERO, Vec3::Z, 11).expect("spawned");
    let dt = 1.0 / 60.0;
    let frame = |scene: &mut VfxScene, i: u32| {
        let t = i as f32 * dt;
        if let Some(instance) = scene.instance_mut(id) {
            instance.set_transform(Vec3::new(t.sin() * 3.0, 1.0, t.cos() * 3.0), Vec3::Z, dt);
        }
        scene.update(
            dt,
            &LodInput {
                camera_pos: Vec3::new(0.0, 4.0, -12.0),
            },
            100_000,
        );
    };
    for i in 0..600 {
        frame(&mut scene, i);
    }
    assert!(scene.ribbon_stats().points > 20, "the ribbon is running");
    let before = allocations();
    for i in 600..1200 {
        frame(&mut scene, i);
    }
    let after = allocations();
    assert!(scene.ribbon_stats().vertices > 0);
    assert_eq!(after - before, 0, "allocations in 600 warm ribbon frames");
}
