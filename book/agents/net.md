# Networking (agents)

Methods for the `regia_net` servers and clients a game runs (ADR-1600). A game publishes each endpoint into the
`regia_net::NetAgentState` resource every frame (`NetServer::publish(label, ..)`, `NetClient::publish(label, ..)`);
these methods read that resource and queue conditioner changes into it. The agent never sees the game's own message
types.

| Method | Params | Result |
|---|---|---|
| `net.status` | — | `{ bridge, endpoints: [{ label, role, state, local_addr, remote_addr, peer_id, tick, tick_rate_hz, peer_count, max_peers, conditioner, conditioner_stats, totals, link, crypto, version_mismatch?, resume_refusals?, quality }], rejections?: { total, dropped, recent: [{ peer, kind, detail, tick }] } }` (`rejections` on a server only: what the replication runtime refused and from whom, spec 120 § 11.2) |
| `net.peers` | `endpoint?` | `{ endpoints: [{ label, role, peers: [{ id, name, role, addr, rtt_ms, jitter_ms, loss_fraction, silent_ms, flagged, missed_input_ticks, grade, interrupted, quality, link, sealed, rebinds, rebinds_refused, path_probes, path_mirrored, path_candidate }] }] }` |
| `net.quality` | `endpoint?`, `peer?` | `{ limits, endpoints: [{ label, role, grade, interrupted, quality, peers: [{ id, name, grade, interrupted, quality }] }] }`, see [Net quality](net-quality.md) |
| `net.conditioner_set` | `endpoint?`, `profile?` or `spec?`, `config?` | `{ queued_for, conditioner, conditions }` |

- `bridge` is `false` (and `endpoints` empty) when the app has no `NetAgentState`.
- `NetAgentState` is an app resource only with `regia_net`'s default `ecs` feature (ADR-8032). A game built with
  `default-features = false` (an ECS-free headless server) has no app for the agent to attach to; its endpoints still
  answer `NetServer::status()` / `peer_rows()` and `NetClient::status()` in code, the same data these methods report.
- `version_mismatch` appears after a `protocol_mismatch` refusal (spec 120 § 12.2, ADR-7985): `side` (`client_older`,
  `server_older`, `content_differs`, `unordered`), `server_is_older`, both versions (`client`, `server`: `wire`,
  `game_version`, `protocol`, `content_hash`; `server` is `null` for a server that predates the version probe) and the
  player-facing `message`. A client shows its own refusal, a server the last client it refused; see
  [Net link (agents)](net-link.md#version-and-content-refusals).
- With the net runtime (`NetPlugin`), `net.status` also answers `mode`, `local_peer`, `tick`, `peers` and `players`;
  see [Net model (agents)](net-model.md). Replication methods are on [Net replication (agents)](net-replication.md).
- `state` is `listening` or `closed` for a server and `connecting`, `connected` or `disconnected:<reason>` for a
  client. Reasons: `left`, `timeout`, `kicked`, `banned`, `server_shutdown`, `protocol_error`, `invalid_message`,
  `malformed_packet`, `reliable_overflow`, `flood`, `server_full`, `too_many_local_players`, `connect_timeout`,
  `closed`, `encryption_unavailable`, `server_key_mismatch`, or a deny reason (`full`, `banned`, `bad_password`,
  `protocol_mismatch`, `build_mismatch`, `invalid_join`, `shutting_down`, `encryption_required`,
  `crypto_version_mismatch`), or `ticket_refused` when the store-ticket hook refuses. That refusal is
  still `DenyReason::InvalidJoin` on the wire; `ClientEnd::as_str` and this `state` report
  `ticket_refused`. See [Join helpers](net-join-helpers.md).
- `tick` is the server's next tick, or the client's estimate of it.
- `totals` counts datagrams in and out, foreign and malformed traffic, accepted and denied joins, socket errors and
  reliable sends refused by the rate cap; `input_bundles_trimmed` and `reliable_inputs` count large client inputs
  (see [Net input (agents)](net-input.md)); on a server `over_rate` counts datagrams and messages dropped by a peer's
  inbound rate and `flood_disconnects` the peers removed with `flood`, and each `net.peers` row carries `inbound`
  (`over_rate_drops`, `flood_ms`, `malformed_points`; see [Net model (agents)](net-model.md#floods-and-fairness-adr-8181)). `link` (clients) and each peer's `link` hold the channel counters:
  packets sent, received, acked and lost, bytes, reliable resends, dropped unreliable messages, duplicates, `rtt_ms`,
  `jitter_ms`, `loss_fraction`, `send_kbps`, `recv_kbps`, `reliable_in_flight`, `recovering`, `recoveries` and
  `transport_stalls`; reassembly (spec 120 § 5.2, ADR-7981): `reliable_buffered_bytes` (reliable fragments stored
  ahead of a gap now), `reliable_assembling_bytes` (the messages being put together in order), `reassembly_refused`
  (packets refused whole because the server-wide budget was full; the sender resends them) and `failure` (`null`, or
  why the link failed for good: `message_too_long`, `inbox_full`, `fragment_too_long`, `reassembly_over_cap`,
  `reassembly_stalled`, `stream_over_cap`, `unknown_stream`); `reassembly_stall_ms` (the peer's traffic counted against
  a gap now, the longest over the streams, 0 with none) and `reassembly_stall_deadline_ms` (the deadline in force:
  `max(4 x RTO, 2 s)`, stretched up to 4x by the peer's measured packet loss, ADR-8521; before the link's first
  round-trip sample the RTO in it is at least the wait of this end's oldest unanswered packet, up to 1 s, so the
  deadline can read up to 4 s there, ADR-8658, reported since ADR-8859). A link whose
  `reassembly_stall_ms` keeps climbing toward the deadline has a peer that is not repairing a gap. See
  [Net model (agents)](net-model.md) for what the server does then. Reliable streams (spec 120 § 5.1, ADR-8302):
  `delivered_past_gap` (reliable messages delivered while another stream, or an earlier message of the same unordered
  stream, waited on a lost fragment: deliveries one ordered stream would have held back), `stream_blocked` (polls where
  a stream's flow-control credit held new data), `background_held` (polls where bulk data waited for the path's
  bandwidth-delay product) and `credits_sent`.
  Socket drops (spec 120 § 4.3, ADR-8407): `socket_drops` in each `link` (packets the socket refused, such as a full
  send buffer; they spent no packet number and are not in `packets_sent`) and in `totals` (every datagram the socket
  refused, handshakes included; not in `datagrams_out`).
- `socket` (endpoints on a real UDP socket; absent on in-memory, relay and Steam transports) is what the operating
  system gave the socket (spec 120 § 4.4, ADR-8407): `recv_buffer_requested` / `recv_buffer_bytes` (2 MiB asked;
  Linux reports twice what it stores), `send_buffer_requested` / `send_buffer_bytes` (512 KiB asked), `dscp` (46 when
  the Expedited Forwarding mark was taken, `null` when refused) and `undersized` (either buffer under half of what was
  asked: the host's limits are low and bursts may drop; the process also logs one warning).
- `streams` (clients, and each row of `net.peers`) lists the link's reliable streams in id order: `stream` (0
  control, 1 inputs, 2 game RPC, 3 replication, 4 lockstep, 5 chat, 6 bulk, 7 unordered RPC, 8-31 the game's),
  `spec` (`ordered`, `priority`, `window_bytes`), `queued_fragments` (sent and not yet acknowledged, the admission
  count), `send_credit_bytes` (what the peer's credit still allows), `buffered_bytes` (received ahead of a gap, at
  most `window_bytes`), `assembling_bytes` and `stall_ms` (peer traffic this stream has waited on a missing
  fragment; the link fails at `reassembly_stall_deadline_ms`, ADR-8572). A stream whose `stall_ms` grows while the
  others' stay 0 is the one a lost fragment holds.
  Each row also names its `class` (`control`, `inputs`, `game_rpc`, `replication`, `lockstep`, `chat`, `bulk`,
  `unordered_rpc`, or `game` for ids 8-31) and carries the class's message cap (spec 120 § 4.5 A2, ADR-8407):
  `spec.msgs_per_s` (240 by default, chat 30, bulk 0 = none), `spec.weight` (its round-robin share within its
  priority), `msgs_last_second` (messages started in the last second, at most the cap) and `msgs_capped` (polls in
  which the cap held a new message back). A growing `msgs_capped` names the class a game is flooding.
- `crypto` (and each peer's `sealed`) is the transport encryption status: see [Net transport encryption](net-crypto.md)
  and `net.crypto`.
- A lockstep match (`regia_net::lockstep`) has its own method, `net.lockstep`: the input delay, frame lag and input
  timing per seat, desync verdicts and resyncs. See [Net lockstep (agents)](net-lockstep.md).
- `flagged` means loss or round-trip time is above the server's health policy. Flagged peers are never removed for
  it; only silence of at least the timeout removes a peer.

## Status

```json
{}
→ { "bridge": true, "endpoints": [{ "label": "server", "role": "server", "state": "listening",
    "local_addr": "0.0.0.0:7777", "tick": 912, "tick_rate_hz": 30, "peer_count": 2, "max_peers": 16,
    "conditioner": { "enabled": false, ... }, "conditioner_stats": { "passed": 5210, ... }, "totals": { ... } }] }
```

## Peers

```json
{ "endpoint": "server" }
→ { "endpoints": [{ "label": "server", "role": "server", "peers": [{ "id": 1, "name": "ada", "role": "player",
    "addr": "<lan-host>", "rtt_ms": 41.5, "jitter_ms": 3.2, "loss_fraction": 0.0, "silent_ms": 12,
    "flagged": false, "missed_input_ticks": 4, "link": { ... } }] }] }
```

## Conditioner

`net.conditioner_set` changes the outbound network conditioner of one endpoint (`endpoint`) or of every endpoint.
`profile` picks a starting point: `off`, `lan`, `home` (30 ms, 0.5 % loss), `wan` (60 ms plus up to 10 ms, 2 % loss,
1 % duplicates, 5 % reorders), `wan_stress` (80 ms plus up to 20 ms, 8 % loss, 2 % duplicates, 15 % reorders),
`bad` (120 ms plus up to 60 ms, 5 % loss) or `burst` (`wan` plus a two-second total-loss burst ten seconds in).
Jitter is added on top of the delay. Without a profile the start is an enabled
pass-through. `config` then overrides fields: `enabled`, `seed`, `loss`, `duplicate`, `reorder`, `reorder_max_ms`,
`delay_ms`, `jitter_ms`, `burst_start_ms`, `burst_len_ms`, `burst_loss`. Probabilities are clamped to 0..1.
`spec`, instead of `profile`, states the conditions in one line, the grammar of the template's `--conditioner`
(`delay=40,jitter=10,loss=2%`, or a profile with overrides; see [Net match across machines](net-match.md)); the
result's `conditions` prints them back.

The endpoint applies the change on its next publish, so step the game (`play.step`) and read `net.status` to see
`conditioner` and `conditioner_stats` move. The conditioner is seeded: the same seed and the same traffic make the
same decisions.

```json
{ "endpoint": "client", "profile": "wan", "config": { "loss": 0.1, "seed": 7 } }
→ { "queued_for": ["client"], "conditioner": { "enabled": true, "loss": 0.1, "delay_ms": 60, ... } }
```

## Errors

| Code | When |
|---|---|
| `net_no_endpoint` | `net.conditioner_set` with no published endpoint (no `NetAgentState`, or nothing published yet). |
| `net_unknown_endpoint` | `endpoint` names no published endpoint (`net.peers`, `net.conditioner_set`). |
| `net_unknown_profile` | `profile` is not one of the names above. |
| `net_bad_conditioner` | `config` is not an object, names an unknown field, or a value has the wrong type; `spec` does not parse, or `spec` and `profile` are both given. |
| `ticket_refused` | The server's `JoinTicketCheck` refused the store ticket. Client `state` is `disconnected:ticket_refused`. The host password is still checked first (`bad_password`). |

See the [networking user guide](../user-guide/networking.md) for the crate itself.

### Interruption and recovery (ADR-7950)

Every `net.status.endpoints[]` row includes `interrupted`: on a client it describes the server;
on a server it means at least one peer is interrupted. Every `net.peers.endpoints[].peers[]`
row includes its own `interrupted`. A peer stays in the list during interruption and retains
its id. The diagnostics overlay labels these rows `INTERRUPTED`. `link.recovering` and
`link.recoveries` report bounded reliable backlog recovery. Defaults: interruption 1500 ms,
silence removal 20000 ms, startup grace 30000 ms. These are observations, not new methods or
error codes. Use the existing `net.conditioner_set` burst window to inject an outage, advance
the host's manual clock, and assert one transition to true and back to false with the peer id
unchanged. A 25000 ms outage requires a policy longer than that on both ends.

A one-way outage raises one transition per end, not one per probe (ADR-7995): the server keeps
its keepalive floor, and either end clears `interrupted` only when a packet from the peer also
acknowledges something new. To check it, drop one direction only and count the transitions.


## Handshake availability (ADR-7946)

For a port change between Challenge and KeyInit, step the endpoints on their
virtual clock and read `net.status`: the client reaches `connected` before its
connect timeout. The password proof follows the same IP-bound cookie rule.
A later challenge for the same nonce replaces an unproven one, but cannot extend
the timeout or replace keys after KeyReply. Ordinary duplicates retain the key share.
If a replaced challenge conflicts with an already-pending server share, a repeated
refresh triggers a bounded fresh-nonce restart without extending the deadline.
The client keeps the first key exchange whose KeyInit left and finishes on whichever
share the server answers, so a second, equally valid challenge (the cookie's time
bucket turned over between two requests on a slow path) costs no restart (ADR-7995):
`totals.handshakes` stays 1 for such a join.
An invalid-cookie KeyInit or plain join response gets a fresh challenge under the
existing pre-proof answer cap; `totals.pre_proof_shed` counts refreshes shed by it,
and `totals.handshakes` does not increase until a valid proof reaches curve work.

`cargo test -p regia_net --test crypto handshake_` runs the deterministic port
change, challenge race, lost-KeyReply duplicate, timeout and budget regressions.
No new method or error code is introduced. QUIC payload and receive-edge counters
are described by [net.quic_status](net-quic.md#path-and-receive-edges-adr-7946).

## Ack/poll diagnostics (ADR-7949)

`net.status` adds `endpoints[].totals.inputs_too_far_ahead`; `net.peers` adds
`endpoints[].peers[].inputs_too_far_ahead`. Both count input samples refused at or beyond the
server's future input buffer, including reliable inputs. Redundant samples are counted when
rejected; these are sample counts, not unique tick counts. Old inputs do not increment them.

For a game publishing endpoints, call `net.status`, step its simulation with `play.step`, then
call `net.peers` for the server. A zero counter with ongoing traffic confirms that the game's
input stamps remain within the horizon. Late acknowledgments of lost or retransmitted packets
leave the RTT estimate unchanged. `link.unreliable_dropped` counts expired queued messages;
unreliable queue admission can return `SendError::Backpressure` to a host. Tick sync and state
acks get priority over reliable backlog. The flight horizon of ADR-7949 is gone (ADR-7995).

Configuration parsing reports `replication_mtu_too_small` for a state MTU below 208. This is a
settings diagnostic, not a new agent RPC error. State frames are bounded by the sealed transport
capacity and are never silently promoted to the ordered channel by the bridge.

## Loss, pacing and recovery (ADR-7995)

One policy now governs the send loop of every connection; the
[networking guide](../user-guide/networking.md) describes it. For an agent it shows in the
`link` object of `net.status` (clients) and of every `net.peers` row:

- `packets_lost` and `loss_fraction` follow the loss the link really had. Reordering and a
  slow path are not loss: with `net.conditioner_set` on the `wan` profile the count matches
  the conditioner's `dropped` counter, and on a path with a 3-second round trip it stays 0.
- `reliable_resends` counts repeats after a loss verdict or a timer. On a clean path it stays 0
  after the first round trip.
- `recovering` is true and `recoveries` goes up only for an outage: a fragment timed out twice
  in a row on a measured path, or packets sent that long apart were lost. A 300 ms stall of the
  peer leaves both unchanged.
- `transport_stalls` counts polls that could send nothing because the transport's datagram
  budget was below 256 bytes. A peer that times out with this counter rising has a transport
  problem, not a network one.

To check a change, set a conditioner profile, step the host with `play.step`, and read the
counters; `cargo test -p regia_net --test transport_unify --test transport_unify_endpoints
--test transport_soak -- --nocapture` prints the same numbers for the built-in profiles. No
method or error code was added.

## Transport v2 counters (wire 5, ADR-8184)

Wire 5 numbers every packet once and acknowledges with frames of up to 32 ranges (see the
[networking guide](../user-guide/networking.md)). The `link` object of `net.status` and of each
`net.peers` row gains:

- `min_rtt_ms`: the smallest round trip of the last 10 s (0 before the first sample).
- `spurious_losses`: packets declared lost that were acknowledged after all. Reordering shows up
  here, not in `packets_lost`.
- `probe_timeouts`: probe timeouts that expired. During an outage it stays low (keepalives
  probe the path); `reliable_resends` stays 0 until the path answers.
- `ack_only_sent`: packets that only carried an acknowledgment.
- `redundant_copies`: small reliable messages (up to 256 bytes) sent once more inside a packet
  that left anyway, so one lost packet does not delay them by a round trip. Repairs are copied
  too; after a loss the copy may leave in a packet of its own (ADR-8400).
- `ack_frame_bytes`: bytes of acknowledgment frames sent (a part of `bytes_sent`). Frames name
  only what the peer has not yet confirmed hearing about (ADR-8423), so on a steady link this
  grows by about 6 bytes per packet; divide by `packets_sent` to read the cost per datagram.

A wire-4 peer that tries to join ends with `protocol_mismatch` (an existing code). To check the
transport, set a conditioner profile with `net.conditioner_set`, step with `play.step` and read
these counters; `cargo test -p regia_net --test transport_v2 -- --nocapture` prints the
acceptance numbers. No method or error code was added.

## Path MTU (ADR-8300)

With `mtu_probing` on (server.toml `[net] mtu_probing = true`, or `LinkOptions::mtu_probing`), the
`link` object of `net.status` and of each `net.peers` row reports path MTU discovery (spec 120
§ 4.3, [networking guide](../user-guide/networking.md)):

- `path_mtu`: the path MTU, headers and encryption included: the datagram size the search has
  confirmed. 1,200 with probing off; up to 1,452 with it on; 1,200 again after a black hole.
  After a lost full-size packet data goes at 1,200 for about a round trip while a probe confirms
  the size; `path_mtu` does not move for that (ADR-10097).
- `mtu_search`: `disabled` (probing off), `base` (waiting for the first round trip), `searching`,
  `search_complete`, or `error` (a black hole dropped the size; the search restarts after 60 s).
- `mtu_probes_sent`, `mtu_probes_lost`: padded probe packets (1,280, 1,400, 1,452 bytes) sent and
  lost. Three lost probes of one size end the search below it.
- `mtu_black_holes`: times the path stopped carrying the size in use.

To check a path: read `net.peers` after a few seconds of play; `path_mtu` 1452 with
`search_complete` means the path carries full-size packets. `mtu_search: error` with
`mtu_black_holes` above 0 means it stopped carrying them (a VPN, a tunnel, a route change); the
connection runs at 1,200 meanwhile and loses nothing. No method or error code was added; a
`server.toml` with a non-boolean `mtu_probing` is refused at load as `net.mtu_probing: true or
false`.

## Rate control counters (ADR-8301)

The transport paces every peer at a rate its controller sets from acknowledgments (spec 120
§ 4.2; see the [networking guide](../user-guide/networking.md)). The `link` object of
`net.status` and of each `net.peers` row gains:

- `rate_target_bps`: the rate the pacer spreads this peer's traffic at, bytes of packet body per
  second (the controller's target, or the shared uplink's fair share when that is lower). On the
  wire each packet also costs its endpoint header and 28 bytes of IP and UDP headers; a shared
  uplink counts those too, and a peer's share of it is converted to body bytes before it caps
  the peer (ADR-10097).
- `send_allowance_bps`: what the game's own traffic may use after inputs, tick sync and
  acknowledgments; replication sizes its frames to it.
- `queue_delay_ms`: queueing delay over the minimum round trip in the latest round.
- `rate_state`: `Startup`, `Increase`, `Hold` or `Decrease`.
- `rate_cuts`: rate cuts so far; `paced_holds`: polls in which the pacer held data back.
- `rate_breaker`: the path could not carry the essential floor at over 50 % loss for 10 s; the
  server reports the peer interrupted.

To check it, set a conditioner profile with a bandwidth cap through `net.conditioner_set`, step
with `play.step` and read `net.peers`: `rate_target_bps` settles under the cap and
`queue_delay_ms` stays low. `cargo test -p regia_net_soak --test rate_control -- --nocapture`
prints the acceptance numbers. No method or error code was added.

Once a cut has found a queue at the path's bottleneck, `rate_target_bps` settles at about 97 %
of the rate the link delivered then, not at the cap itself, and stays there for 15 s before it
tests the link again (ADR-8490). On a lossy, jittery link with no bandwidth cap it does not
sit at the 4096 B/s minimum.

`rate_target_bps` never reads under what the essential class sends (inputs, tick sync, lockstep
frames, up to the floor) plus 4 KiB/s, so `send_allowance_bps` keeps at least that much for the
reliable stream even on a link the controller has cut hard. After a congestion cut the target
comes back to about 97 % of the rate the link delivered, not to the rate that overflowed it
(ADR-8534).

A fresh connection whose game sends less than its 256 KiB/s start rate reads a
`rate_target_bps` of about 1.5x what it sends; while `rate_state` is still `Startup` the target
doubles whenever the game fills it, so a burst of inputs or a transfer that starts later is not
held to that first reading (ADR-8592).

On a listen server the shared uplink caps `rate_target_bps` to each peer's fair share only after
it reads a queue at the host's own uplink, and it reads one only when at least four peers that
are receiving data show a `queue_delay_ms` over 25 ms in the same round (ADR-8733). A few clients
behind slow downlinks show high `queue_delay_ms` on their own rows while the other rows keep
their own controllers' targets; with one to three receiving clients the shared uplink never
cuts on queueing (a fixed `server_rate_bytes` still caps it). To check it, give two or three clients a `net.conditioner_set` bandwidth cap, step,
and read `net.peers`: the uncapped clients' `rate_target_bps` does not fall.

## Connection grade (ADR-8182)

Every `net.peers` row carries `grade` (`good`, `fair`, `poor`, `interrupted`, with hysteresis) and `quality` (the
signals behind it); a client's `net.status` endpoint carries `quality` for its link to the server. After a resume or a
client's own reconnect the grade is reported back from `interrupted` (ADR-8889).
`net.quality` answers the same per endpoint and peer, with the grade limits; [Net quality](net-quality.md) explains each
signal and how to tell an upload problem from a download problem.
