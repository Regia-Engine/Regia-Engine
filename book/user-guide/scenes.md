# Scenes: entities, levels and prefabs

<!-- index: Level -->
<!-- index: Prefab -->
<!-- index: Level directory | see: Level -->
<!-- index: Persistent | see: Level -->

A **level** is a saved scene — a list of entities, each a set of components with values — stored as
canonical RON (`.level.ron`). A **prefab** (`.prefab.ron`) is the same idea for a reusable subtree:
spawn it, then override individual properties per instance.

## Entities and components

An entity is a plain handle. Everything it *is* comes from components. Spawn in a system:

```rust
use regia::prelude::*;
use regia_scene::components::Name;
use regia_scene::prelude::*;

fn spawn_player(mut commands: Commands) {
    commands.spawn((
        Name::new("Player"),
        Transform::from_translation(regia::core::math::Vec3::new(0.0, 1.0, 0.0)),
        // … a mesh, a collider, a controller …
    ));
}
```

Components are nouns (`RigidBody`, `PointLight`, `MeshRenderer`); systems are verb phrases
(`propagate_transforms`); plugins end in `Plugin`. The full glossary is in
`docs/04-conventions.md` § 3.

For blocking out a level before the art exists, `regia_render::Primitive` is a graybox shape
component: add it next to a `Transform` and the renderer meshes it (cuboid, sphere, capsule,
cylinder, cone, conical frustum, plane, torus, tetrahedron, ramp, prism).

```rust
use regia::prelude::*;
use regia::render::{Primitive, PrimitiveShape};
use regia_scene::components::Name;
use regia_scene::prelude::*;

fn block_out(mut commands: Commands) {
    let ramp = PrimitiveShape::Ramp { size: regia::core::math::Vec3::new(6.0, 1.5, 3.0) };
    commands.spawn((Name::new("Ramp"), Transform::default(), Primitive::new(ramp)));
}
```

It is stored in level files like any component, with a `format_version` (currently 1; see
`docs/formats/primitive-v1.md`).

### Two `Name` components

There are two name components, and it matters which one you use:

- `regia_scene::components::Name` is the name stored in level and prefab files (the `name:`
  field of each entity) and set when a level or prefab spawns.
- `Name` from `regia::prelude` (the ECS's own name type) is what the agent protocol's
  name-based entity references (`scene.query_entities { "name": … }`, `{"name": glob}` entity
  selectors) and the editor's outliner read.

The editor copies scene names onto the ECS `Name` for the entities it shows. In a running game,
give entities you want to find from the agent protocol the prelude `Name` (for example
`commands.spawn((Name::new("Player"), …))`); add the scene `Name` too when the entity should be
saved into a level.

## Levels

A level file is what the editor saves and the game loads:

```ron
(
    format_version: 1,
    entities: [
        (
            guid: "8b6f2a11-0000-4000-8000-000000000001",
            name: "Ground",
            components: {
                "regia_scene::transform::Transform": (
                    translation: (0.0, 0.0, 0.0),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (4.0, 0.2, 4.0),
                ),
            },
        ),
    ],
)
```

Entities have a stable `guid`, a `name`, an optional `parent`, and a map of reflected components
keyed by type path. This example shows the Ground entity from the `blank` template's startup level; the full
template also contains a Marker entity. The template is written at format
version 1; the current scene format is version 14 (`regia_scene::format::SCENE_FORMAT_VERSION`),
and the loader migrates every older version in memory. `regia upgrade-project --write` rewrites
the file. Version 2 added the per-level atmosphere, version 3 the look (exposure meter and color
grade), and later versions further per-level settings; see [Content formats](../reference/formats.md)
and `docs/formats/scene-files.md`.

A level can also carry `LevelSettings` (a reflected resource stored with the level): gravity,
game-rules name, ambient audio, the acoustic medium (how sound travels: Earth air, thin Mars air
or none, see [Audio](audio.md)), the planet atmosphere preset with time of day and sky body, and
the map's look. [Rendering](rendering.md) shows the atmosphere block.

Spec: `docs/specs/14-scene-and-framework.md`. The task steps are in
[Make a level and a prefab](../how-to/create/levels-and-prefabs.md).

## One file or a directory

A level is one `.level.ron` or a directory `<name>.level.d/` with one file per entity
(spec 37, format `level-dir` v1,
[`docs/formats/level-dir.md`](../../docs/formats/level-dir.md)). The directory is for
authoring, so version control can merge one entity. A package does not ship it. A level
may exist in only one of the two forms. Both siblings at once is `ambiguous_level_form`.

```console
$ regia level split assets/levels/room.level.ron
$ regia level merge assets/levels/room.level.d
$ regia level validate assets/levels/room.level.ron
```

`split` and `merge` remove the source after a successful conversion. `--keep` leaves it.
`--out` names another destination. `validate` checks layer bindings and memberships and
exits 1 when it finds one. The directory wrapper stays at `level-dir` v1. A new split
writes entity records at the current scene format version (14). Older scene versions still
load through the same in-memory migrators as a single file.

## Despawning a hierarchy

`commands.entity(root).despawn_recursive()` (from `regia_scene::prelude::HierarchyCommands`) and
`regia_scene::hierarchy::despawn_tree(world, root)` remove an entity and its whole `Children` subtree
with an explicit stack, leaves first, so a hierarchy of any depth cannot overflow the stack. Plain
`world.despawn(root)` also removes the children, but recursively, one set of stack frames per level:
a chain of a few thousand links overflowed a 2 MiB Windows thread. Level unload, prefab pruning and the
agent's `scene.despawn` use the iterative walk (ADR-2306).

```rust
use regia_app::prelude::*;
use regia_scene::hierarchy::despawn_tree;

/// Removes `root` and everything under it; false when `root` was already gone.
fn clear_subtree(world: &mut World, root: Entity) -> bool {
    despawn_tree(world, root)
}
```

## Loading a level from your game

The `LevelManager` resource (from `ScenePlugin`) owns the level lifecycle: which level is current,
which are loading, and the load and unload operations. `LoadMode::Replace` despawns the current
level's entities (except those marked `Persistent`) and loads the new one; `LoadMode::Additive`
keeps everything and loads on top, for streamed regions. `LevelLoading`, `LevelLoaded` and
`LevelUnloading` messages fire on the transitions, and the `OnLevelLoaded` / `OnLevelUnloading`
schedules run once each.

`open_level` loads the level's data from the `LevelLibrary` resource. Today a game puts the
parsed level there itself; having the asset server fill the library from `.level.ron` files is
**planned** (`docs/specs/12-assets.md` § 3). The editor opens `Regia.toml`'s `startup_level`
for you, but a running game does not read that field. `regia run --level L` starts the game in
level `L` (the path travels in `REGIA_START_LEVEL`; `StartLevelPlugin`, part of `DefaultPlugins`,
opens it after the game's `Startup` systems and re-opens it when the file is saved in a `--dev`
run). To load your first level from code, do it in a `Startup` system:

```rust
use regia::prelude::*;
use regia::core::id::Guid;
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};

/// The id this game uses for its main level.
const MAIN_LEVEL: LevelId = LevelId(Guid::from_u128(0x6d61_696e_0000_4000_8000_0000_0000_0001));

fn load_main_level(world: &mut World) {
    let Ok(path) = VfsPath::parse("assets://levels/main.level.ron") else { return };
    let bytes = match world.resource::<Vfs>().read_sync(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            error!("cannot read the main level: {err}");
            return;
        }
    };
    let level = match Level::from_ron_str(MAIN_LEVEL, &String::from_utf8_lossy(&bytes)) {
        Ok(level) => level,
        Err(err) => {
            error!("main level does not parse: {err}");
            return;
        }
    };
    world.get_resource_or_insert_with(LevelLibrary::new).insert(level);
    if let Err(err) = open_level(world, MAIN_LEVEL, LoadMode::Replace) {
        error!("cannot open the main level: {err}");
    }
}

fn setup(app: &mut App) {
    app.add_plugins(regia_scene::ScenePlugin)
        .add_systems(Startup, load_main_level);
}
```

Mark entities that should survive a `Replace` load (the player, a music manager) with the
`Persistent` component. An agent reads and edits the loaded level through the `scene.*` methods
([agent guide's scene page](../agents/scene.md)); switching levels from the protocol is not a
method yet.

## Prefabs

A prefab is an entity subtree saved for reuse. Spawn an instance and override fields without
touching the template:

```rust
use regia::prelude::*;
use regia_scene::prelude::*;

fn spawn_pickup(mut commands: Commands) {
    // `guid` is the prefab's asset id, registered from `prefabs/pickup.prefab.ron`.
    let guid = regia::core::id::Guid::from_u128(0x8b6f_2a11_0000_4000_8000_0000_0000_0002);
    // The caller owns the instance root's guid: draw it from the sim RNG so lockstep peers
    // derive the same prefab-internal guids (never `Guid::NIL`, never reused).
    let root_guid = regia::core::id::Guid::from_u128(0x8b6f_2a11_0000_4000_8000_0000_0000_0003);
    commands.spawn_prefab(guid, None, &SpawnOptions::default(), root_guid);
}
```

Instance overrides apply on top of the template's defaults (`PrefabInstance` keeps the authored
overrides as raw RON). Nesting and override merging follow `docs/specs/14-scene-and-framework.md`
(the scene spec).

Like levels, prefab templates live in a library resource (`PrefabLibrary`). A template is baked
from its scene file with `PrefabTemplate::from_scene_file(asset_id, &scene_file, &registry)` and
inserted there; `spawn_prefab` of a template that is not in the library yet spawns a
`PendingPrefab` placeholder that fills in when the template arrives. The editor's prefab mode and
`editor.spawn_prefab` handle all of this for you while authoring.

## Everything is text

Levels and prefabs are diffable RON. Re-saving an unchanged level is byte-identical (canonical
serialization: sorted keys, stable entity order, shortest round-trip floats) — which is what makes
agent edits and `git diff` work. See the [agent guide's scene page](../agents/scene.md) for how the
protocol reads and edits this same state at runtime.
