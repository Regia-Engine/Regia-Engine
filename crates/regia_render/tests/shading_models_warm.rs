// SPDX-License-Identifier: MPL-2.0
//! Character shading models create no pipeline once the renderer is warm: skin, eye and sheen
//! materials branch on their material uniform inside the standard forward pipelines, so turning
//! a model on, switching it and turning it off only rewrites uniforms. Its own test binary: the
//! pipeline counter is per process.

use regia_core::glam::{Mat4, Vec3};
use regia_render::shading_model::{EyeShading, FabricSheen, MaterialShading, SkinShading};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabLocalLight,
    LabMaterial, LabPointLight, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
};

#[test]
fn shading_models_create_no_pipeline_after_warm_up() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip shading model warm-up: {err}");
            return;
        }
    };
    let (vertices, indices) = regia_render::primitives::uv_sphere(0.05, 24, 16);
    let opaque = LabMaterial {
        base_color: [0.8, 0.55, 0.45, 1.0],
        ..LabMaterial::default()
    };
    let masked = LabMaterial {
        alpha_mode: ALPHA_MASK,
        ..opaque.clone()
    };
    let lights = [LabLocalLight::Point(LabPointLight {
        position: [0.0, 0.1, -0.2],
        color: [1.0; 3],
        intensity: 800.0,
        radius: 2.0,
        cast_shadows: true,
        source_radius: None,
    })];
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 512;
    let frame = || FrameScene {
        width: 64,
        height: 48,
        eye: [0.0, 0.0, 0.4],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.01,
        far: 10.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 1_000.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.2], [1.0; 3], 50_000.0),
        local_lights: &lights,
        quality,
        meshes: [(&opaque, -0.06f32), (&masked, 0.06)]
            .into_iter()
            .enumerate()
            .map(|(i, (material, x))| FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 31 + i as u64,
                object_key: 41 + i as u64,
                vertices: &vertices,
                indices: &indices,
                material,
                model: Mat4::from_translation(Vec3::new(x, 0.0, 0.0)).to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            })
            .collect(),
    };
    let mut renderer = FrameRenderer::new();
    // Warm-up with standard materials only.
    for _ in 0..3 {
        renderer.render_offscreen(&device, &frame()).expect("warm-up");
    }
    let created = regia_render::pipeline_cache::pipelines_created();
    let skin = MaterialShading::skin(SkinShading::default());
    let eye = MaterialShading::eye(EyeShading::default());
    let velvet = MaterialShading::sheen(FabricSheen::default());
    let skin_sheen = MaterialShading {
        sheen: Some(FabricSheen::default()),
        ..skin
    };
    let steps: [&[(u64, MaterialShading)]; 6] = [
        &[(31, skin), (32, eye)],
        &[(31, velvet), (32, skin)],
        &[(31, eye), (32, velvet)],
        &[(31, skin_sheen)],
        &[],
        &[(32, skin)],
    ];
    for table in steps {
        renderer.set_material_shading(table.iter().copied());
        renderer.render_offscreen(&device, &frame()).expect("frame");
    }
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        created,
        "a shading model frame after warm-up created a pipeline"
    );
}
