# Top Down template

`regia new <name> --template top_down` writes a top-down game with click or stick movement, a
fixed-angle camera, one enemy that walks a navmesh under a behavior tree, and a health bar.

```
$ regia new "My Game" --template top_down
$ cd my_game
$ cargo run -p my_game_launcher     # play
$ cargo test -p my_game             # unit tests and the agent-protocol smoke
```

Controls: click or WASD / left stick move, Space / South jump, F / right click / West attack, Q and E
turn the view, F5 quicksave, F9 quickload.

## The parts

| Part | File | What it does |
|---|---|---|
| Input, camera, save | `game/src/lib.rs` | `CameraMode::Overhead` tilted 55 degrees; screen-relative movement; a left click turns the cursor into a ray, meets the ground and walks there (`ground_point`, `MoveTarget`); `SaveFlowPlugin` with `PlayerCheckpoint` and `CombatProgress` as saved resources. |
| Level | `game/src/level.rs` | A ground plate and four pillars (static colliders). |
| Enemy | `game/src/enemy.rs` | The level's colliders are baked into a navmesh (`regia_nav`) at startup. The enemy is a crowd agent; a `regia_ai` behavior tree (`TREE`: a `Sense` service, then attack in range, chase when alert, idle otherwise) steers it. A flat box over it is the health bar. |
| Combat | `game/src/combat.rs` | `Health`, `DamageEvent`, the player's attack, death and respawn. |
| HUD | `game/src/hud.rs` | The status line, a hint and a save toast. |
| Tests | `game/tests/agent_smoke.rs` | 120 headless frames, scripted input, an attack that kills the enemy (a load brings it back), and a death that respawns. |

## What agents can read

- `StatusLabel.text` on `hud-status` (`Health: 100 / 100`) and `EnemyLabel.text` on `hud-enemy` (`Enemy: 36`, or `Enemy: down`).
- `ToastLabel.text` on `hud-toast`.
- `Health.current` on the player and on `Enemy`; `CombatProgress` as a resource.
- The enemy's `Blackboard` and behavior tree status through the [AI](../agents/ai.md) methods.

## Replace first

1. The player body: `setup_character` builds a box.
2. More enemies: `enemy::spawn_enemy` creates one; call it per post.
3. The tree: add leaves to `enemy::TREE` and register them in `EnemyPlugin`.
4. The level: any static box collider is baked into the navmesh, so a pillar you add is an obstacle.

See [Which template shows what](project-templates.md) for the other templates.

## Starter lighting

The Top Down level spawns one `Sun` directional light at startup: direction
`[0.3, -1.0, -0.4]`, color `[1.0, 0.97, 0.92]`, and 90,000 lux, with shadows enabled.
Edit it in `game/src/level.rs`. Existing generated projects keep their own source;
copy this spawn from a fresh template to add the v0.5.1 lighting fix (ledger B04).
