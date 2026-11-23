// SPDX-License-Identifier: MPL-2.0
//! Offscreen GPU evidence tests for the M14 VFX suite (lavapipe in CI, any adapter locally).
//!
//! Every shot of the DO27 event set + decals + trail + camera shake is rendered mid-effect
//! through the full stack (CPU scheduling → GPU simulation → transparent draw over the
//! harness's lit ground + sky) and judged with the with/without diff metrics: an empty,
//! black or blown-out effect fails the test (see `regia_vfx::measure`). Warm/cool colour
//! bias is asserted where the effect's palette is unambiguous (muzzle/tracer/impact/
//! explosion/sparks/trail warm, weather cool), and the GPU's own alive counter must be
//! non-zero at capture time.

use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_vfx::evidence::{assert_shot_plausible, effect_shots, render_shot};

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

const W: u32 = 320;
const H: u32 = 180;

/// The harness backdrop alone is healthy (guards the measurement baseline itself).
#[test]
fn backdrop_baseline_is_exposed_correctly() {
    let camera = regia_vfx::VfxCamera {
        eye: Vec3::new(0.0, 2.0, 7.0),
        target: Vec3::new(0.0, 1.0, 0.0),
        fov_y: 55.0f32.to_radians(),
        near: 0.05,
        far: 200.0,
    };
    let mut harness = regia_vfx::offscreen::VfxHarness::new(device(), regia_vfx::VfxProfile::desktop_high())
        .expect("harness builds");
    let frame = harness
        .render(device(), &camera, W, H, 1.0 / 60.0)
        .expect("backdrop renders");
    let luma =
        |p: [u8; 4]| (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0;
    let px = frame.pixels.as_chunks::<4>().0;
    let mean = px.iter().map(|p| luma(*p)).sum::<f32>() / px.len() as f32;
    assert!(mean > 0.15 && mean < 0.70, "backdrop mean luma {mean:.3}");
    let max = px.iter().map(|p| luma(*p)).fold(0.0f32, f32::max);
    assert!(max > 0.65, "no sky highlight: max luma {max:.3}");
}

#[test]
fn every_do27_effect_shot_is_plausible() {
    let mut failures: Vec<String> = Vec::new();
    for shot in effect_shots() {
        let result = match render_shot(device(), &shot, W, H) {
            Ok(r) => r,
            Err(err) => {
                failures.push(format!("{}: render error {err}", shot.name));
                continue;
            }
        };
        println!("{}", regia_vfx::evidence::shot_line(shot.name, &result));
        if let Err(err) = std::panic::catch_unwind(|| assert_shot_plausible(&shot, &result)) {
            let msg = err
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown assertion failure".into());
            failures.push(msg);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} shots failed:\n{}",
        failures.len(),
        effect_shots().len(),
        failures.join("\n")
    );
}

/// Deterministic spawn counts hold through the GPU: the same shot twice schedules identical
/// CPU jobs (the ring-slot assignment on the GPU is atomic, so pixel bytes may differ — the
/// metrics do not; see `regia_vfx::measure` docs).
#[test]
fn shot_metrics_are_stable_across_runs() {
    let shot = &effect_shots()
        .into_iter()
        .find(|s| s.name == "sparks")
        .expect("sparks shot");
    let a = render_shot(device(), shot, W, H).expect("run a");
    let b = render_shot(device(), shot, W, H).expect("run b");
    assert_eq!(a.counters, b.counters, "GPU alive counters differ");
    let (ca, cb) = (a.metrics.coverage, b.metrics.coverage);
    assert!(
        (ca - cb).abs() < 0.01,
        "coverage jumped between runs: {ca:.4} vs {cb:.4}"
    );
}
