// SPDX-License-Identifier: MPL-2.0
//! The fog of war overlay (audit gap FW-1) on a real offscreen frame: a 4 x 4 fog fixture over a
//! wall darkens the unexplored cell, dims the explored ones and leaves the visible one clear; off
//! is byte-identical; only written rows upload.
//!
//! Runs on any headless adapter (lavapipe is the reference). No golden image: every check compares
//! the fogged frame with the same frame without the overlay, by pixel class thresholds.
//! TODO(lead): bless a golden of `fog_classes_frame` on the build server lavapipe reference.

use std::sync::Mutex;

use regia_render::{
    DirectionalLightUniform, FogLook, FogOverlayHandle, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

const W: u32 = 64;
const H: u32 = 64;
/// The wall's Z: the middle of fog row 1.
const WALL_Z: f32 = -16.0;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A grey emissive wall at `WALL_Z` filling the view, seen head-on from +Z so screen X is world X:
/// the view spans world X -48..48 m (half-width `tan(0.45) * 99.4`).
fn scene() -> LabScene {
    let s = 300.0;
    let wall = LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-s, -s, WALL_Z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([s, -s, WALL_Z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([s, s, WALL_Z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-s, s, WALL_Z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 83.4],
        target: [0.0, 0.0, WALL_Z],
        fov_y: 0.9,
        near: 1.0,
        far: 500.0,
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
            emissive: [0.5, 0.5, 0.5],
            emissive_intensity: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![wall],
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice) -> Vec<u8> {
    renderer
        .render_offscreen(device, &scene().as_frame_scene())
        .expect("offscreen frame")
        .pixels
}

fn luma(pixels: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    (u32::from(pixels[i]) + u32::from(pixels[i + 1]) + u32::from(pixels[i + 2])) as f32 / 3.0
}

/// 4 x 4 cells of 32 m over -64..64. Row 1 (Z -32..0, the wall's row): unexplored, explored,
/// visible, explored. Row 2 is all unexplored, so a flipped Z would darken the whole wall; rows
/// 0 and 3 are visible.
fn fixture() -> Vec<u8> {
    let mut texels = vec![255u8; 16];
    texels[4..8].copy_from_slice(&[0, 128, 255, 128]);
    texels[8..12].copy_from_slice(&[0; 4]);
    texels
}

fn look(softness: f32) -> FogLook {
    FogLook {
        enabled: true,
        strength: 1.0,
        explored_darkness: 0.5,
        explored_tint: [1.0; 3],
        unexplored_darkness: 1.0,
        unexplored_tint: [1.0; 3],
        softness,
    }
}

#[test]
fn fog_classes_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let baseline = render(&mut FrameRenderer::new(), &device);

    let handle = FogOverlayHandle::default();
    let mut renderer = FrameRenderer::new();
    assert!(renderer.enable_fog_overlay(&handle));
    assert!(!renderer.enable_fog_overlay(&handle), "registered once");
    // Off (the default look): byte-identical, nothing drawn.
    handle.set_grid(4, 4, [-64.0, -64.0], [128.0, 128.0]);
    handle.write_rows(0, 4, &fixture());
    let off = render(&mut renderer, &device);
    assert!(off == baseline, "a disabled overlay changed the frame");
    assert_eq!(handle.stats().draws, 0);

    handle.set_look(look(0.0));
    let fogged = render(&mut renderer, &device);
    let stats = handle.stats();
    assert_eq!(
        (stats.draws, stats.texture_creates, stats.rows_uploaded),
        (1, 1, 4)
    );
    assert_eq!(stats.bytes_uploaded, 16);

    // Screen X -> world X: -48 + (x + 0.5) * 1.5. Cell centres: col 0 at -48 (x 0), col 1 at -16
    // (x 21), col 2 at 16 (x 42), col 3 at 48 (x 63).
    let y = H / 2;
    let ratio = |x: u32| {
        let base = luma(&baseline, x, y);
        assert!(base > 20.0, "the wall is lit at x {x}: {base}");
        luma(&fogged, x, y) / base
    };
    let (dark, dim, clear, dim_right) = (ratio(0), ratio(21), ratio(42), ratio(63));
    assert!(dark < 0.1, "unexplored is dark: {dark}");
    assert!(dim > 0.25 && dim < 0.9, "explored is dim: {dim}");
    assert!(clear > 0.97, "visible is clear: {clear}");
    assert!(
        dim_right > 0.25 && dim_right < 0.9,
        "explored is dim: {dim_right}"
    );
    // Every row of the image reads the wall's fog row (Z is not flipped): the clear column is
    // clear top to bottom.
    for yy in [4, H / 4, 3 * H / 4, H - 4] {
        let r = luma(&fogged, 42, yy) / luma(&baseline, 42, yy);
        assert!(r > 0.97, "row {yy}: {r}");
    }

    // A steady frame uploads nothing; one written row uploads one row.
    let _ = render(&mut renderer, &device);
    assert_eq!(handle.stats().rows_uploaded, 4);
    handle.write_rows(1, 1, &[255, 255, 255, 255]);
    let cleared = render(&mut renderer, &device);
    let stats = handle.stats();
    assert_eq!(
        (stats.rows_uploaded, stats.last_rows, stats.texture_creates),
        (5, 1, 1)
    );
    let r = luma(&cleared, 0, y) / luma(&baseline, 0, y);
    assert!(r > 0.97, "the updated row shows: {r}");

    // Soft edges: a wider filter spreads the unexplored column into its neighbour.
    handle.write_rows(1, 1, &fixture()[4..8]);
    handle.set_look(look(2.0));
    let soft = render(&mut renderer, &device);
    let soft_dim = luma(&soft, 21, y) / luma(&baseline, 21, y);
    assert!(
        soft_dim < dim,
        "softness pulls the explored cell toward its dark neighbour: {soft_dim} vs {dim}"
    );
    assert!(renderer.disable_fog_overlay());
    let after = render(&mut renderer, &device);
    assert!(after == baseline, "a removed overlay changed the frame");
}
