# Net relevance, priority and budgets (agents)

Which replicated entities each client holds, which of them are updated first, and how many bytes a tick may use
(`regia_net::relevance`, `regia_net::send`, `regia_net::profile`; spec 72, ADR-1662). Guide topic `net-relevance`.

The pipeline is **opt-in**: call `regia_net::install_net_interest(world)` on the server world. Without it every
replicated entity is relevant to every connection and nothing is budgeted (spec 71 behavior; the older
`NetRelevance { filter }` seam still narrows it).

## The pipeline, as a checklist

For a connection and an entity, the first rule that decides wins (highest first):

1. **Owner rule.** Owned by the connection's peer: always relevant. Nothing overrides it. Stage `owner`.
2. **Denies.** `NetVisibility { deny }` names the peer (`visibility`), or one of the entity's `NetGroups` is in the
   connection's `NetGroupVisibility.deny` (`group`): not relevant. Denies beat every grant below.
3. **Owner-exclusive.** Profile `owner_only: true`: relevant to the owner only (`owner_only`). A profile with both
   `owner_only` and `sent_to_all` logs `net.profile_conflict` and is owner-exclusive.
4. **Grants**, which skip the remaining filters, in this order: `sent_to_all` (`sent_to_all`; only to connections
   whose session is `InGame`: a loading peer gets "not relevant" at this stage, a peer without a session counts as in
   game; ADR-7213); a temporary grant from
   `net_grant_relevance` / `net.grant_relevance` to the peer or to a group its players carry in `NetGroups` (`grant`);
   `follow_parent` (the link rule: relevant exactly when its replicated parent or `NetOwnerLink` target is, `link`;
   chains of followers are resolved).
5. **Filters**, which can only remove: `NetVisibility.allow` (if non-empty, only those peers; `visibility`),
   `group_required` (one of the entity's groups must be allowed; `group`), spatial interest (`spatial`), then the
   profile's custom filter (`custom`).

Spatial interest: `relevance: Spatial(radius, hysteresis)` is relevant within `radius` metres (XZ and Y, exact
distance) of any of the connection's viewpoints; once relevant it stays relevant until it is farther than
`radius × (1 + hysteresis)`. `relevance: World` passes everywhere. Viewpoints are `NetViewpoint { player }` entities
(camera or character, forward = local −Z); a player without one uses its predicted owned entities; a player with
neither sees only owned, `sent_to_all`, world-relevant passing the filters, and granted entities.

Losing relevance starts the profile's `linger_ticks` (default 30): the copy stays and keeps updating; if it is still
irrelevant when they run out, a runtime entity is despawned (a level-bound one goes out of scope). Regaining
relevance during the linger costs nothing.

Custom filters: `app.add_net_relevance_filter::<F>("name")` with `F: NetRelevanceFilter`
(`fn relevant(&World, Entity, PeerId) -> bool`). A name registered twice is refused (`net.filter_duplicate`, error);
a profile naming an unregistered filter logs `net.filter_unknown` once and passes.

Filters the engine ships:

| Name | From | Removes |
|---|---|---|
| `fog` | `regia_framework::fog_relevance` (`FogRelevancePlugin`, feature `net`) | enemies the connection's fog team cannot see, with linger and lead grace in ticks; see [Fog of war](fog-of-war.md) § "Fog over the network". Its profile (`fog`) uses world relevance and `linger_ticks: 0`. |

The interest grid (cell `replication.grid_cell_m`, default 64 m) is shared by every connection and updated
incrementally; each connection evaluates only its candidates (owned, world list, the cells around its viewpoints,
grants, link followers and what it already holds).

## Profiles

`.netprofile.ron` (`format_version: 1`; `docs/formats/netprofile-ron.md`). An entity names its profile in
`Replicated.profile` (or, at runtime, `NetProfileRef`); without one it uses `replication.default_profile`. Built-ins
(`engine://net/profiles/<name>.netprofile.ron` or the short name): `default` (priority 1, spatial 150 m, hysteresis
0.15, linger 30), `player` (3.0, 400 m), `projectile` (2.0, 200 m, linger 2), `prop` (0.5, 100 m), `held`
(`follow_parent`), `game_state` (`sent_to_all`, `min_interval_ticks: 6`). An invalid profile fails to load with a
field-level message (`net.profile_invalid`) and its users fall back to the default (`net.profile_fallback`).
`net.profiles` lists them with their users and the failures. A profile's `extrapolation_gate_m` (`null`: off, the
default) is the [extrapolation gate](net-extrapolation-gate.md) of spec 120 § 8.2.

## Priority and budgets

- Every relevant entity with unsent changes (or a pending spawn) gains, each tick,
  `priority × distance_factor × view_factor × owner_factor` (`max(0.1, 1 − d/radius)`, 1.0 or 0.5 outside every
  viewpoint's 120° cone unless `view_falloff: false`, `owner_boost` when owned). Unchanged entities never gain or go.
- Candidates go in accumulator order (ties by `NetId`); `max_interval_ticks` entities that reached their limit go
  first; `min_interval_ticks` holds an entity back. A sent entity gives up the tick's average share of the gained
  priority (ADR-1662), so the send rates follow the priority ratios.
- Byte budget per send tick: `client_rate_bytes / send_hz`, with `send_hz = tick_hz / send_every_ticks`
  (default send every 2 ticks, so 49152/30 = 1638 bytes at 60 Hz before carry). `net.bandwidth`'s `budget`
  is that last send tick's counter. A rate the client declared in its session `Hello` lowers its connection's
  rate (never raises it; `net.bandwidth` `declared_rate`, ADR-7213); `net.set_client_rate` still wins.
  The total is scaled down when `server_rate_bytes` caps it; one send tick
  of unused budget carries over and an overspend is paid back. Message bytes are charged to the next
  tick. Spawn records: at most `spawns_per_send` (default 24) per send tick. A deferred delta is encoded again next tick
  against what the client holds, so nothing is lost.
- Entity state over 16 KiB is not sent: `net.state_too_large` once per entity (`ServerReplication.counters`).
- Resync requests: 10 per second per client; three for one entity within 5 s log `net.resync_loop` (warn) with the
  entity's components.

## Scale, distance tiers and combat heat (spec 120 § 8.3-8.4, ADR-8031)

- Grid bands: entities sit in one grid band per spatial `radius`; a viewpoint queries each band with that band's
  radius (`net.bandwidth` `interest.band_radii_m`). `interest.grid_marked` counts the candidates the queries marked,
  `interest.evaluations` the verdicts computed, `interest.held_kept` the held entities whose verdict stood because
  neither they nor the connection's viewpoints moved (a lingering, granted, listed or non-plain entity is always
  evaluated again).
- Spawns, despawns and ownership changes no longer force a full pass: a new entity resolves its own owner, a despawn
  forces a full owner resolve only when a live `NetOwnerLink` named it, and the copies whose owner flipped are the
  ones the gather visits (`ServerReplication::copies_visited`). Spawn candidates per connection are its newly
  relevant entities plus those still owed a spawn, not its whole held set.
- `rate_tiers` (profile v2): below `near_m` every send, below `mid_m` every `mid_every`-th send, beyond at `far_hz`
  (whole send intervals). Owned entities, spawns and forced records never wait. `net.bandwidth` per peer:
  `tier_held` (updates held back at the last send tick) and `tier_sent { near, mid, far }` (sent at it; an entity
  whose profile has no tiers counts as near). `net.profiles` returns `rate_tiers` (null when unset).
- Combat heat: `net_note_combat(world, entity, amount)` (server) raises `NetCombatHeat`; within `radius_m` (default
  50 m) of a viewpoint the gain is multiplied by `1 + heat × (1 − d / radius_m)`; the heat halves every 2 s and is
  clamped to 10. Read an entity's effect through `net.relevance`'s `acc`.
- Targeted broadcasts check each peer's copy directly, and the per-target unreliable cap (16 queued messages per
  entity per peer) is a counter: queuing cost no longer grows with the queue.

## Methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.relevance` | read (server) | `{ peer, limit? }` | `{ peer, pipeline, entities: [{ entity, net_id, state: relevant\|lingering, acc, last_sent_tick, baseline_tick }] }` |
| `net.why_not_relevant` | read (server) | `{ peer, entity }` | `{ relevant, stage, detail, distance_m, lingering }` |
| `net.bandwidth` | read (server) | `{ peer?, by: component\|entity }` | `{ pipeline, peers: [{ peer, bytes_per_second, rate, declared_rate, budget, budget_used, relevant, spawns_pending, deferred, starved, resyncs, tier_held, tier_sent: { near, mid, far }, by, keys }], interest: { evaluations, grid_marked, held_kept, band_radii_m } }` |
| `net.set_client_rate` | play (server, dev builds) | `{ peer, bytes_per_second }` | `{ rate }` |
| `net.profiles` | read | `{ name? }` | `{ profiles: [{ name, users, default, ...fields }], failed: [{ name, error }] }` |
| `net.grant_relevance` | play (server, dev builds) | `{ entity, peer \| group, ticks }` | `{ until_tick }` |
| `net.relevance_gizmos` | play (server) | `{ visible?, peer?, limit? }` | `{ visible, peer, pipeline, cell_m, radius_m, viewpoints: [{ position, forward }], counts: { relevant, lingering, filtered_out }, entities: [{ entity, net_id, state, asleep }] }` |

How to read `net.why_not_relevant`: `stage` is the rule that decided (the list above); `detail` says why (for
`spatial`, the distance and the limit in use, which includes the hysteresis while the entity is held). `relevant`
is the pipeline's answer this tick; `lingering: true` means the client still holds the copy while it lingers.
`baseline_tick` in `net.relevance` is the state the client acknowledged for its copy: on the state channel the tick of
its baseline version (`null` until the spawn is acknowledged), on the reliable stream the newest acknowledged tick up to
`last_sent_tick` ([net state channel](net-state-channel.md)). `starved` in `net.bandwidth` counts entities whose
accumulator exceeds 10× their priority.

Without `install_net_interest`, `net.relevance` lists every held copy as `relevant` with `pipeline: false`,
`net.why_not_relevant` answers relevant with that detail, and `net.bandwidth` returns `pipeline: false`.

## Errors

`F0002` with `data.reason`:

| Reason | When |
|---|---|
| `net_not_running` | the app has no `regia_net` runtime |
| `net_not_server` | a server method on a client |
| `peer_unknown` | no connected peer has that id |
| `entity_not_found` | the entity does not exist |
| `entity_not_replicated` | the entity has no `NetId` (or the pipeline has not seen it yet) |
| `profile_invalid` | `net.profiles { name }` names a profile that failed to load or is not loaded |
| `rate_out_of_range` | `bytes_per_second` below 4 KiB/s or above 10 MiB/s |
| `interest_off` | `net.set_client_rate` / `net.grant_relevance` on a server without `install_net_interest` |
| `net_dev_only` | `net.set_client_rate` or `net.grant_relevance` in a shipping build (spec 72: development builds) |
| `net_relevance_gizmos_unavailable` | `net.relevance_gizmos { visible }` in an app without the `net_relevance` gizmo group (a dedicated server, no renderer); the rows still come back without `visible` |

## The `net_relevance` gizmo group

`regia::DefaultPlugins` (with the `net` feature) registers the gizmo group `net_relevance` (ADR-2665), off by default.
On a server or listen host with a renderer it draws, for one connection:

- each replicated entity as a small box: green relevant, amber lingering, red filtered out; an asleep entity gets a
  second, larger white box;
- each of the connection's viewpoints as a cross, with a circle of the project default profile's radius on the ground;
- the interest grid's cells around each viewpoint (at most 32 a side).

`net.relevance_gizmos { visible: true, peer: 3 }` shows it for peer 3 (default: the connected peer with the lowest id);
`visible: false` hides it; with no parameter it changes nothing and returns the rows, so an agent can check the same
states on a dedicated server, which has nothing to draw on. `debug.set_gizmos { group: "net_relevance" }` also
toggles it. To verify a profile: show the group, `seeing.screenshot`, and compare `counts` with what the boxes show.

## Not in this build

- Cost (`cargo run --release -p regia_net --example net_relevance_bench`): 20,000 entities, 64 connections, ~2,000
  relevant each, 10 % moving, 8 workers: about 5 ms per send tick on a shared build machine, 2.8 ms of it in the three
  spans spec 72 § 14 counts when its cores are free, and 1.2 to 1.4 ms of 2 on a free MacBook M5. The spec's 3 ms is for
  an idle 8-core desktop and has not been measured on one (ADR-1950). Add `acked walk` after the worker count to move
  the viewpoints too.
- Scale case (spec 120 § 8.4, ADR-8031): `-- 20000 128 300 49152 8 acked walk n214` (128 connections, mixed profiles
  with a 1 km radar profile and distance tiers, 64 projectile spawns per tick, walking viewpoints). On an idle
  MacBook M5: relevance + priority + packing 4.11 ms mean, 4.3 ms p99, 35,392 allocations per send tick (the base
  before ADR-8031: 5.21 ms, 5.4-5.5 ms, 39,898). Budgets: `net_relevance_n214_*` in `docs/perf/budgets.ron`.
  Allocation counts do not depend on the host: Linux gives the Mac's numbers at every commit measured (ADR-8885). The
  bench fills every connection's view before it measures (it prints `fill: N ticks`; about 77 since spawns take the
  byte test), then measures the steady load: 28,302 allocations per send tick, p99 30,501, at v0.6.1.
