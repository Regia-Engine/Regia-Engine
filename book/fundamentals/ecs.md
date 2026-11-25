# The ECS: entities, components, systems

<!-- index: Entity -->
<!-- index: Component -->
<!-- index: System -->
<!-- index: Resource -->
<!-- index: Message -->
<!-- index: ECS | see: Entity -->

Everything in a running Regia Engine game lives in one **world** organized as an Entity Component
System. Spec: `docs/specs/10-core.md`. Regia Engine uses `bevy_ecs` for this, through its own facade crate `regia_ecs`: game code
imports from `regia::prelude` (or `regia_ecs::prelude`) and never names `bevy_ecs` directly,
which lets the engine pin and upgrade it in one place.

| Concept | What it is | Example |
|---|---|---|
| **Entity** | An id. It has no data of its own. | the player, a crate, a camera |
| **Component** | A plain Rust struct or enum attached to an entity | `Transform`, `MeshRenderer`, `RigidBody`, your `Health` |
| **System** | A Rust function that reads and writes components and resources | `propagate_transforms`, your `move_player` |
| **Resource** | A singleton value in the world | `Time`, `RenderSettings`, `AssetServer`, your `Score` |
| **Message** | A buffered event that systems write and read | `KeyboardInput`, `CollisionStarted`, your `Explosion` |

An entity *is* whatever its components say it is: an entity with a `Transform`, a `MeshRenderer`
and a `RigidBody` is a visible physics object; remove the `RigidBody` and it stops simulating.

## Components

Derive `Component`. Also derive `Reflect` and register the type when you want the editor, saves,
hot reload or agents to see it ([Reflection](reflection.md)); that is almost always.

```rust
use regia::prelude::*;

/// Hit points.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct Health {
    /// Current hit points.
    pub current: f32,
    /// Maximum hit points.
    pub max: f32,
}

/// Marks the entity the local player controls.
#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
pub struct Player;

fn register(app: &mut App) {
    app.register_type::<Health>().register_type::<Player>();
}
```

Empty structs like `Player` are **marker components**: they carry no data and exist to be
filtered on. Components can declare **required components** with `#[require(...)]`; spawning one
adds the others with their defaults. `Transform` requires `GlobalTransform`, for example.

## Spawning and changing entities

`Commands` queue structural changes (spawn, despawn, insert, remove) which apply after the system
that queued them, at the next sync point:

```rust
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
// Scene files and the editor store this `Name`; the prelude's `Name` is the plain ECS one.
use regia_scene::components::Name;

#[derive(Component, Default)]
struct Health(f32);

#[derive(Component)]
struct Burning;

fn spawn_enemy(mut commands: Commands) -> Entity {
    commands
        .spawn((
            Name::new("Enemy"),
            Transform::from_translation(Vec3::new(4.0, 0.0, -2.0)),
            Health(100.0),
        ))
        .id()
}

fn ignite(mut commands: Commands, enemy: Entity) {
    commands.entity(enemy).insert(Burning);
}

fn extinguish(mut commands: Commands, enemy: Entity) {
    commands.entity(enemy).remove::<Burning>();
}

fn kill(mut commands: Commands, enemy: Entity) {
    commands.entity(enemy).despawn();
}
```

Despawning an entity also despawns its children.

## Systems and queries

A system is a function whose parameters say what it accesses. The scheduler runs systems in
parallel when their accesses do not conflict, so declare only what you need: `&T` for reading,
`&mut T` for writing.

```rust
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

#[derive(Component)]
struct Velocity(Vec3);

#[derive(Component)]
struct Frozen;

#[derive(Component)]
struct Player;

/// Moves everything that has a velocity, except frozen things.
fn integrate(time: Res<Time>, mut query: Query<(&Velocity, &mut Transform), Without<Frozen>>) {
    let dt = time.delta_secs();
    for (velocity, mut transform) in &mut query {
        transform.translation += velocity.0 * dt;
    }
}

/// Reads one entity: the player.
fn report(players: Query<&Transform, With<Player>>) {
    for transform in &players {
        info!("player at {:?}", transform.translation);
    }
}

/// Reacts only to entities whose velocity changed (or was added) since this system last ran.
fn on_velocity_change(changed: Query<(Entity, &Velocity), Changed<Velocity>>) {
    for (entity, velocity) in &changed {
        debug!("{entity:?} now moves at {:?}", velocity.0);
    }
}
```

- **Filters**: `With<T>` and `Without<T>` restrict a query without reading `T`; `Changed<T>` and
  `Added<T>` select entities whose `T` changed or was added since the system last ran.
- **Single entities**: `query.get(entity)` / `get_mut(entity)` look one entity up and return a
  `Result`; prefer `let Ok(x) = … else { return };` to unwrapping.
- **Resources**: `Res<T>` reads and `ResMut<T>` writes a resource; `Option<Res<T>>` tolerates
  its absence.
- **Local state**: `Local<T>` is per-system state that persists between runs.

## Resources

```rust
use regia::prelude::*;

/// The current score.
#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
pub struct Score(pub u32);

fn add_point(mut score: ResMut<Score>) {
    score.0 += 1;
}

fn setup(app: &mut App) {
    app.init_resource::<Score>()            // Default::default()
        .register_type::<Score>()
        .add_systems(Update, add_point);
}
```

`insert_resource(value)` inserts a specific value; `init_resource::<T>()` uses `Default` (or
`FromWorld`).

## Messages

Messages decouple the system that notices something from the systems that react. Writers and
readers never block each other. Messages live for two frames (the buffer swaps once per frame),
so a reader sees each message once only if it runs at least once in that window; a system held
back by `run_if` (a paused or loading state) can miss messages.

```rust
use regia::prelude::*;

/// Something exploded.
#[derive(Message, Debug, Clone)]
pub struct Explosion {
    /// World position.
    pub at: [f32; 3],
    /// Damage at the center.
    pub damage: f32,
}

fn detonate(mut writer: MessageWriter<Explosion>) {
    writer.write(Explosion { at: [0.0, 0.0, 0.0], damage: 50.0 });
}

fn shake_camera(mut reader: MessageReader<Explosion>) {
    for explosion in reader.read() {
        debug!("boom at {:?}", explosion.at);
    }
}

fn setup(app: &mut App) {
    app.add_message::<Explosion>()
        .add_systems(Update, (detonate, shake_camera).chain());
}
```

The engine reports input (`KeyboardInput`, `MouseMotion`, `TouchInput`, …), lifecycle
(`LifecycleEvent`), physics contacts (`CollisionStarted`, `TriggerEnter`), level loading
(`LevelLoaded`) and more as messages.

## Ordering systems

Systems in the same schedule run in parallel unless you order them:

```rust
use regia::prelude::*;

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum GameSet {
    Input,
    Simulation,
    Presentation,
}

fn read_input() {}
fn steer() {}
fn shoot() {}
fn update_hud() {}

fn setup(app: &mut App) {
    app.configure_sets(Update, (GameSet::Input, GameSet::Simulation, GameSet::Presentation).chain())
        .add_systems(Update, read_input.in_set(GameSet::Input))
        .add_systems(Update, (steer, shoot).chain().in_set(GameSet::Simulation))
        .add_systems(Update, update_hud.in_set(GameSet::Presentation));
}
```

`.chain()` runs a tuple in order; `.before(x)` / `.after(x)` order against a system or set;
`.run_if(condition)` gates a system. Engine plugins expose sets to order against, for example
`regia_framework::script::ScriptsSet::Update` and `PostUpdateSet::TransformPropagate`.

## Exclusive systems

A system that takes `&mut World` has the whole world to itself: it can spawn and read
immediately, run queries, and call engine functions that need the world (loading a level,
saving a game). It cannot run in parallel with anything, so keep these for setup and rare work.

```rust
use regia::prelude::*;

#[derive(Component)]
struct Pickup;

fn count_pickups(world: &mut World) {
    let count = world.query_filtered::<Entity, With<Pickup>>().iter(world).count();
    info!("{count} pickups left");
}
```

## Parents and children

The hierarchy uses the ECS's `ChildOf` / `Children` relationship; a child's `Transform` is
relative to its parent and `ScenePlugin` propagates `GlobalTransform` down the tree every frame.
`regia_scene::HierarchyCommands` adds `set_parent`, `remove_parent`, and
`*_keep_world_transform` variants that keep the entity where it is on screen:

```rust
use regia::prelude::*;
use regia_scene::prelude::*;

fn attach(mut commands: Commands, turret: Entity, tank: Entity) {
    commands.entity(turret).set_parent_keep_world_transform(tank);
}
```

## Where systems run

Systems go into **schedules** (`Startup`, `PreUpdate`, `FixedUpdate`, `Update`, `PostUpdate`,
`Last`, …) that the app runs every frame in a fixed order. Which schedule to use, and how plugins
package systems, is the next chapter: [App, plugins and schedules](app-and-plugins.md).
