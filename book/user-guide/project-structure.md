# Project structure

`regia new "My Game"` writes a Cargo workspace. This page describes what each part is for and the
files you will touch.

## Layout

```
my_game/
  Regia.toml                 project manifest (name, id, startup level, platforms)
  Cargo.toml                   workspace root (members: game, editor, launcher, android)
  game/                        your gameplay crate
    Cargo.toml                 path-depends on the engine checkout
    src/lib.rs                 the game plugin + `// REGIA:SCRIPTS` marker
    src/scripts/               project scripts (registered at the marker)
  launcher/                    desktop entry point (calls regia::run_with)
    build.rs                   Windows exe icon and version info (packaging guide)
  editor/                      hot-reload feature anchor (regia-editor uses it)
  android/                     the Android cdylib (regia::android_main!)
  assets/                      source assets, levels, prefabs, materials…
  config/                      engine config (physics layers, …)
  generators/                  procedural-content generators (specs/23 § 6)
```

`Regia.toml` is the manifest:

```toml
[project]
name = "My Game"
id = "com.regiaengine.my_game"
version = "0.1.0"
build_number = 1
engine = "0.2"
startup_level = "levels/main.level.ron"

[platforms.android]
min_sdk = 29
target_sdk = 35
abis = ["arm64-v8a"]
```

The `quick` template is different: one crate with a `src/main.rs`, no editor, launcher or android
crates ([Quick start](quick-start.md)).

`startup_level` is the level the editor opens with the project. A running game loads its levels
itself (see [Scenes](scenes.md#loading-a-level-from-your-game)); a running game does not read
`startup_level`. `regia run --level L` starts a dev run in level `L`.

## The game plugin

`game/src/lib.rs` is your gameplay. `Game` implements `Plugin`; scripts are registered at the
`// REGIA:SCRIPTS` marker (the editor's `editor.create_script` writes there):

```rust
use regia::prelude::*;

struct MySystems;

impl Plugin for MySystems {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, tick);
    }
}

fn tick() {}

pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(MySystems);
    }
}

// REGIA:SCRIPTS
```

For hot reload, the game is a `GamePlugin` (not just a `Plugin`). Export the game type with
`regia_framework::export_game!(MyGame)`: the macro emits the two dylib symbols the editor
resolves — `regia_game_entry()`, the address of a `#[repr(C)]` table of the plugin's
`register` / `on_unload` / `on_load`, and the `REGIA_ABI_HASH: u64` static — so the editor can
hot-reload the game. A plain launcher runs the same plugin through the `GamePluginEntry(MyGame)`
adapter; see [Hot reload](hot-reload.md) and `samples/hot_fixture`.

## Upgrading the engine

A project names the engine it targets in two places: `Regia.toml [project] engine` holds the release line (`"0.2"`,
or `"0.2.5"` for at least that patch), and the Cargo dependencies on `regia` and `regia_*` name one source. Run
`regia engine info --project my_game` to see both. Pick the source when you create the project:
`regia new my_game --engine-source path:/opt/regia-engine-0.2.5` (also `path:vendor/regia-engine-0.2.5` for a copy
inside the project, `git:<url>#<tag>` and `registry:<name>[@<version>]`).

To move to a new release, use that release's `regia`. Without `--write` it only lists what it would do:

```console
$ regia upgrade-project --project my_game --engine path:/opt/regia-engine-0.3.0
$ regia upgrade-project --project my_game --engine path:/opt/regia-engine-0.3.0 --write
```

This runs the registered migrators and rewrites assets in place (specs/12 § 4a), moves every engine dependency to
the new source and stamps the new line, with a `.bak` of every file it rewrites (a later run keeps it and writes
`.bak.2`). Every format change ships with its
migrator, so an older project upgrades cleanly. Run it after every engine update, patch releases too: it changes
nothing when nothing is needed. It refuses, and writes nothing, when the project needs a newer engine or is older
than the oldest line this `regia` still upgrades. Then read the Upgrade section of the release notes of each line
it lists; API changes in your own code are yours to make. The policy and every status code are in
[Distribution and upgrade (agents)](../agents/distribution-upgrade.md).

## What is `assets://`

Assets are addressed by project-relative paths (`assets/levels/main.level.ron`, written
`levels/main.level.ron`). The `AssetServer` resolves them through the dev cook cache in development
and through mounted `.fpak` archives in a packaged build — the same `assets://` namespace either
way.
