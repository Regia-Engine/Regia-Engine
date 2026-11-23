// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion of the forward depth prepass (`docs/specs/98-visibility-buffer.md`,
//! ADR-1887), on the headless device (lavapipe in CI).
//!
//! A wall stands in front of a field of 2 304 small boxes. Two renderers draw the same frames,
//! one with the two-phase prepass and one without; every frame must match byte for byte, since
//! the two phases write the depth the unculled prepass writes. The GPU counters show the cull
//! working: the first frame has no pyramid history (phase 1 draws everything in the frustum),
//! the second skips the boxes behind the wall, and when the wall moves away the boxes phase 1
//! still skips (last frame's pyramid hid them) are drawn by phase 2.

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    IndirectStrategy, LabMaterial, LabVertex, PrepassOcclusionCounters, PrepassOcclusionSkip, RenderDevice,
    RenderProfile, RenderProfilePreset, box_mesh,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

const SIZE: u32 = 96;
/// Boxes per side of the field (48² = 2 304, above the 2 048-instance Hi-Z threshold).
const FIELD: usize = 48;

struct Geometry {
    wall: (Vec<LabVertex>, Vec<u32>),
    cube: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
    masked: LabMaterial,
    two_sided: LabMaterial,
}

impl Geometry {
    fn new() -> Self {
        let material = LabMaterial {
            base_color: [0.6, 0.55, 0.5, 1.0],
            roughness: 0.7,
            ..LabMaterial::default()
        };
        Self {
            wall: box_mesh([2.5, 2.5, 0.1]),
            cube: box_mesh([0.06, 0.06, 0.06]),
            masked: LabMaterial {
                alpha_mode: ALPHA_MASK,
                base_color: [0.5, 0.6, 0.5, 0.9],
                ..material.clone()
            },
            two_sided: LabMaterial {
                double_sided: true,
                base_color: [0.5, 0.5, 0.6, 1.0],
                ..material.clone()
            },
            material,
        }
    }

    fn mesh<'g>(
        &'g self,
        key: u64,
        mesh_key: u64,
        geometry: &'g (Vec<LabVertex>, Vec<u32>),
        at: Vec3,
    ) -> FrameMesh<'g> {
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 1,
            object_key: key,
            vertices: geometry.0.as_slice(),
            indices: geometry.1.as_slice(),
            material: &self.material,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::Off,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    /// The wall at `wall_x`, then the field behind it: x in [-4.7, 4.7], z in [-6, -1].
    fn meshes(&self, wall_x: f32) -> Vec<FrameMesh<'_>> {
        let mut out = vec![self.mesh(1, 1, &self.wall, Vec3::new(wall_x, 1.0, 2.0))];
        for i in 0..FIELD * FIELD {
            let (col, row) = ((i % FIELD) as f32, (i / FIELD) as f32);
            let at = Vec3::new(-4.7 + col * 0.2, 0.2 + (i % 7) as f32 * 0.25, -1.0 - row * 0.1);
            out.push(self.mesh(i as u64 + 10, 2, &self.cube, at));
        }
        out
    }

    /// Every third box alpha-tested and every fifth double-sided, so the cull's multi-draws run
    /// through the prepass's masked and cull-none pipelines too.
    fn mixed<'g>(&'g self, mut meshes: Vec<FrameMesh<'g>>) -> Vec<FrameMesh<'g>> {
        for (i, mesh) in meshes.iter_mut().enumerate().skip(1) {
            if i.is_multiple_of(3) {
                mesh.material = &self.masked;
                mesh.material_key = 2;
            } else if i.is_multiple_of(5) {
                mesh.material = &self.two_sided;
                mesh.material_key = 3;
            }
        }
        meshes
    }
}

fn scene<'a>(meshes: Vec<FrameMesh<'a>>, quality: FrameQuality) -> FrameScene<'a> {
    FrameScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 1.0, 6.0],
        target: [0.0, 1.0, 0.0],
        fov_y: 1.1,
        near: 0.1,
        far: 60.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.5,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.4, -1.0, -0.3], [1.0, 1.0, 1.0], 1.5),
        local_lights: &[],
        quality,
        meshes,
    }
}

fn quality() -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
}

/// `None` when the device cannot run the GPU-driven prepass (no multi-draw indirect with a
/// first instance): the cull never runs there.
///
/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> Option<SerialGpu> {
    let device = serial_device().expect("headless device (lavapipe)");
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip: the device draws no GPU objects (no multi-draw indirect with a first instance)");
        return None;
    }
    Some(device)
}

fn counters(device: &RenderDevice, renderer: &FrameRenderer) -> PrepassOcclusionCounters {
    renderer
        .prepass_occlusion_counters(device)
        .expect("counter readback")
        .expect("a two-phase frame ran")
}

fn mismatched(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(x, y)| x != y).count()
}

/// Four frames, two renderers, `taau` on or off; returns each frame's phase counts after
/// checking that every frame matches the unculled renderer byte for byte.
fn four_frames(device: &RenderDevice, taau: bool, mixed: bool) -> [PrepassOcclusionCounters; 4] {
    let geometry = Geometry::new();
    let mut culled = FrameRenderer::new();
    culled.set_two_phase_occlusion(true);
    let mut plain = FrameRenderer::new();
    assert!(culled.two_phase_occlusion());
    assert!(!plain.two_phase_occlusion());
    let quality = FrameQuality { taau, ..quality() };
    let mut frame = |wall_x: f32, label: &str| {
        let meshes = geometry.meshes(wall_x);
        let meshes = if mixed { geometry.mixed(meshes) } else { meshes };
        let scene = scene(meshes, quality);
        let a = culled.render_offscreen(device, &scene).expect("two-phase frame");
        let b = plain.render_offscreen(device, &scene).expect("plain frame");
        assert_eq!(
            mismatched(&a.pixels, &b.pixels),
            0,
            "taau {taau}, {label}: the two-phase prepass changed the frame"
        );
        let stats = culled.prepass_occlusion_stats();
        assert!(stats.active, "{label}: the two-phase cull did not run: {stats:?}");
        // The field, plus the wall while it is in view (the CPU frustum drops it once it moves
        // away, before the depth list is built).
        let wall = usize::from(wall_x.abs() < 10.0);
        assert_eq!(
            stats.instances as usize,
            FIELD * FIELD + wall,
            "{label}: {stats:?}"
        );
        assert_eq!(stats.unbounded, 0, "{label}: {stats:?}");
        assert_eq!(
            plain.prepass_occlusion_stats().skipped,
            Some(PrepassOcclusionSkip::Off)
        );
        let counts = counters(device, &culled);
        eprintln!("taau {taau}, {label}: {counts:?}");
        (stats, counts)
    };

    // First frame: no pyramid yet, so phase 1 draws everything in the frustum.
    let (stats, first) = frame(0.0, "first frame");
    assert!(!stats.history, "{stats:?}");
    assert_eq!(first.phase2, 0, "{first:?}");

    // Second frame, same view: the boxes behind the wall are skipped, and phase 2 finds none of
    // them visible.
    let (stats, second) = frame(0.0, "second frame");
    assert!(stats.history, "{stats:?}");
    assert_eq!(second.phase2, 0, "{second:?}");
    assert!(
        second.phase1 + 200 < first.phase1,
        "the wall hid too few boxes: {first:?} then {second:?}"
    );

    // The wall moves out of view: last frame's pyramid still hides the boxes for phase 1, and
    // phase 2 (tested against this frame's depth) draws them.
    let (stats, third) = frame(40.0, "wall moved away");
    assert!(stats.history, "{stats:?}");
    assert!(
        third.phase2 > 200,
        "phase 2 missed the uncovered boxes: {third:?}"
    );

    let (_, fourth) = frame(40.0, "settled");
    [first, second, third, fourth]
}

#[test]
fn two_phase_prepass_matches_the_unculled_frame_and_culls_hidden_instances() {
    let Some(device) = device() else {
        return;
    };
    // Without the temporal jitter a held view rasterizes the same depth every frame, so the
    // settled frame's phase 1 (last frame's pyramid) and phase 2 (its own) agree exactly.
    let [.., third, fourth] = four_frames(&device, false, false);
    assert_eq!(fourth.phase2, 0, "{third:?} then {fourth:?}");
    assert!(
        fourth.phase1 >= third.phase1 + third.phase2 - fourth.culled,
        "{third:?} then {fourth:?}"
    );
}

#[test]
fn two_phase_prepass_stays_exact_across_the_depth_pipelines() {
    let Some(device) = device() else {
        return;
    };
    let [.., third, fourth] = four_frames(&device, false, true);
    assert_eq!(fourth.phase2, 0, "{third:?} then {fourth:?}");
}

#[test]
fn two_phase_prepass_stays_exact_under_the_temporal_jitter() {
    let Some(device) = device() else {
        return;
    };
    // With TAAU the depth moves by up to half a pixel each frame while the cull projects without
    // the jitter (the one-pixel margin keeps it exact). Last frame's pyramid then differs a
    // little from this frame's, so a held view still finds a few boxes in phase 2, but most of
    // the uncovered field is back in phase 1.
    let [.., fourth] = four_frames(&device, true, false);
    assert!(fourth.phase2 * 4 < fourth.phase1, "{fourth:?}");
}

/// The colour pass's cull (ADR-1238) tests the unjittered projection against depth rasterized
/// with the temporal jitter and has no margin (ADR-1887 (7)). Its sphere bound is loose enough
/// that the wall's edge through the field loses no pixel: with Hi-Z occlusion on or off, the
/// jittered frames match byte for byte. A guard, should that bound ever tighten.
#[test]
fn colour_cull_stays_exact_under_the_temporal_jitter() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let quality = FrameQuality {
        taau: true,
        ..quality()
    };
    let mut culled = FrameRenderer::new();
    let mut open = FrameRenderer::new();
    open.set_hiz_occlusion(false);
    for frame in 0..8 {
        let scene = scene(geometry.meshes(0.0), quality);
        let a = culled.render_offscreen(&device, &scene).expect("culled frame");
        let b = open.render_offscreen(&device, &scene).expect("open frame");
        assert_eq!(
            mismatched(&a.pixels, &b.pixels),
            0,
            "frame {frame}: the colour cull changed the jittered frame"
        );
    }
}

/// Two-phase occlusion is a frame-graph node (ADR-4905): switched on, the frame graph holds
/// `prepass_occlusion` right ahead of the depth prepass and the frame records it through the
/// graph's dispatch; switched off, the node leaves the graph, nothing records it and it creates
/// nothing (no phase buffers, so no counters to read), with no call of its own in `frame.rs`.
#[test]
fn two_phase_occlusion_is_added_and_removed_as_a_graph_node() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let dense = scene(geometry.meshes(0.0), quality());
    let holds = |r: &FrameRenderer| {
        r.graph_report()
            .graph
            .nodes
            .iter()
            .any(|n| n.name == "prepass_occlusion")
    };
    let mut renderer = FrameRenderer::new();
    // Off (the default), on dense frames that would take it.
    for _ in 0..2 {
        renderer.render_offscreen(&device, &dense).expect("off frame");
    }
    assert!(!holds(&renderer));
    let report = renderer.graph_report();
    assert!(!report.dispatch.recorded.contains(&"prepass_occlusion"));
    assert!(report.dispatch.recorded.contains(&"depth_prepass"));
    assert_eq!(
        report.dispatch.violations, 0,
        "{:?}",
        report.dispatch.last_violation
    );
    assert!(
        renderer
            .prepass_occlusion_counters(&device)
            .expect("readback")
            .is_none(),
        "the absent node created its phase buffers"
    );
    assert_eq!(
        renderer.prepass_occlusion_stats().skipped,
        Some(PrepassOcclusionSkip::Off)
    );

    // On: the node joins the graph ahead of the prepass and records.
    renderer.set_two_phase_occlusion(true);
    assert!(holds(&renderer));
    renderer.render_offscreen(&device, &dense).expect("on frame");
    let report = renderer.graph_report();
    let at = |name: &str| report.graph.order.iter().position(|n| n == name);
    assert_eq!(at("prepass_occlusion").map(|i| i + 1), at("depth_prepass"));
    let recorded = &report.dispatch.recorded;
    let rank = |name: &str| recorded.iter().position(|n| *n == name);
    assert!(rank("prepass_occlusion") < rank("depth_prepass"), "{recorded:?}");
    assert!(rank("prepass_occlusion").is_some(), "{recorded:?}");
    assert_eq!(
        report.dispatch.violations, 0,
        "{:?}",
        report.dispatch.last_violation
    );
    assert!(
        renderer.prepass_occlusion_stats().active,
        "{:?}",
        renderer.prepass_occlusion_stats()
    );
    counters(&device, &renderer);

    // Off again: the node leaves the graph and is not recorded.
    renderer.set_two_phase_occlusion(false);
    assert!(!holds(&renderer));
    renderer.render_offscreen(&device, &dense).expect("off again");
    let report = renderer.graph_report();
    assert!(!report.dispatch.recorded.contains(&"prepass_occlusion"));
    assert_eq!(
        report.dispatch.violations, 0,
        "{:?}",
        report.dispatch.last_violation
    );
    let stats = renderer.prepass_occlusion_stats();
    assert!(!stats.active && !stats.enabled, "{stats:?}");
    assert_eq!(stats.skipped, Some(PrepassOcclusionSkip::Off));
}

#[test]
fn small_or_switched_off_frames_skip_with_a_reason() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let mut renderer = FrameRenderer::new();
    let mut meshes = geometry.meshes(0.0);
    meshes.truncate(64);
    renderer
        .render_offscreen(&device, &scene(meshes.clone(), quality()))
        .expect("frame");
    let stats = renderer.prepass_occlusion_stats();
    assert!(!stats.enabled && !stats.active);
    assert_eq!(stats.skipped, Some(PrepassOcclusionSkip::Off));

    renderer.set_two_phase_occlusion(true);
    renderer
        .render_offscreen(&device, &scene(meshes.clone(), quality()))
        .expect("frame");
    let stats = renderer.prepass_occlusion_stats();
    assert!(stats.enabled && !stats.active);
    assert_eq!(stats.skipped, Some(PrepassOcclusionSkip::FewInstances));

    let mut no_prepass = scene(meshes, quality());
    no_prepass.depth_prepass = false;
    renderer.render_offscreen(&device, &no_prepass).expect("frame");
    assert_eq!(
        renderer.prepass_occlusion_stats().skipped,
        Some(PrepassOcclusionSkip::NoPrepass)
    );

    renderer.set_hiz_occlusion(false);
    renderer
        .render_offscreen(&device, &scene(geometry.meshes(0.0), quality()))
        .expect("frame");
    assert_eq!(
        renderer.prepass_occlusion_stats().skipped,
        Some(PrepassOcclusionSkip::HizOff)
    );
    assert_eq!(renderer.prepass_occlusion_stats().frames, 0);
}

#[test]
fn profile_column_and_override_pick_the_switch() {
    use regia_render::RenderSettings;
    for (preset, on) in [
        (RenderProfilePreset::MobileLow, false),
        (RenderProfilePreset::MobileHigh, false),
        (RenderProfilePreset::DesktopLow, false),
        (RenderProfilePreset::DesktopHigh, true),
        (RenderProfilePreset::DesktopUltra, true),
        (RenderProfilePreset::Deck, false),
    ] {
        assert_eq!(
            RenderProfile::from_preset(preset).two_phase_occlusion,
            on,
            "{preset:?}"
        );
        let mut settings = RenderSettings::from_profile(preset);
        assert_eq!(settings.effective_two_phase_occlusion(), on, "{preset:?}");
        settings.two_phase_occlusion = Some(!on);
        assert_eq!(settings.effective_two_phase_occlusion(), !on, "{preset:?}");
        settings.two_phase_occlusion = None;
        assert_eq!(settings.effective_two_phase_occlusion(), on, "{preset:?}");
    }
}

/// The prepass's GPU time with the switch on and off on the occluded field (`--ignored`; prints,
/// asserts nothing about time). `prepass_ms` spans phase 1's cull and depth pass, the extra
/// pyramid, phase 2's cull and depth pass; `cull_ms` the colour cull after it. On lavapipe this
/// is a correctness-path number only: the lead runs it on the 5080 and the 7900 at a real size
/// (`REGIA_TWO_PHASE_SIZE=2560x1440`).
#[test]
#[ignore = "timing printout; run on the bench GPU"]
fn two_phase_prepass_cost() {
    let Some(device) = device() else {
        return;
    };
    let (width, height) = std::env::var("REGIA_TWO_PHASE_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((SIZE, SIZE));
    let geometry = Geometry::new();
    for on in [false, true, false, true] {
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        renderer.set_two_phase_occlusion(on);
        let mut scene = scene(geometry.meshes(0.0), quality());
        scene.width = width;
        scene.height = height;
        // Warm-up: pipelines, the pyramid history, the timer ring.
        for _ in 0..8 {
            renderer.render_offscreen(&device, &scene).expect("warm-up");
        }
        let frames = 24;
        let (mut prepass, mut cull) = (0.0f64, 0.0f64);
        for _ in 0..frames {
            renderer.render_offscreen(&device, &scene).expect("frame");
            let t = renderer.gpu_timings();
            prepass += f64::from(t.prepass_ms);
            cull += f64::from(t.cull_ms);
        }
        let counts = on.then(|| counters(&device, &renderer));
        eprintln!(
            "two_phase={on} {width}x{height}: prepass {:.3} ms, colour cull {:.3} ms (mean of {frames}); phases {counts:?}",
            prepass / f64::from(frames),
            cull / f64::from(frames),
        );
    }
}

/// Diagnostics (`--ignored`): per-frame phase counts over a settled view, alone and with a
/// second renderer drawing between frames.
#[test]
#[ignore = "diagnostic printout"]
fn two_phase_settled_counts() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    for interleave in [false, true] {
        let mut culled = FrameRenderer::new();
        culled.set_two_phase_occlusion(true);
        let mut plain = FrameRenderer::new();
        for (i, wall_x) in [0.0, 0.0, 40.0, 40.0, 40.0, 40.0, 40.0, 40.0]
            .into_iter()
            .enumerate()
        {
            let scene = scene(geometry.meshes(wall_x), quality());
            culled.render_offscreen(&device, &scene).expect("frame");
            if interleave {
                plain.render_offscreen(&device, &scene).expect("frame");
            }
            eprintln!(
                "interleave={interleave} frame {i} wall {wall_x}: {:?} drawn {:?}",
                counters(&device, &culled),
                culled.draw_stats()
            );
        }
    }
}

/// `ViewVisibility` folds in the two-phase cull's result (spec 14 § A1, ADR-7255): an ECS world
/// of the same wall and field, extracted each frame, rendered by a renderer with two-phase
/// occlusion and the [`OcclusionFeedback`] handle. A box straight behind the wall reads false once
/// occlusion settles (frame 3: frame 2 is the first to test a pyramid, and its readback lands
/// for the next frame), stays true for a box beside the wall, and reads true one frame after
/// the wall moves away. The test drains the GPU between frames (a GPU that keeps up); the engine
/// itself only polls without waiting. `render.visible_entities`' occlusion count follows it.
#[test]
fn view_visibility_reads_false_behind_an_occluder_and_true_one_frame_after_it_is_uncovered() {
    use regia_app::prelude::*;
    use regia_render::{
        Camera, CpuMaterial, CpuMesh, ExtractedFrame, LodViewport, MeshRenderer, OcclusionFeedback,
        ViewVisibilityStats, extract_frame, update_view_visibility,
    };
    use regia_scene::Transform;
    use regia_scene::components::ViewVisibility;
    use regia_scene::propagate::propagate_transforms;
    use std::sync::Arc;

    let Some(device) = device() else {
        return;
    };
    let mesh = |(vertices, indices): (Vec<LabVertex>, Vec<u32>)| Arc::new(CpuMesh { vertices, indices });
    let material = Arc::new(CpuMaterial {
        base_color: [0.6, 0.55, 0.5, 1.0],
        roughness: 0.7,
        ..CpuMaterial::default()
    });
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<OcclusionFeedback>();
    app.insert_resource(LodViewport {
        width: SIZE,
        height: SIZE,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, (extract_frame, update_view_visibility).chain());
    app.world_mut().spawn((
        Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, 6.0)),
        Camera {
            fov_y: 1.1,
            near: 0.1,
            far: 60.0,
            exposure_ev: Some(0.0),
            ..Camera::default()
        },
    ));
    let wall = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, 2.0)),
            MeshRenderer::new(mesh(box_mesh([2.5, 2.5, 0.1])), Arc::clone(&material)),
        ))
        .id();
    let cube = mesh(box_mesh([0.06, 0.06, 0.06]));
    for i in 0..FIELD * FIELD {
        let (col, row) = ((i % FIELD) as f32, (i / FIELD) as f32);
        let at = regia_core::math::Vec3::new(-4.7 + col * 0.2, 0.2 + (i % 7) as f32 * 0.25, -1.0 - row * 0.1);
        app.world_mut().spawn((
            Transform::from_translation(at),
            MeshRenderer::new(Arc::clone(&cube), Arc::clone(&material)),
        ));
    }
    // Straight behind the wall's centre.
    let hidden = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, -3.0)),
            MeshRenderer::new(Arc::clone(&cube), Arc::clone(&material)),
        ))
        .id();
    let mut renderer = FrameRenderer::new();
    renderer.set_two_phase_occlusion(true);
    let handle = app.world().resource::<OcclusionFeedback>().clone();
    renderer.set_occlusion_feedback(Some(handle));
    let quality = quality();
    let mut frame = |app: &mut App, label: &str| {
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let mut scene = extracted.frame_scene(0, SIZE, SIZE);
        scene.depth_prepass = true;
        scene.quality = quality;
        scene.post = None;
        renderer.render_offscreen(&device, &scene).expect("frame");
        assert!(
            renderer.prepass_occlusion_stats().active,
            "{label}: the two-phase cull ran: {:?}",
            renderer.prepass_occlusion_stats()
        );
        // A GPU that keeps up: the readback of this frame is mapped before the next update.
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let visible = |app: &App, entity: Entity| app.world().get::<ViewVisibility>(entity).is_some_and(|v| v.0);

    // Frame 1 has no pyramid history (nothing occluded yet); frame 2 culls; frame 3 reads it.
    frame(&mut app, "frame 1");
    assert!(
        visible(&app, hidden),
        "frame 1: no occlusion result yet, in the frustum"
    );
    frame(&mut app, "frame 2");
    frame(&mut app, "frame 3");
    assert!(visible(&app, wall), "the wall itself is seen");
    assert!(
        !visible(&app, hidden),
        "settled: the box behind the wall is not view-visible"
    );
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert!(stats.occluded >= 1, "the occlusion count includes it: {stats:?}");

    // A steady frame writes nothing.
    frame(&mut app, "frame 4");
    assert_eq!(app.world().resource::<ViewVisibilityStats>().writes, 0);

    // The wall moves out of view. The frame that moves it still reads the last frame's result,
    // and draws the box (phase 2); the next frame reads that.
    if let Some(mut transform) = app.world_mut().get_mut::<Transform>(wall) {
        transform.translation.x = 40.0;
    }
    frame(&mut app, "uncovered");
    assert!(
        !visible(&app, hidden),
        "the frame that uncovers it reads last frame's occlusion (the one-frame lag)"
    );
    frame(&mut app, "one frame after");
    assert!(
        visible(&app, hidden),
        "one frame after it is uncovered, the box is view-visible"
    );
}

/// The ECS world of the multi-view tests: a wall, the 2 304-box field behind it (above the cull's
/// instance threshold), and the extraction plus `ViewVisibility` systems with an
/// [`regia_render::OcclusionFeedback`]. Returns the app and a spawner for small boxes.
mod multi_view {
    use super::*;
    use regia_app::prelude::*;
    use regia_render::{
        Camera, CpuMaterial, CpuMesh, ExtractedFrame, LodViewport, MeshRenderer, OcclusionFeedback,
        ViewVisibilityPerView, ViewVisibilityStats, extract_frame, update_view_visibility,
    };
    use regia_scene::Transform;
    use regia_scene::components::ViewVisibility;
    use regia_scene::propagate::propagate_transforms;
    use std::sync::Arc;

    pub(super) struct World3 {
        pub(super) app: App,
        material: Arc<CpuMaterial>,
        cube: Arc<CpuMesh>,
    }

    fn mesh((vertices, indices): (Vec<LabVertex>, Vec<u32>)) -> Arc<CpuMesh> {
        Arc::new(CpuMesh { vertices, indices })
    }

    impl World3 {
        /// The world with `window` as the window size, the wall in front of the field.
        pub(super) fn new(window: [u32; 2]) -> Self {
            let material = Arc::new(CpuMaterial {
                base_color: [0.6, 0.55, 0.5, 1.0],
                roughness: 0.7,
                ..CpuMaterial::default()
            });
            let mut app = App::new();
            app.init_resource::<ExtractedFrame>();
            app.init_resource::<ViewVisibilityStats>();
            app.init_resource::<ViewVisibilityPerView>();
            app.init_resource::<OcclusionFeedback>();
            app.insert_resource(LodViewport {
                width: window[0],
                height: window[1],
            });
            app.add_systems(PostUpdate, propagate_transforms);
            app.add_systems(Render, (extract_frame, update_view_visibility).chain());
            let cube = mesh(box_mesh([0.06, 0.06, 0.06]));
            let mut world = Self { app, material, cube };
            world.solid([0.0, 1.0, 2.0], [2.5, 2.5, 0.1]);
            for i in 0..FIELD * FIELD {
                let (col, row) = ((i % FIELD) as f32, (i / FIELD) as f32);
                world.cube([-4.7 + col * 0.2, 0.2 + (i % 7) as f32 * 0.25, -1.0 - row * 0.1]);
            }
            world
        }

        /// A box of half extents `half` at `at`.
        pub(super) fn solid(&mut self, at: [f32; 3], half: [f32; 3]) -> Entity {
            let mesh = mesh(box_mesh(half));
            self.app
                .world_mut()
                .spawn((
                    Transform::from_translation(regia_core::math::Vec3::from_array(at)),
                    MeshRenderer::new(mesh, Arc::clone(&self.material)),
                ))
                .id()
        }

        /// One of the field's small boxes at `at`.
        pub(super) fn cube(&mut self, at: [f32; 3]) -> Entity {
            self.app
                .world_mut()
                .spawn((
                    Transform::from_translation(regia_core::math::Vec3::from_array(at)),
                    MeshRenderer::new(Arc::clone(&self.cube), Arc::clone(&self.material)),
                ))
                .id()
        }

        /// A level camera at `eye` looking at `target` (a turn about +Y from the default -Z
        /// forward), with `camera`'s other fields.
        pub(super) fn camera(&mut self, eye: [f32; 3], target: [f32; 3], camera: Camera) -> Entity {
            let (fx, fz) = (target[0] - eye[0], target[2] - eye[2]);
            let transform = Transform {
                rotation: regia_core::math::Quat::from_rotation_y((-fx).atan2(-fz)),
                ..Transform::from_translation(regia_core::math::Vec3::from_array(eye))
            };
            self.app.world_mut().spawn((transform, camera)).id()
        }

        pub(super) fn visible(&self, entity: Entity) -> bool {
            self.app
                .world()
                .get::<ViewVisibility>(entity)
                .is_some_and(|v| v.0)
        }

        pub(super) fn handle(&self) -> OcclusionFeedback {
            self.app.world().resource::<OcclusionFeedback>().clone()
        }
    }

    /// Our camera fields: the test lens, everything else default.
    pub(super) fn lens(fov_y: f32) -> Camera {
        Camera {
            fov_y,
            near: 0.1,
            far: 60.0,
            exposure_ev: Some(0.0),
            ..Camera::default()
        }
    }
}

/// ADR-7286, split screen: two window views side by side on one renderer, each culling its
/// prepass in two phases into its own readback ring. View A looks at the wall from the front;
/// view B looks across the scene from the side, where a second wall (which A sees too) stands
/// in front of the middle. A box behind both walls reads not visible once occlusion settles; a
/// box the front wall hides from A but B sees reads visible (any view that sees it counts).
///
/// B's occluder is one A sees on purpose: the views share one depth-pyramid history, so B's
/// phase 1 tests A's pyramid, and an occluder of B that A cannot see is skipped in B's phase 1
/// and missing from the pyramid B's phase 2 tests (ADR-7286 known issue; still conservative).
#[test]
fn split_screen_view_visibility_folds_each_views_occlusion() {
    use multi_view::{World3, lens};
    use regia_render::{Camera, ExtractedFrame, ViewVisibilityPerView};

    let Some(device) = device() else {
        return;
    };
    let mut world = World3::new([SIZE * 2, SIZE]);
    world.camera(
        [0.0, 1.0, 6.0],
        [0.0, 1.0, 0.0],
        Camera {
            viewport: [0.0, 0.0, 0.5, 1.0],
            ..lens(1.1)
        },
    );
    world.camera(
        [12.0, 2.8, -3.0],
        [0.0, 2.8, -3.0],
        Camera {
            viewport: [0.5, 0.0, 0.5, 1.0],
            ..lens(1.1)
        },
    );
    // The side wall: covers the middle as B sees it, beside the front wall as A sees it.
    world.solid([6.0, 2.8, -3.0], [0.1, 1.0, 1.0]);
    let both = world.cube([0.0, 2.8, -3.0]);
    let a_only = world.cube([4.0, 2.8, -6.0]);
    let front = world.cube([3.6, 2.8, 3.0]);
    let mut renderer = FrameRenderer::new();
    renderer.set_two_phase_occlusion(true);
    renderer.set_occlusion_feedback(Some(world.handle()));
    let quality = quality();
    let mut frame = |world: &mut World3, label: &str| {
        world.app.update();
        let extracted = world.app.world().resource::<ExtractedFrame>();
        let views = extracted.window_views();
        assert_eq!(views.len(), 2, "{label}: two window views");
        let target = renderer.offscreen_target(&device, SIZE * 2, SIZE, true);
        for (index, (view_index, _)) in views.iter().enumerate() {
            let mut scene = extracted.frame_scene(*view_index, SIZE, SIZE);
            scene.depth_prepass = true;
            scene.quality = quality;
            scene.post = None;
            let rect = [index as u32 * SIZE, 0, SIZE, SIZE];
            let frame_target = regia_render::FrameTarget::rect(
                &target.view,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                SIZE * 2,
                SIZE,
                rect,
                index > 0,
            );
            renderer.set_occlusion_view(*view_index);
            renderer.render_into(&device, &scene, frame_target).expect("view");
            assert!(
                renderer.prepass_occlusion_stats().active,
                "{label}: view {index} culled in two phases: {:?}",
                renderer.prepass_occlusion_stats()
            );
        }
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    for label in ["frame 1", "frame 2", "frame 3", "frame 4"] {
        frame(&mut world, label);
    }
    assert!(world.visible(front), "a box in plain sight of A is visible");
    assert!(
        world.visible(a_only),
        "the front wall hides it from A, but B sees it: visible"
    );
    assert!(
        !world.visible(both),
        "behind a wall in both views: not visible once occlusion settles"
    );
    let per_view = world.app.world().resource::<ViewVisibilityPerView>().clone();
    assert_eq!(per_view.views.len(), 2, "{per_view:?}");
    assert!(
        per_view
            .views
            .iter()
            .all(|line| line.occlusion && line.occluded >= 1),
        "each view folded its own result and hides something: {per_view:?}"
    );
    let mut rings = Vec::new();
    world.handle().view_stats_into(&mut rings);
    assert_eq!(
        rings.iter().map(|ring| ring.view).collect::<Vec<_>>(),
        [0, 1],
        "one readback ring per view: {rings:?}"
    );
    assert!(rings.iter().all(|ring| ring.stats.read >= 2), "{rings:?}");
}

/// ADR-7286: a render-to-texture camera counts as a view, frustum-only (a texture view never
/// culls in two phases), and does not stop the window view's occlusion from counting. The
/// texture camera is spawned first, so the window view is view 1. A box the wall hides from the
/// window view and outside the texture camera's frustum reads not visible; one the wall hides
/// from the window view but in front of the texture camera reads visible.
#[test]
fn a_texture_camera_counts_as_a_frustum_only_view_beside_the_occlusion_culled_window() {
    use multi_view::{World3, lens};
    use regia_render::{Camera, ExtractedFrame, RenderTarget, TextureViewSettings, ViewVisibilityPerView};

    let Some(device) = device() else {
        return;
    };
    const TEXTURE: u64 = 0x7286;
    let mut world = World3::new([SIZE, SIZE]);
    // A narrow texture camera between the wall and the field, looking down -z at x = 4.
    let texture_camera = world.camera(
        [4.0, 2.8, 0.0],
        [4.0, 2.8, -10.0],
        Camera {
            target: RenderTarget::Texture { asset_id: TEXTURE },
            ..lens(0.5)
        },
    );
    world
        .app
        .world_mut()
        .entity_mut(texture_camera)
        .insert(TextureViewSettings {
            size: [64, 64],
            ..TextureViewSettings::default()
        });
    world.camera([0.0, 1.0, 6.0], [0.0, 1.0, 0.0], lens(1.1));
    let hidden = world.cube([0.0, 2.8, -3.0]);
    let in_texture = world.cube([4.0, 2.8, -3.0]);
    let mut renderer = FrameRenderer::new();
    renderer.set_two_phase_occlusion(true);
    renderer.set_occlusion_feedback(Some(world.handle()));
    let quality = quality();
    let mut frame = |world: &mut World3, label: &str| {
        world.app.update();
        let extracted = world.app.world().resource::<ExtractedFrame>();
        // The runtime's order: texture views first, then the window.
        for (view_index, view) in extracted.texture_views() {
            let regia_render::ExtractedTarget::Texture { asset_id } = view.target else {
                continue;
            };
            let settings = extracted
                .texture_view_settings
                .get(&asset_id)
                .cloned()
                .unwrap_or_default();
            let (width, height) = settings.extent();
            let mut scene = extracted.frame_scene(view_index, width, height);
            scene.depth_prepass = true;
            scene.quality = quality;
            scene.post = None;
            renderer
                .render_texture_view(&device, asset_id, scene, &settings)
                .expect("texture view");
        }
        let views = extracted.window_views();
        assert_eq!(views.len(), 1, "{label}: one window view");
        let (view_index, _) = views[0];
        assert_eq!(view_index, 1, "the texture camera was spawned first");
        let mut scene = extracted.frame_scene(view_index, SIZE, SIZE);
        scene.depth_prepass = true;
        scene.quality = quality;
        scene.post = None;
        renderer.set_occlusion_view(view_index);
        renderer.render_offscreen(&device, &scene).expect("window view");
        assert!(
            renderer.prepass_occlusion_stats().active,
            "{label}: the window view culled in two phases"
        );
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    for label in ["frame 1", "frame 2", "frame 3", "frame 4"] {
        frame(&mut world, label);
    }
    assert!(
        world.visible(in_texture),
        "hidden from the window view, but in the texture camera's frustum: visible"
    );
    assert!(
        !world.visible(hidden),
        "hidden from the window view and outside the texture camera's frustum: not visible"
    );
    let per_view = world.app.world().resource::<ViewVisibilityPerView>().clone();
    assert_eq!(per_view.views.len(), 2, "{per_view:?}");
    assert_eq!(per_view.views[0].texture, Some(TEXTURE), "{per_view:?}");
    assert!(
        !per_view.views[0].occlusion,
        "the texture view is frustum-only: {per_view:?}"
    );
    assert!(
        per_view.views[1].occlusion,
        "the window view folds its result: {per_view:?}"
    );
    assert!(!world.handle().has_view(0), "a texture view never records a ring");
}

/// ADR-7286: the visibility buffer's cluster cull folds into `ViewVisibility` per instance. Two
/// cluster-drawn spheres (cooked meshlets) and a forward-drawn wall: one sphere far behind the
/// wall, one in front of it. The cull's per-cluster visibility bits are read back without a
/// stall: once settled, the hidden sphere reads not visible and the other visible; a frame after
/// the wall moves away, both read visible. The wall fills most of the frame: the cluster cull
/// tests a 2×2 texel block of the depth pyramid at the mip that fits a cluster's footprint, so at
/// this small frame size an occluder must cover a wide area around what it hides.
#[test]
fn a_visibility_buffer_instance_behind_a_wall_reads_not_visible() {
    use regia_app::prelude::*;
    use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
    use regia_render::{
        AntiAliasing, Camera, CpuMaterial, CpuMesh, ExtractedFrame, LodViewport, MeshRenderer,
        OcclusionFeedback, RenderSettings, SsaoMode, ViewVisibilityStats, extract_frame, mesh_key,
        update_view_visibility,
    };
    use regia_scene::Transform;
    use regia_scene::components::ViewVisibility;
    use regia_scene::propagate::propagate_transforms;
    use std::sync::Arc;

    const W: u32 = 160;
    const H: u32 = 96;
    let Some(device) = device() else {
        return;
    };
    // A unit UV sphere and its meshlets.
    let (segments, rings) = (32u32, 24u32);
    let mut vertices = Vec::new();
    for r in 0..=rings {
        let theta = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = std::f32::consts::TAU * s as f32 / segments as f32;
            let p = [theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                p,
                [s as f32 / segments as f32, r as f32 / rings as f32],
            ));
        }
    }
    let row = segments + 1;
    let mut indices = Vec::new();
    for r in 0..rings {
        for s in 0..segments {
            let (a, b, c, d) = (
                r * row + s,
                (r + 1) * row + s,
                (r + 1) * row + s + 1,
                r * row + s + 1,
            );
            if r + 1 < rings {
                indices.extend_from_slice(&[a, c, b]);
            }
            if r > 0 {
                indices.extend_from_slice(&[a, d, c]);
            }
        }
    }
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let meshlets = build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        &indices,
        &MeshletSettings::default(),
    )
    .unwrap_or_else(|err| panic!("meshlet cook: {err}"));
    let sphere = Arc::new(CpuMesh { vertices, indices });

    // The visibility buffer's frame conditions: a depth prepass, the post stack, no MSAA.
    let mut settings = RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopHigh);
    settings.profile.visibility_buffer = true;
    // Every frame takes the path, however sparse the scene.
    settings.profile.visibility_path = regia_render::VisibilityPath::DEFAULT;
    settings.profile.depth_prepass = true;
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.gi_probes = false;
    settings.profile.screen_space_gi = false;
    settings.profile.ssr = false;
    settings.profile.volumetric_fog = false;
    settings.profile.volumetric_clouds = false;
    settings.profile.reflection_probes = false;
    settings.profile.bloom = false;
    settings.profile.auto_exposure = false;
    settings.profile.ssao = SsaoMode::Off;
    settings.profile.contact_shadows = false;
    settings.profile.motion_blur = false;
    settings.profile.depth_of_field = false;
    settings.post.bloom = false;
    settings.post.auto_exposure = false;
    settings.post.taa = false;
    settings.post.fxaa = false;

    let material = Arc::new(CpuMaterial {
        base_color: [0.55, 0.5, 0.45, 1.0],
        roughness: 0.9,
        ..CpuMaterial::default()
    });
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<OcclusionFeedback>();
    app.insert_resource(settings);
    app.insert_resource(LodViewport { width: W, height: H });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, (extract_frame, update_view_visibility).chain());
    app.world_mut().spawn((
        Transform::default(),
        Camera {
            fov_y: 0.9,
            near: 0.1,
            far: 40.0,
            exposure_ev: Some(0.0),
            ..Camera::default()
        },
    ));
    let spawn = |app: &mut App, mesh: Arc<CpuMesh>, at: [f32; 3]| {
        app.world_mut()
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::from_array(at)),
                MeshRenderer::new(mesh, Arc::clone(&material)),
            ))
            .id()
    };
    // The wall has no meshlets: it draws forward, into the prepass depth the cluster cull's
    // pyramid is built from.
    let (wall_v, wall_i) = box_mesh([3.0, 3.0, 0.1]);
    let wall = spawn(
        &mut app,
        Arc::new(CpuMesh {
            vertices: wall_v,
            indices: wall_i,
        }),
        [0.0, 0.0, -3.0],
    );
    let hidden = spawn(&mut app, Arc::clone(&sphere), [0.0, 0.0, -12.0]);
    let seen = spawn(&mut app, Arc::clone(&sphere), [1.2, 0.0, -1.8]);

    let mut renderer = FrameRenderer::new();
    renderer.insert_meshlets(mesh_key(&sphere), Arc::new(meshlets));
    renderer.set_occlusion_feedback(Some(app.world().resource::<OcclusionFeedback>().clone()));
    let mut frame = |app: &mut App, label: &str| {
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let scene = extracted.frame_scene(0, W, H);
        renderer
            .render_offscreen(&device, &scene)
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        let vis = renderer.visbuffer_stats();
        assert_eq!(
            vis.instances, 2,
            "{label}: both spheres draw as visibility-buffer clusters: {vis:?}"
        );
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let visible = |app: &App, entity: Entity| app.world().get::<ViewVisibility>(entity).is_some_and(|v| v.0);
    for label in ["frame 1", "frame 2", "frame 3"] {
        frame(&mut app, label);
    }
    let feedback = app.world().resource::<OcclusionFeedback>().clone();
    let mut rings = Vec::new();
    feedback.view_stats_into(&mut rings);
    let mut keys = Vec::new();
    feedback.occluded_view_into(0, &mut keys);
    let report = format!(
        "rings {rings:?}, result keys {keys:?}, hidden key {}, seen key {}, wall key {}",
        hidden.to_bits(),
        seen.to_bits(),
        wall.to_bits()
    );
    assert!(visible(&app, wall), "the wall is seen: {report}");
    assert!(
        visible(&app, seen),
        "the sphere in front of the wall is seen: {report}"
    );
    assert!(
        !visible(&app, hidden),
        "the sphere behind the wall drew no cluster: not view-visible: {report}"
    );
    let stats = *app.world().resource::<ViewVisibilityStats>();
    assert!(stats.occluded >= 1, "{stats:?}");

    // The wall moves away: one frame reads the old result (the lag), the next the new one.
    if let Some(mut transform) = app.world_mut().get_mut::<Transform>(wall) {
        transform.translation.x = 30.0;
    }
    frame(&mut app, "uncovered");
    frame(&mut app, "one frame after");
    assert!(
        visible(&app, hidden),
        "uncovered: its clusters draw, and it reads visible a frame later"
    );
}

/// One split-screen frame of the multi-view tests: the world's update, then each window view
/// drawn into its half of one `2 × SIZE` by `SIZE` target, named for its occlusion readback, and
/// a GPU that keeps up.
fn split_frame(
    world: &mut multi_view::World3,
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    label: &str,
) {
    use regia_render::ExtractedFrame;
    world.app.update();
    let extracted = world.app.world().resource::<ExtractedFrame>();
    let views = extracted.window_views();
    assert_eq!(views.len(), 2, "{label}: two window views");
    let target = renderer.offscreen_target(device, SIZE * 2, SIZE, true);
    for (index, (view_index, _)) in views.iter().enumerate() {
        let mut scene = extracted.frame_scene(*view_index, SIZE, SIZE);
        scene.depth_prepass = true;
        scene.quality = quality();
        scene.post = None;
        let rect = [index as u32 * SIZE, 0, SIZE, SIZE];
        let frame_target = regia_render::FrameTarget::rect(
            &target.view,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            SIZE * 2,
            SIZE,
            rect,
            index > 0,
        );
        renderer.set_occlusion_view(*view_index);
        renderer.render_into(device, &scene, frame_target).expect("view");
        assert!(
            renderer.prepass_occlusion_stats().active,
            "{label}: view {index} culled in two phases: {:?}",
            renderer.prepass_occlusion_stats()
        );
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// The split-screen world of the per-view pyramid tests (ADR-7329): view A looks at the front
/// wall from the front, view B across the scene from the side. B's own occluder stands behind
/// the front wall as A sees it, in A's frustum, so A's pyramid hides it; the box behind it (as B
/// sees it) is behind the front wall for A too. Returns the world, the box, B's occluder and a
/// box in plain sight of A.
fn split_world_with_an_occluder_only_b_sees() -> (multi_view::World3, [regia_app::prelude::Entity; 3]) {
    use multi_view::{World3, lens};
    use regia_render::Camera;
    let mut world = World3::new([SIZE * 2, SIZE]);
    world.camera(
        [0.0, 1.0, 6.0],
        [0.0, 1.0, 0.0],
        Camera {
            viewport: [0.0, 0.0, 0.5, 1.0],
            ..lens(1.1)
        },
    );
    world.camera(
        [12.0, 2.8, -6.0],
        [0.0, 2.8, -6.0],
        Camera {
            viewport: [0.5, 0.0, 0.5, 1.0],
            ..lens(1.1)
        },
    );
    // Small and well inside A's frame, so A's (conservative) pyramid test hides it; near enough
    // to the box that B's test hides the box behind it.
    let occluder = world.solid([3.0, 2.8, -6.0], [0.1, 0.8, 0.8]);
    let behind = world.cube([1.0, 2.8, -6.0]);
    let front = world.cube([3.6, 2.8, 3.0]);
    (world, [behind, occluder, front])
}

/// ADR-7329, per-view pyramid history: in split screen, view B's phase 1 tests B's own last
/// pyramid with B's matrix. B's occluder is hidden from A behind the front wall, so with one
/// pyramid history shared by the views (the base, ADR-7286 known issue) B's phase 1 tested A's
/// pyramid, skipped the occluder, and phase 2's pyramid (built from phase 1's depth) lacked it:
/// the box behind it drew in B and read visible (and A's phase 1, testing B's pyramid with B's
/// matrix, drew both as well). With a pyramid per view it reads not visible once settled, and
/// A's own result holds the occluder and the box.
#[test]
fn split_screen_an_occluder_only_view_b_sees_hides_the_box_behind_it() {
    let Some(device) = device() else {
        return;
    };
    let (mut world, [behind, occluder, front]) = split_world_with_an_occluder_only_b_sees();
    let mut renderer = FrameRenderer::new();
    renderer.set_two_phase_occlusion(true);
    renderer.set_occlusion_feedback(Some(world.handle()));
    for label in ["frame 1", "frame 2", "frame 3", "frame 4", "frame 5"] {
        split_frame(&mut world, &mut renderer, &device, label);
    }
    let handle = world.handle();
    let report = format!(
        "A holds box {} occluder {}; B holds box {} occluder {}",
        handle.is_occluded_in(0, behind.to_bits()),
        handle.is_occluded_in(0, occluder.to_bits()),
        handle.is_occluded_in(1, behind.to_bits()),
        handle.is_occluded_in(1, occluder.to_bits()),
    );
    assert!(
        world.visible(front),
        "the box in front of the front wall is visible: {report}"
    );
    assert!(world.visible(occluder), "B sees its occluder: {report}");
    assert!(
        handle.is_occluded_in(0, behind.to_bits()) && handle.is_occluded_in(0, occluder.to_bits()),
        "A's own result holds both (the front wall hides them from A; with one shared history A's \
         phase 1 tested B's pyramid and drew them): {report}"
    );
    assert!(
        handle.is_occluded_in(1, behind.to_bits()),
        "B's result holds the box behind B's own occluder: {report}"
    );
    assert!(
        !world.visible(behind),
        "behind the front wall for A and behind B's occluder for B: not visible once settled: {report}"
    );
}

/// ADR-7329, the cost: each window view keeps its own Hi-Z pyramid (listed with its bytes, and
/// reported per view in the occlusion readback for `render.visible_entities`), a steady
/// two-view frame creates no texture, buffer or bind group, and removing a view frees its
/// pyramid.
#[test]
fn split_screen_keeps_one_pyramid_per_view_creates_nothing_when_steady_and_frees_a_removed_view() {
    let Some(device) = device() else {
        return;
    };
    let (mut world, _) = split_world_with_an_occluder_only_b_sees();
    let mut renderer = FrameRenderer::new();
    renderer.set_two_phase_occlusion(true);
    renderer.set_occlusion_feedback(Some(world.handle()));
    for label in ["frame 1", "frame 2", "frame 3", "frame 4"] {
        split_frame(&mut world, &mut renderer, &device, label);
    }
    let mut pyramids = Vec::new();
    renderer.occlusion_pyramids_into(&mut pyramids);
    assert_eq!(
        pyramids.iter().map(|p| p.view).collect::<Vec<_>>(),
        [0, 1],
        "one pyramid per window view: {pyramids:?}"
    );
    assert!(
        pyramids
            .iter()
            .all(|p| p.bytes > 0 && (p.width, p.height) == (SIZE, SIZE)),
        "{pyramids:?}"
    );
    let per_view: u64 = pyramids.iter().map(|p| p.bytes).sum();
    let handle = world.handle();
    assert_eq!(
        (handle.view_pyramid_bytes(0), handle.view_pyramid_bytes(1)),
        (pyramids[0].bytes, pyramids[1].bytes),
        "the readback reports each view's pyramid"
    );
    assert_eq!(handle.stats().pyramid_bytes, per_view, "{:?}", handle.stats());

    // Steady: the views swap their pyramids in and out without making anything.
    let before = regia_render::frame_path_counters();
    for label in ["steady 1", "steady 2", "steady 3"] {
        split_frame(&mut world, &mut renderer, &device, label);
    }
    let made = regia_render::frame_path_counters().since(before);
    assert_eq!(
        (made.textures, made.buffers, made.bind_groups, made.pipelines),
        (0, 0, 0, 0),
        "a steady two-view frame creates nothing: {made:?}"
    );

    // View B goes away: its pyramid is freed, and its report with it.
    renderer.retain_occlusion_views(1);
    renderer.occlusion_pyramids_into(&mut pyramids);
    assert_eq!(
        pyramids.iter().map(|p| p.view).collect::<Vec<_>>(),
        [0],
        "{pyramids:?}"
    );
    assert_eq!(handle.view_pyramid_bytes(1), 0, "{:?}", handle.stats());
    assert_eq!(handle.stats().pyramid_bytes, pyramids[0].bytes);
}

/// Cooks `mesh` into meshlets and hands them to `renderer`: it then draws as visibility-buffer
/// clusters.
fn insert_cooked(renderer: &mut FrameRenderer, mesh: &std::sync::Arc<regia_render::CpuMesh>) {
    use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
    let positions: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| v.uv).collect();
    let meshlets = build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        &mesh.indices,
        &MeshletSettings::default(),
    )
    .unwrap_or_else(|err| panic!("meshlet cook: {err}"));
    renderer.insert_meshlets(regia_render::mesh_key(mesh), std::sync::Arc::new(meshlets));
}

/// A unit UV sphere.
fn uv_sphere() -> regia_render::CpuMesh {
    let (segments, rings) = (32u32, 24u32);
    let mut vertices = Vec::new();
    for r in 0..=rings {
        let theta = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = std::f32::consts::TAU * s as f32 / segments as f32;
            let p = [theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                p,
                [s as f32 / segments as f32, r as f32 / rings as f32],
            ));
        }
    }
    let row = segments + 1;
    let mut indices = Vec::new();
    for r in 0..rings {
        for s in 0..segments {
            let (a, b, c, d) = (
                r * row + s,
                (r + 1) * row + s,
                (r + 1) * row + s + 1,
                r * row + s + 1,
            );
            if r + 1 < rings {
                indices.extend_from_slice(&[a, c, b]);
            }
            if r > 0 {
                indices.extend_from_slice(&[a, d, c]);
            }
        }
    }
    regia_render::CpuMesh { vertices, indices }
}

/// ADR-7329, per-view cluster history: the visibility buffer's phase 1 draws the clusters that
/// were visible *in this view* last frame. View A looks at a forward-drawn wall that fills its
/// frame; view B looks across from the side at a cluster-drawn wall that stands behind A's wall,
/// in A's frustum, with a cluster-drawn sphere behind it. With one set of visibility bits shared
/// by the views (the base), A's phase 2 cleared the wall's bits, B's phase 1 did not draw it, and
/// B's pyramid lacked it: the sphere's clusters drew in B and it read visible. With bits per view
/// B's phase 1 draws its wall and the sphere reads not visible; A's result holds both.
#[test]
fn a_visibility_buffer_instance_behind_a_cluster_wall_only_view_b_sees_reads_not_visible() {
    use regia_app::prelude::*;
    use regia_render::{
        AntiAliasing, Camera, CpuMaterial, CpuMesh, ExtractedFrame, LodViewport, MeshRenderer,
        OcclusionFeedback, RenderSettings, SsaoMode, ViewVisibilityStats, extract_frame,
        update_view_visibility,
    };
    use regia_scene::Transform;
    use regia_scene::components::ViewVisibility;
    use regia_scene::propagate::propagate_transforms;
    use std::sync::Arc;

    const W: u32 = 160;
    const H: u32 = 96;
    let Some(device) = device() else {
        return;
    };
    let sphere = Arc::new(uv_sphere());
    let (wall_v, wall_i) = box_mesh([0.1, 0.7, 0.7]);
    let cluster_wall = Arc::new(CpuMesh {
        vertices: wall_v,
        indices: wall_i,
    });
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.profile.visibility_buffer = true;
    settings.profile.visibility_path = regia_render::VisibilityPath::DEFAULT;
    settings.profile.depth_prepass = true;
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.gi_probes = false;
    settings.profile.screen_space_gi = false;
    settings.profile.ssr = false;
    settings.profile.volumetric_fog = false;
    settings.profile.volumetric_clouds = false;
    settings.profile.reflection_probes = false;
    settings.profile.bloom = false;
    settings.profile.auto_exposure = false;
    settings.profile.ssao = SsaoMode::Off;
    settings.profile.contact_shadows = false;
    settings.profile.motion_blur = false;
    settings.profile.depth_of_field = false;
    settings.post.bloom = false;
    settings.post.auto_exposure = false;
    settings.post.taa = false;
    settings.post.fxaa = false;
    let material = Arc::new(CpuMaterial {
        base_color: [0.55, 0.5, 0.45, 1.0],
        roughness: 0.9,
        ..CpuMaterial::default()
    });
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<regia_render::ViewVisibilityPerView>();
    app.init_resource::<OcclusionFeedback>();
    app.insert_resource(settings);
    app.insert_resource(LodViewport {
        width: W * 2,
        height: H,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, (extract_frame, update_view_visibility).chain());
    let lens = |viewport: [f32; 4]| Camera {
        fov_y: 0.9,
        near: 0.1,
        far: 40.0,
        exposure_ev: Some(0.0),
        viewport,
        ..Camera::default()
    };
    // A at the origin looking down -z; B at x = 5 looking down -x along z = -12.
    app.world_mut()
        .spawn((Transform::default(), lens([0.0, 0.0, 0.5, 1.0])));
    app.world_mut().spawn((
        Transform {
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            ..Transform::from_translation(Vec3::new(5.0, 0.0, -12.0))
        },
        lens([0.5, 0.0, 0.5, 1.0]),
    ));
    let spawn = |app: &mut App, mesh: Arc<CpuMesh>, at: [f32; 3]| {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::from_array(at)),
                MeshRenderer::new(mesh, Arc::clone(&material)),
            ))
            .id()
    };
    // A's forward wall (no meshlets) fills A's frame.
    let (front_v, front_i) = box_mesh([3.0, 3.0, 0.1]);
    let front_wall = spawn(
        &mut app,
        Arc::new(CpuMesh {
            vertices: front_v,
            indices: front_i,
        }),
        [0.0, 0.0, -3.0],
    );
    // B's cluster wall, between B and the sphere, behind A's wall and well inside A's frustum
    // (its cluster bound must read hidden in A's pyramid test, so A clears its bits).
    let b_wall = spawn(&mut app, Arc::clone(&cluster_wall), [1.5, 0.0, -12.0]);
    // A small sphere (radius 0.25) behind it, so B's wall covers it with a wide margin.
    let hidden = app
        .world_mut()
        .spawn((
            Transform {
                scale: Vec3::splat(0.25),
                ..Transform::from_translation(Vec3::new(-2.0, 0.0, -12.0))
            },
            MeshRenderer::new(Arc::clone(&sphere), Arc::clone(&material)),
        ))
        .id();

    let mut renderer = FrameRenderer::new();
    insert_cooked(&mut renderer, &sphere);
    insert_cooked(&mut renderer, &cluster_wall);
    renderer.set_occlusion_feedback(Some(app.world().resource::<OcclusionFeedback>().clone()));
    let mut frame = |app: &mut App, label: &str| {
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let views = extracted.window_views();
        assert_eq!(views.len(), 2, "{label}: two window views");
        let target = renderer.offscreen_target(&device, W * 2, H, true);
        for (index, (view_index, _)) in views.iter().enumerate() {
            let scene = extracted.frame_scene(*view_index, W, H);
            let frame_target = regia_render::FrameTarget::rect(
                &target.view,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                W * 2,
                H,
                [index as u32 * W, 0, W, H],
                index > 0,
            );
            renderer.set_occlusion_view(*view_index);
            renderer
                .render_into(&device, &scene, frame_target)
                .unwrap_or_else(|err| panic!("{label}: view {index}: {err}"));
            let vis = renderer.visbuffer_stats();
            assert_eq!(
                vis.instances, 2,
                "{label}: view {index}: the sphere and B's wall draw as clusters: {vis:?}"
            );
        }
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let visible = |app: &App, entity: Entity| app.world().get::<ViewVisibility>(entity).is_some_and(|v| v.0);
    for label in ["frame 1", "frame 2", "frame 3", "frame 4", "frame 5"] {
        frame(&mut app, label);
    }
    let feedback = app.world().resource::<OcclusionFeedback>().clone();
    let report = format!(
        "A holds sphere {} wall {}; B holds sphere {} wall {}",
        feedback.is_occluded_in(0, hidden.to_bits()),
        feedback.is_occluded_in(0, b_wall.to_bits()),
        feedback.is_occluded_in(1, hidden.to_bits()),
        feedback.is_occluded_in(1, b_wall.to_bits()),
    );
    assert!(visible(&app, front_wall), "A sees its wall: {report}");
    assert!(visible(&app, b_wall), "B sees its cluster wall: {report}");
    assert!(
        feedback.is_occluded_in(0, hidden.to_bits()) && feedback.is_occluded_in(0, b_wall.to_bits()),
        "A's result: its wall hides both: {report}"
    );
    assert!(
        !visible(&app, hidden),
        "behind A's wall for A and behind B's cluster wall for B: not visible once settled: {report}"
    );
}

/// One frame of the three-view retirement test (ADR-8620): the world's update, then the window
/// views in `draw` (indices into the frame's views), each into its third of a `3 × SIZE` by
/// `SIZE` target, by each renderer in turn. No `retain_occlusion_views` call: the renderers
/// retire undrawn views themselves. Returns the first renderer's prepass report per drawn view.
fn thirds_frame(
    world: &mut multi_view::World3,
    renderers: &mut [&mut FrameRenderer],
    device: &RenderDevice,
    draw: &[usize],
) -> Vec<regia_render::PrepassOcclusionStats> {
    use regia_render::ExtractedFrame;
    world.app.update();
    let extracted = world.app.world().resource::<ExtractedFrame>();
    let views = extracted.window_views();
    assert_eq!(views.len(), 3, "three window views");
    let mut stats = Vec::new();
    for (which, renderer) in renderers.iter_mut().enumerate() {
        let target = renderer.offscreen_target(device, SIZE * 3, SIZE, true);
        for &index in draw {
            let (view_index, _) = views[index];
            let mut scene = extracted.frame_scene(view_index, SIZE, SIZE);
            scene.depth_prepass = true;
            scene.quality = quality();
            scene.post = None;
            let rect = [index as u32 * SIZE, 0, SIZE, SIZE];
            let frame_target = regia_render::FrameTarget::rect(
                &target.view,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                SIZE * 3,
                SIZE,
                rect,
                index > 0,
            );
            renderer.set_occlusion_view(view_index);
            renderer.render_into(device, &scene, frame_target).expect("view");
            if which == 0 {
                stats.push(renderer.prepass_occlusion_stats());
            }
        }
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    stats
}

/// ADR-8620, automatic retirement: three window views draw for 5 frames, then only view 0 for
/// `OCCLUSION_VIEW_RETIRE_ROUNDS + 1` frames, and the host never calls `retain_occlusion_views`.
/// Views 1 and 2 give their pyramids back: the renderer lists view 0's only and the readback's
/// `pyramid_bytes` drops to one view's bytes (on the base both stayed allocated until the
/// renderer was dropped). A steady single view then makes nothing. View 1 drawing again starts
/// as a new view (no pyramid history) and draws, byte for byte, what a renderer without
/// occlusion culling draws through the same frames.
#[test]
fn an_undrawn_split_screen_view_gives_back_its_pyramid() {
    use multi_view::{World3, lens};
    use regia_render::Camera;
    let Some(device) = device() else {
        return;
    };
    let mut world = World3::new([SIZE * 3, SIZE]);
    for (index, eye) in [[0.0, 1.0, 6.0], [12.0, 2.8, -3.0], [-10.0, 2.0, 5.0]]
        .into_iter()
        .enumerate()
    {
        world.camera(
            eye,
            [0.0, 1.0, -2.0],
            Camera {
                viewport: [index as f32 / 3.0, 0.0, 1.0 / 3.0, 1.0],
                ..lens(1.1)
            },
        );
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_two_phase_occlusion(true);
    renderer.set_occlusion_feedback(Some(world.handle()));
    // The same frames without occlusion culling: what the views must draw.
    let mut control = FrameRenderer::new();
    for _ in 0..5 {
        thirds_frame(
            &mut world,
            &mut [&mut renderer, &mut control],
            &device,
            &[0, 1, 2],
        );
    }
    let mut pyramids = Vec::new();
    renderer.occlusion_pyramids_into(&mut pyramids);
    assert_eq!(
        pyramids.iter().map(|p| p.view).collect::<Vec<_>>(),
        [0, 1, 2],
        "{pyramids:?}"
    );
    let one_view = pyramids[0].bytes;
    let handle = world.handle();
    assert_eq!(handle.stats().pyramid_bytes, 3 * one_view, "{:?}", handle.stats());

    for _ in 0..=regia_render::OCCLUSION_VIEW_RETIRE_ROUNDS {
        thirds_frame(&mut world, &mut [&mut renderer, &mut control], &device, &[0]);
    }
    renderer.occlusion_pyramids_into(&mut pyramids);
    assert_eq!(
        pyramids.iter().map(|p| p.view).collect::<Vec<_>>(),
        [0],
        "views 1 and 2 gave their pyramids back: {pyramids:?}"
    );
    assert_eq!(
        handle.stats().pyramid_bytes,
        one_view,
        "pyramid_bytes dropped to one view's: {:?}",
        handle.stats()
    );
    assert_eq!(
        (handle.view_pyramid_bytes(1), handle.view_pyramid_bytes(2)),
        (0, 0)
    );
    assert_eq!(renderer.retired_occlusion_views(), 2);

    // Steady single view: retirement keeps nothing alive and makes nothing.
    let before = regia_render::frame_path_counters();
    for _ in 0..3 {
        thirds_frame(&mut world, &mut [&mut renderer], &device, &[0]);
    }
    let made = regia_render::frame_path_counters().since(before);
    assert_eq!(
        (made.textures, made.buffers, made.bind_groups, made.pipelines),
        (0, 0, 0, 0),
        "a steady single view creates nothing: {made:?}"
    );
    for _ in 0..3 {
        thirds_frame(&mut world, &mut [&mut control], &device, &[0]);
    }

    // View 1 comes back as a new view, and draws what the unculled renderer draws.
    let back = thirds_frame(&mut world, &mut [&mut renderer, &mut control], &device, &[0, 1]);
    assert!(back[1].active, "view 1 culled in two phases: {:?}", back[1]);
    assert!(
        !back[1].history,
        "a returning view has no pyramid history: {:?}",
        back[1]
    );
    renderer.occlusion_pyramids_into(&mut pyramids);
    assert_eq!(
        pyramids.iter().map(|p| p.view).collect::<Vec<_>>(),
        [0, 1],
        "{pyramids:?}"
    );
    let extracted = world.app.world().resource::<regia_render::ExtractedFrame>();
    let (view_index, _) = extracted.window_views()[1];
    let mut scene = extracted.frame_scene(view_index, SIZE, SIZE);
    scene.depth_prepass = true;
    scene.quality = quality();
    scene.post = None;
    renderer.set_occlusion_view(view_index);
    let got = renderer
        .render_offscreen(&device, &scene)
        .expect("returned")
        .pixels;
    control.set_occlusion_view(view_index);
    let want = control.render_offscreen(&device, &scene).expect("control").pixels;
    assert_eq!(
        mismatched(&got, &want),
        0,
        "the returning view draws the unculled renderer's pixels"
    );
}
