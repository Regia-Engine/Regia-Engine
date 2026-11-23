// SPDX-License-Identifier: MPL-2.0
//! Spec 77 § 6.2 step 3 on the reliable tick stream (post-v0.5 lane `net-travel-tombstone-k`,
//! ADR-7295), two apps (server and client, each with `NetPlugin` and `NetSessionPlugin`) on
//! protocol 1 (`replication.state_channel: Reliable`, so the connection keeps the reliable tick
//! stream), session messages over the in-memory network, replication datagrams over a delay line,
//! manual clocks.
//!
//! `TravelBegin` is a session message and overtakes the replication stream: the server spawns
//! old-level entities and travels while their spawn records are still on the delay line, so the
//! client holds no copy yet when the travel lands. The drop tombstones every old-level id (the
//! persistent `NetId`s survive), so the delayed spawn records are dropped and counted
//! (`ClientReplication.state.counters.tombstoned`) instead of showing the copies until the
//! server's despawn records arrive; the despawn records clear the tombstones, as on the state
//! channel.

use std::net::SocketAddr;

use regia_app::prelude::*;
use regia_net::replication::{ClientReplication, register_replicated};
use regia_net::runtime::{Datagram, NetEntities, NetNode, server_add_peer};
use regia_net::session::lifecycle::{ClientSessionEvent, ClientSessionState};
use regia_net::session::net_session::{ConnectOptions, NetSessionEvents};
use regia_net::session::{HostOptions, NetSession};
use regia_net::socket::LoopbackNet;
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// Replication datagrams spend this many steps on the line: the travel's despawn records reach
/// the client well after the `TravelBegin` session message, and a spawn record's acknowledgment
/// spends a full round trip on it.
const DELAY_STEPS: u64 = 5;

fn net_app(mode: NetMode) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetSessionPlugin));
    register_replicated::<Hp>(app.world_mut(), ReplicateOptions::default());
    let mut settings = app.world_mut().resource_mut::<NetSettings>();
    settings.replication.send_every_ticks = 1;
    // Protocol 1: the connection keeps the reliable tick stream (spec 72 § 4 negotiation).
    settings.replication.state_channel = StateChannel::Reliable;
    app.world_mut().resource_mut::<NetSession>().set_manual_clock(0);
    app
}

/// A server app and a client app; replication datagrams go through a delay line.
struct Pair {
    server: App,
    client: App,
    line: Vec<(u64, bool, Datagram)>,
    step: u64,
}

impl Pair {
    fn connect(port: u16, server: App, client: App) -> Self {
        let mut pair = Self {
            server,
            client,
            line: Vec::new(),
            step: 0,
        };
        pair.server.finish_plugins();
        pair.client.finish_plugins();
        let net = LoopbackNet::new();
        let saddr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
        pair.server
            .world_mut()
            .resource_mut::<NetSession>()
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), true)
            .expect("host");
        pair.client
            .world_mut()
            .resource_mut::<NetSession>()
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        pair
    }

    fn client_state(&self) -> ClientSessionState {
        self.client.world().resource::<NetSession>().state()
    }

    fn step(&mut self) {
        self.step += 1;
        let now = self.step;
        let (due, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut self.line)
            .into_iter()
            .partition(|(at, _, _)| *at <= now);
        self.line = wait;
        for (_, to_server, d) in due {
            let app = if to_server {
                &mut self.server
            } else {
                &mut self.client
            };
            app.world_mut().resource_mut::<NetNode>().inbox.push(d);
        }
        for app in [&mut self.server, &mut self.client] {
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
        }
        self.server.update();
        let out = std::mem::take(&mut self.server.world_mut().resource_mut::<NetNode>().outbox);
        self.line
            .extend(out.into_iter().map(|d| (now + DELAY_STEPS, false, d)));
        self.client.update();
        let out = std::mem::take(&mut self.client.world_mut().resource_mut::<NetNode>().outbox);
        self.line.extend(out.into_iter().map(|mut d| {
            d.peer = PeerId(1);
            (now + DELAY_STEPS, true, d)
        }));
    }

    fn run_until(&mut self, steps: usize, until: impl Fn(&Self) -> bool) -> bool {
        for _ in 0..steps {
            self.step();
            if until(self) {
                return true;
            }
        }
        false
    }
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn client_copies(client: &App) -> usize {
    client.world().resource::<NetEntities>().iter().count()
}

fn travel_arrived(client: &App) -> bool {
    client
        .world()
        .get_resource::<NetSessionEvents>()
        .is_some_and(|log| {
            log.client
                .iter()
                .any(|e| matches!(e, ClientSessionEvent::TravelStarted { .. }))
        })
}

/// Spec 77 § 6.2 step 3 on the reliable tick stream (ADR-7295): the server spawns four entities
/// and travels in the next step, so the spawn records sit on the delay line when `TravelBegin`
/// (a session message, not delayed) arrives and the client holds no copy yet. No tick after the
/// travel may show an old-level copy: the travel's tombstones drop the delayed spawn records of
/// the three plain entities (counted), the persistent one's spawn still applies, and the
/// server's despawn records replace the tombstones. On the base the three copies appear for the
/// ticks until the despawn records arrive.
#[test]
fn the_tick_stream_tombstones_old_level_spawns_in_flight_at_travel() {
    let mut pair = Pair::connect(7821, net_app(NetMode::DedicatedServer), net_app(NetMode::Client));
    assert!(pair.run_until(300, |p| p.client_state() == ClientSessionState::InGame));
    assert!(
        !pair.client.world().resource::<ClientReplication>().state.active(),
        "the client is on the reliable tick stream (protocol 1)"
    );
    server_add_peer(pair.server.world_mut(), PeerId(1));
    let world = pair.server.world_mut();
    let persistent = world.spawn((rep(), Hp { hp: 9 }, NetPersistAcrossTravel)).id();
    let others: Vec<Entity> = (0..3).map(|i| world.spawn((rep(), Hp { hp: i })).id()).collect();
    // One step: the server gathers and the spawn records enter the delay line, DELAY_STEPS away.
    pair.step();
    let kept_id = *pair.server.world().get::<NetId>(persistent).expect("net id");
    let other_ids: Vec<NetId> = others
        .iter()
        .map(|e| *pair.server.world().get::<NetId>(*e).expect("net id"))
        .collect();

    // Travel before the spawn records arrive: TravelBegin (undelayed) overtakes them.
    pair.server
        .world_mut()
        .resource_mut::<NetSession>()
        .travel("levels/b.level.ron")
        .expect("travel");
    assert!(
        pair.run_until(20, |p| travel_arrived(&p.client)),
        "TravelBegin reaches the client"
    );
    assert_eq!(
        client_copies(&pair.client),
        0,
        "the travel arrives before the delayed spawn records"
    );

    // No tick after the travel may show an old-level copy. On the base the delayed spawn
    // records recreate the three plain copies until the despawn records arrive.
    let mut most = 0usize;
    for _ in 0..(DELAY_STEPS * 3) {
        pair.step();
        let copies = client_copies(&pair.client);
        most = most.max(copies);
        assert!(
            copies <= 1,
            "an old-level copy came back on the tick stream after TravelBegin"
        );
    }
    assert_eq!(most, 1, "only the persistent copy ever shows after the travel");
    let kept_copy = pair
        .client
        .world()
        .resource::<NetEntities>()
        .entity(kept_id)
        .expect("the persistent copy spawned after the travel");
    assert!(
        pair.client.world().get_entity(kept_copy).is_ok(),
        "the persistent copy is alive"
    );
    let ids = pair.client.world().resource::<NetEntities>();
    for id in &other_ids {
        assert_eq!(ids.entity(*id), None, "copy {id:?} stays dropped");
    }

    // The delayed spawn records of the three plain entities were dropped and counted; the
    // server's despawn records then replaced the tombstones and changed nothing.
    let tombstoned = pair
        .client
        .world()
        .resource::<ClientReplication>()
        .state
        .counters
        .tombstoned;
    assert!(
        tombstoned >= other_ids.len() as u64,
        "at least one drop per old-level id, got {tombstoned}"
    );
    let dropped = pair
        .client
        .world()
        .resource::<NetSession>()
        .client_travel_counts();
    assert_eq!(
        (dropped.despawned, dropped.persistent),
        (0, 0),
        "the client held no copy when the travel landed"
    );
}

/// The same lane's shape on the state channel stays correct: `TravelBegin` tombstoned the ids
/// already (parent lane), and the new stream-side bookkeeping changes nothing there.
#[test]
fn the_state_channel_still_drops_the_old_level_at_travel() {
    let mut server = net_app(NetMode::DedicatedServer);
    let mut client = net_app(NetMode::Client);
    server
        .world_mut()
        .resource_mut::<NetSettings>()
        .replication
        .state_channel = StateChannel::Unreliable;
    client
        .world_mut()
        .resource_mut::<NetSettings>()
        .replication
        .state_channel = StateChannel::Unreliable;
    let mut pair = Pair::connect(7822, server, client);
    assert!(pair.run_until(300, |p| p.client_state() == ClientSessionState::InGame));
    server_add_peer(pair.server.world_mut(), PeerId(1));
    let world = pair.server.world_mut();
    let persistent = world.spawn((rep(), Hp { hp: 9 }, NetPersistAcrossTravel)).id();
    let others: Vec<Entity> = (0..3).map(|i| world.spawn((rep(), Hp { hp: i })).id()).collect();
    assert!(pair.run_until(60, |p| client_copies(&p.client) == 4));
    let kept_id = *pair.server.world().get::<NetId>(persistent).expect("net id");
    let other_ids: Vec<NetId> = others
        .iter()
        .map(|e| *pair.server.world().get::<NetId>(*e).expect("net id"))
        .collect();
    pair.server
        .world_mut()
        .resource_mut::<NetSession>()
        .travel("levels/b.level.ron")
        .expect("travel");
    assert!(pair.run_until(20, |p| travel_arrived(&p.client)));
    for _ in 0..(DELAY_STEPS * 3) {
        pair.step();
        assert_eq!(client_copies(&pair.client), 1);
    }
    let ids = pair.client.world().resource::<NetEntities>();
    let kept_copy = ids.entity(kept_id).expect("persistent copy");
    assert!(pair.client.world().get_entity(kept_copy).is_ok());
    for id in &other_ids {
        assert_eq!(ids.entity(*id), None);
    }
}
