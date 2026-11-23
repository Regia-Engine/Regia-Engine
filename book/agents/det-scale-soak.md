# Deterministic scale soak

`regia_detsim::scale_soak` exercises 40 independent input seats and 900 live entities by
default (F-07, ADR-8148). It drives every seat, integrates every disc, recycles IDs
with FIFO generations, and resolves ordered contacts across three height bands.
Counts, geometry, speed and recycle interval are `SoakConfig` settings. This is a
synthetic engine workload; its timings do not estimate the full game's tick cost.

## Run and verify

```sh
cargo test -p regia_detsim --test scale_soak
cargo run --release -p regia_detsim --example scale_soak -- --verify
```

The example defaults to seed `5eed09000040`, 120 warmup ticks and 600 measured
ticks at 60 Hz. It restores the default floating-point environment at startup;
embedded hosts call `regia_detsim::fpu::ensure_default_fp_env()` on their sim
thread before stepping, as required by the lockstep math kit. `--verify` compares every tick against the all-pairs oracle after
timing, and reports the first diverging tick on failure. Setup, hashing, report
construction and the oracle are outside the timed region. `step` includes input,
movement, ID recycling, grid construction and contact resolution, plus the small
overhead of inner timer reads. Profile release binaries; debug timings are not
performance evidence. Record the device, build head, profile and host load.

Options: `--ticks N`, `--warmup N`, `--seed HEX`, `--seats N`, `--entities N`,
`--budget-ns N`, `--expected-hash HEX`, `--verify`, `--help`. Unknown options and
missing/invalid values fail. The default threshold is **100,000 ns (0.1 ms)**.
Every pass strictly above it is flagged, even if p99 is below the threshold.
An over-budget result is diagnostic and exits successfully; malformed input,
hash mismatch or oracle divergence exits unsuccessfully.

Use the first device's `hash` as `--expected-hash HEX` on the other devices with
identical options. This is a live cross-device comparison, not a newly blessed
golden. No golden files are written, and `REGIA_BLESS` has no effect on this runner.
`ScaleScenario` implements the F-02 `DetScenario` trait, so outside runners can
register it or use `record`, `compare`, and `Injection` directly.

## Agent surface

As with [the golden harness](det-golden.md), this standalone test has no engine
runtime state or RPC method. Read stdout lines beginning `regia-scale-soak ` and
parse the remainder as JSON. The object has:

| Field | Meaning |
|---|---|
| `scenario`, `target` | `scale_soak` and the running architecture/OS. Deck and the build server both use `x86_64-linux`; record hardware separately. |
| `clock` | `thread_cpu` when the operating system provided a thread CPU clock, otherwise `wall`. Added by the tick profiler (ADR-8498). Quantile fields are unchanged. |
| `seed`, `hash` | Fixed-width lowercase hexadecimal seed and hash of every tick, including warmup. |
| `seats`, `entities`, `half_extent`, `radius`, `speed`, `recycle_interval` | Exact settings used by the scenario. |
| `ticks`, `warmup`, `rate_hz` | Measured tick count, untimed warmup count, and simulation rate. |
| `budget_ns`, `status` | Budget and `ok` or `over_budget`. This status describes timing only. |
| `contacts`, `recycled` | Cumulative workload counters including warmup. |
| `passes` | Five entries: `input`, `movement`, `recycling`, `contacts`, `step`. |

Each pass includes `samples`, `p50_ns`, `p99_ns`, `max_ns`, `over_budget_count` and
`over_budget_ticks`. Quantiles use nearest rank, `ceil(p * samples / 100)`, on
sorted samples. The tick list is in observation order and contains absolute
scenario ticks, so its first possible entry is `warmup`. A value equal to the
budget passes. The complete step overlaps its inner passes; do not add it again.

Successful `--verify` prints `regia-scale-soak-check ` with JSON fields `status`
(`ok`), `ticks` (including warmup), and `hash`. Parse both lines to establish
correctness as well as timing. On failure, stderr begins with
`regia-scale-soak-error ` followed by the lowercase ASCII error code and detail:

| Code | Meaning |
|---|---|
| `invalid_soak_config` | Invalid seat/entity counts, geometry, speed or recycle interval. |
| `invalid_soak_measurement` | No measured ticks, or warmup + ticks exceeds u32. |
| `invalid_arguments` | Unknown option, missing value or malformed numeric value. |
| `hash_mismatch` | The run differs from `--expected-hash`. |
| `diverged` | The grid and all-pairs state first differ at the named tick. |

`ScaleScenario::step_observed` brackets the complete tick and all four passes.
Implement `SoakObserver` with the host's tracing span guards for profiling inside
an application; the core has no tracing dependency. The example uses `PassTimer`
to collect nanoseconds. Observers cannot mutate simulation state through this API.

## Farm commands and cleanup

Build on the authorized host through its farm wrapper; never build on the workstation.
Run the release example on the build server with the lavapipe-only environment from the
lane brief (the executable itself uses no graphics). For the Deck, copy the Linux
binary under `/home/deck/regia-test/` and run under `flock /tmp/regia-deck.lock`.
Build the Android binary with `cargo ndk -t arm64-v8a build --release -p
regia_detsim --example scale_soak`, then use only `adb -s 54101FDAP0015H`, under
`flock /tmp/ferrite-adb-54101FDAP0015H.lock`. Remove the pushed executable afterward.
Build/run the Windows example under `<build-dir>` on the N150. Never replace a
missing device with an unapproved phone, or infer measurements from another CPU.

## Acceptance and DO27 switch

`scale_soak_900_entities_hash_is_stable` compares independent 600-tick runs at
40 seats/900 entities and requires actual contacts and ID reuse. Other tests
compare every tick with the all-pairs oracle, exercise uneven counts and wrapping
recycle batches, validate reset and bad settings, identify an injected bit at its
first tick, and test quantiles/exceedances from fixed samples. No unit test asserts
an elapsed-time budget. The profiling observer must preserve the state hash.

DO27 may replace its generic soak orchestration with this runner or register its
own full-sim `DetScenario`; retain its game-specific roster/content setup. Before
retiring its own code, its agent must run its own deterministic stress checks,
`determinism_fuzz_short_fixtures_agree_every_tick`, the 3600-tick two-seat replay
hash, `switchbacks_tunnel_fight_lockstep_checksum_is_pinned`
(`0xbd96_cd18_09dc_958d`), and
`a_net_client_predicting_its_hull_through_the_tunnels_never_shows_a_vertical_jump`.
Regia's synthetic hash does not replace those pins. Switching contact rules is
still governed by F-06's compatibility guidance and A-10.

## AI-on soak

`regia_detsim::scale_soak::ai` (ADR-8561) adds a second scenario, `AiSoakScenario`
(`ai_soak`), with AI decisions, shots and range queries turned on. It is an
engine-primitive workload: teams of discs that acquire, pick and chase targets
and fire shots. It is not a game's AI, combat or tuning, and its timings do not
measure a game's tick. Every count and radius is an `AiSoakConfig` setting.

Defaults: 4 teams, 40 seats, 900 units in a 192 m square arena, 60 Hz, seed
`5eed_a100_0900`. Each tick runs ten systems, in this order. The profiler row
names come from `AI_SOAK_SYSTEMS`:

| Row | Work per tick |
|---|---|
| `input` | Every seat computes a drive direction. |
| `build` | One `DetSpatial` index over all units (ADR-8318). |
| `acquire` | Per unit: `first_k_within_by(acquire_radius, enemy mask, acquire_k = 24)`. |
| `target` | Per unit: a scored pick over `within(engage_radius)`; the lowest score wins, and equal scores keep the lowest slot. |
| `ally` | Per unit: `nearest` teammate within `ally_radius`. |
| `keep_out` | Per unit: `within(keep_out_radius)` over every team; the neighbors push the unit away. |
| `move` | Integrate, clamp to the arena, and refile each unit with `DetSpatial::update`. |
| `shots` | Fire up to `shots_per_tick` new shots (capped at `max_shots` live). Each live shot runs `first_along_segment` along its step. A hit runs a burst `within(burst_radius)`. |
| `recycle` | Units at zero health free their `DetHandle` and respawn with a FIFO-reused one. |
| `contacts` | Ordered disc contacts through `DetBroadphase::for_each_contact`. |

The decisions (`acquire` through `keep_out`) read the positions from the start
of the tick. Movement applies after them.

### Run

```sh
cargo test -p regia_detsim --test scale_soak ai_soak
cargo run --release -p regia_detsim --example scale_soak -- --ai --verify
cargo run --release -p regia_detsim --example scale_soak -- --ai --curve 300,600,900,1500
cargo run --release -p regia_detsim --example scale_soak -- --ai --long --ticks 216000
```

`--ai` must be the first argument. The other options are `--ticks N` (measured,
default 600), `--warmup N` (default 120), `--seed HEX`, `--teams N`, `--seats N`,
`--entities N`, `--shots-per-tick N`, `--pass-line-ns N` (default 4,000,000),
`--expected-hash HEX`, `--verify`, `--curve N,N,...`, `--long`, `--clock
thread_cpu|wall` and `--help`.

`--clock thread_cpu` (the default) times on the thread CPU clock, or on wall time
when the platform has no thread clock. Windows' thread clock advances in
scheduler quanta of about 15.6 ms, which is coarser than one tick, so a Windows
row reads 0 or 15,625,000 ns. Run Windows hosts with `--clock wall` on an idle
machine. The report's `clock` field always names the clock used.

- `--verify` replays the measured run with the oracle on. Every query also runs
  a linear scan over all rows, and the accepted slot sequences must match. The
  replay's state hash must also match the measured run on every tick. Use it
  for the full length of the run.
- `--curve` runs each entity count (seats are capped at the count) and fits
  `p = a * n^b` by least squares on logarithms, for p50 and p99 of the whole tick.
- `--long` records resident memory (`/proc/self/status`, Linux and Android
  only, otherwise `null`) and a counting allocator's totals at the start, after
  warmup (`warm`), at one minute (warmup + 3,600 ticks, when the run is longer)
  and at the end. `growth` runs from the start, so it includes the one-time sizing
  of the index and buffers; `steady_growth` runs from the end of warmup.

### Output

| Line prefix | JSON fields |
|---|---|
| `regia-ai-soak ` | `scenario` (`ai_soak`), `clock` (`thread_cpu` or `wall`), `target`, `seed`, `teams`, `seats`, `entities`, `shots_per_tick`, `max_shots`, `ticks`, `warmup`, `rate_hz`, `pass_line_ns`, `status` (`inside` when the whole-tick p99 is at or below the pass line, else `over`), `hash`, `tick` (`p50_ns`, `p90_ns`, `p99_ns`, `max_ns`), `systems` (one row per system: `system`, `budget_ns`, `p50_ns`, `p90_ns`, `p99_ns`, `max_ns`), `over_line_count`, `over_line_ticks_first` (up to 32 absolute ticks), `counters`. |
| `regia-ai-soak-check ` | `status` (`ok`), `ticks` (warmup included), `hash`, `queries` (comparisons per query kind: `acquire`, `target`, `ally`, `keep_out`, `segment`, `burst`). |
| `regia-ai-soak-curve ` | `target`, `points` (`entities`, `p50_ns`, `p99_ns`), `exponent_p50`, `exponent_p99`. |
| `regia-ai-soak-memory ` | `samples` (`at`: `start`, `warm`, `one_minute`, `end`; `tick`, `rss_bytes`, `allocations`, `live_bytes`), `growth` and `steady_growth` (each `rss_bytes`, `allocations`, `live_bytes`). |

`status` describes timing only and never fails the run. The `systems` and
`tick` objects have the same shape that `sim.tick_profile` returns
([Tick profile](det-tick-profiler.md)). A host that runs the AI soak publishes
`AiSoakMeasurement::report` as its `DetTickProfile` resource or from
`FixedStepSim::tick_profile`. `sim.tick_profile` then answers with one row per
system.

### Error codes

On failure, stderr begins with `regia-scale-soak-error ` followed by the code:

| Code | Meaning |
|---|---|
| `invalid_ai_soak_teams` | `teams` outside 2..=64, or more teams than entities. |
| `invalid_ai_soak_counts` | `seats` zero or above `entities`. |
| `invalid_ai_soak_geometry` | Arena half-width, unit radius or speed out of range. |
| `invalid_ai_soak_queries` | A query radius or the index cell is not finite and positive, or `acquire_k` is zero. |
| `invalid_ai_soak_shots` | `shots_per_tick` above `max_shots`, or a bad shot speed, radius, life or burst radius. |
| `invalid_ai_soak_combat` | Health or cooldown is zero. |
| `invalid_ai_soak_measurement` | Zero measured ticks, more than 1,048,576 measured ticks, or warmup + ticks above u32. |
| `invalid_arguments` | Unknown option, missing value, malformed number or a `--curve` with fewer than two counts. |
| `hash_mismatch` | The run differs from `--expected-hash`. |
| `oracle_mismatch` | A grid query's accepted sequence differs from the linear scan. Followed by the tick, query, subject and both sequences. |
| `diverged` | The oracle replay's state first differs at the named tick. |

### Tests

- `ai_soak_900_hash_is_stable`: two independent 600-tick runs agree, and the
  run hash equals the pin in `tests/support/ai_soak_pin.rs`. Every query kind,
  firing, hits, bursts, recycling and contacts actually run.
- `ai_soak_queries_match_the_linear_scan_every_tick`: oracle mode for 120
  ticks. Every query kind is compared, and the state is the same with the oracle
  off.
- `ai_soak_counts_are_configurable`: 2 to 64 teams, 1 to 40 seats, 2 to 1,500
  units, firing off and on, under the oracle, plus every validation code.
- `ai_soak_profile_has_a_row_per_system`: a stepping clock gives fixed
  "timings". The report has the ten rows in run order and serializes as
  `sim.tick_profile` does.
- `det_golden`'s `ai_soak_900_matches_the_pinned_fingerprint_on_every_target`
  carries the pin to the aarch64 legs of `cargo xtask det-golden`.

No test asserts a time. Pass lines and device numbers are evidence in
`docs/evidence/det-scale-soak-ai/`.
