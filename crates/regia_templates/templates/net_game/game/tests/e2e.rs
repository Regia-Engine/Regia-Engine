// SPDX-License-Identifier: MIT-0
//! The template networked game end to end (gap audit F3, ADR-2039): a dedicated server and two
//! clients, each a whole app, over encrypted links with the conditioner on, play a scripted minute
//! (3600 ticks at 60 Hz) and must agree afterwards.
//!
//! - `a_scripted_minute_over_encrypted_udp_agrees`: real UDP sockets on 127.0.0.1, encryption
//!   required, server key pinned, a lossy conditioner on every endpoint.
//! - `the_in_memory_minute_replays_bit_for_bit`: the same match over the in-memory datagram
//!   network, twice; every number must repeat.
//! - `a_lobby_launch_brings_both_clients_to_the_server`: the lobby flow (spec 89) on the local
//!   backend: create, join, ready, launch, auto-connect.
//! - `a_player_who_leaves_loses_the_avatar_everywhere`: a client goes silent; after the server's
//!   silence timeout and resume grace its avatar is gone on the server and on the other client.
//!
//! Time is the links' manual clock (one app update = one tick = 16 667 us); nothing sleeps.

use std::net::SocketAddr;
use std::sync::Arc;

use {{crate_name}}::lobby::{LobbyLinkOptions, LobbySteamTransport, NetGameLobbyPlugin, launch_when_ready};
use {{crate_name}}::{
    AvatarView, Bot, NetGameWire, PREDICTION_TOLERANCE_M, PadSource, ShotLog, Side, avatars, headless_app,
    link_options,
};
use regia::app::App;
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::lobby::{LobbyServer, LobbyTransport};
use regia::net::predict::ClientPrediction;
use regia::net::{ConditionerConfig, Lobbies, LocalLobbyHub, LoopbackNet};

const MINUTE: u32 = 3600;
const SETTLE: u32 = 300;

fn manual() -> LinkClock {
    LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    }
}

/// Lossy, jittery, reordering links: 25 ms +- 10 ms one way, 2 % loss, 1 % duplicates.
fn lossy(seed: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss: 0.02,
        duplicate: 0.01,
        reorder: 0.02,
        reorder_max_ms: 20,
        delay_ms: 25,
        jitter_ms: 10,
        ..ConditionerConfig::default()
    }
}

enum Transport {
    Udp,
    Memory(LoopbackNet),
}

struct Match {
    server: App,
    clients: Vec<App>,
}

impl Match {
    /// A server and two clients. Each client's outbound traffic goes through its own seeded
    /// conditioner; `server_conditioner` shapes the server's.
    fn new(transport: &Transport, server_conditioner: ConditionerConfig) -> Self {
        let mut server = headless_app(Side::Server);
        let opts = link_options(manual(), server_conditioner);
        let server_addr = match transport {
            Transport::Udp => {
                {{crate_name}}::host_udp(&mut server, SocketAddr::from(([127, 0, 0, 1], 0)), &opts).expect("bind");
                server.world().resource::<NetLink>().local_addr().expect("bound")
            }
            Transport::Memory(net) => {
                let addr = SocketAddr::from(([127, 0, 0, 1], 7777));
                let socket = net.bind(addr).expect("bind");
                server
                    .world_mut()
                    .resource_mut::<NetLink>()
                    .host::<NetGameWire>(Box::new(socket), &opts)
                    .expect("host");
                addr
            }
        };
        let key = server.world().resource::<NetLink>().server_key();
        assert!(key.is_some(), "the server has a key to pin");
        let mut clients = Vec::new();
        for i in 0..2u64 {
            let mut app = headless_app(Side::Client);
            let opts = LinkOptions {
                server_key: key,
                name: format!("bot{i}"),
                ..link_options(manual(), lossy(100 + i))
            };
            match transport {
                Transport::Udp => {{crate_name}}::connect_udp(&mut app, server_addr, &opts).expect("connect"),
                Transport::Memory(net) => {
                    let socket = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
                    app.world_mut()
                        .resource_mut::<NetLink>()
                        .connect::<NetGameWire>(Box::new(socket), server_addr, &opts)
                        .expect("connect");
                }
            }
            clients.push(app);
        }
        Self { server, clients }
    }

    fn step(&mut self) {
        self.server.update();
        for c in &mut self.clients {
            c.update();
        }
    }

    /// Steps until `done`, at most `limit` steps; returns the steps taken.
    fn step_until(&mut self, limit: u32, done: impl Fn(&mut Self) -> bool) -> u32 {
        for n in 0..limit {
            if done(self) {
                return n;
            }
            self.step();
        }
        panic!("not reached in {limit} steps");
    }
}

/// What one match produced, compared across processes and across runs.
#[derive(Debug, Clone, PartialEq)]
struct Outcome {
    server: Vec<AvatarView>,
    clients: Vec<Vec<AvatarView>>,
    shots: ShotLog,
    corrections: Vec<u64>,
    resyncs: Vec<u64>,
    verify_failures: Vec<u64>,
    sealed: Vec<bool>,
    dropped_by_conditioner: u64,
}

fn play_minute(transport: &Transport, server_conditioner: ConditionerConfig) -> Outcome {
    let mut m = Match::new(transport, server_conditioner);
    // Join: both clients connected and each sees both avatars.
    m.step_until(600, |m| {
        m.clients
            .iter_mut()
            .all(|c| c.world().resource::<NetLink>().is_connected() && avatars(c.world_mut()).len() == 2)
    });
    // The scripted minute: two bots circling in opposite phase, each firing at the other.
    m.clients[0].insert_resource(PadSource::Bot(Bot::new(0.0, 20, MINUTE)));
    m.clients[1].insert_resource(PadSource::Bot(Bot::new(std::f32::consts::PI, 25, MINUTE)));
    for _ in 0..MINUTE + SETTLE {
        m.step();
    }
    let server = avatars(m.server.world_mut());
    let mut clients = Vec::new();
    let mut corrections = Vec::new();
    let mut resyncs = Vec::new();
    let mut verify_failures = Vec::new();
    let mut sealed = Vec::new();
    let dropped = |app: &App| {
        app.world()
            .resource::<NetLink>()
            .status()
            .map_or(0, |s| s.conditioner_stats.dropped)
    };
    let mut dropped_by_conditioner = dropped(&m.server);
    for c in &mut m.clients {
        dropped_by_conditioner += dropped(c);
        clients.push(avatars(c.world_mut()));
        let stats = &c.world().resource::<ClientPrediction>().stats;
        corrections.push(stats.corrections);
        resyncs.push(stats.resyncs);
        verify_failures.push(stats.verify_failures);
        sealed.push(c.world().resource::<NetLink>().is_sealed());
    }
    Outcome {
        server,
        clients,
        shots: m.server.world().resource::<ShotLog>().clone(),
        corrections,
        resyncs,
        verify_failures,
        sealed,
        dropped_by_conditioner,
    }
}

fn assert_agreement(o: &Outcome) {
    assert_eq!(o.server.len(), 2, "two avatars on the server: {o:?}");
    for (i, view) in o.clients.iter().enumerate() {
        assert_eq!(view.len(), 2, "client {i} sees both avatars");
        for (s, c) in o.server.iter().zip(view) {
            assert_eq!(s.slot, c.slot);
            assert_eq!(s.score, c.score, "client {i} agrees on slot {} score", s.slot);
            let gap = s.position.distance(c.position);
            assert!(
                gap <= PREDICTION_TOLERANCE_M * 1.001,
                "client {i} slot {} is {gap} m from the server",
                s.slot
            );
        }
    }
}

fn assert_play(o: &Outcome) {
    // Bot 0 fires every 20 ticks, bot 1 every 25: 180 + 144 commands in the minute, some lost to
    // the conditioner before the redundant input stream recovers them.
    assert!(o.shots.shots >= 300, "shots judged: {:?}", o.shots);
    assert_eq!(
        o.shots.rewound, o.shots.shots,
        "every shot judged at the shooter's view tick"
    );
    // Lag compensation: shots at the shown position of a moving target hit.
    assert!(
        o.shots.hits * 10 >= o.shots.shots * 9,
        "with rewind at least 90 % hit: {:?}",
        o.shots
    );
    let total_tags: u32 = o.server.iter().map(|a| a.score.tags).sum();
    let total_tagged: u32 = o.server.iter().map(|a| a.score.tagged).sum();
    assert_eq!(total_tags, o.shots.hits);
    assert_eq!(total_tagged, o.shots.hits);
    // No desync: no failed verification, and corrections stay rare: the client runs the server's
    // own movement code, so only a command that reached the server too late can make it wrong. A
    // lead resync moves the client's clock, not its state; jitter can call for one or two.
    assert_eq!(o.verify_failures, vec![0, 0], "{o:?}");
    for c in &o.corrections {
        assert!(*c <= 20, "corrections {:?}", o.corrections);
    }
    for r in &o.resyncs {
        assert!(*r <= 2, "lead resyncs {:?}", o.resyncs);
    }
}

#[test]
fn a_scripted_minute_over_encrypted_udp_agrees() {
    let o = play_minute(&Transport::Udp, lossy(11));
    println!("udp outcome: {o:?}");
    assert_eq!(o.sealed, vec![true, true], "both links sealed");
    assert!(o.dropped_by_conditioner > 0, "the conditioner dropped traffic");
    assert_agreement(&o);
    assert_play(&o);
}

#[test]
fn the_in_memory_minute_replays_bit_for_bit() {
    // The server sends to its peers in hash-map order, which differs between two runs in one
    // process; with one conditioner per client the random draws do not depend on that order.
    let first = play_minute(
        &Transport::Memory(LoopbackNet::new()),
        ConditionerConfig::default(),
    );
    println!("memory outcome: {first:?}");
    assert!(
        first.dropped_by_conditioner > 0,
        "the clients' conditioners dropped traffic"
    );
    assert_agreement(&first);
    assert_play(&first);
    let second = play_minute(
        &Transport::Memory(LoopbackNet::new()),
        ConditionerConfig::default(),
    );
    assert_eq!(first, second, "the same seeds give the same match");
}

#[test]
fn a_lobby_launch_brings_both_clients_to_the_server() {
    let mut server = headless_app(Side::Server);
    let opts = link_options(manual(), ConditionerConfig::default());
    {{crate_name}}::host_udp(&mut server, SocketAddr::from(([127, 0, 0, 1], 0)), &opts).expect("bind");
    let server_addr = server.world().resource::<NetLink>().local_addr().expect("bound");
    let key = server.world().resource::<NetLink>().server_key();

    let hub = LocalLobbyHub::new();
    let mut clients: Vec<App> = (0..2u64)
        .map(|i| {
            let mut app = headless_app(Side::Client);
            app.insert_resource(Lobbies::new(Arc::new(hub.member(i + 1, &format!("bot{i}")))));
            app.insert_resource(LobbyLinkOptions(LinkOptions {
                server_key: key,
                ..opts.clone()
            }));
            app.add_plugins(NetGameLobbyPlugin);
            app
        })
        .collect();
    let step = |server: &mut App, clients: &mut Vec<App>| {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
    };
    // Bot 0 creates the lobby; bot 1 lists and joins it.
    clients[0]
        .world_mut()
        .resource_mut::<Lobbies>()
        .create(regia::net::lobby::LobbySpec::default())
        .expect("create");
    for _ in 0..5 {
        step(&mut server, &mut clients);
    }
    let lobby = clients[0]
        .world()
        .resource::<Lobbies>()
        .current()
        .expect("created");
    clients[1]
        .world_mut()
        .resource_mut::<Lobbies>()
        .join(lobby)
        .expect("join");
    for _ in 0..5 {
        step(&mut server, &mut clients);
    }
    // Nobody is ready: no launch.
    assert!(
        !launch_when_ready(&mut clients[0].world_mut().resource_mut::<Lobbies>(), server_addr)
            .expect("owner")
    );
    for c in &mut clients {
        c.world_mut()
            .resource_mut::<Lobbies>()
            .set_ready(true)
            .expect("ready");
    }
    for _ in 0..5 {
        step(&mut server, &mut clients);
    }
    assert!(
        launch_when_ready(&mut clients[0].world_mut().resource_mut::<Lobbies>(), server_addr).expect("owner"),
        "everyone ready: the owner launches"
    );
    for _ in 0..120 {
        step(&mut server, &mut clients);
    }
    for (i, c) in clients.iter_mut().enumerate() {
        assert!(
            c.world().resource::<NetLink>().is_connected(),
            "client {i} followed the lobby"
        );
        assert_eq!(avatars(c.world_mut()).len(), 2, "client {i} sees both avatars");
    }
    assert_eq!(avatars(server.world_mut()).len(), 2);
}

/// Stands in for `regia::steam::SteamLobbyTransport`: a Steam id reaches the server bound at a
/// fixed in-memory address, so the Steam branch of `follow_lobby` runs without the SDK.
struct StandInSteam {
    net: LoopbackNet,
    server: SocketAddr,
}

impl LobbyTransport for StandInSteam {
    fn name(&self) -> &'static str {
        "stand_in_steam"
    }
    fn open(&self) -> std::io::Result<regia::net::link::LinkSocket> {
        Ok(Box::new(self.net.bind(SocketAddr::from(([127, 0, 0, 1], 0)))?))
    }
    fn server_for_host(&self, local: SocketAddr) -> LobbyServer {
        LobbyServer::Address(local)
    }
    fn peer_addr(&self, server: &LobbyServer) -> Option<SocketAddr> {
        matches!(server, LobbyServer::Steam(_)).then_some(self.server)
    }
}

/// A server and two lobby-following clients; the owner launches `LobbyServer::Steam(id)`.
/// `transport` is installed on every client when given. Returns the apps after the launch.
fn steam_launch(transport: bool) -> (App, Vec<App>) {
    let net = LoopbackNet::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let mut server = headless_app(Side::Server);
    let opts = link_options(manual(), ConditionerConfig::default());
    let socket = net.bind(addr).expect("bind");
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(socket), &opts)
        .expect("host");
    let key = server.world().resource::<NetLink>().server_key();
    let hub = LocalLobbyHub::new();
    let mut clients: Vec<App> = (0..2u64)
        .map(|i| {
            let mut app = headless_app(Side::Client);
            app.insert_resource(Lobbies::new(Arc::new(hub.member(i + 1, &format!("bot{i}")))));
            app.insert_resource(LobbyLinkOptions(LinkOptions {
                server_key: key,
                ..opts.clone()
            }));
            if transport {
                app.insert_resource(LobbySteamTransport(Arc::new(StandInSteam {
                    net: net.clone(),
                    server: addr,
                })));
            }
            app.add_plugins(NetGameLobbyPlugin);
            app
        })
        .collect();
    let step = |server: &mut App, clients: &mut Vec<App>| {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
    };
    clients[0]
        .world_mut()
        .resource_mut::<Lobbies>()
        .create(regia::net::lobby::LobbySpec::default())
        .expect("create");
    for _ in 0..5 {
        step(&mut server, &mut clients);
    }
    let lobby = clients[0]
        .world()
        .resource::<Lobbies>()
        .current()
        .expect("created");
    clients[1]
        .world_mut()
        .resource_mut::<Lobbies>()
        .join(lobby)
        .expect("join");
    for _ in 0..5 {
        step(&mut server, &mut clients);
    }
    clients[0]
        .world_mut()
        .resource_mut::<Lobbies>()
        .launch(LobbyServer::Steam(76_561_190_000_000_001))
        .expect("launch");
    for _ in 0..120 {
        step(&mut server, &mut clients);
    }
    (server, clients)
}

#[test]
fn a_steam_id_launch_is_followed_through_the_steam_transport() {
    let (mut server, mut clients) = steam_launch(true);
    for (i, c) in clients.iter_mut().enumerate() {
        assert!(
            c.world().resource::<NetLink>().is_connected(),
            "client {i} followed the Steam launch"
        );
        assert_eq!(avatars(c.world_mut()).len(), 2, "client {i} sees both avatars");
    }
    assert_eq!(avatars(server.world_mut()).len(), 2);
}

#[test]
fn a_steam_id_launch_without_a_transport_is_refused_once() {
    let (_server, clients) = steam_launch(false);
    for (i, c) in clients.iter().enumerate() {
        let link = c.world().resource::<NetLink>();
        assert_eq!(link.role(), "idle", "client {i} has nothing to connect through");
        assert!(
            c.world().resource::<Lobbies>().current().is_none(),
            "client {i} left the lobby instead of retrying every frame"
        );
    }
}

#[test]
fn a_player_who_leaves_loses_the_avatar_everywhere() {
    let mut m = Match::new(
        &Transport::Memory(LoopbackNet::new()),
        ConditionerConfig::default(),
    );
    m.step_until(600, |m| {
        m.clients.iter_mut().all(|c| avatars(c.world_mut()).len() == 2)
    });
    m.clients[1].world_mut().resource_mut::<NetLink>().stop();
    // The server's default silence timeout is 20 s (1,200 ticks, ADR-7950), then the player is
    // dormant for the 60 s resume grace (3,600 ticks, ADR-8298) in case it comes back; its avatar
    // goes when the grace ends.
    m.step_until(5_100, |m| {
        avatars(m.server.world_mut()).len() == 1 && avatars(m.clients[0].world_mut()).len() == 1
    });
    let left = avatars(m.server.world_mut());
    assert_eq!(left[0].slot, 0, "the player who stayed keeps slot 0");
}
