// SPDX-License-Identifier: MPL-2.0
//! The GPU GI denoiser against the CPU reference (ADR-864).
//!
//! Both run every regression scenario frame by frame from the same input; the GPU output must
//! stay within a small tolerance of the CPU's (the drivers' `exp` and `pow` are not
//! bit-exact, and the history carries the difference forward), its history lengths must match
//! exactly, and its scores against the ground truth must match the CPU's. Skips without a GPU
//! adapter.
//!
//! Anti-lag is a threshold test on per-square statistics, and GPU division and square root
//! are not correctly rounded: on noisy input a square sitting exactly at the threshold could
//! decide differently on the two sides. The scenario comparisons therefore run with anti-lag
//! off; `gpu_anti_lag_matches_on_a_clean_light_step` checks it where the decision has a wide
//! margin.

use regia_render::RenderDevice;
use regia_render::gi_denoise::scenario::{GiScenario, GiScenarioKind, run_scenario, run_scenario_gpu};
use regia_render::gi_denoise::{
    GiDenoiseGpu, GiDenoiseInput, GiDenoiseSettings, GiDenoiser, luminance, metrics,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::shared_headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skipping: no GPU adapter (lavapipe) available: {err}");
            None
        }
    }
}

/// Mean and largest per-pixel luminance difference, relative to the CPU frame's mean.
fn difference(cpu: &[[f32; 3]], gpu: &[[f32; 3]]) -> (f32, f32) {
    let mean = cpu.iter().map(|c| f64::from(luminance(*c))).sum::<f64>() / cpu.len() as f64;
    let mut sum = 0.0f64;
    let mut max = 0.0f64;
    for (a, b) in cpu.iter().zip(gpu) {
        let d = f64::from((luminance(*a) - luminance(*b)).abs());
        sum += d;
        max = max.max(d);
    }
    let scale = mean.max(1e-6);
    ((sum / cpu.len() as f64 / scale) as f32, (max / scale) as f32)
}

fn compare(kind: GiScenarioKind, settings: &GiDenoiseSettings, frames: u32, device: &RenderDevice) {
    let scenario = GiScenario {
        frames,
        warmup: 1,
        step_frame: frames / 2,
        ..GiScenario::new(kind)
    };
    let mut cpu = GiDenoiser::new(settings.clone()).expect("cpu denoiser");
    let mut gpu = GiDenoiseGpu::new(device, settings.clone()).expect("gpu denoiser");
    for t in 0..scenario.frames {
        let f = scenario.frame(t);
        let a = cpu.denoise(&f.input()).expect("cpu frame");
        let b = gpu.denoise(&f.input()).expect("gpu frame");
        assert_eq!(a.color.len(), b.color.len());
        for (i, (la, lb)) in a.history_len.iter().zip(&b.history_len).enumerate() {
            assert!(
                (la - lb).abs() < 1e-4,
                "{} frame {t} pixel {i}: history {la} vs {lb}",
                kind.name()
            );
        }
        let (mean, max) = difference(&a.color, &b.color);
        assert!(
            mean < 1e-3 && max < 2e-2,
            "{} frame {t}: GPU differs from the CPU reference by {mean} mean, {max} max",
            kind.name()
        );
        let ra = metrics::relative_rmse(&a.color, &f.truth, &f.surface).expect("cpu score");
        let rb = metrics::relative_rmse(&b.color, &f.truth, &f.surface).expect("gpu score");
        assert!(
            (ra - rb).abs() <= 0.01 * ra.max(0.01),
            "{} frame {t}: rmse {ra} vs {rb}",
            kind.name()
        );
    }
    assert_eq!(gpu.frames(), u64::from(scenario.frames));
}

fn no_antilag() -> GiDenoiseSettings {
    GiDenoiseSettings {
        antilag_sigma: 0.0,
        ..GiDenoiseSettings::default()
    }
}

#[test]
fn gpu_matches_the_cpu_reference_on_every_scenario() {
    let Some(device) = device() else { return };
    for kind in GiScenarioKind::ALL {
        compare(kind, &no_antilag(), 24, &device);
    }
}

#[test]
fn gpu_anti_lag_matches_on_a_clean_light_step() {
    let Some(device) = device() else { return };
    let (w, h) = (24u32, 20u32);
    let n = (w * h) as usize;
    let guide = vec![[0.0f32, 0.0, 1.0, 10.0]; n];
    let motion = vec![[0.0f32; 2]; n];
    let dim = vec![[1.0f32; 3]; n];
    let bright = vec![[4.0f32; 3]; n];
    let mut cpu = GiDenoiser::new(GiDenoiseSettings::default()).expect("cpu denoiser");
    let mut gpu = GiDenoiseGpu::new(&device, GiDenoiseSettings::default()).expect("gpu denoiser");
    for frame in 0..16 {
        let noisy = if frame < 10 { &dim } else { &bright };
        let input = GiDenoiseInput {
            width: w,
            height: h,
            noisy,
            guide: &guide,
            motion: &motion,
        };
        let a = cpu.denoise(&input).expect("cpu frame");
        let b = gpu.denoise(&input).expect("gpu frame");
        for (i, ((ca, cb), (la, lb))) in a
            .color
            .iter()
            .zip(&b.color)
            .zip(a.history_len.iter().zip(&b.history_len))
            .enumerate()
        {
            assert!(
                (ca[0] - cb[0]).abs() < 1e-3 * ca[0].max(1.0),
                "frame {frame} pixel {i}: {ca:?} vs {cb:?}"
            );
            assert!(
                (la - lb).abs() < 1e-3,
                "frame {frame} pixel {i}: history {la} vs {lb}"
            );
        }
        if frame == 10 {
            // The step frame: both blend the new light in at 3/4 (1 -> 3.25).
            assert!((b.color[0][0] - 3.25).abs() < 1e-2, "{:?}", b.color[0]);
        }
    }
}

#[test]
fn gpu_matches_with_feedback_and_without_the_spatial_passes() {
    let Some(device) = device() else { return };
    let feedback = GiDenoiseSettings {
        history_from_first_pass: true,
        ..no_antilag()
    };
    compare(GiScenarioKind::Disocclusion, &feedback, 16, &device);
    let temporal_only = GiDenoiseSettings {
        atrous_iterations: 0,
        ..no_antilag()
    };
    compare(GiScenarioKind::Pan, &temporal_only, 16, &device);
    let even = GiDenoiseSettings {
        atrous_iterations: 4,
        ..no_antilag()
    };
    compare(GiScenarioKind::Static, &even, 12, &device);
}

#[test]
fn gpu_reset_resize_and_bad_input() {
    let Some(device) = device() else { return };
    let mut gpu = GiDenoiseGpu::new(&device, GiDenoiseSettings::default()).expect("gpu denoiser");
    let scenario = GiScenario::new(GiScenarioKind::Static);
    for t in 0..3 {
        gpu.denoise(&scenario.frame(t).input()).expect("frame");
    }
    gpu.reset();
    let out = gpu.denoise(&scenario.frame(3).input()).expect("after the cut");
    assert!(
        out.history_len.iter().all(|l| *l == 1.0),
        "a cut restarts every history"
    );

    // A resize drops the history too.
    let small = GiScenario {
        width: 32,
        height: 24,
        ..GiScenario::new(GiScenarioKind::Static)
    };
    let out = gpu.denoise(&small.frame(0).input()).expect("resized");
    assert_eq!(out.color.len(), 32 * 24);
    assert!(out.history_len.iter().all(|l| *l == 1.0));

    // NaN, infinity and negative input never reach the output.
    let f = small.frame(1);
    let mut noisy = f.noisy.clone();
    noisy[5] = [f32::NAN, f32::INFINITY, -1.0];
    noisy[40] = [f32::NEG_INFINITY; 3];
    let input = GiDenoiseInput {
        noisy: &noisy,
        ..f.input()
    };
    let out = gpu.denoise(&input).expect("frame with bad values");
    assert!(out.color.iter().flatten().all(|v| v.is_finite() && *v >= 0.0));

    let short = GiDenoiseInput {
        motion: &f.motion[..10],
        ..f.input()
    };
    let err = gpu.denoise(&short).expect_err("short buffer");
    assert_eq!(err.code(), "gi_denoise_size_mismatch");
}

#[test]
fn gpu_scenario_scores_match_the_cpu_scores() {
    let Some(device) = device() else { return };
    for kind in GiScenarioKind::ALL {
        let scenario = GiScenario {
            frames: 32,
            step_frame: 24,
            ..GiScenario::new(kind)
        };
        let cpu = run_scenario(&scenario, &no_antilag()).expect("cpu run");
        let gpu = run_scenario_gpu(&scenario, &no_antilag(), &device).expect("gpu run");
        let close = |a: f32, b: f32| (a - b).abs() <= 0.02 * a.abs().max(b.abs()) + 1e-4;
        assert!(
            close(cpu.rel_rmse, gpu.rel_rmse),
            "{}: {cpu:?} vs {gpu:?}",
            kind.name()
        );
        assert!(
            close(cpu.flicker, gpu.flicker),
            "{}: {cpu:?} vs {gpu:?}",
            kind.name()
        );
        assert!(close(cpu.bias, gpu.bias), "{}: {cpu:?} vs {gpu:?}", kind.name());
        assert_eq!(cpu.lag_frames, gpu.lag_frames, "{}", kind.name());
        if let (Some(a), Some(b)) = (cpu.edge_leak, gpu.edge_leak) {
            assert!((a - b).abs() < 1e-3, "{}: leak {a} vs {b}", kind.name());
        }
    }
}
