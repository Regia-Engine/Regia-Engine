// SPDX-License-Identifier: MIT-0
//! The hitch gate (`specs/27` § 2.4) on DO27's own content: the scripted 60 s soak drive
//! (`lookdev::soak::SoakOptions::MINUTE` — road and boustrophedon, camera cuts, time-of-day
//! jumps, VFX and decals) through the full runtime frame on both maps, measured after warm-up.
//!
//! Rows that do not depend on how busy the machine is are always gated: no pipeline, bind group,
//! buffer or texture created on the frame path, no blocking poll, no CPU sky bake inside a frame,
//! and GPU frame times (timestamps) within the § 2.4 ratios. The wall-time rows (frames over
//! 2 × p50, p99/p50, p99.9/p50, longest frame, frames over 50 ms) are gated when the box's CPU
//! floor is quiet (`lookdev::soak::cpu_floor`); on a shared farm they are printed, not gated.
//! The same drive on the frame path of 2026-10-07 (`bare`) must fail.
//!
//! Needs `REGIA_DO27_CONTENT` and a hardware GPU; skips otherwise.
//! `vfarm.sh --gpu bench --content -- cargo test --release -p lookdev --test hitch_soak -- --nocapture`

use lookdev::Planet;
use lookdev::chase::{ChaseOptions, ChaseRun, run, runtime_device};
use lookdev::look_r2::R2Planets;
use lookdev::soak::SoakOptions;

fn soak(device: &regia_render::RenderDevice, planet: Planet, bare: bool) -> ChaseRun {
    let content = lookdev::do27::content_dir().expect("checked by the caller");
    let options = ChaseOptions {
        soak: Some(SoakOptions::MINUTE),
        // The spec's 2 s warm-up at the 60 Hz drive step.
        warmup: 120,
        trace: true,
        prewarm: !bare,
        async_sky: !bare,
        ..ChaseOptions::default()
    };
    let mut planets = R2Planets::default();
    run(device, &mut planets, &content, planet, &options).unwrap_or_else(|err| panic!("{err}"))
}

/// Failures of the rows that do not depend on the machine's load.
fn robust_failures(run: &ChaseRun) -> Vec<String> {
    let report = run.hitch_report();
    let mut out: Vec<String> = report
        .failures()
        .into_iter()
        .filter(|f| {
            f.contains("pipeline")
                || f.contains("bind-group")
                || f.contains("blocking")
                || f.contains("sky bakes")
        })
        .collect();
    let c = report.counters;
    if c.buffers + c.textures > 0 {
        out.push(format!(
            "{} buffers and {} textures created after warm-up (target 0)",
            c.buffers, c.textures
        ));
    }
    let mut gpu: Vec<f32> = run.samples.iter().map(|s| s.gpu.frame_ms).collect();
    gpu.sort_by(f32::total_cmp);
    let q = |p: f32| regia_render::hitch::percentile_sorted(&gpu, p);
    let p50 = q(0.5).max(1e-6);
    let over = gpu.iter().filter(|g| **g > 2.0 * p50).count();
    if over as f32 > 0.001 * gpu.len() as f32 {
        out.push(format!("{over} GPU frames over 2 x GPU p50 (target <= 0.1 %)"));
    }
    if q(0.999) / p50 > 2.0 {
        out.push(format!("GPU p99.9/p50 = {:.2} (target <= 2)", q(0.999) / p50));
    }
    out
}

#[test]
fn the_do27_soak_drive_does_not_hitch() {
    if lookdev::do27::content_dir().is_none() {
        eprintln!("skip: REGIA_DO27_CONTENT is not set");
        return;
    }
    let device = runtime_device().unwrap_or_else(|err| panic!("{err}"));
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        eprintln!("skip: the soak needs a hardware GPU (a 60 s drive at 1440p on a CPU rasterizer)");
        return;
    }
    let floor = lookdev::soak::cpu_floor(600);
    println!("{}", floor.summary());
    let mut failures = Vec::new();
    for planet in [Planet::Europa, Planet::Mars] {
        let run = soak(&device, planet, false);
        let report = run.hitch_report();
        println!("{}", lookdev::soak::report_text(&run, &report));
        for failure in robust_failures(&run) {
            failures.push(format!("{}: {failure}", run.map));
        }
        if floor.quiet() {
            for failure in report.failures() {
                failures.push(format!("{} (wall time): {failure}", run.map));
            }
        }
    }
    assert!(failures.is_empty(), "the soak hitched:\n{}", failures.join("\n"));

    // The gate has teeth: the frame path of 2026-10-07 fails the same drive.
    let bare = soak(&device, Planet::Europa, true);
    println!("{}", lookdev::soak::report_text(&bare, &bare.hitch_report()));
    let bare_failures = robust_failures(&bare);
    println!("bare frame path (2026-10-07): {bare_failures:?}");
    assert!(
        bare_failures.iter().any(|f| f.contains("sky bakes")),
        "the bare frame path should bake the sky inside frames: {bare_failures:?}"
    );
    assert!(
        bare_failures
            .iter()
            .any(|f| f.contains("bind-group") || f.contains("buffers")),
        "the bare frame path should create resources after warm-up: {bare_failures:?}"
    );
}
