// SPDX-License-Identifier: MIT-0
//! A cooked material reproduces what the hero harness draws from the glTF (lane cooked-materials).
//!
//! DO27's X12 canopy (`KHR_materials_transmission`, `_volume`, `_ior`) and a building (packed ORM,
//! `KHR_materials_emissive_strength` lamps) go through the engine's import mapping
//! (`regia_asset_pipeline::gltf_materials`), the cooked material payload (postcard,
//! `COOKED_MATERIAL_VERSION`) and the runtime bridge (`LabMaterial::from_authored_resolved`).
//! Rendered with the same textures as the harness's direct glTF mapping, the frames are
//! pixel-identical. Texture compression is not under test (both sides get the source PNGs).
//!
//! Needs `REGIA_DO27_CONTENT`; without it the test passes after a note.

use lookdev::hero::{self, HeroAsset};
use regia_asset::CookedStandardMaterial;
use regia_core::glam::Vec3;
use regia_render::hard_surface::{AuthoredSurfaceMaps, ResolvedBaseMaps};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    LabMaterial, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 320;
const H: u32 = 180;

fn render(device: &RenderDevice, model: &hero::HeroModel, materials: Vec<LabMaterial>) -> Vec<u8> {
    let centre = (model.min + model.max) * 0.5;
    let size = (model.max - model.min).length().max(1.0);
    let eye = centre + Vec3::new(0.6, 0.35, 0.75).normalize() * size * 0.9;
    let post = PostSettings {
        exposure_ev: 13.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: eye.to_array(),
        target: centre.to_array(),
        fov_y: 0.8,
        near: 0.05,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.4, -0.7, -0.3], [1.0, 0.95, 0.9], 60_000.0),
        local_lights: Vec::new(),
        materials,
        meshes: model.meshes.clone(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
        ..OutdoorParams::default()
    });
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

/// The cooked materials of `file`, with the harness's resolved images for each slot.
fn cooked_materials(file: &std::path::Path, direct: &[LabMaterial]) -> Vec<LabMaterial> {
    let authored = regia_asset_pipeline::gltf_materials(file).expect("import");
    assert_eq!(authored.len(), direct.len(), "{}: material count", file.display());
    authored
        .into_iter()
        .zip(direct)
        .map(|(asset, d)| {
            // Through the cooked payload and back, as a pak load does.
            let bytes = CookedStandardMaterial::new(asset)
                .to_postcard()
                .expect("postcard");
            let cooked = CookedStandardMaterial::from_postcard(&bytes).expect("cooked material");
            let name = cooked.authored.name.clone().unwrap_or_default();
            LabMaterial::from_authored_resolved(
                &name,
                &cooked.authored,
                ResolvedBaseMaps {
                    albedo: d.albedo.clone(),
                    normal: d.normal_map.clone(),
                    occlusion: d.occlusion_map.clone(),
                    emissive: d.emissive_map.clone(),
                },
                AuthoredSurfaceMaps::default(),
            )
        })
        .collect()
}

#[test]
fn cooked_hero_materials_render_like_the_direct_gltf() {
    let Some(content) = lookdev::do27::content_dir() else {
        eprintln!("REGIA_DO27_CONTENT is not set: the hero cook comparison needs DO27's drop");
        return;
    };
    let device = regia_terrain::gpu::headless_bcn_device().expect("device");
    for rel in [
        "hero/vehicles/x12/SM_X12_Glass_00.gltf",
        "hero/buildings/silo/SM_Silo_Rem_LOD0.gltf",
    ] {
        let file = content.join(rel);
        let asset = HeroAsset {
            name: rel.into(),
            files: vec![file.clone()],
            planet: lookdev::Planet::Mars,
        };
        let model = hero::load(&asset, 512).expect("hero load");
        let cooked = cooked_materials(&file, &model.materials);
        for (c, d) in cooked.iter().zip(&model.materials) {
            assert_eq!(c.transmission, d.transmission, "{rel}: transmission");
            assert_eq!(c.ior, d.ior, "{rel}: ior");
            assert_eq!(c.transmission_tint, d.transmission_tint, "{rel}: volume tint");
            assert_eq!(
                c.transmission_thickness, d.transmission_thickness,
                "{rel}: volume thickness"
            );
            assert_eq!(c.alpha_mode, d.alpha_mode, "{rel}: alpha mode");
            assert_eq!(c.emissive, d.emissive, "{rel}: emissive");
            assert_eq!(
                c.emissive_intensity, d.emissive_intensity,
                "{rel}: emissive strength"
            );
            assert_eq!(c.metallic, d.metallic, "{rel}: metallic");
            assert_eq!(c.roughness, d.roughness, "{rel}: roughness");
        }
        let a = render(&device, &model, model.materials.clone());
        let b = render(&device, &model, cooked);
        let diff = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        eprintln!("{rel}: direct vs cooked max channel difference {diff}");
        assert_eq!(
            diff, 0,
            "{rel}: the cooked material renders differently from the glTF"
        );
    }
}
