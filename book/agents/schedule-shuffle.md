# Schedule shuffle and ambiguity checks

Systems that have no `.before`, `.after` or `.chain()` path between them run in an order nobody chose. Two checks find
code that depends on that order. Both are developer tools (a command and a test helper), not runtime features, so there
is no agent-protocol method: an agent runs the command and reads the report.

## Run it

```sh
cargo xtask shuffle-check                 # the smoke scene under 8 seeded orders
cargo xtask shuffle-check --seeds 32 --frames 180
cargo xtask shuffle-check --ambiguities   # only the ambiguity report
cargo xtask shuffle-check --ambiguities --report target/ambiguities.md
```

On the workstation run it through the build wrapper (`localbuild.sh <lane> -- cargo xtask shuffle-check`). It runs
`cargo test -p regia_test_utils --test shuffle_check`, so it needs only lavapipe-free CPU: no GPU is touched.

Reading the output:

- `repeat of the reference seed: identical` — the scene gives the same state twice in a row. If it says `DIFFERS`, the
  scene or a plugin is nondeterministic by itself (a wall clock, a thread race), and the seed comparison means nothing.
- `N of M other seeds diverge` with `seed S hash H: <entity>/<Component>, ...` — the components, by entity `Name`, whose
  value differs from the reference seed. The systems that write those components are the pair that needs an edge.
- The ambiguity table lists each schedule's system count and unordered conflicting pairs, then the top pairs by score:
  a pair that both write the same component or resource scores 3 per shared item, read/write 1, a whole-world
  (`&mut World`) system 1.

## In your own tests

```rust,ignore
use regia_app::prelude::*;

let mut app = build_my_app();
let report = app.ambiguity_report();          // builds every schedule, runs nothing
println!("{}", report.to_markdown(20));

let mut app = build_my_app();
app.shuffle_schedules(7);                      // before the first update
for _ in 0..120 { app.update(); }
```

`regia_test_utils::shuffle::shuffle_check(build, frames, seeds)` does the per-seed comparison for any `App`:
`snapshot(world)` is every reflected component of every entity as debug text, keyed by `Name`, with entity references
printed as those names. Name every entity a test spawns; an unnamed entity is keyed by its component types and content.

## What to do with a finding

An ambiguity is a candidate. Two unordered systems that only push to a list, or that touch unrelated fields of one
resource, are fine. Add an explicit edge only for a pair that a shuffle run shows changing the result, and add a test that
fails without it. Seeds run single-threaded; the real executor can order differently still, so a clean run proves the
listed pairs are not the cause, not that no race exists.

`App::shuffle_schedules` needs the `debug_names` feature (on by default; the same feature that keeps system names).
