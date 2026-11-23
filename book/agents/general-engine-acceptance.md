# General-engine workflow acceptance

The `nav_world_lab` sample has a small CPU scenario that combines an obstacle-detouring
navigation route, an atomic file save, restored navigation state, and an on-disk demo.
Run it with:

```sh
cargo test -p nav_world_lab --test general_workflow
```

The first case saves a moving agent after 30 fixed steps, records its next 360 steps,
loads the save, and compares the continuation at every step. It then reads the demo
file and seeks forward and backward across keyframe boundaries. Hashes include the
agent's transform, velocity, target, waypoints, and next-waypoint index. Loading changes
its entity id; the demo's name-based identity must still produce the same state.
The unchanged mesh and obstacles are level content, outside the snapshot. This check
does not promise that a changing tile cache or an in-flight crowd link is serialized.

The second case drives the same scene through `regia_agent`:

1. Start paused and call `save.save {"slot":"Route"}`.
2. `play.step {"frames":60}`, then use `play.expect` and `play.wait_until` to confirm
   that `Walker` advanced.
3. `save.load {"slot":"Route"}`, then use a name lookup in `play.expect` to confirm
   its saved position. Names survive the save system's entity replacement.
4. `spectator.set` and `spectator.step` move the camera while gameplay stays paused.
   The tracked simulation hash must remain unchanged.
5. `save.delete {"slot":"Route"}` removes the test slot.

The server publishes its endpoint before the client connects. There are no endpoint
poll sleeps or elapsed-time assertions. Only exact `play.step` requests advance the
paused simulation. The temporary project and its files are removed after each case.
These are CPU checks; they do not capture or approve a rendered look.

Complementary existing checks:

```sh
cargo test -p regia_framework --test character_polish --test ragdoll_active --test replay_determinism
cargo test -p regia_nav --test links --test region_rebuild --test link_stream
cargo test -p regia_animation --test retarget_real_content --test crowd
cargo test -p nav_world_lab --test acceptance
cargo test -p crowd_lab --test acceptance
cargo test -p regia_script_lua --test saves --test reload --test sandbox --test ecs
```

`retarget_real_content` includes the checked-in CesiumMan hierarchy and exporter-style
rig cases. Its hierarchy and CPU motion assertions are separate from artist approval
of imported animation. No downloaded content pack is required.

Measure crowd cost separately:

```sh
cargo test --release -p regia_animation --test crowd crowd_step_timing -- --ignored --nocapture --test-threads=1
```

The ignored measurement retains the 20- and 27-joint cases and adds a 60-joint rig
(27 animated/helper joints plus 33 accessory joints parented to the spine). It prints
500-agent step cost with sharing and without sharing. Record the CPU, compiler,
profile, thread affinity and load; compare the serial 60-joint rows with spec 85's
2 ms shared / 6 ms exact budgets. No timing is asserted in the ordinary test suite.

Linux execution, native Windows execution, Android compilation, and Android device
execution are distinct evidence. Optional Lua needs a separate `regia/lua` build;
a default facade build does not prove it. Keep unavailable hardware and artist
review marked pending rather than treating CPU or cross-compile results as substitutes.

## Cross-host digests

Both labs can write a per-step digest trace, one `section step digest` line per fixed step:

```sh
cargo run --release -p nav_world_lab -- --trace nav.trace
cargo run --release -p nav_world_lab --features cross-platform-determinism -- --trace nav-xp.trace
cargo run --release -p crowd_lab -- --trace crowd.trace          # also --in-place, --travelling-hips
```

Run the same commands on each host and `diff` the files: the first differing line names
the section (`cook`, `crowd`, `lanes`, `ragdoll`, `traffic`, `traffic_physics`; `nav`,
`crowd` and `crowd_motion` for `crowd_lab`) and the fixed step that diverged first. The navigation, crowd
and lane sections are compared on the default build. The physics sections (`ragdoll`,
`traffic*`) run rapier, which uses the platform math library unless the
`cross-platform-determinism` feature turns on `regia_physics`'s
`physics-cross-platform-determinism` (spec 15 § 5, ADR-5128); compare them with that
feature. A host-local check that a section does not read the platform math library at
all: run the binary on Linux with an `LD_PRELOAD` library that moves each libm result
by one ulp and compare its trace with an unshimmed run.

Measured on Linux x86_64, Windows, Android aarch64 and macOS aarch64, the default build's
`cook`, `crowd` and `lanes` sections agree on all four hosts and the physics sections differ
on every pair, for two separate reasons. Between hosts of one CPU architecture it is the
platform math library: Linux and Windows, and macOS and Android, give byte-identical traces
once glam, simba and num-traits use the pure-Rust `libm` while glam keeps its SIMD path. Between x86_64 and
aarch64, glam's SIMD path (SSE2 or NEON, used by rapier and parry through glamx) also rounds
differently, from traffic step 2. `cross-platform-determinism` removes both, and
its trace is byte-identical on all four hosts.

`crowd_lab` also compares across CPU architectures (x86_64 against aarch64: Android, macOS)
only with its own `cross-platform-determinism` feature (ADR-5176), which switches glam to its
scalar path and its transcendentals to the pure-Rust `libm` through `regia_core`. The default SIMD builds group a 4-lane dot product
differently (SSE2 `(x + z) + (y + w)`, NEON `(x + y) + (z + w)`) and NEON's quaternion product
fuses multiply-adds, so on the default build `crowd_lab`'s `crowd` section (the skinning
palette) differs between architectures from step 0, and for clips without a root-motion track
(`--in-place`, `--travelling-hips`) its `crowd_motion` section (agent state) does too, because
the gaits are measured from the clips' poses. Its `nav` section agrees on every host either way.

```sh
cargo run --release -p crowd_lab --features cross-platform-determinism -- --trace crowd-xp.trace
cargo test --release -p crowd_lab --features cross-platform-determinism --test cross_arch
```

The `cross_arch` test pins the three shapes' digests measured on x86_64; the same test
must pass unchanged on an aarch64 host.
