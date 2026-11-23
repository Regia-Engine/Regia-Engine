# Net replication (agents)

Entity replication in `regia_net` (spec 71, ADR-1643). Guide topic `net-replication`. The server's world holds the
truth; every entity with `Replicated` gets a copy on each client it is relevant to. The copy appears with a spawn
record, follows field deltas of the registered components, and disappears with a despawn. Meshes, materials,
animation graphs, particles and sounds are not replicated: each process builds them from replicated driving values.

## Marking entities and registering components

- `Replicated { profile, transform, sleep }` opts an entity in (reflected, saved in `.level.ron` / `.prefab.ron`).
  `profile` names a `.netprofile.ron` (spec 72); `transform` is `none`, `position_only`, `movement` (default:
  position, smallest-three rotation, velocity from `NetVelocity`) or `full` (plus scale); `sleep` is `never`,
  `start_asleep` or `after_quiet { quiet_ticks }`.
- `app.replicate::<C>(ReplicateOptions { audience, notify, keep_previous, notify_on_server })` registers a component,
  on every process, in the same order (the order is part of the protocol hash).
- `#[derive(Replicate)]` writes the codec: one change bit per field, so a change to one field sends that field only.
  Field attributes: `skip`, `quantize(min, max, bits)`, `angle(bits)`, `unit_vec(bits)`, `rotation(bits)`
  (default 10), `max_len = n` (default 1,024). A float change smaller than its quantization step is no change.
  A `skip` field never travels: a client copy starts with `Default` and then keeps its own value through every
  update and full resend (struct fields; `ReplicateCodec::keep_local`, ADR-5648).
- Client copies are created by kind: a **prefab** instance is instantiated locally through the host's
  `NetPrefabSpawner` hook, or without one by the engine spawner from the client's `PrefabLibrary`
  (`engine_prefab_spawn`, ADR-7213; missing prefab: a bare entity, `net.prefab_missing` once per asset); a **level-placed**
  entity binds to the client's own entity with the same `(LevelMember, EntityGuid)` (shown with `NetUnbound` and role
  `proxy` until the bind arrives; a missing guid logs `net.level_entity_missing` and spawns bare); anything else
  spawns **bare**. In a streamed level a guid can be missing only because its cell has not spawned here yet: see
  "Level entities in streamed cells" below.
- Hierarchy: `ChildOf` travels when parent and child are both replicated; parents arrive before children of the same
  tick. A replicated child of a local parent arrives as a root (`net.parent_not_replicated` once per entity).

## Level entities in streamed cells

A client of a streamed level (spec 78 § 14) holds only the cells around its own sources, while the server holds the
union of every player's. `regia_world` lists each streamed level's replicated entities by guid when the level opens
(the world index's `replicated` table, as the `regia_scene::streaming::StreamedReplicated` resource), and the client
treats a bind for a listed guid differently from a content mismatch:

- **Kept.** A bind whose entity is not spawned yet is kept (net id per `(level, guid)`), and no copy is made. Updates
  for it are skipped without a resync request. Counter `level_entity_buffered`.
- **Applied.** When the cell spawns the entity, the client asks the server for that entity's full state once (the
  ordinary resync), and the spawn record binds the cell's entity with the server's latest state. Until then the entity
  shows its cooked state with `NetUnbound` and role `proxy`, as any unbound level entity. Counter
  `level_entity_buffer_applied`.
- **Cell unload.** A bound copy that its cell unloads goes back to being kept: no `net.client_despawned_proxy`
  warning and no resync. The next load binds it again with the latest state.
- **Removed on the server.** A despawn notice for a kept bind ends it, and the entity is despawned whenever its cell
  spawns it (also for entities removed before the client joined). Relevance loss only ends the bind.
- **Bounded.** At most `ClientReplication::level_bind_cap` binds are kept (default
  `MAX_BUFFERED_LEVEL_BINDS` = 16,384). A bind over the cap is not kept, makes no copy, logs
  `net.level_bind_buffer_full` once and counts `level_entity_buffer_dropped`.
- **Ended.** A closed level, a despawn, relevance loss or a new session ends kept binds: `level_entity_buffer_cleared`.

A guid in no table is still a mismatch: `net.level_entity_missing` and a bare copy. A client with no streamed level
behaves as before. The counters are fields of `ClientReplication::counters`; `ClientReplication::buffered_level_binds`
lists what is kept as `(level, guid, net_id)`.

```rust,wrap
# use regia_ecs::prelude::World;
# let world = World::new();
// A test or tool: what does this client wait for?
let client = world.resource::<regia_net::replication::ClientReplication>();
for (level, guid, id) in client.buffered_level_binds() {
    println!("level {level:032x} guid {guid:032x} waits as net id {}", id.0);
}
assert_eq!(client.counters.level_entity_missing, 0);
```

## Audiences

| Audience | Who receives the component |
|---|---|
| `all` | every connection the entity is relevant to |
| `owner_only` | only the owning connection |
| `others` | every relevant connection except the owner |
| `spawn_only` | the spawn record only; later changes are never sent |
| `spawn_then_owner` | everyone in the spawn record, only the owner afterwards |
| `proxy_copies` | connections whose copy is a proxy (never a predicting owner) |
| `shared_group` | connections where one of the peer's players shares a name in `NetGroups` with the entity |
| `predicate` | everyone relevant while the predicate returns true (run for every entity that carries the component on every gather; ADR-5456 kept this over ADR-5328's cache) |

A component whose audience stops matching is not removed from the copy; it stops updating. One that starts matching
sends its full value once.

## Notices, references, sleep, teleports

- Change notices `NetChangedEvents<C>`: `changed` (default) fires when the value differs from the copy,
  `every_update` on every received update, `never` never; `previous` is filled with `keep_previous`. With
  `notify_on_server` the server gets the same notices for its own changes. Spawns emit `NetSpawned` and a notice per
  component; despawns `NetDespawning` (in `NetEvents`).
- Entity references travel as `NetId`s. An unknown target leaves the field at `Entity::PLACEHOLDER` and records a
  pending reference (`net.pending_refs`, at most `replication.max_pending_refs`, default 4,096; the oldest is dropped
  and counted in `pending_refs_dropped`); when the target spawns the referrer is sent again in full,
  the field is patched and `NetRefResolved` is emitted. A reference to a non-replicated entity travels as none and
  logs `net.ref_not_replicated` once per component type.
- Sleep: an asleep entity is skipped by change gathering but stays relevant. It falls asleep only after every
  connection acknowledged its latest state, so **no change made while asleep is lost**: any write wakes it and sends
  every field that differs from what the connections hold. `net_resend` sends the current state once and stays
  asleep; a late joiner gets the current state. `NetSettings.replication.sleep_enabled: false` disables sleep.
- `net_teleport(entity)` marks the next transform update with the teleport bit (part of the state, so a lost packet
  resends it); proxies snap and `NetTeleported` is inserted on the copy.
- Position grid: a replicated root's wire translation (after the floating-origin shift) is snapped to its profile's
  `position_step_m` (default 0.001 m; `None` sends it raw), so a client's copy sits on that grid and motion under half a
  step is not sent (ADR-2413 (4), ADR-2665). A child of a replicated parent travels relative and unsnapped. A child that
  loses its replicated parent is re-read at once and sent as a root in the shared frame.
- `NetVec<T>`: a list with stable element ids; it replicates adds, removes and per-element deltas, and
  `netvec_events::<T>` lists the element events of a tick as removes, then adds, then changes, each in id order.
  At most 4,096 elements (`NetVecFull`).

## Methods

| Method | Mode | Params | Result |
|---|---|---|---|
| `net.replicated` | read | `{ filter?: { component?, asleep?, owner? }, limit? }` | `{ entities: [{ entity, net_id, prefab, components, asleep, owner (server) }] }` |
| `net.entity_state` | read | `{ entity }` | `{ net_id, tick, components: { name: value } }` (client: as last applied; server: now) |
| `net.wake` | play (server) | `{ entity }` | `{ asleep: false }` |
| `net.set_asleep` | play (server) | `{ entity }` | `{ asleep: true, pending_acks }` |
| `net.resend` | play (server) | `{ entity }` | `{ queued_to: [peer] }` |
| `net.pending_refs` | read (client) | — | `{ pending: [{ entity, component, field, waiting_for_net_id, since_tick }] }` |

```json
{ "method": "net.replicated", "params": { "filter": { "component": "Health" }, "limit": 10 } }
→ { "entities": [{ "entity": 4294967310, "net_id": 3, "prefab": null, "components": ["Health", "Stats"],
    "asleep": false, "owner": 1 }] }

{ "method": "net.set_asleep", "params": { "entity": 4294967310 } }
→ { "asleep": true, "pending_acks": 1 }
```

`prefab` is the prefab asset id as 32 hex digits, `level:<guid>` for a level-bound entity, or null. `field` in
`net.pending_refs` is null: references are tracked per component.

On a client, `prefab` and `asleep` are read from the copy. `PrefabInstance` is the 32-hex asset id.
`LevelMember` together with `EntityGuid` is `level:<guid>`. `NetAsleep` sets `asleep`. A copy with
none of those is `prefab` null and `asleep` false. `owner` is null on a client, so `filter.owner`
only matches on the server.

`net.entity_state` on a client uses the state-channel clock. The tick is the newest state frame
that carried this entity, or the client's state tick when that entity has no stored state record.
On the state channel that state tick is the newest state frame; otherwise it is the newest applied
tick. Each component's text is the newest received value while the copy still holds `NetReceived`,
and the live component otherwise. The server reports the live values and the tick of that entity's
last change.

## Errors

`F0002` with `data.reason`: `entity_not_replicated`, `entity_not_found`, `net_not_server`, `net_not_running`,
`component_not_replicated` (a `filter.component` that is not registered).

## Settings

`config/net.ron` section `replication` (format: `docs/formats/net-ron.md`): `sleep_enabled` (true),
`max_pending_refs` (4,096), `netvec_max` (4,096; reserved, ADR-1892: the push cap is the constant `NETVEC_MAX`).
