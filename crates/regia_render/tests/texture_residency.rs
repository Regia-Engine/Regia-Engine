// SPDX-License-Identifier: MPL-2.0
//! Shared images upload once. A dynamic texture stays one GPU texture and one bind group
//! across a hundred writes.

use regia_core::glam::Mat4;
use regia_render::{
    DirectionalLightUniform, DynamicTexture, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabTexture, LabVertex, MaterialImage, MaterialImages, RenderDevice, SharedImage,
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

fn solid(size: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for _ in 0..size * size {
        pixels.extend_from_slice(&rgba);
    }
    pixels
}

fn render(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    materials: &[&LabMaterial],
    vertices: &[LabVertex],
) -> Vec<u8> {
    let indices = [0u32, 2, 1, 0, 3, 2];
    let meshes = materials
        .iter()
        .enumerate()
        .map(|(i, material)| FrameMesh {
            is_static: false,
            mesh_key: (i as u64) + 1,
            material_key: (i as u64) + 1,
            object_key: (i as u64) + 1,
            vertices,
            indices: &indices,
            material,
            model: Mat4::from_translation(regia_core::glam::Vec3::new(i as f32 * 6.0, 0.0, 0.0))
                .to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let scene = FrameScene {
        width: 64,
        height: 64,
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
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes,
    };
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

fn center(pixels: &[u8], width: u32) -> [u8; 4] {
    let x = width / 2;
    let y = width / 2;
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn base_color(renderer: &FrameRenderer) -> (usize, u64) {
    let report = renderer.texture_memory();
    let rows: Vec<_> = report
        .textures
        .iter()
        .filter(|t| t.slot == "base_color")
        .collect();
    (rows.len(), rows.iter().map(|t| t.gpu_bytes).sum())
}

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device (lavapipe)")
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn two_materials_sharing_one_image_upload_one_gpu_texture() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let pixels = solid(64, [220, 30, 40, 255]);
    let image = SharedImage::rgba(64, 64, pixels.clone(), true).expect("image");
    let shared = |image: SharedImage| LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        unlit: true,
        images: MaterialImages {
            albedo: Some(MaterialImage::Shared(image)),
            ..MaterialImages::EMPTY
        },
        ..LabMaterial::default()
    };
    let a = shared(image.clone());
    let b = shared(image);
    let mut renderer = FrameRenderer::new();
    let frame = render(&mut renderer, &device, &[&a, &b], &quad());
    let (count, bytes) = base_color(&renderer);
    assert_eq!(count, 1, "one GPU texture for both materials");
    let red = center(&frame, 64);
    assert!(
        red[0] > 180 && red[0] > red[1],
        "the shared image is sampled: {red:?}"
    );

    let private_tex = LabTexture {
        width: 64,
        height: 64,
        rgba: pixels,
    };
    let private = |tex: LabTexture| LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        unlit: true,
        albedo: Some(tex),
        ..LabMaterial::default()
    };
    let mut private_renderer = FrameRenderer::new();
    let _ = render(
        &mut private_renderer,
        &device,
        &[&private(private_tex.clone()), &private(private_tex)],
        &quad(),
    );
    let (private_count, private_bytes) = base_color(&private_renderer);
    assert_eq!(private_count, 2, "cloned RGBA8 images are not deduplicated");
    assert_eq!(
        private_bytes,
        bytes * 2,
        "the shared upload is half the private pair"
    );
    eprintln!(
        "texture residency: shared base_color count={count} bytes={bytes}; private count={private_count} bytes={private_bytes}"
    );
}

#[test]
fn a_hundred_dynamic_writes_allocate_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let dynamic = DynamicTexture::new(64, 64, true);
    dynamic.write(&solid(64, [220, 20, 20, 255])).expect("red");
    let material = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        unlit: true,
        images: MaterialImages {
            albedo: Some(MaterialImage::Dynamic(dynamic.clone())),
            ..MaterialImages::EMPTY
        },
        ..LabMaterial::default()
    };
    let vertices = quad();
    let mut renderer = FrameRenderer::new();
    let first = render(&mut renderer, &device, &[&material], &vertices);
    let red = center(&first, 64);
    assert!(
        red[0] > 180 && red[0] > red[1],
        "the first write is visible: {red:?}"
    );
    let _ = render(&mut renderer, &device, &[&material], &vertices);
    let before = regia_render::frame_path_counters();
    let resident = renderer.texture_memory().textures.len();
    for _ in 0..100 {
        dynamic.write(&solid(64, [20, 220, 30, 255])).expect("green");
        let _ = render(&mut renderer, &device, &[&material], &vertices);
    }
    let delta = regia_render::frame_path_counters().since(before);
    assert_eq!(delta.bind_groups, 0, "dynamic updates do not build bind groups");
    assert_eq!(delta.textures, 0, "dynamic updates do not create textures");
    assert_eq!(renderer.texture_memory().textures.len(), resident);
    let green = center(&render(&mut renderer, &device, &[&material], &vertices), 64);
    assert!(
        green[1] > 180 && green[1] > green[0],
        "the hundredth write replaced the texels: {green:?}"
    );
}
