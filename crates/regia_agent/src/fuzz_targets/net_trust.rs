// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for the transport trust work of ADR-2416: sealed records under deterministic keys,
//! across a rekey, and the `known_hosts.ron` store.

use std::fmt::Display;

use regia_net::crypto::{SealedHeader, SealedKind, fixture_link_pair};
use regia_net::handshake::{PacketKind, split_datagram};
use regia_net::known_hosts::{KNOWN_HOSTS_FORMAT_VERSION, KnownHosts};

use super::{BUG_PREFIX, FuzzInput, FuzzTarget};

// The network hardening targets of ADR-3283 (sealed bodies, the client datagram path, bridge frame
// parts, lobby queries), registered with these so the registry list in `mod.rs` stays as it is.
#[path = "net_wave2.rs"]
mod net_wave2;

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// The fixture links' key seed: public, so the records are the same on every run and machine.
const SEED: [u8; 32] = [0x5E; 32];
/// The fixture session token.
const TOKEN: u32 = 0x00C0_FFEE;
/// Records one input holds at most.
const MAX_RECORDS: usize = 64;

/// The fixture's records: payloads under the first key, a rekey, payloads under the second key,
/// and a goodbye. Empty if the cipher cannot run (the target then rejects everything).
fn sealed_records() -> Vec<Vec<u8>> {
    let Ok((mut client, _)) = fixture_link_pair(&SEED, TOKEN) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for body in [&b"tick 1"[..], b"", &[0xA5; 64]] {
        out.extend(client.seal(SealedKind::Payload, body).ok());
    }
    if client.rekey_now().is_err() {
        return Vec::new();
    }
    for body in [&b"tick 2"[..], &[0x3C; 200]] {
        out.extend(client.seal(SealedKind::Payload, body).ok());
    }
    out.extend(client.seal(SealedKind::Disconnect, &[1]).ok());
    out
}

/// `u16` length, then the datagram, per record.
fn frame(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        let len = u16::try_from(r.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&r[..usize::from(len).min(r.len())]);
    }
    out
}

fn sealed_record_fixture() -> Vec<u8> {
    frame(&sealed_records())
}

/// Opens every framed record, in order, with the fixture's server link: the window, the key epochs
/// and the tag decide. Whatever authenticates must be byte for byte one of the fixture's records
/// (anything else authenticating is a forgery the record layer let through, a bug); the encoding is
/// the authenticated records, framed again, which open the same way on a fresh link.
fn sealed_record_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let genuine = sealed_records();
    let (_, mut server) = fixture_link_pair(&SEED, TOKEN).map_err(e)?;
    let mut accepted = Vec::new();
    let mut rest = b;
    let mut count = 0;
    while !rest.is_empty() {
        count += 1;
        if count > MAX_RECORDS {
            return Err(format!("more than {MAX_RECORDS} records"));
        }
        let (len, tail) = rest.split_first_chunk::<2>().ok_or("truncated length")?;
        let len = usize::from(u16::from_le_bytes(*len));
        if tail.len() < len {
            return Err("truncated record".into());
        }
        let (datagram, tail) = tail.split_at(len);
        rest = tail;
        let Some((PacketKind::Sealed, body)) = split_datagram(datagram) else {
            continue;
        };
        let Some(header) = SealedHeader::parse(body) else {
            continue;
        };
        if server.open(header).is_ok() {
            if !genuine.iter().any(|g| g.as_slice() == datagram) {
                return Err(format!("{BUG_PREFIX}a record no fixture sealed authenticated"));
            }
            accepted.push(datagram.to_vec());
        }
    }
    if accepted.is_empty() {
        return Err("no record authenticated".into());
    }
    Ok(frame(&accepted))
}

fn known_hosts_fixture() -> String {
    concat!(
        "(\n",
        "    format_version: 1,\n",
        "    hosts: {\n",
        "        \"203.0.113.7:7777\": (\n",
        "            key: \"8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a\",\n",
        "            source: FirstUse,\n",
        "        ),\n",
        "        \"lobby:night-raid\": (\n",
        "            key: \"de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f\",\n",
        "            source: Manual,\n",
        "        ),\n",
        "    },\n",
        ")\n",
    )
    .into()
}

/// The ADR-2416 targets, then the ADR-3283 ones.
pub(super) fn targets() -> Vec<FuzzTarget> {
    let mut all = vec![
        // Sealed records under fixed keys, across one rekey (ADR-2416).
        FuzzTarget {
            name: "net.sealed_record",
            format: None,
            format_version: regia_net::REGIA_NET_WIRE_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: sealed_record_fixture,
                load: sealed_record_load,
            },
        },
        // The client's trust-on-first-use store (`known_hosts.ron`, ADR-2416).
        FuzzTarget {
            name: "net.known_hosts",
            format: Some("known-hosts"),
            format_version: KNOWN_HOSTS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: known_hosts_fixture,
                load: |s| KnownHosts::from_ron(s).map(|k| k.to_ron()).map_err(e),
            },
        },
    ];
    all.extend(net_wave2::targets());
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_opens_every_record_and_crosses_a_rekey() {
        let records = sealed_records();
        assert_eq!(records.len(), 6);
        let epochs: Vec<u8> = records
            .iter()
            .filter_map(|r| split_datagram(r).and_then(|(_, b)| SealedHeader::parse(b)))
            .map(|h| h.epoch())
            .collect();
        assert_eq!(epochs, [0, 0, 0, 1, 1, 1]);
        let fixture = sealed_record_fixture();
        assert_eq!(sealed_record_load(&fixture), Ok(fixture.clone()));
        assert_eq!(sealed_record_fixture(), fixture, "deterministic");
    }

    #[test]
    fn a_flipped_byte_never_authenticates_and_replays_are_dropped() {
        let records = sealed_records();
        let first = &records[0];
        for i in 0..first.len() {
            let mut bad = first.clone();
            bad[i] ^= 0x01;
            assert!(sealed_record_load(&frame(&[bad])).is_err(), "byte {i}");
        }
        let twice = frame(&[first.clone(), first.clone()]);
        assert_eq!(sealed_record_load(&twice), Ok(frame(std::slice::from_ref(first))));
    }

    #[test]
    fn the_known_hosts_fixture_round_trips() {
        let text = known_hosts_fixture();
        let loaded = KnownHosts::from_ron(&text).expect("fixture loads");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.to_ron(), text, "the fixture is already canonical");
    }
}
