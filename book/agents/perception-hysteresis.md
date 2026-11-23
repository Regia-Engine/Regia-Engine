# Perception hysteresis

Sight has two cones (ADR-4586). A target that is not in memory is acquired only inside the acquire cone
(`sight_radius`, `sight_half_angle`). A target that is already remembered is kept until it leaves the lose
cone (`sight_lose_radius`, `sight_lose_half_angle`). A target sitting on a single boundary used to enter and
leave memory on consecutive steps. The wider lose cone stops that.

Zero on a lose field means that limit matches the acquire field. An agent that leaves both at zero (the
default) matches the old cone on the acquire path and on the keep path.

`ai.perception_hysteresis` reads the pair, or sets it. Omit both lose fields to read. Pass either field to
write. The two fields are validated together: a field you omit keeps its current value, and a refusal writes
neither.

```json
ai.perception_hysteresis { "entity": 4294967296, "sight_lose_radius": 14.0 }
→ { "entity": 4294967296, "sight_radius": 10.0, "sight_half_angle": 1.5707964,
    "sight_lose_radius": 14.0, "sight_lose_half_angle": 0.0,
    "effective_lose_radius": 14.0, "effective_lose_half_angle": 1.5707964, "applied": true }
```

`applied` is false on a read. `effective_lose_radius` and `effective_lose_half_angle` are the numbers the
keep path tests: a stored zero is replaced by the acquire value. A positive stored value is used as stored.

## What is accepted

Both numbers must be finite and at least 0. A positive `sight_lose_radius` must be greater than or equal to
`sight_radius`. A positive `sight_lose_half_angle` must be greater than or equal to `sight_half_angle`.
Negative zero is accepted and means the acquire range, the same as zero.

A value that fails those rules answers `perception_bad_hysteresis` and leaves both fields as they were.
There is no schema range on the lose fields: a number between 0 and the acquire range has to reach this
check so the code is `perception_bad_hysteresis`, not a generic parameter error.

## Memory

The sight sweep calls one predicate. A pair the agent does not remember is tested against the acquire cone.
A pair it does remember is tested against the lose cone. Line of sight is unchanged, and so is forgetting:
a stimulus that is not refreshed ages out after `max_age_ticks` (`PerceptionLost`, `Expired`), a full list
drops the stalest (`Evicted`), and a despawned source leaves at once (`Despawned`). Leaving the lose cone
stops the refresh. With `max_age_ticks` of 0 the stimulus leaves on that same step. Strength still uses the
acquire radius, so a target kept only by the lose cone can have strength 0. The debug fan draws the acquire
cone.

The sense step is the existing `perception_tick` span plus a nested `perception_sense` span, one per fixed
step.

## Errors

| Code | When |
|---|---|
| `perception_bad_hysteresis` | A lose value is negative or not finite, or a positive value is below the matching acquire range. Nothing is written. |
| `ai_perception_unavailable` | `PerceptionPlugin` is not installed. |
| `F0003` | `entity` is missing, not a live entity, or has no `Perception`. |
| `F0002` | A parameter is present with the wrong JSON type. |

`F0002` is a universal parameter error (`book/agents/protocol.md`). The other three are this method's.
