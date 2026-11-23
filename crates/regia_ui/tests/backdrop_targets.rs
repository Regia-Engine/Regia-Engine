// SPDX-License-Identifier: MPL-2.0
//! Backdrop blur render targets across repeated open and close (v0.5 row C10, ADR-5420).
//!
//! A frosted panel (`BackdropBlur`) is opened and closed 50 times through the whole UI path:
//! spawn, extract, draw into an offscreen target, despawn. `BackdropStats::live_targets` (also in
//! `ui.backdrop_stats`) counts the scratch textures the blur holds. The baseline is the count after
//! the first open and close, the warm cycle: the pool keeps the frame copy and one pair per
//! downsample factor, so a later open reuses them. After every close the count is back at that
//! baseline, and after the warm cycle no draw creates a texture or a pipeline (frame-path
//! counters, this thread only). Runs at UI scale 1.0 and 2.25 (2.25 moves a sigma 8 panel from
//! downsample factor 2 to factor 4 with its prefilter).
//!
//! Needs an adapter: lavapipe by default; a real card with `REGIA_GPU_UUID=<uuid>
//! REGIA_GPU_STRICT=1`. Fixed steps only, no clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_render::RenderDevice;
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::{BackdropBlur, BackdropStats, BackgroundKind, OffscreenTarget, UiScales};
use serde_json::json;

/// Open and close cycles per run.
const CYCLES: u32 = 50;
/// Frames drawn while the panel is open, and again after it closes.
const FRAMES_PER_STATE: u32 = 2;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
/// It is also the Intel Arc fix of ADR-7557: a test opening and dropping its own device destroyed
/// one while other test threads named objects, the Vulkan loader list race of ADR-2113
/// (`fifty_agent_toggles_leave_the_live_count_at_baseline` died in `loader_get_icd_and_device`).
/// The pool's books and the frame-path counters each test reads are its own app's and its own
/// thread's, so sharing the device changes none of them.
fn device() -> SerialGpu {
    let device = match serial_device() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let info = device.adapter.get_info();
    eprintln!("backdrop_targets adapter: {} ({:?})", info.name, info.backend);
    device
}

/// A code-only UI app with a `w`x`h` window at OS scale `scale` and one full-window root whose
/// UI px are device-independent px (so `scale` is the UI scale).
fn ui_app(w: u32, h: u32, scale: f64) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // Startup picks a default window size; the tests use their own.
    app.update();
    set_window(&mut app, w, h, scale);
    let root = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPhysicalSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.2, 0.3, 0.4, 1.0])),
            },
        ))
        .id();
    (app, root)
}

fn set_window(app: &mut App, w: u32, h: u32, scale: f64) {
    app.insert_resource(UiScales {
        window_size: [w as f32, h as f32],
        scale_factor: scale,
    });
}

/// A tinted, rounded, frosted panel at `x`, `y` (UI px), 140x90 UI px, blur `sigma` UI px.
fn open_panel(app: &mut App, root: Entity, x: f32, y: f32, sigma: f32) -> Entity {
    let panel = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(x),
                    top: Dimension::Points(y),
                    right: Dimension::Auto,
                    bottom: Dimension::Auto,
                },
                size: [Dimension::Points(140.0), Dimension::Points(90.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.05, 0.07, 0.1, 0.45])),
            },
            Border {
                radius: [12.0; 4],
                ..Default::default()
            },
            BackdropBlur { radius: sigma },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(panel);
    panel
}

/// What one frame did on this thread's device calls, summed over frames.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Created {
    textures: u64,
    pipelines: u64,
    bind_groups: u64,
}

impl Created {
    fn add(&mut self, delta: regia_render::FramePathCounters) {
        self.textures += delta.textures;
        self.pipelines += delta.pipelines;
        self.bind_groups += delta.bind_groups;
    }
}

/// One app update plus one UI draw into `target`; returns the stats after the draw and what the
/// draw created.
fn frame(app: &mut App, device: &RenderDevice, target: &OffscreenTarget) -> (BackdropStats, Created) {
    app.update();
    let shared = app.world().resource::<SharedUiRenderer>();
    let before = regia_render::frame_path_counters();
    target.draw_ui(device, &mut shared.lock());
    let delta = regia_render::frame_path_counters().since(before);
    let mut created = Created::default();
    created.add(delta);
    (shared.lock().backdrop_stats(), created)
}

/// `FRAMES_PER_STATE` frames; the stats of the last and what they all created.
fn frames(app: &mut App, device: &RenderDevice, target: &OffscreenTarget) -> (BackdropStats, Created) {
    let mut created = Created::default();
    let mut last = BackdropStats::default();
    for _ in 0..FRAMES_PER_STATE {
        let (stats, c) = frame(app, device, target);
        last = stats;
        created.textures += c.textures;
        created.pipelines += c.pipelines;
        created.bind_groups += c.bind_groups;
    }
    (last, created)
}

/// Backdrop quads in the renderer's current list (what extract installed).
fn backdrop_quads(app: &App) -> usize {
    let shared = app.world().resource::<SharedUiRenderer>();
    let quads = shared.lock().quads().to_vec();
    quads
        .iter()
        .filter(|q| q.texture == regia_ui::TextureRef::Backdrop)
        .count()
}

/// The pool's own books always balance: live = created - freed.
fn assert_books(stats: &BackdropStats, at: &str) {
    assert_eq!(
        u64::from(stats.live_targets),
        stats.targets_created - stats.targets_freed,
        "{at}: live targets disagree with created - freed: {stats:?}"
    );
    assert_eq!(
        stats.live_targets == 0,
        stats.live_target_bytes == 0,
        "{at}: bytes without targets or targets without bytes: {stats:?}"
    );
}

/// Opens the panels of `sigmas` (UI px) and closes them `CYCLES` times at UI scale `scale` and
/// checks the live count and the creations; returns the baseline.
fn cycle(scale: f64, sigmas: &[f32], expect_baseline: u32) -> u32 {
    let (w, h) = (720, 480);
    let device = device();
    let target = OffscreenTarget::new(&device, w, h);
    let (mut app, root) = ui_app(w, h, scale);
    let (cold, _) = frames(&mut app, &device, &target);
    assert_eq!(cold.live_targets, 0, "no blur drawn yet: {cold:?}");
    assert_books(&cold, "cold");
    let mut baseline = None;
    let mut created_after_warm = Created::default();
    for n in 0..CYCLES {
        let panels: Vec<Entity> = sigmas
            .iter()
            .enumerate()
            .map(|(i, &sigma)| open_panel(&mut app, root, 20.0 + 150.0 * i as f32, 30.0, sigma))
            .collect();
        let (open, open_created) = frames(&mut app, &device, &target);
        assert_eq!(
            (open.quads, open.blurred, open.skipped),
            (sigmas.len() as u32, sigmas.len() as u32, 0),
            "cycle {n} at scale {scale}: every panel must blur: {open:?}"
        );
        assert_books(&open, "open");
        for panel in panels {
            app.world_mut().despawn(panel);
        }
        let (closed, closed_created) = frames(&mut app, &device, &target);
        let listed = backdrop_quads(&app);
        assert_eq!(listed, 0, "cycle {n}: the closed panel is still extracted");
        assert_eq!(closed.quads, 0, "cycle {n}: the panel is gone: {closed:?}");
        assert_books(&closed, "closed");
        match baseline {
            None => {
                // The warm cycle: the pool now holds the frame copy and a pair per factor.
                assert_eq!(
                    closed.live_targets, expect_baseline,
                    "warm cycle at scale {scale}: {closed:?}"
                );
                assert!(closed.live_target_bytes > 0);
                baseline = Some(closed);
            }
            Some(base) => {
                assert_eq!(
                    open.live_targets, base.live_targets,
                    "cycle {n} at scale {scale}: an open allocated past the pool: {open:?}"
                );
                assert_eq!(
                    (closed.live_targets, closed.live_target_bytes),
                    (base.live_targets, base.live_target_bytes),
                    "cycle {n} at scale {scale}: the close did not return to baseline: {closed:?}"
                );
                assert_eq!(
                    (closed.targets_created, closed.targets_freed),
                    (base.targets_created, base.targets_freed),
                    "cycle {n} at scale {scale}: the blur made or dropped a target after the warm cycle"
                );
                created_after_warm.textures += open_created.textures + closed_created.textures;
                created_after_warm.pipelines += open_created.pipelines + closed_created.pipelines;
                created_after_warm.bind_groups += open_created.bind_groups + closed_created.bind_groups;
            }
        }
    }
    assert_eq!(
        created_after_warm,
        Created::default(),
        "after the warm cycle at scale {scale} the draws created device objects"
    );
    baseline.map(|b| b.live_targets).unwrap_or(0)
}

#[test]
fn fifty_open_close_cycles_at_ui_scale_1_return_to_baseline_and_create_nothing_after_the_first() {
    // Sigma 8 UI px = 8 px: downsample factor 2. Frame copy + one pair.
    assert_eq!(cycle(1.0, &[8.0], 3), 3);
}

#[test]
fn fifty_open_close_cycles_at_ui_scale_2_25_return_to_baseline_and_create_nothing_after_the_first() {
    // Sigma 8 UI px = 18 px: factor 4 with the box prefilter. Frame copy + one pair.
    assert_eq!(cycle(2.25, &[8.0], 3), 3);
}

#[test]
fn two_panels_of_different_factors_hold_one_pair_each_at_both_scales() {
    // Sigma 3 and 12 UI px: factors 1 and 2 at scale 1, factors 2 and 4 at scale 2.25.
    assert_eq!(cycle(1.0, &[3.0, 12.0], 5), 5);
    assert_eq!(cycle(2.25, &[3.0, 12.0], 5), 5);
}

/// A resize drops every scratch texture of the old size, including a factor the new frame does not
/// use, so the pool never holds targets that no draw can reuse.
#[test]
fn a_resize_frees_the_old_size_and_the_pool_stays_bounded() {
    let device = device();
    let small = OffscreenTarget::new(&device, 480, 320);
    let large = OffscreenTarget::new(&device, 720, 480);
    let (mut app, root) = ui_app(480, 320, 1.0);
    let a = open_panel(&mut app, root, 20.0, 30.0, 3.0);
    let b = open_panel(&mut app, root, 200.0, 30.0, 12.0);
    let (both, _) = frames(&mut app, &device, &small);
    assert_eq!((both.blurred, both.live_targets, both.targets_created), (2, 5, 5));
    app.world_mut().despawn(b);
    set_window(&mut app, 720, 480, 1.0);
    let (one, _) = frames(&mut app, &device, &large);
    assert_eq!(one.blurred, 1, "{one:?}");
    // The new frame copy and the factor-1 pair; the factor-2 pair of the old size is gone.
    assert_eq!(one.live_targets, 3, "stale targets kept after a resize: {one:?}");
    assert_eq!((one.targets_created, one.targets_freed), (8, 5), "{one:?}");
    assert_books(&one, "resized");
    app.world_mut().despawn(a);
    let mut base = None;
    for _ in 0..10 {
        let p = open_panel(&mut app, root, 20.0, 30.0, 3.0);
        frames(&mut app, &device, &large);
        app.world_mut().despawn(p);
        let (closed, _) = frames(&mut app, &device, &large);
        assert_books(&closed, "after resize");
        let base = *base.get_or_insert(closed);
        assert_eq!(closed, base, "the pool moved after the resize settled");
    }
}

/// The agent path of the device check: `ui.set_backdrop_blur` frosts and clears one panel 50 times
/// and `ui.backdrop_stats` reads the live count after every clear.
#[test]
fn fifty_agent_toggles_leave_the_live_count_at_baseline() {
    let device = device();
    let target = OffscreenTarget::new(&device, 720, 480);
    let (mut app, root) = ui_app(720, 480, 2.25);
    let panel = open_panel(&mut app, root, 20.0, 30.0, 8.0);
    app.world_mut().entity_mut(panel).remove::<BackdropBlur>();
    frames(&mut app, &device, &target);
    let call = |app: &mut App, method: &str, params: serde_json::Value| {
        regia_ui::agent_bridge::call(app.world_mut(), method, &params).expect(method)
    };
    let cold = call(&mut app, "ui.backdrop_stats", json!({}));
    assert_eq!(cold["live_targets"], json!(0), "{cold}");
    let mut baseline = None;
    for n in 0..CYCLES {
        call(
            &mut app,
            "ui.set_backdrop_blur",
            json!({ "node": panel.to_bits(), "radius": 8 }),
        );
        frames(&mut app, &device, &target);
        let open = call(&mut app, "ui.backdrop_stats", json!({}));
        assert_eq!(open["blurred"], json!(1), "cycle {n}: {open}");
        call(
            &mut app,
            "ui.set_backdrop_blur",
            json!({ "node": panel.to_bits(), "radius": 0 }),
        );
        frames(&mut app, &device, &target);
        let closed = call(&mut app, "ui.backdrop_stats", json!({}));
        assert_eq!(closed["quads"], json!(0), "cycle {n}: {closed}");
        let keys = [
            "live_targets",
            "live_target_bytes",
            "targets_created",
            "targets_freed",
        ];
        let now: Vec<serde_json::Value> = keys.iter().map(|k| closed[*k].clone()).collect();
        let base = baseline.get_or_insert_with(|| now.clone());
        assert_eq!(&now, base, "cycle {n}: {closed}");
    }
    let base = baseline.expect("ran");
    assert_eq!(base[0], json!(3), "frame copy + one pair: {base:?}");
}

/// A closed menu releases its targets after 120 idle window draws, then warms once on reopen.
#[test]
fn idle_backdrop_targets_are_trimmed_and_reopen_once() {
    let device = device();
    let target = OffscreenTarget::new(&device, 320, 200);
    let (mut app, root) = ui_app(320, 200, 1.0);
    let panel = open_panel(&mut app, root, 20.0, 30.0, 8.0);
    let (open, _) = frames(&mut app, &device, &target);
    assert_eq!(open.live_targets, 3);
    app.world_mut().despawn(panel);
    for idle in 1..=120 {
        let (stats, created) = frame(&mut app, &device, &target);
        assert_books(&stats, "idle");
        assert_eq!(created, Created::default());
        assert_eq!(
            stats.live_targets,
            if idle < 120 { 3 } else { 0 },
            "idle {idle}: {stats:?}"
        );
    }
    let stats =
        regia_ui::agent_bridge::call(app.world_mut(), "ui.backdrop_stats", &json!({})).expect("stats");
    assert_eq!(stats["live_targets"], json!(0));
    assert_eq!(stats["live_target_bytes"], json!(0));
    open_panel(&mut app, root, 20.0, 30.0, 8.0);
    let (reopened, created) = frame(&mut app, &device, &target);
    assert_books(&reopened, "reopened");
    assert_eq!(
        (reopened.live_targets, created.textures, created.pipelines),
        (3, 3, 0)
    );
    let (_, warm) = frame(&mut app, &device, &target);
    assert_eq!(warm, Created::default());
}

/// Empty UI lists and tint-only draws cannot keep a closed menu's scratch textures alive.
#[test]
fn idle_trim_counts_empty_and_skipped_window_draws() {
    let device = device();
    let target = OffscreenTarget::new(&device, 128, 128);
    for empty in [true, false] {
        let mut renderer = regia_ui::UiRenderer::default();
        let quads = vec![regia_ui::UiQuad {
            texture: regia_ui::TextureRef::Backdrop,
            blur: 8.0,
            ..regia_ui::UiQuad::fill([16.0, 16.0], [64.0, 64.0], UiColor::WHITE)
        }];
        renderer.set_quads(quads);
        target.draw_ui(&device, &mut renderer);
        assert_eq!(renderer.backdrop_stats().live_targets, 3);
        if empty {
            renderer.set_quads(Vec::new());
        }
        for idle in 1..=120 {
            let mut encoder = device.device.create_command_encoder(&Default::default());
            // No readable target: a present backdrop quad can only be skipped.
            renderer.draw(
                &device,
                &mut encoder,
                target.view(),
                OffscreenTarget::FORMAT,
                128,
                128,
            );
            device.queue.submit([encoder.finish()]);
            let stats = renderer.backdrop_stats();
            assert_books(&stats, "empty or skipped");
            assert_eq!(stats.blurred, 0);
            assert_eq!(stats.skipped, u32::from(!empty));
            assert_eq!(
                stats.live_targets,
                if idle < 120 { 3 } else { 0 },
                "empty {empty}, idle {idle}"
            );
        }
    }
}

/// One successful blur before the deadline restarts all 120 idle draws without allocating.
#[test]
fn a_backdrop_draw_restarts_the_idle_trim_count() {
    let device = device();
    let target = OffscreenTarget::new(&device, 320, 200);
    let (mut app, root) = ui_app(320, 200, 1.0);
    let panel = open_panel(&mut app, root, 20.0, 30.0, 8.0);
    frames(&mut app, &device, &target);
    app.world_mut().despawn(panel);
    for _ in 0..119 {
        let (stats, _) = frame(&mut app, &device, &target);
        assert_eq!(stats.live_targets, 3);
    }
    let panel = open_panel(&mut app, root, 20.0, 30.0, 8.0);
    let (stats, created) = frame(&mut app, &device, &target);
    assert_eq!(stats.blurred, 1);
    assert_eq!(created, Created::default());
    app.world_mut().despawn(panel);
    for _ in 0..119 {
        let (stats, created) = frame(&mut app, &device, &target);
        assert_eq!(stats.live_targets, 3);
        assert_eq!(created, Created::default());
    }
    let (stats, _) = frame(&mut app, &device, &target);
    assert_eq!(stats.live_targets, 0);
    assert_books(&stats, "trimmed after reset");
}

/// Memory-pressure trimming frees every factor, is idempotent, and keeps compiled pipelines.
#[test]
fn explicit_backdrop_trim_releases_all_factors_and_keeps_the_warm_pipelines() {
    let device = device();
    let target = OffscreenTarget::new(&device, 128, 128);
    let mut renderer = regia_ui::UiRenderer::default();
    assert_eq!(regia_ui::UiRenderer::BACKDROP_IDLE_FRAMES, 120);
    renderer.trim_backdrop();
    assert_eq!(renderer.backdrop_stats(), BackdropStats::default());
    renderer.set_quads(
        [4.0, 8.0, 30.0]
            .into_iter()
            .map(|sigma| regia_ui::UiQuad {
                texture: regia_ui::TextureRef::Backdrop,
                blur: sigma,
                ..regia_ui::UiQuad::fill([16.0, 16.0], [64.0, 64.0], UiColor::WHITE)
            })
            .collect(),
    );
    target.draw_ui(&device, &mut renderer);
    assert_eq!(renderer.backdrop_stats().live_targets, 7);
    renderer.trim_backdrop();
    let trimmed = renderer.backdrop_stats();
    assert_books(&trimmed, "explicit trim");
    assert_eq!((trimmed.live_targets, trimmed.live_target_bytes), (0, 0));
    assert_eq!((trimmed.targets_created, trimmed.targets_freed), (7, 7));
    renderer.trim_backdrop();
    assert_eq!(renderer.backdrop_stats(), trimmed, "second trim frees nothing");
    let before = regia_render::frame_path_counters();
    target.draw_ui(&device, &mut renderer);
    let delta = regia_render::frame_path_counters().since(before);
    assert_eq!((delta.textures, delta.pipelines), (7, 0));
    assert_books(&renderer.backdrop_stats(), "reopen all factors");
    let before = regia_render::frame_path_counters();
    target.draw_ui(&device, &mut renderer);
    let delta = regia_render::frame_path_counters().since(before);
    assert_eq!((delta.textures, delta.pipelines, delta.bind_groups), (0, 0, 0));
}
