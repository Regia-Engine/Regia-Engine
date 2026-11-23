// SPDX-License-Identifier: MPL-2.0
//! Proof tests for the per-address hold at a59d76329 (ADR-8733 (1)).
//!
//! Two of them assert the property a held slot should have and fail on that head: the goodbye
//! ticket is spent inside `admit` as soon as any join presents it, before that client confirms,
//! and a join from another address block spends it too. A forged ticket does neither.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use regia_net::client::{ClientConfig, NetClient};
use regia_net::server::{NetServer, ServerConfig, ServerEvent};
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::{ClientEnd, ClientState, CryptoMode, DenyReason, NetGame, PeerId};

#[derive(Debug)]
struct Game;

impl NetGame for Game {
    type Message = u32;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.address_hold_replay";
    const PROTOCOL_VERSION: u32 = 1;
}

type Ticket = [u8; 16];
type Client = NetClient<Game, DeafSocket>;

/// Drops every inbound datagram once `deaf` is set, so an accept can be lost.
struct DeafSocket {
    socket: LoopbackSocket,
    deaf: Arc<AtomicBool>,
}

impl DatagramSocket for DeafSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.deaf.load(Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

struct Rig {
    net: LoopbackNet,
    server: NetServer<Game, LoopbackSocket>,
    server_addr: SocketAddr,
    now: u64,
    joined: Vec<PeerId>,
}

impl Rig {
    fn new(port: u16) -> Self {
        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], port));
        let server = NetServer::with_secret(
            net.bind(server_addr).expect("bind"),
            ServerConfig {
                max_peers_per_ip: 1,
                resume_grace_ms: 60_000,
                crypto: CryptoMode::Auto,
                ..ServerConfig::default()
            },
            [9; 32],
        );
        Self {
            net,
            server,
            server_addr,
            now: 0,
            joined: Vec::new(),
        }
    }

    fn client_at(&self, ip: [u8; 4], resume: Option<Ticket>) -> (Client, Arc<AtomicBool>) {
        let deaf = Arc::new(AtomicBool::new(false));
        let socket = DeafSocket {
            socket: self.net.bind(SocketAddr::from((ip, 0))).expect("bind"),
            deaf: deaf.clone(),
        };
        let config = ClientConfig {
            resume,
            crypto: CryptoMode::Auto,
            ..ClientConfig::default()
        };
        let client = NetClient::connect(socket, self.server_addr, config, &()).expect("connect");
        (client, deaf)
    }

    fn step(&mut self, clients: &mut [&mut Client]) {
        self.now += 10;
        for c in clients.iter_mut() {
            c.update(self.now);
        }
        self.server.update(self.now);
        while let Some(e) = self.server.poll_event() {
            if let ServerEvent::PeerJoined { peer, .. } = e {
                self.joined.push(peer);
            }
        }
    }

    fn until(
        &mut self,
        steps: u64,
        clients: &mut [&mut Client],
        mut done: impl FnMut(&Self, &[&mut Client]) -> bool,
    ) {
        for _ in 0..steps {
            if done(self, clients) {
                return;
            }
            self.step(clients);
        }
    }

    fn settle(&mut self, c: &mut Client) {
        self.until(200, &mut [c], |_, cs| {
            cs[0].is_connected() || matches!(cs[0].state(), ClientState::Ended(_))
        });
    }

    fn leave(&mut self, c: &mut Client) {
        c.disconnect();
        for _ in 0..10 {
            self.step(&mut [c]);
        }
    }
}

fn refused_full(c: &Client) -> bool {
    matches!(
        c.state(),
        ClientState::Ended(ClientEnd::Denied {
            reason: DenyReason::Full,
            ..
        })
    )
}

/// Cap 1, one address. Ada leaves holding T. Bob borrows the one slack slot, so a fresh join is
/// refused and only T still gets in. Ada's return is admitted, then the accept is lost. The
/// retry still holds only T, so it must be that same peer again: spending T before the client
/// confirms turns the retry into a fresh join, which this full block refuses.
#[test]
fn a_lost_accept_retry_of_a_held_slot_ticket_is_the_same_peer() {
    let mut rig = Rig::new(7601);
    let (mut ada, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut ada);
    assert!(ada.is_connected(), "{:?}", ada.state());
    let ticket = ada.resume_ticket().expect("ticket");
    rig.leave(&mut ada);
    assert_eq!(rig.server.session().len(), 0, "the goodbye freed the live peer");

    let (mut bob, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut bob);
    assert!(
        bob.is_connected(),
        "bob borrows the one held slot: {:?}",
        bob.state()
    );
    assert_eq!(rig.server.totals().held_slot_returns, 0);

    let (mut lost, deaf) = rig.client_at([127, 0, 0, 1], Some(ticket));
    let joins_before = rig.joined.len();
    for _ in 0..200 {
        rig.step(&mut [&mut bob, &mut lost]);
        if rig.joined.len() > joins_before {
            deaf.store(true, Ordering::Relaxed);
            break;
        }
    }
    let ghost = *rig.joined.last().expect("the held-slot return was admitted");
    assert!(
        lost.peer_id().is_none(),
        "the accept was lost: {:?}",
        lost.state()
    );
    assert_eq!(rig.server.session().len(), 2, "bob and the unheard return");
    // The first admit already spent T. Drop the deaf attempt so its resends stay quiet, and
    // present T again from a new socket, the way a lost accept retries (new nonce, same ticket).
    drop(lost);
    let (mut retry, _) = rig.client_at([127, 0, 0, 1], Some(ticket));
    rig.until(200, &mut [&mut bob, &mut retry], |_, cs| {
        cs[1].is_connected() || matches!(cs[1].state(), ClientState::Ended(_))
    });
    assert!(
        retry.is_connected(),
        "retry with the unconfirmed ticket must be admitted, not {:?}",
        retry.state()
    );
    assert_eq!(
        retry.peer_id(),
        Some(ghost),
        "the retry is the unheard join, not a second peer"
    );
    assert_eq!(
        rig.server.session().len(),
        2,
        "a replay must not take another slot"
    );
    assert!(
        !refused_full(&retry),
        "the full block refused the ticket holder: {:?}",
        retry.state()
    );
}

/// The same ticket presented from another address block must not spend Ada's hold. Cal's own
/// block has room, so Cal is admitted as an ordinary join; Ada can still come back over Bob.
#[test]
fn a_ticket_from_another_block_does_not_spend_the_hold() {
    let mut rig = Rig::new(7602);
    let (mut ada, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut ada);
    let ticket = ada.resume_ticket().expect("ticket");
    rig.leave(&mut ada);

    let (mut bob, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut bob);
    assert!(bob.is_connected(), "{:?}", bob.state());

    let (mut cal, _) = rig.client_at([127, 0, 0, 2], Some(ticket));
    rig.settle(&mut cal);
    assert!(cal.is_connected(), "cal's own block has room: {:?}", cal.state());
    assert_eq!(
        rig.server.totals().held_slot_returns,
        0,
        "a ticket from another block spent the hold: {:?}",
        rig.server.totals()
    );

    let (mut back, _) = rig.client_at([127, 0, 0, 1], Some(ticket));
    rig.until(200, &mut [&mut bob, &mut cal, &mut back], |_, cs| {
        cs[2].is_connected() || matches!(cs[2].state(), ClientState::Ended(_))
    });
    assert!(
        back.is_connected(),
        "ada's hold must still be there: {:?}",
        back.state()
    );
    assert!(!refused_full(&back), "{:?}", back.state());
}

/// A ticket that is not the one the server minted does not match the hold, does not spend it,
/// and does not skip the cap once the slack slot is taken.
#[test]
fn a_forged_ticket_does_not_spend_or_skip_the_hold() {
    let mut rig = Rig::new(7603);
    let (mut ada, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut ada);
    let ticket = ada.resume_ticket().expect("ticket");
    rig.leave(&mut ada);

    let (mut bob, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut bob);
    assert!(bob.is_connected(), "{:?}", bob.state());

    let forged = [0x5a; 16];
    assert_ne!(forged, ticket);
    let (mut fake, _) = rig.client_at([127, 0, 0, 1], Some(forged));
    rig.settle(&mut fake);
    assert!(
        refused_full(&fake),
        "a forged ticket skipped the cap or was admitted: {:?}",
        fake.state()
    );
    assert_eq!(rig.server.totals().held_slot_returns, 0);

    let (mut back, _) = rig.client_at([127, 0, 0, 1], Some(ticket));
    rig.until(200, &mut [&mut bob, &mut back], |_, cs| {
        cs[1].is_connected() || matches!(cs[1].state(), ClientState::Ended(_))
    });
    assert!(
        back.is_connected(),
        "the real hold survived the forgery: {:?}",
        back.state()
    );
    assert_eq!(rig.server.totals().held_slot_returns, 1);
}

/// ADR-8883, the tail the review read but did not run: the unheard held-slot return times out
/// into dormancy. Its chain still carries the ticket it presented, so a later join with that
/// ticket resumes the same peer (not a fresh join the full block refuses), and the hold is
/// spent, and counted, only once that client confirms.
#[test]
fn a_held_slot_return_that_times_out_is_resumed_by_its_ticket() {
    let mut rig = Rig::new(7604);
    let (mut ada, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut ada);
    let ticket = ada.resume_ticket().expect("ticket");
    rig.leave(&mut ada);
    let (mut bob, _) = rig.client_at([127, 0, 0, 1], None);
    rig.settle(&mut bob);
    assert!(bob.is_connected(), "{:?}", bob.state());

    let (mut lost, deaf) = rig.client_at([127, 0, 0, 1], Some(ticket));
    let joins_before = rig.joined.len();
    for _ in 0..200 {
        rig.step(&mut [&mut bob, &mut lost]);
        if rig.joined.len() > joins_before {
            deaf.store(true, Ordering::Relaxed);
            break;
        }
    }
    let ghost = *rig.joined.last().expect("the held-slot return was admitted");
    drop(lost);
    assert_eq!(
        rig.server.totals().held_slot_returns,
        0,
        "nothing confirmed the return yet"
    );
    // Well past the silence timeout: the unheard peer is dormant, its slot held.
    for _ in 0..3_000 {
        rig.step(&mut [&mut bob]);
    }
    assert!(
        rig.server.dormant_peers().any(|(p, _)| p == ghost),
        "the unheard return went dormant"
    );
    assert!(bob.is_connected(), "{:?}", bob.state());

    let (mut back, _) = rig.client_at([127, 0, 0, 1], Some(ticket));
    rig.until(200, &mut [&mut bob, &mut back], |_, cs| {
        cs[1].is_connected() || matches!(cs[1].state(), ClientState::Ended(_))
    });
    assert!(
        back.is_connected(),
        "the ticket still names the dormant return: {:?}",
        back.state()
    );
    assert_eq!(back.peer_id(), Some(ghost));
    for _ in 0..50 {
        rig.step(&mut [&mut bob, &mut back]);
    }
    assert_eq!(rig.server.session().len(), 2);
    assert_eq!(
        rig.server.totals().held_slot_returns,
        1,
        "spent once, at the confirmation"
    );
}
