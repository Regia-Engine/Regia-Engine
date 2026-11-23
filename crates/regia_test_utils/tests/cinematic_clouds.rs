// SPDX-License-Identifier: MPL-2.0
//! The Cinematic profile's clouds (ADR-1403): every texel marched every frame at full resolution
//! with stratified samples (`clouds.wgsl` `march_texel`), so one frame is clean on its own and
//! two runs of the same frames are identical.
//!
//! The lavapipe tests need no golden: one checks two runs against each other, the other checks
//! a single frame against the mean of several frames of the same march (its converged image).
//! The ignored bench times every pass at 1440p and 4K and, with `REGIA_DUMP_DIR`, writes the
//! frames and a cloud-edge crop for the lane's frame sheets.

use regia_render::{
    AtmosphereParameters, CloudMarch, CloudSettings, DirectionalLightUniform, FrameQuality, FrameRenderer,
    FrameTarget, GiMode, GpuTimings, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, TimeOfDay,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cinematic clouds: {err}");
            None
        }
    }
}

/// The Cinematic frame quality with the passes that are not about clouds turned off (no
/// screen-space, GI, shadow or temporal passes), so lavapipe frames stay quick and every pixel
/// change comes from the cloud march.
fn cloud_quality(march: CloudMarch, step_percent: u32) -> FrameQuality {
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
    q.cloud_march = march;
    q.cloud_step_percent = step_percent;
    q
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

/// A ridge in front of the cloud base, so the frame has a cloud-over-ground edge too.
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

/// Ground, a ridge, and a low view up at a broken cloud layer.
fn lab(width: u32, height: u32) -> LabScene {
    let (gv, gi) = quad([-8000.0, -8000.0], [8000.0, 8000.0], 0.0);
    let (rv, ri) = slab([-900.0, 300.0], 260.0, 1800.0);
    LabScene {
        width,
        height,
        eye: [0.0, 3.0, 0.0],
        target: [0.0, 500.0, 1000.0],
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
        directional: DirectionalLightUniform::new([0.3, -0.5, 0.8], [1.0; 3], 100_000.0),
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

/// A broken layer (coverage 0.45) with no wind, so frames differ only by the march's jitter.
/// `time_s` moves nothing (no wind) but makes each frame's outdoor inputs new, so a held camera
/// is not frozen by the static-screen reuse.
fn outdoor(time_s: f32) -> OutdoorParams {
    OutdoorParams {
        time: Some(TimeOfDay {
            hour: 9.5,
            day_of_year: 80,
            latitude_deg: 52.0,
        }),
        clouds: CloudSettings {
            wind_m_per_s: [0.0, 0.0],
            time_s,
            ..CloudSettings::coverage_layer(0.45)
        },
        planet: AtmosphereParameters::earth(),
        ..OutdoorParams::default()
    }
}

fn renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_camera_cut_detection(None);
    renderer.set_outdoor(outdoor(0.0));
    renderer
}

fn render(r: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, q: FrameQuality) -> Vec<u8> {
    r.render_offscreen(device, &lab.as_frame_scene_with_quality(q))
        .unwrap_or_else(|err| panic!("cinematic cloud frame: {err}"))
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

fn dump(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    if let Err(err) = image::save_buffer(&path, pixels, width, height, image::ColorType::Rgba8) {
        eprintln!("dump {name}: {err}");
    }
}

/// The middle value (0 for none).
fn median(mut values: Vec<f32>) -> f32 {
    values.sort_by(f32::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// FNV-1a over the frame bytes (a short id for the log; the test compares the bytes).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Two fresh renderers draw the same three Cinematic frames byte for byte: the march depends on
/// the frame index only, not on history or timing.
#[test]
fn cinematic_clouds_repeat_bit_for_bit() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let (w, h) = (160, 90);
    let lab = lab(w, h);
    let q = cloud_quality(CloudMarch::CINEMATIC, 300);
    let run = || {
        let mut r = renderer();
        (0..3)
            .map(|_| render(&mut r, &device, &lab, q))
            .collect::<Vec<_>>()
    };
    let first = run();
    let second = run();
    for (k, (a, b)) in first.iter().zip(&second).enumerate() {
        eprintln!("cinematic frame {k}: {:016x} / {:016x}", fnv(a), fnv(b));
        assert!(a == b, "frame {k} differs between two runs");
    }
    // The frames are not blank: the layer covers part of the sky.
    let l = luma(&first[2]);
    let (lo, hi) = l
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    assert!(hi - lo > 20.0, "a cloud frame has contrast: {lo}..{hi}");
}

/// One Cinematic frame lies much closer to its converged image (the mean of eight frames, each
/// with a new start jitter) than one single-sample frame of the same march lies to its own:
/// the start-jitter dither in cloud edges and thin gaps is averaged inside the frame.
#[test]
fn stratified_samples_cut_the_single_frame_dither() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let (w, h) = (192, 108);
    let lab = lab(w, h);
    let noise = |samples: u32| -> f32 {
        let march = CloudMarch {
            samples,
            ..CloudMarch::CINEMATIC
        };
        let q = cloud_quality(march, 300);
        let mut r = renderer();
        let frames: Vec<Vec<f32>> = (0..8).map(|_| luma(&render(&mut r, &device, &lab, q))).collect();
        let mut mean = vec![0.0_f32; frames[0].len()];
        for frame in &frames {
            for (m, v) in mean.iter_mut().zip(frame) {
                *m += v / frames.len() as f32;
            }
        }
        mean_abs_diff(&frames[0], &mean)
    };
    let one = noise(1);
    let four = noise(4);
    eprintln!("single-frame dither (mean |ΔL| to the 8-frame mean): 1 sample {one:.3}, 4 samples {four:.3}");
    assert!(one > 0.05, "one sample per texel dithers: {one}");
    assert!(four < 0.6 * one, "four strata cut the dither: {four} vs {one}");
}

/// Every pass's GPU time at 1440p and 4K: Desktop Ultra's clouds (reprojected, 96 steps, half
/// resolution) against the Cinematic profile, the whole frame at each profile's quality. The eye
/// creeps every frame, as in `cloud_temporal_cost_at_1440p`, so no frame is a held view (a held
/// view under TAA shows its converged output and skips the passes). With `REGIA_DUMP_DIR` it
/// writes, per profile at 4K, the plain look (TAA off) after 32 frames at the home pose and a
/// 960×540 crop of the cloud base. Run on the GPU with `--ignored --nocapture`.
/// `CINEMATIC_BENCH_FRAMES` sets the timed frames (default 60), `CINEMATIC_BENCH_WARMUP` the
/// warm-up frames (default 30) and `CINEMATIC_BENCH_ONLY` a comma list of the rows to run (`ultra`,
/// `cinematic_s1`, `cinematic_s2`, `cinematic`, `cinematic_s8`; default all), for a software
/// rasterizer. `CINEMATIC_BENCH_SHADOWS=1` turns the sun shadows on (the lab scene has none by
/// default), which costs the cascades: Ultra's 4096², Cinematic's 8192². Paged sun shadows keep
/// them cached while the eye creeps; `CINEMATIC_BENCH_SHADOWS=2` also turns the paging off, so
/// every cascade is redrawn every frame (the cost of a cut or a fast pan). Each row also prints the median wall time from submit to GPU completion, which a
/// device without timestamps (or lavapipe, where it is the cost) reads instead.
#[test]
#[ignore = "GPU timing burst at 1440p and 4K"]
fn cinematic_cost_at_1440p_and_4k() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let info = device.adapter.get_info();
    eprintln!(
        "cinematic bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let env_count = |key: &str, default: usize| -> usize {
        regia_core::brand::env_var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let frames = env_count("CINEMATIC_BENCH_FRAMES", 60).max(1);
    let warmup = env_count("CINEMATIC_BENCH_WARMUP", 30);
    let only = regia_core::brand::env_var("CINEMATIC_BENCH_ONLY").ok();
    let shadow_mode = env_count("CINEMATIC_BENCH_SHADOWS", 0);
    let shadows = shadow_mode != 0;
    let wanted = |name: &str| {
        only.as_deref()
            .is_none_or(|list| list.split(',').any(|n| n.trim() == name))
    };
    let ultra = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopUltra));
    let cinematic = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Cinematic));
    let with_samples = |samples: u32| FrameQuality {
        cloud_march: CloudMarch {
            samples,
            ..CloudMarch::CINEMATIC
        },
        ..cinematic
    };
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    for (width, height) in [(2560, 1440), (3840, 2160)] {
        let lab = lab(width, height);
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cinematic bench target"),
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
        for (name, q) in [
            ("ultra", ultra),
            ("cinematic_s1", with_samples(1)),
            ("cinematic_s2", with_samples(2)),
            ("cinematic", cinematic),
            ("cinematic_s8", with_samples(8)),
        ] {
            if !wanted(name) {
                continue;
            }
            let q = FrameQuality {
                paged_sun_shadows: q.paged_sun_shadows && shadow_mode < 2,
                ..q
            };
            let mut r = renderer();
            r.set_ddgi_enabled(None);
            r.set_gpu_timing(&device, true);
            let mut in_flight = std::collections::VecDeque::new();
            let mut samples: Vec<GpuTimings> = Vec::new();
            let mut wall: Vec<f32> = Vec::new();
            for k in 0..frames + warmup {
                let mut scene = lab.as_frame_scene_with_quality(q);
                scene.eye = [k as f32 * 0.002, 3.0, k as f32 * 0.001];
                scene.directional_shadows = shadows;
                let started = std::time::Instant::now();
                r.render_into(&device, &scene, FrameTarget::full(&view, format, width, height))
                    .unwrap_or_else(|err| panic!("bench frame: {err}"));
                if let Some(submission) = r.take_submission() {
                    in_flight.push_back(submission);
                }
                // Timed frames wait for their own submission, so the wall time is one frame's.
                let depth = if k >= warmup { 1 } else { 2 };
                while in_flight.len() >= depth {
                    let Some(oldest) = in_flight.pop_front() else {
                        break;
                    };
                    if let Err(err) = device.device.poll(wgpu::PollType::Wait {
                        submission_index: Some(oldest),
                        timeout: None,
                    }) {
                        panic!("bench poll: {err}");
                    }
                }
                if k >= warmup {
                    wall.push(started.elapsed().as_secs_f32() * 1000.0);
                    samples.push(r.gpu_timings());
                }
            }
            let wall_ms = median(wall);
            let column =
                |pick: &dyn Fn(&GpuTimings) -> f32| -> f32 { median(samples.iter().map(pick).collect()) };
            let frame_ms = column(&|t: &GpuTimings| t.frame_ms);
            let scene_ms = column(&|t: &GpuTimings| t.scene_ms);
            let passes: Vec<String> = GpuTimings::default()
                .passes()
                .iter()
                .enumerate()
                .map(|(i, (pass, _))| format!("{pass} {:.3}", column(&|t: &GpuTimings| t.passes()[i].1)))
                .collect();
            eprintln!(
                "cinematic_bench {name} {width}x{height} sun_shadows {shadow_mode} frame_ms median {frame_ms:.3} scene {scene_ms:.3} wall {wall_ms:.1} | {}",
                passes.join(", ")
            );
            if width == 3840 && regia_core::brand::env_var_os("REGIA_DUMP_DIR").is_some() {
                let plain = FrameQuality { taau: false, ..q };
                let mut r = renderer();
                let mut pixels = Vec::new();
                for k in 0..32 {
                    r.set_outdoor(outdoor(k as f32 / 60.0));
                    pixels = render(&mut r, &device, &lab, plain);
                }
                dump(&format!("{name}_{width}x{height}_plain"), &pixels, width, height);
                let (cw, ch) = (960_u32, 540_u32);
                let (x0, y0) = ((width - cw) / 2, height / 8);
                let mut crop = Vec::with_capacity((cw * ch * 4) as usize);
                for y in y0..y0 + ch {
                    let row = ((y * width + x0) * 4) as usize;
                    crop.extend_from_slice(&pixels[row..row + (cw * 4) as usize]);
                }
                dump(&format!("{name}_{width}x{height}_plain_crop"), &crop, cw, ch);
                eprintln!(
                    "cinematic_bench {name} {width}x{height} hash {:016x}",
                    fnv(&pixels)
                );
            }
        }
    }
}
