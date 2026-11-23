// SPDX-License-Identifier: MPL-2.0
//! A stand-in game for the tests of play-in-editor's separate processes (ADR-2682).
//!
//! It honors the contract of a game that `regia server --clients N` starts
//! (`regia_asset_pipeline::dev_server`):
//!
//! - **server** (no `REGIA_CONNECT`): the engine's server host (`regia_net::server_host`) over a
//!   session-only game, configured by `REGIA_SERVER_DIR`'s `server.toml` and the command line; it
//!   prints the host's console lines (`listening …`, `join code …`) and exits with the host's code.
//! - **client** (`REGIA_CONNECT` set): a UDP session that joins that address as `REGIA_CLIENT_NAME`
//!   and prints each state change. When `REGIA_PIE_FIXTURE_CRASH` names this client it exits with
//!   code 70 (the crash code of spec 76 § 5) once in game.
//!
//! Both print the play-in-editor environment they received, so a test can see it arrived.
//!
//! With `REGIA_PIE_FIXTURE_PROFILE` set, each process also runs the agent protocol (in the
//! directory `REGIA_AGENT_DIR` names, as a game with the `agent` feature does) with a frame
//! profiler, and every loop records the same two spans, `pie_fixture_tick` holding
//! `pie_fixture_work`: the Profiler panel's process source reads them (ADR-9740).
//!
//! With `REGIA_PIE_FIXTURE_VIEW` set, each process runs the agent protocol with a primary window
//! entity (no OS window) and fills `regia_agent::play_view::FrameFeed` every frame with a 160 x 90
//! picture: its left half is lit by the keys this process read, and a bar moves one pixel per frame
//! of game time. The editor's Game tabs stream it (ADR-7831).

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use regia_asset_pipeline::dev_server;
// The `Resource` derive names `bevy_ecs`; the facade re-exports it.
use regia_ecs::prelude::bevy_ecs;
use regia_net::server_host::{
    CliOverrides, EnvReader, HostTransport, MonotonicClock, ServerHost, SessionOnlyGame,
};
use regia_net::session::{ConnectOptions, ConnectTarget, NetSession};

/// Names the client (`client1`) that exits with code 70 once in game.
const CRASH_ENV: &str = "REGIA_PIE_FIXTURE_CRASH";

/// Turns on the agent protocol and the profiled spans.
const PROFILE_ENV: &str = "REGIA_PIE_FIXTURE_PROFILE";

/// Turns on the agent protocol, a primary window entity and the frame feed (ADR-7831).
const VIEW_ENV: &str = "REGIA_PIE_FIXTURE_VIEW";

/// The fixture's frame feed size.
const FEED_SIZE: (u32, u32) = (160, 90);

/// What the fixture's game counted: keys read and frames that ran game time.
#[derive(regia_ecs::prelude::Resource, Default)]
struct ViewCounters {
    keys: u64,
    game_frames: u64,
}

/// Counts the keys and game frames, and paints the frame feed from them.
fn paint_feed(
    mut keys: regia_app::prelude::MessageReader<regia_platform::KeyboardInput>,
    time: regia_app::prelude::Res<regia_app::prelude::Time>,
    mut counters: regia_app::prelude::ResMut<ViewCounters>,
    mut feed: regia_app::prelude::ResMut<regia_agent::play_view::FrameFeed>,
) {
    counters.keys += keys.read().count() as u64;
    if !time.delta().is_zero() {
        counters.game_frames += 1;
    }
    let (w, h) = FEED_SIZE;
    let lit = u8::try_from((counters.keys * 40).min(255)).unwrap_or(255);
    let bar = (counters.game_frames % u64::from(w)) as u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let px = if x == bar {
                [255, 255, 255, 255]
            } else if x < w / 2 {
                [lit, 32, 32, 255]
            } else {
                [16, 16, 48, 255]
            };
            rgba.extend(px);
        }
    }
    *feed = regia_agent::play_view::FrameFeed {
        width: w,
        height: h,
        rgba,
    };
}

/// The agent side of a profiled fixture: an app with the agent plugin and the frame profiler.
struct AgentSide {
    app: regia_app::App,
    profiler: regia_core::profiling::FrameProfiler,
    _log: Option<regia_core::log::LogGuard>,
    /// Time the profiled spans (`REGIA_PIE_FIXTURE_PROFILE`).
    spans: bool,
}

impl AgentSide {
    /// The agent side when [`PROFILE_ENV`] is set.
    fn from_env() -> Option<Self> {
        let spans = var(PROFILE_ENV).is_some();
        let view = var(VIEW_ENV).is_some();
        if !spans && !view {
            return None;
        }
        let profiler = regia_core::profiling::FrameProfiler::new();
        // Only the fixture's own spans are timed: the agent's and the ECS's are below `warn`.
        let log = regia_core::log::init(regia_core::log::LogConfig {
            filter: "warn,pie_fixture=info".into(),
            file_dir: None,
            max_files: 1,
            panic_hook: false,
            profiler: Some(profiler.clone()),
        })
        .ok();
        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins)
            .insert_resource(regia_agent::AgentSettings {
                project_dir: std::env::current_dir().ok(),
            })
            .insert_resource(regia::telemetry::SpanProfiler(profiler.clone()))
            .add_plugins(regia_agent::AgentPlugin);
        if view {
            app.add_message::<regia_platform::KeyboardInput>()
                .add_message::<regia_platform::MouseButtonInput>()
                .add_message::<regia_platform::CursorMoved>()
                .add_message::<regia_platform::MouseWheel>()
                .add_message::<regia_platform::TouchInput>()
                .add_message::<regia_platform::BackPressed>()
                .init_resource::<ViewCounters>()
                .init_resource::<regia_agent::play_view::FrameFeed>()
                .add_systems(regia_app::prelude::Update, paint_feed);
            app.world_mut()
                .spawn((regia_platform::Window::default(), regia_platform::PrimaryWindow));
        }
        app.finish_plugins();
        Some(Self {
            app,
            profiler,
            _log: log,
            spans,
        })
    }

    /// One frame: the two fixture spans, the agent's requests, the profiler's frame end.
    fn frame(&mut self) {
        if self.spans {
            let _tick = tracing::info_span!(target: "pie_fixture", "pie_fixture_tick").entered();
            spin(std::time::Duration::from_micros(300));
            let _work = tracing::info_span!(target: "pie_fixture", "pie_fixture_work").entered();
            spin(std::time::Duration::from_micros(900));
        }
        self.app.update();
        self.profiler.end_frame();
    }
}

/// Busy work for `d` (a stand-in for a game's frame work).
fn spin(d: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < d {
        std::hint::spin_loop();
    }
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn pie_env_line(role: &str) -> String {
    format!(
        "{role} pie mode {} level {} conditioner {} seed {} title {}",
        var(dev_server::PIE_MODE_ENV).unwrap_or_default(),
        var(dev_server::PIE_LEVEL_ENV).unwrap_or_default(),
        var(dev_server::CONDITIONER_ENV).unwrap_or_default(),
        var(dev_server::CONDITIONER_SEED_ENV).unwrap_or_default(),
        var(dev_server::TITLE_ENV).unwrap_or_else(|| "-".to_owned()),
    )
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn server() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match CliOverrides::parse(&args) {
        Ok(cli) => cli,
        Err(errors) => {
            for e in errors {
                eprintln!("{e}");
            }
            return exit(2);
        }
    };
    let dir = var(dev_server::SERVER_DIR_ENV).map_or_else(|| PathBuf::from("."), PathBuf::from);
    let env: EnvReader = Box::new(|key| std::env::var(key).ok());
    let want_agent = cli.agent;
    let mut host = match ServerHost::new(&dir, cli, env, Box::new(MonotonicClock::default())) {
        Ok(host) => host,
        Err(err) => {
            for line in &err.lines {
                eprintln!("{line}");
            }
            return exit(err.exit.code());
        }
    };
    say(&pie_env_line("server"));
    let mut agent = if want_agent { AgentSide::from_env() } else { None };
    let mut game = SessionOnlyGame::new(NetSession::default());
    if let Err(err) = host.start(&mut game, HostTransport::Udp) {
        for line in &err.lines {
            eprintln!("{line}");
        }
        return exit(err.exit.code());
    }
    loop {
        if let Some(agent) = agent.as_mut() {
            agent.frame();
        }
        let stopped = host.run_once(&mut game);
        for line in host.take_console_output() {
            say(&line);
        }
        if let Some(code) = stopped {
            return exit(code.code());
        }
    }
}

fn client(addr: &str) -> ExitCode {
    let name = var(dev_server::NAME_ENV).unwrap_or_else(|| "client".to_owned());
    say(&pie_env_line(&name));
    let addr: SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{name}: {}: {e}", dev_server::CONNECT_ENV);
            return exit(2);
        }
    };
    let crash = var(CRASH_ENV).as_deref() == Some(name.as_str());
    let mut session = NetSession::default();
    let options = ConnectOptions {
        player_names: vec![name.clone()],
        ..ConnectOptions::default()
    };
    if let Err(e) = session.connect(ConnectTarget::Address(addr), options) {
        eprintln!("{name}: connect {addr}: {e}");
        return exit(3);
    }
    let mut agent = AgentSide::from_env();
    let mut last = String::new();
    loop {
        if let Some(agent) = agent.as_mut() {
            agent.frame();
        }
        session.update();
        let state = session.state().as_str().to_owned();
        if state != last {
            say(&format!("{name} state {state}"));
            if state == "in_game" && crash {
                say(&format!("{name} crashing on purpose"));
                return exit(70);
            }
            last = state;
        }
        // A real client paces at its frame rate; the parent kills it to stop it.
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

fn main() -> ExitCode {
    match var(dev_server::CONNECT_ENV) {
        Some(addr) => client(&addr),
        None => server(),
    }
}
