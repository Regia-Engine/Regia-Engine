// SPDX-License-Identifier: MPL-2.0
//! Payload traits: everything game-specific enters `regia_net` through these.
//!
//! The engine never knows a game's messages, inputs or join data. A game implements [`NetCodec`]
//! for its own types and names them in one [`NetGame`] implementation; the server, client and tick
//! driver are generic over that trait. Two games with different [`NetGame`] types get different
//! [`protocol_id`]s and refuse each other at the handshake.

use crate::wire::{WireError, WireReader, WireWriter, fnv1a_64};

/// Version of `regia_net`'s own framing (packet header, handshake, channels). Mixed into every
/// [`protocol_id`], so a framing change refuses old builds instead of misreading them. Version 2
/// adds the sealed records; version 3 adds the reliable input lane for inputs larger than one
/// datagram (ADR-7896); version 4 adds the server's input feedback lane (spec 120 § 6.4,
/// ADR-8077). Version 5 (the v0.6 line's one bump, spec 120 § 12.1) is transport v2: 62-bit packet
/// numbers and acknowledgment frames with ranges (spec 120 § 4.1, ADR-8184), and the resume ticket in
/// the join token and the accept (ADR-8298). Version 6 (the v0.7 line's one bump) opens with the
/// one-round-trip ticket resume: the resumption ticket in the sealed accept and the `ResumeInit`,
/// `ResumeReply` and `ResumeRetry` datagrams (spec 120 § 10.3, ADR-8858). Wire 6 also carries the
/// state channel's 512-version ring (`replication::STATE_VERSION_RING`, ADR-10051): a server of
/// this wire names baselines up to 512 versions back, which a wire 5 client (64) would not hold.
pub const REGIA_NET_WIRE_VERSION: u32 = 6;

/// A type that can be written to and read from the wire.
///
/// `decode` reads untrusted bytes: it must return an error, never panic, on any input.
pub trait NetCodec: Sized {
    /// Appends the encoded value.
    fn encode(&self, w: &mut WireWriter);
    /// Reads one value.
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError>;

    /// Encodes into a fresh buffer. Fails when a field does not fit its length prefix.
    fn to_bytes(&self) -> Result<Vec<u8>, WireError> {
        let mut w = WireWriter::new();
        self.encode(&mut w);
        w.finish_checked()
    }

    /// Decodes a whole buffer; trailing bytes are an error.
    fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(bytes);
        let value = Self::decode(&mut r)?;
        r.finish()?;
        Ok(value)
    }

    /// Appends the value coded against `previous`, the input of the tick before it in the same
    /// input bundle (spec 120 § 6.1 item 2, ADR-8312). The default writes the full value
    /// ([`NetCodec::encode`]), so bundles of a codec that does not override it are byte for byte
    /// what they were. A game that overrides it must override [`NetCodec::decode_delta`] to match;
    /// both ends run the same game build (the game version is in the protocol id). Input bundles
    /// are cut by the full encoded length, so a delta never makes a bundle overflow its datagram.
    fn encode_delta(&self, previous: &Self, w: &mut WireWriter) {
        let _ = previous;
        self.encode(w);
    }

    /// Reads a value written by [`NetCodec::encode_delta`] against `previous` (untrusted bytes).
    /// The default reads a full value ([`NetCodec::decode`]).
    fn decode_delta(previous: &Self, r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let _ = previous;
        Self::decode(r)
    }

    /// Spec 120 § 6.3, for a per-tick input: folds `late`, an input that reached the server after
    /// its tick was simulated without it, into `next`, the input of the next tick the server builds
    /// ([`crate::tick::TickDriver::set_late_merge`]). An input type with button presses or
    /// commands overrides it to keep them (OR the presses of both) and take `next`'s analog values;
    /// a field that must not carry over takes `next`'s value (the per-field opt-out). The default
    /// keeps `next` whole. When the next tick has no input of its own, the late input is used as it
    /// is, without calling this.
    fn merge_late(late: Self, next: Self) -> Self {
        let _ = late;
        next
    }
}

impl NetCodec for () {
    fn encode(&self, _w: &mut WireWriter) {}
    fn decode(_r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(())
    }
}

macro_rules! codec_int {
    ($($t:ident),*) => {$(
        impl NetCodec for $t {
            fn encode(&self, w: &mut WireWriter) {
                w.$t(*self);
            }
            fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
                r.$t()
            }
        }
    )*};
}
codec_int!(u8, u16, u32, u64, i32, f32);

/// Raw byte payloads. A `u16` length prefix holds at most 65,535 bytes. A longer value fails
/// with [`WireError::TooLong`] and writes no field. Games that need a larger blob send it as
/// several messages, or as a stream.
impl NetCodec for Vec<u8> {
    fn encode(&self, w: &mut WireWriter) {
        if let Err(err) = w.bytes_u16(self, usize::from(u16::MAX)) {
            w.fail(err);
        }
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(r.bytes_u16(usize::from(u16::MAX))?.to_vec())
    }
}

/// One game's network vocabulary. Implement it on a marker type and pass that type to
/// `NetServer` / `NetClient`.
pub trait NetGame: 'static {
    /// The game's message set, sent on the reliable or the unreliable channel.
    type Message: NetCodec + Clone + std::fmt::Debug + Send + 'static;
    /// The game's per-tick input, collected by the authoritative tick driver.
    type Input: NetCodec + Clone + std::fmt::Debug + PartialEq + Send + 'static;
    /// Data a client sends with its join request (team choice, loadout, ...). Use `()` for none.
    /// At most [`crate::handshake::MAX_JOIN_DATA_BYTES`] encoded bytes.
    type JoinData: NetCodec + Clone + std::fmt::Debug + Send + 'static;

    /// Stable game identifier, for example a reverse-domain name.
    const GAME_ID: &'static str;
    /// Version of the game's message set. Bump it on any incompatible change.
    const PROTOCOL_VERSION: u32;
    /// The game's version text, as players know it (`"1.4.2"`), at most
    /// [`crate::version_probe::MAX_GAME_VERSION_BYTES`] bytes. Not part of [`protocol_id`]: it only
    /// tells a refused player which side to update (the version probe, spec 120 § 12.2). Dotted
    /// numbers order builds; empty (the default) leaves the wire and protocol versions to do it.
    const GAME_VERSION: &'static str = "";

    /// Hash of content both sides must agree on (a cooked data table, rules file, ...). Mixed into
    /// [`protocol_id`]; `0` when the game has none.
    fn content_hash() -> u64 {
        0
    }

    /// Server-side check of a join request's game data. `Err(reason)` refuses the join with
    /// `DenyReason::InvalidJoin` and the reason text.
    fn validate_join(_data: &Self::JoinData) -> Result<(), String> {
        Ok(())
    }

    /// Store-ticket bytes the server's `JoinTicketCheck` sees, when the host installed one.
    ///
    /// The default is an empty slice, so a game that does not carry a ticket is unchanged.
    /// The password gate still runs first; this slice is not a password.
    fn join_ticket(_data: &Self::JoinData) -> &[u8] {
        &[]
    }
}

/// The 64-bit protocol id both peers must share: FNV-1a over the framing version, the game id and
/// the game's protocol version, combined with the game's content hash.
pub fn protocol_id<G: NetGame>() -> u64 {
    let mut w = WireWriter::new();
    w.u32(REGIA_NET_WIRE_VERSION);
    w.raw(G::GAME_ID.as_bytes());
    w.u8(0);
    w.u32(G::PROTOCOL_VERSION);
    fnv1a_64(w.as_slice()) ^ G::content_hash().rotate_left(17)
}

/// The content hash a build declares in its version stamp: the game's compiled-in
/// [`NetGame::content_hash`] combined with one known only at run time (a mounted content set, a
/// `NetLink` app's replicated schema; ADR-7985). Either alone is itself; both are hashed together
/// (FNV-1a over both).
pub fn combined_content_hash(compiled: u64, runtime: u64) -> u64 {
    match (compiled, runtime) {
        (c, 0) => c,
        (0, r) => r,
        (c, r) => {
            let mut w = WireWriter::new();
            w.u64(c);
            w.u64(r);
            fnv1a_64(w.as_slice())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct GameA;
    impl NetGame for GameA {
        type Message = Vec<u8>;
        type Input = u32;
        type JoinData = ();
        const GAME_ID: &'static str = "test.a";
        const PROTOCOL_VERSION: u32 = 1;
    }

    struct GameB;
    impl NetGame for GameB {
        type Message = Vec<u8>;
        type Input = u32;
        type JoinData = ();
        const GAME_ID: &'static str = "test.a";
        const PROTOCOL_VERSION: u32 = 2;
    }

    struct GameC;
    impl NetGame for GameC {
        type Message = Vec<u8>;
        type Input = u32;
        type JoinData = ();
        const GAME_ID: &'static str = "test.a";
        const PROTOCOL_VERSION: u32 = 1;
        fn content_hash() -> u64 {
            0x1234
        }
    }

    #[test]
    fn protocol_id_separates_versions_and_content() {
        assert_ne!(protocol_id::<GameA>(), protocol_id::<GameB>());
        assert_ne!(protocol_id::<GameA>(), protocol_id::<GameC>());
        assert_eq!(protocol_id::<GameA>(), protocol_id::<GameA>());
    }

    #[test]
    fn builtin_codecs_round_trip() {
        assert_eq!(u32::from_bytes(&7u32.to_bytes().expect("int")), Ok(7));
        assert_eq!(
            Vec::<u8>::from_bytes(&vec![1, 2, 3].to_bytes().expect("bytes")),
            Ok(vec![1, 2, 3])
        );
        assert_eq!(<()>::from_bytes(&[]), Ok(()));
        assert!(matches!(
            u32::from_bytes(&[1, 2, 3, 4, 5]),
            Err(WireError::Trailing(1))
        ));
    }

    #[test]
    fn a_vec_longer_than_the_u16_prefix_is_an_error() {
        let big = vec![1u8; usize::from(u16::MAX) + 1];
        assert!(matches!(
            big.to_bytes(),
            Err(WireError::TooLong { len, max })
                if len == big.len() && max == usize::from(u16::MAX)
        ));
        let ok = vec![7u8; 4];
        let bytes = ok.to_bytes().expect("short payload");
        assert_eq!(Vec::<u8>::from_bytes(&bytes).expect("decode"), ok);
    }
}
