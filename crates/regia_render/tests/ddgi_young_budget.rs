// SPDX-License-Identifier: MPL-2.0
//! The young-probe trace budget (ADR-7870, `DdgiSettings::young_budget`, `specs/79` § 9.13) on the runtime frame
//! path, on lavapipe or a card.
//!
//! A start-up or a light cut makes every probe young, and a young probe re-traces every frame for its first
//! sixteen updates. With a budget a frame traces at most that many young probes, the nearest to the eye first,
//! and every probe still takes its sixteen updates within `DdgiSettings::young_settle_frames`. With the budget off,
//! or too large to bind, the probe buffers are the ones the update wrote before the budget existed: byte for byte.
//! Every check counts probe updates read back from the probe records (no wall clock).

use regia_core::glam::Mat4;
use regia_render::{
    DdgiReadback, DdgiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
};

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip ddgi_young_budget: {err}");
            None
        }
    }
}

/// Probe record words (`ddgi_common.wgsl`): offset, cell + updates, stamp.
const PROBE_WORDS: usize = 12;
/// Updates a young probe takes every frame (`ddgi_trace.wgsl` `DDGI_YOUNG_UPDATES`).
const YOUNG: i32 = 16;
const EYE: [f32; 3] = [0.0, 1.7, 0.0];

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

fn sun(elevation_deg: f32) -> DirectionalLightUniform {
    let e = elevation_deg.to_radians();
    let az = 0.6f32;
    DirectionalLightUniform::new(
        [-(e.cos() * az.cos()), -e.sin(), -(e.cos() * az.sin())],
        [1.0; 3],
        100_000.0,
    )
}

/// A ground slab under the sun, seen from `eye` toward -z.
fn scene(eye: [f32; 3], sun_elevation: f32) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
    LabScene {
        width: 96,
        height: 64,
        eye,
        target: [eye[0], 1.0, eye[2] - 6.0],
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
        directional: sun(sun_elevation),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        // The ground 12 m under the eye, under every probe of both cascades: no probe starts inside geometry or
        // relocates out of it (a probe that leaves geometry restarts its young updates, ADR-2605), so each takes
        // exactly sixteen young updates.
        meshes: vec![block([0.0, -10.55, 0.0], [60.0, 0.25, 60.0])],
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// Two small cascades (6 × 4 × 6 at 1 m and 3 m, 288 probes), settled probes every eighth frame.
fn settings(young_budget: u32) -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims: [6, 4, 6],
        spacing: 1.0,
        update_divisor: 8,
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        young_budget,
        ..DdgiSettings::default()
    }
}

fn renderer(young_budget: u32) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(settings(young_budget));
    r.set_ddgi_enabled(Some(true));
    r
}

fn frame(r: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> DdgiReadback {
    r.render_offscreen(device, &lab.as_frame_scene_with_quality(quality()))
        .unwrap_or_else(|err| panic!("frame: {err}"));
    r.ddgi_read_back(device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"))
}

fn slots(rb: &DdgiReadback) -> usize {
    (rb.dims[0] * rb.dims[1] * rb.dims[2] * rb.cascades) as usize
}

/// (world cell + updates, last-trace frame) of every slot.
fn records(rb: &DdgiReadback) -> Vec<([i32; 4], u32)> {
    (0..slots(rb))
        .map(|slot| (rb.probe(slot as u32).1, rb.data[slot * PROBE_WORDS + 8]))
        .collect()
}

/// Distance (m) from the eye to the world cell of slot `slot`'s probe.
fn distance(rb: &DdgiReadback, slot: usize, tag: [i32; 4]) -> f32 {
    let per = (rb.dims[0] * rb.dims[1] * rb.dims[2]) as usize;
    let spacing = 3f32.powi((slot / per) as i32);
    let p = [tag[0], tag[1], tag[2]].map(|v| v as f32 * spacing);
    ((p[0] - EYE[0]).powi(2) + (p[1] - EYE[1]).powi(2) + (p[2] - EYE[2]).powi(2)).sqrt()
}

/// Renders from a field where every probe is young (`all_young` names the frame that made them so: the first
/// frame, or a light cut) until every probe has taken sixteen updates. Each frame: at most `budget` young probes
/// traced, exactly `budget` while at least that many are young, the traced ones on the first frame no farther from
/// the eye than a ring (one finest spacing) past the nearest one left waiting. Returns the frames it took.
fn settle_under_budget(
    r: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    budget: usize,
    mut previous: Option<Vec<([i32; 4], u32)>>,
    what: &str,
) -> u32 {
    let bound = settings(budget as u32).young_settle_frames();
    for n in 1..=bound {
        let rb = frame(r, device, lab);
        let now = records(&rb);
        let this_frame = now.iter().map(|(_, stamp)| *stamp).max().unwrap_or(0);
        // Young when this frame's `classify` ran: every probe on the frame that restarted them, else under
        // sixteen updates on the frame before (the camera holds, so no slot changes cell).
        let restarted_now = n == 1;
        let young_before = |slot: usize| match &previous {
            Some(prev) if !restarted_now => prev[slot].0[3] < YOUNG,
            _ => true,
        };
        let young: Vec<usize> = (0..now.len()).filter(|&s| young_before(s)).collect();
        let traced: Vec<usize> = young
            .iter()
            .copied()
            .filter(|&s| now[s].1 == this_frame)
            .collect();
        assert!(
            traced.len() <= budget,
            "{what}, frame {n}: {} young probes traced for a budget of {budget}",
            traced.len()
        );
        if young.len() >= budget {
            assert_eq!(
                traced.len(),
                budget,
                "{what}, frame {n}: {} young probes waited, so the budget is spent",
                young.len()
            );
        } else {
            assert_eq!(
                traced.len(),
                young.len(),
                "{what}, frame {n}: under the budget every young probe traces"
            );
        }
        if n == 1 {
            let far_traced = traced
                .iter()
                .map(|&s| distance(&rb, s, now[s].0))
                .fold(0.0f32, f32::max);
            let near_waiting = young
                .iter()
                .filter(|s| !traced.contains(s))
                .map(|&s| distance(&rb, s, now[s].0))
                .fold(f32::MAX, f32::min);
            eprintln!(
                "{what}: frame 1 traced {} of {} young probes, out to {far_traced:.2} m; the nearest waiting at \
                 {near_waiting:.2} m",
                traced.len(),
                young.len()
            );
            assert!(
                far_traced <= near_waiting + 1.0,
                "{what}: the nearest young probes trace first ({far_traced} m traced, {near_waiting} m waits)"
            );
        }
        let settled = now.iter().all(|(tag, _)| tag[3] >= YOUNG);
        previous = Some(now);
        if settled {
            eprintln!("{what}: every probe took {YOUNG} updates in {n} frames (bound {bound})");
            return n;
        }
    }
    panic!("{what}: some probe had fewer than {YOUNG} updates after {bound} frames");
}

/// A start-up and a light cut on a field of 288 probes with a budget of 72: at most 72 young probes a frame,
/// all 72 while that many wait, the nearest first, and every probe's sixteen updates within
/// `young_settle_frames` (16 · 288 / 72 + 16 = 80 frames).
#[test]
fn a_light_cut_traces_at_most_the_young_budget_a_frame_and_every_probe_settles_in_time() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let budget = 72usize;
    assert_eq!(settings(budget as u32).young_settle_frames(), 80);
    let mut r = renderer(budget as u32);
    let lab = scene(EYE, 50.0);
    let start = settle_under_budget(&mut r, &device, &lab, budget, None, "start-up");
    // At least the 16 · 288 / 72 = 64 frames the young updates need at 72 a frame.
    assert!(start >= 64, "start-up settled in {start} frames");
    // Hold a few frames, then cut the light (the sun drops 25°): every probe starts over as a young one.
    let mut last = None;
    for _ in 0..8 {
        last = Some(records(&frame(&mut r, &device, &lab)));
    }
    let cut = scene(EYE, 25.0);
    let after = settle_under_budget(&mut r, &device, &cut, budget, last, "light cut");
    assert!(after >= 64, "the light cut settled in {after} frames");
}

/// With the budget off (0, every tier) and with a budget no frame can reach (the probe count), the same 64 frames
/// (a start-up, a light cut at frame 20, the camera moved 2.5 m at frame 40 so the volumes scroll) leave the same
/// probe records, tiles and rays, byte for byte: a budget that does not bind changes nothing, and the passes it
/// adds read only.
#[test]
fn a_budget_that_does_not_bind_leaves_the_probe_buffers_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let probes = settings(0).probe_count() as u32;
    let (mut off, mut wide) = (renderer(0), renderer(probes));
    let mut compared = 0;
    for n in 1..=64u32 {
        let lab = match n {
            1..=19 => scene(EYE, 50.0),
            20..=39 => scene(EYE, 25.0),
            _ => scene([EYE[0] + 2.5, EYE[1], EYE[2]], 25.0),
        };
        let (a, b) = (frame(&mut off, &device, &lab), frame(&mut wide, &device, &lab));
        if matches!(n, 1 | 16 | 20 | 21 | 40 | 41 | 64) {
            assert_eq!(a.bases, b.bases, "frame {n}: the volumes stand where they stood");
            assert!(
                a.data == b.data,
                "frame {n}: probe data words differ with a budget of {probes}"
            );
            assert!(
                a.rays
                    .iter()
                    .flatten()
                    .map(|v| v.to_bits())
                    .eq(b.rays.iter().flatten().map(|v| v.to_bits())),
                "frame {n}: probe rays differ with a budget of {probes}"
            );
            compared += 1;
        }
    }
    assert_eq!(compared, 7);
}

/// A budget that binds traces fewer probes on the first frame than the field holds (the control for the test
/// above: there the budget path ran and changed nothing because it did not bind).
#[test]
fn a_budget_that_binds_holds_the_first_frame_to_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let lab = scene(EYE, 50.0);
    let count = |budget: u32| {
        let rb = frame(&mut renderer(budget), &device, &lab);
        let now = records(&rb);
        let this_frame = now.iter().map(|(_, s)| *s).max().unwrap_or(0);
        now.iter().filter(|(_, s)| *s == this_frame).count()
    };
    let probes = settings(0).probe_count() as usize;
    assert_eq!(
        count(0),
        probes,
        "without a budget the first frame traces every probe"
    );
    assert_eq!(count(probes as u32), probes);
    assert_eq!(
        count(100),
        100,
        "a budget of 100 traces 100 of the {probes} young probes"
    );
}
