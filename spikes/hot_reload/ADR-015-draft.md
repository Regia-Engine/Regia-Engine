## ADR-015: Game-code hot reload mechanism (M2 spike result)
Date: 2026-10-04   Status: accepted — the canonical text is `docs/decisions.md` ADR-015 (owner, 2026-10-04). This file is the spike draft.

Context:
specs/20 planned dylib reload (engine in one Rust dylib, game library reloaded, state carried by reflection) and
left binary hot-patching with `subsecond` (§ 1a) to this ADR; 05 lists "code hot reload across OSes" and "engine
dylib over Windows' 65,535-export limit" as M2 risks, and specs/25 § 3 asks what happens to a game-owned external
simulation thread. The spike (`spikes/hot_reload/`, numbers in its README) built both mechanisms on Linux and
in the Windows VM with the full skeleton dependency set and a game owning a 120 Hz sim thread. Results:

1. **Dylib reload works on both OSes** — 20 reloads per configuration, every edit kind (field added, removed,
   retyped; system renamed; one-line change), 5,000 entities, state verified after every reload. Save → running:
   ≈ 1–2.5 s Linux, ≈ 3–4 s Windows (budget 10 s). Swap 18–45 ms. Memory +2.1–2.3 MB per reload.
2. **specs/20 as written is unsound with bevy_ecs.** bevy_ecs never forgets a `ComponentId`; a game type rebuilt
   with a changed field *type* keeps its `TypeId` (rustc 1.99, both OSes), so the re-registered type inherits the
   old layout, drop and hooks (spike: 16-byte column, 24-byte value; the host's identity check refuses it).
   Giving each reload its own crate disambiguator (`-C metadata=hotgen<N>`) gives every game type a new
   `TypeId` per reload; with it all edit kinds pass on Linux and Windows.
3. **Exported symbols:** engine dylib with every heavy dependency linked and called = 34,479 exports on Windows
   (53 % of 65,535), 37,762 dynamic symbols on Linux. Font parsing, X11/Wayland protocol bindings and core/alloc
   monomorphisations are ~40 % of it. Regia Engine's own engine code (still M1 stubs) has ≈ 31,000 exports of headroom.
4. **Feature unification is a correctness rule**: an editor build and a game build that resolve different
   features for any shared crate relink the (unhashed) engine dylib; the spike reproduced the editor failing to
   start with `undefined symbol`. Cargo's workspace-wide unification (`-Zfeature-unification`) is unstable.
5. **subsecond** (dx 0.7.10, the protocol bevy_ecs 0.20 uses) patches system bodies in 0.38–0.56 s on Linux,
   also in workspace library crates and in a 146 MB binary with the whole dependency set, with state kept.
   It **silently corrupts memory on a struct layout change** (spike: the patched system read and wrote a
   resource with the new layout over the old allocation; no warning), does not re-run registration code, adds
   `dioxus-devtools` to dev builds and requires the external `dx` tool to own the build. It patches the
   executable `dx` built; game code loaded from a separate library is outside it, so it does not combine with
   dylib reload in one process without further work. **On Windows it did not work**: `dx serve --hot-patch` failed to
   link the fat binary in the VM with all five linker setups tried (repo `rust-lld`, MSVC `link.exe` with and
   without the developer environment, toolchain `lld-link`); with `link.exe` + vcvars, MSVC rejects dx's own
   generated archive (LNK4003) and 1,022 symbols stay unresolved. (The VM has VS Build Tools only and runs
   the job as SYSTEM; a full Visual Studio install might behave differently — not pursued, since the dylib
   path needs neither.)
6. **External sim thread:** (a) stop/join in `on_unload`, save state to a reflected resource, restart in
   `on_load` — works, stall = swap time (≈ 2–5 ticks), needs the whole sim state serialisable. (b) sim code in a
   never-reloaded dylib that links the engine dylib, handle in an engine-owned store — works, zero stall, sim
   edits need an editor restart.

Decision:
1. **The editor reloads game code by dylib reload** (specs/20 §§ 1–3) on Linux and Windows, with these changes:
   - The reloadable library is built with `cargo rustc -p <game> --lib --crate-type dylib --features hot-reload
     -- -C metadata=hotgen<N>` (N = reload counter of the editor session). The `game_dylib` wrapper crate is
     dropped from the project template (02 § 10, specs/21 templates).
   - Before restoring, the editor checks every newly registered game type against bevy_ecs' component registry
     and refuses the reload if a `TypeId` already has a `ComponentId` (defence in depth: should never fire with
     the salt).
   - Order: `on_unload` → snapshot → teardown → load → `register` → identity check → restore → `on_load`
     (new hook) → resume. Snapshot values are stored as RON text per field (no pointer into the old library).
   - Teardown: owned components/resources removed, systems removed by an owner `SystemSet`
     (`Schedules::remove_systems_in_set`, never inside `resource_scope`), reflect registry rebuilt without the
     owner's registrations; owned-instance count asserted zero.
   - **Feature anchor:** the `regia` facade's `hot-reload` feature depends on `regia_dylib` *and*
     `regia_editor` (never linked into the game) and the project's `editor` crate depends on `game`, so the
     editor build and the game build resolve identical features. The editor fingerprints the engine dylib at
     start and refuses to load a game library if a build relinked it ("engine changed — restart the editor").
   - Engine structure: the umbrella moves to `regia_internal`; `regia` becomes a facade that re-exports it
     and, under `hot-reload`, force-links `regia_dylib` (bevy / bevy_internal / bevy_dylib pattern).
     `export_game!` generates `regia_game_entry` and `REGIA_ABI_HASH`.
   - Any additional non-reloaded dylib (e.g. a sim) must itself link `regia_dylib`.
2. **Export budget:** CI counts `regia_dylib.dll` exports on Windows (and dynamic symbols on Linux) on every
   change to dependencies and at each milestone; warn at 50,000, fail at 60,000. The planned split if it fails:
   move the platform/GPU stack (winit, wgpu, ash, x11rb, wayland, font parsing) into `regia_dylib_platform`,
   linked by `regia_dylib`.
3. **subsecond is not adopted for M2–M11.** It stays an opt-in experiment for the standalone dev launcher
   (`regia run --hot-patch`, desktop only) at the earliest at M11, and only with a guard that refuses a patch
   when the reflected layout of any game type changed (rebuild + dylib reload or restart instead). Reasons: it did not
   link on Windows (MSVC) at all with dx 0.7.10; its only failure mode for the most common risky edit (adding a field) is silent memory corruption, it needs an
   external build tool that must track bevy's subsecond version, and the dylib path already meets the 10 s
   budget with every edit kind. The 03 § 6 budget stays "≤ 10 s"; the "≤ 2 s if hot-patching" variant is
   removed until hot-patching is adopted.
4. **External simulations (specs/25 § 3): option (b).** `SimBridgePlugin<S>` hosts the `ExternalSim` in a
   library that is not reloaded: the sim crate is built as its own dylib linking `regia_dylib`, its handle
   lives in an engine-owned resource that hot-reload teardown never touches, and the game library holds only
   presentation. Editing the sim needs an editor restart (like engine code). Reasons: DO27's sim and netcode
   must not be modified (AGENTS.md § 1.9) and cannot be required to serialise and restart their whole state
   (sockets, sessions, its own clock); (b) has no stall and keeps the sim's clock independent of the editor.
   Option (a) — stop/join in `on_unload`, restart from a reflected snapshot in `on_load` — remains the rule for
   threads a game spawns itself (specs/20 § 4).

Consequences:
- Easier: one mechanism for every edit kind on both desktop OSes, measured well inside budget; game state
  survives type changes; DO27's sim keeps running through presentation reloads.
- Harder: every reload leaks its old library, `ComponentId`s and archetypes (≈ 2.2 MB per reload in the spike;
  the "restart after ~50 reloads" hint stays); the game crate is recompiled without incremental reuse on each
  reload (≈ +0.03 s for the spike's game, more for large game crates — measure on DO27's shell at M3); dev
  builds of the editor ship the toolchain's `std` dylib on the library path; the engine dylib's export count
  needs a CI guard; projects must keep the feature-anchor dependencies in the template.
- Specs to update when accepted: specs/20 § 1 (topology table without `game_dylib`; feature anchor; facade
  split; std path), § 1a (subsecond outcome), § 2 (`export_game!`), § 3 (salted build command, identity check,
  `on_unload`/`on_load` order, teardown details, engine fingerprint), § 4 (sim threads → specs/25 § 3);
  specs/25 § 3 (option (b) chosen; sim dylib links the engine dylib); 02 § 10 (project layout without
  `game_dylib`); 03 § 1 (`libloading` already listed; note no new runtime dependency — `dioxus-devtools` is
  not added), 03 § 6 (drop the ≤ 2 s hot-patch row); 05 M2 risk table (symbol-count guard); 06 (CI export-count
  check); docs/provenance.md (Fyrox hot reload, bevy_dylib pattern, bevy hotpatching integration).
