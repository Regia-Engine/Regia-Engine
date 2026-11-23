// SPDX-License-Identifier: MPL-2.0
//! Temporal volumetric clouds: one low-resolution texel in 16 marched per frame, the rest
//! reprojected from the previous frame (`clouds.wgsl` `fs_cloud_fresh` /
//! `fs_cloud_reconstruct`).
//!
//! Motion metrics follow `lookdev::motion`: flicker is the mean |ΔL| between consecutive
//! frames (8-bit luma levels), smear the mean |L − L_reference| on the last pose. Every run
//! compares the reprojected clouds against the same path marched in full each frame.

use regia_render::{
    AtmosphereParameters, CloudSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene,
    FrameTarget, GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, TextureViewSettings, TimeOfDay,
};
use regia_test_utils::golden::ssim_rgba8;

const W: u32 = 320;
const H: u32 = 180;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cloud temporal: {err}");
            None
        }
    }
}

fn noon() -> TimeOfDay {
    TimeOfDay {
        hour: 11.0,
        day_of_year: 172,
        latitude_deg: 30.0,
    }
}

/// Desktop High with the cloud pass and the sky only: no screen-space, GI, shadow or
/// temporal-AA passes that would change pixels from frame to frame on their own.
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

/// A vertical slab facing −Z (a ridge the pan crosses, so the history meets depth edges).
fn slab(x: [f32; 2], top: f32, z: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, -1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x[0], 0.0, z], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], 0.0, z], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], top, z], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x[0], top, z], n, [0.0, 1.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn material(color: [f32; 3]) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.base_color = [color[0], color[1], color[2], 1.0];
    mat.roughness = 0.9;
    mat
}

fn lab(width: u32, height: u32) -> LabScene {
    let (gv, gi) = quad([-8000.0, -8000.0], [8000.0, 8000.0], 0.0);
    let (rv, ri) = slab([-900.0, 300.0], 260.0, 1800.0);
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
        materials: vec![material([0.22, 0.24, 0.16]), material([0.35, 0.3, 0.28])],
        meshes: vec![
            LabMesh {
                vertices: gv,
                indices: gi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: rv,
                indices: ri,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn clouds(coverage: f32, wind: [f32; 2], time_s: f32) -> CloudSettings {
    CloudSettings {
        wind_m_per_s: wind,
        time_s,
        ..CloudSettings::coverage_layer(coverage)
    }
}

fn outdoor(settings: CloudSettings) -> OutdoorParams {
    OutdoorParams {
        time: Some(noon()),
        clouds: settings,
        planet: AtmosphereParameters::earth(),
        ..OutdoorParams::default()
    }
}

fn renderer(settings: CloudSettings) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    // Only explicit cuts: a pan must reach the reprojection, not reset it.
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(outdoor(settings));
    renderer
}

/// One camera pose: eye and look target.
#[derive(Clone, Copy)]
struct Pose {
    eye: [f32; 3],
    target: [f32; 3],
}

const HOME: Pose = Pose {
    eye: [0.0, 3.0, 0.0],
    target: [0.0, 600.0, 1000.0],
};

/// `HOME` yawed by `deg` about +Y and moved `forward` metres along its heading.
fn panned(deg: f32, forward: f32) -> Pose {
    let (s, c) = deg.to_radians().sin_cos();
    let look = [1000.0 * s, 600.0, 1000.0 * c];
    let eye = [forward * s, 3.0, forward * c];
    Pose {
        eye,
        target: [eye[0] + look[0], eye[1] + look[1], eye[2] + look[2]],
    }
}

fn frame(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    q: FrameQuality,
    pose: Pose,
) -> Vec<u8> {
    let mut scene = lab.as_frame_scene_with_quality(q);
    scene.eye = pose.eye;
    scene.target = pose.target;
    renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("cloud frame: {err}"))
        .pixels
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

/// Mean |ΔL| over consecutive frames.
fn flicker(frames: &[Vec<f32>]) -> f32 {
    let pairs = frames.windows(2).map(|w| mean_abs_diff(&w[0], &w[1]));
    pairs.sum::<f32>() / (frames.len().saturating_sub(1)).max(1) as f32
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

/// Mean luma of `count` full-march frames at `pose` and time `time_s`: the converged image a
/// temporal method should approach. Also returns the first single frame (one march's noise).
fn converged(
    device: &RenderDevice,
    lab: &LabScene,
    settings: CloudSettings,
    pose: Pose,
    count: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut r = renderer(settings);
    let mut sum = vec![0.0_f32; (W * H) as usize];
    let mut single = Vec::new();
    for k in 0..count {
        let l = luma(&frame(&mut r, device, lab, quality(false), pose));
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

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A held camera: the reprojected clouds flicker far less than a full march with a new start
/// jitter every frame, converge to the full march's mean, and allocate nothing per steady
/// frame.
#[test]
fn held_camera_flickers_less_and_converges_to_the_full_march() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let settings = clouds(0.5, [0.0, 0.0], 0.0);
    let run = |reprojection: bool| -> (Vec<Vec<f32>>, Vec<u8>, regia_render::FramePathCounters) {
        let mut r = renderer(settings);
        let q = quality(reprojection);
        let mut frames = Vec::new();
        let mut last = Vec::new();
        // 16 frames fill the history; then 32 steady frames are scored.
        for _ in 0..16 {
            frame(&mut r, &device, &lab, q, HOME);
        }
        let before = regia_render::frame_path_counters();
        for _ in 0..32 {
            last = frame(&mut r, &device, &lab, q, HOME);
            frames.push(luma(&last));
        }
        (frames, last, regia_render::frame_path_counters().since(before))
    };
    let (on, on_last, created) = run(true);
    let (off, off_last, _) = run(false);
    let (mean, single) = converged(&device, &lab, settings, HOME, 16);
    let clear = {
        let mut r = renderer(clouds(0.0, [0.0, 0.0], 0.0));
        luma(&frame(&mut r, &device, &lab, quality(true), HOME))
    };
    dump("cloud_temporal_held_on", &on_last);
    dump("cloud_temporal_held_off", &off_last);
    let flicker_on = flicker(&on);
    let flicker_off = flicker(&off);
    let cloud_signal = mean_abs_diff(&single, &clear);
    let bias = mean_abs_diff(&on[on.len() - 1], &mean);
    let noise = mean_abs_diff(&single, &mean);
    eprintln!(
        "held: flicker reprojected {flicker_on:.4} full {flicker_off:.4}; vs converged: reprojected {bias:.4} one march {noise:.4}; cloud signal {cloud_signal:.3}; steady creations {created:?}"
    );
    assert!(
        cloud_signal > 2.0,
        "the shot has no clouds to test ({cloud_signal})"
    );
    assert!(
        flicker_on <= 0.25 * flicker_off,
        "reprojected clouds flicker {flicker_on} vs full march {flicker_off}"
    );
    assert!(
        bias <= noise.max(0.1),
        "held reprojection is farther from the converged full march ({bias}) than one march is ({noise})"
    );
    assert_eq!(created.textures, 0, "textures created per steady cloud frame");
    assert_eq!(
        created.bind_groups, 0,
        "bind groups created per steady cloud frame"
    );
    assert_eq!(created.pipelines, 0, "pipelines created per steady cloud frame");
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

/// True when `a` matches `b` best unshifted: the clouds are not displaced (no lag or trail).
fn best_unshifted(a: &[f32], b: &[f32]) -> bool {
    let zero = shifted_diff(a, b, 0, 0);
    [(-1, 0), (1, 0), (0, -1), (0, 1)]
        .iter()
        .all(|&(dx, dy)| shifted_diff(a, b, dx, dy) >= zero)
}

/// A fast pan at 60 fps: 45°/s of yaw while flying forward at 120 m/s, across a ridge, for
/// 48 frames, then 16 held frames.
///
/// Every reprojected texel is resampled at a new sub-texel position each frame until its
/// refresh (at most 15 frames later), so clouds soften while the camera turns: the
/// low-resolution field has edges a texel wide, and interpolating it differs from marching
/// it at the new texel centres. At 320×180 a low-resolution texel is 0.55°, eight times its
/// size at 1440p, so this shot shows the softening at its worst. The checks: the clouds are
/// not displaced (no lag or trail), the moving error is a small part of what the pan
/// changed in the image, and the held frames converge back to the full march.
#[test]
fn panning_does_not_smear_the_clouds() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let settings = clouds(0.5, [0.0, 0.0], 0.0);
    let pose = |k: usize| panned(0.75 * k as f32, 2.0 * k as f32);
    let path = |q: FrameQuality| -> (Vec<u8>, Vec<u8>) {
        let mut r = renderer(settings);
        for _ in 0..16 {
            frame(&mut r, &device, &lab, q, HOME);
        }
        let mut moving = Vec::new();
        for k in 1..=48 {
            moving = frame(&mut r, &device, &lab, q, pose(k));
        }
        let mut held = Vec::new();
        for _ in 0..16 {
            held = frame(&mut r, &device, &lab, q, pose(48));
        }
        (moving, held)
    };
    let (on, on_held) = path(quality(true));
    let (off, _) = path(quality(false));
    let (mean, single) = converged(&device, &lab, settings, pose(48), 16);
    let (start, _) = converged(&device, &lab, settings, HOME, 1);
    dump("cloud_temporal_pan_on", &on);
    dump("cloud_temporal_pan_off", &off);
    dump("cloud_temporal_pan_held", &on_held);
    let noise = mean_abs_diff(&single, &mean);
    let on_l = luma(&on);
    let smear_on = mean_abs_diff(&on_l, &mean);
    let smear_off = mean_abs_diff(&luma(&off), &mean);
    let settled = mean_abs_diff(&luma(&on_held), &mean);
    let change = mean_abs_diff(&start, &mean);
    eprintln!(
        "pan: vs converged reprojected {smear_on:.4} full {smear_off:.4} one march {noise:.4}; pan changed {change:.4}; held 16 frames after {settled:.4}"
    );
    assert!(
        best_unshifted(&on_l, &mean),
        "reprojected clouds lag or lead the camera"
    );
    assert!(
        smear_on <= 0.1 * change,
        "panning smears the clouds: {smear_on} against a pan change of {change}"
    );
    assert!(
        settled <= 1.5 * noise.max(0.1),
        "16 held frames after the pan stay {settled} from the full march (one march {noise})"
    );
}

/// 40 m/s of wind at 60 fps for 96 frames (64 m of drift). The reprojection removes the
/// wind drift: the clouds are not displaced (no trail), the error stays a small part of how
/// far the field moved, and within a few times one full march's noise. The detail noise
/// drifts 1.6 times faster than the base shape and is caught up by the refresh.
#[test]
fn wind_at_40_m_per_s_leaves_no_trail() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let wind = [40.0, 0.0];
    let dt = 1.0 / 60.0;
    let frames = 96;
    let path = |q: FrameQuality| -> Vec<u8> {
        let mut r = renderer(clouds(0.5, wind, 0.0));
        let mut last = Vec::new();
        for k in 0..frames {
            r.set_outdoor(outdoor(clouds(0.5, wind, dt * k as f32)));
            last = frame(&mut r, &device, &lab, q, HOME);
        }
        last
    };
    let on = path(quality(true));
    let off = path(quality(false));
    let end = clouds(0.5, wind, dt * (frames - 1) as f32);
    let (mean, single) = converged(&device, &lab, end, HOME, 16);
    let (start, _) = converged(&device, &lab, clouds(0.5, wind, 0.0), HOME, 1);
    dump("cloud_temporal_wind_on", &on);
    dump("cloud_temporal_wind_off", &off);
    let noise = mean_abs_diff(&single, &mean);
    let on_l = luma(&on);
    let trail_on = mean_abs_diff(&on_l, &mean);
    let trail_off = mean_abs_diff(&luma(&off), &mean);
    let moved = mean_abs_diff(&start, &mean);
    eprintln!(
        "wind: vs converged reprojected {trail_on:.4} full {trail_off:.4} one march {noise:.4}; field moved {moved:.4}"
    );
    assert!(best_unshifted(&on_l, &mean), "the clouds trail the wind");
    assert!(
        trail_on <= 0.2 * moved,
        "40 m/s wind leaves a trail: {trail_on} against a field move of {moved}"
    );
    assert!(
        trail_on <= 3.0 * noise.max(0.1),
        "40 m/s wind: {trail_on} from the converged full march (one march {noise})"
    );
}

/// After a camera cut the first frame marches every texel: it matches a fresh renderer's
/// first frame of the new shot.
#[test]
fn camera_cut_marches_the_whole_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let settings = clouds(0.5, [0.0, 0.0], 0.0);
    let q = quality(true);
    let shot_b = panned(-70.0, 0.0);
    let mut r = renderer(settings);
    for _ in 0..20 {
        frame(&mut r, &device, &lab, q, HOME);
    }
    r.camera_cut();
    let cut = frame(&mut r, &device, &lab, q, shot_b);
    let mut fresh = renderer(settings);
    let reference = frame(&mut fresh, &device, &lab, q, shot_b);
    // Without the cut the same jump reprojects from a history that never saw shot B.
    let mut uncut = renderer(settings);
    for _ in 0..20 {
        frame(&mut uncut, &device, &lab, q, HOME);
    }
    let jumped = frame(&mut uncut, &device, &lab, q, shot_b);
    dump("cloud_temporal_cut", &cut);
    dump("cloud_temporal_cut_reference", &reference);
    let ssim = ssim_rgba8(&cut, &reference, W, H).unwrap_or(0.0);
    let ssim_jump = ssim_rgba8(&jumped, &reference, W, H).unwrap_or(0.0);
    eprintln!("cut: SSIM vs full march {ssim:.4} (uncut jump {ssim_jump:.4})");
    assert!(ssim >= 0.95, "frame after a cut vs a full march: SSIM {ssim}");
}

/// A small texture-view camera above the main one, looking at the same sky.
fn view_scene(lab: &LabScene, q: FrameQuality) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(q);
    scene.width = 64;
    scene.height = 64;
    scene.eye = [0.0, 40.0, -50.0];
    scene.target = [0.0, 500.0, 900.0];
    scene
}

/// Texture views keep clouds off (ADR-184): a view renders the same with and without a
/// cloud layer, and views between main frames leave the main view's cloud history alone.
#[test]
fn texture_views_stay_cloudless_and_leave_the_history() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = lab(W, H);
    let q = quality(true);
    const VIEW: u64 = 7;
    let settings = TextureViewSettings::default();
    let view_pixels = |coverage: f32| -> Vec<u8> {
        let mut r = renderer(clouds(coverage, [0.0, 0.0], 0.0));
        for _ in 0..3 {
            frame(&mut r, &device, &lab, q, HOME);
            r.render_texture_view(&device, VIEW, view_scene(&lab, q), &settings)
                .unwrap_or_else(|err| panic!("texture view: {err}"));
        }
        r.read_texture_target(&device, VIEW)
            .unwrap_or_else(|err| panic!("read texture view: {err}"))
            .pixels
    };
    let cloudy_view = view_pixels(0.5);
    let clear_view = view_pixels(0.0);
    assert_eq!(cloudy_view, clear_view, "a texture view drew clouds");

    let main_last = |views: bool| -> Vec<u8> {
        let mut r = renderer(clouds(0.5, [0.0, 0.0], 0.0));
        let mut last = Vec::new();
        for k in 0..24 {
            if views {
                r.render_texture_view(&device, VIEW, view_scene(&lab, q), &settings)
                    .unwrap_or_else(|err| panic!("texture view: {err}"));
            }
            last = frame(&mut r, &device, &lab, q, panned(0.5 * k as f32, 0.0));
        }
        last
    };
    let with = main_last(true);
    let without = main_last(false);
    let diff = with
        .iter()
        .zip(&without)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    eprintln!("main view with texture views vs without: max channel difference {diff}");
    assert!(
        diff <= 1,
        "texture views changed the main view's clouds (max {diff})"
    );
}

/// Cloud-pass GPU time at 1440p, coverage 0.5, rendered continuously (no readback, three
/// frames in flight, 120 warm-up frames), full march vs reprojected, Desktop High (64 steps)
/// and Ultra (96), each with the cone light march on and off. Run with
/// `--ignored --nocapture` on the GPU to time.
#[test]
#[ignore = "1440p cloud-pass timing burst"]
fn cloud_temporal_cost_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let info = device.adapter.get_info();
    eprintln!(
        "cloud temporal bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let (width, height) = (2560, 1440);
    let lab = lab(width, height);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cloud bench target"),
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
    for (name, reprojection, percent, cone) in [
        ("high_full_cone", false, 100, true),
        ("high_full_nocone", false, 100, false),
        ("high_reprojected_cone", true, 100, true),
        ("high_reprojected_nocone", true, 100, false),
        ("ultra_full_cone", false, 150, true),
        ("ultra_reprojected_cone", true, 150, true),
        ("ultra_reprojected_nocone", true, 150, false),
    ] {
        let mut q = quality(reprojection);
        q.cloud_step_percent = percent;
        q.cloud_cone_light = cone;
        let mut r = renderer(clouds(0.5, [6.0, 2.0], 0.0));
        r.set_gpu_timing(&device, true);
        let mut in_flight = std::collections::VecDeque::new();
        let mut samples = Vec::new();
        for k in 0..frames {
            // The eye creeps and the wind blows, so nothing is held.
            r.set_outdoor(outdoor(clouds(0.5, [6.0, 2.0], k as f32 / 60.0)));
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
                samples.push(r.gpu_timings().clouds_ms);
            }
        }
        samples.sort_by(f32::total_cmp);
        let median = samples.get(samples.len() / 2).copied().unwrap_or(0.0);
        let p90 = samples.get(samples.len() * 9 / 10).copied().unwrap_or(0.0);
        eprintln!("cloud_temporal_bench {name} {width}x{height} clouds_ms median {median:.3} p90 {p90:.3}");
    }
}
