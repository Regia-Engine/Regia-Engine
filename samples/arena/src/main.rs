// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `samples/arena`.
//!
//! Flags:
//! * `--exit-after-frames N` — automated smoke runs (exit after N frames).
//! * `--seed N` — the match RNG seed (determinism; default is fixed).
//! * `--bots N` — number of AI bots (soak hook; default 7 + the local player).
//! * `--no-local-player` — pure bot-vs-bot match, no human pawn (soak hook; 16 simulated players).
//! * `--warmup N` — warm-up seconds (default 3, `ArenaConfig::warmup_secs`); `0` starts the
//!   match at once.
//! * `--brain state-tree|steering` — the bot brain without a navmesh (default `state-tree`, ADR-2134).
//! * `--mode ffa|tdm|ctf|koth` — the match mode (default `ffa`, the arena's own frag match). A team
//!   mode resolves the shipped rule set of that id through the rule-set registry
//!   (`arena::modes`, `book/tutorials/capture-the-flag.md`).
//! * `--ruleset ID` — with a team mode, resolve this rule set instead (one that extends the mode's).
//! * `--limit N` — with `ctf` or `koth`, the capture or hill score limit instead of the rule set's.
//! * `--bench` — the budget probe: 8 bots, 200 dynamic crates and ~20,000 live particles,
//!   measuring unthrottled frame timings headless and printing p50/p95/p99. With
//!   `REGIA_SPAN_REPORT=N` set it also prints the N heaviest spans and ECS systems of the
//!   measured frames (`docs/perf-log.md`).

use std::process::ExitCode;
use std::time::Instant;

use arena::{ArenaConfig, ArenaMatchMode, ArenaMode, ArenaSample, BotBrainKind, MatchRng};
use regia::prelude::*;

/// `--brain state-tree|steering` (default: the state tree, ADR-2134).
fn brain_flag(args: &[String]) -> Result<Option<BotBrainKind>, String> {
    let Some(i) = args.iter().position(|a| a == "--brain") else {
        return Ok(None);
    };
    match args.get(i + 1).map(String::as_str) {
        Some("state-tree") => Ok(Some(BotBrainKind::StateTree)),
        Some("steering") => Ok(Some(BotBrainKind::Steering)),
        other => Err(format!(
            "--brain takes `state-tree` or `steering`, got {}",
            other.unwrap_or("nothing")
        )),
    }
}

/// `--warmup N` — warm-up seconds, `0` or more (default 3, `ArenaConfig::warmup_secs`).
fn warmup_flag(args: &[String]) -> Result<Option<f32>, String> {
    let Some(i) = args.iter().position(|a| a == "--warmup") else {
        return Ok(None);
    };
    match args.get(i + 1).map(|v| v.parse::<f32>()) {
        Some(Ok(secs)) if secs.is_finite() && secs >= 0.0 => Ok(Some(secs)),
        _ => Err("--warmup needs a number of seconds (0 or more)".into()),
    }
}

/// `--mode ffa|tdm|ctf|koth`, `--ruleset ID` and `--limit N` (default: the free-for-all).
fn mode_flags(args: &[String]) -> Result<Option<ArenaMatchMode>, String> {
    let value = |name: &str| -> Result<Option<String>, String> {
        match args.iter().position(|a| a == name) {
            None => Ok(None),
            Some(i) => args
                .get(i + 1)
                .cloned()
                .map(Some)
                .ok_or_else(|| format!("{name} needs a value")),
        }
    };
    let Some(word) = value("--mode")? else {
        return Ok(None);
    };
    let mut mode = ArenaMatchMode::new(ArenaMode::parse(&word)?);
    if let Some(id) = value("--ruleset")? {
        mode.ruleset = id;
    }
    mode.limit = flag(args, "--limit")?.map(|n| u32::try_from(n).unwrap_or(u32::MAX));
    Ok(Some(mode))
}

fn flag(args: &[String], name: &str) -> Result<Option<u64>, String> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => match args.get(i + 1).map(|v| v.parse::<u64>()) {
            Some(Ok(n)) => Ok(Some(n)),
            _ => Err(format!("{name} needs a number")),
        },
    }
}

/// The budget probe (`docs/specs/14` M20 acceptance): 8 bots, 200 rigid bodies, ~20k particles.
fn run_bench() -> Result<(), String> {
    use regia_audio::AudioSettings;
    use regia_framework::settings::SettingsPath;
    use regia_render::{ExtractedFrame, FrameRenderer, RenderDevice};

    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    const FRAMES: u32 = 300;
    const WARMUP: u32 = 60;

    let mut spans = regia_core::profiling::RunTotals::install_from_env()
        .map_err(|e| format!("span report logging: {e}"))?;
    let device = RenderDevice::headless().map_err(|e| format!("GPU: {e}"))?;

    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: 8,
        local_player: false,
        ..ArenaConfig::default()
    });
    app.insert_resource(MatchRng::new(1));
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ArenaSample,
    ));
    // Headless: the window plugin is not installed, but the HUD still reads these messages.
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    app.update(); // build the world

    // 200 dynamic crates and a 24_000-particle burst (8 bots come from ArenaConfig).
    arena::budget::apply_budget_load(app.world_mut());

    let mut renderer = FrameRenderer::new();
    let render = |renderer: &mut FrameRenderer, app: &mut App| {
        let frame = app
            .world()
            .resource::<ExtractedFrame>()
            .frame_scene(0, WIDTH, HEIGHT);
        renderer.render_into_texture(&device, 0xFFFF_FFFF, &frame)
    };

    for _ in 0..WARMUP {
        app.update();
        render(&mut renderer, &mut app).map_err(|e| format!("warm-up render: {e}"))?;
        device.wait_idle().map_err(|e| format!("warm-up poll: {e}"))?;
        if let Some((totals, _)) = spans.as_mut() {
            totals.skip_frame();
        }
    }

    let mut times = Vec::with_capacity(FRAMES as usize);
    for _ in 0..FRAMES {
        let start = Instant::now();
        app.update();
        render(&mut renderer, &mut app).map_err(|e| format!("render: {e}"))?;
        device.wait_idle().map_err(|e| format!("poll: {e}"))?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        if let Some((totals, _)) = spans.as_mut() {
            totals.end_frame();
        }
    }
    if let Some((totals, rows)) = &spans {
        print!("{}", totals.report(*rows));
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = |q: f64| times[((q * FRAMES as f64) as usize).min(times.len() - 1)];
    println!(
        "arena budget probe ({FRAMES} frames, 8 bots, 200 crates): p50 {:.2} ms ({:.0} fps), p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        p(0.50),
        1000.0 / p(0.50),
        p(0.95),
        p(0.99),
        times[times.len() - 1],
    );
    println!("GPU: {}", device.capabilities.adapter_name);
    // One more step so the burst scheduled on the frames above is reflected in the count.
    let counts = arena::budget::budget_counts(app.world_mut());
    println!(
        "bots: {}, rigid bodies: {}, live particles: {}",
        counts.bots, counts.bodies, counts.particles
    );
    // Only a CPU adapter (lavapipe) gets the warning: the Deck pass of v0.2.15 saw it on RADV (ADR-2952).
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        println!("note: lavapipe timings are not representative; run on the real GPU for budgets");
    }
    if !counts.meets_budget() {
        return Err(format!(
            "budget short of 8 bots / 200 rigid bodies / 20000 particles: bots {}, bodies {}, particles {}",
            counts.bots, counts.bodies, counts.particles
        ));
    }
    Ok(())
}

/// Project directory of this sample in a dev build, resolved at runtime (ADR-10370).
///
/// The shipping binary does not insert agent settings, so this is debug-only.
/// `cargo run` exports `CARGO_MANIFEST_DIR`. The compile-time macro is not remapped.
#[cfg(debug_assertions)]
fn sample_project_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return Some(dir.into());
    }
    let root = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let project = root.join("samples").join("arena");
    project.is_dir().then_some(project)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (exit_after, seed, bots, no_local_player, bench) = match (
        flag(&args, "--exit-after-frames"),
        flag(&args, "--seed"),
        flag(&args, "--bots"),
        args.iter().any(|a| a == "--no-local-player"),
        args.iter().any(|a| a == "--bench"),
    ) {
        (Ok(a), Ok(b), Ok(c), no_local, bench) => (a, b, c, no_local, bench),
        (Err(e), _, _, _, _) | (_, Err(e), _, _, _) | (_, _, Err(e), _, _) => {
            eprintln!("arena: {e}");
            return ExitCode::from(2);
        }
    };
    #[cfg(feature = "agent")]
    let smoke = args.iter().any(|a| a == "--smoke");
    #[cfg(feature = "agent")]
    let smoke_fail = args.iter().any(|a| a == "--smoke-fail");
    let brain = match brain_flag(&args) {
        Ok(brain) => brain,
        Err(e) => {
            eprintln!("arena: {e}");
            return ExitCode::from(2);
        }
    };
    let warmup = match warmup_flag(&args) {
        Ok(warmup) => warmup,
        Err(e) => {
            eprintln!("arena: {e}");
            return ExitCode::from(2);
        }
    };
    let mode = match mode_flags(&args) {
        Ok(mode) => mode,
        Err(e) => {
            eprintln!("arena: {e}");
            return ExitCode::from(2);
        }
    };
    if bench {
        return match run_bench() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("arena bench: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let exit = regia::run_with(ArenaSample, move |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - arena".into(),
                ..Default::default()
            },
        });
        // Soak hook (dev builds only): the agent protocol advertises under the sample's
        // own directory, so `cargo xtask soak --project-dir samples/arena` finds it
        // regardless of the launch CWD. A shipping binary must not bake that source path;
        // with no project directory the agent server uses the per-user data dir.
        #[cfg(debug_assertions)]
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: sample_project_dir(),
        });
        let mut config = ArenaConfig::default();
        if let Some(n) = bots {
            config.bot_count = n as usize;
        }
        if no_local_player {
            config.local_player = false;
        }
        if let Some(brain) = brain {
            config.brain = brain;
        }
        if let Some(secs) = warmup {
            config.warmup_secs = secs;
        }
        app.insert_resource(config);
        if let Some(mode) = mode.clone() {
            app.insert_resource(mode);
        }
        if let Some(n) = seed {
            app.insert_resource(MatchRng::new(n));
        }
        if let Some(n) = exit_after {
            app.insert_resource(arena::ArenaExitAfterFrames(n));
        }
        // The smoke session (`smoke.session.ron`, spec 41 test 19): the functional-test runner and
        // one placed test, only when asked for.
        #[cfg(feature = "agent")]
        if smoke {
            arena::smoke::install(app);
        } else if smoke_fail {
            arena::smoke::install_failing(app);
        }
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
