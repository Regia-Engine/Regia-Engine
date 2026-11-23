# 06 — Testing, performance budgets and CI

## 1. Test layers

| Layer | Tooling | Where |
|---|---|---|
| Unit | `cargo test` | `#[cfg(test)]` modules in each crate |
| Property | hand-rolled deterministic generators + `regia_test_utils::fuzz` / `regia_core::fuzz` mutators | serialization round-trips, undo/redo sequences, pak/TOC parsing, math |
| Integration | `cargo test` in `crates/*/tests/` | headless `App` with selected plugins |
| Golden image | `regia_test_utils::golden` | renderer, UI text, post-processing |
| Fuzz | `cargo xtask fuzz` over `cargo fuzz` (nightly, separate CI job, non-blocking; ADR-2723) | every target of the fuzz registry, including the pak reader (`pak.read`), cooked headers (`cooked.mesh`, `cooked.clip`), RON scene loader (`scene`) and save loader (`save.payload`); committed findings replay on stable in `regia_agent/tests/fuzz_findings.rs` |
| Sample smoke | scripted input playback (`InputRecording` asset) | each sample, each platform |
| Soak | sample runs 30 min with random bot input | nightly on desktop; manual on Android before milestone sign-off |
| Benchmarks | `criterion` (CPU) + the whole-frame benches of § 9 | tracked over time, gating only on reference hardware |

## 2. Golden images
- Test scene described in code or `.level.ron`; rendered headless at fixed resolution (e.g. 512×512) with
  deterministic settings (fixed time, fixed exposure, TAA off unless under test, no jitter).
- Compared with stored PNG in `crates/regia_test_utils/goldens/<test>.png` using SSIM (`image-compare`)
  with per-test threshold (default ≥ 0.98) **and** max per-pixel delta for small regions where precision
  matters (e.g. text).
- On failure: write `<stem>_actual.png` and `<stem>_diff.png` to `target/golden-failures/` (CI uploads as artifact).
- Bless: `REGIA_BLESS=1 cargo test -p <crate> golden_` — only with a commit message explaining the
  intentional change.
- CI renders with Mesa lavapipe (Linux, pinned container image) and WARP (Windows); goldens are blessed on
  lavapipe. (When the macOS port happens, its golden tests start as non-blocking — Metal output differs
  slightly.)
- Real GPUs (a desktop card running the full workspace, e.g. the RTX 5080 tier box) compare against the same
  lavapipe-blessed goldens — no per-vendor golden sets (ADR-025) — with one documented exception: scene classes
  whose pixels are dominated by vendor rasterization or texture-filtering variance compare with a looser SSIM
  threshold, selected by `regia_test_utils::golden::ssim_threshold_for(name, hardware_gpu)`:
  `alpha_modes` ≥ 0.95 (alpha-test edge coverage; RTX 5080 measured 0.9675) and `ibl_spheres` ≥ 0.95
  (hardware filtering of the CPU-built prefiltered mip chain; measured 0.9764). Software rasterizers
  (lavapipe, WARP — `wgpu::DeviceType::Cpu`) keep the 0.98 default for **every** scene; the lavapipe threshold
  is never lowered. On a hardware GPU, goldens compared through `compare_rgba8_to_png_on` (material, Sponza, fox, lens) use only the 0.75 sanity floor `HARDWARE_SANITY_SSIM` (`min` of the test's threshold); `alpha_modes` and `ibl_spheres` call `compare_rgba8_to_png` directly and keep 0.95. Adding a scene class to the table requires a measured vendor delta recorded here.
- Other lavapipe builds (the build machines gate runs LLVM 19; goldens are blessed on LLVM 23) compare against the
  same PNGs. SSIM goldens are unchanged. Pixel-exact goldens go through
  `regia_test_utils::golden::check_pixel_golden`: the reference LLVM, WARP and any unlisted build use the test's own
  per-channel bound with no outliers. A build in `LAVAPIPE_ICD_TOLERANCES` keeps that bound and allows a few edge
  pixels beyond it, in ppm of the frame. LLVM 19 is allowed 30 ppm, measured 2026-10-07: `terrain_fog_on_noon`
  2 px of 360 000 (max 20), `snow_setpiece` 7 px of 518 400 (max 8), `mars_setpiece` 4 px (one at 50). Adding a
  build requires its measured delta recorded here (ADR "farm merge gate"). The decal set piece no longer
  compares pixels (ADR "decal set piece locks the decal contribution"); its numbers above are history.
- The build machines gate's real-GPU pass is **non-golden**: it sets `REGIA_SKIP_GOLDENS=1`, so `compare_rgba8_to_png` and
  `check_pixel_golden` skip the lavapipe compare on that hardware run with a note. Guards and every other
  assertion still run, and the pass must be green apart from its documented quarantine list.
- **Ray queries on lavapipe.** Lavapipe (Mesa 24.1 and later) has `VK_KHR_ray_query`, and wgpu exposes it as
  `EXPERIMENTAL_RAY_QUERY`, so the hardware ray-tracing path is tested on every lavapipe run, not only on a card. The
  ray-query tests (`hwrt_frame`, `hwrt_shadow`, `hwrt_shared_casters`, `hwrt_terrain_casters`) decide at run time through
  `regia_test_utils::hwrt::ray_query_device()`: it reads the returned device's own features (never the process-wide
  `last_device_report`, which a parallel sibling's device creation can overwrite mid-read), so they run when the
  adapter has the feature and print `skipped: no ray queries on <adapter>` otherwise. `REGIA_REQUIRE_RAY_QUERIES=1`
  (the `REGIA_REQUIRE_ASSETS` precedent) turns the skip into a failure; the lavapipe gate sets it so a regressed
  host cannot pass without ray-query coverage. `hwrt_ray_query_spirv` validates both ray-query shaders through the
  shader gate's own `validate_ray_query_spirv_module` (the ADR-026 scan exempts only the `ray_query` builtin's own laid-out object) plus a `RayQueryKHR`/`SPV_KHR_ray_query` scan.
  Commands: `book/agents/hwrt.md`, "Testing ray queries without a card".
- **One GPU test at a time per binary on real cards.** A test binary whose tests each open their own instance and
  device on `cargo test`'s parallel threads can livelock the NVIDIA driver (a driver thread spins, the test threads wait
  forever; the GPU pass reports `HANG`). New GPU tests get their device from
  `regia_test_utils::serial_gpu::serial_device()` (the process's shared device plus the binary's GPU turn; no turn on a
  CPU adapter such as lavapipe, ADR-7483) or
  `RenderDevice::shared_headless()` when the tests touch no per-device state (ADR-2113).

## 3. Reference hardware (record actual models in progress.md at M1)

| Class | Example | Used for |
|---|---|---|
| Desktop reference | 8-core CPU (Ryzen 7 5700X class), RTX 3060 / RX 6600 class GPU, 16 GB | desktop budgets |
| Desktop low | 4-core laptop with integrated GPU (Iris Xe / Radeon 680M class) | Desktop Low profile ≥ 60 fps at 1080p×0.75 |
| Mac (deferred, ADR-009) | Apple M1 (8 GB) | macOS budgets ≈ desktop reference at High — applies once the port happens |
| Android reference | Snapdragon 865/888-class or Dimensity 1200-class (Adreno 650 / Mali-G77), 6–8 GB | Mobile High |
| Android low | Snapdragon 680 / Helio G85 class (Adreno 610 / Mali-G52) | Mobile Low ≥ 30 fps |

DO27's gates use its own hardware tiers instead (`specs/25` § 6: floor RTX 3060 Ti / RX 6700 XT, mid
RTX 4070 / RX 7800 XT, showcase, Steam Deck). Budgets in `specs/26` quoted "at floor tier" refer to those.

## 4. Budgets (Tier 1)

| Metric | Desktop ref | Android ref |
|---|---|---|
| `samples/third_person` frame time (p99) | ≤ 6.9 ms at 1080p High | ≤ 16.6 ms at Mobile High, frame work: per frame the larger of CPU and GPU frame time (ADR-4947) |
| Presented frame interval (p99), vsync-paced | n/a | ≤ 1.05 × the panel's refresh interval at Mobile High, at the refresh rate the run measured (ADR-4947) |
| CPU main thread (sim + extract) for sample | ≤ 3 ms | ≤ 8 ms |
| Cold start to first interactive frame (shipping) | ≤ 3 s | ≤ 5 s |
| Level load (sample level) | ≤ 1.5 s | ≤ 4 s |
| Memory (sample) | ≤ 1.5 GB RSS | ≤ 700 MB PSS |
| Hitch: no frame > 50 ms after first 10 s of play | yes | yes (PSO warm-up must cover sample) |
| Editor idle CPU | < 5 % one core | n/a |
| Hot reload (incremental, small change) | ≤ 10 s end-to-end | n/a |

Budgets live in `budgets/*.ron` as data so tooling can report pass/fail: `default.ron` for the soak,
`android-reference.ron` and `android-low.ron` for `cargo xtask android-pass`, which ends in one verdict per
row (`pass`, `fail`, `not_measured`; `cargo xtask android-budgets --run <dir>` repeats it on a finished run,
ADR-1942). On a phone the frame time row is the frame's work, not its presented interval: a vsync-paced
interval on a 60 Hz panel is 16.67 ms whatever the frame costs, so the presented interval has its own row
(`presented_p99`, ADR-4947). Until `samples/third_person` exists (M20), the desktop column is measured on `samples/sim_bridge`
(and on the DO27 benchmark replay from M17); the Android column becomes gating in M20
(`05-milestones.md`), by `android-pass --enforce-budgets`.

## 5. CI matrix (GitHub Actions or equivalent; runners per § 8 — Windows jobs run in the owner's Windows VM)

| Job | Runner | Steps | Blocking |
|---|---|---|---|
| lint | ubuntu | fmt check, clippy -D warnings (all targets), `cargo deny check`, layer check | yes |
| test-linux | ubuntu (container with pinned Mesa lavapipe) | test workspace incl. goldens | yes |
| test-windows | self-hosted runner in the owner's Windows VM | test workspace incl. goldens (WARP) | yes |
| windows-cross | ubuntu (cargo-xwin, clang-cl) | `cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings`: Windows compile and lint breaks without the VM; allowed to fail until seen green once (ADR-295) | no (for now) |
| android-build | ubuntu + NDK | `cargo ndk -t arm64-v8a -t x86_64 build -p <sample android crates>` | yes |
| android-smoke | ubuntu + emulator (x86_64, API 35, swiftshader); `ci.yml` job `android-smoke` | `cargo xtask package-smoke --skip-linux --android --android-emulator --require-kvm`: sim_bridge APK and AAB for the manifest ABIs (arm64-v8a and x86_64), sizes recorded (shipping APK under 150 MB), then an x86_64 emulator boots headless, installs the dev APK, plays the scripted recording for 60 s and fails on a crash or an ERROR logcat line (ADR-3968). This job pins `system-images;android-35;google_apis;x86_64` | from M16 yes |
| package | ubuntu (xvfb, lavapipe, zig 0.14.1); `ci.yml` job `package` | `cargo xtask package-smoke`: dev and shipping Linux tar of `samples/sim_bridge` and of a `regia new` `third_person` project, each launched for 60 s under xvfb with scripted `.frec` playback, zero `ERROR` records in the JSON log and a clean `summary.json` (ADR-3968). Windows zip smoke is `cargo xtask package-smoke --windows` on the owner's Windows a test machine | yes |
| templates | ubuntu (xvfb, lavapipe) | `cargo xtask templates-check`: `regia new` × templates → build → the game runs 5 frames (ADR-2046) | from M11 yes |
| licensee-walkthrough | the build server (xvfb, lavapipe), per tag, manual | `xvfb-run -a cargo xtask walkthrough`: the book's getting-started commands (`sh,walkthrough=<stage>` fences) as a stranger: engine archive → install → verify → doctor → every template new, smoke, cook, package, run the package elsewhere, upgrade-project no-op → Android check; `target/walkthrough/report.json`. A rehearsal, not the outside-tester proof (ADR-2954) | no (not in preflight) |
| docs | ubuntu; `ci.yml` job `docs` | `cargo xtask book-check`, then `mdbook build book` (mdbook installed in the job). `cargo doc -D warnings` is a step of the `linux` job (`RUSTDOCFLAGS`) | yes |
| fuzz | ubuntu nightly | 10 min per target. Fuzz targets exist: the deterministic registry of ADR-1917 (`regia_agent::fuzz_targets`, spec 97: every frozen format in `docs/formats/format-hashes.ron`, every `regia_net` message kind, the agent request text and parameter parsing; extended by the Lua host and the editor importers through `FuzzTargetExtensions`) runs in every test pass as `crates/regia_agent/tests/fuzz_targets.rs`, and the agent methods `fuzz.targets` / `fuzz.run` expose it in dev builds (`book/agents/fuzzing.md`); the shared mutators are `regia_test_utils::fuzz` and `regia_core::fuzz`. The one `cargo fuzz` (libFuzzer) target is `crates/regia_media/fuzz` (`decode`, owner-run on nightly, ADR-1869 § 8). The open-ended nightly `cargo fuzz` job over the other loaders is still `planned` in `docs/ci/nightly-jobs.toml` | no |
| agent-protocol | ubuntu; `ci.yml` job `agent-protocol` | an MCP stdio smoke (`regia mcp` answers `initialize` and `tools/list`), `cargo test -p regia_cli --test mcp` (the `rmcp` test client), and `--test mcp_conformance` once `crates/regia_cli/tests/mcp_conformance.rs` exists (the step checks for the file) | yes |
| docs | ubuntu | `cargo doc -D warnings`, mdBook build | yes |
| fuzz | ubuntu nightly (`nightly.yml` `fuzz`, ADR-2723) | `cargo xtask fuzz run-all --seconds-per-target 60 --parallel 4` over the fuzz registry (`fuzz/`; `book/agents/fuzzing.md`), then the `regia_media` `decode` target for 10 minutes. The findings are uploaded. The deterministic registry of ADR-1917 (`regia_agent::fuzz_targets`, spec 97: every frozen format in `docs/formats/format-hashes.ron`, every `regia_net` message kind, the agent request text and parameter parsing; extended by the Lua host and the editor importers through `FuzzTargetExtensions`) still runs in every test pass as `crates/regia_agent/tests/fuzz_targets.rs`, committed findings replay there too (`fuzz_findings.rs`), and `fuzz.targets` / `fuzz.run` expose the registry in dev builds | no |
| agent-protocol | ubuntu (headless editor from M11) | protocol integration tests + MCP conformance via `rmcp` test client | yes (from M2) |
| build-times | ubuntu + windows | `cargo build --timings`; compare with budgets in `03` § 6 | report; regression > 20% fails from M3 |
| export-count | ubuntu + windows | `cargo xtask check-exports`: dynamic symbols of `regia_dylib` (Linux `nm -D --defined-only`, Windows `dumpbin /exports`) | yes when dependencies change, and at each milestone. Warn at 50,000, fail at 60,000 (ADR-015) |
| agent-benchmark | owner machine (uses a model API) | `bench/agent_tasks` success rate (`specs/23` § 8) | no (gate input at Gate B and after) |
| agent-benchmark-oracle | ubuntu, no GPU | `cargo xtask agent-bench --oracle` (every task passes with its oracle) and `--oracle --empty` (every task fails without one); the model run itself stays with the owner: `--serve <task>`, then `--check-only --attach <dir>` (ADR-2621) | yes once wired (the task set only grows) |
| agent-protocol-conformance | ubuntu | `cargo test -p regia_cli --test mcp_conformance`: `regia mcp` over stdio with `rmcp`'s client (handshake, tool list, error mapping, resources, cancellation); HTTP conformance waits for an approved HTTP client (ADR-2621) | yes |
| steam-linux | Steam Linux Runtime 3.0 sniper SDK container | shipping build of samples / DO27 shell | from M16 yes |
| bench | self-hosted reference machine (optional) | frame benches, compare to budgets | no (reports) |
| feature-matrix | ubuntu (step of `linux`) | `cargo xtask check-features`: clippy `-D warnings` per optional feature set the default build never compiles (`docs/04` § 11, ADR-2322); fails on an uncovered feature | yes |
| media-feature | ubuntu (+ cargo-xwin, NDK) | `cargo clippy -p regia_media --features video,voice-opus --all-targets -- -D warnings` for x86_64 Linux, `x86_64-pc-windows-msvc` (cargo-xwin) and `aarch64-linux-android` (cargo-ndk), then `cargo test -p regia_media --features video,voice-opus` on Linux (codec fixtures and the fuzz corpus, `tests/codecs.rs`, `tests/fuzz.rs`): the decoder build under the NDK and clang-cl with no system NASM or Meson (ADR-1593 § 5, ADR-1670). `av1-nasm` is not in this job: it needs NASM, which this job keeps out on purpose. Since ADR-1869 the feature links `rav1d` (arm64 assembly assembled by `cc` on aarch64), `unsafe-libopus` and `lewton` | yes (ADR-1869: a decoder has landed); defined in `ci.yml` since ADR-2322, its Windows step allowed to fail until `windows-cross` is seen green (ADR-295) |
| two-tag-upgrade | ubuntu; `nightly.yml` job `two-tag-upgrade` (weekly, Sunday 09:00 UTC) | `cargo xtask two-tag-upgrade --from <older tag> --to <newer tag> --templates all` over the two newest `v0.*` tags: the older tag's `regia new` for every template, the newer tag's `regia upgrade-project --write`, then `cargo check` of the result against the newer tag (spec 94 § 9, ADR-2610). Builds two CLIs and every template, so it is not a per-push job | no (report) |

Caching: `Swatinem/rust-cache` or equivalent; separate cache for `.regia/cooked` of samples keyed by
importer versions.

Jobs outside the merge gate (the player sweep, look CI, soaks, benches, device passes) are listed with their command,
host, schedule, status and owner in `docs/ci/nightly-jobs.toml`. Known flaky tests and the gate's retry rule are in
`docs/ci/flaky-tests.toml` and `book/user-guide/flaky-tests.md`. `cargo xtask nightly` and `cargo xtask flaky`
check both against the tree (ADR-1035). Every job above that has a workflow is defined in `.github/workflows/ci.yml` or `nightly.yml`;
`xtask/tests/workflow_yaml.rs` reads every workflow file (syntax, required keys, `needs`), and `xtask/tests/ci_workflows.rs` ties
`ci.yml` to the merge gate and `nightly.yml` to the registry. `actionlint`, where installed, is the deeper check.

## 6. Determinism tooling
- `InputRecording` asset: records action states per fixed step + RNG seed; playback drives smoke tests and
  bug repro. Engine RNG seeded per session; `--seed` flag.
- Physics determinism test: same recording twice → identical final transforms (same build/platform).
- Record/replay stress (ADR-537): `cargo xtask repro-check` records each determinism scene under 20 seeds and
  replays every recording in a fresh app; the world state hash (`regia_framework::state_hash`, every reflected
  component, map entries sorted) must match at every checkpoint. Playback injects a recorded step before that
  frame's action evaluation, so replay step k sees recorded step k. Agents compare runs with `sim.state_hash` /
  `sim.state_diff` (`book/agents/determinism.md`).
- Replay soak (ADR-867, `book/agents/replay-soak.md`): `cargo xtask replay-soak` records and replays N seeds of
  the CPU soak scenes once per compute-thread count and build profile. Record traces must be equal across runs,
  and every replay must equal its own recording. The gate runs a 2-seed, 120-step matrix
  (`regia_test_utils --test replay_soak`). Playback injects step k's values before step k's own evaluation.
- Procedural graphs (ADR-1640, specs/62 § 6.3): `regia_procgen/tests/eval_core.rs` records the acceptance graph's output
  hash in the test source; the same test must pass unchanged on Linux, Windows, Android aarch64 and macOS. Placement
  math uses `regia_procgen::detmath` and scalar quaternions (`regia_procgen::rot`), never the platform libm or SIMD
  quaternions; a mismatch is a bug, never a re-bless without a reason in the commit message.
- Biomes (ADR-1665, specs/65 § 11): `regia_procgen/tests/biomes.rs` test `t9_…` records the three-biome starter graph's
  output hash; the same rules apply (claims, water distance and resolve use integer grids, `detmath` and total sort keys).

## 7. Layering check
`xtask check-layers` reads `cargo metadata` and asserts the DAG from `02-architecture.md` § 1 (runtime
crates must not depend on `regia_editor`, `regia_asset_pipeline`, `regia_cli`, `egui*`, `gltf`,
`image`, `intel_tex_2`, `meshopt`, `notify` except `regia_asset`'s desktop-only dev feature).
`regia_test_utils` is test-only: it may use `image` and `image-compare`, and may appear only as a
dev-dependency. `regia_agent` encodes screenshots with the `png` crate directly, not `image`.
The `showcase` sample is footage tooling that is never shipped inside a game; it is the one sample allowed to use
`image` (ADR-1439, like `render_lab` using `regia_test_utils`). No other sample or runtime crate may.
The one runtime→editor edge is the `regia` feature `hot-reload` depending on `regia_editor`
(ADR-015, `specs/20` § 1): `check-layers` allows it, and shipping builds do not enable the feature.

`xtask check-layers` also reads the tree of `regia --no-default-features --features server` on Linux and Windows and fails on a
window, render or audio crate (spec 76 § 2.1); `xtask check-server-build` compiles `dedicated_server` that way. The gate's
clippy and tests build the default features only, so these two are what keeps a simulation module from reaching a client one.

The other optional features are compiled by the feature matrix (`cargo xtask check-features`, ADR-2322; the table and
its rules are in `docs/04-conventions.md` § 11): one clippy `-D warnings` run per feature set of the engine crates
(`regia_render` and `regia` with defaults off as games build them, DLSS over the NGX check stub, Steamworks, the media
codecs, `lua`, `xr`, `dcc-feed`, the umbrella one feature at a time and all at once). Preflight runs it as step 7 and
the CI `linux` job after clippy. It is a compile and lint check only: a feature's tests still run only where a job
runs them (`media-feature` for the codecs; the DLSS and Steam smokes on hardware). The real NGX SDK path (`DLSS_SDK`
set) is compiled only on a machine with the SDK; the matrix compiles the same Rust over the check stub.

## 8. Verification environment (owner decisions, ADR-009)
The owner has every device the plan needs except a Mac. The builder uses them directly wherever it can.

| Platform | Where it is verified | Notes |
|---|---|---|
| Linux | the owner's machine (the builder's host) | All frame-rate gates and DO27 tiers are measured here |
| Windows | the owner's **Windows VM** only — no hosted Windows runners | Driven over SSH or a self-hosted CI runner. What cannot be verified in the VM does not ship on Windows. At M1 the builder records whether the VM has GPU passthrough: without it, Windows gets functional verification on WARP only and no Windows frame-rate numbers are claimed |
| Windows (state 2026-11-09) | Linux CI/builders for cross checks; Windows 3080 and the min-spec Windows laptop for native acceptance | **Historical native evidence:** the v0.2.17 pass on 2026-11-08 recorded six test steps and five quits passing, Vulkan/DX12 presentation, native crash capture and deliberate device-loss exit. Its later full-workspace attempt OOMed before tests. **Current candidate:** v0.2.20 B records 16 focused native the min-spec Windows laptop cases on Intel Vulkan, DX12 extension 3/3 and all six DX12 lightmap cases across a timed-out five-case run plus the final exact-case pass. The complete native gate remains open; neither this subset, the historical pass nor Linux MSVC cross checks certify later graph/feature changes. Linux covers Windows target compilation/lints, HLSL translation and backend-policy tests, not driver shader compilation or native crash behavior. The frozen-source recipe and remaining commands are in `docs/evidence/windows/v02020-acceptance-b.md`; the lead runs `cargo xtask windows-pass` on available Windows hardware and retains all logs (`book/user-guide/windows.md`). |
| Android | the owner's devices over `adb` (install, logcat, input, screenshots); emulator for CI smoke | Reference and low-end models recorded at M1 (§ 3). Native crash records (ADR-328): the record format, frame walk and next-launch bundle are Linux tests; the handler itself is verified only on the device, by `cargo xtask android-pass --crash-test` (Pixel 6, adb from the workstation) |
| Steam Deck, DO27 tier GPUs | the owner's hardware, over SSH where reachable | Needed from M15a and at Gate C |
| macOS | the owner's MacBook over SSH (Apple M5, 32 GB, macOS 26.6, Metal 4), testing only — ADR-264 | Builds (`cargo check --workspace`, `regia`, `regia-editor`) and runs the whole test suite on Metal (no lavapipe); non-blocking, never a gate. Not verified: SPIR-V validation (no `spirv-val`), frame-rate budgets, hot reload, signing and notarization. Results: `docs/releases/v0.1.22.md`. Tier 2 (`specs/22` T2.15). How the sweep runs and its two known environment failures: "macOS test sweep" below |

### macOS test sweep (ADR-1379)

The Mac runs the whole workspace suite with the build machines script `<scratch> <repo> <out>`
(outside the repo; nice 19, `-j 4`). It is informational: a red Mac result never blocks a merge or a tag.

- **Sequencing.** One `cargo test --workspace --no-run`, then every binary one at a time in package order with
  `--test-threads 4`, then the *solo* binaries last, alone, with `--test-threads 1`: `regia_editor` and
  `regia_vfx` `hot_reload`, and `regia_test_utils` `gpu_matches_cpu`. The script never deletes the shared
  cargo target during a run (the 2026-10-20 recreation removed `<repo>/target` after every binary).
- **Timeouts.** 1500 s per binary. A binary past that is killed with its process group and re-run test by test,
  120 s each, so a hang costs one test and is named in `summary.json` (`hangs`).
- **Retries.** Each failed test is re-run once alone (`--exact`, one thread). A pass on the retry is listed under
  `flaky_tests`, not `failed_tests`; a test that keeps failing stays failed. A binary whose tests all passed but
  whose process exited non-zero is flagged `exit_only_failure`.
- **Preflight notes.** Before building, the script writes `preflight.json` with the environment gaps it found, so
  their failures are labelled rather than triaged again: `no_spirv_val`, `no_sponza`, `abi_hash_set`.

Two known failures that are about the environment, not the port:

- **`gpu_matches_cpu_on_m5_scenes` needs Sponza.** Without the Sponza and Fox glTFs the test skips those two
  compares with a note (ADR-2431; it used to fail); `REGIA_REQUIRE_ASSETS=1` makes a missing sample a failure
  again, here and in the Khronos sample tests of `regia_asset_pipeline` (`gltf_extensions`, `khronos_samples`,
  `skinned_mesh_v2`), so a job that must cover the samples should set it. The test looks only in
  `REGIA_KHRONOS_ASSETS` or `crates/regia_test_utils/gltf-cache`, not in the cache
  `cargo xtask fetch-test-assets` fills. Run `cargo xtask fetch-test-assets` in the repo on the Mac; the sweep
  then points `REGIA_KHRONOS_ASSETS` at `.regia/test-assets/khronos-gltf` itself. By hand:
  `REGIA_KHRONOS_ASSETS=$PWD/.regia/test-assets/khronos-gltf cargo test -p regia_test_utils --test gpu_matches_cpu`.
- **`hot_reload::hot_fixture_edits_keep_reflected_state` fails in the full sweep and passes alone.** The first
  reload is refused with "engine changed": the engine fingerprint moved between the host build and the first game
  build. ADR-356 found one cause (the warm-up build did not set `REGIA_ABI_HASH`, so cargo rebuilt
  `regia_framework` and relinked the dylib); alone, the test passed on 2026-10-21 (`t5-hot-reload.txt`). In the
  full sweep the binary also competes with the sweep's own build load. The sweep therefore runs it solo and last,
  with one retry, and an inherited `REGIA_ABI_HASH` in the sweep's environment is a preflight note. A failure
  there that passes on the retry is a known flake; one that fails twice needs the `engine_fingerprint` file list
  dumped before and after (ADR-356 follow-up).

Checks only a human can make (audible glitches, controller feel, Bluetooth pairing, the plates review at
Gate C) are listed in `progress.md` under "Needs owner verification" with exact steps. `AGENTS.md` § 1.3
is unchanged: the milestone is not closed until the owner has recorded the result.

## 9. Microbenchmarks (hand-run, no GPU)
Criterion-free timing tests for CPU hot paths. Each is `#[ignore]`, so `cargo test --workspace` and the
gate never run them; they print `perf-micro <name>: n= p50= p99= max=` per operation and assert only that
the work happened, never a time. Always run them in release mode on a quiet machine, and compare numbers
only between runs on the same machine.

```
# ReplaySim step/seek/snapshot and camera path (shot sequence) sampling
cargo test -p regia_framework --release --test perf_micro -- --ignored --nocapture
# Render-out frame delivery (PNG sink to a temp dir, supersample downsample); no ffmpeg needed
cargo test -p regia_agent --release --lib perf_micro -- --ignored --nocapture
```

| Bench | Where | Measures |
|---|---|---|
| `replay_sim_tick`, `replay_sim_seek`, `replay_sim_parse_load` | `regia_framework/tests/perf_micro.rs` | a synthetic 10-minute, 60 Hz replay (64 entities, a full each second): `step(1)`, `step(60)`, `seek`, `snapshot` |
| `camera_path_sampling` | same file | `ShotSequence::sample` for Catmull-Rom, linear and step paths with 8 and 256 keys |
| `perf_micro_writer_pool_4k` | `regia_agent/src/render_out/writer.rs` | render-out writer pool frames per second on a synthetic 4K frame source (no GPU) against the old one-thread PNG path (ADR-1412; it replaced the sink's `perf_micro_frame_sink_throughput`) |
| `perf_micro_accumulate_and_resolve` | `regia_agent/src/render_out/accumulate.rs` | CPU cost of adding one render-size shutter sample and of resolving a delivered frame, 8 and 16 bits, at 1440p and 4K (ADR-1404) |
| `bindless_bench` (needs a GPU; ADR-427) | `regia_render/tests/bindless_bench.rs`: `cargo test -p regia_render --release --test bindless_bench -- --ignored --nocapture` | six material-binding models (per-material uniform groups, step 1's arena at an offset, arena + `first_instance`, arena + immediate, texture table, and level 3's arena at a dynamic offset + texture table, ADR-863) on 1,000 and 10,000 materials: setup, steady-frame encode and submit, churn-frame updates, with the buffers, bind groups and binds counted. Method and results: `docs/evidence/bindless/README.md` |
| `material_scale_bench` (needs a GPU; ADR-427) | `regia_render/tests/material_scale_bench.rs`: same command with `--test material_scale_bench` | `FrameRenderer` with 1,000 and 10,000 distinct materials (steady, in-place edits, replaced keys) on the adapter's limits, on 10 storage buffers per stage (step 1's uniform path) and with the texture table features (bindless level 3, ADR-863): first frame, `render_into` CPU p50 / p99, buffers and bind groups per frame |
| `perf_micro_crowd_step`, `perf_micro_tile_rebuild` (ADR-1484) | `regia_nav/tests/perf_micro.rs`: `cargo test -p regia_nav --release --test perf_micro -- --ignored --nocapture` | `NavCrowd::step` for 50 and 500 agents crossing an open floor (600 steps), and one tile-cache obstacle add + update + remove + update on a 128 m floor with 32-cell tiles |
| `perf_micro_region_rebuild`, `perf_micro_nav_cell_load` (ADR-1934) | `regia_nav/tests/perf_micro.rs`: same command | One 6 m region re-voxelized and polygonised on a 128 m map; one streaming cell loaded and unloaded, 1 and 9 tiles |
| `perf_micro_motion_matching_search` | `regia_animation/src/motion_matching/tests.rs`: `cargo test -p regia_animation --release --lib perf_micro_motion_matching -- --ignored --nocapture` | motion-matching database build time and per-query search cost, linear scan against the exact k-d tree, on 12,000 frames of 27 features (ADR-1478) |
| `perf_micro_square_km_pipeline`, `perf_micro_compile_200_nodes`, `perf_micro_million_points_jitter_filter` (ADR-1640) | `regia_procgen/tests/perf_micro.rs`: `cargo test -p regia_procgen --release --test perf_micro -- --ignored --nocapture --test-threads 1` | The spec 62 § 12 single-core pipeline (1 km × 1 km ground at 2 m = 250 000 candidates, slope and altitude filters, jitter, prune, spawn) with per-node times; compiling a 200-node graph; `points.jitter` and `filter.range` over 1 M points. First numbers (the build server, load ~70, 2026-10-29): pipeline p50 324 ms, compile 1.7 ms, jitter 65 ms, filter 75 ms |
| `perf_2km_three_biomes_full_bake` (ADR-1665) | `regia_procgen/tests/biomes.rs`: `cargo test -p regia_procgen --release --test biomes -- --ignored --nocapture --test-threads 1 perf_` | The spec 65 § 13 bake: a 2 km × 2 km map, 3 biomes, 6 layers, 20 entries through the starter graph, with per-node times (budget ≤ 6 s with 8 workers). First number (the build server, single-threaded evaluator, load ~90, 2026-10-29): 582 338 instances in 5.0 s (biome.layer 2.1 s, resolve 0.8 s, areas raster 0.8 s, grass mask 0.5 s) |
| `pane_bench` (ADR-2046) | `regia_editor/examples/pane_bench.rs`: `cargo run -p regia_editor --example pane_bench --release -- 1280x720 120` | the live viewport pane per frame, GPU texture path (render, submit, egui rebind) against the CPU readback path (render, readback, texture upload), median/p95 ms and fps on the adapter `REGIA_GPU_*` selects. Lavapipe checks it runs; the readback saving needs a real card |
| `assetdb_*` (ADR-2704) | `regia_asset_pipeline/tests/perf_micro_assetdb.rs`: `cargo test -p regia_asset_pipeline --release --test perf_micro_assetdb -- --ignored --nocapture --test-threads=1` | The spec 32 § 9 budgets on a generated 50,000-asset, 400,000-edge registry: project-wide validation with the bridge, graph query depth 2/2 width 50, size map of a 5,000-asset closure, save-time run of 10 assets; allocation counts of each |
| `level_10k_*` (ADR-2704) | `regia_scene/tests/perf_micro_level.rs`: same command with `-p regia_scene --test perf_micro_level` | A 10,000-entity level (spec 14 A2): cooked load, text load, snapshot, text and cooked save; allocation counts |
| `schedule_tick_*` (ADR-2704) | `regia_app/tests/perf_micro_schedule.rs`: same command with `-p regia_app --test perf_micro_schedule` | A tick in the third_person template's shape (its entities and game systems, not its plugins) and with 200 empty systems; allocations per tick on the single-threaded executor |

**Budgets (`cargo xtask perf-budgets`, ADR-2704).** `docs/perf/budgets.ron` ties each spec CPU budget to one of the commands
above and to the number to read from its output (`docs/formats/perf-budgets-ron.md`), so a budget no longer rests on someone
remembering to run its bench. The harness runs each command once, prints budget, measured value and margin, and compares with
a per-host baseline (`docs/perf/budget-baseline-<host>.json`, tolerance 1.5x), since a time only compares with one from the
same machine. It exits non-zero only with `--enforce`. Results go to `docs/perf-log.md` as tables, and a miss is reported
there, not fixed by the harness. Agent guide: `book/agents/perf-budgets.md`.

```
cargo xtask perf-budgets                        # all of it; the first run builds every measured crate in release
cargo xtask perf-budgets --filter "spec 32"     # by name, spec or crate
cargo xtask perf-budgets --update-baseline      # record this host's numbers
cargo xtask perf-budgets --counts-only --enforce   # allocation counts only: cheap, the same on every machine
```

**Allocation counts.** A test binary can install a counting global allocator and print
`perf-count <name>: ops= allocs= bytes=` next to its timings. A count does not depend on the machine or its load, so it has one
baseline (`docs/perf/budget-counts.json`) and `--counts-only --enforce` can gate on any host: any growth fails by default. Count
the measuring thread only, so a worker pool's size cannot move the number, and run the work single-threaded: the multi-threaded
executor moves systems between the main thread and the workers with the host's load, and the count stops repeating.

**Test build and run costs (`cargo xtask test-costs`, ADR-2986).** `cargo xtask test-costs [--packages a,b] [--out DIR]
[--jobs N] [--test-threads N] [--timeout S] [--no-run]` builds the tests in its own target directory
(`target/test-costs/target`, because its link-timing shim changes cargo's fingerprint) and writes, per test binary, its
disk (executable plus split debug info), link time, cargo unit time, test count and run time, and per test its run
time: `test-costs.csv`, `slowest-tests.csv` and `test-costs.md` (sorted by link plus run seconds, with the top lists)
in `target/test-costs/report`. It asserts no time and records the load average; compare numbers within one report.

**Test suites (ADR-2986).** In `regia_ui`, `regia_physics` and `regia_terrain`, the integration tests that need no
process of their own are modules of one binary, `tests/suite/main.rs`, instead of one binary per file. A test there is
named `<module>::<test>`. Run the suite with `cargo test -p regia_ui --test suite`, one module with
`cargo test -p regia_ui --test suite tq_draw::`, and one test with
`cargo test -p regia_ui --test suite tq_draw::intersection_edges -- --exact` (the module is the old file
name). A new `tests/<name>.rs` file is still its own binary by default; move it into `tests/suite/` (with `git mv`, and a
`mod <name>;` line in `main.rs`) only when it has no `#[global_allocator]`, changes no environment variable or working
directory, uses no process-wide singleton (`regia_core::tasks::init`, a global logger or panic hook), is not
`harness = false`, a solo binary or a GPU test (the gate's GPU pass picks binaries by their `src_path`), and no doc,
script or registry names its binary.

### Loaded-gate scheduling of full GPU fixtures (ADR-4760)

The parallel-binary gate's 1,200-second process guard detects a hung test; it is not a
render-performance budget. The following full GPU fixtures run in the serial group
(`--solo <key>` in the build machines supervisor), on lavapipe as well as on a card. They keep all
their existing assertions and remain required gate tests. Do not ignore them or increase
their numeric thresholds. The gate operator applies these selections to the external
orchestration runner; ordinary `cargo test` already runs binaries sequentially.

| Gate key | Reason for serial execution | v0.2.18 alone |
|---|---|---|
| `regia_terrain:water_ocean_frame:test` | Full 128–1024 FFT-grid/spectrum matrix, CPU mirrors and repeated full-frame pipeline construction | 435 s |
| `regia_vfx:hdr_frame:test` | Full daylight/shade and translucent-layer lighting matrix with cold full-frame pipelines | 428 s |
| `regia_render:hwrt_frame_hitch:test` | Cold canonical-frame warm and all available trace variants; removing a variant would reduce coverage | 469 s |

Before returning a fixture to the pooled group, demonstrate its unchanged checks pass
beside other full GPU binaries within the supervisor guard. Timing measurements remain
diagnostics. Allocation checks count the frame's drawing and encode threads (ADR-2299),
then warm for 40 frames and settle on two consecutive batches within one allocation per
frame, with a bounded number of batches; scheduler-dependent worker placement must not
change the count.

### Allocation tests on a loaded machine (ADR-5718)

A test that counts heap allocations installs `regia_test_utils::alloc_counter::CountingAlloc` and follows the rule below
for what it measures. Each rule answers a cause found by running the 52 allocation test binaries 20 times beside each other
on a loaded machine; the module's docs have the details.

| What is measured | Count with | Why |
|---|---|---|
| Work on the calling thread (a system on the single-threaded executor, a query, a codec) | `alloc_counter::measure`, or `thread_allocs` after `quiet_tracing()` | Other threads cannot leak in. `quiet_tracing` matters in the gate's build: `tracing` has its `log` feature there, and without a dispatcher every system run allocates a `String` for a disabled span, which a `cargo test -p` build does not |
| A rendered frame | `count_this_thread` on the drawing thread and as `FrameRenderer::set_encode_worker_start_hook`, then `SteadyAllocCheck::run_warmed` on `counted_allocs`, with `device_pools::fill_usage_scope_pool(device, renderer.encode_threads() + 1)` as its hook | The drawing thread finishes an encode segment a late worker has not started (ADR-2299): its own count jumps by a segment, the frame's threads together do not. The rule warms 40 frames and settles before it measures, because wgpu's pools are still filling in the first frames. One pool does not fill by itself: wgpu keeps a usage scope per pass replayed at the same moment, and a new one (6 allocations), a pooled one resized after the device made more resources (up to 6 reallocations) or its map of partly used textures growing (1) lands in whichever frame first has one more thread inside a pass. The fill holds that many threads inside a pass at once, so all of it happens between the warm frames and the settling |
| Bytes or counts of every thread (`process_stats`, `regia_core::memory`'s ledger) | `let _alone = alloc_counter::exclusive();` as the first line of every test of the binary | Another test's setup lands in the window otherwise. Byte bounds with slack only: the test harness's own threads add a few requests |
| A frame, or two GPU call sequences, compared exactly | No pixel readback in the measured frame (`begin_offscreen`, or `render_into` a target the test owns), `device.poll(wgpu::PollType::wait_indefinitely())` after every frame or submit, and `device.wait_after_submit_on_this_thread(true)` (`RenderDevice`, cargo feature `test-hooks`) before the first frame | What wgpu allocates depends on what the device has finished: its command-buffer free list refills 16 at a time when the device is behind, and each pooled encoder grows its own framebuffer cache the first time it records a given pass. Drained, every frame starts from the same pool. A buffer mapped right after a submit costs one allocation more while that submission is in flight than after the device finished it, and wgpu's submit already looks for finished work before it returns: a frame that reads its pixels back, or maps its upload chunk and its readbacks after its own submit (every GPU-driven frame does), allocates one less whenever load lets the device get there first, in streaks of frames. With the hook every submit of the drawing thread waits for the device, so every map follows a finished submission |

A steady-frame run prints the calling thread's own count and the whole process's beside the counted total. When a run
fails, `REGIA_ALLOC_TRACE=1 cargo test -p <crate> --test <name> -- --nocapture` repeats it with a backtrace per
allocation and lists what the largest batch did that the settled one did not: first by call site, where work that
only moved between threads cancels out, then by whole stack (`REGIA_ALLOC_TRACE=<n>` lists `n` sites). A traced run
that passes lists every batch that counted differently from the run's usual one, with the sites that made more
requests and the sites that made fewer: an event that is still inside the slack gets a name before it meets a second
one. In an exact-count test, `alloc_counter::trace(step).1.report(8)` in the
assertion message names the site. The tracer captures with `std::backtrace` inside the allocator, so it is for a
failing run only (many times slower), and the traced code must not capture a backtrace of its own outside a panic:
std holds a lock while it captures, and the tracer's capture inside that one would wait on it forever. To reproduce a
loaded gate, run the built binaries several at a time on a few CPUs (`taskset -c`), not one binary alone: every cause
above passes alone.


## 10. Baked-light look review

Baked light is judged by eye against the path tracer, not by test counts (ADR-314). `cargo xtask gi-compare`
renders, per named view, the frame lit by the baked volume, the same frame with DDGI, and the `regia_pt`
reference, each as the full image and as indirect light only. It writes one contact sheet per view (the same
exposure on every tile of a row, plus FLIP error tiles) and a `summary.json` with FLIP, SSIM and relative RMSE,
a leak metric over an interior box, and the volume's status (`book/agents/gi-compare.md`).

- **Fixtures:** `assets/fixtures/gi_compare/`: `cornell` (the L1 case under a small overhead emitter),
  `leak_room` (a closed room with one doorway on sunlit ground; its interior views carry the leak mask) and
  `rocks_capsule` (rocks in the bake, a dynamic capsule occluder over it). Each has a `*.views.ron`.
- **Who runs it:** the lead, on a reserved GPU, before a baked-light release and for every change to the bake or
  the baked lookup. The owner and an Opus reviewer judge the sheets; `summary.json` and its flags (leak over
  threshold, an inactive bake, a FLIP regression against `--baseline`) say where to look first. The last line of
  the table says the plates decide.
- **What CI checks:** only the CPU half: the views and fixture parsers (typed errors), the summary schema round
  trip, the verdict on fixture JSON, the leak arithmetic and the sheet layout (`cargo test -p xtask gi_compare`).
  No golden image and no GPU run in the gate.
- **Keep the previous run.** Pass the last accepted `summary.json` as `--baseline` so a regression in FLIP is
  flagged; compare at the same spp and size.

## 11. Perf smoke (`cargo xtask perf-smoke`)

A cheap regression tripwire, not a benchmark suite. It runs five deterministic CPU kernels (`blake3_1mib`,
`mat4_chain_20k`, `sort_u64_100k`, `postcard_roundtrip_2k`, `ron_parse_2k`), takes the median of 15 samples each and
compares it with `docs/perf/smoke-baseline.json`. A bench fails only when its median is more than 3x its baseline
(`--tolerance`), so machine noise passes and an order-of-magnitude regression does not. A bench that is more than 3x
faster prints `FASTER` (the baseline is stale; not a failure). A `null` baseline entry prints `NO-BASELINE` and does
not fail.

```
cargo xtask perf-smoke                  # compare (re-launches itself in release mode from a debug xtask)
cargo xtask perf-smoke --update         # re-record the baseline on the reference machine
REGIA_PERF_HOST="the NVIDIA test machine 7950X3D 2026-10-22" cargo xtask perf-smoke --update   # also record where
```

Rules: record the baseline only on the machine that later checks it (numbers do not transfer between machines), on a
quiet machine, and say so in the commit message. The kernels call no engine API on purpose; they cover the libraries
the engine's hot paths use (content hashing, cooked-payload and scene-text codecs). Engine-level timings stay in the
hand-run benches of § 9. It is not part of `preflight` or the merge gate; run it in a nightly or release job on the
reference machine. Exit code 1 means at least one bench failed.

`cargo xtask perf-budgets` (§ 9) is the other half: engine-level measurements against the specs' budgets. Perf smoke stays
the cheap engine-free tripwire.

## 12. GPU job supervisor on the build server (`tools/gpusup/`)

An unattended bash supervisor keeps the build machines's RTX 5090s and the Intel Arc busy with Regia Engine checks between
gates, within the build machines's sharing rules. It runs on the build server under `<build-dir>` (`$G`) and
writes nothing outside it except the per-run card locks. Not part of the merge gate; it never takes the gate lock and
never touches `<build-dir>`.

- **Loop** (`gpusup.sh loop`, every 60 s): fetches `origin`'s `integ/v0*` heads every 5 minutes into a bare mirror,
  builds the newest one in one persistent tree and target, `$G/work` (synced with `rsync -c` like gate's gate tree, so
  only changed crates rebuild: a cold build on the shared bulk HDD takes hours), in stages: release `do27_chase` +
  `xtask`, the showcase host + CLI, then the gate's rendering-crate test binaries, spooled like gate. Each stage
  snapshots its binaries into `$G/builds/<sha>/`, so jobs keep using one head while the next builds. Then it fills
  free slots: the oldest `$G/queue/*.job`
  first, else the rotation, round robin: `gputest` (the gate's real-GPU pass, once per head), `sweep` (`xtask
  nightly-sweep` + plates + crash row, per head per day, 5090 and Arc), `showcase` (4K look-round plans, changed
  frames against the reference round, per head), `perfab` (`xtask gpu-bench` census per pass against the card
  model's baseline, `tools/gpusup/baselines/<model>.tsv` (`rtx-5090.tsv`: v0.2.10, ADR-2087), the run divided by
  its card's factor when the file lists the card; a pass slower than its `tol_pct` band (3 % unless the file widens
  it) is flagged; a model without a file is skipped, never compared against another model; the filler, at most two
  at once) and
  `soak` (hitch soak rounds for 2.5 h, two 5090s and the Arc per head per day), `hitchsoak` (the release `hitch_soak`
  test, the § 2.4 gate on DO27's content, once per head on a 5090; skipped, not passed, without the content; the
  build snapshots its binary after the chase stage, ADR-2303), and `replaysoak` (`xtask replay-soak`, CPU; it
  compiles, so it is scheduled like a build).
- **Rules it enforces:** at most 3 jobs at once (builds included); a GPU job starts only with MemAvailable >= 35 GB,
  a build with >= 45 GB; every job runs under `timeout`, `nice` and `ionice -c3`; builds use `CARGO_BUILD_JOBS` 12
  and a head's snapshot is deleted once a newer one is ready and unused. A build never starts while the GPU test pass
  or the replay soak uses `$G/work`. Cards are nvidia-smi indices 0, 1, 2, 3, 4, 7 and the Arc,
  never 5 (Vulkan 6) or 6. A card is skipped when its `/tmp/regia-gpu-N.lock` or `/tmp/ferrite-gpu-N.lock` (Arc: `regia-gpu-arc.lock`, `ferrite-gpu-arc.lock`; ADR-8670) is held,
  when another process has a compute context or more than 64 MiB on it (`nvidia-smi -q -x`), when card 7's
  `/tmp/do27-vl-active` exists, or (Arc) when another client's engine counters advance. Every run is pinned by
  `REGIA_GPU_UUID`/`FERRITE_GPU_UUID` (Arc: `_NAME=Intel`) with `_STRICT=1`. <!-- rename-keep -->
- **Results:** one line per run in `$G/results.tsv` (time, job, card, rc, key numbers, output dir); a failure also
  writes `$G/failures/<time>-<job>.md` (command, likely cause, log tail). `tools/gpusup/pull.sh [--loop]` copies them
  to `<scratch>` and `<scratch>` on the workstation.

```
bash tools/gpusup/deploy.sh --start                         # from the workstation: copy the scripts, start (no-op if running)
ssh <host> bash <build-dir>   # cards, jobs, last 10 results
ssh <host> touch <build-dir>           # stop launching (jobs finish)
ssh <host> bash <build-dir> stop --kill   # also end jobs + loop
# Queue a one-off GPU job: one command line, optional directives; runs in its output dir, newest binaries in $GPUSUP_BIN.
printf '# card=nvidia\n# limit=1800\n# lock=outer\n%s\n' '"$GPUSUP_BIN"/do27_chase --list-gpus' > x.tmp && mv x.tmp $G/queue/x.job
```

## 13. Look measurement (ADR-1837)

- **Golden plate floors (ADR-2089).** A run with `REGIA_PLATE_DIR=<dir>` records every golden compare as `<plate>.png` and a
  `<plate>.plate` line holding its PSNR against the lavapipe-blessed golden, also under `REGIA_SKIP_GOLDENS=1`. `cargo xtask
  plate-floors check --plates <dir> --vendor <nvidia|amd|intel|lavapipe>` holds the run to `docs/ci/plate-floors.toml`. Exit 0 passes;
  1 means a plate is under its floor, differs in size, or was not measured (`--partial` lists unmeasured plates instead, for a run of
  part of the suite, and still fails when no floored plate was recorded at all); 2 means the vendor has no floors yet (not gated). A new golden with no floor is listed and not gated until the row is
  re-measured. `cargo xtask plate-floors measure --plates <run> [--plates <run>...] --vendor <v> --note <...>` writes a vendor's
  row: the worst run, less the spread between runs and a 3 dB margin, capped at 40 dB and rounded down to 0.1 dB. The cap is there
  because an exact frame scores infinity, and a floor that high would fail a change the SSIM compare accepts. Goldens stay on lavapipe
  (ADR-025): a floor is a net for a look regression on one vendor, not a second golden. A lane that re-blesses a golden, or changes
  the look on hardware on purpose, re-measures the affected rows in the same commit series and says why. Wiring: the real-GPU pass
  sets the variable and checks `nvidia --partial`, as it runs only the rendering crates (`tools/gpusup/jobs/gputest.sh`;
  `the gate script` is outside the repo); the Arc and lavapipe
  rows are checked when those runs are made. AMD stays empty until a coordinated RX 7900 run measures it.
- **Whose failure a floor failure is (ADR-5798).** `check` cannot tell a vendor that diverges from a golden that moved on
  every device: both read as "under the floor". Before a floor failure is given to a card, run the same test binaries on
  lavapipe with their own `REGIA_PLATE_DIR` and ask `cargo xtask plate-floors attribute --plates <vendor run> --reference
  <lavapipe run> --vendor <v> [--table]`. For each plate under its floor it scores the vendor's recorded frame against the
  lavapipe frame of the same plate (`<plate>.png` in both directories, the PSNR of the `.plate` lines). `INCONSISTENT`
  (ADR-5897): the three scores cannot come from one build, as their RGB RMSEs break the triangle inequality by more than
  half a level (or one frame pair alone differs in size): a stale `<plate>.png`, a run of another build, or the wrong
  directory; the vendor run's own directory is refused as `--reference`. `GOLDEN`: the two frames agree to the floor or
  better and the lavapipe run is under its own lavapipe floor for the plate, so the golden is what moved (a stale golden,
  or a look change on every device), a golden blessed from that lavapipe run would pass, and `plate-floors check --vendor
  lavapipe` on the lavapipe run fails the plate. `SHARED` (ADR-5897): the frames agree but the lavapipe run is not under
  a floor of its own (inside it, or no lavapipe floor): a change both share that only this vendor's floor catches, so it
  is not excused. `VENDOR`: the frames do not agree, so this vendor's frame differs from lavapipe's by more than its floor
  allows; when the look change is intended, the row is re-measured with the re-bless (above). `NOREF`: the lavapipe run
  did not record the plate. Where a frame is missing, the lavapipe run's own score against its floor decides (under it:
  `GOLDEN`, else `VENDOR`). Exit 0 only when every plate under the floor is `GOLDEN`, 1 otherwise, 2 for a vendor with no
  floors; the floor file is only read. `--table` adds a Markdown table of every plate with its three scores. The lavapipe
  golden test does not own a `GOLDEN` plate (it compares by SSIM, which a 36 dB plate can pass), so wiring that lets
  `attribute` excuse a floor failure must also fail on `cargo xtask plate-floors check --plates <lavapipe run> --vendor
  lavapipe` (`--partial` for a run of part of the suite) exit 1: the `GOLDEN` plates are that check's failures. The gate
  scripts do not call `attribute` yet: their GPU pass has no lavapipe plate directory of the same build at hand.
- **Sweep plate floors.** `scripts/plate-psnr.py <ref-dir> <other-dir> --vendor <v>` scores a vendor's DO27 map plates against the
  reference-GPU plates and checks `scripts/plate-floors.json` (a floor per plate, set 3 dB under the 2026-10-21 sweep's measured
  pair). Exit 0 passes, 1 means a plate is under its floor or missing, 2 means the vendor has no floors yet. `--self-test` runs the
  script's own checks. It needs numpy and Pillow, which the build server does not have; the golden floors above need neither.
- **Analytic ground truth.** `cargo test -p regia_pt --lib cpu_brdf` checks the shared BRDF against closed forms that do not use the
  path tracer: GGX normalization, the Smith visibility limits, Schlick against the exact dielectric Fresnel, reciprocity, energy
  conservation and the Lambert albedo. The photographed chart (lux meter, ColorChecker) still needs the owner's hardware (Q16).
- **Live vs baked bench.** `do27_chase --baked-gi <volume.fgiv>` lights the replay with a baked volume instead of the live probes
  and prints whether the volume lit the last frame; `scripts/bench/baked-gi-suite.sh` interleaves live and baked runs on one card.
  It needs a DO27 bundle and a baked volume for the map (the Bore Twelve bake is blocked on the bundle's manifest).
- **Benchmark scenes.** `docs/benchmark-scenes.md` lists the scenes a published number may come from and their licences. `regia
  bench` runs them headless and prints frame stats; `scripts/fetch-bench-scenes.sh` (`regia bench --fetch`) downloads them with
  checksums (`book/agents/bench.md`).
- **Frame-bound verdict.** `play.hitch_report` reports `bound` (CPU-bound, GPU-bound, balanced, mixed or unknown over the measured
  frames) and a `bound` per spike (`book/agents/hitches.md`).
- **Permutation count.** The cook report (`CookReport::graph_permutations`, the `cook:` summary line of `regia cook`) counts the
  distinct forward-shader define sets of the project's material graphs against `FORWARD_PERMUTATION_BUDGET`.

## 14. Unsafe inventory, Miri subset and ThreadSanitizer (ADR-2086, ADR-2726)

The table below lists every `unsafe` site in the workspace by module: what it does, whether Miri can run it, and the test that covers it. `xtask/tests/unsafe_policy.rs` (`every_unsafe_site_is_in_the_inventory`) holds it to the source, so **a new site without a row fails the gate**.

- **Sites** counts blocks, impls, attributes, extern blocks and functions as the scanner does, in shipped code (`src` without its test items). The count is exact in both directions. A file may have several rows, one per answer in the Miri column, and they add up. The message says which row to change. `cargo test -p xtask --test unsafe_policy inventory_by_file -- --nocapture` prints the current counts.
- **`(test code)`** stands for a crate's test items, `tests`, `examples` and everything else outside `src`. Its count is `~N`, a snapshot of 2026-11-05 that is not pinned: a new counting allocator in a crate that has a row needs no edit, and the first `unsafe` in a crate's test code needs a row.
- **Miri** is `yes` or `partly` with the filters of `scripts/miri-unsafe-subset.sh` that run it, or `no (why)`. The check fails on a filter that is not in the script.
- **Covered by** names functions the check finds in the workspace, or says `none: why`. A name there means a test reaches the site; it does not mean the test proves the site's `SAFETY` claim, and for a site marked `no`, only a native run or a device can.

**Plain memory work and its neighbors**

| Crate | File | Sites | What they do | Miri | Covered by |
|---|---|---|---|---|---|
| regia_core | `src/memory/tagging.rs` | 18 | `TaggingAllocator`: a `GlobalAlloc` that keeps a one-byte subsystem tag in a header in front of each block | yes, `memory::tagging` | `header_keeps_alignment_and_tag_follows_the_block`, `the_inner_allocator_gets_back_the_layouts_it_handed_out`, `a_refused_allocation_changes_nothing`, `a_reallocation_too_large_for_the_header_is_refused`, `a_block_can_change_threads_between_alloc_and_free` |
| regia_core | `src/profiling/thread_clock.rs` | 1 | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` into a local | partly, `unsafe_miri`: the call and its out parameter, not the clock | `the_thread_clock_writes_only_its_out_parameter`, `counts_running_work_without_a_context_switch` |
| regia_core | `src/residency/platform/procfs.rs` | 4 | The residency sweeper: `sysconf`, `getpid`, `mincore`, `process_vm_readv`; `Send` for its scratch iovecs | no (syscalls: Miri has no `mincore` or `process_vm_readv`) | `a_region_that_is_not_mapped_reads_nothing_and_does_not_fault`, `a_partial_read_stops_at_the_first_unreadable_page`, `a_region_longer_than_a_batch_reads_every_page` |
| regia_core | `(test code)` | ~40 | Allocator tests and their checking inner allocator; `tests/unsafe_miri.rs`; the sweeper tests' `mmap` and `mprotect` fixtures | partly, `memory::tagging`, `unsafe_miri`: the `mmap` fixtures cannot run | `the_inner_allocator_gets_back_the_layouts_it_handed_out` |
| regia_editor | `src/hot_reload/load.rs` | 4 | `copy_export_table`, `copy_abi_hash`: copy a table and a hash out of a loaded library, with no assumption about alignment or null | yes, `hot_reload::load::tests::an_unaligned_export_table`, `hot_reload::load::tests::a_null_slot`, `hot_reload::load::tests::the_abi_hash_copy` | `an_unaligned_export_table_copies_intact`, `a_null_slot_in_the_export_table_is_an_error`, `the_abi_hash_copy_refuses_null_and_does_not_need_alignment` |
| regia_editor | `src/hot_reload/load.rs` | 15 | `dlopen` of a game library, `dlsym` of its symbols, the call of its entry (count refreshed 2026-11-11, ADR-5832) | no (FFI: `dlopen` and code that only exists in a real library) | `a_real_library_goes_through_every_read`, `a_library_without_the_symbols_and_a_junk_file_are_refused`, `hot_fixture_edits_keep_reflected_state` |
| regia_editor | `src/extension/load.rs` | 4 | `copy_stamp`, `take_extension`: copy a stamp out of a library, take back the box its entry released | yes, `extension::load::tests::the_stamp_copy`, `extension::load::tests::the_entry_box` | `the_stamp_copy_needs_no_alignment_and_refuses_null_and_junk`, `the_entry_box_round_trips_and_null_is_none` |
| regia_editor | `src/extension/load.rs` | 3 | `dlsym` and the calls of a library's stamp and entry functions | no (FFI: code that only exists in a real library) | `stamp_refusal_load_and_reload` |
| regia_editor | `(test code)` | ~10 | Fixture pointers in the load tests; `tests` with environment setup | partly, `hot_reload::load::tests::an_unaligned_export_table`: the rest load real libraries | `an_unaligned_export_table_copies_intact` |
| regia_platform | `src/wide.rs` | 2 | `until_nul`: the NUL-terminated UTF-16 strings the Windows API hands back | yes, `wide::tests` | `the_walk_stops_at_the_first_nul`, `an_empty_string_has_no_units` |
| regia_platform | `src/native_crash/memory_probe.rs` | 3 | The crash handler's checked read: `pipe2`, then `write` of an address range into the pipe, then `read` | partly, `native_crash::memory_probe::tests::readable_memory`, `native_crash::memory_probe::tests::stale_bytes`, `native_crash::memory_probe::tests::a_probe_with_no_pipe`: readable addresses; a bad one is the kernel's `EFAULT`, which only a native run has | `readable_memory_reads_back_at_any_alignment`, `stale_bytes_in_the_pipe_do_not_shift_the_next_read`, `unreadable_addresses_are_none_not_a_fault`, `a_short_copy_is_refused_and_leaves_the_pipe_empty` |
| regia_platform | `src/native_crash.rs` | 1 | Windows `crash_info`: reads the live exception record | yes, `native_crash::windows::tests` (Windows target) | `crash_info_reads_the_access_violation_of_the_live_context`, `crash_info_uses_the_instruction_address_for_other_exceptions`, `crash_info_ignores_the_record_of_a_context_no_filter_is_handling` |
| regia_platform | `src/hid/windows.rs` | 1 | `detail_header`: the size field of a variable-length Win32 struct in a `u32` buffer | yes, `hid::windows::tests` (Windows target) | `the_detail_header_fits_its_buffer_and_the_path_follows_it`, `a_buffer_too_small_for_the_header_is_refused` |
| regia_platform | `src/platform/windows.rs` | 1 | `empty_memory_status`: a zeroed `MEMORYSTATUSEX` | yes, `platform::windows::miri_tests` (Windows target) | `the_empty_memory_status_is_a_valid_value` |
| regia_platform | `src/platform/android/vsync.rs` | 5 | `choreographer_from` (addresses to function pointers), `Poster::post`, `on_frame` (the pointer back to the `Poster`) | yes, `platform::android::vsync::tests` (Android target) | `a_frame_callback_ticks_and_posts_the_next_one_only_in_the_foreground`, `a_function_pointer_survives_the_cast_from_its_address` |
| regia_platform | `(test code)` | ~18 | Fixtures of the tests above: `mmap`, `mprotect`, `pipe` writes, stand-in entry points | partly, `wide::tests`, `hid::windows::tests`, `platform::android::vsync::tests`: the `mmap` fixtures cannot run | `the_walk_stops_at_the_first_nul` |
| regia_dlss | `src/ngx.rs` | 2 | `ngx_table`: NGX's extension table as a slice, with its count bounded first | yes, `table_tests` (the check stub; leak check off) | `the_extension_table_is_bounded_and_names_stay_inside_their_arrays` |
| regia_media | `src/opus.rs` | 2 | `Raw::open`, `Raw::drop`: the multistream decoder's state | partly, `opus_boundary`: with Stacked Borrows off, because the library breaks them (below) | `a_decoder_opens_moves_to_another_thread_and_closes`, `a_refused_layout_is_an_error` |
| regia_media | `src/opus.rs` | 2 | `Send` for the decoder; `opus_multistream_decode_float` | partly, `opus_boundary`: the `Send` move; Miri stops inside the library on any decode (below) | `a_decoder_opens_moves_to_another_thread_and_closes`, `the_decoder_boundary_takes_a_junk_packet`, `gated_decoder_never_blasts_on_concealment_triggers` |

**Cannot run under Miri**

| Crate | File | Sites | What they do | Miri | Covered by |
|---|---|---|---|---|---|
| regia_asset | `src/pak.rs` | 1 | `memmap2::Mmap::map` of a pak file. The bytes it hands over are read by safe code, which Miri runs on owned bytes (`pak::tests::payload_slices`) | no (file `mmap`: Miri maps anonymous memory only) | `open_maps_the_file_without_copying_it`, `read_raw_follows_the_gate_and_a_truncated_pak_is_an_error_not_a_sigbus`, `a_mapped_pak_refuses_other_writers` |
| regia_media | `src/voice_opus.rs` | 9 | `unsafe-libopus` encoder and decoder: create, ctl, encode, decode, reset, destroy; `Send` | no (Miri stops inside the library under every aliasing model, at `opus_encoder_create` or the first encode; below) | `crosses_the_library_boundary`, `hostile_payloads_never_panic_and_never_write_past_a_frame`, `a_tone_round_trips_at_every_profile` |
| regia_media | `(test code)` | ~6 | Direct library calls in the `opus` module's tests | no (the library stops Miri; below) | `a_silk_to_celt_switch_without_redundancy_is_decoded_cold` |
| regia_dlss | `src/ngx.rs` | 36 | NGX: init, feature creation, evaluate, release; wgpu-hal raw Vulkan handles; `Send` and `Sync` for the parameter block and feature handles (count refreshed 2026-11-11, ADR-5832) | no (FFI to NGX: closed source, needs an NVIDIA card and the SDK) | `a_sampled_input_aliasing_the_output_is_detected` |
| regia_dlss | `(test code)` | ~3 | The check stub's `extern` block; the table test | partly, `table_tests`: the table; the stub is never linked | `the_extension_table_is_bounded_and_names_stay_inside_their_arrays` |
| regia_platform | `src/native_crash.rs` | 16 | Linux and Windows crash paths: `clone` watchdog, `pidfd`, `sigaction`, `tgkill`, `_exit`, `TerminateProcess`, thread and process handles, `make_crash_event` | no (syscalls and signal handlers) | `segfault_writes_a_bundle_with_a_readable_minidump`, `pidfd_kill_signals_the_process_the_fd_names` |
| regia_platform | `src/native_crash/android.rs` | 11 | The Android signal handler: raw syscalls, the record write, `__system_property_get`, a deliberate fault | no (syscalls and signal handlers) | none: `cargo xtask android-pass --crash-test` on a device |
| regia_platform | `src/hid/windows.rs` | 17 | SetupAPI, `hid.dll`, `CreateFileW`, `ReadFile`; `Send` for the owned handle | no (FFI: Win32) | none: needs a HID device on Windows; `the_path_starts_at_the_offset_and_ends_at_its_nul` tests the decode beside them |
| regia_platform | `src/platform/windows.rs` | 19 | Known folder, `ShellExecuteW`, locale, system metrics, registry, memory status, `TerminateProcess`, `IsWindowVisible`, `GetFileInformationByHandle` (open-file identity, ADR-4821); the borrowed window handle the bare-Alt subclass installs on (ADR-8158; count refreshed 2026-11-18); `MoveFileExW` without replace (`rename_no_replace`, ADR-8504); the single instance's process probe and the raise of its first window (`EnumWindows` and its callback, `ShowWindow`, `SetForegroundWindow`; ADR-8833; count refreshed 2026-11-21, ADR-10265) | no (FFI: Win32) | none: runs in the Windows pass (`docs/06-testing-and-ci.md` § 8) |
| regia_platform | `src/platform/android/vsync.rs` | 7 | `dlopen`, `dlsym`, `AChoreographer_getInstance`, `clock_gettime`, `ANativeWindow_setFrameRate` | no (FFI: NDK system libraries) | none: `cargo xtask android-pass` on a device |
| regia_platform | `src/platform/android/apk.rs` | 1 | `Send` for an open `AAsset` | no (a promise about an NDK object; no code runs) | none: `cargo xtask android-pass` on a device |
| regia_platform | `src/platform/android/jvm.rs` | 1 | The activity's JNI global reference | no (FFI: JNI) | none: `cargo xtask android-pass` on a device |
| regia_platform | `src/platform/android/mod.rs` | 2 | `_exit`; `kill(pid, 0)`, the single-instance lock's liveness probe of the holder pid (count refreshed 2026-11-21, ADR-10265) | no (ends the process) | none: `cargo xtask android-pass` on a device |
| regia_platform | `src/platform/linux.rs` | 4 | `_exit`; `getuid` and `kill(pid, 0)` for the single-instance lock; the Wayland null-buffer unmap of the fast quit through `dlopen`/`dlsym` of libwayland-client (ADR-8833; count refreshed 2026-11-21, ADR-10265) | no (ends the process) | none: reached by the quit in `process_exit`, which sees the status, not the call |
| regia_platform | `src/platform/macos.rs` | 3 | `_exit`; the AccessKit adapter on a window's view; `kill(pid, 0)` for the single-instance lock (count refreshed 2026-11-21, ADR-10265) | no (FFI: AppKit) | none: runs on the owner's Mac only (testing only, ADR-264) |
| regia_platform | `src/platform/desktop.rs` | 1 | macOS `pthread_main_np` | no (FFI, macOS) | none: runs on the owner's Mac only (testing only, ADR-264) |
| regia_platform | `src/background.rs` | 3 | `setpriority`, `ioprio_set`, `SetThreadPriority` | no (syscalls that take no memory) | `lowering_a_scratch_thread_does_not_fail` |
| regia_platform | `src/console_ctrl.rs` | 2 | `SetConsoleCtrlHandler`, `ExitProcess` | no (FFI: Win32) | none: needs a Windows console; `first_ctrl_c_requests_shutdown_and_the_second_exits` tests the flags |
| regia_platform | `src/paths.rs` | 3 | `statvfs` and its `assume_init`, `GetDiskFreeSpaceExW` | no (syscalls) | `free_space_asks_the_nearest_existing_ancestor` |
| regia_platform | `src/sleep.rs` | 1 | `clock_gettime` of two clocks into a local | partly, `sleep::tests::live_clocks`: the call and its out parameter, not the clocks | `live_clocks_do_not_report_a_sleep_between_back_to_back_polls` |
| regia_quick | `(test code)` | ~8 | The counting `GlobalAlloc` of `tests/draw2d_alloc.rs` (gpu-2d-drawlist; row added 2026-11-18, integ-v06r) | no (not in the recipe) | none: its asserts count allocations |
| regia_render | `src/pipeline_cache.rs` | 2 | `create_pipeline_cache` from a driver blob; two wgpu-hal device handles compared. `decode`, which checks the blob first, is safe code that Miri does run (`pipeline_cache::tests::every_proper_prefix`) | no (GPU: needs a device) | `a_cache_serves_only_the_device_that_opened_it`, `shipped_cache_seeds_and_wrong_caches_are_rejected_safely`, `every_proper_prefix_is_refused_and_alignment_does_not_matter` |
| regia_render | `src/quality/mod.rs` | 2 | VRAM queries through wgpu-hal (Vulkan, DX12) | no (GPU: needs a device) | none: read at device creation by every GPU test; no test checks the value |
| regia_render | `src/hwrt/probe.rs` | 1 | The `ExperimentalFeatures` token | no (a promise to wgpu; no code runs) | `an_offered_request_enables_ray_queries_and_the_token` |
| regia_render | `src/platform/linux.rs` | 3 | `setpriority`, `sysconf` and `clock_gettime`, `atexit` | no (syscalls and exit handlers) | `process_exit_cancels_and_joins_a_compiling_warm_worker` |
| regia_render | `src/platform/macos.rs` | 2 | `atexit`; `MTLDevice.currentAllocatedSize` through wgpu-hal (ADR-5860; count refreshed 2026-11-12, ADR-7386) | no (an exit handler) | none: runs on the owner's Mac only (testing only, ADR-264) |
| regia_render | `src/platform/windows.rs` | 1 | `atexit` | no (an exit handler) | none: runs in the Windows pass (`docs/06-testing-and-ci.md` § 8) |
| regia_render | `(test code)` | ~84 | Counting allocators in the GPU allocation tests; environment setup before a device; `driver_cost`'s harness | no (GPU: these tests need a device) | `a_cache_serves_only_the_device_that_opened_it` |
| regia_framework | `src/extension/mod.rs` | 4 | `#[unsafe(no_mangle)]` on the entry points `regia::extension!` generates (count refreshed 2026-11-12, ADR-7386) | no (attributes: no code runs) | `stamp_refusal_load_and_reload` |
| regia_framework | `src/game.rs` | 2 | `#[unsafe(no_mangle)]` on the entry points `export_game!` generates | no (attributes: no code runs) | `hot_fixture_edits_keep_reflected_state` |
| regia_framework | `(test code)` | ~21 | Counting allocators | no (not in the recipe: the same seven forwarding sites as `alloc_cost`, in slower tests) | `a_thousand_frames_allocate_nothing_and_stay_under_budget` |
| regia | `src/lib.rs` | 1 | `#[unsafe(no_mangle)]` on the `android_main` that `regia::android_main!` generates | no (an attribute: no code runs) | none: `cargo xtask android-pass` on a device |
| lookdev | `src/chase.rs` | 1 | `madvise(MADV_PAGEOUT)` in the overlay chase bench | no (a syscall on real mappings) | none: a bench, run by hand |
| regia_telemetry | `(test code)` | ~7 | A counting allocator | no (not in the recipe: `alloc_cost` asserts a wall-clock budget, which Miri's speed fails; a run to that assert, 1000 frames, showed no undefined behavior) | `a_thousand_frames_allocate_nothing_and_stay_under_budget` |
| regia_ai | `(test code)` | ~14 | Counting allocators | no (not in the recipe: the forwarding shape of `alloc_cost`) | none: their asserts count allocations; they prove no soundness of their own |
| regia_physics | `(test code)` | ~7 | A counting allocator | no (not in the recipe: the forwarding shape of `alloc_cost`) | none: their asserts count allocations; they prove no soundness of their own |
| xtask | `(test code)` | ~1 | The deliberate fault of the crash test | no (a deliberate fault) | `segfault_writes_a_bundle_with_a_readable_minidump` |
| product_viewer | `src/android.rs` | 1 | JNI: the activity object cast to read the launch intent's extras | no (FFI: JNI) | none: a device run of the viewer on Android |
| product_viewer | `src/pipeline_cache.rs` | 1 | `create_pipeline_cache` from the driver blob kept between runs (ADR-2835) | no (GPU: needs a Vulkan device) | none: a device run; wgpu checks the blob's header |
| product_viewer | `(test code)` | ~2 | Test items in `src` | no (not in the recipe) | none: not reviewed at the integ-v049 recovery (ADR-5832) |
| regia_framework | `src/test_alloc.rs` | 7 | A counting `GlobalAlloc` for the crate's unit tests (compiled for `cfg(test)` only) | no (not in the recipe: the forwarding shape of `alloc_cost`) | none: their asserts count allocations; they prove no soundness of their own |
| regia_media | `src/encode/opus.rs` | 6 | `unsafe-libopus` encoder: create, ctl, encode, destroy; `Send` for the encoder | no (Miri stops inside the library, see the Opus note below) | `head_timing_and_refusals`, `packets_cover_the_input_and_the_padding_trims_it_exactly` |
| regia_net | `src/private_file.rs` | 20 | Owner-only files: Unix create mode and mode check; Windows SIDs, a protected DACL, `CreateFileW`, `GetLastError` (count refreshed 2026-11-12, ADR-7386) | no (syscalls and FFI: Win32) | `a_created_file_dacl_is_protected_and_owner_only` (Windows target) |
| regia_net | `src/server_host/platform/windows.rs` | 1 | The native-fault handler that ends a dedicated server with exit code 70: `TerminateProcess` of the current process (moved from `src/server_host/crash.rs`, whose one remaining block is the test child's deliberate fault) | no (signal handlers) | `a_native_fault_exits_70` |
| regia_net | `src/windows_acl.rs` | 17 | Win32 security descriptors and ACLs of the server's secret files (count refreshed 2026-11-12, ADR-7386) | no (FFI: Win32) | `a_private_file_is_not_exposed_and_grants_are`, `create_private_file_refuses_existing` (Windows target) |
| regia_net | `src/socket/platform/unix.rs` | 3 | `setsockopt`/`getsockopt` declarations and calls: UDP buffer sizes, read-back and the DSCP mark on Linux, Android and Apple (ADR-8407) | no (syscalls) | `udp_transport_sets_and_reads_back_its_buffers` |
| regia_net | `src/socket/platform/windows.rs` | 3 | Winsock `setsockopt`/`getsockopt` declarations and calls: UDP buffer sizes, read-back and the DSCP mark (ADR-8407) | no (FFI: Winsock) | `udp_transport_sets_and_reads_back_its_buffers` (Windows target) |
| regia_platform | `src/platform/android/accessibility.rs` | 4 | TalkBack bridge: JNI lookup of the view and the AccessKit adapter on it (ADR-3454) | no (FFI: JNI) | none: `cargo xtask android-pass` on a device |
| regia_platform | `src/platform/android/perf_hint.rs` | 10 | `dlopen`/`dlsym` of the NDK performance-hint API and its session calls (ADR-5461) | no (FFI: NDK system libraries) | none: `cargo xtask android-pass` on a device |
| regia_platform | `src/process_memory.rs` | 6 | A zeroed `PROCESS_MEMORY_COUNTERS` and `K32GetProcessMemoryInfo`; macOS `proc_pid_rusage` and `getrusage` for resident and peak memory (ADR-5860; count refreshed 2026-11-12, ADR-7386) | no (FFI: Win32) | none: runs in the Windows pass (`docs/06-testing-and-ci.md` § 8) |
| regia_xess | `src/native.rs` | 42 | XeSS SDK FFI and the wgpu Vulkan handles it is given (ADR-4656; count refreshed 2026-11-18, integ-v06p-finish) | no (FFI to the XeSS SDK: closed source, needs the SDK and a device) | none: the XeSS proof on the Windows laptop |
| regia | `(test code)` | ~14 | Counting allocators (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_animation | `(test code)` | ~14 | Counting allocators; raw-pointer test fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_asset_pipeline | `(test code)` | ~19 | Counting allocators; raw-pointer and zeroed fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_audio | `(test code)` | ~28 | Counting allocators; raw-pointer fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_nav | `(test code)` | ~8 | Counting allocators (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_net | `(test code)` | ~9 | Counting allocators; raw-pointer and zeroed fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_scene | `(test code)` | ~20 | Counting allocators; zeroed fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_test_utils | `(test code)` | ~13 | Test items in `src`: raw-pointer helpers and environment setup (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_ui | `(test code)` | ~8 | Counting allocators; raw-pointer and zeroed fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_vfx | `(test code)` | ~44 | Counting allocators; zeroed fixtures (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_world | `(test code)` | ~6 | Test items in `src`: counting allocators (row added 2026-11-11, ADR-5832) | no (not in the recipe) | none: their asserts count allocations or set up fixtures; they prove no soundness of their own |
| regia_agent | `src/platform/windows.rs` | 21 | The local transport's Win32 named pipe: `CreateNamedPipeW`, `ConnectNamedPipe`, `ReadFile`, `WriteFile`, overlapped I/O and handle closing; the pipe's owner-only security descriptor through `regia_net::private_file` (ADR-4828, ADR-5878; row added 2026-11-12, ADR-7386) | no (FFI: Win32) | none: runs in the Windows pass (`docs/06-testing-and-ci.md` § 8) |
| regia_platform | `src/child_bind.rs` | 6 | Children that end with their parent (ADR-2682): Linux `prctl(PR_SET_PDEATHSIG)` in `pre_exec`; Windows job object with kill-on-close and `AssignProcessToJobObject` (row added 2026-11-12, ADR-7386); Unix `killpg` of a spawned group and the Windows Toolhelp walk that resumes a suspended spawn (count refreshed 2026-11-18) | no (syscalls and FFI: Win32) | none: the editor's play-in-editor children; the Windows half runs in the Windows pass |
| regia_platform | `src/platform/android/sched.rs` | 3 | The pacing trace's scheduler sample (ADR-5946): `getrusage(RUSAGE_THREAD)` and its `assume_init`, `getcpu` (row added 2026-11-12, ADR-7386) | no (syscalls) | none: `cargo xtask android-pass` on a device |
| regia_test_utils | `src/alloc_counter.rs` | 9 | `CountingAlloc`, the one counting `GlobalAlloc` the allocation tests install (ADR-3291): forwards every call to `System` and counts (row added 2026-11-12, ADR-7386) | no (not in the recipe: the forwarding shape of `alloc_cost`) | `a_step_after_settling_fails_on_the_counted_total` |
| regia_terrain | `(test code)` | ~7 | Counting allocators (row added 2026-11-12, ADR-7386) | no (not in the recipe) | none: their asserts count allocations |
| regia_world_geom | `(test code)` | ~46 | `test_support/allocation_counter.rs`, the byte-identical copy of `regia_test_utils`' counting allocator the `allocation-tests` feature includes (ADR-8421; row added 2026-11-18, integ-v06r) | no (not in the recipe) | `the_allocation_counter_is_the_audited_copy` |
| regia_detsim | `src/fpu.rs` | 4 | The FTZ/DAZ guard (ADR-8013): `stmxcsr`/`ldmxcsr` (x86-64) and `mrs`/`msr fpcr` (aarch64) read and reset the sim thread's floating-point control register (row added 2026-11-16) | no (inline assembly) | `ftz_guard_resets_a_set_mxcsr`, `ftz_guard_resets_a_set_fpcr`, `a_default_thread_reports_default_and_keeps_subnormals` |
| regia_detsim | `(test code)` | ~21 | The counting allocators of `tests/hash_alloc.rs` (row added 2026-11-16, ADR-8047; `regia_test_utils::alloc_counter` would pull the renderer into a layer-0a crate's tests), `tests/det_state_reader.rs` (ADR-8479) and `tests/replay.rs` (ADR-8525), which forward every call to `System` and track this thread's live bytes and their peak (the last two folded in from their own rows 2026-11-19: only a `(test code)` row may leave its count unpinned) | no (not in the recipe) | none: its asserts count allocations |
| regia_net | `src/socket_opts.rs` | 4 | UDP SO_RCVBUF, SO_SNDBUF and DSCP EF sockopts (ADR-7998) | no (syscalls) | `buffers_read_back_reported` |
| regia_editor | `src/hot_reload/build_game.rs` | 3 | The Windows job object of a game build (ADR-7340): `CreateJobObjectW` with kill-on-close, `AssignProcessToJobObject`, `CloseHandle` on cancel (row added 2026-11-18, integ-v06p-finish) | no (FFI: Win32) | `cancel_build_kills_the_whole_tree_on_windows` (Windows target), `a_cancelled_build_kills_its_process_and_reports_cancelled` |
| regia_platform | `src/platform/linux/stack_sampling.rs` | 10 | Linux stack sampling for the stack dump (ADR-8158): the unwinder ABI declarations and callback, `gettid`, the sampler signal handler's errno save and restore, `sigaction` install and query, `tgkill`, the worker's signal mask (row added 2026-11-18, integ-v06p-finish) | no (signal handlers and syscalls) | `a_timed_out_handler_owns_the_buffer_across_later_dumps`, `canceled_unclaimed_request_cannot_write_a_later_threads_buffer`, `blocked_signal_times_out_and_a_late_signal_does_not_poison_the_next_capture`, `request_signals_coalesce_without_sampling_and_conflicts_are_refused` |
| regia_platform | `src/platform/windows/alt_menu.rs` | 8 | The native bare-Alt window subclass (ADR-8158): comctl32 subclass declarations and callback, `DefSubclassProc`, `RemoveWindowSubclass`, the owner-thread check, `SetWindowSubclass` (row added 2026-11-18, integ-v06p-finish) | no (FFI: User32 and comctl32) | `native_alt_subclass_swallows_bare_alt_and_preserves_the_message_chain`, `native_bare_alt_keeps_focus_and_redraws` (Windows target) |
| regia_xess | `src/zeroing.rs` | 6 | Zeroing XeSS's temporary memory on the GPU (ADR-7820): a probe buffer for the transfer memory types, a buffer aliased over the allocation, the recorded fill and barrier (row added 2026-11-18, integ-v06p-finish) | no (Vulkan through ash: needs a device) | `optimal_images_over_filled_memory_read_the_fill` (needs a Vulkan device) |
| regia_xess | `(test code)` | ~4 | The zeroing tests' raw Vulkan objects and command recording on a live device (row added 2026-11-18, integ-v06p-finish) | no (needs a device) | none: test fixtures on a live device |
| regia_detsim | `src/platform/linux.rs` | 1 | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`: the sim tick profiler's per-thread CPU clock (ADR-8498; row added 2026-11-19); the value is profiling only, never hashed | no (syscall) | `thread_clock_is_reported` |
| regia_detsim | `src/platform/android.rs` | 1 | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`: the sim tick profiler's per-thread CPU clock (ADR-8498; row added 2026-11-19); the value is profiling only, never hashed | no (syscall) | `thread_clock_is_reported` |
| regia_detsim | `src/platform/macos.rs` | 1 | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`: the sim tick profiler's per-thread CPU clock (ADR-8498; row added 2026-11-19); the value is profiling only, never hashed | no (syscall) | `thread_clock_is_reported` |
| regia_detsim | `src/platform/windows.rs` | 1 | `GetThreadTimes`: the sim tick profiler's per-thread CPU clock (ADR-8498; row added 2026-11-19); the value is profiling only, never hashed | no (syscall) | `thread_clock_is_reported` (on Windows) |
| regia_framework | `src/foot_probe.rs` | 1 | The foot-placement ground probe reads the live `PhysicsWorld` through the address `publish_physics_probe` stored and `clear_physics_probe` clears (ADR-8388; row added 2026-11-19); read only, never written through | no (not in the recipe) | `install_leaves_an_existing_probe`, the foot-planting tests |
| regia_net | `src/discovery.rs` | 15 | LAN discovery sockets that set `SO_REUSEADDR` before `bind` (ADR-7989): `socket`, `setsockopt`, `bind`, `close` and `from_raw_fd` on unix; the WinSock calls, `closesocket`, `from_raw_socket` and `WSAGetLastError` on Windows; the receive-buffer raise of the flood test (row added 2026-11-19) | no (syscalls) | `two_sockets_on_one_port_with_the_same_magic_both_hear_both`, `a_flood_of_valid_announces_is_read_at_most_256_per_poll` |
| regia_net | `src/port_map/platform/apple.rs` | 2 | The default-gateway reader on macOS and iOS: two `sysctl` calls over `CTL_NET`/`PF_ROUTE` (size, then the route dump; ADR-8893; row added 2026-11-21, ADR-10265) | no (syscalls) | `truncated_and_empty_input_find_nothing` (the route-dump parser this reader feeds; the reader itself runs in the Mac pass) |
| regia_net | `src/port_map/platform/linux.rs` | 6 | The default-gateway reader on Linux and Android: an `AF_NETLINK`/`NETLINK_ROUTE` socket (`socket`, `OwnedFd::from_raw_fd`, `bind`, a zeroed `sockaddr_nl`, `sendto` of the `RTM_GETROUTE` dump request, `recv`; ADR-8893; row added 2026-11-21, ADR-10265) | no (syscalls) | `the_route_dump_request_has_the_rtnetlink_layout`, `the_netlink_reader_agrees_with_proc_net_route` |
| regia_net | `src/port_map/platform/windows.rs` | 3 | The default-gateway reader on Windows: the `GetIpForwardTable` declaration and its two calls (size, then the table; ADR-8893; row added 2026-11-21, ADR-10265) | no (FFI: IP Helper) | none: only the Windows pass reaches the reader (`docs/06-testing-and-ci.md` § 8) |
| regia_net_soak | `(test code)` | ~6 | The counting allocator of `tests/scale.rs` (ADR-8454; row added 2026-11-19) | no (not in the recipe) | none: its asserts count allocations |
| regia_platform | `src/platform/local_time.rs` | 2 | A zeroed `tm` and `localtime_r`: the host's UTC offset for wall-clock display on Linux, macOS and Android (ADR-8586) | no (FFI: the C library's time zone rules) | `the_offset_is_within_a_day_and_whole_minutes` |

**Totals on 2026-11-05: 196 sites in shipped code.** Of those, 47 run under Miri in whole or in part. The other 149 are FFI, syscalls, signal handlers, GPU calls, or attributes and promises with no code to run. What that leaves unchecked is listed in ADR-2726.

### Miri subset

Miri checks `unsafe` code for undefined behavior by interpreting it. It needs a nightly toolchain and cannot run system calls such as `mincore`, `process_vm_readv` or a file-backed `mmap` on the target's behalf, so it runs by hand, not in the merge gate. Run on 2026-11-05: 49 tests in 12 commands, no undefined behavior (nightly 1.101, 2d20b42b8):

```
rustup toolchain install nightly --profile minimal -c miri -c rust-src
ANDROID_NDK_HOME=<ndk> scripts/miri-unsafe-subset.sh
cargo +nightly miri test -p regia_test_utils --lib -- alloc_counter
```

Registered as `miri-unsafe-subset` in `docs/ci/nightly-jobs.toml`. Run it after touching those modules or bumping the toolchain. A test added to the subset writes pointers as pointers (`write_unaligned` of the real type), never as integers: Miri tracks provenance, and a function pointer rebuilt from a `usize` fails there.

**Miri needs neither Windows nor Android to run their code.** It interprets the target and links nothing, so `--target x86_64-pc-windows-gnu` and `--target aarch64-linux-android` run from a Linux host. Code that calls a foreign function still stops at the call, so the Windows and Android groups cover plain memory work only. The same fact lets `regia_dlss`'s `table_tests` run on the check stub (`REGIA_DLSS_CHECK_STUB=1`, ADR-2322) with no SDK. Plain memory work split out of a foreign-call block is what makes this useful: `until_nul`, `detail_header`, `empty_memory_status`, `choreographer_from`, `copy_stamp`.

**Opus is in the subset only up to its first decode: Miri stops inside `unsafe-libopus` 0.2.0.** `tests/opus_boundary.rs` creates and destroys a decoder's state, which passes with `-Zmiri-disable-stacked-borrows`; with Stacked Borrows or Tree Borrows on, Miri stops already in `opus_multistream_decoder_create` (`celt_decoder.rs:1500` and `:1510`). `regia_media/tests/voice_opus.rs` has small tests of the codec boundary (`the_library_boundary_*`, `the_decoder_boundary_*`), and Miri rejects the library on each path it reaches them by (nightly 1.101, 3e92e1a2d, 2026-11-02):

| Miri flags | Where it stops | What the library does |
|---|---|---|
| default (Stacked Borrows) | `opus_encoder_create` → `celt_encoder.rs:3419` | A `memset` from a pointer to the `rng` field clears the rest of the state, past that field |
| `-Zmiri-tree-borrows` | `opus_encoder_create` → `celt_encoder.rs:3429` | A reborrow into the state through a pointer that `memset` invalidated |
| `-Zmiri-disable-stacked-borrows` | Encode: `opus_encoder.rs:1752`. Decode: `silk/dec_API.rs:431` | `0 * a.offset_from(b)` on pointers to different allocations, in the size of a translated copy. This one is undefined under any aliasing model |

Miri stops at the first error, so what follows on each path is unchecked. The library has about 50 of those `0 * .. offset_from` expressions. None of this is in Regia's wrappers, and it cannot be fixed from here without changing the dependency (ADR-2086).

AddressSanitizer checks what Miri could not reach, which is real out-of-bounds reads and writes in the running code:

```
RUSTFLAGS="-Zsanitizer=address --cfg sanitizer_compat" cargo +nightly test -p regia_media --features voice-opus --test voice_opus --target x86_64-unknown-linux-gnu
RUSTFLAGS="-Zsanitizer=address --cfg sanitizer_compat" ASAN_OPTIONS=detect_leaks=0 cargo +nightly test -p regia_media --features opus --test codecs --target x86_64-unknown-linux-gnu
RUSTFLAGS="-Zsanitizer=address --cfg sanitizer_compat" ASAN_OPTIONS=detect_leaks=0 cargo +nightly test -p regia_media --features video --test fuzz --target x86_64-unknown-linux-gnu
```

`--cfg sanitizer_compat` is what makes these runs give a real result. Before ADR-2726 every run aborted with `AddressSanitizer: Joining already joined thread` and reported nothing about the tests; the cause is `crash-handler`'s `pthread_create` (see ThreadSanitizer, below). Clean on 2026-11-05, nightly 1.101 (2d20b42b8): `voice_opus` 22 tests with the leak check on, `codecs` 3 tests, and the WebM fuzz corpus (`fuzz` 3 tests) through `rav1d` and the demuxer. The two commands with `detect_leaks=0` have not had a leak check. These inputs caused no bad memory access in this build; that is all a sanitizer run says.

**`rav1d` under Miri stops before it decodes.** `cargo +nightly miri test -p regia_media --features av1 --test codecs -- av1` stops at the first worker thread's park, in `parking_lot_core` 0.9.12 (`thread_parker/linux.rs:112`): current Miri refuses `&AtomicI32` as the argument of the variadic `syscall(SYS_futex, ..)`, where it wants `*mut u32`. That is Miri's strict check of a C call, not a bad memory access, and not in `rav1d`. It does mean no test that parks on a `parking_lot` lock can run under Miri on this nightly, so AddressSanitizer on the fuzz corpus (above) is the check `rav1d` has.

### ThreadSanitizer

ThreadSanitizer finds data races and lock-order problems in the running code. Like Miri it needs a nightly toolchain, and it runs by hand:

```
rustup toolchain install nightly --profile minimal -c rust-src
scripts/tsan-subset.sh <log directory> [group...]
```

The script sets `RUSTFLAGS="-Zsanitizer=thread --cfg sanitizer_compat"` and builds with `-Zbuild-std --target x86_64-unknown-linux-gnu`, so `std` is instrumented too. Registered as `tsan-subset` in `docs/ci/nightly-jobs.toml`. The groups are CPU-only test binaries: `regia_core`, the telemetry writer, the `regia_ecs` and `regia_app` schedule, the asset server, the platform exit and console-ctrl paths, and the agent job queue.

**`--cfg sanitizer_compat` is not optional, in any sanitizer recipe of this workspace.** `crash-handler` defines its own `pthread_create` (to give every thread an alternate signal stack) and reaches libc's through `dlsym(RTLD_NEXT)`, past the sanitizer's interceptor, so the sanitizer never learns of the thread. Without the cfg, ThreadSanitizer faults on a null thread state at the first thread, and AddressSanitizer aborts with `Joining already joined thread`. The cfg is that crate's documented switch (its README).

A clean run is checked against a planted race: a temporary test with two threads writing one address, linked with `regia_platform` so the interposer is in the binary, is reported as a `data race` and exits with status 66. A run where a report appears exits with 66 too (`TSAN_OPTIONS=exitcode=66`), even if every test passed.

**Triage.** A report in Regia's own code is a real race until shown otherwise: fix it, or report it to the lane that owns the code. A report that is not a race in Regia (a dependency's fence the tool cannot see, an uninstrumented C library) goes in `docs/ci/tsan-suppressions.txt`, with its reason on the comment line above; the policy test fails on an entry with no reason.

**Run of 2026-11-05** (nightly 1.101, 2d20b42b8; 16 threads under a CPU cap, no GPU; base integ/v035). No report in any group, so the suppressions file has no entries yet:

| Group | Tests passed | Reports | Do threads overlap in it? |
|---|---|---|---|
| `core` | 204 | 0 | Yes: scoped parallel chunks, the residency keeper |
| `telemetry` | 110 | 0 | Yes: the trace writer thread and its senders |
| `schedule_ecs` | 35 | 0 | Barely: `regia_ecs`'s tests mostly run one thread |
| `schedule_app` | 126 | 0 | Yes, shown by a temporary test: its systems ran on 5 threads, over 200 updates |
| `asset` | 456 | 0 | Yes: load sets, parallel block decompression, hot reload, pak verify |
| `platform` | 31 | 0 | Yes: the exit paths' workers and the GPU worker registry. The console-ctrl flags are plain atomics; the handler itself is Windows-only and did not run |
| `agent_jobs` | 7 | 0 | **No: these tests never start a thread.** A temporary test with 16 workers reporting while the protocol thread polled also showed no report; it is not in the tree |

A clean run covers the schedules that happened, not all of them, and C libraries in a test binary are not instrumented. The `core`, `telemetry`, `schedule_app`, `asset`, `platform` and `schedule_ecs` tests also ran five more times each with `--test-threads=16`: 30 runs, no report.

## 15. Measured code coverage (`cargo xtask coverage`, ADR-2705)

Test counts do not say what the tests run. `cargo xtask coverage --crate regia_net --crate regia_world [--json out.json] [--lowest 20]` runs the crates' tests instrumented through `cargo llvm-cov test` and prints per-crate line and function coverage plus the lowest-covered **public** functions (the export's function table joined with the `pub fn` items of a source scan, so private helpers do not crowd the list; never-run functions first, larger ones first).

Setup, once per machine or lane, with a private toolchain home so the shared `~/.rustup` is untouched:

```
export RUSTUP_HOME=<lane>/rustup CARGO_HOME=<lane>/cargo-home
rustup toolchain install 1.99.0 --profile minimal -c llvm-tools-preview
cargo install cargo-llvm-cov --locked --root <lane>/tools      # xtask finds <repo>/tools/bin or --tools <dir>
```

Instrumented builds do not share artifacts with normal ones: the target directory is `<repo>/target-coverage` (`--target-dir` to move it). A run is as long as a cold build of the crates and their dependencies, so it is manual (`coverage-measure` in `docs/ci/nightly-jobs.toml`), never part of the merge gate. `--from-export <file>` re-reads a saved `cargo llvm-cov --json` export without running tests. Coverage shows what ran, not what was asserted: a test that calls a function and checks nothing raises the number, so read the tests the number points at.

## 16. Spec 27 § 2.2 calibration suite (ADR-2660)

One hand-run suite scores every `docs/specs/27-renderer-excellence.md` § 2.2 row against the reference path tracer on scenes a lane
can build (no fetched or DO27 content): `crates/regia_pt/tests/spec27_calibration.rs`. Every test in it is `#[ignore]`d, so the gate
does not run it; a run takes about an hour on lavapipe. Each `spec27_*` test records its rows (target, measured, verdict, scene, spp)
to `$REGIA_SPEC27_OUT/rows/<id>.tsv` without asserting the target; `spec27_write_report` adds the rows of the older tests that own
them (from their logs under `$REGIA_SPEC27_OUT/logs/`) and writes `$REGIA_SPEC27_OUT/report.md`, which is committed as
`docs/evidence/spec27/<date>-<adapter>.md`. A row that fails also has a `target_*` test asserting the target, ignored with the reason
and the size of the miss; it is the regression test the fixing lane un-ignores.

```sh
export REGIA_SPEC27_OUT=/tmp/spec27 REGIA_SPEC27_COMMIT=$(git rev-parse --short HEAD); mkdir -p $REGIA_SPEC27_OUT/logs
for t in gi_validation sssr_specular aerial_parity cloud_field; do
  cargo test -p regia_pt --test $t -- --nocapture --test-threads=1 > $REGIA_SPEC27_OUT/logs/$t.log 2>&1
done
cargo test -p regia_pt --test spec27_calibration -- --ignored --nocapture --test-threads=1 --skip spec27_write_report --skip target_
cargo test -p regia_pt --test spec27_calibration -- --ignored --nocapture --exact spec27_write_report
cargo test -p regia_pt --test spec27_calibration -- --ignored target_   # the failing rows' named tests (expected red)
```

On lavapipe set the lane env (`VK_ICD_FILENAMES` to `lvp_icd.json`); on a card, pin it (`REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1`)
or queue it through the GPU job supervisor (§ 12); the commands for a card run are in `book/agents/spec27-calibration.md`. A ★ in
§ 2.2 is replaced only where a run supports the number, with the run's evidence page cited; a target is never loosened to make a
row pass (AGENTS.md rule 4).

## 17. Frame work ratchet (ADR-2711)

GPU budgets (§ 4, `specs/27` § 2.5, master plan § 5.1a) need a card. The frame work ratchet catches the part of a
cost regression that is countable on any device: `crates/regia_render/tests/frame_work_ratchet.rs` draws four
fixed synthetic scenes (a forward box, a 10,000-object field, a 128² terrain lab heightfield, a closed GI room) at
every shipped preset at 160 × 90 on lavapipe and compares each row's counts (`regia_render::FrameWork`: render and
compute passes, dispatches and workgroups, draws and indirect draws, triangles, instances, pipeline and bind-group
sets, submits, creations and their bytes, uploads, heap allocations) with the committed table
`crates/regia_render/tests/data/frame_work.ron`. Each row has the 4 warm-up frames (what the renderer allocated to
draw the scene at that size) and 2 steady frames.

- **Fails**: a count more than 10 % above its row. The message names the row (`scene/preset/WxH warm|steady`), the
  count, the measured and recorded values, and the command below.
- **Notes**: a count more than 10 % under its row prints a note; re-record to tighten the table.
- **Adapter**: only the CPU reference adapter (lavapipe) compares; any other adapter prints its counts.
- **Scope**: shipped presets only, so another lane's opt-in feature does not move it. One encode thread, so binds
  per pass do not depend on the host's core count. The same run proves the ratchet bites (a test-only extra pass
  with its own full-resolution target must fail `forward_box/MobileLow`) and counts the VRAM rows (the forward box
  at each `BudgetTier` resolution against the tier's VRAM budget; `docs/perf-log.md`, counted table).
- **Not a timing**: two frames with equal counts can still differ on a card; the card sweep stays the record for
  time. The slack is per count: in a preset with 30 passes, one cheap extra pass is 3 % and passes on the pass count,
  and is caught by what it adds elsewhere (target bytes, draws, workgroups), or not at all.
- **Run time**: one test, 27 rows plus the extra-pass check: 124 s on lavapipe on a 32-thread host with a warm build
  (the test machine, 2026-11-05), about 7 to 9 s a case on a loaded farm host. Each case takes 3 to 6.5 s there, the
  VRAM rows at full tier resolution included: the time does not grow with the target size.
- **Drivers**: the counts do not depend on the lavapipe build. One tree counted on LLVM 19.1.7 and on LLVM 23.1.4
  gave the same numbers, warm heap allocations within 6 of 1.59 million.

Re-record only after a justified change of the default path (a new default pass, a resized target, a preset change
that is meant), on lavapipe, and give the reason in the commit message, as for a golden image:

```
REGIA_FRAME_WORK_RECORD=1 cargo test -p regia_render --test frame_work_ratchet -- --nocapture
```

Do not re-record to absorb growth nobody can explain, and do not re-record from a hardware adapter (the test
refuses). A frame pass landing the counted wrappers in frame files (the frame lane's patch) re-records once, with
that reason. New raw `wgpu` encoder calls in renderer modules are not counted until
`<scratch>` (or a hand edit to the `counted_*` wrappers) covers
them.

The hand-run CPU encode bench for the § 5.1a "Render encode, 10k draws" row prints and asserts nothing:

```
cargo test --release -p regia_render --test frame_work_ratchet -- --ignored --nocapture encode_bench
```

On a card, add `REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1` and drop the lavapipe loader variables.
The same recipe with `--features opus --test codecs` covers `regia_media::opus`. Clean on 2026-11-02: all 22 `voice_opus` tests and the 3 `opus` tests of `codecs`.

The WebM corpus gets its ASan run through the `decode` cargo-fuzz target instead of `--features video --test fuzz` (`book/agents/fuzzing.md`, ADR-2723): it found one bug on 2026-11-05, a Vorbis comment count that asked for 16 GB from a 7 KB file (fixed in `vendor/lewton`, patches 5 and 6). After the fix, two workers for 22 minutes each ran 39,129 executions over the WebM seeds, leak checks on, with no report. Findings are kept in `crates/regia_media/fuzz/findings/decode/` and replay in `tests/fuzz.rs`.

## 18. Start-up and idle-memory ratchet (ADR-2987)

The frame-work ratchet counts what one frame does. `crates/regia/tests/startup_idle_ratchet.rs` counts the two costs
it cannot see, on lavapipe: the work an app does from its start to its first presented frame, and the heap each engine
subsystem holds after 120 idle frames.

- **What runs:** four plugin sets, each in a fresh child process of the test binary (a cold start, not a process that
  already built a renderer): `hello_window` (`DefaultPlugins`, which is also the `blank` template), the
  `first_person` template's engine plugins, the `third_person`/`top_down` engine plugins, and `DefaultPlugins` with
  every optional engine plugin that needs no hardware or network peer. Each has one 320x180 texture camera over a floor
  quad, the headless stand-in for a window. The start-up window ends at the first rendered texture camera
  (`app.startup_work`'s own snapshot). The tagging allocator is installed in the binary
  (`regia_test_utils::install_tagging_allocator!`).
- **What is compared:** pipelines created, live GPU objects (shader modules, bind group layouts, pipelines, buffers,
  textures, ...), tracked GPU creations and upload bytes, VFS and asset-server files and bytes, heap allocations,
  bytes and peak (start-up), and `idle_heap_live_<subsystem>`, the idle total and the allocations in the idle window,
  and `idle_heap_owner_render.<owner>` for each named renderer heap owner (ADR-3351; 0 for one that never allocated).
  The table is `crates/regia/tests/data/startup_idle.ron` (`format_version` 1, test data).
- **Determinism:** every case runs twice with pinned pool and lavapipe thread counts. Counts must be equal; heap
  figures, which other threads' allocations shift slightly, must agree within 5 % and the larger is compared. The idle
  heap is the per-subsystem minimum over the last 60 of 120 frames, so a worker's transient allocation does not
  count. A figure that depends on thread timing is listed in the test's `EXCLUDED` with its reason and printed, not
  compared (`heap_peak_bytes`, `threads_live`, `tracked_submits`, `idle_heap_transient_bytes`).
- **Verdict:** more than 10 % above a row fails and names the row; more than 10 % under prints a note asking to lower
  the row, so the table only tightens. Only the CPU reference adapter compares; on a card the figures are printed.
- **When to re-record:** only for an intended change in start-up work or idle memory (a feature that has to compile
  more at start-up, a cache that is meant to stay), with the reason in the commit message:
  `REGIA_STARTUP_IDLE_RECORD=1 cargo test -p regia --test startup_idle_ratchet`. A growth nobody intended is a bug to fix,
  not a row to raise.
- **Agent side:** `app.startup_work` and `memory.idle_ledger { frames }` answer the same figures on a running app
  (`book/agents/startup-idle.md`).

## 19. Toolchain canary (ADR-3388)

The merge gate pins Rust 1.99 (`clippy -D warnings --all-targets`, rustdoc `-D warnings`). Before a newer
toolchain becomes the gate, a canary checks the workspace builds clean on it, so the gate toolchain is never
a surprise. It is a manual per-tag job (`toolchain-canary` in `docs/ci/nightly-jobs.toml`), not part of the
merge gate.

Setup, once per machine or lane, in a private toolchain home (the shared `~/.rustup` is untouched):

```
export RUSTUP_HOME=<lane>/rustup CARGO_HOME=<lane>/cargo
rustup toolchain install stable beta nightly --profile minimal -c clippy
```

Then, from the workspace root, one `CARGO_TARGET_DIR` per toolchain (the Android check runs on the workstation
through `the workstation-build.sh`, the Windows check with the `x86_64-pc-windows-gnu` rustup target added to the
same private home):

```
CARGO_TARGET_DIR=<lane>/target-stable  RUSTUP_TOOLCHAIN=stable  cargo check --workspace --all-targets
CARGO_TARGET_DIR=<lane>/target-stable  RUSTUP_TOOLCHAIN=stable  cargo clippy --workspace --all-targets -- -D warnings
CARGO_TARGET_DIR=<lane>/target-beta     RUSTUP_TOOLCHAIN=beta     cargo check --workspace --all-targets
CARGO_TARGET_DIR=<lane>/target-beta     RUSTUP_TOOLCHAIN=beta     cargo clippy --workspace --all-targets -- -D warnings
CARGO_TARGET_DIR=<lane>/target-nightly RUSTUP_TOOLCHAIN=nightly cargo check --workspace --all-targets
CARGO_TARGET_DIR=<lane>/target-nightly RUSTUP_TOOLCHAIN=nightly cargo clippy --workspace --all-targets -- -D warnings
cargo report future-incompatibilities   # per toolchain, after a build on it
ANDROID_NDK_HOME=<ndk> cargo ndk -t arm64-v8a check -p regia                 # the workstation
CARGO_TARGET_DIR=<lane>/target-win RUSTUP_TOOLCHAIN=stable cargo check --target x86_64-pc-windows-gnu -p regia
```

Last canary (2026-11-07, stable 1.99.1, beta 1.100.0-beta.12, nightly 1.101.0-nightly 2026-11-06; the
fixes of ADR-3388 applied): check and `clippy -D warnings` are clean on stable and beta; the Android and
Windows cross-checks pass on stable. Nightly compiles with zero errors and `cargo report
future-incompatibilities` reports none, but its trait solver warns "overflow evaluating the requirement
... well-formed / : Sync / : Send" at bevy-style system-param definitions (`Option<Res<T>>` parameters,
`#[derive(Resource)]` handles) across `regia_render`, `regia_ui`, `regia_terrain`,
`regia_vfx`, `regia_2d`, `regia_media`, `regia_matgraph`, `regia_xr`, `regia_quick`, `regia_procgen`,
`regia_framework`, `regia_telemetry`, `regia_agent` and five samples — 204 warnings across 28 packages in
all, most of them integration tests and examples that build an `App`; first seen on nightly 2026-11-03 by
the fuzz-guided lane. Beta 1.100 does not emit them, so the change is nightly-only today. They are warnings
only — every build succeeds — and they are a solver recursion change, not a code defect: the auto-trait
evaluation of the same types exceeds the default recursion limit, and a full-workspace run with
`RUSTFLAGS='-Zcrate-attr=recursion_limit="256"'` brings the count to zero. They are deliberately not
fixed file by file: 204 attributes across 28 packages (including every affected integration-test crate,
each of which is its own crate root) is churn with a large merge surface, for a diagnostic an experimental
solver may revise before it ever reaches stable. A future stable that turns them hard either ships with
the recursion accounting fixed or takes the `recursion_limit` bump crate by crate (the canary names the
sites); the canary job re-measures each cycle. Do not add `#[allow]`s for them before that toolchain is
the gate.

## 20. Mutation testing (`cargo xtask mutants`, ADR-3389)

Coverage (§ 15) says a line ran; it does not say a test would notice the line being wrong. Mutation testing changes the
code one small edit at a time (a function returns a default, `<` becomes `<=`, a `!` or a match arm is deleted), runs the
crate's tests against each change, and lists the changes no test noticed. `cargo xtask mutants` wraps `cargo mutants`:

```
cargo xtask mutants --crate regia_core --file 'crates/regia_core/src/math/geom/*.rs' [--file <glob>]... [--exclude <glob>]
                    [--json <out>] [--timeout-mult N] [--jobs N] [--build-threads N] [--iterate] [--doctests]
cargo xtask mutants --crate regia_core --from-outcomes target/mutants/mutants.out/outcomes.json   # re-read a run
```

It prints per crate and per file the caught, missed, timeout, unviable and skipped counts with the **mutation score**
`caught / (caught + missed)` (a timeout, where the mutant hung the tests, and an unviable mutant, which did not build,
are counted apart), then every missed mutant as `file:line: mutation`. `--json` writes the same summary (`version` 1).

- **Install**, once per lane, lane-private (it is a tool, not a dependency; no `Cargo.toml` changes):
  `CARGO_INSTALL_ROOT=<repo>/tools cargo install cargo-mutants --locked`. The command looks in `<repo>/tools/bin`, then
  `--tools <dir>`, then `PATH`. Measured with cargo-mutants 27.1.0.
- **Own directories**: cargo-mutants copies the source tree once per job and builds in the copies. The copies
  (`TMPDIR`) and `mutants.out` live under `<repo>/target/mutants` (`--target-dir` to move it). Inside the workspace the
  directory must be under `target/`, which cargo-mutants never copies: anywhere else it would copy the tree into itself
  (the command refuses it). The source tree is never mutated in place, and the command fails if a `.rs` file of the named crates changed during the run.
- **Host limits**: `--jobs` (default 4) is capped at 6, `--build-threads` (default 8) caps the compiler processes of all
  jobs together, and the run refuses to start under 45 GiB `MemAvailable` (`--min-mem-gb`). On the test machine run it in
  one build slot (`~/bin/rig-slot.sh cargo xtask mutants ...`). It is CPU only.
- **Timeouts**: each mutant's tests get `--timeout-mult` (default 5) times the unmutated test time, never under 60 s.
- **Tests run**: the crate's unit and integration tests (`cargo test --lib --tests`); `--doctests` adds doc tests, at
  the cost of compiling each one again for every mutant.
- **Skip list** `docs/ci/mutants-skip.toml`: mutants that are intentionally untested (a `Debug` or `Display` body, a
  log line, a `tracing` span name), each entry with a `reason`. They are left out of the run (an exact-name `-E` regex
  built from `--list --json`) and counted as skipped. An entry that matches nothing in a file the run covered is a
  warning. A missed mutant in real logic gets a test, not an entry.
- **Iterating**: `--iterate` reruns only the mutants the last run in the same target directory did not catch and
  merges both runs' outcomes, so the printed score is the score after the new tests. `--from-outcomes` is repeatable
  for the same merge by hand (a later file wins for the same mutant).

A run of 1,300 mutants of `regia_core` takes about 1.5 hours at 4 jobs on the test machine; it is manual
(`mutants-core` in `docs/ci/nightly-jobs.toml`), never part of the merge gate. Results and the remaining missed list
of the first run are in `book/agents/mutation-testing.md`. The agent page has how to read a missed mutant and turn it
into a test.

## 21. Agent protocol snapshot (the agent-surface ratchet, ADR-3335)

`cargo xtask agent-snapshot [--update|--check]` holds the agent protocol surface — what a protocol client sees —
against the checked-in `docs/api-snapshots/agent-protocol.txt`, the agent-protocol counterpart of the Rust API
snapshots of § "API policy" (`docs/api-review.md`) and `api-snapshot` (ADR-1942). One block per method: name,
permission class, MCP tool name, `mutating` flag, cost class, dispatcher-enforced limits, params schema (kinds,
required flags and the structured JSON Schema shapes), result schema and documented error codes; plus the built-in
extension methods and the universal error codes. It reads `regia_agent::schema::METHODS`,
`contract::CONTRACTS` / `EXTENSION_CONTRACTS`, the `AgentExtension` schemas of `install_builtin_extensions` and
`UNIVERSAL_ERRORS` — the same tables `agent-coverage` reads — so it runs in seconds and never builds the renderer.
It is a step of `preflight` (after `api-snapshot`, with preflight's `--base` forwarded) and of the CI `linux` job.

| Change | Result |
|---|---|
| A method or param removed; a param type, shape or the result schema changed; a new required param; a permission, MCP tool, `mutating` or cost class changed; a limit tightened; an error code removed or renamed | **fail** (`breaking`) unless a release fragment the branch touches (`docs/releases/*.md`) names the method under `## Upgrade` |
| A new method, a new optional param, a new error code, a param becoming optional, a limit relaxed or removed, a new universal code | passes only with a `## New` line naming the method in a touched fragment |
| Any diff, even fully documented | **fail** until `cargo xtask agent-snapshot --update` refreshes the snapshot in the same branch; `--update` itself refuses to write a change no touched fragment documents |

The failure message names the method and the fix. The classification is in
`xtask/src/agent_snapshot.rs` (`diff`); unit tests there cover every breaking and additive class against synthetic
before/after surfaces. `book/agents/protocol.md` ("Stability") documents the rule for protocol clients.

## 22. The gate's real-GPU pass on the test machine (`tools/gaterig/`, ADR-3317)

the build server's RTX 5090s serve a model server, so the merge gate's real-GPU pass can run on the test machine's RTX 5080
(<lan-host>, card `GPU-551a5545-dccc-f510-ce21-9e4a928d7d58`) while the lavapipe pass stays on the build server.

- **Switch.** `GATE_GPU_HOST=the test machine` before `the gate script` (or `start-gate-fast.sh`, which passes the environment on).
  The default is `the build server`; any other value refuses the gate. `GATE_GPU_CARD` is ignored on the rig.
- **What it runs.** `gaterig/the gate script --rev SHA` stages `git archive SHA`, the Khronos samples
  (`.regia/test-assets`, the legacy `.ferrite` spelling as a fallback; a checkout with neither fails the pass, never a
  silently uncovered one) and the gate's runner files in `<build-dir>>`, then on the rig: syncs
  them into `<build-dir>` (`rsync -c`, warm target in `<build-dir>`), builds
  `cargo test --workspace --no-run` (nice 5, 16 jobs, waits up to `GATE_RIG_MEM_WAIT_S` (default 3600 s) for 24 GB
  MemAvailable, then `GPU PASS SKIPPED`), spools every launch, and runs the rendering crates' device-creating binaries
  with the package list, hang limit and quarantine list of the staged `the gate script` (captured before `eval`, so a
  drifted file fails the pass instead of running with unset variables), 4 at a time, failures and hangs re-run alone.
  The build uses the staged tree's pinned toolchain (`rust-toolchain.toml`, rustup shim fallback; `GATERIG_TC` pins one
  explicitly), so a pin bump moves the rig like every other host. Output: the build server pass's lines
  (`gpu tests passed=N failed=M binaries=B`, `HANG`, `FLAKY UNDER LOAD`, `FAILED`, then `GPU-OK` or `GPU TEST FAIL`) in
  `logs/gate/gpu.out`; per-binary logs on the rig in `<build-dir>`.
- **Samples and plates (parity with the build server pass).** The pass exports `REGIA_KHRONOS_ASSETS` (both spellings)
  pointing at the staged samples and `REGIA_REQUIRE_ASSETS=1`, so a sample test can never skip on this pass, and
  `REGIA_PLATE_DIR` (`gate-logs/plates`), so every skipped golden compare still records its PSNR plate. After the run,
  `cargo xtask plate-floors check --plates ... --vendor nvidia --partial` (the built `xtask`, the staged
  `docs/ci/plate-floors.toml`) fails the pass on exit 1, exactly as the build server pass does (ADR-2089); a build without
  the checker or the floors file does not gate.
- **Stale passes.** A pass killed from the workstation leaves its rig side running: the next pass stops the recorded previous
  pass PID and any process in the tree that belongs to a pass (cargo, rustc, the runner, the spooler, a test binary of
  this target) — never a shell or editor someone opened in the tree — and waits for them to exit before
  `rsync --delete`.
- **The card.** Pinned by UUID with `REGIA_GPU_STRICT=1`, NVIDIA ICD only, under `/tmp/regia-gpu-0.lock` and then `/tmp/ferrite-gpu-0.lock` on the rig. A
  gate announces itself in `/tmp/ferrite-gpu-0.gate-pending` as `PID start-time`; lane runs wait while that exact
  process lives (a start-time mismatch means the PID was recycled after a SIGKILL) and give up with exit 3 after
  `GATERIG_GATE_WAIT_S` (default 14400 s). If another process has the card, the pass waits up to 10 minutes and then
  runs and prints `WARNING: the card is shared during this pass`.
- **Card-use guard.** During the pass the card's compute processes are sampled every 5 s; the output says how many
  distinct test binaries were seen on the card (`card use: N ...`, list in `gate-logs/card-procs.txt`), and N = 0
  fails the pass, since few tests print their adapter and a lavapipe fallback would otherwise pass unnoticed.
- **Adapter-loss guard (ADR-5794).** A test that cannot open the pinned card gets `GPU selection failed`, prints a
  skip line and returns as passed, so the counts hide it; and the test harness prints a passing test's own output
  nowhere, so that skip line is not in the log either. The engine therefore writes a refused strict selection
  (`REGIA_GPU_STRICT`) to the real stderr itself, past the harness's capture. After the run, any per-binary log with
  that line fails the pass and names the logs. Lane card runs do the same by hand: `grep -l "GPU selection failed"
  <logs>` must print nothing before a "passed" from a card is believed. On a build older than this fix the line
  shows only with `RUST_TEST_NOCAPTURE=1` (or for a failed test). (The known cause, a driver the Vulkan loader could
  not load again after about 100 reloads in one process, is fixed in the engine; the guard is for the next one.)
- **Card runs take the gate's driver filter.** A lane card run on the build server sets `VK_LOADER_DRIVERS_SELECT=*nvidia*`
  beside `REGIA_GPU_UUID` and `REGIA_GPU_STRICT=1`, as this pass does (`rig-card.sh` sets it on the rig). With every
  installed driver, each new instance makes the loader unload the drivers that report no device, which is one half of
  the loader list race of ADR-2113: a parallel test binary can die with a fault in `libvulkan.so` (`journalctl -k`
  names it; the build server's log has dozens from lane runs). The filter leaves the loader nothing to unload. On the Arc
  the filter is `*intel_icd*` (ANV alone; the GPU pre-gate and gpusup use it): `*intel*` also loads
  `intel_hasvk_icd.json`, which has no device on the Arc, so the race comes back (ADR-3286, ADR-7334).
- **A GPU-only pass** (no gate lock, no the build server): `bash tools/gaterig/the gate script --rev <sha>` from the workstation.
  Measured on e374452f1 (2026-11-06): cold build 799 s, pass 2157 s; warm build 8 s, pass 922 s; 8948 passed, 490
  binaries, the 5090 pass's list test by test. `python3 tools/gaterig/compare-runs.py <logsA> <logsB> --names a,b`
  compares two passes' `gpu-tests` directories test by test (exit 1 when the pass/fail lists differ).
- **Disk.** The warm tree is kept between gates: 284 GB of target and 0.8 GB of source on 2026-11-06 (`/models` had
  1.67 TB free). Deleting `<build-dir>` costs one cold build (about 13 minutes), nothing else.
- **Lane card tests on the 5080:**

```
ssh <host> 'cd <build-dir>>/src && ~/bin/rig-slot.sh --card the test machine --timeout 600 \
  cargo test -p regia_render --test gpu_select'
```

  `cargo test|bench` builds first with `--no-run` (`cargo run`: `cargo build`) in a build slot, without the card lock;
  then the command runs pinned to the card under its lock, nice 10, at most 1800 s (default 900; exit 124 when over).
  `REGIA_GPU_LOCK_HELD=0` is set, so `cargo xtask gpu-bench` under it does not wait on its own lock. `--dry-run`
  prints the build and run commands and takes no lock.
- **Install and check:** `bash tools/gaterig/deploy.sh` (selftest, then the rig's `~/bin` and the workstation's
  `maintainer-scripts/gaterig` plus the the gate script switch). The rig's `rig-slot.sh` gets the `--card` option as a
  checked patch (`rig-slot.sh.patch`), refused when the live file drifted — a rig-side edit is never silently reverted.
  Sibling gate scripts (`fgate2.sh`, `fgate3.sh`, stale copies without the switch) get a guard refusing
  `GATE_GPU_HOST=the test machine`, so a gate launched through one cannot silently run the build server 5090 pass instead.
  `bash tools/gaterig/selftest.sh` alone needs no GPU (22 checks).

## 23. Shader gate (`cargo xtask check-shaders`, ADR-2644)

`cargo xtask check-shaders` runs on Linux with lavapipe and `spirv-val`:

1. Every `regia_render` module of `shader_check::shader_sources` through naga, the Function/Private explicit-layout
   scan and `spirv-val` (ADR-026, ADR-135), then created with the pipelines of the lab, Desktop High and Mobile High
   frames under Vulkan validation.
2. Every crate's plain shaders outside `regia_render` (`xtask::dx12_shaders::plain_sources`: terrain, grass, water, UI,
   VFX, 2D, XR, lookdev, the path tracer's denoiser, the particle liquid) as modules under validation; then those and
   the composed path tracer and lightmap baker through naga's HLSL writer (Shader Model 5.1 and 6.0, the Direct3D 12
   limits, FXC's rules, ADR-367) and naga's MSL writer with wgpu's Metal options
   (`xtask::dx12_shaders::msl_problems`). The test `every_entry_shader_of_the_engine_crates_is_checked` fails when a
   `.wgsl` file with an entry point reaches none of these lists or the matrix's inputs.
3. With `--matrix`: the **shader permutation matrix**. Every forward permutation of every preset of every shipped
   platform (`linux`, `windows`, `macos`, `android`; the lite tiers, the CPU, GPU-driven and instanced paths, both
   output encodings, the studio axis, the three material binding tiers, glass, mask decals, thin film, lightmaps,
   subsurface, OIT, the traced sun on desktop, the Adreno workaround on Android), every pass of every material-graph
   set `permutation_policy::enumerate_profile` lists for the sample graphs, the engine list and the modules the
   renderer builds beyond it (`shader_hlsl::extra_engine_modules`: DDGI, baked GI, the display stage, compute
   skinning and the rest; those the renderer hands wgpu as plain WGSL are parsed as plain WGSL, not composed).
   A module with a ray query is written as Shader Model 6.5 and its `hlsl_sm51` is `skipped` (wgpu compiles ray
   queries with DXC only). Identical
   compositions are checked once: each composed module is written back to WGSL and hashed, and the report lists every
   define set and origin per module. Each distinct module runs `compose`, `naga`, `layout_scan`, `spirv_val`,
   `vulkan_pipeline` (a pipeline per entry point with layouts built from the module), `hlsl_sm6`, `hlsl_sm51` (with the
   Shader Model 6 text scan and the FXC rules) and `msl`. The quad frame of every shipped preset and the lab frame are
   rendered under validation too (the frame's own layouts). Any failure is exit code 1, named by file, defines and
   backend.

Measured 2026-11-05 on the build server's lavapipe (6 workers, load about 180 to 210), after ADR-2644 (6): 3986 inputs merge
into 462 compositions, and naga's WGSL re-emit gives 460 distinct modules; the matrix took 37 minutes at that load. Every
module composes and passes naga, `spirv-val`, both HLSL models and MSL; the seven frames pass. 4 modules fail the layout
scan (the two baked GI modules and the two ray-query modules, listed under Known issues in
`docs/releases/v0.1.28-shader-validation-matrix.md`), so `--matrix` is not yet a passing gate.

Flags: `--matrix`, `--subset` (the fixed subset of the default test `shader_check::matrix::tests`), `--no-pipelines`
(no device: the CPU backends only), `--json` (the report on stdout, every other line on stderr), `--json-out FILE`.
`REGIA_SHADER_MATRIX_JOBS` sets the worker threads (default the available parallelism, at most 6).
The texture-table modules (`TEXTURE_TABLE`, bindless level 3) need a device with binding arrays, which a default device
does not request: their `vulkan_pipeline` is `skipped` (`device lacks TEXTURE_BINDING_ARRAY`). A second run with
`REGIA_BINDLESS_LEVEL=3 cargo xtask check-shaders --matrix` asks lavapipe for them and builds those pipelines too;
run both before a shader change merges.

The JSON report (`ShaderMatrixReport`, `format_version` 1) is a tool report, not a content format:

```json
{
  "format_version": 1, "scope": "full", "inputs": 3986, "compositions": 460,
  "modules": [
    { "file": "lab_forward.wgsl", "hash": "3f0c…", "ok": true,
      "permutations": [ { "defines": ["ALPHA_TEST", "STUDIO_IBL_OFF", "THIN_FILM"],
                          "origins": ["linux/DesktopHigh thin_film studio_off material_uniform", "…"] } ],
      "backends": [ { "backend": "compose", "status": "pass" }, …,
                    { "backend": "vulkan_pipeline", "status": "pass", "detail": "2 pipeline(s)" },
                    { "backend": "msl", "status": "pass", "detail": "3 entry point(s)" } ] } ],
  "adapter": "llvmpipe (LLVM 19.1.7, 256 bits)",
  "frames": [ { "preset": "DesktopLow", "status": "pass" } ],
  "failed_modules": 0
}
```

`status` is `pass`, `fail` or `skipped` (an earlier stage failed, the run turned the backend off, or the device lacks
a feature the module needs; the detail names it). A hash starting with `x` is a module that did not compose or
validate (the key is the file and defines).

What Linux cannot run, and the exact steps for the owner's hosts:

- **Mac (Metal compile).** On the Mac, from the repository root:
  `REGIA_MSL_DUMP_DIR=/tmp/regia-msl cargo xtask check-shaders --matrix --no-pipelines`, then
  `cd /tmp/regia-msl && for f in *.metal; do xcrun -sdk macosx metal -std=metal4.0 -c "$f" -o /dev/null || echo "FAIL $f"; done`.
  `manifest.txt` maps each file to its entry point and stage. (`-std=metal4.0` matches `MSL_LANG_VERSION`; an older
  Xcode takes `-std=metal3.2`.)
- **Windows (FXC).** The matrix's `hlsl_sm51` backend writes every module's Shader Model 5.1 text through
  `shader_hlsl`, so on the Windows box `set REGIA_HLSL_DUMP_DIR=<build-dir>` and
  `cargo xtask check-shaders --matrix --no-pipelines` dump it (`<name>__<entry>.hlsl`, `manifest.txt` lines
  `file|written entry|profile`); then for each manifest line `fxc /nologo /T <profile> /E <written entry> <file>`.
  The dump holds the 5.1 text only; a DXC compile of the 6.0 text runs inside wgpu on a `static-dxc` build
  (`cargo xtask windows-pass`).
  FXC can also die instead of failing: its optimizer overflows the stack, at any stack size, on a comparison re-tested
  the other way round under itself (ADR-5971). `shader_hlsl`'s assertion rule finds that shape on Linux (it follows a
  local's struct members and vector components apart, as FXC does: a store to one leaves the others, ADR-8930); on the Windows
  box `fxc.exe` then ends with `STATUS_STACK_OVERFLOW` (exit code -1073741571), so record exit codes, and
  `cargo test --release -p regia_render --test forward_small_stack` with `REGIA_GPU_INDEX=<the DX12 adapter>
  REGIA_GPU_STRICT=1` and no `dxcompiler.dll` on `PATH` renders a Desktop High frame through FXC on a 2 MiB thread.

## 24. Release evidence ledger (`cargo xtask release-ledger`, ADR-4935)

`docs/release/v0.5.0-evidence-ledger.md` holds one row per v0.5 burn-down item (status, lane, branch head, integration
state, evidence, platforms, what is open). `cargo xtask release-ledger` checks every cited commit and evidence file, that
each `Integration` cell matches git ancestry against the ledger's `Integ lines:`, and that the open-noncuttable list at the
top matches the table. Run it after `git fetch origin` on a host with the scratch disk; `--no-scratch` and
`--no-ancestry` skip (and report) those checks elsewhere, `--strict` fails a row whose lane branch moved past its head,
`--json` prints one object, and `--repo <dir>` checks another checkout. It is a lead tool,
not part of preflight.

## 25. Package builds as a build step (`tools/gaterig/package-prebuild.sh`, ADR-5719)

`regia_cli`'s `package_e2e --ignored` packages `samples/sim_bridge` (dev) and `samples/arena` (`shipping`) through
the real CLI and runs the packaged games. Its cost is the two cargo builds inside it. The `shipping` one is a fat-LTO
link of the whole engine with one codegen unit: after the workspace crates (one single-threaded LLVM job each, the
largest 6 to 10 minutes on the gate host), one `rustc` links for 18 to 30 minutes there on one core, and both grow
with the engine. A runner with a per-binary limit makes these builds in its build phase and leaves the test a cook,
two packages and the runs:

- **The step.** `bash tools/gaterig/package-prebuild.sh`, from the workspace root, beside `cargo test --no-run` (it
  locks the `shipping` profile dir and `<target>/package-e2e`, never `debug`). It issues exactly the cargo calls
  `regia package` issues for the test: the unit test `package_prebuild_script_issues_the_cli_builds` (`regia_cli`,
  `commands/package.rs`) fails when the script and `cargo_build_call` differ. `--print` writes the two calls instead of
  running them. It prints `PREBUILD-OK <seconds>s`, or `PREBUILD-FAIL <step>` and exits 1.
- **Same identity.** A shipping build bakes `REGIA_GIT_HASH` into `regia_platform`, and every crate above it rebuilds
  when the value changes. Give the step the value the test's launch has (`REGIA_GIT_HASH`, or the older
  `FERRITE_GIT_HASH` the gate's spool sets), the same `CARGO_TARGET_DIR`, and the same `REGIA_PACKAGE_E2E_GLIBC`. <!-- rename-keep -->
  The dev build goes to `<target>/package-e2e` in the target dir cargo builds the tests into (`cargo metadata`, so a
  `build.target-dir` config counts), symlinks resolved, which is where the test's `current_exe` puts it. Build the test
  binaries without `--target` (with it they land under `<target>/<triple>`). The shipping build's path remaps use
  `CARGO_TARGET_DIR` as `regia package` does. The script reads the `[project]` version the CLI parses (a comment after
  the string, or a literal string, is fine) and fails with `PREBUILD-FAIL` when there is none (ADR-5902).
- **The check.** `REGIA_PACKAGE_E2E_PREBUILT=1` in the test's environment says the step ran. A package that then
  starts compiling is stopped at its first crate, cargo and its `rustc` jobs with it, and the test fails naming the
  crate: a step that drifted from the CLI shows as one red test in seconds, not as a binary over its limit. The stop
  is one `SIGKILL` to the run's own process group through `/bin/sh`'s `kill` builtin on Linux and macOS, and
  `taskkill /T /F` on Windows; a stop that fails is named in the test's message (ADR-5902). Without
  the variable the test builds what it needs, as before (`cargo test -p regia_cli --test package_e2e -- --ignored`).
  The probe project of `packaged_linux_game_loads_asset_from_pak` is new on every run, so that test always builds.
- **The gate.** `tools/gaterig/the gate script.patch` (v2, ADR-5902) is the change to the build machines's `the gate script`
  (outside the repo): start the step once clippy and rustdoc are done (or the test build is), each of its two cargo
  calls at `GATE_PREBUILD_JOBS` (default 8) jobs, export the check's variable, and hold only package_e2e's ignored
  launch until the step's rc file says 0 (`tools/gaterig/gate-runtests.py.patch` adds the runner's `--hold
  KEY=FILE`); every other binary starts with the pool. A failed step fails the gate with `PACKAGE PREBUILD FAIL` and
  the log's tail. Apply both with `patch -d .claude/orchestration -p1 < <patch>` (`-R --dry-run` first tells whether
  one is already in); on a script that has v1 in, apply `the gate script.v1-to-v2.patch` instead.
- **One cargo call shape per target dir.** `regia_web` is `cdylib` + `rlib`, so its outputs have no hash in their
  names, and `regia_agent` and `regia` link it. A cargo call that resolves a crate under `regia_web` differently from
  the call before it (`cargo build -p xtask` after a workspace test build; a `--test <name>` selection of the
  workspace) rewrites those files, and the next call rebuilds `regia_web` and every crate above it. In the gate that
  was the first spool call recompiling 67 crates. `tools/gaterig/gate-remote-xtask-first.patch` builds xtask before
  the test build (apply it the same way; the two patches apply in either order). Compare two calls without compiling:
  `RUSTC_BOOTSTRAP=1 cargo <call> --unit-graph -Z unstable-options` and look at the units under `regia_web`.

## 26. Byte comparisons on NVIDIA cards (ADR-5953)

On NVIDIA's 615 drivers (seen on the RTX 5080 with 615.78.08 and the RTX 5090 with 615.71.09) one dispatch of one
pipeline, with nothing changed, can write one of two results that differ by 1 ULP: the driver switches a live pipeline
between two variants of its shader in 200 ms windows. Rasterized outputs (depth, shadow maps, motion vectors) repeat;
shaded passes do not have to.

- **Rule.** A test that compares shaded pixels byte for byte between two renderers or two runs belongs on lavapipe. On a
  card, compare rasterized outputs. Rendering the reference twice (a control) shows when the card did not repeat, but a
  control that repeats proves nothing about the other renderers: each pipeline switches on its own.
- **The repro** (fails on both cards today):

```
ssh <host> 'cd <build-dir>>/src && ~/bin/rig-slot.sh --card the test machine --timeout 600 \
  cargo test -p regia_render --test occlusion_gain_cost -- --ignored --exact --nocapture screen_lookup_repeats_alone'
```

  It prints the runs of equal outputs (`A8114 B264 A1632 B246 A235 B253 A19256`: A is the frame's own lookup, B the
  alternate) and when each change happened. `REGIA_OCC_ALONE_ITERS` sets the dispatch count (default 40 000, a
  minute or two; the first change came up to 13 s into the loop on the 5080 and 18 s on the 5090),
  `REGIA_OCC_ALONE_AGGRESSORS=<n>` adds n processes rendering on the card (none are needed).
- **The culls on a card:** `heavy_casters_cull_exactly_where_they_draw` (same file, `--ignored`) asserts that the
  depth right after the depth passes, the final depth and the shadow maps are equal between culled and unculled
  renderers, and prints the pixel patterns with an unculled control. Compare the depth the pass under test wrote
  (`FrameRenderer::set_prepass_depth_capture`, `test-hooks`): the forward pass writes depth again, so the final depth
  hides a caster the prepass wrongly dropped.
- **Whole frames:** `culled_scene_repeats_run_to_run` (same file, `--ignored`) with `REGIA_OCC_DDGI=1` prints, per run,
  which frames' image and probe lookup differ from run 0, compares the probes' uniform blocks, and with
  `REGIA_OCC_RERUN=<n>` runs each frame's lookup alone n more times.

## 27. A GPU test that cannot open a device fails on a card run (`REGIA_REQUIRE_ADAPTER`, ADR-7613)

Many GPU tests skip when no device opens (`let Ok(device) = RenderDevice::headless() else { return }`), and the
harness counts a skipped test as passed. A card run that loses its card partway through (one the build server binary lost
the NVIDIA adapters after about 18 device creations, ADR-5794) then reports every later test as passed.

- **The flag.** `REGIA_REQUIRE_ADAPTER=1` (any value but empty or `0`; `FERRITE_REQUIRE_ADAPTER` also works) makes <!-- rename-keep -->
  `RenderDevice` creation panic when it finds no adapter, or when the `REGIA_GPU_*` selection is refused
  (`GPU selection failed`, strict mode). The skip becomes a failed test whose message names the flag and the error.
  Without the flag nothing changes.
- **Who sets it.** Every run that expects a card: lane card runs on the build server and the rig
  (`REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1 REGIA_REQUIRE_ADAPTER=1`), the gate's GPU pass and the Arc pre-gate
  runner. A lavapipe run that must have its adapter may set it too. Keep the log grep of § 22: the flag does not
  cover a device opened outside `RenderDevice` (`regia_viewer` has its own `REGIA_VIEWER_REQUIRE_ADAPTER`), or a run
  that lands on lavapipe while a card is present (the card-use guard of § 22 catches that).
- **Tests that expect the error.** A test that checks a refused or missing adapter on purpose runs it in a child
  process and removes the flag from the child's environment (`tests/gpu_select.rs` `clear_selection`). The proof is
  `cargo test -p regia_render --test require_adapter`. The child skips as the GPU tests do: it passes without the
  flag and fails with it.
- **One device per test binary.** A GPU test binary takes its device from `RenderDevice::shared_headless()` (or a
  helper built on it), not one per test (ADR-7557, ADR-7613). Opening and dropping a device on parallel test threads
  is the destroy side of the Vulkan loader list race (ADR-2113), and it is what used up the card in the build server case
  above. A test that needs a device of its own (a feature set fixed at creation, a lost-device test) opens it once,
  keeps it until the process exits, and says why in a comment. `regia_render::loader_list_lock_count()` counts
  instance and device creations, so a test can check that asking again creates nothing.

## 28. Editor UI smoke (`cargo xtask editor-smoke`, ADR-7865)

| What | How to run | Time | Outputs |
|---|---|---|---|
| The real editor, every panel, menu item, palette command and registered action through the agent protocol, plus a 200 % UI-scale pass and a light-theme pass; per step: no panic, no error log, no egui id warning, no panel draw over 250 ms (lavapipe) / 50 ms (card), no flat screenshot, undo restores the level | `cargo build -p regia_editor --bin regia-editor && cargo xtask editor-smoke --keep-going` (Linux: private Xvfb, lavapipe); card: `flock /tmp/regia-gpu-<idx>.lock flock /tmp/ferrite-gpu-<idx>.lock env REGIA_GPU_LOCK_HELD=<idx> cargo xtask editor-smoke --card <idx> --keep-going`; a server card that cannot present to Xvfb: add `--headless` (no panel, keyboard, scale or theme steps); Mac and Windows: `--windowed`; one area: `--only 'panel:*'` | the build server 2026-11-15, debug build, farm load 230-700: lavapipe about 20 min for 659 steps; `--card 1 --headless` about 6 min for 476 steps | `target/editor-smoke/<timestamp>/`: `result.json`, `summary.md`, `panels.png`, `menus.png`, `dialogs.png`, `scale200.png`, `light.png`, `shots/`, `levels/`, `inventory.json`, `editor.log`; exit 1 on a FAIL |

The harness enumerates at run time (no list to update when a panel or action lands) and touches nothing outside its run
folder. `book/agents/editor-smoke.md` lists what is skipped on purpose and why.

## 29. Determinism goldens on every target (`cargo xtask det-golden`, ADR-8081)

Lockstep bits must be identical on every target (ADR-8012 A-4). `regia_detsim --test det_golden` checks the engine
scenarios against their committed goldens on one target (ADR-8046). The runners run that binary on six *legs* and
compare the results (book `agents/det-golden-runners.md`). The legs are:
- `host`: x86-64 Linux.
- `qemu`: a static aarch64 Android executable under `qemu-aarch64` user mode.
- `android`: the device, over `adb -s <serial>`.
- `mac`: the Mac M5, building the staged crate.
- `windows`: the Windows rig, building the staged crate.
- `deck`: the Deck, running the host binary.

- **Gate leg.** Add `cargo xtask det-golden gate` to the gate's CPU stage, after the test pass. It builds the host and
  static aarch64 binaries, runs both, and fails unless both legs reported, the three clean scenarios hash equal, and the
  planted glam SIMD scenario (`planted_simd_vec3a`) is reported as differing on aarch64.
  - It needs `qemu-aarch64` on the gate host (the build server: `qemu-user`, an owner install). Until then, the leg fails with
    `leg_error` / `qemu_missing`.
  - The static link needs NDK r27 as `ANDROID_NDK_HOME`: r28's `libdl.a` does not link with `+crt-static`.
  - Cost: two small builds of `regia_detsim` (libm and glam only) and well under a second of run time per leg.
- **Hosts that may not build.** `cargo xtask det-golden stage --legs <legs> --out <dir>` writes
  `<dir>/scripts/<leg>.sh`. Each script is POSIX shell that needs only `sh`, `tar`, `ssh`, `adb`, `flock` or
  `qemu-aarch64`. Run it where its tool is, for example `sh <dir>/scripts/android.sh > <dir>/logs/android.log 2>&1` on
  the host with adb. Then run `cargo xtask det-golden verify <dir> --require <legs>` anywhere.
- **Schedules.** Two jobs in `docs/ci/nightly-jobs.toml` run the legs:
  - `det-golden-nightly`: host, qemu and the device, daily;
  - `det-golden-rc`: all six legs, per tag. It is the determinism row of `docs/release/rc-checklist.md`.
- **A red result.** The two codes below are bugs to root-cause. Never re-bless them:
  - `clean_differs`: the detail groups the legs by hash. `scenario_failed` names the first diverging tick against the
    reference trace. Reproduce it on that leg, then compare live runs with `regia_detsim::golden::compare`.
  - `plant_not_caught`: the leg does not run what it claims, either an x86-64 binary or glam's scalar path (see the
    probe line).

## 29. Net soak (`cargo xtask net-soak`, ADR-7899)

(Numbered § 27 on the v0.5.x line and § 28 before the v0.6 net integration; ADR-7899, ADR-8030 and the net-soak release fragments cite it by the v0.5.x number.)

`regia_net`'s real server and clients run a sample game on an in-process impairment network at the `DatagramSocket`
seam (`regia_net_soak`, `book/agents/net-soak.md`). The network has latency and jitter, random and
Gilbert-Elliott loss, reordering, duplication, a bandwidth cap behind a drop-tail queue, an MTU, outages and NAT
rebinding, and it is deterministic from a seed. Named profiles: `clean`, `good-wifi`, `bad-wifi`, `mobile-4g`,
`transatlantic`, `congested`, `flappy`, `hostile`, and spec 120 § 15.1's twenty (ADR-8030): `high-rtt`, `rtt-step`,
`clock-drift`, `nat-rebind`, `cgnat`, `wifi-to-cell`, `long-outage`, `bufferbloat`, `downstream-loss`,
`asym-bufferbloat`, `listen-uplink`, `lossy-fat-pipe`, `rtt-extreme`, `mtu-1280`, `mtu-blackhole`, `flood`,
`forged-handshake`, `relay-path` (skipped until relay v2 exists), `lockstep-mix` (a lockstep match through
`regia_net_soak::lockstep::run_lockstep`, ADR-8209), `mixed`; plus the `congested+bulk`, `lockstep-mix+action`
and `lockstep-mix+bots` variants. Output: `target/net-soak/<UTC time>/result.json` (format v2; v1 still reads) and
`summary.md`. The command exits 1 when a gating bar fails. `cargo xtask net-soak --list` prints the profiles and bars.

**Bars** (never lowered to pass. A failure is fixed in `regia_net`, or the profile is wrong and an ADR says why):

- `involuntary_disconnects`: zero connections lost that the workload did not end, on every profile. Each lost
  connection counts once: the client's end, or the server's when the client never noticed. The one exception is the engine's
  own removal: an end inside an outage longer than its removal timeout (20 s,
  `regia_net::TimeoutPolicy::default()`), no more than 1 s before that timeout has run; `flappy` and `long-outage`
  have such outages.
- `reconnect_within_grace`: every rejoin is connected within 3 s of its link being usable (`flappy`'s long outage,
  the scripted leave and rejoin, and any involuntary end).
- `no_wedged_client`: every client joins. No connected client goes 2 s, with its link up, without its input reaching
  the server, without a snapshot, or with every input send failing. An outage a gap overlaps excuses itself and the 1 s after it;
  a gap is judged up to the client's own leave or end even when the server removes the peer first (ADR-8887).
- `hostile_dropped` (`hostile`): the server drops every hostile session (over-cap inputs, malformed messages, a
  flood) within 5 s of its first hostile act.
- `others_unaffected` (`hostile`): no honest client ends or wedges while the hostile peer is active (and for 10 s
  after), and no honest client's snapshot staleness rises more than 10 ticks above its own worst in the first 10 s.
- `bandwidth_within_budget` (`congested`): each client's offered downstream rate over the second half of the run is
  at most the link's 256 kbit/s cap.

**Spec 120 bars** (ADR-8030, spec 120 § 15.2): `missed_input`, `corrections`, `staleness_p99`, `rate_vs_bottleneck`,
`queue_delay_p95`, `rpc_p95_with_bulk`, `random_loss_kept`, `interrupted_once`, `resume_same_peer`, `no_hijack`,
`never_kicked_for_ping`, `desync_zero`, `lockstep_upload`, `tick_p99`, `join_under_flood`. Each is a function of the
run's `result.json` entry and is reported on every run whose profile it names. `lockstep_upload` gates since ADR-8209;
the others do not gate yet. `xtask/src/net_soak.rs`
`BARS` names each bar's owning lane (spec 120 § 17), and that lane sets `gating: true` in the commit that shows the
bar green on its profiles. The rules are in `book/agents/net-soak.md` and `cargo xtask net-soak --bars`.

**Staleness and the path** (ADR-10096). `staleness_p99` counts the client's estimate of the server tick minus the tick
of its newest snapshot, so the path's one-way latency is inside every sample (325 ms one way is 9 whole ticks at
30 Hz, and the limit is 4). Each run therefore also reports, per sub-profile, the same p99 with the path's whole
ticks taken off every sample and the mean of each part of a sample (path, transit, hold, rounding, estimate):
`measures.staleness_paths` in `result.json`, and a table in `summary.md` for every sub-profile over its limit. No bar
reads these numbers.

**Lockstep runs** (ADR-8209, ADR-8409). `lockstep-mix`, `lockstep-mix+action` (the action input mix) and `lockstep-mix+bots` (the wander-fire bot mix) are lockstep matches:
the clients are seats, a dedicated server is an observer host and a listen server plays seat 0. `desync_zero` passes
on 0 desync verdicts, 0 state transfers, no removed seat, decided votes, every simulation in step and at least one
agreed checkpoint. `lockstep_upload` (spec 120 § 7.5) passes when the host's 95th-percentile whole second of upload
is at most 4.0 Mbit/s and its busiest second at most 4.4 Mbit/s, IP and UDP headers included, over a 5-minute run;
it is judged at up to 40 seats on the orders mix and the bots mix and 24 on the action mix, and a run with more seats reports its
numbers and skips the bar. `involuntary_disconnects` and `no_wedged_client` are judged on the seats; the bars that
read snapshots are skipped on such a run.

**Sets.** Gate: every replication profile and `congested+bulk` at 8 clients on a dedicated server; `bad-wifi`,
`mobile-4g`, `flappy`, `hostile` and the spec 120 replication profiles at 32; `mixed` at 40; `lockstep-mix` at 8, 32
and 40 seats, `lockstep-mix+action` at 16, 24 and 40, and `lockstep-mix+bots` at 40 seats for 600 s; and `good-wifi` and `congested` at 8 on a
listen server. That is 61 runs of 60 virtual seconds (`rtt-step` 180 s, `long-outage` 120 s, the orders and action lockstep runs
300 s, the bots row 600 s, `cgnat` at least 10
clients) on up to 8 threads, with a target of under 10 minutes on the build server including the build (set before the
lockstep runs joined: measure it again). Nightly: `--plan nightly`, every profile
at 8, 32, 64 and 128 clients in both modes, 300 virtual seconds each, `--jobs 16` at `nice 15`; the lockstep profiles
at up to 40 seats (the bots mix at 40 seats is 600 s).

**Gate row.** `cargo xtask preflight` appends the step `net-soak` (`cargo xtask net-soak --plan gate`) when
`xtask::net_soak::GATE_ROW_ENABLED` is `true` (since the v0.7 freeze, ADR-10265, on the run `61 runs, PASS (0 failed
bars are known blockers, 0 are new)`). It was `false` at v0.5.0 because the run failed on two known blockers:
input bundles larger than the datagram wedge the client (branch `v051-net-input-mtu`), and an over-cap input is never
dropped (branch `v051-net-input-rejection`). To turn the row on:

1. Merge both branches.
2. Run `cargo xtask net-soak --plan gate`. Every bar must pass. A remaining failure is a finding to fix, never a bar to
   move.
3. Set the constant to `true` in a commit that quotes the run's verdict line.
4. Add the same command to the build machines gate script after `perf budget counts`.

`REGIA_NET_SOAK_GATE=1` turns the row on for one preflight run, and `REGIA_NET_SOAK_GATE=0` turns it off.

**Scale** (N215, ADR-8454). `cargo xtask net-soak --plan scale --jobs 1 --release` runs `mixed` and `mixed+hostile` at
40, 64 and 128 clients on a dedicated server and `listen-uplink` at 16 and 32 on a listen server, 180 virtual seconds
each; `--plan scale-gate` is the 40/32 set at 60 s; `--wire udp` puts real UDP sockets under the same network. Run it
one job at a time on a quiet host: `tick_p99` and the server CPU share are wall-clock numbers. At each tag the lead
adds the 40-client row to `docs/releases/net-evidence-ledger.md` and the per-tier numbers to
`docs/perf/net-scale-soak.md`; the `tick_p99` tiers are the `net_scale_tick_p99_*` rows of `docs/perf/budgets.ron`.
