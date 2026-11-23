// SPDX-License-Identifier: MPL-2.0
//! M15b high-end lighting on lavapipe: paged shadows, probe GI, SSR.
//!
//! Goldens are 160×120. Bless with `REGIA_BLESS=1`. A pass that ran must report
//! a positive lavapipe timestamp. Those milliseconds are not a frame budget.

use std::time::Instant;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GiField, GiMode, LabLocalLight, LabMaterial,
    LabMesh, LabPointLight, LabScene, PostSettings, ProbeSun, ReflectionProbe, RenderDevice, RenderProfile,
    RenderProfilePreset, SdfScene, SdfShape, ShadowFilter, box_mesh, sample_irradiance, sequence_flicker,
    update_irradiance,
};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, golden_path, guard_golden, ssim_rgba8,
};

const W: u32 = 160;
const H: u32 = 120;

fn device() -> RenderDevice {
    RenderDevice::headless()
        .expect("m15b lighting requires a GPU; lavapipe is present here and a missing device is a failure")
}

fn translation(offset: [f32; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [offset[0], offset[1], offset[2], 1.0],
    ]
}

fn solid(color: [f32; 3], roughness: f32, emissive: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        metallic: 0.0,
        roughness,
        emissive,
        emissive_intensity: 1.0,
        ..LabMaterial::default()
    }
}

fn placed_box(half: [f32; 3], offset: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: translation(offset),
        joints: Vec::new(),
    }
}

fn base_scene(materials: Vec<LabMaterial>, meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [3.2, 2.4, 3.6],
        target: [0.0, 0.4, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 15.0,
        ambient_lux: 1_500.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([-0.45, -0.85, -0.25], [1.0, 0.98, 0.94], 80_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn shadow_quality() -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        shadow_filter: ShadowFilter::PcssLite,
        gi: GiMode::Ambient,
        screen_space_gi: false,
        ssr: false,
        reflection_probes: false,
        ..FrameQuality::LAB
    }
}

fn render(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &LabScene,
    quality: FrameQuality,
) -> Vec<u8> {
    renderer
        .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))
        .expect("render")
        .pixels
}

fn mean_rgb(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> [f64; 3] {
    let mut acc = [0.0; 3];
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            acc[0] += f64::from(pixels[i]);
            acc[1] += f64::from(pixels[i + 1]);
            acc[2] += f64::from(pixels[i + 2]);
            n += 1.0;
        }
    }
    [acc[0] / n, acc[1] / n, acc[2] / n]
}

fn tint_excess(on: &[u8], off: &[u8]) -> f64 {
    tile_excess(on, off, |c| c[0] - c[1])
}

fn tile_excess(on: &[u8], off: &[u8], score: impl Fn([f64; 3]) -> f64) -> f64 {
    let mut best = f64::MIN;
    for ty in 0..8 {
        for tx in 0..8 {
            let x0 = tx * W / 8;
            let y0 = ty * H / 8;
            let a = mean_rgb(on, x0, y0, x0 + W / 8, y0 + H / 8);
            let b = mean_rgb(off, x0, y0, x0 + W / 8, y0 + H / 8);
            best = best.max(score(a) - score(b));
        }
    }
    best
}

fn dump(name: &str, pixels: &[u8]) {
    let image: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        image::ImageBuffer::from_raw(W, H, pixels.to_vec()).expect("debug image");
    // Best-effort under the OS temp directory: nothing asserts on it (audit-v035-flaky M6).
    let path = std::env::temp_dir().join(format!("m15b_{name}.png"));
    match image.save(&path) {
        Ok(()) => eprintln!("wrote {}", path.display()),
        Err(err) => eprintln!("debug image {} not written: {err}", path.display()),
    }
}

fn lit_guard() -> GoldenGuard {
    GoldenGuard {
        min_lum: 12.0,
        max_lum: 210.0,
        max_white_pct: 20.0,
        max_black_pct: 55.0,
        min_object_pct: 25.0,
    }
}

fn check_golden(device: &RenderDevice, pixels: &[u8], name: &str, guard: GoldenGuard) {
    guard_golden(pixels, W, H, name, guard);
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        pixels,
        W,
        H,
        &golden_path(name),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden");
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn paged_shadows_stay_stable_and_a_mover_dirties_only_its_pages() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let materials = vec![
        solid([0.65, 0.65, 0.62], 0.8, [0.0; 3]),
        solid([0.75, 0.35, 0.2], 0.6, [0.0; 3]),
    ];
    let mut scene = base_scene(
        materials,
        vec![
            placed_box([6.0, 0.08, 6.0], [0.0, 0.0, 0.0], 0),
            placed_box([0.35, 0.7, 0.35], [-0.4, 0.7, 0.2], 1),
            placed_box([0.3, 0.45, 0.3], [1.4, 0.45, -0.6], 1),
        ],
    );
    scene.local_lights = vec![
        LabLocalLight::Point(LabPointLight {
            position: [0.0, 2.2, 0.2],
            color: [1.0, 0.9, 0.7],
            intensity: 4_000.0,
            radius: 6.0,
            cast_shadows: true,
            source_radius: None,
        }),
        LabLocalLight::Point(LabPointLight {
            position: [18.0, 2.0, 0.0],
            color: [0.6, 0.7, 1.0],
            intensity: 2_000.0,
            radius: 3.0,
            cast_shadows: true,
            source_radius: None,
        }),
    ];
    let quality = shadow_quality();
    let mut renderer = FrameRenderer::new();
    let first = render(&mut renderer, &device, &scene, quality);
    assert!(
        renderer
            .shadow_cache_report()
            .cascade_full_redraw
            .iter()
            .any(|full| *full),
        "the first cascade draw is a full clear"
    );
    let first_atlas = renderer.shadow_cache_report().atlas_tiles_drawn;
    assert!(
        first_atlas >= 2,
        "the first frame draws every local light, drew {first_atlas}"
    );
    let second = render(&mut renderer, &device, &scene, quality);
    let quiet = renderer.shadow_cache_report();
    assert!(
        quiet.dirty_pages.is_empty(),
        "a static scene redraws no pages: {quiet:?}"
    );
    assert!(
        quiet.cascades_skipped >= 1,
        "clean cascades are skipped: {quiet:?}"
    );
    assert!(quiet.atlas_skipped, "a static atlas is reused: {quiet:?}");
    assert_eq!(
        quiet.atlas_tiles_drawn, 0,
        "a skipped atlas draws no tiles: {quiet:?}"
    );
    let same = ssim_rgba8(&first, &second, W, H).expect("ssim");
    assert!(
        same >= 0.98,
        "a cached shadow frame must match the full redraw, SSIM {same:.4}"
    );

    // Eye and target move together, so the look direction stays put. Four steps
    // of 3 mm is 12 mm, under one cascade-0 texel (~22 mm at the lab 512² map).
    // Crossing a grid line may redraw a cascade; a broken snap redraws all four.
    let mut frames = vec![second];
    let mut clean_steps = 0u32;
    for step in 1..=4 {
        scene.eye[0] += 0.003;
        scene.target[0] += 0.003;
        frames.push(render(&mut renderer, &device, &scene, quality));
        let cache = renderer.shadow_cache_report();
        if cache.dirty_pages.is_empty() {
            clean_steps += 1;
        }
        eprintln!(
            "m15b cam {step} dirty {} full {:?}",
            cache.dirty_pages.len(),
            cache.cascade_full_redraw
        );
    }
    assert!(
        clean_steps >= 2,
        "sub-texel camera travel must keep most snapped cascades, clean steps {clean_steps} of 4"
    );
    let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let flicker = sequence_flicker(&refs, W, H);
    eprintln!("m15b shadow camera flicker {flicker:.4}");
    assert!(
        flicker < 0.02,
        "camera motion must not shimmer, flicker {flicker}"
    );
    // Those steps are sub-texel, so a metric that returns 0 also passes the
    // ceiling. A known salt pattern must score above it.
    let mut salted = frames[frames.len() - 1].clone();
    for y in (2..H).step_by(4) {
        for x in (2..W).step_by(4) {
            let i = ((y * W + x) * 4) as usize;
            salted[i] = 255 - salted[i];
            salted[i + 1] = 255 - salted[i + 1];
            salted[i + 2] = 255 - salted[i + 2];
        }
    }
    let shimmer = sequence_flicker(&[frames[frames.len() - 1].as_slice(), salted.as_slice()], W, H);
    assert!(
        shimmer > 0.02 && shimmer > flicker,
        "a metric that returns 0 would hide shimmer; camera {flicker} shimmer {shimmer}"
    );

    scene.meshes[1].model = translation([0.7, 0.7, 0.2]);
    let moved = render(&mut renderer, &device, &scene, quality);
    let report = renderer.shadow_cache_report().clone();
    let page_count = 4 * 4 * report.cascade_full_redraw.len().max(1);
    assert!(
        !report.dirty_pages.is_empty(),
        "the moved caster must dirty pages"
    );
    assert!(
        report.dirty_pages.len() < page_count,
        "it must not dirty every page, {} of {page_count}: {report:?}",
        report.dirty_pages.len()
    );
    assert!(
        report.cascade_full_redraw.iter().any(|full| !*full),
        "a caster move is not a cascade-matrix rebuild: {report:?}"
    );
    assert_eq!(
        report.dirty_atlas_tiles,
        vec![0],
        "only the near local light overlaps the mover, {report:?}"
    );
    // The near light is six tiles (cube faces). A caster's box is the cube around its bounding
    // sphere (`world_caster_aabb`: the mover's 0.35 x 0.7 x 0.35 half extents give +-0.86 m), so
    // its old and new boxes reach every face of the near light but the one above it (+Y): those
    // five are redrawn and +Y is kept (ADR-7986). The far light is not dirty. (Redrawing "tile 0",
    // the light's +X face, left the -Y face under it holding the mover's old shadow.)
    assert_eq!(
        report.atlas_tiles_drawn, 5,
        "the mover reaches five faces of the near light, not the whole atlas, {report:?}"
    );
    assert_eq!(
        report.atlas_faces_kept, 1,
        "the near light's face above it is kept, {report:?}"
    );
    let changed = ssim_rgba8(&frames[4], &moved, W, H).expect("ssim");
    assert!(
        changed < 0.999,
        "moving the caster must change the picture, SSIM {changed:.4}"
    );

    let mut suns = Vec::new();
    for (index, direction) in [[-0.2, -0.95, -0.15], [-0.7, -0.6, -0.35], [0.15, -0.9, -0.4]]
        .into_iter()
        .enumerate()
    {
        scene.directional = DirectionalLightUniform::new(direction, [1.0, 0.98, 0.94], 80_000.0);
        let pixels = render(&mut renderer, &device, &scene, quality);
        if index == 0 {
            let versus = ssim_rgba8(&frames[0], &pixels, W, H).expect("ssim");
            assert!(
                versus < 0.995,
                "a new sun angle must move the shadow, SSIM {versus:.4}"
            );
        }
        suns.push(pixels);
    }
    for (index, frame) in frames.iter().enumerate() {
        check_golden(&device, frame, &format!("m15b_shadow_cam{index}"), lit_guard());
    }
    for (index, frame) in suns.iter().enumerate() {
        check_golden(&device, frame, &format!("m15b_shadow_sun{index}"), lit_guard());
    }
}

#[test]
fn probe_gi_tints_a_shadowed_wall_and_a_closed_room_blocks_the_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut scene = base_scene(
        vec![
            solid([0.75, 0.75, 0.72], 0.85, [0.0; 3]),
            solid([0.8, 0.08, 0.06], 0.7, [0.0; 3]),
            solid([0.08, 0.55, 0.12], 0.7, [0.0; 3]),
        ],
        vec![
            placed_box([1.2, 0.05, 1.2], [0.0, 0.0, 0.0], 0),
            placed_box([0.06, 1.0, 1.2], [-1.15, 1.0, 0.0], 1),
            placed_box([0.06, 1.0, 1.2], [1.15, 1.0, 0.0], 2),
            placed_box([1.2, 1.0, 0.06], [0.0, 1.0, -1.15], 0),
            placed_box([1.2, 0.05, 1.2], [0.0, 2.05, 0.0], 0),
        ],
    );
    scene.eye = [0.1, 1.05, 2.4];
    scene.target = [0.55, 0.9, 0.0];
    // EV 11 keeps the probe tint below the white clip. Ambient is high in lux
    // and small after that exposure, so the box is visible with GI off.
    scene.exposure_ev = 11.0;
    scene.ambient_lux = 4_500.0;
    scene.directional = DirectionalLightUniform::new([-0.85, -0.45, -0.15], [1.0, 0.95, 0.9], 12_000.0);
    let mut sdf = SdfScene::default();
    let wall = |center, half, albedo| SdfShape::Box {
        center,
        half,
        albedo,
        emissive: [0.0; 3],
    };
    sdf.shapes
        .push(wall([0.0, 0.0, 0.0], [1.2, 0.05, 1.2], [0.75, 0.75, 0.72]));
    sdf.shapes
        .push(wall([-1.15, 1.0, 0.0], [0.06, 1.0, 1.2], [0.8, 0.08, 0.06]));
    sdf.shapes
        .push(wall([1.15, 1.0, 0.0], [0.06, 1.0, 1.2], [0.08, 0.55, 0.12]));
    sdf.shapes
        .push(wall([0.0, 1.0, -1.15], [1.2, 1.0, 0.06], [0.75, 0.75, 0.72]));
    sdf.shapes
        .push(wall([0.0, 2.05, 0.0], [1.2, 0.05, 1.2], [0.75, 0.75, 0.72]));
    let sun = ProbeSun {
        direction: [-0.85, -0.45, -0.15],
        color: [1.0, 0.95, 0.9],
        illuminance: 12_000.0,
    };
    let started = Instant::now();
    let mut field = GiField::new([-0.7, 0.45, -0.7], [0.7, 0.8, 0.7], [3, 2, 3]).expect("grid");
    field.intensity = 6.0;
    update_irradiance(&mut field, &sdf, sun, 48, 0.0);
    let trace_ms = started.elapsed().as_secs_f32() * 1_000.0;
    let quality_on = FrameQuality {
        gi: GiMode::Probes,
        paged_sun_shadows: true,
        shadow_filter: ShadowFilter::PcssLite,
        screen_space_gi: false,
        ssr: false,
        reflection_probes: false,
        ..FrameQuality::LAB
    };
    let quality_off = FrameQuality {
        gi: GiMode::Ambient,
        ..quality_on
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_gi_cpu_ms(trace_ms);
    renderer.set_gi_field(Some(field));
    let on = render(&mut renderer, &device, &scene, quality_on);
    assert!((renderer.gi_cpu_ms() - trace_ms).abs() < 1.0e-3);
    let off = render(&mut renderer, &device, &scene, quality_off);
    let excess = tint_excess(&on, &off);
    eprintln!("m15b GI red-excess {excess:.2}");
    if excess < 1.5 {
        dump("gi_on", &on);
        dump("gi_off", &off);
    }
    assert!(
        excess > 1.5,
        "GI must add red bleed the ambient term does not, excess {excess:.2}"
    );
    assert!(
        ssim_rgba8(&on, &off, W, H).expect("ssim") < 0.995,
        "GI on and off must differ"
    );

    // Closed shell, local light inside, probes that either see the sky or the shell.
    let mut room = base_scene(
        vec![solid([0.6, 0.58, 0.55], 0.8, [0.0; 3])],
        vec![
            placed_box([1.5, 0.05, 1.5], [0.0, 0.0, 0.0], 0),
            placed_box([1.5, 0.05, 1.5], [0.0, 2.4, 0.0], 0),
            placed_box([0.05, 1.2, 1.5], [-1.5, 1.2, 0.0], 0),
            placed_box([0.05, 1.2, 1.5], [1.5, 1.2, 0.0], 0),
            placed_box([1.5, 1.2, 0.05], [0.0, 1.2, -1.5], 0),
            placed_box([1.5, 1.2, 0.05], [0.0, 1.2, 1.5], 0),
        ],
    );
    room.eye = [0.2, 1.1, 0.7];
    room.target = [0.2, 1.0, -0.4];
    room.exposure_ev = 4.0;
    room.ambient_lux = 12.0;
    room.directional = DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0; 3], 40_000.0);
    room.local_lights = vec![LabLocalLight::Point(LabPointLight {
        position: [0.0, 1.2, 0.0],
        color: [1.0, 0.85, 0.7],
        intensity: 900.0,
        radius: 4.0,
        cast_shadows: false,
        source_radius: None,
    })];
    let mut shell = SdfScene::default();
    let panel = |center, half| SdfShape::Box {
        center,
        half,
        albedo: [0.6, 0.58, 0.55],
        emissive: [0.0; 3],
    };
    shell.shapes.push(panel([0.0, 0.0, 0.0], [1.5, 0.05, 1.5]));
    shell.shapes.push(panel([0.0, 2.4, 0.0], [1.5, 0.05, 1.5]));
    shell.shapes.push(panel([-1.5, 1.2, 0.0], [0.05, 1.2, 1.5]));
    shell.shapes.push(panel([1.5, 1.2, 0.0], [0.05, 1.2, 1.5]));
    shell.shapes.push(panel([0.0, 1.2, -1.5], [1.5, 1.2, 0.05]));
    shell.shapes.push(panel([0.0, 1.2, 1.5], [1.5, 1.2, 0.05]));
    let leak_sun = ProbeSun {
        direction: [0.2, -1.0, 0.1],
        color: [1.0; 3],
        illuminance: 40_000.0,
    };
    // The probe box spans the room: probe GI now replaces the ambient term inside the
    // volume and fades back to it one cell outside, so receivers must be covered.
    let mut closed = GiField::new([-1.2, 0.35, -1.2], [0.8, 0.85, 0.8], [4, 3, 4]).expect("grid");
    closed.intensity = 4.0;
    update_irradiance(&mut closed, &shell, leak_sun, 36, 0.0);
    let mut open_field = GiField::new([-1.2, 0.35, -1.2], [0.8, 0.85, 0.8], [4, 3, 4]).expect("grid");
    open_field.intensity = 4.0;
    update_irradiance(&mut open_field, &SdfScene::default(), leak_sun, 36, 0.0);
    let sample_closed = sample_irradiance(&closed, [0.0, 1.3, 0.0], [0.0, 1.0, 0.0]);
    let sample_open = sample_irradiance(&open_field, [0.0, 1.3, 0.0], [0.0, 1.0, 0.0]);
    let energy = |c: [f32; 3]| c[0] + c[1] + c[2];
    assert!(
        energy(sample_closed) < energy(sample_open) * 0.55,
        "CPU probes inside the shell stay darker, closed {sample_closed:?} open {sample_open:?}"
    );
    let mut room_renderer = FrameRenderer::new();
    room_renderer.set_gi_field(Some(closed));
    let closed_px = render(&mut room_renderer, &device, &room, quality_on);
    room_renderer.set_gi_field(Some(open_field));
    let open_px = render(&mut room_renderer, &device, &room, quality_on);
    let closed_lum = mean_rgb(&closed_px, 0, 0, W, H);
    let open_lum = mean_rgb(&open_px, 0, 0, W, H);
    let closed_y = 0.2126 * closed_lum[0] + 0.7152 * closed_lum[1] + 0.0722 * closed_lum[2];
    let open_y = 0.2126 * open_lum[0] + 0.7152 * open_lum[1] + 0.0722 * open_lum[2];
    eprintln!("m15b interior lum closed {closed_y:.1} sky-probes {open_y:.1}");
    if closed_y > open_y * 0.85 {
        dump("interior", &closed_px);
        dump("interior_sky", &open_px);
    }
    assert!(
        closed_y < open_y * 0.85,
        "sky probes must not light the room as brightly as an open field, closed {closed_y:.1} open {open_y:.1}"
    );
    check_golden(&device, &on, "m15b_gi_on", lit_guard());
    check_golden(&device, &off, "m15b_gi_off", lit_guard());
    check_golden(&device, &closed_px, "m15b_interior", lit_guard());
}

#[test]
fn ssr_shows_the_scene_and_probes_remain_when_ssr_is_off() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut scene = base_scene(
        vec![
            solid([0.42, 0.42, 0.44], 0.35, [0.0; 3]),
            solid([0.02, 0.02, 0.02], 0.4, [0.0, 12.0, 12.0]),
        ],
        vec![
            placed_box([4.0, 0.05, 4.0], [0.0, 0.0, 0.0], 0),
            placed_box([0.28, 0.55, 0.28], [0.0, 1.05, -0.7], 1),
        ],
    );
    scene.eye = [0.0, 1.55, 1.9];
    scene.target = [0.0, 0.12, 0.15];
    scene.directional_shadows = false;
    scene.exposure_ev = 3.0;
    scene.ambient_lux = 4.0;
    scene.directional = DirectionalLightUniform::new([0.15, -1.0, 0.1], [1.0; 3], 25.0);
    scene.post = Some(PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 3.0,
        fxaa: false,
        taa: false,
        film_grain: 0.0,
        vignette: 0.0,
        ..PostSettings::default()
    });
    let mut equirect = vec![0u8; (64 * 32 * 4) as usize];
    for px in equirect.as_chunks_mut::<4>().0 {
        px[2] = 220;
        px[3] = 255;
    }
    let probe = ReflectionProbe {
        id: 1,
        center: [0.0, 0.6, 0.4],
        half_extent: [3.0, 2.0, 3.0],
        equirect,
        width: 64,
        height: 32,
    };
    let quality = FrameQuality {
        gi: GiMode::Ambient,
        screen_space_gi: false,
        ssr: true,
        reflection_probes: true,
        max_reflection_probes: 1,
        paged_sun_shadows: false,
        ..FrameQuality::LAB
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_reflection_probes(vec![probe]);
    let ssr_on = render(&mut renderer, &device, &scene, quality);
    let probe_only = render(
        &mut renderer,
        &device,
        &scene,
        FrameQuality {
            ssr: false,
            screen_space_gi: false,
            ..quality
        },
    );
    renderer.set_reflection_probes(Vec::new());
    let bare = render(
        &mut renderer,
        &device,
        &scene,
        FrameQuality {
            ssr: false,
            reflection_probes: false,
            screen_space_gi: false,
            ..quality
        },
    );
    let cyan = |c: [f64; 3]| c[1].min(c[2]) - c[0];
    let blue = |c: [f64; 3]| c[2];
    let ssr_cyan = tile_excess(&ssr_on, &probe_only, cyan);
    let probe_blue = tile_excess(&probe_only, &bare, blue);
    eprintln!("m15b SSR cyan-excess {ssr_cyan:.1} probe-blue-excess {probe_blue:.1}");
    if ssr_cyan < 4.0 || probe_blue < 4.0 {
        dump("ssr_on", &ssr_on);
        dump("ssr_probe", &probe_only);
        dump("ssr_bare", &bare);
    }
    assert!(
        ssr_cyan > 4.0,
        "SSR must put the cyan bar in the floor, excess {ssr_cyan:.1}"
    );
    assert!(
        probe_blue > 4.0,
        "with SSR off the floor keeps the probe, blue excess {probe_blue:.1}"
    );
    check_golden(&device, &ssr_on, "m15b_ssr_on", lit_guard());
    check_golden(&device, &probe_only, "m15b_ssr_probe", lit_guard());
}

#[test]
fn screen_space_gi_adds_neighbour_colour_and_low_profiles_skip_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut scene = base_scene(
        vec![
            solid([0.55, 0.55, 0.55], 0.7, [0.0; 3]),
            solid([0.15, 0.02, 0.02], 0.5, [36.0, 0.4, 0.12]),
        ],
        // The red emitter is a fin standing out of the grey panel toward the camera, so
        // it faces the grey surface beside it. (Side by side in one plane, the two never
        // exchange light: the screen-space bounce now weighs each neighbour by the
        // receiver's cosine toward it, where the old pass blurred colour across any edge.)
        vec![
            placed_box([0.85, 1.05, 0.06], [-0.7, 1.05, 0.0], 0),
            placed_box([0.06, 1.05, 0.45], [0.2, 1.05, 0.5], 1),
        ],
    );
    scene.eye = [0.0, 1.05, 2.5];
    scene.target = [0.0, 1.05, 0.0];
    scene.directional_shadows = false;
    scene.exposure_ev = 6.0;
    scene.ambient_lux = 4.0;
    scene.directional = DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 30.0);
    scene.post = Some(PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 6.0,
        fxaa: false,
        taa: false,
        film_grain: 0.0,
        vignette: 0.0,
        ..PostSettings::default()
    });
    let on_q = FrameQuality {
        screen_space_gi: true,
        gi: GiMode::ScreenSpace,
        ssr: false,
        reflection_probes: false,
        ..FrameQuality::LAB
    };
    let off_q = FrameQuality {
        screen_space_gi: false,
        gi: GiMode::Ambient,
        ..on_q
    };
    let mut renderer = FrameRenderer::new();
    let on = render(&mut renderer, &device, &scene, on_q);
    let off = render(&mut renderer, &device, &scene, off_q);
    let excess = tint_excess(&on, &off);
    eprintln!("m15b SSGI red-excess {excess:.2}");
    if excess < 0.8 {
        dump("ssgi_on", &on);
        dump("ssgi_off", &off);
    }
    assert!(
        excess > 0.8,
        "screen-space GI must tint a neighbor, excess {excess:.2}"
    );

    let mobile = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileLow));
    let low = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
    assert!(!mobile.ssr && !mobile.screen_space_gi && !mobile.paged_sun_shadows);
    assert_eq!(mobile.gi, GiMode::Ambient);
    // Desktop Low keeps retained (paged) sun cascades since look-r2 round 9: on small frames
    // their redraw cost does not shrink with resolution. It still skips SSR and probes.
    assert!(!low.ssr && !low.reflection_probes && low.paged_sun_shadows);
    assert_eq!(low.gi, GiMode::Ambient);
    let desktop = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    assert!(desktop.paged_sun_shadows && desktop.ssr && desktop.screen_space_gi);
    assert_eq!(desktop.gi, GiMode::Probes);

    renderer.set_gpu_timing(&device, true);
    // Metal has no in-encoder timestamps (ADR-275): the timer is off there, so there is nothing to
    // check. Where the adapter can time passes, a missing timer must not pass as zeros.
    if device.capabilities.timestamp_query {
        // The golden above is SSGI only. Time a frame that also runs shadows and SSR,
        // so a zero on a skipped pass cannot satisfy the check.
        let mut timed_scene = scene.clone();
        timed_scene.directional_shadows = true;
        let timed_q = FrameQuality {
            ssr: true,
            screen_space_gi: true,
            ..on_q
        };
        let _ = render(&mut renderer, &device, &timed_scene, timed_q);
        let timings = renderer.gpu_timings();
        for (name, value) in [
            ("shadows", timings.shadows_ms),
            ("forward", timings.forward_ms),
            ("ssgi", timings.ssgi_ms),
            ("ssr", timings.ssr_ms),
            ("scene", timings.scene_ms),
        ] {
            assert!(
                value.is_finite() && value > 0.0,
                "{name} ran and must not time as zero, got {value}"
            );
        }
        eprintln!(
            "m15b lavapipe timings shadows {:.3} forward {:.3} ssgi {:.3} ssr {:.3} scene {:.3}",
            timings.shadows_ms, timings.forward_ms, timings.ssgi_ms, timings.ssr_ms, timings.scene_ms
        );
    } else {
        eprintln!(
            "m15b: no GPU timer on {} ({:?}); skipping the timing check",
            device.capabilities.adapter_name, device.capabilities.backend
        );
    }
    check_golden(&device, &on, "m15b_ssgi", lit_guard());
}

fn patch_mean(pixels: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> [f64; 3] {
    let mut acc = [0.0; 3];
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * width + x) * 4) as usize;
            acc[0] += f64::from(pixels[i]);
            acc[1] += f64::from(pixels[i + 1]);
            acc[2] += f64::from(pixels[i + 2]);
            n += 1.0;
        }
    }
    [acc[0] / n, acc[1] / n, acc[2] / n]
}

fn assert_emissive(name: &str, mean: [f64; 3], channel: usize) {
    let value = mean[channel];
    let others = mean
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != channel)
        .map(|(_, sample)| *sample)
        .fold(0.0, f64::max);
    assert!(
        value > 180.0 && value > others + 80.0,
        "{name} must show the emissive color, mean {mean:?}"
    );
}

#[test]
fn capture_puts_emissive_colours_on_the_cube_faces() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut scene = base_scene(
        // Emissive 2.4 at EV100 0 exposes the primaries at ~2: bright but inside AgX's
        // range. (At 24 they were 20× over middle grey; AgX deliberately desaturates
        // such highlights toward white, so the face colour test would measure the
        // tonemapper's path to white instead of the capture's face placement.)
        vec![
            solid([0.02, 0.02, 0.02], 1.0, [2.4, 0.0, 0.0]),
            solid([0.02, 0.02, 0.02], 1.0, [0.0, 2.4, 0.0]),
            solid([0.02, 0.02, 0.02], 1.0, [0.0, 0.0, 2.4]),
        ],
        vec![
            placed_box([0.05, 8.0, 8.0], [3.0, 0.0, 0.0], 0),
            placed_box([8.0, 0.05, 8.0], [0.0, 3.0, 0.0], 1),
            placed_box([8.0, 0.05, 8.0], [0.0, -3.0, 0.0], 2),
        ],
    );
    scene.eye = [0.0, 0.0, 0.0];
    scene.target = [1.0, 0.0, 0.0];
    scene.exposure_ev = 0.0;
    scene.ambient_lux = 0.0;
    scene.directional_shadows = false;
    scene.depth_prepass = false;
    scene.ibl = false;
    scene.directional = DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0);
    let quality = FrameQuality {
        gi: GiMode::Ambient,
        screen_space_gi: false,
        ssr: true,
        reflection_probes: true,
        ..FrameQuality::LAB
    };
    let mut renderer = FrameRenderer::new();
    let frame = scene.as_frame_scene_with_quality(quality);
    let probe = renderer
        .capture_reflection_probe(&device, &frame, 7, [0.0, 0.0, 0.0], [4.0, 4.0, 4.0])
        .expect("capture");
    assert_eq!(probe.id, 7);
    assert_eq!((probe.width, probe.height), (64, 32));
    assert_eq!(probe.equirect.len(), 64 * 32 * 4);
    let red = patch_mean(&probe.equirect, 64, 28, 12, 36, 20);
    let green = patch_mean(&probe.equirect, 64, 24, 0, 40, 2);
    let blue = patch_mean(&probe.equirect, 64, 24, 30, 40, 32);
    eprintln!("m15b probe means +X {red:?} +Y {green:?} -Y {blue:?}");
    assert_emissive("+X face", red, 0);
    assert_emissive("+Y face (world up cannot frame this look)", green, 1);
    assert_emissive("-Y face (world up cannot frame this look)", blue, 2);
}

/// Mean absolute RGB difference between two frames, in 8-bit levels.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let mut sum = 0.0f64;
    let mut n = 0.0f64;
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for c in 0..3 {
            sum += (f64::from(p[c]) - f64::from(q[c])).abs();
            n += 1.0;
        }
    }
    sum / n.max(1.0)
}

/// A point light is six atlas tiles (one per cube face), and the atlas cache names dirty
/// *lights*. A caster that moves under the second light must redraw that light's faces: the
/// warm-cache frame then matches a fresh renderer's full draw of the same scene. (Taking the
/// dirty light index as a tile index redrew a face of the first light and left the second
/// light's shadow where the caster had been.)
#[test]
fn a_mover_under_a_later_point_light_redraws_that_lights_faces() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let materials = vec![
        solid([0.65, 0.65, 0.62], 0.8, [0.0; 3]),
        solid([0.75, 0.35, 0.2], 0.6, [0.0; 3]),
    ];
    let scene_with = |mover: [f32; 3]| {
        let mut scene = base_scene(
            materials.clone(),
            vec![
                placed_box([6.0, 0.08, 6.0], [0.0, 0.0, 0.0], 0),
                placed_box([0.35, 0.7, 0.35], mover, 1),
            ],
        );
        // Only the local lights light the floor: no sun shadow, no ambient.
        scene.directional_shadows = false;
        scene.directional = DirectionalLightUniform::new([-0.45, -0.85, -0.25], [1.0, 1.0, 1.0], 0.0);
        scene.ambient_lux = 0.0;
        scene.exposure_ev = 6.0;
        scene.local_lights = vec![
            // Listed first and far away: its six tiles come before the near light's.
            LabLocalLight::Point(LabPointLight {
                position: [18.0, 2.0, 0.0],
                color: [0.6, 0.7, 1.0],
                intensity: 2_000.0,
                radius: 3.0,
                cast_shadows: true,
                source_radius: None,
            }),
            LabLocalLight::Point(LabPointLight {
                position: [0.0, 2.2, 0.2],
                color: [1.0, 0.9, 0.7],
                intensity: 8_000.0,
                radius: 6.0,
                cast_shadows: true,
                source_radius: None,
            }),
        ];
        scene
    };
    let quality = shadow_quality();
    let (from, to) = ([-0.6, 0.7, 0.2], [0.8, 0.7, -0.4]);
    let mut warm = FrameRenderer::new();
    render(&mut warm, &device, &scene_with(from), quality);
    let first = warm.shadow_cache_report().clone();
    let before = render(&mut warm, &device, &scene_with(from), quality);
    render(&mut warm, &device, &scene_with(to), quality);
    let report = warm.shadow_cache_report().clone();
    let cached = render(&mut warm, &device, &scene_with(to), quality);
    let mut cold = FrameRenderer::new();
    render(&mut cold, &device, &scene_with(to), quality);
    let fresh = render(&mut cold, &device, &scene_with(to), quality);
    // Two renderers that draw the same scene in full agree to this many levels (the floor).
    let mut other = FrameRenderer::new();
    render(&mut other, &device, &scene_with(from), quality);
    let floor = mean_abs_diff(&before, &render(&mut other, &device, &scene_with(from), quality));
    let moved = mean_abs_diff(&before, &fresh);
    let stale = mean_abs_diff(&cached, &fresh);
    eprintln!(
        "point faces: move changes {moved:.3} levels, cached vs fresh {stale:.3} (floor {floor:.3}); dirty lights {:?}, tiles drawn {}",
        report.dirty_atlas_tiles, report.atlas_tiles_drawn
    );
    if stale > 0.05 {
        dump("point_faces_cached", &cached);
        dump("point_faces_fresh", &fresh);
    }
    assert!(
        moved > 1.0,
        "the move must change the local-light shadow (control), {moved:.3} levels"
    );
    // The first frame draws all twelve tiles. The far light's sphere reaches neither the floor
    // nor the mover, so its six faces draw no caster, and each near face draws at most the two.
    assert_eq!(first.atlas_tiles_drawn, 12, "{first:?}");
    assert!(
        first.atlas_caster_draws <= 12,
        "only the near light's faces draw casters: {} caster draws over 12 tiles, {first:?}",
        first.atlas_caster_draws
    );
    assert_eq!(
        report.dirty_atlas_tiles,
        vec![1],
        "only the near light's sphere reaches the mover: {report:?}"
    );
    assert!(
        stale <= floor + 0.05,
        "the warm cache must match a full draw after the move: {stale:.3} levels apart (floor {floor:.3}, move {moved:.3})"
    );
}
