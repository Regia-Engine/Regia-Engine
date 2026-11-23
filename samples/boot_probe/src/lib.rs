// SPDX-License-Identifier: MIT-0
//! `boot_probe`: the instant-load boot path end to end (`docs/specs/13-rendering.md` § 2c).
//!
//! A [`regia_render::BootSplash`] (logo + text) is presented from the first resume; the engine
//! warms its boot tier (the canonical lit frame) on worker threads. The probe spawns its game
//! scene — a hard-surface hull; with `game` the Desktop Ultra profile plus sky, clouds and
//! volumetric fog, a DO27-like frame configuration — and finishes the splash as soon as that
//! scene is extracted, like a game whose level is spawned. The engine's
//! [`regia_render::GameFrameWarm`] then warms the runtime renderer on that frame on a warm
//! worker while the splash holds, and the first game frame compiles nothing. It marks
//! `menu_interactive` on the first frame the scene draws. With `--exit`, it prints the boot
//! timeline, the warm-up report, the first game frame's CPU time, the slowest frame after it
//! and the pipelines created on the main thread after the warm as one JSON line and exits.
//!
//! `late_scene` reproduces DO27's order (lane v018-stalls): the splash is finished at boot with
//! no scene (DO27 calls `finish()` at sim boot), the window shows only the clear colour for a
//! while (the UI-only menu), then a map of about `map_mb` MB of distinct meshes is spawned
//! behind it. The report's `late_scene` block gives the longest main-loop interval after the
//! spawn, the intervals over 250 ms and over 5 s (gaps), and the pipelines created outside the
//! warm queue (on the frame path) since the spawn.

use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_render::{BootSplash, GameFrameWarm, PipelineWarmQueue, SplashImage};

/// The probe's plugin.
pub struct BootProbe {
    /// Exit after the splash is gone and the scene has drawn (and print the report).
    pub exit: bool,
    /// Present the splash (off: the old path, first present = first game frame).
    pub splash: bool,
    /// Give up after this long.
    pub timeout: Duration,
    /// Game frame configuration: Desktop Ultra, sky, clouds and volumetric fog.
    pub game: bool,
    /// Render profile of the game frame (default Desktop Ultra).
    pub profile: regia_render::RenderProfilePreset,
    /// Also run the first-launch quality probe (ADR-173) behind the scene and report its result
    /// and the slowest visible frame while it ran.
    pub quality_probe: bool,
    /// DO27's order: finish the splash with no scene, spawn the map this long after start.
    pub late_scene: Option<Duration>,
    /// Size of the late map's distinct mesh data, MB.
    pub map_mb: u32,
}

/// Probe state.
#[derive(Resource)]
struct ProbeState {
    exit: bool,
    started: Instant,
    timeout: Duration,
    frames_after_done: u32,
    /// Pipelines created outside the warm queue when the game-frame warm finished.
    outside_at_warm: Option<u64>,
    /// Slowest frame (CPU delta) after the splash was gone.
    max_frame_ms_after: f32,
    /// Run the quality probe.
    quality_probe: bool,
    /// Its result, once taken.
    probe_result: Option<regia_render::quality::ProbeResult>,
    /// Slowest visible frame (CPU delta) while the probe ran, and how many frames that was.
    max_frame_ms_probe: f32,
    probe_frames: u32,
    /// Late-scene mode: when to spawn the map (since start), and what happened after.
    late: Option<LateScene>,
}

/// The late map's spawn and the frame thread's ticks after it.
struct LateScene {
    at: Duration,
    map_mb: u32,
    spawned_at: Option<Instant>,
    last_tick: Option<Instant>,
    max_interval_ms: f32,
    over_250: u32,
    over_5000: u32,
    /// The warm queue's `pipelines_outside` at the spawn.
    outside_at_spawn: Option<u64>,
    /// The map's first drawn frame: ms after the spawn.
    drawn_after_ms: Option<f32>,
    ticks: u32,
}

impl Plugin for BootProbe {
    fn build(&self, app: &mut App) {
        if self.splash {
            let mut splash = BootSplash::new()
                .with_image(logo(384, 192))
                .with_text("REGIA ENGINE");
            splash.background = [0.01, 0.012, 0.016, 1.0];
            splash.text_scale = 0.035;
            app.insert_resource(splash);
        }
        app.insert_resource(ProbeState {
            exit: self.exit,
            started: Instant::now(),
            timeout: self.timeout,
            frames_after_done: 0,
            outside_at_warm: None,
            max_frame_ms_after: 0.0,
            quality_probe: self.quality_probe,
            probe_result: None,
            max_frame_ms_probe: 0.0,
            probe_frames: 0,
            late: self.late_scene.map(|at| LateScene {
                at,
                map_mb: self.map_mb,
                spawned_at: None,
                last_tick: None,
                max_interval_ms: 0.0,
                over_250: 0,
                over_5000: 0,
                outside_at_spawn: None,
                drawn_after_ms: None,
                ticks: 0,
            }),
        });
        if self.quality_probe {
            app.add_systems(Startup, |mut probe: ResMut<regia_render::QualityProbe>| {
                probe.request(regia_render::quality::ProbeConfig::default());
            });
        }
        if self.game {
            app.insert_resource(regia_render::RenderSettings::from_profile(self.profile))
                .insert_resource(regia_render::TimeOfDay::default())
                .insert_resource(regia_render::VolumetricFogSettings {
                    density: 0.002,
                    ..Default::default()
                })
                .insert_resource(regia_render::CloudSettings {
                    coverage: 0.4,
                    ..Default::default()
                });
        }
        if self.late_scene.is_none() {
            app.add_systems(Startup, spawn_menu_backdrop);
        }
        app.add_systems(Update, (spawn_late_map, finish_splash, report_and_exit).chain());
    }
}

/// A procedural logo: a ring with a soft glow on a transparent background (sRGB RGBA8).
#[must_use]
pub fn logo(width: u32, height: u32) -> SplashImage {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let radius = height as f32 * 0.36;
    for y in 0..height {
        for x in 0..width {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let ring = (1.0 - ((d - radius).abs() / (height as f32 * 0.05))).clamp(0.0, 1.0);
            let glow = (1.0 - ((d - radius).abs() / (height as f32 * 0.2))).clamp(0.0, 1.0) * 0.35;
            let a = ring.max(glow);
            rgba.extend_from_slice(&[
                (230.0 * a.max(ring)) as u8,
                (120.0 * a) as u8,
                (40.0 * a) as u8,
                (255.0 * a) as u8,
            ]);
        }
    }
    SplashImage {
        width,
        height,
        rgba: rgba.into(),
    }
}

fn spawn_menu_backdrop(world: &mut World) {
    regia_render::spawn_lab_scene(world, &regia_render::hard_surface::hull_closeup_scene());
}

/// The late map: the hull close-up plus `map_mb` MB of distinct ground tiles (128×128 quads,
/// about 2 MB each) under it, like a DO27 map's meshes becoming resident at once.
#[must_use]
pub fn late_map(map_mb: u32) -> regia_render::LabScene {
    let mut scene = regia_render::hard_surface::hull_closeup_scene();
    const N: u32 = 128;
    let tile_bytes = ((N + 1) * (N + 1) * 96 + N * N * 6 * 4) as u64;
    let tiles = ((u64::from(map_mb) << 20) / tile_bytes).max(1) as u32;
    let side = (f64::from(tiles).sqrt().ceil() as u32).max(1);
    let size = 8.0f32;
    let materials = scene.materials.len().max(1);
    for t in 0..tiles {
        let (tx, tz) = ((t % side) as f32, (t / side) as f32);
        let origin = [
            (tx - side as f32 / 2.0) * size,
            -2.0 - (t % 5) as f32 * 0.001,
            (tz - side as f32 / 2.0) * size,
        ];
        let mut vertices = Vec::with_capacity(((N + 1) * (N + 1)) as usize);
        for z in 0..=N {
            for x in 0..=N {
                let (u, v) = (x as f32 / N as f32, z as f32 / N as f32);
                vertices.push(regia_render::LabVertex {
                    position: [origin[0] + u * size, origin[1], origin[2] + v * size],
                    normal: [0.0, 1.0, 0.0],
                    uv: [u, v],
                    tangent: [1.0, 0.0, 0.0, 1.0],
                    color: [1.0; 4],
                    joints: [0; 4],
                    weights: [0.0; 4],
                });
            }
        }
        let mut indices = Vec::with_capacity((N * N * 6) as usize);
        for z in 0..N {
            for x in 0..N {
                let i = z * (N + 1) + x;
                indices.extend([i, i + N + 1, i + 1, i + 1, i + N + 1, i + N + 2]);
            }
        }
        scene.meshes.push(regia_render::LabMesh {
            vertices,
            indices,
            material: t as usize % materials,
            model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
            joints: Vec::new(),
        });
    }
    scene
}

/// Late-scene mode: spawns the map once its time has come, and measures every main-loop tick
/// after that (wall interval, frame-thread creates and uploads).
fn spawn_late_map(world: &mut World) {
    let now = Instant::now();
    let outside = world
        .get_resource::<PipelineWarmQueue>()
        .map(|q| q.progress().pipelines_outside);
    let drawn = world
        .get_resource::<GameFrameWarm>()
        .is_some_and(|w| w.first_frame_ms().is_some());
    let Some(mut state) = world.get_resource_mut::<ProbeState>() else {
        return;
    };
    let started = state.started;
    let Some(late) = state.late.as_mut() else {
        return;
    };
    if let Some(spawned) = late.spawned_at {
        let interval = late
            .last_tick
            .map_or(0.0, |t| now.duration_since(t).as_secs_f32() * 1000.0);
        late.ticks += 1;
        late.over_250 += u32::from(interval > 250.0);
        late.over_5000 += u32::from(interval > 5000.0);
        late.max_interval_ms = late.max_interval_ms.max(interval);
        if drawn && late.drawn_after_ms.is_none() {
            late.drawn_after_ms = Some(now.duration_since(spawned).as_secs_f32() * 1000.0);
        }
        late.last_tick = Some(now);
        return;
    }
    if started.elapsed() < late.at {
        return;
    }
    let map_mb = late.map_mb;
    late.spawned_at = Some(now);
    late.outside_at_spawn = outside;
    let scene = late_map(map_mb);
    regia_render::spawn_lab_scene(world, &scene);
    regia_render::boot::mark_named("late_map_spawned");
    let now = Instant::now();
    if let Some(mut state) = world.get_resource_mut::<ProbeState>()
        && let Some(late) = state.late.as_mut()
    {
        late.last_tick = Some(now);
    }
}

/// Finishes the splash once the scene is extracted (the "level is spawned" moment); the
/// engine's `GameFrameWarm` holds the fade until the runtime renderer is warm. Marks
/// `menu_interactive` once the scene's first frame is drawn.
fn finish_splash(
    frame: Option<Res<regia_render::ExtractedFrame>>,
    splash: Option<ResMut<BootSplash>>,
    game_warm: Option<Res<GameFrameWarm>>,
    state: Res<ProbeState>,
) {
    let state_late = state.late.is_some();
    let Some(mut splash) = splash else {
        // No splash: the scene is interactive on its first frame.
        regia_render::boot::mark_named("menu_interactive");
        return;
    };
    // DO27's order (late scene): `finish()` at boot, before any scene exists.
    if frame.is_some_and(|f| f.is_renderable()) || state_late {
        splash.finish();
    }
    // Interactive once the scene's first frame is drawn (after the engine's game-frame warm).
    if splash.is_finishing() && game_warm.is_some_and(|w| w.first_frame_ms().is_some()) {
        regia_render::boot::mark_named("menu_interactive");
    }
}

fn report_and_exit(
    mut state: ResMut<ProbeState>,
    splash: Option<Res<BootSplash>>,
    queue: Option<Res<PipelineWarmQueue>>,
    game_warm: Option<Res<GameFrameWarm>>,
    time: Option<Res<Time>>,
    mut probe: Option<ResMut<regia_render::QualityProbe>>,
    mut exit: MessageWriter<AppExit>,
) {
    if state.quality_probe
        && let Some(p) = probe.as_deref_mut()
    {
        if p.is_running()
            && let Some(time) = &time
        {
            state.probe_frames += 1;
            if state.probe_frames > 1 {
                state.max_frame_ms_probe = state.max_frame_ms_probe.max(time.delta_secs() * 1000.0);
            }
        }
        if let Some(result) = p.take_result() {
            state.probe_result = Some(result);
        }
    }
    if state.outside_at_warm.is_none() && game_warm.as_ref().is_some_and(|w| w.last_report().is_some()) {
        state.outside_at_warm = queue.as_ref().map(|q| q.progress().pipelines_outside);
    }
    // Done once the splash is gone and the scene has drawn: since ADR-173 the splash can be
    // gone (its 1.5 s cap) while the game-frame warm still runs behind it.
    let done = splash.as_ref().is_none_or(|s| s.is_done())
        && game_warm.as_ref().is_none_or(|w| w.first_frame_ms().is_some());
    if done {
        state.frames_after_done += 1;
        if state.frames_after_done > 1
            && let Some(time) = &time
        {
            state.max_frame_ms_after = state.max_frame_ms_after.max(time.delta_secs() * 1000.0);
        }
    }
    if !state.exit {
        return;
    }
    let timed_out = state.started.elapsed() > state.timeout;
    let probe_pending = state.quality_probe && state.probe_result.is_none();
    if (state.frames_after_done < 30 || probe_pending) && !timed_out {
        return;
    }
    let warm = queue.map(|q| q.progress());
    let tiers: Vec<serde_json::Value> = warm
        .as_ref()
        .map(|w| {
            w.tiers
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "tier": t.tier, "jobs": t.jobs_total, "failed": t.jobs_failed,
                        "pipelines": t.pipelines, "busy_ms": t.busy_ms, "ready_after_ms": t.ready_after_ms,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let cache = regia_render::pipeline_cache::status();
    let outside_now = warm.as_ref().map_or(0, |w| w.pipelines_outside);
    let game = game_warm.as_ref().and_then(|w| w.last_report().cloned());
    let report = serde_json::json!({
        "timed_out": timed_out,
        "warm_boot_threads": regia_render::boot_threads(),
        "splash_capped": splash.as_ref().is_some_and(|s| s.was_capped()),
        "marks": regia_render::boot::timeline()
            .into_iter()
            .map(|(m, ms)| serde_json::json!({ "mark": m, "ms": (ms * 10.0).round() / 10.0 }))
            .collect::<Vec<_>>(),
        "warm": tiers,
        "pipelines_outside_queue": outside_now,
        "pipelines_outside_after_warm": state.outside_at_warm.map(|at| outside_now - at),
        "game_warm": game.map(|g| serde_json::json!({ "ms": g.ms, "pipelines": g.pipelines, "error": g.error })),
        "first_game_frame_ms": game_warm.as_ref().and_then(|w| w.first_frame_ms()),
        "max_frame_ms_after_splash": state.max_frame_ms_after,
        "late_scene": state.late.as_ref().map(|l| serde_json::json!({
            "map_mb": l.map_mb,
            "spawned_at_ms": l.spawned_at.map(|t| t.duration_since(state.started).as_secs_f32() * 1000.0),
            "drawn_after_ms": l.drawn_after_ms,
            "ticks": l.ticks,
            "max_interval_ms": l.max_interval_ms,
            "intervals_over_250ms": l.over_250,
            "gaps_over_5s": l.over_5000,
            "scene_warms": game_warm.as_ref().map(|w| w.scene_warms()),
            "pipelines_outside_since_spawn": l.outside_at_spawn.map(|at| outside_now.saturating_sub(at)),
        })),
        "quality_probe": state.probe_result.as_ref().map(|r| serde_json::json!({
            "frames": r.frames, "p50_ms": r.p50_ms, "p95_ms": r.p95_ms, "source": format!("{:?}", r.source),
            "gpu_busy": r.gpu_busy, "size": [r.width, r.height], "timed_out": r.timed_out,
            "max_visible_frame_ms": state.max_frame_ms_probe, "visible_frames": state.probe_frames,
        })),
        "slowest_sites": regia_render::pipeline_warm::warm_site_costs(24)
            .into_iter()
            .map(|c| format!("{} x{} {:.0} ms (max {:.0})", c.site, c.pipelines, c.total_ms, c.max_ms))
            .collect::<Vec<_>>(),
        "pipelines_created": regia_render::pipeline_cache::pipelines_created(),
        "outside_sites": warm.as_ref().map(|w| w.outside_sites.iter().map(|(s, n)| format!("{s} x{n}")).collect::<Vec<_>>()),
        "cache": {
            "enabled": cache.enabled,
            "load": cache.load.map(|l| format!("{l:?}")),
            "load_ms": cache.load_ms,
            "saved_bytes": cache.saved_bytes,
        },
    });
    // The probe's one line of output, for scripts.
    println!("BOOT_PROBE {report}");
    // Save now: a timeout may have left the queue running.
    let _ = regia_render::pipeline_cache::save_installed();
    exit.write(if timed_out {
        AppExit::Error(std::num::NonZeroU8::MIN)
    } else {
        AppExit::Success
    });
}
