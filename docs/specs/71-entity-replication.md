# 71 — Entity replication

Status: proposed (ADR-1599). Plan item D3 (`master-plan.md` § 12), rung v0.5.0 (a first slice may land with the v0.3.x (b)
`regia_net` start). Crate: `regia_net::replication`, derive macro in `regia_macros` (like the existing `Script` and `BtTask` derives,
the expansion names `regia_net` by path and `regia_macros` does not depend on it). Builds on spec 70 (modes, peers, roles,
`NetId`, messages); spec 72 decides *which* connection gets *what* and *when*; this spec decides *what replicating an entity
means*.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: none of this exists. Reused: `regia_scene::components::EntityGuid`, `regia_asset::AssetRef`, `regia_core::id::Name`,
`ChildOf`, `Changed<C>`, the `.level.ron` / `.prefab.ron` reflected-component save path, and the `regia_macros` derive pattern.

Builders read this file and specs 70, 72-77, plus the ambient rule docs. Needed facts from elsewhere: `NetProfile` is the asset type
of spec 72 (`.netprofile.ron`); `PredictedSpawnKey` is spec 73's token tying a predicted spawn to the server's answer.

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-actor-replication`,
`runtime-networking/runtime-networking-netrep`, `runtime-networking/runtime-networking-overview`. Re-derived in Regia's ECS terms;
no wording, identifiers or settings carried over.

## 1. Purpose and scope

The server's world holds the truth. For every entity marked replicated, each client that should see it (spec 72) gets a copy: the
copy appears when the server entity appears (or becomes relevant), carries the replicated components' values, follows their changes,
and disappears when the server entity is despawned (or stops being relevant). Everything else (meshes, materials, animation graphs,
particles, sounds, physics bodies on proxies) is **not** replicated: each process builds it locally from replicated driving values.

In scope: the `Replicated` marker, registering components for replication, spawn and despawn, level-placed entities, hierarchy,
entity and asset references, per-component audiences, change notifications, sleep, collection components with
per-element deltas, the transform/velocity fast path and teleports, and the value codec.

Not in scope: per-connection relevance, priority, bandwidth and baselines (spec 72); prediction (73); interpolation (74); a
separate "part object" concept (Regia has none: a non-entity part of an entity is a component; a separately addressable part is a
child entity).

## 2. Terms

| Term | Meaning |
|---|---|
| `Replicated` | The component that opts an entity into replication. Absent = local only. |
| Replicated component | A component type registered with `app.replicate::<C>(..)`. Only those components of a replicated entity travel. |
| `NetId` | The server-assigned network id of a replicated entity (spec 70). Unique for the session, never reused. |
| Spawn record | What a client needs to create its copy: `NetId`, how to create it (prefab, level binding or bare), parent, the initial values of every replicated component that passes its condition, and the `PredictedSpawnKey` when the spawn answers a client's predicted spawn (spec 73 § 7). |
| Audience | A per-component rule deciding which connections receive that component (§ 6). |
| Asleep | A server-side state in which an entity is skipped by change gathering until it changes or is woken (§ 8). |
| Level-bound entity | A replicated entity that was placed in a level and exists on every process that loads the level, matched by `EntityGuid`. |

## 3. API shape

```rust
/// Opts an entity into replication. Reflected and saved in `.level.ron` / `.prefab.ron`.
#[derive(Component, Reflect, Default, Clone, Debug)]
#[reflect(Component, Default)]
pub struct Replicated {
    /// Relevance, priority and rate profile (`.netprofile.ron`, spec 72). `None` = project default profile.
    pub profile: Option<AssetRef<NetProfile>>,
    /// Transform fast path (§ 9). Default: `Movement` (position, rotation, velocity if present).
    pub transform: TransformReplication,
    /// Sleep policy (§ 8). Default: `Never`.
    pub sleep: NetSleep,
}

#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformReplication { None, PositionOnly, #[default] Movement, Full /* + scale */ }

#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
pub enum NetSleep {
    #[default]
    Never,
    /// Starts asleep once its spawn is acknowledged by every relevant connection; wakes on any replicated change.
    StartAsleep,
    /// Falls asleep after `quiet_ticks` ticks with no replicated change; wakes on any change.
    AfterQuiet { quiet_ticks: u32 },
}

/// Registration. Call once per component type, on every process, in the same order (it feeds the protocol hash).
impl ReplicateAppExt for App {
    fn replicate<C: Component + ReplicateCodec>(&mut self, opts: ReplicateOptions) -> &mut Self;
}

pub struct ReplicateOptions {
    pub audience: Audience,            // § 6, default Audience::All
    pub notify: NotifyWhen,           // § 7, default NotifyWhen::Changed
    pub keep_previous: bool,           // § 7, default false
    pub notify_on_server: bool,        // § 7, default false
}
```

`#[derive(Replicate)]` implements `ReplicateCodec` for a struct or enum: a field-wise encoder with one change bit per field, so a
change to one field sends that field only. Field attributes:

| Attribute | Effect |
|---|---|
| `#[replicate(skip)]` | Field never travels; the client keeps its own value (default on first spawn: `Default`). |
| `#[replicate(quantize(min = a, max = b, bits = n))]` | `f32` sent as an `n`-bit fixed-point value in `[a, b]`; out-of-range values clamp and count `quantize_clamped`. |
| `#[replicate(angle(bits = n))]` | `f32` radians wrapped to `[-π, π)` in `n` bits. |
| `#[replicate(unit_vec(bits = n))]` | `Vec3` of length 1 as two `n`-bit octahedral coordinates. |
| `#[replicate(rotation(bits = n))]` | `Quat` as smallest-three, `n` bits per component (default 10) plus 2 bits for the dropped index. |
| `#[replicate(max_len = n)]` | Upper bound for `String` / `Vec` fields (default 1,024 bytes / elements). |

Without attributes: `bool` 1 bit, integers zig-zag varints, `f32`/`f64` raw, `Vec2/3/4`, `Quat` raw floats, enums a varint tag plus
the variant's fields, `Option` a presence bit, `String` length-prefixed UTF-8, `Vec<T>` length-prefixed, `Entity` a `NetId` (§ 5),
`AssetRef<T>` its `AssetId`, `Handle<T>` the `AssetId` the server's asset server resolves it to (the client loads it through its own
asset server; a missing asset logs `net.asset_missing` once per id and leaves the handle unset). A `skip` field is kept on
the client through every update and full resend by `ReplicateCodec::keep_local` (struct fields; ADR-5648).

## 4. Lifecycle of a replicated entity

### 4.1 Server side

1. Gameplay spawns an entity with `Replicated` (or inserts `Replicated` on an existing entity) during a tick.
2. At the end of that tick (end of `FixedUpdate`, set `NetSet::Assign`, spec 70 § 8.1) `regia_net` assigns the next `NetId` (monotonic `u32`, starting at 1;
   on exhaustion the entity stays local and the server logs `net.netid_exhausted` at error), inserts `NetId` and `NetRole::Authority`,
   and records the spawn.
3. Spec 72 decides per connection when the spawn record goes out. The spawn record holds every replicated component the connection
   may see (§ 6), with full values.
4. Each later tick, changed replicated components are gathered (§ 4.3) and sent as deltas (spec 72).
5. Removing `Replicated` or despawning the entity records a despawn. Every connection that has the copy (or has its spawn in flight)
   receives the despawn. `Replicated` removed while the entity lives: the client copies are despawned; the server entity stays local.

Spawns made in one tick go out parents first (sorted by hierarchy depth, then `NetId`), so a client never sees a child before its
parent from the same tick.

### 4.2 Client side

1. On a spawn record: create the entity (§ 4.4), insert `NetId`, `NetRole` (spec 70 § 5.3), `NetOwnedLocally` if owned, every
   received component, `Transform` per § 9; then emit `NetSpawned { entity, net_id }`.
2. On a delta: write changed fields into the components, insert components that appear, remove components flagged removed;
   then emit change notices (§ 7).
3. On a despawn: emit `NetDespawning { entity }` (handlers may start a local effect on a new local entity), then despawn the copy
   and its non-replicated children. For interpolated proxies the despawn itself waits until the interpolation time reaches the
   despawn tick (spec 74 § 6); the entity carries `NetPendingDespawn` meanwhile.
4. The apply order within one server tick is fixed (spec 70 § 6.5): spawns, state, messages, despawns.

A client never spawns a replicated copy without a spawn record, and never keeps one after its despawn. Gameplay code on a client
that despawns a replicated copy itself gets a warning `net.client_despawned_proxy` and the copy is recreated from the next full
state the server sends (spec 72 § 9 resync).

### 4.3 What counts as a change

- A replicated component is a gather candidate in a tick when its ECS change tick moved (`Changed<C>`) or when it was added or
  removed. Candidates are then compared field by field with the last value sent (the baseline, spec 72); equal fields are not sent.
  Writing the same value therefore costs a compare, not bandwidth.
- Each `ReplicateCodec` compares quantized values: a float change smaller than its quantization step is not a change.

### 4.4 How the client creates the copy

| Spawn kind | When the server uses it | Client behavior |
|---|---|---|
| `Prefab(asset id)` | The server entity is a prefab instance (spec 14) | Instantiate the prefab locally (non-replicated parts come from the prefab), then apply replicated values on top. A prefab the client cannot load: spawn a bare entity, log `net.prefab_missing` once per asset, count it. As built (ADR-7213): without a game `NetPrefabSpawner` the engine spawner instantiates the prefab from the client's `PrefabLibrary`. |
| `Level { level, guid }` | The entity was placed in a loaded level | Bind to the client's own entity with that `EntityGuid` in that level (§ 4.5). |
| `Bare` | Anything else | Spawn an empty entity and insert the replicated components. |

### 4.5 Level-bound entities

- Level-placed entities with `Replicated` exist on every process that loads the level. The server does not send their creation; its
  spawn record binds by `(level id, EntityGuid)`.
- Before the bind arrives, the client's level copy is shown with its level-file state and role `Proxy`, and carries `NetUnbound`.
- If the server despawned a level entity before a client joined (a door blown off at minute 3), the join snapshot (spec 77 § 6.1)
  lists it under `level_removed`, and the client despawns its copy before the first `InGame` frame.
- A bind for a guid the client does not have is a content mismatch (logs `net.level_entity_missing`, spawns the copy as
  `Bare`) **unless** the guid is in the `replicated` table of a streamed level the client has open (spec 78 § 6.2,
  § 14; ADR-2317). Then the cell is simply not spawned on this client: the client keeps the bind (`(level, guid) ->
  NetId`, at most `ClientReplication::level_bind_cap`, default 16,384; over it the bind is dropped and counted), skips
  updates for it without a resync request, and when the cell spawns the entity asks for the full state once and binds as
  usual. A bound copy that a cell unload despawns is kept again the same way. A despawn notice for a kept bind ends it,
  and the entity is despawned whenever its cell spawns it (so is a guid listed under `level_removed` in the join
  snapshot). Counters: `level_entity_buffered`, `level_entity_buffer_applied`, `level_entity_buffer_dropped`,
  `level_entity_buffer_cleared`.
- Relevance loss never despawns a level-bound copy (it is level content): it keeps its last state and gains `NetOutOfScope` until it
  is relevant again, when it receives full current state. Games may hide out-of-scope copies; the engine does not.

### 4.6 Hierarchy

- `ChildOf` replicates when both parent and child are replicated: the parent is sent as a `NetId`. If the child is replicated and the
  parent is not, the client copy is a root entity, and the server warns `net.parent_not_replicated` once per entity.
- A non-replicated child of a replicated entity is local to each process (meshes, sockets, effects from the prefab).

## 5. Entity references

- An `Entity` field in a replicated component travels as the target's `NetId`. A target that is not replicated travels as "none",
  and the server warns `net.ref_not_replicated` once per component type (references are tracked per component, ADR-1643 (5),
  ADR-5648).
- On the client, an unknown `NetId` (target not yet spawned, or not relevant to this client) leaves the field at `Entity::PLACEHOLDER`
  and records a pending reference. When that `NetId` spawns, the field is patched and `NetRefResolved { entity, component }` is
  emitted. Pending references live until the referring entity despawns or the field changes again; at most 4,096 per client (oldest
  dropped, counted).
- A reference to a target that is despawned on the client becomes `Entity::PLACEHOLDER` again (pending), because the server may
  re-spawn it into relevance later under the same `NetId`.

## 6. Audiences

Each replicated component type has one `Audience`, evaluated per connection when its values are sent. The audience says who sees
this component's values; it is chosen by the role the receiving copy plays and by whether the receiver is the entity's owner.

```rust
pub enum Audience {
    /// Every connection the entity is relevant to.
    All,
    /// Only the owning connection.
    OwnerOnly,
    /// Every relevant connection except the owner.
    Others,
    /// The spawn record only; later changes are never sent.
    SpawnOnly,
    /// Everyone in the spawn record, and only the owner afterwards.
    SpawnThenOwner,
    /// Connections whose copy is a `Proxy` (never a predicting owner).
    ProxyCopies,
    /// Connections where one of the peer's players shares a name in `NetGroups` with the entity.
    SharedGroup,
    /// Everyone relevant while the predicate returns true. Evaluated once per entity that
    /// carries the component on every gather, not once per connection (ADR-5456 kept this over
    /// ADR-5328's change-driven cache on this line; ADR-5648).
    Predicate(fn(&World, Entity) -> bool),
}
```

- A component whose audience stops matching a connection is not removed from that client's copy; it just stops updating. An audience
  that starts matching sends the full current value once.
- Groups: `NetGroups(SmallVec<[Name; 4]>)` (`regia_core::id::Name`) on the replicated entity and on each `Player` entity (spec 70 § 4);
  a connection's membership is the union over its peer's players. This is the only group system: spec 72's group visibility reads the
  same components. Changing either re-evaluates at the next send.
- Audiences are per component type, not per field: a struct that needs two audiences is two components.
- An `OwnerOnly` component on a child entity reaches the connection that owns the child by the rule of spec 70 § 5.2.

## 7. Change notices

- After a client applies a tick's state, it emits `NetChanged<C> { entity, previous: Option<C> }` for each component of type `C` whose
  value differed from the client's copy (`NotifyWhen::Changed`), or for every received update of `C` even if equal
  (`NotifyWhen::EveryUpdate`). `NotifyWhen::Never` emits nothing. `previous` is `Some` only with `keep_previous: true` (cloning costs memory and time).
- Spawn records emit `NetChanged<C>` with `previous: None` for every component they carry, after `NetSpawned`.
- With `notify_on_server: true`, the server emits the same `NetChanged<C>` when its own copy of `C` changes on a replicated entity
  (from ECS change detection at the end of the tick). One handler system can then drive the reaction on server and clients alike
  (sounds, effects, UI); without the flag the server sees no notices of its own changes.
- Notices for different components of the same tick have no defined order between types; a reaction that needs two values together
  reads both components in its handler, or puts both values in one component.

## 8. Sleep

Sleep saves server time on entities that rarely change (props, doors, pickups waiting to be taken).

- An asleep entity is skipped by change gathering. Its copies stay on clients that have them; relevance is still evaluated for it
  every send tick (it stays in the interest grid, spec 72 § 5.3), so a player who walks up to a sleeping door receives it; only change
  gathering and field compares are skipped.
- **Entering.** `NetSleep::StartAsleep` entities fall asleep once every relevant connection acknowledged the spawn. `AfterQuiet { quiet_ticks }`
  entities fall asleep after that many ticks with no change. Gameplay can force it: insert `NetAsleep`. In all cases the entity keeps
  sending until every relevant connection has acknowledged its latest state, and only then stops.
- **Waking.** Any write to a replicated component of an asleep entity (ECS change detection) wakes it in that tick, and its change is
  sent. Gameplay can wake explicitly by removing `NetAsleep` or calling `net_wake(entity)`. Because the server keeps the
  acknowledged state of an asleep entity, **no change made while asleep is ever lost**: on wake, every field that differs from what the
  clients last acknowledged is sent.
- `net_resend(entity)`: send the current state once to every relevant connection without leaving sleep (it goes quiet again once
  acknowledged). A resend of a `StartAsleep` entity that has never woken behaves the same.
- `NetSettings.replication.sleep_enabled: false` disables sleep everywhere (a diagnosis switch); every entity then replicates as
  `Never`.
- Memory: the acknowledged state of an asleep entity is stored once (not per connection), because entering sleep waits for every
  connection to acknowledge the same state.

## 9. Transform fast path and teleports

The fast path counts as a registered replicated component named `Transform` for every rule that needs one: spec 73's `predict` and
spec 74's interpolation accept it, and audiences do not apply to it (it goes to every connection the entity is relevant to).

- `TransformReplication::Movement` sends world position (3 × `f32`, or quantized by the profile, spec 72), rotation (smallest-three,
  10 bits per component), and linear velocity if the entity has a `Velocity`/`CharacterState` velocity (3 × `f32`, used by spec 74
  for extrapolation). `PositionOnly` sends position. `Full` adds scale when it is not `Vec3::ONE`.
- The server sends world-space transforms for root entities and parent-relative transforms for replicated children.
- **Teleport.** `net_teleport(entity)` on the server marks the next transform update with a teleport bit. Proxies snap to it instead of
  smoothing (spec 74) and predicting owners reset their correction smoothing (spec 73). The bit is sent reliably (it is part of the
  state, so a lost packet resends it with the next delta).
- Proxies do not run physics bodies for replicated transforms: `regia_net` makes their rigid bodies kinematic on clients and writes the
  replicated pose (rapier bodies in `regia_physics` already support kinematic position targets).

## 10. Collections: `NetVec<T>`

For inventories, buff lists and scoreboards, where sending a whole `Vec` on every change is wasteful:

- `NetVec<T: ReplicateCodec>` is a component-friendly list whose elements have server-assigned stable ids (`u32`, never reused within
  the list). It replicates as adds, removes and per-element field changes.
- Clients get `NetVecEvent { entity, kind: Added|Changed|Removed, id }` per element, after the tick's state is applied, read with
  `netvec_events::<T>(world)` (per-tick resources, ADR-1643 (7)); a tick's events are removes, then adds, then changes, each in id
  order (ADR-5648).
- Element order on the client is the server's order at the tick applied.
- Limits: 4,096 elements; adding more returns `Err(NetVecFull)` on the server.

## 11. Files and formats

- `Replicated`, `NetGroups` and `NetAsleep` are reflected components saved inside `.level.ron` / `.prefab.ron`. Adding them is additive:
  levels and prefabs keep their `format_version`; older files without them load unchanged (they are simply not replicated). Once frozen
  (v0.5.0), any shape change of these components is a version bump of the component with a migrator in `regia upgrade-project`.
- A format page `docs/formats/net-ron.md` documents `config/net.ron` (the builder of spec 70 creates it). Section `replication` (spec 70 § 7): `sleep_enabled: true`, `max_pending_refs: 4096`, `netvec_max: 4096`.
- Wire: spawn record, delta and despawn layouts are internal to `NET_PROTOCOL_VERSION`; the protocol hash covers the registered
  component list, their order, each audience's variant (for `Predicate`, the variant only, not the function) and every field's codec
  (so two builds with a different quantization cannot join).

## 12. Determinism

- `NetId` assignment within a tick follows the entity id order (`Entity::to_bits`) of the entities that gained `Replicated` in that
  tick, which is a pure function of the world's history: deterministic under `enable_fixed_step` with the same inputs (ADR-5648; the
  bit order of the ECS's entity ids, which is not the command order).
- Client apply is deterministic given the packets: spawn order (depth, then `NetId`), component order (registration order), field order
  (declaration order), notice order (component registration order, then `NetId`).
- Quantization is integer arithmetic after one `f32` multiply-and-round per value, identical on every platform (`f32::round` and IEEE
  multiply are exact-rounding operations; no fused multiply-add is used in the codec).

## 13. Agent protocol surface

Guide page: `book/agents/net-replication.md` (topic `net-replication`): marking entities, registering components, audiences table, change
notices, sleep rules, references, teleports, and every error code. `book/COVERAGE.md` rows for each method.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.replicated` | read | `{ filter?: { component?, asleep?, owner? }, limit? }` | `[{ entity, net_id, prefab, components, asleep, owner (server) }]` |
| `net.entity_state` | read | `{ entity }` | replicated component values as last applied (client) or last sent (server), with `tick` |
| `net.wake` | play (server) | `{ entity }` | `{ asleep: false }` |
| `net.set_asleep` | play (server) | `{ entity }` | `{ asleep: true, pending_acks }` |
| `net.resend` | play (server) | `{ entity }` | `{ queued_to: [peer] }` |
| `net.pending_refs` | read (client) | — | `[{ entity, component, field, waiting_for_net_id, since_tick }]` |

Errors (code `F0002`, `data.reason`, as in the neighboring built modules): `entity_not_replicated`, `entity_not_found`, `net_not_server`, `net_not_running`, `component_not_replicated`.

## 14. Tests and acceptance

Loopback hub as in spec 70 § 10; all fixed-step.

1. **Opt-in.** An entity without `Replicated` never appears on a client; with it, appears at the first send after its spawn tick with all
   `All`-audience components; despawn on the server despawns every client copy.
2. **Prefab copy.** A prefab instance spawns on the client with its non-replicated children and meshes from the prefab and its replicated
   values on top; a missing prefab spawns bare and logs once.
3. **Field deltas.** Changing one field of a 6-field component sends that field only (measured as bits on the wire); writing the same
   value sends nothing.
4. **Quantization.** A value within half a step does not count as a change; out-of-range values clamp and count; encode/decode round-trips
   are bit-identical on Linux, Windows and Android (a golden table of 1,000 inputs and expected wire bytes, no images).
5. **Conditions.** One test per `Audience` variant of § 6, with an owner and two other clients, including `ProxyCopies` versus a predicting owner and
   `SharedGroup` membership change mid-session.
6. **Change notices.** `Changed` fires once per real change, `EveryUpdate` on every update, `Never` never; `previous` holds the old value with
   `keep_previous`; `notify_on_server` fires the same handler on the server.
7. **References.** A component pointing at an entity spawned 5 ticks later resolves and emits `NetRefResolved`; a reference to a
   non-replicated entity arrives as none and warns once; a target that leaves relevance turns the field into a pending reference.
8. **Hierarchy.** Parent and child spawned in one tick arrive parent first; a replicated child of a local parent arrives as a root and
   warns once.
9. **Level-bound.** Bind by guid works; a level entity removed before join is despawned on the joining client before its first `InGame`
   frame; a missing guid spawns bare and logs; relevance loss keeps the copy with `NetOutOfScope`, and regaining relevance sends full state.
10. **Sleep.** `StartAsleep` entities send nothing after the acknowledged spawn; a write while asleep wakes and delivers (no lost edits);
    `AfterQuiet` falls asleep after the configured quiet ticks; `net_resend` delivers once and stays asleep; a late-joining client receives an
    asleep entity's current state; the global switch disables sleep.
11. **NetVec.** Add, change and remove events in order; element ids stable across reorder; 4,097th add errors.
12. **Teleport.** A teleport bit survives 50 % loss (it arrives with the next delivered delta) and is applied once.
13. **Protocol hash.** Two apps that register the same components in a different order, or with a different quantization, produce different
    hashes (the join refusal is tested in spec 77).
14. **Agent.** Every § 13 method round-trips; each error code is produced by a test; `cargo xtask agent-coverage` passes.

## 15. Platform notes and performance budget

- Same code on Linux, Windows, Android aarch64 (and macOS for testing). The codec avoids platform-dependent float paths (§ 12).
- Server, Desktop reference class (`06-testing-and-ci.md` § 3): change gathering for 10,000 replicated entities with 5 registered components each and 10 % changed per tick
  under 0.4 ms (span `net_gather`); asleep entities cost one change-tick check per component. As built (ADR-7213): the
  despawn check visits only entities whose `Replicated` left since the last gather (`ServerReplication::despawn_checks`). Field compare and encode are counted in
  spec 72's send budget.
- Client, Android reference class (`06` § 3; measured on the owner's Pixel 6): applying 500 entity deltas per tick under 0.5 ms (span `net_apply`), spawning 100 prefab copies
  in one tick under 4 ms (spread by spec 72's spawn budget).
- Memory: per replicated entity on the server, 16 bytes of bookkeeping plus one baseline per connection (spec 72); per asleep entity one
  acknowledged state.

## 16. Out of scope

Replicating arbitrary non-registered components through reflection at runtime (every replicated type is registered; reflection-based
encoding may back the derive for prototypes but is not a separate path); per-field audiences; client-spawned replicated
entities (clients ask the server with a message). Replay recording has its own spec: `docs/specs/103-net-replay.md` (ADR-3021), a virtual
non-owner connection; the local replay system stays separate.

## 17. Provenance

- Set spec ids used: `runtime-networking-actor-replication`, `runtime-networking-netrep`, `runtime-networking-overview` (behavior only,
  ADR-1526).
- Public sources: G. Fiedler, "Gaffer on Games": "State Synchronization" and "Snapshot Compression" (quantization, smallest-three
  rotations, changed-field bits, per-object state per client); Z. H. Cigolle et al., "A Survey of Efficient Representations for Independent Unit Vectors", JCGT 2014
  (octahedral unit vectors).
- Review note (ADR-1585): the audience variants, the sleep policy names and the notify names are Regia's own; the behaviors (owner,
  non-owner, spawn-only, group, predicate; sleep with resend; notify on change or on every update) are common to replication systems
  in general.
- Regia design choices: the no-lost-edits sleep rule (§ 8), auto-wake on change, `notify_on_server`, level-bound out-of-scope handling,
  the apply order, the pending-reference cap.

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
- <<public engine documentation>>
