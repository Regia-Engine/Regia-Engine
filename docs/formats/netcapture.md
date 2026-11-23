# `.netcapture` — client capture with timing (binary)

Owner: `regia_net::capture` (spec 103 § 14, spec 120 § 15.5, ADR-8221). Current `format_version`: **1**.

What one client received and did, in its own milliseconds: each read of its socket, each connection packet after decryption,
the server's accept, refusal or goodbye, each input stamp, each flush and the game's frame markers with an output digest.
Written by `regia_net::capture::{ClientCapture::save, CaptureTap::save, CaptureFile::save}`; read by
`regia_net::capture::CaptureFile::{load, from_bytes}` and replayed by a `NetClient::replay` client (the net soak's
`regia_net_soak::workload::replay_client`). Not the server-side [`.netdemo`](netdemo.md), which holds what a server sent a
virtual connection and has no timing. A capture holds decrypted traffic: treat it as private game data.

## Layout

All integers little-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `b"RNETCAPT"` |
| 8 | 4 | `format_version` (u32) |
| 12 | 4 | header body length `n` (u32, at most 64 KiB) |
| 16 | 4 | CRC-32 (IEEE) of the body |
| 20 | n | header body |
| 20 + n | ... | records, then the end record |

Header body, in order:

```text
label: u32 length + UTF-8    free text; the net soak writes {"scenario": ..., "client": i} as JSON
ring_bytes: u64              the capture ring's size when it was saved
peak_used_bytes: u64         the most bytes the ring ever held
written: u64                 records written since the ring started
evicted: u64                 records evicted to make room
oversized: u64               records larger than the ring, dropped
```

Each record: `kind` (u8), payload length `len` (u32, at most 1 MiB, checked against the bytes left before anything is
allocated), CRC-32 (IEEE) of the kind byte and the payload, the payload.

| Kind | Name | Payload |
|---|---|---|
| 1 | session | game id (u16 length + UTF-8), protocol id (u64): one connection attempt starts |
| 2 | receive | `at_ms` (u64): `NetClient::receive` at this client time |
| 3 | flush | `at_ms` (u64): `NetClient::flush` |
| 4 | accept | sealed (u8, 0 or 1), the `Accept` datagram re-encoded after decryption |
| 5 | deny | the `Deny` datagram re-encoded after decryption |
| 6 | packet | one connection packet, after decryption and the session-token check |
| 7 | goodbye | has reason (u8, 0 or 1), reason byte (u8) |
| 8 | ended | the end code (UTF-8, `ClientEnd::as_str`) |
| 9 | input | tick (u64), the encoded input (`NetCodec::to_bytes`) |
| 10 | frame | `at_ms` (u64, the game's frame time), output digest (u64) |
| 11 | handshake | round trip in ms (u64): the handshake's shortest round trip, written in front of the accept it led to; a replay anchors its clock with it (ADR-10096, ADR-10231). Optional: a session whose handshake measured none has no such record |
| 238 | end | record count (u64); must be last and match |

Rules: the first record is a session; a file holds whole sessions only (a ring that evicted the start of a session leaves
that session out). Any other kind, a wrong length for a fixed-size kind, a bad checksum, a count that does not match or bytes
after the end record is `capture_corrupt`; a file cut short is `capture_truncated`; another magic is `capture_not_a_capture`;
a version outside 1..=1 is `capture_version`.

## Versions

| Version | Change |
|---|---|
| 1 | First version (ADR-8221). Record kind 11 (`handshake`) joined it before the format first shipped (ADR-10231; an additive kind folded into the unreleased version, ADR-7724): a capture written before it has no such record and replays as it did. |

A capture belongs to its protocol, as a `.netdemo` does: `regia upgrade-project` recognizes the file and rewrites nothing
while the version is current.
