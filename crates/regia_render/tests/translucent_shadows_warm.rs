// SPDX-License-Identifier: MPL-2.0
//! Translucent shadow casters create no pipeline once the renderer is warm: the caster
//! pipelines are built with the frame's device pipelines, and a pane appearing, moving or
//! disappearing only records passes. Its own test binary: the pipeline counter is per process.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabLocalLight,
    LabMaterial, LabSpotLight, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
};

fn square(h: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [[-h, y, -h], [h, y, -h], [h, y, h], [-h, y, h]];
    let vertices = corners
        .into_iter()
        .map(|position| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0]))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

#[test]
fn translucent_shadows_create_no_pipeline_after_warm_up() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip translucent shadow warm-up: {err}");
            return;
        }
    };
    let floor = square(6.0, 0.0);
    let pane = square(1.0, 1.0);
    let floor_mat = LabMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [0.2, 0.3, 0.4, 0.7],
        alpha_mode: ALPHA_BLEND,
        ..LabMaterial::default()
    };
    let tinted = LabMaterial {
        base_color: [0.9, 0.7, 0.5, 1.0],
        transmission: 1.0,
        double_sided: true,
        ..LabMaterial::default()
    };
    let lights = [LabLocalLight::Spot(LabSpotLight {
        position: [0.0, 3.0, 0.0],
        direction: [0.0, -1.0, 0.001],
        color: [1.0; 3],
        intensity: 500.0,
        range: 8.0,
        inner_angle: 0.5,
        outer_angle: 0.7,
        cast_shadows: true,
        source_radius: None,
    })];
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    let frame = |offset: f32, panes: usize| {
        let mut meshes = vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 101,
            object_key: 201,
            vertices: &floor.0,
            indices: &floor.1,
            material: &floor_mat,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }];
        for (i, material) in [&glass, &tinted].into_iter().take(panes).enumerate() {
            meshes.push(FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 102 + i as u64,
                object_key: 202 + i as u64,
                vertices: &pane.0,
                indices: &pane.1,
                material,
                model: Mat4::from_translation(Vec3::new(offset + i as f32 * 0.5, i as f32 * 0.3, 0.0))
                    .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        FrameScene {
            width: 64,
            height: 48,
            eye: [0.0, 2.0, 5.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.8,
            near: 0.1,
            far: 40.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 1_000.0,
            double_sided: false,
            post: None,
            fog: regia_render::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0; 3], 50_000.0),
            local_lights: &lights,
            quality,
            meshes,
        }
    };
    let mut renderer = FrameRenderer::new();
    // Warm-up: no pane, then both (the maps are allocated on first use).
    for panes in [0, 2, 2, 1, 2] {
        renderer
            .render_offscreen(&device, &frame(0.0, panes))
            .expect("warm-up");
    }
    let report = renderer.shadow_cache_report().clone();
    assert_eq!(report.translucent_casters, 2, "{report:?}");
    assert!(report.translucent_passes >= 2, "{report:?}");
    assert!(report.translucent_draws > 0, "{report:?}");
    let created = regia_render::pipeline_cache::pipelines_created();
    for (step, panes) in [2usize, 1, 0, 2, 2, 1].into_iter().enumerate() {
        renderer
            .render_offscreen(&device, &frame(step as f32 * 0.1, panes))
            .expect("frame");
    }
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        created,
        "a translucent shadow frame after warm-up created a pipeline"
    );
}
