# Spec 20 — Game-code hot reload (`regia_dylib`, editor host)

The mechanism is dylib reload: game code in a dynamic library, state moved through reflection, the old
library never unloaded. The shape follows Fyrox's public plugin documentation (MIT) and the
`bevy_dylib` force-link pattern (MIT/Apache-2.0). Both are credited in `docs/provenance.md`. The M2
spike measured it on Linux and Windows; ADR-015 is the result and amends ADR-003.

## 1. Build topology (inside a user project)

| Crate | crate-type | Links engine | Used by |
|---|---|---|---|
| `game` | rlib, and rebuilt as a dylib on each reload | `regia` with feature `hot-reload` → `regia_dylib` | rlib: `launcher` and `android`; dylib: the editor host |
| `editor` | bin | `regia_dylib` + `regia_editor`; depends on `game` | developers |
| `launcher` | bin | `regia` static + `game` rlib | desktop dev and shipping |
| `android` | cdylib (`libmain.so`) | `regia` static + `game` rlib | Android |
| sim dylib (only if the game has an external simulation) | dylib, loaded once, never reloaded | `regia_dylib` | the game, for the life of the process (`specs/25` § 3) |

There is no `game_dylib` wrapper. Cargo cannot switch a crate's `crate-type` by feature, so the editor
builds the game crate itself:

```
cargo rustc -p game --lib --crate-type dylib --features hot-reload --message-format=json -- -C metadata=hotgen<N>
```

`N` is the reload counter of the editor session. That `-C metadata` salt gives every game type a new
`TypeId` on each reload. Without it, bevy_ecs keeps the old `ComponentId` — layout, drop and hooks —
for a `TypeId` it has already seen, and a field *type* change writes a new value into the old column.
The spike reproduced that with rustc 1.99 on both desktop OSes (a `f32` field changed to `f64` kept
its `TypeId`; adding or renaming a field did not). The salt is what makes every edit kind sound.

**Feature anchor.** The engine dylib's file name is not hashed. If an editor build and a game build
resolve different features for any shared crate, cargo relinks that dylib under the running editor and
the next load fails with an undefined symbol (reproduced in the spike). Two dependencies keep the
resolutions identical:

- the `regia` facade's `hot-reload` feature depends on `regia_dylib` and on `regia_editor`;
- the project's `editor` crate depends on `game`.

`regia_editor` is not linked into the game library: the game never names it, so the dylib does not
pull it in. The dependency exists so `cargo rustc -p game --features hot-reload` sees the editor's
features of shared crates. The editor, at start and before every reload, fingerprints the engine dylib
and refuses to load a game library if a build relinked it ("engine changed — restart the editor").

**Facade split.** The engine umbrella moves to `regia_internal`. `regia` re-exports it and, under
`hot-reload`, force-links the dylib:

```rust
#[cfg(feature = "hot-reload")]
#[allow(unused_imports)]
use regia_dylib as _;
```

That is the `bevy` / `bevy_internal` / `bevy_dylib` pattern: one copy of the engine in the process
(statics, type registry, allocator, task pools). Rust's `std` is then dynamic too. The editor launcher
puts the toolchain's `lib/rustlib/<target>/lib` on `LD_LIBRARY_PATH` (Linux) or `PATH` (Windows).

**Extra dylibs.** Any other library that stays loaded (the sim dylib above) must itself link
`regia_dylib`. A second copy of a shared crate such as `serde` makes the game library fail to link.

Same toolchain, same profile, same target directory. Rust has no stable ABI.

**Export budget.** The spike's engine dylib, with every heavy dependency linked and called, exported
34,479 symbols on Windows (53% of the 65,535 DLL limit) and 37,762 dynamic symbols on Linux. CI counts
`regia_dylib` on dependency changes and at each milestone: warn at 50,000, fail at 60,000 (`06`).
If it fails, the platform/GPU stack (winit, wgpu and the font and window-protocol crates) moves into
`regia_dylib_platform`, linked by `regia_dylib`.

`02` § 1: this `hot-reload` edge is the one place a runtime crate names `regia_editor`. Shipping
builds leave the feature off, so a shipped game still contains no editor.

## 1a. Function hot-patching is not adopted

The M2 spike also tried binary hot-patching with `subsecond`, the protocol behind bevy_ecs 0.20's
`hotpatching` feature (dx 0.7.10). On Linux it patched system bodies in about 0.4–0.6 s, including a
workspace library crate and a 146 MB binary that statically linked the whole dependency set, and it
kept ordinary state. A struct gaining a field was patched with no warning: the new code read and wrote
the old allocation with the new layout. It does not re-run registration, it would add
`dioxus-devtools` to dev builds, and `dx` has to own the build. It only patches the executable `dx`
built, so a game library loaded with `libloading` is outside it. On Windows (MSVC, VS Build Tools)
`dx serve --hot-patch` did not link, in five linker setups.

Not used for M2–M11. The earliest it may come back is an opt-in experiment on the standalone dev
launcher (`regia run --hot-patch`, desktop only) at M11, and only with a guard that refuses a patch
when the reflected layout of any game type changed — rebuild and dylib reload, or restart, instead.
`dioxus-devtools` is not an engine dependency. The budget in `03` § 6 is ≤ 10 s, with no separate
hot-patch number.

## 2. Entry point

`export_game!` generates both symbols (ADR-127: a `#[repr(C)]` table, not a trait object):

```rust
// static __REGIA_GAME_EXPORTS: GameExports = { register, on_unload, on_load } → MyGame's GamePlugin
#[unsafe(no_mangle)]
pub extern "Rust" fn regia_game_entry() -> *const regia_framework::GameExports {
    &__REGIA_GAME_EXPORTS
}
// static REGIA_ABI_HASH: u64 = abi_hash_from(option_env!("REGIA_ABI_HASH"))
```

The editor reads `REGIA_ABI_HASH` before calling the entry, and refuses a mismatch ("rebuild the
editor"). The editor's `build.rs` computes the value (FNV-1a 64 of rustc version, package version,
target and profile) and passes it to the game build; the in-framework fallback for a game built
without the pin is the version-only hash, which the editor always refuses (ADR-2237).

## 3. Reload sequence

1. The build in § 1 succeeds.
2. Copy the artifact to `.regia/hot/game-<N>-<pid>-<serial>.<ext>`. The dynamic loader caches by path, so the name
   must be unique. `N` is the same counter passed as `-C metadata=hotgen<N>`; the process id and a
   process-wide serial keep two hosts on one hot dir from overwriting a mapped copy, and the copy is
   published with a no-replace link (ADR-2396).
3. If play-in-editor is running, pause it. Map the new copy with `libloading` (eager, so the
   library's initializers run) and call `regia_game_entry`. Check `REGIA_ABI_HASH` and the
   engine-dylib fingerprint. A failure here is refused before `on_unload`, and the old plugin keeps
   running. The entry therefore runs while the old plugin is still current. Do not unload the old
   library.
4. **`on_unload`.** The plugin stops and joins threads it spawned and writes their state into reflected
   resources. This runs *before* the snapshot, so that state is saved. It runs *after* the new library
   has been mapped and its entry called.
5. **Snapshot.** For every world, using the ownership tags (`specs/10` § 2.4), serialize every component
   and resource owned by the current game plugin, plus script slots, into a `HotReloadSnapshot`. Each
   value is RON text per field, keyed by `Entity::to_bits` (the entity's index and generation, a `u64`)
   plus the type path. Restore looks that key up with `Entity::try_from_bits`. An entity that is gone,
   including one whose index was reused with a new generation, increments `dropped_values` and does not
   receive the component. `EntityGuid` is not the snapshot key. The snapshot holds no pointer into
   the old library.
6. **Teardown.** Remove owned components and resources. Remove systems with
   `Schedules::remove_systems_in_set` on the owner set — never from inside `World::resource_scope`,
   because that call may insert resources and panic. Drop owned observers, messages, asset loaders and
   script registrations. Rebuild the `TypeRegistry` without the owner's registrations (bevy_reflect has
   no unregister). Assert the owned-instance count is zero. Forget the old export table. No pointer
   into the old library remains.
7. Call `register` under the new plugin id. The library is already mapped; this step does not map it
   again and does not unload the old one.
8. **Identity check.** For every newly registered game type, if that `TypeId` already has a
   `ComponentId`, refuse the reload. The salt makes this a backstop, not the normal path.
9. **Restore** through the *new* registry. Same type path: apply field by field (new fields take
   `Default`, removed fields drop, a changed field type takes `Default` and logs a warning). A type
   that disappeared is dropped with a warning that includes the count. Scripts are constructed again
   via `ReflectScript` and their fields applied.
10. **`on_load`.** The plugin restarts threads from the restored resources. Resume play-in-editor.
    Emit `HotReloaded` and show a toast ("Reloaded in 1.3 s (12 types, 340 instances)").
11. The old library is never unloaded. Rust dylibs register TLS destructors, so `dlclose` is unsafe.
    Each reload leaks the old library, its `ComponentId`s and its archetypes (about 2.2 MB in the
    spike). The editor suggests a restart after about 50 reloads.

**As built (ADR-7206).** Script slots: a slot of the engine-owned `Scripts` component belongs to
the library when its type path was registered under the library's owner (`ScriptRegistry` records
owners; `App::register_script` uses the current owner) or its instance's `TypeId` is one of the
owner's reflected types. Step 5 keeps its entity bits, position, `started`/`enabled` and reflected
fields (read through `ReflectScript::reflect_script`, which `#[derive(Script)]` implements, so the
derive needs `Reflect`). Step 6 removes the slots and the owner's registry entries. Step 9 builds
each slot through the new registration, applies the fields, reinserts it at its old position and
keeps `started`; a type path that is no longer registered drops its slots with a warning. The
counts reach the report as `scripts_rebuilt` / `scripts_dropped`. Asset loaders: a game library has
no loader registration path; extension loaders are removed per extension id by the extension
reload. The § 1 facade split is not built (deferred by ADR-7206: as written, `regia/hot-reload` →
`regia_editor` → `regia` is a package cycle, so the split must also move `regia_editor` and
`regia_dylib` onto `regia_internal`).

The order is fixed: map the new library and call `regia_game_entry` → `on_unload` → snapshot →
teardown → `register` → identity check → restore → `on_load` → resume. Mapping and the entry stay
before `on_unload` so an ABI or fingerprint mismatch never tears the running plugin down. A static
initializer or `regia_game_entry` must not assume `on_unload` has already saved thread state.

## 4. Constraints on game code

Documented in the user guide:

- State that should survive a reload lives in reflected components, resources and scripts
  (`Reflect + Default`). Anything else (raw statics, thread-locals, closures stored in engine
  resources) is gone after teardown.
- A thread the *game* spawns is joined in `on_unload` and started again from the restored snapshot in
  `on_load`. Engine task pools are fine; tasks the plugin owned are cancelled at teardown.
- An external simulation does not live in the reloaded library. See `specs/25` § 3: its code is a
  dylib that links `regia_dylib`, and the handle sits in an engine-owned resource that teardown
  does not touch. Editing that sim means restarting the editor, the same as editing engine code.
- Changing engine code means restarting the editor (rebuild the editor and the game together).

## 5. Failure handling

- Build error: no reload. Diagnostics stay in the output log. The old code keeps running.
- Panic during restore: there is no rollback after teardown. Catch the panic, log it, leave the world
  without game types (the snapshot is still on disk at `.regia/hot/last_snapshot.bin`), and offer
  "retry reload" once the code is fixed.
- The edit world is also protected because the level can be re-read from disk or the autosave.

## 6. Acceptance (M11)

Automated test (`regia_editor/tests/hot_reload.rs`, Linux and Windows CI): build fixture game v1,
spawn entities, mutate state, rebuild v2 with the metadata salt (a field added, a field removed, a
field retyped, a system renamed), reload, assert preserved and defaulted values and that v2 systems
run. The identity check refuses an unsalted retype. Repeat 20 reloads.
