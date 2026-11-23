# Net entropy coding (agents)

The entropy stage that shrinks replication state frames below what field deltas and quantization achieve
(`regia_net::entropy`, spec 72 § 18, ADR-3315). Guide topic `net-entropy`.

## What it is

- An adaptive binary range coder (Martin 1979; the public LZMA specification's range coder; provenance in
  `docs/provenance.md`) codes each state frame's body — the record count plus the records — bit by bit against 64
  per-frame adaptive models. The models reset every frame, so a lost, duplicated or reordered frame decodes
  identically however many frames came before it.
- When coding would not shrink a frame, a raw fallback sends it verbatim (one flags byte of overhead).
- Only entity state is compressed. Handshake, authentication, passwords, chat and every game message travel
  uncompressed, so attacker-chosen bytes never sit next to secrets in a compressed blob (the CRIME/BREACH class).
- The stage runs before the link's encryption, on authenticated plaintext; a corrupted or truncated payload is a
  decode error (the connection takes the existing malformed-frame path), never a panic.

## Negotiation

- `NET_PROTOCOL_VERSION` is 3; `MIN_NET_PROTOCOL_VERSION` stays 1. A protocol 2 peer connects with the stage off and
  receives byte-identical protocol 2 frames (a pinned-bytes test guards this).
- The server enables the stage per connection when it is configured for it **and** the client's first `ACK` names
  protocol ≥ 3 — the same acknowledgment that decides the state wire, so no extra round trip.
- Config: `replication.entropy: Off | Range` (default `Off`) in `config/net.ron`; a dedicated host's
  `server.toml` `[net] entropy = "off" | "range"` overrides it for new joins. Negotiated connections receive
  `TAG_STATE_ENTROPY` frames; no peer ever parses a frame it did not negotiate.

## `net.entropy_stats`

Server only. Params: `{ peer? }` (a peer id; omit for every connection).

Returns:

- `protocol`, `min_protocol`: this build's replication protocol versions.
- `configured`: the `config/net.ron` mode.
- `server_toml_override`: the dedicated host's `server.toml` mode, or `null`.
- `peers`: per connection `peer`, `wire`, `entropy` (the negotiated mode), `frames`, `coded`, `raw_fallback`,
  `bytes_in` and `bytes_out` (body bytes before and after the stage), `saved`.

Errors (`F0002`, `data.reason`):

| Reason | When |
|---|---|
| `net_not_running` | the app has no regia_net runtime |
| `net_not_server` | called on a client |
| `peer_unknown` | no connected peer with that id |

## Reading the numbers

`bytes_in` is what the protocol 2 wire would have carried; `bytes_out` is what the entropy wire carried, flag and
length prefix included. The harness measurements behind the defaults are in spec 72 § 18: on the measured workloads
the stage saves 2-9 % of state-frame bytes, median per frame; more when frames pack many entities, less on tiny
frames (model warmup dominates).
