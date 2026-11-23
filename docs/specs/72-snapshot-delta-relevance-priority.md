# 72 — Snapshot delta, relevance and priority

Status: proposed (ADR-1599). Plan item D3 (`master-plan.md` § 12: "snapshot delta and relevance"), rung v0.5.0. Crate:
`regia_net::send` (server send loop) and `regia_net::relevance`. Builds on specs 70 (peers, messages) and 71 (what an entity's
replicated state is). Feeds spec 74 (the client's snapshot buffer) and spec 73 (acknowledged input ticks ride the same packets).

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: none of this exists. Reused: `bevy_tasks` (already a dependency, `docs/03` row), debug draw groups in
`regia_framework::debug_gizmos`, and `regia_framework::camera` for default viewpoints.

Builders read only this file and specs 70-77. Needed facts from elsewhere: the transport (the `ir-net` extraction, spec 70 § 1) gives
per-connection packet sequence numbers, an acknowledgment bitfield per received packet, a loss verdict per sent packet, MTU-aware
fragmentation of reliable records, and the four channels; nothing in this spec adds to that interface.

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-actor-replication`
(relevance, priority, sleep behavior), `runtime-networking/runtime-networking-replication-graph` (shared per-area candidate lists
at scale), `runtime-networking/runtime-networking-netrep` (filter ordering, accumulating priority). Re-derived from public sources
(§ 16); no wording, identifiers, settings or numeric defaults carried over.

## 1. Purpose and scope

Every send tick the server decides, for each client connection: which replicated entities this client should know about
(**relevance**), which of those to update first when the connection cannot carry everything (**priority**), how many bytes it may
use (**budget**), and for each chosen entity, the smallest delta that brings the client from what it has acknowledged to the current
state (**delta against the acknowledged baseline**). Lost packets need no resend logic: the next delta is simply computed against the
older baseline the client still holds.

In scope: the send schedule, baselines and acknowledgments, delta encoding, the relevance pipeline (hard filters, spatial interest,
custom filters, links, temporary grants, hysteresis and linger), the shared interest grid, priority accumulators, bandwidth and spawn
budgets, packet assembly order, resync, `.netprofile.ron`, and debugging surfaces.

Not in scope: transport reliability and fragmentation (T2.3 / the `ir-net` extraction), what is replicated (spec 71), message delivery
rules (spec 70), the client's buffering and interpolation (spec 74).

## 2. Terms

| Term | Meaning |
|---|---|
| Send tick | A server tick on which packets are assembled. With `send_every_ticks = 2` at 60 Hz, 30 per second. |
| Viewpoint | A world position (and facing) from which a client sees the world: by default the camera or controlled character of each of its players. |
| Relevant | The client should currently hold a copy of the entity. |
| Baseline | For one (connection, entity), the newest state version the client has acknowledged. Deltas are computed against it. |
| State version | A snapshot of one entity's replicated state at one send tick, stored once and shared by all connections. |
| Priority accumulator | Per (connection, entity), a number that grows each send tick the entity has unsent changes and resets when sent. |
| Profile | A `.netprofile.ron` asset with an entity's relevance, priority, rate and quantization settings. |
| Interest grid | A uniform 2D grid over the world (XZ plane) whose cells list the spatially filtered entities in them. Built once per send tick, shared by every connection. |

## 3. Send schedule

- The server simulates at `FixedTime` (default 60 Hz). Packets are assembled on send ticks, every `send_every_ticks` ticks
  (`config/net.ron` `replication.send_every_ticks`, default 2). Each packet carries the server tick of the state it holds.
- A profile's `min_interval_ticks` caps how often one entity can be sent (a slow-changing scoreboard: 30). Its `max_interval_ticks`
  (optional) forces a send of an entity with changes at least that often regardless of priority.
- Packets are assembled at the end of the send tick's `FixedUpdate` (`NetSet::Send`, spec 70 § 8.1), after spec 71's gather and after
  messages are queued, and handed to the transport in `PostUpdate` (`NetSet::Flush`).
- A send tick is a tick that is a multiple of `send_every_ticks`. On other ticks new entities still get their `NetId` and messages are
  routed to their connections; they leave on the next send tick. Priorities and byte budgets count send ticks; linger, grants,
  `min_interval_ticks`, `max_interval_ticks` and sleep count ticks.

## 4. Baselines and deltas

### 4.1 State versions

- When a replicated entity has changes in a send tick, the server stores a new state version: the quantized values of every replicated
  component (spec 71 § 3 codec), tagged with the tick. Versions are shared by all connections and share the values that did not change
  with the version before them. A connection holds its baseline version by reference (ADR-1950), so a version lives as long as something
  can delta against it, and a quiet entity holds one.
- A connection whose baseline is 64 versions behind the entity's current one (512 since wire 6, ADR-10051: the ring holds a 3 s round
  trip at 128 Hz, and the client lets go of versions older than the baseline the server names instead of storing a fixed count) (it
  acknowledged nothing for that entity for that many versions, so the client's ring may have dropped it) is sent a full state for that entity as a repeat spawn record, which then becomes its baseline
  once acknowledged.

### 4.2 Acknowledgment

- Every packet has a sequence number; the client acknowledges packets through the transport's ack bitfield (T2.3). The server keeps, per
  connection, the list of (entity, version tick) sent in each of the last 256 packets.
- When a packet is acknowledged, every (entity, version) it carried becomes that connection's baseline for the entity, if newer than the
  current one. When a packet is declared lost, nothing is done: the next delta for those entities is computed against the baseline the
  client still has, so the lost change is carried again automatically.
- The acknowledgment is the state channel's own: each state frame has a sequence number, and the client's `ACK` frame names the newest one
  received and a bit for each of the 256 before it (ADR-1950; spec 70's transport bitfield covers 32 packets and is below the frame layer).
- A record is not offered again while a packet carrying the same version may still be on its way: until that packet is acknowledged, a
  newer packet is acknowledged without it (a loss verdict), or 30 ticks pass with neither. Without this, every change would travel once
  per send tick for a whole round trip (ADR-1950). A copy its client predicts (spec 73) stays offered: reconciliation compares every state
  frame with what the client ran.
- A spawn record's version becomes the first baseline. Until it is acknowledged, every send to that connection for the entity is again a
  full spawn record (marked "repeat"), so a lost spawn heals itself. The client ignores repeat spawn records for entities it has already
  spawned, except to apply their values.

### 4.3 Delta encoding

- A delta for one entity lists the components that changed between baseline and current version (one bit per registered component
  present on the entity, then component adds and removals), and within each changed component the changed fields (spec 71 § 3).
- The receiving client must hold the exact baseline the delta was made against. Each delta names its baseline version tick; the client
  keeps, per entity, the last 64 applied version ticks with their values (the same ring idea) and applies the delta to the named one. A
  delta against a version the client no longer has is dropped and the client asks for a resync of that entity (§ 9).
- The client applies a delta only if its version tick is newer than the newest version it has applied for that entity; older ones are
  stored (as possible baselines) but not applied. This makes reordered packets harmless.
- **As built (ADR-8760):** a client keeps state for at most `MAX_CLIENT_ENTITIES` (65536, the snapshot codec's entity
  limit) entities, or `ClientStateChannel::entity_cap` when the host sets a lower one. A spawn past it (on the state
  channel or the reliable tick stream) is a protocol error that ends the session; tombstones and ownership bits held
  for copies not here yet stay at the same cap by expiring, then dropping their older half.
- **As built (ADR-8850):** the host-set `entity_cap` survives `client_session_started` (every connect) and a replay's
  reset, like `level_bind_cap`. `MAX_CLIENT_ENTITIES` is `snapshot_delta::MAX_ENTITIES`, a constant of the standalone
  snapshot codec — the live replication wire had no entity limit before the state channel adopted it (ADR-8760's
  "the wire's own entity limit" wording corrected). The server warns once per connection when one client's relevant set
  reaches 90 % of it (`ReplicationCounters::relevant_near_cap`).

## 5. Relevance

### 5.1 Pipeline

For each (connection, entity) the server evaluates the stages below. Stages are of two kinds: **grants** (1, 2, 4, 7) make an entity
relevant, **filters** (3, 5, 6) can only remove it. The result is decided by this precedence, highest first:

1. **Owner rule (grant).** An entity owned by the connection's peer is always relevant to it. Nothing overrides this.
2. **Denies (filter, stage 3 deny parts).** A `NetVisibility` deny naming the peer, or a denied group, removes the entity. Denies beat
   every grant below.
3. **Owner-exclusive (filter).** A profile with `owner_only: true` is relevant to its owner only; it beats `sent_to_all`
   (a profile with both logs `net.profile_conflict` at load and is treated as owner-exclusive).
4. **Grants.** `sent_to_all`, the link rule and temporary grants make the entity relevant, skipping the remaining filters.
5. **Filters.** Otherwise the entity is relevant only if it passes the allow parts of stage 3, then stage 5, then stage 6.

The stages:

1. **Owner rule.** As above.
2. **Sent to all.** Profile `sent_to_all: true` (game state, team scores): relevant to every connection in `InGame`.
   As built (ADR-7213): replication reads the session states from `NetSessionPeers` (written by the session system);
   a connection the session does not know counts as in game, and a loading one gets "not relevant" at this stage.
3. **Hard filters**, re-evaluated only when their inputs change (cheap):
   - `owner_only: true` profiles: relevant only to the owner.
   - `NetVisibility { allow: Vec<PeerId>, deny: Vec<PeerId> }` component on the entity: an explicit allow list (if non-empty, only those
     peers) and a deny list.
   - Group visibility: the entity's `NetGroups` (spec 71 § 6) against the connection's `NetGroupVisibility { allow: Vec<Name>, deny:
     Vec<Name> }`, a component on the connection's `Player` entities (spec 70 § 4; for a split-screen peer the union of its players).
     An entity is denied if any of its groups is denied for the connection, and (when `group_required: true` in its profile) passes only
     if one of its groups is allowed.
4. **Link rule.** `follow_parent: true` (held items, attachments): relevant exactly when its `ChildOf` parent or `NetOwnerLink` target is
   relevant (denies on the entity itself still apply).
5. **Spatial interest.** For `relevance: Spatial { radius, hysteresis }` profiles: relevant when the entity is within `radius` of any of
   the connection's viewpoints; once relevant, it stays relevant until it is farther than `radius × (1 + hysteresis)`. `relevance: World`
   passes this stage everywhere.
6. **Custom filters.** A profile may name a registered custom filter (`app.add_net_relevance_filter::<F>("name")`); `F::relevant(&World,
   entity, connection) -> bool` runs per send tick for entities that reached it. Registering a filter name twice is a startup error; a
   profile that names an unregistered filter logs `net.filter_unknown` once and treats the filter as passing.
7. **Temporary grants.** `net_grant_relevance(entity, target, until_tick)` where `target` is a peer or a group: the entity is relevant to the
   target until the tick. Used for reveal effects: a scanned enemy becomes visible to the scanning team for 5 seconds, then returns to its
   normal relevance.

### 5.2 Gaining and losing relevance

- Gaining relevance queues a spawn record (or a bind, for level-bound entities) for the connection, subject to the spawn budget (§ 7).
- Losing relevance starts `linger_ticks` (profile, default 30). If the entity is still irrelevant when they run out, the server sends a
  despawn (runtime entities) or an out-of-scope notice (level-bound entities, spec 71 § 4.5) and drops the connection's baseline for it.
  Regaining relevance during the linger cancels it at no cost.
- Asleep entities (spec 71 § 8) are evaluated like any other entity; sleep only skips their change gathering.
- Viewpoints: `NetViewpoint` component (on a camera or character entity, with the `NetPlayer` it belongs to). If a player has none, its
  controlled character is used; if it has none of either, the player sees only relevant-everywhere, owned and granted entities.

### 5.3 Interest grid

- Entities with spatial relevance are bucketed into a uniform grid over the XZ plane (cell size `replication.grid_cell_m`, default 64 m).
  The grid is rebuilt incrementally: only entities whose cell changed move.
- For each connection, the cells overlapping each viewpoint's largest profile radius are visited, and their entities tested by exact
  distance. Cell lists are built once per send tick and shared by every connection; no per-entity, per-connection loop runs over the whole
  world.
- Entities with `relevance: World` or `sent_to_all` live in a separate list, not in the grid.

## 6. Priority

- Every relevant entity with unsent changes (its current version is newer than the connection's baseline, or it has a pending spawn)
  gains, each send tick: `acc += profile.priority × distance_factor × view_factor × owner_factor`.
  - `distance_factor = max(0.1, 1 − d / radius)` for spatial profiles (d = distance to the nearest viewpoint), `1.0` otherwise.
  - `view_factor = 1.0` if the entity is inside any viewpoint's 120° forward cone, else `0.5` (profile `view_falloff: false` → always 1.0).
  - `owner_factor = profile.owner_boost` (default 4.0) for entities owned by the connection, else 1.0.
- Entities with no unsent changes do not accumulate (an unchanged entity never takes bandwidth, however important).
- Candidates are sorted by `acc` (descending, ties by `NetId`) and packed until the budget is used (§ 7). Each sent entity gives up
  the tick's average share of the gained priority: `acc = max(acc − share, −share)`, where `share` is that gained priority divided by
  the number of records packed on this send tick (ADR-1662). Resetting the accumulator to 0 would drop the overshoot and skew the ratios.
  Unsent entities keep their accumulator, so every entity with changes is eventually sent: an entity with priority `p` is
  starved for a bounded time: with `n` competing entities whose effective priority (priority times factors) is at most `P`, and a
  budget that fits at least one entity per send tick, an entity of effective priority `p` is sent within `ceil(n × P / p) + 1` send
  ticks. Doubling every profile's priority changes nothing; only the ratios decide the order.
- `max_interval_ticks` entities that reach their limit are packed first, in `NetId` order.

## 7. Budgets and packet assembly

- **Bandwidth.** Each connection has a rate in bytes per second (`replication.client_rate_bytes`, default 48 KiB/s; the session may
  lower it per client from the client's declared limit, spec 77). The per-send-tick budget is `rate / send ticks per second`; unused
  budget carries over at most one send tick. The server may also set a total outbound cap (`server_rate_bytes`, default none); if the sum
  exceeds it, every connection's budget is scaled down by the same factor.
  As built (ADR-7213): the rate the client declared in its `Hello` lowers its connection's rate (never raises it);
  `net.set_client_rate` still wins in dev builds.
- **Packet size.** At most `mtu_bytes` (default 1,200) per packet; a send tick may emit several packets for one connection while budget
  remains.
- **Spawn budget.** At most `spawns_per_send` (default 24) spawn records per connection per send tick, highest priority first, so a client
  joining a busy area fills in over a few ticks instead of in one burst.
- **Assembly order** of one connection's packets in a send tick:
  1. Transport control and acknowledgments; input acknowledgment for spec 73 (latest input tick processed, server tick).
  2. Despawns and out-of-scope notices (small; they free client memory).
  3. Reliable messages waiting (spec 70); they are not budget-limited (the transport paces them), but their bytes are charged.
  4. Spawn records, by priority, up to the spawn budget.
  5. Entity deltas, by priority, until the budget is used.
  6. Unreliable messages, while budget remains; the rest stay queued (spec 70 § 6.5 bounds).
- `mtu_bytes` includes the transport's headers: a state frame stays `STATE_PACKET_RESERVE` (64) bytes under it.
- Despawns, out-of-scope notices, ownership changes and reliable messages travel on the reliable tick frame, sent on a send tick only when
  it has something to carry; spawn records and deltas travel in state frames on the unreliable channel. The client sorts a tick's state
  before its messages and despawns when both have arrived; a despawn is remembered for 20 s, so a state frame still on its way cannot bring
  the entity back, and a despawn older than a newer spawn of the same id is ignored (ADR-1950).
- An entity's state larger than one packet (a big inventory) is sent as one fragmented reliable record by the transport; at most one such
  record per connection is in flight. Entity state over 16 KiB is an error at gather time: the entity is not sent and the server logs
  `net.state_too_large` once per entity.
  As built (ADR-8419, spec 120 § 8.1): the 16 KiB cap applies to spawn and update records alike; a client that holds a copy of an
  entity whose record is over it gets a despawn (or, for a level entity, out-of-scope) notice instead of a frozen copy, and the copy
  comes back when the state fits. `net.state_too_large` is logged once per crossing of the cap. The large record's slot frees on its
  own acknowledgment (or four in-flight timeouts), and the in-flight timeout follows the measured round trip of state frames.
  Every record obeys the byte budget (spawns and forced records too), and an overspend is paid back in full, not one send tick.
- The packet's server tick and the per-entity version ticks are the only clocks on the wire; spec 74 builds the client's timeline from them.

## 8. Profiles: `.netprofile.ron`

```ron
NetProfile(
    format_version: 1,
    priority: 1.0,
    relevance: Spatial(radius: 150.0, hysteresis: 0.15),   // or World
    sent_to_all: false,
    owner_only: false,
    group_required: false,
    follow_parent: false,
    custom_filter: None,                // Some("line_of_sight")
    min_interval_ticks: 1,
    max_interval_ticks: None,
    linger_ticks: 30,
    owner_boost: 4.0,
    view_falloff: true,
    position_step_m: Some(0.001),       // quantize replicated positions to 1 mm; None = raw f32
    // owned by spec 74: interpolation: Linear, snap_distance_m: 5.0, max_extrapolate_ms: 100, appear_immediately: false
)
```

- Built-in profiles under `engine://net/profiles/`: `default` (above), `player` (priority 3.0, radius 400 m), `projectile` (priority 2.0,
  radius 200 m, linger 2), `prop` (priority 0.5, radius 100 m), `held` (`follow_parent: true`), `game_state` (`sent_to_all: true`,
  `min_interval_ticks: 6`). `config/net.ron` names the project default (`replication.default_profile`).
- Validation on load: `priority > 0`, `radius > 0`, `0 <= hysteresis <= 1`, `min_interval_ticks >= 1`, `max_interval_ticks >= min`,
  `owner_boost >= 1`, `position_step_m` in `[1e-5, 1.0]`; an invalid profile fails to load with a field-level message and entities using it
  fall back to `default` with an error log.
- A format page `docs/formats/netprofile-ron.md` and a JSON Schema (from reflection) ship with the builder. Migration note: new format, `format_version: 1`, unfrozen until v0.5.0, then registered in `check-formats` and `upgrade-project`.
  As built (ADR-7232): the built-ins are also `engine://net/profiles/<name>.netprofile.ron` text files of the asset server's `engine://`
  source; a project's `.netprofile.ron` files cook (validated, canonical text, cooked type `regia_net::NetProfile`) and ship in the pak;
  `NetPlugin` reads both at `Startup` and loads them into `NetProfiles` once it exists (`regia_net::profile_assets`).
  Profiles are gameplay content: every `.netprofile.ron` in the mounted content is covered by the content hash (spec 77 § 4.1), so a client
  and server with different quantization cannot join. They are not part of the protocol hash, which covers only code-registered types.

`config/net.ron` section `replication` (spec 70 § 7; spec 71's keys included):

```ron
replication: (
    send_every_ticks: 2,
    default_profile: "engine://net/profiles/default.netprofile.ron",
    client_rate_bytes: 49152,
    server_rate_bytes: 0,              // 0 = no total cap
    mtu_bytes: 1200,
    state_channel: Unreliable,         // Reliable = the tick stream for every connection (a diagnosis switch)
    spawns_per_send: 24,
    grid_cell_m: 64.0,
    android_listen_max_clients: 8,
    sleep_enabled: true,               // spec 71 § 8
    max_pending_refs: 4096,            // spec 71 § 5
    netvec_max: 4096,                  // spec 71 § 10
),
```

On a dedicated server, `server.toml`'s `[net]` keys (spec 76 § 4) override the same keys of `config/net.ron` at startup.
As built (ADR-5650, ADR-7213): only the keys the file names override; an unnamed key keeps `config/net.ron`'s value,
a reload applies the change (and a dropped key returns to `config/net.ron`'s value), and `Welcome` carries the
`send_every_ticks` the server really sends at.

## 9. Resync and error handling

- A client that cannot apply a delta (baseline missing, decode error on one entity) sends a resync request for that entity (an internal
  message, rate-limited to 10 per second per client). The server drops that connection's baseline for the entity and sends a full state
  next send tick. Three resyncs of the same entity within 5 seconds log `net.resync_loop` at warn with the entity's components.
- A connection that acknowledges nothing for `session.timeout_s` (spec 77 § 12) is disconnected; replication itself never blocks on a slow client.

## 10. Debugging

- Gizmo group `net_relevance` (off by default): the interest grid, each viewpoint's radius, and replicated entities colored by their state
  for a chosen peer (relevant, lingering, filtered out); asleep entities are outlined.
- Counters per connection: bytes per second by component type and by entity (top 32), entities relevant, spawns pending, budget used,
  starved entities (accumulator over 10× their priority), resyncs.

## 11. Determinism

- Relevance, priority and packing are pure functions of the server world, the connection state and the acknowledgments received. Packets
  for different connections are assembled in parallel (one task per connection, `bevy_tasks`), and each connection's bytes are identical to
  a serial build (no shared mutable state other than read-only versions and grid).
- Ties sort by `NetId`; floating-point priorities are computed in `f32` with the same operation order on every platform.

## 12. Agent protocol surface

Guide page: `book/agents/net-relevance.md` (topic `net-relevance`): the pipeline in § 5.1 as a checklist, profiles, budgets, how to read
`why_not_relevant`, and every error code.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.relevance` | read (server) | `{ peer, limit? }` | `[{ entity, net_id, state: relevant\|lingering, acc, last_sent_tick, baseline_tick }]` |
| `net.why_not_relevant` | read (server) | `{ peer, entity }` | `{ relevant: bool, stage: owner\|sent_to_all\|owner_only\|visibility\|group\|link\|spatial\|custom\|grant, detail }` |
| `net.bandwidth` | read | `{ peer?, by: component\|entity }` | bytes per second per key over the last 5 s, plus budget and usage |
| `net.set_client_rate` | play (server, dev builds) | `{ peer, bytes_per_second }` | `{ rate }` |
| `net.profiles` | read | — | loaded profiles with their users count |
| `net.grant_relevance` | play (server, dev builds) | `{ entity, peer \| group, ticks }` | `{ until_tick }` |

Errors (code `F0002` with `data.reason`, as in the neighboring built modules): `net_not_server`, `peer_unknown`, `entity_not_replicated`, `entity_not_found`, `profile_invalid`, `rate_out_of_range` (below
4 KiB/s or above 10 MiB/s).

## 13. Tests and acceptance

Loopback hub (spec 70 § 10), fixed steps, seeded conditioner.

1. **Delta against baseline.** One field changes at tick 10, the packet is lost; the field arrives in the next delivered packet without any
   resend logic; the wire carries only that field.
2. **Reorder.** Deltas for ticks 12 and 14 arrive in reverse order: the client ends with tick 14's state, and tick 12's is stored but not
   applied.
3. **Baseline ring.** A connection that acknowledges nothing for an entity for more versions than the ring holds (ring + 6: 70 with the original 64, 518 since ADR-10051) receives a full state, then deltas again.
4. **Spawn healing.** A lost spawn is repeated until acknowledged; the client never spawns twice.
5. **Relevance pipeline.** One test per stage of § 5.1, including: owner always relevant, even when denied; deny beats every grant; `owner_only` beats `sent_to_all`; a custom filter cannot add an
   entity excluded by distance; a link-followed weapon appears and disappears with its holder; `group_required` excludes an entity with no
   allowed group.
6. **Hysteresis and linger.** An entity crossing the radius back and forth within the hysteresis band produces no spawn/despawn; leaving
   for longer than the linger produces exactly one despawn; returning within the linger produces none.
7. **Temporary grant.** A grant makes an out-of-range entity relevant to the target peer for exactly N ticks, then normal rules resume.
8. **Interest grid parity.** For 2,000 entities and 8 viewpoints, the grid-based relevant set equals a brute-force all-pairs test on
   every tick of a 600-tick scripted walk.
9. **Priority fairness.** With a budget that fits 10 of 100 changing entities, every entity is sent within the starvation bound of § 6; a
   priority-3 entity is sent 3 times as often as a priority-1 entity (±10 %) over 6,000 ticks; an unchanged entity is never sent.
10. **Budgets.** Measured bytes per second stay within the rate (±1 packet); the spawn budget spreads 200 spawns over ⌈200/24⌉ send ticks; a
    17 KiB entity is not sent and logs once.
11. **Parallel equals serial.** Packet bytes for 16 connections assembled in parallel equal the serial assembly, byte for byte.
12. **Resync.** A corrupted delta for one entity triggers one resync and recovery; a forced loop logs `net.resync_loop`.
13. **Agent.** Every method round-trips; `why_not_relevant` names the right stage for each case of test 5; error codes each produced by a test.

## 14. Platform notes and performance budget

- Dedicated servers run on Linux and Windows (spec 76); a listen server on Android serves at most 8 clients and uses the same code with
  smaller default budgets (`replication.android_listen_max_clients: 8`).
- Server send budget, dedicated server on the Desktop reference class (`06-testing-and-ci.md` § 3: 8 cores, all used as workers): 64 connections, 20,000 replicated entities, 2,000 relevant per
  connection, 10 % changing per tick: relevance + priority + packing under 3 ms per send tick in total wall time (spans `net_relevance`,
  `net_priority`, `net_pack`). Target scale for the bench (not a gate): 100 connections and 50,000 entities under 8 ms.
- Memory: state versions ring at most 64 per entity (only changed ticks are stored, so a quiet entity holds one); per (connection, relevant
  entity) 24 bytes (baseline tick, accumulator, last-sent tick, linger), plus 8 bytes per (connection, replicated entity) for the slot indices that reach it (ADR-1950).

## 15. Out of scope

Client-to-server state replication (clients send input, spec 73, and messages, spec 70); line-of-sight or audibility relevance shipped
by the engine (games register a custom filter); per-component priority; adaptive send rate from measured client bandwidth (a later
congestion lane may adjust `client_rate_bytes` through the same API); compression beyond field deltas and quantization (a later
entropy-coding pass is a protocol version bump).

## 16. Provenance

- Set spec ids used: `runtime-networking-actor-replication`, `runtime-networking-replication-graph`, `runtime-networking-netrep` (behavior
  only, ADR-1526).
- Public sources: G. Fiedler, "Gaffer on Games": "Snapshot Compression" (delta against the last acknowledged snapshot, quantization),
  "State Synchronization" (priority accumulators, starvation-free sending), "Reliability and Congestion Avoidance over UDP" (ack bitfields);
  D. Aldridge, "I Shot You First: Networking the Gameplay of Halo: Reach", GDC 2011 (per-client priority and relevance under a bandwidth
  budget); Valve Developer Community wiki, multiplayer networking article (snapshot rate, delta compression against acknowledged
  snapshots). Uniform-grid spatial hashing: C. Ericson, *Real-Time Collision Detection* (2005), § 7.1.
- Regia design choices: the shared per-entity version ring, the stage order and its "only narrows" rule, the priority formula and its
  factors, the budgets and their defaults, the linger and hysteresis defaults, the resync rule.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 17. Owner decisions

None. Review notes (ADR-1585): profile flag names `sent_to_all` and `owner_only` and the stage name `sent_to_all` are Regia's own;
the version ring, the acknowledged-baseline delta and the starvation bound are original and derive from the public sources of § 16.
`bevy_tasks` parallel assembly is the only concurrency; no new dependency.

## 18. Entropy coding (protocol 3, ADR-3315)

§ 15 deferred "compression beyond field deltas and quantization" to a protocol version bump; this is it.
`NET_PROTOCOL_VERSION` 3 adds an entropy stage for state frames; `MIN_NET_PROTOCOL_VERSION` stays 1.

### Design

- **Coder.** An adaptive binary range coder (G. N. N. Martin, "Range encoding", 1979; the byte emitter and 11-bit
  adaptive probabilities of the public LZMA specification; `docs/provenance.md`) codes each frame's body (the record
  count plus the records) bit by bit against 64 per-frame adaptive models (8 bit positions × 8 contexts from the
  previous byte's high bits), adapting at 1/16 per occurrence. Written from the public descriptions; no code copied.
- **Per-packet models.** Encoder and decoder both start from the fixed initial probabilities for every frame. A lost,
  duplicated or reordered frame therefore decodes identically however many frames went missing or arrived out of
  order before it — no acknowledged baseline, resend or re-sync is involved (proved by the conditioner tests).
- **Raw fallback.** When the coded payload is not smaller than the plain body, the frame carries the body verbatim
  under the same negotiated tag (one flags byte of overhead).
- **Scope (the compression side channel).** Only replication state frames are compressed. The handshake,
  authentication, passwords, chat and every game-message payload travel uncompressed: attacker-chosen bytes never
  sit next to secrets in a compressed blob (the CRIME/BREACH class, ADR-3315). The stage runs before the link's
  encryption on authenticated plaintext, so a corrupted or truncated payload is a decode error on the existing
  malformed-frame path — never a panic (fuzz target `net_entropy_decode`).
- **Wire.** A negotiated connection receives `TAG_STATE_ENTROPY` (8) frames:
  `tag u8 | tick varint | seq varint | flags u8 |` then `count varint | record*` (flags 0) or
  `coded_len varint | coded bytes` (flags 1, the coded count+records with the decoded length in the payload's
  varint header). The header is unchanged, so acknowledgment and loss handling are untouched.
- **Negotiation.** The server enables the stage per connection when configured and the client's first `ACK` names
  protocol ≥ 3 — the same `ACK` that decides the state wire (§ 4.2), so no extra round trip. A protocol 2 peer never
  names 3 and receives byte-identical protocol 2 frames (pinned-bytes test). Config:
  `replication.entropy: Off | Range` in `config/net.ron` (default `Off`, additive serde default); a dedicated host's
  `server.toml` `[net] entropy = "off" | "range"` overrides the project for joins after startup/reload.
- **Reporting.** Per connection, the stage counts frames, coded/raw fallbacks and body bytes before/after
  (`net.entropy_stats`).

### Measurement (the § 13 harness, 300 send ticks per workload, fixed seeds)

| workload | frames | raw bytes | entropy bytes | frames coded | median saved/frame | p95 saved/frame | encode | decode |
|---|---|---|---|---|---|---|---|
| avatars (12) | 300 | 39,643 | 38,728 | 100% | 3 B | 6 B | 5.4 µs | 6.6 µs |
| entities64 | 300 | 148,398 | 137,652 | 100% | 36 B | 45 B | 19.9 µs | 24.7 µs |
| entities256 | 300 | 374,519 | 342,166 | 100% | 107 B | 127 B | 49.5 µs | 61.9 µs |
| level (512, sparse) | 300 | 127,348 | 119,076 | 100% | 28 B | 40 B | 16.9 µs | 21.0 µs |

Bits by field kind across the workloads (share of the raw body): quantized float fields 55-62 % (absolute positions
at 1 mm: stable high bits), record and frame headers (ids, lengths, version ticks) 26-35 %, changed-field masks
5-8 %, varints 1-2 %. The saving grows with frame size: small frames spend their bytes on model warmup. The
end-to-end integration test asserts the saving on the live send path (48 moving entities, conditioned loss,
duplication and reordering) under three conditioner profiles; the criterion bench `cargo bench -p regia_net --bench
entropy` records per-frame encode/decode cost.

### Tests (§ 13 style)

1. **Off is byte-identical.** A pinned-bytes test assembles protocol 2 frames and compares them with a hand-built
   frame; the off path is untouched code.
2. **Transparent decode.** An entropy frame unwraps to the identical plain frame.
3. **Loss/reorder/duplication.** Frames unwrap to the same bytes in any delivery order.
4. **Corruption.** Truncated, length-hostile and bad-flag payloads are decode errors, never panics (fuzz target
   `net_entropy_decode`).
5. **Negotiation.** Protocol 2/3 × server off/range decide the per-connection mode on the first `ACK` (unit test);
   v3↔v3 replicates correctly under the conditioner (integration test).
6. **Harness.** The table above; at least two workloads must show a saving.
