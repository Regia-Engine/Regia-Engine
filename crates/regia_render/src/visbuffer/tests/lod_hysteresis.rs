// SPDX-License-Identifier: MPL-2.0
//! Cluster LOD hysteresis (ADR-4631).
//!
//! An instance whose projected error oscillates inside the band keeps one level. An instance that
//! walks past the band switches once. A camera cut does not reuse the stored level. A still camera
//! draws the hard cut's clusters, including when the error sits on the threshold.

use std::collections::BTreeSet;

use super::super::gpu::CullLists;
use super::*;
use crate::profile::{FrameQuality, RenderProfile, RenderProfilePreset};
use crate::{FrameMesh, FrameRenderer, FrameScene, ShadowCast};

/// Object-space error of the two-level mesh. Small enough that the test's eye moves stay under
/// the camera-cut jump.
const ERROR: f32 = 0.05;
/// Band used when the test wants a wide hold. The default band is the documented fraction.
const BAND_PX: f32 = 0.25;
/// Frames the in-band oscillation holds one level.
const HOLD_FRAMES: usize = 8;

struct View {
    sphere: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
    post: PostSettings,
    center: [f32; 3],
    radius: f32,
    ppu: f32,
}

fn view() -> View {
    let sphere = crate::uv_sphere(1.0, 24, 12);
    let built = two_level_builder(ERROR)(&sphere.0, &sphere.1).expect("two levels");
    let height = SIZE;
    let fov_y: f32 = 1.0;
    View {
        sphere,
        material: LabMaterial::default(),
        post: post(),
        center: [built.bounds[0], built.bounds[1], built.bounds[2]],
        radius: built.bounds[3],
        ppu: height as f32 / (2.0 * (fov_y * 0.5).tan()).max(1e-6),
    }
}

impl View {
    /// Eye on +Z where this mesh's error projects to `projected` pixels.
    fn eye_at(&self, projected: f32) -> [f32; 3] {
        let distance = ERROR * self.ppu / projected;
        [
            self.center[0],
            self.center[1],
            self.center[2] + self.radius + distance,
        ]
    }

    fn scene(&self, eye: [f32; 3]) -> FrameScene<'_> {
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.visibility_buffer = true;
        quality.taau = false;
        quality.msaa_samples = 1;
        // The analytic pixel error uses this vertical FOV, without aspect-ratio adaptation.
        quality.horizontal_plus = false;
        let model = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        FrameScene {
            width: SIZE,
            height: SIZE,
            eye,
            target: self.center,
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: true,
            post: Some(&self.post),
            fog: crate::FogSettings::default(),
            directional: crate::DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 100_000.0),
            local_lights: &[],
            quality,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 1002,
                object_key: 2,
                vertices: &self.sphere.0,
                indices: &self.sphere.1,
                material: &self.material,
                model,
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::Off,
                uv1: &[],
                lod_dither: 0.0,
            }],
        }
    }
}

fn renderer(device: &crate::RenderDevice, band: Option<f32>) -> FrameRenderer {
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(two_level_builder(ERROR)));
    renderer.set_visbuffer_settings(VisBufferSettings {
        occlusion: false,
        cluster_shadows: false,
        lod_hysteresis_px: band,
        ..VisBufferSettings::default()
    });
    renderer
}

fn drawn(lists: &CullLists) -> BTreeSet<(u32, u32)> {
    let cap = lists.capacity as usize;
    let mut set = BTreeSet::new();
    for phase in 0..2 {
        let single = lists.state[phase * 4 + 1] as usize;
        let double = lists.state[16 + phase * 4 + 1] as usize;
        for i in 0..single {
            let slot = phase * cap + i;
            set.insert((lists.clusters[slot * 2], lists.clusters[slot * 2 + 1]));
        }
        for i in 0..double {
            let slot = phase * cap + cap - 1 - i;
            set.insert((lists.clusters[slot * 2], lists.clusters[slot * 2 + 1]));
        }
    }
    set
}

fn eye_jump(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// One frame. The CPU switch count matches the GPU word.
fn step_with_pixels(
    renderer: &mut FrameRenderer,
    device: &crate::RenderDevice,
    scene: &FrameScene<'_>,
) -> (BTreeSet<(u32, u32)>, u32, Vec<u8>) {
    let pixels = render(renderer, device, scene);
    let lists = renderer
        .visbuffer_pass_mut()
        .read_cull_lists(device)
        .expect("readback")
        .expect("a visibility frame ran");
    let set = drawn(&lists);
    let gpu = renderer
        .visbuffer_pass_mut()
        .read_lod_switches(device)
        .expect("readback")
        .expect("switch word");
    let cpu = renderer.visbuffer_stats().lod_switches;
    assert_eq!(cpu, gpu, "the CPU mirror matches the GPU switch word");
    assert!(!set.is_empty(), "the mesh drew clusters");
    (set, gpu, pixels)
}

fn step(
    renderer: &mut FrameRenderer,
    device: &crate::RenderDevice,
    scene: &FrameScene<'_>,
) -> (BTreeSet<(u32, u32)>, u32) {
    let (set, switches, _) = step_with_pixels(renderer, device, scene);
    (set, switches)
}

fn hard_set(device: &crate::RenderDevice, view: &View, projected: f32) -> BTreeSet<(u32, u32)> {
    let mut renderer = renderer(device, Some(0.0));
    let scene = view.scene(view.eye_at(projected));
    let (set, switches) = step(&mut renderer, device, &scene);
    assert_eq!(switches, 0, "a fresh hard cut has nothing to switch from");
    set
}

#[test]
fn the_default_band_is_the_documented_fraction() {
    let settings = VisBufferSettings::default();
    let fraction = crate::visbuffer::LOD_HYSTERESIS_FRACTION;
    assert_eq!(
        settings.resolved_lod_hysteresis_px(),
        fraction * settings.lod_threshold_px.max(0.0)
    );
    let hard = VisBufferSettings {
        lod_hysteresis_px: Some(0.0),
        ..settings
    };
    assert_eq!(hard.resolved_lod_hysteresis_px(), 0.0);
    let negative = VisBufferSettings {
        lod_hysteresis_px: Some(-1.0),
        ..settings
    };
    assert_eq!(negative.resolved_lod_hysteresis_px(), 0.0);
    let pixels = VisBufferSettings {
        lod_hysteresis_px: Some(BAND_PX),
        ..settings
    };
    assert_eq!(pixels.resolved_lod_hysteresis_px(), BAND_PX);
}

/// A non-finite threshold must not freeze the stored cut. `inf` makes every error look inside
/// the band, so the previous level never moves.
#[test]
fn audfix_non_finite_lod_threshold_is_a_hard_cut() {
    let settings = VisBufferSettings {
        lod_threshold_px: f32::INFINITY,
        lod_hysteresis_px: None,
        ..VisBufferSettings::default()
    };
    assert_eq!(settings.resolved_lod_hysteresis_px(), 0.0);
    let nan = VisBufferSettings {
        lod_threshold_px: f32::NAN,
        lod_hysteresis_px: None,
        ..VisBufferSettings::default()
    };
    assert_eq!(nan.resolved_lod_hysteresis_px(), 0.0);
}

/// 0.9 and 1.1 of a 1 px threshold stay inside a 0.25 px band, so the level does not change.
#[test]
fn an_error_inside_the_band_keeps_one_lod() {
    let device = crate::test_device();
    let view = view();
    let coarse = hard_set(&device, &view, 0.9);
    let fine = hard_set(&device, &view, 1.1);
    assert_ne!(
        coarse, fine,
        "the two errors sit on opposite sides of the hard cut"
    );
    let mut renderer = renderer(&device, Some(BAND_PX));
    let mut previous = None;
    let mut switches = 0u32;
    for frame in 0..HOLD_FRAMES {
        let projected = if frame.is_multiple_of(2) { 0.9 } else { 1.1 };
        let eye = view.eye_at(projected);
        if let Some(before) = previous {
            assert!(
                eye_jump(before, eye) < 40.0,
                "frame {frame} jumped the camera cut"
            );
        }
        previous = Some(eye);
        let scene = view.scene(eye);
        let (set, frame_switches) = step(&mut renderer, &device, &scene);
        switches += frame_switches;
        assert_eq!(set, coarse, "frame {frame} left the stored level");
        assert_ne!(set, fine, "frame {frame} took the hard cut");
    }
    assert_eq!(switches, 0, "an in-band oscillation switches");
}

/// 0.4 (outside, coarse) then 1.1 (inside, keep) then 2.0 (outside, fine): one switch.
#[test]
fn an_error_past_the_band_switches_once() {
    let device = crate::test_device();
    let view = view();
    let at_fine = hard_set(&device, &view, 2.0);
    let would_switch = hard_set(&device, &view, 1.1);
    let mut renderer = renderer(&device, Some(BAND_PX));
    let samples = [0.4_f32, 1.1, 2.0];
    let mut seen = Vec::new();
    let mut previous = None;
    for projected in samples {
        let eye = view.eye_at(projected);
        if let Some(before) = previous {
            assert!(
                eye_jump(before, eye) < 40.0,
                "projected {projected} jumped the camera cut"
            );
        }
        previous = Some(eye);
        let scene = view.scene(eye);
        seen.push(step(&mut renderer, &device, &scene));
    }
    let switches: Vec<u32> = seen.iter().map(|(_, count)| *count).collect();
    assert_eq!(
        switches,
        vec![0, 0, 1],
        "the walk switches on the frame that leaves the band"
    );
    assert_eq!(seen[0].0, seen[1].0, "1.1 px stays on the level chosen at 0.4");
    assert_ne!(
        seen[1].0, would_switch,
        "1.1 px would have switched on the hard cut"
    );
    assert_eq!(seen[2].0, at_fine, "2.0 px takes the hard cut");
    assert_ne!(seen[2].0, seen[0].0, "the level changed");
}

/// A cut while the error is still inside the band draws the hard cut, not the stored level.
#[test]
fn a_camera_cut_does_not_reuse_the_old_lod() {
    let device = crate::test_device();
    let view = view();
    let hard = hard_set(&device, &view, 1.1);
    let mut renderer = renderer(&device, Some(BAND_PX));
    let far = view.scene(view.eye_at(0.4));
    let (coarse, switches) = step(&mut renderer, &device, &far);
    assert_eq!(switches, 0);
    let near_eye = view.eye_at(1.1);
    assert!(eye_jump(view.eye_at(0.4), near_eye) < 40.0);
    let near = view.scene(near_eye);
    let (held, switches) = step(&mut renderer, &device, &near);
    assert_eq!(switches, 0);
    assert_eq!(held, coarse, "the band kept the coarse level");
    assert_ne!(held, hard, "the hard cut at 1.1 px is the other level");
    renderer.visbuffer_pass_mut().clear_lod_history();
    let (cut, switches) = step(&mut renderer, &device, &near);
    assert_eq!(switches, 0, "a cleared history is not a switch");
    assert_eq!(cut, hard, "the cut drew the hard cut");
    assert_ne!(cut, held, "the cut reused the stored level");
}

/// Several still frames, on the threshold and well off it, match a band of zero.
#[test]
fn a_still_camera_matches_the_hard_cut() {
    let device = crate::test_device();
    let view = view();
    for projected in [1.0_f32, 0.4] {
        let mut with_band = renderer(&device, None);
        let mut hard = renderer(&device, Some(0.0));
        let scene = view.scene(view.eye_at(projected));
        let mut first = None;
        for frame in 0..4 {
            let (band_set, band_switches, band_pixels) = step_with_pixels(&mut with_band, &device, &scene);
            let (hard_set, hard_switches, hard_pixels) = step_with_pixels(&mut hard, &device, &scene);
            assert_eq!(
                band_pixels, hard_pixels,
                "projected {projected} frame {frame}: still pixels changed"
            );
            assert_eq!(band_switches, 0, "projected {projected} frame {frame} switched");
            assert_eq!(
                hard_switches, 0,
                "hard cut projected {projected} frame {frame} switched"
            );
            assert_eq!(
                band_set, hard_set,
                "projected {projected} frame {frame}: hysteresis changed a still frame"
            );
            if let Some(earlier) = &first {
                assert_eq!(band_set, *earlier, "projected {projected} frame {frame} moved");
            } else {
                first = Some(band_set);
            }
        }
    }
}

/// A valid cut can select clusters from several levels of one instance simultaneously.
/// Distinct boundary errors deliberately put both groups inside the band at a still eye.
#[test]
fn a_still_mixed_level_cut_matches_the_hard_cut() {
    let device = crate::test_device();
    let view = view();
    let builder: MeshletBuilder = Arc::new(|vertices, indices| {
        // Two independent hierarchies with identical bounds but distinct shared boundary errors.
        // One group selects its coarse level and the other its fine level at the same eye.
        let mut mesh = two_level_builder(ERROR * 0.95)(vertices, indices)?;
        let other = two_level_builder(ERROR * 1.05)(vertices, indices)?;
        let vertex_offset = mesh.vertices.len() as u32;
        let triangle_offset = mesh.triangles.len() as u32;
        for cluster in other.meshlets {
            mesh.meshlets.push(super::super::Meshlet {
                vertex_offset: cluster.vertex_offset + vertex_offset,
                triangle_offset: cluster.triangle_offset + triangle_offset,
                ..cluster
            });
        }
        mesh.vertices.extend(other.vertices);
        mesh.triangles.extend(other.triangles);
        Some(mesh)
    });
    let mut band = renderer(&device, Some(BAND_PX));
    let mut hard = renderer(&device, Some(0.0));
    band.set_meshlet_builder(Some(builder.clone()));
    hard.set_meshlet_builder(Some(builder));
    let scene = view.scene(view.eye_at(1.0));
    let (expected, _) = step(&mut hard, &device, &scene);
    let (all_coarse, _) = step(&mut hard, &device, &view.scene(view.eye_at(0.4)));
    let (all_fine, _) = step(&mut hard, &device, &view.scene(view.eye_at(2.0)));
    assert_ne!(expected, all_coarse, "fixture must select some fine clusters");
    assert_ne!(expected, all_fine, "fixture must select some coarse clusters");
    for frame in 0..HOLD_FRAMES {
        let (actual, switches) = step(&mut band, &device, &scene);
        assert_eq!(actual, expected, "mixed cut changed at frame {frame}");
        assert_eq!(switches, 0);
    }
}

/// The range optimization must not leave stale boundary history on a skipped level.
#[test]
fn returning_from_beyond_the_band_keeps_the_new_cut() {
    let device = crate::test_device();
    let view = view();
    let fine = hard_set(&device, &view, 2.0);
    let mut renderer = renderer(&device, Some(BAND_PX));
    for (projected, expected_switches) in [(0.4, 0), (2.0, 1), (0.9, 0)] {
        let (set, switches) = step(&mut renderer, &device, &view.scene(view.eye_at(projected)));
        assert_eq!(switches, expected_switches);
        if projected > 0.4 {
            assert_eq!(set, fine, "the pruned coarse level reintroduced stale history");
        }
    }
}

/// Run after applying the external frame patch, which is owned by the integration lead.
#[test]
fn frame_camera_cut_does_not_reuse_the_old_lod() {
    let device = crate::test_device();
    let view = view();
    let hard = hard_set(&device, &view, 1.1);
    let mut renderer = renderer(&device, Some(BAND_PX));
    let (coarse, _) = step(&mut renderer, &device, &view.scene(view.eye_at(0.4)));
    let near = view.scene(view.eye_at(1.1));
    let (held, switches) = step(&mut renderer, &device, &near);
    assert_eq!(held, coarse);
    assert_eq!(switches, 0);
    assert_ne!(held, hard);
    renderer.camera_cut();
    let (cut, switches) = step(&mut renderer, &device, &near);
    assert_eq!(
        cut, hard,
        "the frame's explicit camera-cut signal must reach the pass"
    );
    assert_eq!(switches, 0);
}

/// Stable object identity must invalidate history independently of automatic camera cuts.
#[test]
fn frame_same_count_object_replacement_uses_the_hard_cut() {
    let device = crate::test_device();
    let view = view();
    let hard = hard_set(&device, &view, 1.1);
    let mut renderer = renderer(&device, Some(BAND_PX));
    renderer.set_camera_cut_detection(None);
    let (coarse, _) = step(&mut renderer, &device, &view.scene(view.eye_at(0.4)));
    let mut near = view.scene(view.eye_at(1.1));
    let (held, switches) = step(&mut renderer, &device, &near);
    assert_eq!(held, coarse);
    assert_eq!(switches, 0);
    near.meshes[0].object_key = 9;
    let (replacement, switches) = step(&mut renderer, &device, &near);
    assert_eq!(
        replacement, hard,
        "new object at the same slot must use the hard cut"
    );
    assert_eq!(switches, 0);
}

/// Object-space error of the split-screen fixture (ADR-8620): large enough that its two views, at
/// in-band distances on opposite sides of the mesh, stand further apart than the camera-cut jump,
/// while each view's own walk stays under it.
const SPLIT_ERROR: f32 = 0.5;

/// The eye on the +Z (`side` 1) or -Z (`side` -1) side where the split fixture's error projects
/// to `projected` pixels.
fn split_eye(view: &View, projected: f32, side: f32) -> [f32; 3] {
    let distance = SPLIT_ERROR * view.ppu / projected;
    [
        view.center[0],
        view.center[1],
        view.center[2] + side * (view.radius + distance),
    ]
}

/// A renderer drawing the split fixture: two-level clusters with [`SPLIT_ERROR`], no automatic
/// camera cut (the views look at the mesh from opposite sides; the test cuts explicitly).
fn split_renderer(device: &crate::RenderDevice, band: Option<f32>, occlusion: bool) -> FrameRenderer {
    let mut renderer = renderer(device, band);
    renderer.set_meshlet_builder(Some(two_level_builder(SPLIT_ERROR)));
    renderer.set_camera_cut_detection(None);
    renderer.set_visbuffer_settings(VisBufferSettings {
        occlusion,
        cluster_shadows: false,
        lod_hysteresis_px: band,
        ..VisBufferSettings::default()
    });
    renderer
}

/// The hard cut of the split fixture seen from `eye` (a fresh renderer, band 0).
fn split_hard(device: &crate::RenderDevice, view: &View, eye: [f32; 3]) -> BTreeSet<(u32, u32)> {
    let mut renderer = split_renderer(device, Some(0.0), false);
    renderer.set_occlusion_view(0);
    step(&mut renderer, device, &view.scene(eye)).0
}

/// One window view's frame of a split screen drawn by hand.
fn view_step(
    renderer: &mut FrameRenderer,
    device: &crate::RenderDevice,
    view: &View,
    index: usize,
    eye: [f32; 3],
) -> BTreeSet<(u32, u32)> {
    renderer.set_occlusion_view(index);
    let (set, switches) = step(renderer, device, &view.scene(eye));
    assert_eq!(
        switches, 0,
        "view {index}: no cut switched (the band holds both views)"
    );
    set
}

/// ADR-8620, per-view LOD hysteresis: two split-screen views at different distances, on opposite
/// sides of the mesh (further apart than the camera-cut jump), drawn alternately. View 0 walks
/// from 0.9 px (coarse) to 1.1 px and holds coarse; view 1 walks from 1.1 px (fine) to 0.9 px and
/// holds fine. With one history shared by the views (the base), every view change read as a
/// camera jump and discarded it, so each view drew the hard cut. With a history per view each
/// holds its level for 20 frames and nothing is discarded; a camera cut in view 1 discards view
/// 1's history only, and one view never copies a history.
#[test]
fn split_screen_views_keep_their_own_lod_history() {
    let device = crate::test_device();
    let view = view();
    let a_start = split_eye(&view, 0.9, 1.0);
    let a_eye = split_eye(&view, 1.1, 1.0);
    let b_start = split_eye(&view, 1.1, -1.0);
    let b_eye = split_eye(&view, 0.9, -1.0);
    let limit = crate::camera_cut::CameraCutDetection::default().max_eye_jump_m;
    assert!(
        eye_jump(a_start, a_eye) < limit && eye_jump(b_start, b_eye) < limit,
        "each walk stays under a cut"
    );
    assert!(
        eye_jump(a_eye, b_eye) > limit,
        "the views stand further apart than a cut"
    );
    let a_hard = split_hard(&device, &view, a_eye);
    let b_hard = split_hard(&device, &view, b_eye);

    let mut renderer = split_renderer(&device, Some(BAND_PX), false);
    let a_held = view_step(&mut renderer, &device, &view, 0, a_start);
    let b_held = view_step(&mut renderer, &device, &view, 1, b_start);
    assert_ne!(
        a_held, a_hard,
        "view 0's start level differs from its hard cut at 1.1 px"
    );
    assert_ne!(
        b_held, b_hard,
        "view 1's start level differs from its hard cut at 0.9 px"
    );
    let discards = renderer.visbuffer_stats().lod_history_discards;
    for frame in 1..=20 {
        let a = view_step(&mut renderer, &device, &view, 0, a_eye);
        assert_eq!(
            a, a_held,
            "frame {frame}: view 0 left its held level (base: the view change discarded it)"
        );
        let b = view_step(&mut renderer, &device, &view, 1, b_eye);
        assert_eq!(
            b, b_held,
            "frame {frame}: view 1 left its held level (base: the view change discarded it)"
        );
    }
    let stats = renderer.visbuffer_stats();
    assert_eq!(
        stats.lod_history_discards, discards,
        "a view change discards no history: {stats:?}"
    );
    assert!(
        stats.lod_history_copies >= 40,
        "each view change copies the histories: {stats:?}"
    );
    assert_eq!(
        stats.view_histories, 2,
        "each view owns its kept-aside history: {stats:?}"
    );

    // A cut in view 1 discards view 1's history only.
    renderer.set_occlusion_view(1);
    renderer.camera_cut();
    let b = view_step(&mut renderer, &device, &view, 1, b_eye);
    assert_eq!(b, b_hard, "view 1's cut drew its hard cut");
    let a = view_step(&mut renderer, &device, &view, 0, a_eye);
    assert_eq!(a, a_held, "view 0 kept its history through view 1's cut");
    let after = renderer.visbuffer_stats();
    assert_eq!(
        after.lod_history_discards,
        discards + 1,
        "one history discarded: {after:?}"
    );

    // One view never copies a history.
    let mut single = split_renderer(&device, Some(BAND_PX), false);
    for eye in [a_start, a_eye, a_eye, a_eye] {
        view_step(&mut single, &device, &view, 0, eye);
    }
    let stats = single.visbuffer_stats();
    assert_eq!(
        (stats.lod_history_copies, stats.view_histories),
        (0, 0),
        "{stats:?}"
    );
}

/// Phase 1's cluster count of the last visibility frame.
fn phase1_clusters(renderer: &mut FrameRenderer, device: &crate::RenderDevice) -> u32 {
    let lists = renderer
        .visbuffer_pass_mut()
        .read_cull_lists(device)
        .expect("readback")
        .expect("a visibility frame ran");
    lists.state[1] + lists.state[17]
}

/// ADR-8620, retirement: three split-screen views draw for 5 frames, then only view 0 for
/// `OCCLUSION_VIEW_RETIRE_ROUNDS + 1` frames, with no `retain_occlusion_views` call. Views 1 and 2
/// give back their cluster bits and LOD history (none is kept aside), and the renderer counts
/// two retirements. View 1 drawing again starts as a new view: phase 1 draws nothing (its bits
/// start at zero), and it draws the clusters and pixels a fresh renderer draws from that eye.
#[test]
fn an_undrawn_view_gives_back_its_bits_and_lod_history() {
    let device = crate::test_device();
    let view = view();
    let eyes = [
        split_eye(&view, 1.1, 1.0),
        split_eye(&view, 0.9, -1.0),
        split_eye(&view, 2.0, 1.0),
    ];
    let mut renderer = split_renderer(&device, Some(BAND_PX), true);
    for _ in 0..5 {
        for (index, eye) in eyes.iter().enumerate() {
            view_step(&mut renderer, &device, &view, index, *eye);
        }
    }
    let stats = renderer.visbuffer_stats();
    assert_eq!(
        stats.view_histories, 3,
        "each of the three views owns its kept-aside history: {stats:?}"
    );
    assert!(stats.view_history_bytes > 0, "{stats:?}");
    assert!(
        phase1_clusters(&mut renderer, &device) > 0,
        "a settled view's phase 1 draws"
    );
    for _ in 0..=crate::OCCLUSION_VIEW_RETIRE_ROUNDS {
        view_step(&mut renderer, &device, &view, 0, eyes[0]);
    }
    let stats = renderer.visbuffer_stats();
    assert_eq!(
        (stats.view_histories, stats.view_history_bytes),
        (0, 0),
        "views 1 and 2 gave their bits and LOD history back: {stats:?}"
    );
    assert_eq!(renderer.retired_occlusion_views(), 2);

    // View 1 comes back as a new view.
    renderer.set_occlusion_view(1);
    let back = render(&mut renderer, &device, &view.scene(eyes[1]));
    assert_eq!(
        phase1_clusters(&mut renderer, &device),
        0,
        "a returning view's phase 1 draws nothing"
    );
    let lists = renderer
        .visbuffer_pass_mut()
        .read_cull_lists(&device)
        .expect("readback")
        .expect("a visibility frame ran");
    let mut fresh = split_renderer(&device, Some(BAND_PX), true);
    fresh.set_occlusion_view(0);
    let fresh_pixels = render(&mut fresh, &device, &view.scene(eyes[1]));
    let fresh_lists = fresh
        .visbuffer_pass_mut()
        .read_cull_lists(&device)
        .expect("readback")
        .expect("a visibility frame ran");
    assert_eq!(
        drawn(&lists),
        drawn(&fresh_lists),
        "the returning view draws a new view's clusters"
    );
    let (worst, close) = diff(&back, &fresh_pixels);
    assert!(
        close >= close_floor(&device, 0.99),
        "the returning view draws a new view's pixels: worst {worst}, close {close}"
    );
}
