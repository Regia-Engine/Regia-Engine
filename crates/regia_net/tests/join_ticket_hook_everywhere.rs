// SPDX-License-Identifier: MPL-2.0
//! KI-NT-047 (ADR-10069): the store's join ticket hook installs on every server type, not only a
//! bare `NetServer`: a `NetSession` host (listen and dedicated), the dedicated `ServerHost`, and
//! a `NetLink` server. In-memory `LoopbackNet`, manual clocks, fixed steps.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::model::NetMode;
use regia_net::session::NetSession;
use regia_net::session::lifecycle::{ClientSessionState, HostOptions, PeerState, SessionSettings};
use regia_net::session::manifest::{ContentManifest, ManifestAsset};
use regia_net::session::net_session::ConnectOptions;
use regia_net::session::proto::DisconnectReason;
use regia_net::socket::LoopbackNet;
use regia_net::*;

/// A store: `good*` admits, `pend*` is pending (a later refusal may come), anything else
/// (an empty ticket too) is refused. Records every verdict and every end.
#[derive(Default)]
struct Store {
    verdicts: Mutex<Vec<Vec<u8>>>,
    ended: Mutex<Vec<Vec<u8>>>,
    late: Mutex<Vec<Vec<u8>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl JoinTicketCheck for Store {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        match self.verdict(ticket) {
            TicketVerdict::Refuse => Err(TicketRefused),
            _ => Ok(()),
        }
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        lock(&self.verdicts).push(ticket.to_vec());
        if ticket.starts_with(b"good") {
            TicketVerdict::Admit
        } else if ticket.starts_with(b"pend") {
            TicketVerdict::Pending
        } else {
            TicketVerdict::Refuse
        }
    }

    fn end_ticket(&self, ticket: &[u8]) {
        lock(&self.ended).push(ticket.to_vec());
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *lock(&self.late))
    }
}

fn manifest() -> ContentManifest {
    let base = vec![ManifestAsset {
        id: "levels/arena.level.ron".into(),
        kind: "level".into(),
        content_hash: 11,
    }];
    ContentManifest::build("0.5.0", "1.0.0", 1, 0xabc, &[("base", &base)])
}

fn host_opts(password: &str) -> HostOptions {
    HostOptions {
        level: "levels/arena.level.ron".into(),
        password: password.into(),
        ..HostOptions::default()
    }
}

fn settings() -> SessionSettings {
    SessionSettings {
        reconnect_grace_s: 2.0,
        ..SessionSettings::default()
    }
}

/// A session client of `server` sending `ticket` (`None`: no ticket).
fn client(
    net: &LoopbackNet,
    server: SocketAddr,
    ticket: Option<&[u8]>,
    password: Option<&str>,
) -> NetSession {
    let mut c = NetSession::new(settings(), manifest());
    c.set_manual_clock(0);
    // An opener on the in-memory network, so a resume reopens there (not UDP).
    let opener: regia_net::session::SocketOpener = {
        let net = net.clone();
        Arc::new(move || {
            net.bind("127.0.0.1:0".parse().expect("addr"))
                .map(|s| Box::new(s) as Box<dyn regia_net::DatagramSocket + Send>)
        })
    };
    c.connect_via(
        opener,
        server,
        ConnectOptions {
            auth: ticket.map(<[u8]>::to_vec),
            password: password.map(str::to_owned),
            allow_unpinned: true,
            ..ConnectOptions::default()
        },
    )
    .expect("connect");
    c
}

/// Steps the server and the clients until `done` or 400 steps of 16 ms.
fn drive(
    server: &mut NetSession,
    clients: &mut [&mut NetSession],
    done: impl Fn(&[&mut NetSession]) -> bool,
) {
    for _ in 0..400 {
        server.advance_manual_clock(16);
        server.update();
        for c in clients.iter_mut() {
            c.advance_manual_clock(16);
            c.update();
        }
        if done(clients) {
            return;
        }
    }
}

fn refused_reason(c: &NetSession) -> Option<DisconnectReason> {
    match c.state() {
        ClientSessionState::Disconnected(reason) => reason,
        _ => None,
    }
}

fn peer_of(c: &mut NetSession) -> regia_net::model::PeerId {
    c.take_client_events()
        .iter()
        .find_map(|e| match e {
            regia_net::session::lifecycle::ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
            _ => None,
        })
        .expect("welcomed")
}

#[test]
fn a_net_session_host_checks_the_ticket_kicks_a_late_refusal_and_ends_it() {
    let net = LoopbackNet::new();
    let saddr: SocketAddr = "127.0.0.1:7841".parse().expect("addr");
    let store = Arc::new(Store::default());
    let mut server = NetSession::new(settings(), manifest());
    server.set_manual_clock(0);
    server.set_join_ticket_check(Some(store.clone()));
    server
        .host_on(net.bind(saddr).expect("bind"), host_opts(""), true)
        .expect("host");

    // No ticket and a bad ticket: refused with the store's code.
    let mut none = client(&net, saddr, None, None);
    let mut bad = client(&net, saddr, Some(b"forged"), None);
    drive(&mut server, &mut [&mut none, &mut bad], |c| {
        c.iter().all(|c| refused_reason(c).is_some())
    });
    for c in [&none, &bad] {
        assert_eq!(
            refused_reason(c),
            Some(DisconnectReason::AuthFailed(TicketRefused::CODE.into())),
            "a join without a good ticket is refused with ticket_refused"
        );
    }
    assert!(
        lock(&store.verdicts).contains(&Vec::new()),
        "an empty ticket reached the hook"
    );

    // A good and a pending ticket join.
    let mut good = client(&net, saddr, Some(b"good-1"), None);
    let mut pend = client(&net, saddr, Some(b"pend-1"), None);
    drive(&mut server, &mut [&mut good, &mut pend], |c| {
        c.iter().all(|c| c.state() == ClientSessionState::InGame)
    });
    assert_eq!(good.state(), ClientSessionState::InGame);
    assert_eq!(pend.state(), ClientSessionState::InGame);
    let good_peer = peer_of(&mut good);

    // The store refuses the pending ticket late: that player is kicked, the other stays.
    lock(&store.late).push(b"pend-1".to_vec());
    drive(&mut server, &mut [&mut good, &mut pend], |c| {
        refused_reason(c[1]).is_some()
    });
    assert_eq!(
        refused_reason(&pend),
        Some(DisconnectReason::Kicked(TicketRefused::CODE.into()))
    );
    assert_eq!(good.state(), ClientSessionState::InGame);
    assert!(
        lock(&store.ended).contains(&b"pend-1".to_vec()),
        "the kicked player's ticket ended"
    );

    // A suspend keeps the slot in grace: the ticket is not ended; the resume checks it again.
    good.suspend();
    server.advance_manual_clock(16);
    server.update();
    assert_eq!(server.peer_state(good_peer), Some(PeerState::Grace));
    assert!(
        !lock(&store.ended).contains(&b"good-1".to_vec()),
        "ended while in grace"
    );
    let before = lock(&store.verdicts).len();
    assert!(good.resume().expect("resume"));
    drive(&mut server, &mut [&mut good], |c| {
        c[0].state() == ClientSessionState::InGame
    });
    assert_eq!(good.state(), ClientSessionState::InGame);
    assert!(lock(&store.verdicts).len() > before, "a resume is checked too");
    assert!(
        !lock(&store.ended).contains(&b"good-1".to_vec()),
        "a resume with the same ticket keeps the store session"
    );

    // Gone for good (past the grace): the ticket ends.
    good.suspend();
    for _ in 0..200 {
        server.advance_manual_clock(16);
        server.update();
    }
    assert_eq!(server.peer_state(good_peer), None);
    assert!(
        lock(&store.ended).contains(&b"good-1".to_vec()),
        "the departed player's ticket ended"
    );

    // In-process loopback players carry no ticket and are not checked.
    let verdicts = lock(&store.verdicts).len();
    server
        .add_loopback_clients(1, regia_net::session::hub::SimInput::Idle)
        .expect("loopback");
    for _ in 0..60 {
        server.advance_manual_clock(16);
        server.update();
    }
    assert_eq!(
        lock(&store.verdicts).len(),
        verdicts,
        "a loopback player reached the store"
    );
}

#[test]
fn the_password_runs_before_the_session_ticket_hook() {
    let net = LoopbackNet::new();
    let saddr: SocketAddr = "127.0.0.1:7842".parse().expect("addr");
    let store = Arc::new(Store::default());
    let mut server = NetSession::new(settings(), manifest());
    server.set_manual_clock(0);
    server
        .host_on(net.bind(saddr).expect("bind"), host_opts("hunter2"), true)
        .expect("host");
    // Installed on a running server: it takes effect at once.
    server.set_join_ticket_check(Some(store.clone()));
    assert!(server.server().expect("server").has_ticket_check());
    let mut wrong = client(&net, saddr, Some(b"good-2"), Some("guess"));
    drive(&mut server, &mut [&mut wrong], |c| refused_reason(c[0]).is_some());
    assert_eq!(refused_reason(&wrong), Some(DisconnectReason::BadPassword));
    assert!(
        lock(&store.verdicts).is_empty(),
        "a wrong password reached the ticket hook"
    );
    let mut right = client(&net, saddr, Some(b"good-2"), Some("hunter2"));
    drive(&mut server, &mut [&mut right], |c| {
        c[0].state() == ClientSessionState::InGame
    });
    assert_eq!(right.state(), ClientSessionState::InGame);
    assert_eq!(*lock(&store.verdicts), vec![b"good-2".to_vec()]);
}

#[test]
fn the_dedicated_host_hands_its_hook_to_the_session() {
    use regia_net::server_host::{CliOverrides, HostTransport, ServerHost, SessionOnlyGame, TestClock};
    let dir = regia_test_utils::unique_temp_dir("ticket-hook-host");
    regia_test_utils::write_atomic(
        &dir.join("server.toml"),
        b"format_version = 1\n[server]\nname = \"Tickets\"\nport = 7843\nmax_players = 8\n\
         [levels]\nrotation = [\"levels/arena.level.ron\"]\n[status]\nenabled = false\n",
    )
    .expect("server.toml");
    let store = Arc::new(Store::default());
    let mut game = SessionOnlyGame::new(NetSession::new(settings(), manifest()));
    let mut host = ServerHost::new(
        &dir,
        CliOverrides::default(),
        Box::new(|_| None),
        Box::new(TestClock::new()),
    )
    .expect("config");
    host.set_join_ticket_check(store);
    host.start(&mut game, HostTransport::Local).expect("start");
    assert!(
        game.session.server().expect("hosting").has_ticket_check(),
        "the dedicated host's session has no ticket hook"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A link game whose join data is the store ticket.
struct Ticketed;
impl NetGame for Ticketed {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = Vec<u8>;
    const GAME_ID: &'static str = "test.ticket_hook_link";
    const PROTOCOL_VERSION: u32 = 1;

    fn join_ticket(data: &Self::JoinData) -> &[u8] {
        data
    }
}

fn link_app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

fn manual() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: 16_000,
        },
        ..LinkOptions::default()
    }
}

#[test]
fn a_net_link_server_checks_the_ticket_its_clients_send() {
    let net = LoopbackNet::new();
    let saddr: SocketAddr = "127.0.0.1:7844".parse().expect("addr");
    let store = Arc::new(Store::default());
    let mut server = link_app(NetMode::DedicatedServer);
    {
        let mut link = server.world_mut().resource_mut::<NetLink>();
        assert!(
            !link.set_ticket_check(store.clone()),
            "an idle link installs nothing"
        );
        link.host::<Ticketed>(Box::new(net.bind(saddr).expect("bind")), &manual())
            .expect("host");
        assert!(link.set_ticket_check(store.clone()));
    }
    let connect = |ticket: &[u8]| {
        let mut c = link_app(NetMode::Client);
        c.world_mut()
            .resource_mut::<NetLink>()
            .connect_with_join::<Ticketed>(
                Box::new(net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind")),
                saddr,
                &manual(),
                ticket.to_vec(),
            )
            .expect("connect");
        c
    };
    let mut bad = connect(b"forged");
    let mut good = connect(b"good-3");
    let mut connected = (false, false);
    for _ in 0..90 {
        server.update();
        bad.update();
        good.update();
        connected.0 |= bad.world().resource::<NetLink>().is_connected();
        connected.1 |= good.world().resource::<NetLink>().is_connected();
    }
    assert!(!connected.0, "a forged ticket joined the link server");
    assert!(connected.1, "a good ticket did not join the link server");
    let seen = lock(&store.verdicts).clone();
    assert!(
        seen.contains(&b"forged".to_vec()) && seen.contains(&b"good-3".to_vec()),
        "{seen:?}"
    );
}
