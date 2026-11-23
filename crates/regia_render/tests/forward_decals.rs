// SPDX-License-Identifier: MPL-2.0
//! Forward decals (ADR-3255, `regia_render::forward_decals`): a decal's normal and roughness reach
//! the lighting.
//!
//! Every test in this binary turns the process switch on before the first renderer (it is latched
//! per process). The byte-identity test runs this binary twice more as child processes, with the
//! switch off and on, and compares a decal-free frame.
//!
//! Oracles:
//! - a normal-decal on a wall under a grazing sun moves the lit footprint in the direction its
//!   normal predicts (N·L ×2.07 for a 17.5° tilt toward the sun, near black away from it), and its
//!   normal is the one the AOV reads back;
//! - a rough decal over a glossy wall's mirror highlight shades exactly as a wall whose own
//!   roughness is the decal's (same surface inputs), so the highlight is gone under it;
//! - a decal-free frame with the switch on is byte-identical to the switch-off frame;
//! - a translucent layer's feed (the VFX runtime's hand-off) shades exactly as the host API.

use regia_core::math::{Quat, Vec3};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use regia_render::forward_decals::{CHANNEL_NORMAL, CHANNEL_ROUGHNESS, ForwardDecal, ForwardDecalFeed};
use regia_render::frame_aov::angular_error_deg;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    LayerCamera, PostSettings, RenderDevice, TranslucentLayer, capture_frame_aov, set_process_forward_decals,
};

const W: u32 = 128;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn switch_on() {
    // Latched per process: every test of this binary asks for on, so the first call wins.
    let _ = set_process_forward_decals(true);
    assert!(
        regia_render::process_forward_decals(),
        "the switch is on in this binary"
    );
}

/// A wall: a quad in the XY plane through the origin, front face +Z, tangent +X.
fn wall(half: f32) -> LabMesh {
    let corners = [
        ([-half, -half, 0.0], [0.0, 1.0]),
        ([half, -half, 0.0], [1.0, 1.0]),
        ([half, half, 0.0], [1.0, 0.0]),
        ([-half, half, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn scene(sun_travel: [f32; 3], material: LabMaterial) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new(sun_travel, [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![wall(1.6)],
    }
}

/// A 0.5 m decal box on the wall's centre: right +X, up +Y, projection axis +Z (the wall normal).
fn decal(channels: u32) -> ForwardDecal {
    let mut d = ForwardDecal::from_box(
        Vec3::ZERO,
        Quat::IDENTITY,
        Vec3::new(0.5, 0.5, 0.2),
        [1.0, 1.0, 1.0, 1.0],
    );
    d.channels = channels;
    d.angle_fade = [0.0, 0.0];
    d.edge_softness = 0.0;
    d.normal_strength = 1.0;
    d
}

/// The inner 40 % of the decal's footprint (the decal spans about ±0.5 m at 3 m: ±20 % of the
/// image height).
fn centre() -> Vec<bool> {
    (0..W * H)
        .map(|i| {
            let (x, y) = (i % W, i / W);
            (x as f32 - W as f32 / 2.0).abs() < 8.0 && (y as f32 - H as f32 / 2.0).abs() < 8.0
        })
        .collect()
}

fn srgb_to_linear(b: u8) -> f64 {
    let c = f64::from(b) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Mean linear luminance of the display pixels under `mask`.
fn mean_luma(pixels: &[u8], mask: &[bool]) -> f64 {
    let (mut sum, mut n) = (0.0, 0usize);
    for (i, m) in mask.iter().enumerate() {
        if *m {
            let p = &pixels[i * 4..i * 4 + 3];
            sum +=
                0.2126 * srgb_to_linear(p[0]) + 0.7152 * srgb_to_linear(p[1]) + 0.0722 * srgb_to_linear(p[2]);
            n += 1;
        }
    }
    sum / n.max(1) as f64
}

/// The fourth frame of a fresh renderer (frames move on their own; compare equal frame numbers).
fn frame4(device: &RenderDevice, lab: &LabScene, decals: &[ForwardDecal]) -> (Vec<u8>, FrameRenderer) {
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let mut pixels = Vec::new();
    for _ in 0..4 {
        assert!(renderer.set_forward_decals(decals));
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    (pixels, renderer)
}

fn matte() -> LabMaterial {
    LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.9,
        metallic: 0.0,
        ..LabMaterial::default()
    }
}

#[test]
fn a_normal_decal_moves_the_lit_wall_the_way_its_normal_predicts() {
    switch_on();
    let device = device();
    // The sun 15° above the wall's plane, from +X (it travels toward −X and into the wall).
    let e = 15f32.to_radians();
    let lab = scene([-e.cos(), 0.0, -e.sin()], matte());
    let mask = centre();
    let (bare, renderer) = frame4(&device, &lab, &[]);
    let stats = renderer
        .forward_decal_stats()
        .expect("this device has the forward-decal binding");
    assert_eq!(stats.packed, 0);
    let bare_l = mean_luma(&bare, &mask);
    assert!(bare_l > 0.005, "the grazing sun lights the bare wall ({bare_l})");

    let mut toward = decal(CHANNEL_NORMAL);
    toward.normal_xy = [0.3, 0.0];
    let mut away = toward;
    away.normal_xy = [-0.3, 0.0];
    let (lit_toward, renderer) = frame4(&device, &lab, &[toward]);
    let stats = renderer.forward_decal_stats().expect("stats");
    assert_eq!(stats.packed, 1);
    assert!(stats.tiles_touched > 0 && stats.max_per_tile == 1, "{stats:?}");
    let (lit_away, _) = frame4(&device, &lab, &[away]);
    let gain_toward = mean_luma(&lit_toward, &mask) / bare_l;
    let gain_away = mean_luma(&lit_away, &mask) / bare_l;
    // Lambert: N·L goes from sin 15° = 0.259 to 0.3 cos 15° + 0.954 sin 15° = 0.537 (×2.07), and to
    // below zero away from the sun. The tonemap compresses the ratio; the direction must hold.
    assert!(gain_toward > 1.4, "tilted toward the sun: ×{gain_toward:.3}");
    assert!(gain_away < 0.4, "tilted away from the sun: ×{gain_away:.3}");

    // The AOV reads the decal's normal (the spec 27 row: present, not stripped).
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    assert!(renderer.set_forward_decals(&[toward]));
    let aov = capture_frame_aov(&mut renderer, &device, &frame).expect("frame AOV");
    let covered: Vec<bool> = mask.iter().zip(&aov.coverage).map(|(a, b)| *a && *b).collect();
    let nz = (1.0f32 - 0.09).sqrt();
    let want = [0.3, 0.0, nz];
    let (_, rms, n) = angular_error_deg(&aov.normal, &vec![want; aov.normal.len()], &covered);
    assert!(n > 100, "{n} covered pixels");
    assert!(rms < 2.0, "decal normal in the AOV: RMS {rms}° off {want:?}");
}

#[test]
fn a_rough_decal_removes_the_mirror_highlight_under_it() {
    switch_on();
    let device = device();
    // Head-on sun: its mirror direction is the camera, a tight highlight on a glossy wall.
    let glossy = LabMaterial {
        base_color: [0.1, 0.1, 0.1, 1.0],
        roughness: 0.15,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    let rough_wall = LabMaterial {
        roughness: 1.0,
        ..glossy.clone()
    };
    let mask = centre();
    let (glossy_px, _) = frame4(&device, &scene([0.0, 0.0, -1.0], glossy.clone()), &[]);
    let mut rough = decal(CHANNEL_ROUGHNESS);
    rough.roughness = 1.0;
    let (decal_px, _) = frame4(&device, &scene([0.0, 0.0, -1.0], glossy), &[rough]);
    let (rough_px, _) = frame4(&device, &scene([0.0, 0.0, -1.0], rough_wall), &[]);
    let highlight = mean_luma(&glossy_px, &mask);
    let under = mean_luma(&decal_px, &mask);
    let reference = mean_luma(&rough_px, &mask);
    assert!(
        under < 0.6 * highlight,
        "the highlight stays under the rough decal: {under:.4} vs {highlight:.4}"
    );
    // The same surface inputs as a wall whose own roughness is 1: the same pixels, to a byte.
    let worst = mask
        .iter()
        .enumerate()
        .filter(|(_, m)| **m)
        .flat_map(|(i, _)| (0..3).map(move |c| i * 4 + c))
        .map(|k| decal_px[k].abs_diff(rough_px[k]))
        .max()
        .unwrap_or(0);
    assert!(
        worst <= 1,
        "decal footprint vs a rough wall: worst byte difference {worst} (means {under:.4} / {reference:.4})"
    );
}

/// A layer that draws nothing and feeds one decal (the VFX runtime's hand-off, ADR-3255).
struct FeedingLayer {
    feed: ForwardDecalFeed,
    /// The binding the frame reported on its last ask (0 never asked, 1 false, 2 true).
    told: AtomicU8,
}

impl TranslucentLayer for FeedingLayer {
    fn simulate(
        &self,
        _depth: &wgpu::TextureView,
        _camera: &LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    fn is_active(&self) -> bool {
        false
    }

    fn has_decals(&self) -> bool {
        false
    }

    fn forward_decal_feed(&self, binding: bool) -> Option<ForwardDecalFeed> {
        self.told.store(1 + u8::from(binding), Ordering::Release);
        Some(self.feed.clone())
    }
}

#[test]
fn a_translucent_layer_feed_shades_like_the_host_api() {
    switch_on();
    let device = device();
    let e = 15f32.to_radians();
    let lab = scene([-e.cos(), 0.0, -e.sin()], matte());
    let mut toward = decal(CHANNEL_NORMAL);
    toward.normal_xy = [0.3, 0.0];
    let (by_api, _) = frame4(&device, &lab, &[toward]);

    let layer = Arc::new(FeedingLayer {
        feed: ForwardDecalFeed {
            generation: 7,
            decals: vec![toward].into(),
            atlas: None,
        },
        told: AtomicU8::new(0),
    });
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![Arc::clone(&layer) as Arc<dyn TranslucentLayer>]);
    let mut pixels = Vec::new();
    for _ in 0..4 {
        pixels = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    }
    assert_eq!(
        layer.told.load(Ordering::Acquire),
        2,
        "the frame told the layer it has the binding"
    );
    let stats = renderer.forward_decal_stats().expect("stats");
    assert_eq!(stats.packed, 1, "the fed decal was packed");
    assert!(pixels == by_api, "a fed decal shades exactly as the host API's");
}

/// Child mode of [`a_decal_free_frame_is_byte_identical_with_the_switch_on_and_off`]: renders the
/// matte wall with no decal under the switch named by `REGIA_FD_CHILD_SWITCH` and writes the
/// fourth frame's bytes to `REGIA_FD_CHILD_OUT`. Ignored: only the parent test runs it (with
/// `--ignored`), so a bare run cannot count its early return as a pass.
#[test]
#[ignore = "run by a_decal_free_frame_is_byte_identical_with_the_switch_on_and_off"]
fn child_decal_free_frame() {
    let (Ok(out), Ok(switch)) = (
        std::env::var("REGIA_FD_CHILD_OUT"),
        std::env::var("REGIA_FD_CHILD_SWITCH"),
    ) else {
        return;
    };
    assert!(set_process_forward_decals(switch == "1"));
    let device = device();
    let e = 15f32.to_radians();
    let lab = scene([-e.cos(), -0.3, -e.sin()], matte());
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let mut pixels = Vec::new();
    for _ in 0..4 {
        pixels = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    }
    assert_eq!(renderer.forward_decal_stats().is_some(), switch == "1");
    let tmp = format!("{out}.tmp");
    std::fs::write(&tmp, &pixels).expect("write child frame");
    std::fs::rename(&tmp, &out).expect("rename child frame");
}

#[test]
fn a_decal_free_frame_is_byte_identical_with_the_switch_on_and_off() {
    let dir = regia_test_utils::unique_temp_dir("forward_decal_identity");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let exe = std::env::current_exe().expect("test binary");
    let mut frames = Vec::new();
    for switch in ["0", "1"] {
        let out = dir.join(format!("frame_{switch}.bin"));
        let status = std::process::Command::new(&exe)
            .args([
                "--ignored",
                "--exact",
                "child_decal_free_frame",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("REGIA_FD_CHILD_OUT", &out)
            .env("REGIA_FD_CHILD_SWITCH", switch)
            .env_remove("REGIA_FORWARD_DECALS")
            .status()
            .expect("run the child");
        assert!(status.success(), "child with switch {switch} failed");
        frames.push(std::fs::read(&out).expect("child frame"));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(frames[0].len(), (W * H * 4) as usize);
    assert!(
        frames[0] == frames[1],
        "a decal-free frame changed with the forward-decal switch on"
    );
}
