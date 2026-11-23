// SPDX-License-Identifier: MPL-2.0
//! Spec 77 §§ 4, 4.1, 6.1, 6.2 and spec 70 § 4 integration items that ADR-1663 (18) deferred
//! (post-v0.5 lane `net-session-lifecycle`, ADR-7212):
//!
//! - acceptance 5: a joining connection's first replication frame carries its own entities and the
//!   relevant-everywhere (`sent_to_all`) game state outside the spawn budget, while 300 other
//!   entities stream in over later send ticks within the budget;
//! - the join snapshot is fed from replication (removed level entities, the peer's records);
//! - mounted mods join the in-memory manifest at session start;
//! - a dev client refused with `protocol_mismatch` sees the differing registered type names;
//! - a client `NetSession` sets `LocalPeer` from the welcome.
//!
//! Travel (acceptance 6) is in `session_post05_travel.rs`. Fixed steps and a manual session clock only: no sockets, no wall-clock budgets.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_app::prelude::*;
use regia_asset::mods_vfs::{ModSource, ModsVfs};
use regia_net::replication::{NetEvents, register_replicated};
use regia_net::runtime::NetEntities;
use regia_net::session::lifecycle::ClientSessionState;
use regia_net::session::net_session::ConnectOptions;
use regia_net::session::{DisconnectReason, HostOptions, NetSession};
use regia_net::socket::LoopbackNet;
use regia_net::testing::LoopbackHub;
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// A component only one side registers (the protocol-mismatch test).
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct ServerOnlyArmor {
    points: u32,
}

fn setup(world: &mut World) {
    register_replicated::<Hp>(world, ReplicateOptions::default());
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        let mut ps = world.resource_mut::<NetProfiles>();
        let everywhere = NetProfile {
            relevance: RelevanceShape::World,
            linger_ticks: 0,
            ..NetProfile::default()
        };
        let state = NetProfile {
            sent_to_all: true,
            ..everywhere.clone()
        };
        ps.insert("everywhere", everywhere).expect("valid profile");
        ps.insert("state", state).expect("valid profile");
        ps.set_default("everywhere");
    }
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn spawn_with(hub: &mut LoopbackHub, profile: &str) -> Entity {
    hub.server
        .world
        .spawn((rep(), Hp { hp: 1 }, NetProfileRef(profile.into())))
        .id()
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

/// Spec 77 acceptance 5 (300-entity part) and § 6.1 item 2: the join frame carries the game state
/// and the peer's own entity regardless of the spawn budget (24 per send tick by default); the
/// other 300 entities stream in over later send ticks, never more than the budget per tick.
#[test]
fn join_frame_carries_own_and_game_state_entities_outside_the_spawn_budget() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
    let others: Vec<Entity> = (0..300).map(|_| spawn_with(&mut hub, "everywhere")).collect();
    // The game state is spawned last: it has the highest `NetId` of the server's entities, so a
    // plain priority order (ties by id) would send it after all 300 others.
    let state = spawn_with(&mut hub, "state");
    let a = hub.add_client();
    let peer = hub.clients[a].peer;
    let own = hub
        .server
        .world
        .spawn((
            rep(),
            Hp { hp: 2 },
            NetOwner(peer),
            NetProfileRef("everywhere".into()),
        ))
        .id();
    let budget = hub
        .server
        .world
        .resource::<NetSettings>()
        .replication
        .spawns_per_send as usize;
    assert_eq!(budget, 24, "the default spawn budget");

    let mut first: Option<usize> = None;
    let mut per_tick: Vec<usize> = Vec::new();
    for _ in 0..80 {
        hub.step();
        let n = hub.clients[a].world.resource::<NetEvents>().spawned.len();
        if n == 0 {
            continue;
        }
        if first.is_none() {
            first = Some(n);
            assert!(
                copy(&hub, a, state).is_some(),
                "the game state arrived in the join frame"
            );
            assert!(
                copy(&hub, a, own).is_some(),
                "the peer's own entity arrived in the join frame"
            );
            let others_now = others.iter().filter(|e| copy(&hub, a, **e).is_some()).count();
            assert!(
                others_now <= budget,
                "other entities stay within the spawn budget in the join frame: {others_now}"
            );
        } else {
            assert!(n <= budget, "later send ticks stay within the spawn budget: {n}");
        }
        per_tick.push(n);
    }
    assert_eq!(
        first,
        Some(budget + 2),
        "24 others plus the 2 join-critical entities"
    );
    assert_eq!(
        per_tick.iter().sum::<usize>(),
        302,
        "every entity arrived: {per_tick:?}"
    );
    assert!(
        per_tick.len() >= 300usize.div_ceil(budget),
        "the 300 others spread over several send ticks: {per_tick:?}"
    );
}

fn session_app(mode: NetMode) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((NetPlugin { mode }, NetSessionPlugin));
    app.world_mut().resource_mut::<NetSession>().set_manual_clock(0);
    app
}

/// Spec 77 § 4.1: the mods the mod manager has mounted join the in-memory manifest at session
/// start, so a mod's content changes the manifest hash.
#[test]
fn mounted_mods_join_the_manifest_at_session_start() {
    let host_hash = |files: Option<&[(&str, &[u8])]>| -> (u64, Vec<String>) {
        let mut app = session_app(NetMode::DedicatedServer);
        if let Some(files) = files {
            let mut vfs = ModsVfs::new();
            let map: BTreeMap<String, Vec<u8>> =
                files.iter().map(|(p, b)| ((*p).to_owned(), b.to_vec())).collect();
            vfs.mount("speedrun", ModSource::Memory(map)).expect("mount");
            app.insert_resource(vfs);
        }
        app.finish_plugins();
        let mut s = app.world_mut().resource_mut::<NetSession>();
        s.host_local(HostOptions::default(), true).expect("host");
        let core = s.server().expect("server").manifest().clone();
        (
            core.content_hash,
            core.mods.iter().map(|m| m.id.clone()).collect(),
        )
    };
    let (plain, none) = host_hash(None);
    assert!(none.is_empty());
    let (modded, ids) = host_hash(Some(&[("levels/x.level.ron", b"(a: 1)")]));
    assert_eq!(ids, vec!["speedrun".to_owned()]);
    assert_ne!(plain, modded, "a mounted mod changes the manifest hash");
    let (other, _) = host_hash(Some(&[("levels/x.level.ron", b"(a: 2)")]));
    assert_ne!(modded, other, "the mod's content feeds the hash");
}

fn drive(server: &mut App, client: &mut App, until: impl Fn(&App) -> bool) -> bool {
    for _ in 0..300 {
        server
            .world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        client
            .world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        client.update();
        server.update();
        if until(client) {
            return true;
        }
    }
    false
}

fn host_and_connect(server: &mut App, client: &mut App, port: u16) {
    let net = LoopbackNet::new();
    let saddr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    server.finish_plugins();
    client.finish_plugins();
    server
        .world_mut()
        .resource_mut::<NetSession>()
        .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), true)
        .expect("host");
    client
        .world_mut()
        .resource_mut::<NetSession>()
        .connect_on(
            net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
            saddr,
            ConnectOptions::default(),
        )
        .expect("connect");
}

/// Spec 70 § 4: `LocalPeer` is `None` until the welcome, then the client's own peer id.
#[test]
fn a_client_net_session_sets_local_peer_from_the_welcome() {
    let mut server = session_app(NetMode::DedicatedServer);
    let mut client = session_app(NetMode::Client);
    host_and_connect(&mut server, &mut client, 7791);
    assert_eq!(client.world().get_resource::<LocalPeer>().and_then(|p| p.0), None);
    assert!(drive(&mut server, &mut client, |c| {
        c.world().resource::<NetSession>().state() == ClientSessionState::InGame
    }));
    let welcomed = client
        .world()
        .resource::<NetSession>()
        .client()
        .and_then(|c| c.peer_id())
        .expect("welcomed peer");
    assert_eq!(
        client.world().get_resource::<LocalPeer>().and_then(|p| p.0),
        Some(welcomed)
    );
}

/// Spec 77 § 4 item 2: a dev client refused with `protocol_mismatch` sees the registered type
/// names that differ, not only the two hashes.
#[test]
fn a_dev_protocol_mismatch_names_the_differing_types() {
    let mut server = session_app(NetMode::DedicatedServer);
    register_replicated::<Hp>(server.world_mut(), ReplicateOptions::default());
    register_replicated::<ServerOnlyArmor>(server.world_mut(), ReplicateOptions::default());
    let mut client = session_app(NetMode::Client);
    register_replicated::<Hp>(client.world_mut(), ReplicateOptions::default());
    host_and_connect(&mut server, &mut client, 7792);
    assert!(drive(&mut server, &mut client, |c| {
        matches!(
            c.world().resource::<NetSession>().state(),
            ClientSessionState::Disconnected(_)
        )
    }));
    let state = client.world().resource::<NetSession>().state();
    let ClientSessionState::Disconnected(Some(DisconnectReason::ProtocolMismatch(detail))) = state else {
        panic!("expected protocol_mismatch, got {state:?}");
    };
    assert!(detail.contains("ServerOnlyArmor"), "{detail}");
    assert!(detail.contains("missing on client"), "{detail}");
    assert!(
        !detail.contains("::Hp"),
        "a type both sides agree on is not listed: {detail}"
    );
}

/// Review finding rev-claude-29-k #3: a join-critical spawn held back by its profile's
/// `min_interval_ticks` (a copy the client has, re-baselined after its baseline expired) still
/// counts as join-pending, so the connection's join window stays open until the record actually
/// goes. It must not flip shut at the gather where the held-back critical first offers.
#[test]
fn a_held_back_join_critical_spawn_keeps_the_join_window_open() {
    use regia_net::replication::{STATE_VERSION_RING, ServerReplication};

    // The scenario's numbers follow the client's version ring (ADR-10051 made it 512; they were
    // written for 64 as `min_interval_ticks: 200` and `i < 200`): the interval is the ring plus
    // 136 ticks, so the baseline expires 136 ticks before the interval lets the record go.
    const MIN_INTERVAL: u32 = STATE_VERSION_RING + 136;

    fn state_setup(world: &mut World) {
        setup(world);
        // The hub forces the reliable tick stream unless a test opts into the state channel.
        world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
        if world.resource::<NetMode>().is_server() {
            let damped = NetProfile {
                sent_to_all: true,
                relevance: RelevanceShape::World,
                min_interval_ticks: MIN_INTERVAL,
                ..NetProfile::default()
            };
            world
                .resource_mut::<NetProfiles>()
                .insert("state", damped)
                .expect("valid profile");
        }
    }

    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 21, state_setup);
    hub.negotiate = true;
    let score = hub
        .server
        .world
        .spawn((
            rep(),
            Hp { hp: 1 },
            NetProfileRef("state".into()),
            NetPersistAcrossTravel,
        ))
        .id();
    let a = hub.add_client();
    let peer = hub.clients[a].peer;
    hub.run(30);
    assert!(
        copy(&hub, a, score).is_some(),
        "the game state arrived in the join frame"
    );
    assert!(
        !hub.server
            .world
            .resource::<ServerReplication>()
            .join_window_open(peer),
        "the join window closed once the join frame went"
    );
    // Mutate every tick without ever resending (min_interval is the ring + 136): after a ring of versions the
    // copy's baseline expires and the game state re-offers as a spawn, held back for the rest
    // of the interval. Wait for exactly that (the link's expired-baseline counter), then travel
    // while it is in exactly that state.
    let expired = |hub: &LoopbackHub| {
        hub.server
            .world
            .resource::<ServerReplication>()
            .state_link(peer)
            .map(|l| l.counters.baselines_expired)
            .unwrap_or(0)
    };
    let mut i = 0u32;
    while expired(&hub) < 1 {
        hub.server.world.entity_mut(score).insert(Hp { hp: i });
        hub.step();
        i += 1;
        assert!(i < MIN_INTERVAL, "the baseline never expired");
    }
    // A few more ticks in the held-back state, still well inside the interval.
    for _ in 0..20 {
        hub.server.world.entity_mut(score).insert(Hp { hp: i });
        hub.step();
        i += 1;
    }
    regia_net::session::despawn_for_travel(&mut hub.server.world);
    assert!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .join_window_open(peer),
        "a travel reopens the join window"
    );
    let critical_before = hub
        .server
        .world
        .resource::<ServerReplication>()
        .counters
        .join_critical_spawns;
    // The held-back re-baselined spawn is join-critical now: the window must stay open until
    // the record actually goes (when `since` reaches min_interval), not flip shut at the first
    // held-back gather after the travel.
    let travel_at = i;
    let mut closed_at = None;
    while i < travel_at + 300 {
        hub.server.world.entity_mut(score).insert(Hp { hp: i });
        hub.step();
        if !hub
            .server
            .world
            .resource::<ServerReplication>()
            .join_window_open(peer)
            && closed_at.is_none()
        {
            closed_at = Some(i - travel_at);
        }
        i += 1;
    }
    let closed_at = closed_at.expect("the join window closes once the critical spawn went");
    assert!(
        (80..=220).contains(&closed_at),
        "the window closed {closed_at} mutations after the travel (min_interval is the ring + 136: held back, it must close only when the record goes, ~115 mutations in; the base closed it at the first gather)"
    );
    assert!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .counters
            .join_critical_spawns
            > critical_before,
        "the late critical spawn is counted as join-critical"
    );
}

/// Review finding rev-claude-29-k #6: the join snapshot feed re-captured every Loading peer every
/// frame. A quiet frame (no replicated entity, owner, removal or profile change) must reuse the
/// capture; only a change re-captures.
#[test]
fn join_snapshots_are_recaptured_only_when_replication_changes() {
    use std::time::Duration;

    use regia_app::prelude::TimeUpdateStrategy;

    let mut server = session_app(NetMode::DedicatedServer);
    server.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    register_replicated::<Hp>(server.world_mut(), ReplicateOptions::default());
    for i in 0..40u32 {
        server.world_mut().spawn((rep(), Hp { hp: i }));
    }
    let mut client = session_app(NetMode::Client);
    client.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    register_replicated::<Hp>(client.world_mut(), ReplicateOptions::default());
    client.world_mut().resource_mut::<NetSession>().auto_level_ready = false;
    host_and_connect(&mut server, &mut client, 7793);
    // With `auto_level_ready` off the client never sends `LevelReady`: the server peer stays
    // Loading for the whole test.
    assert!(
        drive(&mut server, &mut client, |c| {
            c.world().resource::<NetSession>().state() == ClientSessionState::Loading
        }),
        "the client never reached Loading (state {:?})",
        client.world().resource::<NetSession>().state()
    );
    let quiet = |server: &mut App, client: &mut App, n: usize| {
        for _ in 0..n {
            server
                .world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            client
                .world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            client.update();
            server.update();
        }
    };
    quiet(&mut server, &mut client, 4);
    let settled = server.world().resource::<NetSession>().join_counts().captures;
    quiet(&mut server, &mut client, 8);
    let after_quiet = server.world().resource::<NetSession>().join_counts().captures;
    assert_eq!(
        settled, after_quiet,
        "quiet frames re-captured the join snapshot {settled} -> {after_quiet} times"
    );
    server.world_mut().spawn((rep(), Hp { hp: 1000 }));
    quiet(&mut server, &mut client, 2);
    let after_change = server.world().resource::<NetSession>().join_counts().captures;
    assert!(
        after_change > after_quiet,
        "a replication change re-captures the join snapshot"
    );
    quiet(&mut server, &mut client, 6);
    assert_eq!(
        server.world().resource::<NetSession>().join_counts().captures,
        after_change,
        "quiet frames after the change reuse the capture"
    );
}
