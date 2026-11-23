# Smart objects: interaction slots agents can discover, claim and use

A **smart object** is a world entity that advertises *interaction slots* — places an AI agent (or a
player) can stand, sit or operate — so agents can find and use a chair, terminal, door or cover point
without bespoke per-object code. The feature lives in `regia_ai` (`SmartObject`, `SmartObjectSlot`,
`SmartObjectPlugin`); without the plugin the lists are empty and claim/release answer `NOT_FOUND`
(`F0003`).

Authoring: an entity carries `SmartObject` (tags + an `enabled` flag, and optionally a `definition` that names a
`.smartobject.ron`; see *Definitions* below); each slot is a **child** entity
carrying `SmartObjectSlot { role, enabled }`, placed and rotated in the editor. The slot's world pose
is its `Transform`/`GlobalTransform`; its facing is its local −Z axis. A slot is *available* when it is
enabled, its object is enabled, and it is not claimed.

## List

`smartobject.list` reports every object and slot:

```json
{}
→ { "total": 2,
    "objects": [ { "entity": 4294967301, "tags": ["cover"], "enabled": true,
                   "definition": null, "definition_path": null, "definition_cooked": false,
                   "definition_error": null } ],
    "slots": [
      { "entity": 4294967302, "object": 4294967301, "role": "cover",
        "enabled": true, "claimed": false, "claimed_by": null,
        "position": [3.0, 1.5, -2.0], "forward": [0.0, 0.0, -1.0] }
    ],
    "index_rebuilds": 3 }
```

`object` is `null` for a slot with no `SmartObject` ancestor (still a valid slot, but it cannot be
disabled through an object). `claimed_by` is the holding entity's bits or `null`.

`index_rebuilds` counts how many times the slot index was rebuilt (`null` without
`SmartObjectPlugin`). A frame where no slot, object, parent link or slot transform changed adds
nothing, so two reads over quiet frames that differ point at something touching those components
every frame.

## Claim

`smartobject.claim` claims `slot` for `agent`. Claiming the same slot again for the same agent is
idempotent. A slot held by another agent, or a disabled slot, answers `claimed: false` with the current
holder and a `reason`; a missing slot answers `NOT_FOUND` (`F0003`):

```json
{ "slot": 4294967302, "agent": 4294967303 }
→ { "claimed": true, "held_by": 4294967303 }

{ "slot": 4294967302, "agent": 4294967304 }
→ { "claimed": false, "held_by": 4294967303, "reason": "claimed" }
```

## Release

`smartobject.release` releases `slot` held by `agent`. Releasing a slot the agent does not hold answers
`released: false` with the current holder; releasing an unclaimed slot is a no-op (`released: true`):

```json
{ "slot": 4294967302, "agent": 4294967303 }   → { "released": true }
{ "slot": 4294967302, "agent": 4294967304 }   → { "released": false, "held_by": 4294967303 }
```

## Working with the results

- Slots are the claimable unit; `object` groups them. Query by `role` (e.g. `"sit"`, `"operate"`,
  `"cover"`) to find the affordance you want, then `claim` the nearest available one.
- `debug.set_gizmos { "group": "smart_object", "enabled": true }` draws every slot as a wire sphere
  and a facing ray, green when available, amber when claimed, grey when disabled. With v2 a claimed
  slot also gets a ring colored by the holder's priority (cool blue for `background`/`low`, amber for
  `normal`, orange for `high`, red for `urgent`), an in-use slot a second inner ring with spokes, and
  every entrance an arrow pointing at its slot (needs the `debug_draw` feature).
- The runtime API (`regia_ai::SmartObjectIndex`, `claim_slot`, `release_slot`) is the same surface a
  behavior-tree leaf task or a script uses; the agent methods are thin wrappers over it.

## v2: priorities, handles and the claim life cycle

Version 2 (`docs/specs/39-smart-objects-v2.md`) keeps everything above and adds a claim life cycle. A slot is in
one of four states, derived from its components: `free`, `claimed`, `in_use` and `disabled`.

`smartobject.claim` takes an optional `priority` (`background`, `low`, `normal` (default), `high`, `urgent`) and
`use: true` to begin using the slot in the same call. A successful claim returns a `handle`:

```json
{ "slot": 4294967302, "agent": 4294967303, "priority": "low" }
→ { "claimed": true, "held_by": 4294967303, "handle": { "slot": 4294967302, "generation": 0 } }

{ "slot": 4294967302, "agent": 4294967304, "priority": "low" }
→ { "claimed": false, "held_by": 4294967303, "reason": "claimed" }

{ "slot": 4294967302, "agent": 4294967304, "priority": "urgent" }
→ { "claimed": true, "held_by": 4294967304, "handle": { "slot": 4294967302, "generation": 1 } }
```

- A claim on a held slot succeeds only at a **strictly higher** priority than the holder's. The old holder gets a
  `lost` message with reason `preempted` and its handle goes stale.
- The same agent claiming again gets the same handle and may raise the held priority.
- `smartobject.begin_use { handle }` moves `claimed` to `in_use` (answer `{ "state": "in_use" }`). An in-use slot
  cannot be preempted: claims on it answer `claimed: false` with `reason: "in_use"`.
- `smartobject.release` accepts `{ "handle": { slot, generation } }` instead of `slot` and `agent`. A stale handle
  is an `F0002` error with reason `stale_handle`. With `agent` as well as `handle`, only that agent's claim is
  released; another agent gets `F0002` with reason `not_holder`.
- A claim is released automatically (one `lost` message each) when the holder is despawned (`holder_gone`), the slot
  or its object is despawned (`slot_gone`), the slot or its object becomes disabled (`disabled`) or a definition
  reload removes the slot (`slot_removed`). Behavior tasks read `lost` messages and clean up their own state.

## Search

`smartobject.search` filters slots in a region and returns them nearest first:

```json
{ "region": { "center": [0, 0, 0], "radius": 20 },
  "activity": "Has(\"Activity.Sit\")", "user_tags": ["Role.Guard"],
  "priority": "high", "allow_preempt": true, "explain": true }
→ { "hits": [ { "slot": 4294967302, "object": 4294967301, "distance": 3.2, "status": "free" },
              { "slot": 4294967310, "object": 4294967309, "distance": 7.9, "status": "preemptable",
                "holder": 4294967311 } ],
    "rejections": [ { "object": 4294967301, "slot": 4294967306, "stage": "activity",
                      "detail": "activity query failed" } ],
    "rejections_total": 1 }
```

- `region` is `{ center, radius }` (a sphere) or `{ min, max }` (a box). Distance is measured from the sphere's
  center or the box's center to the slot. Hits sort by `(distance, slot entity)`; `limit` applies after sorting. A `limit` that is not a
  non-negative integer, or a `user` that is not entity bits, is `bad_query`; a `user` that is well-formed but names no
  live entity is `entity_not_found` (ADR-1769, ADR-1809; they used to mean "no limit" and "no user").
- Stages run in order and the first failure is the recorded rejection: `region`, `enabled`, `state`, `family`,
  `activity`, `user`, `condition`. When one object-level check rejects every slot of an object alike, a single
  entry with `slot: null` is reported. `rejections` is capped at 256 entries (`rejections_total` is the count).
- `activity` is a tag query as RON text (`Has("A.B")`, `AnyOf([...])`) tested against the slot's effective activity
  set: the definition's and the slot's tags (`Union`, or the slot's own first with `SlotFirst`) plus the runtime tags.
- `user_tags` are the requester's tags, tested against the definition's and the slot's user rules.
- `families` keeps slots that offer one of the behavior families. `check_conditions` (default true) runs the
  registered slot conditions; an unregistered condition counts as failing (one warning in the log).
- A `claimed` slot is a hit only with `allow_preempt` and a `priority` above the holder's (`status: "preemptable"`,
  with the `holder`); an `in_use` slot never; a `disabled` slot only with `include_disabled`.

## Enabling, tags and messages

- `smartobject.set_enabled { entity, enabled, reason? }` adds (`enabled: false`) or removes (`true`) an off-reason on
  a slot or object (default reason `gameplay`). The target is enabled only when its own `enabled` flag is set (and, for a slot, its object's) and it has no
  off-reason left, so two disables need two enables; `enabled: true` never sets a cleared flag. The answer is the target's own effective value.
- `smartobject.tags { entity, add?, remove? }` adds and removes runtime tags on a slot or object and answers the
  runtime tags it holds. Runtime tags join the activity set of the slot (an object's join every slot of it).
- `smartobject.messages { last? }` returns the most recent records, oldest first (at most 256), ordered by object
  and then slot:
  - `{ "kind": "changed", "object", "slot", "what", "tag" }` with `what` one of `claimed`, `in_use`, `released`,
    `tag_added`, `tag_removed`, `enabled`, `disabled`, `custom` (written only when the effective value changed;
    `slot` is `null` for an object-scope change);
  - `{ "kind": "lost", "handle", "holder", "reason" }` with `reason` one of `preempted`, `disabled`, `holder_gone`,
    `slot_gone`, `slot_removed`.

## Definitions

A scene names a definition with `SmartObject::definition`, a persisted asset reference. The id comes from the cook, so a
project needs a `regia cook` first. On the next `PostUpdate` the definition is read (the source text when there is one,
else the pak entry) and the slots appear as children. Objects naming one id share one load.

`smartobject.set_definition { entity, path }` (or `{ entity, id }`) points an object at a definition from a tool. `path` is
under `assets/` and must already be a cooked asset id. It reads the definition at once, so the answer already says how that went:

```json
{ "entity": 4294967301, "path": "props/chair.smartobject.ron" }
→ { "definition": "67e55044-10b1-426f-9247-bb680e5fe0c8", "path": "props/chair.smartobject.ron",
    "loaded": true, "cooked": false, "error": null }
```

An unknown `path` (nothing cooked at `assets/<path>`) is `F0002` with `data.reason` `definition_not_found`. The call
returns before `SmartObject.definition` is written, so the object stays unpointed and saving the file afterward does
not fill it. Run `regia cook`, then call again.

An `id` that does not load is an `error` in the answer, not a protocol error: the object is pointed at that id anyway, so
fixing the file and saving is enough. `error.reason` is `definition_not_found` (the id resolves to nothing, or the file
is gone), `bad_definition` (it does not parse or validate) or `unsupported_version` (a newer `format_version`). `smartobject.list`
shows the same thing per object as `definition_error`, with `definition_path` and `definition_cooked`. A changed file is
read again in dev builds; if the new text does not load, the old definition stays in use, its slots are not touched, and
`definition_error` says why. Check the result with `play.wait_until` on `smartobject.list`.

## Errors

All new failures are `F0002` with `data.reason`:

| Reason | Meaning |
|---|---|
| `missing_param` | A required parameter is absent (also a handle without `generation`). |
| `slot_not_found` | The entity (or the handle's slot) is not a smart-object slot. |
| `stale_handle` | The claim was released, preempted or invalidated since the handle was issued. |
| `not_holder` | `release` with both `handle` and `agent`: that agent does not hold the claim. |
| `slot_claimed`, `slot_in_use`, `slot_disabled` | A runtime claim error mapped from the library (`claim` itself answers `claimed: false` with a `reason` instead). |
| `entity_not_found` | `set_enabled` / `tags` target is neither a slot nor an object, or a `search` `user` is not a live entity. |
| `bad_region` | `region` is neither a sphere nor a box, or has a negative radius, non-finite numbers or `min > max`. |
| `bad_priority` | Not one of `background`, `low`, `normal`, `high`, `urgent`. |
| `bad_query` | `activity` does not parse, or a tag name is not a valid tag name, or a list has the wrong shape, or a `search` `limit` is not a non-negative integer. |
| `tag_unknown` | The tag is valid but not declared in the loaded project tag table. |
| `definition_not_found` | `set_definition`: no cooked asset at `path`; run `regia cook`. |
| `bad_id` | `set_definition`: `id` is not an asset id string. |

## Runtime API

`claim_with(world, slot, user, priority) -> Result<ClaimHandle, SmartObjectError>`, `begin_use`, `release_handle`, `slot_state`,
`SmartObjectIndex::{search, find_first, availability}`, `add_runtime_tag` / `remove_runtime_tag`, `set_enabled`,
`entrance_for` and `slot_behavior::<T>` are in `regia_ai::smart_object_v2`. `claim_slot` / `release_slot` keep their
signatures and delegate at `Normal`. A handle packs into a `u64` with `ClaimHandle::to_bits` for a blackboard `Int`.
Read `smartobject.list` (`claimed_by`) or `smartobject.messages` to see the outcome of a claim.

## In the editor

The Smart Objects panel lists every object and slot with its claim state, holder and priority, and marks the slots in the
viewport. `smartobject.*` reach the world that serves the agent, which in the editor is the edit world;
`editor.smart_object_panel_*` reach the play world. See [gameplay panels](editor-gameplay-panels.md). A changed
`.smartobject.ron` now reaches its objects through the editor's watcher (ADR-2030).
