# AI: perception, attitudes and planning state

What an agent perceives, why it picked an action and which plan it follows. Three opt-in plugins in `regia_ai` keep this
state, and seven methods read it (ADR-2565, ADR-8357, `docs/specs/22-tier2.md` § T2.2). Add the plugins you need; a method whose plugin
is missing answers `ai_perception_unavailable`, `ai_utility_unavailable` or `ai_goap_unavailable`.

| Plugin | Components | Methods |
|---|---|---|
| `PerceptionPlugin` | `Perception`, `Perceived`, `Perceivable`, `Visibility`, `SightSamples` | `ai.perception_get`, `ai.team_knowledge`, `ai.senses`, `ai.attitude_set` |
| `UtilityPlugin` | `UtilityReasoner` plus a `Blackboard` | `ai.utility_list`, `ai.utility_explain` |
| `GoapPlugin` | `GoapAgent`, `GoapStatus` plus a `Blackboard` | `ai.goap_plan` |

## Senses

`PerceptionPlugin` gives every entity with a `Perception` a `Perceived` memory. Each stimulus holds the `entity` (`null` for
an anonymous sound or wound), the `sense`, the source's `team`, the `last_known_position`, the `last_known_velocity`, a
`confidence` in `[0, 1]`, the `last_sensed_tick` and a `strength` in `[0, 1]`.

- **Sight.** Entities with a `Perceivable` are targets. Sight walks the `(agent, target)` pairs round-robin: a sweep starts at
  most every `PerceptionSettings.sight_interval_ticks` fixed steps, and at most `sight_checks_per_tick` pairs run per fixed
  step. A pair needs the attitude filter to pass, the target inside the horizontal cone, and a clear ray through the physics
  world that ignores sensors, both entities and their children. Without a physics world every line is clear.
- **Hearing.** Write a `HearingEvent` message. An agent hears it inside both its `hearing_radius` and the event's `radius`, if
  the attitude filter passes. `can_hear` is the range test only: teams no longer gate it.
- **Damage.** Write a `DamageEvent` message (`victim`, `attacker`, `position`, `amount`). The attacker is registered in or out
  of the cone, whatever the attitude, and an attacker that deals damage can become the target even if it was neutral.
- **Touch.** `Perception.touch_radius` above zero feels a `Perceivable` inside that radius. There is no cone and no line of
  sight. Zero (the default) disables touch. `PerceptionSettings.touch_interval_ticks` is the gap between passes (`0` means 1).
- **Custom.** `PerceptionSenses::register(name, range, interval_ticks, callback)` adds `Sense::Custom(id)`. The callback is
  `fn(&World, observer, candidate) -> Option<f32>`: `Some(strength)` in `[0, 1]` records the candidate, `None` records
  nothing. A positive finite `range` drops candidates farther away before the callback runs. Ids start at 1. An empty name
  is `empty_sense_name`; a repeated name is `duplicate_sense`. `ai.senses` lists the registry. The built-in senses are not
  rows. `ai.senses { "name": "scent" }` returns that one row, or `unknown_sense` when the name is not registered.

`Sense::Touch` and `Sense::Custom` follow `Damage`, so an exhaustive match on the three older senses needs a new arm. The
order of the older three is unchanged.

A stimulus not sensed for `Perceived.max_age_ticks` is forgotten (`PerceptionLost`, `Expired`). Past `capacity` the stalest
goes first (`Evicted`). A source that no longer exists leaves at once (`Despawned`). A new stimulus fires `PerceptionGained`.

Size the budget against the memory: a sweep longer than `max_age_ticks` forgets targets while they are in view. Compare
`stats.sweeps_completed` and `stats.sight_checks_last_tick`.

## Memory and confidence

`confidence` starts at 1 on a fresh sense. Each sense has a half-life on `PerceptionSettings`
(`sight_confidence_half_life`, and the same for hearing, damage and touch). `None` or `Some(0)` does not decay: confidence
stays 1, the last known position stays where it was sensed, and the stimulus still drops when its age passes
`max_age_ticks`. That is the default, and it matches the older memory.

A positive half-life multiplies confidence by `0.5^(1 / half_life_ticks)` on each fixed step the stimulus was not sensed
again. The same step adds `last_known_velocity` to `last_known_position`. A direct sense sets confidence back to 1 and
stores the step from the previous known position into `last_known_velocity`. The target score weighs confidence, so a
fainter but surer memory can beat a stronger stale one. Custom senses take their half-life from
`PerceptionSenses::set_half_life`.

## Team knowledge

`TeamKnowledge` is one resource for the whole app. An agent with `Perception.share_knowledge` writes each of its stimuli
into its attitude-group team, and into its squad when it has an `AiSquadMember`. It also reads facts other agents wrote
for that team or squad, so a squad mate can know a target it never sensed. A reader skips its own reports. Shared facts
decay by `TeamKnowledge.half_life_ticks` (`None` keeps them until a writer replaces them). Each team or squad keeps 32
facts; the stalest sensed tick goes first.

```json
ai.team_knowledge { "team": 0 }
→ { "team": 0, "facts": [ { "entity": 4294967301, "sense": "sight", "source_team": 1,
    "position": [0.0, 0.0, -5.0], "velocity": [0.0, 0.0, 0.0], "confidence": 1.0, "strength": 0.75,
    "sensed_tick": 234, "reporter": 4294967296 } ] }
```

A team number with no facts is `{ "facts": [] }`. A missing `team`, a non-integer, or a number outside 0..=255 is
`unknown_team`. The plugin is checked first: without `PerceptionPlugin` the answer is `ai_perception_unavailable` even
when `team` is absent.

## Visibility and the detection meter

`Visibility` on a target is a multiplier in `[0, 1]` from the game (light, stance, camouflage). Absent means 1. A
non-finite factor counts as 0. `SightSamples` on an observer is how many rays to cast across the target's
`Perceivable.bounds_half_extents`. A count of 0 or 1, or zero extents, is the single center ray. The visible fraction is
the clear rays divided by the sample count.

The gain of one look is `fraction × visibility × cone falloff`. `PerceptionSettings.detection_threshold` of 0 (the
default) acquires on the first clear look. A positive threshold accumulates that gain on `Perceived.meter` and acquires
when the evidence reaches the threshold, so a half-hidden or dark target is seen later. A remembered target inside the
lose cone refreshes when any sample is clear, without refilling the meter. A fully blocked remembered target does not
refresh and ages out as before. Sample rays of one target are cast together or left for the next step, and the total
rays in a step stay inside `sight_checks_per_tick`.

With `debug_draw`, `AiPerceptionGizmos` draws a memory point sized by confidence, a team-knowledge marker, and a
detection-meter bar.

## Attitudes

`Attitudes` maps team x team to `hostile`, `neutral` or `friendly`. It is directional. A pair with no entry reads `same_team`
(default friendly) or `other_team` (default hostile). `Perceived.detect` chooses what sight and hearing register (default
hostile and neutral).

```json
ai.attitude_set { "from": 0, "to": 1, "attitude": "neutral", "symmetric": true }
→ { "from": 0, "to": 1, "attitude": "neutral", "previous": "hostile", "reverse": "neutral" }
```

The change applies at once to target picks and to the next checks. Memory is not rewritten: a stimulus of a team that turned
friendly ages out.

## Reading what an agent perceives

```json
ai.perception_get { "entity": 4294967296 }
→ { "stats": { "tick": 240, "sight_checks_last_tick": 64, "sight_rays_last_tick": 3, "sweeps_completed": 39, "messages_dropped": 0 },
    "settings": { "sight_interval_ticks": 6, "sight_checks_per_tick": 64, "detection_threshold": 0.0, "touch_interval_ticks": 1,
      "sight_confidence_half_life": null, "hearing_confidence_half_life": null, "damage_confidence_half_life": null,
      "touch_confidence_half_life": null },
    "agent": { "entity": 4294967296, "team": 0, "target": 4294967301, "max_age_ticks": 300, "capacity": 32,
      "meter": [ { "target": 4294967301, "evidence": 0.6 } ],
      "stimuli": [ { "entity": 4294967301, "sense": "sight", "team": 1, "last_known_position": [0.0, 0.0, -5.0],
                     "last_known_velocity": [0.0, 0.0, 0.0], "last_sensed_tick": 234, "age_ticks": 6,
                     "strength": 0.75, "confidence": 1.0 } ] } }
```

Without `entity` the answer has `agents`, one per entity in entity order. Use `play.wait_until` / `expect` on
`Perceived.target` to wait for an agent to notice something.

## The target on the blackboard

`Perceived.target` is the strongest fresh stimulus of a hostile source, or of a neutral source that dealt damage. The score
weighs strength, confidence and freshness. A current target counts 1.25x, so two similar targets do not trade places every step. `Perceived.target_key` (default
`perception_target`) writes it to the agent's `Blackboard`, so behavior trees and state trees read it like any other key:

| Key | Value |
|---|---|
| `perception_target` | the entity (absent without a target) |
| `perception_target_position` | its newest known position (absent without a target) |
| `perception_target_known` | `true` while there is a target, then `false` |

`PerceptionStep` runs before `AiStep`, so a tree sees this step's target. A state tree transition reads
`condition: Key("perception_target_known")`. An empty `target_key` writes nothing.

## Utility reasoners and GOAP agents

[Utility AI](utility-ai.md) has `ai.utility_list` and `ai.utility_explain`. `ai.goap_plan` shows a [GOAP](../user-guide/goap.md)
agent's plan, one agent with `entity` or all without:

```json
ai.goap_plan { "entity": 4294967296 }
→ { "stats": { "tick": 12, "plans_started_last_tick": 0, "plans_started_total": 2 },
    "agent": { "entity": 4294967296, "state": "executing", "error": "", "plans_started": 2, "replans": 1,
      "goal": { "facts": { "has_table": true }, "ints": [] },
      "facts": { "has_axe": false, "has_wood": false, "has_table": false, "axe_available": false },
      "steps": [ { "name": "gather_sticks", "cost": 8.0, "done": false, "current": true },
                 { "name": "craft", "cost": 1.0, "done": false, "current": false } ],
      "next_step": 0, "current_action": "gather_sticks", "total_cost": 9.0, "nodes_expanded": 4,
      "budget": { "max_nodes": 2000, "max_depth": 64 },
      "actions": ["pick_up_axe", "chop", "gather_sticks", "craft"] } }
```

`state` is `idle`, `planning`, `executing`, `satisfied` or `failed`; `error` is `no_plan`, `limit_reached` or
`invalid_action` when it failed. Steer an agent by writing its facts with `scene.set_property` on `Blackboard`.

## Errors

| Code | When |
|---|---|
| `ai_perception_unavailable` | `PerceptionPlugin` is not installed |
| `unknown_team` | `ai.team_knowledge`: `team` is missing, not an integer, or outside 0..=255 |
| `unknown_sense` | `ai.senses`: `name` is not a registered custom sense |
| `ai_utility_unavailable` | `UtilityPlugin` is not installed |
| `ai_goap_unavailable` | `GoapPlugin` is not installed |
| `F0002` | a bad parameter; `data.reason` is `bad_entity`, `missing_param`, `bad_team` or `bad_attitude` |
| `F0003` | `data.reason` is `entity_not_found`, `not_a_perceiving_agent`, `not_a_reasoner`, `no_blackboard`, `not_loaded` or `not_a_goap_agent` |

## Determinism

Everything advances by fixed steps. Agents are visited in spawn (arrival) order (ADR-2396); targets are visited in entity order. The sight sweep resumes on the observer it stopped on, so a reused entity index cannot skip a perceiver. Two runs with the same spawn order and inputs give the same
perceived lists, decisions and plans. A GOAP search runs on the task pool, and its result lands exactly
`GoapSettings.plan_latency_ticks` steps after it started, however fast the machine is: a search the pool has not finished by
then runs on the fixed step's thread instead, with the same result. `GoapStats.searches_run_inline` counts those, and it is the
one number here that depends on the machine. A hot reload lands on the next frame: it is a dev-time event. The tracing spans are
`perception_tick`, `utility_tick`, `utility_sync` and `goap_tick`.
