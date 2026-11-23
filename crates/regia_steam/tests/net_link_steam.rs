// SPDX-License-Identifier: MPL-2.0
//! ADR-2039: an app's replication over Steam sockets through `regia_net::link::NetLink`, on the
//! shared mock network. The same `NetLink::host` / `NetLink::connect` the template uses over UDP
//! take a `SteamSocket`; synthetic Steam addresses are not loopback, so the default
//! `CryptoMode::Auto` seals the link. No SDK, no wall time.

use std::net::SocketAddr;

use regia_app::prelude::*;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::replication::register_replicated;
use regia_net::runtime::{NetEntities, NetNode};
use regia_net::{
    NetGame, NetId, NetMode, NetPlugin, Replicate, ReplicateOptions, Replicated, TransformReplication,
};
use regia_steam::{MockSteam, MockSteamNet, SteamBackend, SteamId, SteamSocket, steam_peer_addr};

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.steam_link";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp(u32);

fn app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    register_replicated::<Hp>(app.world_mut(), ReplicateOptions::default());
    app
}

fn opts() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 16_667,
        },
        ..LinkOptions::default()
    }
}

#[test]
fn replication_runs_sealed_over_steam_sockets() {
    let net = MockSteamNet::new();
    let host_id = SteamId(76_561_190_000_000_011);
    let host = MockSteam::shared();
    host.join_net(&net, host_id, "Ann");
    let guest = MockSteam::shared();
    guest.join_net(&net, SteamId(76_561_190_000_000_012), "Bob");

    let mut server = app(NetMode::DedicatedServer);
    let mut client = app(NetMode::Client);
    let host_socket = SteamSocket::open(host.p2p().expect("p2p"), 0).expect("socket");
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<Game>(Box::new(host_socket), &opts())
        .expect("host");
    let server_addr: SocketAddr = steam_peer_addr(host_id, 0);
    let guest_socket = SteamSocket::open(guest.p2p().expect("p2p"), 0).expect("socket");
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<Game>(Box::new(guest_socket), server_addr, &opts())
        .expect("connect");

    let e = server
        .world_mut()
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Hp(7),
        ))
        .id();
    for _ in 0..120 {
        server.update();
        client.update();
    }
    assert!(client.world().resource::<NetLink>().is_connected());
    assert!(
        client.world().resource::<NetLink>().is_sealed(),
        "a Steam link is sealed"
    );
    assert_eq!(server.world().resource::<NetNode>().peers().len(), 1);
    let id = *server.world().get::<NetId>(e).expect("net id");
    let copy = client
        .world()
        .resource::<NetEntities>()
        .entity(id)
        .expect("replicated over Steam");
    assert_eq!(client.world().get::<Hp>(copy), Some(&Hp(7)));
}

/// A Steam socket whose path can be cut both ways (the long-outage profile's 100 % two-way loss).
struct GatedSteam {
    socket: SteamSocket,
    down: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl regia_net::socket::DatagramSocket for GatedSteam {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> std::io::Result<()> {
        if self.down.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.down.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// Spec 120 § 10.3 (S7, ADR-8352): a `NetLink` client on Steam sockets that times out reconnects
/// by itself over Steam, not UDP (the Steam server has no UDP address to reach), with its resume
/// ticket, and keeps its `PeerId`. 45 s of two-way loss: 25 s past the 20 s timeout. Both the
/// link's own socket and an opener that opens a fresh Steam socket per attempt.
#[test]
fn a_timed_out_steam_client_reconnects_over_steam_with_the_same_peer_id() {
    use regia_net::link::{LinkEvent, LinkSocketOpener};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    for opener in [false, true] {
        let net = MockSteamNet::new();
        let host_id = SteamId(76_561_190_000_000_031 + u64::from(opener) * 10);
        let guest_id = SteamId(76_561_190_000_000_032 + u64::from(opener) * 10);
        let host = MockSteam::shared();
        host.join_net(&net, host_id, "Ann");
        let guest = MockSteam::shared();
        guest.join_net(&net, guest_id, "Bob");
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        let host_socket = SteamSocket::open(host.p2p().expect("p2p"), 0).expect("socket");
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(host_socket), &opts())
            .expect("host");
        let server_addr: SocketAddr = steam_peer_addr(host_id, 0);
        let down = std::sync::Arc::new(AtomicBool::new(false));
        let opened = std::sync::Arc::new(AtomicU32::new(0));
        {
            let mut link = client.world_mut().resource_mut::<NetLink>();
            if opener {
                let (guest, down, opened) = (guest.clone(), down.clone(), opened.clone());
                let open: LinkSocketOpener = std::sync::Arc::new(move || {
                    opened.fetch_add(1, Ordering::Relaxed);
                    let p2p = guest.p2p().ok_or_else(|| std::io::Error::other("no p2p"))?;
                    Ok(Box::new(GatedSteam {
                        socket: SteamSocket::open(p2p, 0).map_err(std::io::Error::other)?,
                        down: down.clone(),
                    }))
                });
                link.connect_via::<Game>(open, server_addr, &opts())
                    .expect("connect");
            } else {
                let socket = SteamSocket::open(guest.p2p().expect("p2p"), 0).expect("socket");
                link.connect::<Game>(
                    Box::new(GatedSteam {
                        socket,
                        down: down.clone(),
                    }),
                    server_addr,
                    &opts(),
                )
                .expect("connect");
            }
        }
        let mut client_events = Vec::new();
        let step = |server: &mut App, client: &mut App, n: u32, events: &mut Vec<LinkEvent>| {
            for _ in 0..n {
                server.update();
                client.update();
                events.extend(client.world_mut().resource_mut::<NetLink>().take_events());
            }
        };
        step(&mut server, &mut client, 180, &mut client_events);
        let peer = client
            .world()
            .resource::<NetLink>()
            .status()
            .and_then(|s| s.peer_id)
            .expect("connected over Steam");
        client_events.clear();
        down.store(true, Ordering::Relaxed);
        step(&mut server, &mut client, 45 * 60, &mut client_events);
        assert!(
            client.world().resource::<NetLink>().is_reconnecting(),
            "opener {opener}: {client_events:?}"
        );
        down.store(false, Ordering::Relaxed);
        step(&mut server, &mut client, 2 * 60, &mut client_events);
        let link = client.world().resource::<NetLink>();
        assert!(
            link.is_connected(),
            "opener {opener}: back within 2 s: {client_events:?}"
        );
        assert_eq!(
            link.status().and_then(|s| s.peer_id),
            Some(peer),
            "the same PeerId"
        );
        assert!(link.is_sealed());
        // The server sees it at its Steam address: the reconnect went over Steam.
        let addrs = server.world().resource::<NetLink>().peer_addrs();
        assert_eq!(
            addrs,
            vec![(regia_net::PeerId(peer), steam_peer_addr(guest_id, 0))],
            "opener {opener}"
        );
        let count = |f: &dyn Fn(&LinkEvent) -> bool| client_events.iter().filter(|e| f(e)).count();
        assert_eq!(
            count(&|e| matches!(e, LinkEvent::PeerInterrupted { .. })),
            1,
            "{client_events:?}"
        );
        assert_eq!(
            count(&|e| matches!(e, LinkEvent::PeerRestored { .. })),
            1,
            "{client_events:?}"
        );
        assert_eq!(
            count(&|e| matches!(e, LinkEvent::Ended { .. })),
            0,
            "{client_events:?}"
        );
        let attempts = count(&|e| matches!(e, LinkEvent::Reconnecting { .. })) as u32;
        assert!(attempts >= 4, "{client_events:?}");
        if opener {
            assert_eq!(
                opened.load(Ordering::Relaxed),
                1 + attempts,
                "a fresh Steam socket per attempt"
            );
        }
    }
}
