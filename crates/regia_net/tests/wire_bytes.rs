// SPDX-License-Identifier: MPL-2.0
//! ADR-10097 (KI-NT-032): a share of a link is kept on the wire. A connection's controller steers
//! packet bodies; a shared uplink's fair share is a rate on the wire (bodies, endpoint headers,
//! IP and UDP headers), and the server divides it by what the peer's packets cost on the wire per
//! body byte (`Connection::wire_bytes_per_body_byte`) before it caps the peer. Counted in bodies
//! alone, a sender of small packets put two thirds more on the wire than its share. Virtual time,
//! no randomness.
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

/// UDP and IP header bytes a datagram costs on the link (`cc::DATAGRAM_OVERHEAD_BYTES`).
const DATAGRAM_BYTES: usize = 28;
/// The endpoint header of a sealed connection (`crypto::SEALED_RESERVED_HEADER_BYTES`).
const ENDPOINT_HEADER: [u8; 34] = [0; 34];
const TICK_MS: u64 = 10;

/// A sender under a share of `share` bytes per second that offers `messages` unreliable messages
/// of `message` bytes per 10 ms tick for `seconds`: the bytes it put on the wire after the first
/// second. `on_the_wire` caps it the way a server's shared uplink does (the share divided by the
/// connection's wire cost per body byte, every tick); without it the share caps packet bodies,
/// which is what every share did before ADR-10097.
fn offered(share: f64, message: usize, messages: usize, seconds: u64, on_the_wire: bool) -> u64 {
    let mut c = Connection::new(blind_link());
    let mut wire = 0u64;
    let mut now = 0;
    while now < (seconds + 1) * 1_000 {
        let cost = if on_the_wire {
            c.wire_bytes_per_body_byte()
        } else {
            1.0
        };
        c.set_rate_cap(Some(share / cost));
        for _ in 0..messages {
            // A message the pacer holds back expires with its tick: the game offers a fresh one.
            let _ = c.send(Channel::Unreliable, vec![5; message], ENDPOINT_HEADER.len());
        }
        for d in c.poll(now, &ENDPOINT_HEADER) {
            if now >= 1_000 {
                wire += (d.len() + DATAGRAM_BYTES) as u64;
            }
        }
        now += TICK_MS;
    }
    wire
}

/// A share of 6,000 bytes per second and 80-byte messages at 100 Hz (15.5 kB/s on the wire if
/// all of them left). Capped on the wire, the sender puts its share on the wire over ten seconds
/// and no more (one bucket of slack). The control in the same test caps packet bodies, as every
/// share did before: each 93-byte body rides behind 62 bytes of headers no bucket was charged
/// for, and the sender puts over 130 % of its share on the wire.
#[test]
fn a_share_of_small_packets_is_kept_on_the_wire() {
    let share = 6_000.0;
    let seconds = 10;
    let total = share * seconds as f64;
    let wire = offered(share, 80, 1, seconds, true) as f64;
    let bodies = offered(share, 80, 1, seconds, false) as f64;
    println!(
        "small packets: {:.1} % of the share on the wire (capped in bodies: {:.1} %)",
        wire * 100.0 / total,
        bodies * 100.0 / total
    );
    let packet = (LinkConfig::default().max_packet_bytes + DATAGRAM_BYTES) as f64;
    assert!(
        wire <= total + 5.0 * packet,
        "{wire} B on the wire in {seconds} s at a share of {share} B/s"
    );
    // The share is used, not just respected.
    assert!(
        wire >= 0.9 * total,
        "{wire} B on the wire in {seconds} s: under 90 % of the share of {share} B/s"
    );
    assert!(
        bodies >= 1.3 * total,
        "the control: capped in bodies the sender put {bodies} B on the wire"
    );
}

/// The same with full packets: three 1,100-byte messages per tick against a share of 60,000
/// bytes per second. A full packet's headers are 5 % of it, so the two ways of counting are close:
/// on the wire the sender uses its share and no more.
#[test]
fn a_share_of_full_packets_is_used_and_no_more() {
    let share = 60_000.0;
    let seconds = 10;
    let total = share * seconds as f64;
    let wire = offered(share, 1_100, 3, seconds, true) as f64;
    let bodies = offered(share, 1_100, 3, seconds, false) as f64;
    println!(
        "full packets: {:.1} % of the share on the wire (capped in bodies: {:.1} %)",
        wire * 100.0 / total,
        bodies * 100.0 / total
    );
    assert!(
        (0.95..=1.02).contains(&(wire / total)),
        "full packets used {:.1} % of the share",
        wire * 100.0 / total
    );
    assert!(
        bodies > wire,
        "bodies alone leave the headers out: {bodies} against {wire}"
    );
}

/// A connection that has sent nothing yet costs 1 on the wire per body byte (the cap is the
/// share itself until the first packet), and the figure follows the packets: about 1.67 for
/// 93-byte bodies behind 62 bytes of headers, close to 1.05 for full packets.
#[test]
fn the_wire_cost_follows_the_packets_a_connection_sends() {
    let mut c = Connection::new(LinkConfig::default());
    assert_eq!(c.wire_bytes_per_body_byte(), 1.0);
    for tick in 0..100u64 {
        let _ = c.send(Channel::Unreliable, vec![5; 80], ENDPOINT_HEADER.len());
        let _ = c.poll(tick * TICK_MS, &ENDPOINT_HEADER);
    }
    let small = c.wire_bytes_per_body_byte();
    assert!((1.5..=1.8).contains(&small), "small packets: {small}");
    for tick in 100..400u64 {
        let _ = c.send(Channel::Unreliable, vec![5; 1_100], ENDPOINT_HEADER.len());
        let _ = c.poll(tick * TICK_MS, &ENDPOINT_HEADER);
    }
    let full = c.wire_bytes_per_body_byte();
    assert!((1.03..=1.08).contains(&full), "full packets: {full}");
}

/// ADR-10097: a sender whose packets all vanish while its peer goes on talking reads as
/// unanswered (a server's full uplink queue drops the packets of the peers it serves last). The
/// figure is 0 while acknowledgments come, and 0 again when the peer falls silent too: a link
/// that is down both ways is an outage, not the sender's uplink.
#[test]
fn a_sender_nobody_acknowledges_reads_as_unanswered_while_its_peer_talks() {
    let h = ENDPOINT_HEADER.len();
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    let mut worst_healthy = 0;
    let mut now = 0;
    // Two seconds of a healthy exchange.
    while now < 2_000 {
        a.send(Channel::Unreliable, vec![5; 80], h).expect("queued");
        b.send(Channel::Unreliable, vec![6; 40], h).expect("queued");
        let to_b = a.poll(now, &ENDPOINT_HEADER);
        for d in to_b {
            b.receive(&d[h..], now + 5).expect("a's packet");
        }
        for d in b.poll(now + 5, &ENDPOINT_HEADER) {
            a.receive(&d[h..], now + 10).expect("b's packet");
        }
        while a.next_message().is_some() {}
        while b.next_message().is_some() {}
        worst_healthy = worst_healthy.max(a.unanswered_ms(now + 10));
        now += TICK_MS;
    }
    assert!(
        worst_healthy < 100,
        "a healthy link read {worst_healthy} ms unanswered"
    );
    // a's packets are dropped on the way; b's still arrive (they acknowledge nothing new).
    while now < 3_500 {
        let _ = a.send(Channel::Unreliable, vec![5; 80], h);
        let _ = b.send(Channel::Unreliable, vec![6; 40], h);
        let _dropped = a.poll(now, &ENDPOINT_HEADER);
        for d in b.poll(now + 5, &ENDPOINT_HEADER) {
            a.receive(&d[h..], now + 10).expect("b's packet");
        }
        while a.next_message().is_some() {}
        now += TICK_MS;
    }
    let starved = a.unanswered_ms(now);
    assert!(
        (1_300..=1_600).contains(&starved),
        "{starved} ms after 1.5 s of drops"
    );
    // b falls silent as well: after a second without hearing it, this is an outage.
    while now < 5_000 {
        let _ = a.send(Channel::Unreliable, vec![5; 80], h);
        let _dropped = a.poll(now, &ENDPOINT_HEADER);
        now += TICK_MS;
    }
    assert_eq!(a.unanswered_ms(now), 0);
}
