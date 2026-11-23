# AI: utility reasoners

Utility AI is the third decision model in `regia_ai` next to behavior trees and state trees
(`regia_ai::utility`). A **reasoner** holds any number of named *actions*; each decision it
scores every action from any number of *considerations* — a blackboard key, normalised to
`[0, 1]` by an authored min/max range, mapped through a *response curve* — and picks a winner.
The architecture (considerations, response curves, the number-of-considerations make-up value,
weighted random among the top scorers) is the published infinite-axis utility system of Dave
Mark and Mike Lewis; see `docs/provenance.md`.

## The scoring pipeline

For each action, in declaration order:

1. every consideration reads its key from the agent's `Blackboard` (losslessly — a missing key,
   a value of the wrong type or a non-finite float scores 0 and is flagged `valid: false` in the
   report);
2. each curve output gets the **make-up value** for the action's consideration count `n` (Mike
   Lewis and Dave Mark's compensation factor, GDC 2015): `m = 1 - 1/n`,
   `make_up = (1 - s) * m`, `compensated = s + make_up * s` — the identity for one consideration;
   a consideration scoring 0.5 compensates to `0.5, 0.625, 0.667, 0.6875, 0.7` for 1..5
   considerations;
3. the compensated outputs are multiplied together (an action with no considerations scores 0 and
   is never selected). Five considerations that each score 0.9 multiply to 0.59 raw — less than a
   single consideration scoring 0.8 — but to `0.972^5 = 0.87` compensated, so they win;
4. the compensated score is scaled by the action's `weight`;
5. the current action gains its `momentum` bonus (commitment): another action must exceed it by
   more than the bonus to take over (an exact tie keeps the current action);
6. an action whose cooldown has not elapsed is blocked until its time comes (the `now` passed to
   the decision, never a wall clock). A non-finite `now` blocks every action (`InvalidTime`):
   nothing is selected and no state changes.

Selection is `Best` (highest score, ties to declaration order) or `WeightedRandom` — a draw
from a seeded SplitMix64 stream among the top `top_n` candidates within `margin` of the best,
weighted by score.

## Response curves

Every curve takes `slope`, `exponent`, `x_shift`, `y_shift` (defaults `1, 2, 0, 0`) and is
clamped to `[0, 1]`; non-finite input reads as 0. Documented values at `x = 0, 0.5, 1` with all
parameters at their defaults are in the `ResponseCurve` rustdoc and are pinned by tests.

| kind | formula on `t = x - x_shift` |
|---|---|
| `Linear` | `slope * t + y_shift` |
| `Quadratic` | `slope * t^exponent + y_shift` (signed for odd integer exponents) |
| `Logistic` | `y_shift + slope / (1 + e^(-exponent * t))` |
| `Logit` | `y_shift + slope * (ln(u) - ln(1-u)) / exponent` on `u` clamped inside `(0, 1)` |
| `Step` | `y_shift + (t >= 0 ? slope : 0)` — up exactly at the x-shift |
| `Piecewise` | interpolation of `points: [(x, y)]`, endpoints held outside; needs ≥ 2 points with increasing `x` |

## The file and the API

`.utility.ron` is a versioned document (`format_version: 1`; a migrator stub lives in
`utility::migrate_ron`):

```ron
(
    format_version: 1,
    seed: 7,
    selection: WeightedRandom(top_n: 3, margin: 0.2),
    actions: [
        (name: "attack",
         considerations: [
            (key: "health", min: 0.0, max: 100.0),
            (key: "enemy_distance", min: 0.0, max: 50.0,
             curve: (kind: Quadratic, exponent: 2.0)),
         ],
         cooldown: 1.5, momentum: 0.25),
        (name: "flee",
         considerations: [(key: "threat", min: 0.0, max: 100.0,
                            curve: (kind: Logistic, exponent: 4.0))]),
    ],
)
```

The reasoner is self-contained Rust — no ECS, no clock, no global state:

```rust,wrap
# use regia_ai::*;
# let text = String::new();
# let blackboard = regia_ai::Blackboard::default();
# let now = 0.0_f32;
let mut reasoner = Reasoner::from_ron(&text)?;          // parse + validate
let mut report = reasoner.decide(&blackboard, now);      // commits: current action, cooldowns
let why = reasoner.explain(&blackboard, now);            // same answer, changes nothing
let names = reasoner.list_decisions();                   // action names, declaration order
```

Drive it from a fixed-step system or a behavior-tree leaf and pass fixed-step time as `now`, so
cooldowns and replays do not depend on frame timing. `decide` and `explain` both return a
`DecisionReport` — a serde type (`to_ron`/`to_json` work) that records, per action, every
consideration's raw input, normalised input, curve output and compensated output, the raw and
compensated products, the modification factor,
the weight, the momentum applied, the final score and any block reason — so the editor and the
agent protocol can show exactly why an action did or did not win. `explain` never mutates the
reasoner (its weighted draw runs on a copy of the generator), which makes it the read-only
surface for tooling.

Determinism: iteration is over vectors in declaration order, `now` is explicit, and the only
generator is the seeded stream advanced by weighted draws — the same definition, seed,
blackboard history and `now` sequence always pick the same actions. `Reasoner::reset` restores
the initial state exactly.

## On entities: `UtilityReasoner`

`UtilityPlugin` (opt-in, ADR-2565) runs a reasoner per entity. Put a `UtilityReasoner` and a `Blackboard` on the entity:

<!-- ignore-reason: two lines from different places (app setup and a spawning system) -->
```rust,ignore
app.add_plugins(UtilityPlugin);
commands.spawn((UtilityReasoner::from_path("ai/guard.utility.ron"), Blackboard::new()));
```

| Field | What it is |
|---|---|
| `path` | the file under `assets/`; empty for `UtilityReasoner::from_def` |
| `interval_ticks` | fixed steps between decisions (default 6); a crowd is offset by spawn-order arrival (never entity id), so it does not all score on one step |
| `action_key` | the blackboard key that holds the selected action (default `utility_action`) |
| `current_action`, `decisions`, `generation` | read-only |

The system runs in `UtilityStep` before `AiStep`, so a behavior tree or state tree reads this step's decision from the
blackboard. `now` is simulated time (`UtilityClock`).

**Hot reload.** A changed `.utility.ron` is read again. A file that does not load keeps the definition running and records why
in `definitions[].error`. A good reload keeps the current action, the cooldowns and the generator by action name
(`Reasoner::adopt_state_from`), so tuning a number does not restart the agent. Only the definition whose file changed
is read: a change under the project's `assets/` matches the path exactly (`bosses/guard.utility.ron` is not
`guard.utility.ron`), and a path still waiting for its file is read when that file changes, not on every change.

**Format.** `.utility.ron` v1 is frozen: `docs/formats/utility-ron.md`. It ships raw in a package and is not cooked.

## Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `ai.utility_list` | read | | `{ reasoners: [{ entity, path, interval_ticks, action_key, current_action, decisions, generation, loaded, has_blackboard, actions }], definitions: [{ path, generation, loaded, error }] }` |
| `ai.utility_explain` | read | `entity` | `{ entity, tick, now, current_action, explain, last_decision }` |

`explain` is a `DecisionReport` for the blackboard as it is now; `last_decision` is the report of the last decision taken, or
`null`. Per action and per consideration each shows `raw_input`, `normalized`, `curve_output` and `compensated`, then `raw`,
`modification_factor`, `compensated`, `weight`, `momentum_applied`, `score`, `eligible` and `blocked`. A missing key shows
`raw_input: null` and `valid: false`. Neither method changes anything.

```json
ai.utility_explain { "entity": 4294967296 }
→ { "entity": 4294967296, "tick": 240, "now": 4.0, "current_action": "attack",
    "explain": { "now": 4.0, "mode": "Best", "selected": "attack", "actions": [
      { "index": 0, "name": "attack", "eligible": true, "blocked": null, "raw": 0.9, "modification_factor": 0.0,
        "compensated": 0.9, "weight": 1.0, "momentum_applied": 0.0, "score": 0.9, "considerations": [
          { "key": "health", "raw_input": 90.0, "normalized": 0.9, "curve_output": 0.9, "compensated": 0.9, "valid": true } ] } ] },
    "last_decision": { "now": 4.0, "mode": "Best", "selected": "attack", "actions": [] } }
```

Errors: `ai_utility_unavailable` (no `UtilityPlugin`); `F0002` with `data.reason` `missing_param` or `bad_entity`; `F0003` with
`data.reason` `entity_not_found`, `not_a_reasoner`, `no_blackboard` or `not_loaded`. `ai.utility_list` definition errors use the
reasons below plus `bad_path` and `definition_not_found`.

## Build errors

Build errors are `UtilityAiError` with a snake_case `reason()` (`parse`, `write`,
`unsupported_version`, `empty`, `too_many_actions`, `empty_name`, `duplicate_name`,
`too_many_considerations`, `bad_value`, `bad_range`, `bad_curve`) — the same shape as the state
tree's compile reasons, for the agent protocol's `data.reason`.
