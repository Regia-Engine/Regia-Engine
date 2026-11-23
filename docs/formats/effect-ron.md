# `*.effect.ron` (effect, v1)

Spec: `docs/specs/53-abilities-attributes-effects.md` § 2.4, § 2.5, § 4.3. Format id `effect`,
version 1 (`regia_framework::abilities::EFFECT_FORMAT_VERSION`). Read by `AbilityAssets::load_text` /
`load_file`; validated by `abilities.validate`.

```ron
(format_version: 1, name: "poison",
 lifetime: Timed(Const(6.0)), pulse: Some((every: 1.0, on_apply: false)),
 base_edits: [(attribute: "health", op: AddPre, amount: Curve((table: "", name: "poison")))],
 live_mods: [(attribute: "move_speed", op: Scale, amount: Const(0.8))],
 status_tags: ["Status.Poisoned"], kind_tags: ["Effect.Damage.Poison"],
 stack: Some((key: ByTarget, cap: 3, on_add: (time: Reset, pulse: Keep), when_full: RejectRenew,
              on_lapse: EndAll, cue_each_stack: false)),
 cues: [(tag: "Cue.Status.Poison", at: Sustained)],
 curves: [(name: "poison", points: [(1.0, -2.0), (10.0, -9.0)])])
```

| Field | Default | Meaning |
|---|---|---|
| `format_version` | required | 1 |
| `name`, `description` | `""` | display text |
| `lifetime` | required | `Instant`, `Timed(<amount>)` (seconds; at application, within `1e-4` of an integer rounds to that integer, otherwise `ceil`, and the result is at least 1 tick; `0.1` s at 60 Hz is 6 ticks) or `UntilRemoved` |
| `pulse` | `None` | `Some((every: <seconds>, on_apply: <bool>))`; only on `Timed`/`UntilRemoved`. `every` uses the same tick conversion as a timed length |
| `base_edits` | `[]` | modifiers applied to `base` (on application for `Instant`, on every pulse otherwise) |
| `live_mods` | `[]` | modifiers that shape `current` while the effect lives (not on `Instant`) |
| `status_tags` | `[]` | tags counted on the target while the effect lives and is not suspended |
| `kind_tags` | `[]` | tags describing the effect (matched by immunity, cleanses and removal queries) |
| `apply_if`, `active_if` | `None` | tag queries: required to apply; while false the effect is suspended |
| `chance` | `1.0` | seeded roll per application, `0..=1` |
| `immunity`, `cleanses` | `[]` | tag queries over other effects' `kind_tags` |
| `grants` | `[]` | `(ability: <path>, level: 1, on_effect_end: Cancel \| AfterRun \| Keep)` |
| `chain` | empty | `(on_apply: [<path>], on_lapse: [<path>], on_removed: [<path>])` |
| `stack` | `None` | `(key: BySource \| ByTarget, cap: >= 1, on_add: (time: Reset \| Keep \| Add, pulse: Restart \| Keep), when_full: Reject \| RejectRenew \| Restart \| Spill(<path>), on_lapse: EndAll \| PopOne \| Renew, cue_each_stack: <bool>)` |
| `suspend` | `(freeze_clock: false, on_resume: Continue)` | `on_resume`: `Continue`, `Restart`, `FireAndRestart` |
| `end_with_source` | `false` | end the effect when its source despawns |
| `cues` | `[]` | `(tag: <tag>, at: Applied \| Pulsed \| Removed \| Sustained, params: (scale: None))` |
| `curves` | `[]` | inline curves `(name, points: [(x, y)])`, strictly increasing `x`, at least one point |

A modifier is `(attribute: <name>, op: <op>, amount: <amount>, per_stack: false, when: None)`.
`op` is `AddPre`, `Percent`, `Scale`, `AddPost` or `Replace(priority: <i16>)`. `amount` is one of
`Const(<f32>)`, `Curve((table: "", name: <curve>))` (`""` = this file's `curves`), `Stat(side: Source
| Target, attribute: <name>, read: AtApply | Live, weight: 1.0, offset: 0.0)`, `Param(key: <tag>,
default: <f32>)` or `Function(<registered name>)`. Tag queries use the `TagQuery` RON form
(`Has("A.B")`, `HasExact(..)`, `AllOf([..])`, `AnyOf([..])`, `NoneOf([..])`). Asset references are
asset paths.

- Unknown top-level fields are errors (`deny_unknown_fields`) reported with a line and column.
- Validation codes: `effect_unknown_attribute`, `effect_bad_lifetime`, `effect_instant_live_mods`,
  `effect_pulse_on_instant`, `stack_cap_zero`, `undeclared_tag` (against the `TagRegistry`),
  `curve_unsorted`, `reference_missing` (an inline curve that is not in `curves`), `format_too_new`,
  `unknown_field`, `parse_error`, and the warning `modifier_negative_scale`.
- Written canonically by `regia_framework::abilities::to_ron`; a resave is byte-identical.
- Migrations: none (v1 is the first version; `effect_migration_sources()` is empty).

## Cooked form

`AbilityAssets::cook_text` parses and validates a `.attrs.ron`, `.effect.ron` or `.ability.ron` file exactly as
`load_text` does (any error refuses the cook; warnings do not; an ability's cost and cooldown must already be loaded)
and encodes it as the format `abilities-cooked` (spec 53 § 4.1, ADR-7216): a little-endian `u32` version
(`ABILITIES_COOKED_VERSION`, 1), then postcard of `CookedAbilityAsset` (`Attributes`, `Effect` or `Ability` with the
asset value). Tags are carried by name and level-curve references by table and curve name, resolved when sampled, as
for the text form. `AbilityAssets::load_cooked` runs the same validation again, so damaged bytes get the codes the text
would; it adds `cooked_unsupported_version` (version 0 or newer than the build), `cooked_bad_file` (truncated or
damaged bytes, bytes after the asset) and `cooked_kind_mismatch` (a path naming another format than the bytes hold).
The same asset cooks to the same bytes. The cooked form is a cache: a shape change bumps the version and paks are
cooked again; there is nothing to migrate.
