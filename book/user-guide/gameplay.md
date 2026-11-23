# Gameplay: players, characters, rules and save games

`regia_framework` is the gameplay layer: players and controllers, characters, game rules, scripts,
settings, and save games. It is the "framework" half of `docs/specs/14`.

## Players and controllers

A **player** is a human participant (local, or networked in v0.4.0). A **controller** drives a
character: `PlayerController` for humans, `AiController` for bots.

```rust
use regia::prelude::*;
use regia_scene::prelude::*;
use regia_framework::prelude::*;
use regia::core::math::Vec3;

fn spawn_player(mut commands: Commands) {
    let character = commands
        .spawn((
            Character,
            CharacterMovement::default(),
            Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
        ))
        .id();
    let mut controller = PlayerController::new(0);
    controller.possess(character);
    commands.spawn((controller, PlayerStart));
}
```

`PlayerManager` tracks indices (split-screen local players); possession switches what a controller
drives. The protocol's `players.list` / `scene.possess` read and change this — see
[the agent guide's gameplay page](../agents/gameplay.md).

## Characters

A `Character` is a capsule that moves with modes (`MovementMode`: walking, falling, flying,
swimming, custom) and can jump, crouch and sprint, driven by `CharacterInput`:

```rust
use regia::prelude::*;
use regia_framework::prelude::*;
use regia::core::math::Vec3;

fn apply_input(mut query: Query<(&mut CharacterInput, &CharacterState)>) {
    for (mut input, state) in &mut query {
        input.move_dir = Vec3::new(1.0, 0.0, 0.0);
        if state.grounded {
            input.jump = true;
        }
    }
}
```

Movement resolves against the world through the `CollisionQuery` backend (rapier, or the
flat/heightfield stubs — see [Physics](physics.md)). `CharacterState` is the read-out for animation
and gameplay: velocity, `grounded`, `mode`, time in air.

## Game rules

`GameRules` is the per-level win/lose and spawn logic: hooks for level start, a player joining
(where and what to spawn), a character dying, a per-tick update, and whether the match is over.
Implement it (or use `DefaultRules`, free-for-all with no limits) and install it in the
`GameRulesResource`, or register it by name in `GameRulesRegistry` so a level selects it through
`LevelSettings.game_rules` (`RulesPlugin` resolves that name when a level loads). The framework
calls `on_player_joined` when a local player joins; the game calls `tick`, `on_level_start`,
`on_character_died` and `is_match_over` from its own systems, at the moments it defines. The
[arena tutorial](../tutorials/arena.md) shows the pattern.

For a multiplayer mode (teams, flags, a hill, rounds, ready-up) use the match core instead of
writing those rules: [Game modes](game-modes.md#which-layer-do-i-use) says which layer fits, and
the [capture-the-flag tutorial](../tutorials/capture-the-flag.md) plays one with bots.

## Game states and GameFlow

Game flow (menus, loading, play, pause) is a set of **states** (`regia_ecs::state`, from
`bevy_state`). Add `GameFlowPlugin` for the standard flow, Boot → MainMenu → Loading → Playing ⇄
Paused, or declare your own:

```rust
use regia::prelude::*;

#[derive(States, Reflect, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Level {
    #[default]
    Forest,
    Cave,
}

fn build(app: &mut App) {
    app.init_state::<Level>()
        .add_systems(OnEnter(Level::Cave), spawn_cave)
        .add_systems(Update, drip_water.run_if(in_state(Level::Cave)));
}

fn spawn_cave(mut commands: Commands) {
    // Despawned automatically when the game leaves Level::Cave.
    commands.spawn((Name::new("cave"), DespawnOnExit(Level::Cave)));
}

fn drip_water() {}
```

Change state with `ResMut<NextState<Level>>::set(Level::Cave)`; the switch happens once per frame,
right after `PreUpdate`, running `OnExit` of the old value, then `OnTransition`, then `OnEnter` of
the new one. `App::add_sub_state` adds states that exist only under one value of another
(`#[source(GameFlow = GameFlow::Paused)]`); `App::add_computed_state` adds states derived from
others (implement `ComputedStates`). A computed state that maps several source values to the same
value (Playing and Paused → `InGame`) should set `ALLOW_SAME_STATE_TRANSITIONS = false`, or moving
between those values re-runs its `OnExit`/`OnEnter` and despawns its scoped entities. States must
derive `Reflect`: that is what lets the agent protocol (`state.get` / `state.set`, see
[Game states](../agents/states.md)) and the editor see them.

With `GameFlowPlugin`, register load work in `OnEnter(GameFlow::Loading)` through
`LoadingTasks::begin` and mark it done with `LoadingTasks::finish`; play starts on the first frame
nothing is pending. `regia_framework::flow::request_pause` / `toggle_pause` move between
Playing and Paused, and Paused freezes `Time`.

## Scripts

A **script** is a Rust behavior attached to an entity, editable in the inspector. Derive it and
register it:

```rust
use regia::prelude::*;
use regia_framework::prelude::*;

#[derive(Script, Reflect, Default, Clone)]
#[script(order = -7)]
struct Door {
    open: bool,
}

impl regia_framework::script::Script for Door {
    fn update(&mut self, ctx: &mut ScriptCtx) {
        // read/write the entity's components through ctx
    }
}

fn register(app: &mut App) {
    app.register_script::<Door>();
}
```

Scripts are ordinary reflected data with an `update` hook (plus `start`, `fixed_update`,
`late_update`, `on_event` and `on_message`), held as slots in an entity's `Scripts` component — the
simplest way to give an entity behavior. The editor's `editor.create_script` writes a new script and registers it at
`// REGIA:SCRIPTS`.

Inside a hook, `ctx` reaches the entity (`get`, `get_mut`, `insert`, `remove`, `transform_mut`,
`despawn`), the world (`spawn`, `find_by_name`, `commands`, `world`), other scripts
(`with_script`, `send_message`, `broadcast`, `send_message_in`), the clock (`delta_secs`, `time`),
input (`action2d`, `action_pressed`), timers and coroutines (`after`, `every`, `start_coroutine`),
seeded random numbers (`rand_f32`, `rand_range`) and physics queries: `ctx.physics()` returns the
raycast, shapecast, overlap and closest-point queries over the physics world, or `None` without
physics. Messages are delivered after every `update` of the frame; one sent from `on_message`,
`on_event` or `late_update` arrives on the next frame. A system that must run after the scripts'
`late_update` orders itself `.after(ScriptsSet::Late)`.

## Coroutines

Sequences that span frames — a cutscene, a tutorial step, spawn waves — are easiest to write as a
**coroutine**: an `async` block the engine polls once per frame (`CoroutinePlugin`). The block gets a
`Co` context instead of the world; world access goes through closures the executor runs between
polls, so nothing is borrowed across an `.await`:

```rust
use regia::prelude::*;
use regia_framework::coroutine::{Co, start_coroutine};

#[derive(Resource, Default)]
struct Wave(u32);

fn start_waves(world: &mut World, spawner: Entity) {
    // Bound to `spawner`: despawning it cancels the sequence.
    start_coroutine(world, Some(spawner), |co: Co| async move {
        for _ in 0..3 {
            co.queue(|world| world.resource_mut::<Wave>().0 += 1); // spawn a wave
            co.wait_seconds(10.0).await; // game time: pausing stops the clock
        }
        co.wait_until(|world| world.resource::<Wave>().0 >= 3).await;
        let finished_on = co.with_world(|world| world.resource::<Time>().frame_count()).await;
        co.queue(move |world| world.resource_mut::<Wave>().0 = finished_on as u32);
    });
}
```

`wait_frames(n)` / `next_frame()` count frames the game runs unpaused (a frame with `Time::paused` set does not
count, ADR-10105), `with_world` returns a value in the same frame, and
`Coroutines::take_result` reads a finished coroutine's output. Scripts start owner-bound coroutines
with `ctx.start_coroutine(..)`. **Coroutines are not saved and do not survive a hot reload**: keep
progress in reflected state and restart the sequence from it after loading. Agents see running
coroutines with `coroutines.list` ([agent page](../agents/coroutines.md)).

## Settings

`GameSettings` is the persisted, live-applied configuration (graphics preset, display, audio,
input, language, accessibility). Load from `user://settings.ron`; every change is applied and
saved. The agent protocol exposes it as `settings.get` / `settings.set` (see
[the agent guide's framework page](../agents/framework.md)).

## Save games

Mark entities `Saveable` and register saveable resources; `SavePlugin` serializes the world to a
named slot (atomic write, CRC-32 checked) and restores it with migrations:

```rust
use regia::prelude::*;
use regia_framework::prelude::*;

#[derive(Component, Reflect, Default, Clone)]
#[reflect(Component, Default)]
struct Coin {
    count: u32,
}

fn spawn_coin(mut commands: Commands) {
    commands.spawn((Saveable, Coin { count: 1 }));
}
```

`save.save` / `save.load` / `save.list_slots` drive it from the protocol; see
[the agent guide's gameplay page](../agents/gameplay.md) and `docs/formats/save-format.md`.

Add `SaveFlowPlugin` to save from ordinary systems: write a `SaveRequest` (`Save`, `Load`, `Delete`,
`QuickSave`, `QuickLoad`) and read the `SaveOutcome` the next frame. Bind the `QuickSave` and `QuickLoad`
actions for quicksave keys. `SaveInfo` (display name, level, play time, save count) and `SaveBlobs` (your
own bytes by key) are saved in every slot. Insert a `SaveStore` to keep saves somewhere other than files,
for example `SaveStore::new(SteamRemoteStorage::new(&steam))` for Steam cloud saves. `MoveTo` walks a
character to a point. The `third_person` and `top_down` templates use the save flow (quicksave keys,
`SaveInfo`, `SaveOutcome`); details on the
[save flow](../agents/save-flow.md) page.

## Gameplay tags

`regia_framework::tags` gives entities hierarchical tags such as `Combat.Damage.Fire`. Add `TagPlugin`, list the
tags your game uses in a `game.tags.ron` (`TagRegistry::load`), put a `TagContainer` on an entity, and ask it
questions with `has` (a held `Combat.Damage.Fire` satisfies `Combat.Damage`), `has_exact`, or a `TagQuery` such
as `AllOf([Has("Combat.Damage"), NoneOf([Has("Status.Immune")])])`. A `TagsChanged` message fires when a
container changes. The format and the agent methods are on the [gameplay tags](../agents/gameplay-tags.md) page.
