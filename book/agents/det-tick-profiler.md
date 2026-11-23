# Tick profile

`regia_detsim::profile::TickProfiler` records one nanosecond sample per system per tick, plus the
whole tick, into a fixed ring. It is a separate struct. It does not read or write simulation
state, so turning it on cannot move a desync hash. The names are the same
`regia_detsim::hash::Systems` list the hash uses: one list names both.

```rust
use regia_detsim::hash::Systems;
use regia_detsim::profile::{HostClock, ProfileSettings, TickProfiler};

const SYSTEMS: Systems<2> = Systems::new(["move", "ai"]);

fn profile_one_tick(tick: u64) -> regia_detsim::profile::ProfileReport {
    let mut profiler = TickProfiler::new(SYSTEMS, HostClock::probe(), ProfileSettings::default())
        .expect("ring");
    profiler.begin_tick();
    profiler.begin("move").expect("move");
    // step the move system, then:
    profiler.end("move").expect("move");
    profiler.begin("ai").expect("ai");
    // step the ai system, then:
    profiler.end("ai").expect("ai");
    profiler.end_tick(tick);
    profiler.report()
}
```

`TickProfiler::enter` returns a guard that ends the system when dropped. `begin` and `end` stay
on the thread that opened the tick. A second `begin` of a system that is already open is ignored.
An unknown name returns `unknown_system`.

The whole-tick time is the clock from `begin_tick` to `end_tick`, not the sum of the systems, so
a system that nests others is not added twice into the tick total. Each system's own time is
inclusive: it includes work it calls.

## Clock

`HostClock::probe` reads per-thread CPU time when the operating system has it:

| OS | Call |
|---|---|
| Linux, Android | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` |
| macOS | `clock_gettime` with the thread CPU clock |
| Windows | `GetThreadTimes` (user plus kernel, 100 ns units) |

If that call fails, the profiler uses a monotonic elapsed clock and says so. Reports use
`clock: "thread_cpu"` or `clock: "wall"`. Tests inject samples with `ManualClock` so a quantile
does not depend on a real timer. The clock is not read from a lockstep step function.

Thread CPU time is the time this thread actually ran. A wall clock also counts time the operating
system gave to other threads, so a busy machine stretches every system by the same wait and hides
which system was over budget.

## Ring, quantiles, budgets

The ring defaults to 3600 ticks (60 s at 60 Hz) and can be set from 1 to 1 048 576. `begin`,
`end` and `end_tick` allocate nothing after `TickProfiler::new`. `report` may allocate; that is
the publish path.

Quantiles are nearest rank on the samples the ring still holds: the 1-based rank is
`ceil(percent * n / 100)`, the value is the sample at that rank (NIST/SEMATECH e-Handbook of
Statistical Methods § 7.2.6.2). Integer arithmetic is `(n * percent).div_ceil(100)`. For two
samples, p50 is the lower one and p99 is the higher one. An empty ring reports 0.

The default budget is 4.0 ms (4 000 000 ns) for the whole tick and for each system. A sample is
over budget only when it is strictly greater. `over_budget` lists every retained tick that
exceeded the tick budget or any system budget, oldest first, with every system's nanoseconds.
The ring drops a tick, including an over-budget one, once a newer tick overwrites it.

A tracing span `detsim_system` (field `system`) is entered while an INFO subscriber is installed.
With no subscriber the tick path does not touch that slot, and the profiler stays `Send`.

## Agent method

A sim host stores `FixedStepSim::tick_profile` after each step. An ECS game that does not run
the host inserts `regia_framework::sim_host::DetTickProfile`. The host's report wins when both
exist.

| Method | Tool | Permission | Mutating | Params | Errors |
|---|---|---|---|---|---|
| `sim.tick_profile` | `sim_tick_profile` | read | no | `count` integer, optional | `no_tick_profile`, `bad_count` |

```json
sim.tick_profile { "count": 10 }
→ { "clock": "thread_cpu", "samples": 3600, "tick_budget_ns": 4000000,
    "system_budget_ns": 4000000,
    "systems": [{ "system": "move", "budget_ns": 4000000, "p50_ns": 800000,
                  "p90_ns": 1200000, "p99_ns": 2100000, "max_ns": 3000000 }],
    "tick": { "p50_ns": 2500000, "p90_ns": 3200000, "p99_ns": 3900000, "max_ns": 5100000 },
    "over_budget": [{ "tick": 1200, "total_ns": 5100000,
                      "systems": [{ "system": "move", "ns": 3000000 }] }] }
```

`samples` is how many ticks the ring still holds. `count` keeps the newest over-budget ticks
(1 to 3600). Omit `count`, or pass null, to return every over-budget tick the ring still holds.
Fewer than `count` returns all of them. A non-integer `count` is rejected by the shared parameter
gate as `F0002` (`wrong_type`) before this method runs.

| Code | When |
|---|---|
| `no_tick_profile` | No host report and no `DetTickProfile` resource. |
| `bad_count` | `count` is present and is not an integer from 1 to 3600. |
| `F0002` | `count` is not an integer (shared type gate). |

Read p99 against the budget, and also read `over_budget`: a single slow tick can sit past p99
and still be the one that missed the frame.

## Editor view

The editor's Profiler window has a **Lockstep** tab (ADR-8562). It shows the same report as
`sim.tick_profile`: one row per system in the profiler's list order (idle systems too), each with a
p99 bar against the whole-tick budget line, p50, p99 and max in milliseconds, and the row in red when
its p99 is over its budget; then the whole-tick row; then the over-budget ticks the ring holds,
newest first, each with its slowest system. It reads the edit world's sim host or `DetTickProfile`,
else play's (copied by the editor loop only while the tab is shown). Below the rows, a toggle per
part of the deterministic-index debug draw (Grid, Occupancy, Candidates, Contacts, Queries) sets the
same switch as `sim.det_debug_draw`; the agent parity rows `profiler.lockstep_*` name that method.
Selecting the tab is view-only; the data is `sim.tick_profile`.

The model is `regia_editor::profiler_model::lockstep_view` (pure, tested headless:
`cargo test -p regia_editor --lib lockstep_profile_view_shows_every_system_row`).
