// SPDX-License-Identifier: MPL-2.0
//! The boot warm builds the group layer pipelines (ADR-2866): after `OverlayDraw::warm`, the first
//! frame with an isolated group and a mask creates no pipeline on the frame thread. Counted through
//! the frame-path counters by label. Needs a software adapter (lavapipe). Own test binary: the
//! frame-path trace is process-wide.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use regia_render::{FramePathKind, OverlayDraw, RenderDevice};
use regia_ui::render::OverlayShim;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer, UiRendererHandle};

const SIZE: u32 = 128;
const MASK_PIPELINE: &str = "ui group layer mask pipeline";

static SERIAL: Mutex<()> = Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn lock(handle: &UiRendererHandle) -> std::sync::MutexGuard<'_, UiRenderer> {
    match handle.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// One group with a rounded mask: a begin marker, a primitive, the mask, the composite.
fn group() -> Vec<UiQuad> {
    let size = [64.0, 64.0];
    vec![
        UiQuad {
            texture: TextureRef::LayerBegin,
            ..UiQuad::fill([8.0, 8.0], size, UiColor::TRANSPARENT)
        },
        UiQuad::fill([0.0, 0.0], size, UiColor::WHITE),
        UiQuad {
            texture: TextureRef::LayerMask {
                image: None,
                tile: false,
            },
            corner_radius: [8.0; 4],
            border_width: [0.0, 0.0, 64.0, 64.0],
            blur: 1.0,
            ..UiQuad::fill([0.0, 0.0], size, UiColor::WHITE)
        },
        UiQuad {
            texture: TextureRef::LayerComposite,
            alpha: 0.5,
            ..UiQuad::fill([8.0, 8.0], size, UiColor::WHITE)
        },
    ]
}

/// Draws the group and returns the pipelines the frame thread created, and the layers drawn.
fn first_group_frame(device: &RenderDevice, handle: &UiRendererHandle) -> (Vec<String>, u32) {
    let target = OffscreenTarget::new(device, SIZE, SIZE);
    lock(handle).set_quads(group());
    regia_render::trace_frame_path(true);
    target.draw_ui(device, &mut lock(handle));
    let events = regia_render::take_frame_path_events();
    regia_render::trace_frame_path(false);
    let pipelines = events
        .iter()
        .filter(|e| e.kind == FramePathKind::Pipeline)
        .map(|e| e.label.clone())
        .collect();
    (pipelines, lock(handle).layer_stats().drawn)
}

#[test]
fn the_boot_warm_builds_the_layer_pipelines() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let handle = UiRenderer::new_handle();
    let mut shim = OverlayShim::new(handle.clone());
    // The boot tier runs the overlay layers' warm on a worker thread.
    let worker = device.clone();
    std::thread::spawn(move || shim.warm(&worker, wgpu::TextureFormat::Rgba8Unorm))
        .join()
        .expect("warm worker");
    let (pipelines, drawn) = first_group_frame(&device, &handle);
    assert_eq!(drawn, 1, "the group must draw, or this frame does not cover it");
    assert!(
        !pipelines.iter().any(|l| l == MASK_PIPELINE),
        "the first group compiled the mask pipeline after the warm: {pipelines:?}"
    );
}

/// A host that warms in the window's format: the layer format is warm too, so a frame in the
/// offscreen format below only compiles what that format itself needs.
#[test]
fn a_direct_renderer_warm_also_builds_the_layer_pipelines() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let handle = UiRenderer::new_handle();
    lock(&handle).warm(&device, OffscreenTarget::FORMAT);
    let (pipelines, drawn) = first_group_frame(&device, &handle);
    assert_eq!(drawn, 1);
    assert!(pipelines.is_empty(), "compiled after the warm: {pipelines:?}");
}

/// The teeth: without a warm, the first group does compile the mask pipeline.
#[test]
fn without_the_warm_the_first_group_compiles_the_mask_pipeline() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let handle = UiRenderer::new_handle();
    let (pipelines, drawn) = first_group_frame(&device, &handle);
    assert_eq!(drawn, 1);
    assert!(pipelines.iter().any(|l| l == MASK_PIPELINE), "{pipelines:?}");
}
