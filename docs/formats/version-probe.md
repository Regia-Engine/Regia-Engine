# Version probe and version stamp (datagrams, layout version 1)

Owner: `regia_net::version_probe` (spec 120 § 12.2, ADR-7985). Not a file: plaintext datagrams that every build from v0.5.x
on answers or reads on the game socket, so a build of any later version can tell a refused player which side to update.
**Frozen forever**: a later build may add probe version 2, but it still answers a version 1 request with this layout.
Integers are little-endian; `str_u8` is a one-byte length and UTF-8 bytes.

| Datagram | Layout | Size |
|---|---|---|
| Probe request | `RGVP` \| `probe_version u8` (1) \| `nonce u64` \| zero padding | exactly 64 bytes; shorter requests get no answer |
| Probe answer | `RGVR` \| `probe_version u8` \| `nonce u64` \| `wire u32` \| `game_version str_u8` (<= 32) \| `protocol u32` \| `content_hash u64` | at most 64 bytes (never larger than the request) |
| Version stamp | `RGVS` \| `wire u32` \| `protocol u32` \| `content_hash u64` \| `game_version str_u8` (<= 32) | at most 53 bytes |

- `wire` is `REGIA_NET_WIRE_VERSION`, `game_version` is `NetGame::GAME_VERSION` (a session announces its manifest's game
  version), `protocol` is `NetGame::PROTOCOL_VERSION`, `content_hash` is the game's content hash combined with a run-time one
  (`NetServer::set_content_hash`, `LinkOptions::content_hash` plus the `NetLink` app's replicated schema).
- A **stamp** rides in the zero padding of every `ConnectRequest` (byte 18 of the body on), and, when the request carried one,
  after the detail of a `protocol_mismatch` `Deny`. A build that predates stamps reads only its own fields and ignores the
  padding, and is never sent the trailer.
- Answers are rate limited per source prefix: IPv4 /24, IPv6 /56, 4 answers per second with a burst of 8
  (`PROBE_ANSWERS_PER_PREFIX_PER_S`), plus the server's global pre-proof answer cap.

Example: an answer to nonce `0x0102030405060708` from game version `1.2.0`, wire 3, protocol 7, content `0x1122334455667788`
(35 bytes, pinned by `probe_bytes_are_pinned`):

```text
52 47 56 52 01 08 07 06 05 04 03 02 01 03 00 00 00 05 31 2e 32 2e 30 07 00 00 00 88 77 66 55 44 33 22 11
```

Coverage: `cargo test -p regia_net --lib version_probe` pins every byte; the fuzz target `net.version_probe` decodes and
re-encodes the three datagrams and fails on any answer larger than its request.
