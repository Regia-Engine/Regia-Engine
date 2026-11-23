// SPDX-License-Identifier: MPL-2.0
//! Stream 2 has one name (spec 120 § 12.3, ADR-10067): v0.7 removes the v0.6 alias
//! `Channel::Reliable`, so a message on the game RPC stream is `Channel::Stream(StreamId::GAME_RPC)`
//! (spelled `Channel::GAME_RPC`) on both ends, whichever form sent it. Public `Connection` API, no
//! wall clock.

use regia_net::connection::{Channel, Connection, LinkConfig, StreamId};

/// Sends `message` on `channel` from a fresh connection and returns what the peer reports.
fn deliver(channel: Channel, message: Vec<u8>) -> Option<(Channel, Vec<u8>)> {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    a.send(channel, message, 0).expect("queued");
    for packet in a.poll(0, &[]) {
        b.receive(&packet, 0).expect("valid packet");
    }
    b.next_message()
}

#[test]
fn stream_two_is_reported_as_the_stream_it_is() {
    assert_eq!(
        Channel::of_stream(StreamId::GAME_RPC),
        Channel::Stream(StreamId::GAME_RPC),
        "stream 2 maps to its own stream, not an alias"
    );
    for stream in [StreamId::CONTROL, StreamId::GAME_RPC, StreamId::BULK, StreamId(9)] {
        assert_eq!(
            Channel::of_stream(stream),
            Channel::Stream(stream),
            "stream {}",
            stream.0
        );
    }
}

#[test]
fn game_rpc_const_is_stream_two() {
    assert_eq!(Channel::GAME_RPC, Channel::Stream(StreamId::GAME_RPC));
    assert_eq!(Channel::GAME_RPC.stream(), Some(StreamId::GAME_RPC));
    assert!(Channel::GAME_RPC.is_reliable());
    // Usable as a pattern.
    let kind = match Channel::Stream(StreamId(2)) {
        Channel::GAME_RPC => "game_rpc",
        Channel::Stream(_) => "other stream",
        Channel::Unreliable => "unreliable",
    };
    assert_eq!(kind, "game_rpc");
}

#[test]
fn a_game_rpc_message_arrives_on_the_stream_it_was_sent_on() {
    let explicit = deliver(Channel::Stream(StreamId::GAME_RPC), b"explicit".to_vec());
    assert_eq!(
        explicit,
        Some((Channel::Stream(StreamId::GAME_RPC), b"explicit".to_vec()))
    );
    let named = deliver(Channel::GAME_RPC, b"named".to_vec());
    assert_eq!(named, Some((Channel::GAME_RPC, b"named".to_vec())));
}

#[test]
fn the_channel_serializes_as_its_stream() {
    let text = ron::to_string(&Channel::GAME_RPC).expect("serializes");
    assert!(text.starts_with("Stream("), "{text}");
    let back: Channel = ron::from_str(&text).expect("parses");
    assert_eq!(back, Channel::GAME_RPC);
    assert!(
        ron::from_str::<Channel>("Reliable").is_err(),
        "the removed alias no longer parses"
    );
}
