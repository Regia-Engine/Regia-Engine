// SPDX-License-Identifier: MPL-2.0
//! Hair cards create no pipeline once the renderer is warm: the feature is a branch of the one
//! forward shader, so a hair material appearing, changing its strategy (hashed, sorted) or
//! disappearing only records draws. Its own test binary: the pipeline counter is per process.
//!
//! The first frames build the pipelines a hashed card, a sorted (blended) card and its shadow
//! need, the way a game's first frames with hair do; the frames after them must build none.

use regia_core::glam::Mat4;
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, HairAlpha, HairFactors,
    LabMaterial, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
};

fn quad(h: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [[-h, y, -h], [h, y, -h], [h, y, h], [-h, y, h]];
    let vertices = corners
        .into_iter()
        .map(|p| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

fn hair(alpha: HairAlpha) -> LabMaterial {
    let factors = HairFactors {
        alpha,
        ..HairFactors::strands()
    };
    let mut material = LabMaterial {
        double_sided: true,
        hair: factors,
        ..LabMaterial::default()
    };
    factors.apply_alpha_mode(&mut material.alpha_mode, &mut material.alpha_cutoff);
    material
}

#[test]
fn hair_cards_create_no_pipeline_after_warm_up() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip hair warm-up: {err}");
            return;
        }
    };
    let floor = quad(6.0, 0.0);
    let card = quad(0.8, 1.0);
    let floor_mat = LabMaterial {
        base_color: [0.8; 4],
        ..LabMaterial::default()
    };
    let hashed = hair(HairAlpha::Hashed);
    let sorted = hair(HairAlpha::Sorted);
    let coverage = hair(HairAlpha::Coverage);
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    fn frame<'a>(
        floor: &'a (Vec<LabVertex>, Vec<u32>),
        card: &'a (Vec<LabVertex>, Vec<u32>),
        floor_mat: &'a LabMaterial,
        quality: FrameQuality,
        materials: &[&'a LabMaterial],
        offset: f32,
    ) -> FrameScene<'a> {
        let mut meshes = vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 101,
            object_key: 201,
            vertices: &floor.0,
            indices: &floor.1,
            material: floor_mat,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }];
        for (i, material) in materials.iter().copied().enumerate() {
            meshes.push(FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 102 + i as u64,
                object_key: 202 + i as u64,
                vertices: &card.0,
                indices: &card.1,
                material,
                model: Mat4::from_translation(regia_core::glam::Vec3::new(
                    offset + i as f32 * 0.5,
                    i as f32 * 0.3,
                    0.0,
                ))
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
            local_lights: &[],
            quality,
            meshes,
        }
    }
    let mut renderer = FrameRenderer::new();
    // Warm-up: none, a hashed card, hashed + sorted, then all three.
    for materials in [
        vec![],
        vec![&hashed],
        vec![&hashed, &sorted],
        vec![&hashed, &sorted, &coverage],
    ] {
        renderer
            .render_offscreen(
                &device,
                &frame(&floor, &card, &floor_mat, quality, &materials, 0.0),
            )
            .expect("warm-up");
    }
    let created = regia_render::pipeline_cache::pipelines_created();
    let steps: [Vec<&LabMaterial>; 6] = [
        vec![&sorted, &hashed],
        vec![&hashed],
        vec![],
        vec![&coverage, &sorted],
        vec![&hashed, &hashed, &sorted],
        vec![&sorted],
    ];
    for (step, materials) in steps.iter().enumerate() {
        renderer
            .render_offscreen(
                &device,
                &frame(&floor, &card, &floor_mat, quality, materials, step as f32 * 0.1),
            )
            .expect("frame");
    }
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        created,
        "a hair frame after warm-up created a pipeline"
    );
}
