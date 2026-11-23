# Live asset reload

Save a file, see it in the running game or editor, no restart. This is the rapid-prototyping loop
of `docs/specs/12-assets.md` § 8.

## Where it runs

| Host | How to start it | What watches |
|---|---|---|
| Editor | opening a project (on unless `REGIA_ASSET_WATCH=0`) | `assets/` and, in a source checkout, the engine's WGSL |
| Dev game | `regia run --dev` | `regia run` cooks; the game watches the cooked cache and the files it reads itself |
| Shipping game | never | the `dev-assets` feature is off in `regia package` builds |

```console
$ regia run --dev
regia run: live asset reload on (/home/me/mygame/assets); save a file under assets/ to see it in the game
regia run: reloaded assets/props/crate.material.ron in 41 ms
regia run: cook error in assets/props/crate.material.ron: 7:5: expected `)`
```

`--level L` starts the game in `L` (a `.level.ron`, relative to the project or to `assets/`)
instead of the level its code opens: the engine opens it after the game's `Startup` systems.

```console
$ regia run --dev --level levels/test_arena.level.ron
regia run: starting in /home/me/mygame/assets/levels/test_arena.level.ron
```

## What reloads

Each edit is debounced (120 ms after the last write, so an editor's write-and-rename save counts
once), then only that file and the sources that embed it are re-cooked, on a background thread.

| Kind | Files | What happens |
|---|---|---|
| Meshes | `.glb`, `.gltf` (+ its `.bin` and images) | re-imported; the asset server replaces the mesh in place — every handle stays valid |
| Textures | `.png`, `.jpg` | re-compressed; replaced in place |
| Materials | `.material.ron` (v3) | re-cooked; replaced in place |
| Material graphs | `.matgraph.ron`, `.matinst.ron`, `.matfunc.ron` | re-cooked (a function or image re-cooks the graphs that use it, a graph its instances); every `MaterialGraphRef` entity swaps to the new material and its pipelines build on the renderer's workers, never on the frame — meshes draw with the standard surface for the few frames until they are ready |
| Engine shaders | `crates/regia_render/shaders/*.wgsl` (dev checkout) | recomposed and validated for every entry shader that imports the file; renderers rebuild their pipelines on the next frame |
| Levels | `.level.ron`, `.level.d/` | the editor reloads the open level and keeps the camera; with unsaved edits it asks first (status bar: **Reload from disk** / **Keep mine**; agents answer with `editor.level_reload_resolve`). A `regia run --dev --level L` game re-opens `L` on save (`Persistent` entities stay) |
| Prefabs | `.prefab.ron` | re-baked; every placed instance is re-instantiated with its transform and overrides |
| Terrain | `*terrain.layers.ron` (any letter case; `terrain.layers.ron` itself too), `.scatter.ron` | tiles re-cook; a `Terrain` with a `TerrainLayersSource` re-reads its layer stack; `ScatterSets` entries are replaced |
| Decals, VFX | `.decal.ron`, `.vfx.ron` | the prefab/system is replaced by name; running effects continue with the new parameters; placed static decals are rewritten |
| Audio | `.wav`, `.ogg`, `.flac`, `.mp3` | sounds registered with `SoundAssets::add_file` re-decode in place (handles stay valid); the next play uses the new sound |
| UI layouts | `.ui.ron` | the template is re-parsed and every root spawned from it is respawned |

**Errors never break the running app.** A file that does not parse, a material graph that does
not compile or a shader that does not validate keeps the previous version live; the status bar
shows `cook error in <file>: <reason>` and the log has the full message.

## Status

The editor's status bar shows the latest line: `reloaded assets/rock.png in 120 ms` or
`cook error in …`. Agents read the same through `assets.watch_status`
([Assets](../agents/assets.md#live-reload-status)).

## Making your own system reloadable

Systems that cache data derived from an asset listen for the two messages `regia_asset` sends:

- `AssetReloaded { id, kind }` — a cooked asset was replaced in place (rebuild GPU uploads,
  colliders, …);
- `SourceAssetChanged { path, kind }` — a source file your system reads changed (re-read it).

Record the outcome in `AssetWatchStatus` so the status line and the agent see it:

```rust,ignore
fn reload_my_tables(
    mut changes: MessageReader<SourceAssetChanged>,
    mut tables: ResMut<MyTables>,
    mut status: Option<ResMut<AssetWatchStatus>>,
) {
    for change in changes.read().filter(|c| c.path.extension().is_some_and(|e| e == "csv")) {
        let outcome = match tables.reload(&change.path) {
            Ok(()) => WatchOutcome::Reloaded,
            Err(err) => WatchOutcome::Error(err.to_string()), // keep the old table
        };
        if let Some(status) = status.as_deref_mut() {
            status.record(WatchEntry { path: change.path.display().to_string(), kind: change.kind, millis: 0, outcome });
        }
    }
}
```

## Game code

Code reload is separate: see [Hot reload](hot-reload.md) (the toolbar's **⚙ Compile**,
Ctrl+Alt+F11, 500 ms after the last `src/` save).
