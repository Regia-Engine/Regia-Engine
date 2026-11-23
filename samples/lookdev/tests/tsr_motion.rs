// SPDX-License-Identifier: MIT-0
//! `specs/27` § 2.3 on the motion chart: the TSR-grade upscaler at render scale 0.67
//! while the camera pans 2 px/frame (lavapipe-deterministic; the RTX 5090 measures the
//! same numbers).
//!
//! * motion clarity: MTF50 of the panning line pairs ≥ 85 % of the native still;
//! * thin geometry: ≥ 90 % of the 1-px lattice bars keep half their native contrast;
//! * ghosting: the trail 8 frames behind a moving box carries ≤ 2 % of its contrast.
//!
//! The current TAAU at 0.67 is measured too, as the baseline TSR must beat.

use lookdev::tsr_motion::{ChartMode, measure_mode, measure_mtf, render_pan};

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn tsr_at_067_meets_the_motion_targets() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let native = render_pan(&device, ChartMode::NativeStill, 1, 0.0).expect("native still");
    let native_mtf = measure_mtf(&native.0).mtf50;
    let (tsr, _) = measure_mode(&device, ChartMode::Tsr(0.67), 120, &native, None).expect("tsr");
    let (taau, _) = measure_mode(&device, ChartMode::Taau(0.67), 120, &native, None).expect("taau");
    for r in [&tsr, &taau] {
        eprintln!(
            "{}: mtf50 {:.3} (native {native_mtf:.3}) lattice {:.2} ghost bias {:.4} residual {:.4}",
            r.mode, r.pan.mtf50, r.lattice, r.ghost.bias, r.ghost.residual
        );
    }

    let clarity = tsr.pan.mtf50 / native_mtf.max(1.0e-6);
    assert!(
        clarity >= 0.85,
        "TSR motion clarity {:.0} % of native",
        100.0 * clarity
    );
    assert!(
        tsr.lattice >= 0.90,
        "TSR lattice bars {:.0} %",
        100.0 * tsr.lattice
    );
    assert!(
        tsr.ghost.bias <= 0.02,
        "TSR ghost {:.2} %",
        100.0 * tsr.ghost.bias
    );

    // And it is the sharper upscaler: clarity and the 1-px lattice. Ghosting is held to
    // the absolute § 2.3 gate above, not to TAAU: since motion-look round 1 TAAU rejects
    // history on velocity disagreement and prints no trail on this chart (bias 0.0), so
    // "less than TAAU" stopped being a ghosting bar (integration of lane/motion-look2).
    assert!(tsr.pan.mtf50 > taau.pan.mtf50);
    assert!(tsr.lattice > taau.lattice);
}

/// Brief tsr2 item 1: under TSR at 0.67 the material textures sample the mip the output
/// resolution would (negative mip bias), so a still textured plane keeps native texture
/// detail at every texel density; without the bias the 0.67 frame is visibly softer.
#[test]
fn tsr_at_067_keeps_native_texture_detail() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use lookdev::tsr_motion::{TEXTURE_DENSITIES, measure_texture_detail, render_texture_plane};
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let frames = 48;
    let (native, native_bias) =
        render_texture_plane(&device, ChartMode::NativeTaa, frames, None).expect("native");
    let (tsr, tsr_bias) = render_texture_plane(&device, ChartMode::Tsr(0.67), frames, None).expect("tsr");
    let unbiased = regia_render::TsrSettings {
        // Any offset ≥ −log2(0.67) clamps the bias to 0: render-resolution mips.
        texture_bias_offset: 1.0,
        ..regia_render::TsrSettings::for_scale(0.67)
    };
    let (flat, flat_bias) =
        render_texture_plane(&device, ChartMode::Tsr(0.67), frames, Some(unbiased)).expect("tsr unbiased");
    let (taau, taau_bias) = render_texture_plane(&device, ChartMode::Taau(0.67), frames, None).expect("taau");
    let (still, _) = render_texture_plane(&device, ChartMode::NativeStill, 1, None).expect("still");
    // TSR at native scale with the 0.67 tuning: the same reconstruction, full-resolution mips.
    let (native_tsr, native_tsr_bias) = render_texture_plane(
        &device,
        ChartMode::Tsr(1.0),
        frames,
        Some(regia_render::TsrSettings::for_scale(0.67)),
    )
    .expect("tsr 1.0");
    let r = measure_texture_detail(&native_tsr);
    if let Some(dir) = std::env::var_os("REGIA_TSR_TEXTURE_OUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("out dir");
        for (name, image) in [
            ("still", &still),
            ("native_taa", &native),
            ("tsr", &tsr),
            ("tsr_unbiased", &flat),
            ("taau", &taau),
        ] {
            lookdev::default_look::write_png(
                &dir.join(format!("{name}.png")),
                image.width,
                image.height,
                &image.pixels,
            )
            .expect("png");
        }
    }
    let s = measure_texture_detail(&still);
    let n = measure_texture_detail(&native);
    let t = measure_texture_detail(&tsr);
    let f = measure_texture_detail(&flat);
    let a = measure_texture_detail(&taau);
    eprintln!("bias native {native_bias} tsr {tsr_bias} unbiased {flat_bias} taau {taau_bias}");
    for (k, density) in TEXTURE_DENSITIES.iter().enumerate() {
        eprintln!(
            "density {density}: still {:.3} tsr-1.0 {:.3} native {:.3} tsr {:.3} ({:.0} %) tsr-unbiased {:.3} ({:.0} %) taau {:.3} ({:.0} %)",
            s[k],
            r[k],
            n[k],
            t[k],
            100.0 * t[k] / n[k],
            f[k],
            100.0 * f[k] / n[k],
            a[k],
            100.0 * a[k] / n[k]
        );
    }
    eprintln!(
        "vs tsr-1.0: tsr {:?} unbiased {:?}",
        [t[0] / r[0], t[1] / r[1], t[2] / r[2]],
        [f[0] / r[0], f[1] / r[1], f[2] / r[2]]
    );
    assert_eq!(native_bias, 0.0);
    assert_eq!(native_tsr_bias, 0.0);
    assert_eq!(taau_bias, 0.0, "TAAU frames keep bias 0");
    assert_eq!(flat_bias, 0.0);
    assert!(tsr_bias < -0.5, "TSR 0.67 bias {tsr_bias}");
    for k in 0..3 {
        // Native-scale TSR detail within 15 % at every density, and the bias is what
        // closes the gap (the render-resolution mips lose ≥ 20 % more).
        let kept = t[k] / r[k];
        let flat = f[k] / r[k];
        assert!(
            (0.85..=1.15).contains(&kept),
            "density {}: TSR 0.67 keeps {:.0} % of native texture detail",
            TEXTURE_DENSITIES[k],
            100.0 * kept
        );
        assert!(
            flat < kept - 0.2,
            "density {}: unbiased {:.0} % vs biased {:.0} %",
            TEXTURE_DENSITIES[k],
            100.0 * flat,
            100.0 * kept
        );
    }
}

/// Brief tsr2 item 2: alpha-tested edges under TSR. Against a 4×4-supersampled still,
/// the hashed alpha test lets TSR's accumulation reach fractional edge coverage, where a
/// hard cutoff stair-steps the magnified diagonals and aliases the minified strands.
#[test]
fn tsr_hashed_alpha_resolves_mask_edges() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use lookdev::tsr_motion::{downsample, mask_error, render_mask_chart};
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let frames = 64;
    let reference = downsample(
        &render_mask_chart(&device, ChartMode::NativeStill, 4, 1, None).expect("reference"),
        4,
    );
    let hashed = render_mask_chart(&device, ChartMode::Tsr(0.67), 1, frames, None).expect("tsr");
    let hard_settings = regia_render::TsrSettings {
        hashed_alpha: false,
        ..regia_render::TsrSettings::for_scale(0.67)
    };
    let hard =
        render_mask_chart(&device, ChartMode::Tsr(0.67), 1, frames, Some(hard_settings)).expect("tsr hard");
    let native = render_mask_chart(&device, ChartMode::NativeTaa, 1, frames, None).expect("native taa");
    let still = render_mask_chart(&device, ChartMode::NativeStill, 1, 1, None).expect("native still");
    if let Some(dir) = std::env::var_os("REGIA_TSR_TEXTURE_OUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("out dir");
        for (name, image) in [
            ("mask_reference", &reference),
            ("mask_tsr_hashed", &hashed),
            ("mask_tsr_hard", &hard),
            ("mask_native_taa", &native),
            ("mask_still", &still),
        ] {
            lookdev::default_look::write_png(
                &dir.join(format!("{name}.png")),
                image.width,
                image.height,
                &image.pixels,
            )
            .expect("png");
        }
    }
    let e_hashed = mask_error(&hashed, &reference);
    let e_hard = mask_error(&hard, &reference);
    let e_native = mask_error(&native, &reference);
    let e_still = mask_error(&still, &reference);
    eprintln!(
        "mask error [slanted, strands]: tsr hashed {e_hashed:?} tsr hard {e_hard:?} native taa {e_native:?} still {e_still:?}"
    );
    // Magnified edges keep the hard cutoff (TSR's jitter already resolves them).
    assert!(
        e_hashed[0] <= e_hard[0] + 0.25,
        "slanted edges: hashed {:.2} vs hard {:.2}",
        e_hashed[0],
        e_hard[0]
    );
    // Minified strands: a hard cutoff loses them (the mips average them below it); the
    // hashed test keeps their coverage.
    assert!(
        e_hashed[1] < 0.8 * e_hard[1] && e_hashed[1] < 0.8 * e_native[1],
        "strands: hashed {:.2}, hard {:.2}, native {:.2}",
        e_hashed[1],
        e_hard[1],
        e_native[1]
    );
}

/// The motion sharpen (`TsrSettings::motion_sharpness`) follows each pixel's own motion: a
/// held view converges to the same image with or without it.
#[test]
fn tsr_motion_sharpen_leaves_held_frames_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use lookdev::tsr_motion::render_texture_plane;
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let with = |motion_sharpness: f32| regia_render::TsrSettings {
        motion_sharpness,
        ..regia_render::TsrSettings::for_scale(0.67)
    };
    let (off, _) = render_texture_plane(&device, ChartMode::Tsr(0.67), 24, Some(with(0.0))).expect("off");
    let (on, _) = render_texture_plane(&device, ChartMode::Tsr(0.67), 24, Some(with(1.0))).expect("on");
    let worst = off
        .pixels
        .iter()
        .zip(&on.pixels)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(worst <= 1, "held frame changed by up to {worst} levels");
}
