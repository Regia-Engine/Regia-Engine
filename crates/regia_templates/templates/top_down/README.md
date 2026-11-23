# {{name}}

A Regia Engine project from the Top Down template (`docs/specs/91-character-save-templates.md`):
a playable top-down starter with click-or-stick movement, a fixed-angle camera, a navmesh
enemy with a behavior tree and a health bar, and quicksave.

## What is in the box

| Part | File | What it does |
|---|---|---|
| Input and camera | `game/src/lib.rs` | WASD / left stick moves relative to the screen; **left click walks to the clicked ground point** (`ground_point` turns the cursor into a ray against the ground); Q / E or the right stick turn the view; F, right click or West attacks; Space / South jumps. The overhead camera is tilted 55 degrees (`CameraMode::Overhead`). F5 quicksaves, F9 quickloads (`SaveFlowPlugin`). |
| Level | `game/src/level.rs` | A ground plate and four pillars: static colliders with box meshes. |
| Enemy | `game/src/enemy.rs` | One enemy that walks a **navmesh** (`regia_nav`: the level's colliders are baked at startup, the enemy is a crowd agent), thinks with a **behavior tree** (`regia_ai`: a `Sense` service, then attack in range, chase when alert, idle otherwise) and wears a **health bar**. |
| Combat | `game/src/combat.rs` | `Health`, `DamageEvent`, the player's attack, death and respawn. |
| HUD | `game/src/hud.rs` | The status line (`StatusLabel` carries the string for agents), the controls hint and a "Saved" / "Loaded" toast. |
| Tests | `game/tests/agent_smoke.rs` | A headless smoke and three agent-protocol tests: scripted input moves the player, an attack kills the enemy and a load brings it back, dying respawns. |

Controls: click or WASD move, F or right click attack, Q / E turn the view, F5 quicksave, F9 quickload.

## Where to extend

- **The player.** `setup_character` in `game/src/lib.rs` builds a box body; replace it with your
  model. `PlayerCheckpoint` (saved) holds where the player stands: add inventory or quest state to
  it, or register more saveable resources with `register_saveable_resource`.
- **More enemies.** `enemy::spawn_enemy` creates one enemy; call it per post and give each its
  own `CrowdLink`. The crowd (`max_agents: 8`) steers them around each other. Add leaves to the
  tree text `enemy::TREE` (patrol, flee) and register them in `EnemyPlugin`.
- **The level.** `level::setup_level` builds boxes in code. Any static box collider is baked into
  the navmesh at startup (`enemy::setup`); for a level authored in the editor, bake from its
  collision meshes instead.
- **Click targets.** `MoveTarget` holds the ground point the player walks to; swap the straight
  line for a navmesh path by asking the crowd (`NavCrowd::add_agent` for the player too).
- **Hit feedback and sound.** `combat::apply_damage` is where every hit passes: add a flash, a
  number or a sound there (the `third_person` template shows a `SoundCue` pattern).

## Open it in the editor

```
regia-editor --project <this directory>
```

## Run the desktop launcher

```
cargo run -p {{crate_name}}_launcher
```

The first build compiles the engine through the path dependency written by `regia new`
(`{{engine}}`). That path is this engine checkout; move the project elsewhere and the
dependency breaks until you point it at the engine again.

## Saves

`PlayerCheckpoint` (position and view) and `CombatProgress` (player health, deaths, whether the
enemy is defeated) are saved resources. Add `Saveable` to anything else that should survive a load, and register game resources with
`app.register_saveable_resource::<T>()`. `SaveInfo` (display name, level, play time) and
`SaveBlobs` (your own bytes by key) ride along in every save. To keep saves in Steam Remote
Storage instead of files, add `regia_steam` and insert
`SaveStore::new(SteamRemoteStorage::new(&steam))`. Agents drive the same flow with
`save.quick_save`, `save.quick_load` and `save.status` (`book/agents/save-flow.md`).

## Game code

`game/src/lib.rs` is the gameplay crate (input wiring, character spawn, camera setup, save).
`cargo test -p {{crate_name}}` runs the unit tests and the agent smoke.
New scripts go in `game/src/scripts/` and are registered at the `// REGIA:SCRIPTS`
marker. Hot reload of that crate is the editor host in `docs/specs/20-hot-reload.md`.
