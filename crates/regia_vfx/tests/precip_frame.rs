// SPDX-License-Identifier: MPL-2.0
//! Rain through the real frame path (spec 99 § 11): the layer draws in the frame renderer's
//! translucent pass, asks for the occlusion view, and the drops and splashes keep out from under a
//! roof. The oracle is geometric: the eye stands under the slab near its edge, so every ray left of
//! the center stays under it.

use regia_test_utils::serial_gpu::{SerialGpu, serial_device};
use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    QualityTier, RenderDevice,
};
use regia_vfx::precip::{PrecipState, PrecipitationGpu};
use regia_vfx::runtime::translucent_layer_with_precipitation;
use regia_vfx::{
    PrecipBudget, PrecipIntegrator, PrecipitationHandle, PrecipitationSettings, VfxGpu, VfxProfile,
    WeatherConfig, WeatherState,
};

const W: u32 = 480;
const H: u32 = 270;
const DT: f32 = 1.0 / 60.0;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> SerialGpu {
    match serial_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    (
        corners
            .into_iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        vec![0, 2, 1, 0, 3, 2],
    )
}

fn scene() -> LabScene {
    let (floor_v, floor_i) = quad(
        [
            [-60.0, 0.0, -60.0],
            [60.0, 0.0, -60.0],
            [60.0, 0.0, 60.0],
            [-60.0, 0.0, 60.0],
        ],
        [0.0, 1.0, 0.0],
    );
    // A slab over the eye, whose edge runs just right of it.
    let (roof_v, roof_i) = quad(
        [
            [-60.0, 3.0, -60.0],
            [-0.5, 3.0, -60.0],
            [-0.5, 3.0, 60.0],
            [-60.0, 3.0, 60.0],
        ],
        [0.0, -1.0, 0.0],
    );
    let dark = LabMaterial {
        base_color: [0.05, 0.05, 0.06, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [-1.0, 1.6, 8.0],
        target: [-1.0, 1.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 120.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![dark],
        meshes: vec![
            LabMesh {
                vertices: floor_v,
                indices: floor_i,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: roof_v,
                indices: roof_i,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn stepped_state(state: WeatherState) -> PrecipState {
    let config = WeatherConfig::default();
    let settings = PrecipitationSettings {
        enabled: true,
        ..PrecipitationSettings::from_config(&config)
    };
    let rates = state.rates(&config);
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut integrator = PrecipIntegrator::default();
    for _ in 0..10 {
        integrator.step(&rates, &config, [0.0, 0.0], &budget, DT);
    }
    PrecipState {
        integrator,
        rates,
        config,
        settings,
        quality: QualityTier::High,
        active: true,
        clock_s: 1.0,
        ..PrecipState::default()
    }
}

fn renderer(device: &RenderDevice, state: Option<PrecipState>) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    let gpu = match VfxGpu::new(device, VfxProfile::default()) {
        Ok(gpu) => gpu,
        Err(err) => panic!("vfx: {err:?}"),
    };
    let handle = PrecipitationHandle::default();
    if let Some(state) = state {
        if let Ok(mut slot) = handle.state.lock() {
            *slot = state;
        }
        if let Ok(mut slot) = handle.gpu.lock() {
            *slot = PrecipitationGpu::new(device, &VfxProfile::default()).ok();
        }
    }
    let layer = translucent_layer_with_precipitation(Arc::new(Mutex::new(gpu)), handle, device.queue.clone());
    renderer.set_translucent_layers(vec![layer]);
    renderer
        .set_precipitation_occlusion_budget(PrecipBudget::for_quality(QualityTier::High).occlusion, 16_384);
    renderer
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    match renderer.render_offscreen(device, &lab.as_frame_scene_with_quality(FrameQuality::LAB)) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    }
}

/// Pixels of columns `x0..x1` whose largest channel moved by more than 1 of 255.
fn changed(a: &[u8], b: &[u8], (x0, x1): (u32, u32)) -> usize {
    let mut count = 0;
    for y in 0..H {
        for x in x0..x1 {
            let at = ((y * W + x) * 4) as usize;
            count += usize::from((0..3).any(|c| a[at + c].abs_diff(b[at + c]) > 1));
        }
    }
    count
}

/// Rain in the open half; nothing, drop or splash, in the half under the slab.
#[test]
fn nothing_falls_under_the_roof() {
    let device = device();
    let lab = scene();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let mut rainy = renderer(&device, Some(stepped_state(WeatherState::STORM)));
    let rain = render(&mut rainy, &device, &lab);
    let info = rainy
        .precipitation_occlusion()
        .expect("the layer asked for the view");
    assert_eq!(info.redraws, 1, "{info:?}");
    assert_eq!(info.tilt_deg, 0.0, "no wind");
    let sheet = std::env::var_os("REGIA_PRECIP_SHEET_DIR");
    if let Some(dir) = sheet {
        let path = std::path::Path::new(&dir).join("frame_storm.png");
        let file = std::fs::File::create(&path).expect("file");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("header")
            .write_image_data(&rain)
            .expect("pixels");
    }
    let open = changed(&plain, &rain, (W / 2 + 60, W));
    let covered = changed(&plain, &rain, (0, W / 2 - 2));
    assert!(open > 500, "rain in the open: {open} pixels");
    let mut places = Vec::new();
    for y in 0..H {
        for x in 0..W / 2 - 2 {
            let at = ((y * W + x) * 4) as usize;
            if (0..3).any(|c| plain[at + c].abs_diff(rain[at + c]) > 1) {
                places.push((x, y, [plain[at], rain[at]]));
            }
        }
    }
    assert_eq!(covered, 0, "rain under the roof: {places:?}");
}

/// Without rain the layer asks for no view and draws no pixel.
#[test]
fn clear_weather_asks_for_nothing() {
    let device = device();
    let lab = scene();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let mut clear = renderer(&device, Some(stepped_state(WeatherState::CLEAR)));
    let drawn = render(&mut clear, &device, &lab);
    assert_eq!(clear.precipitation_occlusion(), None);
    assert_eq!(plain, drawn);
}

/// CPU frame time with the module off and at the tier budget (correctness only on lavapipe):
/// `cargo test --release -p regia_vfx --test precip_frame bench -- --ignored --nocapture`.
#[test]
#[ignore = "prints numbers"]
fn bench_precipitation_frame_cost() {
    let device = device();
    let lab = scene();
    let measure = |state: Option<PrecipState>, label: &str| {
        let mut renderer = renderer(&device, state);
        render(&mut renderer, &device, &lab);
        let started = std::time::Instant::now();
        for _ in 0..30 {
            render(&mut renderer, &device, &lab);
        }
        println!(
            "{label}: {:.2} ms a frame",
            started.elapsed().as_secs_f64() * 1000.0 / 30.0
        );
    };
    measure(None, "off");
    measure(
        Some(stepped_state(WeatherState::STORM)),
        "storm rain, High budget",
    );
    measure(
        Some(stepped_state(WeatherState::BLIZZARD)),
        "blizzard snow, High budget",
    );
}
