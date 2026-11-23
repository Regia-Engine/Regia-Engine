// SPDX-License-Identifier: MIT-0
//! FSR 3.1 (TSR's comparison baseline, ADR-113) on the `specs/27` § 2.3 motion chart at render
//! scale 0.67 while the camera pans 2 px/frame (lavapipe-deterministic).
//!
//! FSR 3.1 is opt-in and not a shipping default, so this test holds it to "works end to
//! end": it upscales a sharper image than the old TAAU, keeps most of the 1-px lattice, and
//! leaves no visible trail behind a moving box. The TSR gates of § 2.3 stay on TSR
//! (`tsr_motion.rs`); the head-to-head numbers are in the lane report.

use lookdev::tsr_motion::{ChartMode, measure_mode, measure_mtf, render_pan};

#[test]
fn fsr3_at_067_upscales_the_motion_chart() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let native = render_pan(&device, ChartMode::NativeStill, 1, 0.0).expect("native still");
    let native_mtf = measure_mtf(&native.0).mtf50;
    let (fsr3, image) = measure_mode(&device, ChartMode::Fsr3(0.67), 120, &native, None).expect("fsr3");
    let (taau, _) = measure_mode(&device, ChartMode::Taau(0.67), 120, &native, None).expect("taau");
    for r in [&fsr3, &taau] {
        eprintln!(
            "{}: mtf50 {:.3} (native {native_mtf:.3}) lattice {:.2} ghost bias {:.4} residual {:.4}",
            r.mode, r.pan.mtf50, r.lattice, r.ghost.bias, r.ghost.residual
        );
    }
    // A finite, non-black image (no NaN from the history or the sharpen).
    let lit = image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
        .count();
    assert!(
        lit > image.pixels.len() / 40,
        "FSR 3.1 output is (nearly) black: {lit} lit pixels"
    );

    assert!(
        fsr3.pan.mtf50 > taau.pan.mtf50,
        "FSR 3.1 MTF50 {:.3} vs TAAU {:.3}",
        fsr3.pan.mtf50,
        taau.pan.mtf50
    );
    assert!(
        fsr3.lattice >= 0.5,
        "FSR 3.1 lattice bars {:.0} %",
        100.0 * fsr3.lattice
    );
    assert!(
        fsr3.ghost.bias <= 0.05,
        "FSR 3.1 ghost {:.2} %",
        100.0 * fsr3.ghost.bias
    );
}
