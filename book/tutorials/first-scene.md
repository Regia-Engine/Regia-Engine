# Tutorial: your first scene

[Your first project](../getting-started.md) put one box on screen. This tutorial builds a small
scene on purpose: a ground, a lamp post made of parts that move together, a row of spheres that
share one mesh, a sun and a point light, and a camera that frames it. Then it checks the result
from outside the game, with the agent protocol, and loads the same kind of content from a level
file. Steps 2 and 3 add functions to the file from the step before (merge the imports at the
top); the page ends with the whole `game/src/lib.rs`, which `cargo xtask book-check` compiles.

You need a project (`regia new "Scene Lab"`, then `cd scene_lab`) and the engine crates in
`game/Cargo.toml` that [Your first project](../getting-started.md#3-put-something-on-screen) lists:
`regia`, `regia_core`, `regia_render` and `regia_scene`. Run the game after each step with
`cargo run -p scene_lab_launcher`.

## Step 1: a ground and a camera

The renderer reads three kinds of things from the world: a `Camera`, lights, and entities with a
`MeshRenderer`. Positions come from `Transform`. The engine is right-handed with +Y up, and a
camera looks down its own −Z axis, so to look at the origin from the +Z side you stand back and
tilt down.

```rust
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{Camera, CpuMaterial, CpuMesh, MeshRenderer, box_mesh};
use regia_scene::Transform;

/// The game plugin the launcher runs.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_scene::ScenePlugin)
            .add_systems(Startup, spawn_ground_and_camera);
    }
}

/// A flat gray slab and a camera standing back from it.
fn spawn_ground_and_camera(mut commands: Commands) {
    commands.spawn((
        Name::new("Camera"),
        Transform {
            translation: Vec3::new(0.0, 4.0, 12.0),
            rotation: Quat::from_rotation_x(-0.3),
            ..Transform::default()
        },
        Camera::default(),
    ));

    // `box_mesh` takes half extents: this slab is 30 m by 0.2 m by 30 m, its top face at y = 0.
    let (vertices, indices) = box_mesh([15.0, 0.1, 15.0]);
    commands.spawn((
        Name::new("Ground"),
        Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.35, 0.33, 0.30, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            }),
        ),
    ));
}
```

You see a gray slab under the default procedural sky. The sky already lights it a little
(image-based lighting), which is why it is not black; there is no sun yet, so there are no
shadows.

## Step 2: a sun, and objects that share a mesh

Add a `DirectionalLight` (the sun) and a row of spheres. The sun's `direction` is the way the
light *travels*, so a sun high in the sky shining down and slightly forward points mostly along
−Y. `illuminance` is in lux: 100 000 is a clear noon.

The row of spheres shows the rule that keeps scenes cheap: **a mesh and a material are shared
through `Arc`**. The GPU copy is cached by the `Arc`'s identity, so five spheres built from one
`Arc<CpuMesh>` upload one mesh.

Add these functions to the file from step 1, merging the imports at the top:

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_render::{CpuMaterial, CpuMesh, DirectionalLight, MeshRenderer, uv_sphere};
use regia_scene::Transform;

/// A noon sun with shadows.
pub fn spawn_sun(commands: &mut Commands) {
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            direction: [-0.4, -1.0, -0.3],
            color: [1.0, 0.97, 0.92],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
}

/// Five spheres, one mesh, five materials of increasing roughness.
pub fn spawn_spheres(commands: &mut Commands) {
    let (vertices, indices) = uv_sphere(0.6, 32, 16);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    for i in 0..5 {
        let roughness = 0.1 + 0.2 * i as f32;
        commands.spawn((
            Name::new(format!("Sphere {i}")),
            Transform::from_translation(Vec3::new(-4.0 + 2.0 * i as f32, 0.6, 2.0)),
            MeshRenderer::new(
                Arc::clone(&mesh),
                Arc::new(CpuMaterial {
                    base_color: [0.8, 0.2, 0.15, 1.0],
                    metallic: 0.0,
                    roughness,
                    ..CpuMaterial::default()
                }),
            ),
        ));
    }
}
```

Call both from the startup system (`spawn_sun(&mut commands); spawn_spheres(&mut commands);`).
Run it: the left sphere is glossy and the right one matte, and every sphere throws a shadow on the
ground. Each material is its own `Arc`, because each differs; only the mesh is shared.

## Step 3: a parent and its children

A lamp post is a pole and a lamp head. If you move the post, the head should come with it. Make
the pole the **parent** and the head a **child**: a child's `Transform` is relative to its parent,
and `ScenePlugin` propagates the result to the child's `GlobalTransform`, which is what the
renderer reads.

Add this function to the file from step 2, merging the imports at the top:

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer, PointLight, box_mesh, uv_sphere};
use regia_scene::Transform;
use regia_scene::hierarchy::HierarchyCommands;

/// A lamp post at `base`: a pole, and a glowing head that is a child of the pole.
pub fn spawn_lamp_post(commands: &mut Commands, base: Vec3) {
    let (vertices, indices) = box_mesh([0.08, 1.5, 0.08]);
    let pole = commands
        .spawn((
            Name::new("Lamp Post"),
            // The pole's origin is its center, so lift it by half its 3 m height.
            Transform::from_translation(base + Vec3::new(0.0, 1.5, 0.0)),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(CpuMaterial {
                    base_color: [0.1, 0.1, 0.12, 1.0],
                    metallic: 0.8,
                    roughness: 0.4,
                    ..CpuMaterial::default()
                }),
            ),
        ))
        .id();

    let (vertices, indices) = uv_sphere(0.2, 16, 8);
    let head = commands
        .spawn((
            Name::new("Lamp Head"),
            // Relative to the pole: at its top.
            Transform::from_translation(Vec3::new(0.0, 1.6, 0.0)),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(CpuMaterial {
                    base_color: [1.0, 0.9, 0.7, 1.0],
                    emissive: [1.0, 0.8, 0.5],
                    emissive_intensity: 20.0,
                    ..CpuMaterial::default()
                }),
            ),
        ))
        .id();
    commands.entity(head).set_parent(pole);

    // The light is a second child of the pole, at the head's local position. A point
    // light's position in the world is its entity's `GlobalTransform` — the `position`
    // field is ignored on a component — so parenting the light moves it with the post.
    let light = commands
        .spawn((
            Name::new("Lamp Light"),
            Transform::from_translation(Vec3::new(0.0, 1.6, 0.0)),
            PointLight {
                position: [0.0; 3], // unused as a component: the transform carries it
                color: [1.0, 0.8, 0.5],
                intensity: 800.0,
                radius: 12.0,
                cast_shadows: false,
            },
        ))
        .id();
    commands.entity(light).set_parent(pole);
}
```

Two things to notice. First, the head's `Transform` says `y = 1.6` and nothing about where the
pole is; its place in the world is the parent's transform times its own. Change the pole's
translation (in code, in the editor or through the agent) and the head follows. Second, the point
light is a second child of the pole at the same local position, so it follows too: like every
light except the sun, its position in the world comes from its entity's `GlobalTransform`
(the `position` field exists for extracted lights, not for the component). The sun, by
contrast, has no position at all.

Call `spawn_lamp_post(&mut commands, Vec3::new(4.0, 0.0, -2.0))`. Run it: at noon neither the
emissive head (20 units, against a sunlit ground of about 9 000) nor the 800 lm point light (about
7 lux at the ground, 1/15 000 of the sun) shows. `intensity` is lumens, and the auto exposure
(which settles on the new average brightness over a second or so) keys on the sun. Set the sun's
`illuminance` to a deep-twilight 5.0 and rerun: the head glows and the lamp throws a pool of light
around the post.

## Step 4: look at it without looking at it

Leave the game running and, in a second terminal in the project directory, ask the agent protocol
what is in the world. This is how a script (or an AI agent) checks a scene: it does not need to
read pixels to know that a lamp post has a child.

```console
$ regia agent call scene.query_entities '{"name":"Lamp*"}'
$ regia agent call scene.query_entities '{"component":"MeshRenderer","near":{"point":[0,0,2],"radius":5}}'
$ regia agent call seeing.screenshot --out scene.png
```

The first call lists the pole, the head and the light with their translations; the head's and
the light's are local to the pole, so both read `(0, 1.6, 0)`. The second finds **seven**
entities, not five: the Ground, the five spheres and the Lamp Head. A spatial filter compares
each entity's *local* `Transform` translation, not its world position — the Lamp Head is about
6.5 m from `(0, 0, 2)` in the world (its world position is `(4, 3.1, −2)`: the pole's origin plus
the head's 1.6 m local offset), but its local translation is only 2.6 m away, so it
matches (and the Lamp Post, whose local translation is its world position at 5.9 m, does not).
The third writes a PNG of the window.

Move something and see it change, without recompiling. Take the id from the query result:

```console
$ regia agent call scene.set_property '{"entity": 4294967297, "component": "Transform", "path": "translation.x", "value": 7.0}'
```

`scene.set_property` edits the live world. It is a debugging tool: the next run starts from your
code again. [Scene: finding, inspecting and editing entities](../agents/scene.md) lists the other
calls, including `scene.spawn` and `scene.reparent`.

## Step 5: move the scene into a level file

Everything above is Rust code, which is the right place for gameplay and generated content. Static
content, like this ground and these spheres, belongs in a **level**: a diffable `.level.ron` file
that the editor and the agent protocol can both edit. Open the project in the editor:

```console
$ regia-editor --project .
```

Create entities with the editor's **File → Create entity** menu, give them `Transform`s, and save: the level lands in
`assets/levels/`. A level file is plain text, one entity per record, with a stable `guid`:

```ron
(
    format_version: 13,
    entities: [
        (
            guid: "5c2d7a10-0000-4000-8000-000000000001",
            name: "Pedestal",
            components: {
                "regia_scene::transform::Transform": (
                    translation: (0.0, 0.5, 0.0),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
            },
        ),
    ],
)
```

To run the level in your game, read it in a `Startup` system and open it, exactly as
[Scenes](../user-guide/scenes.md#loading-a-level-from-your-game) shows (`Level::from_ron_str`,
`LevelLibrary`, `open_level`). The editor does not run your `Startup` systems, which is why the
code-built scene above is not in the level it opens; the split is the point. Static layout lives
in the level, and code adds what changes.

## The whole file

Replace `game/src/lib.rs` with this. It is checked by `cargo xtask book-check`, so it compiles
against the engine as the book describes it.

```rust
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, MeshRenderer, PointLight, box_mesh, uv_sphere,
};
use regia_scene::Transform;
use regia_scene::hierarchy::HierarchyCommands;

/// The game plugin the launcher runs.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_scene::ScenePlugin)
            .add_systems(Startup, spawn_scene);
    }
}

/// The ground, the camera, the sun, the spheres and the lamp post.
fn spawn_scene(mut commands: Commands) {
    commands.spawn((
        Name::new("Camera"),
        Transform {
            translation: Vec3::new(0.0, 4.0, 12.0),
            rotation: Quat::from_rotation_x(-0.3),
            ..Transform::default()
        },
        Camera::default(),
    ));

    // `box_mesh` takes half extents: this slab is 30 m by 0.2 m by 30 m, its top face at y = 0.
    let (vertices, indices) = box_mesh([15.0, 0.1, 15.0]);
    commands.spawn((
        Name::new("Ground"),
        Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.35, 0.33, 0.30, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            }),
        ),
    ));

    spawn_sun(&mut commands);
    spawn_spheres(&mut commands);
    spawn_lamp_post(&mut commands, Vec3::new(4.0, 0.0, -2.0));
}

/// A noon sun with shadows.
pub fn spawn_sun(commands: &mut Commands) {
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            direction: [-0.4, -1.0, -0.3],
            color: [1.0, 0.97, 0.92],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
}

/// Five spheres, one mesh, five materials of increasing roughness.
pub fn spawn_spheres(commands: &mut Commands) {
    let (vertices, indices) = uv_sphere(0.6, 32, 16);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    for i in 0..5 {
        let roughness = 0.1 + 0.2 * i as f32;
        commands.spawn((
            Name::new(format!("Sphere {i}")),
            Transform::from_translation(Vec3::new(-4.0 + 2.0 * i as f32, 0.6, 2.0)),
            MeshRenderer::new(
                Arc::clone(&mesh),
                Arc::new(CpuMaterial {
                    base_color: [0.8, 0.2, 0.15, 1.0],
                    metallic: 0.0,
                    roughness,
                    ..CpuMaterial::default()
                }),
            ),
        ));
    }
}

/// A lamp post at `base`: a pole, a glowing head, and the light, all moving together.
pub fn spawn_lamp_post(commands: &mut Commands, base: Vec3) {
    let (vertices, indices) = box_mesh([0.08, 1.5, 0.08]);
    let pole = commands
        .spawn((
            Name::new("Lamp Post"),
            // The pole's origin is its center, so lift it by half its 3 m height.
            Transform::from_translation(base + Vec3::new(0.0, 1.5, 0.0)),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(CpuMaterial {
                    base_color: [0.1, 0.1, 0.12, 1.0],
                    metallic: 0.8,
                    roughness: 0.4,
                    ..CpuMaterial::default()
                }),
            ),
        ))
        .id();

    let (vertices, indices) = uv_sphere(0.2, 16, 8);
    let head = commands
        .spawn((
            Name::new("Lamp Head"),
            // Relative to the pole: at its top.
            Transform::from_translation(Vec3::new(0.0, 1.6, 0.0)),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(CpuMaterial {
                    base_color: [1.0, 0.9, 0.7, 1.0],
                    emissive: [1.0, 0.8, 0.5],
                    emissive_intensity: 20.0,
                    ..CpuMaterial::default()
                }),
            ),
        ))
        .id();
    commands.entity(head).set_parent(pole);

    // The light is a second child of the pole, at the head's local position. A point
    // light's position in the world is its entity's `GlobalTransform` — the `position`
    // field is ignored on a component — so parenting the light moves it with the post.
    let light = commands
        .spawn((
            Name::new("Lamp Light"),
            Transform::from_translation(Vec3::new(0.0, 1.6, 0.0)),
            PointLight {
                position: [0.0; 3], // unused as a component: the transform carries it
                color: [1.0, 0.8, 0.5],
                intensity: 800.0,
                radius: 12.0,
                cast_shadows: false,
            },
        ))
        .id();
    commands.entity(light).set_parent(pole);
}
```

## What you built, and where next

You used `Transform`, `Camera`, `DirectionalLight`, `PointLight` and `MeshRenderer`; shared one
mesh between several entities; built a parent and a child; checked all of it through the agent
protocol; and met the level file. Next:

- [Tutorial: a game loop with input](game-loop.md) makes the scene respond to the keyboard.
- [Tutorial: your first baked light](baked-light.md) lights a scene with path-traced indirect
  light, instead of a flat ambient term.
- [Materials](../user-guide/materials.md) and [Rendering](../user-guide/rendering.md) go deeper on
  what you set here.
