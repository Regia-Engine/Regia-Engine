// SPDX-License-Identifier: MPL-2.0
//! Object motion on ReSTIR GI reservoirs (ADR-4001).
//!
//! Camera reprojection rejects a receiver that moved. It does not reject a still floor whose
//! stored hit sat on an object that has since left: the ray to that empty point stays unoccluded.
//! These plates check the confirm ray along the stored hit normal and the original material
//! point under the current instance transform, including tangential overlapping motion.
//!
//! The sphere translates by `SPHERE_TRANSLATION_M` (1.0 m) between frame 0 (the last still frame)
//! and frame 1. Indirect luminance is the linear HDR plate (cd/m², before exposure) with directional
//! shadows off and symmetric vertical 500-lux sunlight, so the floor patches share the same
//! direct sun and the gap is GI. The
//! denoiser takes the new sample in full (`alpha_color` 1, no à-trous) so a rejected reservoir
//! shows on frame 1. Spatial radius stays inside one patch and does not bridge the two sample
//! points. No wall-clock assert and no golden image.
//!
//! Run explicitly: `cargo test -p regia_pt --test restir_gi_object_motion -- --ignored
//! --test-threads=1 --nocapture` on lavapipe with ray queries and hit-vertex return.

use std::sync::{Mutex, OnceLock};

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::{Vec3, Vec4};
use regia_render::restir_gi::{HIT_MOTION_METRES, RestirGiSettings};
use regia_render::view_math::look_at_view;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, box_mesh,
};

const W: u32 = 96;
const H: u32 = 64;
/// Sphere center translation between frame 0 and frame 1, meters. Well above `HIT_MOTION_METRES`.
const SPHERE_TRANSLATION_M: f32 = 1.0;
const SPHERE_RADIUS: f32 = 0.40;
const START_CENTER: [f32; 3] = [0.0, 0.55, -0.15];
/// Floor point the sphere leaves beside. Symmetric with [`ARRIVAL`] about the start center.
const VACATED: [f32; 3] = [-0.50, 0.0, 0.40];
/// Floor point beside the sphere after the translation.
const ARRIVAL: [f32; 3] = [0.50, 0.0, 0.40];
const EYE: [f32; 3] = [0.0, 1.85, 2.15];
const TARGET: [f32; 3] = [0.0, 0.2, -0.1];
const FOV_Y: f32 = 1.0;
const NEAR: f32 = 0.05;
const FAR: f32 = 40.0;
const WARMUP_FRAMES: u32 = 40;
/// Fixed frames at the new pose, starting with frame 1. One cosine ray is noisy; the mean is the
/// documented GI sample.
const MOVE_FRAMES: u32 = 4;
/// Work readback samples frames; sustained fixed-step motion makes the counter observable
/// without depending on which frame the asynchronous snapshot copied.
const COUNTER_MOTION_FRAMES: u32 = 8;
/// Half-width of each floor window, pixels. The windows are farther apart than the spatial radius.
const PATCH_RADIUS: i32 = 4;
/// Spatial taps stay inside a patch (~8 px) and do not reach the other sample (~26 px away).
const SPATIAL_RADIUS_PX: f32 = 8.0;
/// Frame 0: the two patches must already agree, or the later gap is not the move.
const WARM_MATCH_CD: f32 = 8.0;
/// Frame 1 window: indirect Rec.709 gap, cd/m², between the new position and the space left behind.
const GI_EPSILON_CD: f32 = 1.0;
const CAMERA_STEP_M: f32 = 0.12;
/// `motion_rejections / (motion_rejections + temporal_reused)` after a still-scene camera step.
const CAMERA_REJECT_FRACTION: f32 = 0.25;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| {
        let device = RenderDevice::headless_with_ray_queries().expect("ray-query headless device");
        let features = device.device.features();
        assert!(features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY));
        assert!(features.contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN));
        device
    })
}

fn with_device<T>(body: impl FnOnce(&RenderDevice) -> T) -> T {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
    body(device())
}

fn translation(offset: [f32; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [offset[0], offset[1], offset[2], 1.0],
    ]
}

fn lambert(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        metallic: 0.0,
        roughness: 0.95,
        ..LabMaterial::default()
    }
}

fn emissive_sphere_material() -> LabMaterial {
    LabMaterial {
        base_color: [0.85, 0.35, 0.12, 1.0],
        metallic: 0.0,
        roughness: 0.9,
        emissive: [1.0, 0.22, 0.05],
        emissive_intensity: 600.0,
        ..LabMaterial::default()
    }
}

fn floor_mesh() -> LabMesh {
    let (vertices, indices) = box_mesh([4.0, 0.08, 4.0]);
    LabMesh {
        vertices,
        indices,
        material: 0,
        model: translation([0.0, -0.08, 0.0]),
        ..LabMesh::unskinned()
    }
}

fn sphere_mesh_at(center: [f32; 3]) -> LabMesh {
    let (vertices, indices) = regia_render::primitives::models::sphere_mesh(SPHERE_RADIUS, 10, 16);
    LabMesh {
        vertices,
        indices,
        material: 1,
        model: translation(center),
        ..LabMesh::unskinned()
    }
}

fn scene(sphere_center: [f32; 3], eye: [f32; 3]) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 0.0;
    LabScene {
        width: W,
        height: H,
        eye,
        target: TARGET,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 180.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 0.97, 0.92], 500.0),
        local_lights: Vec::new(),
        materials: vec![lambert([0.62, 0.60, 0.56]), emissive_sphere_material()],
        meshes: vec![floor_mesh(), sphere_mesh_at(sphere_center)],
    }
}

fn restir_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Probes;
    q.screen_space_gi = false;
    q.gi_probes = true;
    q.baked_gi = false;
    q.paged_sun_shadows = false;
    q.ssao = SsaoMode::Off;
    q.restir_gi = true;
    q
}

fn probe_settings() -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims: [10, 6, 10],
        spacing: 0.75,
        spacing_ratio: 2.0,
        update_divisor: 1,
        hysteresis: 0.55,
        min_proxy_radius: 0.2,
        bake_on_caller: true,
        screen_max: [W, H],
        ..DdgiSettings::default()
    }
}

fn motion_settings(spatial_taps: u32) -> RestirGiSettings {
    RestirGiSettings {
        temporal: true,
        spatial_taps,
        spatial_radius_px: SPATIAL_RADIUS_PX,
        validation_interval: 0,
        denoise: regia_render::gi_denoise::GiDenoiseSettings {
            alpha_color: 1.0,
            alpha_moments: 1.0,
            atrous_iterations: 0,
            antilag_sigma: 0.0,
            ..regia_render::gi_denoise::GiDenoiseSettings::default()
        },
        ..RestirGiSettings::default()
    }
}

fn project(eye: [f32; 3], world: [f32; 3]) -> (f32, f32) {
    let view = look_at_view(Vec3::from_array(eye), Vec3::from_array(TARGET), Vec3::Y);
    let proj = perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    let clip = proj * view * Vec4::new(world[0], world[1], world[2], 1.0);
    assert!(clip.w > 1.0e-4, "sample {world:?} is behind the camera");
    let ndc_x = clip.x / clip.w;
    let ndc_y = clip.y / clip.w;
    let px = (ndc_x + 1.0) * 0.5 * W as f32;
    let py = (1.0 - ndc_y) * 0.5 * H as f32;
    assert!(
        (0.0..W as f32).contains(&px) && (0.0..H as f32).contains(&py),
        "sample {world:?} projects to ({px}, {py}) outside {W}x{H}"
    );
    (px, py)
}

fn rec709(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn patch_luma(rgb: &[[f32; 3]], px: f32, py: f32) -> f32 {
    // Projection returns pixel-edge coordinates; integer texels are centered at index + 0.5.
    let cx = (px - 0.5).round() as i32;
    let cy = (py - 0.5).round() as i32;
    let mut sum = 0.0f32;
    let mut count = 0u32;
    for y in (cy - PATCH_RADIUS)..=(cy + PATCH_RADIUS) {
        for x in (cx - PATCH_RADIUS)..=(cx + PATCH_RADIUS) {
            if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
                continue;
            }
            let i = (y as u32 * W + x as u32) as usize;
            sum += rec709(rgb[i]);
            count += 1;
        }
    }
    assert!(count >= 9, "patch at ({px}, {py}) sampled {count} pixels");
    sum / count as f32
}

fn render_frame(device: &RenderDevice, renderer: &mut FrameRenderer, lab: &LabScene) -> Vec<[f32; 3]> {
    let frame = lab.as_frame_scene_with_quality(restir_quality());
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &frame).expect("frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (W, H));
    hdr.rgb
}

fn prepare(renderer: &mut FrameRenderer, spatial_taps: u32) {
    renderer.set_ddgi_settings(probe_settings());
    renderer.set_ddgi_enabled(Some(true));
    renderer
        .set_restir_gi_settings(motion_settings(spatial_taps))
        .expect("settings");
}

#[test]
fn symmetric_world_patches_sample_mirrored_pixel_centers() {
    // A synthetic image symmetric about the camera must give equal symmetric floor windows.
    // Rounding projected pixel edges directly shifts both windows right by half a pixel.
    let rgb: Vec<[f32; 3]> = (0..W * H)
        .map(|index| {
            let x = (index % W) as f32 + 0.5 - W as f32 * 0.5;
            [x * x; 3]
        })
        .collect();
    let (ax, ay) = project(EYE, ARRIVAL);
    let (vx, vy) = project(EYE, VACATED);
    let arrival = patch_luma(&rgb, ax, ay);
    let vacated = patch_luma(&rgb, vx, vy);
    assert!(
        (arrival - vacated).abs() <= 1.0e-3,
        "symmetric world points sampled asymmetric windows: {arrival} vs {vacated}"
    );
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn moving_sphere_gi_does_not_match_the_space_it_left() {
    const { assert!(SPHERE_TRANSLATION_M > HIT_MOTION_METRES * 4.0) };
    with_device(|device| {
        let mut renderer = FrameRenderer::new();
        prepare(&mut renderer, 6);
        let cuts = renderer.camera_cut_count();
        let mut frame0 = Vec::new();
        for index in 0..WARMUP_FRAMES {
            let rgb = render_frame(device, &mut renderer, &scene(START_CENTER, EYE));
            if index + 1 == WARMUP_FRAMES {
                frame0 = rgb;
            }
        }
        let (ax, ay) = project(EYE, ARRIVAL);
        let (vx, vy) = project(EYE, VACATED);
        let warm_arrival = patch_luma(&frame0, ax, ay);
        let warm_vacated = patch_luma(&frame0, vx, vy);
        eprintln!(
            "frame 0 (sphere at {START_CENTER:?}): arrival ({ax:.1},{ay:.1}) {warm_arrival:.3} cd/m², vacated ({vx:.1},{vy:.1}) {warm_vacated:.3} cd/m²"
        );
        assert!(
            warm_arrival.is_finite() && warm_vacated.is_finite() && warm_arrival > 1.0 && warm_vacated > 1.0,
            "floor patches are not lit"
        );
        assert!(
            (warm_arrival - warm_vacated).abs() <= WARM_MATCH_CD,
            "frame 0 patches differ by {} cd/m² before the sphere moves (limit {WARM_MATCH_CD})",
            (warm_arrival - warm_vacated).abs()
        );

        let before_motion = renderer.restir_gi_report().total_work.motion_rejections;
        let end = [
            START_CENTER[0] + SPHERE_TRANSLATION_M,
            START_CENTER[1],
            START_CENTER[2],
        ];
        let mut arrival_sum = 0.0f32;
        let mut vacated_sum = 0.0f32;
        let mut frame1_gap = 0.0f32;
        for step in 0..MOVE_FRAMES {
            let rgb = render_frame(device, &mut renderer, &scene(end, EYE));
            let arrival = patch_luma(&rgb, ax, ay);
            let vacated = patch_luma(&rgb, vx, vy);
            if step == 0 {
                frame1_gap = arrival - vacated;
                eprintln!("frame 1 (sphere at {end:?}): arrival {arrival:.3}, vacated {vacated:.3}");
            }
            arrival_sum += arrival;
            vacated_sum += vacated;
        }
        let arrival = arrival_sum / MOVE_FRAMES as f32;
        let vacated = vacated_sum / MOVE_FRAMES as f32;
        let gap = arrival - vacated;
        eprintln!(
            "frames 1..={MOVE_FRAMES} mean: arrival {arrival:.3}, vacated {vacated:.3}, gap {gap:.3} (epsilon {GI_EPSILON_CD})"
        );
        assert_eq!(
            renderer.camera_cut_count(),
            cuts,
            "the sphere move was treated as a camera cut"
        );
        assert!(
            frame1_gap.is_finite() && gap.is_finite() && frame1_gap > GI_EPSILON_CD && gap > GI_EPSILON_CD,
            "GI at the sphere's new position ({arrival:.3}) matches the empty space it left ({vacated:.3}) within {GI_EPSILON_CD} cd/m²; frame 1 gap {frame1_gap:.3}"
        );
        // Keep the single-move image check above independent of sampled work telemetry. A
        // one-frame rejection can fall between readbacks, so exercise the counter over a fixed
        // sustained-motion window after recording the image evidence.
        for step in 0..COUNTER_MOTION_FRAMES {
            let center = if step.is_multiple_of(2) { START_CENTER } else { end };
            render_frame(device, &mut renderer, &scene(center, EYE));
        }
        let report = renderer.restir_gi_report();
        let rejected = report.total_work.motion_rejections.saturating_sub(before_motion);
        eprintln!("moving-sphere motion rejections: {rejected}");
        assert!(
            rejected > 0,
            "the moving sphere rejected no opaque history samples"
        );
        assert!(report.active, "ReSTIR GI did not run: {:?}", report.fallback);
    });
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn still_scene_camera_step_keeps_today_camera_rejection() {
    with_device(|device| {
        let mut renderer = FrameRenderer::new();
        prepare(&mut renderer, 0);
        let cuts = renderer.camera_cut_count();
        for _ in 0..24 {
            render_frame(device, &mut renderer, &scene(START_CENTER, EYE));
        }
        let (px, py) = project(EYE, [0.0, 0.0, 0.15]);
        let before = render_frame(device, &mut renderer, &scene(START_CENTER, EYE));
        let before_luma = patch_luma(&before, px, py);
        let stepped = [EYE[0] + CAMERA_STEP_M, EYE[1], EYE[2]];
        const { assert!(CAMERA_STEP_M < 50.0, "the step must stay under the eye-jump cut") };
        for _ in 0..8 {
            render_frame(device, &mut renderer, &scene(START_CENTER, stepped));
        }
        let settled = renderer.restir_gi_report().total_work;
        for _ in 0..16 {
            render_frame(device, &mut renderer, &scene(START_CENTER, stepped));
        }
        let later = renderer.restir_gi_report().total_work;
        let (qx, qy) = project(stepped, [0.0, 0.0, 0.15]);
        let after = render_frame(device, &mut renderer, &scene(START_CENTER, stepped));
        let after_luma = patch_luma(&after, qx, qy);
        let motion = later.motion_rejections.saturating_sub(settled.motion_rejections);
        let temporal = later.temporal_reused.saturating_sub(settled.temporal_reused);
        let denom = motion.saturating_add(temporal);
        let fraction = if denom == 0 {
            1.0
        } else {
            motion as f32 / denom as f32
        };
        eprintln!(
            "camera step {CAMERA_STEP_M} m: motion_rejections {motion}, temporal_reused {temporal}, fraction {fraction:.3}, luma {before_luma:.3} -> {after_luma:.3}"
        );
        assert_eq!(
            renderer.camera_cut_count(),
            cuts,
            "a 0.12 m eye step was a camera cut"
        );
        assert!(
            temporal > 100,
            "the hold window recorded no temporal reuse ({temporal})"
        );
        assert!(
            fraction <= CAMERA_REJECT_FRACTION,
            "object-motion rejection of a still scene was {fraction:.3} (limit {CAMERA_REJECT_FRACTION})"
        );
        let scale = before_luma.abs().max(after_luma.abs()).max(1.0);
        assert!(
            (before_luma - after_luma).abs() <= 0.5 * scale,
            "still-scene GI at one floor point moved from {before_luma:.3} to {after_luma:.3}"
        );
    });
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn tangential_translation_rejects_a_material_point_on_an_overlapping_surface() {
    // The slab covers every path within the 40 m ray range before and after a 0.25 m X move.
    // Its bottom plane stays at the same height: a surface-occupancy-only confirm accepts it.
    const SLIDE_M: f32 = 0.25;
    with_device(|device| {
        let make_scene = |slide| {
            let mut lab = scene(START_CENTER, EYE);
            let (vertices, indices) = box_mesh([100.0, 0.1, 100.0]);
            lab.meshes[1] = LabMesh {
                vertices,
                indices,
                material: 1,
                model: translation([slide, 3.5, 0.0]),
                ..LabMesh::unskinned()
            };
            lab
        };
        let mut renderer = FrameRenderer::new();
        prepare(&mut renderer, 0);
        for _ in 0..24 {
            render_frame(device, &mut renderer, &make_scene(0.0));
        }
        let before = renderer.restir_gi_report();
        assert!(before.active && before.total_work.temporal_reused > 100);
        let cuts = renderer.camera_cut_count();
        // The work surface samples completed frames; move on every fixed step so its snapshot
        // cannot skip the only motion event. Even after eight steps the slab covers every hit.
        for step in 1..=COUNTER_MOTION_FRAMES {
            render_frame(device, &mut renderer, &make_scene(SLIDE_M * step as f32));
        }
        let after = renderer.restir_gi_report();
        let rejected = after
            .total_work
            .motion_rejections
            .saturating_sub(before.total_work.motion_rejections);
        eprintln!("overlapping slab slide {SLIDE_M} m: {rejected} material-point rejections");
        assert_eq!(renderer.camera_cut_count(), cuts);
        assert!(
            rejected > 100,
            "an overlapping plane slid {SLIDE_M} m but only {rejected} histories were rejected"
        );
    });
}
