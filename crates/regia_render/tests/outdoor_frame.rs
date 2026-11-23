// SPDX-License-Identifier: MPL-2.0
//! Desktop High frame hook: an empty view becomes the physical sky, and IBL follows the time of day.

use regia_render::{
    FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, TimeOfDay,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip outdoor frame: {err}");
            None
        }
    }
}

fn high_quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality
}

fn mean(pixels: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for px in pixels.as_chunks::<4>().0 {
        sum += 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
        n += 1.0;
    }
    sum / n.max(1.0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn empty_desktop_high_frame_is_the_physical_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let scene = LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 2.0, 0.0],
        target: [0.0, 2.4, -8.0],
        fov_y: 0.8,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 14.0,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        ..OutdoorParams::default()
    });
    let frame = scene.as_frame_scene_with_quality(high_quality());
    let image = renderer.render_offscreen(&device, &frame).expect("sky frame");
    assert!(
        mean(&image.pixels) > 12.0,
        "empty frame stayed on the dark clear: {}",
        mean(&image.pixels)
    );
}

#[test]
fn atmosphere_ibl_noon_is_brighter_than_night() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut material = LabMaterial::EMPTY;
    material.roughness = 1.0;
    material.metallic = 0.0;
    let mesh = LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-4.0, 0.0, -4.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([4.0, 0.0, -4.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([4.0, 0.0, 4.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-4.0, 0.0, 4.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    };
    let scene = LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 6.0, 8.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.7,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 12.0,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![mesh],
    };
    let mut renderer = FrameRenderer::new();
    let quality = high_quality();
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        ..OutdoorParams::default()
    });
    let noon = renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
        .expect("noon ibl");
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::night()),
        ..OutdoorParams::default()
    });
    let night = renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
        .expect("night ibl");
    let noon_l = mean(&noon.pixels);
    let night_l = mean(&night.pixels);
    assert!(
        noon_l > night_l * 2.0 + 4.0,
        "ibl noon {noon_l} vs night {night_l}"
    );
}
