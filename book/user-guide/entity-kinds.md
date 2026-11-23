# Entity kinds and the project map

A level is a list of entities, and each entity is whatever set of components you give it. That stays. **Entity
kinds** add an opt-in label on top, so a project reads at a glance: the outliner shows a crate icon for every crate
and an enemy icon for every spawner, the editor tells you when something is missing a component its kind needs, and
`regia describe` writes an overview of the whole project.

A kind is a **label and a contract**, not a class: kinds have no parent kinds, nothing is inherited from one kind to
another, an entity has at most one kind, and a kind never changes what your systems do. Two kinds that share
components simply both list them. Group kinds with a **category** instead.

## Declaring a kind in code

A kind is a zero-size marker component. Its contract is the components it requires:

```rust
use regia::prelude::*;
use regia_scene::Transform;
use regia_scene::kind::{EntityKind, KindAppExt, KindMeta};

#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
struct Health(f32);

#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
#[require(Transform, Health)]
struct Crate;

impl EntityKind for Crate {
    const KIND: KindMeta =
        KindMeta::new("Crate", "crate", [0xc0, 0x8a, 0x3e], "A breakable supply crate").category("Props");
}

// In your plugin's `build`:
fn register_kinds(app: &mut App) {
    app.register_kind::<Crate>();
}
```

`regia_scene::entity_kind!(Crate, "Crate", icon = "crate", color = [0xc0, 0x8a, 0x3e], description = "...",
category = "Props");` writes the same `impl`. Spawning `Crate` inserts `Transform` and `Health` (that is how
`#[require]` works). The marker is zero-size, so a kind costs no memory per entity and adds no system.

## Declaring a kind as data

Put a `.kind.ron` anywhere under `assets/` (the format is in `docs/formats/kind-ron.md`):

```ron
(
    format_version: 1,
    name: "Crate",
    icon: "crate",
    color: "#c08a3e",
    description: "A breakable supply crate",
    category: Some("Props"),
    required: ["regia_scene::transform::Transform"],
    marker: None,
)
```

Give an entity the kind with the `KindTag` component (`"regia_scene::kind::KindTag": ("Crate")` in a scene file).
A data kind does not add its required components for you; the lint reports what is missing. The editor reads the
project's `.kind.ron` files when it opens the project, so the outliner shows data kinds with their icon and color
(reopen the project after adding one). A game that wants data kinds at run time calls
`regia_scene::kind::load_project_kinds(world, project_root)`.

If a code kind should be known to tools that do not run your game (the cook, `regia preflight`), add a `.kind.ron`
with `marker: Some("my_game::Crate")`, or commit the `project-map.json` that `regia describe --live` writes.

## Prefabs

Put the marker or the `KindTag` on a prefab's root and the prefab has that kind. Every instance inherits it: the
outliner shows the kind on the instance and on its root.

## In the outliner

Each row shows an icon before the name:

- a kinded entity: its kind's icon in its color; hover for the kind's name and description;
- an entity without a kind: an icon for what its components make it (sun, point light, camera, decal, sound,
  character, vehicle, trigger, body, mesh, collider, terrain, probe, spline, volume, UI); a group node has none;
- an entity without a kind that does two jobs (a light that is also a trigger): an orange `?`. Split it: put the
  second job on a child entity, or give it a kind.

Search with `kind:Crate`, `kind:none` or `kind:?` (alone or before a name). **Group by category** lists the rows
under one section per kind category, then `Uncategorized`, then `No kind`.

Icon names: `crate`, `box`, `enemy`, `person`, `door`, `key`, `coin`, `chest`, `flag`, `spawn`, `pickup`, `weapon`,
`vehicle`, `trigger`, `light`, `camera`, `sound`, `tree`, `rock`, `water`, `fire`, `star`, `gear`, `kind`, and the role
names above.

## Strictness

```toml
# Regia.toml
[kinds]
strictness = "warn"   # off (default) | warn | deny
```

With `warn` or `deny`, `regia preflight` and `regia cook` check every level: an entity with game components and no
kind, a kinded entity missing a required component, a `KindTag` naming no kind, and two markers on one entity. `warn`
prints them; `deny` makes them errors (preflight exits 1, the cook stops). "Two roles on one entity, consider a child"
is always a suggestion. Lights, cameras, plain meshes, group nodes and the parts under a kinded entity never need a
kind. The default, `off`, checks nothing, so existing projects are unchanged.

## The project map

```sh
regia describe --project .            # offline: kinds, prefabs, components, levels
regia describe --project . --live     # with the game running: plus the systems
```

writes `project-map.md` and `project-map.json`: one entry per kind and prefab with its components, which systems
read and write each component (with `--live`, from the running app's schedules), the levels using it and a "where to
look" index of the files that define each kind, prefab and level. The output is the same bytes for the same project,
so it can be committed and diffed; regenerate it when the project changes. Agents call `project.describe`
([agent page](../agents/entity-kinds.md)).

The `dedicated_server` sample has a data kind (`Crate`, on its crate prefab) and a code kind (`SpawnPoint`) at
`strictness = "warn"`.
