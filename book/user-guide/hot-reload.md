# Hot reload

Hot reload recompiles your **game crate** and swaps it into the running editor without restarting —
state survives. It is the core of the Regia Engine iteration loop.

## How it works

The game is a dylib (`regia_dylib`), linked once and swapped per reload. Because Rust dylibs
cannot be safely unloaded (TLS destructors), each reload copies the game dylib to a **unique
filename** and never unloads the old one. The editor snapshots the reflected world state before the
swap and restores it after (`docs/specs/20-hot-reload.md`).

## Making a game reloadable

Implement `GamePlugin` (not just `Plugin`) and export it:

```rust
use regia::prelude::*;
use regia_framework::prelude::*;

struct MySystems;

impl Plugin for MySystems {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, tick);
    }
}

fn tick() {}

struct MyGame;

impl GamePlugin for MyGame {
    fn register(&self, app: &mut App) {
        app.add_plugins(MySystems);
    }

    fn on_unload(&self, world: &mut World) {
        // join game threads, write their state into reflected resources
    }

    fn on_load(&self, world: &mut World) {
        // restart those threads from the restored resources
    }
}

// A plain launcher (or test app) runs the game plugin through the adapter:
//   app.add_plugins(GamePluginEntry(MyGame));
```

The hot-reload host resolves two dylib symbols, `regia_game_entry` and `REGIA_ABI_HASH`. A
game library exports both with `regia_framework::export_game!(MyGame);` — it generates
`regia_game_entry()`, which returns the address of a static `#[repr(C)]` table
(`regia_framework::GameExports`) whose `register` / `on_unload` / `on_load` call your
`GamePlugin`, and the `REGIA_ABI_HASH` static (a `const` computation, so it works in a static
initializer; the editor pins it per build through the `REGIA_ABI_HASH` environment variable). A plain launcher or test runs the same plugin
through `GamePluginEntry(MyGame)` (above), which adapts any `GamePlugin` to an app `Plugin`.

```rust
use regia::prelude::*;
use regia_framework::prelude::*;

struct ExportedGame;

impl GamePlugin for ExportedGame {
    fn register(&self, _app: &mut App) {}
}

regia_framework::export_game!(ExportedGame);
```

The editor rebuilds the game when a file under `src/` changes (500 ms after the last change) or
when you press **⚙ Compile** in the toolbar (Ctrl+Alt+F11). The build runs in the background;
the toolbar shows `code: compiling 3.2 s`, then `code: Reloaded in 1.3 s (12 types, 340
instances)` or `code: error — …` (the old code keeps running on an error).

State that should survive lives in **reflected** components and resources. `on_unload`
runs before the snapshot, `on_load` after the restore.

Scripts (`#[derive(Script, Reflect, Default, Clone)]`, registered with `app.register_script`) are
rebuilt on every reload: the editor saves each script slot's reflected fields, drops the old
instance, builds the script again from the new code and puts the fields back, so the next tick
runs your edited `update` with the state it had. `start` does not run again. If you delete or stop
registering a script type, its slots are removed and the log says how many. A script whose
`ReflectScript` you implement by hand (without the derive) comes back with default fields.

## Running

```console
$ regia-editor --project my_game --game my_game
```

`--game <crate>` names the game dylib to host. Rebuild the game crate and the editor picks it up:
state survives, a small edit is back in under a second. Play-in-editor runs the reloaded game;
`docs/specs/20` § 3 walks the exact reload sequence (pause → snapshot → swap → restore → resume).

## What survives a reload, and what does not

- State that should survive lives in **reflected** components and resources
  (`Reflect + Default`). After the swap each value is restored field by field through the new
  library's types: a new field takes its `Default`, a removed field is dropped, a field whose type
  changed takes its `Default` (with a warning in the log), and a type that no longer exists is
  dropped (the warning gives the count).
- Game **scripts** are not snapshotted: a script slot survives a reload still running the old
  library's code (a known issue; see the hot-reload fragment's
  Known issues, `docs/releases/v0.1.28-spec-08-20-hot-reload.md`).
- Raw statics, thread-locals and closures stored in engine resources are **gone** after a reload:
  the old library's systems, observers and registrations are torn down before the new one registers.
- A thread your game spawns must be joined in `on_unload` (write its state into a reflected
  resource first) and started again from that resource in `on_load`. Engine task pools are fine.
- An external simulation does not live in the reloaded library: build it as its own dylib that
  links `regia_dylib`, and keep its handle in an engine-owned resource
  (`docs/specs/25-sim-bridge-and-do27.md` § 3). Editing that simulation means restarting the editor.
- Changing **engine** code means restarting the editor: rebuild the editor and the game together.
  The editor fingerprints the engine dylib when it starts and refuses every reload once that dylib
  was relinked ("engine changed — restart the editor"), whether the game build or another cargo run
  on the same target directory relinked it.

## When a reload fails

- **Build error**: nothing is swapped and the old code keeps running; the compiler output is in the
  output log and on the toolbar (`code: error — …`).
- **Refused** (engine changed, ABI hash mismatch, an editor without `hot-dylib`, an identity clash):
  the old code keeps running. An identity clash is found after the new library registered; the
  previous plugin is registered again with its state.
- **Panic during restore**: there is no rollback after teardown. The editor logs the panic, leaves
  the world without game types and keeps the snapshot at `.regia/hot/last_snapshot.bin`. Fix the
  code and compile again: the retry restores from that snapshot. The level itself is safe on disk
  and in the autosave.

Each reload maps a new copy of the library and never unloads the old one (Rust dylibs cannot be
unloaded safely), about 2.2 MB each. From the 50th reload on, the toast suggests a restart.

Agents drive the same loop with `agent.hot_reload` and `editor.code_status`
([Game-code hot reload (agents)](../agents/hot-reload.md)).
