// SPDX-License-Identifier: MPL-2.0
//! Lobby identity wiring (spec 89 § 4, ADR-7881): `LobbyPlugin` takes the lobby identity from the
//! app's `NetSession` manifest, the one the content handshake sends, so apps with different
//! manifests never list each other's lobbies and apps with the same manifest do; a `Lobbies` with
//! no identity refuses to create, list or matchmake unless the game opts in. Local backend, fixed
//! steps, no wall time.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_net::lobby::{LobbyError, LobbyId, LobbyIdentity, LobbyQuery, LobbySpec, hash_hex, keys};
use regia_net::session::ContentManifest;
use regia_net::{Lobbies, LobbyPlugin, LocalLobbyHub, NetSession, NetSessionPlugin};

/// An app on `hub` whose `NetSession` carries `manifest`. The game never calls
/// `Lobbies::set_identity`: the plugin must wire it.
fn app(hub: &LocalLobbyHub, id: u64, manifest: &ContentManifest) -> App {
    let mut app = App::new();
    app.insert_resource(Lobbies::new(Arc::new(hub.member(id, &format!("p{id}")))));
    let mut session = NetSession::default();
    session.set_manifest(manifest.clone());
    session.set_manual_clock(0);
    app.insert_resource(session);
    app.add_plugins((NetSessionPlugin, LobbyPlugin));
    app.finish_plugins();
    app
}

fn manifest(content: &str) -> ContentManifest {
    let asset = regia_net::session::ManifestAsset {
        id: content.into(),
        kind: "level".into(),
        content_hash: 7,
    };
    ContentManifest::build(
        "0.5.1",
        "1.0.0",
        1,
        0x5eed,
        &[("base", std::slice::from_ref(&asset))],
    )
}

fn create(app: &mut App) -> LobbyId {
    {
        let mut lobbies = app.world_mut().resource_mut::<Lobbies>();
        let spec = lobbies.default_spec();
        lobbies.create(spec).expect("create");
    }
    app.update();
    app.world()
        .resource::<Lobbies>()
        .current()
        .expect("the local hub creates at once")
}

fn listed(app: &mut App) -> Vec<LobbyId> {
    app.world_mut()
        .resource_mut::<Lobbies>()
        .list(LobbyQuery::default())
        .expect("list");
    app.update();
    match app.world().resource::<Lobbies>().last_list() {
        Some(Ok(found)) => found.iter().map(|l| l.id).collect(),
        other => panic!("list answered {other:?}"),
    }
}

#[test]
fn apps_with_different_manifests_never_see_each_others_lobbies() {
    let hub = LocalLobbyHub::new();
    let ours = manifest("units/tank");
    let theirs = manifest("units/other_tank");
    assert_ne!(ours.content_hash, theirs.content_hash);
    let mut host = app(&hub, 1, &ours);
    let mut same = app(&hub, 2, &ours);
    let mut other = app(&hub, 3, &theirs);
    let lobby = create(&mut host);
    assert_eq!(listed(&mut same), [lobby], "the same manifest lists the lobby");
    assert_eq!(
        listed(&mut other),
        Vec::<LobbyId>::new(),
        "a different manifest must not list it"
    );
    // The keys are the session manifest's, the values the content handshake compares.
    let data = &hub.lobbies()[0].data;
    assert_eq!(data[keys::CONTENT], hash_hex(ours.content_hash));
    assert_eq!(data[keys::PROTOCOL], hash_hex(ours.protocol_hash));
}

#[test]
fn the_plugin_follows_the_session_manifest_and_keeps_an_explicit_identity() {
    let hub = LocalLobbyHub::new();
    let first = manifest("a");
    let mut a = app(&hub, 1, &first);
    {
        let lobbies = a.world().resource::<Lobbies>();
        assert_eq!(
            lobbies.identity(),
            LobbyIdentity::Session,
            "set at finish, before Startup"
        );
        assert_eq!(lobbies.identity_values().2, first.content_hash);
    }
    // Mods mounted at session start change the session's manifest: the lobby filter follows.
    let second = manifest("b");
    a.world_mut()
        .resource_mut::<NetSession>()
        .set_manifest(second.clone());
    a.update();
    assert_eq!(
        a.world().resource::<Lobbies>().identity_values().2,
        second.content_hash
    );
    // A game that set its identity keeps it.
    a.world_mut()
        .resource_mut::<Lobbies>()
        .set_identity("example.arena", &first);
    a.update();
    let lobbies = a.world().resource::<Lobbies>();
    assert_eq!(lobbies.identity(), LobbyIdentity::Explicit);
    assert_eq!(
        lobbies.identity_values(),
        ("example.arena", first.protocol_hash, first.content_hash)
    );
}

#[test]
fn the_protocol_hash_matches_the_handshake_after_finish() {
    // A session manifest with no protocol hash: `NetSessionPlugin` fills it in at `finish`, and the
    // lobby identity must carry the same value whatever order the plugins finished in.
    let hub = LocalLobbyHub::new();
    for lobby_first in [false, true] {
        let mut app = App::new();
        app.insert_resource(Lobbies::new(Arc::new(hub.member(9, "p9"))));
        let mut session = NetSession::default();
        session.set_manifest(ContentManifest::build("0.5.1", "1.0.0", 1, 0, &[]));
        app.insert_resource(session);
        if lobby_first {
            app.add_plugins((LobbyPlugin, NetSessionPlugin));
        } else {
            app.add_plugins((NetSessionPlugin, LobbyPlugin));
        }
        app.finish_plugins();
        let handshake = app.world().resource::<NetSession>().manifest().protocol_hash;
        assert_ne!(handshake, 0);
        assert_eq!(
            app.world().resource::<Lobbies>().identity_values().1,
            handshake,
            "lobby_first = {lobby_first}"
        );
    }
}

#[test]
fn no_identity_refuses_create_list_and_matchmaking_unless_opted_in() {
    let hub = LocalLobbyHub::new();
    // No NetSession: nothing to take the identity from.
    let mut app = App::new();
    app.insert_resource(Lobbies::new(Arc::new(hub.member(1, "p1"))));
    app.add_plugins(LobbyPlugin);
    app.finish_plugins();
    app.update();
    {
        let mut lobbies = app.world_mut().resource_mut::<Lobbies>();
        assert_eq!(lobbies.identity(), LobbyIdentity::Unset);
        let spec = lobbies.default_spec();
        assert_eq!(lobbies.create(spec), Err(LobbyError::NoIdentity));
        assert_eq!(lobbies.list(LobbyQuery::default()), Err(LobbyError::NoIdentity));
        let request = lobbies.default_request();
        assert_eq!(lobbies.start_matchmaking(request), Err(LobbyError::NoIdentity));
        assert_eq!(LobbyError::NoIdentity.code(), "lobby_no_identity");
        assert!(lobbies.current().is_none());
    }
    assert!(hub.lobbies().is_empty(), "nothing reached the backend");
    // The explicit opt-in restores the old behavior.
    app.world_mut().resource_mut::<Lobbies>().allow_unidentified(true);
    let lobby = create(&mut app);
    assert_eq!(hub.lobbies()[0].data[keys::GAME], "");
    // A game id alone is an identity too.
    let mut named = App::new();
    let mut lobbies = Lobbies::new(Arc::new(hub.member(2, "p2")));
    lobbies.set_game("example.arena");
    named.insert_resource(lobbies);
    named.add_plugins(LobbyPlugin);
    named.finish_plugins();
    assert_eq!(listed(&mut named), Vec::<LobbyId>::new(), "another game id");
    assert_ne!(lobby, LobbyId(0));
}

#[test]
fn an_unavailable_backend_still_answers_unavailable() {
    let mut lobbies = Lobbies::default();
    assert_eq!(lobbies.list(LobbyQuery::default()), Err(LobbyError::Unavailable));
    assert_eq!(lobbies.create(LobbySpec::default()), Err(LobbyError::Unavailable));
}
