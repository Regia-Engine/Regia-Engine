// SPDX-License-Identifier: MIT-0
//! `vfx_lab --crash-sheets` films the same bytes on every run (ADR-5991). The harness steps a
//! fixed `DT`, so two renders of the crash on one tier and view must match byte for byte. The
//! Lite tier (Mobile High: lists drawn unsorted) drew different bytes run to run while the
//! update kernel gathered its draw lists with an atomic counter; the Full tier (sorted) did not.

use regia_render::RenderDevice;
use regia_vfx::crash::CrashTier;
use vfx_lab::crash_bench::crash_sheet_frames;

/// The first two sheet times (0.1 s: the flash and the fireball, 0.6 s: the blast front), small
/// so lavapipe finishes quickly.
const TIMES: [f32; 2] = [0.1, 0.6];
const SIZE: (u32, u32) = (160, 90);

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn assert_reproducible(tier: CrashTier) {
    let device = device();
    for view in ["shot4", "near"] {
        let a = crash_sheet_frames(&device, tier, view, SIZE, &TIMES).expect("first render");
        let b = crash_sheet_frames(&device, tier, view, SIZE, &TIMES).expect("second render");
        for ((t, x), y) in TIMES.iter().zip(&a).zip(&b) {
            assert!(
                x.iter().any(|&v| v != 0),
                "{} {view} {t} s: empty frame",
                tier.name()
            );
            let differ = x
                .as_chunks::<4>()
                .0
                .iter()
                .zip(y.as_chunks::<4>().0)
                .filter(|(p, q)| p != q)
                .count();
            assert_eq!(
                differ,
                0,
                "{} {view} {t} s: two renders differ in {differ} of {} pixels",
                tier.name(),
                SIZE.0 * SIZE.1
            );
        }
    }
}

/// Lite tier (Mobile High, unsorted lists): the bug ADR-5991 fixes.
#[test]
fn lite_crash_sheets_render_the_same_bytes_twice() {
    assert_reproducible(CrashTier::Lite);
}

/// Full tier (Desktop High, sorted lists): the control that already matched.
#[test]
fn full_crash_sheets_render_the_same_bytes_twice() {
    assert_reproducible(CrashTier::Full);
}
