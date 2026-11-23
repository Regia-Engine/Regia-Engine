# Net migration (agents)

`regia_net::migration` keeps one or two clients of a listen server ready to take the match over: the **successor
journal** (spec 120 § 10.5, ADR-8853). The host streams each successor a full state every 5 s and the log of every
tick; the successor applies it to a shadow world. This page is what an agent reads to check it: the `net.migration`
method, what its numbers mean, the refusal codes, and how to verify the behaviors. The player-facing description is in
[Networking](../user-guide/networking.md). Host migration for lockstep matches is another mechanism; see
[Net lockstep](net-lockstep.md).

Part 2, the hand-off (`regia_net::handoff`, ADR-8901: the first successor becomes the server and the clients resume
to it), reports through the `handoff` object of the same method; see [The hand-off](#the-hand-off) below.

## `net.migration`

`net.migration {endpoint?}` reports every host journal and successor the game publishes into `NetAgentState` with
`JournalHost::publish(label, state)` and `JournalSuccessor::publish(label, state)`. Without `endpoint` it lists all of
them; without the bridge resource the list is empty.

```json
{ "endpoint": "server" }
→ { "endpoints": [{ "label": "server", "role": "host", "state": "warm", "tick": 3600,
      "journal_lag_ticks": 1, "journal_lag_ticks_p99": 1,
      "successors": [{ "peer": 2, "rank": 0, "state": "warm", "acked_tick": 3599,
                       "lag_ticks": 1, "lag_ticks_p99": 1, "lag_ticks_max": 2, "queued_ticks": 0,
                       "full_seq": 12, "bytes_sent": 861250, "bytes_per_s": 14176,
                       "budget_bytes_per_s": 16008, "budget_bytes": 870112, "seconds_over_budget": 0,
                       "digest_mismatches": 0, "replay_divergences": 0, "fell_behind": 0 }],
      "rank": null, "base_tick": 3302, "full_states": 12, "full_state_bytes": 49812,
      "log_ticks": 600, "depth_ticks": 600, "digest_checks": 0, "digest_mismatches": 0,
      "replay_divergences": 0, "reloads": 0, "successor_changes": 1,
      "refused": 0, "last_refusal": null, "promotable": false }] }
```

Host fields (`role: "host"`):

| Field | Meaning |
|---|---|
| `state` | `no_successor` (no client can be one), `seeding` (no successor is warm yet) or `warm` (at least one is). |
| `tick` | The newest journaled tick. |
| `successors[]` | The successors, first successor first. `rank` 0 takes over first. |
| `successors[].state` | `seeding` (its first full state is on the way, or it is being seeded again), `warm` (it holds a full state and follows the log), `stale` (it refused a payload and waits for a full state). |
| `successors[].acked_tick` | The newest tick the successor said it applied. |
| `successors[].lag_ticks`, `lag_ticks_p99`, `lag_ticks_max` | Journal lag: the newest journaled tick minus `acked_tick`, read just before the host journals a tick. It includes the acknowledgment's way back, so it reads at most about a round trip above the shadow's true lag. |
| `journal_lag_ticks`, `journal_lag_ticks_p99` | The worst successor's `lag_ticks` and `lag_ticks_p99`. |
| `successors[].queued_ticks` | Journaled ticks not yet handed to the transport: above 1 for long means the log alone is over the budget. |
| `successors[].full_seq` | The newest full state sent whole. |
| `successors[].bytes_sent`, `bytes_per_s` | Journal bytes handed to the transport, in total and in the last whole second. |
| `successors[].budget_bytes_per_s`, `budget_bytes` | The budget now (`budget_share`, a quarter by default, of `NetServer::send_allowance` for that peer) and the budget accrued since the appointment. `bytes_sent` never exceeds `budget_bytes`. |
| `successors[].seconds_over_budget` | Whole seconds in which more left than that second's budget plus what an idle bucket holds (two ticks of the rate, at least one payload). Always 0. |
| `successors[].digest_mismatches`, `replay_divergences` | From the successor's acknowledgment; see below. |
| `successors[].fell_behind` | Times the successor's next record had left the host's ring (its budget did not carry the journal for about two depths) and it was seeded again. |
| `base_tick`, `full_states`, `full_state_bytes` | The newest full state's tick, how many were taken, and its size. |
| `log_ticks`, `depth_ticks` | Ticks in the host's ring and the journal depth (600). |
| `successor_changes` | Times the set of successors or their order changed (the first appointment counts). |
| `refused`, `last_refusal` | Acknowledgments refused and the last code. |

Successor fields (`role: "successor"`; a client that is not one reports `role: "client"`, `state: "idle"`):

| Field | Meaning |
|---|---|
| `state` | `seeding`, `warm` or `stale`. |
| `rank` | 0 for the first successor, 1 for the second. |
| `tick` | The newest tick applied to the shadow world. |
| `journal_lag_ticks` | Log records held and not applied yet (0 when warm). |
| `base_tick`, `full_state_bytes`, `log_ticks` | The newest full state held, its size, and the ticks of log held after it (at most `depth_ticks`). |
| `full_states`, `reloads` | Full states taken, and how many of them had to be loaded into the shadow. A shadow whose digest already matched the host's at the full state's tick adopts it without loading: `reloads` stays 1 on a correct journal. |
| `digest_checks` | Times the shadow's digest was compared with the host's (every full-state tick, and every load). |
| `replay_divergences` | Full-state ticks at which the replayed shadow differed from the host. Not zero means the game's log leaves something out: the full state that follows repairs the shadow, and the game should journal the missing event. |
| `digest_mismatches` | Loaded full states whose digest was not the one sent with them. Not zero means the game's full-state encoding and its digest disagree. |
| `refused`, `last_refusal` | Payloads refused and the last code. |
| `promotable` | The shadow holds a full state and has applied the log: `JournalSuccessor::promote` would succeed. |

## How it works

- **Successors.** Every second `JournalHost::pump_server` reads the players' link numbers and keeps the two best
  connected as successors: the lowest round trip plus its variation plus 10 ms per percent of loss, ties by the lowest
  peer id. The score is smoothed with a 10 s time constant (`score_smooth_ms`), so one burst of loss does not unseat a
  successor. A client whose send allowance is under 16 KiB/s (near the rate controller's floor) is **starved** and
  ranks after every client that is not; an interrupted client is never picked. A sitting successor is replaced when it
  leaves or goes silent (at once), when it fell out of the journal and is not warm again while a client with a link
  that is not starved waits, or when another client's score is under half of its own and at least 20 ms better (both
  checked every 10 s). A warm successor is never replaced for its allowance alone. When the first successor goes, the
  second becomes the first and keeps its shadow.
- **Full states.** `JournalHost::full_wanted(now_ms)` is true every 5 s while a successor is free to start one, and
  when a successor needs one the newest cannot give (it is older than the journal depth, or that shadow failed on it;
  at most one extra per second). The game then calls `record_full(now_ms, tick, bytes, digest)`. A full state travels
  as a 54-byte start and data parts that fill what is left of each payload and of the budget.
- **Log.** `record_tick(tick, inputs, events, digest)` every tick. A log payload has a five-byte header and one flags
  byte per tick; empty blobs are not written, so an idle tick costs six bytes. When the budget is short several ticks
  share a payload. The log goes before full-state data.
- **Budget.** Each successor has a token bucket filled at `budget_share` × `send_allowance(peer)`; nothing borrows.
  The transport still schedules the bulk stream last and paces it.
- **One fragment at a time.** What one poll hands a successor is one payload of at most `part_bytes` (900): the log
  and a part of the full state together, in one fragment of the bulk stream. `pump_server` reads the stream's
  `queued_fragments` (`NetServer::stream_status`) and hands out the next payload only while at most one fragment is
  unacknowledged, and none when the peer's send allowance is under 32 KiB/s (stop-and-wait). The reason: the
  transport fails a connection whose stream has a gap with later fragments stored behind it for about 2 s
  (`reassembly_stalled`), and on a link at the rate floor the sender's pacing holds bulk repairs back that long. With
  nothing behind the gap that cannot happen. The price: at most one payload per poll, 54 KB/s per successor at 60 Hz.
- **Depth.** A successor holds its newest full state and at most `depth_ticks` (600) ticks of log after it. A record
  at exactly that depth is taken; one past it is refused (`journal_past_window`). The host does not send past the
  window: its log waits there for the next full state, and the shadow stays promotable at the tick it has. A
  successor whose next record has left the host's ring (two depths) has fallen behind and is seeded again.
- **Digests.** The host's world digest rides the log record of a full-state tick. The successor compares its shadow
  there. If it matched, the full state that follows is adopted without loading. Otherwise the full state is loaded,
  its digest is checked again (`journal_digest_mismatch` on failure), and the log held after it is replayed.

## Refusal codes

These appear in `last_refusal` and in `JournalError::code()`; `net.migration` itself has one error code.

| Code | Meaning |
|---|---|
| `migration_unknown_endpoint` | `net.migration` named a label no host journal or successor published. |
| `journal_malformed` | The payload does not decode, or a length is over its cap. |
| `journal_not_successor` | Journal data for a client that is not a successor, or an acknowledgment from a peer that is not one. |
| `journal_unexpected` | A payload this side never takes (a log sent to the host, an acknowledgment sent to a client). |
| `journal_stale_appointment` | The payload names an appointment that is over. |
| `journal_no_base` | A log record arrived before any full state started. |
| `journal_gap` | A log record skipped ticks. The shadow is stale. |
| `journal_past_window` | A log record more than `depth_ticks` after the newest full state. The shadow is stale. |
| `journal_full_out_of_order` | Full-state bytes out of order, without a start, or past the declared length. |
| `journal_full_too_large` | `record_full` got more than 8 MiB. |
| `journal_record_too_large` | `record_tick` got an inputs or events blob over 16 KiB. |
| `journal_load_failed` | The shadow world refused a full state. The shadow is stale. |
| `journal_apply_failed` | The shadow world refused a log record. The shadow is stale. |
| `journal_digest_mismatch` | A loaded full state does not have the digest sent with it. The shadow is stale. |
| `journal_tick_not_after` | `record_tick` got a tick at or before the newest. |
| `journal_full_tick_unknown` | `record_full` got a tick that is not the newest journaled tick. |
| `journal_not_warm` | `promote` on a shadow that is not promotable. |

## Verifying

```text
wait_until net.migration {"endpoint":"server"}: endpoints[0].state == "warm"
expect net.migration {"endpoint":"server"}: endpoints[0].successors[0].lag_ticks_p99 <= 3
expect net.migration {"endpoint":"server"}: endpoints[0].successors[0].seconds_over_budget == 0
expect net.migration {"endpoint":"server"}: endpoints[0].replay_divergences == 0, endpoints[0].digest_mismatches == 0
expect net.migration {"endpoint":"client"}: endpoints[0].promotable == true, endpoints[0].reloads == 1
```

- **Journal lag.** `lag_ticks_p99` at the host includes the acknowledgment's way back. On a good link it reads 1-2
  ticks; the shadow's true lag is one tick less.
- **A successor that never gets warm, or lags.** `state: "seeding"`, `queued_ticks` or `fell_behind` rising and
  `budget_bytes_per_s` small: the journal does not fit its share of that link's allowance. Raise `budget_share`,
  lengthen `full_every_ms`, make the full state smaller, or journal less per tick. On a link the rate controller holds
  at its 4 KiB/s floor a quarter share is 1 KiB/s: about 0.5 KiB/s of log and a 2.5 KiB full state every 5 s.
- **A missing event.** `replay_divergences` above 0 on a successor: compare what the game applies to its world with
  what it passes to `record_tick`.
- **A game that sends the payloads itself** (not through `pump_server`) must use `JournalHost::poll_links` with the
  bulk stream's unacknowledged fragment count. `poll` alone keeps feeding the stream, and on a weak link that can
  cost the client its connection (`reassembly_stalled`; `net.peers` shows the stream's `stall_ms`).

The tests behind this page: `crates/regia_net/src/migration_tests.rs` (scripted pipe) and
`crates/regia_net_soak/tests/migration_journal.rs` (the real transport on seeded `good-wifi` and `bad-wifi` links).

## The hand-off

`regia_net::handoff` (spec 120 § 10.5, ADR-8901) moves the match to a successor when the host is lost. A game
publishes its side with `HandoffHost::publish(label, state)` on the host and `HandoffClient::publish(label, state)` on
every client (a promoted successor keeps publishing its `HandoffClient`); `net.migration` then carries a `handoff`
object for that label (`null` when nothing was published; a label only the hand-off published has empty journal
fields).

```json
{ "endpoint": "client3" }
→ { "endpoints": [{ "label": "client3", "role": "client", "state": "idle",
      "handoff": { "role": "client", "state": "resumed", "epoch": 2, "roster_version": 2,
                   "successors": [{ "peer": 5, "rank": 0, "addr": "203.0.113.9:40000", "key": "5f0c…" }],
                   "host_lost_ms": 1500, "takeover_ms": 3000, "rank": null, "granted": false,
                   "host_losses": 1, "last_handoff_ms": 1720, "last_handoff_rank": 0,
                   "expected_peers": 0, "resumed_peers": 0, "quorum": false,
                   "rosters_sent": 0, "grants_sent": 0, "refused": 0, "last_refusal": null } }] }
```

(The endpoint's journal fields, described above, are left out of this example.)

| Field | Meaning |
|---|---|
| `role` | `host`; `client`; `successor` (a client the roster lists); `promoted` (a successor that became the server); `returning_host` (a lost host on its way back as a client). |
| `state` | Host: `no_successor` or `ready` (a successor announced its key and every client is being told). Others: `watching`, `resuming` (the host is lost and a successor is being tried), `armed` (a successor whose server is open and waits for a majority of the players, or for its host), `resumed`, `promoted`, `abandoned` (no successor answered). |
| `epoch`, `roster_version` | The host generation of the roster held (1 for the host that started the match, one more per hand-off) and its version. |
| `successors` | The roster: each successor's peer id, rank, the address it would serve on and the public key of the server it would become. Tickets and secrets are never reported. |
| `host_lost_ms`, `takeover_ms` | The host's thresholds: silence after which it counts as lost (1,500) and how long a successor has to answer (3,000). |
| `rank`, `granted` | A successor's rank, and whether it holds the host's grant for the key it announced (without it, it cannot promote). |
| `host_losses` | Times this endpoint saw its host lost. |
| `last_handoff_ms`, `last_handoff_rank` | The last hand-off: milliseconds from the loss being seen to this endpoint being accepted by the successor (or promoted), and that successor's rank. |
| `expected_peers`, `resumed_peers`, `quorum` | Armed or promoted: the players of the lost host it holds a place for (its own player's seat included), how many resumed, and whether a majority of its clients did. |
| `rosters_sent`, `grants_sent` | Host: payloads sent. |
| `refused`, `last_refusal` | Hand-off payloads refused and the last code. |

How it runs is in [Networking](../user-guide/networking.md#host-migration-the-hand-off-spec-120--105). In short: a
client that has not heard the host for `host_lost_ms` tries the roster's successors in order, each for `takeover_ms`,
on a second socket; the successor whose turn it is arms (opens a server under the announced key on the announced port
and holds every player dormant under its old `PeerId`, `NetServer::adopt_dormant`) and commits (promotes its shadow)
when a majority of the players arrived or its own connection to the host ended; a client that hears its host again
before a successor accepted it stays, and an armed successor that hears it disarms.

Refusal codes (`HandoffError::code()`, in `last_refusal`):

| Code | Meaning |
|---|---|
| `handoff_malformed` | The payload does not decode. |
| `handoff_wrong_kind` | A payload this side never takes (a roster sent to the host, an announcement sent to a client). |
| `handoff_zero_key` | An announced or listed key of all zeros. |
| `handoff_not_a_player` | An announcement from a peer the host does not list as a player. |
| `handoff_too_many_successors` | A roster with more than two successors. |
| `handoff_too_many_peers` | A grant with more than 1,024 players. |
| `handoff_stale` | A roster or grant older than the one held. |
| `handoff_duplicate_peer` | A roster or grant that lists a peer twice. |
| `handoff_zero_ticket` | A roster ticket of all zeros. |
| `handoff_bad_threshold` | A roster threshold of zero or over a minute. |

`NetServer::adopt_dormant` refuses with `adopt_resume_off` (the server has no resume grace), `adopt_zero_ticket`,
`adopt_server_id`, `adopt_id_in_use`, `adopt_ticket_in_use` and `adopt_closed` (`AdoptError::code()`).

Verifying a hand-off:

```text
wait_until net.migration {"endpoint":"server"}: endpoints[0].handoff.state == "ready"
expect net.migration {"endpoint":"client3"}: endpoints[0].handoff.successors[0].rank == 0
# stop the host, then on the successor and on a client:
wait_until net.migration {"endpoint":"client5"}: endpoints[0].handoff.role == "promoted"
wait_until net.migration {"endpoint":"client5"}: endpoints[0].handoff.quorum == true
expect net.migration {"endpoint":"client3"}: endpoints[0].handoff.state == "resumed", endpoints[0].handoff.last_handoff_ms <= 5000
```

- **A client that ends `abandoned`.** No successor answered: none was warm (`promotable` false on both), the
  successor's address is not reachable from this client, or the client's own link was out for longer than both
  takeover windows. `net.status` of the promoted server shows who resumed (`resumes`, `resumes_refused`).
- **`resumes_refused` above 0 on the promoted server.** A client presented a ticket this server does not hold: its
  roster named another successor, or its place was given up when the resume grace ended.
- **`quorum` false on a promoted server after 10 s.** It took the match over without a majority (it announced no
  port, or its own connection ended first) and fewer than half of the players came: the old host may still be
  serving the rest (`NoQuorum` in the game's event readout).
- **A successor that stays `armed`.** Fewer than half of the players reached it and its host connection has not
  ended: `resumed_peers` says how many came. It commits when the transport gives the host up, or disarms when the
  host is heard again.

The tests: `crates/regia_net/src/handoff_tests.rs` (a scripted clock and a loopback net) and
`crates/regia_net_soak/tests/migration_handoff.rs` (16 players on seeded `good-wifi` and `bad-wifi` links).
