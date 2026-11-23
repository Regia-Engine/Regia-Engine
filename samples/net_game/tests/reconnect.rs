// SPDX-License-Identifier: MIT-0
//! The template networked game's reconnecting overlay (spec 120 § 10.2 and § 10.3, ADR-8352): a
//! client loses its link for 45 s (25 s past the 20 s timeout); its `LinkBanner` goes
//! interrupted, then "Reconnecting (attempt N)", and back online as the same player with its
//! avatar, by itself. The other client and the server keep playing. Manual clocks; nothing
//! sleeps.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use net_game::{LinkBanner, NetGameWire, Side, avatars, headless_app, link_options};
use regia::app::App;
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::socket::{DatagramSocket, LoopbackSocket};
use regia::net::{ConditionerConfig, LoopbackNet};

fn manual() -> LinkClock {
    LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    }
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

fn banner(app: &App) -> LinkBanner {
    app.world().resource::<LinkBanner>().clone()
}

#[test]
fn a_long_outage_shows_the_overlay_and_comes_back_as_the_same_player() {
    let net = LoopbackNet::new();
    let mut server = headless_app(Side::Server);
    let opts = link_options(manual(), ConditionerConfig::default());
    let addr = SocketAddr::from(([127, 0, 0, 1], 7787));
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(net.bind(addr).expect("bind")), &opts)
        .expect("host");
    let key = server.world().resource::<NetLink>().server_key();
    let down = Arc::new(AtomicBool::new(false));
    let mut clients: Vec<App> = Vec::new();
    for i in 0..2 {
        let mut app = headless_app(Side::Client);
        let socket = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let socket: Box<dyn DatagramSocket + Send> = if i == 1 {
            Box::new(GateSocket {
                socket,
                down: down.clone(),
            })
        } else {
            Box::new(socket)
        };
        app.world_mut()
            .resource_mut::<NetLink>()
            .connect::<NetGameWire>(
                socket,
                addr,
                &LinkOptions {
                    server_key: key,
                    ..opts.clone()
                },
            )
            .expect("connect");
        clients.push(app);
    }
    let step = |server: &mut App, clients: &mut [App]| {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
    };
    for _ in 0..600 {
        step(&mut server, &mut clients);
    }
    assert_eq!(avatars(server.world_mut()).len(), 2);
    assert_eq!(banner(&clients[1]), LinkBanner::Online);
    let peer = clients[1]
        .world()
        .resource::<NetLink>()
        .status()
        .and_then(|s| s.peer_id);
    assert!(peer.is_some());

    down.store(true, Ordering::Relaxed);
    let mut seen_interrupted = false;
    let mut attempts = Vec::new();
    for _ in 0..45 * 60 {
        step(&mut server, &mut clients);
        match banner(&clients[1]) {
            LinkBanner::Interrupted => seen_interrupted = true,
            LinkBanner::Reconnecting { attempt } => {
                if attempts.last() != Some(&attempt) {
                    attempts.push(attempt);
                }
            }
            LinkBanner::Lost { reason } => panic!("the link was given up: {reason}"),
            LinkBanner::Online => {}
        }
    }
    assert!(seen_interrupted, "the overlay said interrupted first");
    assert!(
        attempts.len() >= 4,
        "the overlay counted the attempts: {attempts:?}"
    );
    assert_eq!(attempts, (1..=attempts.len() as u32).collect::<Vec<_>>());
    let line = banner(&clients[1]).text().expect("overlay up");
    assert!(line.starts_with("Reconnecting (attempt "), "{line}");
    // The other player kept playing, and the server still holds the quiet player's avatar.
    assert_eq!(banner(&clients[0]), LinkBanner::Online);
    assert_eq!(avatars(server.world_mut()).len(), 2, "dormant, not gone");

    down.store(false, Ordering::Relaxed);
    let mut back = None;
    for n in 0..120 {
        step(&mut server, &mut clients);
        if banner(&clients[1]) == LinkBanner::Online {
            back = Some(n);
            break;
        }
    }
    assert!(back.is_some(), "back online within 2 s of the link returning");
    assert_eq!(banner(&clients[1]).text(), None, "the overlay is gone");
    assert_eq!(
        clients[1]
            .world()
            .resource::<NetLink>()
            .status()
            .and_then(|s| s.peer_id),
        peer,
        "the same player"
    );
    for _ in 0..120 {
        step(&mut server, &mut clients);
    }
    assert_eq!(avatars(server.world_mut()).len(), 2);
    assert_eq!(
        avatars(clients[1].world_mut()).len(),
        2,
        "it sees both avatars again"
    );
}
