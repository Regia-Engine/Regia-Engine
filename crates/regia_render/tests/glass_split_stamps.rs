// SPDX-License-Identifier: MPL-2.0
//! The glass section's split stamps (ADR-7986): the refraction copy and the glass front-layer pass
//! run before the forward pass's translucent section (`forward_glass`), and are timed by their own
//! four stamps (`GpuTimings::{refraction_copy_ms, glass_front_layer_ms}`). A frame with a visible
//! refracting pane takes the copy and the front layer, so it writes the four stamps inside the
//! forward node; a frame whose pane is behind the camera takes neither, writes none of them and
//! reads 0 for both. Counters only (which stamps the frame wrote), no wall-clock bound and no
//! golden image. Lavapipe has timestamps; a device without `TIMESTAMP_QUERY` times nothing, which
//! the test reports and checks instead of passing on an empty list.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GpuTimings, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

/// The four split stamps: before and after the copy, before and after the front layer.
const SPLIT: std::ops::Range<u32> = 45..49;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3], material: usize) -> LabMesh {
    let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    LabMesh {
        vertices: (0..4)
            .map(|k| LabVertex::from_pos_normal_uv(corners[k], normal, uv[k]))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2, 0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A ground plane and a refracting pane at `pane_z` (in front of the eye at z = 6 when below 6).
fn scene(pane_z: f32) -> LabScene {
    LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 1.6, 6.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.35, 0.42, 0.22, 1.0],
                roughness: 0.85,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.9, 1.0, 0.2],
                transmission: 1.0,
                transmission_thickness: 0.05,
                roughness: 0.05,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            quad(
                [
                    [-8.0, 0.0, -8.0],
                    [8.0, 0.0, -8.0],
                    [8.0, 0.0, 8.0],
                    [-8.0, 0.0, 8.0],
                ],
                [0.0, 1.0, 0.0],
                0,
            ),
            quad(
                [
                    [-1.0, 0.4, pane_z],
                    [1.0, 0.4, pane_z],
                    [1.0, 2.2, pane_z],
                    [-1.0, 2.2, pane_z],
                ],
                [0.0, 0.0, 1.0],
                1,
            ),
        ],
    }
}

/// Four Desktop High frames with GPU timing on; the last one's timings.
fn timed(device: &RenderDevice, pane_z: f32) -> GpuTimings {
    let profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    let mut quality = FrameQuality::from_profile(&profile);
    quality.screen_space_refraction = true;
    quality.msaa_samples = 1;
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let mut lab = scene(pane_z);
    lab.post = Some(post.clone());
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    for i in 0..4 {
        renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame {i}: {err}"));
    }
    renderer.gpu_timings()
}

#[test]
fn the_refraction_copy_and_front_layer_are_stamped_only_when_glass_is_on_screen() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let on_screen = timed(&device, 2.0);
    if !device.capabilities.timestamp_query {
        eprintln!("no TIMESTAMP_QUERY on this device: nothing is timed");
        assert!(on_screen.stamps().is_empty(), "{:?}", on_screen.stamps());
        return;
    }
    let written = |gpu: &GpuTimings| -> Vec<u32> {
        gpu.stamps()
            .iter()
            .map(|stamp| stamp.query)
            .filter(|query| SPLIT.contains(query))
            .collect()
    };
    let split = on_screen.stamps();
    assert_eq!(
        written(&on_screen),
        SPLIT.collect::<Vec<_>>(),
        "a frame with a visible refracting pane writes the copy and front-layer stamps: {split:?}"
    );
    for stamp in split.iter().filter(|stamp| SPLIT.contains(&stamp.query)) {
        assert!(
            stamp.owner == "forward" && stamp.in_owner,
            "split stamp {stamp:?} must sit inside the forward node"
        );
    }
    eprintln!(
        "on screen: copy {:.4} ms, front layer {:.4} ms, glass {:.4} ms",
        on_screen.refraction_copy_ms, on_screen.glass_front_layer_ms, on_screen.forward_glass_ms
    );
    // Behind the eye (z = 6): no visible refracting draw, so no copy and no front layer.
    let behind = timed(&device, 9.0);
    assert!(
        written(&behind).is_empty(),
        "a frame without a visible refracting draw takes no copy: {:?}",
        behind.stamps()
    );
    assert_eq!(behind.refraction_copy_ms, 0.0);
    assert_eq!(behind.glass_front_layer_ms, 0.0);
}
