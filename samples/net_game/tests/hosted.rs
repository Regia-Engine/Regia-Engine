// SPDX-License-Identifier: MIT-0
//! Net Game under the dedicated-server host (ADR-2440): the same `ServerHost` that `regia server`
//! and a packaged server run, with the game's `HostedLink`, a relay inside the host, and two
//! in-process clients that join by join code on 127.0.0.1, play, and leave.
//!
//! Time is the host's test clock (one pacing iteration = one tick) and the links' manual clock
//! (one app update = 16 667 us); every port is picked by the OS. Nothing sleeps, so every tick of
//! the transcript repeats from run to run.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use net_game::{Bot, NetGameWire, PadSource, Side, avatars, headless_app, link_options};
use regia::net::ConditionerConfig;
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::server_host::{
    AppHostGame, CliOverrides, ExitCode, HostState, HostTransport, HostedLink, JoinResolver, JoinTicket,
    ServerHost, TestClock,
};
use regia::prelude::*;

/// Ticks the bots play.
const PLAY: u64 = 600;

fn manual() -> LinkClock {
    LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    }
}

/// A fresh directory under the system temp dir: process id plus a counter, never reused.
fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("net_game_hosted_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

const SERVER_TOML: &str = r#"
format_version = 1
[server]
name = "Hosted Net Game"
bind = "127.0.0.1"
port = 7777
max_players = 8
tick_hz = 60
[levels]
rotation = ["levels/arena.level.ron"]
[net]
encryption = "required"
identity_file = "server.key"
[status]
enabled = false
[logs]
dir = "logs"
"#;

/// Set by the stop hook.
#[derive(Resource)]
struct Stopped;

fn mark_stopped(world: &mut World) {
    world.insert_resource(Stopped);
}

/// What happened when, by host tick.
#[derive(Debug, Clone, PartialEq)]
struct Transcript {
    ticks: PhaseTicks,
    server_avatars: Vec<(u32, [u32; 3], u32, u32)>,
    sealed: Vec<bool>,
}

/// The host tick at which each phase ended. Every phase waits for loopback UDP datagrams (the
/// relay's reply, the lookups, the handshake, the goodbyes), so these count how many ticks the
/// OS took to deliver them: Linux hands a loopback datagram to the receiving socket inside
/// `sendto`, so they repeat exactly there; macOS queues it to an input thread, and the M5 saw the
/// same session's join code on tick 3 in one run and tick 5 in the next (ADR-7386).
#[derive(Debug, Clone, PartialEq)]
struct PhaseTicks {
    join_code: u64,
    resolved: u64,
    joined: u64,
    left: u64,
    stopped: u64,
}

struct Hosted {
    dir: PathBuf,
    host: ServerHost,
    game: AppHostGame,
    clock: TestClock,
}

impl Hosted {
    fn start() -> Self {
        let dir = temp_dir("srv");
        write(&dir.join("server.toml"), SERVER_TOML);
        let clock = TestClock::new();
        let cli = CliOverrides {
            relay: Some("local:0".into()),
            ..CliOverrides::default()
        };
        let mut host =
            ServerHost::new(&dir, cli, Box::new(|_| None), Box::new(clock.clone())).expect("config");
        let mut app = headless_app(Side::Server);
        app.insert_resource(
            HostedLink::new::<NetGameWire>(link_options(manual(), ConditionerConfig::default()))
                .with_on_stop(mark_stopped),
        );
        let mut game = AppHostGame::new(app);
        host.start(&mut game, HostTransport::UdpAnyPort).expect("start");
        assert_eq!(host.state(), HostState::Running);
        assert!(host.game_owns_port(), "the game's link binds the game port");
        let addr = host.game_addr().expect("bound");
        assert_ne!(addr.port(), 0);
        assert_eq!(
            game.app.world().resource::<NetLink>().local_addr(),
            Some(addr),
            "the port the host reports is the link's"
        );
        Self {
            dir,
            host,
            game,
            clock,
        }
    }

    fn tick(&mut self) {
        assert_eq!(self.host.run_once(&mut self.game), None, "the host keeps running");
    }

    fn now_ms(&self) -> u64 {
        use regia::net::server_host::HostClock;
        self.clock.now().as_millis() as u64
    }

    fn players(&mut self) -> usize {
        let json = self.host.health_json(&mut self.game);
        let after = json.split("\"players\":").nth(1).expect("players field");
        after
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a number")
    }
}

fn client(ticket: &JoinTicket, i: u32) -> App {
    let mut app = headless_app(Side::Client);
    app.insert_resource(PadSource::Bot(Bot::new(
        std::f32::consts::PI * i as f32,
        30,
        PLAY as u32,
    )));
    let opts = LinkOptions {
        server_key: ticket.server_key,
        name: format!("bot{i}"),
        ..link_options(manual(), ConditionerConfig::default())
    };
    net_game::connect_udp(&mut app, ticket.addr, &opts).expect("connect");
    app
}

fn bits(v: f32) -> u32 {
    v.to_bits()
}

/// One whole session; returns its transcript.
fn session() -> Transcript {
    let mut h = Hosted::start();

    // The host registers the game port's ticket with its relay and learns a join code.
    let mut tick = 0;
    while h.host.join_code().is_none() {
        assert!(tick < 30, "a join code within 30 ticks");
        h.tick();
        tick += 1;
    }
    let join_code_tick = h.host.tick();
    let code = h.host.join_code().expect("code").to_owned();
    let relay = h.host.relay_addr().expect("relay");
    assert!(relay.ip().is_loopback(), "the local relay listens on 127.0.0.1");
    let console: Vec<String> = h.host.take_console_output();
    assert!(
        console
            .iter()
            .any(|l| l.starts_with("listening ") && l.contains(" key ")),
        "{console:?}"
    );
    assert!(
        console
            .iter()
            .any(|l| *l == format!("join code {code} relay {relay}")),
        "{console:?}"
    );

    // Each client resolves the code on the relay (looked up on the host's clock).
    let mut resolvers: Vec<JoinResolver> = (0..2)
        .map(|_| JoinResolver::udp(relay, &code.to_lowercase()).expect("resolver"))
        .collect();
    let mut tickets: Vec<Option<JoinTicket>> = vec![None, None];
    while tickets.iter().any(Option::is_none) {
        assert!(h.host.tick() < join_code_tick + 30, "resolved within 30 ticks");
        let now = h.now_ms();
        for (r, t) in resolvers.iter_mut().zip(tickets.iter_mut()) {
            if t.is_none()
                && let Some(done) = r.update(now)
            {
                *t = Some(done.expect("the code resolves"));
            }
        }
        h.tick();
    }
    let resolved_tick = h.host.tick();
    let tickets: Vec<JoinTicket> = tickets.into_iter().map(|t| t.expect("ticket")).collect();
    let addr: SocketAddr = h.host.game_addr().expect("bound");
    for t in &tickets {
        assert_eq!(t.addr, addr, "the ticket names the game port");
        assert_eq!(
            t.server_key,
            h.game.app.world().resource::<NetLink>().server_key(),
            "the ticket carries the key to pin"
        );
    }

    // Join: both clients connect to the hosted link.
    let mut clients: Vec<App> = tickets.iter().zip(0u32..).map(|(t, i)| client(t, i)).collect();
    let mut steps = 0;
    while h.players() < 2
        || clients
            .iter()
            .any(|c| !c.world().resource::<NetLink>().is_connected())
    {
        assert!(steps < 300, "both joined within 300 ticks");
        h.tick();
        for c in &mut clients {
            c.update();
        }
        steps += 1;
    }
    let joined_tick = h.host.tick();
    let players = h.host.execute(&mut h.game, "players");
    assert_eq!(players.len(), 2, "{players:?}");
    assert!(players.iter().all(|l| l.ends_with(" game-port")), "{players:?}");
    let status = h.host.execute(&mut h.game, "status");
    assert!(status[0].contains("players=2/8"), "{status:?}");

    // Play: the bots run and shoot for PLAY ticks, then stand still while the state settles.
    for _ in 0..PLAY + 120 {
        h.tick();
        for c in &mut clients {
            c.update();
        }
    }
    let server_view = avatars(h.game.app.world_mut());
    assert_eq!(server_view.len(), 2, "one avatar per player");
    for c in &mut clients {
        let seen = avatars(c.world_mut());
        assert_eq!(seen.len(), 2, "every client sees both avatars");
        for (a, b) in seen.iter().zip(&server_view) {
            assert_eq!(a.slot, b.slot);
            assert!(
                a.position.distance(b.position) < 1e-3,
                "client and server agree: {a:?} vs {b:?}"
            );
            assert_eq!(a.score, b.score);
        }
    }
    let sealed: Vec<bool> = clients
        .iter()
        .map(|c| c.world().resource::<NetLink>().is_sealed())
        .collect();
    assert!(
        sealed.iter().all(|s| *s),
        "encryption required: every link is sealed"
    );
    let server_avatars = server_view
        .iter()
        .map(|a| {
            (
                a.slot,
                [bits(a.position.x), bits(a.position.y), bits(a.position.z)],
                a.score.tags,
                a.score.tagged,
            )
        })
        .collect();

    // Leave: each client says goodbye; the server drops both avatars without waiting for a timeout.
    for c in &mut clients {
        c.world_mut().resource_mut::<NetLink>().close();
        c.update();
    }
    let mut steps = 0;
    while h.players() > 0 || !avatars(h.game.app.world_mut()).is_empty() {
        assert!(steps < 30, "both left within 30 ticks (a goodbye, not a timeout)");
        h.tick();
        steps += 1;
    }
    let left_tick = h.host.tick();

    // Stop: a shutdown request drains (the link closes) and stops; the game's hook runs.
    h.host.signals().request_shutdown();
    let mut steps = 0;
    let code = loop {
        assert!(steps < 30, "stopped within 30 ticks");
        if let Some(code) = h.host.run_once(&mut h.game) {
            break code;
        }
        steps += 1;
    };
    assert_eq!(code, ExitCode::Ok);
    assert_eq!(h.host.state(), HostState::Stopped);
    assert!(
        h.game.app.world().contains_resource::<Stopped>(),
        "the stop hook ran"
    );
    assert_eq!(
        h.game.app.world().resource::<NetLink>().role(),
        "idle",
        "the link closed"
    );
    let stopped_tick = h.host.tick();
    let _ = std::fs::remove_dir_all(&h.dir);
    Transcript {
        ticks: PhaseTicks {
            join_code: join_code_tick,
            resolved: resolved_tick,
            joined: joined_tick,
            left: left_tick,
            stopped: stopped_tick,
        },
        server_avatars,
        sealed,
    }
}

#[test]
fn two_clients_join_by_code_play_and_leave_under_the_host() {
    let first = session();
    let second = session();
    // The match is the game's own: the same bot inputs over the same game ticks, on every host.
    assert_eq!(
        (&first.server_avatars, &first.sealed),
        (&second.server_avatars, &second.sealed),
        "the same match on every run"
    );
    // The phase ticks repeat where loopback delivery is synchronous (`PhaseTicks`); elsewhere
    // each phase is still bounded by the waits in `session`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert_eq!(first.ticks, second.ticks, "the same ticks on every run");
}
