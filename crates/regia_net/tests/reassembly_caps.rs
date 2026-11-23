// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 5.2 and § 11.5 (ADR-7981, audit row H4, adversarial row 16): reliable reassembly is
//! bounded. Counted, not timed: a thread-local counting allocator measures the heap one
//! [`Connection`] holds for a hostile peer (the audit's experiment E6), and virtual milliseconds
//! drive every deadline. These tests use only the API that existed before the caps, so each one
//! runs unchanged against the old code and fails there.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_net::LinkConfig;
use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection};

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

fn add(bytes: isize) {
    let _ = LIVE.try_with(|c| c.set(c.get() + bytes));
}

struct Counting;

// SAFETY: forwards to the system allocator; the counter is a thread-local `Cell` that never
// allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as isize);
        // SAFETY: the caller's layout goes straight through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as isize);
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size as isize - layout.size() as isize);
        // SAFETY: as `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        add(-(layout.size() as isize));
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live() -> isize {
    LIVE.with(Cell::get)
}

/// Packet header (a two-byte packet number, no acknowledgment frame) followed by one reliable
/// record.
fn fragment(seq: u16, id: u16, more: bool, frag: &[u8]) -> Vec<u8> {
    let mut p = keepalive(seq);
    p.push(if more { 0x82 } else { 0x02 });
    p.extend_from_slice(&id.to_le_bytes());
    p.extend_from_slice(&(frag.len() as u16).to_le_bytes());
    p.extend_from_slice(frag);
    p
}

/// A packet with no records: the wire 5 header alone (flags `0x01`: a two-byte packet number,
/// no acknowledgment frame; then the number).
fn keepalive(seq: u16) -> Vec<u8> {
    let mut p = vec![0x01];
    p.extend_from_slice(&seq.to_le_bytes());
    p
}

/// The longest fragment a packet of `config` carries: the packet less the 9-byte packet header
/// and the 5-byte reliable record header.
fn max_fragment(config: &LinkConfig) -> usize {
    config.max_packet_bytes - 9 - 5
}

/// The brief's per-connection bound: one reliable message plus one window of packets.
fn per_connection_bound(config: &LinkConfig) -> isize {
    (config.max_reliable_message_bytes + 1024 * config.max_packet_bytes) as isize
}

/// Sends the E6 bomb (ids 1..=1023, never the next expected id 0) with fragments of `len` bytes,
/// then 70 s of keepalives, 100 ms apart. Returns (peak live bytes, live bytes at the end, whether
/// the connection refused traffic by the end).
fn bomb(len: usize) -> (isize, isize, bool) {
    let config = LinkConfig::default();
    let mut c = Connection::new(config);
    let frag = vec![0xEE_u8; len];
    let before = live();
    let mut peak = 0;
    for id in 1..1024_u16 {
        let _ = c.receive(&fragment(id, id, true, &frag), u64::from(id));
        peak = peak.max(live() - before);
    }
    let mut refused = false;
    for i in 0..700_u16 {
        refused = c
            .receive(&keepalive(2000 + i), 2_000 + u64::from(i) * 100)
            .is_err();
        peak = peak.max(live() - before);
    }
    (peak, live() - before, refused)
}

/// Audit E6 exactly (2,000-byte fragments, longer than a 1,200-byte packet carries): before the
/// caps one connection held 2,129,984 B, 8.1x the message cap, for the whole session. Now the first
/// over-long fragment fails the connection and nothing is held.
#[test]
fn the_e6_bomb_is_refused_and_frees_what_it_held() {
    let config = LinkConfig::default();
    let (peak, end, refused) = bomb(2000);
    println!("e6 2000 B: peak={peak} end={end} refused={refused}");
    assert!(
        peak <= per_connection_bound(&config),
        "peak {peak} B over the bound {}",
        per_connection_bound(&config)
    );
    assert!(refused, "the connection must be failed");
    assert!(end <= 64 * 1024, "held {end} B after the failure");
}

/// The E6 bomb with fragments that fit the packet: what is stored stays within one message plus one
/// window of packets, and the gap the peer never fills fails the connection (`reassembly_stalled`)
/// long before 70 s of keepalives are over, freeing everything.
#[test]
fn an_in_size_bomb_is_bounded_and_the_stalled_peer_fails() {
    let config = LinkConfig::default();
    let (peak, end, refused) = bomb(max_fragment(&config));
    println!("e6 in-size: peak={peak} end={end} refused={refused}");
    assert!(
        peak <= per_connection_bound(&config),
        "peak {peak} B over the bound {}",
        per_connection_bound(&config)
    );
    assert!(refused, "the stalled connection must be failed");
    assert!(end <= 64 * 1024, "held {end} B after the failure");
}

/// Adversarial row 16: the first fragment of a message (more follow) and then only keepalives used
/// to wedge the reliable stream for the whole session.
#[test]
fn a_message_that_never_ends_fails_the_connection() {
    let mut c = Connection::new(LinkConfig::default());
    c.receive(&fragment(1, 0, true, &[7; 100]), 0)
        .expect("a first fragment is valid");
    let failed_at = (0..100_u16).find_map(|i| {
        let now = 100 + u64::from(i) * 100;
        c.receive(&keepalive(2 + i), now).is_err().then_some(now)
    });
    let failed_at = failed_at.expect("the stalled message must fail the connection within 10 s");
    // The deadline is max(4 x RTO, 2 s) of the peer's traffic: never sooner than 2 s.
    assert!(failed_at >= 2_000, "failed at {failed_at} ms");
}

/// A reliable fragment longer than one packet of the connection's configuration is a protocol
/// violation, not a message.
#[test]
fn an_over_long_fragment_is_malformed() {
    let config = LinkConfig::default();
    let mut c = Connection::new(config.clone());
    let ok = vec![1_u8; max_fragment(&config)];
    c.receive(&fragment(1, 0, false, &ok), 0)
        .expect("a full fragment is valid");
    assert!(c.next_message().is_some());
    let long = vec![1_u8; max_fragment(&config) + 1];
    assert!(c.receive(&fragment(2, 1, false, &long), 1).is_err());
    assert!(c.next_message().is_none(), "nothing delivered");
    assert!(
        c.receive(&keepalive(3), 2).is_err(),
        "and the connection is failed for good"
    );
}

/// An honest 256 KiB reliable message over 10 % loss, 10 % reorders, duplicates and jitter both
/// ways (harsher than the bad-wifi profile's ~3 % loss) arrives whole: an honest sender never
/// trips a cap or the stall deadline.
#[test]
fn an_honest_256_kib_message_arrives_over_a_bad_link() {
    let impaired = |seed| {
        NetConditioner::new(ConditionerConfig {
            enabled: true,
            seed,
            loss: 0.10,
            duplicate: 0.01,
            reorder: 0.10,
            reorder_max_ms: 30,
            delay_ms: 8,
            jitter_ms: 30,
            ..ConditionerConfig::default()
        })
    };
    let to = std::net::SocketAddr::from(([10, 0, 0, 1], 1));
    let header = [0xAA_u8, 0xBB];
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    let mut ab = impaired(7);
    let mut ba = impaired(8);
    let message: Vec<u8> = (0..256 * 1024_u32).map(|i| (i * 31 % 251) as u8).collect();
    a.send(Channel::GAME_RPC, message.clone(), header.len())
        .expect("fits");
    let mut got = None;
    for now in 0..60_000_u64 {
        let mut out = Vec::new();
        for p in a.poll(now, &header) {
            out.extend(ab.process(p, to, now));
        }
        out.extend(ab.release_due(now));
        for (bytes, _) in out {
            b.receive(&bytes[header.len()..], now)
                .expect("honest traffic is valid");
        }
        let mut back = Vec::new();
        for p in b.poll(now, &header) {
            back.extend(ba.process(p, to, now));
        }
        back.extend(ba.release_due(now));
        for (bytes, _) in back {
            a.receive(&bytes[header.len()..], now)
                .expect("honest traffic is valid");
        }
        if let Some((Channel::GAME_RPC, bytes)) = b.next_message() {
            got = Some(bytes);
            break;
        }
    }
    assert_eq!(got.as_deref(), Some(&message[..]), "the message arrives whole");
}
