# Determinism: state hashes and record/replay

A Regia Engine simulation with a fixed timestep, a seeded RNG and recorded input should give the same
state every time it runs. Two tools check that: the `sim.state_hash` / `sim.state_diff` protocol methods
for a running app, and `cargo xtask repro-check`, a record/replay stress run over 20 seeds and three
scenes (ADR-537).

## The state hash

The hash covers every reflected component of every entity, written as canonical text:

- Each value is keyed `<entity>/<Component>`, where `<entity>` is the entity's `Name`. Duplicate names
  and unnamed entities get a `#rank`. Entity ids are not part of the state. An entity reference inside a
  value prints as the referenced entity's key, so two runs that allocate different ids still hash the
  same.
- Struct fields appear in declaration order, and lists in their own order.
- **Map and set entries are sorted**, so a component holding a `HashMap` hashes the same however the
  map happens to iterate.
- Floats print in the shortest form that round-trips, which is distinct for distinct values (`0.0` and
  `-0.0` included).
- Resources are not included. The harness lets a scene add its own: the arena adds its match RNG.
- An opaque type without a reflected `Debug` contributes only its type name, so its contents are not
  hashed.

The same snapshot is what `cargo xtask shuffle-check` compares ([Schedule shuffle](schedule-shuffle.md)).

## `sim.state_hash`

Params, all optional:

- `prefix`: only keys that start with this, for example `"hero/"`.
- `ignore`: component short names to leave out, for example `["EntityGuid"]`.
- `keys`: `true` adds a hash per key.

Read-only.

```json
{ "method": "sim.state_hash", "params": { "keys": true, "ignore": ["EntityGuid"] } }
```

Returns `{ hash, frame, entities, values, nondeterministic, keys? }`:

- `hash`: 16 hex digits.
- `frame`: the app's frame count.
- `values`: the number of keys.
- `nondeterministic`: `[{ code, source, detail }]`, what made this session not reproducible; empty when nothing did.
  An extension hot reload (`plugin.reload`) adds `{ "code": "nondeterministic_reload", "source": "<extension id>",
  "detail": "generation N" }` (ADR-9850). Hashes of a run that lists an event are not comparable with a clean run's;
  `sim.state_diff` answers the same field. A demo recorded in such a session carries `!nondeterministic_reload:<id>`
  at the end of its `build_hash`, and `demo.inspect` lists it under `nondeterministic` and in `mismatches`.
- `keys`: `{ "<entity>/<Component>": "<16 hex>" }`, only with `keys: true`.

## `sim.state_diff`

Params:

- `against` (required): the `keys` object of an earlier `sim.state_hash { keys: true }`, from this
  run or from another.
- `prefix` and `ignore`: as for `sim.state_hash`, applied to both the live snapshot and `against`.
  Filtered keys do not count as added, changed or removed.
- `limit`: how many keys each list shows, 1 to 1000 (default 50).

Read-only.

Returns `{ hash, frame, same, counts: { changed, added, removed }, changed, added, removed, truncated }`.
`changed` lists the keys whose value differs. `added` lists the keys that exist now but are missing
from `against`, and `removed` the reverse.

To check a run for determinism by hand:

1. Start two runs with the same seed and input, both on a fixed timestep with a manual clock
   (`enable_fixed_step` in the app).
2. Pause both with `play.pause` and step them to the same frame with `play.step`.
3. Call `sim.state_hash` in each run and compare the two `hash` values.
4. If they differ, call `sim.state_hash { keys: true }` in run A and pass its `keys` to
   `sim.state_diff { against }` in run B. The `changed` keys name the components to look at; the
   systems that write them are the suspects.

Errors are `INVALID_PARAMS` (`data.code` `F0002`). The message ends in a bracketed code:

| Code | Cause |
|---|---|
| `bad_prefix` | `prefix` is not a string. |
| `bad_ignore` | `ignore` is not an array of strings. |
| `bad_keys` | `keys` is not a boolean. |
| `bad_against` | `against` is missing, not an object, or one of its values is not a hex hash. |
| `bad_limit` | `limit` is not an integer from 1 to 1000. |

## The record/replay stress run

```sh
cargo xtask repro-check                       # 20 seeds × every scene, 240 frames each
cargo xtask repro-check --seeds 50 --steps 600
cargo xtask repro-check --report target/repro  # also writes target/repro.engine.md and target/repro.arena.md
```

For each scene and seed, `regia_test_utils::repro::record_replay` runs the scene twice:

1. **Record.** The scene feeds seeded input to player 0 through `InputInjection`, and
   `InputRecorder` records it. The input is a pure function of the seed and the step
   (`repro::input_noise`). The state is kept every 20 steps.
2. **Replay.** A fresh app is built from the same seed and plays the recording back with no drive. Its
   state is hashed at the same steps.

A run passes when every checkpoint matches. A failing run is reported like this (an illustration, not
a real finding):

```text
- arena_navmesh: 19 of 20 seeds replay identically (412 values hashed)
  - arena_navmesh seed 11400714819323198486: diverges by step 140: bot3/Transform, bot3/NavAgentState
    - bot3/Transform
      recorded {translation:{x:3.25,...
      replayed {translation:{x:3.2500002,...
```

The scenes:

| Scene | Where | What it covers |
|---|---|---|
| `physics_pile` | `regia_test_utils::repro` | 24 seeded boxes and spheres dropped into a walled pit with seeded velocities, and a character controller walking into them. |
| `character_rig` | `regia_test_utils::repro` | The schedule-shuffle smoke scene: falling boxes, the character, spring bones on a rope, a clip-driven arm with a socket. The seed moves the character's start. |
| `arena_match` | `samples/arena/tests/repro_check.rs` | The bot match with a local player: steering bots, hitscan, rockets, props, damage, scoring and the match RNG. |
| `arena_navmesh` | the same file | The same match with behaviour-tree bots on the navmesh (`regia_ai`, `regia_nav`). |

Both runs use the engine's default multi-threaded executor. A race between two systems that have no
order shows up as a divergence that comes and goes between runs. A scene that does not run exactly one
fixed step per frame is reported as an error instead of being compared.

### Your own scene

```rust,wrap
use regia_test_utils::repro::{ReproConfig, ReproScene, inject_next_frame, input_noise, record_replay};
# use regia_app::App;
# use regia_core::math::Vec3;
# fn my_level_app(_seed: u64) -> App { App::new() }

let scene = ReproScene {
    name: "my_level",
    build: my_level_app,          // fn(u64) -> App: plugins, InputRecordingPlugin, entities, fixed step
    actions: &["Move", "Fire"],   // recorded and replayed
    drive: |app, seed, step| {
        inject_next_frame(app, "Move", Vec3::new(input_noise(seed, step / 30, 0), 0.0, 0.0));
    },
    extra: None,                  // or Some(fn(&World, &mut Snapshot)) for resource state
    ignore: &["EntityGuid"],      // random or wall-clock values, not simulation state
};
let run = record_replay(&scene, 7, &ReproConfig::default());
assert!(run.is_clean(), "{run:?}");
```

Name every entity the scene spawns. Order your fixed-step systems after `InputRecordingSet`, so the
replay sees each recorded step on the same step.

## What keeps a run deterministic

These rules come from the fixes the harness motivated (ADR-537):

- **Integrate with the fixed step in `FixedUpdate`.** Use `FixedTime::step()` or `Time::delta_secs()`
  (inside `FixedUpdate` both are one step); outside it `Time::delta_secs()` is the frame delta.
  A frame can run zero or several fixed steps, so the frame delta is not the step length. A system
  that integrated with it moved differently at another frame rate. Behaviour trees, nav agents and the
  arena's bots, projectiles and match clock now use the step.
- **Give every fixed-step system an order.** Systems that touch the same data and have no edge between
  them run in thread-timing order on the multi-threaded executor. `regia_ai::AiStep` and
  `regia_nav::NavStep` name the AI and nav steps so a game can chain them; `AiPlugin` already runs
  `AiStep` after `PhysicsStep`; the arena chains input, physics, AI, nav, bots, weapons, damage and
  the match.
- **Never let a hash map's order reach state.** A despawn, spawn, list or float sum in `HashMap` order
  differs between runs, because each map gets a random hasher seed. The sim bridge now despawns vanished
  entities in sim-id order, and animation morph weights leave the evaluator in target order.
- **Read each message once.** A fresh `MessageCursor` reads the previous frame's messages a second time.
  Keep the cursor between calls. The arena's fire, explosion and death handlers now do.
- **Playback is same-step.** A recorded step's values are injected before that frame's action
  evaluation, so the replay's step `k` sees what the recording's step `k` saw. Before ADR-537 a replay
  ran one frame behind its recording.

Some residual risks are known and not yet fixed:

- Systems that spawn through `Commands` in parallel can reserve entity ids in thread order.
- The physics world inserts new bodies in entity order.

If the harness ever shows a physics divergence that comes and goes, look there first.
