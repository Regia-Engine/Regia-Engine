# Locomotion cost evidence (ADR-8728)

Evidence only: no test asserts these numbers. Command, on each device (release build):

```text
cargo run --release -p regia_animation --example locomotion_cost             # 64 bipeds + 16 hexapods
cargo run --release -p regia_animation --example locomotion_cost -- --scaling  # 16 / 64 / 256 characters
```

The scene: bipeds (the demo humanoid walking in place on circles, `FootPlacement` on both legs, pose
output) and hexapods (two-bone legs, `GaitRig` driven by the transform) on a rolling analytic ground
(`0.12 sin(0.31 x) cos(0.23 z)`), 120 warm-up frames, then 2000 frames at a fixed 1/60 s step.
`locomotion_ms` is the time inside the `foot_placement`, `gait_rig_step` and `gait_rig_pose` spans,
summed over every rig in the frame (a minimal span timer enables only those three spans, so the rest of
the frame runs without tracing). `frame_ms` is the whole `App::update`, with and without the locomotion
components, for context. All rigs at full IK detail (no `AnimLodCamera`) unless the row says `camera`.

Targets from the brief (evidence, not asserts): locomotion p99 at most 0.8 ms per frame on the N150 and
the Deck, at most 1.0 ms on the TEST Pixel 9 Pro, 64 bipeds + 16 hexapods, full detail.

## Results

Locomotion time per frame (ms, the three spans summed over every rig). `bipeds+hexapods`.

| Device | Build | Characters | locomotion p50 | locomotion p99 | max | frame p50 / p99 with | frame p50 / p99 without |
|---|---|---|---|---|---|---|---|
| N150 laptop (Windows 11, MSVC, 4 threads) | release, `fd0672b10` | 12+4 | 0.089 | 0.102 | 0.138 | 0.202 / 0.333 | 0.104 / 0.188 |
| N150 laptop | release, `fd0672b10` | 48+16 | 0.363 | 0.398 | 0.536 | 0.617 / 0.836 | 0.195 / 0.290 |
| N150 laptop | release, `fd0672b10` | **64+16** | 0.446 | **0.478** | 0.525 | 0.752 / 0.863 | 0.249 / 0.330 |
| N150 laptop | release, `fd0672b10` | 192+64 | 3.074 | 3.231 | 4.242 | 4.898 / 5.412 | 1.656 / 1.895 |
| N150 laptop | release, `fd0672b10`, `--camera` | 64+16 | 0.236 | 0.263 | 5.404 | 0.518 / 0.653 | 0.248 / 0.455 |
| N150 laptop | release, before the `prepare_foot_placement` query-state fix | 64+16 | 0.449 | 0.520 | 1.742 | 0.758 / 1.080 | 0.249 / 0.356 |
| MacBook Air M5 (macOS, 10 threads; testing only) | release, `fd0672b10` | 12+4 | 0.021 | 0.040 | 0.047 | 0.102 / 0.196 | 0.083 / 0.112 |
| MacBook Air M5 | release, `fd0672b10` | 48+16 | 0.085 | 0.094 | 0.099 | 0.198 / 0.231 | 0.109 / 0.140 |
| MacBook Air M5 | release, `fd0672b10` | **64+16** | 0.102 | **0.126** | 0.143 | 0.223 / 0.272 | 0.120 / 0.151 |
| MacBook Air M5 | release, `fd0672b10` | 192+64 | 0.322 | 0.342 | 0.886 | 0.549 / 0.588 | 0.214 / 0.242 |
| MacBook Air M5 | release, `fd0672b10`, `--camera` (LOD by distance, rigs out to 100 m) | 64+16 | 0.053 | 0.081 | 0.092 | 0.177 / 0.246 | 0.122 / 0.158 |

| TEST Pixel 9 Pro (Android 17, Tensor G4), big cores only (`taskset f0`: three 2.6 GHz + one 3.1 GHz) | release (thin LTO), `fd0672b10` sources through a measurement crate (see below) | 12+4 | 0.049 | 0.143 | 0.218 | 0.079 / 0.265 | 0.024 / 0.031 |
| TEST Pixel 9 Pro, big cores | same | 48+16 | 0.169 | 0.179 | 0.188 | 0.246 / 0.262 | 0.068 / 0.077 |
| TEST Pixel 9 Pro, big cores | same | **64+16** | 0.203 | **0.280** | 1.388 | 0.299 / 0.421 | 0.084 / 0.094 |
| TEST Pixel 9 Pro, big cores | same | 192+64 | 0.690 | 0.710 | 0.766 | 0.993 / 1.020 | 0.262 / 0.277 |
| TEST Pixel 9 Pro, big cores | same, `--camera` | 64+16 | 0.111 | 0.203 | 1.284 | 0.205 / 0.417 | 0.084 / 0.095 |
| the build server (Threadripper 3990X, 128 threads), one CPU leg, **host load 135-155** (shared; noisy) | release, `fd0672b10` | 12+4 | 0.120 | 0.276 | 0.633 | 0.626 / 4.904 | 0.427 / 5.106 |
| the build server, same load | release | 48+16 | 0.604 | 0.903 | 3.275 | 1.880 / 9.487 | 0.829 / 7.618 |
| the build server, same load | release | 64+16 | 0.708 | 1.216 | 2.805 | 2.252 / 16.060 | 1.019 / 7.667 |
| the build server, same load | release | 192+64 | 2.682 | 3.485 | 4.764 | 7.538 / 18.392 | 3.407 / 21.169 |
| Steam Deck | — | — | not measured | not measured | | | |

Steam Deck: not measured. `<host>` answered "No route to host" at 04:5x, 05:26 and 06:2x on 2026-11-19.
Command for the lead (on the Deck, `flock /tmp/regia-deck.lock`, a release build of this branch or a binary
built on a host with glibc at most the Deck's):
`cargo run --release -p regia_animation --example locomotion_cost -- --scaling && cargo run --release -p regia_animation --example locomotion_cost`.

The Pixel binary is not the example itself: `cargo ndk ... --example locomotion_cost` builds every
`regia_animation` dev-dependency and failed linking the `regia_web` cdylib for Android. The same example
source (and the digest scenario) was built as a stand-alone crate outside the workspace that depends on
`regia_animation` by path, with the workspace's release profile (thin LTO, 16 codegen units). The last N150
row is the first run, without the `prepare_foot_placement` query-state fix (its span is inside `foot_placement`).

Reading: the N150 and the Pixel meet the brief's targets at 64+16 with margin (0.48 ms and 0.28 ms p99).
Scaling is about linear from 16 to 64 characters everywhere; from 64 to 256 it stays linear on the M5 and the
Pixel and is super-linear on the N150 (0.40 to 3.23 ms p99), likely its small cache; with `AnimLodCamera` and
the default LOD bands the same 64+16 crowd spread to 100 m costs about half.

## Cross-architecture bits (F7)

`cargo test -p regia_animation --test locomotion_cost plant_scenario_digest -- --nocapture` (the planter's
`two_runs_agree_bit_for_bit` scenario, 200 frames, inputs built without platform trigonometry):

| Host | Arch | default build | `--features cross-platform-determinism` |
|---|---|---|---|
| the build server (Linux, glibc) | x86_64 | `3b963adce60ed704` | `57beb214bc0d061f` |
| N150 laptop (Windows, MSVC) | x86_64 | `3b963adce60ed704` | `57beb214bc0d061f` |
| MacBook Air M5 (macOS) | aarch64 | `c4c0b5d21f4fcb75` | `57beb214bc0d061f` |
| TEST Pixel 9 Pro (Android) | aarch64 | `c4c0b5d21f4fcb75` | `57beb214bc0d061f` |

The default build differs between x86_64 and aarch64 (glam's SIMD paths, ADR-5176); with glam's scalar path
all four agree, pinned by `plant_scenario_digest_is_pinned_across_architectures` (compiled only with the
feature). A first run that built the inputs with `f32::sin`/`cos` and `Quat::from_rotation_y` differed between
Linux and Windows on x86_64: the platform math libraries moved the inputs, not the planter.
