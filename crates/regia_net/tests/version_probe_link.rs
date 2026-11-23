// SPDX-License-Identifier: MPL-2.0
//! `NetLink` refuses other versions and other content with the older side named (spec 120 § 12.2,
//! ADR-7985). Two apps over the in-memory `LoopbackNet`, manual clock, fixed steps.

use std::net::SocketAddr;

use regia_app::App;
use regia_ecs::prelude::*;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::model::NetMode;
use regia_net::replication::register_replicated;
use regia_net::socket::LoopbackNet;
use regia_net::version_probe::MismatchSide;
use regia_net::*;

/// Release 1.2.0 of a replication game.
struct Old;
impl NetGame for Old {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.version_probe_link";
    const PROTOCOL_VERSION: u32 = 1;
    const GAME_VERSION: &'static str = "1.2.0";
}

/// Release 1.3.0 of the same game.
struct New;
impl NetGame for New {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.version_probe_link";
    const PROTOCOL_VERSION: u32 = 2;
    const GAME_VERSION: &'static str = "1.3.0";
}

/// A replicated component only one side registers.
#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Fuel(u32);

fn app(mode: NetMode) -> App {
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

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A server app of `S` and a client app of `C`; `edit` prepares the client app. Returns the
/// client's link events and whether it connected.
fn attempt<S, C>(
    port: u16,
    server_opts: &LinkOptions,
    client_opts: &LinkOptions,
    edit: impl FnOnce(&mut App),
) -> (Vec<LinkEvent>, bool, App)
where
    S: NetGame<Message = NetFrame>,
    C: NetGame<Message = NetFrame, JoinData = ()>,
{
    let net = LoopbackNet::new();
    let mut server = app(NetMode::DedicatedServer);
    let mut client = app(NetMode::Client);
    edit(&mut client);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<S>(Box::new(net.bind(addr(port)).expect("bind")), server_opts)
        .expect("host");
    client
        .world_mut()
        .resource_mut::<NetLink>()
        .connect::<C>(
            Box::new(net.bind(addr(0)).expect("bind")),
            addr(port),
            client_opts,
        )
        .expect("connect");
    let mut events = Vec::new();
    let mut connected = false;
    for _ in 0..60 {
        server.update();
        client.update();
        events.extend(client.world_mut().resource_mut::<NetLink>().take_events());
        connected |= client.world().resource::<NetLink>().is_connected();
    }
    (events, connected, client)
}

fn mismatch(events: &[LinkEvent]) -> (MismatchSide, Option<bool>, String) {
    events
        .iter()
        .find_map(|e| match e {
            LinkEvent::VersionMismatch {
                side,
                server_is_older,
                message,
            } => Some((*side, *server_is_older, message.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no VersionMismatch in {events:?}"))
}

fn ended_with_protocol_mismatch(events: &[LinkEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, LinkEvent::Ended { reason } if reason == "protocol_mismatch"))
}

#[test]
fn an_old_client_app_is_told_to_update_the_game() {
    let (events, connected, client) = attempt::<New, Old>(7301, &manual(), &manual(), |_| {});
    assert!(!connected);
    assert!(ended_with_protocol_mismatch(&events), "{events:?}");
    let (side, server_is_older, message) = mismatch(&events);
    assert_eq!(side, MismatchSide::ClientOlder);
    assert_eq!(server_is_older, Some(false));
    assert!(
        message.contains("update the game") && message.contains("1.3.0"),
        "{message}"
    );
    // `net.status` shows it.
    let status = client.world().resource::<NetLink>().status().expect("status");
    assert_eq!(
        status.version_mismatch.map(|r| r.side),
        Some(MismatchSide::ClientOlder)
    );
}

#[test]
fn a_new_client_app_is_told_the_server_is_older() {
    let (events, connected, _) = attempt::<Old, New>(7302, &manual(), &manual(), |_| {});
    assert!(!connected);
    assert!(ended_with_protocol_mismatch(&events), "{events:?}");
    let (side, server_is_older, message) = mismatch(&events);
    assert_eq!(side, MismatchSide::ServerOlder);
    assert_eq!(server_is_older, Some(true));
    assert!(
        message.contains("the server is older than this build"),
        "{message}"
    );
}

/// Same build, other game content (`LinkOptions::content_hash`): refused, `content_differs`.
#[test]
fn other_link_content_is_refused() {
    let server_opts = LinkOptions {
        content_hash: 0xabc,
        ..manual()
    };
    let client_opts = LinkOptions {
        content_hash: 0xdef,
        ..manual()
    };
    let (events, connected, _) = attempt::<New, New>(7303, &server_opts, &client_opts, |_| {});
    assert!(!connected, "mismatched content must not join: {events:?}");
    assert!(ended_with_protocol_mismatch(&events), "{events:?}");
    let (side, server_is_older, message) = mismatch(&events);
    assert_eq!(side, MismatchSide::ContentDiffers);
    assert_eq!(server_is_older, None);
    assert!(message.contains("install the same content"), "{message}");

    // The same content joins.
    let (events, connected, _) = attempt::<New, New>(7304, &server_opts, &server_opts, |_| {});
    assert!(connected, "{events:?}");
}

/// With no content hash set (the default `0`), the replicated schema is still checked: a client
/// that registers a component the server does not know is refused instead of joining.
#[test]
fn another_replicated_schema_is_refused_by_default() {
    let (events, connected, _) = attempt::<New, New>(7305, &manual(), &manual(), |client| {
        register_replicated::<Fuel>(client.world_mut(), ReplicateOptions::default());
    });
    assert!(!connected, "another schema must not join: {events:?}");
    assert!(ended_with_protocol_mismatch(&events), "{events:?}");
    assert_eq!(mismatch(&events).0, MismatchSide::ContentDiffers);
}
