// SPDX-License-Identifier: MPL-2.0
//! The runtime lens effects pass (ADR-1863) on a real frame: off is byte-identical, the ghosts are
//! additive and point-symmetric about the frame centre, land where the traced table puts them and
//! have the aperture's edge, chromatic aberration fringes an edge, dirt only adds light, and the
//! pass sees the view's post settings at `before_post` only.
//!
//! Runs on any headless adapter (lavapipe is the reference). No golden image: every check compares
//! frames of this test with each other or with the CPU ghost table.

use std::sync::{Arc, Mutex};

use regia_render::lens_effects::{default_ghost_table, lens_effects_counters};
use regia_render::{
    DirectionalLightUniform, FramePass, FramePassContext, FrameRenderer, GraphSlot, LabMaterial, LabMesh,
    LabScene, LabVertex, LensEffects, PostSettings, RenderDevice,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

const W: u32 = 128;
const H: u32 = 72;
const FOV_Y: f32 = 0.9;
/// Distance of the quads from the eye.
const DEPTH: f32 = 5.0;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// World-space rectangle at `DEPTH` covering centred frame coordinates `c0..c1` (x right, y down,
/// both -1..1).
fn rect(c0: [f32; 2], c1: [f32; 2]) -> LabMesh {
    rect_at(c0, c1, DEPTH)
}

/// [`rect`] at `depth` metres from the eye (the same frame coverage).
fn rect_at(c0: [f32; 2], c1: [f32; 2], depth: f32) -> LabMesh {
    let half_h = depth * (FOV_Y * 0.5).tan();
    let half_w = half_h * W as f32 / H as f32;
    let (x0, x1) = (c0[0] * half_w, c1[0] * half_w);
    let (y0, y1) = (-c0[1] * half_h, -c1[1] * half_h);
    let z = -depth;
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x0, y1, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x1, y1, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x1, y0, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x0, y0, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }
}

/// A dark frame with emissive rectangles of `radiance` (linear, white).
fn scene(rects: Vec<LabMesh>, radiance: f32, lens: LensEffects) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.lens_effects = lens;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -1.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: radiance,
            ..LabMaterial::default()
        }],
        meshes: rects,
    }
}

fn display(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    renderer
        .render_offscreen(device, &lab.as_frame_scene())
        .expect("offscreen frame")
        .pixels
}

/// The linear HDR image the post stack reads (after the lens effects pass).
fn hdr(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<[f32; 3]> {
    renderer.request_hdr_capture();
    let _ = display(renderer, device, lab);
    match renderer.take_hdr_capture(device) {
        Ok(Some(capture)) => {
            assert_eq!((capture.width, capture.height), (W, H));
            capture.rgb
        }
        Ok(None) => panic!("no HDR capture (the post stack did not run)"),
        Err(err) => panic!("HDR capture: {err}"),
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Mean added luminance in a `(2r+1)²` window around centred frame position `c`.
fn window(added: &[f32], c: [f32; 2], r: i32) -> f32 {
    let px = ((c[0] * 0.5 + 0.5) * W as f32) as i32;
    let py = ((c[1] * 0.5 + 0.5) * H as f32) as i32;
    let (mut sum, mut n) = (0.0, 0);
    for y in (py - r)..=(py + r) {
        for x in (px - r)..=(px + r) {
            if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
                sum += added[(y as u32 * W + x as u32) as usize];
                n += 1;
            }
        }
    }
    if n == 0 { 0.0 } else { sum / n as f32 }
}

/// The small light at centred `c`, 4 × 4 pixels.
fn light_at(c: [f32; 2]) -> LabMesh {
    let half = [2.0 / W as f32, 2.0 / H as f32];
    rect([c[0] - half[0], c[1] - half[1]], [c[0] + half[0], c[1] + half[1]])
}

const FLARE: LensEffects = LensEffects {
    flare_intensity: 400.0,
    flare_threshold: 1.0,
    coating: 1.0,
    dirt_intensity: 0.0,
    chromatic_aberration: 0.0,

    ..LensEffects::OFF
};

#[test]
fn off_settings_or_no_pass_leave_the_frame_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lights = || vec![light_at([0.5, -0.4])];
    let off = scene(lights(), 500.0, LensEffects::OFF);
    let asked = scene(lights(), 500.0, LensEffects::CINEMATIC);
    let mut plain = FrameRenderer::new();
    let mut with_pass = FrameRenderer::new();
    assert!(with_pass.enable_lens_effects());
    assert!(!with_pass.enable_lens_effects(), "registered once");
    for frame in 0..2 {
        let a = display(&mut plain, &device, &off);
        let b = display(&mut with_pass, &device, &off);
        assert!(a == b, "frame {frame}: the idle pass changed the frame");
        // Strengths without the pass change nothing either.
        let c = display(&mut plain, &device, &asked);
        assert!(a == c, "frame {frame}: settings alone changed the frame");
    }
    let report = with_pass.graph_report();
    let info = report
        .extensions
        .iter()
        .find(|p| p.name == regia_render::lens_effects::LENS_EFFECTS_PASS_NAME)
        .expect("listed in the graph");
    assert_eq!(info.slot, GraphSlot::BeforePost);
    assert_eq!((info.runs, info.failures), (2, 0));
}

#[test]
fn built_in_ghosts_are_additive_and_point_symmetric() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let c = [0.5, -0.4];
    let added_at = |renderer: &mut FrameRenderer, at: [f32; 2]| {
        let off = hdr(
            renderer,
            &device,
            &scene(vec![light_at(at)], 500.0, LensEffects::OFF),
        );
        let on = hdr(renderer, &device, &scene(vec![light_at(at)], 500.0, FLARE));
        off.iter()
            .zip(&on)
            .map(|(a, b)| {
                for ch in 0..3 {
                    assert!(b[ch].is_finite(), "non-finite flare");
                    assert!(
                        b[ch] >= a[ch] - 1.0e-3 * a[ch].abs().max(1.0),
                        "flare darkened a texel"
                    );
                }
                lum(*b) - lum(*a)
            })
            .collect::<Vec<f32>>()
    };
    let recorded_before = lens_effects_counters()
        .views_recorded
        .load(std::sync::atomic::Ordering::Relaxed);
    let added = added_at(&mut renderer, c);
    assert!(
        lens_effects_counters()
            .views_recorded
            .load(std::sync::atomic::Ordering::Relaxed)
            > recorded_before
    );
    let total: f32 = added.iter().sum();
    assert!(total > 0.0, "the flare added no light");

    // Point symmetry about the frame centre: the light at -c gives the image rotated by 180°.
    let mirrored = added_at(&mut renderer, [-c[0], -c[1]]);
    let mut diff = 0.0f32;
    for y in 0..H {
        for x in 0..W {
            let a = added[(y * W + x) as usize];
            let b = mirrored[((H - 1 - y) * W + (W - 1 - x)) as usize];
            diff += (a - b).abs();
        }
    }
    assert!(diff <= 0.1 * total, "not point-symmetric: {diff} of {total}");

    let table = default_ghost_table(1.0).expect("table");
    println!("built-in table: {table:?}; added {total}, asymmetry {diff}");
}

/// Centred frame position of pixel `i`.
fn centred(i: usize) -> [f32; 2] {
    let (x, y) = ((i as u32 % W) as f32 + 0.5, (i as u32 / W) as f32 + 0.5);
    [x / W as f32 * 2.0 - 1.0, y / H as f32 * 2.0 - 1.0]
}

/// Distance in pixels between two centred frame positions.
fn pixels(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = (a[0] - b[0]) * 0.5 * W as f32;
    let dy = (a[1] - b[1]) * 0.5 * H as f32;
    (dx * dx + dy * dy).sqrt()
}

#[test]
fn a_known_ghost_lands_at_m_times_c_per_channel() {
    use regia_render::lens_effects::{GhostEntry, GhostTable};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // One small ghost on the far side of the centre, red magnified less than blue.
    let m = [-0.4f32, -0.5, -0.6];
    let table = GhostTable {
        lens: "test ghost".into(),
        coating: 1.0,
        ranked: 1,
        ghosts: vec![GhostEntry {
            first_bounce: 0,
            second_bounce: 1,
            energy: [0.01; 3],
            magnification: m,
            radius: 0.01,
        }],
    };
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(regia_render::LensEffectsPass::with_ghost_table(table)))
        .expect("register");
    let c = [0.5f32, -0.4];
    let lens = LensEffects {
        flare_intensity: 1.0,
        ..FLARE
    };
    let off = hdr(
        &mut renderer,
        &device,
        &scene(vec![light_at(c)], 500.0, LensEffects::OFF),
    );
    let on = hdr(&mut renderer, &device, &scene(vec![light_at(c)], 500.0, lens));
    // A ghost is a flat image of the aperture, so where it is reads off its centre of light.
    let peak = |channel: usize| {
        let added: Vec<f32> = off
            .iter()
            .zip(&on)
            .map(|(a, b)| b[channel] - a[channel])
            .collect();
        let value = added.iter().copied().fold(0.0f32, f32::max);
        let (mut sum, mut at) = (0.0f32, [0.0f32; 2]);
        for (index, v) in added.iter().enumerate().filter(|(_, v)| **v > 0.5 * value) {
            let c = centred(index);
            sum += v;
            at = [at[0] + c[0] * v, at[1] + c[1] * v];
        }
        ([at[0] / sum.max(1.0e-6), at[1] / sum.max(1.0e-6)], value)
    };
    let expected = |channel: usize| [m[channel] * c[0], m[channel] * c[1]];
    for channel in [0, 1, 2] {
        let (at, value) = peak(channel);
        let want = expected(channel);
        println!("channel {channel}: peak {value} at {at:?}, expected {want:?}");
        assert!(value > 0.05, "channel {channel}: no ghost ({value})");
        assert!(
            pixels(at, want) <= 4.0,
            "channel {channel}: ghost at {at:?}, expected {want:?}"
        );
        // Nothing on the light's own side: the mirrored position stays dark.
        let mirrored = [-want[0], -want[1]];
        let added: Vec<f32> = off
            .iter()
            .zip(&on)
            .map(|(a, b)| b[channel] - a[channel])
            .collect();
        assert!(
            window(&added, mirrored, 1) < 0.01 * value,
            "channel {channel}: light at -m·c"
        );
    }
    // Red and blue are told apart: each peak is nearer its own prediction than the other's.
    let (red, _) = peak(0);
    let (blue, _) = peak(2);
    assert!(pixels(red, expected(0)) < pixels(red, expected(2)));
    assert!(pixels(blue, expected(2)) < pixels(blue, expected(0)));
}

#[test]
fn chromatic_aberration_fringes_an_edge_and_leaves_flat_areas() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    // A white field from x = 0.6 to beyond the right edge: a vertical edge right of the centre,
    // where a 3 % aberration shifts red and blue by about a pixel each way.
    let field = || vec![rect([0.6, -1.2], [1.2, 1.2])];
    let fringe = LensEffects {
        chromatic_aberration: 0.03,
        ..LensEffects::OFF
    };
    let skipped_before = lens_effects_counters()
        .aberration_skipped
        .load(std::sync::atomic::Ordering::Relaxed);
    let off = hdr(&mut renderer, &device, &scene(field(), 1.0, LensEffects::OFF));
    let on = hdr(&mut renderer, &device, &scene(field(), 1.0, fringe));
    assert_eq!(
        lens_effects_counters()
            .aberration_skipped
            .load(std::sync::atomic::Ordering::Relaxed),
        skipped_before,
        "the HDR target could not be copied"
    );
    // The background (the clear colour) is faintly blue; the white field has no split at all.
    let split = |img: &[[f32; 3]]| img.iter().map(|c| (c[0] - c[2]).abs()).fold(0.0f32, f32::max);
    let baseline = split(&off);
    assert!(
        baseline < 0.05,
        "the frame without aberration is nearly neutral: {baseline}"
    );
    let fringed = split(&on);
    assert!(
        fringed > baseline + 0.2,
        "no red/blue split at the edge: {fringed} (baseline {baseline})"
    );
    // Far from the edge (the frame's left third, and deep inside the field near its middle row)
    // nothing moves: the shift there stays inside one flat region.
    let row = H / 2;
    for x in [4, W / 6, (W * 9) / 10] {
        let i = (row * W + x) as usize;
        for ch in 0..3 {
            assert!(
                (on[i][ch] - off[i][ch]).abs() <= 1.0e-3,
                "x {x} channel {ch}: {} vs {}",
                on[i][ch],
                off[i][ch]
            );
        }
    }
}

#[test]
fn dirt_only_adds_light() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let lights = || vec![light_at([0.2, 0.1])];
    let dirt = LensEffects {
        dirt_intensity: 5.0,
        flare_threshold: 1.0,
        ..LensEffects::OFF
    };
    let off = hdr(&mut renderer, &device, &scene(lights(), 2000.0, LensEffects::OFF));
    let on = hdr(&mut renderer, &device, &scene(lights(), 2000.0, dirt));
    let mut added = 0.0f32;
    let mut lit = 0usize;
    for (a, b) in off.iter().zip(&on) {
        let d = lum(*b) - lum(*a);
        assert!(d >= -1.0e-3 * lum(*a).max(1.0), "dirt darkened a texel");
        added += d;
        if d > 1.0e-4 {
            lit += 1;
        }
    }
    assert!(added > 0.0, "the dirt added nothing");
    assert!(
        lit > (W * H / 4) as usize,
        "the glare lights the dirt across the frame: {lit}"
    );
}

/// The glare that lights the dirt comes from a coarse mip, whose texel is a square; it must read
/// as a round glow. The dirt mask is the same in both frames, so dividing by a frame with an even
/// glare over the whole image cancels it and leaves the glare's own shape.
#[test]
fn the_glare_on_the_dirt_is_round() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (w, h) = (640u32, 360u32);
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let dirt = LensEffects {
        dirt_intensity: 5.0,
        flare_threshold: 1.0,
        ..LensEffects::OFF
    };
    let mut frame = |rects: Vec<LabMesh>, radiance: f32, lens: LensEffects| {
        let mut lab = scene(rects, radiance, lens);
        (lab.width, lab.height) = (w, h);
        renderer.request_hdr_capture();
        let _ = display(&mut renderer, &device, &lab);
        renderer
            .take_hdr_capture(&device)
            .expect("capture")
            .expect("an HDR capture")
            .rgb
    };
    let added = |off: &[[f32; 3]], on: &[[f32; 3]]| -> Vec<f32> {
        off.iter().zip(on).map(|(a, b)| lum(*b) - lum(*a)).collect()
    };
    let light = || vec![rect([-0.01, -0.02], [0.01, 0.02])];
    let field = || vec![rect([-1.2, -1.2], [1.2, 1.2])];
    let lit = added(
        &frame(light(), 20_000.0, LensEffects::OFF),
        &frame(light(), 20_000.0, dirt),
    );
    let even = added(
        &frame(field(), 100.0, LensEffects::OFF),
        &frame(field(), 100.0, dirt),
    );
    // Mean glare in a small window `(dx, dy)` pixels from the light, the mask divided out.
    let glare = |dx: f32, dy: f32| {
        let (cx, cy) = (((w / 2) as f32 + dx) as i32, ((h / 2) as f32 + dy) as i32);
        let (mut sum, mut n) = (0.0f32, 0);
        for y in (cy - 3)..=(cy + 3) {
            for x in (cx - 3)..=(cx + 3) {
                let i = (y as u32 * w + x as u32) as usize;
                if even[i] > 1.0e-3 {
                    sum += lit[i] / even[i];
                    n += 1;
                }
            }
        }
        sum / n.max(1) as f32
    };
    // The same distance along the axes and along the diagonals, where a square kernel differs most.
    let r = 64.0f32;
    let d = r / std::f32::consts::SQRT_2;
    let axes = (glare(r, 0.0) + glare(-r, 0.0) + glare(0.0, r) + glare(0.0, -r)) / 4.0;
    let diagonals = (glare(d, d) + glare(-d, d) + glare(d, -d) + glare(-d, -d)) / 4.0;
    println!("glare at {r} px: axes {axes}, diagonals {diagonals}");
    assert!(axes > 0.0 && diagonals > 0.0, "no glare: {axes} {diagonals}");
    assert!(
        (axes - diagonals).abs() <= 0.35 * axes.max(diagonals),
        "the glare is not round: axes {axes}, diagonals {diagonals}"
    );
}

/// The probe passes' log: pass name and the dirt strength `FramePassContext::post` carried.
type PostLog = Arc<Mutex<Vec<(&'static str, Option<f32>)>>>;

/// What a probe pass saw in `FramePassContext::post`.
struct PostProbe {
    name: &'static str,
    slot: GraphSlot,
    seen: PostLog,
}

impl FramePass for PostProbe {
    fn name(&self) -> &str {
        self.name
    }
    fn slot(&self) -> GraphSlot {
        self.slot
    }
    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((self.name, ctx.post.map(|p| p.lens_effects.dirt_intensity)));
        Ok(())
    }
}

#[test]
fn the_pass_context_carries_the_post_settings_at_before_post_only() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut renderer = FrameRenderer::new();
    for (name, slot) in [
        ("p_opaque", GraphSlot::AfterOpaque),
        ("p_before_post", GraphSlot::BeforePost),
        ("p_after_post", GraphSlot::AfterPost),
    ] {
        renderer
            .register_pass(Box::new(PostProbe {
                name,
                slot,
                seen: Arc::clone(&seen),
            }))
            .expect("register");
    }
    let lens = LensEffects {
        dirt_intensity: 0.25,
        ..LensEffects::OFF
    };
    let _ = display(
        &mut renderer,
        &device,
        &scene(vec![light_at([0.0, 0.0])], 10.0, lens),
    );
    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![
            ("p_opaque", None),
            ("p_before_post", Some(0.25)),
            ("p_after_post", None)
        ]
    );
}

/// After [`FramePass::prewarm`] the first frame that asks for every effect creates no lens
/// pipeline (the game-frame warm calls it for every registered pass).
#[test]
fn prewarm_builds_the_pipelines_the_first_frame_needs() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut pass = regia_render::LensEffectsPass::new();
    let tracked = regia_render::TrackedDevice::observe(&device.device);
    assert_eq!(pass.prewarm(&tracked), Ok(6));
    let traced_after_warm = lens_effects_counters()
        .tables_traced
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut renderer = FrameRenderer::new();
    renderer.register_pass(Box::new(pass)).expect("register");
    // The renderer's own first-frame creates come first (the record ring is short).
    let lights = || vec![light_at([0.3, 0.2])];
    let _ = display(&mut renderer, &device, &scene(lights(), 500.0, LensEffects::OFF));
    let before = regia_render::create_seq();
    let all = LensEffects {
        chromatic_aberration: 0.004,
        ..LensEffects::CINEMATIC
    };
    let _ = display(&mut renderer, &device, &scene(lights(), 500.0, all));
    let mut out = regia_render::create_buffer();
    let n = regia_render::recent_creates(before, &mut out);
    let lens_pipelines: Vec<&str> = out[..n]
        .iter()
        .filter(|r| r.kind == regia_render::FramePathKind::Pipeline && r.label().starts_with("lens effects"))
        .map(regia_render::CreateRecord::label)
        .collect();
    assert!(
        lens_pipelines.is_empty(),
        "built on the frame: {lens_pipelines:?}"
    );
    assert_eq!(
        lens_effects_counters()
            .tables_traced
            .load(std::sync::atomic::Ordering::Relaxed),
        traced_after_warm,
        "the first frame traced the lens on the frame thread"
    );
}

/// `flare_threshold` is exposed luminance. The HDR target's pre-exposure moves in whole stops, so
/// two exposures a hair apart can sit on either side of a step: the same highlight (the same
/// exposed luminance) must cast the same flare on both, or an adapting exposure makes the flare
/// pop each time it crosses a stop.
#[test]
fn the_same_exposed_highlight_casts_the_same_flare_across_a_pre_exposure_step() {
    use regia_render::{exposure_from_ev100, pre_exposure_scale};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let flare_at = |renderer: &mut FrameRenderer, ev: f32| {
        let exposure = exposure_from_ev100(ev);
        let frame = |lens: LensEffects| {
            // Radiance scaled against the exposure: the same exposed highlight at both.
            let mut lab = scene(vec![rect([0.4, -0.6], [0.6, -0.2])], 500.0 / exposure, lens);
            lab.exposure_ev = ev;
            if let Some(post) = lab.post.as_mut() {
                post.exposure_ev = ev;
            }
            lab
        };
        let off = hdr(renderer, &device, &frame(LensEffects::OFF));
        let exposed = off.iter().map(|c| lum(*c)).fold(0.0f32, f32::max) * exposure;
        // A threshold the highlight clears by a margin that a stop of pre-exposure would erase.
        let on = hdr(
            renderer,
            &device,
            &frame(LensEffects {
                flare_threshold: exposed * 2.0 / 3.0,
                ..FLARE
            }),
        );
        let added: f32 = off.iter().zip(&on).map(|(a, b)| lum(*b) - lum(*a)).sum();
        // The capture is radiance; the added light as the display sees it.
        (added * exposure, pre_exposure_scale(exposure), exposed)
    };
    // `log2(exposure)` is `-ev - log2(1.2)`, so -2.5 (the rounding step) is at EV100 about 2.237.
    let (below, pre_below, exposed_below) = flare_at(&mut renderer, 2.2);
    let (above, pre_above, exposed_above) = flare_at(&mut renderer, 2.28);
    assert_eq!(pre_below, 2.0 * pre_above, "the two exposures straddle a step");
    println!(
        "flare {below} at pre-exposure {pre_below} (highlight {exposed_below}), \
         {above} at {pre_above} (highlight {exposed_above})"
    );
    assert!(below > 0.0 && above > 0.0, "no flare: {below}, {above}");
    assert!(
        (below - above).abs() <= 0.1 * below.max(above),
        "the flare popped across the step: {below} vs {above}"
    );
}

/// A ghost of a small light is an image of the aperture: flat inside, then a real edge, not a blur.
#[test]
fn a_ghost_of_a_small_light_has_the_aperture_s_edge() {
    use regia_render::lens_effects::{GhostEntry, GhostTable};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let radius = 0.1f32;
    let table = GhostTable {
        lens: "one ghost".into(),
        coating: 1.0,
        ranked: 1,
        ghosts: vec![GhostEntry {
            first_bounce: 0,
            second_bounce: 1,
            energy: [0.05; 3],
            magnification: [-1.0; 3],
            radius,
        }],
    };
    let (w, h) = (640u32, 360u32);
    let frame = |renderer: &mut FrameRenderer, lens: LensEffects| {
        let mut lab = scene(vec![light_at([0.4, 0.0])], 5000.0, lens);
        (lab.width, lab.height) = (w, h);
        renderer.request_hdr_capture();
        let _ = display(renderer, &device, &lab);
        renderer
            .take_hdr_capture(&device)
            .expect("capture")
            .expect("an HDR capture")
            .rgb
    };
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(regia_render::LensEffectsPass::with_ghost_table(table)))
        .expect("register");
    let off = frame(&mut renderer, LensEffects::OFF);
    let on = frame(
        &mut renderer,
        LensEffects {
            flare_intensity: 1.0,
            flare_threshold: 1.0,
            ..LensEffects::OFF
        },
    );
    // The row through the ghost, at -0.4 of the half width, against the distance from its centre
    // as a fraction of its radius.
    let row = h / 2;
    let centre = 0.3 * w as f32;
    let at = |fraction: f32| {
        let x = (centre + fraction * radius * w as f32) as u32;
        let i = (row * w + x) as usize;
        lum(on[i]) - lum(off[i])
    };
    let inside = at(0.0);
    println!("profile: {:?}", [0.0, 0.5, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2].map(at));
    assert!(inside > 0.01, "no ghost: {inside}");
    assert!(
        at(0.5) > 0.7 * inside && at(-0.5) > 0.7 * inside,
        "not flat inside: {} {} {}",
        at(-0.5),
        inside,
        at(0.5)
    );
    assert!(
        at(1.2) < 0.05 * inside && at(-1.2) < 0.05 * inside,
        "light outside the aperture: {} {} of {inside}",
        at(-1.2),
        at(1.2)
    );
}

/// A wide bright area is many lights: its ghost must stay continuous, not a lattice of one sharp
/// aperture per cell of the pass's grid.
#[test]
fn a_broad_light_casts_a_continuous_ghost() {
    use regia_render::lens_effects::{GhostEntry, GhostTable};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let table = GhostTable {
        lens: "small ghost".into(),
        coating: 1.0,
        ranked: 1,
        ghosts: vec![GhostEntry {
            first_bounce: 0,
            second_bounce: 1,
            energy: [0.05; 3],
            magnification: [-1.0; 3],
            radius: 0.02,
        }],
    };
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(regia_render::LensEffectsPass::with_ghost_table(table)))
        .expect("register");
    let lens = LensEffects {
        flare_intensity: 1.0,
        flare_threshold: 1.0,
        ..LensEffects::OFF
    };
    let wide = || vec![rect([0.1, -0.8], [0.9, 0.8])];
    let off = hdr(&mut renderer, &device, &scene(wide(), 50.0, LensEffects::OFF));
    let on = hdr(&mut renderer, &device, &scene(wide(), 50.0, lens));
    // The ghost mirrors the area to x -0.9..-0.1; look at the core, away from its soft border.
    let mut samples = Vec::new();
    for y in (H / 4)..(3 * H / 4) {
        for x in (2 * W / 16)..(6 * W / 16) {
            let i = (y * W + x) as usize;
            samples.push(lum(on[i]) - lum(off[i]));
        }
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let deviation =
        (samples.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / samples.len() as f32).sqrt();
    println!("broad ghost: mean {mean}, deviation {deviation}");
    assert!(mean > 0.0, "no ghost");
    assert!(
        deviation < 0.25 * mean,
        "a lattice, not a continuous ghost: {deviation} around {mean}"
    );
}

/// The largest strengths over the brightest light the HDR target holds stay finite: the post stack
/// turns a non-finite texel black, so an overflowing ghost would be a black hole.
#[test]
fn the_largest_strengths_stay_finite() {
    use regia_render::lens_effects::{GhostEntry, GhostTable};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let table = GhostTable {
        lens: "hot ghost".into(),
        coating: 1.0,
        ranked: 1,
        ghosts: vec![GhostEntry {
            first_bounce: 0,
            second_bounce: 1,
            energy: [1.0; 3],
            magnification: [-0.5; 3],
            radius: 0.05,
        }],
    };
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(regia_render::LensEffectsPass::with_ghost_table(table)))
        .expect("register");
    let lens = LensEffects {
        flare_intensity: 1000.0,
        flare_threshold: 0.0,
        coating: 1.0,
        dirt_intensity: 10.0,
        chromatic_aberration: 0.05,

        ..LensEffects::OFF
    };
    let lights = || vec![rect([0.3, -0.4], [0.6, -0.1])];
    let on = hdr(&mut renderer, &device, &scene(lights(), 60_000.0, lens));
    for (i, texel) in on.iter().enumerate() {
        assert!(
            texel.iter().all(|c| c.is_finite()),
            "texel {i} of the HDR image is not finite: {texel:?}"
        );
    }
    // Whether an overflow turns into infinity depends on the device (lavapipe saturates), so
    // the cap itself is the check: the pass never adds more than 4096 per channel, which keeps
    // the sum with the scene inside the half-float range everywhere.
    let additive = LensEffects {
        chromatic_aberration: 0.0,
        ..lens
    };
    let off = hdr(
        &mut renderer,
        &device,
        &scene(lights(), 60_000.0, LensEffects::OFF),
    );
    let on = hdr(&mut renderer, &device, &scene(lights(), 60_000.0, additive));
    let added = off
        .iter()
        .zip(&on)
        .flat_map(|(a, b)| (0..3).map(move |ch| b[ch] - a[ch]))
        .fold(0.0f32, f32::max);
    assert!(added > 1000.0, "the ghosts did not draw: {added}");
    assert!(added <= 4096.0 * 1.01, "the pass added {added} to a channel");
}

/// Views of two sizes in turn (uneven split screen, a texture camera at its own size) keep their
/// targets: the pass builds them once, not once per view per frame.
#[test]
fn views_of_two_sizes_keep_their_targets() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let at = |size: (u32, u32)| {
        let mut lab = scene(vec![light_at([0.3, 0.2])], 500.0, LensEffects::CINEMATIC);
        (lab.width, lab.height) = size;
        lab
    };
    let (small, large) = (at((W, H)), at((W * 2, H * 2)));
    let built = || {
        lens_effects_counters()
            .targets_built
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = built();
    for _ in 0..2 {
        let _ = display(&mut renderer, &device, &small);
        let _ = display(&mut renderer, &device, &large);
    }
    assert_eq!(built() - before, 2, "one set of targets per size");
    for _ in 0..4 {
        let _ = display(&mut renderer, &device, &small);
        let _ = display(&mut renderer, &device, &large);
    }
    assert_eq!(built() - before, 2, "the sizes alternate without a rebuild");
}

/// The image the post stack reads comes from a different owner per temporal upscaler. Every one
/// must be copyable, or a game on that upscaler silently loses the chromatic aberration (and the
/// frame's per-frame copy, so a writing pass would draw on the upscaler's own image).
#[test]
fn every_temporal_upscaler_gives_the_pass_a_copyable_image() {
    use regia_render::{FrameQuality, RenderProfile, RenderProfilePreset, TemporalUpscaler};
    use std::sync::atomic::Ordering;
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = true;
    quality.render_scale_permille = 750;
    let lens = LensEffects {
        chromatic_aberration: 0.01,
        ..LensEffects::OFF
    };
    let post = PostSettings {
        taa: true,
        lens_effects: lens,
        ..PostSettings::golden_deterministic()
    };
    let lab = scene(vec![rect([0.2, -1.2], [1.2, 1.2])], 1.0, lens);
    for upscaler in [
        TemporalUpscaler::Taau,
        TemporalUpscaler::Tsr,
        TemporalUpscaler::Fsr3,
    ] {
        let mut renderer = FrameRenderer::new();
        renderer.enable_lens_effects();
        renderer.set_temporal_upscaler(upscaler);
        let counters = lens_effects_counters();
        let skipped = counters.aberration_skipped.load(Ordering::Relaxed);
        let recorded = counters.views_recorded.load(Ordering::Relaxed);
        for _ in 0..3 {
            let mut frame = lab.as_frame_scene_with_quality(quality);
            frame.post = Some(&post);
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        assert_eq!(
            counters.views_recorded.load(Ordering::Relaxed) - recorded,
            3,
            "{upscaler:?}: the pass did not draw"
        );
        assert_eq!(
            counters.aberration_skipped.load(Ordering::Relaxed),
            skipped,
            "{upscaler:?}: its output cannot be copied"
        );
    }
}

/// The starburst (ADR-3196) puts six spikes through a bright light, along the iris-blade
/// directions (`IRIS_ROTATION` plus multiples of 60 degrees in the shader), and nothing between
/// them; it only adds light and is point-symmetric about the light.
#[test]
fn starburst_spikes_follow_the_six_blades() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let lights = || vec![light_at([0.0, 0.0])];
    let burst = LensEffects {
        starburst: 4.0,
        flare_threshold: 1.0,
        ..LensEffects::OFF
    };
    let off = hdr(&mut renderer, &device, &scene(lights(), 2000.0, LensEffects::OFF));
    let on = hdr(&mut renderer, &device, &scene(lights(), 2000.0, burst));
    let added: Vec<f32> = off.iter().zip(&on).map(|(a, b)| lum(*b) - lum(*a)).collect();
    assert!(
        added.iter().all(|d| *d >= -1.0e-3),
        "the starburst darkened a texel"
    );
    // Mean added light at pixel radius `r` and angle `a` from the light, in a 3 x 3 window.
    let ring = |angle: f32, r: f32| {
        let (dx, dy) = (r * angle.cos(), r * angle.sin());
        window(&added, [dx * 2.0 / W as f32, dy * 2.0 / H as f32], 0)
    };
    let blade = 0.35f32;
    let sixth = std::f32::consts::FRAC_PI_3;
    let (mut spike, mut gap) = (0.0, 0.0);
    for k in 0..6 {
        for r in [10.0f32, 14.0] {
            spike += ring(blade + k as f32 * sixth, r);
            gap += ring(blade + (k as f32 + 0.5) * sixth, r);
        }
    }
    assert!(spike > 0.0, "no spikes drawn");
    assert!(
        spike > 2.0 * gap,
        "the spikes ({spike}) should stand well above the gaps between them ({gap})"
    );
    // Point symmetry: opposite spikes carry the same light.
    let a = ring(blade, 12.0);
    let b = ring(blade + std::f32::consts::PI, 12.0);
    assert!(
        (a - b).abs() <= 0.15 * a.max(b),
        "spike {a} against its opposite {b}"
    );
    // The strength scales the light (linear in the source, so the spike scales with it).
    let doubled = LensEffects {
        starburst: 8.0,
        ..burst
    };
    let on2 = hdr(&mut renderer, &device, &scene(lights(), 2000.0, doubled));
    let added2: Vec<f32> = off.iter().zip(&on2).map(|(a, b)| lum(*b) - lum(*a)).collect();
    let (s1, s2): (f32, f32) = (added.iter().sum(), added2.iter().sum());
    assert!((s2 / s1 - 2.0).abs() < 0.1, "twice the strength, {s1} to {s2}");
}

/// `after_dof` (ADR-3196) moves the pass behind the camera's depth of field: with no lens running
/// the frame is the same either way, with a defocused light the flare is made from the blurred
/// light (a lower, wider ghost) instead of the sharp one.
#[test]
fn after_dof_moves_the_pass_behind_the_depth_of_field() {
    use regia_render::{DepthOfField, FrameQuality, RenderProfile, RenderProfilePreset};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    let capture = |lens: LensEffects, dof: Option<DepthOfField>| {
        let mut lab = scene(vec![light_at([0.5, -0.4])], 2000.0, lens);
        if let Some(post) = lab.post.as_mut() {
            post.depth_of_field = dof;
        }
        let mut frame = lab.as_frame_scene_with_quality(quality);
        let post = lab.post.clone();
        frame.post = post.as_ref();
        let mut renderer = FrameRenderer::new();
        renderer.enable_lens_effects();
        renderer.request_hdr_capture();
        renderer.render_offscreen(&device, &frame).expect("frame");
        match renderer.take_hdr_capture(&device) {
            Ok(Some(capture)) => capture.rgb,
            other => panic!("no HDR capture: {:?}", other.map(|c| c.is_some())),
        }
    };
    let before = LensEffects {
        flare_threshold: 1.0,
        ..FLARE
    };
    let after = LensEffects {
        after_dof: true,
        ..before
    };
    // No depth of field: the slot stays where it was, the frames are identical.
    assert!(
        capture(before, None) == capture(after, None),
        "no lens, same frame"
    );
    // Defocused (focus at 1 m, the light at 5 m, wide aperture): the two orders differ, as the
    // flare reads the blurred light in one and the sharp light in the other.
    let blur = DepthOfField {
        focal_distance: 1.0,
        aperture_f_stops: 1.4,
        // A large sensor and a wide aperture: a circle of confusion of about 10 pixels at 128 x 72.
        sensor_height: 0.24,
        max_circle_of_confusion_diameter: 16.0,
        ..DepthOfField::default()
    };
    let sharp = capture(before, Some(blur));
    let late = capture(after, Some(blur));
    assert!(sharp != late, "after_dof changed nothing under a depth of field");
    let plain = capture(LensEffects::OFF, Some(blur));
    // The added light of each order over the same defocused frame: both draw, with comparable
    // total energy (the same light feeds both), but not the same picture.
    let added = |frame: &[[f32; 3]]| -> Vec<f32> {
        frame.iter().zip(&plain).map(|(a, b)| lum(*a) - lum(*b)).collect()
    };
    let (a_before, a_after) = (added(&sharp), added(&late));
    let (e_before, e_after): (f32, f32) = (a_before.iter().sum(), a_after.iter().sum());
    assert!(
        e_before > 0.0 && e_after > 0.0,
        "the flare drew: {e_before}, {e_after}"
    );
    assert!(
        (e_after / e_before - 1.0).abs() < 0.5,
        "comparable energy: {e_before} before, {e_after} after"
    );
    // Many texels of the ghost differ: the pass read a different image.
    let differing = a_before
        .iter()
        .zip(&a_after)
        .filter(|(x, y)| (*x - *y).abs() > 0.01 * x.abs().max(y.abs()).max(1.0))
        .count();
    assert!(differing > 20, "only {differing} texels differ");
}

/// What the pass costs on this adapter (a report, not a check): prints the mean milliseconds of a
/// 1280 × 720 frame with each effect, and of one ghost table trace, for the lane report and the
/// hardware runs: `cargo test -p regia_render --release --test lens_effects_frame -- --ignored
/// lens_effects_cost --nocapture`.
#[test]
#[ignore = "prints timings; needs a quiet machine to mean anything"]
fn lens_effects_cost() {
    use regia_render::lens_effects::{GhostTable, default_flare_lens};
    use std::time::Instant;
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    const FRAMES: u32 = 30;
    let mean_ms = |lens: LensEffects| {
        let mut lab = scene(vec![light_at([0.5, -0.4])], 500.0, lens);
        (lab.width, lab.height) = (1280, 720);
        let mut renderer = FrameRenderer::new();
        renderer.enable_lens_effects();
        // The first frames build the renderer's and the pass's resources.
        for _ in 0..4 {
            let _ = display(&mut renderer, &device, &lab);
        }
        let started = Instant::now();
        for _ in 0..FRAMES {
            let _ = display(&mut renderer, &device, &lab);
        }
        started.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES)
    };
    let lens = default_flare_lens();
    let started = Instant::now();
    let table = GhostTable::trace(&lens, 1.0).expect("trace");
    println!(
        "ghost table: {:.2} ms ({} ghosts of {} ranked)",
        started.elapsed().as_secs_f64() * 1000.0,
        table.ghosts.len(),
        table.ranked
    );
    let flare = LensEffects {
        dirt_intensity: 0.0,
        chromatic_aberration: 0.0,
        ..LensEffects::CINEMATIC
    };
    let dirt = LensEffects {
        dirt_intensity: 0.3,
        ..LensEffects::OFF
    };
    let aberration = LensEffects {
        chromatic_aberration: 0.002,
        ..LensEffects::OFF
    };
    let starburst = LensEffects {
        starburst: 1.0,
        flare_threshold: 1.0,
        ..LensEffects::OFF
    };
    let everything = LensEffects {
        starburst: 1.0,
        ..LensEffects::CINEMATIC
    };
    for (name, lens) in [
        ("off", LensEffects::OFF),
        ("starburst", starburst),
        ("everything (cinematic + starburst)", everything),
        ("flare", flare),
        ("dirt", dirt),
        ("aberration", aberration),
        ("cinematic", LensEffects::CINEMATIC),
    ] {
        println!("{name}: {:.2} ms per frame", mean_ms(lens));
    }
}

/// The image the post stack reads may be the temporal resolve's history (TAA) or the held-screen
/// cache of a static view, both read again next frame. The pass works on the frame's per-frame
/// copy (`FramePass::writes_color`), so a static scene with the effects on settles instead of
/// adding the flare onto itself every frame.
#[test]
fn effects_do_not_accumulate_with_taa_or_a_held_screen() {
    use regia_render::{FrameQuality, RenderProfile, RenderProfilePreset};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let lens = LensEffects {
        dirt_intensity: 2.0,
        ..FLARE
    };
    let post = PostSettings {
        taa: true,
        lens_effects: lens,
        ..PostSettings::golden_deterministic()
    };
    let mut lab = scene(vec![light_at([0.5, -0.4])], 500.0, lens);
    lab.post = Some(post.clone());
    let mut renderer = FrameRenderer::new();
    renderer.enable_lens_effects();
    let mut captures = Vec::new();
    for frame in 0..10 {
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        if frame == 4 || frame == 9 {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(&device, &scene).expect("frame");
        if frame == 4 || frame == 9 {
            let capture = renderer
                .take_hdr_capture(&device)
                .expect("capture")
                .expect("an HDR capture");
            captures.push(capture.rgb.iter().map(|c| lum(*c)).sum::<f32>());
        }
    }
    let (early, late) = (captures[0], captures[1]);
    assert!(early > 0.0 && early.is_finite());
    assert!(
        (late - early).abs() <= 0.02 * early,
        "the frame's light grew from {early} to {late}: the effects accumulate"
    );
}

/// By-eye shots for the look review (not a check): run with
/// `REGIA_LENS_SHOTS_DIR=<dir> cargo test -p regia_render --test lens_effects_frame -- --ignored lens_effects_shots`.
/// Writes `off`, `flare`, `cinematic`, `dirt` and `aberration` as binary PPM images of a dim sky with a
/// sun and two lamps.
#[test]
#[ignore = "writes images for the by-eye review; needs REGIA_LENS_SHOTS_DIR"]
fn lens_effects_shots() {
    let Ok(dir) = std::env::var("REGIA_LENS_SHOTS_DIR") else {
        return;
    };
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (w, h) = (640u32, 360u32);
    let emissive = |radiance: f32, tint: [f32; 3]| LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: tint,
        emissive_intensity: radiance,
        ..LabMaterial::default()
    };
    let with = |mut mesh: LabMesh, material: usize| {
        mesh.material = material;
        mesh
    };
    let shot = |lens: LensEffects, table: Option<regia_render::lens_effects::GhostTable>| {
        let mut lab = scene(Vec::new(), 1.0, lens);
        lab.width = w;
        lab.height = h;
        lab.materials = vec![
            emissive(0.25, [0.55, 0.65, 0.9]),
            emissive(0.08, [0.5, 0.45, 0.4]),
            emissive(20_000.0, [1.0, 0.95, 0.85]),
            emissive(400.0, [1.0, 0.8, 0.55]),
        ];
        lab.meshes = vec![
            with(rect_at([-1.2, -1.2], [1.2, 0.35], 7.0), 0),
            with(rect_at([-1.2, 0.35], [1.2, 1.2], 6.0), 1),
            with(rect([-0.58, -0.6], [-0.54, -0.53]), 2),
            with(rect([0.33, 0.18], [0.35, 0.22]), 3),
            with(rect([0.6, 0.3], [0.62, 0.34]), 3),
        ];
        let mut renderer = FrameRenderer::new();
        match table {
            Some(table) => {
                renderer
                    .register_pass(Box::new(regia_render::LensEffectsPass::with_ghost_table(table)))
                    .expect("register");
            }
            None => {
                renderer.enable_lens_effects();
            }
        }
        let _ = display(&mut renderer, &device, &lab);
        display(&mut renderer, &device, &lab)
    };
    let cases = [
        ("off", LensEffects::OFF),
        (
            "flare",
            LensEffects {
                dirt_intensity: 0.0,
                chromatic_aberration: 0.0,
                ..LensEffects::CINEMATIC
            },
        ),
        ("cinematic", LensEffects::CINEMATIC),
        (
            "dirt",
            LensEffects {
                dirt_intensity: 2.0,
                ..LensEffects::OFF
            },
        ),
        (
            "aberration",
            LensEffects {
                chromatic_aberration: 0.01,
                ..LensEffects::OFF
            },
        ),
    ];
    let mut shots: Vec<(String, Vec<u8>)> = cases
        .into_iter()
        .map(|(name, lens)| (name.to_owned(), shot(lens, None)))
        .collect();
    // The same flare at other stops of the built-in lens.
    for f_number in [8.0, 11.0, 16.0, 22.0] {
        let lens = regia_render::lens_effects::flare_lens_at(f_number);
        let table = regia_render::lens_effects::GhostTable::trace(&lens, 1.0).expect("trace");
        let flare = LensEffects {
            flare_intensity: 4.0,
            dirt_intensity: 0.0,
            chromatic_aberration: 0.0,
            ..LensEffects::CINEMATIC
        };
        shots.push((format!("f{f_number}"), shot(flare, Some(table))));
    }
    for (name, pixels) in shots {
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in pixels.as_chunks::<4>().0 {
            ppm.extend_from_slice(&px[..3]);
        }
        let path = std::path::Path::new(&dir).join(format!("{name}.ppm"));
        std::fs::write(&path, ppm).expect("write shot");
        println!("wrote {}", path.display());
    }
}
