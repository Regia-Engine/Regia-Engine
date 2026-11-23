# Replay soak

A replay is only useful if it lands on the same state as the run it recorded. The replay soak checks this.
It records a seeded input stream on headless scenes, replays each recording in a fresh app, and compares
state hashes at checkpoints. It does this once per compute-thread count and build profile, so it also catches
state that depends on the thread count, the optimizer or a per-process hash seed (ADR-867).

The soak is a developer tool (a command and a test helper), like the [schedule shuffle](schedule-shuffle.md).
It is not a runtime feature, so it has no agent-protocol method: an agent runs the command and reads the
report files.

## Run it

```sh
cargo xtask replay-soak                                   # 8 seeds x 4 scenes, threads 1,2,8, debug + release
cargo xtask replay-soak --seeds 32 --steps 1800           # longer: 30 s of game time per run
cargo xtask replay-soak --scenes pile --threads 1,16 --builds release --every 1
cargo xtask replay-soak --features regia_physics/physics-cross-platform-determinism
```

On the workstation, run it through the build wrapper. Every run is one `cargo test -p regia_test_utils --test
replay_soak` process. Only the CPU is used (no GPU), so lavapipe-only machines run it too.

| Flag | Default | Meaning |
|---|---|---|
| `--seeds N` | 8 | seeds per scene |
| `--first-seed S` | 1 | seeds run `S .. S + N` |
| `--steps N` | 600 | fixed steps per run (60 per second) |
| `--every N` | 10 | steps between checkpoints; 1 hashes every step; the last step always does |
| `--scenes a,b` | all | `smoke`, `pile`, `combat`, `crowd` |
| `--threads 1,2,8` | `1,2,8` | compute-pool sizes, one process each; `auto` keeps the default formula |
| `--builds debug,release` | both | cargo profiles, one build each (`debug`, `release` or a custom profile name) |
| `--features F` | none | cargo features for the test build |
| `--tag mac-` | empty | prefix for run labels, so files from several machines can sit in one directory |
| `--out DIR` | `target/replay-soak` | output directory |
| `--no-run` | off | merge the `run-*.json` files already in `--out`, run nothing |
| `--baseline FILE` | none | compare the reference traces with another merged `replay-soak.json` |

The command exits with 1 in any of these cases: a record trace differs between runs, a replay leaves its
recording, a run wrote no report (a build or test failure), or a baseline trace differs.

## The scenes

| Scene | What runs | Inputs |
|---|---|---|
| `smoke` | The shuffle-check scene: eight boxes, a walking character, spring bones, a clip-driven rig with a socket | `Move` and `Jump` drive the hero, `Shove` pushes `box0` |
| `pile` | 36 seeded boxes, spheres and capsules dropped into a walled bin, with collision events on | `Shove` pushes the striker sphere |
| `combat` | A turret, six seeded targets with framework `Health`, the damage step, deaths and respawns | `Aim` turns the turret, `Fire` shoots |
| `crowd` | Twelve seeded nav agents on a floor with a hole, planned with `NavQuery` and steered with ORCA avoidance (`NavPlugin`) | `Rally` sends everyone to a point that way, `Scatter` sends everyone home |

The `tally` entity carries `SoakTally`. It folds every `CollisionStarted` pair (in `pile`) and every death (in
`combat`) into a hash, in arrival order. A reordered event therefore changes the state hash, even when the
bodies end up in the same place.

The input generator is seeded (`InputStream`). Buttons alternate presses of 1 to 8 steps with releases of 3 to
40 steps. Axes hold a random value, or zero 30 % of the time, for 2 to 30 steps.

## Reading the report

`replay-soak.md` starts with the verdict, the runs (the first is the reference) and the matrix. It then has one
table row per scene and seed:

```text
| Scene | Seed | Final hash (reference) | debug-t1 | debug-t2 | release-t8 |
|---|---|---|---|---|---|
| pile | 3 | `5c1f0e9a2b7d4410` | ok | ok | trace @240 |
```

- `ok`: the run's record trace equals the reference run's, and its replay equals its recording.
- `trace @S`: the record trace leaves the reference at step S. The divergence section lists the entities whose
  hash differs there. The state depends on the thread count, the build or the process.
- `replay @S`: the replay leaves its own recording at step S. The divergence section lists up to 12
  `<entity>/<Component>` values from both sides. Something in the scene is not driven by the recording: a
  wall clock, a hash order, an unordered system pair, or input applied on the wrong step.
- `missing`: the run has no result for this scene and seed.

## What to do with a divergence

1. If it is `replay @S` in every run, the scene is nondeterministic on its own. Look at the named component's
   writers: a `HashMap` iteration that spawns, despawns or emits, `Time::delta_secs()` read outside
   `FixedUpdate` in a simulation system (inside it the delta is one step), or an `Instant::now()`.
2. If it is `trace @S` only between thread counts, two systems with no ordering race on the named
   entities. Engine steps have sets to order against: `PhysicsStep`, `NavStep`, `AiStep`, `DamageSet` and
   `InputRecordingSet`. `cargo xtask shuffle-check --ambiguities` lists the candidate pairs. Add the edge, and add a test that
   fails without it.
3. If it is `trace @S` only between builds, look for float code whose result depends on optimization. Also look
   for a type-registry or `TypeId` order used as a tie-break.
4. Rerun with `--every 1 --seeds 1 --first-seed S --scenes X` to get the exact step.

## Across machines

Run on each machine with its own tag, copy the run files into one directory, and merge them:

```sh
cargo xtask replay-soak --tag mac- --builds release --threads 1,4      # on the Mac
cargo xtask replay-soak --no-run --out merged/                         # after copying run-*.json there
cargo xtask replay-soak --baseline linux/replay-soak.json              # or compare against another merge
```

Different CPUs and OSes may round floats differently. In that case, a `trace` divergence between machines is
expected unless the build enables `regia_physics/physics-cross-platform-determinism`.

## Report formats

`run-<label>.json` (`format: "regia-replay-soak-run"`, `version: 1`) is written by one test process:

```json
{
  "format": "regia-replay-soak-run", "version": 1,
  "label": "release-t8", "build": "release", "threads_requested": 8, "threads": 8,
  "os": "linux", "arch": "x86_64", "steps": 600, "every": 10,
  "runs": [
    {"scene": "combat", "seed": 1, "entities": 8, "values": 31, "active_inputs": 702,
      "record": [{"step": 10, "hash": "9a0c…", "entities": {"tally": "17be…", "turret": "40d2…"}}],
      "replay": [{"step": 10, "hash": "9a0c…"}],
      "replay_divergence": null}
  ]
}
```

A `replay_divergence` is `{"step": S, "differing": [{"key", "record", "replay"}]}`. Values longer than 400
characters are cut and end in `...`. Hashes are 16 lowercase hex digits. `threads_requested: null` means the
default formula.

`replay-soak.json` (`format: "regia-replay-soak"`, `version: 1`) is the merged report. It has these fields:

- `configs[]`: `label`, `build`, `threads`, `os`, `arch`. The first entry is the reference.
- `reference`, `steps`, `every` and `pass`.
- `cells[]`: `scene`, `seed`, `reference_trace[]` (`step`, `hash`), `final_hash`, and `results[]` with one entry per
  run. Each result has `label`, `present`, `matches_reference`, `first_divergent_step`, `divergent_entities`,
  `replay_matches` and `replay_divergence`.
- `problems[]`.
- `baseline`: `path`, `reference`, `compared`, `missing` and `mismatches[]` (`scene`, `seed`, `step`).

## Playback timing

`InputRecordingPlugin` injects recorded step k's values in `PreUpdate`, before `InputSet::Evaluation`, for the
frame that runs step k. Step k of a replay therefore sees what step k of the recording saw, triggers and phases
included, and playback ends on the last recorded frame. Before ADR-867, the values landed one frame later, and
every replay ran one step behind its recording. Exact replays need at least one fixed step per frame;
`enable_fixed_step` runs exactly one.

## In your own tests

```rust,wrap
use regia_test_utils::replay_soak::{SoakConfig, SoakScene, soak_seed};
# use regia_app::App;
# use regia_input::ActionKind;
# fn build_my_app(_seed: u64) -> App { App::new() }

let scene = SoakScene {
    name: "mine",
    build: build_my_app,
    actions: &[("Move", ActionKind::Axis2D)],
    components: &[], // or only the gameplay components: &["Transform", "Health", "Pawn"]
};
let report = soak_seed(&scene, 7, &SoakConfig { steps: 600, every: 10 });
assert!(report.replay_matches(), "{:?}", report.replay_divergence);
```

`build` must return an app that runs one fixed step per update. It must also have `InputPlugin` with those
actions, `InputRecordingPlugin`, and gameplay ordered `.after(InputRecordingSet)`. Name every entity: the
snapshot keys entities by `Name`. If the app holds asset handles or render state whose ids come from process-wide
counters, list the gameplay components in `components`. Two apps in one process otherwise differ in those ids.
`samples/arena/tests/replay_soak.rs` does this for the arena.
