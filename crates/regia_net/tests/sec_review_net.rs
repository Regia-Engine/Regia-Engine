// SPDX-License-Identifier: MPL-2.0
//! The v0.5 security review of the session layer (ADR-5870 § 9): wrong passwords and remote admin
//! tokens are counted per address block across connections, not per connection.

use std::net::SocketAddr;

use regia_net::model::PeerId;
use regia_net::session::hub::SessionHub;
use regia_net::session::lifecycle::{
    AUTH_BACKOFF_BASE_MS, AUTH_BACKOFF_MAX_MS, AUTH_STRIKE_WINDOW_MS, HostOptions, MAX_AUTH_STRIKES,
    SessionSettings, auth_backoff_ms,
};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};

fn manifest() -> ContentManifest {
    ContentManifest::build("0.5.0", "1.2.0", 1, 0xabc, &[])
}

fn hello(name: &str, password: Option<&str>) -> Hello {
    Hello {
        protocol_version: 1,
        protocol_hash: 0xabc,
        manifest: manifest(),
        player_names: vec![name.to_owned()],
        password: password.map(str::to_owned),
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: None,
    }
}

fn hub_with(opts: HostOptions) -> SessionHub {
    let mut hub = SessionHub::new(
        SessionSettings::default(),
        HostOptions {
            level: "levels/arena.level.ron".into(),
            max_players: 64,
            ..opts
        },
        manifest(),
    );
    hub.server.set_dev_build(false);
    hub
}

/// Adds a client at `addr` and runs until it is in game or refused; the refusal code, if any.
fn attempt(hub: &mut SessionHub, h: Hello, addr: SocketAddr) -> Option<&'static str> {
    let c = hub.add_client(h);
    hub.clients.set_addr(c, addr);
    assert!(hub.run_until(600, |h| {
        let client = h.clients.client(c);
        client.in_game() || client.disconnect_reason().is_some()
    }));
    hub.clients
        .client(c)
        .disconnect_reason()
        .map(DisconnectReason::code)
}

fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, last], port))
}

/// Each try needs a fresh handshake, but nothing counted wrong passwords across them: one address
/// could guess at the handshake rate for as long as it liked.
#[test]
fn wrong_passwords_lock_out_their_address_block_for_a_window() {
    let mut hub = hub_with(HostOptions {
        password: "secret".into(),
        ..HostOptions::default()
    });
    for i in 0..MAX_AUTH_STRIKES {
        let port = 40_000 + i as u16;
        assert_eq!(
            attempt(&mut hub, hello("Mal", Some("guess")), addr(1, port)),
            Some("bad_password")
        );
    }
    // The right password from the same address is refused unchecked until the window ends.
    assert_eq!(
        attempt(&mut hub, hello("Mal", Some("secret")), addr(1, 41_000)),
        Some("bad_password")
    );
    // Another address is not affected.
    assert_eq!(
        attempt(&mut hub, hello("Ann", Some("secret")), addr(2, 41_001)),
        None
    );
    hub.run_secs(AUTH_STRIKE_WINDOW_MS as f64 / 1000.0 + 1.0);
    assert_eq!(
        attempt(&mut hub, hello("Mal", Some("secret")), addr(1, 41_002)),
        None
    );
}

/// ADR-5993 (lead decision 7): ten wrong passwords from one address block slow it down instead
/// of locking it out for the rest of a minute: a household behind one NAT address whose member
/// typed the password wrong is back in after the first wait (4 s), not after 60 s.
#[test]
fn wrong_passwords_slow_their_block_but_never_lock_it_out() {
    let mut hub = hub_with(HostOptions {
        password: "secret".into(),
        ..HostOptions::default()
    });
    for i in 0..MAX_AUTH_STRIKES {
        let port = 44_000 + i as u16;
        assert_eq!(
            attempt(&mut hub, hello("Kid", Some("guess")), addr(7, port)),
            Some("bad_password")
        );
    }
    // Inside the wait: refused unchecked (and not counted).
    assert_eq!(
        attempt(&mut hub, hello("Mum", Some("secret")), addr(7, 44_100)),
        Some("bad_password")
    );
    // After the first wait, well inside the old 60 s window, the right password gets in.
    hub.run_secs(AUTH_BACKOFF_BASE_MS as f64 / 1000.0 + 0.5);
    assert_eq!(
        attempt(&mut hub, hello("Mum", Some("secret")), addr(7, 44_101)),
        None
    );
}

/// ADR-5993: each wrong try past the limit doubles the wait, and the wait stops at 30 s: a block
/// that keeps guessing gets one checked try per 30 s, and the right password still gets in.
#[test]
fn the_auth_backoff_doubles_and_stops_at_thirty_seconds() {
    assert_eq!(auth_backoff_ms(MAX_AUTH_STRIKES - 1), 0);
    assert_eq!(auth_backoff_ms(MAX_AUTH_STRIKES), AUTH_BACKOFF_BASE_MS);
    assert_eq!(auth_backoff_ms(MAX_AUTH_STRIKES + 1), 2 * AUTH_BACKOFF_BASE_MS);
    assert_eq!(auth_backoff_ms(MAX_AUTH_STRIKES + 3), AUTH_BACKOFF_MAX_MS);
    assert_eq!(auth_backoff_ms(u32::MAX), AUTH_BACKOFF_MAX_MS);
    assert_eq!(AUTH_BACKOFF_MAX_MS, 30_000);
    let mut hub = hub_with(HostOptions {
        password: "secret".into(),
        ..HostOptions::default()
    });
    let mut port = 45_000;
    let mut strikes = 0;
    while strikes < MAX_AUTH_STRIKES + 5 {
        port += 1;
        assert_eq!(
            attempt(&mut hub, hello("Mal", Some("guess")), addr(8, port)),
            Some("bad_password")
        );
        strikes += 1;
        hub.run_secs(auth_backoff_ms(strikes) as f64 / 1000.0 + 0.5);
    }
    // Fifteen strikes: the last wait was the 30 s cap, and it has passed.
    port += 1;
    assert_eq!(
        attempt(&mut hub, hello("Mal", Some("secret")), addr(8, port)),
        None
    );
}

/// Three wrong admin tokens ended the connection, and the next connection started from zero.
#[test]
fn wrong_admin_tokens_count_across_connections() {
    let mut hub = hub_with(HostOptions {
        admin_token: Some("tok3n".into()),
        ..HostOptions::default()
    });
    let mut strikes = 0;
    let mut port = 42_000;
    while strikes < MAX_AUTH_STRIKES {
        port += 1;
        assert_eq!(attempt(&mut hub, hello("Mal", None), addr(1, port)), None);
        let c = hub.clients.clients().len() - 1;
        for _ in 0..3 {
            if strikes == MAX_AUTH_STRIKES {
                break;
            }
            hub.clients.client_mut(c).core.send_admin(SessionMsg::AdminLogin {
                token: "wrong".into(),
            });
            hub.run(5);
            strikes += 1;
        }
        hub.run(30);
    }
    // A new connection from the same address with the right token is refused and dropped.
    port += 1;
    assert_eq!(attempt(&mut hub, hello("Mal", None), addr(1, port)), None);
    let c = hub.clients.clients().len() - 1;
    let peer: PeerId = hub.server.peer_of(hub.clients.client(c).link).expect("joined");
    hub.clients.client_mut(c).core.send_admin(SessionMsg::AdminLogin {
        token: "tok3n".into(),
    });
    hub.run(30);
    assert!(!hub.server.is_admin(peer));
    assert_eq!(
        hub.clients
            .client(c)
            .disconnect_reason()
            .map(DisconnectReason::code),
        Some("auth_failed")
    );
    // Another address logs in.
    assert_eq!(attempt(&mut hub, hello("Ann", None), addr(2, 43_000)), None);
    let c = hub.clients.clients().len() - 1;
    let peer = hub.server.peer_of(hub.clients.client(c).link).expect("joined");
    hub.clients.client_mut(c).core.send_admin(SessionMsg::AdminLogin {
        token: "tok3n".into(),
    });
    hub.run(5);
    assert!(hub.server.is_admin(peer));
}
