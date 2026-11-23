// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 10.3 (S7, ADR-8180): a Steam lobby client that the platform suspends resumes
//! over Steam sockets, not UDP, and keeps its `PeerId`. Two apps with `SteamLobbyPlugin` on the
//! shared mock network matchmake and launch; the guest is suspended and resumed through the
//! `Lifecycle` schedule, the way the platform layer drives it. No SDK, no wall time.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_input::InputContexts;
use regia_net::lobby::LobbyServer;
use regia_net::session::lifecycle::{ClientSessionState, HostOptions, PeerState};
use regia_net::{Lobbies, NetSession, NetSessionPlugin};
use regia_platform::{AppLifecycle, LifecycleEvent};
use regia_steam::{
    MockSteam, MockSteamNet, SteamBackend, SteamId, SteamLobbyPlugin, SteamPlugin, SteamSettings,
    steam_peer_addr,
};

fn user(net: &MockSteamNet, id: u64, name: &str) -> Arc<MockSteam> {
    let mock = MockSteam::shared();
    mock.join_net(net, SteamId(id), name);
    mock
}

fn app(mock: &Arc<MockSteam>) -> App {
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings().with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>),
    );
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    app.insert_resource(session);
    app.add_plugins((NetSessionPlugin, SteamLobbyPlugin::default()));
    app.finish_plugins();
    app
}

fn step(apps: &mut [&mut App], n: usize) {
    for _ in 0..n {
        for app in apps.iter_mut() {
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            app.update();
        }
    }
}

fn lifecycle(app: &mut App, event: LifecycleEvent) {
    app.world_mut().resource_mut::<AppLifecycle>().last_event = Some(event);
    app.run_lifecycle();
    app.world_mut().resource_mut::<AppLifecycle>().last_event = None;
}

fn guest_peer(app: &App) -> Option<regia_net::PeerId> {
    app.world()
        .resource::<NetSession>()
        .client()
        .and_then(|c| c.peer_id())
}

#[test]
fn a_suspended_steam_lobby_client_resumes_over_steam_with_the_same_peer_id() {
    let net = MockSteamNet::new();
    let ann = user(&net, 76_561_190_000_000_110, "Ann");
    let bob = user(&net, 76_561_190_000_000_120, "Bob");
    let mut host = app(&ann);
    let mut guest = app(&bob);
    step(&mut [&mut host, &mut guest], 1);
    {
        let mut l = host.world_mut().resource_mut::<Lobbies>();
        let mut req = l.default_request();
        req.rounds.truncate(1);
        l.start_matchmaking(req).expect("matchmaking");
    }
    step(&mut [&mut host], 3);
    {
        let mut l = guest.world_mut().resource_mut::<Lobbies>();
        let req = l.default_request();
        l.start_matchmaking(req).expect("matchmaking");
    }
    step(&mut [&mut host, &mut guest], 3);
    let server = host
        .world_mut()
        .resource_scope(|world, mut lobbies: Mut<Lobbies>| {
            let mut session = world.resource_mut::<NetSession>();
            lobbies.launch_hosting(&mut session, HostOptions::default())
        })
        .expect("launch");
    assert_eq!(server, LobbyServer::Steam(76_561_190_000_000_110));
    step(&mut [&mut host, &mut guest], 150);
    assert_eq!(
        guest.world().resource::<NetSession>().state(),
        ClientSessionState::InGame
    );
    let peer = guest_peer(&guest).expect("the guest was welcomed");
    let steam_server = steam_peer_addr(SteamId(76_561_190_000_000_110), 0);

    // The platform suspends the guest: the `suspended` goodbye leaves over Steam and the host
    // holds the grace slot.
    lifecycle(&mut guest, LifecycleEvent::Suspended);
    step(&mut [&mut host], 3);
    assert_eq!(
        host.world()
            .resource::<NetSession>()
            .server()
            .and_then(|s| s.peer_state(peer)),
        Some(PeerState::Grace)
    );

    // Resumed: the session reconnects over the transport it connected with.
    lifecycle(&mut guest, LifecycleEvent::Resumed);
    step(&mut [&mut host, &mut guest], 300);
    let session = guest.world().resource::<NetSession>();
    assert_eq!(
        session.state(),
        ClientSessionState::InGame,
        "the resumed client reached the game over Steam"
    );
    // Only a Steam socket reaches this synthetic address: a UDP socket times out.
    assert_eq!(session.server_addr(), Some(steam_server));
    assert_eq!(guest_peer(&guest), Some(peer), "the same PeerId");
    let host_session = host.world().resource::<NetSession>();
    let core = host_session.server().expect("host");
    assert_eq!(core.peer_ids(), vec![peer], "no second peer on the host");
    assert_eq!(core.peer_state(peer), Some(PeerState::InGame));
}
