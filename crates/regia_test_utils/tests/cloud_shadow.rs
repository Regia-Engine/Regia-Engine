// SPDX-License-Identifier: MPL-2.0
//! Cloud transmittance map, fog coupling and overcast sky light (ADR-320, ADR-321).
//!
//! The map (`clouds.wgsl` `fs_cloud_shadow_map`) is read by the forward sun term
//! (`lab_forward.wgsl`), the froxel fog's in-scatter (`volumetric_fog.wgsl`) and the terrain's
//! shared colour pass; `CloudSettings::overcast_ibl` dims and greys the sky light. All three
//! switches are off by default, and then the frame is the one before this lane: the
//! `clouds_noon` / `clouds_moved` goldens (`planet_frames.rs`) hold it byte for byte; the test
//! here checks that the map's size and extent change nothing while the switches are off.
//!
//! Lavapipe-deterministic: one frame per renderer (the first frame marches every cloud texel
//! and the whole map), fixed exposure, no temporal passes.

use regia_render::{
    AtmosphereParameters, CloudPattern, CloudSettings, DirectionalLightUniform, FrameQuality, FrameRenderer,
    FrameTarget, GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, TextureViewSettings, VolumetricFogSettings,
};

const W: u32 = 320;
const H: u32 = 180;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cloud shadow: {err}");
            None
        }
    }
}

/// Desktop High with the sky and clouds (and the froxel fog when `fog`): no screen-space, GI,
/// cascade or temporal passes that would move pixels on their own.
fn quality(fog: bool) -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.gi = GiMode::Ambient;
    quality.contact_shadows = false;
    quality.volumetric_fog = fog;
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 0;
    quality.reflection_probes = false;
    quality.parallax_occlusion = false;
    quality.screen_space_refraction = false;
    quality.volumetric_clouds = true;
    quality.physical_atmosphere = true;
    quality.near_sun_tile = 0;
    quality.translucent_shadow_cascades = 0;
    quality.translucent_local_shadows = false;
    quality.depth_velocity = false;
    quality.reflection_occlusion = false;
    quality.paged_sun_shadows = false;
    quality.cloud_reprojection = false;
    quality
}

fn material(color: [f32; 3]) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.base_color = [color[0], color[1], color[2], 1.0];
    mat.roughness = 0.9;
    mat
}

/// A ground plane seen from 30 m, looking down the +Z axis toward the horizon, under a sun
/// straight overhead (an authored light: no clock, so the split weather's edge lies on x = 0).
fn lab(width: u32, height: u32) -> LabScene {
    let n = [0.0, 1.0, 0.0];
    let (lo, hi) = (-12_000.0, 12_000.0);
    let vertices = vec![
        LabVertex::from_pos_normal_uv([lo, 0.0, lo], n, [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([hi, 0.0, lo], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([hi, 0.0, hi], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([lo, 0.0, hi], n, [0.0, 1.0]),
    ];
    LabScene {
        width,
        height,
        eye: [0.0, 30.0, 0.0],
        target: [0.0, 0.0, 400.0],
        fov_y: 55.0_f32.to_radians(),
        near: 0.3,
        far: 40_000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 14.5,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![material([0.3, 0.3, 0.28])],
        meshes: vec![LabMesh {
            vertices,
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// Half the sky cloudy (world x ≥ 0), still air. A fine weather map (about 23 m texels) keeps
/// the bilinear edge narrow next to the ground the halves sample.
fn split(settings: CloudSettings) -> CloudSettings {
    CloudSettings {
        pattern: CloudPattern::SplitHemisphere,
        map_resolution: 1024,
        wind_m_per_s: [0.0, 0.0],
        coverage: 0.5,
        ..settings
    }
}

fn outdoor(settings: CloudSettings, planet: AtmosphereParameters, fog: bool) -> OutdoorParams {
    OutdoorParams {
        clouds: settings,
        planet,
        fog: if fog {
            VolumetricFogSettings {
                density: 2.0e-4,
                height_falloff: 1.0 / 1200.0,
                ..VolumetricFogSettings::default()
            }
        } else {
            VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    }
}

fn render_with(
    device: &RenderDevice,
    settings: CloudSettings,
    planet: AtmosphereParameters,
    fog: bool,
) -> Vec<u8> {
    let lab = lab(W, H);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(outdoor(settings, planet, fog));
    let scene = lab.as_frame_scene_with_quality(quality(fog));
    renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("cloud shadow frame: {err}"))
        .pixels
}

fn render(device: &RenderDevice, settings: CloudSettings) -> Vec<u8> {
    render_with(device, settings, AtmosphereParameters::earth(), false)
}

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

/// Mean luma of the pixel rectangle `[x0, x1) × [y0, y1)`.
fn mean_rect(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0.0;
    let mut count = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += luma(&pixels[i..i + 4]);
            count += 1.0;
        }
    }
    sum / f32::max(count, 1.0)
}

fn dump(name: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    if let Err(err) = image::save_buffer(&path, pixels, W, H, image::ColorType::Rgba8) {
        eprintln!("dump {name}: {err}");
    }
}

/// Mean ground luma of the darker and the brighter screen half (the split weather's edge is
/// the world x = 0 line, straight ahead), away from the edge column, in the lower part of the
/// frame (the ground). Which half is cloudy depends on the view's handedness, not tested here.
fn halves(pixels: &[u8]) -> (f32, f32) {
    let left = mean_rect(pixels, W / 20, H * 3 / 5, W * 2 / 5, H - 2);
    let right = mean_rect(pixels, W * 3 / 5, H * 3 / 5, W - W / 20, H - 2);
    (left.min(right), left.max(right))
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// With the three switches off the map's size and extent change nothing (the frame is the
/// pre-lane one, held by the `clouds_noon` golden), and a texture view with the map switches on
/// renders exactly as one with them off (texture views keep clouds off, ADR-184).
#[test]
fn switches_off_leave_the_frame_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let base = split(CloudSettings::default());
    let off = render(&device, base);
    let off_other_map = render(
        &device,
        CloudSettings {
            shadow_map_size: 128,
            shadow_map_extent_m: 5_000.0,
            ..base
        },
    );
    assert_eq!(off, off_other_map, "an unused map's size changed the frame");

    let view = |settings: CloudSettings| -> Vec<u8> {
        let lab = lab(W, H);
        let q = quality(false);
        let mut r = FrameRenderer::new();
        r.set_ddgi_enabled(Some(false));
        r.set_outdoor(outdoor(settings, AtmosphereParameters::earth(), false));
        let main = lab.as_frame_scene_with_quality(q);
        r.render_offscreen(&device, &main)
            .unwrap_or_else(|err| panic!("main frame: {err}"));
        let mut scene = lab.as_frame_scene_with_quality(q);
        scene.width = 64;
        scene.height = 64;
        r.render_texture_view(&device, 7, scene, &TextureViewSettings::default())
            .unwrap_or_else(|err| panic!("texture view: {err}"));
        r.read_texture_target(&device, 7)
            .unwrap_or_else(|err| panic!("read texture view: {err}"))
            .pixels
    };
    // `overcast_ibl` stays off here: a texture view lights with the IBL the main view baked
    // (ADR-184), so it shares the main view's overcast sky light by design.
    let maps_on = CloudSettings {
        shadow_map: true,
        fog_coupling: true,
        ..base
    };
    assert_eq!(view(base), view(maps_on), "a texture view read the cloud map");
}

/// `shadow_map` darkens the sun term under the cloudy half and leaves the ambient: the
/// shadowed ground is clearly darker than the lit half but not black (the old screen-space
/// multiply took a 0.35 floor of the whole pixel instead).
#[test]
fn shadow_map_shadows_the_sun_term_only() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let off = render(&device, split(CloudSettings::default()));
    let on = render(
        &device,
        split(CloudSettings {
            shadow_map: true,
            ..CloudSettings::default()
        }),
    );
    dump("cloud_shadow_off", &off);
    dump("cloud_shadow_on", &on);
    let (cloudy, clear) = halves(&on);
    let (cloudy_old, clear_old) = halves(&off);
    eprintln!(
        "ground luma: map cloudy {cloudy:.1} clear {clear:.1}; screen-space cloudy {cloudy_old:.1} clear {clear_old:.1}"
    );
    assert_ne!(on, off, "the map changed nothing");
    assert!(clear > 20.0, "the lit half should be sunlit ({clear})");
    assert!(
        cloudy < 0.8 * clear,
        "the cloudy half is not shadowed ({cloudy} vs {clear})"
    );
    assert!(cloudy > 2.0, "the shadowed ground kept no ambient ({cloudy})");
}

/// `fog_coupling`: the cloud layer shadows the fog's sunlight too, so the fog loses light
/// (never gains it) and the frame changes.
#[test]
fn fog_coupling_shadows_the_sunlit_air() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let planet = AtmosphereParameters::earth();
    let off = render_with(&device, split(CloudSettings::default()), planet, true);
    let on = render_with(
        &device,
        split(CloudSettings {
            fog_coupling: true,
            ..CloudSettings::default()
        }),
        planet,
        true,
    );
    dump("cloud_fog_off", &off);
    dump("cloud_fog_on", &on);
    let brighter = on
        .as_chunks::<4>()
        .0
        .iter()
        .zip(off.as_chunks::<4>().0)
        .filter(|(a, b)| luma(&a[..]) > luma(&b[..]) + 1.5)
        .count();
    let mean = |p: &[u8]| mean_rect(p, 0, 0, W, H);
    eprintln!(
        "fog coupling: mean luma on {:.2} off {:.2}; brighter pixels {brighter}",
        mean(&on),
        mean(&off)
    );
    assert_ne!(on, off, "fog coupling changed nothing");
    assert!(mean(&on) < mean(&off), "shadowed fog should lose light");
    assert!(
        brighter <= (W * H / 200) as usize,
        "{brighter} pixels got brighter with the cloud shadow in the fog"
    );
}

/// `overcast_ibl` dims the sky light of an Earth frame; on Mars it changes nothing, and an
/// airless world draws no clouds at all.
#[test]
fn overcast_ibl_dims_earth_and_leaves_mars_and_airless() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let overcast = |on: bool| CloudSettings {
        overcast_ibl: on,
        pattern: CloudPattern::Constant,
        wind_m_per_s: [0.0, 0.0],
        ..CloudSettings::coverage_layer(0.9)
    };
    for (name, planet, expect_change) in [
        ("earth", AtmosphereParameters::earth(), true),
        ("mars", AtmosphereParameters::mars(), false),
        ("airless", AtmosphereParameters::airless(), false),
    ] {
        let off = render_with(&device, overcast(false), planet, false);
        let on = render_with(&device, overcast(true), planet, false);
        dump(&format!("cloud_overcast_{name}"), &on);
        if expect_change {
            let ground = |p: &[u8]| mean_rect(p, 0, H * 3 / 5, W, H);
            eprintln!(
                "overcast {name}: ground luma on {:.2} off {:.2}",
                ground(&on),
                ground(&off)
            );
            assert!(
                ground(&on) < ground(&off),
                "{name}: the overcast did not dim the ground"
            );
        } else {
            assert_eq!(on, off, "{name}: the overcast switch changed the frame");
        }
    }
}

/// The `cloud_shadow_map` debug view draws the map over the frame: the cloudy half is dark,
/// the clear half bright.
#[test]
fn debug_view_shows_the_map() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let debug = render(
        &device,
        split(CloudSettings {
            shadow_map_debug: true,
            ..CloudSettings::default()
        }),
    );
    dump("cloud_shadow_debug", &debug);
    let (cloudy, clear) = halves(&debug);
    eprintln!("debug view: cloudy {cloudy:.1} clear {clear:.1}");
    assert!(
        clear > cloudy + 20.0,
        "the debug view does not show the split ({cloudy} vs {clear})"
    );
}

/// GPU cost of the map and the fog coupling at 1440p, coverage 0.5, 360 frames with the eye and
/// the wind moving (no readback, three frames in flight, 120 warm-up frames). The map pass is
/// recorded at the start of the frame, inside `shadows_ms` (this scene draws no cascades, so
/// that interval is the map); the coupling's cost is the change in `fog_ms`. Run with
/// `--ignored --nocapture` on the GPU.
#[test]
#[ignore = "1440p cloud-shadow timing burst"]
fn cloud_shadow_cost_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let info = device.adapter.get_info();
    eprintln!(
        "cloud shadow bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let (width, height) = (2560, 1440);
    let mut lab = lab(width, height);
    lab.eye = [0.0, 3.0, 0.0];
    lab.target = [0.0, 600.0, 1000.0];
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cloud shadow bench target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let frames: usize = regia_core::brand::env_var("CLOUD_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(360);
    for (name, shadow_map, fog_coupling) in [
        ("off", false, false),
        ("map", true, false),
        ("map_fog", true, true),
    ] {
        let settings = |k: usize| CloudSettings {
            shadow_map,
            fog_coupling,
            wind_m_per_s: [6.0, 2.0],
            time_s: k as f32 / 60.0,
            ..CloudSettings::coverage_layer(0.5)
        };
        let mut q = quality(true);
        q.cloud_reprojection = true;
        let mut r = FrameRenderer::new();
        r.set_ddgi_enabled(Some(false));
        r.set_gpu_timing(&device, true);
        let mut in_flight = std::collections::VecDeque::new();
        let mut map_ms = Vec::new();
        let mut fog_ms = Vec::new();
        for k in 0..frames {
            r.set_outdoor(outdoor(settings(k), AtmosphereParameters::earth(), true));
            let mut scene = lab.as_frame_scene_with_quality(q);
            scene.eye = [k as f32 * 0.002, 3.0, k as f32 * 0.001];
            r.render_into(&device, &scene, FrameTarget::full(&view, format, width, height))
                .unwrap_or_else(|err| panic!("bench frame: {err}"));
            if let Some(submission) = r.take_submission() {
                in_flight.push_back(submission);
            }
            if in_flight.len() >= 3
                && let Some(oldest) = in_flight.pop_front()
                && let Err(err) = device.device.poll(wgpu::PollType::Wait {
                    submission_index: Some(oldest),
                    timeout: None,
                })
            {
                panic!("bench poll: {err}");
            }
            if k >= 120 {
                let t = r.gpu_timings();
                map_ms.push(t.shadows_ms);
                fog_ms.push(t.fog_ms);
            }
        }
        let median = |v: &mut Vec<f32>| {
            v.sort_by(f32::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        };
        eprintln!(
            "cloud_shadow_bench {name} {width}x{height} shadows_ms(map) median {:.3} fog_ms median {:.3}",
            median(&mut map_ms),
            median(&mut fog_ms)
        );
    }
}
