// SPDX-License-Identifier: MPL-2.0
//! The world-space radiance cache behind screen-space GI (`specs/79` § 12, ADR-2085).
//!
//! Off, a frame is the frame it always was. On, the budget holds, the cells fill and the cache rests
//! when the light does; a closed room stays dark at every tier; a warmed frame creates nothing.
//! `radiance_cache_alloc.rs` counts the steady frame's allocations.

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Vec3};
use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, DynamicGiTier, EyeCamera, EyeFrame, FrameQuality, FrameRenderer,
    FrameTarget, GiMode, LabMaterial, LabMesh, LabScene, PostSettings, RadianceCacheSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, box_mesh, read_rgba8,
};

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip radiance_cache: {err}");
            None
        }
    }
}

const W: u32 = 160;
const H: u32 = 96;

fn block(centre: [f32; 3], half: [f32; 3]) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// Probes and screen-space GI at native scale, single sample, no temporal resolve.
fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.msaa_samples = 1;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Probes;
    q.gi_probes = true;
    q.screen_space_gi = true;
    q.baked_gi = false;
    q
}

fn settings(tier: DynamicGiTier) -> DdgiSettings {
    let mut s = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    };
    tier.apply(&mut s);
    s.update_divisor = 1;
    s
}

fn scene(eye: [f32; 3], target: [f32; 3], meshes: Vec<LabMesh>) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([-0.53, -0.77, -0.35], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes,
    }
}

fn sealed_room(t: f32) -> Vec<LabMesh> {
    vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0]),
        block([3.0 + t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t]),
        block([-3.0 - t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t]),
        block([0.0, 1.5, 3.0 + t], [3.0, 1.5, t]),
        block([0.0, 1.5, -3.0 - t], [3.0, 1.5, t]),
        block([0.0, 3.0 + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t]),
    ]
}

/// A room with a tall opening in its +z wall, the sun coming in through it.
fn room_with_door(t: f32) -> Vec<LabMesh> {
    vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0]),
        block([3.0 + t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t]),
        block([-3.0 - t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t]),
        block([0.0, 1.5, -3.0 - t], [3.0, 1.5, t]),
        block([-1.9, 1.5, 3.0 + t], [1.1, 1.5, t]),
        block([1.9, 1.5, 3.0 + t], [1.1, 1.5, t]),
        block([0.0, 3.0 + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t]),
    ]
}

fn frame_pixels(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, frames: u32) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..frames {
        let frame = lab.as_frame_scene_with_quality(quality());
        last = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels;
    }
    last
}

fn linear_luma(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn renderer(tier: DynamicGiTier, cache: bool) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(settings(tier));
    r.set_ddgi_enabled(Some(true));
    r.set_radiance_cache(cache);
    r
}

/// Turning the cache off gives back the plain frame, to the byte: the switch leaves nothing behind.
#[test]
fn off_frames_match_a_renderer_that_never_had_it() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut plain = renderer(DynamicGiTier::Low, false);
    let plain_px = frame_pixels(&mut plain, &device, &lab, 16);
    assert!(!plain.radiance_cache_status().active);
    assert_eq!(
        plain.radiance_cache_status().gpu_bytes,
        0,
        "off creates no buffers"
    );
    assert_eq!(plain.radiance_cache_status().total_updated, 0);
    let mut toggled = renderer(DynamicGiTier::Low, true);
    let on_px = frame_pixels(&mut toggled, &device, &lab, 8);
    assert!(toggled.radiance_cache_status().active, "the gather read it");
    assert!(toggled.radiance_cache_status().gpu_bytes > 0);
    toggled.set_radiance_cache(false);
    assert_eq!(
        toggled.radiance_cache_status().gpu_bytes,
        0,
        "off frees the buffers"
    );
    let off_px = frame_pixels(&mut toggled, &device, &lab, 8);
    assert!(!toggled.radiance_cache_status().active);
    assert_eq!(
        plain_px, off_px,
        "after the switch the frame is the plain frame, byte for byte"
    );
    assert_ne!(plain_px, on_px, "the cached gather changes the image");
}

/// The budget holds on every frame, the cache fills, and it rests when the probes do.
#[test]
fn the_budget_holds_and_a_settled_view_comes_to_rest() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut r = renderer(DynamicGiTier::Low, true);
    let budget = 512;
    r.set_radiance_cache_settings(RadianceCacheSettings {
        budget_cells: budget,
        levels: 2,
        dims: [16, 8, 16],
        ..RadianceCacheSettings::default()
    });
    let cells = r.radiance_cache_settings().cells();
    let mut idle_at = None;
    let mut total = 0;
    for frame in 0..200u32 {
        frame_pixels(&mut r, &device, &lab, 1);
        let s = r.radiance_cache_status();
        assert!(s.active, "frame {frame}");
        assert!(s.updated_cells <= budget, "frame {frame}: {s:?}");
        total += u64::from(s.updated_cells);
        assert_eq!(s.total_updated, total, "frame {frame}: the counters agree");
        if s.idle {
            idle_at = Some(frame);
            break;
        }
    }
    let idle_at = idle_at.unwrap_or_else(|| panic!("never rests: {:?}", r.radiance_cache_status()));
    assert!(
        total >= cells,
        "every cell was written at least once: {total} of {cells}"
    );
    // Resting: nothing is rewritten, and the image holds.
    let held = frame_pixels(&mut r, &device, &lab, 1);
    let s = r.radiance_cache_status();
    assert_eq!(
        s.updated_cells, 0,
        "a rested cache rewrites nothing (idle at {idle_at}): {s:?}"
    );
    let again = frame_pixels(&mut r, &device, &lab, 1);
    assert_eq!(r.radiance_cache_status().updated_cells, 0);
    assert_eq!(held.len(), again.len());
    // A camera that moves wakes it: the scrolled cells go first, up to the budget, and the rest wait.
    let moved = scene([10.0, 1.6, -1.5], [10.0, 0.8, 1.5], room_with_door(0.1));
    frame_pixels(&mut r, &device, &moved, 1);
    let s = r.radiance_cache_status();
    assert_eq!(s.updated_cells, budget, "{s:?}");
    assert_eq!(s.scrolled_cells, budget, "{s:?}");
    assert!(s.pending_cells > 0, "{s:?}");
    assert!(!s.idle, "{s:?}");
}

/// `specs/27` § 2.2 with the cache on: the same bound as the plain frame on every surface of a sealed
/// room, at the Deck and Desktop High tiers.
#[test]
fn a_sealed_room_stays_dark_with_the_cache_on() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    sealed_room_bound(&device, false);
}

/// The same bound with the half-resolution gather and its upsample, the path of the 0.67 budget.
#[test]
fn a_sealed_room_stays_dark_with_the_half_resolution_gather() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    sealed_room_bound(&device, true);
}

fn sealed_room_bound(device: &RenderDevice, half: bool) {
    let renderer = |tier, cache| {
        let mut r = renderer(tier, cache);
        r.set_ssgi_half_res(Some(half));
        r
    };
    let eye = [0.0, 1.6, -1.5];
    let views = [
        ("floor", [0.0, 0.0, 1.5]),
        ("ceiling", [0.0, 3.0, 1.5]),
        ("wall", [3.0, 1.6, -1.5]),
        ("upper wall", [3.0, 2.8, -1.5]),
    ];
    for tier in [DynamicGiTier::Low, DynamicGiTier::High] {
        let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], sealed_room(0.1));
        let mut out = renderer(tier, true);
        let out_px = frame_pixels(&mut out, device, &outside, 40);
        let exterior = linear_luma(&out_px, 40, 50, 120, 90);
        assert!(exterior > 0.1, "the exterior ground must be lit ({exterior})");
        drop(out);
        let mut r = renderer(tier, true);
        for (i, (name, target)) in views.into_iter().enumerate() {
            let lab = scene(eye, target, sealed_room(0.1));
            let px = frame_pixels(&mut r, device, &lab, if i == 0 { 100 } else { 3 });
            assert!(r.radiance_cache_status().active);
            let share = linear_luma(&px, 40, 20, 120, 76) / exterior;
            eprintln!(
                "{tier:?} half {half} {name}: {:.4} % of the exterior ground",
                share * 100.0
            );
            assert!(
                share <= 0.005,
                "light leaks onto the {name} at {tier:?} (half {half}) with the cache on: {:.3} %",
                share * 100.0
            );
        }
    }
}

/// The control for the path tracer plates, where the gather's added light is judged against the tracer.
/// Ground and sky only, the probes right and nothing near to hit: the gather has nothing to add,
/// so what it adds here is the cost of clamping a noisy difference at zero.
#[test]
fn an_open_ground_hardly_changes() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene(
        [0.0, 1.6, -6.0],
        [0.0, 0.0, 2.0],
        vec![block([0.0, -0.25, 0.0], [120.0, 0.25, 120.0])],
    );
    let mean = |r: &mut FrameRenderer, frames: u32| {
        let frame = lab.as_frame_scene_with_quality(quality());
        for i in 0..frames {
            if i + 1 == frames {
                r.request_hdr_capture();
            }
            r.render_offscreen(&device, &frame)
                .unwrap_or_else(|err| panic!("frame: {err}"));
        }
        let hdr = r
            .take_hdr_capture(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the frame recorded its HDR copy"));
        // The lower half of the frame: ground.
        let rows = hdr.height as usize;
        let cols = hdr.width as usize;
        let ground = &hdr.rgb[(rows / 2) * cols..];
        ground
            .iter()
            .map(|c| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
            .sum::<f32>()
            / ground.len() as f32
    };
    let mut plain = renderer(DynamicGiTier::Low, false);
    let without = mean(&mut plain, 60);
    let mut cached = renderer(DynamicGiTier::Low, true);
    let with = mean(&mut cached, 60);
    let s = cached.radiance_cache_status();
    assert!(s.active, "{s:?}");
    let ratio = with / without;
    eprintln!("open ground: plain {without:.3}, cached {with:.3}, ratio {ratio:.5}");
    assert!(
        (0.999..=1.01).contains(&ratio),
        "open ground: plain {without}, cached {with}"
    );
}

/// A held view keeps gathering until the cache has filled: the composite it then reuses must not be
/// one read from a half-empty cache. Desktop High's static-screen reuse is on in this frame.
#[test]
fn a_held_view_does_not_freeze_a_half_filled_cache() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut r = renderer(DynamicGiTier::Low, true);
    r.set_radiance_cache_settings(RadianceCacheSettings {
        budget_cells: 256,
        levels: 1,
        dims: [16, 8, 16],
        ..RadianceCacheSettings::default()
    });
    let cells = r.radiance_cache_settings().cells();
    frame_pixels(&mut r, &device, &lab, 80);
    let s = r.radiance_cache_status();
    assert!(s.idle, "the cache fills under a held view: {s:?}");
    assert!(s.total_updated >= cells, "{s:?}");
    assert_eq!(s.updated_cells, 0, "at rest: {s:?}");
}

/// With the probes off the switch does nothing: the plain gather runs, no buffers, and a held view
/// still reuses its composite (the cache's fill must not hold the static-screen key open).
#[test]
fn without_probes_the_switch_does_nothing() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut q = quality();
    q.gi = GiMode::ScreenSpace;
    q.gi_probes = false;
    let draw = |r: &mut FrameRenderer| {
        let frame = lab.as_frame_scene_with_quality(q);
        r.render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    };
    let mut plain = FrameRenderer::new();
    plain.set_ddgi_enabled(Some(false));
    let mut asked = FrameRenderer::new();
    asked.set_ddgi_enabled(Some(false));
    asked.set_radiance_cache(true);
    let mut last = (Vec::new(), Vec::new());
    for _ in 0..6 {
        last = (draw(&mut plain), draw(&mut asked));
    }
    assert_eq!(last.0, last.1, "the plain frame, byte for byte");
    let s = asked.radiance_cache_status();
    assert!(s.enabled && !s.active, "{s:?}");
    assert_eq!((s.gpu_bytes, s.total_updated), (0, 0), "{s:?}");
    assert_eq!(
        plain.screen_pass_report().screen_cache_hit,
        asked.screen_pass_report().screen_cache_hit,
        "the held view behaves as without the switch"
    );
}

/// A warmed cached frame builds no pipeline, and a renderer with the cache off never builds one.
#[test]
fn a_warmed_frame_builds_nothing() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut off = renderer(DynamicGiTier::Low, false);
    frame_pixels(&mut off, &device, &lab, 2);
    assert_eq!(
        off.warm_pipeline_key_for_tests(&device, "radiance_cache"),
        0,
        "the warm builds nothing while the cache is off"
    );
    drop(off);
    let mut r = renderer(DynamicGiTier::Low, true);
    frame_pixels(&mut r, &device, &lab, 4);
    assert_eq!(r.warm_pipeline_key_for_tests(&device, "radiance_cache"), 2);
    let before = pipelines_created();
    frame_pixels(&mut r, &device, &lab, 8);
    assert_eq!(
        pipelines_created() - before,
        0,
        "steady cached frames build no pipeline"
    );
    // A camera move scrolls the grids: no pipeline, the same buffers.
    let bytes = r.radiance_cache_status().gpu_bytes;
    let moved = scene([1.0, 1.6, -1.5], [1.0, 0.8, 1.5], room_with_door(0.1));
    frame_pixels(&mut r, &device, &moved, 4);
    assert_eq!(pipelines_created() - before, 0, "a scroll builds no pipeline");
    assert_eq!(r.radiance_cache_status().gpu_bytes, bytes);
}

/// The linear HDR of the last of `frames` frames.
fn hdr_after(
    r: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    q: FrameQuality,
    frames: u32,
) -> Vec<[f32; 3]> {
    let frame = lab.as_frame_scene_with_quality(q);
    for i in 0..frames {
        if i + 1 == frames {
            r.request_hdr_capture();
        }
        r.render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    }
    r.take_hdr_capture(device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the frame recorded its HDR copy"))
        .rgb
}

fn mean_luma(plane: &[[f32; 3]]) -> f32 {
    plane
        .iter()
        .map(|c| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
        .sum::<f32>()
        / plane.len() as f32
}

/// The half-resolution gather gives about the light the full one gives, through the upsample: every
/// pixel stays finite and, with the default add-only result, at or above the frame it corrects. A
/// darkening result stays finite and non-negative there too.
#[test]
fn the_half_resolution_gather_adds_about_what_the_full_one_adds() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    const FRAMES: u32 = 80;
    let halved = |cache: bool| {
        let mut r = renderer(DynamicGiTier::Low, cache);
        r.set_ssgi_half_res(Some(true));
        r
    };
    // Without the static-screen cache: a held full-resolution frame keeps a composite from the frame it
    // first held, while the probes under the gather-free frame still converge, so frames of one age
    // compare only with it off.
    let mut q = quality();
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    let mut no_gather = q;
    no_gather.screen_space_gi = false;
    let base = hdr_after(
        &mut renderer(DynamicGiTier::Low, false),
        &device,
        &lab,
        no_gather,
        FRAMES,
    );
    let full = hdr_after(&mut renderer(DynamicGiTier::Low, true), &device, &lab, q, FRAMES);
    let plain_half = hdr_after(&mut halved(false), &device, &lab, q, FRAMES);
    let mut cached = halved(true);
    let half = hdr_after(&mut cached, &device, &lab, q, FRAMES);
    let s = cached.radiance_cache_status();
    assert!(s.active && s.idle, "{s:?}");
    assert_ne!(half, plain_half, "the half gather read the cache");
    let (b, f, h, p) = (
        mean_luma(&base),
        mean_luma(&full),
        mean_luma(&half),
        mean_luma(&plain_half),
    );
    eprintln!("door room: base {b:.3}, full {f:.3}, half {h:.3}, plain half {p:.3}");
    assert!(
        f > b && h > b,
        "the gather adds light: base {b}, full {f}, half {h}"
    );
    let ratio = (h - b) / (f - b);
    eprintln!("half adds {ratio:.3} of what the full gather adds");
    assert!(
        (0.7..=1.3).contains(&ratio),
        "half adds {ratio} of the full gather"
    );
    for (name, plane) in [("full", &full), ("half", &half)] {
        let mut lowest = f32::MAX;
        for (px, under) in plane.iter().zip(&base) {
            for (v, u) in px.iter().zip(under) {
                assert!(v.is_finite() && *v >= 0.0, "{name} gather pixel {px:?}");
                lowest = lowest.min(*v - *u);
            }
        }
        eprintln!("lowest {name} minus base: {lowest}");
        assert!(
            lowest >= -1e-2 * b,
            "{name} is add-only: {lowest} under a mean of {b}"
        );
    }

    cached.set_radiance_cache_settings(RadianceCacheSettings {
        darken_limit: 0.75,
        ..cached.radiance_cache_settings()
    });
    let signed = hdr_after(&mut cached, &device, &lab, q, 60);
    for px in &signed {
        assert!(
            px.iter().all(|v| v.is_finite() && *v >= 0.0),
            "signed half gather pixel {px:?}"
        );
    }
}

/// A held view at half resolution reuses its gather, but only once the gather was read from a cache at
/// rest; a move or the switch gathers again. Both ways in: a forced half gather at native scale, and the
/// 0.67 Desktop High budget under temporal AA, which is how a preset would reach it.
#[test]
fn a_held_half_resolution_view_reuses_a_gather_only_from_a_cache_at_rest() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let forced = quality();
    let mut budget = quality();
    budget.taau = true;
    budget.render_scale_permille = 670;
    budget.csm_resolution = 2048;
    assert!(budget.ssgi_half_res() && !forced.ssgi_half_res());
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    for (name, q, force) in [("forced", forced, Some(true)), ("budget", budget, None)] {
        let mut r = renderer(DynamicGiTier::Low, true);
        r.set_ssgi_half_res(force);
        r.set_radiance_cache_settings(RadianceCacheSettings {
            budget_cells: 256,
            levels: 1,
            dims: [16, 8, 16],
            ..RadianceCacheSettings::default()
        });
        let draw = |r: &mut FrameRenderer, lab: &LabScene| {
            let frame = lab.as_frame_scene_with_quality(q);
            r.render_offscreen(&device, &frame)
                .unwrap_or_else(|err| panic!("{name}: frame: {err}"));
            r.screen_pass_report()
        };
        let mut was_idle = false;
        let mut reused_at = None;
        for frame in 0..200u32 {
            let report = draw(&mut r, &lab);
            assert!(report.ssgi_composite, "{name} frame {frame}: {report:?}");
            let s = r.radiance_cache_status();
            assert!(s.active, "{name} frame {frame}: {s:?}");
            if !report.ssgi_gather {
                assert!(
                    was_idle,
                    "{name} frame {frame}: reused a gather of a filling cache: {s:?}"
                );
                reused_at = Some(frame);
                break;
            }
            was_idle = s.idle;
        }
        let reused_at =
            reused_at.unwrap_or_else(|| panic!("{name}: never reused: {:?}", r.radiance_cache_status()));
        eprintln!("{name}: the gather is reused from frame {reused_at}");
        let held = draw(&mut r, &lab);
        assert!(!held.ssgi_gather && held.ssgi_composite, "{name}: {held:?}");
        assert_eq!(r.radiance_cache_status().updated_cells, 0, "{name}");
        let before = pipelines_created();
        // Far enough that the cells it brings in take more than one frame's budget: it gathers until they
        // are in, and only a gather read from a cache at rest may be held.
        let moved = scene([3.0, 1.6, -0.5], [3.0, 0.8, 2.5], room_with_door(0.1));
        assert!(draw(&mut r, &moved).ssgi_gather, "{name}: a move gathers again");
        let mut was_idle = r.radiance_cache_status().idle;
        assert!(!was_idle, "{name}: the move outran the budget");
        let mut held_again = false;
        for frame in 0..200u32 {
            let report = draw(&mut r, &moved);
            assert!(
                report.ssgi_gather || was_idle,
                "{name} frame {frame} after the move: reused a gather of a filling cache"
            );
            was_idle = r.radiance_cache_status().idle;
            held_again = !report.ssgi_gather;
            if held_again {
                break;
            }
        }
        assert!(held_again, "{name}: it comes to rest again");
        assert_eq!(
            pipelines_created() - before,
            0,
            "{name}: a move builds no pipeline"
        );
        r.set_radiance_cache(false);
        assert!(
            draw(&mut r, &moved).ssgi_gather,
            "{name}: the switch gathers again"
        );
    }
}

/// The half-resolution path warms with the cache: no pipeline on a warmed frame.
#[test]
fn a_warmed_half_resolution_frame_builds_nothing() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut r = renderer(DynamicGiTier::Low, true);
    r.set_ssgi_half_res(Some(true));
    frame_pixels(&mut r, &device, &lab, 4);
    assert_eq!(r.warm_pipeline_key_for_tests(&device, "radiance_cache"), 2);
    let _built = r.warm_pipeline_key_for_tests(&device, "ssgi_half_res");
    let before = pipelines_created();
    frame_pixels(&mut r, &device, &lab, 8);
    assert_eq!(pipelines_created() - before, 0);
}

/// With the probes on and the gather off the cache has nothing to feed: no buffers, and the frame
/// and its screen cache are the plain renderer's.
#[test]
fn probes_without_the_gather_leave_the_frame_alone() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let mut q = quality();
    q.screen_space_gi = false;
    let draw = |r: &mut FrameRenderer| {
        let frame = lab.as_frame_scene_with_quality(q);
        let px = r
            .render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels;
        (px, r.screen_pass_report())
    };
    let mut plain = renderer(DynamicGiTier::Low, false);
    let mut asked = renderer(DynamicGiTier::Low, true);
    for frame in 0..40 {
        let (a, ar) = draw(&mut plain);
        let (b, br) = draw(&mut asked);
        assert_eq!(a, b, "frame {frame}: the plain frame, byte for byte");
        assert_eq!(ar.screen_cache_hit, br.screen_cache_hit, "frame {frame}");
    }
    let s = asked.radiance_cache_status();
    assert!(s.enabled && !s.active, "{s:?}");
    assert_eq!((s.gpu_bytes, s.total_updated), (0, 0), "{s:?}");
}

/// An XR eye is a texture view: it takes the plain gather, in both eyes alike (an effect in one eye
/// reads as a fault in a headset), its image is the image without the cache, and the main view's
/// status stands.
#[test]
fn an_eye_takes_the_plain_gather_and_leaves_the_status_alone() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
    let lab = scene([0.0, 1.6, -1.5], [0.0, 0.8, 1.5], room_with_door(0.1));
    let view = look_at_mat4(Vec3::new(-0.03, 1.6, -1.5), Vec3::new(-0.03, 0.8, 1.5), Vec3::Y);
    let projection = perspective(1.0, W as f32 / H as f32, 0.05, 400.0);
    let camera = EyeCamera {
        view,
        projection,
        cull: projection * view,
    };
    let eye_pixels = |r: &mut FrameRenderer, eye: usize, q: FrameQuality| {
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test eye"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
        r.render_eye(
            &device,
            EyeFrame {
                eye,
                camera,
                scene: lab.as_frame_scene_with_quality(q),
                target: FrameTarget::full(&target, FORMAT, W, H),
                hidden_area: None,
                depth_target: None,
            },
        )
        .unwrap_or_else(|err| panic!("eye: {err}"));
        read_rgba8(&device.device, &device.queue, &texture, W, H).unwrap_or_else(|err| panic!("read: {err}"))
    };
    let mut plain = renderer(DynamicGiTier::Low, false);
    frame_pixels(&mut plain, &device, &lab, 12);
    let plain_eye = eye_pixels(&mut plain, 0, quality());
    let mut q_no_gather = quality();
    q_no_gather.screen_space_gi = false;
    let mut no_gather = renderer(DynamicGiTier::Low, false);
    frame_pixels(&mut no_gather, &device, &lab, 12);
    let no_gather_eye = eye_pixels(&mut no_gather, 0, q_no_gather);
    assert_ne!(plain_eye, no_gather_eye, "the eye ran a gather");

    let mut cached = renderer(DynamicGiTier::Low, true);
    frame_pixels(&mut cached, &device, &lab, 12);
    let status = cached.radiance_cache_status();
    assert!(status.active && status.gpu_bytes > 0, "{status:?}");
    let cached_eye = eye_pixels(&mut cached, 0, quality());
    assert_eq!(
        plain_eye, cached_eye,
        "the eye's image is the image without the cache"
    );
    let right = eye_pixels(&mut cached, 1, quality());
    assert_eq!(
        cached.radiance_cache_status(),
        status,
        "the main view's status stands"
    );
    let mut plain_right = renderer(DynamicGiTier::Low, false);
    frame_pixels(&mut plain_right, &device, &lab, 12);
    assert_eq!(eye_pixels(&mut plain_right, 1, quality()), right);

    // An XR session has no main view: nothing is created.
    let mut xr = renderer(DynamicGiTier::Low, true);
    for _ in 0..3 {
        xr.begin_stereo_frame();
        eye_pixels(&mut xr, 0, quality());
        eye_pixels(&mut xr, 1, quality());
        xr.end_stereo_frame();
    }
    let s = xr.radiance_cache_status();
    assert!(s.enabled && !s.active, "{s:?}");
    assert_eq!((s.gpu_bytes, s.total_updated), (0, 0), "{s:?}");
}

/// The cached gather's cost by construction (ADR-2605), counted on the GPU
/// (`RadianceCacheSettings::count_work`): the economy gather marches half the rays, each at most six
/// steps, and a ray that reaches the sky stops, so it makes at most 24 depth taps a pixel where the
/// default makes 64. The same frames, the same pixels gather; the depth steps fall below 3/8 of the
/// default's, and the sky early-out alone takes steps off a view with sky in it.
#[test]
fn the_economy_gather_does_less_work_by_count() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    // Ground, two blocks and the sky above the horizon.
    let lab = scene(
        [0.0, 1.6, -6.0],
        [0.0, 1.2, 4.0],
        vec![
            block([0.0, -0.25, 0.0], [120.0, 0.25, 120.0]),
            block([-2.5, 1.0, 2.0], [1.0, 1.0, 1.0]),
            block([2.0, 1.5, 4.0], [1.2, 1.5, 0.8]),
        ],
    );
    let count = |gather: RadianceCacheSettings| {
        let mut r = renderer(DynamicGiTier::Low, true);
        r.set_radiance_cache_settings(RadianceCacheSettings {
            count_work: true,
            ..gather
        });
        // The count comes back a few frames late; 12 frames stay short of the cache's first full
        // pass (49 152 cells at 2048 a frame), so every one of them gathers.
        frame_pixels(&mut r, &device, &lab, 12);
        let s = r.radiance_cache_status();
        assert!(s.active, "{s:?}");
        s.work.unwrap_or_else(|| panic!("a count came back: {s:?}"))
    };
    let full = count(RadianceCacheSettings::default());
    let economy = count(RadianceCacheSettings::economy());
    let no_sky_stop = count(RadianceCacheSettings {
        sky_early_out: false,
        ..RadianceCacheSettings::economy()
    });
    eprintln!(
        "gather work: full {full:?}, economy {economy:?}, economy without the sky stop {no_sky_stop:?}"
    );
    assert!(full.pixels > 200, "the gather ran over the frame: {full:?}");
    assert_eq!(economy.pixels, full.pixels, "the same pixels gather");
    assert_eq!(full.rays, full.pixels * 8);
    assert_eq!(economy.rays, economy.pixels * 4);
    assert!(full.steps <= full.rays * 8 && economy.steps <= economy.rays * 6);
    assert!(
        u64::from(economy.steps) * 8 <= u64::from(full.steps) * 3,
        "economy {economy:?} against full {full:?}"
    );
    assert!(
        economy.steps < no_sky_stop.steps,
        "the sky stop saves steps where the sky is in view: {economy:?} vs {no_sky_stop:?}"
    );
}

/// The reduced-resolution trace (ADR-2605): `FrameRenderer::set_ssgi_half_res` halves the gather on any
/// tier, so a quarter of the pixels march, and the economy gather's cut multiplies with it. Counted on
/// the GPU, same 12 frames as the test above. The gather's resolution stays the frame's to choose; a
/// scale below half is not built (the report's lines for the frame files).
#[test]
fn the_half_resolution_gather_counts_a_quarter_of_the_pixels() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene(
        [0.0, 1.6, -6.0],
        [0.0, 1.2, 4.0],
        vec![
            block([0.0, -0.25, 0.0], [120.0, 0.25, 120.0]),
            block([-2.5, 1.0, 2.0], [1.0, 1.0, 1.0]),
            block([2.0, 1.5, 4.0], [1.2, 1.5, 0.8]),
        ],
    );
    let count = |gather: RadianceCacheSettings, half: bool| {
        let mut r = renderer(DynamicGiTier::Low, true);
        r.set_ssgi_half_res(Some(half));
        r.set_radiance_cache_settings(RadianceCacheSettings {
            count_work: true,
            ..gather
        });
        frame_pixels(&mut r, &device, &lab, 12);
        let s = r.radiance_cache_status();
        assert!(s.active, "half {half}: {s:?}");
        s.work
            .unwrap_or_else(|| panic!("half {half}: a count came back: {s:?}"))
    };
    let full = count(RadianceCacheSettings::default(), false);
    let half = count(RadianceCacheSettings::default(), true);
    let half_economy = count(RadianceCacheSettings::economy(), true);
    eprintln!("gather work: full {full:?}, half {half:?}, half with the economy gather {half_economy:?}");
    assert!(full.pixels > 200, "the gather ran over the frame: {full:?}");
    assert!(
        half.pixels * 3 <= full.pixels && half.pixels * 5 >= full.pixels,
        "half {half:?} against full {full:?}"
    );
    assert_eq!(half.rays, half.pixels * 8);
    assert_eq!(half_economy.pixels, half.pixels, "the same pixels gather");
    assert!(
        u64::from(half_economy.steps) * 8 <= u64::from(full.steps),
        "half with the economy gather {half_economy:?} against full {full:?}"
    );
}

/// `RadianceCacheStatus::work` is a count of a frame that gathered, or nothing: the first read-back
/// must not report the zeros of a counter nothing has written, and switching the cache off leaves no
/// count behind (ADR-2605).
#[test]
fn the_work_count_is_of_a_frame_that_gathered() {
    let _gpu = serial();
    let Some(device) = device() else {
        return;
    };
    let lab = scene(
        [0.0, 1.6, -6.0],
        [0.0, 1.2, 4.0],
        vec![block([0.0, -0.25, 0.0], [120.0, 0.25, 120.0])],
    );
    let mut r = renderer(DynamicGiTier::Low, true);
    r.set_radiance_cache_settings(RadianceCacheSettings {
        count_work: true,
        ..RadianceCacheSettings::default()
    });
    let mut counted = 0;
    for frame in 0..12 {
        frame_pixels(&mut r, &device, &lab, 1);
        if let Some(work) = r.radiance_cache_status().work {
            assert!(
                work.pixels > 0,
                "frame {frame} reports a count of nothing: {work:?}"
            );
            counted += 1;
        }
    }
    assert!(counted > 0, "a count came back");
    r.set_radiance_cache(false);
    let s = r.radiance_cache_status();
    assert!(s.work.is_none(), "switched off, the count stays: {s:?}");
}
