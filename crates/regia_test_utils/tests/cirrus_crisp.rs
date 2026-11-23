// SPDX-License-Identifier: MPL-2.0
//! Crisp cirrus (ADR-1516) on lavapipe, no golden: Cinematic frames from fresh renderers repeat
//! byte for byte, so each sheet's frame hash is a fixed id. The first test prints the hashes of
//! the default sheets (the lane compared them between the builds before and after the change);
//! the second checks that a crisp sheet changes only sky pixels, keeps the sky's mean, and
//! sharpens the streaks.
//!
//! The view is a telephoto look up at the sheet (12° field, 14° to 26° elevation) along the
//! streaks (+X, the streak direction with no wind), so one pixel spans about 30 m of the sheet
//! across the trails and the strands are resolved. A wall 100 m away covers the bottom rows.

use regia_render::{
    AtmosphereParameters, CirrusSheet, CloudGenus, CloudMarch, CloudSettings, DirectionalLightUniform,
    FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, TimeOfDay,
};

const W: u32 = 320;
const H: u32 = 180;
/// Rows from here down show the wall (its top is about 2.5 rows higher).
const GROUND_ROW: u32 = 168;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cirrus crisp: {err}");
            None
        }
    }
}

/// The Cinematic frame quality with the passes that are not about clouds turned off, as in
/// `cinematic_clouds.rs`, so every sky pixel change comes from the cloud pass.
fn cloud_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Cinematic));
    q.gpu_driven = false;
    q.ssao = SsaoMode::Off;
    q.ssr = false;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.gi = GiMode::Ambient;
    q.contact_shadows = false;
    q.volumetric_fog = false;
    q.taau = false;
    q.csm_cascade_count = 1;
    q.csm_resolution = 256;
    q.local_shadow_atlas_size = 0;
    q.reflection_probes = false;
    q.parallax_occlusion = false;
    q.screen_space_refraction = false;
    q.near_sun_tile = 0;
    q.translucent_shadow_cascades = 0;
    q.translucent_local_shadows = false;
    q.depth_velocity = false;
    q.reflection_occlusion = false;
    q.bent_normals = false;
    q.paged_sun_shadows = false;
    q.cloud_reprojection = false;
    q.cloud_march = CloudMarch::CINEMATIC;
    q.cloud_step_percent = 300;
    q
}

/// A telephoto view up at the sheet along +X, and a wall in front of the bottom rows.
fn lab() -> LabScene {
    lab_sized(W, H)
}

/// [`lab`] at an arbitrary resolution (the cost bench renders at 1440p).
fn lab_sized(width: u32, height: u32) -> LabScene {
    let n = [-1.0, 0.0, 0.0];
    // 28.8 m tall at 100 m: its top is 15° above the eye, the frame's bottom edge 14°.
    let (x, top) = (100.0f32, 2.0 + 100.0 * 15.0_f32.to_radians().tan());
    let mut material = LabMaterial::EMPTY;
    material.base_color = [0.22, 0.24, 0.16, 1.0];
    material.roughness = 0.9;
    LabScene {
        width,
        height,
        eye: [0.0, 2.0, 0.0],
        // 20° above the horizon at the centre, so the frame spans 14° to 26°.
        target: [10_000.0, 2.0 + 10_000.0 * 20.0_f32.to_radians().tan(), 0.0],
        fov_y: 12.0_f32.to_radians(),
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
        directional: DirectionalLightUniform::new([0.3, -0.5, 0.8], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([x, 0.0, -300.0], n, [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([x, 0.0, 300.0], n, [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x, top, 300.0], n, [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([x, top, -300.0], n, [0.0, 1.0]),
            ],
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// The cirrus genus with no wind and the given sheet.
fn outdoor(sheet: Option<CirrusSheet>) -> OutdoorParams {
    let mut clouds = CloudSettings::default().with_genus(CloudGenus::Cirrus);
    clouds.wind_m_per_s = [0.0, 0.0];
    clouds.time_s = 0.0;
    if let Some(sheet) = sheet {
        clouds.look.cirrus = sheet;
    }
    OutdoorParams {
        time: Some(TimeOfDay {
            hour: 9.5,
            day_of_year: 80,
            latitude_deg: 52.0,
        }),
        clouds,
        planet: AtmosphereParameters::earth(),
        ..OutdoorParams::default()
    }
}

/// The third frame of a fresh renderer (RGBA8).
fn frame(device: &RenderDevice, sheet: Option<CirrusSheet>) -> Vec<u8> {
    let lab = lab();
    let q = cloud_quality();
    let mut r = FrameRenderer::new();
    r.set_ddgi_enabled(Some(false));
    r.set_camera_cut_detection(None);
    r.set_outdoor(outdoor(sheet));
    let mut last = Vec::new();
    for _ in 0..3 {
        last = r
            .render_offscreen(device, &lab.as_frame_scene_with_quality(q))
            .unwrap_or_else(|err| panic!("cirrus frame: {err}"))
            .pixels;
    }
    last
}

fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

/// FNV-1a over the frame bytes.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
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

/// Mean absolute Laplacian of the luma over the sky rows: how much fine structure the sky shows.
fn sky_detail(l: &[f32]) -> f32 {
    let w = W as usize;
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in 1..(GROUND_ROW as usize - 2) {
        for x in 1..w - 1 {
            let i = y * w + x;
            sum += (4.0 * l[i] - l[i - 1] - l[i + 1] - l[i - w] - l[i + w]).abs();
            n += 1.0;
        }
    }
    sum / n
}

fn sky_mean(l: &[f32]) -> f32 {
    let rows = (GROUND_ROW as usize - 2) * W as usize;
    l[..rows].iter().sum::<f32>() / rows as f32
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The sheets the base look and the fibrous look round draw repeat byte for byte from fresh
/// renderers; their hashes are printed (ADR-1516: identical in the builds before and after
/// crisp cirrus, which leaves every non-crisp path unchanged).
#[test]
fn default_cirrus_frames_repeat_and_print_their_hashes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let genus = CloudGenus::Cirrus.layer().look.cirrus;
    for (name, sheet) in [
        ("cirrus_genus", None),
        ("no_cirrus", Some(CirrusSheet::NONE)),
        ("cirrus_fibrous", Some(genus.with_fibrous_look())),
    ] {
        let a = frame(&device, sheet);
        let b = frame(&device, sheet);
        eprintln!("cirrus frame hash {name}: {:016x}", fnv(&a));
        dump(name, &a);
        assert!(a == b, "{name}: two fresh renderers differ");
    }
}

/// A crisp sheet changes only sky pixels, keeps the sky's mean brightness within two levels,
/// and shows more fine structure than the same fibrous sheet.
#[test]
fn crisp_changes_only_the_sky_and_sharpens_the_streaks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let fibrous = CirrusSheet {
        coverage: 0.9,
        optical_depth: 0.2,
        scale_m: 1_500.0,
        ..CloudGenus::Cirrus.layer().look.cirrus.with_fibrous_look()
    };
    let base = frame(&device, Some(fibrous));
    let crisp = frame(&device, Some(fibrous.with_crisp(1.0)));
    dump("fibrous", &base);
    dump("crisp", &crisp);
    assert!(base != crisp, "crisp draws something different");
    let (lb, lc) = (luma(&base), luma(&crisp));
    // The wall is untouched (the sheet composites over sky texels only).
    let ground = (GROUND_ROW * W * 4) as usize;
    assert!(base[ground..] == crisp[ground..], "crisp changed wall pixels");
    let (mb, mc) = (sky_mean(&lb), sky_mean(&lc));
    let (db, dc) = (sky_detail(&lb), sky_detail(&lc));
    eprintln!("sky mean {mb:.2} -> {mc:.2}, sky detail {db:.3} -> {dc:.3}");
    assert!((mc - mb).abs() < 2.0, "sky mean moved {mb} -> {mc}");
    assert!(dc > 1.3 * db, "sharper streaks: detail {db} -> {dc}");
}

/// The band/warp cross-fade keeps small crisp amounts continuous: a 0.01 crisp frame is far
/// closer to the fibrous frame than a full-crisp frame (with the old hard switch any non-zero
/// amount jumped straight to the B-spline reads).
#[test]
fn small_crisp_blends_continuously() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let fibrous = CirrusSheet {
        coverage: 0.9,
        optical_depth: 0.2,
        scale_m: 1_500.0,
        ..CloudGenus::Cirrus.layer().look.cirrus.with_fibrous_look()
    };
    let base = frame(&device, Some(fibrous));
    let tiny = frame(&device, Some(fibrous.with_crisp(0.01)));
    let full = frame(&device, Some(fibrous.with_crisp(1.0)));
    /// Mean absolute luma difference over the sky rows.
    fn dist(a: &[u8], b: &[u8]) -> f32 {
        let (la, lb) = (luma(a), luma(b));
        let rows = (GROUND_ROW - 2) as usize * W as usize;
        la[..rows]
            .iter()
            .zip(&lb[..rows])
            .map(|(x, y)| (x - y).abs())
            .sum::<f32>()
            / rows as f32
    }
    let d_small = dist(&base, &tiny);
    let d_full = dist(&base, &full);
    eprintln!("sky mean abs diff: crisp 0.01 {d_small:.3}, crisp 1 {d_full:.3}");
    // Calibrated on lavapipe: 0.72 vs 2.16 code values. The blend keeps a small crisp close
    // (a hard switch jumped straight to the full B-spline distance); the bounded remainder is
    // the supersampling switch, which steps on above 0 by design.
    assert!(
        d_small < 1.0,
        "a 0.01 crisp changes the sky only slightly: {d_small} mean code values"
    );
    assert!(
        d_small < 0.5 * d_full,
        "small crisp must be much closer than full crisp: {d_small} vs {d_full}"
    );
}

/// The cloud pass's GPU cost of a crisp sheet against the same fibrous sheet, at 1440p on the
/// s1v t11 look (coverage 0.65, optical depth 0.11, tile 1500 m, the ADR-1509 fibrous values).
/// Run on a GPU with `--ignored --nocapture`; `CRISP_BENCH_FRAMES` sets the timed frames
/// (default 30) and `CRISP_BENCH_WARMUP` the warm-up (default 15).
#[test]
#[ignore = "GPU timing burst at 1440p"]
fn crisp_cost_at_1440p() {
    use regia_render::{FrameTarget, GpuTimings};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let info = device.adapter.get_info();
    eprintln!(
        "crisp bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let env_count = |key: &str, default: usize| -> usize {
        regia_core::brand::env_var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let frames = env_count("CRISP_BENCH_FRAMES", 30).max(1);
    let warmup = env_count("CRISP_BENCH_WARMUP", 15);
    let (width, height) = (2560_u32, 1440_u32);
    let lab = lab_sized(width, height);
    let q = cloud_quality();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("crisp bench target"),
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
    let fibrous = CirrusSheet {
        coverage: 0.65,
        optical_depth: 0.11,
        scale_m: 1_500.0,
        ..CloudGenus::Cirrus.layer().look.cirrus.with_fibrous_look()
    };
    for (name, sheet) in [("crisp0", fibrous), ("crisp1", fibrous.with_crisp(1.0))] {
        let mut r = FrameRenderer::new();
        r.set_ddgi_enabled(Some(false));
        r.set_camera_cut_detection(None);
        r.set_gpu_timing(&device, true);
        let mut samples: Vec<GpuTimings> = Vec::new();
        for k in 0..frames + warmup {
            let mut params = outdoor(Some(sheet));
            params.clouds.time_s = k as f32 / 60.0;
            r.set_outdoor(params);
            let mut scene = lab.as_frame_scene_with_quality(q);
            scene.eye = [k as f32 * 0.002, 2.0, k as f32 * 0.001];
            r.render_into(&device, &scene, FrameTarget::full(&view, format, width, height))
                .unwrap_or_else(|err| panic!("crisp bench frame: {err}"));
            let Some(submission) = r.take_submission() else {
                continue;
            };
            if let Err(err) = device.device.poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            }) {
                panic!("crisp bench poll: {err}");
            }
            if k >= warmup {
                samples.push(r.gpu_timings());
            }
        }
        let median = |pick: &dyn Fn(&GpuTimings) -> f32| -> f32 {
            let mut v: Vec<f32> = samples.iter().map(pick).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        if samples.is_empty() {
            eprintln!("crisp_bench {name}: no timed frames");
            continue;
        }
        let passes: Vec<String> = GpuTimings::default()
            .passes()
            .iter()
            .enumerate()
            .map(|(i, (pass, _))| format!("{pass} {:.3}", median(&|t: &GpuTimings| t.passes()[i].1)))
            .collect();
        eprintln!(
            "crisp_bench {name} {width}x{height} frame_ms median {:.3} | {}",
            median(&|t: &GpuTimings| t.frame_ms),
            passes.join(", ")
        );
    }
}
