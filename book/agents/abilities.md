# Abilities, attributes and effects (agents)

The `abilities.*` methods inspect and drive `regia_framework::abilities`
(`docs/specs/53-abilities-attributes-effects.md`): attributes (numbers with a durable `base` and a
derived `current`), effects (instant, timed or until removed, with pulses and stacking), abilities
(grants with a small state machine, cost, cooldown and triggers) and cues (presentation-only
messages). The host adds `AbilityPlugin`; `AgentPlugin` registers the methods on the `AgentExtension`
in every app with the agent (ADR-1809; `regia_agent::abilities::install_ability_methods` does the
same for a host that builds its own extension). Without `AbilityPlugin` every method answers
`{ "available": false }`.

Assets are read from the `AbilityAssets` resource by path (`.attrs.ron`, `.effect.ron`,
`.ability.ron`; formats in `docs/formats/attrs-ron.md`, `docs/formats/effect-ron.md`,
`docs/formats/ability-ron.md`). Entity params are entity ids as everywhere in the protocol (spec 23).

Effects and ability states advance only in `FixedUpdate`: step with `play.step`. Two
`abilities.apply_effect` calls in one tick resolve in call order. A refused activation or
application is a normal result (`started: false` with a `cause`, or an `outcome`), not an error.

## Worked example: fire bolt with a burn

```jsonc
// Grant the ability (play).
{ "method": "abilities.grant", "params": { "entity": 4294967301, "ability": "abilities/fire_bolt.ability.ron" } }
// -> { "grant": 1 }
{ "method": "abilities.activate", "params": { "entity": 4294967301, "grant": 1 } }
// -> { "started": true, "state": "running" }     (cost paid, cooldown applied: charge OnStart)
{ "method": "play.step", "params": { "frames": 3 } }
{ "method": "abilities.check", "params": { "entity": 4294967301, "grant": 1 } }
// -> { "ok": false, "cause": "on_cooldown", "tag": "Cooldown.FireBolt", "attribute": null }
{ "method": "abilities.apply_effect", "params": { "target": 4294967301, "effect": "effects/burn.effect.ron", "source": 4294967301 } }
// -> { "outcome": "applied", "id": 3, "stacks": 1 }
{ "method": "play.step", "params": { "frames": 1 } }
{ "method": "abilities.inspect", "params": { "entity": 4294967301, "attribute": "armor" } }
// -> breakdown: base 10, add_pre -2 -> 8, percent -> 8, scale 0.5 -> 4, add_post -> 4, current 4
{ "method": "abilities.cues", "params": { "since_tick": 0 } }
// -> { "cues": [{ "tag": "Cue.Burn", "kind": "begin", ... }] }
{ "method": "play.expect", "params": { "assertions": [ { "name": "armor is 4", "condition": {
    "entity": 4294967301, "component": "Attributes", "path": "list[0].current", "op": "eq", "value": 4.0 } } ] } }
// (`list` is in the .attrs.ron's declaration order; armor is its first attribute here)
```

`play.wait_until` and `play.expect` read the reflected `Attributes` component, not a method answer. For grant
states, call `abilities.inspect` again after `play.step` (a grant reads `cooling` until its cooldown tag is gone).

The editor's Abilities inspector edits these assets in a typed form with undo and redo, and autosaves unsaved
edits for crash recovery (kind `abilities`): see [editor gameplay data panels](editor-data-panels.md) and
[editor autosave](editor-autosave.md).

## Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `abilities.list` | read | `entity?` | `{ available, entities: [{ entity, name, attributes, live_effects, grants: [{ id, asset, level, state, input_code }] }] }` |
| `abilities.inspect` | read | `entity`, `attribute?` | `{ available, entity, attributes, effects, tags, grants, breakdown }` |
| `abilities.check` | read | `entity`, `grant` | `{ ok, cause?, tag?, attribute? }` |
| `abilities.grant` | play | `entity`, `ability`, `level?`, `input_code?`, `try_now?`, `one_shot?` | `{ grant }` or `{ grant: null, cause }` |
| `abilities.revoke` | play | `entity`, `grant`, `defer?` | `{ revoked }` |
| `abilities.activate` | play | `entity`, `grant`, `picks?`, `signal?` | `{ started, cause?, tag?, attribute?, state }` |
| `abilities.charge` | play | `entity`, `grant` | `{ charged, cause? }` |
| `abilities.end` | play | `entity`, `grant` | `{ state }` |
| `abilities.cancel` | play | `entity`, `grant` | `{ cancelled, state }` |
| `abilities.signal` | play | `entity`, `tag`, `source?`, `amount?` | `{ started: [grant] }` |
| `abilities.apply_effect` | play | `target`, `effect`, `source?`, `level?`, `params?` | `{ outcome, id?, stacks? }` |
| `abilities.remove_effect` | play | `entity`, `id` or `query`, `limit?` | `{ removed }` |
| `abilities.set_base` | edit_scene | `entity`, `attribute`, `base` | `{ old, new }` |
| `abilities.add_attributes` | edit_scene | `entity`, `asset` | `{ attributes }` |
| `abilities.cues` | read | `since_tick?`, `limit?` | `{ available, cues: [{ tick, entity, tag, kind, source, amount, effect }] }` |
| `abilities.validate` | read | `path` | `{ available, ok, diagnostics: [{ code, severity, message, span }] }` |
| `abilities.stats` | read | (none) | `{ available, tick, entities, live_effects, applications, removals, modifier_evaluations, entities_skipped, signals_unmatched, warnings, ended }` |
| `abilities.state_hash` | read | (none) | `{ available, hash, quantum }` |

Classes map to permissions: read = `Permission::Read`, play = `Permission::Play`, edit_scene =
`Permission::EditScene`.

### Details

- `abilities.list`: entities with `Attributes` or an `AbilityBook`, ascending entity id; `entity`
  limits it to one. `state` is `ready`, `running`, `ending` or `cooling` (`cooling` is a `ready`
  grant whose owner holds one of its cooldown tags; it is derived, not stored).
- `abilities.inspect`: `attributes` are `{ name, base, current, min, max, derived }`; `effects` are
  `{ id, asset, level, source, stacks, remaining_ticks, next_pulse_tick, suspended, status_tags,
  applied_tick }`; `tags` are the effective tags, each `{ tag, source, count }` with `source`
  `explicit` (the authored `TagContainer`) or `counted` (granted by effects and running abilities,
  with the count); `grants` as in `abilities.list`. `breakdown` lists, per attribute (or only
  `attribute`), `{ attribute, base, phases: [{ phase, value_after, modifiers: [{ effect, op, amount,
  stacks, contribution }] }], replace_winner, unclamped, clamp: [min, max], current }`. The phases
  are `add_pre`, `percent`, `scale`, `add_post` and `replace`, in that order.
- `abilities.check`: the activation check without starting. The first failure is reported, with the
  blocking `tag` or the short `attribute` (`cannot_afford`) where there is one.
- `abilities.grant`: `level` defaults to 1 (capped by the asset's `max_level`); `input_code` defaults
  to the asset's. `try_now` starts at once and keeps nothing when the check fails (`grant: null` and
  the `cause`). `one_shot` removes the grant after its first run ends.
- `abilities.revoke`: a running grant is cancelled first; with `defer` it loses its input binding at
  once and is removed when its run ends.
- `abilities.activate`: `picks` is an array of entity ids or `{ "point": [x, y, z] }` objects;
  `signal` is a tag carried on the run. The target requirements are checked against the picked
  entities.
- `abilities.charge`: pays cost and cooldown of a running `Explicit` grant; a failed charge returns
  `charged: false` with `cannot_afford` and changes nothing.
- `abilities.end`: `running` -> `ending`; the grant is `ready` (or `cooling`) after the next step.
- `abilities.cancel`: `cancelled` is true when the run moved to `ending`.
- `abilities.signal`: matches the signal against the `Signal` triggers of the entity's grants now, in
  ascending grant id (a child tag starts a parent-tag trigger); `started` lists the grants that
  started. A signal that matches nothing counts in `signals_unmatched`.
- `abilities.apply_effect`: `params` is `{ "Tag.Name": number }` for `Param` amounts. `outcome` is
  one of the application outcomes below; `id` is the live entry (absent for instant effects and
  refusals), `stacks` its count.
- `abilities.remove_effect`: by `id` (reason `removed`), or every live effect whose `kind_tags`
  match `query` (a RON tag query such as `Has("Effect.Damage")`), at most `limit`.
- `abilities.set_base`: sets `base` directly (no edit gates), clamped to the attribute's range;
  `old` and `new` are the bases before and after. In the editor this is one undo step.
- `abilities.add_attributes`: gives the entity the attributes of an `.attrs.ron` (loaded from
  `AbilityAssets`, or read from the project's `assets/` folder when not loaded, like the abilities panel: `attrs/x.attrs.ron`
  and `assets/attrs/x.attrs.ron` name the same file, and the set is recorded under the key as given; an absolute path under
  the project or the temp directory is read as it is); replaces existing `Attributes`.
- `abilities.cues`: the recent `CueEvent`s from a ring of 256, oldest first; `kind` is `burst`,
  `begin` or `end`.
- `abilities.validate`: parses and validates a file without loading it into the world; references
  resolve against the loaded assets. Codes: `attribute_duplicate`, `attribute_cycle`,
  `attribute_range`, `attribute_name`, `effect_unknown_attribute`, `effect_bad_lifetime`,
  `effect_instant_live_mods`, `effect_pulse_on_instant`, `stack_cap_zero`, `undeclared_tag`,
  `curve_unsorted`, `reference_missing`, `ability_bad_cost`, `ability_bad_cooldown`,
  `unknown_run_state`, `format_too_new`, `unknown_field`, `parse_error`, and the warnings
  `modifier_negative_scale`, `ability_no_max_run_ticks`, `effect_over_budget` (more than 16 modifiers) and
  `attributes_over_budget` (more than 64 attributes). The two budget warnings never stop a file loading.
  `span` is `{ line, column }` when known.
- `abilities.stats`: counters only, no wall time. `modifier_evaluations` and `entities_skipped` are
  from the last step (skipped: an entity with no live effect and nothing changed, or one whose live modifiers are all fixed,
  with no `when` and no `Live` stat read, and nothing but effect clocks changed since its last recompute); `warnings`
  counts runtime warnings by code: `curve_missing`, `param_missing`, `magnitude_fn_missing`,
  `unknown_attribute`, `attribute_derived`, `attribute_not_finite`, `pulse_overrun`,
  `chain_too_deep`, `effects_over_soft_limit` (an application took an entity past 32 live effects; it still
  applied) and `message_budget` (a step got more than 4,096 messages of one request kind; the rest of that burst
  was dropped, in message order, and one warning counts the burst). `ended` counts ended effects by reason (the
  `EffectEnded` reason codes): `expired`, `removed`, `cleansed`, `restarted`, `source_gone` (the source despawned
  and the asset sets `end_with_source`) and `target_gone` (the target despawned; its effects end in the next step's
  effect pass, with the asset's end cues, which `abilities.cues` lists with `kind: end`). Only reasons that happened appear.
- `abilities.state_hash`: a hash two platforms running the same inputs agree on, as 16 hex digits. Counts, ticks and
  ids fold exactly; `base` and `current` fold on the `quantum` grid (`1e-4`), and the hash uses no float beyond that, so
  a difference in the last places of a libm `Function` amount does not move it. Compare it across two machines after the
  same steps. Entities fold by `EntityGuid`, so spawn order does not matter once they have one. Two values either side of a
  grid edge still hash apart however close they are: a mismatch names a step to compare with `abilities.inspect`,
  not a proof of a divergence. On one machine, use the bit-exact `sim.state_hash` instead.

## Errors

Protocol errors are `F0002` (bad params) or `F0003` (not found) with `data.reason`:

| Reason | Code | When |
|---|---|---|
| `entity_not_found` | F0003 / F0002 | the entity does not exist (F0002 when the id is malformed) |
| `no_attributes` | F0003 | the entity has no `Attributes` |
| `unknown_attribute` | F0003 | the entity has no attribute of that name |
| `attribute_derived` | F0002 | `set_base` on a derived attribute |
| `attribute_not_finite` | F0002 | the value is not finite |
| `effect_not_found` | F0003 | the effect asset is not loaded, or no live effect has that id |
| `grant_not_found` | F0003 | the entity has no grant with that id |
| `asset_not_found` | F0003 / F0002 | the ability or attribute asset is not loaded and cannot be read (F0002 when the path param is not a string, or the `.attrs.ron` `abilities.add_attributes` reads does not validate) |
| `not_cancelable` | F0002 | `cancel` on an ability with `cancelable: false` |
| `grant_limit` | F0002 | the book already holds 32 grants (`abilities.grant`) |
| `bad_query` | F0002 | a tag or tag query does not parse |
| `format_too_new` | F0002 | the file's `format_version` is newer than this engine reads |
| `reference_missing` | F0003 | an ability's cost or cooldown effect is not loaded |
| `bad_value` | F0002 | a malformed param: a pick that is not an entity id or `{ point: [x, y, z] }` of finite numbers, a `level` that is not an integer in 0..=4294967295, an `input_code` outside 0..=65535 |

## Refusal causes

Results, not errors (`cause` in `abilities.check`, `abilities.activate`, `abilities.charge`,
`abilities.grant` with `try_now`), in check order: `not_granted`, `owner_locked` (the owner has
`AbilitiesLocked`), `input_blocked`, `level_zero`, `already_running`, `owner_missing_tag`,
`owner_forbidden_tag`, `blocked_by_ability`, `on_cooldown`, `cannot_afford`, `target_rejected`, and
`pick_timeout` (a run awaiting picks timed out; reported as a lifecycle refusal).

## Application outcomes

`applied` (a new entry or an instant edit), `stacked` (merged into an entry), `failed_chance`,
`blocked_condition` (`apply_if` did not match), `blocked_immunity`, `rejected_full` (at the stack
cap), `spilled` (at the cap, another effect applied instead), `limit_reached` (128 live effects on
the target), `chain_too_deep` (a chain deeper than 4).

## Cooked ability assets and the runtime loader (spec 53 § 4.1, ADR-9370)

`regia cook` cooks `.attrs.ron`, `.effect.ron` and `.ability.ron` to `abilities-cooked` (type path
`regia_framework::abilities::CookedAbilityAsset`, registry kind `ability_asset`), so a packaged game's pak lists
them. Each file is cooked with the files its cost and cooldown reach, in load order (costs and cooldowns before the
abilities naming them), and validated as the game's load validates it; checks on the game's run-state types are
left to the game's load. Cook errors: `abilities_reference_missing` (a cost or cooldown names a file the project
lacks), `abilities_reference_cycle` (references loop; every asset on the loop fails), `abilities_reference_invalid`
(a file the asset names does not cook), plus the load codes (`ability_bad_cost`, `parse_error`, ...).

At run time `app.load_ability_assets_at_startup([...])` (or `load_ability_assets(world, &paths)`) loads asset paths
into `AbilityAssets`: in a packaged game (paks mounted) the cooked form from the pak (`regia package`
ships no raw ability files, ADR-9850 (c)); in a dev tree the loose RON (it always runs what is on disk). `AbilityAssetRegistrations::loads` keeps each path's result (`cooked`, diagnostics); a path that cannot
be read gets `asset_not_found`. `abilities.stats` lists those results in `asset_loads`:

```json
{"asset_loads": [{"path": "abilities/fire_bolt.ability.ron", "cooked": true, "loaded": true, "errors": []}]}
```

`cooked: true` means the asset came from the pak; `errors` holds the codes of a refused asset (`asset_not_found`,
`abilities_reference_cycle`, `reference_missing`, ...). It is empty when the game loads its assets itself.

## Checks agents run

- After `abilities.activate`, step once and read `abilities.inspect`: run tags appear in `tags`
  with `source: counted`, and the cost shows in the attribute's `base`.
- `abilities.stats.modifier_evaluations` stays at or below the number of live modifiers; an idle
  world reads 0, and so does a settled world of fixed modifiers (two steps after the last change).
- `abilities.validate` on every asset before a commit; an `error` severity refuses the asset at load.
