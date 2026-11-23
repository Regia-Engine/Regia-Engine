// SPDX-License-Identifier: MPL-2.0
//! Connection grades across a resume (spec 120 § 10.3 and § 13.1, ADR-8889).
//!
//! A resumed peer, or a client that reconnected by itself (ADR-8352), runs on a fresh connection
//! whose quality tracker starts again at `Good`. The game heard `Interrupted` for the outage (once,
//! ADR-8182), so it must hear the change back: otherwise a HUD or a host's scoreboard that follows
//! `QualityChanged` shows `Interrupted` for the rest of the match.
//!
//! The long-outage profile of spec 120 § 15.1 over `NetLink` on the in-memory network with manual
//! clocks (no wall clock, no sleeps): two-way 100 % loss for 45 s, 25 s past the 20 s timeout.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::{LinkGrade, NetGame, NetMode, NetPlugin, PeerId};

#[derive(Debug)]
struct ResumeGame;

impl NetGame for ResumeGame {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.quality_resume";
    const PROTOCOL_VERSION: u32 = 1;
}

/// One app update of 60 Hz.
const STEP_US: u64 = 16_667;
const FRAMES_PER_S: u64 = 60;
const PORT: u16 = 7941;

fn options() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: STEP_US,
        },
        ..LinkOptions::default()
    }
}

fn app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

/// A socket whose path can be cut both ways.
struct GateSocket {
    socket: LoopbackSocket,
    down: Arc<AtomicBool>,
}

impl DatagramSocket for GateSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.down.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.down.load(Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// The grade changes in `events` for `peer`, in order.
fn grades(events: &[LinkEvent], peer: PeerId) -> Vec<(LinkGrade, LinkGrade)> {
    events
        .iter()
        .filter_map(|e| match e {
            LinkEvent::QualityChanged { peer: p, from, to } if *p == peer => Some((*from, *to)),
            _ => None,
        })
        .collect()
}

/// After a resume both ends report the grade back out of `Interrupted`: the client's link to the
/// server and the server's row for the resumed peer each show one `Interrupted` for the outage and
/// end on a grade that is not `Interrupted`.
#[test]
fn a_resume_reports_the_grade_back_from_interrupted_on_both_ends() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    let mut server = app(NetMode::DedicatedServer);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<ResumeGame>(Box::new(net.bind(server_addr).expect("bind")), &options())
        .expect("host");
    let down = Arc::new(AtomicBool::new(false));
    let mut client = app(NetMode::Client);
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<ResumeGame>(
            Box::new(GateSocket {
                socket: net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
                down: down.clone(),
            }),
            server_addr,
            &options(),
        )
        .expect("connect");
    let (mut server_events, mut client_events) = (Vec::new(), Vec::new());
    let mut run = |frames: u64, server: &mut App, client: &mut App| {
        for _ in 0..frames {
            server.update();
            server_events.extend(server.world_mut().resource_mut::<NetLink>().take_events());
            client.update();
            client_events.extend(client.world_mut().resource_mut::<NetLink>().take_events());
        }
    };
    run(3 * FRAMES_PER_S, &mut server, &mut client);
    let peer = client
        .world()
        .resource::<NetLink>()
        .status()
        .and_then(|s| s.peer_id)
        .map(PeerId)
        .expect("connected");
    down.store(true, Ordering::Relaxed);
    run(45 * FRAMES_PER_S, &mut server, &mut client);
    down.store(false, Ordering::Relaxed);
    run(10 * FRAMES_PER_S, &mut server, &mut client);
    assert!(
        client.world().resource::<NetLink>().is_connected(),
        "the client resumed: {client_events:?}"
    );
    assert!(
        server_events
            .iter()
            .any(|e| matches!(e, LinkEvent::Resumed { peer: p, .. } if *p == peer)),
        "the server took the peer back: {server_events:?}"
    );
    for (end, changes) in [
        ("client", grades(&client_events, PeerId::SERVER)),
        ("server", grades(&server_events, peer)),
    ] {
        let interrupted = changes
            .iter()
            .filter(|(_, to)| *to == LinkGrade::Interrupted)
            .count();
        assert_eq!(
            interrupted, 1,
            "{end}: one Interrupted for the outage: {changes:?}"
        );
        let last = changes.last().map(|(_, to)| *to);
        assert!(
            last.is_some_and(|g| g != LinkGrade::Interrupted),
            "{end}: the grade came back after the resume: {changes:?}"
        );
        // Every change starts from the grade the one before it showed.
        for pair in changes.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "{end}: a continuous sequence: {changes:?}");
        }
    }
}
