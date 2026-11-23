# Determinism matrix: thread counts, executors and builds

A Regia Engine simulation stepped on a fixed timestep with the same input should reach the same state on every run. The
[schedule shuffle](schedule-shuffle.md) checks one way that breaks (system order). The determinism matrix checks the
others: the schedule executor, the size of the compute thread pool, and the build profile (debug against release). It
is a test harness, not a runtime feature, so there is no agent-protocol method: an agent runs the test and reads its
failure message (ADR-1347).

## Run it

```sh
cargo test -p regia_test_utils --test determinism_matrix
cargo test -p regia_test_utils --test determinism_matrix --release     # the release side of debug vs release
REGIA_DET_THREADS=1,3,16 cargo test -p regia_test_utils --test determinism_matrix
```

It needs no GPU. `REGIA_DET_FRAMES` (default 240) sets the run length; the pinned-trace check only runs at 240.

Each scene must give the same trace:

| Test | Compares |
|---|---|
| `a_scene_is_repeatable_in_one_process` | two runs in the same process |
| `the_executor_does_not_change_any_scene` | the single-threaded and the multi-threaded executor |
| `the_compute_pool_size_does_not_change_any_scene` | compute pools of 1, 2, 4 and 8 threads, one child process each |
| `every_scene_matches_its_pinned_trace_in_this_build` | the trace pinned for this target in `crates/regia_test_utils/tests/fixtures/determinism/` |

The pool is process-global and sized once, so the thread-count test re-runs its own test binary once per size with
`REGIA_DET_CHILD_THREADS=<n>` and reads the traces the child prints. The child checks that it really got `n`
threads. With `--nocapture` the test also prints, per thread count, on how many steps the `race` spawners ran out of
lane order (`det_matrix::race_out_of_order`; always 0 on the single-threaded executor). That number is evidence
that the race happened; it depends on timing and is never asserted.

## Reading a failure

A trace is the snapshot hash of the whole world every 20 frames, plus one hash per `<entity>/<Component>` at the last
frame. The snapshot is the one the schedule shuffle uses: reflected components keyed by entity `Name`, entity ids left
out. A failure reads:

```text
1 vs 4 threads: scene race: first differs at frame 60; at the last frame: race0-48/Transform, race1-48/Velocity, ...
```

- The **first differing frame** brackets when the drift started (between it and the checkpoint before it).
- The **keys** name the components that moved. The systems that write them, or that spawn those entities, are the
  suspects.
- `not comparable` means the two runs used different frame counts: check `REGIA_DET_FRAMES`.

A pinned trace that differs in one build only (say, release) points at code whose result depends on optimization:
an integer overflow that panics in debug and wraps in release, a float expression the optimizer evaluates at compile
time, or `#[cfg(debug_assertions)]` code that writes state.

## Blessing the pinned traces

The pinned file is per target (`<scene>.<arch>-<os>.trace`): the platform math library and rapier's float reductions
differ between targets. Write it like a golden image, on the reference machine, and give the reason in the commit
message:

```sh
REGIA_BLESS=1 cargo test -p regia_test_utils --test determinism_matrix every_scene_matches_its_pinned_trace
```

A missing file fails on Linux x86_64 (the gate's target) and is only reported elsewhere. Like a golden image, the
check is skipped when `REGIA_SKIP_GOLDENS=1` (the real-GPU pass). glibc picks FMA variants of
`sin`, `cos` and `exp` at run time, so a new CPU or glibc can differ from the reference: compare the failing keys
before re-blessing.

## Debug against release directly

Without a pinned file, run the child test in both profiles and compare what they print (the trace lines start with
`regia-det-trace`, `cp`, `key`, `scene`, `frames`, `every` or `end`):

```sh
for p in dev release; do
  REGIA_DET_CHILD_THREADS=4 cargo test --profile "$p" -p regia_test_utils --test determinism_matrix \
    det_matrix_child -- --exact --nocapture | grep -E '^(regia-det|scene|frames|every|cp|key|end)' > "trace-$p.txt"
done
diff trace-dev.txt trace-release.txt && echo "debug and release agree"
```

The first differing `cp` line is the first checkpoint that drifted; differing `key` lines name the components.

## The scenes

- `smoke`: the schedule-shuffle scene (falling boxes, a walking character, spring bones, a clip and a socket).
- `race`: three spawner systems with no order between them drop colliding boxes into a walled bin on the same steps,
  and a reaper despawns boxes after 75 steps. On the multi-threaded executor the spawners run in parallel, so they get
  their entity ids, and reuse the reaped ones, in thread-timing order.
- `ragdoll`: three limp humanoid ragdolls (capsule bodies on limited joints) dropped on a floor and on each other.

`det_matrix::scenes()` lists them; add a scene there (a `fn() -> App` that steps one fixed step per update with a
manual clock) and bless its pinned trace.

## Rules that keep a simulation deterministic

- **Order by arrival, not by entity id.** `Commands::spawn` reserves an entity id when the command is *recorded*, so
  two systems that spawn in the same pass on different threads get their ids in timing order. The commands are
  applied in a fixed order. Code that must process new entities in a repeatable order sorts them with
  `regia_ecs::spawn_order` instead of by `Entity`:

  ```rust,wrap
  # use regia::prelude::*;
  # let app: &mut App = todo!();
  # #[derive(Component)] struct Projectile;
  use regia_ecs::spawn_order::{SpawnOrder, try_track_spawn_order};

  // In the plugin, before any entity has the component. Treat an error as fatal: the
  // fallback order is entity ids, which parallel spawns reserve in thread-timing order,
  // so a simulation that sorts by it diverges between replays.
  if let Err(error) = try_track_spawn_order::<Projectile>(app.world_mut()) {
      panic!("spawn-order tracking of Projectile failed: {error:?}");
  }

  // In the system:
  fn launch(order: Res<SpawnOrder<Projectile>>, new: Query<Entity, Added<Projectile>>) {
      let mut entities: Vec<Entity> = new.iter().collect();
      order.sort_by_spawn(&mut entities, |e| *e);
      // ...
  }
  ```

  Physics (bodies, colliders, joints; 2D bodies and colliders) and the aero feed already do
  this, and their plugins panic at build time when tracking cannot start
  (the component already in use, or its `on_add`/`on_remove` hooks taken by another owner) —
  add those plugins before anything inserts the tracked components. The script pass tracks
  `Scripts` with the soft `track_spawn_order`
  (`crates/regia_framework/src/script.rs`); it returns false, and the keys fall back to
  entity ids, when tracking cannot start, so it never panics. The soft call is meant for
  non-simulation uses; the script pass is the remaining simulation user (see the fixrev-07-k
  fragment's Known issues). Never insert a tracked simulation
  component through `ParallelCommands`: its per-thread queues apply in thread-arrival order,
  so cross-thread arrival order is thread-timing order again.
- **Order every spawning system against what reads its entities.** A sync point applies the commands of every system
  that has finished, ordered before it or not. A spawner with no order to the physics step lands its bodies before or
  after a step depending on timing: order it `.before(PhysicsStep)` (or `.after`).
- **Use the fixed step's time in `FixedUpdate`**: `FixedTime::step()`, not the frame delta.
- **Expect f32 math on constants to differ between builds by an ulp.** The compiler evaluates `sin`, `exp`, `powf`
  and `powi` with constant arguments while compiling, in f64, and rounds once; at run time the f32 functions (and
  repeated multiplication for `powi`) can land one ulp away. Release builds inline more, so they fold more. Keep
  values that seed a simulation (initial rotations, tables) in data, or compute them in f64, and write small integer
  powers as multiplications. `crates/regia_core/tests/float_folding.rs` checks that `sqrt`, `mul_add` and f64 calls
  fold to the run-time bits, and reports the f32 cases that do not.
- **Iterate maps in a fixed order.** `HashMap` iteration order changes per process. Sort, or use a `BTreeMap`, before
  anything that writes state or spawns.

## In your own tests

```rust,wrap
# fn build_my_app() -> regia::prelude::App { todo!() }
use regia_test_utils::det_matrix::{Executor, diff, record};

let single = record("mine", build_my_app, 240, 20, Executor::SingleThreaded);
let multi = record("mine", build_my_app, 240, 20, Executor::MultiThreaded);
assert_eq!(diff(&single, &multi, 12), None);
```

For thread counts, copy the `det_matrix_child` / `run_children` pair from
`crates/regia_test_utils/tests/determinism_matrix.rs` into your test file: the child test must have a unique name in
its binary.
