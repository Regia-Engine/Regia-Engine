# Determinism goldens on every target

[Determinism goldens](det-golden.md) proves bit identity on one target at a time. Lockstep needs it
across targets: the Deck, Android, the Mac and Windows play in the same matches (ADR-8012 A-4). The
runners run the same test binary (`regia_detsim --test det_golden`) on each target and compare what
the targets printed (ADR-8081).

```sh
cargo xtask det-golden gate                                  # the merge gate's leg: host + qemu-aarch64
cargo xtask det-golden run --legs host,qemu,mac,windows,deck --adb <serial>
cargo xtask det-golden stage --legs android --out <dir>      # build only; run the leg's script elsewhere
cargo xtask det-golden verify <dir> --require host,qemu      # compare <dir>/logs/<leg>.log
```

## Legs

| Leg | Target the reports name | What runs | Needs |
|---|---|---|---|
| `host` | `x86_64-linux` | the test binary built here | nothing |
| `qemu` | `aarch64-android` | a **static** aarch64 Android executable (`-C target-feature=+crt-static`), built here with cargo-ndk, under `qemu-aarch64` user mode | `qemu-aarch64` on `PATH` (`QEMU_AARCH64` overrides); NDK r27 to link (r28's `libdl.a` does not link statically) |
| `android` | `aarch64-android` | the device binary over `adb -s <serial>` in `/data/local/tmp/regia-det-golden`, removed after | `--adb <serial>` or `REGIA_ADB_SERIAL`; holds `/tmp/ferrite-adb-<serial>.lock` (`REGIA_ADB_LOCK=` turns the lock off) |
| `mac` | `aarch64-macos` | the staged crate built and tested on the Mac, the directory removed after | `--mac-host` / `REGIA_MAC_HOST` (ssh), cargo there |
| `windows` | `x86_64-windows` | the staged crate built and tested on the rig (cmd shell, 4 build jobs), source and target removed after | `--windows-host` / `REGIA_WINDOWS_HOST`; refuses with `rig_busy` while `<build-dir>` names someone (`REGIA_WINDOWS_RESERVE=` turns it off) |
| `deck` | `x86_64-linux` | the host binary copied to the Deck, removed after | `--deck-host` / `REGIA_DECK_HOST`; holds `/tmp/regia-deck.lock` |

The qemu leg reports `aarch64-android` because the binary is an Android one; the bits are the
same as the device's, and the golden keeps one hash per target key.

## Stage, run, verify

`stage --out <dir>` builds only what the chosen legs need:

- `goldens/`: the committed `.detgold` files (the binaries read them through `REGIA_DET_GOLDEN_DIR`);
- `bin/host`, `bin/aarch64-static`, `bin/aarch64-android`: the test binaries;
- `bundle/`: `regia_detsim` as a standalone package (generated `Cargo.toml` with the exact versions
  in `Cargo.lock` and the workspace dev profile) for the native Mac and Windows builds;
- `scripts/<leg>.sh`: one POSIX script per leg that needs only `sh`, `tar`, `ssh`, `adb`, `flock`
  or `qemu-aarch64`.

A host that may not build (the one with adb, say) runs a leg from a stage built elsewhere:

```sh
sh <dir>/scripts/android.sh > <dir>/logs/android.log 2>&1
```

`run` stages, runs each leg's script here and verifies. `verify <dir>` reads `logs/<leg>.log`
(the file name is the leg) and writes `summary.md` and `summary.json`:

```text
| Scenario | host (x86_64-linux) | mac (aarch64-macos) | windows (x86_64-windows) |
|---|---|---|---|
| steer_slalom | 536e1716 ok | 536e1716 ok | 536e1716 ok |
| slope_follow | 09e1674f ok | 09e1674f ok | 09e1674f ok |
| disc_collide | c671fe6f ok | c671fe6f ok | c671fe6f ok |
| planted_simd_vec3a | 9c84eae2 ok | 5d95f9ad differs @ tick 0 | 9c84eae2 unrecorded_target |
```

## The planted scenario

`planted_simd_vec3a` breaks the lockstep rules on purpose, so a run proves it can see a
divergence. It lives in the test code only, never in `src/`, and steps glam `Vec3A` bodies with
glam's SIMD operations. Its golden keeps only the x86-64 Linux hash, and a run must report it as
**differing, with a first diverging tick, on every aarch64 leg** and as equal on every x86-64 leg.
On an aarch64 leg that reports it equal, the run is not testing what it claims (an x86-64 binary,
or glam's scalar path). The test never blesses the plant on any target but the reference.

`Vec3A::dot` on its own would plant nothing: glam computes `(x*x' + y*y') + z*z'` on SSE2 and NEON
alike. Two operations do differ. `Vec3A::cross` is fused on NEON (one rounding). `Vec4::dot` sums
`(x+z)+(y+w)` on SSE2 and `(x+y)+(z+w)` on NEON. The probe test
(`vec3a_dot_has_the_scalar_order_on_every_target`) prints how many of 4096 seeded inputs leave the
scalar order on each target:

```text
regia-det-probe {"target":"aarch64-macos","glam_simd":true,"samples":4096,"vec3a_dot":0,"vec3a_cross":2460,"vec4_dot_xy_zw":0,"vec4_dot_xz_yw":1627}
```

## Problem codes

`verify` fails with one or more of these (`summary.json` → `problems[].code`):

| Code | Meaning |
|---|---|
| `missing_leg` | a `--require`d leg has no log |
| `no_aarch64_leg` / `no_x86_64_leg` | no leg of that architecture reported, so nothing crossed architectures |
| `wrong_arch` | a leg's reports name another architecture than the leg's |
| `no_reports` | a log has no `regia-det-golden` line (the binary did not run) |
| `leg_error` | the leg script printed `regia-det-leg-error <code>: ...`: `qemu_missing`, `adb_serial_missing`, `adb_device_missing`, `ssh_failed`, `rig_busy` |
| `tests_failed` | a test in the binary failed or the binary exited non-zero |
| `scenario_missing` | an engine scenario has no line on a leg |
| `scenario_failed` | a clean scenario's status is not `ok` or `unrecorded_target`; the detail names the first diverging tick |
| `clean_differs` | a clean scenario (an engine one, or a registered one wherever it is reported) hashes differently on two legs; the detail groups the legs by hash |
| `plant_not_caught` | an aarch64 leg reports the planted scenario as equal |
| `plant_moved_on_x86_64` | an x86-64 leg moved the planted scenario off the reference bits |
| `leg_owed` | not a failure: with `--qemu-when-present` and no `qemu-aarch64`, the qemu leg did not run; `summary.json` lists it under `owed` and `status` is `OWED` (ADR-8838) |

## Where it runs

- **Merge gate:** `cargo xtask det-golden gate` (host + qemu). It needs `qemu-aarch64` on the gate
  host; until it is installed the leg fails with `leg_error` / `qemu_missing`.
- **Nightly:** job `det-golden-nightly` in `docs/ci/nightly-jobs.toml` (host, qemu, the Android device).
- **Release candidates:** job `det-golden-rc`, a row of `docs/release/rc-checklist.md` (all six legs).

## Example

The runner's decision is a pure function of the logs, so an agent can check a folder of logs it
collected itself:

```text
$ cargo xtask det-golden verify target/det-golden/stage --require host,qemu
# Determinism goldens across targets: FAIL
...
- `missing_leg`: no log for the required `qemu` leg
- `no_aarch64_leg`: no aarch64 leg reported: cross-architecture identity is unproven
```

The standalone bundle preserves the crate feature declarations, inherited dependency
options and additive member features (ADR-8242). For example, `--features serde`
keeps the hover serialization derives enabled while serde remains a required
dependency of the integrated desync hash. Unknown dependencies still stop staging.

## Clean reference failures (ADR-8231)

The library now enforces reference agreement during ordinary `cargo test`, including when a
clean target's own blessed line matches. Its `disagrees_with_reference` and
`bless_refused_clean_differs` status codes pass through the runner's log parser into each report
and summary cell; `verify` maps both to `scenario_failed`. The former identifies a blessed clean
disagreement, and the latter refuses a differing clean per-target bless without writing the file.
Both include first-diverging-tick diagnostics when the fixture contains a reference trace.
The runner continues to require matching clean hashes across legs independently of that status.

The planted SIMD scenario explicitly returns true from `DetScenario::planted`. Its reference-only
fixture and the runner's aarch64 negative-control rule stay unchanged: never bless that committed
fixture's aarch64 line to satisfy a run. Temporary policy tests verify that explicitly planted
traces can record differing target lines while clean traces cannot. Standalone bundles include
the shared temporary-directory helper without adding any rendering dependencies.

## Match rules

`match_rules_ctf_koth_3600` is a fourth clean scenario (ADR-8503). `CLEAN_SCENARIOS` lists it after
`steer_slalom`, `slope_follow` and `disc_collide`, so every leg must report it and the hashes must
agree. The Mac and Windows bundles vendor `regia_rules` under `vendor/regia_rules` because the
scenario's test lives in `regia_detsim`'s `det_golden` binary. Host, qemu, Android and Deck run that
same binary from the workspace build.

## Every engine golden, and owed legs (ADR-8838)

`CLEAN_SCENARIOS` lists every committed golden in `crates/regia_detsim/goldens` except the plant:
the 13 registry scenarios (`steer_slalom` ... `ground_vehicle_mix`, `math_rng_fixed_vectors` among
them), `spatial_queries` and `match_rules_ctf_koth_3600`. The `det_golden` binary prints a line for
each, so every leg must report all 15 (`scenario_missing` otherwise) and their hashes must agree.
The xtask test `every_committed_engine_golden_is_a_clean_scenario` fails when a golden is committed
without joining the list.

`--qemu-when-present` (on `run` and `gate`) runs the qemu leg only where `qemu-aarch64` is on `PATH`
(or `$QEMU_AARCH64` names it). Without it, the qemu leg is not staged; `logs/qemu.log` holds one
`regia-det-leg-owed qemu_missing: ...` line, and the summary says `OWED` and lists the leg, plus
"no aarch64 leg reported" when qemu was the only aarch64 leg. The exit code is 0 if nothing else
failed. An owed leg never counts as a pass: a log with any report line is held to the normal rules
even if it also carries an owed line. Without the flag a missing `qemu-aarch64` still fails with
`leg_error` / `qemu_missing`. The nightly job `det-golden-gate-qemu` runs
`cargo xtask det-golden gate --qemu-when-present` on the gate host, so the qemu leg starts the day
`qemu-user` is installed there.

```text
$ cargo xtask det-golden gate --qemu-when-present
# Determinism goldens across targets: OWED
...
Owed (not run; not a pass):

- `leg_owed`: qemu: qemu_missing: qemu-aarch64 is not on PATH (install qemu-user); the qemu leg is owed and runs when it is there
- `leg_owed`: no aarch64 leg reported: cross-architecture identity stays owed until the owed aarch64 leg runs
```
