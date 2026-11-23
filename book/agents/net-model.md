# Net model (agents)

The network model of `regia_net` (spec 70, ADR-1643): what a process is, who may change an entity, who may send
input for it, how each process treats its copy, and the one-off **net messages** between processes. Guide topic
`net-model`.

The rule behind it: **gameplay written for the network runs unchanged offline.** An offline process is its own
server, so the same systems, messages and ownership checks run in single-player with no transport.

## Modes

| Mode | Server? | Local players? | Accepts connections? |
|---|---|---|---|
| `offline` | yes (its own) | yes | no |
| `dedicated_server` | yes | no | yes |
| `listen_server` | yes | yes | yes |
| `client` | no | yes | no |

Systems are gated with the run conditions `on_server()`, `on_client()`, `with_local_players()` and `offline()`;
code never inspects the transport. The mode is set before the first fixed step of a session and never changes
mid-tick. The mode resource is `regia_net::NetMode` (spec 70 calls the same four values the process kind; ADR-5647).

## Peers, players, ownership and roles

- `PeerId(0)` (`PeerId::SERVER`) is the server process (also the listen host). Clients get ids from 1 in the order
  their handshake completes. A player is `NetPlayerId { peer, local }` (`local` 0..=7 for split screen), carried by
  `NetPlayer` on the `Player` entity.
- **Authority**: the server creates and changes every replicated entity. A client writing a replicated component
  changes only its own copy until the next server state overwrites it.
- **Owner**: the peer that may send input and to-server messages for an entity. The owner is found by walking up:
  the entity's own `NetOwner`, else its `NetOwnerLink` target, else its `ChildOf` parent, repeated. No `NetOwner` on
  the chain means the server owns it; a cycle or a chain longer than 32 steps also resolves to the server and logs
  `net.owner_chain_broken` once per entity. Clients only learn one bit per entity: `NetOwnedLocally`.
- **Roles** (`NetRole`, on replicated entities): `authority` on the server and offline, `predicted` on the owning
  client when the server entity has `NetPredicted` (spec 73), `proxy` for every other client copy.

## Messages

A message type implements `NetMessage` (direction, channel, optional rate limit, `ALWAYS_ADMIT`, `ALIGN_TO_VIEW`,
`SKIP_PREDICTING_OWNER`, `validate`) plus the marker trait of its direction, and is registered with
`app.add_net_message::<M>()` on every process in the same order. `#[derive(NetMessage)]` writes all of it from
`#[net(..)]` attributes ([Net assets and derived messages](net-assets.md)). Handlers read `NetMessages<M>` (filled at
`NetSet::Apply`, cleared at the next one); senders use the `NetSender` system parameter.

Where a message runs:

| Direction | Sent by | Target owner | Runs on |
|---|---|---|---|
| ToServer | client | that client | server |
| ToServer | client | another peer, or the server | nowhere: dropped by the server, counted `not_owner` |
| ToServer | server (offline, listen host) | any | server, at `NetSet::Apply` of the next tick |
| ToOwner | server | a client peer | that client only |
| ToOwner | server | the server (unowned, or a host player) | server, at `NetSet::Apply` of the next tick |
| ToOwner | client | any | nowhere: `send_to_owner` returns `Err(NotServer)` |
| Broadcast | server | any | server and every client that holds the target |
| Broadcast | client | any | that client only (cosmetic, never sent) |

Guarantees: reliable messages arrive exactly once while the connection lives and `ReliableOrdered` ones run in send
order per connection; unreliable ones arrive at most once and `UnreliableSequenced` never runs an older message after
a newer one of the same type and target. A client applies one server tick as: spawns, replicated state, the tick's
messages, despawns, so a message always sees the state of its tick or newer (`late: true` when newer state was
already applied). A message whose target has not spawned waits up to 32 ticks (`unresolved_wait_ticks`), then is
dropped as `unresolved`.

Bounds: 4,096 reliable messages or 1 MiB queued per connection (more disconnects it with `reliable_overflow`); 16
unreliable messages per target per connection per tick (the oldest is dropped, `dropped_overflow`); 1,024
broadcasts per server tick (the rest are dropped as `dropped_overflow`, with one warning per second); 64 KiB per
encoded message (`TooLarge`).

More rules (spec 70 § 6.2-6.5, ADR-5647):

- **Session messages** have no target and use the peer directly: `send_session_to_server(msg)` from a client runs on
  the server, `send_session_to_peer(peer, msg)` names one peer, `broadcast_session(msg)` runs on every client in game
  and on the server.
- **Broadcasts are not stored.** A client where the target is not relevant at the send tick never sees that
  broadcast, even if the entity becomes relevant later. Persistent facts belong in replicated state.
- **`ALIGN_TO_VIEW`** types targeting a proxy wait on the client until its interpolation tick reaches the message's
  tick (at most `max_delay_ms` of `interpolation`, 250 ms by default), so an effect lines up with the shown pose; other
  types run on arrival.
- **Unknown targets on the server** are dropped at once and counted `unresolved`; only clients wait for a spawn.
- **Send errors** (`NetSendError`, returned by `NetSender`; nothing is sent): `NotReplicated` (the target has no
  `NetId`), `NotServer` (a server-only send on a client), `NotClient` (a client-only send on a dedicated server),
  `TooLarge`, `Unregistered` (no `add_net_message`), `WrongDirection` (the marker trait and `DIRECTION` disagree),
  `UnknownPeer` (`send_session_to_peer` to a peer that is not connected). `NotServer` and `NotClient` log once per
  message type.
- **Inbound queue** (ADR-1892): in front of the flood guard each peer's queue between two ticks holds 4,096 messages;
  a frame that does not fit is shed undecoded (`ALWAYS_ADMIT` types too) and counts as a breach, two seconds of
  overflow in a row disconnect the peer with `flood`, and `net.peer_flood` reports the count as `inbound_shed`.
- **Logs are capped**: rejected messages log at debug at most 10 lines per peer per second (every reject is still
  counted). Ownership is resolved once per target entity per message pass, never once per message.

The server's **flood guard** gives each peer a token bucket per message type (default 120 per 60 ticks), a cap of
512 messages per tick and 256 KiB per second. A peer that breaches becomes `slowed` (rates halved), is `clear`
again after 300 quiet ticks, and is disconnected with `flood` after 600 ticks of breaching while slowed. The frame
after a server stall longer than 250 ms is not counted. A `Verdict::Reject` from `validate` drops and counts the
message; `Verdict::Kick` disconnects the peer with `invalid_message`; undecodable bytes with `malformed_packet`.

Gameplay guidance: prefer replicated state over messages for anything that must be true later; send per-tick input
through the prediction command stream (spec 73), not through reliable messages; give every reliable message a button
can trigger a rate limit.

## Methods

| Method | Mode | Params | Result |
|---|---|---|---|
| `net.status` | read | — | adds `{ mode, local_peer, tick, peers, players: [{ peer, local }] }` to the endpoint list of [net.status](net.md) |
| `net.entity` | read | `{ entity }` | `{ net_id, role, owner (server), owned_locally, relevant_to (server) }` |
| `net.messages` | read | `{ type? }` | `{ types: [{ type, direction, channel, sent, received, delivered, rejected, rate_limited, not_owner, unresolved, dropped_overflow, late, bytes_sent, bytes_received, version, stable_id }] }` (`type` is the message id; `version` and `stable_id`: [derived messages](net-assets.md)) |
| `net.peer_flood` | read (server) | `{ peer }` | `{ state: clear\|slowed, breaches_last_second, last_breach_tick, inbound_shed }` (`inbound_shed`: messages dropped undecoded because the peer's inbound queue was full) |
| `net.set_owner` | play (server, dev builds) | `{ entity, peer \| null }` | `{ owner }` |

```json
{ "method": "net.entity", "params": { "entity": 4294967310 } }
→ { "net_id": 3, "role": "authority", "owner": 1, "owned_locally": false, "relevant_to": [1, 2] }

{ "method": "net.messages", "params": { "type": "Fire" } }
→ { "types": [{ "type": "game::Fire", "direction": "to_server", "channel": "reliable_ordered", "sent": 0,
    "received": 14, "delivered": 12, "rejected": 1, "rate_limited": 1, "not_owner": 0, "unresolved": 0,
    "dropped_overflow": 0, "late": 0 }] }
```

Verify a change with `play.step` (or a frame step) and read the counters again.

## Errors

The errors in this table are `F0002` with `data.reason`. A bad parameter (a missing `entity`, a
non-integer `peer`, a non-string `type`) is `F0002` with `data.hint` and no `data.reason`:

| Reason | When |
|---|---|
| `net_not_running` | The app has no net runtime (`NetPlugin` not added). |
| `net_not_server` | A server-only method (`net.peer_flood`, `net.set_owner`, `net.wake`, `net.set_asleep`, `net.resend`) on a client. |
| `entity_not_replicated` | The entity has no `NetId`. |
| `entity_not_found` | The entity does not exist. |
| `peer_unknown` | No connected peer has that id. |
| `message_type_unknown` | `net.messages` names a type that is not registered. |
| `net_dev_only` | `net.set_owner` in a shipping build (ADR-2415). |

Disconnect reasons the model adds to the session's list: `invalid_message`, `malformed_packet`,
`reliable_overflow`, `flood`, `server_full`, `too_many_local_players`.

## Settings

`config/net.ron` section `messages` (format: `docs/formats/net-ron.md`): `default_rate`, `peer_cap_per_tick`,
`peer_bytes_per_second`, `slowed_recover_ticks`, `disconnect_after_slowed_ticks`, `stall_forgive_ms`,
`unresolved_wait_ticks`.

## Hostile input and input readiness (ADR-7892)

Under the model, `NetServer` decodes every message a peer sends. What it cannot decode never reaches the game: the
host hears it as `ServerEvent::Rejected { peer, kind, error }` (spec 70 § 6.7). `kind` (`RejectKind::as_str`):

| Kind | What it was | What the server does |
|---|---|---|
| `input_too_long` | an input bundle with a field past its cap (`WireError::TooLong`: a command tail longer than the game's input codec allows, or more than 32 inputs) | disconnects that peer at once with `protocol_error`; `PeerLeft` follows; no other peer is touched |
| `input_malformed` | a truncated input bundle, a value the input codec refuses, or bytes after the bundle | one malformed point |
| `message_malformed` | a game message that does not decode, or leaves bytes over | one malformed point |
| `unknown_lane` | a message on a lane the server does not know | one malformed point |
| `reliable_too_long` | a reliable message that reassembled past `LinkConfig::max_reliable_message_bytes` | disconnects that peer at once with `protocol_error` |
| `inbox_full` | more reliable messages waiting than the connection holds | disconnects that peer at once with `protocol_error` |
| `fragment_too_long` | a reliable fragment longer than one packet of the link's `max_packet_bytes` carries (spec 120 § 5.2, ADR-7981) | disconnects that peer at once with `protocol_error` |
| `reassembly_over_cap` | out-of-order reliable fragments past the peer's cap (`Connection::reassembly_cap_bytes`: `LinkConfig::connection_buffer_bytes`, 2 MiB, never above one window of full fragments) | disconnects that peer at once with `protocol_error` |
| `reassembly_stalled` | a reliable message left unfinished, or a gap before stored fragments left unfilled, for `max(4 x RTO, 2 s)` of the peer's own traffic, stretched up to 4x by the peer's measured packet loss (an honest sender repairs a lost fragment about every two RTOs and may lose repairs in a row; ADR-8521) | disconnects that peer at once with `protocol_error` |
| `flood` | two seconds over the peer's inbound packet, byte or message rate, or more reliable messages in one receive than 4,096 (ADR-8181) | disconnects that peer with `flood` |
| `spectator_input` | an input (either input lane) from a spectator, refused undecoded (ADR-7984) | one malformed point |

No packet takes a point back (ADR-8181): the score leaks 4 points a second, and at 1024 points the peer leaves with
`malformed_packet`, so bad packets mixed with good or empty ones still add up. A
host that wants zero tolerance kicks on the event:

```rust,wrap
# use regia_net::{DatagramSocket, DisconnectReason, NetGame, NetServer, ServerEvent};
# fn handle_events<G: NetGame, S: DatagramSocket>(server: &mut NetServer<G, S>) {
while let Some(event) = server.poll_event() {
    match event {
        ServerEvent::Rejected { peer, kind, .. } if !kind.is_fatal() => {
            server.kick(peer, DisconnectReason::Kicked);
        }
        _ => { /* joins, leaves, messages */ }
    }
}
# }
```

For readiness on a set the game picks (its live players, while a player that leaves cleanly stays registered until
its removal tick), `server.driver().ready_for(tick, live.iter().copied())` and `server.driver().has_input(peer,
tick)` answer without taking or counting anything; `all_ready_for(tick)` still asks about every registered player.

The replication runtime refuses two more kinds itself (ADR-7984), named as `runtime::NetRejection { peer, kind, detail,
tick }` (`NetNode::take_rejections`) rather than as an endpoint event:

| Kind | What it was | What the runtime does |
|---|---|---|
| `server_only_message` | a `ToOwner` or `Broadcast` message type from a client (`detail` is the type's name) | disconnects that peer with `protocol_error` |
| `voice_malformed` | a voice packet that does not parse (it spent a token of the sender's voice bucket first), or a voice control message with an unknown kind or a block account over 256 bytes (`detail` is `voice control`) | one malformed point; at 1024 the peer leaves with `malformed_packet`; a tick in which the peer delivered a clean message and nothing was charged takes one back (a silent tick, or one with a voice packet over the bucket, takes nothing back) |

A system that decodes client bytes itself charges them the same way with `NetSender::reject_peer(peer, kind, detail)`.
On the client, a server message that does not decode on the reliable stream ends the link with `protocol_error` (the
stream is ordered; skipping one would desync the client), and one on the unreliable channel is dropped and counted.

No new agent method: `net.status` `totals.malformed` counts the endpoint's refusals, on a server `rejections { total,
dropped, recent }` lists the runtime's (newest 16 with the sender), and `net.peers` drops the peer when the server
does. Each peer's `link` in `net.peers` carries `reliable_buffered_bytes`, `reassembly_refused` and
`failure`, so a reassembly bomb is visible before the deadline drops it. Across peers the server stores at most
`ServerConfig::reassembly_budget_bytes()` (`server_buffer_bytes`, default `min(64 MiB, 32 KiB x max_peers)`) of
out-of-order fragments; a packet that would pass it is refused whole, as if lost, and the honest sender resends it.
Check the caps: `cargo test -p regia_net --test reassembly_caps` and `cargo test -p regia_net --lib bombers`. Check it yourself: `cargo test -p regia_net --test input_rejection` (real UDP loopback) and
`cargo test -p regia_net --test fuzz over_cap_input_tails`, `cargo test -p regia_net --test swallowed_decode` and
`cargo test -p regia_net --features voice --test swallowed_decode_runtime`.

## Floods and fairness (ADR-8181)

The server polices each connected peer before it decrypts anything: packet and byte buckets after the address and
session-token lookup, a message bucket per delivered message (`ServerConfig::inbound`, spec 120 § 11.3). Over them a
datagram is dropped unopened. A peer over rate for two seconds (net of recovery: a clean receive interval takes half
its length off) is removed with `flood`, after a `Rejected` event of kind `flood`. Per receive each peer gets at most
`max(256, 4096 / peers)` datagrams handled, and datagrams no peer owns share 4,096, so a flooder cannot push the other
players' packets back.

What an agent sees, without a new method:

| Where | Field | Meaning |
|---|---|---|
| `net.status` `totals` | `over_rate` | datagrams and unreliable messages dropped by a peer's rate or slice, or the unattributed work budget |
| `net.status` `totals` | `flood_disconnects` | peers removed with `flood` |
| `net.peers` row | `inbound.over_rate_drops` | that peer's drops since it joined |
| `net.peers` row | `inbound.flood_ms` | its flood timer; removal at `flood_disconnect_ms` (2,000) |
| `net.peers` row | `inbound.malformed_points` | its malformed score; removal at 1,024 |

A rising `flood_ms` on one peer while the others' `rtt_ms` and `missed_input_ticks` hold is the policy working. Check it:
`cargo test -p regia_net --test flood_fairness` (a 100k/s flooder next to seven good-wifi players, bad/good alternation,
soak F3 and F4 shapes, an honest 3x burst) and `cargo test -p regia_net --lib inbound`.

**Who can score a peer (ADR-8606).** Only traffic the session authenticated: a sealed record that opened under the peer's
keys, or a plain-link packet from its validated address. Each refusal scores its kind's weight
(`RejectKind::malformed_weight`): 16 points for an authenticated packet or message that fails to decode, an unknown lane or an
out-of-range field (`input_malformed`, `message_malformed`, `unknown_lane`, `voice_malformed`), 1 for `spectator_input`; loss,
reordering, duplication, replays and reconnects never score. At the leak of 4 points a second a peer sending 63 malformed
messages a second goes in about 1 s and 5 a second in about 13.5 s, while one every 4 s or slower never accumulates. A sealed
record from a peer's address that fails its open (a spoofer with the peer's token) gives back the rate tokens it took, so it
never moves that peer's `inbound.over_rate_drops`, `flood_ms` or `malformed_points`; it shows only in `net.status` `totals`
`auth_failed` (or `replayed`). A plain packet from an address the peer never validated is counted in `totals.malformed` and
raises no `Rejected` for the peer. So a peer whose `malformed_points` climb sent the bad traffic itself. Check it:
`cargo test -p regia_net --test malformed_attribution`.
