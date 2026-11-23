# Net state channel (agents)

How entity state travels between a server and its clients (`regia_net::replication`, spec 72 §§ 3, 4, 7, 14, ADR-1950).
Guide topic `net-state-channel`.

## The two wires

| Wire (`net.state_channel` `wire`) | Who | How state travels |
|---|---|---|
| `undecided` | a connection before the client's first `ACK` | empty tick frames only, one per send tick |
| `acked_deltas` | a client that speaks protocol 2 or later (protocol 3 adds the [entropy stage](net-entropy.md)) | spawns and deltas in unreliable `STATE` frames, against the baseline the client acknowledged |
| `reliable_stream` | a protocol 1 client, or every client with `replication.state_channel: Reliable` | one reliable tick frame per send tick |

On `acked_deltas`:

- A lost frame needs no resend. The next delta for its entities is computed against the baseline the client still
  holds, so the lost change travels again by itself.
- A record is not sent again while a frame with the same version may still be on its way. It goes again when a newer
  frame is acknowledged without it, or after the in-flight timeout with no acknowledgment at all. The timeout follows the
  measured round trip of state frames (`in_flight_timeout_ticks`: the smoothed round trip plus four deviations plus two
  ticks; `IN_FLIGHT_TIMEOUT_TICKS`, 30, before the first sample; ADR-8419).
- A spawn is repeated until acknowledged, not while one of any version is still on its way. The client spawns once; to a
  copy that has it, a repeat is a change (no second spawn notice).
- A record for a version the client already holds still counts when it carries more: an audience change (ownership
  opening an owner-only component) makes no new version. A true repeat lands in `records_stored_only`.
- `net_resend`, a join and a connection gaining an asleep entity read the world again, so writes change detection never
  saw still reach the client.
- The client keeps up to 512 versions per entity (`STATE_VERSION_RING`, ADR-10051), and in practice far fewer: it lets
  go of the versions older than the baseline the server named, 8 ticks after a newer frame of the entity arrived
  (`STATE_BASE_REORDER_TICKS`). An entity that changes every tick stores about one round trip of versions plus those of
  8 to 16 ticks (measured at the default send rate: 9 to 10 on a 12 ms path, 87 to 90 at 3,000 ms).
  Older arrivals are stored as possible baselines, not applied, so reordering inside 8 ticks is harmless. A frame later
  than that which names a version already let go counts as lost: it is not acknowledged and asks for nothing
  (`records_retired_baseline`). A delta of the newest frame against a version the client does not have is dropped and
  asks for a resync.
- A baseline 512 versions behind gets a full state (`baselines_expired` counts them): a round trip of 4 s at 128 Hz or
  8.5 s at 60 Hz for an entity that changes every tick.
- Ownership bits, despawns, out-of-scope notices and reliable messages stay on reliable tick frames, which go out only
  when they have something to say. Every send tick still sends one `STATE` frame: it is the client's clock.

The negotiation needs no extra message. A protocol 1 server stops reading an `ACK` after the tick, and a protocol 1
client answers its first frame with an `ACK`, so protocol 2 clients add the extension after the tick and the server
decides on the first `ACK` it gets. That costs one round trip at join, before the join snapshot. Spec 77's `Hello` names
the oldest protocol the client speaks (`MIN_NET_PROTOCOL_VERSION`), so builds of protocol 1 and 2 join each other.
Protocol 4 (ADR-8467) changed the transform record (below): a build of protocol 3 or older has another protocol hash
(the record's revision is part of it), so it is refused at the handshake with `protocol_mismatch` and never misread; a
dev build names `regia_net::replication::transform_record` among the differing types.

## The transform record (spec 120 § 8.2, ADR-8467)

- Position: whole steps of the entity profile's `position_step_m` (default 1 mm), 20 bits inside a cell of `2^20` steps
  plus the cell per axis; the client's position is within half a step of the server's. A replicated child, a profile
  without a step, or a position past the grid's range (`|steps| > 2^31`) travels as raw `f32`.
- Rotation: smallest-three, 10 bits per kept component (as before).
- Velocity: 3 x 12 bits of 1/32 m/s; past +-64 m/s or not finite, raw `f32`.
- On `acked_deltas` an update is a delta against the baseline the client acknowledged: one change bit per part and per
  component and a short residual for what changed, so a still rotation or a steady velocity costs one bit. The client
  rebuilds the server's integers exactly; a delta adds no error. Spawns and `reliable_stream` updates carry the
  absolute record (156 bits for a moving root at the default precision).
- Records name their tick distances (`version_ago`, `base_ago`) in one bit for "this tick" or seven for up to 32 ticks,
  and an update without components spends one bit on its empty entry list.

Measured on a walking crowd (256 walkers, default profile and budget, 100 ms round trip): 9.0 bytes per moving entity
update where protocol 3 spent 35.8, so the default budget carries 142 walkers per send tick instead of 45
(`crates/regia_net/tests/quantized_transform.rs`). Revision 3 (ADR-8522) adds a steady bit to every state-channel
transform block and a velocity-predicted position residual (one bit picks it); the extrapolation gate itself is a
profile option, off by default: see [net extrapolation gate](net-extrapolation-gate.md).

## Cadence and packet size

- `replication.send_every_ticks` (default 2): frames are assembled on ticks that are a multiple of it. Priorities and
  byte budgets count send ticks (`rate / send ticks per second`); linger, `min_interval_ticks` and sleep keep counting
  ticks. New entities get their `NetId` on the next tick whether it sends or not, and messages wait for the next send
  tick. Server-side `NetChanged` notices follow send ticks too.
- `replication.mtu_bytes` (default 1200): the largest state frame with transport headers;
  `STATE_PACKET_RESERVE` (64 bytes) of it is left for them. A send tick sends several frames while budget remains. A
  record larger than one frame goes on the reliable replication stream, one at a time per connection: the next waits
  for the last one's own acknowledgment (or four in-flight timeouts), not for a newer frame's (ADR-8419).
- Entity state over 16 KiB (`send::MAX_ENTITY_STATE_BYTES`) is not sent, as a spawn or as an update. A client that held
  a copy gets a despawn notice (out of scope for a level entity) instead of keeping a frozen copy; the entity comes back
  when its state fits. The server logs `net.state_too_large` once per crossing of the cap.
- Every record obeys the byte budget, spawns and forced records (`max_interval_ticks`, a join's own and game-state
  entities) too; the first record of a send tick with budget left may overshoot, and the overspend is paid back from
  the following send ticks in full.

## Reading `net.state_channel`

`{ peer? }`, read. On a server:

| Field | Meaning |
|---|---|
| `protocol`, `min_protocol` | what this build speaks |
| `transform_record` | `revision` (4: the quantized record with the steady bit and predicted residuals, ADR-8522, and the far-tier record, ADR-10124), `position_cell_bits` (20), `default_step_m` (the step a record names in one bit), `velocity_step_mps` (1/32), `prediction_tick_hz` (60: the position predictor's assumed rate), `gate_rotation_deg` (0.5: the rotation half of the extrapolation gate), `far_record_bits` (40), `far_xz_shift` (5) and `far_y_shift` (10: a far record's coarse steps, see [Net far record](net-far-record.md)) |
| `state_channel`, `send_every_ticks`, `client_rate_bytes`, `server_rate_bytes`, `mtu_bytes` | the settings in force (on a dedicated server: `config/net.ron` with the `server.toml` keys it names applied, ADR-7213) |
| `peers[].wire` | the table above |
| `peers[].packets_sent`, `packets_acked`, `packets_lost` | state frames; lost means it left the log unacknowledged |
| `peers[].rtt_ticks`, `rtt_samples` | the smoothed round trip of state frames in ticks (`null` before the first acknowledgment) and the samples taken |
| `peers[].in_flight_timeout_ticks` | how long an unacknowledged record counts as on its way (from the round trip) |
| `peers[].log_frames` | sent frames remembered for acknowledgments: at least the last 256, every frame younger than two timeouts, at most 4096 |
| `peers[].oversize_pending` | a record larger than a frame is on its way; the next one waits |
| `peers[].records_sent` | spawn and delta records |
| `peers[].baselines_expired` | full states sent because a baseline left the ring |
| `peers[].oversize_records` | records larger than a frame |
| `peers[].gate_held` | update candidates the [extrapolation gate](net-extrapolation-gate.md) held back (0 unless a profile sets `extrapolation_gate_m`) |
| `peers[].far_records` | far-tier updates sent as [far records](net-far-record.md) (0 unless a profile sets `rate_tiers`) |
| `far_records` | whether far-tier updates may travel as far records (`ServerReplication::set_far_records`, on by default) |
| `far_settle_seconds` | 2: a far copy whose entity has not changed for this long gets its exact transform ([far records](net-far-record.md), ADR-10199) |
| `last_send_ms` | the last send tick's wall time: `changes`, `relevance`, `priority_and_packing` (spec 72 § 14's budget is the last two), and where it went: `relevance_shared` is the serial part of `relevance`; `walk_cpu`, `priority_cpu`, `pack_cpu`, `commit_cpu` are CPU time summed over connections, so they can exceed the wall time |
| `state_too_large`, `too_large_entities` | entities whose state crossed the 16 KiB cap (counted once per crossing), and the `NetId`s withheld now |
| `ordered_candidates` | ranking candidates put in order at the last send tick, summed over connections: about what the budgets can reach, not everything that waits. A value near the relevant count times the connections means a budget is not stopping the ranking |

On a client: `on_state_channel`, `packets`, `duplicates`, `records_applied`, `records_stored_only` (arrived late),
`baselines_missing`, `record_errors`, `records_tombstoned` (for a despawned id), `entities_stored`, `versions_stored`,
the version ring (ADR-10051): `version_ring` (the most versions one entity keeps, 512), `longest_ring` (the most one
entity has stored now: about one round trip of versions plus those of 8 to 16 ticks), `version_bytes_stored` (bytes of the stored
versions and their component lists, without the component values they share) and `records_retired_baseline` (deltas of
a frame reordered by more than 8 ticks against a version let go since: the frame counts as lost, no resync),
and the client entity cap (ADR-8760): `entity_cap` (65536 unless the host set `ClientStateChannel::entity_cap` lower),
`tombstones`, `ownership_held` (ownership bits for copies not here yet), `spawns_over_cap` (spawns refused past the cap;
each ends the session with `protocol_error`) and `cap_evicted` (tombstones and held ownership bits dropped oldest first
when their map reached the cap). The host-set `entity_cap` survives connects and replay resets (ADR-8850). When one
client's relevant set reaches 90 % of the cap the server warns once (`net: one client's relevant set is near the entity
cap`, ADR-8850), counted per connection in `ServerReplication::counters.relevant_near_cap` — the early signal before
the client-side refusal.

What to check:

- A client that should be on the channel shows `reliable_stream`: it did not send the extension, so it is an older build.
- `packets_lost` high and `baselines_expired` rising: the client's acknowledgments are not getting through.
- `baselines_missing` on a client above a handful: the server named a version the client does not hold (a round trip
  longer than the 512-version ring, or a copy that lost its state).
- `baselines_expired` rising with `packets_lost` low: the round trip is longer than the ring holds (512 versions: 4 s at
  128 Hz, 8.5 s at 60 Hz for an entity that changes every tick).
- `longest_ring` far above the round trip in ticks: acknowledgments are late or lost, so the server's baseline is old.
- `records_retired_baseline` rising: the path reorders frames by more than 8 ticks; those frames count as lost.
- `spawns_over_cap` above 0: the server tried to give this client more entities than its cap; the session ended with
  `protocol_error`. A real game past 65536 live entities per client wants interest management (spec 72 § 6).
- `cap_evicted` rising: despawns or ownership changes for unseen copies arrive faster than they expire.
- `oversize_records` rising: an entity's state does not fit a packet; a `NetVec` or a large component may want its own
  entity.
- `too_large_entities` not empty: those entities are on no client. Split their state (a `NetVec`, a child entity) or
  shrink it under 16 KiB; they come back by themselves when they fit.
- `in_flight_timeout_ticks` far above `rtt_ticks`: the round trip is jittery; `log_frames` near 4096: frames in flight
  outrun the log (a very fast connection on a very long round trip), so acknowledgments start to miss.

## Errors

`F0002` with `data.reason`: `net_not_running`, `peer_unknown`.
