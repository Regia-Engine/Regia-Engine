// SPDX-License-Identifier: MPL-2.0
//! Spec 76 § 11: the dedicated-server host, in process, with a test clock and loopback clients.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_net::CryptoMode;
use regia_net::server_host::config::{CliOverrides, SERVER_TOML_FORMAT_VERSION, resolve_config};
use regia_net::server_host::{
    ExitCode, HostClock, HostState, HostTransport, ServerHost, SessionOnlyGame, TestClock,
};
use regia_net::session::NetSession;
use regia_net::session::hub::SimInput;
use regia_net::session::lifecycle::{ClientSessionEvent, PeerState, ServerSessionEvent, SessionSettings};
use regia_net::session::net_session::{ConnectOptions, ConnectTarget};
use regia_net::session::proto::{DisconnectReason, SessionMsg};

fn unique_temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-net-host-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// Writes a secret the host must accept as private (a key or an admin token): created through
/// `regia_net::private_file::create_private`, so it is mode 0600 on Unix and carries the
/// owner-only DACL on Windows whatever the temp folder grants (its inherited entries never
/// apply). A plain write would inherit the temp folder's ACL on Windows, which the host is right
/// to refuse when TEMP is shared, e.g. under `<build-dir>` (Windows pass 2026-11-18, ADR-8504).
fn write_private(path: &Path, text: &str) {
    let mut file = regia_net::private_file::create_private(path).expect("create private");
    file.write_all(text.as_bytes()).expect("write private");
}

const TOML: &str = r#"
format_version = 1
[server]
name = "Test"
port = 7790
max_players = 8
tick_hz = 60
[levels]
rotation = ["levels/a.level.ron", "levels/b.level.ron"]
match_end_delay_s = 1
[logs]
dir = "logs"
"#;

fn no_env() -> regia_net::server_host::EnvReader {
    Box::new(|_| None)
}

fn args(list: &[&str]) -> CliOverrides {
    CliOverrides::parse(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).expect("cli")
}

struct Bench {
    dir: PathBuf,
    host: ServerHost,
    game: SessionOnlyGame,
    clock: TestClock,
}

fn bench_with(toml: &str, prepare: impl FnOnce(&Path)) -> Bench {
    let dir = unique_temp_dir("bench");
    write(&dir.join("server.toml"), toml);
    prepare(&dir);
    let clock = TestClock::new();
    let host =
        ServerHost::new(&dir, CliOverrides::default(), no_env(), Box::new(clock.clone())).expect("config");
    let mut game = SessionOnlyGame::new(NetSession::default());
    game.clock = Some(clock.clone());
    Bench {
        dir,
        host,
        game,
        clock,
    }
}

fn bench() -> Bench {
    let mut b = bench_with(TOML, |_| {});
    b.host.start(&mut b.game, HostTransport::Local).expect("start");
    b
}

impl Bench {
    fn ticks(&mut self, n: usize) -> Option<ExitCode> {
        for _ in 0..n {
            if let Some(code) = self.host.run_once(&mut self.game) {
                return Some(code);
            }
        }
        None
    }

    fn in_game(&self) -> usize {
        let server = self.game.session.server().expect("server");
        server
            .peer_ids()
            .into_iter()
            .filter(|p| server.peer_state(*p) == Some(PeerState::InGame))
            .count()
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// 5. Precedence: default < file < env < CLI, for every overridable key; validation; unknown keys.
#[test]
fn config_precedence_validation_and_unknown_keys() {
    let dir = unique_temp_dir("config");
    write(
        &dir.join("server.toml"),
        &format!("{TOML}\n[future]\nthing = 1\n[server.extra]\n"),
    );
    let empty: HashMap<&str, &str> = HashMap::new();
    let env_of = |m: HashMap<&'static str, &'static str>| move |k: &str| m.get(k).map(|v| (*v).to_owned());
    // Defaults < file.
    let r = resolve_config(&dir, &CliOverrides::default(), &env_of(empty.clone())).expect("ok");
    assert_eq!(r.settings.port, 7790);
    assert_eq!(r.settings.max_players, 8);
    assert_eq!(r.settings.idle_tick_hz, 10, "default kept");
    assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    // File < env.
    let env: HashMap<&str, &str> = [
        ("REGIA_SERVER_PORT", "7791"),
        ("REGIA_SERVER_MAX_PLAYERS", "9"),
        ("REGIA_SERVER_LEVEL", "levels/env.level.ron"),
        ("REGIA_SERVER_PASSWORD", "envpw"),
    ]
    .into_iter()
    .collect();
    let r = resolve_config(&dir, &CliOverrides::default(), &env_of(env.clone())).expect("ok");
    assert_eq!(
        (
            r.settings.port,
            r.settings.max_players,
            r.settings.rotation.clone(),
            r.settings.password.as_str()
        ),
        (7791, 9, vec!["levels/env.level.ron".to_owned()], "envpw")
    );
    // Env < CLI.
    let cli = args(&[
        "--port",
        "7792",
        "--max-players",
        "10",
        "--level",
        "levels/cli.level.ron",
        "--bind",
        "127.0.0.1",
        "--name",
        "Cli",
        "--log-dir",
        "cli-logs",
    ]);
    let r = resolve_config(&dir, &cli, &env_of(env)).expect("ok");
    let s = r.settings;
    assert_eq!((s.port, s.max_players), (7792, 10));
    assert_eq!(s.rotation, vec!["levels/cli.level.ron".to_owned()]);
    assert_eq!(s.bind.to_string(), "127.0.0.1");
    assert_eq!((s.name.as_str(), s.log_dir.as_str()), ("Cli", "cli-logs"));
    // --config names another file.
    write(&dir.join("other.toml"), &TOML.replace("7790", "7795"));
    let r = resolve_config(&dir, &args(&["--config", "other.toml"]), &env_of(empty.clone())).expect("ok");
    assert_eq!(r.settings.port, 7795);

    // Each validation rule: one field-level error, exit code 2.
    // A version newer than this build reads: one past the current version (v2 adds `[persistence]`,
    // ADR-3354; v3 bots and entropy, ADR-4854; v1 and v2 still load). A fixed number goes stale at
    // the next bump (ADR-5517).
    let newer = format!("format_version = {}", SERVER_TOML_FORMAT_VERSION + 1);
    let cases = [
        ("format_version = 1", newer.as_str(), "format_version"),
        ("[logs]", "[persistence]\nkeep = 0\n[logs]", "persistence.keep"),
        (
            "[logs]",
            "[persistence]\ndir = \"../up\"\n[logs]",
            "persistence.dir",
        ),
        ("port = 7790", "port = 0", "server.port"),
        ("port = 7790", "port = 7778", "server.port"),
        ("max_players = 8", "max_players = 300", "server.max_players"),
        ("tick_hz = 60", "tick_hz = 5", "server.tick_hz"),
        ("[net]", "[net]\nsend_every_ticks = 0", "net.send_every_ticks"),
        (
            "rotation = [\"levels/a.level.ron\", \"levels/b.level.ron\"]",
            "rotation = []",
            "levels.rotation",
        ),
        ("max_players = 8", "max_players = \"many\"", "server.max_players"),
    ];
    for (from, to, field) in cases {
        let text = if from == "[net]" {
            format!("{TOML}\n[net]\nsend_every_ticks = 0\n")
        } else {
            TOML.replace(from, to)
        };
        let d = unique_temp_dir("bad");
        write(&d.join("server.toml"), &text);
        let err = ServerHost::new(&d, CliOverrides::default(), no_env(), Box::new(TestClock::new()))
            .expect_err(field);
        assert_eq!(err.exit, ExitCode::Config);
        assert!(
            err.lines
                .iter()
                .any(|l| l.starts_with(field) && l.contains("expected")),
            "{field}: {:?}",
            err.lines
        );
        let _ = std::fs::remove_dir_all(&d);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// 6. Lifecycle: Starting → Loading → Running; two clients; a shutdown request drains (both get
// `server_shutting_down`) and exits 0; a second request exits at once.
#[test]
fn lifecycle_drain_and_second_request() {
    let mut b = bench_with(TOML, |_| {});
    assert_eq!(b.host.state(), HostState::Starting);
    b.host.start(&mut b.game, HostTransport::Local).expect("start");
    assert_eq!(b.host.state(), HostState::Running);
    assert_eq!(b.host.level(), "levels/a.level.ron");
    b.game
        .session
        .add_simulated_clients(2, SimInput::Idle)
        .expect("sims");
    b.ticks(60);
    assert_eq!(b.in_game(), 2);
    b.host.signals().request_shutdown();
    let code = b.ticks(600);
    assert_eq!(code, Some(ExitCode::Ok));
    assert_eq!(b.host.state(), HostState::Stopped);
    let endings: Vec<_> = b.game.session.simulated_endings().cloned().collect();
    assert_eq!(endings, vec![DisconnectReason::ServerShuttingDown; 2]);

    // A second request while draining exits at once.
    let mut b = bench();
    b.game
        .session
        .add_simulated_clients(1, SimInput::Idle)
        .expect("sims");
    b.ticks(60);
    b.host.signals().request_shutdown();
    assert_eq!(b.ticks(1), None);
    assert_eq!(b.host.state(), HostState::Draining);
    // The client cannot answer anymore (cut): the drain would wait for the grace.
    b.game.session.simulated_mut().expect("sims").cut(0);
    b.host.signals().request_shutdown();
    assert_eq!(b.ticks(1), Some(ExitCode::Ok));
}

// 7. Reload applies hot keys; a port change needs a restart.
#[test]
fn reload_applies_hot_keys_and_reports_restart_keys() {
    let mut b = bench();
    write(
        &b.dir.join("server.toml"),
        &TOML
            .replace("max_players = 8", "max_players = 4\nmotd = \"hello\"")
            .replace("port = 7790", "port = 7799"),
    );
    b.host.signals().request_reload();
    b.ticks(1);
    let opts = b.game.session.server().expect("server").options().clone();
    assert_eq!((opts.motd.as_str(), opts.max_players), ("hello", 4));
    assert_eq!(b.host.settings().port, 7790, "port needs a restart");
    let out = b.host.execute(&mut b.game, "reload");
    assert!(
        out.iter()
            .any(|l| l.contains("nothing") || l.contains("bans.ron")),
        "{out:?}"
    );
    assert!(
        out.iter()
            .any(|l| l.contains("needs a restart") && l.contains("server.port")),
        "{out:?}"
    );
}

// 8. Every console command, through the console channel; commands run at the next tick.
#[test]
fn console_commands_queue_to_the_next_tick() {
    let mut b = bench();
    b.game.chat = Some(Vec::new());
    b.game
        .session
        .add_simulated_clients(3, SimInput::Idle)
        .expect("sims");
    b.ticks(60);
    let console = b.host.console_sender();
    let run = |b: &mut Bench, line: &str| -> Vec<String> {
        console.send(line.to_owned()).expect("send");
        assert!(
            b.host.take_console_output().is_empty(),
            "not executed before the tick"
        );
        b.ticks(1);
        b.host.take_console_output()
    };
    let help = run(&mut b, "help");
    assert_eq!(help.len(), regia_net::server_host::CONSOLE_HELP.len());
    let status = run(&mut b, "status");
    assert!(
        status[0].contains("level=levels/a.level.ron") && status[0].contains("players=3/8"),
        "{status:?}"
    );
    // Spec 76 § 6 (ADR-5650): `status` names each player with its ping; `players` has ping and
    // loss from the transport's estimate (simulated clients have meters), not `-`.
    assert_eq!(
        b.game.session.peer_link_stats().len(),
        3,
        "every simulated peer has an estimate"
    );
    // The number between `key=` and `unit` in `line`, if there is one.
    let number = |line: &str, key: &str, unit: &str| -> Option<f64> {
        let rest = &line[line.find(key)? + key.len()..];
        rest[..rest.find(unit)?].parse().ok()
    };
    assert_eq!(status.len(), 4, "{status:?}");
    assert!(status[1].starts_with("  1.0 ping="), "{status:?}");
    assert!(number(&status[1], "ping=", "ms").is_some(), "{status:?}");
    let players = run(&mut b, "players");
    assert_eq!(players.len(), 3, "{players:?}");
    assert!(players[0].starts_with("1.0 sim-1 127.0.0.1:"), "{players:?}");
    assert!(number(&players[0], "ping=", "ms").is_some(), "{players:?}");
    assert!(
        number(&players[0], "loss=", "%").is_some_and(|l| (0.0..=100.0).contains(&l)),
        "{players:?}"
    );
    assert!(run(&mut b, "kick 1.0 afk")[0].contains("kicked 1.0"));
    b.ticks(10);
    assert_eq!(b.game.session.peer_state(regia_net::model::PeerId(1)), None);
    assert!(run(&mut b, "kick 9.0")[0].contains("player_unknown"));
    let ban = run(&mut b, "ban 2 30m spamming");
    assert!(ban[0].contains("banned 2 (entry 1)"), "{ban:?}");
    b.ticks(10);
    assert_eq!(b.game.session.peer_state(regia_net::model::PeerId(2)), None);
    let bans = run(&mut b, "bans");
    assert!(
        bans[0].starts_with("1 address 127.0.0.1") && bans[0].contains("spamming"),
        "{bans:?}"
    );
    assert!(run(&mut b, "unban 1")[0].contains("removed ban entry 1"));
    assert_eq!(run(&mut b, "bans"), vec!["no bans".to_owned()]);
    assert!(run(&mut b, "unban 7")[0].contains("ban_entry_unknown"));
    assert!(run(&mut b, "level levels/b.level.ron")[0].starts_with("travel"));
    assert_eq!(b.host.level(), "levels/b.level.ron");
    // `level` does not move the rotation: from index 0 `next` is `b`, then it wraps to `a`.
    let next = run(&mut b, "next");
    assert!(
        next[0].starts_with("travel ") && next[0].ends_with(" to levels/b.level.ron"),
        "{next:?}"
    );
    assert_eq!(b.host.level(), "levels/b.level.ron");
    let next = run(&mut b, "next");
    assert!(next[0].ends_with(" to levels/a.level.ron"), "{next:?}");
    assert_eq!(b.host.level(), "levels/a.level.ron");
    assert!(run(&mut b, "say hello all")[0].contains("said: hello all"));
    assert_eq!(b.game.chat.as_deref(), Some(&["hello all".to_owned()][..]));
    assert!(run(&mut b, "reload")[0].starts_with("reload applied"));
    assert!(run(&mut b, "frobnicate")[0].contains("unknown command `frobnicate`"));
    assert!(run(&mut b, "quit 1")[0].contains("draining"));
    assert_eq!(b.host.state(), HostState::Draining);
    assert_eq!(b.ticks(200), Some(ExitCode::Ok));
}

// Match end: the host travels to the next rotation level after the delay.
#[test]
fn match_end_rotates_after_the_delay() {
    let mut b = bench();
    b.game.match_over = true;
    b.ticks(30);
    assert_eq!(b.host.level(), "levels/a.level.ron");
    b.game.match_over = false;
    b.ticks(40);
    assert_eq!(b.host.level(), "levels/b.level.ron");
}

/// Finding 5 (deep-zai net-lobby): a travel started from inside the game whose level fails to
/// load on the host must not update the host's reported level: `status`, `/health` and the
/// rotation bookkeeping would claim the new level while the game still runs the old one, the
/// pairing `LevelLoaded` never fires, and a save-on-leave persistence layer keeps saving the
/// old level every tick.
struct FailingTravelGame {
    session: NetSession,
    /// Level `load_level` refuses (simulates content missing on the host).
    broken: String,
    levels_loaded: Vec<String>,
    events: Vec<String>,
}

impl FailingTravelGame {
    fn new(broken: &str) -> Self {
        Self {
            session: NetSession::default(),
            broken: broken.to_owned(),
            levels_loaded: Vec::new(),
            events: Vec::new(),
        }
    }
}

impl regia_net::server_host::HostGame for FailingTravelGame {
    fn session(&mut self) -> &mut NetSession {
        &mut self.session
    }

    fn load_level(&mut self, level: &str) -> Result<(), String> {
        self.levels_loaded.push(level.to_owned());
        if level == self.broken {
            Err("level failed to load".to_owned())
        } else {
            Ok(())
        }
    }

    fn step(&mut self) {
        self.session.update();
    }

    fn host_event(&mut self, event: &regia_net::server_host::HostEvent<'_>) -> Result<(), String> {
        self.events.push(
            match event {
                regia_net::server_host::HostEvent::LevelLeaving { .. } => "leaving",
                regia_net::server_host::HostEvent::LevelLoaded { .. } => "loaded",
                _ => "other",
            }
            .to_owned(),
        );
        Ok(())
    }
}

// An in-game travel whose level fails to load keeps the host on the old level and does not
// retry (or re-save) every tick.
#[test]
fn a_failed_in_game_travel_keeps_the_hosted_level() {
    let dir = unique_temp_dir("failed-travel");
    write(&dir.join("server.toml"), TOML);
    let clock = TestClock::new();
    let mut host = ServerHost::new(&dir, CliOverrides::default(), no_env(), Box::new(clock)).expect("config");
    let mut game = FailingTravelGame::new("levels/broken.level.ron");
    host.start(&mut game, HostTransport::Local).expect("start");
    assert_eq!(host.level(), "levels/a.level.ron");

    // The game travels from inside (agent `server.change_level`); the level fails to load.
    let _travel = game
        .session
        .server_mut()
        .expect("hosted")
        .travel("levels/broken.level.ron");
    host.run_once(&mut game);
    assert_eq!(
        host.level(),
        "levels/a.level.ron",
        "a failed load keeps the reported level"
    );
    assert_eq!(game.events.iter().filter(|e| *e == "leaving").count(), 1);
    assert!(
        !game.events.iter().any(|e| e == "loaded"),
        "no LevelLoaded pairs the failed leave"
    );

    // Not retried every tick (a save-on-leave hook must not save every tick either).
    host.run_once(&mut game);
    host.run_once(&mut game);
    assert_eq!(
        game.levels_loaded
            .iter()
            .filter(|l| *l == "levels/broken.level.ron")
            .count(),
        1,
        "the failed travel is not retried every tick"
    );
    assert_eq!(game.events.iter().filter(|e| *e == "leaving").count(), 1);

    // A working travel afterwards is followed.
    let _travel = game
        .session
        .server_mut()
        .expect("hosted")
        .travel("levels/b.level.ron");
    host.run_once(&mut game);
    assert_eq!(host.level(), "levels/b.level.ron");
    let _ = std::fs::remove_dir_all(&dir);
}

// A failed in-game travel is skipped only while it is the same travel: a new travel to the same
// level (the content was fixed, an agent calls `server.change_level` again) loads it, also after
// the session went back to the hosted level in between.
#[test]
fn a_new_travel_retries_a_level_that_failed_before() {
    let dir = unique_temp_dir("failed-travel-retry");
    write(&dir.join("server.toml"), TOML);
    let clock = TestClock::new();
    let mut host = ServerHost::new(&dir, CliOverrides::default(), no_env(), Box::new(clock)).expect("config");
    let mut game = FailingTravelGame::new("levels/broken.level.ron");
    host.start(&mut game, HostTransport::Local).expect("start");
    let travel = |game: &mut FailingTravelGame, level: &str| {
        let _ = game.session.server_mut().expect("hosted").travel(level);
    };
    let loads = |game: &FailingTravelGame| {
        game.levels_loaded
            .iter()
            .filter(|l| *l == "levels/broken.level.ron")
            .count()
    };

    travel(&mut game, "levels/broken.level.ron");
    host.run_once(&mut game);
    host.run_once(&mut game);
    assert_eq!(loads(&game), 1, "the failed travel is tried once");
    assert_eq!(host.level(), "levels/a.level.ron");

    // The content is fixed and the same level is travelled to again: the host follows.
    game.broken.clear();
    travel(&mut game, "levels/broken.level.ron");
    host.run_once(&mut game);
    assert_eq!(
        loads(&game),
        2,
        "a new travel to the level that failed loads it again"
    );
    assert_eq!(host.level(), "levels/broken.level.ron");

    // Fail it once more, go back to a working level, then retry: still followed.
    game.broken = "levels/c.level.ron".to_owned();
    travel(&mut game, "levels/c.level.ron");
    host.run_once(&mut game);
    assert_eq!(host.level(), "levels/broken.level.ron", "c failed to load");
    travel(&mut game, "levels/broken.level.ron");
    host.run_once(&mut game);
    game.broken.clear();
    travel(&mut game, "levels/c.level.ron");
    host.run_once(&mut game);
    assert_eq!(
        host.level(),
        "levels/c.level.ron",
        "a travel back and a retry of the failed level loads it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// 9. Remote admin.
#[test]
fn remote_admin_token_three_strikes_and_world_readable_file() {
    if !cfg!(debug_assertions) {
        return; // Remote admin on an unencrypted transport is a dev-build override.
    }
    let mut b = bench_with(TOML, |dir| {
        write_private(&dir.join("admin.token"), "s3cret\n");
    });
    b.host.allow_unencrypted_admin = true;
    b.host.start(&mut b.game, HostTransport::Local).expect("start");
    assert!(b.host.remote_admin());
    b.game
        .session
        .add_simulated_clients(2, SimInput::Idle)
        .expect("sims");
    b.ticks(60);
    let send = |b: &mut Bench, i: usize, msg: SessionMsg| {
        b.game
            .session
            .simulated_mut()
            .expect("sims")
            .client_mut(i)
            .core
            .send_admin(msg);
    };
    send(
        &mut b,
        0,
        SessionMsg::AdminLogin {
            token: "s3cret".into(),
        },
    );
    b.ticks(5);
    send(
        &mut b,
        0,
        SessionMsg::AdminCommand {
            line: "status".into(),
        },
    );
    b.ticks(5);
    let log = &b.game.session.simulated().expect("sims").client(0).log;
    let outputs: Vec<&Vec<String>> = log
        .iter()
        .filter_map(|e| match e {
            regia_net::session::ClientSessionEvent::AdminOutput(l) => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(outputs[0], &vec!["admin: logged in".to_owned()]);
    assert!(outputs[1][0].starts_with("name=Test"), "{outputs:?}");
    for _ in 0..3 {
        send(
            &mut b,
            1,
            SessionMsg::AdminLogin {
                token: "guess".into(),
            },
        );
        b.ticks(3);
    }
    b.ticks(5);
    let endings: Vec<_> = b
        .game
        .session
        .simulated_endings()
        .map(DisconnectReason::code)
        .collect();
    assert_eq!(endings, vec!["auth_failed"]);

    // Audit v0.2.12: group bits too (0640, 0660, 0620), as for `server.key`.
    #[cfg(unix)]
    for mode in [0o644, 0o640, 0o660, 0o620, 0o604] {
        use std::os::unix::fs::PermissionsExt;
        let mut b = bench_with(TOML, |dir| {
            let p = dir.join("admin.token");
            write(&p, "s3cret\n");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
        });
        b.host.allow_unencrypted_admin = true;
        b.host.start(&mut b.game, HostTransport::Local).expect("start");
        assert!(
            !b.host.remote_admin(),
            "a token file other users can access ({mode:o}) disables remote admin"
        );
    }
    // Without the dev override a host with no network transport has no encrypted link: off.
    let mut b = bench_with(TOML, |dir| write_private(&dir.join("admin.token"), "s3cret\n"));
    b.host.start(&mut b.game, HostTransport::Local).expect("start");
    assert!(!b.host.remote_admin());
}

fn udp_toml(extra: &str) -> String {
    // The host validates `server.port` as 1..=65535, so ask the OS for a free one.
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    let port = probe.local_addr().expect("addr").port();
    let base = TOML.replace("port = 7790", &format!("port = {port}\nbind = \"127.0.0.1\""));
    format!("{base}\n[net]\n{extra}\n")
}

fn admin_token_file(dir: &Path) {
    write_private(&dir.join("admin.token"), "s3cret\n");
}

/// A client on real UDP to the bench's host, stepped on the bench's clock.
fn udp_client(b: &mut Bench, opts: ConnectOptions, encryption: CryptoMode) -> NetSession {
    let addr = b.game.session.local_addr().expect("bound");
    let mut client = NetSession::new(
        SessionSettings {
            encryption,
            ..SessionSettings::default()
        },
        b.game.session.manifest().clone(),
    );
    client.set_manual_clock(0);
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()));
    client
        .connect(ConnectTarget::Address(loopback), opts)
        .expect("connect");
    client
}

fn drive(b: &mut Bench, client: &mut NetSession, steps: usize, until: impl Fn(&NetSession) -> bool) {
    for _ in 0..steps {
        if until(client) {
            return;
        }
        client.advance_manual_clock(16);
        client.update();
        b.ticks(1);
        client.update();
        std::thread::yield_now();
    }
}

fn admin_lines(client: &mut NetSession) -> Vec<String> {
    client
        .take_client_events()
        .into_iter()
        .filter_map(|e| match e {
            ClientSessionEvent::AdminOutput(lines) => Some(lines),
            _ => None,
        })
        .flatten()
        .collect()
}

fn in_game(c: &NetSession) -> bool {
    c.state().as_str() == "in_game"
}

// ADR-1901: with the encrypted transport remote admin needs no override, over a sealed link only.
#[test]
fn remote_admin_runs_over_the_encrypted_transport_and_refuses_a_plain_link() {
    let mut b = bench_with(&udp_toml(""), admin_token_file);
    b.host.start(&mut b.game, HostTransport::Udp).expect("start");
    assert!(b.host.remote_admin(), "no dev override needed");
    let key = b.game.session.server_key().expect("a key");

    let mut sealed = udp_client(
        &mut b,
        ConnectOptions {
            server_key: Some(key),
            ..ConnectOptions::default()
        },
        CryptoMode::Required,
    );
    drive(&mut b, &mut sealed, 20_000, in_game);
    assert!(in_game(&sealed), "{:?}", sealed.state());
    assert!(sealed.link_sealed());
    sealed.send_admin(SessionMsg::AdminLogin {
        token: "s3cret".into(),
    });
    drive(&mut b, &mut sealed, 200, |_| false);
    sealed.send_admin(SessionMsg::AdminCommand {
        line: "status".into(),
    });
    drive(&mut b, &mut sealed, 200, |_| false);
    let lines = admin_lines(&mut sealed);
    assert_eq!(
        lines.first().map(String::as_str),
        Some("admin: logged in"),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.starts_with("name=Test")), "{lines:?}");

    // `Auto` leaves this loopback peer plain: the login is refused, without a strike.
    let mut plain = udp_client(&mut b, ConnectOptions::default(), CryptoMode::Auto);
    drive(&mut b, &mut plain, 20_000, in_game);
    assert!(in_game(&plain), "{:?}", plain.state());
    assert!(!plain.link_sealed());
    for _ in 0..5 {
        plain.send_admin(SessionMsg::AdminLogin {
            token: "s3cret".into(),
        });
        drive(&mut b, &mut plain, 100, |_| false);
    }
    let lines = admin_lines(&mut plain);
    assert_eq!(
        lines,
        vec!["remote admin needs an encrypted link".to_owned(); 5],
        "refused every time, and three of them did not disconnect it"
    );
    assert!(in_game(&plain));
}

#[test]
fn encryption_off_keeps_remote_admin_off_and_a_public_bind_refuses_secrets() {
    let local = udp_toml("encryption = \"off\"");
    let mut b = bench_with(&local, admin_token_file);
    b.host.start(&mut b.game, HostTransport::Udp).expect("start");
    assert!(!b.host.remote_admin());
    assert_eq!(b.game.session.server_key(), None);
    assert_eq!(
        b.host.execute(&mut b.game, "serverkey"),
        vec!["server key: none (this server does not encrypt)".to_owned()]
    );

    // A password on a public bind would cross the wire in the clear.
    let public = udp_toml("encryption = \"off\"")
        .replace("bind = \"127.0.0.1\"", "bind = \"0.0.0.0\"")
        .replace("[server]\n", "[server]\npassword = \"pw\"\n");
    let mut b = bench_with(&public, |_| {});
    let err = b
        .host
        .start(&mut b.game, HostTransport::Udp)
        .expect_err("a password needs encryption");
    assert!(
        err.lines.iter().any(|l| l.contains("encrypted link")),
        "{:?}",
        err.lines
    );
}

#[test]
fn the_server_key_survives_a_restart_and_the_file_is_private() {
    let mut first = bench_with(&udp_toml(""), |_| {});
    first
        .host
        .start(&mut first.game, HostTransport::Udp)
        .expect("start");
    let key = first.game.session.server_key().expect("a key");
    let path = first.dir.join("server.key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("key file").permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    let meta = std::fs::metadata(&path).expect("key file");
    assert_eq!(
        regia_net::private_file::exposed_to_other_users(&path, &meta),
        Ok(false),
        "the host writes its key private on every OS"
    );
    let lines = first.host.execute(&mut first.game, "serverkey");
    assert_eq!(lines, vec![format!("server key: {key}")]);
    let saved = std::fs::read_to_string(&path).expect("read");

    let mut second = bench_with(&udp_toml(""), |dir| {
        write_private(&dir.join("server.key"), &saved)
    });
    second
        .host
        .start(&mut second.game, HostTransport::Udp)
        .expect("start");
    assert_eq!(second.game.session.server_key(), Some(key), "pins stay valid");
}

#[cfg(unix)]
#[test]
fn a_key_file_others_can_read_stops_the_start() {
    use std::os::unix::fs::PermissionsExt;
    let mut seed = bench_with(&udp_toml(""), |_| {});
    seed.host
        .start(&mut seed.game, HostTransport::Udp)
        .expect("start");
    let saved = std::fs::read_to_string(seed.dir.join("server.key")).expect("read");
    let mut b = bench_with(&udp_toml(""), |dir| {
        let p = dir.join("server.key");
        write(&p, &saved);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    });
    let err = b
        .host
        .start(&mut b.game, HostTransport::Udp)
        .expect_err("refused");
    assert_eq!(err.exit, ExitCode::Config);
}

#[test]
fn encryption_is_read_from_server_toml_and_a_bad_value_is_a_field_error() {
    let dir = unique_temp_dir("encryption");
    write(&dir.join("server.toml"), &udp_toml("encryption = \"required\""));
    let empty: HashMap<&str, &str> = HashMap::new();
    let env = move |k: &str| empty.get(k).map(|v| (*v).to_owned());
    let r = resolve_config(&dir, &CliOverrides::default(), &env).expect("ok");
    assert_eq!(r.settings.encryption, CryptoMode::Required);
    assert_eq!(r.settings.identity_file, "server.key");
    write(&dir.join("server.toml"), &udp_toml("encryption = \"on\""));
    let errors = resolve_config(&dir, &CliOverrides::default(), &env).expect_err("bad value");
    assert!(errors.iter().any(|e| e.field == "net.encryption"), "{errors:?}");
    let default = resolve_config(
        &{
            let d = unique_temp_dir("encryption-default");
            write(&d.join("server.toml"), TOML);
            d
        },
        &CliOverrides::default(),
        &env,
    )
    .expect("ok");
    assert_eq!(default.settings.encryption, CryptoMode::Auto);
    let _ = std::fs::remove_dir_all(&dir);
}

fn http(addr: std::net::SocketAddr, request: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let _ = s.write_all(request);
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

// 10. Health endpoint.
#[test]
fn health_endpoint() {
    let toml = format!("{TOML}\n[status]\nenabled = true\nbind = \"127.0.0.1\"\nport = 0\n");
    let mut b = bench_with(&toml, |_| {});
    b.host.start(&mut b.game, HostTransport::Local).expect("start");
    b.ticks(120);
    let addr = b.host.health_addr().expect("bound");
    let ok = http(addr, b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
    let body = ok.split("\r\n\r\n").nth(1).expect("body");
    for key in [
        "\"ok\":true",
        "\"state\":\"running\"",
        "\"tick\":",
        "\"players\":0",
        "\"max_players\":8",
        "\"level\":\"levels/a.level.ron\"",
        "\"uptime_s\":",
        "\"tick_ms_p99\":",
        "\"overruns_last_min\":0",
    ] {
        assert!(body.contains(key), "{key} in {body}");
    }
    assert!(http(addr, b"GET /nope HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 404"));
    assert!(http(addr, b"POST /health HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 405"));
    let mut big = b"GET /health HTTP/1.1\r\nX-Pad: ".to_vec();
    big.extend(std::iter::repeat_n(b'a', 2048));
    big.extend_from_slice(b"\r\n\r\n");
    assert!(http(addr, &big).starts_with("HTTP/1.1 431"));
    b.game
        .session
        .add_simulated_clients(1, SimInput::Idle)
        .expect("sim");
    b.ticks(60);
    b.host.execute(&mut b.game, "quit 30");
    let draining = http(addr, b"GET /health HTTP/1.1\r\n\r\n");
    assert!(draining.starts_with("HTTP/1.1 503"), "{draining}");
    assert!(draining.contains("\"state\":\"draining\""));
}

// 11. Exit codes: bad config 2, port in use 3, missing level 4.
#[test]
fn exit_codes() {
    let dir = unique_temp_dir("exit");
    write(
        &dir.join("server.toml"),
        &TOML.replace("tick_hz = 60", "tick_hz = 1000"),
    );
    let e = ServerHost::new(
        &dir,
        CliOverrides::default(),
        no_env(),
        Box::new(TestClock::new()),
    )
    .expect_err("bad");
    assert_eq!(e.exit.code(), 2);

    let taken = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    let port = taken.local_addr().expect("addr").port();
    let mut b = bench_with(
        &TOML.replace("port = 7790", &format!("port = {port}\nbind = \"127.0.0.1\"")),
        |_| {},
    );
    let e = b.host.start(&mut b.game, HostTransport::Udp).expect_err("in use");
    assert_eq!(e.exit.code(), 3);

    // The status (health) port in use is a bind failure too (spec 76 § 5, audit ADR-5650).
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tcp");
    let status_port = held.local_addr().expect("addr").port();
    let mut b = bench_with(
        &format!("{TOML}\n[status]\nenabled = true\nbind = \"127.0.0.1\"\nport = {status_port}\n"),
        |_| {},
    );
    let e = b
        .host
        .start(&mut b.game, HostTransport::Local)
        .expect_err("status port in use");
    assert_eq!(e.exit.code(), 3);
    assert!(e.lines[0].contains("status.port"), "{:?}", e.lines);
    drop(held);

    // Exit 4 is the starting row. A later missing row is skipped when the host advances.
    let mut b = bench_with(TOML, |_| {});
    b.game.levels = vec!["levels/b.level.ron".into()];
    let e = b
        .host
        .start(&mut b.game, HostTransport::Local)
        .expect_err("missing level");
    assert_eq!(e.exit.code(), 4);
    assert!(e.lines[0].contains("levels/a.level.ron"), "{:?}", e.lines);
    let _ = std::fs::remove_dir_all(&dir);
}

// 12. Pacing through the injectable clock: one tick per step, overruns counted.
#[test]
fn pacing_with_a_test_clock() {
    let mut b = bench();
    let start = b.clock.now();
    let tick0 = b.host.tick();
    b.ticks(600);
    assert_eq!(b.host.tick() - tick0, 600);
    assert_eq!(b.host.stats().overruns_total, 0);
    let elapsed = b.clock.now() - start;
    let step = Duration::from_secs_f64(1.0 / 60.0);
    assert!(elapsed >= step * 599 && elapsed <= step * 601, "{elapsed:?}");

    // One slow tick (1.5 steps) is one overrun.
    let clock = b.clock.clone();
    let mut slow_once = Some(());
    b.game.on_step = Some(Box::new(move |_| {
        if slow_once.take().is_some() {
            clock.advance(step.mul_f64(1.5));
        }
    }));
    b.ticks(10);
    assert_eq!(b.host.stats().overruns_total, 1);
    assert!(b.host.stats().worst_ms >= 24.0);
}

// Idle slowdown: an empty server that opts in ticks at idle_tick_hz; a join restores tick_hz.
#[test]
fn idle_slowdown_when_the_game_allows_it() {
    let mut b = bench();
    b.game.idle_slowdown = true;
    let t0 = b.clock.now();
    b.ticks(10);
    let idle = b.clock.now() - t0;
    assert!(
        idle >= Duration::from_millis(900),
        "10 idle ticks at 10 Hz: {idle:?}"
    );
    assert_eq!(b.game.tick_hz, 10, "the game's fixed tick follows the host");
    b.game
        .session
        .add_simulated_clients(1, SimInput::Idle)
        .expect("sim");
    b.ticks(30);
    let t1 = b.clock.now();
    b.ticks(60);
    let busy = b.clock.now() - t1;
    assert!(busy <= Duration::from_millis(1100), "60 ticks at 60 Hz: {busy:?}");
    assert_eq!(b.game.tick_hz, 60);
    assert!(
        b.game
            .session
            .take_server_events()
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerJoined { .. }))
    );
}

/// Audit L3 (zai): `server.toml` `tick_hz` reaches the app's fixed clock, which is what the net
/// runtime counts seconds with.
#[test]
fn an_app_game_ticks_at_the_host_rate() {
    let mut game = regia_net::server_host::AppHostGame::new(regia_app::App::new());
    assert_eq!(game.app.world().resource::<regia_app::FixedTime>().hz, 60.0);
    regia_net::server_host::HostGame::set_tick_hz(&mut game, 30);
    assert_eq!(regia_net::runtime::tick_hz(game.app.world()).round(), 30.0);
}

/// An idle slowdown changes the host rate under a fixed-step app: the frame delta must follow the
/// new step, or the simulation clock runs at a fraction of the pacing.
#[test]
fn a_rate_change_moves_the_fixed_step_frame_delta_with_it() {
    use regia_app::{TimeUpdateStrategy, enable_fixed_step};
    use std::time::Duration;
    let mut app = regia_app::App::new();
    enable_fixed_step(&mut app, 60.0);
    let mut game = regia_net::server_host::AppHostGame::new(app);
    regia_net::server_host::HostGame::set_tick_hz(&mut game, 10);
    let TimeUpdateStrategy::ManualDuration(delta) = *game.app.world().resource::<TimeUpdateStrategy>() else {
        panic!("a fixed-step app keeps a manual delta");
    };
    assert_eq!(delta, Duration::from_millis(100));
    regia_net::server_host::HostGame::set_tick_hz(&mut game, 60);
    let TimeUpdateStrategy::ManualDuration(delta) = *game.app.world().resource::<TimeUpdateStrategy>() else {
        panic!("manual");
    };
    assert_eq!(delta, Duration::from_secs_f64(1.0 / 60.0));
}

/// Spec 76 § 4 `[net]` (audit 2026-11-10, ADR-5650): the rates in `server.toml` reach the game's
/// replication settings at start and after a reload. Before, they reached only the session
/// handshake: `server_rate_bytes` capped nothing and clients were told a `send_every_ticks`
/// the server did not send at.
#[test]
fn server_toml_net_rates_reach_the_replication_settings() {
    let rates = |game: &regia_net::server_host::AppHostGame| {
        let r = &game.app.world().resource::<regia_net::NetSettings>().replication;
        (r.client_rate_bytes, r.server_rate_bytes, r.send_every_ticks)
    };
    let dir = unique_temp_dir("net-rates");
    let net = "[net]\nclient_rate_bytes = 20000\nserver_rate_bytes = 150000\nsend_every_ticks = 3\n";
    write(&dir.join("server.toml"), &format!("{TOML}{net}"));
    let mut app = regia_app::App::new();
    app.add_plugins(regia_net::NetPlugin {
        mode: regia_net::NetMode::DedicatedServer,
    });
    let mut game = regia_net::server_host::AppHostGame::new(app);
    assert_eq!(rates(&game), (49_152, 0, 2), "the replication defaults");
    let mut host = ServerHost::new(
        &dir,
        CliOverrides::default(),
        no_env(),
        Box::new(TestClock::new()),
    )
    .expect("config");
    host.start(&mut game, HostTransport::Local).expect("start");
    assert_eq!(rates(&game), (20_000, 150_000, 3));
    let welcome = regia_net::server_host::HostGame::session(&mut game)
        .server()
        .expect("server")
        .options()
        .send_every_ticks;
    assert_eq!(welcome, 3, "clients are told the rate the server sends at");

    write(
        &dir.join("server.toml"),
        &format!("{TOML}[net]\nclient_rate_bytes = 30000\nserver_rate_bytes = 0\nsend_every_ticks = 1\n"),
    );
    let report = host.reload(&mut game);
    assert!(report.needs_restart.is_empty(), "{:?}", report.needs_restart);
    assert_eq!(rates(&game), (30_000, 0, 1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 76 § 4 (audit 2026-11-10, ADR-5650): `--upgrade-config` rewrites an old `server.toml` to the
/// current version, keeping comments and settings; a current file is left alone; a bad version
/// or a missing file is an error naming the file.
#[test]
fn upgrade_config_rewrites_an_old_file_and_keeps_its_comments() {
    use regia_net::server_host::config::{ConfigUpgrade, SERVER_TOML_FORMAT_VERSION, upgrade_config_file};
    let dir = unique_temp_dir("upgrade");
    let v1 = "# my server\nformat_version = 1\n[server]\nname = \"Old\" # keep me\nport = 7790\n\
              [levels]\nrotation = [\"levels/a.level.ron\"]\n";
    write(&dir.join("server.toml"), v1);
    let cli = args(&["--upgrade-config"]);
    assert!(cli.upgrade_config);
    match upgrade_config_file(&dir, &cli).expect("upgrade") {
        ConfigUpgrade::Upgraded { from, .. } => assert_eq!(from, 1),
        other => panic!("not upgraded: {other:?}"),
    }
    let text = std::fs::read_to_string(dir.join("server.toml")).expect("read");
    assert!(
        text.contains("# my server") && text.contains("# keep me"),
        "{text}"
    );
    assert!(
        text.contains(&format!("format_version = {SERVER_TOML_FORMAT_VERSION}")),
        "{text}"
    );
    let host = ServerHost::new(
        &dir,
        CliOverrides::default(),
        no_env(),
        Box::new(TestClock::new()),
    )
    .expect("the upgraded file loads");
    assert_eq!(
        (host.settings().name.as_str(), host.settings().port),
        ("Old", 7790)
    );
    assert!(matches!(
        upgrade_config_file(&dir, &cli),
        Ok(ConfigUpgrade::Current { .. })
    ));
    write(&dir.join("other.toml"), "format_version = 99\n");
    let err = upgrade_config_file(&dir, &args(&["--config", "other.toml", "--upgrade-config"]))
        .expect_err("no migrator from v99");
    assert!(err.contains("other.toml"), "{err}");
    let empty = unique_temp_dir("upgrade-none");
    assert!(upgrade_config_file(&empty, &cli).is_err(), "no file");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&empty);
}

/// The peers in game now that are not in `known`.
fn new_in_game(b: &Bench, known: &[regia_net::model::PeerId]) -> Vec<regia_net::model::PeerId> {
    let server = b.game.session.server().expect("server");
    server
        .peer_ids()
        .into_iter()
        .filter(|p| !known.contains(p) && server.peer_state(*p) == Some(PeerState::InGame))
        .collect()
}

/// Moves the host's test clock (which drives the session clock) until the session's unix clock
/// reads at least `unix_s`.
fn advance_session_to(b: &mut Bench, unix_s: u64) {
    while b.game.session.now_unix_s() < unix_s {
        let behind = unix_s - b.game.session.now_unix_s();
        b.clock.advance(Duration::from_millis(behind * 1000 - 500));
        b.ticks(1);
    }
}

/// fixrev6 (review sprint-rev-claude-13-k finding 5, ADR-5973): the console's `ban <id> 60s`
/// stamps the entry's expiry on the session clock (`NetSession::now_unix_s`), the clock the
/// session's join check reads, and `bans` lists by it too. With the session clock an hour off the
/// system clock (an NTP step on a long-running host, a replay base), the address is refused at
/// 59 s and let back in at 61 s of the session clock. Before the fix the expiry was the system
/// clock + 60: ahead, the ban was over before it began; behind, it held for an extra hour.
fn console_ban_ends_on_the_session_clock(ahead: bool) {
    let mut b = bench();
    let wall = regia_net::session::unix_now_s();
    let base = if ahead { wall + 3600 } else { wall - 3600 };
    b.game.session.set_unix_base_s(base);
    b.game
        .session
        .add_simulated_clients(2, SimInput::Idle)
        .expect("sims");
    b.ticks(60);
    assert_eq!(b.in_game(), 2);
    let console = b.host.console_sender();
    let run = |b: &mut Bench, line: &str| -> Vec<String> {
        console.send(line.to_owned()).expect("send");
        b.ticks(1);
        b.host.take_console_output()
    };
    let before = b.game.session.now_unix_s();
    let out = run(&mut b, "ban 1 60s spam");
    let after = b.game.session.now_unix_s();
    assert!(out[0].contains("banned 1 (entry 1)"), "{out:?}");
    b.ticks(10);
    let until = b
        .game
        .session
        .server()
        .expect("server")
        .bans()
        .list()
        .entries
        .first()
        .and_then(|e| e.until)
        .expect("the 60 s ban is in the list right after the console issued it");

    advance_session_to(&mut b, until - 1);
    let known = b.game.session.server().expect("server").peer_ids();
    b.game
        .session
        .add_simulated_clients(1, SimInput::Idle)
        .expect("sim");
    b.ticks(30);
    assert!(
        new_in_game(&b, &known).is_empty(),
        "59 s into a 60 s ban (session clock): the address is still refused"
    );
    let bans = run(&mut b, "bans");
    assert!(bans[0].starts_with("1 address 127.0.0.1"), "{bans:?}");

    advance_session_to(&mut b, until + 1);
    let known = b.game.session.server().expect("server").peer_ids();
    b.game
        .session
        .add_simulated_clients(1, SimInput::Idle)
        .expect("sim");
    b.ticks(30);
    assert!(
        !new_in_game(&b, &known).is_empty(),
        "61 s into a 60 s ban (session clock): the address may join"
    );
    assert_eq!(run(&mut b, "bans"), vec!["no bans".to_owned()]);
    assert!(
        (before + 60..=after + 60).contains(&until),
        "the expiry {until} is the session clock ({before}..={after}) + 60"
    );
}

#[test]
fn console_ban_ends_on_the_session_clock_when_it_runs_ahead_of_the_wall() {
    console_ban_ends_on_the_session_clock(true);
}

#[test]
fn console_ban_ends_on_the_session_clock_when_it_runs_behind_the_wall() {
    console_ban_ends_on_the_session_clock(false);
}
