// SPDX-License-Identifier: MPL-2.0
//! The bake sizing model (`regia_pt::bake_sizing`, ADR-1099) against a real bake of the small
//! synthetic level: the probe and under-terrain counts, the main trace's rays, the volume bytes
//! and the batch size are what `size_job` predicted. Runs the real kernels on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI); 8 rays and one sample per probe keep it short.

use regia_pt::PathTracer;
use regia_pt::bake_sizing::{SizingLayout, SyntheticLevel, size_job};
use regia_pt::probe::bake_gi_volume;
use regia_render::RenderDevice;

#[test]
fn a_bake_of_the_small_stand_in_matches_its_sizing() {
    let level = SyntheticLevel::small();
    let layout = SizingLayout {
        rays_per_probe: 8,
        samples: 1,
        probes_per_batch: 512,
        ..SizingLayout::recommended()
    };
    let job = level.job(&layout).expect("builds");
    let sizing = size_job(&job).expect("sizes");
    assert_eq!(sizing.gpu.batch_probes, 512);

    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile the path tracer");
    let mut batches = 0u64;
    let bake = bake_gi_volume(&tracer, &job.scene, &job.layout, &job.settings, |_, _, _| {
        batches += 1;
        true
    })
    .expect("bake");

    assert_eq!(bake.report.probes, sizing.probes);
    assert_eq!(bake.volume.probe_count(), sizing.probes);
    // Both test the nominal positions against the same heightfield.
    assert_eq!(bake.report.under_terrain, sizing.under_terrain);
    assert_eq!(bake.report.rejected_rays, 0);
    // Every live probe traces rays × samples in the main trace; a bake that skips probes it
    // already knows are inside geometry traces fewer, never more.
    let skipped_max = bake.report.inside_geometry as u64 * 8;
    assert!(
        bake.report.rays_traced <= sizing.rays.main
            && bake.report.rays_traced + skipped_max >= sizing.rays.main,
        "{} rays traced, {} predicted",
        bake.report.rays_traced,
        sizing.rays.main
    );

    let file = bake.volume.to_bytes().expect("encode").len() as u64;
    assert!(
        file > sizing.volume.payload && file <= sizing.volume.file_max,
        "{file} bytes, {:?}",
        sizing.volume
    );
    let v = &bake.volume;
    let ground = v.terrain.as_ref().map_or(0, |t| t.ground.len() as u64 * 4);
    assert_eq!(
        sizing.volume.runtime,
        (v.meta_bytes().len() + v.sh_bytes().len() + v.moment_bytes().len()) as u64
            + ground
            + 48 * v.bricks.len() as u64
    );

    // The main trace runs in batches of 512; relocation adds at most two rounds (and a
    // classification pass) of at most as many.
    let main_batches = sizing.rays.live_probes.div_ceil(512);
    assert!(
        batches >= main_batches && batches <= 4 * main_batches,
        "{batches} batches, {main_batches} in the main trace"
    );
}
