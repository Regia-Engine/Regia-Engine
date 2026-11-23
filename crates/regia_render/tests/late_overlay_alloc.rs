// SPDX-License-Identifier: MPL-2.0
//! The late overlay warm (ADR-5131) allocates nothing on a frame where the overlay did not change
//! (fixrev-demo-warm-k, review finding 5).
//!
//! `warm_late_overlay_layers` runs in `PreUpdate` of every windowed app for its whole life. Once the
//! warm queue had started it built two `Vec`s per frame (the layer names and the shared layers)
//! just to find that every layer was warmed already. The heap ledger
//! (`regia_test_utils::heap_ledger`, ADR-2987) counts this thread's allocations inside a
//! `Gameplay` scope; the system's runs are compared with as many runs of a system that does
//! nothing (the run call's own cost). A layer installed later still gets its job.
//!
//! Its own test binary: it installs the tagging allocator. Needs the headless device (lavapipe):
//! the queue only takes late layers once it has started.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::memory::{self, Subsystem};
use regia_ecs::ecs::system::{IntoSystem, System};
use regia_render::overlay::OverlayDraw;
use regia_render::{PipelineWarmQueue, RenderDevice, WarmTier, WindowOverlay, late_overlay_warm_system};

regia_test_utils::install_tagging_allocator!();

const FRAMES: u64 = 64;

/// An overlay layer that draws nothing.
struct Quiet;

impl OverlayDraw for Quiet {
    fn draw(
        &mut self,
        _: &RenderDevice,
        _: &mut wgpu::CommandEncoder,
        _: &wgpu::TextureView,
        _: wgpu::TextureFormat,
        _: u32,
        _: u32,
    ) {
    }
}

fn run(system: &mut impl System<In = (), Out = ()>, world: &mut World) {
    if let Err(err) = system.run((), world) {
        panic!("system: {err}");
    }
}

/// Allocations this thread made in `FRAMES` runs of `system`.
fn allocations_over_frames(system: &mut impl System<In = (), Out = ()>, world: &mut World) -> u64 {
    let total = || {
        regia_test_utils::heap_ledger::require_ledger()
            .subsystem(Subsystem::Gameplay)
            .total_allocations
    };
    let before = total();
    for _ in 0..FRAMES {
        memory::scoped(Subsystem::Gameplay, || run(system, world));
    }
    total() - before
}

fn boot_jobs(queue: &PipelineWarmQueue) -> u32 {
    queue.progress().tiers[WarmTier::Boot as usize].jobs_total
}

fn idle() {}

#[test]
fn a_steady_overlay_costs_the_late_warm_no_allocation_per_frame() {
    // The binary's one shared device, one test at a time (ADR-7483).
    let device = match regia_test_utils::serial_gpu::serial_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("note: no GPU adapter ({err}); skipped");
            return;
        }
    };
    let queue = PipelineWarmQueue::new(1);
    queue.start(&device);
    let mut world = World::new();
    world.insert_resource(queue.clone());
    world.insert_resource(WindowOverlay::default());
    world.resource_mut::<WindowOverlay>().set(Box::new(Quiet));
    world
        .resource_mut::<WindowOverlay>()
        .insert(10, "hud", Box::new(Quiet));

    let mut warm = late_overlay_warm_system();
    warm.initialize(&mut world);
    let mut idle = IntoSystem::into_system(idle);
    idle.initialize(&mut world);
    // The first runs queue the layers' job and settle.
    for _ in 0..4 {
        run(&mut warm, &mut world);
        run(&mut idle, &mut world);
    }
    assert_eq!(boot_jobs(&queue), 1, "one job for the two layers");
    assert!(
        queue.wait_tier(WarmTier::Boot, Duration::from_secs(120)),
        "boot tier"
    );

    let baseline = allocations_over_frames(&mut idle, &mut world);
    let steady = allocations_over_frames(&mut warm, &mut world);
    println!("{FRAMES} frames: late overlay warm {steady} allocations, an idle system {baseline}");
    assert!(
        steady.saturating_sub(baseline) < FRAMES,
        "fewer allocations than frames: {steady} against {baseline} for an idle system"
    );
    assert_eq!(boot_jobs(&queue), 1, "nothing new to warm");

    // A layer installed now still gets its job, once.
    world
        .resource_mut::<WindowOverlay>()
        .insert(20, "late", Box::new(Quiet));
    run(&mut warm, &mut world);
    run(&mut warm, &mut world);
    assert_eq!(boot_jobs(&queue), 2, "one job for the late layer");
    queue.shutdown();
}

/// fixrev-demo-warm-verify-k: review findings 4 and 5 together, on one system that keeps its state between frames
/// like the app's `PreUpdate` system. After it has looked at a steady overlay it looks again only when the overlay
/// changed; a layer replaced under the same name (`set`, or `insert` with a name in use) is such a change and is a new
/// layer, so it gets its own job, once.
#[test]
fn a_layer_replaced_under_its_name_on_a_running_system_gets_one_job() {
    // The binary's one shared device, one test at a time (ADR-7483).
    let device = match regia_test_utils::serial_gpu::serial_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("note: no GPU adapter ({err}); skipped");
            return;
        }
    };
    let queue = PipelineWarmQueue::new(1);
    queue.start(&device);
    let mut world = World::new();
    world.insert_resource(queue.clone());
    world.insert_resource(WindowOverlay::default());
    world.resource_mut::<WindowOverlay>().set(Box::new(Quiet));
    world
        .resource_mut::<WindowOverlay>()
        .insert(10, "hud", Box::new(Quiet));

    let mut warm = late_overlay_warm_system();
    warm.initialize(&mut world);
    for _ in 0..4 {
        run(&mut warm, &mut world);
    }
    assert_eq!(boot_jobs(&queue), 1, "one job for the two layers");

    world.resource_mut::<WindowOverlay>().set(Box::new(Quiet));
    for _ in 0..4 {
        run(&mut warm, &mut world);
    }
    assert_eq!(boot_jobs(&queue), 2, "one job for the replaced ui layer");

    world
        .resource_mut::<WindowOverlay>()
        .insert(10, "hud", Box::new(Quiet));
    for _ in 0..4 {
        run(&mut warm, &mut world);
    }
    assert_eq!(boot_jobs(&queue), 3, "one job for the replaced hud layer");
    assert_eq!(
        world.resource::<WindowOverlay>().layer_names().len(),
        2,
        "replaced, not added"
    );
    queue.shutdown();
}
