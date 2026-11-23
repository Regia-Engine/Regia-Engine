# Third Person template

`regia new <name> --template third_person` writes a playable third-person game you can read in one
sitting and replace piece by piece. It is a rewrite, for a fresh project, of the features of the
engine's `samples/third_person`: the files below are compiled into the `regia` binary
(`crates/regia_templates`), so the template needs nothing from the engine checkout except the
engine crates themselves.

```
$ regia new "My Game" --template third_person
$ cd my_game
$ cargo run -p my_game_launcher     # play
$ cargo test -p my_game             # headless smoke and agent-protocol checks
```

Controls: WASD move, Space jump, mouse look, J or left click attack, F5 quicksave, F9 quickload, Esc pause menu. The game opens on a title screen (Play, Continue, Settings, Quit).

## The parts

| Part | File | What it does |
|---|---|---|
| Input and save | `game/src/lib.rs` | The action map, camera-relative movement, and the save: `SaveFlowPlugin` with `PlayerCheckpoint` (position, view yaw) and `CollectProgress` as saved resources ([Save flow](../agents/save-flow.md)). |
| Level | `game/src/level.rs` | Ground, two blocks and four stairs: static colliders with box meshes. |
| Character and camera | `game/src/character.rs` | The rapier character controller, the boom camera, and a procedural body whose feet are placed on the ground with a ray under each foot (foot IK). |
| Collectibles | `game/src/collectible.rs` | Four coins; `CollectProgress` records which are gone and `reconcile` makes the world match it, so a load restores a coin collected after the save. |
| HUD and pause menu | `game/src/hud.rs` | The `Coins: n / 4` counter, a "Saved" / "Loaded" toast and the pause menu (Resume, Save, Load, Quit, Settings). |
| Combat | `game/src/combat.rs` | `Health`, `DamageEvent`, `Died` and the three-hit sword combo: a press inside 0.9 s of a swing continues it, and the third swing hits hardest. |
| Enemies | `game/src/enemy.rs` | Three grunts: idle at a post, chase what they see, wind up, swing, flinch when hit, topple when dead. `CombatProgress` saves who is dead and a load brings the others back. |
| Death and respawn | `game/src/respawn.rs` | Health zero shows "You died"; after 2.5 s the player stands at the spawn point at full health. |
| Title and settings | `game/src/menu.rs` | The title screen and the settings panel (volumes, sensitivity, invert Y, graphics preset); the pause menu opens the same panel. |
| Sound | `game/src/sound.rs` | `SoundCue` messages play synthesized swing, hit, fall and pickup clips, footsteps and an ambient bed. `SoundStats` counts what played. |
| Tests | `game/tests/agent_smoke.rs` | 120 headless frames, scripted input that moves the character, a coin pickup that survives a save and load, a swing that kills a grunt and a load that brings it back, a death that respawns, and the title starting the game. |

## What agents can read

The template adds these reflected components, so `scene.query_entities` and `play.wait_until`
can use them (see [Testing](../agents/testing.md)):

- `HudLabel.text` on the entity named `hud-counter`: the counter string the screen shows.
- `ToastLabel.text` on `hud-toast`: the current "Saved" / "Loaded" / "Save failed" message.
- `CollectibleId` on every coin that is still in the world.
- `CombatLabel.text` on `hud-combat` (`Health: 100 / 100`), `EnemiesLabel.text` on `hud-enemies` (`Enemies: 0 / 3`) and `BannerLabel.text` on `hud-banner` (`You died` or empty).
- `Health` on the player and on every grunt (`Grunt0`, `Grunt1`, `Grunt2`), `EnemyId` on each grunt, `Fighter` on the player, and the `CombatProgress`, `Respawn`, `MenuState` and `SoundStats` resources.
- `PlayerCheckpoint` and `CollectProgress` as resources.

To kill a grunt as an agent: set `Health.current` of `Grunt0` to 5, place the player 1.5 m in front of it
at `(-5, 1, -7.5)` and `play.inject_input` the key `KeyJ`; the `hud-enemies` label then reads `Enemies: 1 / 3`.

The pause menu is the entity named `pause-menu`; `Time.paused` is true while it is open.

## Replace first

1. The body (`attach_rig`): load your model and drive it from `CharacterState`.
2. The pickups (`spots()` and `collect_nearby`): put your rewards where the id is pushed.
3. The level: `level.rs` builds boxes in code; grow `assets/levels/main.level.ron` in the editor.
4. The saved state: add fields to `PlayerCheckpoint` or register more saveable resources.

The engine's third-person sample adds a skinned animated character, behavior-tree enemies on a
navmesh crowd, a montage-timed combo, ragdolls, trigger-volume scripts, split-screen and a
vehicle; read it when you need those. [Which template shows what](project-templates.md) compares
this template with the First Person and Top Down ones.

## Starter lighting

The Third Person level spawns one `Sun` directional light at startup: direction
`[0.3, -1.0, -0.4]`, color `[1.0, 0.97, 0.92]`, and 90,000 lux, with shadows enabled.
Edit it in `game/src/level.rs`. Existing generated projects keep their own source;
copy this spawn from a fresh template to add the v0.5.1 lighting fix (ledger B04).
