// SPDX-License-Identifier: MIT-0
//! Regression for the accidentally unified server/client build from the real match.
//! Run with `cargo test -p net_game --features server --test client_build_mode`.
//! The guard must exit before opening a window; the actual match client is built separately.

use std::process::Command;

#[test]
fn a_windowed_client_built_with_server_mode_refuses_before_launch() {
    let output = Command::new(env!("CARGO_BIN_EXE_net_game_client"))
        .args(["--ticks", "1"])
        .env_remove("REGIA_JOIN_CODE")
        .env_remove("REGIA_CLIENT_OFFSCREEN")
        .env_remove("REGIA_NET_CONDITIONER")
        .env_remove("REGIA_NET_CONDITIONER_SEED")
        .output()
        .expect("client starts");
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{error}");
    assert!(
        error.contains("this client was built with regia's `server` feature"),
        "{error}"
    );
    assert!(
        error.contains("build the client in its own cargo invocation"),
        "{error}"
    );
}

/// A late client must drive the app built by the dedicated-server entry point, not only the
/// smaller headless test app. Fixed steps and real UDP make the composition check repeatable.
#[test]
fn a_late_udp_client_moves_on_the_dedicated_server_app() {
    let server = regia::server::build_app(
        net_game::NetGamePlugin {
            side: net_game::Side::Server,
        },
        |_| {},
        60,
        false,
        None,
    )
    .expect("dedicated app");
    assert_late_udp_input(server);
}

#[test]
fn a_late_udp_client_moves_on_the_minimal_server_app() {
    assert_late_udp_input(net_game::headless_app(net_game::Side::Server));
}

fn assert_late_udp_input(mut server: regia::prelude::App) {
    use net_game::{Pad, PadSource, Side, avatars, headless_app, link_options};
    use regia::net::ConditionerConfig;
    use regia::net::link::{LinkClock, LinkOptions, NetLink};
    use regia::net::predict::ClientPrediction;
    use regia::net::runtime::NetNode;

    let options = link_options(
        LinkClock::Manual {
            start_ms: 0,
            step_us: 16_667,
        },
        ConditionerConfig::default(),
    );
    net_game::host_udp(&mut server, ([127, 0, 0, 1], 0).into(), &options).expect("host");
    for _ in 0..600 {
        server.update();
    }
    assert!(server.world().resource::<NetNode>().tick.0 >= 600);
    let link = server.world().resource::<NetLink>();
    let address = link.local_addr().expect("bound");
    let client_options = LinkOptions {
        server_key: link.server_key(),
        ..options
    };
    let mut client = headless_app(Side::Client);
    client.insert_resource(PadSource::Manual(Pad {
        move_x: 1.0,
        ..Pad::default()
    }));
    net_game::connect_udp(&mut client, address, &client_options).expect("connect");
    for _ in 0..1200 {
        server.update();
        client.update();
    }
    assert!(client.world().resource::<NetLink>().is_connected());
    let server_avatars = avatars(server.world_mut());
    let client_avatars = avatars(client.world_mut());
    let prediction = &client.world().resource::<ClientPrediction>().stats;
    println!(
        "dedicated late join: server={server_avatars:?} client={client_avatars:?} prediction={prediction:?}"
    );
    assert_eq!(server_avatars.len(), 1);
    assert!(
        server_avatars[0].position.x > 10.0,
        "the server must apply the client's movement"
    );
    assert_eq!(client_avatars, server_avatars);
    assert!(
        prediction.corrections <= 20,
        "bounded reconciliation: {prediction:?}"
    );
    assert_eq!(prediction.verify_failures, 0);
}
