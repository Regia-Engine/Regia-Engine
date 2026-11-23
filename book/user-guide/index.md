# Crates and imports

A quick map of where things live, for when you know what you want and need the `use` line. The
concepts behind it are in [The ECS](../fundamentals/ecs.md) and
[App, plugins and schedules](../fundamentals/app-and-plugins.md).

## The shape of a Regia Engine game

A game is a Rust plugin registered into the engine's `App`:

```rust
use regia::prelude::*;

struct MyGame;

impl Plugin for MyGame {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_player)
            .add_systems(Update, move_camera);
    }
}

// Your systems: `spawn_player` runs once at startup, `move_camera` every frame.
fn spawn_player() {}
fn move_camera() {}

fn main() {
    regia::run(MyGame);
}
```

`regia::run` adds `DefaultPlugins` — platform (window, lifecycle, input messages), renderer,
assets, audio, telemetry, and (with the `ui`/`agent` features) the runtime UI and the agent
protocol server — before your plugin. Everything else is components, resources, systems and assets.

## Imports

`regia::prelude` re-exports the app, ECS, reflection and platform preludes (`App`, `Plugin`,
`Commands`, `Query`, `Res`, schedules, `Time`, window and input message types, the VFS, `tracing`
macros) and, with the `ui` feature, the runtime UI prelude. The umbrella crate also re-exports
some crates as modules: `regia::app`, `regia::core`, `regia::ecs`, `regia::framework`,
`regia::physics`, `regia::platform` and `regia::world`; with the `client` feature (on by default)
`regia::audio`, `regia::render`, `regia::telemetry` and `regia::two_d`; with the `server` feature
`regia::ai` and `regia::nav`; and with their own features `regia::agent`, `regia::crash`,
`regia::fluid`, `regia::lua`, `regia::media`, `regia::net`, `regia::procgen`, `regia::quick`,
`regia::steam`, `regia::ui` and `regia::xr`. Every other area is its own crate that your game depends on by
path (as the templates do), most with a prelude:

```rust
use regia::prelude::*;         // App, Plugin, Commands, Query, Res, schedules, platform, tracing
use regia_scene::prelude::*;   // Transform, Name, LevelManager, PrefabCommands, …
use regia_render::{MeshRenderer, PointLight, Camera};   // renderer has no prelude
use regia_framework::prelude::*;   // GamePlugin, PlayerManager, Character, GameRules, Script, …
use regia_input::prelude::*;   // InputConfig, InputPlugin, Actions
use regia_audio::prelude::*;   // AudioPlugin, Audio, …
use regia_animation::prelude::*;   // Animator, AnimationClip, …
use regia_physics::{RigidBody, ColliderShape, PhysicsQuery};   // physics has no prelude
use regia_vfx::{VfxPlugin, VfxRuntimePlugin, SpawnEffect};       // particles, decals
use regia_nav::{NavPlugin, NavMesh, NavAgent};                   // navigation
use regia_ai::{AiPlugin, BehaviorTree, Blackboard};              // behavior trees
use regia_matgraph::{MaterialGraph, MaterialGraphPlugin};        // material graphs
```

## The model

- **Entity** — a handle to a bundle of **components** (data). `Transform`, `Name`, `MeshRenderer`,
  `PointLight`, `RigidBody`, `Character`…
- **System** — a function that runs each frame (or on a schedule) and reads/writes components.
- **Resource** — singleton state (`Time`, `GameSettings`, `AssetServer`, `ClearColor`).
- **Asset** — a cooked file loaded through the `AssetServer` (mesh, texture, material, level,
  prefab, audio…).
- **Level / Prefab** — saved scenes as canonical RON (`.level.ron`, `.prefab.ron`).
