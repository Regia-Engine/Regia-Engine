// SPDX-License-Identifier: MPL-2.0
//! Pre-compressed material maps end to end: an RGBA8 base colour cooked to BC7 sRGB KTX2
//! (`cook_texture_rgba8`, what `cook_material_maps` writes), read back as a
//! `regia_render::CompressedTexture` and drawn in place of the RGBA8 slot. The frame stays
//! within BC7's error of the RGBA8 frame, and the map costs a quarter of the GPU memory.

use std::sync::Arc;

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::{TextureCookQuality, cook_texture_rgba8};
use regia_render::{
    CompressedMaps, CompressedTexture, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality,
    FrameRenderer, FrameScene, LabMaterial, LabTexture, LabVertex,
};

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [(x + 2.0) / 4.0, (z + 2.0) / 4.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

/// Smooth colour ramps with a coarse checker: BC7-friendly, but every region distinct.
fn albedo(size: u32) -> LabTexture {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let checker = if ((x / 32) + (y / 32)) % 2 == 0 { 40 } else { 0 };
            rgba.extend_from_slice(&[
                ((x * 200) / size) as u8 + checker,
                ((y * 200) / size) as u8 + checker,
                120,
                255,
            ]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

fn render(
    renderer: &mut FrameRenderer,
    device: &regia_render::RenderDevice,
    material: &LabMaterial,
) -> Vec<u8> {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let scene = FrameScene {
        width: 96,
        height: 96,
        eye: [0.0, 3.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.1,
        near: 0.05,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 1.0),
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
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

#[test]
fn a_bc7_base_colour_draws_like_the_rgba8_one_at_a_quarter_of_the_memory() {
    let device = match regia_terrain::gpu::headless_bcn_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device with BC (lavapipe?): {err}"),
    };
    let image = albedo(256);
    let cooked = cook_texture_rgba8(
        256,
        256,
        &image.rgba,
        TextureCookUsage::ColorSrgb,
        "linux",
        TextureCookQuality::High,
    )
    .expect("cook");
    let map = CompressedTexture::from_ktx2(&cooked.ktx2).expect("ktx2");
    assert_eq!((map.width, map.height), (256, 256));
    assert!(map.format.is_srgb() && format!("{:?}", map.format).starts_with("Bc7"));
    assert!(map.levels.len() >= 7, "the cooked mip chain comes along");

    let rgba8 = LabMaterial {
        albedo: Some(image),
        roughness: 0.8,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    let bc7 = LabMaterial {
        albedo: None,
        compressed: Some(Arc::new(CompressedMaps {
            albedo: Some(map.clone()),
            ..CompressedMaps::default()
        })),
        ..rgba8.clone()
    };
    let a = render(&mut FrameRenderer::new(), &device, &rgba8);
    // Kept alive: its textures are what the memory report below reads.
    let mut bc7_renderer = FrameRenderer::new();
    let b = render(&mut bc7_renderer, &device, &bc7);
    let max = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
    let mean = a
        .iter()
        .zip(&b)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum::<f64>()
        / a.len() as f64;
    assert!(
        max <= 24 && mean < 3.0,
        "BC7 frame within the encoder's error: max {max}, mean {mean:.2}"
    );
    let white = render(
        &mut FrameRenderer::new(),
        &device,
        &LabMaterial {
            albedo: None,
            ..rgba8.clone()
        },
    );
    let off = b
        .iter()
        .zip(&white)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum::<f64>()
        / b.len() as f64;
    assert!(
        off > 10.0,
        "the compressed map is sampled, not the white default ({off:.1})"
    );

    let report = regia_render::memory_report(&device);
    if report.available {
        let bcn: u64 = report
            .labels
            .iter()
            .filter(|l| l.label == "material texture (bcn)")
            .map(|l| l.bytes)
            .sum();
        assert!(bcn > 0, "the BC7 map is labeled");
        // 256² BC7 with mips ≈ 87 KiB; RGBA8 would be ≈ 350 KiB.
        assert!(bcn < 160 * 1024, "BC7 map resident bytes {bcn}");
        assert_eq!(
            map.bytes_from(0),
            map.levels.iter().map(|l| l.len() as u64).sum::<u64>()
        );
    }
}
