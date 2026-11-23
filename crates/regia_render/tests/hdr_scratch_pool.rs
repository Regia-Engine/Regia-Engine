// SPDX-License-Identifier: MPL-2.0
//! The two HDR scratch images in the view's transient pool (ADR-7877, ledger M03 part 1).
//!
//! Public API only, so the same file builds against the code before the move: it prints a hash
//! of every frame of a held outdoor scene with a translucent HDR layer (both scratch slots in
//! use) and the main view's pooled bytes at 1440p. The hashes must match between the two builds;
//! the pooled bytes grow by exactly the two scratch images.
//!
//! The frame comparison and the 1440p measurement are `#[ignore]`d (lavapipe at 1440p takes
//! minutes); run them with `cargo test -p regia_render --test hdr_scratch_pool -- --ignored
//! --nocapture`.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    LayerCamera, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, TimeOfDay,
    TranslucentLayer, VolumetricFogSettings,
};

/// Takes part in the HDR pass and draws nothing: the frame records `translucent_hdr`.
struct HdrLayer;

impl TranslucentLayer for HdrLayer {
    fn simulate(
        &self,
        _depth: &wgpu::TextureView,
        _camera: &LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    fn participates_in_hdr(&self) -> bool {
        true
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn scene(width: u32, height: u32) -> LabScene {
    LabScene {
        width,
        height,
        eye: [0.0, 1.8, 0.0],
        target: [0.0, 6.0, -60.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.6, -0.4], [1.0, 0.95, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.4, 0.3, 0.25, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            quad(
                [
                    [-200.0, 0.0, 20.0],
                    [200.0, 0.0, 20.0],
                    [200.0, 0.0, -200.0],
                    [-200.0, 0.0, -200.0],
                ],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [
                    [-150.0, 0.0, -120.0],
                    [150.0, 0.0, -120.0],
                    [150.0, 60.0, -120.0],
                    [-150.0, 60.0, -120.0],
                ],
                [0.0, 0.0, 1.0],
            ),
        ],
    }
}

fn renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        fog: VolumetricFogSettings {
            density: 0.01,
            height_falloff: 0.05,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    });
    renderer.set_translucent_layers(vec![Arc::new(HdrLayer)]);
    renderer
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    }
}

/// FNV-1a over the pixels.
fn hash(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Frames of a held, then moved, then held again outdoor view with a translucent HDR layer
/// (both scratch slots), and a resize: prints one hash per frame for the before/after compare.
#[test]
#[ignore = "before/after frame hashes (compare two builds by hand)"]
fn hdr_scratch_frames_hash() {
    let device = device();
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    assert!(quality.reuse_static_screen());
    let post = post();
    let mut renderer = renderer();
    let mut hashes = Vec::new();
    for (width, height, eyes) in [(160_u32, 90_u32, 8_usize), (192, 108, 8)] {
        let mut lab = scene(width, height);
        for step in 0..eyes {
            // Frames 0-2 held, 3-4 moving, 5-7 held at the new place.
            if (3..5).contains(&step) {
                lab.eye[0] += 0.5;
            }
            let mut frame = lab.as_frame_scene_with_quality(quality);
            frame.post = Some(&post);
            let image = renderer.render_offscreen(&device, &frame).expect("frame");
            hashes.push(hash(&image.pixels));
        }
    }
    for (i, h) in hashes.iter().enumerate() {
        println!("hdr_scratch_frame {i:2} {h:016x}");
    }
    let report = renderer.graph_report();
    println!(
        "hdr_scratch_pool after the resize: {} textures, {} bytes",
        report.graph.transients.textures, report.graph.transients.bytes
    );
}

/// The main view's pooled bytes at 2560 × 1440, Desktop High, both scratch slots in use.
#[test]
#[ignore = "1440p measurement on lavapipe (minutes)"]
fn hdr_scratch_pool_bytes_at_1440p() {
    let device = device();
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let post = post();
    let mut renderer = renderer();
    let lab = scene(2560, 1440);
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    for _ in 0..3 {
        renderer.render_offscreen(&device, &frame).expect("frame");
    }
    let stats = renderer.graph_report().graph.transients;
    println!(
        "hdr_scratch_pool 1440p: {} textures, {} bytes ({:.1} MiB), created {}, reused {}",
        stats.textures,
        stats.bytes,
        stats.bytes as f64 / (1024.0 * 1024.0),
        stats.created,
        stats.reused
    );
}
