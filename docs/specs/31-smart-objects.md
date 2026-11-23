# Spec 31: Smart objects

Smart objects let AI agents (and, by the same API, scripts and the player) discover, claim and use
world objects generically: a chair to sit on, a terminal to operate, a door to open, a cover point to
hide behind. An object advertises **interaction slots** — placed, rotated points with a *role*, a
*facing*, an enabled flag and a claim state — and a queryable index maps them for agents. No bespoke
per-object code.

Status: proposed (see `docs/decisions.md` ADR-1507). This is a v0.4.0-feature (specs/22 § T2.2 AI)
brought forward as a self-contained slice: the runtime types, index, claim/release API, debug overlay
and agent surface land now; behavior-tree leaf tasks that *use* slots are game code and stay out of
scope.

Built status (conformance audit 2026-11-10, ADR-5632): everything below is implemented in
`regia_ai::smart_object` and `regia_agent::smart_object`, extended by spec 39 (smart objects v2,
ADR-1638: priorities, handles, filtered search, definitions, entrances, a region grid) and
ADR-2012 (change-driven index rebuilds). Where those changed a statement here, the text says so.

## 1. Model

- **`SmartObject`** (component): a marker + metadata on an ordinary world entity (usually with a
  `Transform` and meshes). Fields: `tags: Vec<String>` (free-form classification), `enabled: bool`.
  Disabling the object disables every slot under it.
- **`SmartObjectSlot`** (component): one interaction point, authored as a **child** entity so it
  inherits the object's transform and can be placed/rotated in the editor. Fields: `role: String`
  (the affordance), `enabled: bool`, and `claimed_by: Option<Entity>` — transient runtime state that
  is `#[reflect(ignore)]` (never persisted or reflected).
- The slot's world pose is its `GlobalTransform`; its **facing** is its local −Z axis in world space
  (the same forward convention as cameras, lights and characters).

A slot with no `SmartObject` ancestor is still a valid, indexable slot (it just cannot be disabled
through an object). The `SmartObject`/`SmartObjectSlot` split mirrors the engine's noun-components
convention and keeps slots placed exactly where the artist put them, under a parent that can be
animated or disabled as one.

## 2. Runtime API (`regia_ai::smart_object`)

- **`SmartObjectIndex`** (resource, not reflected): kept once per frame in `PostUpdate`, after
  `PostUpdateSet::TransformPropagate`, and refreshed on every fixed step before `AiStep` from the
  local transforms (ADR-4651), by `SmartObjectPlugin`. A frame or step where nothing the index
  reads changed does not rebuild it (spec 39 § 12, ADR-2012; `rebuilds()` counts the rebuilds). It
  is a linear list ordered by `(role, the slot's spawn-order arrival)`; the role queries and
  `nearest_available` scan it. A linear index is correct and cheap for the hundreds of slots a
  level holds. Above `GRID_MIN_SLOTS` (256) slots the index also keeps a uniform grid
  (`cell_size`, default 8 m) that spec 39's region searches use (`candidates`, ADR-1638): this is
  the spatial follow-up the acceptance section foresaw, behind the same API, with no format change.
  A slot needs a `GlobalTransform` to be indexed; a slot on a parked layer is left out (ADR-2396).
  - `len` / `is_empty` / `slots` / `slot(entity)` / `for_role(role)` / `available(role)` /
    `nearest_available(position, role)`.
- **`SlotInfo`** (borrowed view): `entity`, `object`, `role`, `position`, `forward`, `enabled`,
  `claimed`, `available()`.
- **`claim_slot(world, slot, agent) -> Result<(), SmartObjectError>`** and
  **`release_slot(world, slot, agent)`**: checked, idempotent claim coordination stored on the slot.
  Errors: `SlotNotFound`, `SlotDisabled`, `SlotClaimed`, `NotClaimedBy` (spec 39 adds
  `StaleHandle`, `NotHolder` and `SlotInUse` for its handle and use API; `claim_slot` is
  `claim_with` at `ClaimPriority::Normal`).
- **`smart_object_ancestor(world, entity)`** / **`effective_enabled(world, entity)`**: ancestor and
  availability helpers for scripts and leaf tasks.

## 3. Editor

Slots and objects are plain reflected components, so the inspector edits them today (tags, role,
enabled). The editor's Smart Objects panel (ADR-2030, `editor.smart_object_panel_*`) adds live debug
through the play world. Spawning a slot as a gizmo-placed child and role badges remain a follow-up;
the debug overlay below already visualizes slots in the viewport.

## 4. Agent protocol (`book/agents/smart-objects.md`)

- `smartobject.list` (read) — objects (entity, tags, enabled) and slots (role, position, forward,
  enabled/claimed, holder).
- `smartobject.claim { slot, agent }` (play) — `{ claimed, held_by, reason }`; `NOT_FOUND` for a
  missing slot.
- `smartobject.release { slot, agent }` (play) — `{ released, held_by }`.

All three degrade gracefully without `SmartObjectPlugin`: `list` answers an empty list
(`index_rebuilds: null`), `claim` and `release` answer `NOT_FOUND` (`F0003`) naming the plugin, and
no slot state changes. Spec 39 extends `claim` (`priority`, `use`, a returned `handle`), `release`
(by `handle`) and `list` (`index_rebuilds`, the object's definition); the book page documents the
whole surface.

## 5. Debug overlay

A new gizmo group `SmartObjectGizmos` (`"smart_object"`, off by default) draws each slot as a wire
sphere and a short facing ray, green when available, amber when claimed, grey when disabled. Toggle
with `debug.set_gizmos { "group": "smart_object" }`. Spec 39 adds a priority-colored ring on a
claimed slot (two rings and spokes in use) and an arrow from each entrance to its slot. The overlay
is behind `regia_ai`'s `debug_draw` feature (off in the server build).

## 6. Acceptance

- `cargo test -p regia_ai` passes: index rebuild (position, role, facing, object resolution through a
  middle transform node), `nearest_available` (closest, claim-aware, non-finite-safe), claim/release
  (idempotent, checked, disabled-object), and the slot-without-a-transform skip.
- `cargo test -p regia_agent` passes: `list`/`claim`/`release` round-trip and the no-plugin
  degradation.
- `cargo xtask agent-coverage` and `check-layers` pass (the `regia_agent → regia_ai` same-layer edge
  is listed; every method has a schema row, contract, guide topic and `book/COVERAGE.md` row).
- A level with ≥ 1,000 slots rebuilds the index within the frame budget: the rebuild is a single
  linear pass plus one `ChildOf` hop per slot, profiled with `tracing` (`smart_object_index` span).
  When a spatial structure is needed, it replaces the linear list behind the same `SmartObjectIndex`
  API (a follow-up ADR, no format change).

Tests proving each item (ADR-5632): `regia_ai` unit tests in `smart_object.rs`
(`index_rebuilds_slots_with_position_role_and_facing`, `nearest_available_picks_the_closest_unclaimed_slot`,
`nearest_available_rejects_a_non_finite_position`, `claim_and_release_are_checked_and_idempotent`,
`a_disabled_object_hides_its_slots`, `claim_rejects_a_disabled_slot`, `a_slot_without_a_transform_is_not_indexed`,
`smart_object_ancestor_walks_the_hierarchy`); `tests/smart_object_spec31.rs`
(`a_rotated_object_turns_its_slot_facing_through_a_middle_node`, `effective_enabled_follows_the_slot_and_its_object`,
`a_thousand_slot_level_indexes_every_slot_and_answers_like_a_scan`); `tests/smart_object_index_stale.rs`
(object resolution through a middle node, change-driven rebuilds); `regia_agent` `smart_object::tests`
(`list_reports_objects_and_slots`, `claim_and_release_round_trip`, `claim_of_a_missing_slot_is_not_found`,
`refusals_name_the_holder_and_list_reports_the_facing`, `without_a_plugin_the_list_is_empty`,
`claim_without_plugin_is_not_found`); the layer edge is `xtask/src/layers.rs` (`regia_agent` → `regia_ai`).
