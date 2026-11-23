# `*.ability.ron` (ability, v1)

Spec: `docs/specs/53-abilities-attributes-effects.md` § 2.6, § 4.4. Format id `ability`, version 1
(`regia_framework::abilities::ABILITY_FORMAT_VERSION`). Read by `AbilityAssets::load_text` /
`load_file`; validated by `abilities.validate`.

```ron
(format_version: 1, name: "fire_bolt", kind_tags: ["Ability.Spell.Fire"], run_tags: ["State.Casting"],
 blocks: Some(Has("Ability.Spell")), owner_forbids: Some(Has("Status.Silenced")),
 cost: Some("effects/mana_10.effect.ron"), cooldown: Some("effects/cd_fire_bolt.effect.ron"),
 charge: OnStart, run_state: ScratchOnly, when_running: Reject, triggers: [], max_run_ticks: Some(120),
 input_code: Some(1), run_site: HostOnly, cues: [(tag: "Cue.Cast.Fire", at: Applied)])
```

| Field | Default | Meaning |
|---|---|---|
| `format_version` | required | 1 |
| `name`, `description` | `""` | display text |
| `kind_tags` | `[]` | tags describing the ability |
| `run_tags` | `[]` | tags counted on the owner while it runs |
| `cancels`, `blocks` | `None` | tag queries over other abilities' `kind_tags`: cancelled when this starts; cannot start while this runs |
| `owner_needs`, `owner_forbids` | `None` | tag queries against the owner's effective tags |
| `target_needs`, `target_forbids` | `None` | tag queries against each picked entity |
| `cost` | `None` | an `Instant` effect path (`ability_bad_cost` otherwise) |
| `cooldown` | `None` | a `Timed` effect path with non-empty `status_tags`, the cooldown tags (`ability_bad_cooldown` otherwise) |
| `charge` | `OnStart` | `OnStart` or `Explicit` (the driver calls `charge`) |
| `run_state` | `ScratchOnly` | `ScratchOnly`, `KeepAcrossRuns` or `FreshEachRun` |
| `run_type` | `""` | the registered run-state type for `KeepAcrossRuns`/`FreshEachRun` (`unknown_run_state` otherwise) |
| `when_running` | `Reject` | `Reject`, `Restart` or `QueueOne` |
| `triggers` | `[]` | `Signal(<tag>)`, `TagGained(<tag>)`, `WhileTagged(<tag>)` |
| `max_level` | `0` | highest grant level (0 = no limit) |
| `cancelable` | `true` | whether `cancel` works |
| `max_run_ticks` | `None` | forces `end` after n ticks (warning `ability_no_max_run_ticks` when absent) |
| `needs_picks` | `false` | the run waits for `submit_picks` before charging |
| `pick_timeout_ticks` | `None` | a run still awaiting picks ends with `pick_timeout` |
| `input_code` | `None` | default input code of new grants |
| `run_site` | `HostOnly` | `OwnerOnly`, `OwnerPredicts`, `HostStarts`, `HostOnly`; stored only until networking lands |
| `cues` | `[]` | `(tag, at)`: `Applied` = started, `Removed` = ended, `Sustained` = begin/end |

- Unknown top-level fields are errors (`deny_unknown_fields`) reported with a line and column.
- Validation codes: `reference_missing` (cost or cooldown not loaded), `ability_bad_cost`,
  `ability_bad_cooldown`, `unknown_run_state`, `undeclared_tag`, `format_too_new`, `unknown_field`,
  `parse_error`, and the warning `ability_no_max_run_ticks`.
- Written canonically by `regia_framework::abilities::to_ron`; a resave is byte-identical.
- Migrations: none (v1 is the first version; `ability_migration_sources()` is empty).

## Cooked form

Cooked with the other ability formats as `abilities-cooked` (ADR-7216); see [the effect format](effect-ron.md#cooked-form).
