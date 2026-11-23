# 04 — Conventions

## 1. Language and formatting

- Edition 2024, MSRV = workspace `rust-version`. `rustfmt.toml` in repo root (max width 110).
- `cargo clippy --workspace --all-targets -- -D warnings` must pass. Workspace lints in root `Cargo.toml`.
- Modules: one concept per file; files over ~800 lines are a smell, split them.
- No glob imports except `use super::*` in tests and `regia::prelude::*` in game code / samples.

## 2. Naming

- Crates `regia_<area>`. Public types `UpperCamelCase`, no `F`/`Fe` prefixes (Rust paths namespace them).
- Components are nouns (`RigidBody`, `PointLight`), events are past-tense or noun phrases
  (`CollisionStarted`, `AssetEvent`), systems are verb phrases (`propagate_transforms`).
- Plugins end in `Plugin` (`PhysicsPlugin`). Resources describe a singleton (`AssetServer`, `Time`).
- WGSL: `snake_case` functions, `UpperCamelCase` structs, files `kebab-case.wgsl`.

## 3. Glossary (use these terms, not proprietary ones)

| Regia Engine term | Meaning |
|---|---|
| **World** | The ECS world holding all live game state |
| **Level** | A saved scene asset that can be loaded into the world (persistent or streamed) |
| **Entity / Component / Resource / System** | ECS terms |
| **Prefab** | Reusable entity subtree asset; instances can override properties |
| **Script** | A Rust behaviour attached to an entity (`impl Script`), editable in the inspector |
| **GameRules** | Per-level/project rules object: spawning, win/lose, match flow |
| **Player** | A human participant (local index or network connection) |
| **Controller** | Something that drives a `Character` or other controllable entity: `PlayerController`, `AiController` |
| **Character** | Capsule-based movable entity with movement modes |
| **CameraRig** | Camera behaviour (boom/follow/first-person/fixed) with blending |
| **Material** | Shader + parameters asset |
| **Anim Graph** | Asset combining state machines, blend spaces and layers |
| **Sequencer** | The cinematics timeline asset (`.seq.ron`): named tracks keying a target's property over one master clock, plus markers fired on crossing |
| **Track** | One property animation in a sequencer or shot sequence: keys over time |
| **Marker** | A named instant on the sequencer timeline, dispatched when playback crosses it |
| **Pak** | Packaged cooked-content archive |
| **Cook** | Convert source assets into platform-specific runtime data |
| **PIE** | Play-in-editor |
| **Procedural graph** | A `.pgraph.ron` node graph that turns rules into placed content: reads the world, refines point sets, spawns instances, prefabs or data (specs/62) |
| **Generator** | The `ProcGenerator` component on a level entity: binds a procedural graph or variant, parameter overrides and a seed, and runs it over its bounds |
| **Graph variant** | A `.pgvar.ron` file: a parent graph plus parameter overrides (the graph-level analog of a material instance) |
| **Layer** | A named, switchable set of level entities (`LayerMembership`; states `Absent`/`Parked`/`Live`), spec 37 |
| **Placed level** | A level placed inside another level as one transformed object, inline or streamed (`PlacedLevel`), spec 37 |
| **Spatial query** | An AI query that generates candidate points or entities, filters and scores them (`.spatialquery.ron`), spec 33 |
| **Size map** | Byte breakdown of an asset and everything it pulls in, shared versus exclusive, spec 32 (shipped as `size_map`; ADR-1661 had proposed "footprint map", the rename is an open owner/counsel question, ADR-2312) |
| **DCC scene feed** | Live stream of scene, camera and pose data from a content-creation tool into the editor, spec 60 |
| **Process kind** | What a networked process is: `Offline`, `DedicatedServer`, `ListenServer` or `Client` (`ProcessKind`), spec 70 |
| **Predicted spawn key** | Token tying a client's predicted spawn to the server's spawn record (`PredictedSpawnKey`), spec 73 |
| **Ambient task** | A state-tree task that runs every tick for the whole life of the tree, before selection, spec 38 |

## 4. Errors and panics

- Library crates: one `thiserror` error enum per module boundary (`AssetLoadError`, `ShaderError`...).
  Include the asset path / entity / type path in error context.
- `unwrap()` is a clippy warning. `expect("why this can't fail")` is allowed for true invariants.
- Never panic on user content or IO. See `02-architecture.md` § 11.

## 5. `unsafe` policy

`unsafe_code = "deny"` workspace-wide. A crate may opt out only if listed here, and every `unsafe` block, `impl`,
extern block and `#[unsafe(..)]` attribute carries a `// SAFETY:` comment stating the invariant; an `unsafe fn` outside
an `unsafe impl` states its contract under `# Safety`. A test or example target of any crate may opt out locally for a
counting `GlobalAlloc`, process-environment setup, a deliberate fault under test or a harness that measures the driver
(`regia_render/examples/driver_cost`), with the same comment (ADR-1897, ADR-2086); library code of an unlisted crate may
not. A test that counts allocations installs `regia_test_utils::alloc_counter::CountingAlloc` instead of writing its own
allocator (ADR-3291). `clippy::undocumented_unsafe_blocks` enforces the comment on what the host target compiles, and
`xtask/tests/unsafe_policy.rs` enforces all of this on the source of every platform (ADR-2086).

| Crate | Reason |
|---|---|
| regia_platform | Raw window handles, Android JNI, `AChoreographer` and `AAssetManager`, the native crash handler and its signal-safe syscalls, Windows HID, registry and file-identity calls, `SetConsoleCtrlHandler` (`console_ctrl`, ADR-1886), thread priority and clock syscalls |
| regia_render | `libc` thread priority, process age and `atexit` (`platform/`), the experimental-features token (`hwrt`), the VRAM query through wgpu-hal (`quality`), the driver pipeline-cache blob (`pipeline_cache`), the Metal device's allocated size through wgpu-hal (`platform/macos.rs`, ADR-5860) |
| regia_editor | `libloading` for game and extension libraries, in `hot_reload/load.rs` and `extension/load.rs` only; the hot-reload dynamic-std probe FFI (Windows `K32` module enumeration, macOS dyld image list) in `hot_reload/load.rs` (ADR-5413); the cancelled-build tree kill on Windows (a `KILL_ON_JOB_CLOSE` job object around cargo) in `hot_reload/build_game.rs`, and its test-only process-exit-code read (ADR-7340) |
| regia_asset | `memmap2` mapping (`pak`) |
| regia_core | Counting `GlobalAlloc` wrapper for memory diagnostics (dev-only feature, `specs/10` § 3.3); code residency `libc` calls (ADR-125); the thread CPU clock, `profiling::thread_cpu_time` (ADR-214) |
| regia_media | FFI to media decoders and encoders (ADR-1593). The crate exists since ADR-1670 (spec 50 milestone 1: demuxer, isolation seam, player, reference raw codecs) and module-level opt-outs for codec FFI only: `opus` and `voice_opus` (`unsafe-libopus`; `voice_opus` behind feature `voice-opus`, ADR-1911); `av1` uses `rav1d`'s safe API, ADR-1869; `encode::opus` (the Opus encoder of `media-encode`, `unsafe-libopus`; `encode::av1` uses `rav1e`'s safe API), ADR-3369 |
| regia_dlss | FFI to NVIDIA's NGX (DLSS Super Resolution) and wgpu-hal raw Vulkan handles; only the module compiled when the SDK is linked (`ngx.rs`) uses `unsafe` (ADR-2002) |
| regia | `regia::android_main!`: `#[unsafe(no_mangle)]` on the generated `android_main`, under a local `#[allow(unsafe_code)]` (ADR-012) |
| regia_framework | `export_game!` and `regia::extension!`: `#[unsafe(no_mangle)]` on the generated library entry points, under a local `#[allow(unsafe_code)]` (`specs/20` § 2, `specs/57` § 2.8) |
| lookdev | `chase::page_out_self`, one `madvise(MADV_PAGEOUT)` per mapping in the overlay chase bench (ADR-2086) |
| regia_net | Windows DACL calls for private files (`private_file`: identity keys and known hosts, ADR-4828; `windows_acl`: admin tokens), and the native-crash exit code 70 (`server_host::crash`, spec 76 § 5). UDP socket buffers and the DSCP EF mark (`socket_opts`: `SO_RCVBUF`, `SO_SNDBUF`, `IP_TOS` / `IPV6_TCLASS`, ADR-7998); the scheduler-sockets lane's `SocketReport::configure` (buffer sizes, read-back, DSCP) through `setsockopt`/`getsockopt` in `socket/platform/{unix,windows}.rs` (ADR-8407) |
| regia_xess | External Intel XeSS DLL loading and wgpu-hal Vulkan interop, confined to `native.rs` when the SDK is generated (ADR-4656); no SDK source or bindings committed |
| regia_test_utils | `alloc_counter`, the one counting `GlobalAlloc` the allocation tests install (test support, never a runtime dependency; ADR-3291) |
| product_viewer | The Android launch intent read through JNI (`android.rs`: the activity global reference cast, the same use as `regia_platform`'s `jvm.rs`) and the driver pipeline-cache blob (`pipeline_cache.rs`, ADR-2835); each site carries its `SAFETY:` comment (ADR-5667) |
| regia_test_utils | Test-only: one `#[cfg(test)]` module of `hwrt.rs` replaces `REGIA_REQUIRE_RAY_QUERIES` with `std::env::set_var` under its own lock (ADR-4926); nothing ships from this crate (ADR-5667) |
| regia_world | Test-only: the `#[cfg(test)]` counting `GlobalAlloc` in `lib.rs` (`alloc_count`) that the unit tests' steady-state allocation checks run under; it forwards to `System` and is not in any shipped build (ADR-5667) |
| regia_agent | The local transport's Win32 named pipe FFI (`CreateNamedPipeW`, `ConnectNamedPipe`, `ReadFile`, `WriteFile`, overlapped I/O), confined to `platform/windows.rs` under a module-level `#[allow(unsafe_code)]` (ADR-5878, ADR-7386) |
| regia_asset_pipeline | Test-only: the `#[cfg(test)]` `reference` module of `gltf_meshopt.rs` calls the meshopt codec's C functions as the reference for the two ported decoders (ADR-3016, ADR-7386); nothing ships from it |
| regia_detsim | The FTZ/DAZ guard (`fpu`): `stmxcsr`/`ldmxcsr` on x86-64 and `mrs`/`msr fpcr` on aarch64, the only way to read and reset a sim thread's floating-point environment, under a module-level `#[allow(unsafe_code)]` (ADR-8013) |

Adding a crate to this list requires an ADR. Everything else in the workspace has no `unsafe` in `src`, and there is no
`transmute` of GPU data, no raw-pointer arena and no hand-written `Pod` impl anywhere in it (GPU data goes through
`bytemuck`'s derives).

Generated entry points are expanded in the calling crate, so Android crates of user projects and samples, and game and
extension libraries, need no opt-out (ADR-012).

**Counts** (audit of 2026-11-06, after ADR-3291 moved the tests' counting allocators into `regia_test_utils`;
`cargo test -p xtask --test unsafe_policy inventory -- --nocapture`). Columns up to "Fns in an impl" count shipped code,
which is `src` without its test items; "Fns in an impl" are the methods of an `unsafe impl`, covered by the comment on the
impl:

| Crate | Blocks | Impls | Attributes | Extern blocks | Fns | Fns in an impl | In tests and examples |
|---|---|---|---|---|---|---|---|
| lookdev | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| regia | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| regia_asset | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| regia_core | 17 | 2 | 0 | 0 | 0 | 4 | 34 |
| regia_dlss | 32 | 4 | 0 | 0 | 1 | 0 | 3 |
| regia_editor | 12 | 0 | 0 | 0 | 5 | 0 | 7 |
| regia_framework | 0 | 0 | 5 | 0 | 0 | 0 | 0 |
| regia_media | 11 | 2 | 0 | 0 | 0 | 0 | 10 |
| regia_platform | 83 | 2 | 0 | 0 | 2 | 0 | 9 |
| regia_render | 10 | 0 | 0 | 0 | 0 | 0 | 14 |
| regia_test_utils | 4 | 1 | 0 | 0 | 0 | 4 | 35 |
| xtask | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

Total: 204 sites in shipped code, 113 in tests and examples. The counts are a snapshot, not a pin: other work moves them.

**The riskiest sites and what tests them.** Miri (`docs/06-testing-and-ci.md` § 14) runs the rows marked so; the others make
system calls Miri cannot run.

| Site | Risk | Test |
|---|---|---|
| `regia_core::memory::TaggingAllocator` | a wrong header offset or layout corrupts the heap of every dev build | `memory::tagging::tests` (every alignment, a checking inner allocator, refusals), **Miri** |
| `regia_editor::hot_reload::load` | reads an ABI hash and a table of function pointers out of a library | `read_abi_hash` and `copy_export_table` on odd addresses and nulls (**Miri**), a real library through the real loader, `tests/hot_reload.rs` end to end |
| `regia_editor::extension::load` | takes a `Box<dyn RegiaExtension>` from another library | `take_extension` round trip (**Miri**), stamp refusals before anything loads, `tests/dynamic_extension.rs` end to end |
| `regia_platform::native_crash` | runs in a signal handler or exception filter | `xtask/tests/native_crash.rs` (real faults), `memory_probe::tests` (bad addresses, short copies), `crash_info` on Windows |
| `regia_core::residency` procfs sweeper | hands addresses that may be gone to the kernel | `residency::platform::tests` (unmapped, a partial region, more pages than a batch) |
| `regia_media::opus`, `voice_opus` | unsafe Rust translated from C, fed untrusted packets | `tests/voice_opus.rs`, `tests/codecs.rs`, the `decode` fuzz target |
| `regia_asset::pak` mapping | a file cut under a live view is a `SIGBUS` | `pak` tests (gate, cut file, every prefix, any alignment); the residue is stated in the module docs (ADR-177, ADR-285) |
| `regia_dlss::ngx` | raw Vulkan handles and NGX, built only with the SDK | `tests/gpu_smoke.rs` on an NVIDIA card |
| `regia_test_utils::alloc_counter` | the global allocator of every allocation test binary; a wrong forward corrupts their heap | `alloc_counter::tests` (every alignment, `realloc` both ways, zeroed, refused requests, threads, hooks), **Miri** |

## 6. Logging and profiling

- `tracing` macros only (no `println!` outside CLI output). Targets are crate names.
- Levels: `error` = user-visible failure; `warn` = degraded but continuing; `info` = lifecycle
  (startup, level loaded); `debug`/`trace` = developer detail. No `info!` per frame.
- Per-frame work gets `#[tracing::instrument(skip_all)]` or `info_span!` scopes. GPU passes get timestamp
  queries named the same as the render-graph node.

## 7. Reflection and serialization rules

- Every saved/edited type: `#[derive(Reflect, Default, Clone)]` + `#[reflect(Component, Default)]`
  (or `Resource`/`Asset`), registered via its plugin.
- Use editor attributes for UX: `#[reflect(@Range(0.0..=1.0))]`, `@Tooltip("...")`, `@DisplayName`,
  `@Category("Movement")`, `@Hidden`. These are custom attribute types defined in `regia_ecs::attrs`.
- Fields that must not persist: `#[reflect(ignore)]` + `#[serde(skip)]` as applicable.
- Version data formats: every cooked binary format has a `u32` version; loaders reject unknown versions
  with a clear error; the cooker rebuilds on version bumps.

## 8. Platform code

- Platform-specific code lives in `platform/<os>.rs` modules selected with `#[cfg(target_os = ...)]`
  behind a common trait or function signature. No scattered `cfg`s in gameplay-facing code.
- "Desktop" = `not(target_os = "android")` until iOS exists; use the alias `cfg(desktop)` defined via
  a build script / `cfg_aliases` if it reduces noise (ADR required for new build deps).

## 9. Documentation

- Every public item has a doc comment; every crate root has a `//!` overview linking its spec.
- Examples in docs compile (`cargo test --doc`).
- User guide: `book/` (mdBook) started in M18 (engine-stable), completed in M20; each editor workflow gets a page with screenshots.
- Agent guide: `book/agents/` grows from M2 — one page per feature as it lands (`AGENTS.md` § 1.8, `specs/23` § 4).
- Release fragments (ADR-2312): each change writes `docs/releases/unreleased/<lane>.md` with New / Changed / Fixed /
  Upgrade / Known issues, every host-visible API change under Upgrade, and no version in the file name. At a tag
  the fragments become `docs/releases/v<tag>.md`; `docs/releases/README.md` has the steps.

## 10. Commits

Conventional style: `render: add cascaded shadow maps (M5)`. One logical change per commit. Commit
messages for golden-image re-blesses state what visibly changed and why.

## 11. Preflight: the gate's cheap checks before you submit

`cargo xtask preflight` runs the merge gate's cheap checks in the gate's order and stops at the first failure with
a short summary (the failing step, its last 40 lines, the command, and the fix when there is one):

1. Conflict markers: no `.rs`, `.toml`, `.md` or `.wgsl` file (tracked, or untracked and not ignored) has a line
   that starts with a merge-conflict marker. The build machines gate refuses such a commit before it builds.
2. `cargo fmt --all --check`
3. `book-check --fences`: bare `rust` fences in `book/` that cannot be a module body (a `let`, a call, a `...`).
   Mark a fragment ```` ```rust,ignore ````; a bare fence is compiled by `book-check`.
4. `cargo clippy --workspace --all-targets -- -D warnings`
5. `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-D warnings`
6. In the build machines gate's order: `check-versions`, `notices --check`, `spdx --check` (the SPDX first line of every
   first-party `.rs`/`.wgsl` file, ADR-10225; `cargo xtask spdx --write` adds it), `check-formats`, `book-check`, `check-shaders`,
   `check-unwraps`, `det-scan` (the lockstep determinism scan, ADR-8013; preflight only until the gate runs it),
   `check-layers`, `check-server-build`, `check-net-core` (the ECS-free `regia_net` core builds, links no ECS crate
   and runs its headless example without `ecs`; spec 120 § 15.5, ADR-8501), `check-packaging`, `api-snapshot` (a removed or changed
   public item of a stable crate; `docs/api-snapshots/README.md`). A public item leaves the API only after a
   `#[deprecated(since, note)]` window (one minor release before 1.0, two after) listed in the release fragment's
   Upgrade section; a deliberate break is recorded with `--update --breaking` and `docs/api-review.md` "API policy"
   says what counts as public (ADR-2527)
7. `check-features`: the feature matrix (below; skip it with `--no-features`)
8. `cargo test --workspace --no-run` (skip it with `--no-build`)

`--changed` limits the cargo steps (4, 5, 7, 8) to the crates that differ from `--base` (default `main`, merge-base
diff; committed, staged, unstaged and untracked files; a rename counts for the old and the new path) plus the
workspace crates that depend on them, so a broken caller is still found; `--no-dependents` drops the second half.
Without `--no-dependents`, a change to the root `Cargo.toml`, `Cargo.lock`, `rustfmt.toml`, `clippy.toml`,
`deny.toml`, `rust-toolchain.toml`, `.cargo/` or the like widens the scope to the whole workspace, which is what
integration lanes and the gate still run. With `--no-dependents` those files do not widen it (ADR-8441): the scope
is the members that own a changed file, including a member's own `Cargo.toml` and an `include_str!` /
`include_bytes!` hit, and a `Cargo.lock`-only change (no member manifest or source) builds nothing. Preflight then
prints one line naming the workspace-wide files and stating that the full workspace test build is owed to the
integration line. `rustfmt.toml` does not need a wider cargo scope, because the fmt step is already
`cargo fmt --all --check` and clippy does not read it. `.cargo/` and `clippy.toml` are read by every scoped cargo
invocation, so the crates the lane changed are checked under the new flags and lints; clippy and the test build of
every other crate stay with the integration line. A file outside every crate that a crate compiles in through a
literal `include_str!` / `include_bytes!` path (a
project template under `crates/regia_templates/templates/`, a table under `docs/`, another crate's fixture) selects that crate too, and the
run prints the hit (ADR-1379); any other docs-only change builds nothing. `--list` prints the plan; with
`--timings FILE` naming an earlier run's file it also prints each step's last time and their sum. `--keep-going`
(`-k`) runs every step and lists every failure at the end instead of stopping at the first. Every run ends with a
table of step times, and `--timings FILE` writes them as JSON. Before a `test build` whose scope is the whole
workspace, preflight checks the free space of the filesystem that holds the cargo target directory
(`CARGO_TARGET_DIR`, otherwise `<workspace>/target`). Under 500 GB (free bytes / 2^30, the same unit as `df -BG`)
it does not start that step and marks it FAILED: `N GB free on <mount>; a workspace test build needs ~400 GB`.
`REGIA_PREFLIGHT_ALLOW_WORKSPACE_TEST_BUILD=1` skips the check, for a gate tree that has already decided the disk
can hold the link. A package-scoped test build is not checked. A failed free-space query refuses the workspace
test build the same way, unless that variable is set. Preflight never edits a file: run `cargo fmt --all`
first.

**The feature matrix (`cargo xtask check-features`, ADR-2322).** Step 4 builds every crate once, with the features
the workspace unifies: the defaults, plus whatever another member or a dev-dependency turns on. Code behind a feature
nothing turns on (DLSS, the Steamworks backend, the media codecs, `lua`, the umbrella's re-exports) and the off side
of a feature a dev-dependency turns on (`regia_render` without `frame-gen` and `test-hooks`, the build every shipped
game compiles) is never compiled by it. `check-features` runs one `cargo clippy --locked -p <crate> [--no-default-features]
--features <set> [--all-targets] -- -D warnings` per row of the table in `xtask/src/feature_matrix.rs`, one row after
the other in the shared target directory (a row changes the features of a few crates, so cargo rebuilds only those and
their dependents; a second run is incremental), keeps going after a failure, and ends with a table of rows, times and
the error lines of every failing row. `--check` runs `cargo check` instead (faster; warnings do not fail), `-j N`
limits rustc jobs per row, `--set NAME` runs one row, `--packages a,b` the rows of those crates (preflight
`--changed` passes its scope), `--list` prints the rows, `--fail-fast` stops at the first failure.

| Rows | Crate | Feature sets |
|---|---|---|
| leaf crates | `regia_render` | none (shipping: no `frame-gen`/`test-hooks`), `dev-assets`, `dlss`, `dlss` over the NGX check stub, `static-dxc` (Windows hosts only) |
| | `regia_dlss` | `sdk` over the NGX check stub (`REGIA_DLSS_CHECK_STUB=1`, cfg `regia_dlss_linked`), `sdk` without an SDK; both with tests |
| | `regia_steam` | `steamworks` with tests |
| | `regia_media` | `video,voice-opus`, `av1`, `opus`, `vorbis` (each with tests), `av1-nasm` (only with `nasm` on PATH) |
| | `regia_agent` | defaults off (no XR), `dcc-feed`, `dcc-feed-assets`, `reference-trace,test-hooks`, `denoise` |
| | others | `regia_pt` `denoise` / `lightmap-gpu`, `regia_cli` `lightmap-gpu`, `regia_editor` `hot-dylib`, `regia_core` `tracy`, `regia_physics` `physics2d,physics-cross-platform-determinism`, `regia_platform` bare / `accesskit`, `regia_ui` `accesskit`, `regia_crash` `tls`, `regia_asset` bare / `content-http,dev-assets`, `regia_net` bare / `voice`, `regia_ai` bare / `spatial-query-probe`, `regia_procgen` `pipeline`, `regia_framework` bare / `net,functional-tests`, `regia_input`, `regia_ecs`, `regia_app` bare |
| umbrella | `regia` | `--no-default-features --features client` (a shipping game), then one feature at a time over the defaults: `lua`, `xr`, `media`, `media-codecs,voice-opus`, `steam`, `crash`, `net`, `voice-net`, `physics2d`, `frame-gen`, `dlss`, `content-http`, `accesskit`, `dev-assets`, `dcc-feed`; `media-codecs-asm` (with `nasm`), `static-dxc` (Windows) |
| all at once | `regia` | every compatible feature together, `server` included (`REGIA_ALL`); not `static-dxc` or `media-codecs-asm` |

A row whose host need is unmet (a tool not on `PATH`, another OS) is reported as skipped with the reason, not failed.
The table must name every feature: before it compiles anything the command fails when a `[features]` key of a
`crates/*` manifest is in no row (a default feature counts when a row of that crate turns the defaults off), when a row
names a feature or crate that does not exist, or when a feature is in `OUTSIDE` without a reason (`regia_dylib`, which
forwards `regia`'s features one to one and has its own test, `tests/feature_forwarding.rs`; `regia_render`'s
`frame-gen` and `test-hooks`, on in every workspace build). A new feature therefore needs a row in the same commit.
`--coverage-only` runs just that check. The NGX check stub (`crates/regia_dlss/check_stub/ngx_bindings.rs`) declares
the NGX items `src/ngx.rs` uses, so the linked DLSS path is type-checked and linted without NVIDIA's SDK; it links
nothing, so only `check` and `clippy` work with it. When `ngx.rs` uses a new NGX item, add it to the stub. The real
SDK build (`DLSS_SDK` set) is still checked only where the SDK is installed (`docs/06` § 7). The `denoise` rows set `OIDN_DIR` to
`xtask/check_stubs/oidn` (an empty stand-in: the `oidn` build script only emits a link path, which a check never
uses), so they need no Open Image Denoise library either. In a printed command `{root}` is the workspace root.

Flaky tests, the gate's retry rule and the nightly jobs are registered in `docs/ci/` (`cargo xtask flaky`,
`cargo xtask nightly`; `book/user-guide/flaky-tests.md`).

On the workstation a lane runs every build through the wrapper, and preflight prints the exact line when the wrapper
exists:

```sh
cd <worktree> && <workspace> <lane> -- cargo xtask preflight --changed
```

On the build server (and CI) run `cargo xtask preflight` directly. The gates themselves are unchanged; preflight is how a
lane finds out before the gate does.

**Formatting policy.** The tree must be `cargo fmt --all` clean. A lane formats before every commit
(`cargo fmt --all`); preflight and the build server gate only check (`--check`) and fail on a diff. The workstation
`gate.sh`, run on the main checkout, formats and commits `style: cargo fmt` itself instead of failing.

## 12. Schedule order: shuffle and ambiguity checks

Systems with no `.before`/`.after`/`.chain()` path between them run in an order nobody chose. Code that only works in
the order it happened to get is a latent bug that a new plugin, a new system or a different thread count exposes.
Two tools find it (`book/agents/schedule-shuffle.md`):

- `App::shuffle_schedules(seed)` re-sorts every schedule's unconstrained systems from a seed (single-threaded, so a
  seed is repeatable). `cargo xtask shuffle-check [--seeds N] [--frames F]` runs the headless smoke scene
  (`regia_test_utils::shuffle::smoke_app`: rigid bodies, a character, spring bones, a clip-driven rig) once per
  seed and fails when the observable state (every reflected component by entity `Name`) differs between seeds.
- `App::ambiguity_report()` lists, per schedule, the system pairs that touch the same component or resource, one
  of them writing, with no order between them, ranked (write/write first). `shuffle-check --ambiguities` prints it for the
  engine's render, animation, physics and framework schedules.

A report line is a candidate, not a bug: two unordered systems that both only push to a list are fine. Add an
explicit edge only for an ambiguity that a shuffle run shows changing the result, with a test that fails without it.

## 13. JSON examples in the agent guide

Every `json` fence in `book/agents/` and `book/user-guide/agent-protocol.md` is checked against the protocol by
`cargo xtask agent-examples` (the last preflight step; rules in `book/agents/protocol.md`). Write an example the
way an agent would send it. Tag a fence `json,ignore` or put `<!-- not-a-request -->` before it when it is not a
call. Do not baseline a finding to hide a typo: `docs/ci/agent-examples.toml` is only for pages that describe
behavior the code lacks, with the reason on the row.

### Provenance record check (ADR-4581)

`cargo xtask check-provenance` checks every row of `docs/provenance.md`: explicit source
class from cleanroom § 2, substantive evidence, a permissive license for code-reference
rows, public-page URLs for set-derived behavior and the reader/builder process plus
retained-spec links. The existing `book-check` CLI stage runs it after its book checks pass, so CI and
preflight enforce it without a separate workflow step.
The primary classes support counts; mixed citations are retained. The check opens no
external set or source. See [cleanroom record status](01-cleanroom.md#provenance-record-status).

## 14. Doc examples of the stable crates

Every crate in `STABLE_CRATES` (`xtask/src/api_snapshot.rs`, ADR-2527) opens its `lib.rs` `//!` docs with a
`# Getting started` example: the smallest real use of the crate's main entry point, compiled by `cargo test --doc`
(`no_run` when it needs a window or a GPU; `ignore` does not count). Hidden `#` lines carry the setup, and the example
asserts a computed value where the API allows it. `cargo xtask doc-examples` counts the compiled and `ignore` fences per
crate, crate-level and item-level apart, against `docs/ci/doc-examples.toml`: a crate may not drop below its floor or
exceed its ignore ceiling, and one without a crate-level compiled example needs a `waiver` row that names who covers it.
`--update` records a raise; a lower floor needs `--lower "<why>"`, which appends the reason to the file. It is a
`cargo xtask preflight` step.
