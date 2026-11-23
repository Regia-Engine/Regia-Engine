// SPDX-License-Identifier: MPL-2.0
//! Weather-driven volumetric clouds on the GPU (ADR-429): a [`Weather`] resource steps the
//! cloud layer frame by frame, the way `drive_cloud_weather` does in a game.
//!
//! The motion metrics follow `regia_test_utils/tests/cloud_temporal.rs`: luma in 8-bit levels,
//! compared against the same layer marched in full every frame and averaged (the converged
//! image). The work budget is counted (`cloud_work_counters`, `frame_path_counters`), never timed;
//! the ignored bench at the end times it on a real GPU.

use regia_render::{
    AtmosphereParameters, CloudSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget,
    GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SsaoMode, TimeOfDay, cloud_steady_work, cloud_work_counters,
    frame_path_counters,
};
use regia_test_utils::golden::ssim_rgba8;
use regia_vfx::{Weather, WeatherState};

const W: u32 = 320;
const H: u32 = 180;
const DT: f32 = 1.0 / 60.0;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cloud weather: {err}");
            None
        }
    }
}

/// Desktop High with the cloud pass and the sky only (as `cloud_temporal.rs`): nothing else
/// changes pixels from frame to frame.
fn quality(reprojection: bool) -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.gi = GiMode::Ambient;
    quality.contact_shadows = false;
    quality.volumetric_fog = false;
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
    quality.cloud_reprojection = reprojection;
    quality
}

fn quad(min: [f32; 2], max: [f32; 2], y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([min[0], y, min[1]], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([max[0], y, min[1]], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([max[0], y, max[1]], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([min[0], y, max[1]], n, [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

fn material(color: [f32; 3]) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.base_color = [color[0], color[1], color[2], 1.0];
    mat.roughness = 0.9;
    mat
}

/// Ground to the horizon, the camera low and looking up into the layer.
fn lab(width: u32, height: u32) -> LabScene {
    let (gv, gi) = quad([-8000.0, -8000.0], [8000.0, 8000.0], 0.0);
    LabScene {
        width,
        height,
        eye: [0.0, 3.0, 0.0],
        target: [0.0, 600.0, 1000.0],
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
        materials: vec![material([0.22, 0.24, 0.16])],
        meshes: vec![LabMesh {
            vertices: gv,
            indices: gi,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn outdoor(clouds: CloudSettings) -> OutdoorParams {
    OutdoorParams {
        time: Some(TimeOfDay {
            hour: 11.0,
            day_of_year: 172,
            latitude_deg: 30.0,
        }),
        clouds,
        planet: AtmosphereParameters::earth(),
        ..OutdoorParams::default()
    }
}

fn renderer(clouds: CloudSettings) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(outdoor(clouds));
    renderer
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, q: FrameQuality) -> Vec<u8> {
    let scene = lab.as_frame_scene_with_quality(q);
    renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("cloud weather frame: {err}"))
        .pixels
}

/// Steps `weather` one frame, writes it into `clouds` and renders.
fn weather_frame(
    r: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    q: FrameQuality,
    weather: &mut Weather,
    clouds: &mut CloudSettings,
) -> Vec<u8> {
    weather.step(DT);
    weather.apply(clouds);
    r.set_outdoor(outdoor(*clouds));
    frame(r, device, lab, q)
}

fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

fn mean_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len().max(1) as f32
}

/// Mean |a − b| with `b` shifted by `(dx, dy)` pixels, over the frame minus a 6-pixel border.
fn shifted_diff(a: &[f32], b: &[f32], dx: i32, dy: i32) -> f32 {
    let (w, h) = (W as i32, H as i32);
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in 6..h - 6 {
        for x in 6..w - 6 {
            let i = (y * w + x) as usize;
            let j = ((y + dy) * w + (x + dx)) as usize;
            sum += (a[i] - b[j]).abs();
            n += 1.0;
        }
    }
    sum / n
}

/// True when `a` matches `b` best unshifted: the clouds are not displaced.
fn best_unshifted(a: &[f32], b: &[f32]) -> bool {
    let zero = shifted_diff(a, b, 0, 0);
    [(-1, 0), (1, 0), (0, -1), (0, 1)]
        .iter()
        .all(|&(dx, dy)| shifted_diff(a, b, dx, dy) >= zero)
}

/// Mean luma of `count` full-march frames of `clouds` and the first of them (one march's noise).
fn converged(
    device: &RenderDevice,
    lab: &LabScene,
    clouds: CloudSettings,
    count: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut r = renderer(clouds);
    let mut sum = vec![0.0_f32; (W * H) as usize];
    let mut single = Vec::new();
    for k in 0..count {
        let l = luma(&frame(&mut r, device, lab, quality(false)));
        for (acc, v) in sum.iter_mut().zip(&l) {
            *acc += v;
        }
        if k == 0 {
            single = l;
        }
    }
    let n = count.max(1) as f32;
    (sum.into_iter().map(|v| v / n).collect(), single)
}

fn layer(state: WeatherState) -> WeatherState {
    WeatherState {
        precipitation: 0.0,
        ..state
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The wind turns from 40 m/s north to 40 m/s east (over half a second) ten minutes into the
/// weather clock, then blows across the view for two seconds. With the drift integrated the clouds carry on from where they are: no frame
/// marches the whole sky again (the history stays valid), the clouds are not displaced against
/// the full march, and the error is a small part of how far the field moved. Before ADR-429 the
/// same change moved the drift by `Δwind × time` (24 km) in one frame.
#[test]
fn wind_change_keeps_the_clouds_in_place() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let start = CloudSettings {
        wind_m_per_s: [0.0, 40.0],
        time_s: 600.0,
        ..CloudSettings::coverage_layer(0.5)
    };
    let north = WeatherState::from_clouds(&start);
    let east = WeatherState {
        wind_m_per_s: [40.0, 0.0],
        ..north
    };
    let mut weather = Weather::from_clouds(&start);
    let mut clouds = start;
    let mut r = renderer(clouds);
    let q = quality(true);
    for _ in 0..16 {
        weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let before_turn = clouds;
    let counted = cloud_work_counters();
    let mut last = Vec::new();
    for k in 0..128 {
        if k == 8 {
            weather.transition_to(east, 0.5);
        }
        last = weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let work = cloud_work_counters().since(counted);
    let (mean, single) = converged(&device, &lab, clouds, 16);
    let (origin, _) = converged(&device, &lab, before_turn, 1);
    let noise = mean_abs_diff(&single, &mean);
    let on = luma(&last);
    let trail = mean_abs_diff(&on, &mean);
    let moved = mean_abs_diff(&origin, &mean);
    eprintln!(
        "wind turn: vs converged {trail:.4}, one march {noise:.4}, field moved {moved:.4}; work {work:?}"
    );
    assert_eq!(work.weather_cuts, 0, "a wind change is not a weather cut");
    assert_eq!(
        work.view_full_marches, 0,
        "the wind change kept the cloud history"
    );
    assert_eq!(work.view_reprojected, 128);
    assert!(
        best_unshifted(&on, &mean),
        "the clouds are displaced after the wind turned"
    );
    assert!(
        trail <= 0.2 * moved,
        "the wind turn leaves a trail: {trail} against a field move of {moved}"
    );
    assert!(
        trail <= 3.0 * noise.max(0.1),
        "the wind turn: {trail} from the converged full march (one march {noise})"
    );
}

/// Switching at once from fair weather to a storm is a weather cut: that frame marches every
/// texel and matches a fresh renderer's first frame of the storm, like a camera cut.
#[test]
fn weather_cut_marches_the_whole_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let q = quality(true);
    let fair = layer(WeatherState {
        coverage: 0.3,
        wind_m_per_s: [0.0, 0.0],
        ..WeatherState::FAIR
    });
    let storm = layer(WeatherState {
        coverage: 0.8,
        cloud_type: 0.8,
        wind_m_per_s: [0.0, 0.0],
        ..WeatherState::STORM
    });
    let mut weather = Weather::new(fair);
    let mut clouds = CloudSettings::default();
    weather.apply(&mut clouds);
    let mut r = renderer(clouds);
    let mut before = Vec::new();
    for _ in 0..20 {
        before = weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let counted = cloud_work_counters();
    weather.set_now(storm);
    let cut = weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    let work = cloud_work_counters().since(counted);
    let mut fresh = renderer(clouds);
    let reference = frame(&mut fresh, &device, &lab, q);
    let ssim = ssim_rgba8(&cut, &reference, W, H).unwrap_or(0.0);
    let ssim_before = ssim_rgba8(&before, &reference, W, H).unwrap_or(0.0);
    eprintln!(
        "weather cut: SSIM vs a fresh full march {ssim:.4} (the fair frame before {ssim_before:.4}); work {work:?}"
    );
    assert_eq!(work.weather_cuts, 1);
    assert_eq!(work.view_full_marches, 1, "the cut frame marches every texel");
    assert!(
        ssim >= 0.95,
        "frame after a weather cut vs a fresh full march: SSIM {ssim}"
    );
    assert!(
        ssim > ssim_before,
        "the storm changed the sky ({ssim_before} before)"
    );
}

/// A one-second transition from 25 % to 60 % coverage, then 16 held frames. No frame is a
/// weather cut; the frame half way is within a part of the transition's own change of the full
/// march at that frame's weather, and the held frames converge to the full march.
#[test]
fn coverage_transition_tracks_the_full_march() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let q = quality(true);
    let from = layer(WeatherState {
        coverage: 0.25,
        wind_m_per_s: [0.0, 0.0],
        ..WeatherState::FAIR
    });
    let to = WeatherState {
        coverage: 0.6,
        ..from
    };
    let mut weather = Weather::new(from);
    let mut clouds = CloudSettings::default();
    weather.apply(&mut clouds);
    let mut r = renderer(clouds);
    for _ in 0..16 {
        weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let start = clouds;
    let counted = cloud_work_counters();
    weather.transition_to(to, 1.0);
    let mut mid = (Vec::new(), clouds);
    for k in 0..60 {
        let pixels = weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
        if k == 29 {
            mid = (pixels, clouds);
        }
    }
    let mut held = Vec::new();
    for _ in 0..16 {
        held = weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let work = cloud_work_counters().since(counted);
    let (end_mean, end_single) = converged(&device, &lab, clouds, 16);
    let (mid_mean, _) = converged(&device, &lab, mid.1, 8);
    let (start_mean, _) = converged(&device, &lab, start, 8);
    let noise = mean_abs_diff(&end_single, &end_mean);
    let change = mean_abs_diff(&start_mean, &end_mean);
    let lag = mean_abs_diff(&luma(&mid.0), &mid_mean);
    let settled = mean_abs_diff(&luma(&held), &end_mean);
    eprintln!(
        "coverage ramp: half way {lag:.4} from its full march, transition change {change:.4}; held {settled:.4} (one march {noise:.4}); work {work:?}"
    );
    assert_eq!(clouds.coverage, 0.6, "the transition ends exactly on the target");
    assert_eq!(work.weather_cuts, 0);
    assert_eq!(work.view_full_marches, 0, "a transition keeps the history");
    assert!(change > 1.0, "the transition changes the sky ({change})");
    assert!(
        lag <= 0.5 * change,
        "half way through, the reprojected clouds lag the weather: {lag} against a change of {change}"
    );
    assert!(
        settled <= 1.5 * noise.max(0.1),
        "16 held frames after the transition stay {settled} from the full march (one march {noise})"
    );
}

/// The per-frame work budget (ADR-429), counted: a two-second fair-to-overcast transition with
/// the cloud shadow map and fog coupling on, wind turning, camera held. Every frame marches
/// exactly the steady-state texels (`cloud_steady_work`), no frame marches the sky or the map
/// whole, the weather map is re-packed into its existing texture (no texture, bind group or
/// pipeline created, no new noise field) and at most once per 1/256 step.
#[test]
fn weather_transition_stays_inside_the_work_budget() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let q = quality(true);
    let fair = layer(WeatherState::FAIR);
    let overcast = layer(WeatherState::OVERCAST);
    let mut weather = Weather::new(fair);
    let mut clouds = CloudSettings {
        shadow_map: true,
        fog_coupling: true,
        ..CloudSettings::default()
    };
    weather.apply(&mut clouds);
    let mut r = renderer(clouds);
    for _ in 0..20 {
        weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let frames = 120u64;
    let counted = cloud_work_counters();
    let created = frame_path_counters();
    weather.transition_to(overcast, 2.0);
    for _ in 0..frames {
        weather_frame(&mut r, &device, &lab, q, &mut weather, &mut clouds);
    }
    let work = cloud_work_counters().since(counted);
    let made = frame_path_counters().since(created);
    let steady = cloud_steady_work(&clouds, W, H, clouds.steps, true);
    eprintln!(
        "weather budget: {frames} frames, work {work:?}, steady per frame {steady:?}, created {made:?}"
    );
    assert!(!weather.transitioning());
    assert_eq!(work.weather_cuts, 0);
    assert_eq!(work.view_full_marches, 0);
    assert_eq!(work.view_reprojected, frames);
    assert_eq!(work.view_texels, frames * steady.view_texels);
    assert_eq!(work.map_whole, 0, "a transition never marches the map whole");
    assert_eq!(
        work.map_texels,
        frames * steady.map_texels,
        "one refresh band a frame"
    );
    assert_eq!(work.weather_fields, 0, "the noise field is built once");
    assert_eq!(work.weather_textures, 0, "the weather map reuses its texture");
    let steps = ((overcast.coverage - fair.coverage) / regia_vfx::weather::WEATHER_VALUE_STEP).ceil() as u64
        + ((fair.cloud_type - overcast.cloud_type) / regia_vfx::weather::WEATHER_VALUE_STEP).ceil() as u64
        + 1;
    assert!(
        work.weather_uploads <= steps,
        "{} weather uploads for {steps} steps",
        work.weather_uploads
    );
    assert!(work.weather_uploads > 0, "the transition reached the weather map");
    assert_eq!(made.textures, 0, "textures created during the transition");
    assert_eq!(made.bind_groups, 0, "bind groups created during the transition");
    assert_eq!(made.pipelines, 0, "pipelines created during the transition");
}

/// Times a weather transition at 1440p on Desktop High with the cloud shadow map and fog
/// coupling on: `clouds_ms` (march, reconstruct, composite) and `shadows_ms` (the map; the scene
/// has no cascades) per frame while the weather changes and while it is held, against the
/// ADR-429 budget of 0.7 ms for the clouds pass (the clouds-temporal target). Prints only; run it
/// on a real GPU (`--ignored --nocapture`).
#[test]
#[ignore = "GPU bench: run on a real GPU with --ignored --nocapture"]
fn cloud_weather_cost_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let info = device.adapter.get_info();
    eprintln!(
        "cloud weather bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let (width, height) = (2560, 1440);
    let lab = lab(width, height);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cloud weather bench target"),
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
        .unwrap_or(480);
    let q = quality(true);
    for (name, transition_s) in [("held", 0.0_f32), ("transition", 4.0)] {
        let mut weather = Weather::new(layer(WeatherState::FAIR));
        let mut clouds = CloudSettings {
            shadow_map: true,
            fog_coupling: true,
            ..CloudSettings::default()
        };
        weather.apply(&mut clouds);
        let mut r = renderer(clouds);
        r.set_gpu_timing(&device, true);
        let mut in_flight = std::collections::VecDeque::new();
        let mut clouds_ms = Vec::new();
        let mut map_ms = Vec::new();
        for k in 0..frames {
            if k == 120 && transition_s > 0.0 {
                weather.transition_to(layer(WeatherState::OVERCAST), transition_s);
            }
            weather.step(DT);
            weather.apply(&mut clouds);
            r.set_outdoor(outdoor(clouds));
            let scene = lab.as_frame_scene_with_quality(q);
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
                let timings = r.gpu_timings();
                clouds_ms.push(timings.clouds_ms);
                map_ms.push(timings.shadows_ms);
            }
        }
        let stat = |mut v: Vec<f32>| {
            v.sort_by(f32::total_cmp);
            (
                v.get(v.len() / 2).copied().unwrap_or(0.0),
                v.get(v.len() * 9 / 10).copied().unwrap_or(0.0),
            )
        };
        let (clouds_median, clouds_p90) = stat(clouds_ms);
        let (map_median, map_p90) = stat(map_ms);
        let verdict = if clouds_median <= 0.7 { "within" } else { "OVER" };
        eprintln!(
            "cloud_weather_bench {name} {width}x{height} clouds_ms median {clouds_median:.3} p90 {clouds_p90:.3} \
             map_ms median {map_median:.3} p90 {map_p90:.3}; {verdict} the 0.7 ms clouds budget"
        );
    }
}
