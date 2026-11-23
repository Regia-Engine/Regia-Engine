# Tutorial: a game loop with input

This tutorial builds **Orb Run**, the smallest thing that is a game: you move a box around an
arena with the keyboard or a gamepad, orbs sit on the floor, touching one collects it, and
collecting them all wins. It is about the *loop*: how input becomes movement, why simulation runs
in `FixedUpdate` and input is read in `Update`, how systems are ordered, how a system tells another
one something happened, and how to test the whole thing without touching a key.

You need a project ([Your first project](../getting-started.md)) with these crates in
`game/Cargo.toml`: `regia`, `regia_core`, `regia_input`, `regia_render` and
`regia_scene` (the [first-scene tutorial](first-scene.md) shows the lines). The page ends with the
whole `game/src/lib.rs`; the sections before it explain each part, so you can read first and paste
once.

## The loop, in one picture

A frame runs these schedules in order (see [App, plugins and schedules](../fundamentals/app-and-plugins.md)):

| Schedule | Orb Run puts here |
|---|---|
| `PreUpdate` | the engine's `InputPlugin` evaluates bindings into `Actions` (its own systems, not yours) |
| `FixedUpdate` | `move_player`, then `collect_orbs`: the simulation, at a fixed 60 Hz |
| `Update` | `read_intent`: read the input actions, store what the player wants; `spin_orbs` (cosmetic) |

Input is evaluated once per frame, before the simulation, and the simulation consumes it zero or
more times per frame. `read_intent` runs in `Update`, *after* that frame's fixed steps, so a key
press reaches `MoveIntent` one frame after it was evaluated — one frame of latency, the same at
any frame rate. The win check lives inside `collect_orbs`, in `FixedUpdate`. That
split keeps the simulation independent of the frame rate: at 30 fps `FixedUpdate` runs twice per
frame and at 240 fps it runs on one frame in four, and the player moves the same distance per
second either way. It also keeps the simulation deterministic, which is what lets you test it.

## Step 1: name your inputs

Gameplay code never asks "is W down". It asks for an **action**, a named input with a kind, and the
bindings (keys, sticks, buttons) are data. Orb Run has one action, `Move`, a 2D axis:

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .binding(Binding::new("Move", InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")))
        .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
}
```

`keys2d(up, down, left, right)` turns four keys into one axis whose y is "forward"; the left
stick is a second binding on the same action, so both work at once. Keys are named by physical
position (`KeyW` is the key in W's place on a US layout, wherever your layout puts the letter), so
the controls stay under the same fingers on an AZERTY keyboard. [Input](../user-guide/input.md)
covers modifiers (dead zones, curves), triggers (hold, tap), contexts and rebinding.

## Step 2: read input in `Update`, simulate in `FixedUpdate`

`read_intent` copies the action into a `MoveIntent` resource. `move_player` reads that resource
at the fixed step:

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
fn read_intent(actions: Res<Actions>, mut intent: ResMut<MoveIntent>) {
    intent.0 = actions.axis2d("Move").clamp_length_max(1.0);
}

fn move_player(
    intent: Res<MoveIntent>,
    fixed: Res<FixedTime>,
    mut players: Query<&mut Transform, With<Player>>,
) {
    let dt = fixed.step().as_secs_f32();
    for mut transform in &mut players {
        // The axis's y is "forward", which is -Z in the world.
        let step = Vec3::new(intent.0.x, 0.0, -intent.0.y) * PLAYER_SPEED * dt;
        let moved = transform.translation + step;
        transform.translation = Vec3::new(
            moved.x.clamp(-ARENA_LIMIT, ARENA_LIMIT),
            moved.y,
            moved.z.clamp(-ARENA_LIMIT, ARENA_LIMIT),
        );
    }
}
```

Three habits to take from it:

- **Clamp the axis.** `keys2d` already normalizes a diagonal of length √2 to length 1, but
  several bindings on one action (keys plus a gamepad stick pushed past the dead zone) can still
  add past 1; `clamp_length_max(1.0)` keeps combined input from being faster than a single one.
- **Use the step, not the frame delta.** Inside `FixedUpdate`, `Res<Time>` still reports the
  *frame's* delta. The step length is `FixedTime::step()`.
- **Do not read edges in `FixedUpdate`.** `just_pressed` is true for one frame, and a fixed step may
  not run on that frame. Continuous values (held keys, sticks) are safe to read anywhere. For a
  button press, read it in `Update` and store it, as `read_intent` does for the stick.

## Step 3: order the systems

Systems in one schedule may run in parallel, in any order. Orb Run needs `move_player` to finish
before `collect_orbs` checks distances, so they are chained:

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
app.add_systems(Update, (read_intent, spin_orbs))
    .add_systems(FixedUpdate, (move_player, collect_orbs).chain());
```

`.chain()` runs the tuple in order. [App, plugins and schedules](../fundamentals/app-and-plugins.md)
shows system sets, which are the tool for ordering across plugins.

## Step 4: tell the rest of the game that something happened

When the player touches an orb, `collect_orbs` despawns it, bumps the score and writes an
`OrbCollected` **message**. A message is a buffered event: the writer does not know who reads it.
Sound, particles and a HUD can each read it without `collect_orbs` changing:

<!-- ignore-reason: fragment: the whole file compiles at the end of the page -->
```rust,ignore
fn collect_orbs(
    mut commands: Commands,
    players: Query<&Transform, With<Player>>,
    orbs: Query<(Entity, &Transform), With<Orb>>,
    mut scores: Query<&mut Score>,
    mut collected: MessageWriter<OrbCollected>,
) {
    let (Ok(player), Ok(mut score)) = (players.single(), scores.single_mut()) else {
        return;
    };
    for (entity, orb) in &orbs {
        if player.translation.distance(orb.translation) < PICKUP_RADIUS {
            commands.entity(entity).despawn();
            score.collected += 1;
            collected.write(OrbCollected { at: orb.translation.to_array() });
        }
    }
    if score.collected == score.total && score.total > 0 {
        info!("all orbs collected");
    }
}
```

`players.single()` returns an error when there is not exactly one player; the `let ... else`
returns quietly instead of panicking. That is the habit for any system that runs before its
entities exist (a level still loading, a player that just died).

The score lives in a **component** on an entity named `Score`, not in a resource. The reason is
testability: the agent protocol's `play.expect` reads reflected component fields on entities, so a
number you want a test to check should live where a test can read it
([the arena tutorial](arena.md) makes the same choice).

## The whole file

Replace `game/src/lib.rs` with this. It is checked by `cargo xtask book-check`, so it compiles
against the engine as the book describes it.

```rust
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_input::prelude::*;
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, MeshRenderer, box_mesh, uv_sphere,
};
use regia_scene::Transform;

/// Player speed, meters per second.
const PLAYER_SPEED: f32 = 6.0;
/// The player stays inside a square this far from the origin, meters.
const ARENA_LIMIT: f32 = 12.0;
/// An orb is collected when the player is closer than this, meters.
const PICKUP_RADIUS: f32 = 1.0;
/// Orbs placed around the arena.
const ORB_COUNT: u32 = 8;

/// Marks the box the player drives.
#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
pub struct Player;

/// Marks a collectible orb.
#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
pub struct Orb;

/// Progress through the level. A component, so `play.expect` can read it.
#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
pub struct Score {
    /// Orbs picked up so far.
    pub collected: u32,
    /// Orbs the level started with.
    pub total: u32,
}

/// What the player asked for this frame, written in `Update` and read in `FixedUpdate`.
#[derive(Resource, Default)]
pub struct MoveIntent(pub Vec2);

/// Written when an orb is collected.
#[derive(Message, Debug, Clone)]
pub struct OrbCollected {
    /// Where the orb was.
    pub at: [f32; 3],
}

/// Named actions bound to keyboard and gamepad.
fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .binding(Binding::new("Move", InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")))
        .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
}

/// The game plugin the launcher runs.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins(regia_scene::ScenePlugin)
            .add_plugins(InputPlugin::new(input_config()))
            .register_type::<Player>()
            .register_type::<Orb>()
            .register_type::<Score>()
            .init_resource::<MoveIntent>()
            .add_message::<OrbCollected>()
            .add_systems(Startup, spawn_world)
            .add_systems(Update, (read_intent, spin_orbs))
            .add_systems(FixedUpdate, (move_player, collect_orbs).chain());
    }
}

/// Camera, sun, floor, the player, the orbs and the score.
fn spawn_world(mut commands: Commands) {
    commands.spawn((
        Name::new("Camera"),
        Transform {
            translation: Vec3::new(0.0, 22.0, 16.0),
            rotation: Quat::from_rotation_x(-0.95),
            ..Transform::default()
        },
        Camera::default(),
    ));
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

    let (vertices, indices) = box_mesh([14.0, 0.1, 14.0]);
    commands.spawn((
        Name::new("Floor"),
        Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.3, 0.32, 0.35, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            }),
        ),
    ));

    let (vertices, indices) = box_mesh([0.5, 0.5, 0.5]);
    commands.spawn((
        Name::new("Player"),
        Player,
        Transform::from_translation(Vec3::new(0.0, 0.5, 0.0)),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [0.9, 0.45, 0.15, 1.0],
                roughness: 0.5,
                ..CpuMaterial::default()
            }),
        ),
    ));

    // One mesh and one material, shared by every orb.
    let (vertices, indices) = uv_sphere(0.35, 24, 12);
    let orb_mesh = Arc::new(CpuMesh { vertices, indices });
    let orb_material = Arc::new(CpuMaterial {
        base_color: [0.2, 0.7, 0.9, 1.0],
        emissive: [0.2, 0.7, 0.9],
        emissive_intensity: 4.0,
        roughness: 0.3,
        ..CpuMaterial::default()
    });
    for i in 0..ORB_COUNT {
        // A ring of orbs, 8 m from the center. Fixed positions: the same level every run.
        let angle = i as f32 / ORB_COUNT as f32 * std::f32::consts::TAU;
        commands.spawn((
            Name::new(format!("Orb {i}")),
            Orb,
            Transform::from_translation(Vec3::new(8.0 * angle.cos(), 0.6, 8.0 * angle.sin())),
            MeshRenderer::new(Arc::clone(&orb_mesh), Arc::clone(&orb_material)),
        ));
    }

    commands.spawn((Name::new("Score"), Score { collected: 0, total: ORB_COUNT }));
}

/// `Update`: store what the player is asking for.
fn read_intent(actions: Res<Actions>, mut intent: ResMut<MoveIntent>) {
    intent.0 = actions.axis2d("Move").clamp_length_max(1.0);
}

/// `FixedUpdate`: move the player by the step length, inside the arena.
fn move_player(
    intent: Res<MoveIntent>,
    fixed: Res<FixedTime>,
    mut players: Query<&mut Transform, With<Player>>,
) {
    let dt = fixed.step().as_secs_f32();
    for mut transform in &mut players {
        // The axis's y is "forward", which is -Z in the world.
        let step = Vec3::new(intent.0.x, 0.0, -intent.0.y) * PLAYER_SPEED * dt;
        let moved = transform.translation + step;
        transform.translation = Vec3::new(
            moved.x.clamp(-ARENA_LIMIT, ARENA_LIMIT),
            moved.y,
            moved.z.clamp(-ARENA_LIMIT, ARENA_LIMIT),
        );
    }
}

/// `FixedUpdate`, after `move_player`: collect orbs the player touches.
fn collect_orbs(
    mut commands: Commands,
    players: Query<&Transform, With<Player>>,
    orbs: Query<(Entity, &Transform), With<Orb>>,
    mut scores: Query<&mut Score>,
    mut collected: MessageWriter<OrbCollected>,
) {
    let (Ok(player), Ok(mut score)) = (players.single(), scores.single_mut()) else {
        return;
    };
    for (entity, orb) in &orbs {
        if player.translation.distance(orb.translation) < PICKUP_RADIUS {
            commands.entity(entity).despawn();
            score.collected += 1;
            collected.write(OrbCollected { at: orb.translation.to_array() });
        }
    }
    if score.collected == score.total && score.total > 0 {
        info!("all orbs collected");
    }
}

/// `Update`: turn the orbs, so they read as pickups. Purely cosmetic.
fn spin_orbs(time: Res<Time>, mut orbs: Query<&mut Transform, With<Orb>>) {
    for mut transform in &mut orbs {
        transform.rotation = Quat::from_rotation_y(2.0 * time.delta_secs()) * transform.rotation;
    }
}
```

Run `cargo run -p orb_run_launcher` (or whatever your project is called). WASD or the left stick
moves the box; touching an orb removes it. The log prints `all orbs collected` once, when
the last one goes. (It prints on every fixed step after that, as written. Making it fire once is
the exercise at the end.)

## Test it without touching a key

A game you can only test by playing is a game you test less. The agent protocol can drive the
loop. Pause the game so time only moves when you say, send the `Move` action, step frames and
assert, all from a second terminal in the project directory:

```console
$ regia agent call play.pause
$ regia agent call play.inject_input '{"actions":[{"action":"Move","value":[1.0,0.0],"duration_frames":1200}]}'
$ regia agent call play.step '{"frames":120}'
$ regia agent call play.expect '{"assertions":[{"name":"player moved right","condition":{"entity":{"name":"Player"},"component":"Transform","path":"translation.x","op":"gt","value":1.0}}]}'
```

Paste the script as one block. The injection window is counted in frames — including paused
ones — so pausing, typing the calls in slowly and only then stepping lets the window expire
while no fixed step runs, and the player never moves. Also, `play.step` advances exact frames,
but a stepped frame still carries its wall-clock time (unless the host pins the clock with
`TimeUpdateStrategy::ManualDuration`), so how many fixed steps a batch of 120 contains depends
on the display's frame rate. On a fast uncapped display one batch may not move the player a
full meter: step another 120 and expect again — a script simply loops the two calls until the
assertion passes. To drive the player to a
specific orb, move along z first, since the ring's orbs sit at fixed angles. When the player is
in range, check the score:

```console
$ regia agent call play.expect '{"assertions":[{"name":"one orb collected","condition":{"entity":{"component":"Score"},"component":"Score","path":"collected","op":"gte","value":1}}]}'
```

(One orb of eight is what this drive collects; `all orbs collected` cannot print until a script
walks the whole ring.)

Action injection needs `InputPlugin` to be installed, which `Game` does. The conditions and the
rest of the grammar are in [Testing gameplay](../agents/testing.md).

## Exercises

1. **Fire once.** `info!("all orbs collected")` runs every fixed step after the last orb. Add a
   `done: bool` field to `Score` (it derives `Default`, so the spawn needs `..Score::default()`)
   and log only when it flips.
2. **Make the message do something.** Add a system that reads `MessageReader<OrbCollected>` and
   plays a sound or spawns a `Gizmos` marker at `at`; `collect_orbs` does not change. See
   [Audio](../user-guide/audio.md) and [Gizmos](../user-guide/gizmos.md).
3. **Add a jump.** Bind a `Jump` button to `Space`, read `just_pressed("Jump")` in `Update` into a
   `JumpRequested` resource, and consume it in `FixedUpdate` (clear it after use). That is the
   edge-input pattern from step 2.
4. **Replace the box with a physics character.** [Tutorial: a third-person game](third-person.md)
   does the movement with a rapier character controller.

Next: [Tutorial: your first baked light](baked-light.md).
