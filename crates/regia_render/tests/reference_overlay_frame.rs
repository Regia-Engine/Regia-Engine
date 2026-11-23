// SPDX-License-Identifier: MPL-2.0
//! The reference overlay (ADR-2463) on a real offscreen frame: off draws nothing and leaves the
//! frame byte-identical, the difference of a frame with its own picture is zero, a wipe at 0.5
//! shows the frame left of the middle and the reference right of it, a blend lies between the two,
//! and a reference of another aspect is letterboxed with the frame showing around it.
//!
//! Runs on any headless adapter (lavapipe is the reference). No golden image: every check
//! compares frames of this test with each other.

use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings,
    ReferenceImage, ReferenceOverlayHandle, ReferenceOverlayMode, ReferenceOverlaySettings, RenderDevice,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

const W: u32 = 64;
const H: u32 = 36;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A lit quad covering the left 60 % of the frame on a dark ground, so the frame has structure.
fn scene() -> LabScene {
    let z = -3.0;
    let mesh = LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-2.0, -0.6, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.4, -0.6, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.4, 0.8, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-2.0, 0.8, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -1.0],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings::golden_deterministic()),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [0.9, 0.5, 0.2],
            emissive_intensity: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![mesh],
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice) -> Vec<u8> {
    renderer
        .render_offscreen(device, &scene().as_frame_scene())
        .expect("offscreen frame")
        .pixels
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Arc<ReferenceImage> {
    let rgba: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
    Arc::new(ReferenceImage::new(w, h, rgba).expect("image"))
}

fn px(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn set(handle: &ReferenceOverlayHandle, mode: ReferenceOverlayMode, opacity: f32, split: f32) {
    handle.set_settings(ReferenceOverlaySettings { mode, opacity, split });
}

#[test]
fn off_is_byte_identical_and_draws_nothing() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(|e| e.into_inner());
    let device = device();
    let mut plain = FrameRenderer::new();
    let mut with_pass = FrameRenderer::new();
    let handle = ReferenceOverlayHandle::default();
    assert!(with_pass.enable_reference_overlay(&handle));
    assert!(
        !with_pass.enable_reference_overlay(&handle),
        "a second enable is a no-op"
    );
    handle.set_image(Some(solid(W, H, [255, 0, 255, 255])));
    // Mode off with an image set.
    let a = render(&mut plain, &device);
    let b = render(&mut with_pass, &device);
    assert_eq!(a, b, "mode off must not change a pixel");
    assert_eq!(handle.stats().draws, 0);
    assert_eq!(handle.stats().uploads, 0);
}

#[test]
fn difference_of_the_frame_with_itself_is_zero() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(|e| e.into_inner());
    let device = device();
    let mut renderer = FrameRenderer::new();
    let frame = render(&mut renderer, &device);
    assert!(
        frame.as_chunks::<4>().0.iter().any(|p| p[0] > 8),
        "the test frame must not be black, or zero proves nothing"
    );
    let handle = ReferenceOverlayHandle::default();
    renderer.enable_reference_overlay(&handle);
    handle.set_image(Some(Arc::new(
        ReferenceImage::new(W, H, frame.clone()).expect("reference"),
    )));
    set(&handle, ReferenceOverlayMode::Difference, 1.0, 0.5);
    let diff = render(&mut renderer, &device);
    for (i, p) in diff.as_chunks::<4>().0.iter().enumerate() {
        assert_eq!(&p[..3], &[0, 0, 0], "pixel {i} is not zero");
    }
    assert_eq!(handle.stats().draws, 1);
    assert_eq!(handle.stats().frame_copies, 1);
    assert_eq!(handle.stats().failures, 0);

    // A different reference is not zero.
    handle.set_image(Some(solid(W, H, [0, 255, 0, 255])));
    let other = render(&mut renderer, &device);
    assert!(other.as_chunks::<4>().0.iter().any(|p| p[1] > 0 || p[0] > 0));
    assert_eq!(handle.stats().uploads, 2, "one upload per image, not per frame");
    let again = render(&mut renderer, &device);
    assert_eq!(other, again);
    assert_eq!(handle.stats().uploads, 2);

    // Flip of the frame with itself is zero too.
    handle.set_image(Some(Arc::new(
        ReferenceImage::new(W, H, frame).expect("reference"),
    )));
    set(&handle, ReferenceOverlayMode::Flip, 1.0, 0.5);
    let flip = render(&mut renderer, &device);
    assert!(flip.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]));
}

#[test]
fn wipe_at_half_splits_the_frame() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(|e| e.into_inner());
    let device = device();
    let mut renderer = FrameRenderer::new();
    let frame = render(&mut renderer, &device);
    let handle = ReferenceOverlayHandle::default();
    renderer.enable_reference_overlay(&handle);
    handle.set_image(Some(solid(W, H, [0, 0, 255, 255])));
    set(&handle, ReferenceOverlayMode::Wipe, 1.0, 0.5);
    let wiped = render(&mut renderer, &device);
    for y in 0..H {
        for x in 0..W {
            if x < W / 2 {
                assert_eq!(
                    px(&wiped, x, y),
                    px(&frame, x, y),
                    "left half is the frame at ({x},{y})"
                );
            } else {
                assert_eq!(
                    px(&wiped, x, y),
                    [0, 0, 255, 255],
                    "right half is the reference at ({x},{y})"
                );
            }
        }
    }
}

#[test]
fn blend_lies_between_frame_and_reference() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(|e| e.into_inner());
    let device = device();
    let mut renderer = FrameRenderer::new();
    let frame = render(&mut renderer, &device);
    let handle = ReferenceOverlayHandle::default();
    renderer.enable_reference_overlay(&handle);
    handle.set_image(Some(solid(W, H, [0, 0, 255, 255])));
    set(&handle, ReferenceOverlayMode::Blend, 0.5, 0.5);
    let blended = render(&mut renderer, &device);
    let (f, b) = (px(&frame, W - 2, 2), px(&blended, W - 2, 2));
    assert!(
        b[2] > f[2] && b[2] < 255,
        "blue rises but is not replaced: {f:?} -> {b:?}"
    );
    set(&handle, ReferenceOverlayMode::Blend, 1.0, 0.5);
    let full = render(&mut renderer, &device);
    assert_eq!(px(&full, 3, 3), [0, 0, 255, 255]);
    set(&handle, ReferenceOverlayMode::Blend, 0.0, 0.5);
    let none = render(&mut renderer, &device);
    assert_eq!(none, frame, "opacity 0 leaves the frame");
}

#[test]
fn another_aspect_is_letterboxed_with_the_frame_around_it() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(|e| e.into_inner());
    let device = device();
    let mut renderer = FrameRenderer::new();
    let frame = render(&mut renderer, &device);
    let handle = ReferenceOverlayHandle::default();
    renderer.enable_reference_overlay(&handle);
    // 4:3 reference in the 16:9 view: pillarboxed, 48 px wide, 8 px of frame either side.
    handle.set_image(Some(solid(40, 30, [0, 0, 255, 255])));
    set(&handle, ReferenceOverlayMode::Wipe, 1.0, 0.0);
    let out = render(&mut renderer, &device);
    assert_eq!(px(&out, 2, 18), px(&frame, 2, 18), "left bar shows the frame");
    assert_eq!(
        px(&out, W - 3, 18),
        px(&frame, W - 3, 18),
        "right bar shows the frame"
    );
    assert_eq!(
        px(&out, W / 2, 18),
        [0, 0, 255, 255],
        "the reference fills the middle"
    );
}
