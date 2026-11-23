// SPDX-License-Identifier: MIT-0
//! Lobbies for {{name}} (spec 89): players gather in a lobby, mark themselves ready, and the
//! owner publishes the dedicated server's address; every member, the owner too, then connects its
//! [`NetLink`] there.
//!
//! The backend is whatever [`Lobbies`] holds: [`regia::net::LocalLobbyHub`] in tests and LAN
//! builds, Steam's lobbies through `regia::steam` in a Steam build. A Steam id server
//! ([`LobbyServer::Steam`]) is reached through `regia::steam::SteamSocket` and
//! [`NetLink::connect`]. [`follow_lobby`] does that for you when the app holds a
//! [`LobbySteamTransport`] (in a Steam build: `regia::steam::SteamLobbyTransport`); without one a
//! Steam id launch is refused with a logged `no_transport`, never retried.

use std::net::SocketAddr;
use std::sync::Arc;

use regia::net as regia_net;
use regia::prelude::*;
use regia_net::Lobbies;
use regia_net::link::{LinkError, LinkOptions, NetLink};
use regia_net::lobby::{LobbyError, LobbyServer, LobbyTransport};

use crate::NetGameWire;

/// Owner: publishes the dedicated server at `server` once every member is ready.
///
/// # Errors
///
/// Not in a lobby, not its owner, or the backend refused the write. `Ok(false)` while some member
/// is not ready yet.
pub fn launch_when_ready(lobbies: &mut Lobbies, server: SocketAddr) -> Result<bool, LobbyError> {
    if !lobbies.ready_to_launch() {
        return Ok(false);
    }
    lobbies.launch(LobbyServer::Address(server))?;
    Ok(true)
}

/// The UDP server the current lobby launched to, if any.
pub fn launched_server(lobbies: &Lobbies) -> Option<SocketAddr> {
    match lobbies.snapshot().and_then(|s| s.server) {
        Some(LobbyServer::Address(addr)) => Some(addr),
        _ => None,
    }
}

/// What [`follow_lobby`] connects with.
#[derive(Resource, Debug, Clone)]
pub struct LobbyLinkOptions(pub LinkOptions);

/// How [`follow_lobby`] reaches a [`LobbyServer::Steam`] launch: the transport that opens the
/// socket and maps the Steam id to its address (`regia::steam::SteamLobbyTransport` in a Steam
/// build). Insert it beside [`Lobbies`] only in builds that ship on Steam.
#[derive(Resource, Clone)]
pub struct LobbySteamTransport(pub Arc<dyn LobbyTransport>);

impl std::fmt::Debug for LobbySteamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LobbySteamTransport")
            .field(&self.0.name())
            .finish()
    }
}

/// The server the current lobby launched to, of either kind.
pub fn launched_target(lobbies: &Lobbies) -> Option<LobbyServer> {
    lobbies.snapshot().and_then(|s| s.server)
}

/// `PreUpdate`, after the lobby pump: an idle link connects to the server the lobby launched to,
/// over UDP for an address and over Steam sockets for a Steam id. One attempt per launch; a
/// refusal is logged, not retried every frame.
pub fn follow_lobby(world: &mut World) {
    let Some(server) = world.get_resource::<Lobbies>().and_then(launched_target) else {
        return;
    };
    let Some(link) = world.get_resource::<NetLink>() else {
        return;
    };
    if link.role() != "idle" {
        return;
    }
    let opts = world
        .get_resource::<LobbyLinkOptions>()
        .map_or_else(LinkOptions::default, |o| o.0.clone());
    let transport = world.get_resource::<LobbySteamTransport>().map(|t| t.0.clone());
    let result = match (server, transport) {
        (LobbyServer::Address(addr), _) => world
            .resource_mut::<NetLink>()
            .connect_udp::<NetGameWire>(addr, &opts),
        (LobbyServer::Steam(_), None) => Err(LinkError::Bind("no_transport".into())),
        (steam @ LobbyServer::Steam(_), Some(transport)) => connect_steam(world, &*transport, steam, &opts),
    };
    match result {
        Ok(()) => info!(target: "{{crate_name}}", ?server, "lobby launched: connecting"),
        Err(e) => {
            warn!(target: "{{crate_name}}", ?server, code = e.code(), "lobby launched, connect failed: {e}");
            // Forget the lobby so the failure is not retried every frame.
            if let Some(mut lobbies) = world.get_resource_mut::<Lobbies>() {
                lobbies.leave();
            }
        }
    }
}

/// Connects the world's link to a Steam id server through `transport`.
fn connect_steam(
    world: &mut World,
    transport: &dyn LobbyTransport,
    server: LobbyServer,
    opts: &LinkOptions,
) -> Result<(), LinkError> {
    let Some(addr) = transport.peer_addr(&server) else {
        return Err(LinkError::Bind("no_transport".into()));
    };
    let socket = transport.open().map_err(|e| LinkError::Bind(e.to_string()))?;
    world
        .resource_mut::<NetLink>()
        .connect::<NetGameWire>(socket, addr, opts)
}

/// Adds lobby following to a client app: [`regia::net::LobbyPlugin`] and [`follow_lobby`]. Insert
/// [`Lobbies`] with your backend before adding it.
pub struct NetGameLobbyPlugin;

impl Plugin for NetGameLobbyPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_net::LobbyPlugin);
        // This game connects a `NetLink`, not a `NetSession`, so the lobby plugin has no session
        // manifest to take the lobby identity from: name the game, so only its own lobbies are
        // listed (`lobby_no_identity` otherwise). A game that set an identity keeps it.
        if let Some(mut lobbies) = app.world_mut().get_resource_mut::<Lobbies>()
            && lobbies.identity_values().0.is_empty()
        {
            lobbies.set_game(<NetGameWire as regia_net::NetGame>::GAME_ID);
        }
        // The lobby pump runs after the net receive; a connect made here is read on the next frame.
        app.add_systems(PreUpdate, follow_lobby.after(regia_net::lobby::lobby_system));
    }
}
