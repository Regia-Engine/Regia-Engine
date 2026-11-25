# Make a level and a prefab

<!-- index: Level -->
<!-- index: Prefab -->
<!-- index: Level directory | see: Level -->

Save a scene as a level, turn a reusable subtree into a prefab, and check that the files
load. The component model, load modes, and the two `Name` types are in
[Scenes](../../user-guide/scenes.md). Reflection, which is what lets a level store your
components, is in [Reflection](../../fundamentals/reflection.md).

## Before you start

A project from `regia new "My Game"` already has a startup level named by
`startup_level` in `Regia.toml`. That path is what the editor opens. A running game does
not read it unless you load the file yourself or start with `regia run --level`.

A level is canonical RON. Re-saving an unchanged level writes the same bytes. Two forms
exist:

| Form | Path | When to use it |
|---|---|---|
| One file | `assets/levels/room.level.ron` | Small levels. One file to read and to diff. |
| Directory | `assets/levels/room.level.d/` | Large levels. One entity file each, so a merge touches one entity. Authoring only: a package does not ship this directory. |

Keep one form. Two siblings (`room.level.ron` and `room.level.d`) are
`ambiguous_level_form`. The directory wrapper format is `level-dir` v1
([`docs/formats/level-dir.md`](../../../docs/formats/level-dir.md)). A new split writes
entity records at the current scene format version (`SCENE_FORMAT_VERSION`, 14). Older
files still load. The loader migrates them in memory.

## Steps

1. Open the startup level in the editor, or start from the template file under
   `assets/levels/`. Each entity has a `guid`, a `name`, an optional parent, and a map of
   components keyed by Rust type path. Components you want saved must derive `Reflect` and
   be registered. See [The ECS](../../fundamentals/ecs.md).

2. Save. The editor writes the `.level.ron` (or the open directory). To split or merge on
   the command line:

   ```console
   $ regia level split assets/levels/room.level.ron
   $ regia level merge assets/levels/room.level.d
   $ regia level validate assets/levels/room.level.ron
   ```

   `split` writes `room.level.d` beside the file and removes the file. `merge` does the
   reverse. Pass `--keep` to leave the source, or `--out` to write somewhere else.
   `validate` checks layer bindings and memberships. It prints `level validate: … is clean`
   or exits 1 with one finding per line.

3. Make a prefab from entities you will spawn more than once. In the editor, use
   **Create prefab from selection**. An agent calls `editor.create_prefab` with the entity
   ids and a project path such as `props/crate.prefab.ron`. The file is a scene subtree
   (`.prefab.ron`). Instances store overrides on top of that template. Details are on
   [Create prefab from selection](../../agents/prefab-create.md).

4. Cook so the level's asset references resolve, then run the game in that level.

   ```console
   $ regia cook --project .
   $ regia run --project . --level levels/room.level.ron
   ```

   `--level` is a project path under `assets/`. `StartLevelPlugin` opens it after the
   game's `Startup` systems. In a `--dev` run, saving the file re-opens it. Entities with
   the `Persistent` component survive a replace load.

5. Optional: write a `.glb`, then a USDA stage, for a DCC tool.

   ```console
   $ regia export glb --level assets/levels/room.level.ron --out build/exchange/room.glb
   $ regia export usda --glb build/exchange/room.glb --out build/exchange/room.usda
   ```

   `export glb` also accepts `--prefab assets/props/crate.prefab.ron`. By default it
   converts axes to glTF's +Z forward. `--keep-axes` leaves Regia's axes and adds no root
   axis node. `--entities` limits the export to those guids and their descendants.
   `export usda` writes triangles, points, normals, and the first UV set. It does not write
   materials or animation. See [Import a model](import-a-model.md).

## Finished check

- The level is exactly one of `.level.ron` or `.level.d/`.
- `regia level validate` on that path exits 0 when the level uses layers. A level with no
  layer bindings prints that it is clean.
- `regia cook --project .` exits 0.
- `regia run --project . --level <path>` opens that level. If it does not, the log names
  the file that failed to parse or the asset id that did not load. Fix that file. Do not
  ignore a missing reference.

Spawning a prefab from gameplay code, override merging, and `LoadMode::Additive` are in
[Scenes](../../user-guide/scenes.md). World-streaming cells want the directory form and a
cooked world index. That workflow is in the editor's World Map page, which this how-to
does not replace.
