# Tutorial: a third-person game

In this tutorial you build **Coin Run**, a small third-person game: run and jump around a level,
collect every coin, see your progress on a HUD, save and load, test it automatically through the
agent protocol, and package it. It starts from the `third_person` template and adds one feature
per step. The engine's `samples/third_person` is the finished, larger version of the same idea
(an animated character with foot IK, a pause menu, split-screen); this tutorial points to it
where it goes further.

You need the tools from [Installing Regia Engine](../start/install.md) and the concepts from
[Fundamentals](../fundamentals/ecs.md).

## Step 1: create the project

From the folder that holds your projects:

```sh,walkthrough=new,template=third_person
regia new "Coin Run" --template third_person
cd coin_run
```

```console
$ cargo run -p coin_run_launcher
```

You get a window with a gray floor, two blocks and an orange box standing in for the player. WASD
or the left stick moves, Space or the south button jumps, the mouse or right stick orbits the
camera. The camera boom shortens when a wall gets between it and the player.

## Step 2: read the template

Open `game/src/lib.rs`. Condensed to its essentials (the template builds its cube mesh by hand;
this version uses the renderer's `box_mesh` helper), it is:

```rust
use std::sync::Arc;

use regia::physics::{CharacterBody, Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::camera::{CameraMode, CameraPlugin, CameraRig, ChaseCamera, ThirdPerson};
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState,
    use_rapier_character_controller,
};
use regia_framework::script::ScriptPlugin;
use regia_input::prelude::*;
use regia_render::{Camera, CpuMaterial, CpuMesh, Environment, MeshRenderer, box_mesh};
use regia_scene::Transform;

pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        // 1. Input: named actions bound to keyboard, mouse and gamepad.
        let config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Move", InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")))
            .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
            .binding(Binding::new("Look", InputSource::MouseDelta))
            .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right)))
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new("Jump", InputSource::gamepad_button(GamepadButton::South)));

        // 2. The engine systems this game uses.
        app.add_plugins((
            regia_scene::ScenePlugin,
            PhysicsPlugin,
            CharacterPlugin,
            CameraPlugin,
            ScriptPlugin,
            InputPlugin::new(config),
        ));
        // 3. Characters move through rapier's kinematic character controller.
        use_rapier_character_controller(app);

        // 4. Gameplay systems.
        app.init_resource::<Environment>()
            .add_systems(Startup, (setup_level, setup_character, setup_camera).chain())
            .add_systems(Update, (write_character_input, write_camera_look));
    }
}

/// A gray static box with a collider and a mesh.
fn block(commands: &mut Commands, translation: Vec3, half: Vec3) {
    let (vertices, indices) = box_mesh(half.to_array());
    commands.spawn((
        Transform::from_translation(translation),
        RigidBody::static_body(),
        Collider { shape: ColliderShape::Box { half_extents: half }, ..Collider::default() },
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial { base_color: [0.55, 0.6, 0.65, 1.0], ..CpuMaterial::default() }),
        ),
    ));
}

fn setup_level(mut commands: Commands) {
    block(&mut commands, Vec3::new(0.0, -0.5, 0.0), Vec3::new(20.0, 0.5, 20.0));
    block(&mut commands, Vec3::new(3.0, 0.5, -2.0), Vec3::new(1.0, 1.0, 1.0));
    block(&mut commands, Vec3::new(-3.0, 1.0, 2.0), Vec3::new(2.0, 2.0, 2.0));
}

/// The player: a character capsule (movement, state, physics body) with a stand-in mesh.
fn setup_character(mut commands: Commands) {
    let (vertices, indices) = box_mesh([0.3, 0.9, 0.3]);
    commands.spawn((
        Name::new("Player1"),
        Transform::from_translation(Vec3::new(0.0, 1.2, 5.0)),
        Character,
        CharacterMovement::default(),
        CharacterInput::default(),
        CharacterState::default(),
        CharacterBody::default(),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial { base_color: [0.9, 0.5, 0.2, 1.0], ..CpuMaterial::default() }),
        ),
    ));
}

/// A camera rig in third-person mode chasing the character.
fn setup_camera(mut commands: Commands, characters: Query<Entity, With<Character>>) {
    let Some(target) = characters.iter().next() else { return };
    let mode = ThirdPerson {
        boom_length: 4.5,
        socket_offset: Vec3::new(0.0, 1.6, 0.0),
        target_offset: Vec3::new(0.0, 1.2, 0.0),
        ..ThirdPerson::default()
    };
    commands.spawn((
        Transform::default(),
        CameraRig { mode: CameraMode::ThirdPerson(mode), ..CameraRig::default() },
        ChaseCamera { target },
        Camera::default(),
    ));
}

/// Move input relative to the camera's yaw.
fn write_character_input(
    actions: Res<Actions>,
    rigs: Query<&CameraRig>,
    mut inputs: Query<&mut CharacterInput, With<Character>>,
) {
    let axes = actions.axis2d("Move");
    let yaw = rigs.iter().next().map_or(0.0, |rig| rig.yaw);
    let forward = (Quat::from_rotation_y(yaw) * -Vec3::Z).with_y(0.0).normalize_or_zero();
    let right = (Quat::from_rotation_y(yaw) * Vec3::X).with_y(0.0).normalize_or_zero();
    for mut input in &mut inputs {
        input.move_dir = (forward * axes.y + right * axes.x).clamp_length_max(1.0);
        input.jump = actions.pressed("Jump");
    }
}

/// Mouse / right stick orbits the boom.
fn write_camera_look(actions: Res<Actions>, mut rigs: Query<&mut CameraRig>) {
    let look = actions.axis2d("Look");
    for mut rig in &mut rigs {
        rig.yaw -= look.x * 0.003;
        rig.pitch = (rig.pitch + look.y * 0.003).clamp(-1.2, 1.2);
    }
}
```

The pieces:

1. **Input** ([Input](../user-guide/input.md)). Gameplay reads `Move`, `Look` and `Jump`, never
   a key code, so the game works the same with a keyboard or a gamepad.
2. **Plugins**. `ScenePlugin` propagates transforms, `PhysicsPlugin` runs rapier, `CharacterPlugin`
   moves `Character` entities from their `CharacterInput`, `CameraPlugin` drives `CameraRig`s.
3. **The character** is data: `CharacterMovement` holds speeds, acceleration, jump height, air
   control and coyote time; `CharacterInput` is what to do this frame; `CharacterState` reports
   back (`grounded`, velocity, movement mode); `CharacterBody` is its physics capsule.
4. **The camera** is a `CameraRig` with a `ThirdPerson` boom chasing the character
   (`ChaseCamera`), plus the render `Camera` on the same entity.

Read [Physics](../user-guide/physics.md) and [Gameplay framework](../user-guide/gameplay.md) for
what each component offers.

## Step 3: coins

Add coins the player collects by running through them. A coin is a small spinning box with a
`Coin` component holding its id; a system checks the distance from each character to each coin.

```rust
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::character::Character;
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer, box_mesh};
use regia_scene::Transform;

/// Where the coins are, in id order.
pub const COIN_SPOTS: [[f32; 3]; 6] = [
    [0.0, 1.0, -3.0],
    [4.0, 1.0, 0.0],
    [3.0, 2.6, -2.0], // on top of block A
    [-3.0, 4.6, 2.0], // on top of block B
    [-6.0, 1.0, -6.0],
    [8.0, 1.0, 6.0],
];

/// A collectible coin.
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct Coin {
    /// Index into [`COIN_SPOTS`].
    pub id: u32,
}

/// Which coins are gone, in collection order.
#[derive(Resource, Reflect, Debug, Clone, Default)]
#[reflect(Resource, Default)]
pub struct CoinProgress {
    /// Collected coin ids.
    pub collected: Vec<u32>,
}

/// Spawns one coin.
pub fn spawn_coin(commands: &mut Commands, id: u32) {
    let [x, y, z] = COIN_SPOTS[id as usize];
    let (vertices, indices) = box_mesh([0.2, 0.2, 0.05]);
    commands.spawn((
        Name::new(format!("Coin{id}")),
        Transform::from_translation(Vec3::new(x, y, z)),
        Coin { id },
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            Arc::new(CpuMaterial {
                base_color: [1.0, 0.78, 0.2, 1.0],
                metallic: 1.0,
                roughness: 0.3,
                ..CpuMaterial::default()
            }),
        ),
    ));
}

fn spawn_coins(mut commands: Commands) {
    for id in 0..COIN_SPOTS.len() as u32 {
        spawn_coin(&mut commands, id);
    }
}

/// Collects every coin a character is within 0.9 m of.
fn collect_coins(
    mut commands: Commands,
    mut progress: ResMut<CoinProgress>,
    characters: Query<&Transform, With<Character>>,
    coins: Query<(Entity, &Coin, &Transform)>,
) {
    for character in &characters {
        for (entity, coin, transform) in &coins {
            if character.translation.distance(transform.translation) < 0.9
                && !progress.collected.contains(&coin.id)
            {
                progress.collected.push(coin.id);
                commands.entity(entity).despawn();
                info!("collected coin {}", coin.id);
            }
        }
    }
}

/// Spins the coins (visual only).
fn spin_coins(time: Res<Time>, mut coins: Query<&mut Transform, With<Coin>>) {
    for mut transform in &mut coins {
        transform.rotation = Quat::from_rotation_y(time.elapsed_secs() * 2.0);
    }
}

/// Add this plugin next to the template's: `app.add_plugins(CoinsPlugin)`.
pub struct CoinsPlugin;

impl Plugin for CoinsPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Coin>()
            .register_type::<CoinProgress>()
            .init_resource::<CoinProgress>()
            .add_systems(Startup, spawn_coins)
            .add_systems(Update, (collect_coins, spin_coins));
    }
}
```

Run it and collect a coin: the log prints `collected coin 0`. A distance check is the simplest
pickup. Physics **sensors** are the other way: a `Collider` with `sensor: true` raises
`TriggerEnter` messages, and a [script](../user-guide/gameplay.md) can react to them per entity;
`samples/third_person/src/collectible.rs` does it that way.

## Step 4: a HUD

The runtime UI is part of `DefaultPlugins`. Spawn a `UiRoot` with a text node, and update the
text when the progress changes. The counter also lives in a reflected component, so tests can
read it (step 6):

```rust
use regia::prelude::*;

/// Mirrors the coin progress for the HUD and for agent checks.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct CoinCounter {
    /// Coins collected.
    pub collected: u32,
    /// Coins in the level.
    pub total: u32,
}

#[derive(Resource, Default)]
struct CoinProgress {
    collected: Vec<u32>,
}

const TOTAL: u32 = 6;

fn setup_hud(mut commands: Commands) {
    let root = commands
        .spawn((
            UiRoot::default(),
            SafeAreaPadding,
            NodeStyle::new(Style { padding: Edges::px(16.0), ..Default::default() }),
        ))
        .id();
    let label = commands
        .spawn((
            Name::new("hud-coins"),
            UiNode,
            Text {
                spans: vec![TextSpan::lit(format!("Coins: 0 / {TOTAL}")).with_style(TextStyle {
                    font_px: 28.0,
                    color: UiColor([1.0, 0.92, 0.6, 1.0]),
                    ..Default::default()
                })],
                ..Default::default()
            },
            CoinCounter { collected: 0, total: TOTAL },
        ))
        .id();
    commands.entity(root).add_child(label);
}

fn refresh_hud(progress: Res<CoinProgress>, mut labels: Query<(&mut Text, &mut CoinCounter)>) {
    if !progress.is_changed() {
        return;
    }
    let collected = progress.collected.len() as u32;
    for (mut text, mut counter) in &mut labels {
        counter.collected = collected;
        let line = if collected == counter.total {
            "All coins! Well done.".to_string()
        } else {
            format!("Coins: {collected} / {}", counter.total)
        };
        if let Some(span) = text.spans.first_mut() {
            *span = TextSpan::lit(line).with_style(span.style.clone());
        }
    }
}

fn setup(app: &mut App) {
    app.register_type::<CoinCounter>()
        .add_systems(Startup, setup_hud)
        .add_systems(Update, refresh_hud);
}
```

`UiRoot`, `UiNode`, `Text`, `NodeStyle` and the rest come from `regia::prelude` when the `ui`
feature is on (the default). Layout is flexbox: `NodeStyle` holds direction, size, padding and
gaps; `SafeAreaPadding` keeps the HUD clear of notches and rounded corners. The same HUD can be
authored as a `.ui.ron` file with a `bind:` to a reflected field instead of a system
([UI](../user-guide/ui.md)).

## Step 5: save and load

Make the progress a **saveable resource** and add `SavePlugin`. Save on F5 and load on F9; after a
load, bring the world in line with the loaded progress (despawn collected coins, respawn coins
collected after the save):

```rust
use regia::prelude::*;
use regia_framework::prelude::{
    RegisterSaveableExt, SaveDirectory, SaveGame, SavePlugin, SaveSpec, SaveableResource,
};

#[derive(Resource, Reflect, Debug, Clone, Default)]
#[reflect(Resource, Default)]
pub struct CoinProgress {
    pub collected: Vec<u32>,
}

impl SaveableResource for CoinProgress {}

#[derive(Component)]
struct Coin {
    id: u32,
}

fn spawn_coin(commands: &mut Commands, id: u32) {
    // as in step 3
}

/// A save or load the player asked for this frame.
#[derive(Resource, Default)]
struct SaveRequest(Option<bool>); // Some(true) = save, Some(false) = load

/// F5 asks for a save, F9 for a load.
fn read_save_keys(mut keys: MessageReader<KeyboardInput>, mut request: ResMut<SaveRequest>) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed || key.repeat {
            continue;
        }
        match key.physical_key.as_str() {
            "F5" => request.0 = Some(true),
            "F9" => request.0 = Some(false),
            _ => {}
        }
    }
}

/// Saving and loading need the whole world, so this is an exclusive system.
fn apply_save_request(world: &mut World) {
    let Some(save) = world.resource_mut::<SaveRequest>().0.take() else { return };
    let dir = world.get_resource::<SaveDirectory>().cloned().unwrap_or_default();
    let slot = SaveGame::slot(&dir, "Coins");
    if save {
        match SaveGame::save(&slot, world, &SaveSpec::default()) {
            Ok(result) => info!("saved {} bytes", result.bytes_written),
            Err(err) => error!("save failed: {err}"),
        }
    } else {
        match SaveGame::load(&slot, world, &SaveSpec::default()) {
            Ok(_) => info!("loaded"),
            Err(err) => error!("load failed: {err}"),
        }
    }
}

/// Makes the live coins match the progress (after a load, or any time).
fn reconcile_coins(mut commands: Commands, progress: Res<CoinProgress>, coins: Query<(Entity, &Coin)>) {
    if !progress.is_changed() {
        return;
    }
    let mut present = Vec::new();
    for (entity, coin) in &coins {
        if progress.collected.contains(&coin.id) {
            commands.entity(entity).despawn();
        } else {
            present.push(coin.id);
        }
    }
    for id in 0..6u32 {
        if !progress.collected.contains(&id) && !present.contains(&id) {
            spawn_coin(&mut commands, id);
        }
    }
}

fn setup(app: &mut App) {
    app.add_plugins(SavePlugin::default())
        .register_saveable_resource::<CoinProgress>()
        .init_resource::<SaveRequest>()
        .add_systems(Update, (read_save_keys, apply_save_request, reconcile_coins).chain());
}
```

Saves are written atomically to `user://saves/` with a CRC check; a corrupt or truncated file
fails to load with an error instead of crashing. `SavePlugin` also autosaves to the `Auto` slot
when the app is suspended (Android going to the background). Entities marked `Saveable` are saved
with all their reflected components; this game only needs the resource. In a larger game, bind
`Save` and `Load` actions instead of reading keys ([Input](../user-guide/input.md)). The save system is described
in [Gameplay framework](../user-guide/gameplay.md) and `docs/formats/save-format.md`.

## Step 6: test it through the agent protocol

Leave the game running and drive it from a terminal (or let an AI agent do it through
`regia mcp`). Pause it, walk the player forward for two seconds, and check that a coin was
collected:

```console
$ regia agent call play.pause
$ regia agent call play.inject_input '{"actions":[{"action":"Move","value":[0.0,1.0],"duration_frames":120}]}'
$ regia agent call play.step '{"frames":120}'
$ regia agent call play.expect '{"assertions":[{"name":"a coin was collected","condition":{"entity":{"name":"hud-coins"},"component":"CoinCounter","path":"collected","op":"gte","value":1}}]}'
$ regia agent call seeing.screenshot --out after-run.png
```

From the start position the first coin (`[0, 1, -3]`) is straight ahead, so walking forward
collects it. `play.step` advances exact frames while paused, which makes the check deterministic;
`play.wait_until` waits for a condition instead. Each method is in the
[agent guide's testing page](../agents/testing.md). The same calls work from Rust tests: build an
`App` with your plugin, run frames with `app.update()`, and assert on the world.

## Step 7: package it

```console
$ regia package --platform linux --profile shipping
$ regia package --platform android --format apk
```

A Linux shipping package is built against glibc 2.31 (the Steam Linux Runtime), which needs
`cargo-zigbuild` and `zig` on `PATH` (`regia doctor --platform deck` checks both); add
`--glibc host` to build against your own system's glibc instead, or leave out `--profile shipping`
for a development package. The Android package needs the
[Android setup](../start/install.md#5-android-optional).

The result runs from any directory: the cooked assets are in `content/*.fpak` next to the
executable. See [Packaging](../user-guide/packaging.md).

## Where to go from here

`samples/third_person` takes the same game much further; read its modules in this order:

| Module | What it adds |
|---|---|
| `assets.rs` | Fetching and cooking the Khronos Fox glTF, loading its mesh, skeleton and clips, binding the skin |
| `anim.rs` | An animation graph driven by `CharacterState` (idle, walk, run, jump) and foot IK on stairs and slopes ([Animation](../user-guide/animation.md)) |
| `character.rs`, `camera.rs` | One character and camera per local player, split-screen viewports |
| `collectible.rs` | Collectibles as sensor colliders with a `CollectScript` |
| `hud.rs` | The counter, health, damage numbers and a pause menu with buttons |
| `save.rs` | Save and load through the save flow (`SaveFlowPlugin`), with a migration for older saves |
| `level.rs` | Stairs, a slope, platforms |
| `enemy.rs` | Two enemy kinds driven by `.bt.ron` behavior trees over a navmesh crowd: patrol, notice, chase, attack, give up |
| `combat.rs`, `melee.rs` | Health and hits through abilities and attributes, a three-hit melee combo timed by a montage, hit detection with a physics shape query |
| `ragdoll_fx.rs`, `checkpoint.rs` | Ragdoll deaths and respawning at the last checkpoint |
| `menu.rs`, `sound.rs` | A title screen and a settings menu that persists; procedural footsteps, hits and an ambient bed |

### Adding enemies and combat

The sample's combat is a good next step for Coin Run, in this order:

1. **Health.** Add `AbilityPlugin` and `HealthBridge`, give the player an `.attrs.ron` with
   `health` and `health_max`, and apply an instant effect with a `Param` amount for each hit
   ([Abilities](../agents/abilities.md)). The bridge raises `DiedEvent` when health reaches zero.
2. **An enemy brain.** Write the tree as `.bt.ron` (a `sense` service writing blackboard keys, a
   selector of `condition` + leaf sequences) and implement the leaves as `BtTask`s. Re-evaluate the
   selector every tick by making movement leaves finish in one tick; only an attack runs over
   several ticks.
3. **Movement.** Bake a `NavMesh` from the level's static colliders at startup, add each enemy to a
   `NavCrowd`, set targets from the leaves and copy the agent position onto the enemy after
   `NavCrowdStep`.
4. **Death.** Read `DiedEvent`, spawn a limp `Ragdoll`, and respawn the player at a checkpoint.

`samples/third_person/README.md` lists the files, and the agent page
[Third-person demo](../agents/third-person-demo.md) shows how its tests drive and check all of it.

Then try the [arena tutorial](arena.md): AI bots, weapons, match rules and navigation.
