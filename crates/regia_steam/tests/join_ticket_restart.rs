// SPDX-License-Identifier: MPL-2.0
//! Steam join tickets through `NetServer` admission (net-rev-b #13, ADR-8502).
//!
//! The Steam calls go to a fake that keeps Steam's rule: one auth session per Steam id, and a
//! second `BeginAuthSession` for an open id is `DuplicateRequest`. Time is a step counter;
//! nothing sleeps and nothing reads the wall clock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use regia_net::LoopbackSocket;
use regia_net::prelude::*;
use regia_net::ticket::{JoinTicketCheck, TicketRefused, TicketVerdict};
use regia_steam::{BeginAuthError, SteamAuthApi, SteamAuthSessions, encode_steam_auth_ticket};

const STEP_MS: u64 = 10;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Game whose join data is the store ticket frame.
struct TicketGame;

impl NetGame for TicketGame {
    type Message = ();
    type Input = ();
    type JoinData = Vec<u8>;
    const GAME_ID: &'static str = "regia.test.steam_ticket";
    const PROTOCOL_VERSION: u32 = 1;

    fn join_ticket(data: &Vec<u8>) -> &[u8] {
        data
    }
}

type Sessions = Arc<Mutex<HashMap<u64, Vec<u8>>>>;

/// Steam's session table.
struct FakeSteam {
    open: Sessions,
}

impl SteamAuthApi for FakeSteam {
    fn begin_auth_session(&self, steam_id: u64, ticket: &[u8]) -> Result<(), BeginAuthError> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if open.contains_key(&steam_id) {
            return Err(BeginAuthError::DuplicateRequest);
        }
        open.insert(steam_id, ticket.to_vec());
        Ok(())
    }

    fn end_auth_session(&self, steam_id: u64) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&steam_id);
    }
}

fn open_sessions(open: &Sessions) -> HashMap<u64, Vec<u8>> {
    open.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn connect(net: &LoopbackNet, port: u16, ticket: Vec<u8>) -> NetClient<TicketGame, LoopbackSocket> {
    NetClient::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(port),
        ClientConfig {
            name: "ada".into(),
            ..ClientConfig::default()
        },
        &ticket,
    )
    .expect("join fits")
}

fn drive(
    now: &mut u64,
    steps: u32,
    server: &mut NetServer<TicketGame, LoopbackSocket>,
    clients: &mut [&mut NetClient<TicketGame, LoopbackSocket>],
) {
    for _ in 0..steps {
        *now += STEP_MS;
        for client in clients.iter_mut() {
            client.update(*now);
        }
        server.update(*now);
    }
}

/// A client crashes and starts again on a new port while its old peer is still connected. Steam
/// answers the new ticket `DuplicateRequest`. The restart is admitted, the stale peer is kicked,
/// and the stale peer leaving does not end the new session.
#[test]
fn a_client_that_restarts_on_a_new_port_is_admitted_and_the_stale_peer_is_kicked() {
    let net = LoopbackNet::new();
    let open: Sessions = Arc::default();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7801)).expect("bind server"),
        ServerConfig::default(),
        [21; 32],
    );
    server.set_ticket_check(SteamAuthSessions::new(FakeSteam {
        open: Arc::clone(&open),
    }));
    let mut now = 0u64;
    let before = encode_steam_auth_ticket(5, b"before-crash");
    let after = encode_steam_auth_ticket(5, b"after-restart");

    let mut crashed = connect(&net, 7801, before);
    drive(&mut now, 100, &mut server, &mut [&mut crashed]);
    assert!(crashed.is_connected(), "first join, state {:?}", crashed.state());
    assert_eq!(server.peer_rows().len(), 1);

    // The process died: `crashed` is never updated again, and its peer is still live on the
    // server when the restarted client joins from a new port.
    let mut restarted = connect(&net, 7801, after.clone());
    drive(&mut now, 100, &mut server, &mut [&mut restarted]);
    assert!(
        restarted.is_connected(),
        "a restart on a new port is admitted, state {:?}",
        restarted.state()
    );
    assert_eq!(
        server.peer_rows().len(),
        1,
        "the stale peer was kicked, rows {:?}",
        server.peer_rows()
    );
    assert_eq!(
        open_sessions(&open).get(&5),
        Some(&b"after-restart".to_vec()),
        "the restarted client holds the only session"
    );

    restarted.disconnect();
    drive(&mut now, 20, &mut server, &mut [&mut restarted]);
    assert!(server.peer_rows().is_empty(), "the restarted client left");
    assert!(
        open_sessions(&open).is_empty(),
        "leaving ends the session, open {:?}",
        open_sessions(&open)
    );
}

/// A late `ValidateAuthTicketResponse` refusal kicks the player on the next receive. A refusal
/// that arrives after the player left does not kick the rejoin (net-rev-b #12).
#[test]
fn a_validate_refusal_after_leave_does_not_kick_the_rejoin() {
    let net = LoopbackNet::new();
    let open: Sessions = Arc::default();
    let sessions = Arc::new(SteamAuthSessions::new(FakeSteam {
        open: Arc::clone(&open),
    }));
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7802)).expect("bind server"),
        ServerConfig::default(),
        [22; 32],
    );
    server.set_ticket_check(Shared(Arc::clone(&sessions)));
    let mut now = 0u64;

    let mut first = connect(&net, 7802, encode_steam_auth_ticket(6, b"one"));
    drive(&mut now, 100, &mut server, &mut [&mut first]);
    assert!(first.is_connected(), "first join, state {:?}", first.state());
    first.disconnect();
    drive(&mut now, 20, &mut server, &mut [&mut first]);
    assert!(open_sessions(&open).is_empty(), "leaving ends the session");
    // Steam's callback for the first session was still queued.
    sessions.on_validate_response(6, false);

    let mut rejoin = connect(&net, 7802, encode_steam_auth_ticket(6, b"two"));
    drive(&mut now, 100, &mut server, &mut [&mut rejoin]);
    assert!(
        rejoin.is_connected(),
        "a refusal for an ended session does not kick the rejoin, state {:?}",
        rejoin.state()
    );

    sessions.on_validate_response(6, false);
    drive(&mut now, 20, &mut server, &mut [&mut rejoin]);
    match rejoin.state() {
        ClientState::Ended(end) => assert_eq!(end.as_str(), "kicked", "a live refusal kicks, got {end:?}"),
        other => panic!("expected a kick, got {other:?}"),
    }
    assert!(open_sessions(&open).is_empty(), "the kick ends the session");
}

/// Lets the test keep a handle to the sessions the server owns.
struct Shared(Arc<SteamAuthSessions<FakeSteam>>);

impl JoinTicketCheck for Shared {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        JoinTicketCheck::check_ticket(&*self.0, ticket)
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        JoinTicketCheck::verdict(&*self.0, ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        JoinTicketCheck::end_ticket(&*self.0, ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        JoinTicketCheck::take_refusals(&*self.0)
    }
}
