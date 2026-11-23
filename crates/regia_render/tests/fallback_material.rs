// SPDX-License-Identifier: MPL-2.0
//! A mesh whose base colour texture is missing draws the neutral fallback (mid grey,
//! roughness 0.7), not its authored tint factor alone (a blue faceted patch on DO27's set
//! pieces when a texture did not resolve).

use regia_asset::{MaterialAssetRon, TextureSlot};
use regia_render::hard_surface::{AuthoredSurfaceMaps, ResolvedBaseMaps};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, RenderDevice,
};

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

fn render(material: &LabMaterial) -> [u8; 4] {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let scene = FrameScene {
        width: 64,
        height: 64,
        eye: [0.0, 4.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.8),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    let pixels = FrameRenderer::new()
        .render_offscreen(&device, &scene)
        .expect("frame")
        .pixels;
    let i = ((32 * 64 + 32) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

#[test]
fn a_mesh_with_a_missing_albedo_renders_grey_not_blue() {
    let mut asset = MaterialAssetRon::default_standard();
    // A blue tint meant to multiply the texture.
    asset.base.base_color = [0.25, 0.45, 1.0, 1.0];
    asset.base.metallic = 0.6;
    asset.base.base_color_map = Some(TextureSlot::Path("textures/ice_albedo.png".into()));
    let material = LabMaterial::from_authored_resolved(
        "SM_Test/M_Ice",
        &asset,
        ResolvedBaseMaps::default(),
        AuthoredSurfaceMaps::default(),
    );
    assert_eq!(material.base_color, [0.5, 0.5, 0.5, 1.0]);
    assert_eq!(material.metallic, 0.0);
    assert!((material.roughness - 0.7).abs() < 1e-6);
    assert!(material.normal_map.is_none());

    let px = render(&material);
    eprintln!("fallback pixel {px:?}");
    let (r, g, b) = (f32::from(px[0]), f32::from(px[1]), f32::from(px[2]));
    assert!(r > 20.0, "the fallback must be lit, got {px:?}");
    assert!(
        (b - r).abs() <= 6.0 && (b - g).abs() <= 6.0,
        "a missing albedo must render neutral gray, got {px:?}"
    );

    // The same asset with its texture resolved keeps the authored factor.
    let resolved = LabMaterial::from_authored_resolved(
        "SM_Test/M_Ice",
        &asset,
        ResolvedBaseMaps {
            albedo: Some(regia_render::LabTexture {
                width: 1,
                height: 1,
                rgba: vec![255, 255, 255, 255],
            }),
            ..Default::default()
        },
        AuthoredSurfaceMaps::default(),
    );
    assert_eq!(resolved.base_color, [0.25, 0.45, 1.0, 1.0]);
    let tinted = render(&resolved);
    assert!(
        u16::from(tinted[2]) > u16::from(tinted[0]) + 20,
        "resolved texture keeps the authored tint: {tinted:?}"
    );
}
