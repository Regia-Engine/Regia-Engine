# Tutorial: your first baked light

Real-time lighting estimates how light bounces around a scene every frame, with a budget of a
few milliseconds. A **baked** light does that work once, ahead of time, with the reference path
tracer, and stores the answer in a file. At runtime the renderer looks the answer up: path-traced
bounce light, sky light and emitted light at a small fixed cost, with no settling time after a
load or a camera cut. The catch is that the answer is for the scene as it was when you baked it.

This tutorial bakes the light for a small outdoor scene, attaches the result to a level, runs the
game with it, and checks, with the agent protocol, that the bake is really in use. It uses the
`rolling-hills` scene that ships inside the `regia` tool, so you do not need any art.

You need a project ([Your first project](../getting-started.md)), the crates `regia`,
`regia_core`, `regia_render` and `regia_scene` in `game/Cargo.toml`, and **a GPU**: the bake
runs on the first graphics adapter (`--dry-run` does not). The background and every runtime
control are in [Baked GI volumes](../agents/baked-gi.md); this page is the guided walk.

## What gets baked

A bake stores a volume of **probes**: points in space where the tracer recorded the light arriving
from every direction, as four terms (bounce of the sun, sky, sky-body fill, and emitted light),
plus distances that tell the lookup which probes can see a surface. The probes are laid out in a
layer that follows the terrain, with optional finer **bricks** around things that need detail.
The file is a `.fgiv`.

What is *not* baked: direct sun light and its shadows (the sun is real-time, as always), and
anything that moves. Moving things are lit from the volume at runtime, and can darken it near them
with capsule occluders ([Dynamic occluders](../agents/baked-gi.md#dynamic-occluders)).

## Step 1: see what a bake would do

```console
$ regia bake-gi rolling-hills --dry-run
```

`rolling-hills` is a built-in scene: a 256 m procedural heightfield, one boulder, a sun and a
uniform sky. A dry run loads the scene, lays out the probes and prints one JSON object (progress
goes to stderr) without tracing anything. Look at three things in it:

- `layers.terrain`: the terrain layer's origin, spacing and counts. Fewer, wider-spaced probes
  bake faster and smooth the light more.
- `layers.probes`: the total number of probes. This is the number that sets the file size and
  the bake time.
- `ok: true`. A description error prints `{"ok": false, "code": ..., "message": ...}` and exits
  non-zero. The codes are listed in [Baking GI from meshes](../agents/baked-gi-bake.md#errors).

## Step 2: bake

```console
$ mkdir -p assets/maps
$ regia bake-gi rolling-hills --out assets/maps/hills.fgiv
```

The command traces from every probe on the GPU and writes `assets/maps/hills.fgiv`. It takes
from seconds to a few minutes, depending on the GPU and the settings. The output object adds the
`report`: how many probes are `valid`, how many were `inside_geometry` or `under_terrain` (probes
the bake had to move or throw out), and how many rays were cast. A healthy bake has almost every
probe valid. `--rays`, `--samples`, `--bounces` and `--seed` trade quality for time; a fixed
`--seed` makes the result repeatable.

## Step 3: a scene that matches the bake

Light that was baked for one scene is only right for that scene. Your game must put the same
terrain under the sun, or the lookup will light a surface that is not where the bake thought it
was. The bake's heightfield is a known function, so the game builds the same hills as a mesh.
This step and the next add functions to `game/src/lib.rs`; the page ends with the whole file,
which `cargo xtask book-check` compiles. Start from a fresh `regia new` project, or delete the
`setup` system (and its camera and sun) that [Your first project](../getting-started.md) put in
`lib.rs` — keeping it would spawn a second camera and a second sun, and the bake's checks would
compare against the wrong light.

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer, uv_sphere,
};
use regia_scene::Transform;

/// Samples per side of the ground mesh: 129 vertices at 2 m is the bake's 256 m square.
const GRID: usize = 129;
/// Meters between ground vertices.
const SPACING: f32 = 2.0;

/// The height of the `rolling-hills` bake scene at (`x`, `z`), meters.
fn hills_height(x: f32, z: f32) -> f32 {
    6.0 * (x * 0.045).sin() * (z * 0.037).cos() + 3.0 * (x * 0.11 + z * 0.07).sin()
}

/// The slope of `hills_height` along x and z, for vertex normals.
fn hills_slope(x: f32, z: f32) -> (f32, f32) {
    let w = x * 0.11 + z * 0.07;
    let dx = 6.0 * 0.045 * (x * 0.045).cos() * (z * 0.037).cos() + 3.0 * 0.11 * w.cos();
    let dz = -6.0 * 0.037 * (x * 0.045).sin() * (z * 0.037).sin() + 3.0 * 0.07 * w.cos();
    (dx, dz)
}

/// A grid mesh over the hills, centered on the origin.
fn hills_mesh() -> CpuMesh {
    let half = (GRID - 1) as f32 * SPACING * 0.5;
    let mut vertices = Vec::with_capacity(GRID * GRID);
    for j in 0..GRID {
        for i in 0..GRID {
            let (x, z) = (i as f32 * SPACING - half, j as f32 * SPACING - half);
            let (dx, dz) = hills_slope(x, z);
            let normal = Vec3::new(-dx, 1.0, -dz).normalize();
            vertices.push(LabVertex {
                position: [x, hills_height(x, z), z],
                normal: normal.to_array(),
                uv: [i as f32 / (GRID - 1) as f32, j as f32 / (GRID - 1) as f32],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    let mut indices = Vec::with_capacity((GRID - 1) * (GRID - 1) * 6);
    for j in 0..GRID - 1 {
        for i in 0..GRID - 1 {
            let a = (j * GRID + i) as u32;
            let b = a + 1;
            let d = a + GRID as u32;
            let c = d + 1;
            // Counter-clockwise seen from above (+Y): a -> d -> c, a -> c -> b.
            indices.extend_from_slice(&[a, d, c, a, c, b]);
        }
    }
    CpuMesh { vertices, indices }
}

/// Camera, sun, hills and the bake's boulder.
fn spawn_scene(mut commands: Commands) {
    commands.spawn((
        Name::new("Camera"),
        Transform {
            translation: Vec3::new(-30.0, 6.0, 30.0),
            // Yaw toward +X / -Z, pitch down a little.
            rotation: Quat::from_rotation_y(-0.785) * Quat::from_rotation_x(-0.2),
            ..Transform::default()
        },
        Camera::default(),
    ));

    // The bake's sun is `toward: (0.4, 0.6, 0.3)`, the direction *to* the sun. The light
    // travels the opposite way.
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            direction: [-0.4, -0.6, -0.3],
            color: [1.0, 0.96, 0.9],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));

    commands.spawn((
        Name::new("Hills"),
        Transform::default(),
        MeshRenderer::new(
            Arc::new(hills_mesh()),
            Arc::new(CpuMaterial {
                base_color: [0.35, 0.3, 0.26, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            }),
        ),
    ));

    // The bake's boulder: radius 4 m at (10, 2, -6), mostly below the hill.
    let (vertices, indices) = uv_sphere(4.0, 32, 16);
    commands.spawn((
        Name::new("Boulder"),
        Transform::from_translation(Vec3::new(10.0, 2.0, -6.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.4, 0.4, 0.4, 1.0],
                roughness: 0.8,
                ..CpuMaterial::default()
            }),
        ),
    ));
}
```

The numbers are not arbitrary. They are the ones in the `rolling-hills` definition: the height
formula, the sun's `toward` vector `(0.4, 0.6, 0.3)` and illuminance 100 000 lux, and the boulder.
The bake grid is 1 m and the mesh 2 m, so the rendered surface is a little coarser than the
traced one; for a tutorial that is invisible. A real project bakes the *same meshes* it renders.
[Baking GI from meshes](../agents/baked-gi-bake.md) describes how a `.bake.ron` names them.

## Step 4: a level that names its bake

A level carries its bake in its look settings: when the level opens, the framework loads the
volume and installs it in the renderer. Create `assets/levels/hills.level.ron`:

```ron
(
    format_version: 13,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (
            look: (
                baked_gi: Some((
                    volume: "maps/hills.fgiv",
                )),
            ),
        ),
    },
)
```

`volume` is a path under `assets/`. Open the level from `Startup`, next to the scene, with the same
loader as in [Scenes](../user-guide/scenes.md#loading-a-level-from-your-game). Add this function
to the same file (merging the imports at the top):

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
use regia::core::id::Guid;
use regia::prelude::*;
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};

/// The id this game uses for the hills level.
const HILLS_LEVEL: LevelId = LevelId(Guid::from_u128(0x6869_6c6c_0000_4000_8000_0000_0000_0001));

/// Reads `levels/hills.level.ron` and opens it. The level names the bake; the framework installs it.
pub fn load_hills_level(world: &mut World) {
    let Ok(path) = VfsPath::parse("assets://levels/hills.level.ron") else {
        return;
    };
    let bytes = match world.resource::<Vfs>().read_sync(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            error!("cannot read the hills level: {err}");
            return;
        }
    };
    let level = match Level::from_ron_str(HILLS_LEVEL, &String::from_utf8_lossy(&bytes)) {
        Ok(level) => level,
        Err(err) => {
            error!("the hills level does not parse: {err}");
            return;
        }
    };
    world.get_resource_or_insert_with(LevelLibrary::new).insert(level);
    if let Err(err) = open_level(world, HILLS_LEVEL, LoadMode::Replace) {
        error!("cannot open the hills level: {err}");
    }
}
```

## The whole file

Replace `game/src/lib.rs` with this (a fresh project's blank `Game::build` body, or in place of
the getting-started `setup`). It is checked by `cargo xtask book-check`, so it compiles against
the engine as the book describes it.

```rust
use std::sync::Arc;

use regia::core::id::Guid;
use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer, uv_sphere,
};
use regia_scene::Transform;
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};

/// Samples per side of the ground mesh: 129 vertices at 2 m is the bake's 256 m square.
const GRID: usize = 129;
/// Meters between ground vertices.
const SPACING: f32 = 2.0;

/// The id this game uses for the hills level.
const HILLS_LEVEL: LevelId = LevelId(Guid::from_u128(0x6869_6c6c_0000_4000_8000_0000_0000_0001));

/// The game plugin the launcher runs.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_scene::ScenePlugin)
            .add_systems(Startup, (load_hills_level, spawn_scene));
    }
}

/// The height of the `rolling-hills` bake scene at (`x`, `z`), meters.
fn hills_height(x: f32, z: f32) -> f32 {
    6.0 * (x * 0.045).sin() * (z * 0.037).cos() + 3.0 * (x * 0.11 + z * 0.07).sin()
}

/// The slope of `hills_height` along x and z, for vertex normals.
fn hills_slope(x: f32, z: f32) -> (f32, f32) {
    let w = x * 0.11 + z * 0.07;
    let dx = 6.0 * 0.045 * (x * 0.045).cos() * (z * 0.037).cos() + 3.0 * 0.11 * w.cos();
    let dz = -6.0 * 0.037 * (x * 0.045).sin() * (z * 0.037).sin() + 3.0 * 0.07 * w.cos();
    (dx, dz)
}

/// A grid mesh over the hills, centered on the origin.
fn hills_mesh() -> CpuMesh {
    let half = (GRID - 1) as f32 * SPACING * 0.5;
    let mut vertices = Vec::with_capacity(GRID * GRID);
    for j in 0..GRID {
        for i in 0..GRID {
            let (x, z) = (i as f32 * SPACING - half, j as f32 * SPACING - half);
            let (dx, dz) = hills_slope(x, z);
            let normal = Vec3::new(-dx, 1.0, -dz).normalize();
            vertices.push(LabVertex {
                position: [x, hills_height(x, z), z],
                normal: normal.to_array(),
                uv: [i as f32 / (GRID - 1) as f32, j as f32 / (GRID - 1) as f32],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    let mut indices = Vec::with_capacity((GRID - 1) * (GRID - 1) * 6);
    for j in 0..GRID - 1 {
        for i in 0..GRID - 1 {
            let a = (j * GRID + i) as u32;
            let b = a + 1;
            let d = a + GRID as u32;
            let c = d + 1;
            // Counter-clockwise seen from above (+Y): a -> d -> c, a -> c -> b.
            indices.extend_from_slice(&[a, d, c, a, c, b]);
        }
    }
    CpuMesh { vertices, indices }
}

/// Reads `levels/hills.level.ron` and opens it. The level names the bake; the framework installs it.
pub fn load_hills_level(world: &mut World) {
    let Ok(path) = VfsPath::parse("assets://levels/hills.level.ron") else {
        return;
    };
    let bytes = match world.resource::<Vfs>().read_sync(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            error!("cannot read the hills level: {err}");
            return;
        }
    };
    let level = match Level::from_ron_str(HILLS_LEVEL, &String::from_utf8_lossy(&bytes)) {
        Ok(level) => level,
        Err(err) => {
            error!("the hills level does not parse: {err}");
            return;
        }
    };
    world.get_resource_or_insert_with(LevelLibrary::new).insert(level);
    if let Err(err) = open_level(world, HILLS_LEVEL, LoadMode::Replace) {
        error!("cannot open the hills level: {err}");
    }
}

/// Camera, sun, hills and the bake's boulder.
fn spawn_scene(mut commands: Commands) {
    commands.spawn((
        Name::new("Camera"),
        Transform {
            translation: Vec3::new(-30.0, 6.0, 30.0),
            // Yaw toward +X / -Z, pitch down a little.
            rotation: Quat::from_rotation_y(-0.785) * Quat::from_rotation_x(-0.2),
            ..Transform::default()
        },
        Camera::default(),
    ));

    // The bake's sun is `toward: (0.4, 0.6, 0.3)`, the direction *to* the sun. The light
    // travels the opposite way.
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            direction: [-0.4, -0.6, -0.3],
            color: [1.0, 0.96, 0.9],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));

    commands.spawn((
        Name::new("Hills"),
        Transform::default(),
        MeshRenderer::new(
            Arc::new(hills_mesh()),
            Arc::new(CpuMaterial {
                base_color: [0.35, 0.3, 0.26, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            }),
        ),
    ));

    // The bake's boulder: radius 4 m at (10, 2, -6), mostly below the hill.
    let (vertices, indices) = uv_sphere(4.0, 32, 16);
    commands.spawn((
        Name::new("Boulder"),
        Transform::from_translation(Vec3::new(10.0, 2.0, -6.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.4, 0.4, 0.4, 1.0],
                roughness: 0.8,
                ..CpuMaterial::default()
            }),
        ),
    ));
}
```

Run the game. A missing or unreadable volume logs one warning and changes nothing: the frame keeps
its other indirect light, so a typo in the path costs you the bake, not the game.

## Step 5: prove the bake is in use

A screenshot that "looks lit" proves little. Ask the engine. With the game running:

```console
$ regia agent call level.get_baked_gi
```

The reply you want has `"source": "level"`, `"error": null`, and `probes` equal to the probe count
from the bake. `error_code` is `volume_not_found` when the file is not under `assets/` at that
path, and `volume_unreadable` when it is damaged or from another engine version (rebake). Then
check the renderer:

```console
$ regia agent call play.pause
$ regia agent call play.step '{"frames":2}'
$ regia agent call render.get_gi_mode
```

`baked.active` must be `true`, and `baked.error` `null`. `uses_bake` in the same reply says whether
the current look would use an installed volume at all: a chosen Balanced preset does not, so a
volume can be `installed` and still not `active`. Those are two different questions, and the second
is the one that matters.

## Step 6: compare

Capture a frame with the bake, remove it, step, capture again:

```console
$ regia agent call seeing.screenshot --out with-bake.png
$ regia agent call level.set_look '{"look":{"baked_gi":null}}'
$ regia agent call play.step '{"frames":2}'
$ regia agent call seeing.screenshot --out without-bake.png
```

Compare the two images. The baked frame has path-traced indirect light: shadowed slopes pick up
warm bounce from the sunlit ground and cool light from the sky, and the hollows under the boulder
are darker. The other frame falls back to the analytic or probe-based terms. Neither is "right"
in an absolute sense; the bake is the one that follows the reference tracer, at a lower runtime
cost. [Baked GI volumes](../agents/baked-gi.md) has the per-preset table of what the bake replaces.

If the colors look wrong rather than merely different, the likeliest cause is the sky: the
built-in scene was baked under a *uniform* sky of fixed radiance, and the game renders a
procedural one. Scale the baked sky term with the level's settings (`settings: (sky_scale:
Some(0.8))` inside `baked_gi`) until it fits, or bake with a description whose sky matches your
look. A sun that moved more than 3 degrees from the baked one logs a warning once; it means the
bounce and shadows no longer agree, and a rebake is due.

## When to rebake

The volume is a snapshot. Rebake after you change: the terrain or any static mesh, a static
light's position or color, the sun's direction, or an interior. You do not rebake for changes to
light *levels*: the sun-bounce term follows the frame's sun intensity and the fill term follows its
sky-body fill, so exposure and brightness changes still apply. Moving objects are never in the
bake; give them capsule occluders if you want them to darken the light next to them.

## What you did, and where next

You inspected a bake with `--dry-run`, baked a volume, built a scene that matches it, attached it
to a level, confirmed through `level.get_baked_gi` and `render.get_gi_mode` that it is installed
*and* active, and compared with and without it. Next:

- [Baking GI from meshes](../agents/baked-gi-bake.md): your own `.bake.ron` with static meshes,
  interiors and automatic detail bricks.
- [Baked GI from the level](../agents/baked-gi-maps.md): the level fields, hot reload and the
  failure codes.
- [Lighting, shadows and GI](../systems/lighting.md): where the bake sits among the other
  lighting terms.
