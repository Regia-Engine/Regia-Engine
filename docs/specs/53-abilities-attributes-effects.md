# Spec 53: Abilities, attributes and effects

Status: proposed (ADR-1579). Target rung: v0.6.0 (group G14, gameplay framework). Network prediction
arrives later through `regia_net`; this spec only reserves the hooks (§ 2.8).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_framework::tags` (ADR-1463), `regia_framework::damage`, `regia_framework::timers`;
`regia_ai::abilities` (ADR-1563 a, on `integ/ext2`) is a separate, smaller AI component (§ 1.2).

Source area: runtime-gameplay/runtime-gameplay-abilities (primary), runtime-gameplay/runtime-gameplay-tags
(vocabulary only).

## 1. Purpose, scope and baseline

### 1.1 Purpose

A game needs numbers on entities (health, stamina, move speed, armor), a way to change them for a while
or for good (a poison, a buff, a heal over time), and triggerable actions (a dash, a spell, a reload) that
check requirements, pay a cost, go on cooldown and tell the presentation layer what happened. Built per
game as loose fields and timers, this gives inconsistent stacking, order bugs and state no tool can
inspect. This spec defines one data-driven, fixed-step, deterministic model:

- **Attributes**: named `f32` values with a durable `base` and a derived `current`, clamps and derived
  values.
- **Effects**: authored packages of modifiers, tags and rules, applied once, for a time, or until removed,
  optionally pulsing on a period, with stacking.
- **Abilities**: authored actions with a small state machine, tag-driven requirements, cost, cooldown,
  cancel and block rules, and triggers.
- **Cues**: presentation-only messages (VFX, audio, camera shake, UI) that never change simulation state.

Not in scope: a scripting language (ability logic beyond data is an ordinary Rust system that reads
`AbilityLifecycle` and drives `end`/`charge`, § 3), animation (an ability may name a montage for a cue
consumer to play, `regia_animation::montage`), network replication (§ 2.8), AI decision making (behavior
trees and state trees decide *when* to activate), and a replacement for `Health`/`DamageEvent` (§ 3.5).

### 1.2 Baseline in the repo (do not respecify)

| Item | Where | Status |
|---|---|---|
| Gameplay tags: `Tag` (64-bit FNV-1a id, name kept for saving), `TagContainer` (sorted set component; `has` matches a held child, `has_exact` does not; `matches(&TagQuery)`), `TagQuery` (all/any/none, nestable, RON form), `TagRegistry` + `.tags.ron` validator, `TagPlugin`, `TagsChanged { entity, added, removed }` raised in `PostUpdate` | `regia_framework::tags` | implemented (ADR-1463), reused as is |
| `Health { current, max }`, `DamageEvent { target, amount, source, position }`, `Died`, `DiedEvent { entity, killer }`, `DamageSet` (FixedUpdate, before `ScriptsSet::FixedUpdate`) | `regia_framework::damage` | implemented; § 3.5 bridges to it |
| `TimerHub`, `schedule_after`, `schedule_every` | `regia_framework::timers` | implemented; used by game drivers for delays |
| `Rng` (PCG32, `Rng::with_stream(seed, stream)`, same sequence on every platform) | `regia_core::math::Rng` | implemented; § 7 uses it |
| `EntityGuid(Guid)` stable entity id | `regia_scene::components` | implemented; § 7 ordering |
| Save games: `Saveable` entities, reflected components encoded by type path; **`Entity` fields inside components are raw bits, not remapped** | `regia_framework::save_game` | implemented; § 4.6 works around the limit |
| State hash: `world_snapshot`, `snapshot_hash` over reflected components | `regia_framework::state_hash` | implemented; tests use it |
| `Abilities` component: string-keyed slots with cooldown seconds and an `f32` resource cost, ticked in `regia_ai::AiStep`, inspected through generic `scene.*` | `regia_ai::abilities` (on `integ/ext2`, ADR-1563 a) | implemented; **kept**, see decision below |

Decision (ADR-1585): `regia_ai::abilities::Abilities` stays as the lightweight AI slot component of spec 22
§ T2.2; this spec's system is a separate module in `regia_framework`. The two do not share types. To avoid a
prelude clash, this module's definition types are named `EffectAsset`/`AbilityAsset` (not `AbilityDef`), and
`regia_framework::prelude` exports only the plugin, the components and the request messages. A behavior-tree
task that activates a spec 53 ability needs a `regia_ai -> regia_framework` same-layer edge in
`xtask/src/layers.rs` and an ADR; it is a follow-up (§ 11), not part of this spec. Nothing migrates: the AI
component is not a file format.

## 2. Model

### 2.1 Nouns

| Term | Kind | Meaning |
|---|---|---|
| `Attributes` | Component, reflected | All attributes of one entity: an ordered `Vec<Attribute>` (declaration order). |
| `Attribute` | Record | `id: AttrId` (`u64`, FNV-1a of the name via `regia_framework::tags::fnv1a64`, stable across runs), `name`, `base`, `current`, `min`, `max`, `max_from: Option<AttrId>`, `derived: Option<Derived>`. |
| `AttributesAsset` | Asset (`.attrs.ron`) | Authored attribute list with defaults (§ 4.2). |
| `Modifier` | Record | One numeric change to one attribute: `attribute`, `op: ModOp`, `amount: Magnitude`, `per_stack`, `when: Option<TagQuery>`. |
| `EffectAsset` | Asset (`.effect.ron`) | Immutable authored effect (§ 2.4). |
| `AppliedEffect` | Record | One live application: `id: EffectId`, asset path, level, source, applied tick, remaining ticks, stacks, next pulse tick, captured amounts, `suspended`, `intent: IntentSeq`. |
| `AppliedEffects` | Component, reflected | The live effects of one entity, sorted by `EffectId` (`u64`, monotonic per world from the saved resource `AbilityCounters`, never reused). Instant effects never enter it. |
| `AbilityAsset` | Asset (`.ability.ron`) | Immutable authored ability (§ 2.6). |
| `AbilityBook` | Component, reflected | The grants on one entity: `Vec<Grant>` sorted by `GrantId`. |
| `Grant` | Record | `id: GrantId` (`u32`, never reused while the book lives), asset path, level, input code, state, run data, `pending_removal`. |
| `TagCounts` | Component, reflected | Counted tags granted by effects and running abilities: sorted `Vec<(Tag, u16)>`. |
| `AbilitySignal` | Message | Payload that can trigger abilities: `target`, `tag`, `source`, `amount`, `picks`, `context: TagContainer`. |
| `Picks` | Record | Up to 32 `Pick`s: `Entity(Entity)`, `Point(Vec3)`, `Hit { entity, point, normal }`, `Direction(Vec3)`; producer order is kept. |
| `CueEvent` | Message | Presentation request (§ 2.7). |
| `IntentSeq` | Newtype `u32` | Reserved client-intent sequence number, 0 = none; carried, never interpreted (§ 2.8). |

**Effective tags.** An entity's effective tags are its explicit `TagContainer` plus every tag in its
`TagCounts` with a count above zero. Counts make two sources granting one tag safe: the tag leaves only when
the last source ends. `TagContainer` stays the authored set and is never written by this module.
`effective_has(world, e, tag)` and `effective_matches(world, e, &TagQuery)` use the same hierarchy rule as
`TagContainer::has`. When the effective set changes, the module writes `EffectiveTagsChanged { entity, added,
removed }` (its own message; baseline `TagsChanged` keeps reporting the explicit container only).

### 2.2 Attributes

- Built from an `.attrs.ron` asset or in code (`Attributes::from_asset`, `Attributes::push`). A repeated name
  on one entity is an error (`attribute_duplicate`).
- **Base and current.** `base` is durable; `current` is what every reader uses. `current` is recomputed from
  `base` and the live modifiers every fixed step (§ 2.3). Instant effects and pulses change `base`. Timed and
  until-removed effects add live modifiers that shape `current` and disappear with the effect.
- **Clamps.** `min <= max` is checked at load (`attribute_range`). `current` is clamped to `[min, max]` after
  every recompute; `base` is clamped when written. `max_from: Some(other)` makes the upper bound
  `min(max, other.current)` every step, so a health / health-ceiling pair needs no manual sync. A cycle in
  `max_from` links is rejected (`attribute_cycle`).
- **Derived values.** `derived: Some(Derived { inputs: Vec<(AttrId, f32)>, offset: f32 })` sets
  `base = offset + sum(input.current * weight)` each step, in a topological order computed once when the set is
  built (ties by declaration order). Instant edits to a derived attribute are refused (`attribute_derived`);
  live modifiers on it work and apply after the derived base. Cycles are rejected (`attribute_cycle`).
- Non-finite writes are refused (`attribute_not_finite`); the old value stays.
- **Messages.** A change of `current` larger than `f32::EPSILON * max(1, |v|)` writes one
  `AttributeChanged { entity, attribute, lane: Current, old, new, source }` per attribute per step in
  `AbilitySet::Notify`; a `base` change writes the same message with `lane: Base`. Reactions (die at zero) are
  systems reading these messages.
- **Edit gates.** Before an instant edit lands, every `EditGate` registered on `AbilityPlugin` (registration
  order) sees an `EditRequest { target, attribute, delta, source, effect }` and answers
  `EditVerdict::{Pass, Scale(f32), Refuse}` (a shield, "no healing while diseased"). Gates are plain
  `fn` pointers; § 3.3 covers hot reload.

### 2.3 Modifier evaluation

`ModOp` and its phase:

| `ModOp` | Phase | Combine within phase |
|---|---|---|
| `AddPre` | 1 | sum |
| `Percent` | 2 | sum of fractions (`0.25` = +25 %), applied as `* (1 + sum)` |
| `Scale` | 3 | product |
| `AddPost` | 4 | sum |
| `Replace { priority: i16 }` | 5 | highest priority wins; ties by earlier applied tick, then lower `EffectId` |

Per attribute, every step, from scratch (no incremental drift):

```
v = base
v = v + sum(AddPre)
v = v * (1 + sum(Percent))
v = v * product(Scale)
v = v + sum(AddPost)
if any Replace: v = winning Replace amount
current = clamp(v, min, effective_max)
```

- Sums and products are taken in ascending `EffectId`, then modifier index, so float order is fixed. Only
  `Replace` depends on order, and it has the explicit tie-break.
- `per_stack: true` multiplies by the stack count: `AddPre`, `Percent`, `AddPost` use `amount * stacks`;
  `Scale` uses `amount` multiplied by itself `stacks` times (repeated multiplication, not `powf`); `Replace`
  ignores stacks.
- `when: Some(query)` makes the modifier count only while the target's effective tags match; checked every
  step. Modifiers of a suspended effect count nothing.
- A negative `Scale` is allowed (sign flip); the validator warns (`modifier_negative_scale`).

**Amount sources** (`Magnitude`):

| Variant | Value |
|---|---|
| `Const(f32)` | The constant. |
| `Curve(LevelCurveRef)` | A curve sampled at the effect level. `LevelCurveRef { table: String, name: String }`; `table: ""` means the effect file's own `curves` list. |
| `Stat { side: Source \| Target, attribute, read: AtApply \| Live, weight, offset }` | `offset + weight * attr.current`, captured at application (`AtApply`) or read every step (`Live`). Live reads that form a cycle are rejected at build (`attribute_cycle`). |
| `Param { key: Tag, default: f32 }` | A number supplied by the caller of `apply_effect`, keyed by tag; `default` when absent, plus one `param_missing` warning per application. |
| `Function(MagnitudeFnId)` | A registered `fn(&MagnitudeInput) -> f32`. Pure: reads only the source and target attribute snapshots, level and stacks. An unregistered id yields `0.0` and `magnitude_fn_missing`. |

Curve lookup goes through the `LevelCurves` trait (`fn sample(&self, table: &str, name: &str, x: f32) ->
Option<f32>`), held as a boxed resource. The module ships one implementation that reads only the effect file's
inline `curves` (piecewise linear, at least one point, strictly increasing `x`, clamped outside the range).
When spec 51's `CurveTables` resource is present (same crate), an adapter implements `LevelCurves` over
`CurveTables::get(table)?.sample_named(name, x)`; this spec does not define a second curve-table format or a
second `CurveRef` type. A missing curve yields `0.0` and `curve_missing`; it never panics.

### 2.4 Effects

`EffectAsset` fields (grouped by what they decide):

- **Lifetime.** `lifetime: Instant | Timed(Magnitude) | UntilRemoved`. A timed length in seconds becomes
  ticks at application. Let `x = f64::from(seconds) * tick_rate`, with `seconds` the `f32` magnitude.
  Non-finite `x`, and `x <= 1`, are 1 tick. When `x` is within `1e-4` of an integer, the length is
  that integer (`round`); otherwise it is `ceil(x)`. The result is clamped to `u32::MAX`. `0.1 * 60`
  is about `6.000000089` and is 6 ticks, not 7. A product of `6.0002` is 7 ticks.
- **Pulse.** `pulse: Option<Pulse { every: f32, on_apply: bool }>` (seconds > 0, at least 1 tick), only on
  `Timed`/`UntilRemoved` (`effect_pulse_on_instant`). The period uses the same tick conversion as a timed
  length, including the `1e-4` snap (`every: 0.1` at 60 Hz is 6 ticks). Each pulse runs the `base_edits`
  once as an instant edit.
- **Numbers.** `base_edits: Vec<Modifier>`: change `base` (the only list an `Instant` effect may use; on a
  pulsing effect, run on every pulse). `live_mods: Vec<Modifier>`: shape `current` while the effect lives
  (refused on `Instant`: `effect_instant_live_mods`).
- **Tags.** `status_tags: Vec<Tag>`: counted on the target while the effect lives and is not suspended.
  `kind_tags: Vec<Tag>`: describe the effect itself (matched by immunity, cleanses and removal queries; never
  given to the target).
- **Conditions.** `apply_if: Option<TagQuery>`: the target must match to apply. `active_if: Option<TagQuery>`:
  while the target does not match, the effect is **suspended**, not removed. `chance: f32` (0..=1, default 1):
  seeded roll per application (§ 7).
- **Interactions.** `immunity: Vec<TagQuery>`: while this effect lives, applications whose `kind_tags` match
  any query are refused. `cleanses: Vec<TagQuery>`: on success, live effects whose `kind_tags` match are
  removed. `grants: Vec<EffectGrant { ability, level, on_effect_end: Cancel | AfterRun | Keep }>`: abilities
  granted to the target while the effect lives.
- **Chain.** `chain: Chain { on_apply, on_lapse, on_removed: Vec<EffectRef> }`: further effects applied to the
  target after a successful application, after natural expiry, or after any other removal.
- **Stacking and suspension.** `stack: Option<StackRule>` (§ 2.5; `None` = every application is its own entry),
  `suspend: SuspendRule { freeze_clock: bool, on_resume: Continue | Restart | FireAndRestart }`.
- **Presentation.** `cues: Vec<CueBinding>` (§ 2.7).

**Application order** (target `T`, source `S`, level `L`; each step ends the attempt on failure and reports the
outcome):

1. `chance` roll (`failed_chance`).
2. `apply_if` against T's effective tags (`blocked_condition`).
3. Immunity of T's live effects (`blocked_immunity`; also writes `EffectRefused`).
4. Stacking (§ 2.5), skipped for `Instant` and for `stack: None`.
5. Amounts evaluated; `AtApply` reads captured.
6. `cleanses` run, then the new effect takes hold (its `base_edits` for `Instant`, or its entry in
   `AppliedEffects`). Cleansing first means an effect never removes itself.
7. `status_tags` counted, `grants` granted, `chain.on_apply` queued. Chained effects apply in the next
   effect pass of the same step, never recursively; a chain deeper than 4 stops with `chain_too_deep`.
8. `EffectApplied { target, id, outcome, stacks }` and the cues are written.

**Ending.** An effect expires when `remaining_ticks` reaches 0 at the end of a step, after any pulse due on
that tick has run. Reasons: `expired`, `removed` (call or query), `cleansed`, `restarted` (stack restart),
`source_gone`, `target_gone`. On any ending: `EffectEnded { target, id, reason }`, `status_tags` uncounted,
grants handled per `on_effect_end`, `chain.on_lapse` (expired) or `chain.on_removed` (any other reason) queued,
end cues written. A despawned source ends the effect only when the asset sets `end_with_source: true`.

**Suspension** (`active_if` false, or `set_suspended` from game code): live modifiers, status tags and pulses
stop; the remaining time keeps counting unless `freeze_clock` is set. When it lifts, `on_resume` decides the
pulse timer: `Continue` (keep the stored next pulse tick), `Restart` (next pulse one full period from now),
`FireAndRestart` (pulse now, then a full period).

### 2.5 Stacking

`StackRule` fields:

| Field | Meaning |
|---|---|
| `key: BySource \| ByTarget` | Which applications merge: one entry per `(asset, source)` or one per asset on the target. |
| `cap: u16` | Highest stack count, >= 1 (`stack_cap_zero` otherwise). |
| `on_add: OnAdd { time: Reset \| Keep \| Add, pulse: Restart \| Keep }` | What a successful merge does to the remaining time (back to full, unchanged, or plus the new length) and to the pulse timer. |
| `when_full: Reject \| RejectRenew \| Restart \| Spill(EffectRef)` | At the cap: refuse; refuse but renew time per `on_add.time`; end the entry (`restarted`) and apply fresh at 1 stack; or refuse and apply another effect to the target instead. |
| `on_lapse: EndAll \| PopOne \| Renew` | When time runs out: end the entry; drop one stack and renew the time (ending at 0); or renew the time with the count unchanged (only removal ends it). |
| `cue_each_stack: bool` | Whether every added stack writes a burst cue (default: only the first). |

A merge adds one stack (up to `cap`). The entry keeps its level unless the new application has a higher level;
then the entry takes the higher level and re-evaluates its amounts. Each count change writes
`StacksChanged { target, id, old, new }`.

### 2.6 Abilities

`AbilityAsset` fields:

- **Tags.** `kind_tags: Vec<Tag>` (describe the ability); `run_tags: Vec<Tag>` (counted on the owner while
  it runs); `cancels: Option<TagQuery>` (running abilities whose `kind_tags` match are cancelled when this one
  starts); `blocks: Option<TagQuery>` (while this one runs, abilities whose `kind_tags` match cannot start).
- **Requirements.** `owner_needs`, `owner_forbids` (against the owner's effective tags); `target_needs`,
  `target_forbids` (against each picked entity).
- **Price.** `cost: Option<EffectRef>` (an `Instant` effect; `ability_bad_cost` otherwise) and
  `cooldown: Option<EffectRef>` (a `Timed` effect with non-empty `status_tags`, which are the **cooldown
  tags**; `ability_bad_cooldown` otherwise). An ability can start only while the owner holds none of its
  cooldown tags and the cost would not take any attribute below `min` (a dry run through the edit gates).
  `charge: OnStart | Explicit`: pay cost and start cooldown when the run starts, or when the driver calls
  `charge` (a channel that may fail without paying).
- **Run data.** `run_state: ScratchOnly | KeepAcrossRuns | FreshEachRun`. `ScratchOnly` keeps only
  `Grant.scratch: [f32; 4]`. The other two hold a user type (`Reflect + Default`, registered by name on the
  plugin): one value per grant kept between runs, or a fresh default per run dropped at the end.
- **Starting.** `when_running: Reject | Restart | QueueOne` (a start request while already running).
  `triggers: Vec<Trigger>` with `Trigger::{Signal(Tag), TagGained(Tag), WhileTagged(Tag)}`: a matching
  `AbilitySignal` (hierarchy match), the owner gaining a tag, or "run while the owner holds the tag, end when
  it is lost". A grant may add `extra_triggers`. Every trigger start path that fails its check writes
  `AbilityLifecycle::Refused { cause, tag }` like any start (§ 2.6 table); a `WhileTagged` grant additionally
  caches its last cacheable refusal and skips the re-check (and with it the repeat message) while the cause
  is unchanged, reporting it only through `activation_check_count` in the meantime.
- **Other.** `max_level`, `cancelable: bool` (default true), `max_run_ticks: Option<u32>` (forces `end` after
  n ticks), `pick_timeout_ticks: Option<u32>`, `input_code: Option<u16>`, `run_site` (§ 2.8), `cues`.

**States.** `AbilityState::{Ready, Running, Ending, Cooling}`. `Cooling` is not stored: a `Ready` grant whose
owner holds one of its cooldown tags reads as `Cooling` (decision: derived, so there is one timer, the cooldown
effect; `play.wait_until` polls `abilities.inspect` like any other value).

| From | Event | To | Also |
|---|---|---|---|
| `Ready` | `activate`, check passes | `Running` | cancel matching runs, count `run_tags`, charge if `OnStart`, `AbilityLifecycle::Started` |
| `Ready` | `activate`, check fails | `Ready` | `AbilityLifecycle::Refused { cause, tag }` |
| `Running` | `charge` succeeds | `Running` | cost and cooldown applied, `Charged`; a failed charge returns `cannot_afford` and changes nothing |
| `Running` | `end` | `Ending` | `Ending { cancelled: false }` |
| `Running` | `cancel`, `cancelable` | `Ending` | `Cancelled`, then `Ending { cancelled: true }` |
| `Running` | `cancel`, not cancelable | `Running` | returns `not_cancelable` |
| `Ending` | next step boundary | `Ready` (reads `Cooling` while a cooldown tag is held) | `run_tags` uncounted, run data dropped per `run_state`, `Ended` |
| any | `revoke` | removed | cancels first if running; `Revoked` |

An ability still `Running` when its owner despawns is dropped with the owner. A grant revoked with
`defer: true` loses its input binding at once and is removed when its run ends.

**Activation check order** (the first failure is reported, with the blocking tag where there is one):
`not_granted`, `owner_locked` (the owner has the `AbilitiesLocked` component), `input_blocked`, `level_zero`,
`already_running` (per `when_running`), `owner_missing_tag`, `owner_forbidden_tag`, `blocked_by_ability` (a
running ability's `blocks` matches), `on_cooldown`, `cannot_afford` (the short attribute is reported),
`target_rejected`. `AbilitySettings.cheats { free_costs, no_cooldowns }` skip those two checks (dev tool).

**Granting.** `grant_ability` returns a `GrantId`. `GrantOptions { level, input_code, try_now, one_shot,
extra_triggers }`: `try_now` attempts a start at once and keeps nothing when the check fails (the call reports
the cause); `one_shot` removes the grant after its first run ends.

**Picks and signals.** `activate` may carry `Picks` and an `AbilitySignal`; both are stored on the run. An
ability that needs picks before charging enters `Running` with `awaiting_picks: true`; `submit_picks(grant,
Picks)` or `abort_picks(grant)` resolves it, `pick_timeout_ticks` ends it with `pick_timeout`, and the target
requirements are checked on submit (`target_rejected`). `send_signal` writes an `AbilitySignal`; in
`AbilitySet::Triggers` each signal is matched against the `Signal` triggers of the target's grants in
ascending `GrantId`, and every match goes through the normal start path with the payload attached. Unmatched
signals count in `AbilityStats.signals_unmatched`.

**Input.** `bind_input(grant, code)`; `AbilityInput { entity, code, down: bool }` messages are consumed in
`AbilitySet::Input`. A press starts every grant bound to the code in ascending `GrantId` (only the first that
starts when `AbilityBook.one_per_press` is set); a release writes `AbilityLifecycle::InputReleased` for running
grants on that code (a driver may end a channel). `block_input(entity, code, bool)` blocks a code. Touch and
gamepad reach codes through the normal `regia_input` action layer, never directly.

### 2.7 Cues

`CueBinding { tag: Tag, at: CueAt, params: CueParams }` with `CueAt::{Applied, Pulsed, Removed, Sustained}`
links a cue tag (by convention under `Cue.`) to an effect or ability moment (not to be confused with the
`regia_audio` sound-cue asset, which a cue consumer may play). The module writes
`CueEvent { entity, tag, kind: CueKind, source, amount, location: Option<Vec3>, normal: Option<Vec3>,
effect: Option<EffectId> }` with `CueKind::{Burst, Begin, End}`: `Applied`, `Pulsed` and `Removed` bindings
write a `Burst`; a `Sustained` binding writes `Begin` when the effect starts and `End` when it ends, with the
stack count in `amount`. Abilities bind cues to `Started` and `Ended` the same way.

- Cues only describe. No simulation system reads `CueEvent`. Consumers (VFX, audio, camera shake through
  `regia_vfx::CameraShakeRequest`, UI) are ordinary `Update` systems. Dropping every `CueEvent` changes no
  attribute, tag or state (test 16).
- `AbilitySettings.cues_enabled = false` silences all cues; `AbilitySettings.cue_filter: Option<fn(Entity,
  &World) -> bool>` can suppress per entity at write time.
- Cue order is the order of the state changes that caused them (§ 7).

Decision (ADR-1585): no cue router in v1. A data table mapping cue tags to VFX, sound and shake assets is a
follow-up (§ 11): only `CameraShakeRequest` exists today, so a router would have to invent request types in
`regia_vfx` and `regia_audio`. Games read `CueEvent` directly.

### 2.8 Networking hooks (reserved, not implemented)

Nothing here sends bytes. State-changing calls take an `IntentSeq` (0 when unused), stored on the
`AppliedEffect`, the run and the resulting messages. Randomness uses the seeded streams of § 7; assets have
stable ids (path hash) and live entries per-entity ids, so a later `regia_net` layer can send "apply effect X at
tick N with intent K" instead of state. `AbilityAsset.run_site: OwnerOnly | OwnerPredicts | HostStarts |
HostOnly` is parsed, validated and stored; until `regia_net` lands every ability runs as `HostOnly`. A rollback
copies `Attributes`, `AppliedEffects`, `AbilityBook`, `TagCounts` and `AbilityCounters` (plain `Clone` data).

## 3. Rust API

### 3.1 Crate, modules, layering

Crate `regia_framework` (Gameplay layer), module `abilities` with submodules `attributes`, `modifiers`,
`effects`, `stacking`, `grants`, `cues`, `assets`, `migrate`. It uses `regia_app`, `regia_ecs`, `regia_core`,
`regia_scene` (`EntityGuid`), `regia_input` and the sibling `tags`, `damage` and (when built) `curve_table`
modules, all already dependencies of the crate: **no new crate, no new layer edge**. `regia_agent` already
depends on `regia_framework` (allowed same-layer edge), so the agent handlers live in
`regia_agent::abilities`. `smallvec` is already a dependency of `regia_framework`; no new dependency.

### 3.2 Key types and functions (sketch)

```rust
pub struct AttrId(pub u64);
pub enum ModOp { AddPre, Percent, Scale, AddPost, Replace { priority: i16 } }
pub struct EffectId(pub u64);
pub struct GrantId(pub u32);
pub struct IntentSeq(pub u32);
pub enum AbilityState { Ready, Running, Ending, Cooling }

#[derive(Component, Reflect, Clone, Default)] pub struct Attributes { /* Vec<Attribute> */ }
#[derive(Component, Reflect, Clone, Default)] pub struct AppliedEffects { /* Vec<AppliedEffect> */ }
#[derive(Component, Reflect, Clone, Default)] pub struct AbilityBook { /* Vec<Grant>, one_per_press */ }
#[derive(Component, Reflect, Clone, Default)] pub struct TagCounts { /* Vec<(Tag, u16)> */ }
#[derive(Component, Reflect, Clone, Default)] pub struct AbilitiesLocked;
// Attributes::{get (current), base, set_base -> Result<(), AbilityError>}

pub struct ApplyEffect { pub asset: Handle<EffectAsset>, pub source: Option<Entity>, pub target: Entity,
    pub level: u32, pub params: SmallVec<[(Tag, f32); 4]>, pub intent: IntentSeq }
pub fn apply_effect(world: &mut World, req: ApplyEffect) -> Result<ApplyOutcome, AbilityError>;
pub fn remove_effect(world: &mut World, target: Entity, id: EffectId) -> Result<(), AbilityError>;
pub fn remove_effects_matching(world: &mut World, target: Entity, q: &TagQuery, limit: Option<u32>) -> u32;
pub fn set_suspended(world: &mut World, target: Entity, id: EffectId, on: bool) -> Result<(), AbilityError>;

pub fn grant_ability(world: &mut World, owner: Entity, asset: Handle<AbilityAsset>, opts: GrantOptions)
    -> Result<GrantId, AbilityError>;
pub fn revoke_ability(world: &mut World, owner: Entity, g: GrantId, defer: bool) -> Result<(), AbilityError>;
pub fn check_activation(world: &World, owner: Entity, g: GrantId, ctx: &StartContext) -> Result<(), Refusal>;
pub fn activate(world: &mut World, owner: Entity, g: GrantId, ctx: StartContext) -> Result<(), Refusal>;
pub fn charge(world: &mut World, owner: Entity, g: GrantId) -> Result<(), Refusal>;
pub fn end(world: &mut World, owner: Entity, g: GrantId) -> Result<(), AbilityError>;
pub fn cancel(world: &mut World, owner: Entity, g: GrantId) -> Result<(), AbilityError>;
pub fn submit_picks(world: &mut World, owner: Entity, g: GrantId, picks: Picks) -> Result<(), Refusal>;
pub fn abort_picks(world: &mut World, owner: Entity, g: GrantId) -> Result<(), AbilityError>;
pub fn send_signal(world: &mut World, signal: AbilitySignal);
pub fn effective_has(world: &World, e: Entity, tag: Tag) -> bool;
pub fn effective_matches(world: &World, e: Entity, q: &TagQuery) -> bool;
pub fn explain_attribute(world: &World, e: Entity, a: AttrId) -> Option<AttributeBreakdown>;
```

`remove_effects_matching` matches `kind_tags` and ends with reason `removed`. `AttributeBreakdown` holds the
base, each phase with every contributing modifier `{ effect, op, amount, stacks, contribution }`, the value after
each phase, the `Replace` winner and the clamp applied.

`AbilityError` is a `thiserror` enum with `fn reason(&self) -> &'static str` returning the § 5.2 reason.
`Refusal { cause: RefusalCause, tag: Option<Tag>, attribute: Option<AttrId> }`. `StartContext { picks:
Option<Picks>, signal: Option<AbilitySignal>, intent: IntentSeq }`. The `&mut World` functions are the entry
points for exclusive systems, the agent and tests; ordinary systems write request messages (`ApplyEffect`,
`StartAbility`, `CancelAbility`, `AbilitySignal`, `AbilityInput`), consumed per kind in message order (ADR-7573).

### 3.3 Plugin and registration

`AbilityPlugin` registers the types; `EffectAsset`, `AbilityAsset` and `AttributesAsset` as assets with loaders
for `.effect.ron`, `.ability.ron` and `.attrs.ron`; the resources `AbilitySettings`, `AbilityCounters`,
`AbilityStats`, `LevelCurves` (built-in inline implementation), `EditGates`, `MagnitudeFns`, `RunStateTypes`;
the messages above; and `AbilitySet`. It adds `TagPlugin` if absent. Builder methods:
`AbilityPlugin::default().gate(fn).magnitude_fn("name", fn).run_state::<T>("name")`.

Decision (ADR-1585): registrations made by a game plugin (edit gates, magnitude functions, run-state types)
are stored with the registering plugin's owner id. Hot reload (spec 20 § 3 steps 6 and 7) drops the old
plugin's entries at teardown and the new library registers again; no function pointer into an old library
survives. A missing entry after reload reads as `magnitude_fn_missing` (amount 0) or as "no gate", never a
crash. Ability assets reference functions and run-state types by name, never by pointer.

### 3.4 Schedule

All simulation runs in `FixedUpdate`, in the chained `AbilitySet` sets, before `ScriptsSet::FixedUpdate` and
before `DamageSet`:

`Input` -> `Requests` (apply/start/cancel/revoke messages: each kind in message order, the kinds in the
fixed order apply -> start -> cancel -> revoke — each kind is its own message queue, so a cross-kind write
order cannot exist and is not observable; ADR-7573) -> `Triggers` (signals, tag
triggers) -> `Effects` (suspension, pulses, expiry, stack lapses, chained effects) -> `Recompute` (derived
values, modifier evaluation, clamps, tag counts) -> `States` (`Ending` -> `Ready`, `max_run_ticks`,
`pick_timeout_ticks`) -> `Notify` (`AttributeChanged`, `EffectiveTagsChanged`, lifecycle messages,
`CueEvent`).

Requests written by scripts or by systems after `AbilitySet` are consumed on the next step (one step of
latency, always the same). `Update` systems (VFX, UI) only read messages and components.

### 3.5 Bridge to `Health`

`HealthBridge` (optional plugin) is for entities that carry both `Attributes` with `health` and `health_max`
and a `Health` component; the bridge inserts the marker `HealthFromAttributes` on them. Each step in
`AbilitySet::Requests` it turns each `DamageEvent` for a marked entity into an instant `AddPre` base edit of
`-amount` on `health` (through the edit gates), and in `Notify` writes `Health { current, max }` from the
attributes; when `health` reaches 0 from above it inserts `Died` and writes `DiedEvent { entity, killer }`
exactly as `apply_damage` does. A negative `DamageEvent` (a heal) still changes `current` after that
and does not remove `Died`. `Health::is_dead` follows `current`, so it can be false while `Died`
remains; `HealthBridgeState.alive` follows `current > 0` the same way. A later hit that reaches zero
from above raises `DiedEvent` again. Deaths of one step are emitted in the § 7 entity order (ascending
`EntityGuid`, then bits), not archetype-table order, so simultaneous deaths are deterministic across spawn
order and seeks (ADR-4756). Baseline change: `regia_framework::damage::apply_damage` skips entities with
`HealthFromAttributes` (one `Without<_>` filter), so a hit is never applied twice. A game that keeps health only
in `Attributes` does not need the bridge.

## 4. Files and formats

### 4.1 Formats

| Extension | Format id (`cargo xtask check-formats`) | Version | Doc |
|---|---|---|---|
| `.attrs.ron` | `attrs` (ADR-1695) | 1 | `docs/formats/attrs-ron.md` |
| `.effect.ron` | `effect` | 1 | `docs/formats/effect-ron.md` |
| `.ability.ron` | `ability` | 1 | `docs/formats/ability-ron.md` |

All are RON, written canonically (sorted map keys, shortest round-trip floats); the asset structs are
`Reflect + Serialize + Deserialize` with `format_version` first, `#[serde(default)]` on optional fields and
`deny_unknown_fields` on the top level (typos reported with a span). New formats start at
`format_version: 1`. A higher version is refused (`format_too_new`). Frozen-format rule (spec 12 § 4a): any
shape change bumps the version, ships a migrator in `regia_framework::abilities::migrate`, a row in spec 12
§ 4a, and `regia upgrade-project` support. Cook writes postcard with tags as `u64` ids plus a name table and
curve references resolved; the cooked form never holds an unresolved name. Not built yet (ADR-5639): the runtime
reads the RON files directly; the postcard cook is a v0.5.x gap. The lead registers the formats
(`cargo xtask check-formats --update`) and regenerates the manual (`cargo xtask manual`).

### 4.2 `.attrs.ron`

```ron
(format_version: 1, description: "Hero stats", attributes: [
  (name: "health_max", base: 100.0, min: 1.0, max: 9999.0),
  (name: "health", base: 100.0, min: 0.0, max: 9999.0, max_from: Some("health_max")),
  (name: "stamina", base: 50.0, min: 0.0, max: 100.0),
  (name: "strength", base: 10.0, min: 0.0, max: 100.0),
  (name: "move_speed", base: 4.0, min: 0.0, max: 12.0),
  (name: "armor", base: 0.0, min: 0.0, max: 1000.0,
   derived: Some((inputs: [("strength", 2.0)], offset: 0.0))),
])
```

Names match `[a-z][a-z0-9_]{0,47}`; one flat namespace per entity.

### 4.3 `.effect.ron`

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

### 4.4 `.ability.ron`

Fields as § 2.6. References to effects and abilities are asset paths (stable ids). `cost` must name an `Instant`
effect and `cooldown` a `Timed` effect with non-empty `status_tags`.

```ron
(format_version: 1, name: "fire_bolt", kind_tags: ["Ability.Spell.Fire"], run_tags: ["State.Casting"],
 blocks: Some(Has("Ability.Spell")), owner_forbids: Some(Has("Status.Silenced")),
 cost: Some("effects/mana_10.effect.ron"), cooldown: Some("effects/cd_fire_bolt.effect.ron"),
 charge: OnStart, run_state: ScratchOnly, when_running: Reject, triggers: [], max_run_ticks: Some(120),
 input_code: Some(1), run_site: HostOnly, cues: [(tag: "Cue.Cast.Fire", at: Applied)])
```

### 4.5 Validation

The loaders and `abilities.validate` report: `attribute_duplicate`, `attribute_cycle`, `attribute_range`,
`effect_unknown_attribute`, `effect_bad_lifetime`, `effect_instant_live_mods`, `effect_pulse_on_instant`,
`stack_cap_zero`, `undeclared_tag` (against the `TagRegistry`), `curve_unsorted`, `reference_missing`,
`ability_bad_cost`, `ability_bad_cooldown`, `unknown_run_state`, and the warnings `modifier_negative_scale`,
`ability_no_max_run_ticks`. A modifier naming an attribute the target lacks never creates a hidden attribute:
applying the effect skips that modifier and logs `unknown_attribute` once per application. Wiring these checks
into the asset validator registry (`regia_asset_pipeline::validators`, ADR-1568) is a follow-up (§ 11).

### 4.6 Save games

`Attributes`, `AppliedEffects`, `AbilityBook` and `TagCounts` are reflected and saved by the baseline save path
on `Saveable` entities; `AbilityCounters` is registered as a saveable resource; remaining time is stored in ticks. Because the save path does not
remap `Entity` fields yet, `AppliedEffect.source` and the stored picks are `#[reflect(skip_serializing)]`: a
loaded effect has no source (`end_with_source` does not fire) and a loaded run has no picks. Proper remapping
follows the save-game entity remapping work; then these fields serialize, which is a shape change handled by
that work's version bump. A loaded `AbilityBook` takes its tag baseline on the first `Triggers` pass after
the load without `TagGained` events (`seen_tags` is reflect-ignored, like the step caches); `WhileTagged`
triggers evaluate the held tags directly, so they start on that same pass. Losing and regaining a tag after
the baseline fires `TagGained` normally.

## 5. Agent surface

### 5.1 Methods

Registered by `regia_agent::abilities::install_ability_methods(world)` on the world's `AgentExtension` (the
pattern of `regia_agent::cloth`); `AgentPlugin` calls it for every app with the agent (ADR-1809). Without `AbilityPlugin` every method answers `{ available: false }`. Entity
params are entity ids as in spec 23.

| Method | Class | Params | Result |
|---|---|---|---|
| `abilities.list` | read | `{ entity? }` | entities with `Attributes`/`AbilityBook`: attribute count, live effect count, grants with state |
| `abilities.inspect` | read | `{ entity, attribute? }` | attributes `{ name, base, current, min, max, derived }`; live effects `{ id, asset, level, source, stacks, remaining_ticks, next_pulse_tick, suspended, status_tags, applied_tick }`; effective tags with counts per source; grants `{ id, asset, level, state, input_code }`; per attribute the breakdown of § 3.2 |
| `abilities.check` | read | `{ entity, grant }` | `{ ok, cause?, tag?, attribute? }` |
| `abilities.grant` | play | `{ entity, ability, level?, input_code?, try_now?, one_shot? }` | `{ grant }` or `{ grant: null, cause }` |
| `abilities.revoke` | play | `{ entity, grant, defer? }` | `{ revoked }` |
| `abilities.activate` | play | `{ entity, grant, picks?, signal? }` | `{ started, cause?, tag?, state }` |
| `abilities.charge` | play | `{ entity, grant }` | `{ charged, cause? }` |
| `abilities.end` | play | `{ entity, grant }` | `{ state }` |
| `abilities.cancel` | play | `{ entity, grant }` | `{ cancelled, state }` |
| `abilities.signal` | play | `{ entity, tag, source?, amount? }` | `{ started: [grant] }` |
| `abilities.apply_effect` | play | `{ target, effect, source?, level?, params?: { tag: f32 } }` | `{ outcome, id?, stacks? }` |
| `abilities.remove_effect` | play | `{ entity, id }` or `{ entity, query }` | `{ removed }` |
| `abilities.set_base` | edit_scene | `{ entity, attribute, base }` | `{ old, new }` (clamped; an undo step in the editor) |
| `abilities.add_attributes` | edit_scene | `{ entity, asset }` | `{ attributes }` |
| `abilities.cues` | read | `{ since_tick?, limit? }` | recent `CueEvent`s from a ring of 256 |
| `abilities.validate` | read | `{ path }` | diagnostics `{ code, severity, message, span? }` |
| `abilities.stats` | read | `{}` | `AbilityStats`: entities, live effects, applications, removals, modifier evaluations last step, `signals_unmatched` |
| `abilities.state_hash` | read | `{}` | `{ available, hash, quantum }`: the § 7 cross-platform hash as 16 hex digits (ADR-2012) |

Classes map to agent permissions: read = `Permission::Read`, play = `Permission::Play`, edit_scene =
`Permission::EditScene`. A refused activation or application is a normal result (`started: false`, `cause`),
not an error. Effects advance only in `FixedUpdate`: step with `play.step`; two `apply_effect` calls in one tick
resolve in call order.

### 5.2 Errors

Protocol errors follow the neighboring built modules (`regia_agent::post_volumes`): `F0002` (bad params) or
`F0003` (not found) with `data.reason` set to one of: `entity_not_found`, `no_attributes`, `unknown_attribute`,
`attribute_derived`, `attribute_not_finite`, `effect_not_found`, `grant_not_found`, `asset_not_found`,
`not_cancelable`, `bad_query`, `format_too_new`, `reference_missing`. Refusal causes (results, not errors):
`not_granted`, `owner_locked`, `input_blocked`, `level_zero`, `already_running`, `owner_missing_tag`,
`owner_forbidden_tag`, `blocked_by_ability`, `on_cooldown`, `cannot_afford`, `target_rejected`,
`pick_timeout`. Application outcomes: `applied`, `stacked`, `failed_chance`, `blocked_condition`,
`blocked_immunity`, `rejected_full`, `spilled`, `limit_reached`, `chain_too_deep`. All are lowercase ascii and
`_` and are listed on the book page.

### 5.3 Book page and coverage

Page `book/agents/abilities.md`: a worked fire-bolt-with-burn example (grant, `abilities.check` refusing with
`on_cooldown`, activate, `play.step`, `abilities.inspect` showing the breakdown, `abilities.cues`, `play.expect`
on attribute values), every method with params and results, every reason, cause and outcome. `book/COVERAGE.md`
§ 2: one row per method (`| abilities.list | abilities_list | [abilities](agents/abilities.md) |`, ...); § 4: rows
for the three formats. Every method needs a schema row and passes `cargo xtask agent-coverage`.

## 6. Editor surface

The agent surface is primary. The inspector shows `Attributes` (base editable, current with the breakdown on
hover), `AppliedEffects` (table with a remove button) and `AbilityBook` (grant/revoke; start button in play).
The assets open in the reflection form editor with live validation. Debug overlay group `abilities` (off by
default): live-effect icons above the entity and a state ring (ready green, running yellow, cooling blue).

## 7. Determinism

- All logic runs in `FixedUpdate`; lengths, pulses and cooldowns are integer ticks (`ceil` from seconds at
  application). No wall clock.
- Order: requests per kind in message order, the kinds in the fixed order apply -> start -> cancel ->
  revoke (ADR-7573); entities in ascending `EntityGuid` (entities without one after those with
  one, by `Entity` bits); effects on one entity by `EffectId`; grants by `GrantId`; messages and cues in
  generation order. No `HashMap` iteration affects results (sorted `Vec`s or `BTreeMap`).
- `chance` rolls use `regia_core::math::Rng::with_stream(AbilitySettings.seed ^ target_key, effect asset id)`,
  where `target_key` is the low 64 bits of the target's `EntityGuid`. A target without a guid refuses
  deterministically (`failed_chance`) instead of rolling, so a missing guid cannot change accept versus reject
  between spawn orders (ADR-4756); the stream position per `(target, asset)` is kept in `AbilityCounters`
  (saved) and advances exactly once per roll. The generator of a live row is kept advanced in place by a
  transient, unsaved cache (a mismatch — a load, a host edit — falls back to the replay once), and the
  recompute pass drops rows whose key matches no live `EntityGuid`, so a streaming world does not keep one
  row per dead target forever, in memory or in the save; a guid that returns after its rows were pruned
  starts a fresh stream (ADR-7573).
- **Bit-exact on one platform**: tick counts, stack counts, states, tag counts, ids, message order and
  `current` values. Evaluation uses IEEE `+ - * /` and repeated multiplication only (no `powf`, no
  `mul_add`, no libm). **Within tolerance across platforms** (`1e-5` relative): `Function` amounts that call
  libm. The cross-platform state hash folds counts exactly and attributes quantized to `1e-4`.

## 8. Tests and acceptance (no GPU)

Unit tests in `regia_framework/src/abilities/*`, integration tests in `regia_framework/tests/abilities.rs`
with a headless `App`, `MinimalPlugins` and `enable_fixed_step(&mut app, 60.0)`. Files go through
`regia_test_utils::unique_temp_dir`. No sleeps, no timing asserts.

1. **Op order.** Base 10, `AddPre +5`, `Percent +0.5` and `+0.25`, `Scale 2` and `1.5`, `AddPost -3` gives
   `((10+5) * 1.75) * 3 - 3 = 75.75`. Two `Replace`s of priority 1 and 2 in either insertion order give the
   priority-2 value; equal priority resolves by applied tick, then `EffectId`.
2. **Clamps and derived.** `current` never leaves `[min, max]`; lowering `health_max` below `health` lowers
   `health` in the same step; a cyclic `max_from` fails with `attribute_cycle`. `armor = 2 * strength`: a timed
   `Scale 2` on `strength` doubles `armor` the same step; an instant edit to `armor` fails (`attribute_derived`).
3. **Base and current.** An instant `-30` on `health` persists after a timed buff ends; a timed `AddPre +20`
   disappears and `base` is unchanged.
4. **Ticks.** `Timed(1.0)` at 60 Hz ends on the 60th step after application. With `pulse (every 0.25,
   on_apply: true)` the edits run at ticks 0, 15, 30, 45 and 60 (the pulse on the expiry tick runs before
   removal): exactly 5, pinned by tick. With `on_apply: false`: 4.
5. **Pulse catch-up.** A 1-tick pulse with `next_pulse_tick` set 20 ticks behind runs 8 times in one step,
   then logs `pulse_overrun`.
6. **Stacking matrix.** An explicit table over `key x when_full x on_add.time x on_lapse` (at least 24 rows):
   stack count, remaining ticks and the `StacksChanged` sequence per row equal an expected list in the test
   file. `PopOne` loses one stack per length and ends at 0; `Renew` never ends alone.
7. **By source.** Two sources apply a `BySource` poison: two entries that end independently; `ByTarget` merges.
8. **Counted tags.** Two effects add the same status tag; when the first ends `effective_has` stays true; after
   the second it is false; `EffectiveTagsChanged` reports exactly one add and one remove.
9. **Immunity, cleanse, conditions.** Immunity refuses with `blocked_immunity`; `cleanses` removes by kind tag
    before the new effect's tags land; `active_if` suspends and resumes under each `on_resume` (pulse tick
    asserts) and with `freeze_clock`.
10. **Amount sources.** `Curve` interpolates inline curves; a missing curve gives 0 and `curve_missing` without
    panic; `Param` uses the caller value and the default; `Stat` with `AtApply` ignores later changes and
    `Live` follows them; an unregistered `Function` gives 0 and `magnitude_fn_missing`.
11. **States.** Every row of the § 2.6 table with its messages in order; every refusal cause from a crafted
    setup with the reported tag; a failed cost leaves attributes unchanged; `Explicit` charge pays only when
    charged.
12. **Cancel and block.** Starting B cancels running A when B's `cancels` matches A; A's `blocks` stops C while
    A runs, and C can start the step after A ends.
13. **Run state.** `FreshEachRun` gives a default value per run; `KeepAcrossRuns` keeps its value; `ScratchOnly`
    keeps only `scratch`.
14. **Signals.** A child tag starts a parent-tag `Signal` trigger; no match increments `signals_unmatched`;
    `extra_triggers` widen matching; two matching grants start in `GrantId` order.
15. **Picks.** Submit resolves a pending pick; abort ends the run; `pick_timeout_ticks` ends it with
    `pick_timeout`; a bad pick is refused with `target_rejected`.
16. **Cues are inert.** Scenario A consumes cues, scenario B discards every `CueEvent`; `snapshot_hash` equal
    after 600 steps.
17. **Determinism.** 50 entities with a `Name` and an `EntityGuid`, 20 effects each, applications drawn from a
    seeded `Rng` and addressed by name: two runs, and a run with the spawn order reversed, give the same
    `snapshot_hash` after 1,000 steps (`world_snapshot` keys entities by `Name`; `AbilityCounters` is added with
    `insert_resource_text`), including `chance` effects.
18. **Formats.** RON round trip of every asset and of runtime state is byte-identical on resave; a
    `format_version: 2` file fails with `format_too_new`; an unknown field fails with a span.
19. **Health bridge.** A `DamageEvent` on a bridged entity lowers `health` once (not twice), a shield gate
    halves it, and reaching 0 raises one `DiedEvent`.
20. **Hot reload registration.** Dropping the owner's registrations and registering again leaves no stale entry;
    a missing function reads as 0 (unit test on the registry, no dylib).
21. **Agent.** In the `regia_agent` test suite: `abilities.grant` -> `abilities.check` (`on_cooldown`) ->
    `abilities.activate` -> `play.step` -> `abilities.inspect` (breakdown lists each modifier and the value after
    each phase) -> `abilities.cues`; the no-plugin answer `{ available: false }`; each `data.reason` once.
22. **Budget counter.** 1,000 entities x 8 effects: `AbilityStats` reports at most 8,000 modifier evaluations in a
    step, and 0 for idle entities after warm-up (dirty flag, § 10). Counters only, no wall time.
23. **Lead TODO (needs a device and a golden):** a `samples/` scene with a hit-and-burn cue spawning VFX and
    sound; any golden image is made by the lead.

## 9. Platform notes

Pure CPU, no platform code: Linux, Windows and Android aarch64 build the same source; macOS is testing-only and
needs nothing new. Avoid `f32::mul_add` and libm in evaluation paths (§ 7). Android: keep per-step scratch in
preallocated `Vec`s held in `Local` state (no per-step allocation after warm-up); the cue ring and message queues
are bounded (§ 10).

## 10. Budgets

Min-spec class (4-core x86 laptop or mid-range aarch64 phone), per 60 Hz fixed step:

| Item | Budget |
|---|---|
| Entities with `Attributes` | 1,000 |
| Live effects per entity | soft 32, hard 128 (`limit_reached`) |
| Live effects total | 8,000 typical, 32,000 hard |
| Modifiers per effect / attributes per entity / grants per book | 16 / 64 / 32 |
| `AbilitySet` total | <= 0.5 ms for 1,000 entities x 8 effects; <= 0.1 ms when nothing changed |
| Memory | <= 256 bytes per `AppliedEffect`; <= 64 KiB for 1,000 idle entities |
| Messages per step | 4,096; `CueEvent` ring of 256 kept for the agent |

An entity with no live effect, no suspension change, no base edit and no tag change since the last step is
skipped through a dirty flag; a skipped entity holds exactly the value a full recompute would give (tested).
As built (ADR-2012): budgets over file contents are warnings (`effect_over_budget`, `attributes_over_budget`) so old files keep
loading; `grant_limit` refuses the 33rd grant; `effects_over_soft_limit` warns past 32 live effects; `message_budget` drops the tail
of a burst past 4,096 messages of one request kind in a step. The § 7 hash is `abilities_state_hash` (`abilities.state_hash`).
The dirty flag also skips an entity with live effects whose modifiers are all fixed (no `when`, no `Live` stat read) when nothing
wrote its effects (a write of clocks alone aside), its attributes or the assets since the last recompute; `entities_skipped`
counts it. Release, lane host: 1,000 x 8 untimed 0.30 to 0.36 ms, timed 0.35 ms, idle 68 us.
Memory as built (ADR-2426): 1,000 idle entities cost the ability system 29,700 bytes (debug, the build server; it was 317,428),
also when each has a health of its own. Each pass keeps its § 7 order as the sorted entities alone, checked each step to
still be exactly the query's entities in ascending key order; entities that report the same values share one
`AttributeChanged` snapshot, with at most one row of their own; `Attributes` did not grow (96 bytes).
Tracing spans: `abilities_requests`, `abilities_triggers`, `abilities_effects`, `abilities_recompute`,
`abilities_states`, `abilities_notify`, and `abilities_apply_effect` per call. Wall-time budgets are measured
by the lead on min-spec hardware, not asserted in tests.

## 11. Out of scope and follow-ups

- Network replication, rollback and prediction (`regia_net`; § 2.8 reserves the hooks).
- An async helper layer for ability logic (wait for a montage end, a delay, a tag, an input); v1 drivers are
  ordinary systems using `timers` and messages.
- A cue router (tag -> VFX/sound/shake table), which needs request messages in `regia_vfx` and `regia_audio`.
- A behavior-tree / state-tree task that starts spec 53 abilities (needs the `regia_ai -> regia_framework` edge).
- An `ability_assets` validator in `regia_asset_pipeline::validators` (ADR-1568).
- A visual ability graph; a curve editor (spec 51); pick producers such as projectiles and areas (games call
  `submit_picks`); effect icons and text (game data); a spatial index for area effects.

## 12. Provenance

Algorithm sources (public sources only; the shared row is `docs/provenance.md`, "Abilities, attributes, effects spec"):

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Modifier phases | `AddPre`, then `Percent`, then `Scale`, then `AddPost`, then `Replace`, recomputed from scratch each tick | Common RPG stat-stacking practice; Nystrom, *Game Programming Patterns* (2014), "Component" and "Dirty Flag" chapters |
| Fixed-step timing | Integer-tick lengths, pulses and cooldowns | Fiedler, "Fix Your Timestep!" (gafferongames.com, 2004); Gregory, *Game Engine Architecture* (game loop chapter) |
| Ability states | Ready/running/ending with requirement, cost and cooldown checks | Nystrom, *Game Programming Patterns*, "State" chapter |
| Counted tags | Reference-counted tag grants so overlapping sources do not clobber each other | Standard reference counting; ADR-1463 left counts for this item |
| Intent sequence | Client intent numbered so the host's answer can be matched | Bernier, "Latency Compensating Methods in Client/Server In-game Protocol Design and Optimization" (GDC 2001); Fiedler, "Networked Physics" series |
| Seeded streams | Per-target random streams independent of iteration order | O'Neill, "PCG: A Family of Simple Fast Space-Efficient Statistically Good Algorithms for Random Number Generation" (2014) |

### Owner decisions

None.

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
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. As built: target_gone and the cooked form (ADR-7216, sprint post05-anim-abilities)

- § 2.4 `target_gone` is built (ADR-7216 (7)): an `on_despawn` hook on `AppliedEffects` records the live effects of a
  despawning target; the start of the next effect pass writes `EffectEnded { reason: target_gone }` per effect (§ 7
  order, then id) and the end cues on the dead entity; chains are skipped. `AbilityStats.ended` counts endings by
  reason (`abilities.stats`).
- § 4.1 the cooked form is built as `abilities-cooked` v1 (ADR-7216 (8)): a `u32` version and postcard of
  `CookedAbilityAsset`; `AbilityAssets::cook_text` / `load_cooked` share the text path's validation. Refined: tags are
  carried as names (no `u64` ids or name table) and level-curve references stay by name, resolved when sampled. It is
  not yet wired into the pak cook (no runtime pak loader for ability assets).

## 14. As built: the pak cook and the runtime loader (ADR-9370, sprint post05w3-montage-net-abilities-cook)

- § 4.1 the cook is wired: `regia_asset_pipeline::data_cook` cooks `.attrs.ron`, `.effect.ron` and `.ability.ron`
  (importer `abilities`, type path `regia_framework::abilities::CookedAbilityAsset`) with the files their cost and
  cooldown reach, in load order (`abilities::load_order`, `cook_ability_set`); a missing reference fails the cook with
  `abilities_reference_missing`, a loop with `abilities_reference_cycle` (every asset on it), a named file that does
  not cook with `abilities_reference_invalid`; run-state type checks are left to the game's load. The runtime loader
  `load_ability_assets` / `App::load_ability_assets_at_startup` reads the pak's cooked form first in a packaged game
  (paks mounted) and the loose RON in a dev tree. `regia package` ships no raw ability files (ADR-9850 (c), which replaced the raw copy). Tests:
  `crates/regia_asset_pipeline/tests/abilities_cook.rs`, `crates/regia_framework/tests/abilities_load.rs`.

## 15. As built: the typed asset form (ADR-7472, sprint w8-inspector-forms)

- § 6 the Abilities inspector's asset editor opens `.attrs.ron`, `.effect.ron` and `.ability.ron` in a typed form
  with live validation: one row per top-level field (nested records and lists below it), each validation message
  beside the field it names, the rest under the form; the raw RON text is the Advanced toggle and is shown while the
  text does not parse. Refined: the form walks the format's serde value (the ability formats are serde types without
  reflection), not the data-asset reflection form. A typed edit writes the format's canonical text, so it saves the
  bytes a text edit with the same change saves; Save stays refused with `abilities_panel_invalid_asset` while an error
  remains. Agent surface: `editor.abilities_panel_asset` `field` + `value`, answer `form` and per-diagnostic `field`,
  codes `abilities_panel_parse` and `abilities_panel_no_field` (`book/agents/editor-data-panels.md`). Content Browser
  New and `editor.create_asset` make each kind at its current version. Tests:
  `crates/regia_editor/src/abilities_panel/form_tests.rs`.
