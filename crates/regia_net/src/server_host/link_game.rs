// SPDX-License-Identifier: MPL-2.0
//! Replication games under the host (ADR-2440, ADR-2039 point 8).
//!
//! A game whose traffic is replication frames over a [`NetLink`] (the `net-game` template) puts a
//! [`HostedLink`] resource in its app. When the host starts it, the **link** binds the game port,
//! with the host's `server.toml` applied: `net.encryption`, the server key from
//! `net.identity_file`, `server.max_players`, `server.password` and `server.tick_hz`. The host's
//! own [`NetSession`](crate::NetSession) then runs local (no socket): the console, remote-admin
//! plumbing, bans file, level rotation, `/health` and the agent's `server.*` methods work as
//! before, and the player count, idle slowdown, drain and `kick` follow the link's peers.
//!
//! A game without a [`HostedLink`] is hosted as before: the session binds the game port.

use std::net::SocketAddr;

use regia_ecs::prelude::{Resource, World, bevy_ecs};

use crate::bridge::NetFrame;
use crate::crypto::{CryptoMode, ServerIdentity};
use crate::link::{LinkError, LinkOptions, LinkSocket, NetLink};
use crate::payload::NetGame;
use crate::status::NetStatus;

/// What the host hands the game when the game binds the game port itself.
#[derive(Debug, Clone)]
pub struct GamePort {
    /// The host silence policy for the replication transport.
    pub timeout: crate::session::TimeoutPolicy,
    /// Where to bind (`server.bind:server.port`, or port 0 for
    /// [`HostTransport::UdpAnyPort`](super::HostTransport::UdpAnyPort)).
    pub addr: SocketAddr,
    /// `net.encryption`.
    pub crypto: CryptoMode,
    /// The server key from `net.identity_file` (`None` when encryption is off).
    pub identity: Option<ServerIdentity>,
    /// `server.max_players`.
    pub max_players: u32,
    /// `server.password` (`None` when empty).
    pub password: Option<String>,
    /// `server.tick_hz`.
    pub tick_hz: u32,
    /// `net.mtu_probing` (format v4, additive; ADR-8300).
    pub mtu_probing: bool,
}

/// Binds a link for game `G` (the type-erased part of [`HostedLink`]).
pub type LinkBinder = fn(&mut NetLink, SocketAddr, &LinkOptions) -> Result<(), LinkError>;

/// Hosts a link for game `G` on a socket the host hands over (the link half of a shared port,
/// ADR-2665 (6)).
pub type LinkSocketBinder = fn(&mut NetLink, LinkSocket, &LinkOptions) -> Result<(), LinkError>;

/// Called once when the host stops (the template prints its final avatars).
pub type StopHook = fn(&mut World);

fn bind_udp<G: NetGame<Message = NetFrame>>(
    link: &mut NetLink,
    addr: SocketAddr,
    opts: &LinkOptions,
) -> Result<(), LinkError> {
    link.host_udp::<G>(addr, opts)
}

fn bind_socket<G: NetGame<Message = NetFrame>>(
    link: &mut NetLink,
    socket: LinkSocket,
    opts: &LinkOptions,
) -> Result<(), LinkError> {
    link.host::<G>(socket, opts)
}

/// The resource that makes the host bind the game port with the app's [`NetLink`] (see the module
/// docs). Insert it in the server side of the game's plugin.
#[derive(Resource, Clone)]
pub struct HostedLink {
    options: LinkOptions,
    bind: LinkBinder,
    bind_on: LinkSocketBinder,
    /// `crate::payload::protocol_id::<G>()`, to route a shared port's connect requests.
    protocol: u64,
    on_stop: Option<StopHook>,
    /// The link's status just before the host closed the game port (ADR-3075).
    final_status: Option<NetStatus>,
}

impl std::fmt::Debug for HostedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedLink")
            .field("options", &self.options)
            .field("on_stop", &self.on_stop.is_some())
            .finish_non_exhaustive()
    }
}

impl HostedLink {
    /// The game `G` served over UDP with `options` as the base: the host replaces the crypto
    /// mode, identity, peer capacity, password and tick rate from `server.toml`; the clock, the
    /// conditioner and the label stay the game's.
    pub fn new<G: NetGame<Message = NetFrame>>(options: LinkOptions) -> Self {
        Self {
            options,
            bind: bind_udp::<G>,
            bind_on: bind_socket::<G>,
            protocol: crate::payload::protocol_id::<G>(),
            on_stop: None,
            final_status: None,
        }
    }

    /// The game's protocol id (what its clients' connect requests carry).
    pub fn protocol(&self) -> u64 {
        self.protocol
    }

    /// Runs `hook` on the game's world once the host has stopped.
    #[must_use]
    pub fn with_on_stop(mut self, hook: StopHook) -> Self {
        self.on_stop = Some(hook);
        self
    }

    /// The game's base options.
    pub fn options(&self) -> &LinkOptions {
        &self.options
    }

    /// The options the link is hosted with: the base with the host's settings applied.
    pub fn hosted_options(&self, port: &GamePort) -> LinkOptions {
        LinkOptions {
            timeout: port.timeout,
            crypto: port.crypto,
            identity: port.identity.clone(),
            max_peers: port.max_players.max(1) as usize,
            password: port.password.clone().filter(|p| !p.is_empty()),
            tick_rate_hz: port.tick_hz.max(1),
            server_key: None,
            mtu_probing: port.mtu_probing,
            ..self.options.clone()
        }
    }

    /// Binds `link` on the game port. Returns the bound address.
    ///
    /// # Errors
    ///
    /// The link's error (`link_running`, `link_bind_failed`, `link_password_plain`).
    pub fn bind(&self, link: &mut NetLink, port: &GamePort) -> Result<SocketAddr, LinkError> {
        (self.bind)(link, port.addr, &self.hosted_options(port))?;
        Ok(link.local_addr().unwrap_or(port.addr))
    }

    /// Hosts `link` on `socket` (the link half of a shared port, ADR-2665 (6)) with the host's
    /// settings applied. Returns the socket's address.
    ///
    /// # Errors
    ///
    /// The link's error (`link_running`, `link_password_plain`).
    pub fn bind_on(
        &self,
        link: &mut NetLink,
        socket: LinkSocket,
        port: &GamePort,
    ) -> Result<SocketAddr, LinkError> {
        let addr = socket.local_addr().unwrap_or(port.addr);
        (self.bind_on)(link, socket, &self.hosted_options(port))?;
        Ok(addr)
    }

    /// The stop hook, if any.
    pub fn on_stop(&self) -> Option<StopHook> {
        self.on_stop
    }

    /// The link's status as the host closed the game port: the stop hook runs after the close,
    /// when [`NetLink::status`] has nothing left, so a game reports its final counters from here.
    pub fn final_status(&self) -> Option<&NetStatus> {
        self.final_status.as_ref()
    }

    /// Records the link's status before the close (the host calls it).
    pub fn set_final_status(&mut self, status: Option<NetStatus>) {
        self.final_status = status;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::LinkClock;

    struct Wire;

    impl NetGame for Wire {
        type Message = NetFrame;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "test.hosted_link";
        const PROTOCOL_VERSION: u32 = 1;
    }

    fn port(addr: SocketAddr) -> GamePort {
        GamePort {
            timeout: crate::session::TimeoutPolicy::default(),
            addr,
            crypto: CryptoMode::Required,
            identity: None,
            max_players: 4,
            password: Some(String::new()),
            tick_hz: 30,
            mtu_probing: false,
        }
    }

    #[test]
    fn the_host_settings_replace_the_game_base_but_not_its_clock() {
        let base = LinkOptions {
            crypto: CryptoMode::Off,
            max_peers: 64,
            clock: LinkClock::Manual {
                start_ms: 0,
                step_us: 16_667,
            },
            label: "game".into(),
            ..LinkOptions::default()
        };
        let hosted = HostedLink::new::<Wire>(base);
        let opts = hosted.hosted_options(&port(SocketAddr::from(([127, 0, 0, 1], 0))));
        assert_eq!(opts.crypto, CryptoMode::Required);
        assert_eq!(opts.max_peers, 4);
        assert_eq!(opts.tick_rate_hz, 30);
        assert_eq!(opts.password, None, "an empty password is no password");
        assert_eq!(opts.label, "game");
        assert!(matches!(opts.clock, LinkClock::Manual { step_us: 16_667, .. }));
    }

    #[test]
    fn a_hosted_link_binds_a_free_port_and_refuses_a_second_bind() {
        let hosted = HostedLink::new::<Wire>(LinkOptions::default());
        let mut link = NetLink::default();
        let p = port(SocketAddr::from(([127, 0, 0, 1], 0)));
        let addr = hosted.bind(&mut link, &p).expect("binds");
        assert_ne!(addr.port(), 0, "the OS picked a port");
        assert_eq!(link.role(), "server");
        assert_eq!(
            hosted.bind(&mut link, &p).map_err(|e| e.code()),
            Err("link_running")
        );
        link.close();
        assert_eq!(link.role(), "idle");
    }
}
