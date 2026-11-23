# Net transport encryption (agents)

`regia_net` encrypts and authenticates connected traffic (`regia_net::crypto`, ADR-1901). Guide topic `net-crypto`. Read this page
before you tell anyone a link is secure: what it protects depends on whether the client pinned the server's key.

## The switch

| Mode | Plain link allowed |
|---|---|
| `off` | Always. A peer that requires encryption is refused |
| `auto` (default) | Loopback only. Every other peer must be sealed |
| `required` | Never, loopback included |

Each side decides from its own mode and the peer's address, so a network attacker cannot strip encryption from a link that
requires it. Set the mode in `ServerConfig::crypto`, `ClientConfig::crypto`, `session.encryption` in `config/net.ron`, `[net]
encryption` in `server.toml`, or `encryption` on `session.host` and `session.connect`.

## What it protects

| Threat | Result |
|---|---|
| A listener on the network | Sees sizes and timing, not contents: messages, join tokens, passwords, admin tokens and reconnect tokens are sealed |
| Tampered, truncated or forged datagrams | Dropped, and the session continues. Counted in `auth_failed` or `malformed` |
| Replayed datagrams | Dropped before decryption. Counted in `replayed` |
| Plain traffic on a sealed link, even with the right session token | Dropped. Counted in `auth_failed` |
| Rewriting the plain negotiation | The handshake fails, or the client refuses |
| Spoofed source addresses | Cost one keyed hash and a reply no larger than the request. Nothing is charged to an unproved source, so forging a victim's address cannot use up its handshake budget (ADR-3283). Curve work starts only after a valid cookie, capped per address block (an IPv4 address or an IPv6 /64) and per second |

| Not protected | Why |
|---|---|
| **An unpinned client against an active attacker** | Without a pin the client accepts any key, so an attacker in the middle can read its traffic, password included. `session.connect` and `NetLink` with a known-hosts store pin on first use; the very first connection, and a connect with `allow_unpinned`, are still exposed. Pin the key when this matters |
| Blocking traffic | An on-path attacker can always drop packets, and can forge the plain `Deny` before the key exchange finishes |
| Traffic size and timing | Not hidden |
| A client's identity | The handshake authenticates the server only. Use the session authenticator (spec 77 § 5) |

The suite is `Noise_NX_25519_ChaChaPoly_BLAKE2s` through the `snow` crate. `snow` has had no formal audit (ADR-1901), so do not
describe this transport as audited.

## Server keys and pinning

A server proves its static key in the handshake. `net.status` reports it as `crypto.server_key`, and on a dedicated server the
`serverkey` console command prints it.

- Pin with `ConnectOptions::server_key`, `ClientConfig::server_key` or `server_key` on `session.connect` (64 hex digits). A pin
  forces a sealed link, on loopback too. A mismatch ends the attempt with `server_key_mismatch` before any secret leaves the client.
- Get the key to clients over something the attacker does not control: a join code or lobby listing from your own service, the
  store page, or the server's operator. The relay's opaque connect token and the LAN discovery card can carry it.
- A dedicated server keeps its key in `server.key` ([format](../../docs/formats/server-key.md)). A listen server keeps its key in
  `user://listen-server.key` (created on the first host, reused on later runs) when the app has a `user://` mount, so returning
  players' pinned keys still match (ADR-4936); without a `user://` mount, or when the game calls `NetSession::set_identity` or
  `NetSession::set_listen_identity_path`, the game chooses — and a bare `NetSession` keeps a fresh per-run key.
- A `NetLink` client can trust a key on first use instead: `LinkOptions::known_hosts` records the key the first time it meets a
  host and refuses a different one later with `net_host_key_changed` (ADR-2416, [net-trust](net-trust.md)). `session.connect`
  does the same through `user://known_hosts.ron` unless the call passes `allow_unpinned` (ADR-3283,
  [net-session](net-session.md)).

## Joining from an address that already has a peer

A new join replaces the peer at its address only after it passes every join check, and a peer on a sealed link only once that peer
has been silent for 2 s (`regia_net::server::SEALED_PEER_QUIET_MS`). Until then the server drops the join without an answer and counts
it in `handshakes_refused`. A client that restarted on the same port keeps resending and gets in. Someone who reads a live peer's
traffic and forges its address does not.

## Passwords and remote admin

- With `encryption: off`, a client refuses to send a password to a non-loopback server, and a host refuses a password or admin token
  on a non-loopback bind: `password_needs_encryption`.
- On a sealed link the transport password proof includes the Noise handshake hash, so a recorded proof is useless on another session.
- A wrong password ends the key exchange: one exchange buys one guess. If the sealed refusal is lost, the client ends with
  `connect_timeout`.
- Remote admin needs a sealed link. A login over a plain link gets `remote admin needs an encrypted link`, and no strike.
- A host password is still a password: pin the key, and do not reuse it elsewhere.

## Agent surface

`net.crypto {endpoint?}` answers, per endpoint:

```json
{ "endpoints": [{ "label": "server", "role": "server", "mode": "auto", "suite": "Noise_NX_25519_ChaChaPoly_BLAKE2s",
  "server_key": "85…", "pinned": false, "sealed": true, "sealed_peers": 3,
  "counters": { "sealed_out": 9120, "opened_in": 8877, "auth_failed": 0, "replayed": 2,
                "handshakes": 3, "handshakes_refused": 0, "pre_proof_shed": 0 } }] }
```

| Field | Meaning |
|---|---|
| `server_key` | Server: its key. Client: the key the server proved |
| `pinned` | Client: a pin was checked |
| `sealed` | Client: its link is sealed. Server: every connected peer's link is |
| `auth_failed` | Forged or tampered datagrams, plain traffic on a sealed link. Steady growth means someone is sending them |
| `replayed` | Replays, or duplicates from the network or the conditioner. A few are normal on a lossy link |
| `handshakes_refused` | Key exchanges refused for load, and joins refused because a live sealed peer holds the address. Growth with no real joins looks like a flood |
| `pre_proof_shed` | Server: connect requests and unusable-cookie or conflicting-share refreshes dropped unanswered before proof because the source prefix's fair share of the answer budget, or the server-wide cap itself (`ServerConfig::pre_proof_answers_per_s`, default 4,096 per second), was spent (ADR-4936, ADR-8718). Growth with no real joins looks like a spoofed flood; real clients still get their prefix's share, and every answer whose cookie comes back is given back to its prefix |
| `rekeys` | Key switches on the live sealed links, both directions summed ([net-trust](net-trust.md#rekeying), ADR-2416) |

Unknown endpoint: `net_unknown_endpoint`. `net.peers` rows carry `sealed`. `session.state` carries `encryption`, `server_key` and, on a
client, `sealed`.

| End or error code | Where | Meaning |
|---|---|---|
| `encryption_required` | Server refusal | The client cannot or will not encrypt, and the server requires it |
| `encryption_unavailable` | Client end | The key exchange could not start, or the sealed link could not be opened. A plain challenge is not this end: it is unauthenticated, so it is counted in `auth_failed` and the attempt waits until `connect_timeout` |
| `crypto_version_mismatch` | Either | The two sides run different suite versions |
| `server_key_mismatch` | Client end | The server proved a key that is not the pinned one |
| `password_needs_encryption` | `session.host`, `session.connect` | A secret would cross a non-loopback link in the clear |
| `protocol_mismatch` | Server refusal | A different wire version, for example a build from before encryption |

**Resumption tickets (wire 6, ADR-8858).** A sealed accept carries a ticket for a one-round-trip resume
([net-link](net-link.md)). The ticket is sealed under a server ticket key (two live, rotated every resume grace, derived
from the server secret) and holds the link's resumption PSK; the client holds the same PSK, derived from the handshake and
never sent. A resume runs `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`: the PSK proves the client, a fresh `ee` gives the new
link forward secrecy, and the next PSK comes from the new link, so a leaked PSK does not give the next one. The first
flight carries the join and no game data, so a replayed flight delivers nothing. A resumed link reports the server key of
the link its credential came from; a client that pins another key does not use the credential. What the ticket does not
protect: anyone who reads a `ResumeInit` learns the ticket (it is opaque, and useless without the PSK) and that the
client is resuming.

## Check it yourself

```text
session.host { "mode": "dedicated", "port": 0 }
session.connect { "address": "<lan-host>", "server_key": "<key from session.state>" }
wait_until session.state state == in_game
expect net.crypto: endpoints[0].sealed == true, counters.auth_failed == 0
```

## Cost

A sealed packet is 25 bytes larger, so a default 1200-byte packet carries 2.1 % less. A join costs a few curve operations, once per
attempt: a resend gets the stored reply. For CPU cost per tick, compare `net_bench` with and without `--sealed`. One release run on a
loaded 32-client server, 900-byte state, 600 ticks, gave 39 to 43 µs plain and 195 to 236 µs sealed per tick, about 170 µs more.

## Address changes (ADR-8299)

A sealed peer whose address changes keeps its session once the new address answers a path probe; a record copied and raced from
another address never moves it, and a client follows a server whose address changes the same way. The rules, counters
(`rebinds`, `rebinds_refused`, `path_probes`, `path_mirrored`, `net.peers` `path_candidate`) and limits are in
[net trust](net-trust.md#nat-rebinding).
