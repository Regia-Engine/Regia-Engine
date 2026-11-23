// SPDX-License-Identifier: MPL-2.0
//! The lite forward path's surface-weather warm (spec 99 § 6.5, ADR-2032): with
//! `FrameRenderer::set_warm_surface_weather(true)` (a project with `effects.surface` on) the first
//! lite frame builds the wet variant beside the dry one, so the first wet frame compiles nothing;
//! without it the first wet frame composes the variant. One test in its own binary: the pipeline
//! counter (`pipeline_cache::pipelines_created`) is process-global. Lavapipe never takes the lite
//! path by itself, so the renderer forces it (`FrameRenderer::set_forward_lite`).

use regia_render::forward_lite::ForwardLite;
use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice, SurfaceWeatherFrame,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A concrete floor under the sun.
fn scene() -> LabScene {
    let corners = [
        [-9.0, 0.0, -9.0],
        [9.0, 0.0, -9.0],
        [9.0, 0.0, 9.0],
        [-9.0, 0.0, 9.0],
    ];
    let vertices = corners
        .into_iter()
        .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
        .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], uv))
        .collect();
    LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 7.0, 9.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 60.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    match renderer.render_offscreen(device, &lab.as_frame_scene_with_quality(FrameQuality::LAB)) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    }
}

/// Pipelines the first wet frame creates after two dry ones (warm on or off), and the dry and wet
/// pictures.
fn created_by_the_first_wet_frame(
    device: &RenderDevice,
    tier: ForwardLite,
    warm: bool,
) -> (u64, Vec<u8>, Vec<u8>) {
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(tier));
    renderer.set_warm_surface_weather(warm);
    let dry = render(&mut renderer, device, &lab);
    // A second dry frame: everything the first one builds lazily is built.
    render(&mut renderer, device, &lab);
    let before = pipelines_created();
    renderer.set_surface_weather(SurfaceWeatherFrame {
        wetness: 1.0,
        ..SurfaceWeatherFrame::DRY
    });
    let wet = render(&mut renderer, device, &lab);
    (pipelines_created() - before, dry, wet)
}

#[test]
fn a_warmed_lite_renderer_compiles_nothing_on_its_first_wet_frame() {
    let device = device();
    for tier in [ForwardLite::Low, ForwardLite::High] {
        let (cold, cold_dry, cold_wet) = created_by_the_first_wet_frame(&device, tier, false);
        assert!(
            cold > 0,
            "{tier:?}: without the warm the wet frame composes the variant"
        );
        let (warm, warm_dry, warm_wet) = created_by_the_first_wet_frame(&device, tier, true);
        assert_eq!(
            warm, 0,
            "{tier:?}: the warmed renderer built the variant with the dry frame"
        );
        // The warm changes no picture: the dry and the wet frames are the cold renderer's.
        assert_eq!(warm_dry, cold_dry, "{tier:?}: dry");
        assert_eq!(warm_wet, cold_wet, "{tier:?}: wet");
        assert_ne!(cold_dry, cold_wet, "{tier:?}: the wet frame is wet");
    }
}
