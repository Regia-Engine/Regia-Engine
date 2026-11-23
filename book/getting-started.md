# Your first project

<!-- Steps 1 to 5 run as written in crates/regia_cli/tests/getting_started_walkthrough.rs: keep every command, the Cargo.toml fence and the lib.rs fence runnable. -->

This chapter creates a project, runs it, puts something on screen, makes it move, opens it in
the editor and takes a screenshot through the agent protocol. It assumes the tools from
[Installing Regia Engine](start/install.md) are on your `PATH`.

## 1. Create the project

Open a terminal in the folder that should hold your projects (your home folder will do; not inside
the engine folder) and run:

```sh,walkthrough=new,template=blank
regia new "My Game"
```

`regia new <name>` writes a Cargo workspace in a new directory named after the snake_case form
of the name, here `my_game`, and prints its path (`--path DIR` chooses the parent directory; an
existing empty folder of that name is used, one with anything in it is refused). Letters and digits
are kept, spaces and dashes become `_`, other characters are dropped. A name that starts with a
digit, is a Rust or Java reserved word (the name is also the Android application id), or is a
Windows device name (`con`, `nul`, `com1`, `lpt1`...) is refused; the editor's project browser uses
the same rule. Pick a template with `--template`:

| Template | What you get |
|---|---|
| `blank` (default; `empty` is the same template) | An empty game plugin and a startup level with two entities. |
| `third_person` | A playable third-person game: rapier character movement, a boom camera, a procedural walking body with foot placement, coins, a three-hit sword combo against three enemies, death and respawn, a title screen with a settings menu, an Esc pause menu, procedural sound, quicksave on F5 and quickload on F9, and agent-protocol smoke tests. [A tour of the template](user-guide/third-person-template.md). |
| `first_person` | A first-person starter: WASD and mouse look, a crosshair, a gate and a button you use with E (with a prompt), a pause menu with a settings panel, a controls screen, quicksave, and agent-protocol smoke tests. [A tour of the template](user-guide/first-person-template.md). |
| `top_down` | A top-down starter: click or stick movement, a fixed-angle overhead camera (Q and E turn it), an enemy that walks a navmesh under a behavior tree with a health bar, an attack, quicksave on F5 and quickload on F9, and agent-protocol smoke tests. [A tour of the template](user-guide/top-down-template.md). |
| `quick` | One crate and one `main.rs`: the shortest start ([Quick start](user-guide/quick-start.md)). |
| `net-game` | A networked game: a dedicated server and a client of one shared simulation ([Networked game](tutorials/networked-game.md)). |

[Which template shows what](user-guide/project-templates.md) compares them side by side.

For example, a first-person and a top-down starter:

```sh,walkthrough=new,template=first_person
regia new "Rooftop Run" --template first_person
```

```sh,walkthrough=new,template=top_down
regia new "Garden Patrol" --template top_down
```

The editor's project browser and the `editor.new_project` agent method create the same
templates except `quick`.

The generated `game/Cargo.toml` depends on the engine by path (the checkout that built your
`regia` binary), so `cargo` always builds your game against exactly that engine. If you move
the engine, update the path. [Project structure](user-guide/project-structure.md) describes every
file; the ones you need now are:

- `game/src/lib.rs`: your gameplay, a `Plugin` named `Game`;
- `launcher/src/main.rs`: the desktop entry point, which calls `regia::run_with(my_game::Game, ...)`
  (with `--exit-after-frames N` it quits after N frames, for smoke runs);
- `Regia.toml`: the project manifest;
- `assets/levels/main.level.ron`: the startup level the editor opens.

## 2. Run it

```console
$ cd my_game
$ regia run
```

`regia run` cooks the assets, builds the launcher and starts it (`regia run --dev` also reloads
assets you save while it runs); `cargo run -p my_game_launcher` builds and runs it without the
cook.

The first build compiles the engine, which takes a few minutes; later builds only compile your
crates. A window opens. It is empty: the blank `Game` plugin adds no systems, there is no camera
and nothing to draw. `regia::run` has already set up logging, the window, input, the renderer,
the asset server, the runtime UI and the agent-protocol server (`DefaultPlugins`); everything
else comes from your plugin.

## 3. Put something on screen

To see anything you need three things: a camera, something to draw and some light. The renderer
reads them from components on entities. Add the engine crates the game will use to
`game/Cargo.toml`, next to the existing `regia` line. The paths below are an example: copy the path
prefix (everything before `/crates/regia`) from the `regia` line `regia new` wrote, which points at
your engine folder:

```toml
[dependencies]
regia = { path = "../../regia/crates/regia" }
regia_core = { path = "../../regia/crates/regia_core" }
regia_render = { path = "../../regia/crates/regia_render" }
regia_scene = { path = "../../regia/crates/regia_scene" }
```

Then replace `game/src/lib.rs` with:

```rust
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{Camera, CpuMaterial, CpuMesh, DirectionalLight, MeshRenderer, box_mesh};
use regia_scene::Transform;

/// The game plugin the launcher runs.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        // Transform propagation, levels and prefabs. `DefaultPlugins` does not add it.
        app.add_plugins(regia_scene::ScenePlugin)
            .add_systems(Startup, setup);
    }
}

/// Spawns a camera, a sun and a box.
fn setup(mut commands: Commands) {
    // The camera looks down its local -Z axis: stand back and tilt down a little.
    commands.spawn((
        Name::new("Camera"),
        Transform {
            translation: Vec3::new(0.0, 1.5, 5.0),
            rotation: Quat::from_rotation_x(-0.25),
            ..Transform::default()
        },
        Camera::default(),
    ));

    // A noon sun: 100 000 lux, travelling down and slightly forward.
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            direction: [0.3, -1.0, -0.4],
            color: [1.0, 0.98, 0.95],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));

    // A one-meter box with an orange material.
    let (vertices, indices) = box_mesh([0.5, 0.5, 0.5]);
    commands.spawn((
        Name::new("Box"),
        Transform::from_translation(Vec3::new(0.0, 0.5, 0.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.9, 0.45, 0.15, 1.0],
                roughness: 0.6,
                ..CpuMaterial::default()
            }),
        ),
    ));
}
```

Close the window and run it again:

```console
$ regia run
```

You see an orange box under a blue procedural sky. The sky also lights
the scene (image-based lighting); the sun adds direct light and casts the box's shadow (this
scene has no ground, so there is nothing for the shadow to fall on yet); auto exposure picks the
brightness. [Rendering](user-guide/rendering.md) and [The default look](user-guide/default-look.md)
explain each part.

A few things to notice:

- **Systems are plain functions.** `setup` takes `Commands`, a queue of world changes that apply
  after the system runs. Registering it in `Startup` runs it once before the first frame.
- **Components are plain structs.** `Transform`, `Camera`, `DirectionalLight` and
  `MeshRenderer` are components; an entity is the tuple you spawn.
- **`Transform` brings `GlobalTransform` with it** (a required component), which the renderer
  reads; `ScenePlugin` keeps it up to date every frame.
- **`Name`** here is the ECS name from `regia::prelude`, the one the agent protocol searches
  (step 5). Level files store `regia_scene::components::Name`; see
  [Scenes](user-guide/scenes.md#two-name-components).
- **Meshes and materials are shared.** `MeshRenderer` holds `Arc`s; the GPU upload is cached by
  the `Arc`'s identity, so a thousand boxes sharing one mesh upload it once.

## 4. Make it move

Add a component that marks things to spin, and a system that runs every frame:

```rust
use regia::prelude::*;
use regia_core::math::Quat;
use regia_scene::Transform;

/// Spins its entity around the vertical axis.
#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
pub struct Spin {
    /// Radians per second.
    pub speed: f32,
}

fn spin(time: Res<Time>, mut query: Query<(&Spin, &mut Transform)>) {
    for (spin, mut transform) in &mut query {
        transform.rotation = Quat::from_rotation_y(spin.speed * time.delta_secs()) * transform.rotation;
    }
}

pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_scene::ScenePlugin)
            .register_type::<Spin>()
            .add_systems(Update, spin);
    }
}
```

Insert `Spin { speed: 1.0 }` into the box's tuple in `setup`. `Query<(&Spin, &mut Transform)>`
visits every entity that has both components; `Res<Time>` is the frame clock. Deriving `Reflect`
and calling `register_type` makes `Spin` visible to the editor's inspector, to save games, to hot
reload and to the agent protocol. [The ECS](fundamentals/ecs.md) and
[Reflection](fundamentals/reflection.md) cover these in depth.

## 5. Look at it from an agent

While the launcher runs, open a second terminal in the project directory:

```console
$ regia agent call agent.hello
$ regia agent call scene.query_entities '{"name":"Box"}'
$ regia agent call seeing.screenshot --out shot.png
$ regia agent call quit.request
```

The last call closes the game as closing its window would, and `regia run` returns.

A dev build of every Regia Engine game runs the agent-protocol server. The game writes its endpoint
and a bearer token to `.regia/agent/` in the project, and `regia agent call` reads them. The
same calls are what an AI agent makes through `regia mcp`. See
[Agent protocol and MCP](user-guide/agent-protocol.md).

## 6. Open it in the editor

```console
$ regia-editor --project .
```

The editor opens the startup level from `Regia.toml` (`levels/main.level.ron`) in its own
edit world: a ground and a marker entity with transforms. The [editor tour](start/editor-tour.md)
walks through the panels. Your `setup` system spawns its entities at runtime, so they are not in
the level file; to author content in the editor, create entities there and save the level, then
load that level from your game ([Scenes](user-guide/scenes.md)).

## 7. Package it

From the project folder:

```sh,walkthrough=package
regia package --platform linux
```

`regia package` cooks the project's assets into `.fpak` archives, builds the launcher and
writes `dist/linux/<id>-linux.tar.gz`, a self-contained build that runs from any directory.
`--platform windows` and `--platform android` work the same way. See
[Running and iterating](start/running.md) and [Packaging](user-guide/packaging.md).

## Where next

- [Editor tour](start/editor-tour.md) and [Running and iterating](start/running.md) finish the
  getting-started section; [Checking your game with an agent](start/first-agent-check.md) runs a
  First Person project and checks it from a second terminal.
- [Fundamentals](fundamentals/ecs.md) explains the concepts this chapter used.
- [Tutorial: a third-person game](tutorials/third-person.md) builds a playable game.
