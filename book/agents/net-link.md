# Net link (agents)

`regia_net::link::NetLink` runs an app's replication over a real datagram transport: raw UDP, the
in-memory network of tests, or Steam's relay through `regia_steam::SteamSocket` (ADR-2039). The
`net-game` template and `samples/net_game` use it; the tutorial is
[Build a networked game](../tutorials/networked-game.md).

The link publishes its endpoint report into `NetAgentState` every frame, so the existing methods see
it, and adds `net.resume` for its dormant players:

| Method | What the link contributes |
|---|---|
| `net.status` | One report under the label `server` or `client` (or `LinkOptions::label`): role, state (`listening`, `connecting`, `connected`, `reconnecting`, `disconnected:<code>`), local and remote address, tick rate, totals, link statistics, crypto status, and `reconnect` while a client reconnects by itself (below) |
| `net.peers` | Server: one row per connected peer with RTT, jitter, loss and `sealed` |
| `net.crypto` | Whether the link is sealed and the server's key |
| `net.conditioner_set` | Reaches the link by its label on the next frame |
| `net.resume` | Server: dormant players and resume counters (below) |

`NetLink::stop` drops the endpoint; the next frame removes its report.

Prediction (`net.prediction`), interpolation (`net.interpolation`), lag compensation (`lagcomp.*`)
and lobbies (`lobby.*`) work as on their own pages; the link feeds prediction the transport's RTT
and jitter each frame.

## Checking a running game

1. `net.status`: the client report's `state` is `connected` and `crypto.sealed` is `true`.
2. `net.peers` on the server: every client is listed with `sealed: true`.
3. `net.conditioner_set` with `{"endpoint": "client", "profile": "wan"}` to test a bad network, then
   `net.prediction` to watch corrections.

## Errors

`NetLink::host*` and `NetLink::connect*` return a `LinkError` whose `code()` is stable:

| Code | Meaning |
|---|---|
| `link_running` | The link already has an endpoint; call `NetLink::stop` first |
| `link_bind_failed` | The UDP port could not be bound (in use, no permission, bad address) |
| `link_join_too_large` | The client name does not fit the handshake |
| `link_password_plain` | A password would cross a plain link (`CryptoMode::Off`) to a non-loopback address; use `auto` or `required` |
| `net_known_hosts_unreadable`, `net_known_hosts_newer`, `net_bad_host_id` | `LinkOptions::known_hosts` is set and the store is damaged or newer than this build reads, or the host id is not valid ([net-trust](net-trust.md)) |

A client whose connection ends reports `LinkEvent::Ended { reason }` with the transport's code:
`connect_timeout`, `server_key_mismatch`, `encryption_unavailable`, `closed`, or a disconnect reason
such as `timeout` or `kicked`. A server reports `LinkEvent::Joined` and `LinkEvent::Left { reason }`, and
`LinkEvent::Dormant { peer }` / `LinkEvent::Resumed { peer, name }` for a resume (ADR-8298).

## Resume (`net.resume`)

A player whose connection times out is dormant for `LinkOptions::resume_grace_ms` (60 s) and comes back as the same
peer with `NetLink::resume` (spec 120 § 10.3, ADR-8298). `net.resume` (read, no params) reports the server's view:

```json
{ "hook": true, "dormant": [{ "peer": 2, "since_tick": 1800 }], "went_dormant": 1, "resumed": 0, "takeovers": 0,
  "expired": 0, "hook_calls": 0, "kept": 0, "despawned_entities": 0, "messages_kept": 0, "messages_expired": 0,
  "messages_overflowed": 0, "last_resume": null }
```

`takeovers` counts resumes that replaced a connection the server still held; `expired` counts grace ends; `hook` is
whether the game set an `on_grace_expired` hook (`NetGraceHook`), and `hook_calls`, `kept` and `despawned_entities` say
what it did. On a client, or before any player went dormant, every count is 0. The transport's own counters are in
`net.status` totals: `dormant`, `resumes`, `resume_takeovers`, `resumes_refused` (a ticket that matched nothing: spent,
expired or forged; the join went on as a new peer), `resume_retries` (resumes that presented the ticket of an earlier
resume whose accept the client never confirmed: a retry after a lost accept, the same peer again, ADR-8535) and
`dormant_expired`. A ticket is spent once the client's first packet on the resumed connection arrives; until then the
client may still hold it, or any older ticket of the same unconfirmed chain (at most 8, each for one grace window,
ADR-8623), and a join with any of them is the same peer again. `held_slot_returns` counts joins whose ticket named
the per-address slot held for a player that left with a goodbye (ADR-8733): a new peer, which the per-address cap
(`max_peers_per_ip`) did not count again, counted when its client confirms the accept (ADR-8883); such a ticket is not counted in `resumes_refused`. The method has no error
codes.

To check a resume: cut the client's network, wait past the 20 s timeout, and `net.resume` lists the peer under
`dormant`; restore it and call `NetLink::resume` on the client; `net.resume` shows `resumed: 1`, `dormant: []` and
`last_resume.peer` equal to the old id.

## Version and content refusals

A client of another release, another replicated schema or other content (`LinkOptions::content_hash`) is refused with
`LinkEvent::Ended { reason: "protocol_mismatch" }`, followed by `LinkEvent::VersionMismatch { side, server_is_older, message }`
(spec 120 § 12.2, ADR-7985). `net.status` shows the last refusal on both ends as `version_mismatch`:

```json
{"version_mismatch": {"side": "client_older", "server_is_older": false,
  "client": {"wire": 3, "game_version": "1.2.0", "protocol": 1, "content_hash": 81985529216486895},
  "server": {"wire": 3, "game_version": "1.3.0", "protocol": 2, "content_hash": 81985529216486895},
  "message": "the server runs 1.3.0, you have 1.2.0: update the game (client_older)"}}
```

| `side` | Meaning | `server_is_older` |
|---|---|---|
| `client_older` | This client is the older release: update the game | `false` |
| `server_older` | The server is older than this build (or predates the version probe: `server` is `null`) | `true` |
| `content_differs` | Same release, other schema or content: install the server's content | `null` |
| `unordered` | The versions do not say (another game, version text that is not dotted numbers) | `null` |

To check it: connect a client of the old build, step a few frames (`play.step`), then `net.status` on the client; on the
server the same field names the last client it refused. Against a server that predates stamps the event arrives about
1.5 s after the `Ended` (the version probe's wait).

## Automatic reconnect (`net.status` `reconnect`)

A client with a resume ticket does not end at the timeout: it reconnects by itself (spec 120 § 10.3, ADR-8352), 1 s, 2 s,
4 s and then every 8 s apart (minus up to an eighth of jitter) for `LinkOptions::reconnect.window_ms` (60 s), over the
transport it connected with. While it runs, the client's `net.status` report says:

```json
{ "state": "reconnecting", "reconnect": { "attempt": 3, "since_ms": 7200, "next_in_ms": 1100, "window_left_ms": 52800 } }
```

`reconnect` is absent otherwise. The game sees `LinkEvent::PeerInterrupted` once, `LinkEvent::Reconnecting { attempt }`
per attempt, then one `LinkEvent::PeerRestored` (the same `PeerId`), or `LinkEvent::ReconnectFailed { attempts, reason }`
followed by `LinkEvent::Ended { reason }` (`reason` is `timeout` when the window ended, else the server's refusal code).
No new method and no new error code.

To check it: cut the client's network past the 20 s timeout; `net.status` on the client shows `state: "reconnecting"` and
a growing `attempt`, `net.resume` on the server lists the peer under `dormant`; restore the network: within about a second
the client's `state` is `connected` with the old `peer_id`, `reconnect` is gone, and `net.resume` shows `resumed: 1`. In
`samples/net_game` the HUD node `link_banner` shows the same through `ui.*` methods.

## Clock

`LinkClock::Wall` reads the monotonic clock. `LinkClock::Manual { start_ms, step_us }` advances by
`step_us` after every app update: with `step_us = 16_667` and a 60 Hz fixed step, several apps in
one test run in lockstep with no sleeps, and over the in-memory network a match replays bit for bit.

## One-round-trip resume: the resumption ticket (wire 6, ADR-8858)

A sealed link's accept also carries a **resumption ticket** when the server keeps dormant peers (`resume_grace_ms`
above 0). Only the server that sealed it can read it. The client keeps it with the link's resumption PSK (a secret
both ends derive from the handshake; it is never sent) as a `ResumeCredential`: `NetClient::resumption()` after the
accept, `ClientConfig::resumption` on the next connect, together with `ClientConfig::resume` (the 16-byte ticket of
the same accept). A plain link has no PSK and gets no credential.

A connect with a credential opens with `ResumeInit` instead of `ConnectRequest`: one 1,200-byte datagram with the
ticket, the proof that the client holds the PSK and a fresh key share. It carries the join (name, role, build, join
data) and no game data. A server that resumes answers `ResumeReply`, and the client is connected after that one
round trip; the server admits the peer when the client's first sealed record arrives (ADR-8902). A dormant player
or a player whose old connection the server still holds comes back as the same peer; a player that left on purpose
comes back from its own address block as a new peer in the slot its address held. From another address block the server
first asks for a cookie (`ResumeRetry`): two round trips. A flight the server cannot resume is refused to the full
handshake, and the client joins in the same three round trips as before through the 16-byte ticket.

Every reconnect path presents the credential (part 3 of 3, ADR-10088). A `NetLink` client keeps the credential of its
last accept beside the ticket; `resume()`, `resume_on(socket)` and the automatic reconnect (own socket, or fresh sockets
of the link's transport: UDP, relay, Steam) present both, each attempt of one outage with a newer flight. A new accept
replaces ticket and credential together. A `NetSession` client does the same on every `resume: true` connect
([net-session](net-session.md)). Without a credential (a plain link, a server with `resume_grace_ms` 0) the reconnect is
the full handshake with the ticket. `cargo test -p regia_net --test link_resume_one_rtt` and
`--test session_resume_one_rtt` run every path on the in-memory network.

Until the client's first sealed record arrives nothing has changed on the server: a copy of the first datagram, sent
again by anyone from anywhere, gets the stored answer (or a cookie request, or nothing) and moves no peer, address,
held slot or ticket. A ticket resumes once: after the confirmation the same credential is refused with `resume_epoch`
(or `resume_no_peer` for a spent held slot).

`net.status` totals:

| Field | Meaning |
|---|---|
| `resume_inits` | Client: connects that opened with a `ResumeInit`. Server: `ResumeInit` datagrams read |
| `resume_fallbacks` | Client: resume flights that ended in the full handshake. Server: `ResumeInit` datagrams refused to the full handshake |
| `resumed_one_rtt` | Server: resumes admitted at their key confirmation (each also counted in `resumes` or `held_slot_returns`). Client: connects that finished on a `ResumeReply` |
| `resume_replays` | Server: byte-identical copies of a pending flight that got its stored `ResumeReply` again (no key work, no handshake charge) |
| `resume_lapsed` | Server: answered flights that left unconfirmed (no record within 10 s, the ticket expired, or a newer flight of the same ticket took their place) |

`net.status` also carries `resume_refusals` on a server: a map from the refusal code below to how often it happened,
for flights that were not answered with a `ResumeReply` and for resumes refused at their confirmation. It is absent
while empty.

```json
{ "role": "server", "totals": { "resume_inits": 4, "resume_fallbacks": 1, "resumed_one_rtt": 1, "resume_replays": 0, "resume_lapsed": 0 },
  "resume_refusals": { "resume_epoch": 1, "resume_other_block": 1, "resume_psk": 1 } }
```

A server names why it refused a resume (`NetServer::last_resume_refusal`, and a debug log line `resume refused`).
The codes are not method errors: the client only sees a challenge and carries on.

| Code | Meaning |
|---|---|
| `resume_unavailable` | This server does not admit this resume: `resume_grace_ms` is 0, it does not encrypt, it is closing, the source is banned, or it asks for a password the ticket does not vouch for |
| `resume_protocol` | The flight or its ticket is another build's: the answer is the `protocol_mismatch` deny |
| `resume_malformed` | The ticket or the sealed join has the wrong size, version or shape |
| `resume_key_retired` | The ticket was sealed under a key the server no longer keeps (older than one grace) |
| `resume_key_unknown` | The ticket names a key this server never had: another server's ticket |
| `resume_ticket_auth` | The ticket did not open: another server's, or a changed byte |
| `resume_expired` | The ticket's lifetime (one resume grace) is over |
| `resume_epoch` | The ticket was spent: the client confirmed a newer accept since |
| `resume_other_block` | The source is outside the ticket's address block: the server asks for a cookie (`ResumeRetry`) |
| `resume_psk` | The flight does not prove the ticket's PSK: no answer |
| `resume_no_peer` | The ticket names no dormant or live peer and no held slot: a fresh join |
| `resume_busy` | The server's resume register is full, or the flight is over the handshake budget of its address or of the server |
| `resume_stale_flight` | The flight is older than one the server already saw with this ticket and is not pending: a replay. No answer |

At most two flights of one ticket wait for their confirmation; a third, newer flight takes the place of the older of
the two, which is then refused (`resume_stale_flight` if its datagram is played again, and its client is not admitted).

To check it: join a server with `resume_grace_ms` above 0 over a sealed link, keep `resume_ticket()` and
`resumption()`, connect again with both. The client is connected after one datagram with its old peer id and its totals
show `resume_inits: 1`, `resumed_one_rtt: 1`, `resume_fallbacks: 0`; the server's `net.status` shows
`totals.resumed_one_rtt: 1`, `totals.resumes: 1` and no `resume_refusals`. Connect once more with the first credential:
the server's `resume_refusals` shows `resume_epoch: 1` and the client joins through the full handshake.
`cargo test -p regia_net --test resume_server` runs the admission, the replays and the refusals;
`cargo test -p regia_net_soak --test resume_one_rtt` runs the resume over the `asym-bufferbloat` path in virtual time;
`cargo test -p regia_net --test resume_ticket` runs the client's side against a scripted server.
