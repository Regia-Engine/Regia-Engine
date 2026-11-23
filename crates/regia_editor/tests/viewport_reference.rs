// SPDX-License-Identifier: MPL-2.0
//! The reference image (concept ghost, `book/agents/reference-image.md`): every mode draws in
//! the pane render and in captures, a pane without a reference renders exactly as before, and
//! the agent methods set, read and clear it.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_editor::reference::{ReferenceImage, ReferenceMode, ViewportReferences};
use regia_editor::render::{PaneRequest, capture_pane, render_pane};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_scene::prelude::Transform;
use serde_json::json;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

const W: u32 = 240;
const H: u32 = 180;

fn scene() -> World {
    let mut world = World::new();
    world.spawn(Transform {
        translation: Vec3::new(0.0, 0.6, 0.0),
        scale: Vec3::splat(1.5),
        ..Default::default()
    });
    world
}

fn request(reference: Option<ReferenceImage>, ids: bool) -> PaneRequest {
    PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera::default(),
        width: W,
        height: H,
        ids,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference,
    }
}

fn solid(rgba: [u8; 4]) -> Arc<egui::ColorImage> {
    Arc::new(egui::ColorImage::filled(
        [8, 8],
        egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]),
    ))
}

fn rgb(image: &egui::ColorImage, x: usize, y: usize) -> [u8; 3] {
    let p = image.pixels[y * image.width() + x];
    [p.r(), p.g(), p.b()]
}

/// The pixel of the first covered / uncovered id-buffer cell, scanning rows.
fn covered_and_background(ids: &regia_editor::picking::IdBuffer) -> ((usize, usize), (usize, usize)) {
    let mut covered = None;
    let mut background = None;
    for y in 0..ids.height as usize {
        for x in 0..ids.width as usize {
            let hit = ids.ids[y * ids.width as usize + x] != 0;
            if hit && covered.is_none() {
                covered = Some((x, y));
            }
            if !hit && background.is_none() {
                background = Some((x, y));
            }
        }
    }
    (
        covered.expect("a mesh pixel"),
        background.expect("a background pixel"),
    )
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn an_absent_reference_changes_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = scene();
    let plain = render_pane(&device, &mut world, &request(None, false)).expect("render");
    let again = render_pane(&device, &mut world, &request(None, false)).expect("render");
    assert_eq!(plain.image.pixels, again.image.pixels);
    // An invisible reference is the same picture.
    let hidden = ReferenceImage::new(solid([255, 0, 0, 255]), ReferenceMode::Over, 0.0);
    let with = render_pane(&device, &mut world, &request(Some(hidden), false)).expect("render");
    assert_eq!(plain.image.pixels, with.image.pixels);
}

#[test]
fn every_mode_draws_in_the_pane() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = scene();
    let plain = render_pane(&device, &mut world, &request(None, true)).expect("render");
    let ids = plain.ids.as_ref().expect("ids");
    let (covered, background) = covered_and_background(ids);

    // Over, opaque: the reference replaces everything.
    let over = ReferenceImage::new(solid([255, 0, 0, 255]), ReferenceMode::Over, 1.0);
    let frame = render_pane(&device, &mut world, &request(Some(over), false)).expect("render");
    assert_eq!(rgb(&frame.image, covered.0, covered.1), [255, 0, 0]);
    assert_eq!(rgb(&frame.image, background.0, background.1), [255, 0, 0]);

    // Behind: the background becomes the reference, the mesh stays lit.
    let behind = ReferenceImage::new(solid([255, 0, 0, 255]), ReferenceMode::Behind, 1.0);
    let frame = render_pane(&device, &mut world, &request(Some(behind), false)).expect("render");
    assert_eq!(rgb(&frame.image, background.0, background.1), [255, 0, 0]);
    assert_eq!(
        rgb(&frame.image, covered.0, covered.1),
        rgb(&plain.image, covered.0, covered.1),
        "the scene stays over the reference"
    );

    // Difference against the render itself: black everywhere.
    let same = ReferenceImage::new(Arc::new(plain.image.clone()), ReferenceMode::Difference, 1.0);
    let frame = render_pane(&device, &mut world, &request(Some(same), false)).expect("render");
    let worst = frame
        .image
        .pixels
        .iter()
        .map(|p| p.r().max(p.g()).max(p.b()))
        .max()
        .unwrap_or(0);
    assert!(worst <= 2, "difference of identical pictures peaks at {worst}");
    // Against a flat grey it shows the scene's departure from it.
    let grey = ReferenceImage::new(solid([128, 128, 128, 255]), ReferenceMode::Difference, 1.0);
    let frame = render_pane(&device, &mut world, &request(Some(grey), false)).expect("render");
    let p = plain.image.pixels[background.1 * W as usize + background.0];
    let expected = [p.r().abs_diff(128), p.g().abs_diff(128), p.b().abs_diff(128)];
    assert_eq!(rgb(&frame.image, background.0, background.1), expected);

    // Silhouette: a reference with a transparent field and an opaque disc-like block.
    let mut subject = egui::ColorImage::filled([32, 32], egui::Color32::TRANSPARENT);
    for y in 8..24 {
        for x in 8..24 {
            subject.pixels[y * 32 + x] = egui::Color32::WHITE;
        }
    }
    let outline = ReferenceImage::new(Arc::new(subject), ReferenceMode::Silhouette, 1.0);
    let frame = render_pane(&device, &mut world, &request(Some(outline), false)).expect("render");
    let outlined = frame
        .image
        .pixels
        .iter()
        .filter(|p| p.r() == 255 && p.g() == 60 && p.b() == 170)
        .count();
    assert!(outlined > 100, "outline pixels: {outlined}");
    let changed = frame
        .image
        .pixels
        .iter()
        .zip(&plain.image.pixels)
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(changed, outlined, "only the outline is drawn");
}

#[test]
fn captures_include_the_reference_under_the_overlays() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = scene();
    let bare = capture_pane(&device, &mut world, &request(None, false)).expect("capture");
    let over = ReferenceImage::new(solid([255, 0, 0, 255]), ReferenceMode::Over, 1.0);
    let capture = capture_pane(&device, &mut world, &request(Some(over), false)).expect("capture");
    assert_eq!(
        rgb(&capture.image, 2, 2),
        [255, 0, 0],
        "the reference is in the capture"
    );
    assert_ne!(capture.image.pixels, bare.image.pixels);
    // The editor grid still paints over it: some non-red pixel remains.
    assert!(
        capture
            .image
            .pixels
            .iter()
            .any(|p| !(p.r() == 255 && p.g() == 0 && p.b() == 0)),
        "overlays draw over the reference"
    );
}

fn call(
    world: &mut World,
    name: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let methods = regia_editor::viewport_agent::methods();
    let (_, handler) = methods
        .iter()
        .find(|(schema, _)| schema.name == name)
        .expect("method");
    handler(world, &params)
}

#[test]
fn agent_methods_set_read_and_clear() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = scene();
    world.insert_resource(device);
    assert_eq!(
        call(&mut world, "editor.get_reference", json!({})).expect("ok")["reference"],
        json!(null)
    );

    // A 2×2 PNG, encoded with the viewport's own helper.
    let png =
        regia_editor::render::viewport_png_base64(&egui::ColorImage::filled([2, 2], egui::Color32::BLUE))
            .expect("png");
    let set = call(
        &mut world,
        "editor.set_reference",
        json!({ "png_base64": png, "mode": "over", "opacity": 1.0, "rect_uv": [0.0, 0.0, 1.0, 1.0] }),
    )
    .expect("set");
    assert_eq!(set["mode"], "over");
    assert_eq!(set["width"], 2);
    assert!(world.resource::<ViewportReferences>().panes.contains_key(&0));

    // The next capture carries it.
    let capture = call(
        &mut world,
        "editor.capture_viewport",
        json!({ "width": 128, "height": 96 }),
    )
    .expect("capture");
    assert_eq!(capture["reference"]["mode"], "over");

    // A later call without an image updates the settings only.
    let update = call(
        &mut world,
        "editor.set_reference",
        json!({ "mode": "difference", "opacity": 0.25 }),
    )
    .expect("update");
    assert_eq!(update["mode"], "difference");
    assert_eq!(update["width"], 2);

    assert!(call(&mut world, "editor.set_reference", json!({ "mode": "sideways" })).is_err());
    assert!(
        call(
            &mut world,
            "editor.set_reference",
            json!({ "rect_uv": [0.5, 0.0, 0.5, 1.0] })
        )
        .is_err()
    );
    assert!(call(&mut world, "editor.set_reference", json!({ "png_base64": "!!" })).is_err());

    assert_eq!(
        call(&mut world, "editor.clear_reference", json!({})).expect("ok")["cleared"],
        true
    );
    assert_eq!(
        call(&mut world, "editor.get_reference", json!({})).expect("ok")["reference"],
        json!(null)
    );
    let capture = call(
        &mut world,
        "editor.capture_viewport",
        json!({ "width": 128, "height": 96 }),
    )
    .expect("capture");
    assert_eq!(capture["reference"], json!(null));
}
