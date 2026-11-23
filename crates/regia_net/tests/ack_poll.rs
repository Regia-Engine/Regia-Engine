// SPDX-License-Identifier: MPL-2.0
//! Spec 120 §§ 4.1 and 5.3: bounded bursts, timely unreliable data and clean RTT samples.
//! Virtual millisecond time only; the packets move directly between sans-IO connections.
use regia_net::connection::{Channel, Connection, LinkConfig};

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): these
/// tests measure the transport's blind start (the 64-packet initial window and what it makes of
/// acknowledgments, bursts and rates), which the first flight replaces until a path answers.
fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

fn deliver(a: &mut Connection, b: &mut Connection, now: u64) -> usize {
    let packets = a.poll(now, &[]);
    let count = packets.len();
    for packet in packets {
        b.receive(&packet, now).expect("valid packet");
    }
    count
}

#[test]
fn one_reply_acknowledges_a_legal_burst_without_false_loss() {
    for len in [1, 1186] {
        let mut a = Connection::new(blind_link());
        let mut b = Connection::new(blind_link());
        for i in 0..64 {
            a.send(Channel::GAME_RPC, vec![i; len], 0).expect("queued");
        }
        let mut received = Vec::new();
        for now in 0..8 {
            let burst = deliver(&mut a, &mut b, now);
            let replies = deliver(&mut b, &mut a, now);
            // Wire 5 (ADR-8184): one acknowledgment frame names a burst of any size, so the 64
            // full packets of the first poll get one reply too (ADR-7995 sent two ranges, each
            // twice, in four replies).
            if burst > 0 {
                assert_eq!(replies, 1, "one reply per burst of {burst}");
            }
            received.extend(std::iter::from_fn(|| b.next_message()));
            if !a.has_unacked_reliable() {
                break;
            }
        }
        assert_eq!(received.len(), 64, "every message delivered once");
        for (i, (channel, bytes)) in received.iter().enumerate() {
            assert_eq!(*channel, Channel::GAME_RPC);
            assert_eq!(*bytes, vec![i as u8; len]);
        }
        assert!(!a.has_unacked_reliable(), "all delivered fragments acknowledged");
        assert_eq!(a.stats().packets_lost, 0, "delivered burst is not loss");
        assert_eq!(a.stats().reliable_resends, 0);
    }
}

#[test]
fn an_unpacked_unreliable_message_survives_until_the_next_poll() {
    let mut a = Connection::new(blind_link());
    let mut b = Connection::new(blind_link());
    for _ in 0..64 {
        a.send(Channel::GAME_RPC, vec![9; 1186], 0).expect("queued");
    }
    a.send(Channel::Unreliable, vec![1, 2, 3], 0).expect("queued");
    for now in 0..4 {
        deliver(&mut a, &mut b, now);
        deliver(&mut b, &mut a, now);
    }
    let messages: Vec<_> = std::iter::from_fn(|| b.next_message()).collect();
    assert!(
        messages.contains(&(Channel::Unreliable, vec![1, 2, 3])),
        "unpacked unreliable retained"
    );
    assert_eq!(a.stats().unreliable_dropped, 0);
}

#[test]
fn a_late_ack_does_not_change_the_pre_stall_rtt() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    deliver(&mut a, &mut b, 0);
    deliver(&mut b, &mut a, 40);
    assert_eq!(a.stats().rtt_ms, 40.0);
    a.send(Channel::GAME_RPC, vec![1], 0).expect("queued");
    deliver(&mut a, &mut b, 50);
    let late = b.poll(60, &[]);
    a.poll(2051, &[]); // Mark the original packet lost before its ack returns.
    for packet in late {
        a.receive(&packet, 2052).expect("late ack");
    }
    assert_eq!(a.stats().rtt_ms, 40.0, "a lost packet is not an RTT sample");
    assert!(!a.has_unacked_reliable(), "late ack still retires reliable data");
}

#[test]
fn an_ack_hole_retransmits_before_the_rto() {
    let config = LinkConfig {
        resend_min_ms: 1000,
        ..LinkConfig::default()
    };
    let mut a = Connection::new(config.clone());
    let mut b = Connection::new(config);
    for i in 0..4 {
        a.send(Channel::GAME_RPC, vec![i; 1186], 0).expect("queued");
    }
    let burst = a.poll(0, &[]);
    for packet in &burst[1..] {
        b.receive(packet, 10).expect("three later packets arrive");
    }
    deliver(&mut b, &mut a, 20);
    deliver(&mut a, &mut b, 21);
    assert_eq!(a.stats().packets_lost, 1, "an explicit ack hole counts loss");
    assert_eq!(
        a.stats().reliable_resends,
        1,
        "loss schedules an immediate resend"
    );
    assert_eq!(std::iter::from_fn(|| b.next_message()).count(), 4);
}

#[test]
fn a_retransmitted_fragment_never_samples_rtt() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    a.send(Channel::GAME_RPC, vec![1], 0).expect("queued");
    a.poll(0, &[]); // Lost original.
    deliver(&mut a, &mut b, 100); // Retransmission.
    deliver(&mut b, &mut a, 120);
    assert_eq!(a.stats().rtt_ms, 0.0, "Karn: retransmission supplies no sample");
    assert!(!a.has_unacked_reliable());
}

#[test]
fn an_mtu_below_the_state_floor_is_refused() {
    assert!(
        regia_net::NetSettings::from_ron("(replication: (mtu_bytes: 100))").is_err(),
        "an undersize MTU must not silently grow to the payload floor"
    );
}

#[test]
fn simultaneous_full_bursts_can_ack_each_other() {
    let mut a = Connection::new(blind_link());
    let mut b = Connection::new(blind_link());
    for _ in 0..64 {
        a.send(Channel::GAME_RPC, vec![1; 1186], 0).expect("queued");
        b.send(Channel::GAME_RPC, vec![2; 1186], 0).expect("queued");
    }
    let a_burst = a.poll(0, &[]);
    let b_burst = b.poll(0, &[]);
    for p in a_burst {
        b.receive(&p, 1).expect("valid");
    }
    for p in b_burst {
        a.receive(&p, 1).expect("valid");
    }
    for now in 2..10 {
        deliver(&mut a, &mut b, now);
        deliver(&mut b, &mut a, now);
    }
    assert!(
        !a.has_unacked_reliable() && !b.has_unacked_reliable(),
        "no bidirectional window deadlock"
    );
    assert_eq!(a.stats().packets_lost + b.stats().packets_lost, 0);
    assert_eq!(a.stats().reliable_resends + b.stats().reliable_resends, 0);
    assert_eq!(std::iter::from_fn(|| a.next_message()).count(), 64);
    assert_eq!(std::iter::from_fn(|| b.next_message()).count(), 64);
}

#[test]
fn unreliable_queue_is_bounded_and_expires_only_at_its_deadline() {
    use regia_net::connection::{MAX_UNRELIABLE_QUEUE_BYTES, SendError};
    let mut a = Connection::new(LinkConfig {
        max_packets_per_poll: 1,
        ..LinkConfig::default()
    });
    a.set_tick_rate(60);
    for _ in 0..2 {
        a.send(Channel::GAME_RPC, vec![1; 1186], 0).expect("queued");
    }
    a.send(Channel::Unreliable, vec![2], 0).expect("queued");
    a.poll(0, &[]);
    assert_eq!(a.stats().unreliable_dropped, 0);
    a.poll(16, &[]);
    assert_eq!(
        a.stats().unreliable_dropped,
        0,
        "not stale before the tick deadline"
    );
    a.poll(17, &[]);
    assert_eq!(a.stats().unreliable_dropped, 1, "stale at the tick deadline");
    let mut b = Connection::new(LinkConfig::default());
    let mut queued = 0;
    while b.send(Channel::Unreliable, Vec::new(), 0).is_ok() {
        queued += 1;
        assert!(
            queued <= MAX_UNRELIABLE_QUEUE_BYTES / 3,
            "empty records also consume the cap"
        );
    }
    assert!(matches!(
        b.send(Channel::Unreliable, vec![1], 0),
        Err(SendError::Backpressure(_))
    ));
    b.send_essential(vec![9], 0)
        .expect("ordinary traffic cannot fill the essential reserve");
}

#[test]
fn a_tick_sync_receipt_names_only_a_packed_message() {
    // Public behavior: essential data gets the first packet even at a one-packet poll cap.
    let mut a = Connection::new(LinkConfig {
        max_packets_per_poll: 1,
        ..LinkConfig::default()
    });
    let mut b = Connection::new(LinkConfig::default());
    a.send(Channel::GAME_RPC, vec![1; 1186], 0).expect("queued");
    a.send_essential(vec![2; 9], 0).expect("queued");
    deliver(&mut a, &mut b, 0);
    assert_eq!(b.next_message(), Some((Channel::Unreliable, vec![2; 9])));
    assert!(
        a.has_unacked_reliable(),
        "the essential packet did not discard the reliable fragment"
    );
}
