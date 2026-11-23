# M2 hot-reload spike

Evidence for ADR-015 ([`ADR-015-draft.md`](ADR-015-draft.md)): game-code hot reload by **dylib reload**
(specs/20 §§ 1–3) versus **binary hot-patching with `subsecond`** (specs/20 § 1a), on Linux and Windows,
with the full skeleton dependency set and a game that owns an external simulation thread (specs/25 § 3).

Everything here is throwaway spike code in its own cargo workspaces. Nothing outside `spikes/` was changed;
the engine changes the spike implies are listed in the ADR draft.

Sources (cleanroom, docs/01): the dylib design follows the public Fyrox plugin hot-reload docs (MIT) and the
`bevy_dylib` crate pattern (MIT/Apache-2.0); the hot-patch wiring follows bevy's public `hotpatching`
integration (bevy_ecs 0.20 `hotpatching` feature, MIT/Apache-2.0) and the `subsecond` / `dioxus-devtools`
crate docs (MIT/Apache-2.0). No proprietary engine source was consulted.

## Layout

| Crate | Kind | Role (specs/20 § 1 name) |
|---|---|---|
| `crates/spike_engine` | rlib | stand-in for `regia_internal`: today's `regia` **plus every heavy skeleton dependency actually called** (`heavy.rs`: rapier step, kira, wgpu, winit, taffy, cosmic-text, fluent, naga_oil, gilrs, ktx2, png, tungstenite, notify, lz4/postcard/ron/blake3/uuid, memmap2) and the hot-reload runtime prototype (`hot.rs`, `loader.rs`) |
| `crates/spike_engine_dylib` | dylib | `regia_dylib` (bevy_dylib pattern) |
| `crates/spike_regia` | rlib | the `regia` facade; feature `dynamic` = specs/20's `hot-reload` |
| `crates/spike_editor` | rlib | stand-in for `regia_editor`: egui, egui-wgpu, egui-winit, egui_dock, egui-snarl, transform-gizmo-egui, rfd |
| `crates/game` | rlib | the game: reflected components/resources, systems, sim owner. `src/generated.rs` is rewritten by the host before each rebuild (the "developer edit") |
| `crates/game_dylib` | dylib | specs/20's wrapper, kept to show why it is replaced (mode `wrapper`) |
| `crates/sim`, `crates/sim_dylib` | rlib / dylib | mock external sim (hover discs, own thread, 120 Hz own clock) and its never-reloaded dylib (option b) |
| `crates/host` | bin | headless editor-host stand-in: builds, copies, loads, snapshots, tears down, restores, verifies, measures |
| `crates/editor_host` | bin | editor-side link test: engine dylib + egui stack + game library load |
| `subsecond/` | separate workspace | `patch_host` (bin) + `patch_game` (lib), bevy_ecs `hotpatching`, run under `dx serve --hot-patch` |

## How to run

Linux (the toolchain comes from `rust-toolchain.toml`, 1.99.0):

```sh
cd spikes/hot_reload
./run.sh --mode salted --edit layout --sim inproc --gens 20     # one run
./matrix.sh 20                                                  # all modes -> results/linux-*.log|csv
./measure.sh                                                    # sizes + exported symbol counts
```

`run.sh` puts `$(rustc --print sysroot)/lib/rustlib/<host>/lib` (dynamic `std`) and `target/debug/deps`
(engine dylib) on `LD_LIBRARY_PATH`, as the editor launcher would. Host options:

* `--mode salted` — `cargo rustc -p game --lib --crate-type dylib -F hot-reload -- -C metadata=hotgen<N>`:
  a new crate disambiguator per reload, so every game type gets a new `TypeId`.
* `--mode unsalted` — same without `-C metadata` (game types keep their `TypeId` across builds).
* `--mode wrapper` — specs/20 as written: `cargo build -p game_dylib` (equivalent to unsalted).
* `--edit layout` — odd generations variant A (`Mover { speed: f32, legacy: u32, ticks: u64 }`, system
  `move_a`), even generations variant B (`legacy` removed, `heading: [f32; 3]` added, system renamed
  `move_b_renamed`; 16 → 24 bytes). `--edit retype` — A ↔ C, where C only changes `speed: f32` to `f64`
  (same field names, 16 → 24 bytes). `--edit oneline` — only `pub const GEN` changes (the one-line edit).
* `--sim inproc` — option (a): sim code inside the reloaded library; `on_unload` stops + joins the thread and
  saves its state into a reflected resource, `on_load` restarts it from that state.
  `--sim persistent` — option (b): sim code in `sim_dylib` (never reloaded), handle in the engine's
  `PersistentStore`; the thread runs straight through reloads.

Each generation: write `generated.rs`, build (timed), `on_unload` → snapshot (reflected, RON text per field,
keyed by entity + type path) → teardown (remove owned components/resources, `remove_systems_in_set(OwnerSet)`,
rebuild the reflect registry without the plugin's types; asserts zero leftovers) → copy to `.hot/<lib>-<N>.<ext>`
→ `libloading` load (never unloaded) → `REGIA_ABI_HASH` check → entry → `register` (owner-tagged) →
`ComponentId` identity check → restore through the new registry → `on_load` → 3 frames → verify: 5,000 `Mover`s,
the `speed` values the host wrote are intact, `ticks` kept counting, the added field holds its default and the
removed one is gone, the new generation's system ran (`Score.gen_seen == N`) and exactly the game's 3 `Update`
systems exist (old ones removed), the sim did not go backwards.

Windows (in the VM): same commands through PowerShell; `PATH` gets `<sysroot>\bin`,
`<sysroot>\lib\rustlib\x86_64-pc-windows-msvc\lib` and `target\debug\deps`, then
`target\debug\host.exe --mode … --gens 20 --csv …` (see `results/windows-*.log`).

subsecond (needs `dx` from `dioxus-cli` **0.7.x** — bevy_ecs 0.20.0-rc.2 depends on `subsecond ^0.7.0-rc.0`,
which resolves to 0.7.10; dx 0.8.0-alpha speaks the 0.8 protocol):

```sh
cargo install --locked dioxus-cli --version 0.7.10        # 1 min 27 s on the owner's machine
cd spikes/hot_reload/subsecond
PATCH_SECONDS=600 dx serve --hot-patch -p patch_host --interactive false --open false > /tmp/dx.log 2>&1 &
./patch_probe.sh /tmp/dx.log 5          # edits patch_host/src/main.rs (binary crate)
./patch_probe.sh /tmp/dx.log 3 lib      # edits patch_game/src/lib.rs (library crate)
# `--features full` links the whole skeleton dependency set statically (146 MB binary)
```

Windows: `powershell -File subsecond\patch_probe.ps1 -Dx <path>\dx.exe -Rounds 5` (did not get past the
build in the VM; see results).

## Results

Machines: Linux = owner's Arch box, 32 threads (shared with other jobs: load average 24–31 during the
matrix, so absolute build times are noisy); Windows = the owner's VM, 6 vCPU, CPU only. Debug profile as in
the engine workspace (`opt-level=1` for workspace crates, `3` for dependencies, line tables only).

### Dylib reload — 20 reloads each, all checks passed unless noted

| | Linux | Windows |
|---|---|---|
| Clean build of the engine dylib + host (all heavy deps) | ≈ 1 min | 7 min 07 s |
| Game rebuild, one-line edit (salted), median / max | 0.71 s / 1.39 s | 2.91 s / 3.69 s |
| Game rebuild, field add/remove/rename (salted), median / max | 0.88–2.12 s / 4.6 s ¹ | 3.84 s / 5.84 s |
| Game rebuild, one-line edit, *unsalted* (incremental cache reused), median | 0.68 s | — |
| Swap (unload → snapshot → teardown → load → register → restore → on_load), median | 18–32 ms ¹ | 42–45 ms |
| … of which `dlopen`/`LoadLibrary` + ABI check + entry | 1.1 ms | 19 ms |
| … snapshot + restore of 5,000 entities × 2 components + 2 resources (≈ 340 KB RON) | 13–26 ms | 18–23 ms |
| First frame after reload (schedule rebuild) | 0.2 ms | 0.3 ms |
| **End-to-end save → running (build + swap)** | **≈ 1–2.5 s** (budget 10 s) | **≈ 3–4 s** (budget 10 s) |
| RSS growth per reload, salted / unsalted | +2.3 MB / +1.1 MB | +2.1 MB / +0.9 MB |
| `--mode unsalted --edit retype` | **refused at gen 2**: `Mover` keeps its `TypeId`, bevy_ecs has it on file as 16 B, new type is 24 B | same |
| `--mode unsalted/wrapper --edit layout` | passes (renaming/adding/removing fields changes the `TypeId`) | passes |

¹ the quieter first Linux run gave build medians 0.84–1.02 s and swap 14–22 ms (`results/` holds the second,
loaded run; both runs passed every check).

External sim thread (120 Hz, own clock):

| | (a) sim inside the reloaded library | (b) sim in a never-reloaded `sim_dylib` |
|---|---|---|
| Works, 20 reloads, Linux + Windows | yes | yes |
| Sim stall per reload | the swap time (`on_unload` join → `on_load` restart): 18–45 ms ≈ 2–5 ticks; resumes from the exact saved tick (checked) | none — ticks keep advancing during build and swap (e.g. `sim tick 1→437`) |
| Requirements | full sim state serialisable; thread stop/join in `on_unload`; sim restarts from a snapshot | sim crate built as a dylib **that links the engine dylib** (otherwise serde/postcard exist twice and the game library fails to link); its handle lives in an engine-owned store the teardown never touches; editing the sim needs an editor restart |
| What reloads | sim + presentation | presentation only |

### Exported symbols (Windows limit 65,535)

| Library | Linux (`nm -D --defined-only`) | Windows (`dumpbin /exports`) |
|---|---|---|
| engine dylib (full skeleton deps, engine crates still M1 stubs) | **37,762** (153 MB) | **34,479** (47 MB DLL + 149 MB PDB) — **53 % of 65,535** |
| game library (one generation) | 753 (1.9 MB) | 804 (0.9 MB) |
| `sim_dylib` | 91 | 98 |
| editor host exe (engine dynamic, egui stack static) | — (16 MB) | — (5.7 MB) |

Top exporters in the engine dylib (Linux): read_fonts 4,792 · x11rb_protocol 2,961 · core 2,635 · alloc
2,365 · wayland_protocols 1,828 · rustix 1,178 · rapier3d 929 · bevy_ecs 893 · ash 799 ·
wayland_protocols_plasma 796 · taffy 776 · glam 701 · parry3d 695 · wgpu_core 649 (`results/linux-symbols.txt`).
Headroom for Regia Engine's own engine code: ≈ 31,000 exports on Windows.

### subsecond hot-patching (`dx serve --hot-patch`, dx 0.7.10)

| | Linux | Windows |
|---|---|---|
| `dx serve --hot-patch` builds and runs the app | yes (first fat build 41–57 s) | **no** — 5 linker setups tried, none linked (`results/windows-subsecond.log`): repo default `rust-lld` → dx's linker wrapper fails; MSVC `link.exe` → LNK1104 `opengl32.lib` (dx links outside rustc's MSVC discovery); `link.exe` in the vcvars64 environment → LNK4003 on dx's own `libdeps-*.a` + 1,022 unresolved externals; `rust-lld` in vcvars → as the first; toolchain `lld-link` → build scripts fail |
| Edit of a system body in the **binary** crate: save → new body's output | 396–483 ms (dx: "Hot-patching … took 351–406 ms") | — |
| Edit in a workspace **library** crate (the `game` rlib layout) | 377–450 ms (patch 230–256 ms) | — |
| Same, binary statically linking the full skeleton set (146 MB) | 379–563 ms | — |
| State across patches | kept (`value` continues; resource untouched) | — |
| Struct layout change (`Game` gains `extra: [u64; 4]`) | **patched anyway, silently**: new code reads the old 16-byte resource with the new layout — `frame`/`value` jump back to garbage, `extra` reads `[2285, 4579, 0, …]` from beyond the allocation and writes into it. No warning. | — |
| Installing `dx` (`cargo install --locked dioxus-cli --version 0.7.10`) | 1 min 27 s | 11 min 9 s |

## Findings that change the plan (details in the ADR draft)

1. **bevy_ecs keeps the old `ComponentId` (layout, drop, hooks) for a `TypeId` it has seen.** A rebuilt game
   type keeps its `TypeId` when only field *types* change (observed with rustc 1.99 on both OSes: adding,
   removing or renaming a field changed the `TypeId`; `speed: f32 → f64` did not), so specs/20's teardown → re-register → restore writes a 24-byte value into a 16-byte column.
   Fix that works on both OSes: give every reload its own crate disambiguator
   (`cargo rustc -p game --crate-type dylib -- -C metadata=hotgen<N>`). Cost: old ids are never freed
   (+1.2 MB/reload extra here) and the game crate loses incremental reuse (one-line rebuild 0.71 s vs 0.68 s
   here; larger for big game crates). The `game_dylib` wrapper crate is no longer needed. The host still runs
   the identity check before every restore.
2. **Editor and game builds must resolve identical features**, or the engine dylib (whose file name has no
   hash) is relinked under the running editor: the spike reproduced `undefined symbol` at editor start after
   a plain `cargo rustc -p game` once the editor's egui stack had changed shared crates' features. Fix used
   here: the facade's hot-reload feature also depends on the editor crate (never linked into the game), and
   the host fingerprints the engine dylib before each reload ("restart the editor"). With that, `host`,
   `editor_host` and game builds share one engine dylib on Linux and Windows (verified by hash).
3. **Every extra dylib must link the engine dylib** (`sim_dylib` → `spike_engine_dylib`), or shared crates
   appear twice and the game library does not link.
4. `on_unload` must run **before** the snapshot (threads flush their state into reflected resources) and a new
   `on_load` **after** restore (threads restart from restored state). specs/20 § 3 has them the other way.
5. bevy_ecs 0.20 specifics: resources are components on resource entities (teardown uses
   `remove_resource_by_id`; restore applies onto the entity `init_resource` created);
   `Schedules::remove_systems_in_set` must not run inside `World::resource_scope` (it may insert resources —
   panics); bevy_reflect has no unregister, so teardown rebuilds the `TypeRegistry` without the owner's types;
   system names need bevy_ecs' `debug` feature (off in the engine).
6. subsecond patches function bodies in the dx-built executable **and its workspace library crates** in
   0.4–0.6 s, but it has no notion of layout: a struct change is silent memory corruption. It also does not
   re-run registration code (new systems/types need a reload), needs the `dx` tool pinned to the subsecond
   protocol bevy_ecs uses (0.7.x today), and adds `dioxus-devtools` (+ dioxus-core, dioxus-signals,
   tungstenite 0.28, rand 0.9) to dev builds. It patches only the executable dx built — a game library
   loaded with `libloading` is outside its jump table, so the two mechanisms do not stack in one process
   without further work. **On Windows (MSVC) it did not link at all** in the VM with dx 0.7.10.
7. Rust `std` is dynamic in this topology (`libstd-*.so` / `std-*.dll` from the toolchain); the launcher must
   set `LD_LIBRARY_PATH` / `PATH` (done by `run.sh` / the PowerShell runs). No TLS, allocator or task-pool
   problems were seen in 20 × 8 Linux and 20 × 5 Windows reloads (one engine copy: bevy task pools, the
   reflect registry and the global allocator are shared through the dylib).
