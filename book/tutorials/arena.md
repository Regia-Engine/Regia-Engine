# Tutorial: the arena

`samples/arena` is a first-person arena shooter against AI bots. Where the
[third-person tutorial](third-person.md) built a game from nothing, this one takes a complete
sample apart: how a match is structured with **game rules**, how **bots** decide what to do (a
steering brain, or a **behavior tree** on a **navmesh**), how **weapons** use physics queries,
how effects and sound are triggered, and how the whole match is tested and replayed
deterministically. Each section ends with something to change.

## Run it

The arena is one of the engine's samples, so this page works in the engine folder (an engine
release includes `samples/`), not in a project of your own:

```console
$ cargo run --release -p arena
$ cargo run --release -p arena -- --bots 3 --seed 42
$ cargo run --release -p arena -- --no-local-player --bots 8
```

You are a pawn on a square arena with crates, walls and seven bots. WASD and the mouse move and
look, the left button fires (gamepad sticks and the right trigger work too; on a touch screen the
HUD shows a virtual stick and a fire button). The match goes warm-up → round → match over, ending
at the score limit or when the timer runs out; a pawn that dies respawns at a spawn point.

`--seed N` makes the match deterministic: every random decision draws from one seeded generator
(`MatchRng`), so the same seed plays out the same way. `--bench` runs a budget probe (8 bots, 200
rigid bodies, about 20 000 particles) and prints frame-time percentiles.

## The shape of the sample

`samples/arena/src/lib.rs` is the plugin. It adds the engine plugins the game needs (input with a
`Fire` action, scene, physics, camera, input recording, damage, VFX, audio), registers its
reflected types so the agent protocol can inspect them, installs its rules, and orders its
fixed-step simulation in three sets:

| Set | Systems |
|---|---|
| `ArenaSimSet::Bots` | bots steer and decide to fire (`bots.rs`, or the behavior tree in `bot_bt.rs`) |
| `ArenaSimSet::Weapons` | fire requests become hitscan rays or pooled rockets; explosions push crates (`weapons.rs`) |
| `ArenaSimSet::Match` | the match clock, deaths, scores, respawns (`match_flow.rs`) |

All three run in `FixedUpdate`, so the simulation advances at the same rate whatever the frame
rate. Presentation (HUD, camera, sounds) runs in `Update`.

## Game rules

The match flow is a `GameRules` implementation, `ArenaRules` in `match_flow.rs`. The trait's hooks
are the decisions a mode makes: what happens when the level starts (`on_level_start`: spawn the
pawns and the match state), when a player joins (`on_player_joined`: where to spawn them), when
a character dies (`on_character_died`: respawn with full health), every tick (`tick`: advance
warm-up → round → match over), and whether the match is over (`is_match_over`).

The framework calls `on_player_joined` when a local player joins through `PlayerManager`, and
`RulesPlugin` installs the rules a level names in `LevelSettings.game_rules`. The other hooks are
called by the game at the moments it defines, which keeps the timing explicit. The arena's pattern
is to take the rules out of their resource, call a hook with a `RulesCtx`, and put them back:

```rust
use regia::prelude::*;
use regia_framework::prelude::{GameRules, GameRulesResource, RulesCtx, RulesPlugin, SpawnRequest};

/// A timed free-for-all: the match ends after `limit_secs`.
#[derive(Default)]
pub struct TimedMatch {
    elapsed: f32,
    limit_secs: f32,
}

impl GameRules for TimedMatch {
    fn on_player_joined(&mut self, ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
        // Spawn at the default spawn logic's position (the first PlayerStart).
        let _ = ctx;
        None
    }

    fn tick(&mut self, ctx: &mut RulesCtx) {
        if let Some(time) = ctx.world.get_resource::<Time>() {
            self.elapsed += time.delta_secs();
        }
    }

    fn is_match_over(&self, _world: &World) -> bool {
        self.elapsed >= self.limit_secs
    }
}

/// `FixedUpdate`: advance the rules' clock and report the end of the match once.
fn tick_rules(world: &mut World, mut announced: Local<bool>) {
    let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
    {
        let mut ctx = RulesCtx::new(world);
        rules.rules.tick(&mut ctx);
    }
    let over = rules.rules.is_match_over(world);
    world.insert_resource(rules);
    if over && !*announced {
        *announced = true;
        info!("match over");
    }
}

fn setup(app: &mut App) {
    app.add_plugins(RulesPlugin)
        .insert_resource(GameRulesResource { rules: Box::new(TimedMatch { elapsed: 0.0, limit_secs: 180.0 }) })
        .add_systems(FixedUpdate, tick_rules);
}
```

Register rules by name in `GameRulesRegistry` (`register::<TimedMatch>("my_game::TimedMatch")`)
and a level can select them with `LevelSettings.game_rules`.

The arena keeps its live match state (`phase`, `scores`, time) in an `ArenaState` **component** on
an entity named `"match"`, not in a resource, because the agent protocol's `play.expect` reads
reflected component fields on entities (its resource checks cover a fixed set of engine
resources). Keep state you want to test somewhere tests can read it.

**Try it:** change `ArenaConfig::score_limit` and `match_secs` (in `config.rs`) and watch the
phase change in `ArenaState` with `regia agent call scene.query_entities '{"component":"ArenaState"}'`.

## Bots: a steering brain

By default each bot is a `Pawn` driven by `drive_bots` (`bots.rs`), a single fixed-step system:

1. pick the nearest living enemy within 30 m, re-picking every few seconds;
2. steer toward it (or wander with seeded jitter), clamped to the arena;
3. cast a ray from the eye toward the target with `PhysicsQuery`, excluding itself; if the first
   thing hit is the target, the shot is clear;
4. if clear and the cooldown has elapsed, write a `FireRequest` (every fourth shot a rocket).

The raycast is what makes walls matter: a crate or wall between two bots blocks the shot until
one of them moves. `PhysicsQuery` is the same API the [Physics](../user-guide/physics.md) chapter
describes, and the agent protocol exposes it as `physics.raycast`.

**Try it:** change `ENGAGE_RANGE` or `FIRE_COOLDOWN` in `bots.rs` and compare scores for a fixed
`--seed`.

## Bots: a behavior tree on a navmesh

Insert `ArenaConfig { use_navmesh: true, .. }` before adding the plugin, and the bots switch to
the navigation stack ([Navigation and AI](../user-guide/navigation-ai.md)):

- `navmesh.rs` builds a navmesh from the arena's static geometry with `NavMeshBuilder`;
- each bot gets a `NavAgent` (radius, max speed), a `NavAgentState`, a `Blackboard` and a compiled
  `BehaviorTree`;
- `bot_bt.rs` defines two leaf tasks, `SeekEnemy` (pick the nearest enemy, write it to the
  blackboard, plan a navmesh path into `NavAgentState`) and `FireAtEnemy` (line-of-sight check and
  cooldown), and composes them under a `Parallel` node so the bot moves and shoots at once;
- `NavPlugin` follows the path with ORCA local avoidance, so bots flow around each other, and
  `AiPlugin` ticks every tree in `FixedUpdate` and publishes a `BehaviorTreeStatus` snapshot the
  agent protocol can read.

The tree is built in code:

```rust
use regia_ai::{BehaviorTreeBuilder, BehaviorTreeDef, BtContext, BtStatus, BtTask, BtTaskRegistration, ParallelPolicy};

#[derive(Default, regia_ai::BtTask)]
struct SeekEnemy;

impl BtTask for SeekEnemy {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Running
    }
}

#[derive(Default, regia_ai::BtTask)]
struct FireAtEnemy;

impl BtTask for FireAtEnemy {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Running
    }
}

/// A parallel of seek + fire: both leaves run every tick.
fn build_bot_tree() -> BehaviorTreeDef {
    let mut builder = BehaviorTreeBuilder::new();
    let seek = builder.leaf("seek", <SeekEnemy as BtTaskRegistration>::task_type_path());
    let fire = builder.leaf("fire", <FireAtEnemy as BtTaskRegistration>::task_type_path());
    let root = builder.parallel("bot", ParallelPolicy::RequireAll, vec![seek, fire]);
    builder.build(root)
}
```

The leaves are registered in the `BtTaskRegistry`, and `BehaviorTree::compile(&def, &registry)`
turns the definition into the runtime tree each bot carries. A definition can also be saved as
`.bt.ron`.

**Try it:** add a third leaf that retreats when `Health::current` is low, and put it with
`SeekEnemy` under a selector.

## Weapons, damage and physics

`weapons.rs` turns `FireRequest`s into shots:

- **Rifle (hitscan)**: a `PhysicsQuery` raycast from the muzzle; a hit on a pawn applies damage
  through `regia_framework::damage` (`Health`, `DamageEvent`, `DiedEvent`); a hit elsewhere
  spawns an impact effect and a decal.
- **Rocket (projectile)**: taken from a `PrefabPool` (`rocket`) instead of spawned, advanced in
  `FixedUpdate` with a swept check, and on impact an explosion: radial damage, an `overlap` query
  that writes `ExternalImpulse` on dynamic crates so they fly, an explosion effect, a scorch
  decal and a sound.

Pooling (`regia_framework::pool`) keeps frequently spawned entities alive and recycles them,
which avoids per-shot allocation. `match_flow.rs` reads `DiedEvent`s, scores the killer and calls
`on_character_died` to respawn the victim.

## Effects and sound

The arena registers the built-in showcase effects (`muzzle_flash`, `impact`, `explosion`, …) in a
`VfxScene` and triggers them with `SpawnEffect` messages ([VFX and decals](../systems/vfx.md)).
`audio.rs` builds the sound bank and plays spatial one-shots at the shot's position
([Audio](../user-guide/audio.md)). The HUD in `hud.rs` shows the score, the match timer, the phase and a crosshair,
plus a pause menu that works with mouse, gamepad focus navigation and touch ([UI](../user-guide/ui.md)).

## Testing the match

The arena is built to be verified without a human. Its tests show three techniques you can reuse:

- **Agent assertions** (`tests/agent_expect.rs`): the same `play.expect` conditions an agent sends
  over the protocol, for example "a bot is alive" (`Health.current > 0` on any `Pawn`) and "the
  match is in the round" (`ArenaState.phase == Round`). The
  [agent guide's arena page](../agents/arena.md) lists them.
- **Determinism** (`tests/determinism.rs`): a three-minute bot match is recorded with
  `InputRecording` (the seed plus every fixed-step action value), replayed through
  `InputInjection`, and must end with identical scores.
- **Offscreen screenshots** (`tests/offscreen.rs`): the match renders headless on lavapipe and the
  first-person and overhead frames are checked for content and kept as evidence.

From a terminal, against a running arena:

```console
$ regia agent call play.pause --project samples/arena
$ regia agent call play.step '{"frames":600}' --project samples/arena
$ regia agent call play.expect '{"assertions":[{"name":"round started","condition":{"entity":{"component":"ArenaState"},"component":"ArenaState","path":"phase","op":"eq","value":"Round"}}]}' --project samples/arena
```

(The arena advertises its agent endpoint in its own sample directory, hence `--project
samples/arena`.)

## Soak it

```console
$ cargo build --release -p arena
$ cargo xtask soak --exe target/release/arena --minutes 2 --project-dir samples/arena \
    --input budgets/arena-soak.input.ron -- --bots 16 --no-local-player
```

A soak run plays for minutes and checks frame-time percentiles, the 50 ms hitch rule, memory,
start-up time and log errors against `budgets/default.ron` ([Profiling](../systems/profiling.md)).

## Where next

The arena runs every gameplay system together. The reference chapters list the exact
shapes: [agent methods](../reference/agent-methods.md), [content formats](../reference/formats.md)
and [render profiles](../reference/render-profiles.md).
