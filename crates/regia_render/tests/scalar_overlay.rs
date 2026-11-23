// SPDX-License-Identifier: MPL-2.0
//! Per-vertex scalar overlays in the frame renderer (Tincal engine request F10).
//!
//! Four fins share one mesh (an instanced batch on a GPU-driven device). An overlay on one of
//! them colours only that fin; the pixels of the others are untouched; removing the overlay
//! restores the frame exactly. The overlay is drawn unlit over the lit frame, follows a skinned
//! palette, hides behind nearer geometry, clamps its range, and a values length mismatch draws
//! nothing without panicking. Steady frames and new values create no GPU buffers or pipelines.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, JointMatrix, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, Ramp, RenderDevice, ScalarOverlay, frame_path_counters,
};

const W: u32 = 192;
const H: u32 = 128;
const FIN_MESH: u64 = 0xF1D0_0B1F;
const SKIN_MESH: u64 = 0xF1D0_0B20;
const BLOCKER_MESH: u64 = 0xF1D0_0B21;
const OBJECT_BASE: u64 = 0x5CA1_A000;
/// Fin centres on X (the camera looks down −Z at x = 0).
const FIN_X: [f32; 4] = [-2.4, -0.8, 0.8, 2.4];
const HALF: f32 = 0.6;
/// Grid vertices per side of a fin.
const GRID: usize = 9;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// A `GRID × GRID` quad in the XY plane, front face +Z. Vertex `i` is at column `i % GRID`.
fn fin(half: f32, skinned: bool) -> LabMesh {
    let mut vertices = Vec::new();
    for row in 0..GRID {
        for col in 0..GRID {
            let u = col as f32 / (GRID - 1) as f32;
            let v = row as f32 / (GRID - 1) as f32;
            let mut vertex = LabVertex::from_pos_normal_uv(
                [(u * 2.0 - 1.0) * half, (1.0 - v * 2.0) * half, 0.0],
                [0.0, 0.0, 1.0],
                [u, v],
            );
            if skinned {
                vertex.weights = [1.0, 0.0, 0.0, 0.0];
            }
            vertices.push(vertex);
        }
    }
    let mut indices = Vec::new();
    for row in 0..GRID - 1 {
        for col in 0..GRID - 1 {
            let a = (row * GRID + col) as u32;
            let (b, c, d) = (a + 1, a + GRID as u32, a + GRID as u32 + 1);
            indices.extend([a, c, b, b, c, d]);
        }
    }
    LabMesh {
        vertices,
        indices,
        ..LabMesh::unskinned()
    }
}

fn joint_at(x: f32, y: f32) -> JointMatrix {
    JointMatrix {
        m: [[1.0, 0.0, 0.0, x], [0.0, 1.0, 0.0, y], [0.0, 0.0, 1.0, 0.0]],
    }
}

/// Four fins; fin `skinned` (if any) is skinned with a one-joint palette at its slot, and a
/// `blocker` quad stands in front of fin 1 when asked.
fn scene(skinned: Option<usize>, blocker: bool) -> LabScene {
    let mut scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.6, 6.0],
        target: [0.0, 0.6, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.7,
            ..LabMaterial::default()
        }],
        meshes: Vec::new(),
    };
    for (i, x) in FIN_X.into_iter().enumerate() {
        if skinned == Some(i) {
            scene.meshes.push(LabMesh {
                joints: vec![joint_at(x, 0.6)],
                ..fin(HALF, true)
            });
        } else {
            scene.meshes.push(LabMesh {
                model: translate(x, 0.6, 0.0),
                ..fin(HALF, false)
            });
        }
    }
    if blocker {
        scene.meshes.push(LabMesh {
            model: translate(FIN_X[1], 0.6, 0.5),
            ..fin(0.3, false)
        });
    }
    scene
}

/// The frame scene with stable mesh and object keys: object `i` is `OBJECT_BASE + i`.
fn frame_of(lab: &LabScene, skinned: Option<usize>) -> FrameScene<'_> {
    let mut frame = lab.as_frame_scene_with_quality(FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    });
    for (i, mesh) in frame.meshes.iter_mut().enumerate() {
        mesh.mesh_key = if i == 4 {
            BLOCKER_MESH
        } else if skinned == Some(i) {
            SKIN_MESH
        } else {
            FIN_MESH
        };
        mesh.object_key = OBJECT_BASE + i as u64;
    }
    frame
}

fn key(i: usize) -> u64 {
    OBJECT_BASE + i as u64
}

/// One value per vertex: the vertex's column, 0 on the left edge to 1 on the right.
fn column_values() -> Arc<[f32]> {
    (0..GRID * GRID)
        .map(|i| (i % GRID) as f32 / (GRID - 1) as f32)
        .collect()
}

fn constant(value: f32) -> Arc<[f32]> {
    vec![value; GRID * GRID].into()
}

/// Pixel column band of fin `i` (its projected half width, inset by two pixels, so the edge columns read about 0.1 and 0.9 of the ramp).
fn band(i: usize) -> std::ops::Range<u32> {
    let half_w = 6.0 * 0.4_f32.tan() * W as f32 / H as f32;
    let centre = (FIN_X[i] / half_w + 1.0) * 0.5 * W as f32;
    let half = HALF / half_w * 0.5 * W as f32;
    (centre - half + 2.0) as u32..(centre + half - 2.0) as u32
}

/// Pixels differing by more than 1/255 inside and outside fin `i`'s column band (with a
/// four-pixel margin outside it).
fn diff_by_band(a: &[u8], b: &[u8], i: usize) -> (usize, usize) {
    let inside = band(i);
    let outside = inside.start.saturating_sub(4)..inside.end + 4;
    let (mut ins, mut out) = (0, 0);
    for (n, (pa, pb)) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0).enumerate() {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        if d <= 1 {
            continue;
        }
        let x = n as u32 % W;
        if inside.contains(&x) {
            ins += 1;
        } else if !outside.contains(&x) {
            out += 1;
        }
    }
    (ins, out)
}

/// Mean RGB of a pixel box on fin `i`: `lo..hi` is the fraction of the band across, at mid height.
fn fin_rgb(px: &[u8], i: usize, lo: f32, hi: f32) -> [f64; 3] {
    let b = band(i);
    let span = (b.end - b.start) as f32;
    let (x0, x1) = (b.start + (span * lo) as u32, b.start + (span * hi) as u32);
    let (mut sum, mut n) = ([0.0; 3], 0.0);
    for y in H / 2 - 3..H / 2 + 3 {
        for x in x0..x1.max(x0 + 1) {
            let k = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f64::from(px[k + c]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

fn near(got: [f64; 3], want: [f32; 3], tolerance: f64) -> bool {
    got.iter()
        .zip(want)
        .all(|(g, w)| (g - f64::from(w) * 255.0).abs() <= tolerance)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438).
static GPU: Mutex<()> = Mutex::new(());

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    /// Declared last so the device and renderer drop while the lock is held.
    _gpu: MutexGuard<'static, ()>,
}

impl Rig {
    fn new() -> Self {
        let gpu = GPU.lock().unwrap_or_else(PoisonError::into_inner);
        Self {
            device: device(),
            renderer: FrameRenderer::new(),
            _gpu: gpu,
        }
    }

    fn render(&mut self, frame: &FrameScene<'_>) -> Vec<u8> {
        self.renderer
            .render_offscreen(&self.device, frame)
            .expect("frame")
            .pixels
    }
}

#[test]
fn only_the_targeted_entity_changes_and_removal_restores() {
    let mut rig = Rig::new();
    let lab = scene(None, false);
    let frame = frame_of(&lab, None);
    let base = rig.render(&frame);
    let _ = rig.render(&frame);

    let ramp = Ramp::viridis();
    rig.renderer
        .set_scalar_overlays([(key(1), ScalarOverlay::new(column_values()))]);
    assert_eq!(rig.renderer.scalar_overlay_count(), 1);
    let shown = rig.render(&frame);
    assert_eq!(rig.renderer.scalar_overlay_stats().drawn, 1);
    let (inside, outside) = diff_by_band(&base, &shown, 1);
    assert!(inside > 100, "fin 1 changed ({inside} px)");
    assert_eq!(outside, 0, "nothing else changed");
    for other in [0, 2, 3] {
        assert_eq!(
            diff_by_band(&base, &shown, other).0,
            0,
            "fin {other} is untouched"
        );
    }

    // Unlit: the colour is the ramp's, not the lit grey. Left edge dark purple, right yellow.
    let left = fin_rgb(&shown, 1, 0.0, 0.08);
    let right = fin_rgb(&shown, 1, 0.92, 1.0);
    let mid = fin_rgb(&shown, 1, 0.45, 0.55);
    eprintln!("left {left:?} mid {mid:?} right {right:?}");
    assert!(near(left, ramp.sample(0.11), 24.0), "left {left:?}");
    assert!(near(right, ramp.sample(0.89), 24.0), "right {right:?}");
    assert!(near(mid, ramp.sample(0.5), 24.0), "mid {mid:?}");

    // Removal restores the frame exactly.
    rig.renderer.set_scalar_overlays(std::iter::empty());
    assert_eq!(rig.renderer.scalar_overlay_count(), 0);
    let restored = rig.render(&frame);
    assert_eq!(restored, base, "removing the overlay restores the pixels");
}

#[test]
fn range_clamps_and_reverses() {
    let mut rig = Rig::new();
    let lab = scene(None, false);
    let frame = frame_of(&lab, None);
    let ramp = Ramp::viridis();
    let paint = |rig: &mut Rig, overlay: ScalarOverlay| {
        rig.renderer.set_scalar_overlays([(key(2), overlay)]);
        rig.render(&frame)
    };
    let probe = |px: &[u8]| fin_rgb(px, 2, 0.3, 0.7);

    let low = paint(&mut rig, ScalarOverlay::new(constant(-50.0)));
    assert!(
        near(probe(&low), ramp.sample(0.0), 6.0),
        "below the range clamps to the ramp start"
    );
    let high = paint(&mut rig, ScalarOverlay::new(constant(50.0)));
    assert!(
        near(probe(&high), ramp.sample(1.0), 6.0),
        "above the range clamps to the ramp end"
    );
    let mid = paint(&mut rig, ScalarOverlay::new(constant(5.0)).with_range(0.0, 10.0));
    assert!(
        near(probe(&mid), ramp.sample(0.5), 6.0),
        "a wider range maps linearly"
    );
    let flipped = paint(&mut rig, ScalarOverlay::new(constant(0.0)).with_range(1.0, 0.0));
    assert!(
        near(probe(&flipped), ramp.sample(1.0), 6.0),
        "a reversed range flips the ramp"
    );
    let step_below = paint(&mut rig, ScalarOverlay::new(constant(0.4)).with_range(0.5, 0.5));
    assert!(
        near(probe(&step_below), ramp.sample(0.0), 6.0),
        "an empty range steps at its value"
    );
    let nan = paint(&mut rig, ScalarOverlay::new(constant(f32::NAN)));
    assert!(near(probe(&nan), ramp.sample(0.0), 6.0), "NaN reads as zero");
    let diverging = paint(
        &mut rig,
        ScalarOverlay::new(constant(0.0))
            .with_ramp(Ramp::diverging())
            .with_range(-1.0, 1.0),
    );
    assert!(
        near(probe(&diverging), Ramp::diverging().sample(0.5), 6.0),
        "zero is the diverging ramp's neutral middle"
    );
}

#[test]
fn opacity_blends_over_the_lit_frame() {
    let mut rig = Rig::new();
    let lab = scene(None, false);
    let frame = frame_of(&lab, None);
    let base = rig.render(&frame);
    let lit = fin_rgb(&base, 3, 0.3, 0.7);
    let want = Ramp::viridis().sample(1.0);

    rig.renderer
        .set_scalar_overlays([(key(3), ScalarOverlay::new(constant(1.0)).with_opacity(0.5))]);
    let half = rig.render(&frame);
    let got = fin_rgb(&half, 3, 0.3, 0.7);
    for c in 0..3 {
        let expected = (lit[c] + f64::from(want[c]) * 255.0) / 2.0;
        assert!(
            (got[c] - expected).abs() <= 4.0,
            "channel {c}: {got:?} vs {expected}"
        );
    }

    rig.renderer
        .set_scalar_overlays([(key(3), ScalarOverlay::new(constant(1.0)).with_opacity(0.0))]);
    assert_eq!(rig.render(&frame), base, "zero opacity draws nothing");
    rig.renderer
        .set_scalar_overlays([(key(3), ScalarOverlay::new(constant(1.0)).with_opacity(f32::NAN))]);
    assert_eq!(rig.render(&frame), base, "NaN opacity draws nothing");
}

#[test]
fn a_length_mismatch_draws_nothing_and_does_not_panic() {
    let mut rig = Rig::new();
    let lab = scene(None, false);
    let frame = frame_of(&lab, None);
    let base = rig.render(&frame);
    for len in [0, 1, GRID * GRID - 1, GRID * GRID + 1, 10_000] {
        rig.renderer
            .set_scalar_overlays([(key(1), ScalarOverlay::new(vec![1.0; len]))]);
        let out = rig.render(&frame);
        assert_eq!(
            out,
            base,
            "{len} values for {} vertices draw nothing",
            GRID * GRID
        );
        let stats = rig.renderer.scalar_overlay_stats();
        assert_eq!((stats.drawn, stats.mismatched), (0, 1), "{len}: {stats:?}");
    }
    // The same entity with matching values draws on the next frame.
    rig.renderer
        .set_scalar_overlays([(key(1), ScalarOverlay::new(column_values()))]);
    let fixed = rig.render(&frame);
    assert_ne!(fixed, base);
    assert_eq!(rig.renderer.scalar_overlay_stats().mismatched, 0);
    // An overlay on a key no mesh has is ignored.
    rig.renderer
        .set_scalar_overlays([(0xDEAD, ScalarOverlay::new(column_values()))]);
    assert_eq!(rig.render(&frame), base);
}

#[test]
fn steady_and_edited_frames_create_nothing() {
    let mut rig = Rig::new();
    let lab = scene(None, false);
    let frame = frame_of(&lab, None);
    let values = column_values();
    rig.renderer
        .set_scalar_overlays([(key(1), ScalarOverlay::new(values.clone()))]);
    let _ = rig.render(&frame);
    let _ = rig.render(&frame);
    let uploads = rig.renderer.scalar_overlay_stats();
    assert_eq!(uploads.value_uploads, 1);
    assert_eq!(uploads.geometry_uploads, 1);

    // Steady frames, re-submitted every frame as a host does: no buffers, groups or pipelines,
    // and no new uploads.
    let before = frame_path_counters();
    for _ in 0..3 {
        rig.renderer
            .set_scalar_overlays([(key(1), ScalarOverlay::new(values.clone()))]);
        let _ = rig.render(&frame);
    }
    let spent = frame_path_counters().since(before);
    eprintln!("steady frames: {spent:?}");
    assert_eq!(spent.buffers, 0);
    assert_eq!(spent.bind_groups, 0);
    assert_eq!(spent.pipelines, 0);
    assert_eq!(rig.renderer.scalar_overlay_stats().value_uploads, 1);

    // New values, a new range and a new ramp write into the existing buffers.
    let before = frame_path_counters();
    let edited: Arc<[f32]> = (0..GRID * GRID).map(|i| (i / GRID) as f32 / 8.0).collect();
    rig.renderer.set_scalar_overlays([(
        key(1),
        ScalarOverlay::new(edited)
            .with_range(0.25, 0.75)
            .with_ramp(Ramp::diverging()),
    )]);
    let _ = rig.render(&frame);
    let spent = frame_path_counters().since(before);
    eprintln!("edited frame: {spent:?}");
    assert_eq!(spent.buffers, 0);
    assert_eq!(spent.bind_groups, 0);
    assert_eq!(spent.pipelines, 0);
    assert_eq!(rig.renderer.scalar_overlay_stats().value_uploads, 2);
}

#[test]
fn nearer_geometry_hides_the_overlay() {
    let mut rig = Rig::new();
    let lab = scene(None, true);
    let frame = frame_of(&lab, None);
    let base = rig.render(&frame);
    rig.renderer
        .set_scalar_overlays([(key(1), ScalarOverlay::new(constant(1.0)))]);
    let shown = rig.render(&frame);
    let (inside, outside) = diff_by_band(&base, &shown, 1);
    assert!(
        inside > 100,
        "the visible part of fin 1 is coloured ({inside} px)"
    );
    assert_eq!(outside, 0);
    // The blocker (half 0.3 at z = 0.5) covers the middle of fin 1: those pixels are unchanged.
    let b = band(1);
    let cx = (b.start + b.end) / 2;
    for y in H / 2 - 3..H / 2 + 3 {
        for x in cx - 3..cx + 3 {
            let k = ((y * W + x) * 4) as usize;
            assert_eq!(
                base[k..k + 4],
                shown[k..k + 4],
                "pixel ({x}, {y}) is behind the blocker"
            );
        }
    }
    // Next to the blocker, still on the fin, the overlay shows.
    let edge = fin_rgb(&shown, 1, 0.0, 0.1);
    assert!(near(edge, Ramp::viridis().sample(1.0), 24.0), "{edge:?}");
}

#[test]
fn a_skinned_mesh_is_coloured_where_its_palette_draws_it() {
    let mut rig = Rig::new();
    // Fin 1 is skinned at its own slot.
    let lab = scene(Some(1), false);
    let frame = frame_of(&lab, Some(1));
    let base = rig.render(&frame);
    rig.renderer
        .set_scalar_overlays([(key(1), ScalarOverlay::new(column_values()))]);
    let shown = rig.render(&frame);
    let (inside, outside) = diff_by_band(&base, &shown, 1);
    assert!(
        inside > 100 && outside == 0,
        "skinned fin 1: {inside} in, {outside} out"
    );
    assert!(near(
        fin_rgb(&shown, 1, 0.0, 0.08),
        Ramp::viridis().sample(0.11),
        24.0
    ));
    assert!(near(
        fin_rgb(&shown, 1, 0.92, 1.0),
        Ramp::viridis().sample(0.89),
        24.0
    ));

    // The joint moves to slot 2's place: the overlay moves with it, nothing is left behind.
    let mut moved = scene(Some(1), false);
    moved.meshes[1].joints = vec![joint_at(FIN_X[2] + 0.0, 0.6)];
    // Slot 2 also has a rigid fin: hide it by moving that fin away.
    moved.meshes[2].model = translate(FIN_X[2], 5.0, 0.0);
    let moved_frame = frame_of(&moved, Some(1));
    rig.renderer.set_scalar_overlays(std::iter::empty());
    let moved_base = rig.render(&moved_frame);
    rig.renderer
        .set_scalar_overlays([(key(1), ScalarOverlay::new(column_values()))]);
    let moved_shown = rig.render(&moved_frame);
    let (inside, outside) = diff_by_band(&moved_base, &moved_shown, 2);
    assert!(
        inside > 100 && outside == 0,
        "moved fin: {inside} in, {outside} out"
    );
    assert!(near(
        fin_rgb(&moved_shown, 2, 0.92, 1.0),
        Ramp::viridis().sample(0.89),
        24.0
    ));
}

#[test]
fn instances_of_one_mesh_color_independently() {
    let mut rig = Rig::new();
    let lab = scene(None, false);
    let frame = frame_of(&lab, None);
    let base = rig.render(&frame);
    // Two of the four instances, with different values: each changes, the others do not.
    rig.renderer.set_scalar_overlays([
        (key(0), ScalarOverlay::new(constant(0.0))),
        (key(3), ScalarOverlay::new(constant(1.0))),
    ]);
    let out = rig.render(&frame);
    assert_eq!(rig.renderer.scalar_overlay_stats().drawn, 2);
    assert!(diff_by_band(&base, &out, 0).0 > 100);
    assert!(diff_by_band(&base, &out, 3).0 > 100);
    assert_eq!(diff_by_band(&base, &out, 1).0, 0);
    assert_eq!(diff_by_band(&base, &out, 2).0, 0);
    assert!(near(fin_rgb(&out, 0, 0.3, 0.7), Ramp::viridis().sample(0.0), 6.0));
    assert!(near(fin_rgb(&out, 3, 0.3, 0.7), Ramp::viridis().sample(1.0), 6.0));
}
