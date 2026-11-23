// SPDX-License-Identifier: MPL-2.0
//! The dedicated-server build (`docs/specs/76-dedicated-server-package.md` § 2, ADR-1886).
//!
//! With the `server` feature [`run`](crate::run) hosts the game under
//! [`ServerHost`]: no window, no renderer, no audio device. The
//! host owns the loop. Each tick is one `App::update`, and the fixed step is pinned to
//! `server.tick_hz`. An overrun runs further updates, up to `FixedTime::max_substeps` (default 5).
//!
//! The server directory (`server.toml`, `bans.ron`, `content/`, `logs/`) is the directory of the
//! executable; `regia server` points [`SERVER_DIR_ENV`] at a dev directory instead.
//!
//! Gameplay code that names a render or audio type goes behind `#[cfg(not(feature = "server"))]`.
//! A server build that needs one fails to compile; the engine provides no stand-ins.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_net::model::NetMode;
use regia_net::server_host::{
    AppHostGame, CliOverrides, EnvReader, ExitCode, HostError, HostTransport, LevelHook, MonotonicClock,
    ServerHost,
};
use regia_net::{NetPlugin, NetSessionPlugin};
use regia_platform::{DirMount, MountPoint, Vfs, VfsPath};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::streaming::{StreamedLevelOpener, StreamedOpen};

use crate::{Profiler, profiler, start_logging};

/// Environment variable naming the server directory; default: the executable's directory.
pub const SERVER_DIR_ENV: &str = "REGIA_SERVER_DIR";

/// What every server has before the game's plugins: `MinimalPlugins`, the [`Vfs`], crash reports,
/// the profiler and the asset server. No window, no renderer, no audio.
#[derive(Default)]
pub struct ServerPlugins;

impl Plugin for ServerPlugins {
    fn build(&self, app: &mut App) {
        regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
        if !app.world().contains_resource::<Profiler>() {
            app.insert_resource(Profiler(profiler()));
        }
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_app::ResidencyPlugin::default());
        app.add_plugins(regia_platform::PlatformPlugin);
        app.add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings::default(),
        });
        // Spec 10 § 1.3: the frame profiler keeps the last 300 frames. A client closes them in
        // `Last`; a server has no client loop, so each `App::update` closes one frame here.
        app.add_systems(regia_app::Last, end_server_profiler_frame);
    }
}

/// Closes the process-wide frame profiler once per server `App::update`.
fn end_server_profiler_frame(profiler: Option<Res<Profiler>>) {
    if let Some(profiler) = profiler {
        profiler.0.end_frame();
    }
}

/// The simulation plugins (spec 76 § 2): scene, physics, animation pose evaluation, framework, nav,
/// AI, world streaming with its nav hook (`regia_nav::NavStreamPlugin`), net, server world persistence. Added after the game, so a plugin the game already added with its own settings wins
/// (`App::add_plugins` skips a duplicate).
pub struct SimulationPlugins;

impl Plugin for SimulationPlugins {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            regia_scene::ScenePlugin,
            regia_physics::PhysicsPlugin,
            regia_animation::AnimationPlugin,
        ));
        app.add_plugins((
            regia_framework::script::ScriptPlugin,
            regia_framework::timers::TimerPlugin,
            regia_framework::character::CharacterPlugin,
            regia_framework::damage::DamagePlugin,
        ));
        app.add_plugins((regia_nav::NavPlugin, regia_ai::AiPlugin));
        // World streaming (spec 78 § 14): idle until a level cooked for it opens. The nav hook loads
        // a streamed cell's cooked nav tiles and answers its `nav` gate, so server AI walks them and
        // `sim_ready` (final on a server) waits for them; inert for a level without nav tiles.
        app.add_plugins((regia_world::WorldStreamingPlugin, regia_nav::NavStreamPlugin));
        app.add_plugins((
            NetPlugin {
                mode: NetMode::DedicatedServer,
            },
            NetSessionPlugin,
        ));
        // The world kept across restarts (ADR-3354): inert until `server.toml` turns on
        // `[persistence]`.
        app.add_plugins(regia_framework::server_persistence::ServerPersistencePlugin);
        // One record per finished match of the `MatchRules` session, under `<server dir>/matches`
        // (`book/agents/match-results.md`). Records nothing until a match ends.
        app.add_plugins(regia_framework::match_results::MatchResultsPlugin);
        // Loaded-cell relevance (spec 78 § 21). Registered once a streamed level is open, not before.
        app.add_plugins(crate::stream_relevance::StreamCellRelevancePlugin);
    }
}

/// `Offline` and `Client` are refused at startup (spec 76 § 2): exit code 2.
///
/// # Errors
///
/// `mode` cannot host: the message names the field and what a server accepts.
pub fn check_net_mode(mode: NetMode) -> Result<(), HostError> {
    match mode {
        NetMode::DedicatedServer | NetMode::ListenServer => Ok(()),
        NetMode::Offline | NetMode::Client => Err(HostError {
            exit: ExitCode::Config,
            lines: vec![format!(
                "NetMode: {mode:?} cannot run in a server build (expected DedicatedServer or ListenServer)"
            )],
        }),
    }
}

/// The server directory: [`SERVER_DIR_ENV`], else the executable's directory, else `.`.
#[must_use]
pub fn server_dir() -> PathBuf {
    regia_core::brand::env_var_os(SERVER_DIR_ENV)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The host file of a rotation level: `levels/a.level.ron` under `assets://`.
fn level_file(app: &App, level: &str) -> Option<PathBuf> {
    let vfs = app.world().get_resource::<Vfs>()?;
    let path = VfsPath::parse(&format!("assets://{level}")).ok()?;
    if !vfs.exists(&path) {
        return None;
    }
    vfs.host_path(&path)
}

/// Opens `level` in the app, replacing the open one. Returns its id.
fn open_level_file(app: &mut App, level: &str) -> Result<LevelId, String> {
    let file = level_file(app, level).ok_or_else(|| "not in the server content".to_string())?;
    // A level cooked for streaming opens through the runtime (spec 78 §§ 6.5, 14).
    let streamed = regia_framework::level_assets::level_id_of(&file);
    if let Some(opener) = app.world().get_resource::<StreamedLevelOpener>().copied() {
        match (opener.0)(app.world_mut(), streamed, LoadMode::Replace) {
            StreamedOpen::NotCooked => {}
            StreamedOpen::Opened | StreamedOpen::AlwaysLoaded => return Ok(streamed),
            StreamedOpen::Failed(err) => return Err(err),
        }
    }
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let parsed = Level::from_ron_str(LevelId(regia_core::id::Guid::new_random()), &text)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let id = parsed.id;
    let world = app.world_mut();
    world
        .get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new)
        .insert(parsed);
    // `open_rotating` records only an id this returns. A file that validates and then fails to
    // spawn (a prefab that nests itself) puts the previous level back and would otherwise leave
    // this parse in the library for the life of the process.
    if let Err(err) = open_level(world, id, LoadMode::Replace) {
        if let Some(mut library) = world.get_resource_mut::<LevelLibrary>() {
            library.remove(id);
        }
        return Err(err.to_string());
    }
    Ok(id)
}

/// How many opened levels stay in the [`LevelLibrary`]: the open one and the one it replaced, which
/// may still be unloading. A rotating server must not grow by one parsed level per travel.
const KEPT_LEVELS: usize = 2;

/// Opens `level` and forgets the opened levels past [`KEPT_LEVELS`].
fn open_rotating(app: &mut App, opened: &mut VecDeque<LevelId>, level: &str) -> Result<(), String> {
    opened.push_back(open_level_file(app, level)?);
    while opened.len() > KEPT_LEVELS {
        forget_oldest(app, opened);
    }
    Ok(())
}

/// Drops the oldest opened level from the library unless a later open reuses its id: a streamed
/// open returns the same stable id every time, and that level is the live one.
fn forget_oldest(app: &mut App, opened: &mut VecDeque<LevelId>) {
    let Some(old) = opened.pop_front() else { return };
    if opened.contains(&old) {
        return;
    }
    if let Some(mut library) = app.world_mut().get_resource_mut::<LevelLibrary>() {
        library.remove(old);
    }
}

/// The host's level hook.
fn level_loader() -> LevelHook {
    let mut opened = VecDeque::new();
    Box::new(move |app: &mut App, level: &str| open_rotating(app, &mut opened, level))
}

/// Points `logs://` and crash reports at the server's own log directory: they would go to the
/// player's user directory, which a service account may not have.
fn use_log_dir(app: &mut App, log_dir: &Path) {
    if let Some(mut vfs) = app.world_mut().get_resource_mut::<Vfs>() {
        vfs.mount(MountPoint::Logs, DirMount::writable(log_dir.to_path_buf()));
    }
    regia_platform::crash::configure(
        Some(log_dir.to_path_buf()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
}

/// Builds the app the host runs: [`ServerPlugins`], the game, `configure`, then
/// [`SimulationPlugins`] and the fixed step of `tick_hz`. With `log_dir` a panic in a game's plugin
/// already reports there.
///
/// # Errors
///
/// The game left the app in a [`NetMode`] a server refuses.
pub fn build_app(
    game: impl Plugins,
    configure: impl FnOnce(&mut App),
    tick_hz: u32,
    agent: bool,
    log_dir: Option<&Path>,
) -> Result<App, HostError> {
    let mut app = App::new();
    app.insert_resource(Profiler(profiler()));
    app.add_plugins(ServerPlugins);
    if let Some(dir) = log_dir {
        use_log_dir(&mut app, dir);
    }
    app.add_plugins(game);
    configure(&mut app);
    app.add_plugins(SimulationPlugins);
    warn_on_tick_mismatch(&app, tick_hz);
    // `enable_fixed_step` pins one step per `App::update` and sets `max_substeps` to 1. The host
    // loop is the catch-up (spec 76 § 5), so the game's cap is restored. Unset stays at 5.
    let max_substeps = app
        .world()
        .get_resource::<FixedTime>()
        .map(|fixed| fixed.max_substeps.clamp(1, regia_app::MAX_SUBSTEPS_LIMIT))
        .unwrap_or(5);
    regia_app::enable_fixed_step(&mut app, f64::from(tick_hz));
    if let Some(mut fixed) = app.world_mut().get_resource_mut::<FixedTime>() {
        fixed.max_substeps = max_substeps;
    }
    add_agent(&mut app, agent);
    check_net_mode(app.world().get_resource::<NetMode>().copied().unwrap_or_default())?;
    app.finish_plugins();
    Ok(app)
}

/// Whether `server.tick_hz` replaces a different rate the game set, the default 60 included.
fn tick_overrides(game_hz: f64, tick_hz: u32) -> bool {
    game_hz != f64::from(tick_hz)
}

/// `server.tick_hz` replaces the game's own fixed rate, and game code tuned for that rate may not
/// expect it (spec 76 § 4).
fn warn_on_tick_mismatch(app: &App, tick_hz: u32) {
    if let Some(game) = app.world().get_resource::<FixedTime>()
        && tick_overrides(game.hz, tick_hz)
    {
        tracing::warn!(
            target: "regia",
            game_hz = game.hz,
            tick_hz,
            "server.tick_hz overrides the game's FixedTime rate"
        );
    }
}

/// `--agent`: the agent protocol server in the dedicated-server process (spec 76 § 10). A build
/// without the `agent` feature (every packaged server unless the profile allows it) says so.
#[cfg(feature = "agent")]
fn add_agent(app: &mut App, agent: bool) {
    if agent {
        app.add_plugins(regia_agent::AgentPlugin);
    }
}

#[cfg(not(feature = "agent"))]
fn add_agent(_app: &mut App, agent: bool) {
    if agent {
        tracing::warn!(target: "regia", "--agent: this server was built without the `agent` feature");
    }
}

/// A panic ends a server with code 70 after the crash report is written (spec 76 § 5); restarting
/// is the supervisor's job.
fn exit_70_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        std::process::exit(ExitCode::Crash.code());
    }));
}

/// Ends the process with `code` after printing `lines`.
fn fail(lines: &[String], code: ExitCode) -> ! {
    for line in lines {
        eprintln!("{line}");
    }
    std::process::exit(code.code());
}

/// Runs the host; returns its exit code.
fn host_game(host: &mut ServerHost, game: &mut AppHostGame) -> ExitCode {
    if let Err(err) = host.start(game, HostTransport::Udp) {
        fail(&err.lines, err.exit);
    }
    match host.signals().install_os_handlers() {
        Ok(events) => tracing::info!(target: "regia", ?events, "server host: OS events registered"),
        Err(err) => tracing::warn!(target: "regia", "server host: OS events not registered: {err}"),
    }
    host.spawn_stdin_console();
    loop {
        let stopped = host.run_once(game);
        for line in host.take_console_output() {
            println!("{line}");
        }
        if let Some(code) = stopped {
            return code;
        }
    }
}

/// [`crate::run_with`] of the `server` feature: parses the command line and `server.toml`, builds
/// the app, and runs the host until it stops. Never returns a failure: a failed start ends the
/// process with the exit code of spec 76 § 5 (2 configuration, 3 port, 4 content, 70 crash).
pub fn run_with(game: impl Plugins, configure: impl FnOnce(&mut App)) -> AppExit {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match CliOverrides::parse(&args) {
        Ok(cli) => cli,
        Err(errors) => {
            let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
            fail(&lines, ExitCode::Config)
        }
    };
    let base = server_dir();
    if cli.upgrade_config {
        // Spec 76 § 4 (ADR-5650): rewrite the file and exit instead of starting.
        match regia_net::server_host::config::upgrade_config_file(&base, &cli) {
            Ok(regia_net::server_host::config::ConfigUpgrade::Upgraded { path, from }) => println!(
                "{}: upgraded from format_version {from} to {}",
                path.display(),
                regia_net::server_host::config::SERVER_TOML_FORMAT_VERSION
            ),
            Ok(regia_net::server_host::config::ConfigUpgrade::Current { path }) => {
                println!("{}: already current", path.display());
            }
            Err(line) => fail(&[line], ExitCode::Config),
        }
        return AppExit::Success;
    }
    let env: EnvReader = Box::new(|key| regia_core::brand::env_var(key).ok());
    let agent = cli.agent;
    let mut host = ServerHost::new(&base, cli, env, Box::new(MonotonicClock::default()))
        .unwrap_or_else(|err| fail(&err.lines, err.exit));
    let log_dir = base.join(&host.settings().log_dir);
    let log = start_logging(Some(log_dir.clone()), Some(host.settings().keep_log_files));
    // Before the plugins: the platform's report hook wraps this one, so a panic writes its report
    // and then exits 70, also from a game's `Plugin::build`.
    exit_70_on_panic();
    // A native fault (SIGSEGV in C code or a driver) exits 70 too (spec 76 § 5, ADR-2440).
    if let Err(err) = regia_net::server_host::install_native_crash_exit() {
        tracing::info!(target: "regia", "server host: native crash exit code not installed: {err}");
    }
    let server_settings = host.settings().clone();
    let app = build_app(
        game,
        move |app| {
            app.insert_resource(server_settings);
            configure(app);
        },
        host.settings().tick_hz,
        agent,
        Some(&log_dir),
    )
    .unwrap_or_else(|err| fail(&err.lines, err.exit));

    let mut game = AppHostGame::new(app);
    game.level_exists = Some(Box::new(|app: &mut App, level: &str| {
        level_file(app, level).is_some()
    }));
    game.load_level = Some(level_loader());
    let code = host_game(&mut host, &mut game);
    if let Some(log) = &log {
        log.flush();
    }
    host.signals().mark_stopped();
    match code {
        ExitCode::Ok => AppExit::Success,
        code => std::process::exit(code.code()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoGame;

    impl Plugin for NoGame {
        fn build(&self, _app: &mut App) {}
    }

    #[test]
    fn a_server_refuses_offline_and_client() {
        assert!(check_net_mode(NetMode::DedicatedServer).is_ok());
        assert!(check_net_mode(NetMode::ListenServer).is_ok());
        for mode in [NetMode::Offline, NetMode::Client] {
            let err = check_net_mode(mode).expect_err("refused");
            assert_eq!(err.exit.code(), 2);
            let line = &err.lines[0];
            assert!(line.contains("DedicatedServer"), "{line}");
            assert!(line.contains("ListenServer"), "{line}");
        }
    }

    #[test]
    fn server_exit_codes_match_spec_76() {
        assert_eq!(ExitCode::Ok.code(), 0);
        assert_eq!(ExitCode::Config.code(), 2);
        assert_eq!(ExitCode::Bind.code(), 3);
        assert_eq!(ExitCode::Content.code(), 4);
        assert_eq!(ExitCode::Crash.code(), 70);
    }

    #[test]
    fn server_keeps_the_games_max_substeps() {
        let pinned = build_app(
            NoGame,
            |app| {
                let mut fixed = FixedTime::default();
                fixed.max_substeps = 1;
                app.insert_resource(fixed);
            },
            30,
            false,
            None,
        )
        .expect("app");
        assert_eq!(pinned.world().resource::<FixedTime>().max_substeps, 1);

        let wider = build_app(
            NoGame,
            |app| {
                let mut fixed = FixedTime::default();
                fixed.max_substeps = 10;
                app.insert_resource(fixed);
            },
            30,
            false,
            None,
        )
        .expect("app");
        assert_eq!(wider.world().resource::<FixedTime>().max_substeps, 10);

        let open = build_app(NoGame, |_| {}, 30, false, None).expect("app");
        assert_eq!(
            open.world().resource::<FixedTime>().max_substeps,
            5,
            "an unset cap stays at the spec default, not the one-step pin"
        );
    }

    #[test]
    fn server_updates_close_profiler_frames() {
        let mut app = build_app(NoGame, |_| {}, 30, false, None).expect("app");
        let before = app.world().resource::<Profiler>().0.frame_count();
        app.update();
        app.update();
        let after = app.world().resource::<Profiler>().0.frame_count();
        if before >= regia_core::profiling::HISTORY_FRAMES {
            assert_eq!(after, regia_core::profiling::HISTORY_FRAMES);
        } else {
            assert!(after > before, "each server update closes a profiler frame");
        }
    }

    #[test]
    fn the_server_directory_defaults_to_the_executable() {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        if regia_core::brand::env_var_os(SERVER_DIR_ENV).is_none() {
            assert_eq!(Some(server_dir()), exe_dir);
        }
    }

    #[test]
    fn the_app_defaults_to_dedicated_server_and_keeps_a_game_plugin() {
        let app = build_app(NoGame, |_| {}, 30, false, None).expect("app");
        assert_eq!(app.world().resource::<NetMode>(), &NetMode::DedicatedServer);
        assert!(app.world().contains_resource::<regia_net::session::NetSession>());
        assert!(app.world().contains_resource::<Vfs>());
        // ADR-3354: every server can keep its world; `[persistence] enabled` turns it on.
        assert!(
            app.world()
                .contains_resource::<regia_framework::server_persistence::ServerPersistence>()
        );
    }

    /// Spec 78 §§ 13.2, 14: a dedicated server loads streamed nav tiles, so its `sim_ready` waits on
    /// the `nav` gate; a game that added the hook itself keeps its own (no duplicate).
    #[test]
    fn a_server_answers_the_streamed_nav_gate() {
        let app = build_app(NoGame, |_| {}, 30, false, None).expect("app");
        assert!(
            app.world().contains_resource::<regia_nav::NavStreamState>(),
            "the nav stream hook is installed"
        );
        let gates = app.world().resource::<regia_scene::streaming::StreamGates>();
        let level = LevelId(regia_core::id::Guid::from_u128(0x5e_7e));
        let cell = regia_scene::stream_grid::CellCoord::new(0, 0);
        assert!(
            gates.missing(level, cell).contains(&regia_nav::NAV_GATE),
            "a spawned cell waits on the nav gate: {:?}",
            gates.missing(level, cell)
        );

        struct OwnHook;
        impl Plugin for OwnHook {
            fn build(&self, app: &mut App) {
                app.add_plugins(regia_nav::NavStreamPlugin);
            }
        }
        let app = build_app(OwnHook, |_| {}, 30, false, None).expect("a game's own hook wins");
        assert!(app.world().contains_resource::<regia_nav::NavStreamState>());
    }

    #[test]
    fn a_game_that_stays_offline_is_refused_with_exit_2() {
        let offline = NetPlugin {
            mode: NetMode::Offline,
        };
        let err = build_app(offline, |_| {}, 30, false, None)
            .err()
            .expect("refused");
        assert_eq!(err.exit.code(), 2);
    }

    /// Spec 78 §§ 6.5, 14: a level cooked for streaming opens through the runtime, by its stable id.
    #[test]
    fn a_level_cooked_for_streaming_opens_through_the_runtime() {
        let dir = std::env::temp_dir().join(format!("regia_server_streamed_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        std::fs::write(dir.join("levels/a.level.ron"), "not a level: must not be parsed").expect("write");
        let mut app = App::new();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
        app.insert_resource(vfs);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(StreamedLevelOpener(|_, _, _| StreamedOpen::Opened));
        let file = level_file(&app, "levels/a.level.ron").expect("in the content");
        let opened = open_level_file(&mut app, "levels/a.level.ron").expect("opens");
        assert_eq!(opened, regia_framework::level_assets::level_id_of(&file));

        app.insert_resource(StreamedLevelOpener(|_, _, _| {
            StreamedOpen::Failed("corrupt".into())
        }));
        assert!(open_level_file(&mut app, "levels/a.level.ron").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn levels_resolve_through_assets_only() {
        let dir = std::env::temp_dir().join(format!("regia_server_levels_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        std::fs::write(
            dir.join("levels/a.level.ron"),
            "(format_version: 1, entities: [])",
        )
        .expect("write");
        let mut app = App::new();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
        app.insert_resource(vfs);
        app.add_plugins(regia_scene::ScenePlugin);
        assert!(level_file(&app, "levels/a.level.ron").is_some());
        assert!(level_file(&app, "levels/missing.level.ron").is_none());
        open_level_file(&mut app, "levels/a.level.ron").expect("opens");
        assert!(open_level_file(&mut app, "levels/missing.level.ron").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotating_server_keeps_two_parsed_levels() {
        let dir = std::env::temp_dir().join(format!("regia_server_rotation_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        std::fs::write(
            dir.join("levels/a.level.ron"),
            "(format_version: 1, entities: [])",
        )
        .expect("write");
        let mut app = App::new();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
        app.insert_resource(vfs);
        app.add_plugins(regia_scene::ScenePlugin);
        let mut opened = VecDeque::new();
        let mut seen = Vec::new();
        for _ in 0..5 {
            open_rotating(&mut app, &mut opened, "levels/a.level.ron").expect("opens");
            app.update();
            seen.extend(opened.back().copied());
        }
        let library = app.world().resource::<LevelLibrary>();
        let kept: Vec<bool> = seen.iter().map(|id| library.contains(*id)).collect();
        assert_eq!(kept, [false, false, false, true, true]);
        let mut load = level_loader();
        load(&mut app, "levels/a.level.ron").expect("the hook opens");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Duplicate guids fail inside `SceneFile::from_ron_str` (`finish_load` validates), so the
    /// parse never reaches the library. This is the audit's example and it does not leak.
    #[test]
    fn a_duplicate_guid_file_is_refused_before_the_library() {
        let dir = regia_test_utils::unique_temp_dir("server_dup_guid");
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        let bad = dir.join("levels/bad.level.ron");
        let duplicate = br#"(
            format_version: 1,
            entities: [
                (guid: "8b6f2a11-0000-4000-8000-00000000000a", name: "a", components: {}),
                (guid: "8b6f2a11-0000-4000-8000-00000000000a", name: "b", components: {}),
            ],
        )"#;
        regia_test_utils::write_atomic(bad.as_path(), duplicate).expect("bad");
        let mut app = App::new();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
        app.insert_resource(vfs);
        app.add_plugins(regia_scene::ScenePlugin);
        app.world_mut().insert_resource(LevelLibrary::new());
        let before = format!("{:?}", app.world().resource::<LevelLibrary>());
        let err = open_level_file(&mut app, "levels/bad.level.ron").expect_err("duplicate guids");
        assert!(err.contains("duplicate entity guids"), "{err}");
        assert_eq!(before, format!("{:?}", app.world().resource::<LevelLibrary>()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that parses and then fails to spawn stays in the library. `open_rotating` only
    /// records an id `open_level` accepted, so each failed travel would keep another `Arc<Level>`.
    /// A prefab that nests itself validates and fails only at the nesting limit.
    #[test]
    fn a_failed_spawn_does_not_stay_in_the_level_library() {
        let dir = regia_test_utils::unique_temp_dir("server_bad_spawn");
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        let good = dir.join("levels/good.level.ron");
        let bad = dir.join("levels/bad.level.ron");
        regia_test_utils::write_atomic(good.as_path(), b"(format_version: 1, entities: [])").expect("good");
        let asset = "8b6f2a11-0000-4000-8000-0000000000cc";
        let looping = format!(
            r#"(
                format_version: 1,
                entities: [
                    (guid: "8b6f2a11-0000-4000-8000-0000000000d0", name: "Loop", components: {{}},
                     prefab_instance: (prefab: "asset:{asset}", overrides: [])),
                ],
            )"#
        );
        let holder = format!(
            r#"(
                format_version: 1,
                entities: [
                    (guid: "8b6f2a11-0000-4000-8000-00000000000b", name: "Holder", components: {{}},
                     prefab_instance: (prefab: "asset:{asset}", overrides: [])),
                ],
            )"#
        );
        regia_test_utils::write_atomic(bad.as_path(), holder.as_bytes()).expect("bad");
        let mut app = App::new();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
        app.insert_resource(vfs);
        app.add_plugins(regia_scene::ScenePlugin);
        let parsed = regia_scene::format::SceneFile::from_ron_str(&looping).expect("prefab parses");
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let template = regia_scene::prefab::PrefabTemplate::from_scene_file(
            asset.parse().expect("asset guid"),
            &parsed,
            &registry.read(),
        )
        .expect("bake");
        app.world_mut()
            .resource_mut::<regia_scene::prefab::PrefabLibrary>()
            .insert(template);
        let good_id = open_level_file(&mut app, "levels/good.level.ron").expect("good opens");
        let before = format!("{:?}", app.world().resource::<LevelLibrary>());
        for _ in 0..2 {
            let err = open_level_file(&mut app, "levels/bad.level.ron").expect_err("self-nesting prefab");
            assert!(
                err.contains("prefab nesting"),
                "the open must fail at spawn, after the parse: {err}"
            );
        }
        let after = format!("{:?}", app.world().resource::<LevelLibrary>());
        assert_eq!(
            before, after,
            "a failed spawn must not leave its parse in the library"
        );
        assert!(
            !after.contains("8b6f2a11-0000-4000-8000-00000000000b"),
            "the rejected level is still stored: {after}"
        );
        assert!(app.world().resource::<LevelLibrary>().contains(good_id));
        assert_eq!(
            app.world()
                .resource::<regia_scene::level::LevelManager>()
                .current(),
            Some(good_id),
            "the level that was open stays open"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_streamed_rotation_keeps_the_live_level() {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let id = LevelId(regia_core::id::Guid::new_random());
        let level = Level::from_scene_file(id, regia_scene::format::SceneFile::default());
        app.world_mut()
            .get_resource_or_insert_with(LevelLibrary::new)
            .insert(level);
        let mut opened: VecDeque<LevelId> = VecDeque::new();
        // The same stable id three times, as three travels of a one-level rotation.
        for _ in 0..3 {
            opened.push_back(id);
            while opened.len() > KEPT_LEVELS {
                // The loop body of `open_rotating`, minus the file open.
                forget_oldest(&mut app, &mut opened);
            }
        }
        assert!(app.world().resource::<LevelLibrary>().contains(id));
    }

    #[test]
    fn the_tick_warning_includes_the_default_rate() {
        assert!(tick_overrides(60.0, 30));
        assert!(tick_overrides(60.0, 128));
        assert!(!tick_overrides(60.0, 60));
        assert!(!tick_overrides(30.0, 30));
    }
}
