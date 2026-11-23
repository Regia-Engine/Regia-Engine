// SPDX-License-Identifier: MPL-2.0
//! The boot warm builds the backdrop blur (ADR-2585, the ADR-1836/2129 remainder): after
//! `OverlayDraw::warm` (the boot tier's "overlay layers" job, on a warm worker), the first frame
//! with blurred panels creates no pipeline on the frame thread, for both blur paths (the plain
//! separable blur and the factor-4 prefilter). Counted through the frame-path counters by label.
//! Needs a software adapter (lavapipe).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::{FramePathKind, OverlayDraw, RenderDevice};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};
use regia_ui::render::OverlayShim;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer, UiRendererHandle};

const SIZE: u32 = 128;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483; the v0.6 pre-gate's card-6 hang, ADR-7703,
/// is the same livelock). The counts are unaffected: the frame-path counters are per thread and the
/// pipelines a warm builds live in each test's own `UiRenderer`, not in the device.
fn device() -> SerialGpu {
    match serial_device() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn backdrop(pos: [f32; 2], size: [f32; 2], sigma: f32) -> UiQuad {
    UiQuad {
        texture: TextureRef::Backdrop,
        blur: sigma,
        ..UiQuad::fill(pos, size, UiColor::WHITE)
    }
}

fn lock(handle: &UiRendererHandle) -> std::sync::MutexGuard<'_, UiRenderer> {
    match handle.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Draws two blurred panels (σ 4: the plain blur; σ 20: factor 4 with the prefilter) and returns
/// the pipelines and bind groups the frame thread created, by label, and how many panels blurred.
fn first_blurred_frame(device: &RenderDevice, handle: &UiRendererHandle) -> (Vec<String>, Vec<String>, u32) {
    let target = OffscreenTarget::new(device, SIZE, SIZE);
    lock(handle).set_quads(vec![
        backdrop([8.0, 8.0], [48.0, 48.0], 4.0),
        backdrop([64.0, 64.0], [56.0, 56.0], 20.0),
    ]);
    let before = regia_render::frame_path_counters();
    regia_render::trace_frame_path(true);
    target.draw_ui(device, &mut lock(handle));
    let events = regia_render::take_frame_path_events();
    regia_render::trace_frame_path(false);
    let delta = regia_render::frame_path_counters().since(before);
    let labels = |kind: FramePathKind| -> Vec<String> {
        events
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.label.clone())
            .collect()
    };
    let pipelines = labels(FramePathKind::Pipeline);
    assert_eq!(
        delta.pipelines,
        pipelines.len() as u64,
        "every pipeline create is traced"
    );
    (
        pipelines,
        labels(FramePathKind::BindGroup),
        lock(handle).backdrop_stats().blurred,
    )
}

#[test]
fn the_boot_warm_builds_the_blur_so_the_first_blurred_panel_compiles_nothing() {
    let device = device();
    let handle = UiRenderer::new_handle();
    let mut shim = OverlayShim::new(handle.clone());
    // The boot tier runs the overlay layers' warm on a worker thread.
    let worker = device.clone();
    std::thread::spawn(move || shim.warm(&worker, OffscreenTarget::FORMAT))
        .join()
        .expect("warm worker");
    let (pipelines, bind_groups, blurred) = first_blurred_frame(&device, &handle);
    assert_eq!(
        blurred, 2,
        "both panels must blur, or this frame does not cover the blur"
    );
    assert!(
        pipelines.is_empty(),
        "the first blurred panel compiled pipelines after the boot warm: {pipelines:?}"
    );
    // The blur's uniform ring and its group are built by the warm too.
    assert!(
        !bind_groups.iter().any(|label| label == "ui blur uniform bg"),
        "the blur state was created on the frame: {bind_groups:?}"
    );
}

/// The same warm through `UiRenderer::warm` (hosts that own the renderer and warm it directly).
#[test]
fn a_direct_renderer_warm_also_builds_the_blur() {
    let device = device();
    let handle = UiRenderer::new_handle();
    lock(&handle).warm(&device, OffscreenTarget::FORMAT);
    let (pipelines, _, blurred) = first_blurred_frame(&device, &handle);
    assert_eq!(blurred, 2);
    assert!(pipelines.is_empty(), "compiled after the warm: {pipelines:?}");
}

/// The teeth: without a warm, the first blurred frame does compile both blur pipelines.
#[test]
fn without_the_warm_the_first_blurred_panel_compiles_the_blur() {
    let device = device();
    let handle = UiRenderer::new_handle();
    let (pipelines, _, blurred) = first_blurred_frame(&device, &handle);
    assert_eq!(blurred, 2);
    let blur = pipelines
        .iter()
        .filter(|label| label.as_str() == "ui backdrop blur pipeline")
        .count();
    assert_eq!(blur, 2, "the plain blur and the prefilter: {pipelines:?}");
}
