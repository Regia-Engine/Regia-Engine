// SPDX-License-Identifier: MPL-2.0
//! ADR-8733 (amends ADR-3283): a player that leaves holding a reconnect ticket keeps its slot of
//! the per-address cap for the resume grace, and its return with the ticket is never counted as a
//! new peer of its address. Every client here shares 127.0.0.1, one address block.

mod common;

use std::net::SocketAddr;

use common::Duel;
use common::Team;
use regia_net::LoopbackSocket;
use regia_net::prelude::*;

type Server = NetServer<Duel, LoopbackSocket>;
type Client = NetClient<Duel, LoopbackSocket>;

const STEP_MS: u64 = 10;
const GRACE_MS: u64 = 60_000;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn server(net: &LoopbackNet, cap: usize, grace_ms: u64) -> Server {
    let config = ServerConfig {
        max_peers_per_ip: cap,
        resume_grace_ms: grace_ms,
        ..ServerConfig::default()
    };
    NetServer::with_secret(net.bind(addr(7777)).expect("bind server"), config, [7; 32])
}

fn client(net: &LoopbackNet, name: &str, resume: Option<[u8; 16]>) -> Client {
    let config = ClientConfig {
        name: name.into(),
        resume,
        ..ClientConfig::default()
    };
    NetClient::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(7777),
        config,
        &Team(0),
    )
    .expect("join fits")
}

/// Steps everyone until `done` or `max_steps`; returns the clock.
fn run(
    server: &mut Server,
    clients: &mut [&mut Client],
    from: u64,
    max_steps: u64,
    mut done: impl FnMut(&[&mut Client]) -> bool,
) -> u64 {
    let mut now = from;
    for _ in 0..max_steps {
        if done(clients) {
            return now;
        }
        now += STEP_MS;
        for c in clients.iter_mut() {
            c.update(now);
        }
        server.update(now);
        while server.poll_event().is_some() {}
    }
    now
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

/// Joins `c` alone and returns the clock once it has connected or ended.
fn settle(s: &mut Server, c: &mut Client, from: u64) -> u64 {
    run(s, &mut [c], from, 200, |cs| {
        cs[0].is_connected() || matches!(cs[0].state(), ClientState::Ended(_))
    })
}

/// Says goodbye and lets the server read it.
fn leave(s: &mut Server, c: &mut Client, from: u64) -> u64 {
    c.disconnect();
    run(s, &mut [c], from, 10, |_| false)
}

/// The cgnat soak row in miniature: the waiting player takes the slot of one that left, and the
/// one that left still comes back with its ticket. The block ends one over its cap, never more.
#[test]
fn a_player_that_left_with_a_ticket_comes_back_over_a_taken_slot() {
    let net = LoopbackNet::new();
    let mut s = server(&net, 2, GRACE_MS);
    let mut a = client(&net, "ada", None);
    let mut now = settle(&mut s, &mut a, 0);
    let mut b = client(&net, "bob", None);
    now = settle(&mut s, &mut b, now);
    assert!(a.is_connected() && b.is_connected());
    let mut c = client(&net, "cal", None);
    now = settle(&mut s, &mut c, now);
    assert!(refused_full(&c), "the cap holds: {:?}", c.state());

    let ticket = a.resume_ticket().expect("the server keeps tickets");
    now = leave(&mut s, &mut a, now);
    assert_eq!(s.session().len(), 1);
    let mut c = client(&net, "cal", None);
    now = settle(&mut s, &mut c, now);
    assert!(
        c.is_connected(),
        "the waiting player borrows the held slot: {:?}",
        c.state()
    );

    let mut back = client(&net, "ada", Some(ticket));
    now = settle(&mut s, &mut back, now);
    assert!(back.is_connected(), "the holder comes back: {:?}", back.state());
    assert_eq!(s.session().len(), 3, "cap 2 + one slot of slack");
    assert_eq!(s.totals().held_slot_returns, 1);
    assert_eq!(
        s.totals().resumes_refused,
        0,
        "a held-slot ticket is not a refused resume"
    );

    let mut d = client(&net, "dee", None);
    settle(&mut s, &mut d, now);
    assert!(
        refused_full(&d),
        "a fresh join stays bound by the cap: {:?}",
        d.state()
    );
}

/// Two holders away at once lend one slot between them; both still come back.
#[test]
fn two_holders_away_lend_one_slot() {
    let net = LoopbackNet::new();
    let mut s = server(&net, 2, GRACE_MS);
    let mut a = client(&net, "ada", None);
    let mut now = settle(&mut s, &mut a, 0);
    let mut b = client(&net, "bob", None);
    now = settle(&mut s, &mut b, now);
    let (ta, tb) = (
        a.resume_ticket().expect("ticket"),
        b.resume_ticket().expect("ticket"),
    );
    now = leave(&mut s, &mut a, now);
    now = leave(&mut s, &mut b, now);
    let mut c = client(&net, "cal", None);
    now = settle(&mut s, &mut c, now);
    assert!(c.is_connected(), "{:?}", c.state());
    let mut d = client(&net, "dee", None);
    now = settle(&mut s, &mut d, now);
    assert!(refused_full(&d), "only one held slot is lent: {:?}", d.state());
    let mut a2 = client(&net, "ada", Some(ta));
    now = settle(&mut s, &mut a2, now);
    let mut b2 = client(&net, "bob", Some(tb));
    settle(&mut s, &mut b2, now);
    assert!(
        a2.is_connected() && b2.is_connected(),
        "{:?} {:?}",
        a2.state(),
        b2.state()
    );
    assert_eq!(s.session().len(), 3);
}

/// A hold ends with the grace: the ticket then buys nothing and the cap binds the return.
#[test]
fn a_hold_lapses_with_the_grace() {
    let net = LoopbackNet::new();
    let mut s = server(&net, 2, 1_000);
    let mut a = client(&net, "ada", None);
    let mut now = settle(&mut s, &mut a, 0);
    let mut b = client(&net, "bob", None);
    now = settle(&mut s, &mut b, now);
    let ticket = a.resume_ticket().expect("ticket");
    now = leave(&mut s, &mut a, now);
    let mut c = client(&net, "cal", None);
    now = settle(&mut s, &mut c, now);
    assert!(c.is_connected());
    // Past the grace (the others keep talking meanwhile).
    now = run(&mut s, &mut [&mut b, &mut c], now, 150, |_| false);
    let mut back = client(&net, "ada", Some(ticket));
    settle(&mut s, &mut back, now);
    assert!(refused_full(&back), "{:?}", back.state());
    assert_eq!(s.session().len(), 2);
}

/// A server that keeps no tickets (`resume_grace_ms` 0) holds nothing: the per-address cap is
/// exactly ADR-3283's.
#[test]
fn without_tickets_the_cap_counts_live_peers_only() {
    let net = LoopbackNet::new();
    let mut s = server(&net, 2, 0);
    let mut a = client(&net, "ada", None);
    let mut now = settle(&mut s, &mut a, 0);
    let mut b = client(&net, "bob", None);
    now = settle(&mut s, &mut b, now);
    assert_eq!(a.resume_ticket(), None);
    now = leave(&mut s, &mut a, now);
    let mut c = client(&net, "cal", None);
    now = settle(&mut s, &mut c, now);
    assert!(c.is_connected());
    let mut back = client(&net, "ada", None);
    settle(&mut s, &mut back, now);
    assert!(refused_full(&back), "{:?}", back.state());
}

/// A live takeover (the ticket of a peer still connected) at a full address moves that peer to
/// the new connection; it is not a new peer of the address (before ADR-8733 the peer it replaces
/// counted against the cap and the takeover was refused `full`).
#[test]
fn a_live_takeover_at_a_full_address_is_not_a_new_peer() {
    let net = LoopbackNet::new();
    let mut s = server(&net, 1, GRACE_MS);
    let mut a = client(&net, "ada", None);
    let now = settle(&mut s, &mut a, 0);
    let ticket = a.resume_ticket().expect("ticket");
    let peer = a.peer_id().expect("peer");
    let mut again = client(&net, "ada", Some(ticket));
    settle(&mut s, &mut again, now);
    assert!(again.is_connected(), "{:?}", again.state());
    assert_eq!(again.peer_id(), Some(peer), "the same peer, resumed");
    assert_eq!(s.session().len(), 1);
}
