// SPDX-License-Identifier: MPL-2.0
//! Reflection probes captured under a planet atmosphere show that planet's sky and light.
//!
//! A probe face is an LDR frame (no post stack), and the LUT sky only draws into the HDR
//! target, so before v0.1.5 a capture on Mars drew the analytic Earth sky (blue zenith) and
//! swapped the shared IBL for the procedural Earth one: every probe DO27 captured on
//! Switchbacks held a blue sky, which the stochastic SSR's probe fallback then put on Mars
//! rock and armor.

use regia_render::{
    AtmosphereParameters, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SsaoMode, TimeOfDay,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip probe capture planet: {err}");
            None
        }
    }
}

fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.render_scale_permille = 1000;
    quality
}

fn afternoon() -> TimeOfDay {
    TimeOfDay {
        hour: 15.0,
        day_of_year: 172,
        latitude_deg: 30.0,
    }
}

/// A Desktop High view over a grey ground plane.
fn scene() -> LabScene {
    let mut ground = LabMaterial::EMPTY;
    ground.base_color = [0.18, 0.18, 0.18, 1.0];
    ground.roughness = 0.9;
    let n = [0.0, 1.0, 0.0];
    let s = 3000.0;
    LabScene {
        width: 96,
        height: 54,
        eye: [0.0, 2.0, 0.0],
        target: [0.0, 2.6, -10.0],
        fov_y: 1.0,
        near: 0.1,
        far: 8000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 13.5,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.3, -0.6, 0.4], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![ground],
        meshes: vec![LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([-s, 0.0, -s], n, [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([s, 0.0, -s], n, [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([s, 0.0, s], n, [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([-s, 0.0, s], n, [0.0, 1.0]),
            ],
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// Mean sRGB colour of the rows `y0..y1` of an RGBA8 image `width` wide.
fn mean_rows(pixels: &[u8], width: u32, y0: u32, y1: u32) -> [f32; 3] {
    let mut acc = [0.0f32; 3];
    let mut n = 0.0f32;
    for y in y0..y1 {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            for c in 0..3 {
                acc[c] += f32::from(pixels[i + c]);
            }
            n += 1.0;
        }
    }
    acc.map(|v| v / n.max(1.0))
}

fn dump(name: &str, pixels: &[u8], width: u32, height: u32) {
    if let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        let Ok(file) = std::fs::File::create(path) else {
            return;
        };
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(pixels);
        }
    }
}

fn mars_renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(afternoon()),
        planet: AtmosphereParameters::mars(),
        ..OutdoorParams::default()
    });
    renderer
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_probe_captured_on_mars_holds_the_mars_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let scene = scene();
    let frame = scene.as_frame_scene_with_quality(quality());
    let mut renderer = mars_renderer();
    let view = renderer.render_offscreen(&device, &frame).expect("mars frame");
    dump("probe_planet_view", &view.pixels, scene.width, scene.height);
    // The frame's own upper sky: butterscotch.
    let sky = mean_rows(&view.pixels, scene.width, 0, 8);
    assert!(sky[0] > sky[2] + 20.0, "mars frame sky not butterscotch: {sky:?}");
    let probe = renderer
        .capture_reflection_probe(&device, &frame, 1, [0.0, 2.0, 0.0], [20.0, 10.0, 20.0])
        .expect("capture");
    dump(
        "probe_planet_equirect",
        &probe.equirect,
        probe.width,
        probe.height,
    );
    // Equirect rows 0..h/2 are the upper hemisphere (row 0 the zenith): the probe's sky.
    let upper = mean_rows(&probe.equirect, probe.width, 1, probe.height / 2 - 3);
    eprintln!("mars frame sky {sky:?}, probe upper sky {upper:?}");
    assert!(
        upper[0] > upper[2] + 20.0,
        "probe sky is not the Mars sky (blue = the Earth analytic sky): {upper:?}"
    );
    // The ground under the probe is lit by the Mars sky, not tinted blue by the Earth IBL.
    let lower = mean_rows(
        &probe.equirect,
        probe.width,
        probe.height / 2 + 3,
        probe.height - 1,
    );
    eprintln!("probe ground {lower:?}");
    assert!(lower[0] >= lower[2], "probe ground lit blue: {lower:?}");
}

#[test]
fn a_probe_capture_does_not_move_a_held_camera() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // The camera is held; after the capture the host's next frame must still see no camera
    // motion. Each face stored its own view-projection as the "previous" one, so the next
    // frame measured its motion against a 90° face looking elsewhere (TAA history and motion
    // blur across the whole screen).
    let scene = scene();
    let mut quality = FrameQuality::LAB;
    quality.taau = true;
    quality.depth_velocity = true;
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut renderer = mars_renderer();
    let peak = |renderer: &mut FrameRenderer| {
        renderer.render_offscreen(&device, &frame).expect("frame");
        let field = renderer
            .read_motion(&device)
            .expect("read motion")
            .expect("velocity target");
        field
            .motion
            .iter()
            .map(|m| m[0].abs().max(m[1].abs()))
            .fold(0.0f32, f32::max)
    };
    for _ in 0..3 {
        peak(&mut renderer);
    }
    let held = peak(&mut renderer);
    let _ = renderer
        .capture_reflection_probe(&device, &frame, 1, [0.0, 2.0, 0.0], [20.0, 10.0, 20.0])
        .expect("capture");
    let after = peak(&mut renderer);
    eprintln!("held camera motion peak {held}, after a capture {after}");
    assert!(held < 1e-3, "a held camera moved: {held}");
    assert!(
        after < 1e-3,
        "after a probe capture the held camera's frame has motion {after} (uv)"
    );
}

#[test]
fn a_mars_ldr_frame_outside_a_capture_is_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // The Mars sky in the faces is for captures only: a host's own LDR frame on Mars (no
    // post stack, no LUT sky) keeps the analytic sky and fog it had in v0.1.4, before and
    // after a capture, so frames no probe touches cannot move in a patch release.
    let mut scene = scene();
    scene.post = None;
    let mut quality = FrameQuality::LAB;
    quality.taau = false;
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut earth = FrameRenderer::new();
    earth.set_outdoor(OutdoorParams {
        time: Some(afternoon()),
        ..OutdoorParams::default()
    });
    let reference = earth
        .render_offscreen(&device, &frame)
        .expect("earth frame")
        .pixels;
    let mut renderer = mars_renderer();
    let before = renderer
        .render_offscreen(&device, &frame)
        .expect("mars frame")
        .pixels;
    let _ = renderer
        .capture_reflection_probe(&device, &frame, 1, [0.0, 2.0, 0.0], [20.0, 10.0, 20.0])
        .expect("capture");
    let after = renderer
        .render_offscreen(&device, &frame)
        .expect("mars frame")
        .pixels;
    dump("probe_planet_ldr_earth", &reference, scene.width, scene.height);
    dump("probe_planet_ldr_mars", &after, scene.width, scene.height);
    let sky = |pixels: &[u8]| mean_rows(pixels, scene.width, 0, 8);
    let (want, b, a) = (sky(&reference), sky(&before), sky(&after));
    eprintln!("ldr sky earth {want:?}, mars before a capture {b:?}, after {a:?}");
    for (name, got) in [("before", b), ("after", a)] {
        for c in 0..3 {
            assert!(
                (got[c] - want[c]).abs() <= 2.0,
                "mars LDR frame {name} a capture left the analytic sky: {got:?} vs {want:?}"
            );
        }
    }
}
