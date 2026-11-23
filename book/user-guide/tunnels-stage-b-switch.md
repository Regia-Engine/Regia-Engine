# Tunnels Stage B: the switch note for DO27

This page is for Direct Order 27 (DO27), the game that matches Regia Engine's tunnel query bit for bit today. It
says what changes when DO27 moves from **Stage A** (the compat path, `regia_world_geom::compat`, DO27's own T3
arithmetic copied unchanged) to **Stage B** (the native world-geometry query and mesh layers behind the `stage_b`
feature), what DO27 re-cooks, which pins it re-records, and what holds if a bar is still missed. The general
adoption rules are in [Adopting engine features](adopting-features.md); the field-by-field host changes are in the
[DO27 host checklist](do27-host-checklist.md).

Written on the v0.7 line (lane v07-tunnels-repin-switch-note, ADR-10082) after the four W-11 speed lanes, the W-29
acceptance suite, Stage B correctness (ADR-8898) and the cooked-format step (ADR-8816) merged.

## The statement (ADR-8538)

W-29's native acceptance suite reruns W-05's Stage A proofs against the native query on cooked fixtures, plus the
bars that decide DO27's Stage B bump (spec 121 § 13.2, § 13.3, § 14 rule 3). DO27 can run that one suite to decide
the bump. The tunnel fight, which needs DO27's whole simulation, is rebuilt as a Regia two-peer drive
(`two_peer_tunnel_drive_checksums_equal_x86_aarch64`); DO27 keeps its own pin and re-records it in the bump.

**DO27's Stage B switch is one sim-version bump, outside an alpha week** (spec 121 § 14 rule 3; owner,
2026-11-16). DO27 replaces `ir_content::surface::column` and `TunnelsState::{support, place_floor, confine,
underground}` with thin calls on `WorldGeom::{floors_at, support, place_floor, push_disc, covered_at}`. The waist
rule, the hover rules and the on-foot grid stay DO27's. It adds `QUERY_BITS_VERSION` and `MATH_BITS_VERSION` to its
handshake beside `content_hash`, folds `GeomState::state_hash` into its checksum (one more little-endian `u64` term of
its FNV-1a-64 fold), and re-records in the same bump: the tunnel-fight pin
(`switchbacks_tunnel_fight_lockstep_checksum_is_pinned`), the predicted-hull drive
(`a_net_client_predicting_its_hull_through_the_tunnels_never_shows_a_vertical_jump`),
`determinism_fuzz_short_fixtures_agree_every_tick`, the 3600-tick replay hash and the lockstep checkpoints. Old
replays play on old builds.

**When.** The bump is v0.7.x work, not part of the v0.7.0 tag (owner, 2026-11-19): v0.7.0's bar is "ready for DO27
to switch". DO27 is paused until the tag; its own pin runs and the sim-version bump follow on v0.7.x with a 48 h fix
turnaround from Regia.

## What stays the same

- **Stage A does not move.** `stage_a_bits` (with and without `stage_b`) is green on this line with the same table;
  `compat` never reads `cfg(feature = "stage_b")` and `det-scan` refuses one there. Until DO27 makes the bump it keeps
  matching Stage A bit for bit, with no checksum change.
- **`QUERY_BITS_VERSION` is 8** on this line. None of the W-11 speed work moved a query answer (each lane pins its
  fast path equal to the per-call form). `MATH_BITS_VERSION` is 1: the `atan2` canonicalization (plan section B)
  already moved it — DO27 sends 1 today and re-records the pins that call `math::atan2` once; if it moves again
  before DO27's bump, DO27 sends the new value. It does not change the tunnel query, which reads no trigonometry.

## What DO27 re-cooks

| Data | Why | How |
|---|---|---|
| Every `<map>.meshlayers` (cooked, `terrain-mesh-layers-cooked`) | Version 2 (ADR-8816): fixed-width ground counts behind a hashed head; `content_hash` is now the BLAKE3 of that head, so **every cooked map's `content_hash` changed** even where the cooked value did not | `regia upgrade-project --write` migrates a v1 file to the same value with its v2 hash (`cooked::migrate_file`); a fresh `regia cook` writes v2. Anything DO27 stored with a hash (handshake fixtures, a saved `GeomState`) needs the new one |
| Maps with a filled skylight mouth (Switchbacks `west`, `mid_east`) | ADR-8898: the cook cuts every filled mouth from the layout as meshed, so the whole skylight climb K8 opens is cut from the ground. The cooked value moves (`lattice_cook_hashes_are_pinned` re-pinned) | Run `regia cook` after `regia upgrade-project --write`. ADR-10346 invalidates a migrated package's input hash and revisions the cook key, so ordinary cooking rebuilds the cuts, including in already-upgraded legacy caches. The migrator preserves geometry; the source cook applies the new cuts. No `--clean` is required |
| Nav cells (`nav-cell`) | Version 3 (ADR-8899, lane w11-fmt-nav-cell-v3): conservative rasterization. A v1 or v2 cell still loads, with a warning that names the re-cook | Re-cook. **Not on this line yet**: the lane reported PARTIAL and lands through the format batch 2 integration |
| The terrain index section | Terrain index v3 (unequal spacing and masked rasters, plan section F) | **Not built on this line**: scheduled for format batch 2 (freeze Sat 11-28). Until it lands, DO27's current terrain index stays valid |

## The bars, measured per device

The bars are spec 121 § 11 (absorb plan § 4.7). Each W-11 lane measured its own calls on its own branch; the combined
line is measured below where a device was reachable. "Build host" is an idle core of the build host (the build server; the
the test machine was used where no idle the build server core existed, which is not a clean bar measurement).

Combined line = this page's line (all four W-11 lanes merged), measured 2026-11-20 with release benches built on
the build server: the Deck on core 3 (`taskset -c 3`, idle), the test machine on its least busy core pair (a shared host
at load 17-20). Lane = the W-11 lane's own branch, before the merge (ADR-8815 to ADR-8818). Raw output:
`docs/evidence/v07-tunnels-repin-switch-note/` and each lane's `docs/evidence/w11-*` folder.

| Bar (spec 121 § 11) | Steam Deck | Android test device (Pixel 9 Pro) | Build host |
|---|---|---|---|
| 500 mixed queries ≤ 0.5 ms Deck, ≤ 0.8 ms Android, ≤ 0.25 ms build host (p50; 0 allocations) | **0.269-0.271 ms, met** (combined, 3 runs; 1.64 ms before W-11) | **met by each lane alone**: 0.239 ms (lane 001, one run), 0.717 ms (lane 003); combined line not measured (the device was off adb) | combined on the test machine: **0.110 ms** (2 runs; 0.183 ms in a run with a busy sibling thread), under 0.25 ms but on a shared host. Idle the build server core: 0.363-0.398 ms with lane 001 alone (missed); the combined line is not measured there (the build server at load 420) |
| The same tick after one 6 m crater (no bar) | 0.289-0.294 ms (5.90 ms before W-11) | 0.270 ms (lane 001) | the test machine 0.118 ms |
| The same tick after 30 craters (no bar) | 0.315-0.318 ms (6.54 ms before lane 004) | 0.99-1.02 ms (lane 004, before lane 001's descent merged) | the test machine 0.131 ms |
| Index build ≤ 50 ms (load of the cooked Switchbacks) | **28.6 ms, met** (combined; strict load 19.5-19.7 ms; 79 ms before W-11) | **49.3-49.6 ms, met** by under 1 ms (lane 002; p99 50.8-52.2 ms) | the test machine 14.1-16.3 ms (combined) |
| D1 crater, 6 m radius ≤ 0.2 ms | **0.086-0.092 ms, met** (combined; 3 m radius 0.023-0.028 ms) | **0.060-0.065 ms, met** (lane 004) | the test machine 0.045-0.067 ms (combined) |
| D4 column (`support`) ≤ 150 ns and ≤ 0.25 of compat | **missed**: 500-1430 ns, ratio 1.07-2.04 (combined) | **missed**: 325-814 ns, ratio 1.13-2.22 (lane 003) | **missed**: the test machine 370-1012 ns, ratio 1.28-2.46 (lane 003, loaded) |
| Sim geometry RAM ≤ 35 MB | 34,368,306 B on Switchbacks (combined; counter, every host) | same | same |

D4 is at parity with the compat kernel, not at a quarter of it: `support` is 1.5-3.1 times faster than before W-11
and bit-identical. Inside the mixed tick it costs about 0.49 µs a call on the Deck, and the tick meets its bar with it.

## If a bar is missed: the fallback

The plan's fallback (plan-v07-to-v10 § 5) holds: if the W-11 bars are still over 1.5x at the 11-30 checkpoint,
**v0.7 ships Stage A as the default** and Stage B behind its feature with the measured numbers as a known issue. DO27
then switches on Stage A with no checksum change, and Stage B with its one sim-version bump moves to v0.8. A bar is
re-set only by a written owner ruling with the measured number; this page never re-sets one.

**Waiting for an owner ruling (recorded here, not re-set):**

- **D4, the column bar** (150 ns, 0.25 of compat; ADR-8817). Measured above on every device; the lane recommends
  option C, accept the measured numbers for v0.7. Until the ruling, D4 is a missed bar.
- **The build-host bar** (0.25 ms on an idle the build server core). The combined line reads 0.110 ms on the test machine, a shared
  host; the idle-the build server number is owed (the build server had no idle core during this lane).
- **The Pixel's combined numbers** are owed: each lane met its bar alone; the merged line was not run on the device.
- **D6, crater radius or diameter**: both readings meet 0.2 ms on every device measured, so the ruling no longer
  decides pass or fail.
- `native_resident_geometry_fits_the_sim_budget` is red on this line: the 35 MB bar is met (34,368,306 B), but the
  cook's estimate leaves out the run-time query tables and reads 1.35 % low against the test's 1 % rule (ADR-10082 lists
  the options). It is a consistency check of the cook's figure, not a memory bar.

D4 is the one bar over the 11-30 checkpoint's 1.5x trigger (500 ns and up against 150 ns on the Deck). Every other
bar measured on this line is met, including the mixed tick that `support` sits inside. So the D4 ruling decides
whether the fallback applies: if the owner accepts the measured numbers (option C), no measured bar triggers it.

## Running the checks

```text
cargo test -p regia_world_geom --features stage_b,fixtures --test native_acceptance --test host_step_echo -- --nocapture
cargo test -p regia_world_geom --test stage_a_bits                      # Stage A bits without Stage B
cargo test -p regia_world_geom --features stage_b --test stage_a_bits   # ... and with it: the same table
cargo test -p regia_world_geom --features stage_b,stats,fixtures --test lattice_ground --test native_table
cargo bench -p regia_world_geom --features stage_b --bench mixed_calls   # the mixed tick, the crater, the index build
```

The two-peer golden's aarch64 leg is the `native_acceptance` run on an aarch64 host (the Mac, or the test binary on
the Android device with `REGIA_DET_GOLDEN_DIR` pointing at a copy of `crates/regia_world_geom/goldens`); a passing
run with `REGIA_BLESS=1` records that target's line. `cargo xtask det-golden` covers `regia_detsim`'s goldens, not
this one.

The acceptance bars and what each test proves are in [Terrain mesh layers](../agents/terrain-mesh-layers.md),
"Acceptance".
