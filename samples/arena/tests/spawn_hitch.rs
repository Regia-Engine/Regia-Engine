// SPDX-License-Identifier: MIT-0
//! Spawns do not hitch (`specs/27` § 2.4), measured through the agent protocol.
//!
//! A headless arena renders every frame through the runtime renderer (a texture-target camera:
//! culling, shadows, forward, VFX and decals) with the residency keeper on, as
//! `DefaultPlugins` runs it. A client thread asks `play.hitch_report { "drive": "spawns" }`; the
//! drive spawns a crate (mesh, material, rigid body, collider), an effect, a decal and a
//! positional sound every 0.25 s and despawns the oldest crates. After the 3 s warm-up no spawn
//! may create a pipeline, bind group, buffer or texture, and on a hardware GPU no frame that
//! spawned may take longer than 50 ms (frames long without a spawn are counted, not gated: on a
//! shared box they are the machine's).
//!
//! `SPAWN_SOAK_SECONDS` (default 20) sets the measured time; `SPAWN_SOAK_SQUEEZE=MB[:MS][:file]`
//! reclaims MB of the process every MS during the run (a box short of memory, see
//! `regia_core::residency::MemorySqueeze`); `REGIA_KEEP_RESIDENT=0` turns the keeper off for
//! the A/B. `SPAWN_SOAK_JSON=<path>` writes the report. `SPAWN_SOAK_SPANS=1` times every span
//! (enabling them costs frame time: diagnosis only) and prints the slowest.
//!
//! `vfarm.sh --gpu auto -- cargo test --release -p arena --test spawn_hitch -- --nocapture`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use regia_render::{Camera, RenderDevice, RenderTarget};
use serde_json::{Value, json};

use arena::spawn_drive::{DRIVE_NAME, SpawnDrive};
use arena::{ArenaConfig, ArenaSample, MatchRng};

const WARMUP_S: f64 = 3.0;

/// Per span name: entries and time inside it (ms) since the last [`SpanTimes::take`]. The test
/// takes it after every frame, so a long spawn frame shows where it went.
#[derive(Default)]
struct SpanTimes(Mutex<FrameSpans>);

impl SpanTimes {
    fn take(&self) -> FrameSpans {
        self.0
            .lock()
            .map(|mut t| std::mem::take(&mut *t))
            .unwrap_or_default()
    }
}

/// One frame's spans: entries and ms per span name.
type FrameSpans = HashMap<&'static str, (u32, f64)>;

/// A span's mean (entries, ms) per spawn frame and per other frame.
type SpanRow<'a> = (&'a str, (f64, f64), (f64, f64));

/// Span time per name summed over a class of frames, and the frame count.
#[derive(Default)]
struct SpanSums {
    frames: u64,
    ms: HashMap<&'static str, (f64, f64)>,
}

impl SpanSums {
    fn add(&mut self, frame: &FrameSpans) {
        self.frames += 1;
        for (name, (calls, ms)) in frame {
            let entry = self.ms.entry(name).or_default();
            entry.0 += f64::from(*calls);
            entry.1 += ms;
        }
    }

    /// Mean entries and ms per frame.
    fn mean(&self, name: &str) -> (f64, f64) {
        let (calls, ms) = self.ms.get(name).copied().unwrap_or_default();
        let frames = self.frames.max(1) as f64;
        (calls / frames, ms / frames)
    }
}

/// The spans of one frame, longest first.
fn top_spans(frame: &FrameSpans, n: usize) -> String {
    let mut rows: Vec<(&str, (u32, f64))> = frame.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    rows.iter()
        .take(n)
        .map(|(name, (calls, ms))| match calls {
            1 => format!("{name} {ms:.1}"),
            _ => format!("{name} {ms:.1} ({calls}x)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `tracing` layer feeding [`SpanTimes`].
struct Timing(Arc<SpanTimes>);

impl<S> tracing_subscriber::Layer<S> for Timing
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(Instant::now());
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let Some(ms) = span
            .extensions()
            .get::<Instant>()
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
        else {
            return;
        };
        if let Ok(mut times) = self.0.0.lock() {
            let entry = times.entry(span.name()).or_default();
            entry.0 += 1;
            entry.1 += ms;
        }
    }
}

fn seconds() -> f64 {
    std::env::var("SPAWN_SOAK_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0)
}

/// The agent's drive request switches the arena's spawn drive on for the run.
fn follow_hitch_drive(drive: Option<Res<regia_agent::HitchDrive>>, mut spawns: ResMut<SpawnDrive>) {
    spawns.active = drive.is_some_and(|d| d.name == DRIVE_NAME);
}

fn app(project: PathBuf, device: RenderDevice) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: 4,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 3600.0,
        use_navmesh: false,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(0x5A11));
    app.insert_resource(device);
    app.add_plugins((
        MinimalPlugins,
        regia_app::ResidencyPlugin::default(),
        regia_render::RenderPlugin { headless: true },
        ArenaSample,
        regia_vfx::VfxRuntimePlugin,
    ))
    .add_plugins(regia_agent::AgentPlugin)
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project),
    });
    app.add_systems(
        Update,
        follow_hitch_drive.before(arena::spawn_drive::drive_spawns),
    );
    // The HUD and input systems read these platform messages even headless (no window plugin).
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // Wall-clock time (`TimeUpdateStrategy::Automatic`, the default): `play.hitch_report`
    // measures real frame intervals. `enable_fixed_step` would advance a synthetic 16.7 ms a
    // frame and hide every hitch.
    app
}

fn read_token(project: &std::path::Path) -> String {
    std::fs::read_to_string(endpoint::agent_dir(project).join("token")).expect("token file")
}

#[test]
fn spawns_do_not_hitch_through_the_runtime_frame() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let hardware = device.adapter.get_info().device_type != wgpu::DeviceType::Cpu;
    let span_times = Arc::new(SpanTimes::default());
    if std::env::var_os("SPAWN_SOAK_SPANS").is_some_and(|v| v != "0") {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let _ = tracing_subscriber::registry()
            .with(Timing(Arc::clone(&span_times)))
            .try_init();
    }
    let seconds = seconds();
    let project = std::env::temp_dir().join(format!("regia-spawn-hitch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("project dir");

    let mut app = app(project.clone(), device);
    // The agent server starts in `Plugin::finish`.
    app.finish_plugins();
    app.update(); // Startup: the world, the bots and the camera
    {
        let world = app.world_mut();
        let mut cameras = world.query::<&mut Camera>();
        for mut camera in cameras.iter_mut(world) {
            camera.target = RenderTarget::Texture { asset_id: 0x5A11 };
        }
    }

    let reply: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let (reply_t, done_t) = (Arc::clone(&reply), Arc::clone(&done));
    let agent_dir = endpoint::agent_dir(&project);
    let script_project = project.clone();
    let script = std::thread::Builder::new()
        .name("spawn-hitch-agent-client".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..3000 {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&script_project))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                done_t.store(true, Ordering::SeqCst);
                panic!("no endpoint at {}", agent_dir.display());
            };
            let _ = client.call("agent.hello", json!({ "client": "spawn-hitch", "model": "worker" }));
            let timeout_ms = ((WARMUP_S + seconds) * 1000.0) as u64 + 120_000;
            let result = client.call(
                "play.hitch_report",
                json!({ "seconds": seconds, "warmup_s": WARMUP_S, "drive": DRIVE_NAME, "timeout_ms": timeout_ms }),
            );
            if let Ok(mut slot) = reply_t.lock() {
                *slot = Some(result.unwrap_or_else(|err| json!({ "error": format!("{err:?}") })));
            }
            done_t.store(true, Ordering::SeqCst);
        })
        .expect("client thread");

    let squeeze_settings = std::env::var("SPAWN_SOAK_SQUEEZE")
        .ok()
        .and_then(|v| regia_core::residency::SqueezeSettings::parse(&v));
    let mut squeeze = None;
    // Each measured frame's wall time and whether it spawned: the report says how many frames
    // were long, this says whether a spawn made them long.
    let mut frames: Vec<(f32, bool)> = Vec::new();
    let mut drive_started: Option<Instant> = None;
    let (mut spawn_spans, mut other_spans) = (SpanSums::default(), SpanSums::default());
    let mut slow_spawns: Vec<(f32, f32, FrameSpans)> = Vec::new();
    let mut previous_ms = 0.0_f32;
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(WARMUP_S + seconds + 300.0);
    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
        if squeeze.is_none()
            && let Some(settings) = squeeze_settings
            && app.world().contains_resource::<regia_agent::HitchDrive>()
        {
            squeeze = regia_core::residency::MemorySqueeze::start(settings);
            assert!(
                squeeze.is_some(),
                "SPAWN_SOAK_SQUEEZE needs a writable memory.reclaim"
            );
        }
        let before = app.world().resource::<SpawnDrive>().spawned();
        let frame = Instant::now();
        app.update();
        let ms = frame.elapsed().as_secs_f32() * 1000.0;
        let spawned_now = app.world().resource::<SpawnDrive>().spawned() > before;
        let spans = span_times.take();
        if app.world().contains_resource::<regia_agent::HitchDrive>() {
            let since = drive_started.get_or_insert_with(Instant::now);
            if since.elapsed().as_secs_f64() >= WARMUP_S {
                frames.push((ms, spawned_now));
                if spawned_now {
                    spawn_spans.add(&spans);
                    slow_spawns.push((ms, previous_ms, spans));
                    slow_spawns.sort_by(|a, b| b.0.total_cmp(&a.0));
                    slow_spawns.truncate(5);
                } else {
                    other_spans.add(&spans);
                }
                previous_ms = ms;
            }
        }
    }
    drop(squeeze);
    script.join().expect("client thread");
    let _ = std::fs::remove_dir_all(&project);

    let report = reply
        .lock()
        .ok()
        .and_then(|r| r.clone())
        .expect("a hitch report reply");
    let spawned = app.world().resource::<SpawnDrive>().spawned();
    println!(
        "spawn hitch soak: {} s on {} ({}), {spawned} spawn events, squeeze {:?}",
        seconds,
        app.world().resource::<RenderDevice>().adapter.get_info().name,
        if hardware { "hardware" } else { "CPU rasterizer" },
        squeeze_settings
    );
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    let spawn_frames: Vec<f32> = frames.iter().filter(|f| f.1).map(|f| f.0).collect();
    let long_spawn: Vec<f32> = spawn_frames.iter().copied().filter(|ms| *ms > 50.0).collect();
    let long_other = frames.iter().filter(|f| !f.1 && f.0 > 50.0).count();
    let worst_spawn = spawn_frames.iter().copied().fold(0.0_f32, f32::max);
    let mut sorted = spawn_frames.clone();
    sorted.sort_by(f32::total_cmp);
    let p99_spawn = sorted.get(sorted.len() * 99 / 100).copied().unwrap_or(0.0);
    println!(
        "measured frames {}: spawn frames {} (p99 {p99_spawn:.2} ms, worst {worst_spawn:.2} ms, over 50 ms {}); \
         other frames over 50 ms {long_other}",
        frames.len(),
        spawn_frames.len(),
        long_spawn.len()
    );
    if let Ok(path) = std::env::var("SPAWN_SOAK_JSON") {
        let _ = std::fs::write(path, serde_json::to_string_pretty(&report).unwrap_or_default());
    }
    assert!(report.get("error").is_none(), "{report}");
    let expected = ((WARMUP_S + seconds) / 0.25 * 0.5) as u64;
    assert!(spawned >= expected, "{spawned} spawn events (want >= {expected})");
    let calls = &report["device_calls"];
    for key in [
        "pipelines",
        "bind_groups",
        "buffers",
        "textures",
        "blocking_polls",
    ] {
        assert_eq!(calls[key], 0, "{key} created after warm-up: {calls}");
    }
    assert_eq!(report["frames_with_creates"], 0, "{report}");
    if !spawn_spans.ms.is_empty() {
        // What a spawn frame spends more on than a frame without a spawn.
        let mut rows: Vec<SpanRow> = spawn_spans
            .ms
            .keys()
            .map(|name| (*name, spawn_spans.mean(name), other_spans.mean(name)))
            .collect();
        rows.sort_by(|a, b| (b.1.1 - b.2.1).total_cmp(&(a.1.1 - a.2.1)));
        println!("span ms (entries) per frame, spawn frames vs other frames (largest difference first):");
        for (name, spawn, other) in rows.iter().take(20) {
            println!(
                "  {name}: {:.3} ({:.2}) vs {:.3} ({:.2})",
                spawn.1, spawn.0, other.1, other.0
            );
        }
        println!("slowest spawn frames (the frame before in brackets):");
        for (ms, previous, spans) in &slow_spawns {
            println!("  {ms:.1} ms [{previous:.1}]: {}", top_spans(spans, 10));
        }
    }
    if hardware {
        // Frames a spawn made long. Frames that were long without a spawn are the machine's
        // (a shared farm) and stay in the report's `over_50ms`.
        assert!(long_spawn.is_empty(), "spawn frames over 50 ms: {long_spawn:?}");
    }
}
