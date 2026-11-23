// SPDX-License-Identifier: MPL-2.0
//! The entropy stage's state-frame decoder (spec 72 § 18, ADR-3315): `net_entropy_decode`. The
//! fixture is a `TAG_STATE_ENTROPY` frame whose body range-codes smaller than raw (the common
//! case); the loader unwraps it and re-encodes canonically, so the round-trip property holds on
//! every mutated input that still loads.

use regia_net::bits::{BitReader, BitWriter};
use regia_net::entropy::{self, TAG_STATE_ENTROPY};

use super::{FuzzInput, FuzzTarget};

/// A valid entropy-coded state frame: tag 8, tick 7, seq 3, the coded flag, and the coded body.
fn fixture() -> Vec<u8> {
    let body: Vec<u8> = (0..300u32)
        .map(|i| if i.is_multiple_of(3) { 0 } else { (i % 7) as u8 })
        .collect();
    let (payload, true) = entropy::code_or_raw(&body) else {
        // invariant: this fixed, generated fixture must exercise the coded branch; no user bytes enter here.
        panic!("the fixture body must code smaller than raw");
    };
    let mut w = BitWriter::new();
    w.write_bits(u64::from(TAG_STATE_ENTROPY), 8);
    w.write_varint(7);
    w.write_varint(3);
    w.write_bits(1, 8);
    w.write_varint(payload.len() as u64);
    w.write_bytes(&payload);
    w.finish()
}

/// Parses and re-encodes. Every failure is a rejection (`Err`), never a panic; the return is the
/// canonical encoding, which the registry round-trips.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let frame = entropy::unwrap_frame(bytes).map_err(|e| e.to_string())?;
    // This target decodes entropy-negotiated frames; a plain protocol 2 frame is out of scope.
    if frame.tag != TAG_STATE_ENTROPY {
        return Err("not an entropy frame".into());
    }
    // The unwrapped frame is in the raw form: tag, tick, seq, flags 0, body.
    let mut r = BitReader::new(&frame.bytes);
    let tag = r.read_bits(8).map_err(|e| e.to_string())?;
    if tag != u64::from(frame.tag) {
        return Err("tag mismatch".into());
    }
    let tick = r.read_varint().map_err(|e| e.to_string())?;
    let seq = r.read_varint().map_err(|e| e.to_string())?;
    if frame.tag == TAG_STATE_ENTROPY {
        let flags = r.read_bits(8).map_err(|e| e.to_string())?;
        if flags != 0 {
            return Err("unwrapped frame keeps its flags byte set".into());
        }
    }
    let body = &frame.bytes[r.bit_pos() / 8..];
    // Canonical re-encode: coded when smaller, the raw fallback otherwise.
    let (payload, coded) = entropy::code_or_raw(body);
    let mut w = BitWriter::new();
    w.write_bits(u64::from(frame.tag), 8);
    w.write_varint(tick);
    w.write_varint(seq);
    if frame.tag == TAG_STATE_ENTROPY {
        if coded {
            w.write_bits(1, 8);
            w.write_varint(payload.len() as u64);
            w.write_bytes(&payload);
        } else {
            w.write_bits(0, 8);
            w.write_bytes(body);
        }
    } else {
        w.write_bytes(body);
    }
    Ok(w.finish())
}

/// The entropy target.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "net_entropy_decode",
        format: None,
        format_version: regia_net::NET_PROTOCOL_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_loads_and_round_trips() {
        let bytes = fixture();
        assert_eq!(bytes[0], TAG_STATE_ENTROPY);
        let canonical = load(&bytes).expect("loads");
        assert_eq!(load(&canonical).expect("reloads"), canonical);
        assert_eq!(canonical, bytes, "the fixture is already canonical");
        // The plain protocol 2 tag is out of scope for this target.
        let mut plain = bytes.clone();
        plain[0] = 7;
        assert!(load(&plain).is_err());
        // Truncations reject.
        for cut in 0..bytes.len() {
            let _ = load(&bytes[..cut]);
        }
    }
}
