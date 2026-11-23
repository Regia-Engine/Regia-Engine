// SPDX-License-Identifier: MPL-2.0
//! Local lights in the froxel fog (ADR-1365): a spot lights only the air inside its cone, a point
//! light every direction; and the pass's cost with four of each (ignored bench).
//!
//! No golden: the test compares the two halves of the frame. The camera stands in a fog box and
//! looks down −Z at the lamp 8 m ahead; a spot aimed at +X lights the right half of the frame only.
//! The box is an interior (its fog sees no sky) and the planet sky is off, so only the lamp
//! lights the air.

use regia_render::{
    DirectionalLightUniform, FogLight, FogLightCandidate, FogVolume, FrameQuality, FrameRenderer,
    FrameTarget, InteriorVolume, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabSpotLight,
    LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    VolumetricFogSettings, pick_fog_lights,
};

const W: u32 = 128;
const H: u32 = 72;
/// Exposure for a few tens of cd/m² of lit haze.
const EV: f32 = 8.0;
/// Candela of the test lamp.
const LAMP_CD: f32 = 40_000.0;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// Night: no sun, no sky, a dark wall 30 m behind the lamp.
fn scene(width: u32, height: u32) -> LabScene {
    LabScene {
        width,
        height,
        eye: [0.0, 2.0, 8.0],
        target: [0.0, 2.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.05, 0.05, 0.05, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![quad(
            [
                [-80.0, -20.0, -30.0],
                [80.0, -20.0, -30.0],
                [80.0, 40.0, -30.0],
                [-80.0, 40.0, -30.0],
            ],
            [0.0, 0.0, 1.0],
        )],
    }
}

fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    assert!(quality.volumetric_fog, "Desktop High marches the froxel fog");
    quality.taau = false;
    quality.physical_atmosphere = false;
    quality
}

fn outdoor(lights: Vec<FogLight>) -> OutdoorParams {
    OutdoorParams {
        fog: VolumetricFogSettings {
            density: 0.0,
            anisotropy: 0.0,
            planet_medium: false,
            ..VolumetricFogSettings::default()
        },
        volumes: vec![FogVolume::Box {
            center: [0.0, 2.0, 0.0],
            extents: [14.0, 6.0, 14.0],
            density: 0.05,
        }],
        lights,
        interiors: vec![InteriorVolume::with_floor([0.0, 2.0, 0.0], [15.0, 7.0, 15.0])],
        ..OutdoorParams::default()
    }
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: EV,
        bloom: false,
        ..PostSettings::golden_deterministic()
    }
}

/// Mean luma of the left and right thirds of the frame's middle rows.
fn halves(pixels: &[u8]) -> (f32, f32) {
    let luma = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        0.2126 * f32::from(pixels[i]) + 0.7152 * f32::from(pixels[i + 1]) + 0.0722 * f32::from(pixels[i + 2])
    };
    let mut left = 0.0;
    let mut right = 0.0;
    let mut n = 0.0;
    for y in H / 3..2 * H / 3 {
        for x in 0..W / 3 {
            left += luma(x, y);
            right += luma(W - 1 - x, y);
            n += 1.0;
        }
    }
    (left / n, right / n)
}

fn render(device: &RenderDevice, lights: Vec<FogLight>) -> (f32, f32) {
    let lab = scene(W, H);
    let post = post();
    let mut frame = lab.as_frame_scene_with_quality(quality());
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(outdoor(lights));
    let mut last = (0.0, 0.0);
    // A few frames so the fog history settles on the same picture.
    for _ in 0..4 {
        let image = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("fog frame: {err}"));
        last = halves(&image.pixels);
    }
    last
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_spot_lights_only_the_fog_inside_its_cone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let centre = [0.0, 2.0, 0.0];
    let dark = render(&device, Vec::new());
    let point = render(&device, vec![FogLight::point(centre, [1.0; 3], LAMP_CD, 12.0)]);
    let spot = render(
        &device,
        vec![FogLight::spot(
            centre,
            [1.0, 0.0, 0.0],
            [1.0; 3],
            LAMP_CD,
            12.0,
            0.3,
            0.5,
        )],
    );
    eprintln!("left/right luma: dark {dark:?} point {point:?} spot {spot:?}");
    let lit = |v: f32, base: f32| (v - base).max(0.0);
    let (pl, pr) = (lit(point.0, dark.0), lit(point.1, dark.1));
    let (sl, sr) = (lit(spot.0, dark.0), lit(spot.1, dark.1));
    assert!(
        pl > 5.0 && pr > 5.0,
        "the point light lights the fog on both sides: {point:?} over {dark:?}"
    );
    assert!(
        (pl - pr).abs() <= 0.2 * pl.max(pr),
        "a point light is symmetric: left {pl:.1} right {pr:.1}"
    );
    assert!(
        sr > 5.0,
        "the spot lights the fog it points at: {spot:?} over {dark:?}"
    );
    assert!(
        sl <= 0.25 * sr,
        "the spot leaves the fog behind it dark: left {sl:.1} right {sr:.1}"
    );
}

/// Mean luma of the right third of the frame: its top and bottom thirds.
fn right_top_bottom(pixels: &[u8]) -> (f32, f32) {
    let luma = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        0.2126 * f32::from(pixels[i]) + 0.7152 * f32::from(pixels[i + 1]) + 0.0722 * f32::from(pixels[i + 2])
    };
    let mut top = 0.0;
    let mut bottom = 0.0;
    let mut n = 0.0;
    for y in 0..H / 3 {
        for x in 2 * W / 3..W {
            top += luma(x, y);
            bottom += luma(x, H - 1 - y);
            n += 1.0;
        }
    }
    (top / n, bottom / n)
}

/// The spot aimed at +X, casting or not, with a wall over the upper half of its cone 1.5 m out.
fn shadowed_spot(device: &RenderDevice, cast_shadows: bool) -> (f32, f32) {
    let spot = LabSpotLight {
        position: [0.0, 2.0, 0.0],
        direction: [1.0, 0.0, 0.0],
        color: [1.0; 3],
        intensity: LAMP_CD,
        range: 12.0,
        inner_angle: 0.3,
        outer_angle: 0.5,
        cast_shadows,
        source_radius: None,
    };
    let mut lab = scene(W, H);
    lab.local_lights = vec![LabLocalLight::Spot(spot)];
    lab.meshes.push(quad(
        [
            [1.5, 2.05, -4.0],
            [1.5, 2.05, 4.0],
            [1.5, 8.0, 4.0],
            [1.5, 8.0, -4.0],
        ],
        [-1.0, 0.0, 0.0],
    ));
    let post = post();
    let mut frame = lab.as_frame_scene_with_quality(quality());
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(outdoor(vec![FogLight::from_local(&LabLocalLight::Spot(spot))]));
    let mut last = (0.0, 0.0);
    for _ in 0..4 {
        let image = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("fog frame: {err}"));
        last = right_top_bottom(&image.pixels);
    }
    last
}

#[test]
fn a_shadowed_spot_leaves_the_air_behind_a_caster_dark() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let open = shadowed_spot(&device, false);
    let shadowed = shadowed_spot(&device, true);
    eprintln!("right third top/bottom luma: unshadowed {open:?} shadowed {shadowed:?}");
    assert!(
        open.0 > 5.0 && open.1 > 5.0,
        "the cone lights both halves: {open:?}"
    );
    assert!(
        (open.0 - open.1).abs() <= 0.25 * open.0.max(open.1),
        "without shadows the beam is symmetric about its axis: {open:?}"
    );
    assert!(
        shadowed.0 <= 0.5 * open.0,
        "the wall's shadow darkens the air above the axis: {shadowed:?} vs {open:?}"
    );
    assert!(
        shadowed.1 >= 0.8 * open.1,
        "the air below the wall stays lit: {shadowed:?} vs {open:?}"
    );
}

/// [`ring`]'s spots as shadow-casting scene lights: the frame gives each an atlas tile and the
/// fog taps it.
fn shadowed_ring(count: usize) -> Vec<LabLocalLight> {
    (0..count)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / count as f32;
            LabLocalLight::Spot(LabSpotLight {
                position: [3.0 * a.cos(), 2.0, 3.0 * a.sin()],
                direction: [-a.cos(), -0.3, -a.sin()],
                color: [1.0, 0.9, 0.8],
                intensity: 8_000.0,
                range: 12.0,
                inner_angle: 0.3,
                outer_angle: 0.6,
                cast_shadows: true,
                source_radius: None,
            })
        })
        .collect()
}

fn ring(count: usize, spot: bool) -> Vec<FogLight> {
    (0..count)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / count as f32;
            let p = [3.0 * a.cos(), 2.0, 3.0 * a.sin()];
            if spot {
                FogLight::spot(
                    p,
                    [-a.cos(), -0.3, -a.sin()],
                    [1.0, 0.9, 0.8],
                    8_000.0,
                    12.0,
                    0.3,
                    0.6,
                )
            } else {
                FogLight::point(p, [1.0, 0.9, 0.8], 8_000.0, 12.0)
            }
        })
        .collect()
}

/// Froxel fog GPU time at 2560×1440 Desktop High with no local light, four points, four spots and
/// four shadow-casting spots (one atlas tap each; 3 frames in flight, 120 warm-up frames). Run with `--ignored --nocapture` on the GPU to time.
#[test]
#[ignore = "1440p fog-pass timing burst"]
fn fog_local_lights_cost_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let info = device.adapter.get_info();
    eprintln!(
        "fog local lights bench adapter {} timestamp_query={}",
        info.name, device.capabilities.timestamp_query
    );
    let (width, height) = (2560, 1440);
    let post = post();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("fog bench target"),
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
    let frames: usize = regia_core::brand::env_var("FOG_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(360);
    let casters = shadowed_ring(4);
    let shadowed: Vec<FogLight> = casters.iter().map(FogLight::from_local).collect();
    for (name, lights, scene_lights) in [
        ("none", Vec::new(), Vec::new()),
        ("points4", ring(4, false), Vec::new()),
        ("spots4", ring(4, true), Vec::new()),
        ("spots4_shadowed", shadowed, casters),
    ] {
        let mut lab = scene(width, height);
        lab.local_lights = scene_lights;
        let mut r = FrameRenderer::new();
        r.set_ddgi_enabled(Some(false));
        r.set_camera_cut_detection(None);
        r.set_outdoor(outdoor(lights));
        r.set_gpu_timing(&device, true);
        let mut in_flight = std::collections::VecDeque::new();
        let mut samples = Vec::new();
        for k in 0..frames {
            let mut scene = lab.as_frame_scene_with_quality(quality());
            scene.post = Some(&post);
            // The eye creeps, so the fog history is reprojected every frame.
            scene.eye = [k as f32 * 0.002, 2.0, 8.0 + k as f32 * 0.001];
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
                samples.push(r.gpu_timings().fog_ms);
            }
        }
        samples.sort_by(f32::total_cmp);
        let median = samples.get(samples.len() / 2).copied().unwrap_or(0.0);
        let p90 = samples.get(samples.len() * 9 / 10).copied().unwrap_or(0.0);
        eprintln!("fog_local_lights_bench {name} {width}x{height} fog_ms median {median:.3} p90 {p90:.3}");
    }
}

/// The fog lights the renderer fed before ADR-1365: the first four in extraction order (points,
/// then spots), each divided by 4π and drawn without a cone.
fn old_fog_lights(lights: &[LabLocalLight]) -> Vec<FogLight> {
    let to_cd = 1.0 / (4.0 * std::f32::consts::PI);
    let points = lights.iter().filter_map(|l| match l {
        LabLocalLight::Point(p) => Some(FogLight::point(
            p.position,
            p.color,
            p.intensity * to_cd,
            p.radius,
        )),
        LabLocalLight::Spot(_) => None,
    });
    let spots = lights.iter().filter_map(|l| match l {
        LabLocalLight::Spot(s) => Some(FogLight::point(s.position, s.color, s.intensity * to_cd, s.range)),
        LabLocalLight::Point(_) => None,
    });
    points.chain(spots).take(4).collect()
}

/// What `extract_frame` feeds now: the pick from the eye.
fn new_fog_lights(lights: &[LabLocalLight], eye: [f32; 3], far: f32) -> Vec<FogLight> {
    let candidates: Vec<FogLightCandidate> = lights
        .iter()
        .enumerate()
        .map(|(key, light)| FogLightCandidate {
            key: key as u64,
            light: FogLight::from_local(light),
        })
        .collect();
    pick_fog_lights(&candidates, eye, far, &[])
        .iter()
        .map(|c| c.light)
        .collect()
}

fn write_png(dir: &std::path::Path, name: &str, width: u32, height: u32, pixels: &[u8]) {
    let path = dir.join(format!("{name}.png"));
    let Ok(file) = std::fs::File::create(&path) else {
        eprintln!("cannot write {}", path.display());
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
    eprintln!("wrote {}", path.display());
}

/// Exposure of the by-eye captures.
const CAPTURE_EV: f32 = 8.0;

/// A dusty corridor at night, 6 m wide and 4 m tall, running down −Z.
fn corridor(width: u32, height: u32, local_lights: Vec<LabLocalLight>) -> LabScene {
    LabScene {
        width,
        height,
        eye: [0.0, 1.7, 0.0],
        target: [0.0, 1.6, -10.0],
        fov_y: 1.0,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: CAPTURE_EV,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights,
        materials: vec![LabMaterial {
            base_color: [0.3, 0.28, 0.25, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            quad(
                [
                    [-3.0, 0.0, 5.0],
                    [3.0, 0.0, 5.0],
                    [3.0, 0.0, -60.0],
                    [-3.0, 0.0, -60.0],
                ],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [
                    [-3.0, 4.0, 5.0],
                    [3.0, 4.0, 5.0],
                    [3.0, 4.0, -60.0],
                    [-3.0, 4.0, -60.0],
                ],
                [0.0, -1.0, 0.0],
            ),
            quad(
                [
                    [-3.0, 0.0, 5.0],
                    [-3.0, 0.0, -60.0],
                    [-3.0, 4.0, -60.0],
                    [-3.0, 4.0, 5.0],
                ],
                [1.0, 0.0, 0.0],
            ),
            quad(
                [
                    [3.0, 0.0, 5.0],
                    [3.0, 0.0, -60.0],
                    [3.0, 4.0, -60.0],
                    [3.0, 4.0, 5.0],
                ],
                [-1.0, 0.0, 0.0],
            ),
        ],
    }
}

/// By-eye captures for the lead: the corridor's haze before and after ADR-1365, written to
/// `$FOG_CAPTURE_DIR` (`headlight_{old,new}.png`: the tunnel headlight alone;
/// `flash_{old,new}.png`: four dim ceiling lamps, the headlight and a muzzle flash 3 m ahead;
/// `doorway_{old,new}.png`: a shadowed floodlight's shaft through a doorway, phase 2).
/// Run with `FOG_CAPTURE_DIR=/tmp/fog cargo test -p regia_render --test fog_local_lights
/// fog_light_captures -- --ignored --nocapture`.
#[test]
#[ignore = "writes by-eye captures to FOG_CAPTURE_DIR"]
fn fog_light_captures() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(dir) = regia_core::brand::env_var_os("FOG_CAPTURE_DIR").map(std::path::PathBuf::from) else {
        eprintln!("set FOG_CAPTURE_DIR to write the captures");
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        panic!("capture dir {}: {err}", dir.display());
    }
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let (width, height) = (640, 360);
    let headlight = LabLocalLight::Spot(LabSpotLight {
        position: [0.0, 1.7, 0.0],
        direction: [0.0, -0.05, -1.0],
        color: [1.0, 0.95, 0.85],
        intensity: 40_000.0,
        range: 70.0,
        inner_angle: 0.15,
        outer_angle: 0.45,
        cast_shadows: false,
        source_radius: None,
    });
    let lamp = |z: f32| {
        LabLocalLight::Point(LabPointLight {
            position: [0.0, 3.7, z],
            color: [1.0, 0.8, 0.55],
            intensity: 1_500.0,
            radius: 8.0,
            cast_shadows: false,
            source_radius: None,
        })
    };
    let flash = LabLocalLight::Point(LabPointLight {
        position: [0.5, 1.5, -3.0],
        color: [1.0, 0.75, 0.4],
        intensity: 60_000.0,
        radius: 10.0,
        cast_shadows: false,
        source_radius: None,
    });
    let shots = [
        ("headlight", vec![headlight]),
        (
            "flash",
            vec![
                lamp(-18.0),
                lamp(-26.0),
                lamp(-34.0),
                lamp(-42.0),
                flash,
                headlight,
            ],
        ),
    ];
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: CAPTURE_EV,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    for (name, lights) in shots {
        let lab = corridor(width, height, lights.clone());
        for (variant, fog_lights) in [
            ("old", old_fog_lights(&lights)),
            ("new", new_fog_lights(&lights, lab.eye, lab.far)),
        ] {
            let pixels = render_capture(&device, &lab, &post, fog_lights);
            write_png(&dir, &format!("{name}_{variant}"), width, height, &pixels);
        }
    }
    // Phase 2: a shadow-casting floodlight behind a partition throws its shaft through the
    // doorway. `doorway_old` is the fog without the atlas tap (the fog light moved 1 mm, so it
    // matches no tile); the surfaces are lit and shadowed the same in both.
    let flood = LabSpotLight {
        position: [-2.0, 3.0, -16.0],
        direction: [2.0, -1.75, 6.0],
        color: [1.0, 0.95, 0.85],
        intensity: 40_000.0,
        range: 30.0,
        inner_angle: 0.3,
        outer_angle: 0.5,
        cast_shadows: true,
        source_radius: None,
    };
    let mut lab = corridor(width, height, vec![LabLocalLight::Spot(flood)]);
    for (x0, x1, y0, y1) in [
        (-3.0, -0.6, 0.0, 4.0),
        (0.6, 3.0, 0.0, 4.0),
        (-0.6, 0.6, 2.5, 4.0),
    ] {
        lab.meshes.push(quad(
            [[x0, y0, -10.0], [x1, y0, -10.0], [x1, y1, -10.0], [x0, y1, -10.0]],
            [0.0, 0.0, 1.0],
        ));
    }
    let shadowed = FogLight::from_local(&LabLocalLight::Spot(flood));
    let mut unshadowed = shadowed;
    unshadowed.position[0] += 1.0e-3;
    for (variant, fog_light) in [("old", unshadowed), ("new", shadowed)] {
        let pixels = render_capture(&device, &lab, &post, vec![fog_light]);
        write_png(&dir, &format!("doorway_{variant}"), width, height, &pixels);
    }
}

/// Six frames of the corridor's fog with `fog_lights`; the last frame's pixels.
fn render_capture(
    device: &RenderDevice,
    lab: &LabScene,
    post: &PostSettings,
    fog_lights: Vec<FogLight>,
) -> Vec<u8> {
    let mut frame = lab.as_frame_scene_with_quality(quality());
    frame.post = Some(post);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(OutdoorParams {
        fog: VolumetricFogSettings {
            density: 0.0,
            planet_medium: false,
            ..VolumetricFogSettings::default()
        },
        volumes: vec![FogVolume::Box {
            center: [0.0, 2.0, -27.5],
            extents: [3.0, 2.0, 32.5],
            density: 0.03,
        }],
        lights: fog_lights,
        interiors: vec![InteriorVolume::with_floor([0.0, 2.0, -27.5], [3.0, 2.0, 32.5])],
        ..OutdoorParams::default()
    });
    let mut pixels = Vec::new();
    for _ in 0..6 {
        pixels = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("capture frame: {err}"))
            .pixels;
    }
    pixels
}
