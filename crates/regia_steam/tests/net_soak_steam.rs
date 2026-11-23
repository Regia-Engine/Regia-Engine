// SPDX-License-Identifier: MPL-2.0
//! The Steam sockets transport under a bad network, without Steam or an account (ADR-7899).
//!
//! `SteamSocket` runs over a `P2pMessages` backed by the net soak's impairment network
//! (`regia_net_soak::impair`), so a server and four clients of the soak's sample game play over
//! Steam's synthetic addresses on the `bad-wifi` profile (jitter, random and burst loss, reorder,
//! duplication). Steam's own relay is not in the loop; this checks the adapter and the session over
//! it, the part the engine owns.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use regia_net::{ClientConfig, DatagramSocket, NetClient, NetServer, ServerConfig};
use regia_net_soak::game::{SoakGame, SoakInput};
use regia_net_soak::impair::{ImpairedNet, ImpairedSocket};
use regia_net_soak::profiles::Profile;
use regia_steam::{P2pMessages, SteamAccept, SteamError, SteamId, SteamSocket, steam_id_of, steam_peer_addr};

const CHANNEL: u16 = 7;

struct ImpairedP2p {
    me: SteamId,
    socket: Mutex<ImpairedSocket>,
}

impl P2pMessages for ImpairedP2p {
    fn local_id(&self) -> Option<SteamId> {
        Some(self.me)
    }

    fn send(&self, to: SteamId, channel: u16, bytes: &[u8]) -> Result<(), SteamError> {
        let mut s = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        // Like Steam, a send that cannot go is a lost datagram, not an error for the session.
        let _ = s.send_to(bytes, steam_peer_addr(to, channel));
        Ok(())
    }

    fn receive(&self, channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)> {
        let mut s = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        let mut buf = vec![0u8; 65_536];
        while out.len() < max {
            match s.recv_from(&mut buf) {
                Ok(Some((n, from))) => {
                    if let Some((id, ch)) = steam_id_of(from)
                        && ch == channel
                    {
                        out.push((id, buf[..n].to_vec()));
                    }
                }
                _ => break,
            }
        }
        out
    }

    fn set_accept(&self, _accept: SteamAccept) {}
}

fn steam_socket(net: &ImpairedNet, id: u64, profile: &Profile, duration_ms: u64) -> SteamSocket {
    let addr: SocketAddr = steam_peer_addr(SteamId(id), CHANNEL);
    let link = if id == 1 {
        Default::default()
    } else {
        profile.client_link(id as usize, duration_ms)
    };
    let ep = net.add_endpoint(addr, link).expect("free synthetic address");
    let p2p = ImpairedP2p {
        me: SteamId(id),
        socket: Mutex::new(net.socket(ep)),
    };
    SteamSocket::open(Arc::new(p2p), CHANNEL).expect("signed-in user")
}

#[test]
fn steam_sockets_hold_a_session_on_bad_wifi() {
    let duration_ms = 20_000;
    let profile = Profile::named("bad-wifi").expect("profile");
    let net = ImpairedNet::new(0x57EA);
    let server_socket = steam_socket(&net, 1, &profile, duration_ms);
    let server_addr = steam_peer_addr(SteamId(1), CHANNEL);
    let mut server: NetServer<SoakGame, SteamSocket> = NetServer::new(server_socket, ServerConfig::default());
    let mut clients: Vec<NetClient<SoakGame, SteamSocket>> = (2..6u64)
        .map(|id| {
            NetClient::connect(
                steam_socket(&net, id, &profile, duration_ms),
                server_addr,
                ClientConfig {
                    input_redundancy: 15,
                    ..ClientConfig::default()
                },
                &(id as u32),
            )
            .expect("join data fits")
        })
        .collect();
    let mut with_input = 0u64;
    let mut simulated = 0u64;
    let mut now = 0u64;
    while now <= duration_ms {
        net.advance_to_ms(now);
        if now.is_multiple_of(16) {
            for c in clients.iter_mut() {
                c.receive(now);
                while c.poll_event().is_some() {}
                if c.is_connected() {
                    let tick = c.estimated_server_tick(now) + 2;
                    let _ = c.send_input(
                        tick,
                        SoakInput {
                            dx: 1,
                            ..SoakInput::default()
                        },
                    );
                }
                c.flush(now);
            }
        }
        if now.is_multiple_of(32) {
            server.receive(now);
            while server.poll_event().is_some() {}
            while server.driver().tick() * 1000 / 30 <= now {
                let inputs = server.advance_tick(|_, _| {});
                if now > 5_000 {
                    simulated += server.session().len() as u64;
                    with_input += inputs.len() as u64;
                }
            }
            server.flush(now);
        }
        now += 2;
    }
    assert!(
        clients.iter().all(|c| c.is_connected()),
        "every client stays connected"
    );
    assert!(c_sealed(&clients), "the links are encrypted as shipped");
    assert_eq!(server.session().len(), 4);
    // Bad Wi-Fi loses about 3 % of datagrams one way; redundancy 15 covers every loss burst of the
    // profile, so nearly every tick has its input. The bar is the soak's: no client stops reaching
    // the server.
    assert!(
        with_input * 100 >= simulated * 95,
        "{with_input} of {simulated} peer-ticks had input"
    );
}

fn c_sealed(clients: &[NetClient<SoakGame, SteamSocket>]) -> bool {
    clients.iter().all(NetClient::is_sealed)
}
