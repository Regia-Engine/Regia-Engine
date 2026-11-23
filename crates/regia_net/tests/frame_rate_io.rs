// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 4.5 A1 (DO27 rule A1, ADR-8407): all net I/O and budgets are per simulation tick.
//! The same client at 30 fps and at 300 fps, both simulating at 30 Hz on the headless app clock
//! (`TimeUpdateStrategy::ManualDuration`, manual link clocks), sends the same bytes and datagrams
//! per second within 2 %, with the server replicating 30 entities that change every tick. This
//! already holds at the N204 base (the bridge sends per tick; acknowledgments, keepalives and
//! pacing run on time, not on polls), so the test guards it. Virtual time only: no sockets, no
//! sleeps, no wall clock.

use std::net::SocketAddr;
use std::time::Duration;

use regia_app::{App, FixedUpdate, TimeUpdateStrategy};
use regia_ecs::prelude::*;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::prelude::*;
use regia_net::{NetMode, NetPlugin, ReplicateAppExt, ReplicateOptions, Replicated};

/// A replicated value the server changes every tick, so state frames flow every tick and the
/// client acknowledges them.
#[derive(Component, Clone, Debug, PartialEq, Default, regia_net::Replicate)]
struct Counter(u32);

fn bump(mut q: Query<&mut Counter>) {
    for mut c in &mut q {
        c.0 = c.0.wrapping_add(1);
    }
}

#[derive(Debug)]
struct LinkGame;
impl NetGame for LinkGame {
    type Message = regia_net::bridge::NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.frame_rate_io";
    const PROTOCOL_VERSION: u32 = 1;
}

/// The simulation rate of both runs.
const TICK_HZ: f64 = 30.0;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// An app simulating at 30 Hz whose frames are `1/fps` long, with a link clock that advances one
/// frame per update.
fn app(mode: NetMode, fps: u32) -> (App, LinkOptions) {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, TICK_HZ);
    let frame = Duration::from_secs(1) / fps;
    app.insert_resource(TimeUpdateStrategy::ManualDuration(frame));
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app.replicate::<Counter>(ReplicateOptions::default());
    let options = LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: frame.as_micros() as u64,
        },
        ..LinkOptions::default()
    };
    (app, options)
}

/// (datagrams, bytes) the client sent per second over 10 s of simulated time after it joined,
/// with the client rendering at `client_fps` and the server at 30 fps.
fn client_io_per_second(client_fps: u32) -> (f64, f64) {
    let net = LoopbackNet::new();
    let (mut server, server_options) = app(NetMode::DedicatedServer, 30);
    let (mut client, client_options) = app(NetMode::Client, client_fps);
    for i in 0..30 {
        server.world_mut().spawn((Replicated::default(), Counter(i)));
    }
    server.add_systems(FixedUpdate, bump);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<LinkGame>(Box::new(net.bind(addr(7300)).expect("bind")), &server_options)
        .expect("host");
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<LinkGame>(
            Box::new(net.bind(addr(0)).expect("bind")),
            addr(7300),
            &client_options,
        )
        .expect("join");
    // Frames of the client per server frame: both clocks advance together.
    let ratio = (client_fps / 30) as usize;
    let mut joined = false;
    for _ in 0..300 {
        server.update();
        for _ in 0..ratio {
            client.update();
        }
        if client.world().resource::<NetLink>().is_connected() {
            joined = true;
            break;
        }
    }
    assert!(joined, "{client_fps} fps: the client joined");
    // Two seconds to settle, then ten measured.
    for _ in 0..60 {
        server.update();
        for _ in 0..ratio {
            client.update();
        }
    }
    let link = |app: &App| {
        app.world()
            .resource::<NetLink>()
            .status()
            .and_then(|s| s.link)
            .expect("client link")
    };
    let before = link(&client);
    for _ in 0..300 {
        server.update();
        for _ in 0..ratio {
            client.update();
        }
    }
    let after = link(&client);
    assert!(client.world().resource::<NetLink>().is_connected());
    (
        (after.packets_sent - before.packets_sent) as f64 / 10.0,
        (after.bytes_sent - before.bytes_sent) as f64 / 10.0,
    )
}

#[test]
fn the_same_client_at_30_and_300_fps_sends_the_same_per_second() {
    let (d30, b30) = client_io_per_second(30);
    let (d300, b300) = client_io_per_second(300);
    assert!(
        d30 > 0.0 && b30 > 0.0,
        "the client sends: {d30} datagrams/s, {b30} B/s"
    );
    let within = |a: f64, b: f64| (a - b).abs() <= 0.02 * a.max(b);
    assert!(
        within(d30, d300) && within(b30, b300),
        "30 fps: {d30:.1} datagrams/s, {b30:.0} B/s; 300 fps: {d300:.1} datagrams/s, {b300:.0} B/s (bar 2 %)"
    );
}
