// SPDX-License-Identifier: MPL-2.0
//! FSR 3.1 on the spec 27 § 2.3 temporal chart.
//!
//! The TSR chart (`regia_render::temporal_chart`, ADR-2638) already renders its scene through
//! [`regia_render::TemporalUpscaler::Fsr3`]. This test drives that public entry
//! ([`regia_render::temporal_chart::run`] with [`regia_render::temporal_chart::ChartRequest::pan`])
//! at scale 0.67 and at scale 0.5 and prints the five § 2.3 numbers at each scale.
//!
//! The scene, the 2 px/frame pan, the 120 motion frames, the moving-box ghosting pass and the
//! held-chart stability pass are `run`'s. This file does not reimplement a metric, and it does
//! not edit `temporal_chart.rs`, `tsr/` or the FSR shaders. Default [`regia_render::Fsr3Settings`]
//! are what the chart already applies (no extra settings field: every chart input the resolve
//! reads is already on the frame).
//!
//! The test fails only when a number is non-finite or a run does not finish. A miss of a § 2.3
//! target is printed and is not a failure.
//!
//! Agent surface: no new method. `seeing.temporal_chart { upscaler: "fsr3", scale }` already
//! returns these five numbers for one scale (`book/agents/temporal-chart.md`). This test is the
//! measurement that runs both scales without pinning a target.

use regia_render::TemporalUpscaler;
use regia_render::temporal_chart::{ChartReport, ChartRequest, TARGETS, run};

/// Scales spec 27 § 4.2 stage 4 measures (the TSR chart's pair).
const SCALES: [f32; 2] = [0.67, 0.5];

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

/// The five § 2.3 numbers, in spec order.
fn five(report: &ChartReport) -> [(&'static str, f32); 5] {
    [
        ("motion_clarity", report.motion.motion_clarity),
        ("thin_geometry", report.motion.thin_geometry),
        ("ghosting", report.ghost.ghosting),
        ("disocclusion_px", report.ghost.disocclusion_px),
        ("stability", report.stability.flip),
    ]
}

fn print_scale(scale: f32, report: &ChartReport) {
    let numbers = five(report);
    println!(
        "fsr_chart scale={scale:.2} motion_clarity={:.6} thin_geometry={:.6} \
         ghosting={:.6} disocclusion_px={:.6} stability={:.6}",
        numbers[0].1, numbers[1].1, numbers[2].1, numbers[3].1, numbers[4].1
    );
    // Beside the five: the residual and held |ΔL| the TSR table prints in brackets.
    println!(
        "fsr_chart scale={scale:.2} detail ghost_residual={:.6} stability_luma_delta={:.6} \
         frames_rendered={}",
        report.ghost.ghost_residual, report.stability.luma_delta, report.frames_rendered
    );
    let misses: Vec<&str> = report
        .checks()
        .iter()
        .filter(|check| !check.3)
        .map(|check| check.0)
        .collect();
    if misses.is_empty() {
        println!("fsr_chart scale={scale:.2} meets § 2.3");
    } else {
        println!(
            "fsr_chart scale={scale:.2} misses § 2.3: {} \
             (targets clarity>={:.2} thin>={:.2} ghost<={:.3} smear<={:.0} stability<={:.3})",
            misses.join(", "),
            TARGETS.motion_clarity,
            TARGETS.thin_geometry,
            TARGETS.ghosting,
            TARGETS.disocclusion_px,
            TARGETS.stability,
        );
    }
}

/// FSR 3.1 at 0.67 and 0.5 on the TSR chart's pan. Prints ten numbers. Fails only on a
/// non-finite number or a run that does not finish.
#[test]
fn fsr31_chart_prints_ten_numbers_at_067_and_050() {
    let device = device();
    for scale in SCALES {
        let request = ChartRequest::pan(TemporalUpscaler::Fsr3, scale);
        let report = match run(&device, &request) {
            Ok(report) => report,
            Err(err) => panic!("FSR 3.1 chart at scale {scale} did not finish: {err}"),
        };
        assert_eq!(
            report.request.upscaler,
            TemporalUpscaler::Fsr3,
            "chart run did not keep the FSR 3.1 resolve"
        );
        for (name, value) in five(&report) {
            assert!(
                value.is_finite(),
                "FSR 3.1 scale {scale} {name} is not finite: {value}"
            );
        }
        print_scale(scale, &report);
    }
}

/// Highest ghost residual (the trail's peak, the bracketed number beside ghosting) the FSR 3.1
/// ghost scene may leave at each scale (ADR-7933). A regression pin on the ADR-7678 resolve, not a
/// § 2.3 target: ADR-7678 reads 0.0423 / 0.0419 at 0.67 and 0.0765 / 0.0759 at 0.5 (lavapipe /
/// RTX 5090); the held ADR-7875 accumulate read 0.0617 / 0.0611 and 0.0854 / 0.0858. A resolve
/// change that raises the residual past this needs its own ADR.
const GHOST_RESIDUAL_PINS: [(f32, f32); 2] = [(0.67, 0.05), (0.5, 0.08)];

/// The FSR 3.1 ghost scene's residual stays at the ADR-7678 resolve's level at 0.67 and 0.5
/// (ADR-7933: review finding 7 asked for the residual to be pinned).
#[test]
fn fsr31_ghost_residual_holds_at_067_and_050() {
    let device = device();
    let mut misses = Vec::new();
    for (scale, pin) in GHOST_RESIDUAL_PINS {
        let ghost = match regia_render::temporal_chart::measure_ghost(
            &device,
            &ChartRequest::pan(TemporalUpscaler::Fsr3, scale),
        ) {
            Ok(ghost) => ghost,
            Err(err) => panic!("FSR 3.1 ghost scene ({scale}) did not finish: {err}"),
        };
        println!(
            "fsr_chart scale={scale:.2} ghosting={:.6} ghost_residual={:.6} disocclusion_px={:.1}",
            ghost.ghosting, ghost.ghost_residual, ghost.disocclusion_px
        );
        assert!(
            ghost.ghost_residual.is_finite(),
            "FSR 3.1 {scale} ghost residual is not finite"
        );
        if ghost.ghost_residual > pin {
            misses.push(format!(
                "scale {scale}: ghost residual {:.4} (pin ≤ {pin:.3})",
                ghost.ghost_residual
            ));
        }
    }
    assert!(
        misses.is_empty(),
        "FSR 3.1 ghost residual above the ADR-7678 level: {}",
        misses.join("; ")
    );
}
