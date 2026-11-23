// SPDX-License-Identifier: MPL-2.0
//! The outdoor composite's aerial perspective inside an `InteriorVolume` (ADR-4757): the air under
//! a roof takes no in-scatter from the sky, the light from beyond still crosses it, and the change
//! is continuous across a door, so walking out shows no pop. No golden: every check compares two
//! frames of the same renderer setup, one with the interior box and one without, so the difference
//! is exactly the in-scatter the composite took out (the transmittance is the same in both).
//!
//! - `a_box_no_ray_crosses_changes_no_pixel`: outdoors, with a box behind the camera, the frame is
//!   bit-identical to the frame without one (the outdoor path is unchanged).
//! - `the_air_in_a_closed_room_scatters_no_sky`: in a sealed room the frame without the box
//!   is the aerial's in-scatter alone; with the box it is black.
//!   With `FrameRenderer::set_aerial_interiors(false)` the box is ignored: bit-identical to no box.
//! - `the_eye_in_a_fifth_box_takes_its_air_out` (ADR-7975): five boxes, the eye in the one listed
//!   fifth: the composite binds the four nearest the eye, so the frame is the room alone's.
//! - `walking_out_of_a_door_has_no_pop`: looking out of a door at a wall 36 m away while the eye
//!   walks out, the in-scatter taken out follows the eye's stretch of air under the roof, a straight
//!   line to zero at the door, and from there the frame is the open air's bit for bit.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, InteriorVolume, LabMaterial, LabMesh, LabScene,
    LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 96;
const H: u32 = 64;
/// Frames per still (no technique here keeps a history; the second is read back).
const STILL_FRAMES: u32 = 2;
/// The room's air: x and z in [-4, 4], y in [0, 4]. Its +x wall holds the door.
const ROOM_CENTER: [f32; 3] = [0.0, 2.0, 0.0];
const ROOM_HALF: [f32; 3] = [4.0, 2.0, 4.0];
/// The door in the +x wall: z in [-1, 1], y in [0, 2.5].
const DOOR_HALF_Z: f32 = 1.0;
const DOOR_TOP: f32 = 2.5;
/// The wall the door looks out at.
const FAR_WALL_X: f32 = 40.0;

fn quad(corners: [[f32; 3]; 4], material: usize) -> LabMesh {
    let [a, b, c, _] = corners.map(regia_core::glam::Vec3::from_array);
    let normal = (b - a).cross(c - a).normalize_or_zero().to_array();
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// An axis-aligned rectangle on the plane `axis = at`, spanning `[a0, a1] × [b0, b1]` on the
/// other two axes in order.
fn rect(axis: usize, at: f32, a: (f32, f32), b: (f32, f32), material: usize) -> LabMesh {
    let p = |u: f32, v: f32| {
        let mut out = [0.0; 3];
        let (ia, ib) = match axis {
            0 => (1, 2),
            1 => (2, 0),
            _ => (0, 1),
        };
        out[axis] = at;
        out[ia] = u;
        out[ib] = v;
        out
    };
    quad([p(a.0, b.0), p(a.1, b.0), p(a.1, b.1), p(a.0, b.1)], material)
}

/// The room's six faces (the +x wall as a frame around the door when `door`), the ground outside
/// and the far wall. Material 0 is a mid gray, material 1 black; nothing is lit (see `scene`).
fn meshes(door: bool) -> Vec<LabMesh> {
    let [hx, hy, hz] = ROOM_HALF;
    let (y0, y1) = (ROOM_CENTER[1] - hy, ROOM_CENTER[1] + hy);
    let mut m = vec![
        rect(1, y0, (-hz, hz), (-hx, hx), 0),
        rect(1, y1, (-hz, hz), (-hx, hx), 0),
        rect(0, -hx, (y0, y1), (-hz, hz), 0),
        rect(2, -hz, (-hx, hx), (y0, y1), 0),
        rect(2, hz, (-hx, hx), (y0, y1), 0),
    ];
    if door {
        m.push(rect(0, hx, (y0, y1), (-hz, -DOOR_HALF_Z), 0));
        m.push(rect(0, hx, (y0, y1), (DOOR_HALF_Z, hz), 0));
        m.push(rect(0, hx, (DOOR_TOP, y1), (-DOOR_HALF_Z, DOOR_HALF_Z), 0));
    } else {
        m.push(rect(0, hx, (y0, y1), (-hz, hz), 0));
    }
    // The open ground outside and the far wall, black so the frame there is the air alone.
    m.push(rect(1, 0.0, (-60.0, 60.0), (hx, FAR_WALL_X), 1));
    m.push(rect(0, FAR_WALL_X, (-5.0, 40.0), (-60.0, 60.0), 1));
    m
}

fn scene(eye: [f32; 3], target: [f32; 3], door: bool) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.9,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 9.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        // The atmosphere takes only the authored sun's direction (its strength is the planet's), so
        // a sun of 0 lux leaves the tables lit and every surface dark: the frame is the air alone.
        directional: DirectionalLightUniform::new([0.3, -0.9, 0.3], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.0, 0.0, 0.0, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
        ],
        meshes: meshes(door),
    }
}

/// Desktop High with the physical atmosphere (the aerial composite) and nothing that keeps a
/// history or lights the room's inside on its own.
fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    assert!(q.physical_atmosphere, "Desktop High draws the atmosphere");
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

/// A renderer whose outdoor composite reads `interiors` (kept across a test's frames: building
/// the pipelines dominates a lavapipe run).
fn renderer(interiors: Vec<InteriorVolume>) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(OutdoorParams {
        interiors,
        ..renderer.outdoor_params().clone()
    });
    renderer
}

/// The linear HDR frame of `lab` drawn by `renderer`.
fn capture(device: &RenderDevice, renderer: &mut FrameRenderer, lab: &LabScene) -> Vec<[f32; 3]> {
    capture_quality(device, renderer, lab, quality())
}

fn capture_quality(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    quality: FrameQuality,
) -> Vec<[f32; 3]> {
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: lab.exposure_ev,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    for i in 0..STILL_FRAMES {
        if i + 1 == STILL_FRAMES {
            renderer.request_hdr_capture();
        }
        renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    }
    let hdr = renderer
        .take_hdr_capture(device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the frame recorded its HDR copy"));
    assert_eq!((hdr.width, hdr.height), (W, H));
    hdr.rgb
}

/// Optional raw HDR evidence for comparing this shader with the parent revision, with no
/// reference image added to the golden suite. Caller-selected evidence directory, atomic writes.
fn write_outdoor_capture(name: &str, pixels: &[[f32; 3]]) {
    let Some(dir) = std::env::var_os("REGIA_AERIAL_OUTDOOR_CAPTURE") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("evidence directory");
    let bytes: Vec<u8> = pixels.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
    let path = dir.join(format!("{name}.rgb32le"));
    regia_test_utils::write_atomic(&path, &bytes).expect("atomic HDR evidence publish");
}

/// Capture the existing four sky-golden views for an exact parent/current shader comparison.
/// This evidence path is inactive in an ordinary test run and never writes reference goldens.
fn write_sky_evidence(device: &RenderDevice) {
    let Some(dir) = std::env::var_os("REGIA_AERIAL_OUTDOOR_CAPTURE") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let panorama = |time| regia_render::SkyView {
        time,
        planet: regia_render::AtmosphereParameters::earth(),
        body: None,
        size: (180, 60),
        fov_y: 1.0,
        look_dir: [0.0, 0.0, 1.0],
        panorama: true,
    };
    let skies = [
        (
            "sky_noon",
            regia_render::render_time_of_day_sky(
                device,
                &regia_render::TimeOfDay::noon(),
                (128, 96),
                12f32.to_radians(),
                true,
            ),
        ),
        (
            "sky_dawn",
            regia_render::render_sky_view(device, &panorama(regia_render::TimeOfDay::dawn())),
        ),
        (
            "sky_dusk",
            regia_render::render_sky_view(device, &panorama(regia_render::TimeOfDay::dusk())),
        ),
        (
            "sky_night",
            regia_render::render_time_of_day_sky(
                device,
                &regia_render::TimeOfDay::night(),
                (160, 100),
                70f32.to_radians(),
                false,
            ),
        ),
    ];
    for (name, sky) in skies {
        let sky = sky.expect("sky evidence");
        regia_test_utils::write_atomic(&dir.join(format!("{name}.rgba8")), &sky.pixels)
            .expect("atomic sky evidence");
    }
}

fn luminance(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

fn mean_luminance(img: &[[f32; 3]]) -> f64 {
    img.iter().map(|c| luminance(*c)).sum::<f64>() / img.len().max(1) as f64
}

/// Mean luminance of the `n × n` block at the frame's center.
fn center(img: &[[f32; 3]], n: u32) -> f64 {
    let (x0, y0) = (W / 2 - n / 2, H / 2 - n / 2);
    let mut sum = 0.0;
    for y in y0..y0 + n {
        for x in x0..x0 + n {
            sum += luminance(img[(y * W + x) as usize]);
        }
    }
    sum / f64::from(n * n)
}

fn room() -> InteriorVolume {
    InteriorVolume::with_floor(ROOM_CENTER, ROOM_HALF)
}

/// Serializes device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    // Evidence-only negative control: load this repository's parent WGSL through the existing
    // reload API, so both variants run in the same executable without rebuilding the engine.
    if let Some(path) = std::env::var_os("REGIA_AERIAL_BASELINE_SHADER") {
        let source = std::fs::read_to_string(path).expect("parent shader evidence input");
        let swap = regia_render::shader_reload::stage_shader_edit("outdoor_sky.wgsl", &source)
            .expect("parent shader validates");
        assert!(swap.validated > 0, "the override must validate real entries");
    }
    RenderDevice::headless()
        .unwrap_or_else(|err| panic!("the aerial composite needs a GPU (lavapipe is one): {err}"))
}

#[test]
fn a_box_no_ray_crosses_changes_no_pixel() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // Outside, 20 m from the room, looking away from it down the open ground at the far wall.
    let lab = scene([24.0, 1.7, 0.0], [FAR_WALL_X, 1.2, 0.0], true);
    let open = capture(&device, &mut renderer(Vec::new()), &lab);
    write_outdoor_capture("default", &open);
    write_sky_evidence(&device);
    let behind = capture(&device, &mut renderer(vec![room()]), &lab);
    assert!(
        mean_luminance(&open) > 0.0,
        "the frame shows the lit ground and the air"
    );
    let differ = open.iter().zip(&behind).filter(|(a, b)| a != b).count();
    assert_eq!(
        differ, 0,
        "a box no view ray crosses must leave every pixel bit-identical"
    );
}

#[test]
fn the_air_in_a_closed_room_scatters_no_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene([-3.0, 1.7, -2.5], [3.5, 2.2, 2.5], false);
    let without = capture(&device, &mut renderer(Vec::new()), &lab);
    let with = capture(&device, &mut renderer(vec![room()]), &lab);
    let (off, on) = (mean_luminance(&without), mean_luminance(&with));
    eprintln!("sealed room: mean luminance {off} without the box, {on} with it");
    // The switch off (`set_aerial_interiors(false)`): the box is ignored, the frame is the open air's.
    let mut switched_off = renderer(vec![room()]);
    switched_off.set_aerial_interiors(false);
    assert!(!switched_off.aerial_interiors());
    let off_frame = capture(&device, &mut switched_off, &lab);
    assert!(
        off_frame == without,
        "with the switch off the composite draws the air as without the box"
    );
    // No light reaches the room's surfaces, so what the frame shows without the box is the
    // outdoor in-scatter of a few meters of air.
    assert!(
        off > 0.0,
        "without the box the room's air carries the sky's in-scatter (mean {off})"
    );
    assert!(
        on <= off * 1.0e-3,
        "inside the box the air takes no in-scatter: mean {on} with, {off} without"
    );
}

#[test]
fn walking_out_of_a_door_has_no_pop() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let door_x = ROOM_HALF[0];
    // The eye walks out along the room's axis, 1.6 m up, looking through the door at the far wall.
    let steps: Vec<f32> = (0..=7).map(|k| 2.0 + 0.5 * k as f32).collect();
    let (mut open_air, mut boxed) = (renderer(Vec::new()), renderer(vec![room()]));
    let mut taken = Vec::new();
    let mut outdoor = Vec::new();
    for &x in &steps {
        let lab = scene([x, 1.6, 0.0], [FAR_WALL_X, 1.6, 0.0], true);
        let without = capture(&device, &mut open_air, &lab);
        let with = capture(&device, &mut boxed, &lab);
        let (off, on) = (center(&without, 5), center(&with, 5));
        eprintln!(
            "eye x {x}: center {off} without the box, {on} with it, taken out {}",
            off - on
        );
        taken.push(off - on);
        outdoor.push(off);
        if x > door_x + 0.01 {
            let differ = without.iter().zip(&with).filter(|(a, b)| a != b).count();
            assert_eq!(
                differ, 0,
                "eye at x {x}, outside the door looking away from the room: bit-identical"
            );
        }
    }
    // The block at the center sees the black far wall: the frame there is the air alone, and the
    // share of it the box takes out is the eye's stretch under the roof over the whole ray.
    for (k, &x) in steps.iter().enumerate() {
        let inside = (door_x - x).max(0.0);
        let expect = outdoor[k] * f64::from(inside / (FAR_WALL_X - x));
        let got = taken[k];
        assert!(
            (got - expect).abs() <= 0.15 * expect + 0.002 * outdoor[k],
            "eye at x {x}: took out {got}, the stretch under the roof predicts {expect} (of {})",
            outdoor[k]
        );
        if k > 0 {
            // No pop: each 0.5 m step changes the in-scatter taken out by at most the air of that
            // step (plus the readback's rounding), never the whole ray's.
            let step = (taken[k - 1] - got).abs();
            let bound = outdoor[k] * f64::from(0.5 / (FAR_WALL_X - x)) * 1.3 + 0.002 * outdoor[k];
            assert!(
                step <= bound,
                "eye at x {x}: the in-scatter taken out jumped by {step} (bound {bound})"
            );
        }
    }
}

/// ADR-7975: a map with five interior boxes (Switchbacks has five passages), the eye in the sealed
/// room listed fifth. The composite binds the four boxes nearest the eye, so the room is bound and
/// its air takes no in-scatter: the frame is bit-identical to the frame with the room alone (the
/// other four lie far under the ground, where no view ray goes), and the CPU twin agrees (no
/// in-scatter kept on a ray inside the room). Before, the first four in list order were bound and
/// the room glowed with sky light.
#[test]
fn the_eye_in_a_fifth_box_takes_its_air_out() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (eye, target) = ([-3.0, 1.7, -2.5], [3.5, 2.2, 2.5]);
    let lab = scene(eye, target, false);
    let buried = |x: f32| InteriorVolume::with_floor([x, -1000.0, 0.0], [4.0, 2.0, 4.0]);
    let five = vec![buried(-100.0), buried(-50.0), buried(50.0), buried(100.0), room()];
    let without = capture(&device, &mut renderer(Vec::new()), &lab);
    let alone = capture(&device, &mut renderer(vec![room()]), &lab);
    let fifth = capture(&device, &mut renderer(five.clone()), &lab);
    let (off, on) = (mean_luminance(&without), mean_luminance(&fifth));
    eprintln!("sealed room listed fifth: mean luminance {off} without boxes, {on} with five");
    assert!(
        off > 0.0,
        "without boxes the room's air carries the sky's in-scatter"
    );
    assert!(
        on <= off * 1.0e-3,
        "the room listed fifth must take its air out: mean {on} with five boxes, {off} without"
    );
    let differ = alone.iter().zip(&fifth).filter(|(a, b)| a != b).count();
    assert_eq!(
        differ, 0,
        "five boxes, the eye's fifth: bit-identical to the room alone"
    );
    // The CPU twin, on the ray to the frame's target: all of it under the roof.
    let d = [target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let dir = [d[0] / len, d[1] / len, d[2] / len];
    let spans = regia_render::interior_spans_on_ray(eye, dir, len, &five);
    assert_eq!(
        spans.inscatter_share_near(),
        0.0,
        "the CPU twin keeps no in-scatter either"
    );
}

#[test]
fn overlapping_boxes_remove_the_shared_air_only_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene([2.0, 1.6, 0.0], [FAR_WALL_X, 1.6, 0.0], true);
    let one = capture(&device, &mut renderer(vec![room()]), &lab);
    let overlapping = capture(&device, &mut renderer(vec![room(), room(), room()]), &lab);
    assert_eq!(
        one, overlapping,
        "duplicate volumes must not darken the exterior through a door"
    );
}

#[test]
fn live_toggle_releases_a_frozen_temporal_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene([-3.0, 1.7, -2.5], [3.5, 2.2, 2.5], false);
    let mut renderer = renderer(vec![room()]);
    let mut q = quality();
    q.taau = true;
    q.screen_space_gi = true;
    q.paged_sun_shadows = true;
    assert!(q.reuse_static_screen());
    // Fixed steps exceed the held-view convergence count; no timing dependency.
    for _ in 0..20 {
        capture_quality(&device, &mut renderer, &lab, q);
    }
    let dark = capture_quality(&device, &mut renderer, &lab, q);
    renderer.set_aerial_interiors(false);
    let lit = capture_quality(&device, &mut renderer, &lab, q);
    let (a, b) = (mean_luminance(&dark), mean_luminance(&lit));
    assert!(
        b > 0.01 && a < b * 0.001,
        "live toggle must release held history: {a} -> {b}"
    );
    renderer.set_aerial_interiors(true);
    let dark_again = capture_quality(&device, &mut renderer, &lab, q);
    assert!(
        mean_luminance(&dark_again) < b * 0.001,
        "old outdoor history must not leak into the room"
    );
}
