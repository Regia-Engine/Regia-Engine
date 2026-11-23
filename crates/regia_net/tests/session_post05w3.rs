// SPDX-License-Identifier: MPL-2.0
//! Spec 77 § 6.2 step 3 and § 4.1 / acceptance 3 end to end (post-v0.5 lane `net-travel-mods`,
//! ADR-7259), two apps (server and client, each with `NetPlugin` and `NetSessionPlugin`): session
//! messages over the in-memory network, replication datagrams over a delay line, manual clocks.
//!
//! - At `TravelBegin` the client drops its non-persistent replicated copies itself: in the frame
//!   the travel arrives it holds only the persistent copies, under their `NetId`s, although the
//!   server's despawn records are still on the delay line.
//! - Mods carry the version text of their own `luamod.ron` into the session manifest; a client
//!   missing one DLC pak and running another mod version is refused with exactly those two items;
//!   a mod that declares no version still joins a matching server.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_app::prelude::*;
use regia_asset::mods_vfs::{ModSource, ModsVfs};
use regia_net::replication::register_replicated;
use regia_net::runtime::{Datagram, NetEntities, NetNode, server_add_peer};
use regia_net::session::lifecycle::{ClientSessionEvent, ClientSessionState};
use regia_net::session::net_session::{ConnectOptions, NetSessionEvents};
use regia_net::session::{ContentManifest, DisconnectReason, HostOptions, ManifestAsset, NetSession};
use regia_net::socket::LoopbackNet;
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// Replication datagrams spend this many steps on the line: the travel's despawn records reach
/// the client well after the `TravelBegin` session message.
const DELAY_STEPS: u64 = 5;

fn net_app(mode: NetMode, mods: Option<ModsVfs>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetSessionPlugin));
    if let Some(vfs) = mods {
        app.insert_resource(vfs);
    }
    register_replicated::<Hp>(app.world_mut(), ReplicateOptions::default());
    app.world_mut()
        .resource_mut::<NetSettings>()
        .replication
        .send_every_ticks = 1;
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

/// Spec 77 § 6.2 step 3: in the frame `TravelBegin` arrives the client holds only the persistent
/// copies (the server's despawn records are still DELAY_STEPS away), each under its `NetId` and on
/// the same client entity; the later despawn records change nothing; the session reports what the
/// client dropped.
#[test]
fn the_client_holds_only_persistent_copies_at_travel_begin() {
    let mut pair = Pair::connect(
        7811,
        net_app(NetMode::DedicatedServer, None),
        net_app(NetMode::Client, None),
    );
    assert!(pair.run_until(300, |p| p.client_state() == ClientSessionState::InGame));
    server_add_peer(pair.server.world_mut(), PeerId(1));
    let world = pair.server.world_mut();
    let persistent = world.spawn((rep(), Hp { hp: 9 }, NetPersistAcrossTravel)).id();
    let others: Vec<Entity> = (0..3).map(|i| world.spawn((rep(), Hp { hp: i })).id()).collect();
    assert!(pair.run_until(60, |p| client_copies(&p.client) == 4));
    let kept_id = *pair.server.world().get::<NetId>(persistent).expect("net id");
    let kept_copy = pair
        .client
        .world()
        .resource::<NetEntities>()
        .entity(kept_id)
        .expect("persistent copy");
    let other_ids: Vec<NetId> = others
        .iter()
        .map(|e| *pair.server.world().get::<NetId>(*e).expect("net id"))
        .collect();

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
        1,
        "at the TravelBegin step the client holds only the persistent copy"
    );
    let ids = pair.client.world().resource::<NetEntities>();
    assert_eq!(ids.entity(kept_id), Some(kept_copy), "same NetId, same entity");
    for id in &other_ids {
        assert_eq!(ids.entity(*id), None, "copy {id:?} dropped at TravelBegin");
        assert!(
            pair.client.world().get_entity(kept_copy).is_ok(),
            "the persistent copy is alive"
        );
    }
    let dropped = pair
        .client
        .world()
        .resource::<NetSession>()
        .client_travel_counts();
    assert_eq!((dropped.despawned, dropped.persistent), (3, 1));

    // The server's own despawn records arrive later and change nothing.
    for _ in 0..(DELAY_STEPS * 3) {
        pair.step();
    }
    assert_eq!(client_copies(&pair.client), 1);
    assert_eq!(
        pair.client.world().resource::<NetEntities>().entity(kept_id),
        Some(kept_copy)
    );
    assert!(pair.client.world().get_entity(kept_copy).is_ok());
}

fn asset(id: &str, kind: &str, hash: u64) -> ManifestAsset {
    ManifestAsset {
        id: id.into(),
        kind: kind.into(),
        content_hash: hash,
    }
}

fn manifest(with_dlc: bool) -> ContentManifest {
    let base = vec![
        asset("levels/yard.level.ron", "level", 1),
        asset("config/rules.ron", "config", 2),
    ];
    let dlc = vec![asset("levels/harbor.level.ron", "level", 21)];
    let mut packs: Vec<(&str, &[ManifestAsset])> = vec![("base", base.as_slice())];
    if with_dlc {
        packs.push(("dlc_harbor", dlc.as_slice()));
    }
    ContentManifest::build("0.5.0", "1.2.0", regia_net::NET_PROTOCOL_VERSION, 0, &packs)
}

fn speedrun(version: Option<&str>) -> ModsVfs {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert("levels/fast.level.ron".into(), b"(a: 1)".to_vec());
    if let Some(v) = version {
        files.insert(
            "luamod.ron".into(),
            format!(
                "(format_version: 1, id: \"speedrun\", name: \"Speedrun\", version: \"{v}\", \
                 entry: \"main.lua\", api_version: 1)"
            )
            .into_bytes(),
        );
        files.insert("main.lua".into(), Vec::new());
    }
    let mut vfs = ModsVfs::new();
    vfs.mount("speedrun", ModSource::Memory(files)).expect("mount");
    vfs
}

fn modded_app(mode: NetMode, with_dlc: bool, version: Option<&str>) -> App {
    let mut app = net_app(mode, Some(speedrun(version)));
    app.world_mut()
        .resource_mut::<NetSession>()
        .set_manifest(manifest(with_dlc));
    app
}

/// Spec 77 acceptance 3 end to end: the server runs `base` + `dlc_harbor` and mod `speedrun`
/// 1.0.0; a client without the DLC pak and with `speedrun` 1.1.0 is refused with
/// `content_mismatch` naming exactly those two items, the mod with both versions. Each side's
/// session manifest carries the version its mod declares.
#[test]
fn a_client_missing_a_dlc_pak_with_another_mod_version_gets_exactly_two_items() {
    let mut pair = Pair::connect(
        7812,
        modded_app(NetMode::DedicatedServer, true, Some("1.0.0")),
        modded_app(NetMode::Client, false, Some("1.1.0")),
    );
    assert!(pair.run_until(300, |p| matches!(
        p.client_state(),
        ClientSessionState::Disconnected(_)
    )));
    let server_mods = pair
        .server
        .world()
        .resource::<NetSession>()
        .session_manifest()
        .mods;
    assert_eq!(server_mods.len(), 1);
    assert_eq!(server_mods[0].version, "1.0.0", "the server's mod version");
    let client_mods = pair
        .client
        .world()
        .resource::<NetSession>()
        .session_manifest()
        .mods;
    assert_eq!(client_mods[0].version, "1.1.0", "the client's mod version");
    let state = pair.client_state();
    let ClientSessionState::Disconnected(Some(DisconnectReason::ContentMismatch(detail))) = state else {
        panic!("expected content_mismatch, got {state:?}");
    };
    let lines: Vec<&str> = detail.lines().collect();
    assert_eq!(
        lines,
        [
            "mod:speedrun: different (client 1.1.0, server 1.0.0)",
            "pack:dlc_harbor: missing on client",
        ],
        "exactly the two differing items"
    );
}

/// Spec 77 § 4.1 (ADR-7259): a mod without a declared version (no `luamod.ron`) has empty version
/// text on both sides and joins a server that runs the same mod.
#[test]
fn a_mod_without_a_declared_version_joins_a_matching_server() {
    let mut pair = Pair::connect(
        7813,
        modded_app(NetMode::DedicatedServer, true, None),
        modded_app(NetMode::Client, true, None),
    );
    assert!(
        pair.run_until(300, |p| p.client_state() == ClientSessionState::InGame),
        "joins: {:?}",
        pair.client_state()
    );
    let mods = pair
        .client
        .world()
        .resource::<NetSession>()
        .session_manifest()
        .mods;
    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0].id, "speedrun");
    assert_eq!(mods[0].version, "", "no declared version");
}
