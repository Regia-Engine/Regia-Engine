// SPDX-License-Identifier: MPL-2.0
//! Local reflection captures as level content (ADR-2860), on a headless device:
//!
//! - a bake saved to a `.fcap`, read back and installed in a fresh renderer renders the same frame
//!   bit for bit as the freshly baked one, with no capture face rendered;
//! - a selection swap (budget 1, the selection's eye moving from one capture to another) fades
//!   over frames: no frame-to-frame step on the mirror floor is more than a third of the whole
//!   change (golden-free);
//! - frames where nothing moved create nothing and write no layer;
//! - a frame reads at most the tier's budget of captures.

use regia_asset::local_capture_bake::{FcapCapture, FcapFile};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabLocalLight, LabMaterial, LabMesh, LabPointLight,
    LabScene, LocalCaptureBake, LocalCaptureDesc, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, local_capture_budget,
};
use std::sync::Arc;

const W: u32 = 96;
const H: u32 = 64;
const EV: f32 = 7.0;
const HALF: f32 = 3.0;
const HEIGHT: f32 = 3.0;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(d) => Some(d),
        Err(err) => {
            eprintln!("skip local_capture_selection: {err}");
            None
        }
    }
}

fn slab(center: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (v, i) = regia_render::box_mesh(half);
    LabMesh {
        vertices: regia_render::translate_mesh(&v, center),
        indices: i,
        material,
        ..LabMesh::unskinned()
    }
}

/// A closed room with a mirror floor, a red and a blue cube, lit by one point light.
fn room() -> LabScene {
    let t = 0.1;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.5, 2.6],
        target: [0.0, 0.2, -1.0],
        fov_y: std::f32::consts::FRAC_PI_3,
        near: 0.05,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: EV,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: vec![LabLocalLight::Point(LabPointLight {
            position: [0.5, 2.5, 0.5],
            color: [1.0; 3],
            intensity: 6000.0,
            radius: 12.0,
            cast_shadows: false,
            source_radius: None,
        })],
        materials: vec![
            LabMaterial {
                base_color: [0.95, 0.95, 0.95, 1.0],
                metallic: 1.0,
                roughness: 0.02,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.6, 0.6, 0.6, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.1, 0.08, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.08, 0.15, 0.8, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            slab([0.0, -t, 0.0], [HALF + t, t, HALF + t], 0),
            slab([0.0, HEIGHT + t, 0.0], [HALF + t, t, HALF + t], 1),
            slab([-HALF - t, HEIGHT / 2.0, 0.0], [t, HEIGHT / 2.0, HALF], 1),
            slab([HALF + t, HEIGHT / 2.0, 0.0], [t, HEIGHT / 2.0, HALF], 1),
            slab([0.0, HEIGHT / 2.0, -HALF - t], [HALF, HEIGHT / 2.0, t], 1),
            slab([0.0, HEIGHT / 2.0, HALF + t], [HALF, HEIGHT / 2.0, t], 1),
            slab([-1.4, 0.9, -1.4], [0.35; 3], 2),
            slab([1.4, 0.9, -1.4], [0.35; 3], 3),
        ],
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.taau = false;
    q
}

fn hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    frame: &regia_render::FrameScene<'_>,
) -> Vec<[f32; 3]> {
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, frame).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("a post stack frame")
        .rgb
}

/// The bottom third of the image: the mirror floor in front of the camera.
fn floor_mean_abs_diff(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let start = (H * 2 / 3 * W) as usize;
    let (sum, n) = a[start..]
        .iter()
        .zip(&b[start..])
        .fold((0.0f64, 0usize), |(s, n), (x, y)| {
            let d = (x[0] - y[0]).abs() + (x[1] - y[1]).abs() + (x[2] - y[2]).abs();
            (s + f64::from(d), n + 1)
        });
    (sum / n.max(1) as f64) as f32
}

/// Mean absolute difference over the whole image.
fn image_mean_abs_diff(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from((x[0] - y[0]).abs() + (x[1] - y[1]).abs() + (x[2] - y[2]).abs()))
        .sum();
    (sum / a.len().max(1) as f64) as f32
}

fn to_fcap(renderer: &FrameRenderer) -> FcapFile {
    FcapFile {
        width: regia_render::LOCAL_CAPTURE_WIDTH,
        captures: renderer
            .local_captures()
            .into_iter()
            .filter_map(|i| {
                let bake = i.desc.bake.clone()?;
                Some(FcapCapture {
                    id: i.desc.id,
                    center: i.desc.center,
                    half_extent: i.desc.half_extent,
                    weight: i.desc.weight,
                    fade: i.desc.fade,
                    coverage: bake.coverage,
                    mips: bake.mips.clone(),
                })
            })
            .collect(),
    }
}

#[test]
fn a_saved_and_loaded_bake_renders_the_freshly_baked_frame() {
    let Some(device) = device() else { return };
    let lab = room();
    let frame = lab.as_frame_scene_with_quality(quality());
    let eye = frame.eye;
    let budget = local_capture_budget(&frame.quality);
    let place = |renderer: &mut FrameRenderer, bake: Option<Arc<LocalCaptureBake>>| {
        let mut desc = LocalCaptureDesc::new(7, [0.0, 1.5, 0.0], [HALF, HEIGHT / 2.0, HALF]);
        desc.bake = bake;
        renderer.place_local_capture(desc).expect("place");
    };
    let mut fresh = FrameRenderer::new();
    place(&mut fresh, None);
    fresh.bake_local_capture(&device, &frame, 7).expect("bake");
    fresh.select_local_captures(&device, eye, budget);
    let _ = hdr(&device, &mut fresh, &frame);
    let baked = hdr(&device, &mut fresh, &frame);

    let dir = regia_test_utils::unique_temp_dir("capture-round-trip");
    let path = dir.join("room.fcap");
    to_fcap(&fresh).write(&path).expect("write");
    let file = FcapFile::read(&path).expect("read back");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(file.captures.len(), 1);
    let c = &file.captures[0];
    let bake = Arc::new(LocalCaptureBake {
        width: file.width,
        coverage: c.coverage,
        mips: c.mips.clone(),
    });

    let mut loaded = FrameRenderer::new();
    place(&mut loaded, Some(bake));
    loaded.select_local_captures(&device, eye, budget);
    let _ = hdr(&device, &mut loaded, &frame);
    let installed = hdr(&device, &mut loaded, &frame);
    let info = &loaded.local_captures()[0];
    assert!(info.baked && !info.pending, "installed, nothing queued");
    assert_eq!(info.layer, Some(0));
    assert_eq!(info.uploads, 1);
    let differing = baked.iter().zip(&installed).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "the loaded bake renders the freshly baked frame bit for bit"
    );
    // And the capture shows: the floor differs from a renderer without it.
    let mut bare = FrameRenderer::new();
    let _ = hdr(&device, &mut bare, &frame);
    let without = hdr(&device, &mut bare, &frame);
    assert!(
        floor_mean_abs_diff(&baked, &without) > 1.0,
        "the capture shows in the mirror"
    );
}

#[test]
fn a_slot_change_fades_without_a_pop_and_warm_frames_create_nothing() {
    let Some(device) = device() else { return };
    let lab = room();
    let frame = lab.as_frame_scene_with_quality(quality());
    let mut renderer = FrameRenderer::new();
    // Two captures, one in each half of the room (each box the whole room), baked from different
    // points: their maps differ, so a swap changes the mirror.
    for (id, x) in [(1u64, -1.8f32), (2, 1.8)] {
        renderer
            .place_local_capture(LocalCaptureDesc::new(
                id,
                [x, 1.0, 0.5],
                [HALF, HEIGHT / 2.0, HALF],
            ))
            .expect("place");
        renderer.bake_local_capture(&device, &frame, id).expect("bake");
    }
    let near_1 = [-2.5, 1.5, 0.5];
    let near_2 = [2.5, 1.5, 0.5];
    // Settle on capture 1 with a budget of one.
    for _ in 0..12 {
        renderer.select_local_captures(&device, near_1, 1);
        let _ = hdr(&device, &mut renderer, &frame);
    }
    let selected: Vec<_> = renderer
        .local_captures()
        .into_iter()
        .filter(|i| i.blend > 0.0)
        .collect();
    assert_eq!(selected.len(), 1, "budget 1");
    assert_eq!(selected[0].desc.id, 1);
    // Warm frames: no texture, no layer write, no swap.
    let counters = renderer.local_capture_counters();
    let uploads: Vec<u32> = renderer.local_captures().iter().map(|i| i.uploads).collect();
    for _ in 0..4 {
        renderer.select_local_captures(&device, near_1, 1);
        let _ = hdr(&device, &mut renderer, &frame);
    }
    assert_eq!(
        renderer.local_capture_counters(),
        counters,
        "warm frames create nothing"
    );
    assert_eq!(
        renderer
            .local_captures()
            .iter()
            .map(|i| i.uploads)
            .collect::<Vec<_>>(),
        uploads,
        "warm frames write no layer"
    );
    let before = hdr(&device, &mut renderer, &frame);
    // The selection's eye moves to capture 2 (the render camera stays): the swap fades.
    let mut frames = vec![before.clone()];
    for _ in 0..(regia_render::LOCAL_CAPTURE_FADE_FRAMES + 4) {
        renderer.select_local_captures(&device, near_2, 1);
        frames.push(hdr(&device, &mut renderer, &frame));
    }
    assert_eq!(renderer.local_capture_counters().1, counters.1 + 1, "one swap");
    let after = frames.last().cloned().unwrap_or_default();
    let whole = floor_mean_abs_diff(&before, &after);
    let worst_step = frames
        .windows(2)
        .map(|w| floor_mean_abs_diff(&w[0], &w[1]))
        .fold(0.0f32, f32::max);
    eprintln!("capture swap: whole change {whole:.3}, worst frame step {worst_step:.3}");
    assert!(whole > 0.5, "the swap changes the mirror: {whole}");
    assert!(
        worst_step <= whole / 3.0,
        "a swap fades over frames, no pop: worst step {worst_step} of {whole}"
    );
    let selected: Vec<_> = renderer
        .local_captures()
        .into_iter()
        .filter(|i| i.blend > 0.0)
        .collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].desc.id, 2);
}

#[test]
fn a_frame_reads_at_most_its_tiers_budget() {
    let Some(device) = device() else { return };
    let lab = room();
    for (preset, want) in [
        (RenderProfilePreset::MobileLow, 2usize),
        (RenderProfilePreset::DesktopUltra, 4),
    ] {
        let q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        let budget = local_capture_budget(&q);
        assert_eq!(budget, want, "{preset:?}");
        let frame = lab.as_frame_scene_with_quality(quality());
        let mut renderer = FrameRenderer::new();
        for id in 0..6u64 {
            let x = -2.5 + id as f32;
            renderer
                .place_local_capture(LocalCaptureDesc::new(id, [x, 1.0, 0.0], [0.5, 1.0, 0.5]))
                .expect("place");
            renderer.bake_local_capture(&device, &frame, id).expect("bake");
        }
        // Captures 0 to 3 held the layers before the selection; the extra ones fade out.
        for _ in 0..(regia_render::LOCAL_CAPTURE_FADE_FRAMES + 2) {
            renderer.select_local_captures(&device, [0.0, 1.5, 0.0], budget);
        }
        let selected = renderer
            .local_captures()
            .iter()
            .filter(|i| i.layer.is_some())
            .count();
        assert_eq!(selected, budget, "{preset:?}: the budget's nearest boxes");
        assert!(selected <= regia_render::LOCAL_CAPTURE_LAYERS);
    }
}

/// Desktop High with screen reflections: stochastic SSR when `stochastic`, else the linear march.
fn reflective_quality(stochastic: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q.physical_atmosphere = false;
    q.taau = false;
    q.ssr = true;
    q.reflection_probes = true;
    q.gpu_driven = stochastic;
    q
}

/// The last of `frames` frames of a fresh renderer, with the room's capture and/or a coarse probe.
fn reflective_frame(
    device: &RenderDevice,
    stochastic: bool,
    capture: bool,
    probe: bool,
    frames: u32,
) -> Vec<[f32; 3]> {
    // Mirror walls too: stochastic SSR takes the probe only for reflections that point down (a
    // wall seen from above), the linear march only on floors.
    let mut lab = room();
    lab.materials[1].metallic = 1.0;
    lab.materials[1].roughness = 0.05;
    let frame = lab.as_frame_scene_with_quality(reflective_quality(stochastic));
    let mut renderer = FrameRenderer::new();
    // Every variant captures the probe (the same frames on the renderer); only some install it.
    let p = renderer
        .capture_reflection_probe(device, &frame, 1, [0.0, 1.5, 0.0], [HALF, HEIGHT / 2.0, HALF])
        .expect("probe capture");
    if probe {
        renderer.set_reflection_probes(vec![p]);
    }
    if capture {
        renderer
            .place_local_capture(LocalCaptureDesc::new(
                9,
                [0.0, 1.5, 0.0],
                [HALF, HEIGHT / 2.0, HALF],
            ))
            .expect("place");
        renderer.bake_local_capture(device, &frame, 9).expect("bake");
    }
    for _ in 1..frames {
        renderer.select_local_captures(device, frame.eye, 4);
        renderer.render_offscreen(device, &frame).expect("frame");
    }
    renderer.select_local_captures(device, frame.eye, 4);
    hdr(device, &mut renderer, &frame)
}

/// ADR-2860 (4): a screen reflection that misses inside a capture's box keeps the forward pass's
/// HDR capture instead of the coarse probe. With the capture covering the whole room, adding a
/// probe changes no pixel of an SSR or SSSR frame; without the capture the probe does change the
/// mirror (so the probe is live in this scene).
#[test]
fn screen_reflection_misses_keep_the_capture_instead_of_the_probe() {
    let Some(device) = device() else { return };
    for (stochastic, frames) in [(true, 12u32), (false, 3)] {
        let capture_only = reflective_frame(&device, stochastic, true, false, frames);
        let capture_and_probe = reflective_frame(&device, stochastic, true, true, frames);
        let probe_only = reflective_frame(&device, stochastic, false, true, frames);
        let neither = reflective_frame(&device, stochastic, false, false, frames);
        let probe_effect = image_mean_abs_diff(&probe_only, &neither);
        let differing = capture_only
            .iter()
            .zip(&capture_and_probe)
            .filter(|(a, b)| a != b)
            .count();
        eprintln!(
            "stochastic {stochastic}: the probe moves the bare mirror by {probe_effect:.3}; \
             with the capture {differing} pixels differ"
        );
        assert!(
            probe_effect > 0.5,
            "the probe is live in this scene: {probe_effect}"
        );
        assert_eq!(
            differing, 0,
            "inside the capture's box the probe changes nothing (stochastic {stochastic})"
        );
    }
}
