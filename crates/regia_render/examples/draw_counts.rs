// SPDX-License-Identifier: MPL-2.0
//! Prints the bindless step 1 draw counters (ADR-316) for the lookdev hard-surface scenes: the
//! hull close-up and the 12-hull feature-cost grid, at the third steady frame on a headless
//! device. One line per pass and a total, then where material constants live and, at bindless
//! level 3 (`REGIA_BINDLESS_LEVEL=3`, ADR-863), what the material texture table holds.
//!
//! `cargo run --release -p regia_render --example draw_counts [WIDTH HEIGHT]` (default 1280×720).
//! Prints `skip: <reason>` and exits 0 without a GPU adapter. The counts do not depend on timing, so
//! two runs on one adapter and tier print the same numbers.

use std::process::ExitCode;

use regia_render::hard_surface::{self, HardSurfaceFeature};
use regia_render::{BindlessCaps, FrameQuality, FrameRenderer, LabScene, RenderDevice, RenderError};

fn report(
    device: &RenderDevice,
    name: &str,
    scene: &LabScene,
    quality: FrameQuality,
) -> Result<(), RenderError> {
    let mut renderer = FrameRenderer::new();
    let frame = scene.as_frame_scene_with_quality(quality);
    for _ in 0..3 {
        renderer.render_offscreen(device, &frame)?;
    }
    let counts = renderer.draw_counts();
    for (pass, c) in counts.passes() {
        println!(
            "{name} {pass}: draws {} pipeline_switches {} bind_group_switches {:?} material_batches {}",
            c.draws, c.pipeline_switches, c.bind_group_switches, c.material_batches
        );
    }
    let total = counts.total();
    println!(
        "{name} total: draws {} pipeline_switches {} bind_group_switches {:?} material_batches {}",
        total.draws, total.pipeline_switches, total.bind_group_switches, total.material_batches
    );
    match renderer.material_arena_stats() {
        Some(arena) => println!(
            "{name} material constants: storage arena, {} live of {} slots, stride {} B, {} buffer(s)",
            arena.live, arena.capacity, arena.stride, arena.buffer_creates
        ),
        None => println!("{name} material constants: per-material uniform buffers"),
    }
    if let Some(table) = renderer.texture_table_stats() {
        println!(
            "{name} texture table: {} live of {} views, {} overflows, {} rebuilds in {} frames, {} record group(s)",
            table.live, table.capacity, table.overflows, table.rebuilds, table.refreshes, table.record_groups
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (width, height) = match args.as_slice() {
        [w, h, ..] => (*w, *h),
        _ => (1280, 720),
    };
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            println!("skip: no GPU adapter ({err})");
            return ExitCode::SUCCESS;
        }
    };
    let caps = BindlessCaps::from_device(&device.device);
    let table = regia_render::texture_table::device_support(&device.device);
    println!(
        "adapter {} ({:?}), bindless tier {}, texture table {}",
        device.capabilities.adapter_name,
        device.capabilities.backend,
        caps.tier,
        match table {
            regia_render::TableSupport::Table { capacity } => format!("on ({capacity} views)"),
            regia_render::TableSupport::Fallback(reason) => format!("off ({reason})"),
        }
    );
    let quality = hard_surface::hull_closeup_quality();
    let mut close_up = hard_surface::hull_closeup_scene();
    close_up.width = width;
    close_up.height = height;
    let grid = hard_surface::feature_cost_scene(HardSurfaceFeature::ALL[0], true, width, height);
    for (name, scene) in [("hull_closeup", &close_up), ("hull_grid", &grid)] {
        if let Err(err) = report(&device, name, scene, quality) {
            eprintln!("{name}: {err}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
