// SPDX-License-Identifier: MPL-2.0
//! Remote players through the framework (spec 70 §§ 4 and 5.2, ADR-7212), two apps over the
//! in-memory network with manual session clocks:
//!
//! - a joining peer becomes a `Player` (`PlayerKind::Remote`) through `PlayerManager`, with a
//!   `PlayerController`, on its `NetPlayer` entity; the client app's `LocalPeer` is set from the
//!   welcome; leaving frees the `PlayerManager` slot;
//! - `PlayerController::possess` on a remote player's controller makes the pawn owned by that peer
//!   (`NetOwner`), `unpossess` removes it, and possessing another pawn moves it.

use std::net::SocketAddr;
use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::player::{Player, PlayerController, PlayerKind, PlayerManager};
use regia_framework::rules::RulesPlugin;
use regia_net::model::{LocalPeer, NetMode, NetOwner, NetPlayer, NetPlayerId, PeerId};
use regia_net::session::lifecycle::ClientSessionState;
use regia_net::session::net_session::ConnectOptions;
use regia_net::session::{HostOptions, NetSession};
use regia_net::socket::LoopbackNet;
use regia_net::{NetPlugin, NetSessionPlugin};

/// One fixed step per `update`, as the session's manual clock advances 16 ms per `step`: the
/// possession owner sync runs in `FixedUpdate`, which under the default real-time strategy ran or
/// not depending on how long the previous update took (integ-post05b, ADR-7310).
fn fixed_step_clock(app: &mut App) {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed);
}

fn server_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((
        NetPlugin {
            mode: NetMode::DedicatedServer,
        },
        NetSessionPlugin,
        RulesPlugin,
    ));
    app.insert_resource(PlayerManager::new(4));
    fixed_step_clock(&mut app);
    app.world_mut().resource_mut::<NetSession>().set_manual_clock(0);
    app
}

fn client_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((
        NetPlugin {
            mode: NetMode::Client,
        },
        NetSessionPlugin,
    ));
    fixed_step_clock(&mut app);
    app.world_mut().resource_mut::<NetSession>().set_manual_clock(0);
    app
}

fn step(server: &mut App, client: &mut App) {
    for app in [&mut *server, &mut *client] {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
    }
    client.update();
    server.update();
}

fn connect(port: u16) -> (App, App) {
    let mut server = server_app();
    let mut client = client_app();
    server.finish_plugins();
    client.finish_plugins();
    let net = LoopbackNet::new();
    let saddr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
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
    for _ in 0..300 {
        step(&mut server, &mut client);
        if client.world().resource::<NetSession>().state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(
        client.world().resource::<NetSession>().state(),
        ClientSessionState::InGame
    );
    // One more round so the server applies the client's InGame.
    step(&mut server, &mut client);
    (server, client)
}

fn local_peer(client: &App) -> PeerId {
    client
        .world()
        .get_resource::<LocalPeer>()
        .and_then(|p| p.0)
        .expect("LocalPeer set from the welcome")
}

fn remote_player(server: &App, peer: PeerId) -> Entity {
    server
        .world()
        .resource::<NetSession>()
        .player_entity(NetPlayerId { peer, local: 0 })
        .expect("player entity")
}

#[test]
fn a_joining_peer_is_a_remote_player_through_the_player_manager() {
    let (mut server, mut client) = connect(7793);
    let peer = local_peer(&client);
    let entity = remote_player(&server, peer);
    let player = server
        .world()
        .get::<Player>(entity)
        .cloned()
        .expect("the NetPlayer entity is a Player");
    assert_eq!(player.kind, PlayerKind::Remote);
    assert_eq!(
        server.world().get::<NetPlayer>(entity).map(|p| p.id),
        Some(NetPlayerId { peer, local: 0 })
    );
    assert!(
        server
            .world()
            .get::<PlayerController>(player.controller)
            .is_some()
    );
    let pm = server.world().resource::<PlayerManager>();
    assert_eq!(pm.count(), 1);
    assert_eq!(pm.player_entity(player.index), Some(entity));
    assert!(pm.can_join(), "a remote player takes no local split-screen seat");

    // Leaving frees the slot and the controller.
    client.world_mut().resource_mut::<NetSession>().disconnect();
    for _ in 0..10 {
        step(&mut server, &mut client);
    }
    assert_eq!(server.world().resource::<PlayerManager>().count(), 0);
    assert!(server.world().get_entity(player.controller).is_err());
}

#[test]
fn possession_sets_and_removes_the_pawns_owner() {
    let (mut server, mut client) = connect(7794);
    let peer = local_peer(&client);
    let entity = remote_player(&server, peer);
    let controller = server
        .world()
        .get::<Player>(entity)
        .map(|p| p.controller)
        .expect("player");
    let pawn = server.world_mut().spawn_empty().id();
    let other = server.world_mut().spawn_empty().id();

    let possess = |server: &mut App, target: Option<Entity>| {
        let world = server.world_mut();
        let mut pc = world.get_mut::<PlayerController>(controller).expect("controller");
        match target {
            Some(t) => pc.possess(t),
            None => pc.unpossess(),
        }
    };
    possess(&mut server, Some(pawn));
    step(&mut server, &mut client);
    assert_eq!(server.world().get::<NetOwner>(pawn), Some(&NetOwner(peer)));

    possess(&mut server, Some(other));
    step(&mut server, &mut client);
    assert_eq!(server.world().get::<NetOwner>(other), Some(&NetOwner(peer)));
    assert_eq!(
        server.world().get::<NetOwner>(pawn),
        None,
        "the old pawn is released"
    );

    possess(&mut server, None);
    step(&mut server, &mut client);
    assert_eq!(
        server.world().get::<NetOwner>(other),
        None,
        "unpossess removes the owner"
    );
}
