# Determinism goldens

The lockstep policy (ADR-8012 A-4) says every peer, on every OS and CPU architecture, computes the
same bits from the same inputs. The rules on [Lockstep math and the determinism scan](lockstep-math.md)
keep the code honest; the golden harness proves it: a scenario is stepped for a fixed number of
ticks, the state is hashed after every tick, and the hashes are compared with a committed golden.
When they differ, the report names the **first diverging tick** (ADR-8046).

The harness is `regia_detsim::golden`, re-exported as `regia_test_utils::det_golden`. It depends only
on `regia_detsim`, so a game's sim crate and a cross-built test binary link it without the renderer.

## Run it

```sh
cargo test -p regia_detsim --test det_golden -- --nocapture
```

`engine_scenarios_match_their_committed_goldens` checks the six engine scenarios and prints one
line per scenario:

```text
regia-det-golden {"scenario":"steer_slalom","target":"x86_64-linux","seed":"0x000000005eed0001","ticks":1200,"rate_hz":60,"status":"ok","hash":"32317dd9a9adab0f","expected":"32317dd9a9adab0f","first_diverging_tick":null,"expected_tick_hash":null,"actual_tick_hash":null,"agrees_with_reference":true,"golden":".../steer_slalom.detgold","detail":"1200 ticks match this target's committed hash"}
```

That `regia-det-golden ` line is the agent surface: the harness has no runtime state, so there is
no protocol method; a runner (a cross-arch leg under qemu, `adb`, the Mac, the Deck, the Windows rig)
greps the lines and reads the JSON. The existing `agent.run_tests` method can run the regression
target with `{"package":"regia_detsim","target":"det_golden"}`; poll its returned job through
`agent.job_status` as described in [Jobs and test runs](jobs.md). No new RPC is added.

Fields:

| Field | Meaning |
|---|---|
| `scenario`, `target` | The scenario and the running build's `<arch>-<os>` (`x86_64-linux`, `x86_64-windows`, `aarch64-macos`, `aarch64-android`; the Deck is `x86_64-linux`). |
| `seed`, `ticks`, `rate_hz` | The run. |
| `status` | One of the codes below. |
| `hash`, `expected` | This build's run hash and the committed one it was compared with (16 hex digits, or `null`). |
| `first_diverging_tick` | The first tick whose hash differs from the golden's per-tick trace, or `null`. |
| `expected_tick_hash`, `actual_tick_hash` | The two hashes at that tick. |
| `agrees_with_reference` | Whether this build equals the reference target (the one whose trace the golden keeps). |
| `golden`, `detail` | The golden file and a sentence for a human. |

### Status codes

| Code | Passes | Meaning | What to do |
|---|---|---|---|
| `ok` | yes | Equals this target's hash and, unless explicitly planted, the reference. | Nothing. |
| `unrecorded_target` | yes | This target has no `hash.<target>` line yet, and equals the reference. | Record it: rerun with `REGIA_BLESS=1` on that host and commit the golden. |
| `blessed` | yes | `REGIA_BLESS=1` wrote the golden. | Commit it, with the reason in the message. |
| `diverged` | no | The bits differ. `first_diverging_tick` names where. | Find the tick-path change: a std float method, a SIMD type, an FMA, hash order, an uninitialized input. Never bless a cross-arch difference away. |
| `golden_missing` | no | No golden file. | Record it with `REGIA_BLESS=1` on `x86_64-linux`. |
| `golden_stale` | no | The golden was recorded with another seed, tick count, rate or name. | The scenario's run changed on purpose: bless again. |
| `golden_invalid` | no | The golden does not parse or cannot be read. | Fix or re-bless it. |
| `golden_unwritable` | no | `REGIA_BLESS=1` could not write the file. | Check the directory. |
| `disagrees_with_reference` | no | A clean scenario matches its target line but disagrees with the reference. | Fix the cross-target difference; a previously blessed target line cannot waive determinism. |
| `bless_refused_clean_differs` | no | Blessing a clean non-reference target would record differing bits; the file is unchanged. | Diagnose the first diverging tick and rerun after the fix. |

Harness errors (`DetError::code`): `bad_scenario_name` (names are 1-64 bytes of `a-z`, `0-9`, `_`),
`duplicate_scenario`, `injection_unsupported`, `golden_invalid`.

## Golden files

`<scenario>.detgold`, text, one `key=value` per line; the engine's are in
`crates/regia_detsim/goldens/`:

```text
# Determinism golden (ADR-8046). Written with REGIA_BLESS=1; do not edit by hand.
format=regia-det-golden-1
scenario=steer_slalom
seed=0x000000005eed0001
ticks=1200
rate_hz=60
hash.x86_64-linux=32317dd9a9adab0f
trace_target=x86_64-linux
t=b032c11420e12d06
...
```

- `hash.<target>=` is the run hash of one target: FNV-1a 64 over the tick count and every tick
  hash. There is one per target that has been checked and blessed.
- `trace_target` and its `t=` lines (one per tick) are the per-tick hashes of the reference target.
  A bless keeps the existing trace target's lines unless it is that target, so the trace stays the
  `x86_64-linux` one.
- A target without its own line must equal the reference (`unrecorded_target`). Ordinary scenarios
  must agree even when their own target line matches. A deliberately planted scenario may carry
  a different per-target hash: only then can `ok` accompany `agrees_with_reference: false`.

The tick hash is FNV-1a 64 over the little-endian bits of every state field in hash order. NaNs are
folded to `0x7fc0_0000` (x86-64 and aarch64 produce different NaN payloads); `-0.0` and `0.0` stay
distinct.

## Write a scenario

A scenario follows the lockstep rules: `regia_detsim::math` for transcendentals, no SIMD types, no
hash-container iteration, no threads, inputs only from the seed.

```rust
use regia_test_utils::det_golden::{self, DetRun, DetScenario, StateHasher, TickRate};

struct Drift {
    x: f32,
    v: f32,
}

impl DetScenario for Drift {
    fn name(&self) -> &str {
        "my_game_drift"
    }
    fn default_run(&self) -> DetRun {
        DetRun::new(42, 600, TickRate::DEFAULT)
    }
    fn reset(&mut self, run: &DetRun) {
        self.x = 0.0;
        self.v = (run.seed % 7) as f32;
    }
    fn step(&mut self, _tick: u32) {
        self.v *= 0.99;
        self.x += self.v / 60.0;
    }
    fn hash_state(&self, h: &mut StateHasher) {
        h.f32("x", self.x);
        h.f32("v", self.v);
    }
    // Optional: flip bit `bit` of the first hashed field, for fault injection.
    fn flip_bit(&mut self, bit: u32) -> bool {
        self.x = f32::from_bits(self.x.to_bits() ^ (1 << (bit % 32)));
        true
    }
}

fn record_drift() -> usize {
    let mut s = Drift { x: 0.0, v: 0.0 };
    let run = s.default_run();
    det_golden::record(&mut s, &run).tick_hashes.len()
}
```

Hash every field a later tick reads. Use `h.item("unit", i)` before each element of a list, so a
report names `unit[3].x` rather than a bare `x`.

### Register it from a game

A game keeps its goldens in its own repository and registers its scenarios beside the engine's in
one test:

<!-- ignore-reason: `MyScenario` and the golden directory are the game's -->
```rust,ignore
use regia_test_utils::det_golden::{DetRegistry, DetScenario, GoldenMode, REPORT_PREFIX};

#[test]
fn determinism_goldens() {
    let mut registry = DetRegistry::with_engine_scenarios();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("det_goldens");
    registry
        .register(dir, || Box::new(MyScenario::new()) as Box<dyn DetScenario>)
        .expect("unique name");
    for report in registry.check_all(GoldenMode::from_env()) {
        println!("{REPORT_PREFIX}{}", report.to_json());
        assert!(report.passed(), "{report}");
    }
}
```

`REGIA_DET_GOLDEN_DIR` points the engine scenarios' golden directory somewhere else (a runner that
ships the goldens next to a cross-built binary).

## Find where two runs part

`golden::compare(expected, actual, &run, injection)` records both and returns a `Divergence`: the
first diverging tick, both hashes, and every field that differs there (`car[0].pos_x: 0x... -> 0x...`).
An `Injection { tick, bit }` flips one state bit in the second run after that tick steps, which is how
`a_flipped_bit_names_the_first_diverging_tick` proves the report: a bit flipped at tick 437 is
reported at tick 437, naming `car[0].pos_x` and exactly that bit.

## The engine scenarios

| Name | What it exercises | Run |
|---|---|---|
| `steer_slalom` | Four vehicles (`scenarios::Vehicle`) with seeded throttle, strafe and steer that change every 30 ticks: `sin`, `cos`, `sqrt`, clamps, grip and drag. | 1200 ticks at 60 Hz |
| `slope_follow` | Eight sleds sliding under gravity on an analytic hill field, friction, walls: `sin`, `cos`, `atan2`, `sqrt`. | 900 ticks at 60 Hz |
| `disc_collide` | 24 discs in a box, pushed apart and exchanging impulses pair by pair in `(i, j)` order: branchy `sqrt` code with a contact count. | 900 ticks at 60 Hz |
| `hover_libm_1000` | The lockstep hover step (`regia_detsim::hover`, [Hover step (lockstep)](det-hover.md)): the engine's default hull at full throttle from rest, the libm-1000 run. Its final pose and velocity equal the committed hash line of `tests/fixtures/hover/libm-1000.txt`. | 1000 ticks at 60 Hz |
| `hover_bad_input` | The hover step's bad-input guards ([Hover step (lockstep), Bad input](det-hover.md#bad-input)): the default hull on seeded input over a rolling floor with ledges, fed NaN and infinite input axes and floors, `dt` values of 0, `-dt`, NaN and infinity, and a poisoned state every 200 ticks (a recovery); air-time rules, then clamped. Hashes the recovery count too. | 600 ticks at 60 Hz |
| `hover_mixed` | Every rule of the hover step under seeded input ([Hover step (lockstep), Mixed golden](det-hover.md#mixed-golden)): 12 hulls (the default hull and two derived ones on four worlds: gravity and air (9.81, 1.2), (3.71, 0.02), (1.35, 5.3), (0.113, vacuum)) on a course with ramps, crests, a 3 m drop, a 51-degree face and steps past the grade teleport limit; signed zeros, saturated, over-range, threshold and deadband-edge inputs; both flight modes, auto-level and modern yaw on and off; scheduled hops and jumps. Hashes the pose, every `HoverState` word, the input and floor per hull, and the branch counts. | 3600 ticks at 60 Hz |
| `lowg_ballistic_mixed` | The low-g and ballistic rules on the same four worlds in both low-g modes (eight cases): seeded suit apexes through `lowg::arc_row_for` and `lowg::jump_velocity`, debris tosses with drag (some fast enough to hit the drag cap) through `ballistic::step_debris`, mortar shells through `ballistic::step_per_tick` under the mortar gravity and the mode's charge, suit jumps under the jump-feel gravity, and each world's `hover::hop_launch_speed`. | 3600 ticks at 60 Hz |
| `hover_fleet_terrain` | The hover host kit ([Hover step (lockstep), Host kit](det-hover.md#host-kit)): a `HoverFleet` of 64 hulls of two types on rolling terrain (`RollingField`), the ground attitude pass, seeded inputs handed over in descending slot order, a jump every 120 ticks, air-time then clamped rules. Hashes every hull's position and state. | 1800 ticks at 60 Hz |
| `ground_vehicle_mix` | The lockstep ground vehicles ([Ground vehicles (lockstep)](det-ground-vehicles.md)): four worlds (gravity 9.81, 1.62, 3.71, 24.79 m/s²), each with 8 tracked and 8 wheeled vehicles of two parameter sets on a procedural hill field (`GroundField`), seeded inputs every 30 ticks (pivot turns, handbrake pulls, releases), grade limits and coasting roll-back. Hashes every state word and the grade-limited and recovered counts. | 1800 ticks at 60 Hz |

## The 60 Hz vs 120 Hz rate gate

`golden::rate_gate` drives a `RateModel` (reset, step with `[throttle, strafe, steer]` at a `dt`,
read velocity and heading) at 60 and 120 Hz with the same inputs and checks:

| Gate | Limit |
|---|---|
| Position after 1 s of full throttle | 0.25 m |
| Position after 5 s of full throttle | 1.0 m |
| Heading after 2 s of full steer at rest | 2 degrees |
| Flat shot, 160 units/s over 80 units, time to impact | max(16 ms, one 60 Hz tick) |

`rate_gate::measure(&mut model, &RateGates::default())` returns the numbers, and
`RateGateReport::failures` lists the gates a model misses. Do not retune a model to pass. The engine
vehicle passes (`rate_60_vs_120_position_heading_and_flat_shot`), and so does the hover step
(`hover::HoverRateModel`, `rate_60_vs_120_on_the_hover_step`); a model that ignores `dt` fails
(`rate_gates_reject_a_model_that_ignores_dt`).

The engine scenarios and rate gate select float bounds through
`regia_detsim::float::det_min`, `det_max` and `det_clamp` (review G2). The committed
scenario traces remain the acceptance pins; do not bless them when adopting these
ordering helpers. Run `cargo test -p regia_detsim --test det_golden -- --nocapture`.

## Blessing determinism goldens safely

`DetScenario::planted()` defaults to false. Both `record` and `record_injected` carry it into
`DetTrace::planted`; injecting a bit into an ordinary scenario does not make it planted.
Only diagnostic negative controls, such as `planted_simd_vec3a`, return true. A hand-built
`DetTrace` should set `planted: false`. The flag is run policy, never part of the hash or file.

A clean trace that matches a previously blessed target hash but differs from the reference
returns `disagrees_with_reference`; `DetReport::passed()` is false even in ordinary `cargo test`.
Other mismatches keep `diverged`. A clean non-reference bless that would differ returns
`bless_refused_clean_differs` before writing. Both new statuses carry
`agrees_with_reference: false`, the reference hash in `expected`, and the first differing tick
and tick hashes when a reference trace exists. A golden with no trace still fails, with null
tick fields. Runner verification maps either failure to `scenario_failed`.

For intentional scenario changes, review the changed bits, update the reference target first,
and record agreeing target lines afterward. Missing goldens still establish their reference
on the recording target; use `x86_64-linux` for repository fixtures. Do not mark game scenarios
planted to suppress a desync. See the [user guide](../user-guide/deterministic-simulation.md).

Review G3 / ADR-8231 clarifies the existing float policy: the golden `StateHasher` preserves
`-0.0` versus `+0.0`, while the desync hasher's canonical rule folds both to positive zero.
Goldens therefore expose zero-sign drift that a desync hash can hide; both fold NaN payloads.
No hasher or scenario bits change. `regia-det-golden-1` is registered as the existing committed
fixture format, with no encoding change, version bump or migrator.

## Match rules

`match_rules_ctf_koth_3600` is registered from `regia_rules::proof` into the same harness. It is not
one of the three engine scenarios (`engine_scenarios()` stays those three). Its golden is
`goldens/match_rules_ctf_koth_3600.detgold`. The runners treat it as a clean scenario: every leg
must report it, and the hashes must agree. See [Match modes and match flow](match-rules.md).
