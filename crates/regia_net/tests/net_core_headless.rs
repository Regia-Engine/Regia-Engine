// SPDX-License-Identifier: MPL-2.0
//! The headless example of the ECS-free core (spec 120 § 3.2, ADR-8501), run as a test. It builds
//! and passes with and without the `ecs` feature; `cargo xtask check-net-core` runs it with
//! `--no-default-features`, so the core alone carries a two-client session: handshake, per-tick
//! inputs through the tick driver, and `#[derive(NetMessage)]` messages through the registry.

#[path = "../examples/net_core_headless.rs"]
mod headless;

use headless::{Arena, SCORE_EVERY_TICKS, Score, frame, registry, unframe};
use regia_net::codec::{CodecContext, LayoutHasher};
use regia_net::{DecodeError, NetGame};

const TICKS: u64 = 300;

#[test]
fn two_clients_play_a_headless_session_on_the_core() {
    let report = headless::run(2, TICKS).expect("session runs");
    assert_eq!(report.peers.len(), 2, "both clients connected: {report:?}");
    assert_eq!(report.joined, 2, "the server reported both joins");
    assert_eq!(report.ticks, TICKS);
    // Inputs drove the simulation: client 0 steps right, client 1 left, one stride per tick.
    let a = report.positions.get(&report.peers[0]).copied().unwrap_or(0);
    let b = report.positions.get(&report.peers[1]).copied().unwrap_or(0);
    let played = i32::try_from(TICKS - report.missed_inputs).expect("small");
    assert!(
        a > played / 2 && b < -played / 2,
        "positions {a} {b} after {TICKS} ticks"
    );
    assert_eq!(a, -b, "both clients' inputs reached the same ticks: {a} {b}");
    // On a clean loopback after the input lead settles, no tick misses an input.
    assert!(
        report.missed_inputs < 20,
        "missed {} inputs on a clean loopback",
        report.missed_inputs
    );
    // Both clients received every standing, in order, and the last one matches the server.
    let expected = usize::try_from(TICKS / SCORE_EVERY_TICKS).expect("small");
    for (k, scores) in report.scores.iter().enumerate() {
        assert_eq!(scores.len(), expected, "client {k} standings: {scores:?}");
        assert!(
            scores.windows(2).all(|w| w[0].tick < w[1].tick),
            "client {k} in order"
        );
        let last = scores.last().expect("a standing");
        assert_eq!(
            last.peers,
            report.positions.keys().map(|p| p.0).collect::<Vec<_>>()
        );
        assert_eq!(
            last.positions,
            report.positions.values().copied().collect::<Vec<_>>()
        );
    }
}

#[test]
fn the_example_main_succeeds() {
    assert_eq!(headless::main(), std::process::ExitCode::SUCCESS);
}

#[test]
fn messages_frame_through_the_core_registry() {
    let registry = registry();
    let score = Score {
        tick: 42,
        peers: vec![1, 2],
        positions: vec![7, -7],
    };
    let bytes = frame(&registry, &score).expect("registered");
    assert_eq!(unframe::<Score>(&registry, &bytes), Some(score.clone()));
    // An index no type is registered at is refused, not panicked on.
    let mut wrong = bytes.clone();
    wrong[0] = 9;
    assert_eq!(unframe::<Score>(&registry, &wrong), None);
    assert!(matches!(
        registry.decode(9, &bytes[2..], &CodecContext::detached()),
        Err(DecodeError::Invalid("message type"))
    ));
    // Truncated codec bytes are a decode error.
    assert!(
        registry
            .decode(0, &bytes[2..3], &CodecContext::detached())
            .is_err()
    );
    // The registry's layout feeds the game's content hash, so another message set cannot join.
    let mut other = regia_net::message::NetMessageRegistry::default();
    let mut h = LayoutHasher::new();
    other.hash_into(&mut h);
    assert_ne!(Arena::content_hash(), h.finish());
    other.register::<Score>();
    let mut h = LayoutHasher::new();
    other.hash_into(&mut h);
    assert_eq!(Arena::content_hash(), h.finish());
}

/// Without the `ecs` feature, `ValidateContext::core` builds the context a host passes to
/// `NetMessageRegistry::validate`.
#[cfg(not(feature = "ecs"))]
#[test]
fn a_core_host_validates_through_the_registry() {
    use regia_net::PeerId;
    use regia_net::message::{ValidateContext, Verdict};
    let registry = registry();
    let score = Score {
        tick: 1,
        peers: vec![],
        positions: vec![],
    };
    let cx = ValidateContext::core(PeerId(3), regia_net::NetTick(1));
    assert_eq!(registry.validate(0, &score, &cx), Verdict::Accept);
    assert_eq!(
        registry.validate(0, &5u32, &cx),
        Verdict::Reject("message type mismatch")
    );
    assert_eq!(registry.validate(7, &score, &cx), Verdict::Reject("message type"));
}

/// With the `ecs` feature the same registry still decodes (the paths did not move).
#[cfg(feature = "ecs")]
#[test]
fn the_registry_is_the_same_type_with_the_ecs() {
    let registry: regia_net::message::NetMessageRegistry = registry();
    assert_eq!(registry.find("example.net_core_headless.score"), Some(0));
    assert_eq!(
        registry.types()[0].direction,
        regia_net::message::Direction::Broadcast
    );
}
