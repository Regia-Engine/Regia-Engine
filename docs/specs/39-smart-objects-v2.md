# 39 — Smart objects v2: shared definitions, filtered search, claim priorities, change messages

Status: proposed (ADR-1578). Plan item G8, target v0.4.0. Crates: `regia_ai` (`smart_object`), `regia_agent`.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: spec 31 / ADR-1507 is **implemented** on main (`regia_ai::smart_object`, `book/agents/smart-objects.md`,
rows in `book/COVERAGE.md`); this spec records the gaps and extends it additively. Gameplay tags are implemented
(ADR-1463, `regia_framework::tags`).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-gameplay-smart-objects` (a glance at
`runtime-gameplay-tags` for query behavior; Regia's own tag system is already built). Re-derived; no wording,
list order or name ladders kept.

## 1. Baseline (spec 31, implemented) and gaps

Built today (`crates/regia_ai/src/smart_object.rs`): `SmartObject { tags: Vec<String>, enabled }`;
`SmartObjectSlot { role: String, enabled, claimed_by: Option<Entity> (reflect-ignored) }`; `SmartObjectIndex`
(resource, linear list ordered by `(role, the slot's spawn-order arrival)` (ADR-4651), rebuilt each
`PostUpdate` after transform propagation and refreshed on every fixed step (before `AiStep`) from the local
transforms, so a claim on any step of a frame reads that step's pose, not a frame-old snapshot;
`slots/slot/for_role/available/nearest_available/object_of`); `SlotInfo`; `claim_slot(world, slot, agent) ->
Result<(), SmartObjectError>` and `release_slot` (errors `SlotNotFound`, `SlotDisabled`, `SlotClaimed(slot, holder)`,
`NotClaimedBy`); `smart_object_ancestor`, `effective_enabled`; `SmartObjectPlugin`; gizmo group `smart_object`;
agent `smartobject.list/claim/release`.

Gaps this spec closes: every slot is hand-built (no shared definition); search is role + nearest only (no activity
tags, user tags, preconditions, behavior families, no reason for a rejection); a claim is all-or-nothing and the
holder is never told it lost the slot; no generation-checked handle; no entrance points; the index is linear.
Everything in spec 31 keeps working; additions are new defaulted fields, new types and new methods.

## 2. Decisions (ADR-1585)

- Tags use `regia_framework::tags` (`TagContainer`, `TagQuery`). `regia_ai` and `regia_framework` sit in the same
  layer (Gameplay) and neither depends on the other today: add the edge `("regia_ai", "regia_framework")` to the
  allowed list in `xtask/src/layers.rs` (with a one-line comment) and run `cargo xtask check-layers`.
- `regia_ai` gains a dependency on `regia_asset` (lower layer, allowed) to load `SmartObjectDef`.
- The existing `SmartObject.tags: Vec<String>` is untouched (free-form classification). Typed matching uses new
  `TagContainer` fields.
- Notifications are ECS messages (`Message`, as `TagsChanged` is), not trait-object delegates.
- Claim priority has five levels, not a long ladder. Entrance preference is a plain `i8`.
- Slot state is derived, never stored twice.

## 3. Definition asset `.smartobject.ron` (v1)

```ron
SmartObjectDef(
  format_version: 1,
  activity: ["Activity.Sit"],                       // tags offered by every slot
  activity_merge: Union,                            // Union | SlotFirst (§ 5.2)
  user_rule: DefinitionAndSlot,                     // DefinitionAndSlot | SlotOnly | SlotElseDefinition (§ 5.3)
  user_query: Some(AnyOf([Has("Role.Civilian"), Has("Role.Guard")])),
  conditions: [ (path: "my_game::ChairIntact", params: {}) ],
  behaviors: [ (family: "guard_sit", data: "(pose: \"alert\")") ],   // defaults for every slot
  slots: [
    (name: "left", offset: (translation: (-0.4, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0)),
     activity: [], initial_tags: [], enabled: true, user_query: None, conditions: [],
     behaviors: [ (family: "civilian_sit", data: "(pose: \"relaxed\")") ],
     entrances: [ (offset: (translation: (-0.4, 0.0, 1.0), rotation: (0.0, 0.0, 0.0, 1.0)), kind: Both, rank: 0) ]),
    (name: "right", offset: (translation: (0.4, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0))),
  ],
)
```

- Tag and query types are the `regia_framework::tags` serde forms. A tag not declared in the project tag table is
  dropped with a warning at load (`tag_unknown`); the load continues.
- `behaviors` pairs a consumer-defined family name with a RON payload (kept as text, decoded on demand, § 7). A slot
  inherits the definition's behaviors; for a family present on both, the slot's entry wins.
- `conditions` name registered `SlotCondition` types (§ 6) with a string parameter map.
- `slots[].name` unique per definition, `[A-Za-z0-9_]{1,32}`; at most 64 slots; `entrances` at most 8 per slot;
  violations are load errors (`bad_definition` + detail). `offset` is a local `Transform` (translation, rotation as
  xyzw quaternion, normalized at load).
- As built (ADR-2012): `regia cook` validates a definition and stores its canonical RON text, which gives it the id
  `SmartObject.definition` names; `regia_ai::smart_object_load` resolves the id and reads the file, else the pak entry.
- `format_version: 1` (new format, no migrator). Add to `cargo xtask check-formats` as `smart-object-def` and a
  no-op step list for `regia upgrade-project`; lead runs `cargo xtask check-formats --update` and `cargo xtask
  manual`. A newer version is refused (`unsupported_version`). Document in `docs/formats/smartobject-ron.md`.

## 4. Components, handles, state

Additions to spec 31's components (all new fields default; existing scenes load unchanged):

- `SmartObject.definition: Option<AssetRef<SmartObjectDef>>` (persisted) and `runtime_tags: TagContainer`
  (`#[reflect(ignore)]`, starts from none).
- `SmartObjectSlot`: `def_slot: Option<u16>` (persisted; set on slots materialized from a definition),
  `activity: TagContainer` (persisted, for hand-built slots), `user_query: Option<TagQuery>` (persisted), and the
  transient, `#[reflect(ignore)]` fields `priority_held: ClaimPriority`, `in_use: bool`, `generation: u32`,
  `runtime_tags: TagContainer`, `off_reasons: BTreeSet<String>`. Existing `claimed_by` keeps its meaning.
  Host-visible: both structs gain fields; code building them by literal must use `..Default::default()` (release
  note, *Upgrade*).
- `SlotState { Free, Claimed, InUse, Disabled }`, derived: `Disabled` if the slot, its object or any
  `off_reasons` says so (existing `effective_enabled` plus reasons); else `InUse` if `in_use`; else `Claimed` if
  `claimed_by` is set; else `Free`.
- `ClaimPriority { Background = 0, Low = 1, Normal = 2, High = 3, Urgent = 4 }` (default `Normal`), `Ord`.
- `ClaimHandle { slot: Entity, generation: u32 }`. `generation` increments whenever the slot is released or
  invalidated, so an old handle never matches. `to_bits()`/`from_bits()` pack it into a `u64` for a blackboard
  `Int` key (no new `BbValue` variant).
- **Materialization.** `smart_object_materialize` (system, `PostUpdate`, before the index rebuild): when a
  `SmartObject` with a loaded `definition` appears or its asset reloads, spawn one child per definition slot with
  `SmartObjectSlot { role: <slot name>, def_slot: Some(i), .. }` and a `Transform` from `offset`, named
  `<object>/slot_<name>`; on reload reconcile by `def_slot` (survivors keep entity id and claim state; removed
  slots despawn after invalidating holders; new slots spawn). A hand-edited child of a materialized slot is not
  reconciled.
- `SlotEntrance` (component on a child of a slot, or generated from `entrances`): `kind: EntranceKind
  (Enter|Exit|Both)`, `rank: i8` (higher is preferred).

## 5. Search

```rust
pub struct SlotQuery {
    pub region: QueryRegion,               // Sphere { center, radius } | Aabb { min, max }
    pub activity: Option<TagQuery>,        // tested against the slot's effective activity set (§ 5.2)
    pub user_tags: TagContainer,           // the requester's tags (§ 5.3)
    pub families: Vec<String>,             // non-empty: the slot must offer one of these behavior families
    pub priority: ClaimPriority,           // the requester's, default Normal
    pub allow_preempt: bool,               // also list Claimed slots held at a LOWER priority
    pub include_disabled: bool,            // default false
    pub check_conditions: bool,            // default true
    pub limit: Option<usize>,
}
impl SmartObjectIndex {
    pub fn search(&self, world: &World, q: &SlotQuery, log: Option<&mut RejectionLog>) -> Vec<SlotHit>;
    pub fn find_first(&self, world: &World, q: &SlotQuery) -> Option<SlotHit>;
    pub fn availability(&self, world: &World, slot: Entity, priority: ClaimPriority) -> Availability;
}
pub struct SlotHit { pub slot: Entity, pub object: Option<Entity>, pub distance: f32, pub status: HitStatus }
pub enum HitStatus { Free, Preemptable { holder: Entity } }
pub enum Availability { Free, Preemptable, Unavailable(UnavailableWhy) }   // Disabled | HeldHigher | InUse | NotFound
```

- Results sort by `(distance, the slot's order in the index)` ascending, never by the entity id. An exact
  distance tie keeps the slot's index order (role, then the slot's spawn-order arrival) on every path — the
  linear scan, the grid (which visits cell by cell in cell-coordinate order, position-derived, and within a
  cell in index order) and the logged walk over the whole list — so a search returns the same hits with or
  without a rejection log. Distance is to the slot's world position; slots
  with non-finite positions are skipped. `limit` applies after sorting.
- A `Claimed` slot is a hit only with `allow_preempt` and `priority` strictly above the holder's; an `InUse` slot
  is never a hit; a `Disabled` slot only with `include_disabled`.
- `availability` answers whether a claim at `priority` could succeed. It ignores conditions and tag filters.
- **5.1 Stages.** Per candidate, in this order, first failure wins and is the recorded rejection: `region`,
  `enabled`, `state`, `family`, `activity`, `user`, `condition` (object-level conditions, then slot-level).
  `RejectionLog { entries: Vec<Rejection> }` with `Rejection { object, slot: Option<Entity>, stage, detail }`; when
  an object-level check rejects every slot of the object identically, a single `slot: None` entry is logged. With
  no log passed the path allocates nothing beyond the result vector.
- **5.2 Activity set.** `Union`: definition's and slot's activity tags together. `SlotFirst`: the slot's own tags if
  it has any, else the definition's. Runtime tags (§ 8) always join the set after merging. A `None` query passes.
  Matching is `TagContainer::matches`.
- **5.3 User rule.** `DefinitionAndSlot`: the definition's `user_query` and the slot's both must pass. `SlotOnly`:
  only the slot's. `SlotElseDefinition`: the slot's if present, otherwise the definition's. An absent query passes.
  The tags tested are `SlotQuery::user_tags`.

## 6. Claiming, life cycle, entrances

```rust
pub fn claim_with(world: &mut World, slot: Entity, user: Entity, priority: ClaimPriority)
    -> Result<ClaimHandle, SmartObjectError>;
pub fn begin_use(world: &mut World, handle: ClaimHandle) -> Result<(), SmartObjectError>;
pub fn release_handle(world: &mut World, handle: ClaimHandle) -> Result<(), SmartObjectError>;
pub fn slot_transform(world: &World, slot: Entity) -> Option<GlobalTransform>;
pub fn entrance_for(world: &World, slot: Entity, from: Vec3, kind: EntranceKind, nav: Option<NavProbe>) -> Option<Vec3>;
pub trait SlotCondition: Send + Sync + 'static { fn test(&self, ctx: &ConditionCtx<'_>) -> bool; } // world, object, slot, user
```

`SmartObjectError` gains `StaleHandle`, `NotHolder` and `SlotInUse(Entity)`; losing a slot to a preempting claim is a message
(`SlotLost`), not an error. `claim_slot`/`release_slot` stay and delegate with `Normal`; their signatures and results do not change.

- `Free -> Claimed`: `claim_with`. A claim on a `Claimed` slot succeeds only at a strictly higher priority than the
  holder's; the old holder receives `SlotLost { reason: Preempted }` and its handle goes stale. Equal or lower:
  `SlotClaimed`. `InUse` or `Disabled`: `SlotInUse`/`SlotDisabled`. The same user claiming again gets the same
  handle and may raise the held priority.
- `Claimed -> InUse`: `begin_use`. `Claimed|InUse -> Free`: `release_handle`. A stale or foreign handle is
  `StaleHandle`/`NotHolder`; never a panic.
- **Automatic release**, in the same `PostUpdate` pass that detects it and before the index rebuild: holder
  despawned (`HolderGone`), slot or object despawned (`SlotGone`), slot or object becomes `Disabled` (`Disabled`),
  definition reload removed the slot (`SlotRemoved`). Each clears the claim, bumps `generation`, and writes one
  `SlotLost`.
- `entrance_for` picks the highest `rank` among entrances matching `kind` (`Both` matches either), ties by distance to
  `from`, then entity bits. With `NavProbe { extents }` and `regia_nav` present the point is projected onto the
  navigation mesh within `extents`; an entrance that cannot be projected is skipped. Without a probe the raw point.
- Conditions run only in `search` with `check_conditions`. An unregistered path counts as failing with a one-time
  warning (`condition_not_registered`). Registered by type path in the `SmartObjectRegistry` resource.

## 7. Behavior payloads

`slot_behavior::<T: Clone + 'static>(world, slot, family) -> Option<T>` decodes the RON text for `family`
into the serde type the consumer registered with `SmartObjectRegistry::register_behavior::<T>(family)` (`register_behavior` needs `DeserializeOwned + Clone + Send + Sync + 'static`), and
caches it per slot. An unknown family or a decode failure returns `None` and logs once per slot and family.

## 8. Tags, enabling, messages

- `add_runtime_tag`/`remove_runtime_tag(world, target, tag)` on an object or slot; each returns whether anything
  changed. `set_enabled(world, target, enabled, reason)`: each target keeps a set of off-reasons; effective
  `enabled` is true only when the set is empty (default reason `"gameplay"`).
- Messages (ECS `Message`s, written in `PostUpdate`, ordered by object entity bits, then slot order):
  `SlotChanged { object: Option<Entity>, slot: Option<Entity>, what: SlotChange, tag: Option<Tag> }` with
  `SlotChange { Claimed, InUse, Released, TagAdded, TagRemoved, Enabled, Disabled, Custom }` (only when the
  effective value changed; `slot: None` is object scope; `Custom` is sent by `notify_slot(slot, tag)`), and
  `SlotLost { handle: ClaimHandle, reason: LostReason }` with `LostReason { Preempted, Disabled, HolderGone,
  SlotGone, SlotRemoved }`. A holder reacting to `SlotLost` cleans up its own task state; the claim is already cleared.

## 9. Index scale

Up to 256 slots the linear index stays. Above, `SmartObjectIndex` keeps a uniform grid (cell size 8 m,
`cell_size` field) over slot world positions, rebuilt only when a slot's `GlobalTransform` changed or slots were
added or removed; `search` visits only overlapping cells. The result must equal the linear scan (tested).
No API change.

## 10. Agent protocol (`book/agents/smart-objects.md` extended; one `book/COVERAGE.md` row per new method)

`smartobject.list/claim/release` keep their shapes. `claim` gains optional `priority` (name), `use` (bool) and
returns `handle` as `{slot, generation}`; `release` also accepts `handle`. New:

| Method | Class | Params | Result |
|---|---|---|---|
| `smartobject.search` | read | `region`, `activity?` (tag query RON), `user_tags?`, `families?`, `priority?`, `allow_preempt?`, `include_disabled?`, `check_conditions?`, `limit?`, `explain?` | `{ hits: [{slot, object, distance, status, holder?}], rejections? }` |
| `smartobject.begin_use` | play | `handle` | `{ state }` |
| `smartobject.set_enabled` | play | `entity`, `enabled`, `reason?` | `{ enabled }` |
| `smartobject.tags` | play | `entity`, `add?`, `remove?` | `{ tags }` |
| `smartobject.messages` | read | `last?` (<= 256) | recent `SlotChanged`/`SlotLost` records |
| `smartobject.set_definition` | edit_scene | `entity`, `path` or `id` | `{ definition, path, loaded, cooked, error }` (ADR-2012) |

Errors are `F0002` with `data.reason`, following the existing smart-object methods: `slot_not_found`,
`slot_claimed`, `slot_in_use`, `slot_disabled`, `stale_handle`, `not_holder`, `entity_not_found`, `bad_region`,
`bad_priority`, `bad_query`, `tag_unknown`, `unsupported_version`. Without the plugin: empty lists, `slot_not_found`
otherwise. The gizmo group `smart_object` gains a ring per `Claimed` slot colored by priority, a filled ring for
`InUse`, and entrance arrows. The page gains the definition format, a search example with `explain`, and the reason list.

## 11. Tests and acceptance (tests first)

`crates/regia_ai/tests/smart_object_v2.rs` and the agent crate; fixed worlds, no sleeps, seeded random scenes,
temp files via `regia_test_utils::unique_temp_dir`.

1. **Merging and user rule.** Tables over `activity_merge` and `user_rule` x definition/slot tags and queries;
   absent queries pass; runtime tags join the set.
2. **Stages.** A slot failing two stages records only the first; an object-level failure logs one `slot: None`
   entry; results are identical with and without a log.
3. **Priorities.** Normal claim; a Low claim fails (`SlotClaimed`); a High claim preempts, the old handle's
   `release_handle` is `StaleHandle`, the old holder got `SlotLost{Preempted}`; `InUse` cannot be preempted;
   same-user re-claim is idempotent and can raise priority; `allow_preempt` lists the slot as `Preemptable` only for a
   higher requester; `availability` matches.
4. **Life cycle.** Free -> Claimed -> InUse -> Free; every automatic release (holder despawn, slot despawn, object
   disabled, definition reload dropping a slot) emits exactly one `SlotLost` with the right reason; two off-reasons
   need two enables.
5. **Materialization.** Two-slot definition spawns two children with the right transforms; reload adds/removes by
   `def_slot` and survivors keep entity ids and claims; slot behavior overrides the definition's for the same family.
6. **Conditions and behaviors.** Evaluated only on request; unregistered path rejects with one warning;
   `slot_behavior` decode, cache, and failure.
7. **Index equivalence.** 50 seeded scenes up to 5,000 slots: grid search equals linear scan; a moved or removed slot
   shows next frame.
8. **Entrances.** Highest rank wins, then distance, then entity bits; `Both` matches either kind; nav projection
   and an unreachable entrance skipped (uses a tiny test navmesh from `regia_nav`'s tests).
9. **Messages.** A scripted scenario yields an exact, ordered message vector.
10. **Format.** Round trip; each load error; a v2 file refused; a fixture so `check-formats` has a sample.
11. **Agent (rule 8).** Each method: success, missing parameter, each reason; spec 31's tests pass unchanged; a
    dogfood script claims with `priority`, gets preempted, and reads `smartobject.messages`.

Acceptance: `cargo test -p regia_ai -p regia_agent`; `cargo xtask check-formats`; `cargo xtask agent-coverage`;
`cargo xtask check-layers` (new `regia_ai -> regia_framework` edge); spec 31 acceptance still holds.

## 12. Budgets, platforms

Desktop core, recorded in `docs/progress.md` as counters and timings (informational, not asserted): no-change index
rebuild with 10,000 slots under 0.05 ms; one `search` over 10,000 slots hitting about 20 under 0.2 ms; full rebuild
after all moved under 2 ms; materializing 1,000 two-slot objects under 8 ms. Tests assert algorithmic facts (cells
visited, no allocation without a log), not time. Spans: `smart_object_index`, `smart_object_search`,
`smart_object_materialize`. Platform-independent; Android aarch64 builds unchanged; no randomness, no wall clock.

As built (ADR-2012): a frame where nothing the index reads changed skips the rebuild. `Changed` filters watch the slots
(`SmartObjectSlot`, `GlobalTransform`) and the objects, and observers mark the index stale for a removal and for a parent link
set or cleared anywhere, because a filter on `ChildOf` would walk every child in the world each frame. A host's write to the
resource (`cell_size`) also rebuilds. The fixed-step refresh keeps its own watch: any entity's local `Transform` change
(physics moves parents, not slots), a slot/object component change, and the same observer events on its own stale
flag. Positions always resolve through the local-`Transform` chain (`GlobalTransform` only for an entity or ancestor
without one). `SmartObjectIndex::rebuilds()` counts rebuilds from either schedule; `smartobject.list` reports it as
`index_rebuilds`.

### As built: refinements recorded by the conformance audit (ADR-5636)

The code legitimately refines the text above in these places (ADR-1638 unless named):

- `cargo xtask check-formats` and `regia upgrade-project` name the format `smartobject` (not `smart-object-def`); the
  fuzz/format corpus fixture is `crates/regia_agent/tests/fixtures/format-corpus/smartobject/`.
- `entrance_for(world, slot, from, kind, nav: Option<(&NavQuery, Vec3)>)`: the navigation query and its half-extents
  replace `NavProbe` (§ 6).
- `SlotLost` also carries `holder: Entity` (§ 8). Messages are ordered by object entity bits, then **slot entity bits**,
  arrival order within one slot (§ 8 "slot order").
- `SlotQuery` has an extra `user: Option<Entity>`: a slot the requester already holds counts as free for it (§ 5).
- `SmartObject` also gains the transient `off_reasons` (§ 8 needs it on objects too); `SmartObjectSlot::user_query` is
  `#[reflect(ignore)]` (`TagQuery` has no reflection), so a hand-built slot's requester rule is not saved in scenes.
- `release_handle_as(world, handle, user)` gives `NotHolder`; `release_handle` alone never does. The agent's
  `smartobject.release` with `handle` and `agent` uses it (reason `not_holder`).
- Agent reasons beyond § 10: `missing_param` (a required parameter is absent), `definition_not_found` and `bad_id`
  (`set_definition`). `smartobject.claim` keeps the spec 31 shape: a refused claim answers `claimed: false` with
  `reason` `claimed` / `in_use` / `disabled`, so `slot_claimed`, `slot_in_use` are produced only where a library error is
  mapped. `bad_definition` / `unsupported_version` come back in `set_definition`'s `error` object (and
  `smartobject.list`'s `definition_error`), not as `F0002`. `smartobject.search` with `explain` also returns
  `rejections_total` (the list is capped at 256 entries).

## 13. Provenance

Spatial hashing: Teschner et al., "Optimized spatial hashing for collision detection of deformable objects" (2003)
(uniform grid is textbook). Smart objects that advertise interaction: Kallmann and Thalmann, "Direct 3D interaction with smart objects" (1999).
Shared row: `docs/provenance.md`, "AI / smart objects v2 (spec 39)"; original code. The set area above was read for behavior only.

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

## 14. Out of scope

Streaming-aware registration (needs layer/streaming work, spec 37). Network authority for claims. Per-slot animation
state machines (consumers use behavior payloads). Viewport gizmo placement of definition slots (editor lane). A
state-tree or behavior-tree task wrapping search + claim + use (game or template code; the handle's `Int` packing is
the blackboard pattern). Weighted or random choice among hits. More than five priority levels.

### Owner decisions

None.

### As built: the definition editor (ADR-7736)

The editor half of § 14's "viewport gizmo placement of definition slots" is built in `regia_editor`: a Smart Object
Definition panel (`smart_object_def_panel`) edits a `.smartobject.ron` as forms with one undo step per change, shows
`SmartObjectDef::validate`'s message (Save requires a valid definition), warns before a save drops comments, and
writes `SmartObjectDef::to_ron`; Content ▸ New ▸ Smart object creates one. `smart_object_handles` places the open
definition's slots and entrances in the viewport (grab points and yaw dials) on the panel's preview entity or a level
object whose `definition` names the file. Agent: `editor.smart_object_def_state`, `editor.smart_object_def_edit`,
`editor.smart_object_slot_handles`, `editor.smart_object_slot_handle_drag` (`book/agents/smart-object-editor.md`). The
format is unchanged.
