// SPDX-License-Identifier: MPL-2.0
//! Replication settings users tune (post-v0.5 lane net-config-replication, ADR-7213): `server.toml`
//! `[net]` rates with presence tracking (spec 76 § 4, spec 72 § 8), `sent_to_all` only to connections
//! in `InGame` (spec 72 § 5.1), the client's declared rate in the replication budget (spec 72 § 7),
//! the engine prefab spawner (spec 71 § 4.4) and the lobby identity from the session manifest (spec
//! 89 § 4). Everything runs in fixed steps; no wall clock.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use regia_ecs::prelude::*;
use regia_net::lobby::{Lobbies, LobbyPlugin, LocalLobbyHub, hash_hex, keys};
use regia_net::relevance::{NetSessionPeers, SessionPeer};
use regia_net::replication::{ClientReplication, NetPrefabSpawner, engine_prefab_spawn, register_replicated};
use regia_net::runtime::NetEntities;
use regia_net::server_host::config::CliOverrides;
use regia_net::server_host::{AppHostGame, HostGame, HostNetRates, HostTransport, ServerHost, TestClock};
use regia_net::session::{ContentManifest, NetSession};
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::prefab::{PrefabInstance, PrefabLibrary, PrefabTemplate, TemplateEntity};

fn unique_temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-net-config-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
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

/// A dedicated app whose `config/net.ron` (here: the `NetSettings` resource) sets its own rates.
fn dedicated_game(client: u32, server: u32, every: u32) -> AppHostGame {
    let mut app = regia_app::App::new();
    app.add_plugins(NetPlugin {
        mode: NetMode::DedicatedServer,
    });
    {
        let mut settings = app.world_mut().resource_mut::<NetSettings>();
        settings.replication.client_rate_bytes = client;
        settings.replication.server_rate_bytes = server;
        settings.replication.send_every_ticks = every;
    }
    AppHostGame::new(app)
}

fn rates(game: &AppHostGame) -> (u32, u32, u32) {
    let r = &game.app.world().resource::<NetSettings>().replication;
    (r.client_rate_bytes, r.server_rate_bytes, r.send_every_ticks)
}

/// What the session tells joining clients (`Welcome.send_every_ticks`) and the rate it negotiates.
fn advertised(game: &mut AppHostGame) -> (u32, u32) {
    let opts = game.session().server().expect("server").options();
    (opts.send_every_ticks, opts.client_rate_bytes)
}

fn host(dir: &Path) -> ServerHost {
    ServerHost::new(
        dir,
        CliOverrides::default(),
        Box::new(|_| None),
        Box::new(TestClock::new()),
    )
    .expect("config")
}

/// Item 1 acceptance: a `server.toml` that names only `send_every_ticks` keeps `net.ron`'s two rates,
/// and the server tells clients the cadence it really sends at. A reload that drops the key goes
/// back to `net.ron`'s cadence (the key is not named any more), and the advertisement follows.
#[test]
fn a_server_toml_naming_only_send_every_ticks_keeps_net_rons_rates() {
    let dir = unique_temp_dir("only-cadence");
    write(
        &dir.join("server.toml"),
        &format!("{TOML}[net]\nsend_every_ticks = 3\n"),
    );
    let mut game = dedicated_game(20_000, 150_000, 4);
    let mut host = host(&dir);
    assert_eq!(host.settings().send_every_ticks, Some(3));
    assert_eq!(host.settings().client_rate_bytes, None, "not named: not set");
    host.start(&mut game, HostTransport::Local).expect("start");
    assert_eq!(
        rates(&game),
        (20_000, 150_000, 3),
        "net.ron's rates stay; server.toml's cadence wins"
    );
    assert_eq!(
        advertised(&mut game),
        (3, 20_000),
        "clients are told what the server uses"
    );

    write(&dir.join("server.toml"), TOML);
    let report = host.reload(&mut game);
    assert!(report.needs_restart.is_empty(), "{:?}", report.needs_restart);
    assert!(
        report.applied.iter().any(|k| k == "net.send_every_ticks"),
        "{:?}",
        report.applied
    );
    assert_eq!(rates(&game), (20_000, 150_000, 4), "back to net.ron's cadence");
    assert_eq!(advertised(&mut game), (4, 20_000));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 1: with no `[net]` rate keys at all the host advertises `net.ron`'s cadence, not
/// `server.toml`'s default of 2 (before, a game sending every 4 ticks told clients 2).
#[test]
fn an_unnamed_cadence_advertises_the_net_ron_value() {
    let dir = unique_temp_dir("no-net");
    write(&dir.join("server.toml"), TOML);
    let mut game = dedicated_game(30_000, 0, 4);
    let mut host = host(&dir);
    host.start(&mut game, HostTransport::Local).expect("start");
    assert_eq!(rates(&game), (30_000, 0, 4));
    assert_eq!(advertised(&mut game), (4, 30_000));
    let _ = std::fs::remove_dir_all(&dir);
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

fn setup(world: &mut World) {
    register_replicated::<Hp>(world, ReplicateOptions::default());
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        let mut ps = world.resource_mut::<NetProfiles>();
        let world_p = NetProfile {
            relevance: RelevanceShape::World,
            linger_ticks: 0,
            ..NetProfile::default()
        };
        ps.insert(
            "all",
            NetProfile {
                sent_to_all: true,
                ..world_p.clone()
            },
        )
        .expect("profile");
        ps.insert("everywhere", world_p).expect("profile");
    }
}

fn hub(seed: u64) -> LoopbackHub {
    LoopbackHub::new(NetMode::DedicatedServer, seed, setup)
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

fn session_peer(hub: &mut LoopbackHub, peer: PeerId, in_game: bool, declared_rate_bytes: Option<u32>) {
    let mut peers = hub
        .server
        .world
        .get_resource_or_insert_with(NetSessionPeers::default);
    peers.peers.insert(
        peer,
        SessionPeer {
            in_game,
            declared_rate_bytes,
        },
    );
}

/// Item 2 acceptance (spec 72 § 5.1 stage 4, ADR-1662 (8)): a `sent_to_all` entity reaches only
/// connections whose session is `InGame`. A loading peer gets none of that traffic (a `World`
/// entity still reaches it), a peer the session does not know (a game without a session) is
/// treated as in game, and the loading peer gets the entity once it plays.
#[test]
fn a_peer_outside_in_game_gets_no_sent_to_all_traffic() {
    let mut hub = hub(71);
    let loading = hub.add_client();
    let plain = hub.add_client();
    let loading_peer = hub.clients[loading].peer;
    session_peer(&mut hub, loading_peer, false, None);
    let scores = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 3 }, NetProfileRef("all".into())))
        .id();
    let level_prop = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 4 }, NetProfileRef("everywhere".into())))
        .id();
    hub.run(10);
    assert!(
        copy(&hub, plain, scores).is_some(),
        "a peer without a session row plays"
    );
    assert!(
        copy(&hub, loading, level_prop).is_some(),
        "World relevance is not gated"
    );
    assert!(
        copy(&hub, loading, scores).is_none(),
        "sent_to_all waits for InGame"
    );
    let verdict = hub
        .server
        .world
        .resource::<NetInterest>()
        .why(
            &hub.server.world,
            loading_peer,
            *hub.server.world.get::<NetId>(scores).expect("id"),
        )
        .expect("verdict");
    assert!(!verdict.relevant);
    assert_eq!(verdict.stage, RelevanceStage::SentToAll);

    session_peer(&mut hub, loading_peer, true, None);
    hub.run(10);
    let c = copy(&hub, loading, scores).expect("in game now");
    assert_eq!(hub.clients[loading].world.get::<Hp>(c), Some(&Hp { hp: 3 }));
}

/// Item 3 acceptance (spec 72 § 7): the rate the client declared in its `Hello` lowers its
/// replication budget below the project's rate; another connection keeps the project's budget,
/// and a declared rate above the project's does not raise it.
#[test]
fn a_low_declared_client_rate_caps_bytes_per_tick() {
    let mut hub = hub(73);
    let slow = hub.add_client();
    let fast = hub.add_client();
    let greedy = hub.add_client();
    let (slow_peer, fast_peer, greedy_peer) = (
        hub.clients[slow].peer,
        hub.clients[fast].peer,
        hub.clients[greedy].peer,
    );
    {
        let mut settings = hub.server.world.resource_mut::<NetSettings>();
        settings.replication.send_every_ticks = 1;
        settings.replication.client_rate_bytes = 49_152;
    }
    session_peer(&mut hub, slow_peer, true, Some(6_000));
    session_peer(&mut hub, greedy_peer, true, Some(1_000_000));
    let ents: Vec<Entity> = (0..200)
        .map(|i| {
            hub.server
                .world
                .spawn((rep(), Hp { hp: i }, NetProfileRef("everywhere".into())))
                .id()
        })
        .collect();
    // Spawns settle first (200 records at 24 per send tick), then one second of changes is measured.
    hub.run(30);
    let from = [slow, fast].map(|c| hub.client_logs[c].len());
    for round in 0..60u32 {
        for (i, e) in ents.iter().enumerate() {
            if let Some(mut hp) = hub.server.world.get_mut::<Hp>(*e) {
                hp.hp = 10_000 * (round + 1) + i as u32;
            }
        }
        hub.run(1);
    }
    let interest = hub.server.world.resource::<NetInterest>();
    let budget = |p: PeerId| interest.peers.get(&p).expect("peer").send.counters.budget;
    let rate = |p: PeerId| interest.peers.get(&p).expect("peer").send.rate(49_152);
    assert_eq!(rate(slow_peer), 6_000, "the declared rate");
    assert_eq!(rate(fast_peer), 49_152, "the project's rate");
    assert_eq!(
        rate(greedy_peer),
        49_152,
        "a declared rate never raises the project's"
    );
    let floor = regia_net::send::ConnSendState {
        session_rate: Some(1),
        ..Default::default()
    };
    assert_eq!(
        floor.rate(49_152),
        regia_net::send::MIN_CLIENT_RATE,
        "not below the 4 KiB/s floor"
    );
    assert_eq!(floor.rate(600), 600, "nor above a project rate under the floor");
    // 60 send ticks per second: 100 bytes per tick for the slow peer, ~819 for the others.
    assert!(budget(slow_peer) <= 2 * 100, "slow budget {}", budget(slow_peer));
    assert!(budget(fast_peer) >= 800, "fast budget {}", budget(fast_peer));
    // Bytes that reached each client over the measured second (60 send ticks): the slow peer stays
    // within its declared 6,000 B/s plus one packet of slack; the fast peer takes far more.
    let bytes = |c: usize, from: usize| -> usize {
        hub.client_logs[c][from..]
            .iter()
            .map(|(_, d)| d.bytes.len())
            .sum()
    };
    let (slow_bytes, fast_bytes) = (bytes(slow, from[0]), bytes(fast, from[1]));
    // The budget governs record bytes; each of the 60 tick frames also carries its header and acks
    // (framing outside the budget, spec 72 § 7), allowed here at 32 bytes per frame.
    assert!(
        slow_bytes <= 6_000 + 1_200 + 60 * 32,
        "slow peer got {slow_bytes} bytes in one second"
    );
    assert!(
        fast_bytes > 3 * slow_bytes,
        "fast {fast_bytes} vs slow {slow_bytes}"
    );
}

#[derive(Component, Clone, Debug, PartialEq, Default)]
struct NotAHook;

/// Item 4 acceptance (spec 71 § 4.4): with no `NetPrefabSpawner` installed, a Prefab spawn record
/// instantiates the prefab from the client's `PrefabLibrary` (the engine spawner), with the
/// replicated values on top. A prefab the client does not hold still falls back to bare and logs
/// `net.prefab_missing` once.
#[test]
fn a_prefab_spawn_record_spawns_the_prefab_with_no_game_hook() {
    let mut hub = hub(74);
    let a = hub.add_client();
    let asset = regia_core::id::Guid::from_u128(42);
    let template = PrefabTemplate {
        asset,
        entities: vec![
            TemplateEntity {
                local_id: regia_core::id::Guid::from_u128(1),
                name: Some("crate_root".into()),
                parent: None,
                components: Vec::new(),
                nested: None,
            },
            TemplateEntity {
                local_id: regia_core::id::Guid::from_u128(2),
                name: Some("crate_lid".into()),
                parent: Some(0),
                components: Vec::new(),
                nested: None,
            },
        ],
        warnings: Vec::new(),
    };
    {
        let w = &mut hub.clients[a].world;
        w.init_resource::<regia_ecs::reflect::AppTypeRegistry>();
        let mut library = PrefabLibrary::new();
        library.insert(template);
        w.insert_resource(library);
        assert!(!w.contains_resource::<NetPrefabSpawner>(), "no game hook");
    }
    let inst = |prefab| PrefabInstance {
        prefab,
        ..PrefabInstance::default()
    };
    let everywhere = || NetProfileRef("everywhere".into());
    let good = hub
        .server
        .world
        .spawn((rep(), inst(42), Hp { hp: 7 }, NotAHook, everywhere()))
        .id();
    let missing = hub
        .server
        .world
        .spawn((rep(), inst(43), Hp { hp: 8 }, everywhere()))
        .id();
    hub.run(4);
    let w = &hub.clients[a].world;
    let c = copy(&hub, a, good).expect("copy");
    let name = |e: Entity| {
        w.get::<regia_scene::components::Name>(e)
            .map(|n| n.as_str().to_owned())
    };
    let children =
        |e: Entity| -> Vec<Entity> { w.get::<Children>(e).map(|ch| ch.to_vec()).unwrap_or_default() };
    // The copy is the instance root; the template's entities hang under it.
    let parts = children(c);
    assert_eq!(parts.len(), 1, "the prefab's root under the instance");
    assert_eq!(name(parts[0]), Some("crate_root".to_owned()));
    let lid = children(parts[0]);
    assert_eq!(lid.len(), 1, "non-replicated parts come from the prefab");
    assert_eq!(name(lid[0]), Some("crate_lid".to_owned()));
    assert_eq!(w.get::<Hp>(c), Some(&Hp { hp: 7 }), "replicated values on top");
    let mc = copy(&hub, a, missing).expect("bare copy");
    assert!(children(mc).is_empty(), "a prefab the client lacks spawns bare");
    assert_eq!(w.resource::<ClientReplication>().counters.prefab_missing, 1);
}

/// Item 5 acceptance (spec 89 § 4): a game that never called `Lobbies::set_identity` creates and
/// lists lobbies under the protocol and content hashes of `NetSession::manifest()`; a game that set
/// an identity keeps its own.
#[test]
fn a_lobby_without_identity_carries_the_manifest_hashes() {
    let hub = LocalLobbyHub::new();
    let manifest = ContentManifest::build("e", "g", 1, 0xABCD, &[]);
    let mut app = regia_app::App::new();
    let mut session = NetSession::default();
    session.set_manifest(manifest.clone());
    app.insert_resource(session);
    app.insert_resource(Lobbies::new(Arc::new(hub.member(1, "p1"))));
    app.add_plugins(LobbyPlugin);
    app.update();
    let mut lobbies = app.world_mut().resource_mut::<Lobbies>();
    let spec = lobbies.default_spec();
    lobbies.create(spec).expect("create");
    app.update();
    let lobbies = app.world().resource::<Lobbies>();
    let data = &lobbies.snapshot().expect("in a lobby").data;
    assert_eq!(data[keys::PROTOCOL], hash_hex(0xABCD));
    assert_eq!(data[keys::CONTENT], hash_hex(manifest.content_hash));

    // A game identity wins over the manifest.
    let mut own = Lobbies::new(Arc::new(hub.member(2, "p2")));
    own.set_identity("example.duel", &ContentManifest::build("e", "g", 1, 0x1234, &[]));
    let mut app = regia_app::App::new();
    let mut session = NetSession::default();
    session.set_manifest(manifest);
    app.insert_resource(session);
    app.insert_resource(own);
    app.add_plugins(LobbyPlugin);
    app.update();
    let mut lobbies = app.world_mut().resource_mut::<Lobbies>();
    let spec = lobbies.default_spec();
    lobbies.create(spec).expect("create");
    app.update();
    let data = &app.world().resource::<Lobbies>().snapshot().expect("lobby").data;
    assert_eq!(data[keys::PROTOCOL], hash_hex(0x1234));
}

/// Item 8 (spec 71 § 15, ADR-7213): the despawn check visits the entities whose `Replicated` left
/// since the last gather, not every tracked entity. The first gather of a world checks them all; a
/// quiet tick checks none; a tick with three despawns and one `Replicated` removal checks four,
/// and their copies still go on the client.
#[test]
fn the_despawn_check_visits_only_entities_that_lost_replicated() {
    use regia_net::replication::ServerReplication;
    let mut hub = hub(78);
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..1_000)
        .map(|i| {
            hub.server
                .world
                .spawn((rep(), Hp { hp: i }, NetProfileRef("everywhere".into())))
                .id()
        })
        .collect();
    let checks = |hub: &LoopbackHub| hub.server.world.resource::<ServerReplication>().despawn_checks();
    // 1,000 spawn records at 24 per send tick (every 2 ticks) need about 84 ticks.
    hub.run(120);
    assert_eq!(checks(&hub), 0, "a quiet tick checks nothing");
    assert!(ents.iter().all(|e| copy(&hub, a, *e).is_some()));
    let gone = [ents[3], ents[500], ents[999]];
    let unreplicated = ents[42];
    let id_of = |hub: &LoopbackHub, e: Entity| *hub.server.world.get::<NetId>(e).expect("id");
    let gone_ids: Vec<NetId> = gone.iter().map(|e| id_of(&hub, *e)).collect();
    let kept_id = id_of(&hub, unreplicated);
    for e in gone {
        assert!(hub.server.world.despawn(e));
    }
    hub.server.world.entity_mut(unreplicated).remove::<Replicated>();
    hub.run(1);
    assert_eq!(checks(&hub), 4, "three despawns and one removal");
    hub.run(4);
    assert_eq!(checks(&hub), 0);
    let client_copy =
        |hub: &LoopbackHub, id: NetId| hub.clients[a].world.resource::<NetEntities>().entity(id);
    for id in gone_ids.iter().chain([&kept_id]) {
        assert!(client_copy(&hub, *id).is_none(), "copy {id:?} goes on the client");
    }
    assert_eq!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .entities()
            .count(),
        996
    );
    assert!(
        hub.server.world.get::<NetId>(unreplicated).is_none(),
        "the server entity stays, local"
    );
}

/// Review finding 3 (ADR-7213): `despawn_checks` counts the deduplicated `NetId`s the gather
/// checks, not the raw queue length — a remove → re-add → remove inside one tick queues the
/// entity twice, but the gather checks it once (and the copy still goes).
#[test]
fn despawn_checks_count_deduplicated_ids() {
    use regia_net::replication::ServerReplication;
    let mut hub = hub(79);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 1 }, NetProfileRef("everywhere".into())))
        .id();
    hub.run(10);
    assert!(copy(&hub, a, e).is_some(), "the copy arrived");
    // remove → re-add → remove inside one tick: two queue entries, one checked entity.
    hub.server.world.entity_mut(e).remove::<Replicated>();
    hub.server.world.entity_mut(e).insert(rep());
    hub.server.world.entity_mut(e).remove::<Replicated>();
    hub.run(1);
    assert_eq!(
        hub.server.world.resource::<ServerReplication>().despawn_checks(),
        1,
        "the duplicate queue entry does not inflate the counter"
    );
    hub.run(4);
    assert!(copy(&hub, a, e).is_none(), "the copy still goes");
}

/// Review finding 4 (ADR-7213): the engine prefab spawner mints root guids from a per-world
/// counter, so two worlds that spawn the same prefabs in the same order mint the same guids and
/// a replay stays bit-deterministic; a process-wide counter broke that.
#[test]
fn prefab_spawn_guids_are_per_world_deterministic() {
    let make = || {
        let mut world = World::new();
        world.init_resource::<regia_ecs::reflect::AppTypeRegistry>();
        let mut library = PrefabLibrary::new();
        library.insert(PrefabTemplate {
            asset: regia_core::id::Guid::from_u128(42),
            entities: vec![TemplateEntity {
                local_id: regia_core::id::Guid::from_u128(1),
                name: Some("crate_root".into()),
                parent: None,
                components: Vec::new(),
                nested: None,
            }],
            warnings: Vec::new(),
        });
        world.insert_resource(library);
        world
    };
    let guid_of = |world: &mut World| {
        let root = engine_prefab_spawn(world, 42).expect("spawn");
        world
            .get::<regia_scene::components::EntityGuid>(root)
            .expect("guid")
            .0
    };
    let mut a = make();
    let mut b = make();
    let (a1, a2) = (guid_of(&mut a), guid_of(&mut a));
    let (b1, b2) = (guid_of(&mut b), guid_of(&mut b));
    assert_eq!(a1, b1, "fresh worlds mint the same first guid");
    assert_eq!(a2, b2, "fresh worlds mint the same second guid");
    assert_ne!(a1, a2, "two copies in one world stay unique");
}

/// A custom host that applies `server.toml`'s `[net]` rates itself but leaves
/// `net_rates_in_use` at the trait default (all `None`) — review finding 2's scenario.
struct RatesHidingGame {
    session: NetSession,
    named: Option<HostNetRates>,
}

impl HostGame for RatesHidingGame {
    fn session(&mut self) -> &mut NetSession {
        &mut self.session
    }

    fn step(&mut self) {
        self.session.update();
    }

    fn set_net_rates(&mut self, rates: &HostNetRates) {
        self.named = Some(*rates);
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `f` with a WARN-level fmt subscriber and returns its output (as `player_data.rs` does).
fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = sink
        .0
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    (out, text)
}

/// Review finding 2 (ADR-7213, ADR-5650's bug class): a custom `HostGame` that applies the named
/// `[net]` rates itself but does not override `net_rates_in_use` is warned once per
/// named-but-unreported key at start — the `Welcome` would otherwise advertise `server.toml`'s
/// values while the game sends at its own cadence.
#[test]
fn a_custom_host_hiding_its_rates_is_warned_per_named_key() {
    let dir = unique_temp_dir("hidden-rates");
    write(
        &dir.join("server.toml"),
        &format!("{TOML}[net]\nclient_rate_bytes = 10000\nsend_every_ticks = 5\n"),
    );
    let mut game = RatesHidingGame {
        session: NetSession::default(),
        named: None,
    };
    let mut host = host(&dir);
    let (_, log) = capture(|| host.start(&mut game, HostTransport::Local).expect("start"));
    let named = game.named.expect("set_net_rates ran");
    assert_eq!(named.client_rate_bytes, Some(10_000));
    assert_eq!(named.send_every_ticks, Some(5));
    assert!(
        log.contains("[net] client_rate_bytes"),
        "no client_rate_bytes warn in: {log}"
    );
    assert!(
        log.contains("[net] send_every_ticks"),
        "no send_every_ticks warn in: {log}"
    );
    assert!(
        !log.contains("[net] server_rate_bytes"),
        "an unnamed key must not be warned about: {log}"
    );
}

/// Review finding 5 (spec 89 § 4, ADR-7213): a lobby created before the session manifest was
/// followed (a `Startup` create not ordered `.after(lobby_follow_manifest)`) carries zero hashes;
/// once the followed hashes are known the owner re-sends the engine identity keys.
#[test]
fn a_lobby_created_before_the_manifest_follow_is_patched() {
    let hub = LocalLobbyHub::new();
    let manifest = ContentManifest::build("e", "g", 1, 0xABCD, &[]);
    let content_hash = manifest.content_hash;
    let mut app = regia_app::App::new();
    let mut session = NetSession::default();
    session.set_manifest(manifest);
    app.insert_resource(session);
    app.insert_resource(Lobbies::new(Arc::new(hub.member(1, "p1"))));
    app.add_plugins(LobbyPlugin);
    // The footgun: the create runs before `lobby_follow_manifest` (here: before any update).
    {
        let mut lobbies = app.world_mut().resource_mut::<Lobbies>();
        let spec = lobbies.default_spec();
        lobbies.create(spec).expect("create");
    }
    app.update(); // Startup follows the manifest; the pump lands the create.
    app.update(); // The stale keys are re-sent; the snapshot refreshes.
    let lobbies = app.world().resource::<Lobbies>();
    let data = &lobbies.snapshot().expect("in a lobby").data;
    assert_eq!(data[keys::PROTOCOL], hash_hex(0xABCD));
    assert_eq!(data[keys::CONTENT], hash_hex(content_hash));
}
