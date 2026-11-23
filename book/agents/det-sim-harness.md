# Seeded simulation runs, soak and profile

`regia_detsim` provides a headless runner over the [F-02 scenario contract](det-golden.md):
seed → host bootstrap → fixed steps → state fingerprint (S-18, ADR-8151). It has no
renderer, ECS, game assets or network. The included `ai_arena` binary demonstrates
two teams pursuing and attacking each other, using `regia_detsim::math` and stable
unit order. Its rules are sample rules, not a game's AI or balance model.

## Run and inspect

```sh
cargo run -p regia_detsim --bin ai_arena -- --seed 100 --ticks 1200
cargo run -p regia_detsim --bin ai_arena -- --seed 100 --ticks 1200 --soak 10
cargo run --release -p regia_detsim --bin ai_arena -- --seed 100 --profile 1200 --warmup 30
cargo test -p regia_detsim --test sim_harness --test ai_arena_runner
```

The executable writes exactly one `regia-det-sim ` JSON line to stdout. This is
its agent surface, following F-02's headless report convention; it does not open a
`regia_agent` endpoint or register an editor method. A runner waits for process
exit, parses the JSON line, and checks `status` and exit code. `--help` prints usage.
No golden is created or re-blessed.

| Field | Meaning |
|---|---|
| `scenario`, `mode` | `ai_arena`; `run`, `soak` or `profile`. |
| `target` | F-02 target key, for example `x86_64-linux` or `aarch64-macos`. |
| `seed`, `rate_hz`, `units_per_team` | Hexadecimal 64-bit seed, simulation rate and team size. Inputs use decimal integers. |
| `status` | `ok`, `diverged` or `error`. Exit codes: 0, 1 or 2, respectively. |
| `ticks`, `fingerprint` | Completed steps and F-02 state hash (16 hexadecimal digits). Zero ticks hashes the bootstrap. |
| `alive`, `health`, `attacks` | Run mode: per-team living counts and remaining health, plus total attacks. |
| `pairs_completed`, `steps_compared` | Soak mode: fully matching independent pairs and compared steps. Bootstrap comparisons are extra and not counted as steps. |
| `divergence` | Soak mode: `null`, or `{pair,tick,expected,actual}` for the first mismatch. Pair is zero-based; tick counts completed steps, so 0 is bootstrap and 1 follows `DetScenario::step(0)`. |
| `warmup_ticks`, `samples`, `p50_ns`, `p99_ns`, `max_ns` | Profile mode: untimed warmup, measured steps and nearest-rank durations in nanoseconds. Timing includes harness instrumentation, excludes bootstrap/hash/sort, and never feeds simulation. |
| `code` | Error mode: stable code below. Error reports have only `status` and `code`. |

The binary accepts `--seed`, `--ticks`, `--rate-hz`, `--units-per-team`,
`--soak PAIRS`, `--profile SAMPLES`, and `--warmup TICKS`. Defaults are seed 100,
1200 ticks, 60 Hz, four units per team and 30 warmup ticks. Run and soak allow
zero ticks. Profile requires samples > 0 and uses warmup + samples as its total;
`--ticks` and `--profile` cannot be combined. Duplicate options, unknown options,
missing values and conflicting modes are errors, not silently ignored.

The command bounds rate to 1..=1000 Hz, team size to 1..=128, total ticks to
1,000,000 and soak pairs to 1..=1000. It rejects estimated work above 100,000,000
unit-pair visits (`max(ticks,1) × total_units² × worlds`, where soak worlds is
`2 × pairs`). These runner limits protect automation; library callers supply
explicit run budgets and arena settings.

## Error codes

| Code | Meaning |
|---|---|
| `invalid_arguments` | Unknown, duplicate, missing, non-integer, out-of-range or incompatible argument. |
| `work_limit` | Runner tick, pair or estimated work limit exceeded. |
| `bad_scenario_name` | Host scenario name violates F-02's lowercase ASCII name rules. |
| `tick_budget` | Requested batch exceeds remaining steps; no part of it ran. |
| `empty_soak` | Zero pairs requested. |
| `empty_profile` | Zero measured samples requested. |
| `tick_overflow` | Warmup plus samples exceeds u32 tick numbering. |
| `allocation_failed` | Profile sample storage could not be reserved. |
| `invalid_arena_settings` | Invalid team size, health, damage, cooldown, distance or speed. |

A broken stdout pipe exits 2 without panicking; no report can be delivered then.
A divergence is a failed check, not an API error. A hash collision is possible;
equality does not replace testing the host's complete state-hash coverage.

## Host API

```rust
//! A seeded run, paired soak and profile with identical final state.

use regia_detsim::{arena::{ArenaScenario, ArenaSettings}, golden::{DetRun, TickRate},
    harness::SimHarness, profile::{self, ProfileConfig}, soak};

fn main() -> Result<(), regia_detsim::harness::HarnessError> {
    let settings = ArenaSettings::default();
    let run = DetRun::new(100, 50, TickRate::DEFAULT);
    let mut harness = SimHarness::new(ArenaScenario::new(settings)?, run)?;
    harness.step_n(50)?;
    let final_state = harness.snapshot();
    assert_eq!(final_state.tick, 50);
    assert_eq!(harness.remaining(), 0);

    let template = ArenaScenario::new(settings)?;
    let result = soak::run(|| template.clone(), run, 3)?;
    assert!(result.passed());
    assert_eq!(result.final_snapshot, final_state);

    let measured = profile::run(template, ProfileConfig {
        seed: 100, rate: TickRate::DEFAULT, warmup_ticks: 10, samples: 40,
    })?;
    assert_eq!(measured.final_snapshot, final_state);
    Ok(())
}
```

Implement `DetScenario` around the host's unchanged simulation. Its `reset` owns
seed-to-bootstrap, `step(tick)` uses zero-based tick numbering and `hash_state`
feeds every field that affects future steps in a stable order, including random
streams and settings. `SimHarness::new` validates the name and restores the
calling thread's floating-point environment before reset. Every step is then
bracketed by an `fpu::FpEnvWatch` check (ADR-8840): foreign code that set
flush-to-zero on the thread between two steps is undone before the next one, and
a change made inside a step is counted in `fp_env().during_step()`, a desync risk
to report. `set_fp_check_every(n)` coarsens the cadence (`0`: the construction
guard only). If the harness moves to another thread, initialize that thread with
`fpu::ensure_default_fp_env` too.
The immutable `state` accessor exposes host-specific counters; `into_inner`
returns the finished scenario. No mutable bypass can desynchronize the harness's
tick count. The caller chooses the maximum run length through `DetRun.ticks`.

Use `golden::record` for a retained trace or `golden::compare` for named field
diagnostics. A soak holds two worlds and counters regardless of run length and
checks bootstrap and every tick; a transient mismatch cannot disappear behind a
matching final hash. Its factory must create independent instances of the same
scenario/settings, with no shared mutable state. Profiling stores one duration
per measured tick, rejects invalid requests before bootstrap, and reports timing
separately from the deterministic snapshot. Correctness tests assert step counts
and hashes, never wall-clock budgets.

## Arena rules and coverage

`ArenaSettings` exposes team size, spawn radius, pursuit speed, attack range,
health, damage and tick cooldown. Distances/speed must be finite and at most
1,000,000; distances are positive and speed may be zero. Integer combat values
and team size must be positive. These are original sample defaults, with no game
content or imported game-specific constants.

Bootstrap uses F-02 SplitMix64. Each live unit selects the nearest live opponent;
equal distances keep the first stable index. Movement stops at attack range.
Attacks on one tick are accumulated and applied simultaneously, including attacks
from units killed on that same tick. Scratch buffers are preallocated at bootstrap
and overwritten before use; all authoritative state and settings enter the hash.
Runs continue for the requested tick count after elimination, so run/soak/profile
have the same step contract.

`same_seed_same_snapshot_after_n_ticks` ports the source's seed 100, 50-tick equality
acceptance onto an F-02 engine scenario. Further tests cover every-tick F-02 hash
identity, rejected batches, zero ticks, transient/bootstrapped divergence,
integer percentile ranks, timing/hash isolation, repeated arena reset, cooldowns,
simultaneous lethal attacks, invalid settings, process repeatability and errors.

## DO27 switch

Keep game bootstrap, game AI, content and full-state checksums with the host.
Replace generic orchestration with these APIs after adapting `DetScenario`.
The sample's fingerprints are not expected to equal game fingerprints. Before
removing a helper, run the game's original harness/soak/profile tests and its
pinned tunnel checksum, no-vertical-jump, per-tick determinism fuzz fixtures and
3600-tick two-seat replay check. Regia does not update those pins.
