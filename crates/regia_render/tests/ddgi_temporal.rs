// SPDX-License-Identifier: MPL-2.0
//! Temporal stability of the GPU irradiance probes (DDGI) under camera motion (ADR-424).
//!
//! The camera walks 24 m along a corridor that is the same at every x (a floor and two
//! sunlit walls far longer than the view), looking down it. A correct renderer draws the same
//! image at every step, so every frame-to-frame change and every difference from the held,
//! converged view is an error of the renderer's temporal path, not parallax
//! (`regia_render::temporal_stability`). The same walk without probes is the baseline: it
//! holds what the other passes (shadow-map texel snapping) contribute.
//!
//! Before ADR-424 the probe lookup blended each cascade into the next from the cell-snapped
//! volume, so a band at every cascade edge popped each time a volume scrolled a cell, and the
//! last cascade drew a ring of full probe light past its fade that jumped with it.

use regia_core::glam::Mat4;
use regia_render::temporal_stability::{TemporalStability, measure, rgba8_luma};
use regia_render::{
    DdgiSettings, DirectionalLightUniform, DynamicGiTier, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip ddgi_temporal: {err}");
            None
        }
    }
}

const W: u32 = 160;
const H: u32 = 96;
/// Rows below the horizon by more than 3° (the camera pitches 20° down): everything there is
/// within ~20 m, so the corridor's far end never enters the measured region.
const REGION: [u32; 4] = [0, 24, W, H];
/// Steps of the walk, and its length per step (m).
const STEPS: u32 = 96;
const STEP_M: f32 = 0.25;

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn grey(v: f32) -> LabMaterial {
    LabMaterial {
        base_color: [v, v, v, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    }
}

/// Raw frames: no temporal AA and no screen-space GI (either would hide or smear probe noise).
fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.screen_space_gi = false;
    q
}

/// Probe volumes small enough for lavapipe (as `tests/ddgi.rs`): two cascades of 12×8×12 at
/// 1 m and 3 m, settled probes re-traced every other frame, bricks baked on the caller.
fn probe_settings() -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims: [12, 8, 12],
        spacing: 1.0,
        update_divisor: 2,
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    }
}

/// The corridor seen from `x`, looking 20° down along +x. Floor and walls run 240 m, so the
/// probes and the view never reach an end during the walk.
fn corridor(x: f32) -> LabScene {
    corridor_of(x, 120.0)
}

/// [`corridor`] with the floor and walls `half` metres either side of x = 0.
fn corridor_of(x: f32, half: f32) -> LabScene {
    let meshes = vec![
        block([0.0, -0.25, 0.0], [half, 0.25, 30.0], 0),
        block([0.0, 2.0, -3.15], [half, 2.0, 0.15], 1),
        block([0.0, 2.0, 3.15], [half, 2.0, 0.15], 1),
    ];
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
    LabScene {
        width: W,
        height: H,
        eye: [x, 1.7, 0.0],
        target: [x + 8.0, -1.2, 0.0],
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: Some(post),
        // Low across the corridor: one wall's inner face sunlit, part of the floor in the
        // other wall's shadow (lit only by the bounce the probes carry).
        directional: DirectionalLightUniform::new([0.2, -0.574, -0.8], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![grey(0.6), grey(0.85)],
        meshes,
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<f32> {
    let frame = lab.as_frame_scene_with_quality(quality());
    let pixels = renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("frame: {err}"))
        .pixels;
    rgba8_luma(&pixels, W, H, Some(REGION))
}

/// One run: `settle` held frames at x = 0 (the last is the reference), the walk to
/// x = 24 m, then 60 frames held there (the same view as the reference: the probe grid has
/// the same phase at both ends). Returns (walk, held) measured against the reference.
fn run(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    settle: u32,
) -> (TemporalStability, TemporalStability) {
    run_at(renderer, device, settle, STEP_M)
}

/// [`run`] with `step_m` metres a step.
fn run_at(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    settle: u32,
    step_m: f32,
) -> (TemporalStability, TemporalStability) {
    let mut reference = Vec::new();
    for _ in 0..settle {
        reference = render(renderer, device, &corridor(0.0));
    }
    let walk: Vec<Vec<f32>> = (1..=STEPS)
        .map(|i| render(renderer, device, &corridor(i as f32 * step_m)))
        .collect();
    let end = corridor(STEPS as f32 * step_m);
    let held: Vec<Vec<f32>> = (0..60).map(|_| render(renderer, device, &end)).collect();
    let walk = measure(&walk, Some(reference.as_slice()), 1.0).unwrap_or_else(|err| panic!("walk: {err}"));
    let held = measure(&held, Some(reference.as_slice()), 1.0).unwrap_or_else(|err| panic!("held: {err}"));
    (walk, held)
}

fn report(name: &str, m: &TemporalStability) {
    let ghost = m.ghost.unwrap_or_default();
    eprintln!(
        "{name}: step mean {:.3} p99 {:.2} max {:.2} (worst pair ending at frame {}), TI {:.3}, flicker {:.3}, \
         drift {:.3}; vs held view: mean {:.3} max {:.3} last {:.3} settled from {:?}",
        m.step_mean,
        m.step_p99,
        m.step_max,
        m.worst_step_frame,
        m.temporal_information,
        m.flicker,
        m.drift,
        ghost.mean,
        ghost.max,
        ghost.last,
        ghost.settle_frame,
    );
}

/// Luma levels (0–255) throughout. The bars are the held-camera ones of `tests/ddgi.rs`
/// (`converged_probes_do_not_flicker_and_a_round_trip_returns_the_same_light`: mean |Δ| < 0.5,
/// p99 < 3, a round trip within a mean of 2), applied to the walk on top of the baseline.
#[test]
fn a_camera_walking_an_invariant_corridor_sees_no_probe_pops() {
    let Some(device) = device() else {
        return;
    };
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let (base_walk, base_held) = run(&mut flat, &device, 4);
    report("no probes, walk", &base_walk);
    report("no probes, held", &base_held);

    let mut probes = FrameRenderer::new();
    probes.set_ddgi_settings(probe_settings());
    probes.set_ddgi_enabled(Some(true));
    let (walk, held) = run(&mut probes, &device, 60);
    assert!(probes.ddgi_active(), "Desktop High runs the probes");
    report("probes, walk", &walk);
    report("probes, held", &held);

    let base_ghost = base_walk.ghost.unwrap_or_default();
    assert!(
        base_ghost.max < 3.0,
        "without probes the walk should hold the image (the corridor is not invariant?): {base_walk:?}"
    );
    // Frame to frame: no band pops where a cascade blends into the next, and no ring where
    // the last one ends.
    assert!(
        walk.step_mean <= base_walk.step_mean + 0.5 && walk.step_p99 <= base_walk.step_p99 + 3.0,
        "the probes pop or flicker under camera motion: {walk:?} vs no probes {base_walk:?}"
    );
    // Against the held view: the walk shows the light the held camera converged to.
    let ghost = walk.ghost.unwrap_or_default();
    assert!(
        ghost.mean <= base_ghost.mean + 2.0,
        "the walk's light strays from the held view: {walk:?} vs no probes {base_walk:?}"
    );
    // Held at the far end (the same view): the light comes back to the reference.
    let held_ghost = held.ghost.unwrap_or_default();
    let base_held_ghost = base_held.ghost.unwrap_or_default();
    assert!(
        held_ghost.last <= base_held_ghost.last + 2.0,
        "after the walk the probes do not return to the held light: {held:?} vs no probes {base_held:?}"
    );
}

/// The same walk and bars on the layouts that ship, at their own update rates. The 1 m layout
/// above needs no room rule, so it says nothing about the placement and the ray read every tier
/// runs (ADR-1949, `specs/79` § 9.1). Before that rule the Low tier failed the walk's bar here
/// (mean 2.312 against 2.113) and read 1.679 after it.
#[test]
fn a_camera_walking_on_a_shipped_tier_holds_the_light() {
    let Some(device) = device() else {
        return;
    };
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let (base_walk, base_held) = run(&mut flat, &device, 4);
    let base_ghost = base_walk.ghost.unwrap_or_default();
    let base_held_ghost = base_held.ghost.unwrap_or_default();

    for tier in [DynamicGiTier::Low, DynamicGiTier::High] {
        let mut probes = FrameRenderer::new();
        probes.set_ddgi_settings(DdgiSettings {
            bake_on_caller: true,
            min_proxy_radius: 0.2,
            ..tier.settings()
        });
        probes.set_ddgi_enabled(Some(true));
        let (walk, held) = run(&mut probes, &device, 60);
        assert!(probes.ddgi_active(), "{tier:?}: Desktop High runs the probes");
        report(&format!("{tier:?}, walk"), &walk);
        report(&format!("{tier:?}, held"), &held);
        assert!(
            walk.step_mean <= base_walk.step_mean + 0.5 && walk.step_p99 <= base_walk.step_p99 + 3.0,
            "{tier:?}: the probes pop or flicker under camera motion: {walk:?} vs no probes {base_walk:?}"
        );
        let ghost = walk.ghost.unwrap_or_default();
        assert!(
            ghost.mean <= base_ghost.mean + 2.0,
            "{tier:?}: the walk's light strays from the held view: {walk:?} vs no probes {base_walk:?}"
        );
        let held_ghost = held.ghost.unwrap_or_default();
        assert!(
            held_ghost.last <= base_held_ghost.last + 2.0,
            "{tier:?}: after the walk the probes do not return to the held light: {held:?} vs no probes {base_held:?}"
        );
    }
}

/// A camera that walks fast and stops (0.5 m a frame, 48 m, then held): 60 frames later the light is
/// back to the held view's (the held bar of the walks above). New probes at that speed are young when
/// the camera stops, and a settled probe re-traces only every 16 frames on Low, so before seeding
/// (ADR-2605, `specs/79` § 9.2) the Low tier was still 2.583 from the held view (bar about 2.15).
/// Every tier seeds a scrolled probe from the one it stepped past (High since ADR-5001: its fast stop
/// is `after_a_fast_camera_stops_the_light_comes_back_on_high`).
///
/// Open since ADR-5001 (Low read 2.27 on an RTX 5090 against a bar of 2.14); ADR-5290 attributes it: against a
/// field held 60 frames at the stop's own position the stop is back to 0.84, and a fresh field held at x = 48 m
/// already reads 2.13 (+1.91) from the one at x = 0, growing with the distance from the corridor's centre (+0.07
/// at 24 m, +2.71 at 72 m) with two cascades or four (`fast_stop_against_the_stop_view`,
/// `corridor_offset_follows_the_probe_slots`). The probe field is not the same along the corridor; the test is
/// left as it was.
///
/// Fixed at the cause by ADR-5338: the brick baker's f32 closest-point search was metres off on the corridor's
/// 240 m two-triangle wall faces, differently at every voxel column, so the probes traced a different wall at
/// every position along the corridor (`ddgi_bake` `a_long_wall_bakes_the_same_all_along_it`). With the search in
/// f64 the Low stop reads 0.85 on an RTX 5090 (bar 2.14; was 2.24), Medium 1.16, High 0.99.
#[test]
fn after_a_fast_camera_stops_the_light_comes_back_on_the_low_tier() {
    fast_stop_comes_back(&[DynamicGiTier::Low, DynamicGiTier::Medium]);
}

/// The same fast stop on High. Before ADR-5001 High kept the probes' defaults (no seeding, no
/// visibility blend) and read 2.065 on lavapipe and 2.173 on an RTX 5090 against bars of 2.142 and
/// 2.139, so this test was ignored. Since ADR-5001 High seeds and blends on its 1 m layout: 1.828 on
/// lavapipe and 1.831 on the RTX 5090 (2026-11-09), so it runs in every pass.
#[test]
fn after_a_fast_camera_stops_the_light_comes_back_on_high() {
    fast_stop_comes_back(&[DynamicGiTier::High]);
}

/// Walks each tier at 0.5 m a frame and holds 60 frames: the held light ends within 2 luma levels
/// of the no-probe renderer's held view.
fn fast_stop_comes_back(tiers: &[DynamicGiTier]) {
    let Some(device) = device() else {
        return;
    };
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let (_, base_held) = run_at(&mut flat, &device, 4, 0.5);
    let base_held_ghost = base_held.ghost.unwrap_or_default();
    for &tier in tiers {
        let mut probes = FrameRenderer::new();
        probes.set_ddgi_settings(DdgiSettings {
            bake_on_caller: true,
            min_proxy_radius: 0.2,
            ..tier.settings()
        });
        probes.set_ddgi_enabled(Some(true));
        let (walk, held) = run_at(&mut probes, &device, 60, 0.5);
        report(&format!("{tier:?} at 0.5 m a frame, walk"), &walk);
        report(&format!("{tier:?} at 0.5 m a frame, held"), &held);
        let held_ghost = held.ghost.unwrap_or_default();
        assert!(
            held_ghost.last <= base_held_ghost.last + 2.0,
            "{tier:?}: 60 frames after a fast camera stops the probes still miss the held light: \
             {held:?} vs no probes {base_held:?}"
        );
    }
}

/// ADR-2605 for the owner: the fast stop on High with seeding switched on (High keeps it off) and
/// on Medium. `cargo test -p regia_render --test ddgi_temporal -- --ignored --nocapture fast_stop_opt_in`.
#[test]
#[ignore = "diagnostics: prints the fast stop with seeding on High and Medium, asserts nothing"]
fn fast_stop_opt_in() {
    let Some(device) = device() else {
        return;
    };
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let (_, base_held) = run_at(&mut flat, &device, 4, 0.5);
    eprintln!(
        "bar (no probes held last + 2): {:.3}",
        base_held.ghost.unwrap_or_default().last + 2.0
    );
    let high_seeded = DdgiSettings {
        seed_updates: regia_render::dynamic_gi::LOW_SEED_UPDATES,
        visibility_blend: true,
        ..DynamicGiTier::High.settings()
    };
    let recent = |s: DdgiSettings| DdgiSettings {
        recent_weight: 3,
        ..s
    };
    for (name, settings) in [
        ("High + ADR-2605", high_seeded.clone()),
        ("High + ADR-2605 + recent_weight 3", recent(high_seeded)),
        ("Medium", DynamicGiTier::Medium.settings()),
        ("Low + recent_weight 3", recent(DynamicGiTier::Low.settings())),
    ] {
        let mut probes = FrameRenderer::new();
        probes.set_ddgi_settings(DdgiSettings {
            bake_on_caller: true,
            min_proxy_radius: 0.2,
            ..settings
        });
        probes.set_ddgi_enabled(Some(true));
        let (walk, held) = run_at(&mut probes, &device, 60, 0.5);
        report(&format!("{name} at 0.5 m a frame, walk"), &walk);
        report(&format!("{name} at 0.5 m a frame, held"), &held);
    }
}

/// ADR-5938's candidate for the Low tier under a moving camera (measurement): High's probe grid with Low's brick
/// pool and screen lookup region, each settled probe re-traced every 64 frames (`gi_acceptance` `low_dense_ddgi`),
/// beside Low and High as they ship. The walk of [`a_camera_walking_on_a_shipped_tier_holds_the_light`] and the
/// fast stop of [`fast_stop_comes_back`], with their bars printed. A new probe traces every frame for its first
/// sixteen updates on every layout, so what a slower settled rate changes is how long the probes behind the
/// camera keep the light they took while young.
/// `cargo test -p regia_render --test ddgi_temporal -- --ignored --nocapture the_low_dense_candidate`.
#[test]
#[ignore = "measurement: ADR-5938's candidate layout on the walk and the fast stop, prints only"]
fn the_low_dense_candidate_under_a_moving_camera() {
    let Some(device) = device() else {
        return;
    };
    let low = DynamicGiTier::Low.settings();
    let dense = DdgiSettings {
        update_divisor: 64,
        max_instances_per_cascade: low.max_instances_per_cascade,
        sdf_pool_bytes: low.sdf_pool_bytes,
        screen_max: low.screen_max,
        ..DynamicGiTier::High.settings()
    };
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let (base_walk, base_held) = run(&mut flat, &device, 4);
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let (_, base_fast_held) = run_at(&mut flat, &device, 4, 0.5);
    eprintln!(
        "bars: walk step mean {:.3}, p99 {:.2}, vs held view mean {:.3}; held last {:.3}; fast stop held last {:.3}",
        base_walk.step_mean + 0.5,
        base_walk.step_p99 + 3.0,
        base_walk.ghost.unwrap_or_default().mean + 2.0,
        base_held.ghost.unwrap_or_default().last + 2.0,
        base_fast_held.ghost.unwrap_or_default().last + 2.0,
    );
    for (name, settings) in [
        ("Low", low),
        ("High", DynamicGiTier::High.settings()),
        ("candidate (High's grid, divisor 64)", dense),
    ] {
        for (label, step_m) in [
            ("walk at 0.25 m a frame", STEP_M),
            ("fast stop at 0.5 m a frame", 0.5),
        ] {
            let mut probes = FrameRenderer::new();
            probes.set_ddgi_settings(DdgiSettings {
                bake_on_caller: true,
                min_proxy_radius: 0.2,
                ..settings.clone()
            });
            probes.set_ddgi_enabled(Some(true));
            let (walk, held) = run_at(&mut probes, &device, 60, step_m);
            report(&format!("{name}, {label}, walk"), &walk);
            report(&format!("{name}, {label}, held"), &held);
        }
    }
}

/// Mean and signed mean of `a − b`, luma levels.
fn diff(a: &[f32], b: &[f32]) -> (f32, f32) {
    let n = a.len().min(b.len()).max(1) as f32;
    let (abs, signed) = a
        .iter()
        .zip(b)
        .fold((0.0, 0.0), |(s, t), (x, y)| (s + (x - y).abs(), t + (x - y)));
    (abs / n, signed / n)
}

/// Measurement (ADR-5290): what the fast stop's remainder is. The reference is the view after 60 held
/// frames at x = 0, itself a young field; this compares the end of the fast stop with it and with a
/// field held 600 frames at either end, signed (positive: brighter). Prints only.
/// `REGIA_FAST_STOP_VARIANTS=word,...` keeps the variants whose names contain a word.
#[test]
#[ignore = "measurement: the fast stop's remainder against young and long-held fields, prints only"]
fn fast_stop_attribution() {
    let Some(device) = device() else {
        return;
    };
    let low = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DynamicGiTier::Low.settings()
    };
    let variants: Vec<(&str, DdgiSettings)> = vec![
        ("low", low.clone()),
        (
            "low, seed 0",
            DdgiSettings {
                seed_updates: 0,
                ..low.clone()
            },
        ),
        (
            "low, recent 1",
            DdgiSettings {
                recent_weight: 1,
                ..low.clone()
            },
        ),
        (
            "low, divisor 8",
            DdgiSettings {
                update_divisor: 8,
                ..low.clone()
            },
        ),
        (
            "medium",
            DdgiSettings {
                bake_on_caller: true,
                min_proxy_radius: 0.2,
                ..DynamicGiTier::Medium.settings()
            },
        ),
        (
            "high",
            DdgiSettings {
                bake_on_caller: true,
                min_proxy_radius: 0.2,
                ..DynamicGiTier::High.settings()
            },
        ),
    ];
    let only = std::env::var("REGIA_FAST_STOP_VARIANTS").ok();
    let end_x = STEPS as f32 * 0.5;
    for (name, settings) in variants {
        if only
            .as_deref()
            .is_some_and(|o| !o.split(',').any(|w| name.contains(w.trim())))
        {
            continue;
        }
        // A field held at x = 0: the reference at 60 frames, then 600 frames.
        let mut held = FrameRenderer::new();
        held.set_ddgi_settings(settings.clone());
        held.set_ddgi_enabled(Some(true));
        let mut young = Vec::new();
        let mut old = Vec::new();
        for i in 0..600 {
            let px = render(&mut held, &device, &corridor(0.0));
            if i == 59 {
                young = px;
            } else if i == 599 {
                old = px;
            }
        }
        drop(held);
        // The fast stop, then held on: 60 frames (the test's frame) and 600.
        let mut walker = FrameRenderer::new();
        walker.set_ddgi_settings(settings);
        walker.set_ddgi_enabled(Some(true));
        for _ in 0..60 {
            render(&mut walker, &device, &corridor(0.0));
        }
        for i in 1..=STEPS {
            render(&mut walker, &device, &corridor(i as f32 * 0.5));
        }
        let mut stop60 = Vec::new();
        let mut stop600 = Vec::new();
        for i in 0..600 {
            let px = render(&mut walker, &device, &corridor(end_x));
            if i == 59 {
                stop60 = px;
            } else if i == 599 {
                stop600 = px;
            }
        }
        let show = |(abs, signed): (f32, f32)| format!("{abs:.3} ({signed:+.3})");
        eprintln!(
            "{name}: stop+60 vs ref60 {} | stop+60 vs held600 {} | ref60 vs held600 {} | stop+600 vs held600 {} | \
             stop+60 vs stop+600 {}",
            show(diff(&stop60, &young)),
            show(diff(&stop60, &old)),
            show(diff(&young, &old)),
            show(diff(&stop600, &old)),
            show(diff(&stop60, &stop600)),
        );
    }
}

/// Measurement (ADR-5290): whether the corridor is the same view at both ends of the fast stop's walk. A
/// fresh field held 600 frames at x = 24 and 48 m against one held at x = 0, signed (positive: brighter),
/// in the test's 240 m corridor and in one 2400 m long. Prints only.
#[test]
#[ignore = "measurement: the corridor's held light along the walk, prints only"]
fn corridor_held_light_along_the_walk() {
    let Some(device) = device() else {
        return;
    };
    let tiers: Vec<(&str, DdgiSettings)> = [DynamicGiTier::Low, DynamicGiTier::Medium, DynamicGiTier::High]
        .into_iter()
        .map(|t| {
            (
                t.name(),
                DdgiSettings {
                    bake_on_caller: true,
                    min_proxy_radius: 0.2,
                    ..t.settings()
                },
            )
        })
        .collect();
    for (name, settings) in tiers {
        for half in [120.0, 1200.0] {
            let held_at = |x: f32| {
                let mut r = FrameRenderer::new();
                r.set_ddgi_settings(settings.clone());
                r.set_ddgi_enabled(Some(true));
                let mut px = Vec::new();
                for _ in 0..600 {
                    px = render(&mut r, &device, &corridor_of(x, half));
                }
                px
            };
            let start = held_at(0.0);
            let mid = held_at(24.0);
            let end = held_at(48.0);
            let show = |(abs, signed): (f32, f32)| format!("{abs:.3} ({signed:+.3})");
            eprintln!(
                "{name}, corridor {:.0} m: held 600 at x = 24 vs x = 0 {} | x = 48 vs x = 0 {}",
                2.0 * half,
                show(diff(&mid, &start)),
                show(diff(&end, &start)),
            );
        }
    }
}

/// Measurement (ADR-5290): the fast stop against a field held at the stop's own position (the view a camera that
/// stood there sees), and the position offset by layout: Low with only its 2 and 6 m cascades (both whole cells
/// at x = 48), and x = 108 m (a whole cell of every Low cascade). Signed, positive: brighter. Prints only.
#[test]
#[ignore = "measurement: the fast stop against the stop position's own held field, prints only"]
fn fast_stop_against_the_stop_view() {
    let Some(device) = device() else {
        return;
    };
    let with = |t: DynamicGiTier| DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..t.settings()
    };
    let held_at = |settings: &DdgiSettings, x: f32, half: f32, frames: u32| {
        let mut r = FrameRenderer::new();
        r.set_ddgi_settings(settings.clone());
        r.set_ddgi_enabled(Some(true));
        let mut px = Vec::new();
        for _ in 0..frames {
            px = render(&mut r, &device, &corridor_of(x, half));
        }
        px
    };
    let show = |(abs, signed): (f32, f32)| format!("{abs:.3} ({signed:+.3})");
    let low2 = DdgiSettings {
        cascades: 2,
        ..with(DynamicGiTier::Low)
    };
    for (name, settings, x, half) in [
        ("low, 2 cascades, x = 48", low2, 48.0, 120.0),
        ("low, x = 108", with(DynamicGiTier::Low), 108.0, 120.0),
        ("low, x = 108, 2400 m", with(DynamicGiTier::Low), 108.0, 1200.0),
    ] {
        let a = held_at(&settings, 0.0, half, 600);
        let b = held_at(&settings, x, half, 600);
        eprintln!("{name}: held 600 vs x = 0 {}", show(diff(&b, &a)));
    }
    let end_x = STEPS as f32 * 0.5;
    for tier in [DynamicGiTier::Low, DynamicGiTier::Medium, DynamicGiTier::High] {
        let settings = with(tier);
        let stop_view = held_at(&settings, end_x, 120.0, 60);
        let start_view = held_at(&settings, 0.0, 120.0, 60);
        let mut walker = FrameRenderer::new();
        walker.set_ddgi_settings(settings);
        walker.set_ddgi_enabled(Some(true));
        for _ in 0..60 {
            render(&mut walker, &device, &corridor(0.0));
        }
        for i in 1..=STEPS {
            render(&mut walker, &device, &corridor(i as f32 * 0.5));
        }
        let mut stop60 = Vec::new();
        for _ in 0..60 {
            stop60 = render(&mut walker, &device, &corridor(end_x));
        }
        eprintln!(
            "{}: stop+60 vs held 60 at the stop {} | vs held 60 at x = 0 {} | held 60 at the stop vs at x = 0 {}",
            tier.name(),
            show(diff(&stop60, &stop_view)),
            show(diff(&stop60, &start_view)),
            show(diff(&stop_view, &start_view)),
        );
    }
}

/// Measurement (ADR-5290): whether the corridor's position offset follows the probe slots. The volume maps a
/// cell to its slot modulo the probes per axis (12 on Low) and a settled probe re-traces on the frames its
/// slot picks, so the same corridor at x = 72 m (36 cells of 2 m and 12 of 6 m: every slot of Low's two finest
/// cascades as at x = 0) runs the same rays in the same order, and at x = 48 m (8 cells of 6 m) it does not.
/// Fresh fields, 600 held frames, signed. Prints only.
#[test]
#[ignore = "measurement: the corridor's held light at slot-aligned and unaligned positions, prints only"]
fn corridor_offset_follows_the_probe_slots() {
    let Some(device) = device() else {
        return;
    };
    let low = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DynamicGiTier::Low.settings()
    };
    let low2 = DdgiSettings {
        cascades: 2,
        ..low.clone()
    };
    let held_at = |settings: &DdgiSettings, x: f32| {
        let mut r = FrameRenderer::new();
        r.set_ddgi_settings(settings.clone());
        r.set_ddgi_enabled(Some(true));
        let mut px = Vec::new();
        for _ in 0..600 {
            px = render(&mut r, &device, &corridor(x));
        }
        px
    };
    let show = |(abs, signed): (f32, f32)| format!("{abs:.3} ({signed:+.3})");
    for (name, settings) in [("low, 2 cascades", low2), ("low", low)] {
        let start = held_at(&settings, 0.0);
        for x in [24.0, 48.0, 72.0] {
            let other = held_at(&settings, x);
            eprintln!(
                "{name}: held 600 at x = {x} vs x = 0 {}",
                show(diff(&other, &start))
            );
        }
    }
}
