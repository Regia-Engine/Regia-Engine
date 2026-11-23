# {{name}}

A Regia Engine project from the Third Person template (`docs/specs/21` § 3): a playable
third-person starter. Every part here is a rewrite, for a fresh project, of a feature of the
engine's `samples/third_person`, small enough to read in one sitting and to replace piece by piece.

## What is in the box

| Part | File | What it does |
|---|---|---|
| Input and save | `game/src/lib.rs` | Action map (WASD / left stick, mouse / right stick, Space / South, F5, F9, Esc), the camera-relative move, and the save: `SaveFlowPlugin` (`book/agents/save-flow.md`) with `PlayerCheckpoint` and `CollectProgress` as saved resources. |
| Level | `game/src/level.rs` | Ground, two blocks and four stairs, each a static collider with a box mesh. |
| Character and camera | `game/src/character.rs` | The rapier character controller (move, jump, coyote time), the boom camera that shortens when geometry blocks it, and a procedural body: torso, head, swinging arms and feet. A ray under each foot places it on the ground (foot IK), so feet land on stairs. |
| Collectibles | `game/src/collectible.rs` | Four coins you collect by walking into them. `CollectProgress` records which ids are gone; `reconcile` makes the world match it, so a load brings back a coin collected after the save. |
| HUD and pause menu | `game/src/hud.rs` | The `Coins: n / 4` counter, a "Saved" / "Loaded" toast and the Esc pause menu (Resume, Save, Load, Quit). `HudLabel` carries the counter string so agents can read it. |
| Combat | `game/src/combat.rs` | `Health`, `DamageEvent`, `Died`, and the three-hit sword combo on J / left click / West (press again inside 0.9 s to continue; the third swing hits hardest). |
| Enemies | `game/src/enemy.rs` | Three grunts: idle at a post, chase when they see you, wind up and swing, flinch when hit, topple when dead. `CombatProgress` saves which are dead; a load brings back the rest. |
| Death and respawn | `game/src/respawn.rs` | Health zero shows "You died", then the player stands at the spawn point with full health. |
| Title and settings | `game/src/menu.rs` | A title screen (Play, Continue, Settings, Quit) and the settings panel (volumes, sensitivity, invert Y, graphics preset). Hosts skip the title with `GameFlowSettings { skip_main_menu: true, .. }`. |
| Sound | `game/src/sound.rs` | Synthesized swing, hit, fall, pickup and footstep clips behind `SoundCue`, plus an ambient bed. |
| Tests | `game/tests/agent_smoke.rs` | A headless smoke (120 frames) and agent-protocol tests: scripted input moves the character, a coin pickup survives a save and load, a swing kills a grunt and a load brings it back, dying respawns, and the title starts the game. |

Controls: WASD move, Space jump, mouse look, J or left click attack, F5 quicksave, F9 quickload, Esc pause.

## Run it

```
cargo run -p {{crate_name}}_launcher        # the desktop game
cargo test -p {{crate_name}}                # headless smoke + agent-protocol checks
regia-editor --project <this directory>     # the editor
```

The first build compiles the engine through the path dependency written by `regia new`
(`{{engine}}`). That path is this engine checkout; move the project elsewhere and the
dependency breaks until you point it at the engine again.

`regia package --platform linux` writes a tarball with the launcher, the cooked assets pack and
`Regia.toml` (`docs/specs/21` § 4).

## Where to extend

- **More fights.** `enemy::posts()` lists where the grunts stand; add a post and its id is saved
  for free (`CombatProgress.defeated`). The brain is `enemy::think`, a plain state machine; for
  enemies that walk around obstacles, the `top_down` template drives its enemy with a navmesh
  crowd and a behavior tree.
- **A different weapon.** `combat::COMBO` holds the damage and recovery of the three swings, and
  `player_attack` decides who is hit. Anything else that hurts (a trap, a projectile) writes a
  `DamageEvent`; nothing else changes.
- **Respawn at checkpoints.** `respawn::run_respawn` puts the player at `SPAWN_POINT`; store the
  last checkpoint in `CombatProgress` (it is saved) and read it there.
- **Menus.** `menu.rs` is the title and settings panels; `hud.rs` holds the pause menu. Both are
  plain `regia_ui` nodes with a `Name` each, so agents can click them.
- **Sound.** Gameplay writes a `SoundCue`; `sound::build_sounds` makes the clips (synthesized, so
  there are no files). Replace a clip with a `SoundAsset` loaded from your own audio.

## What to replace first

1. **The body.** `attach_rig` in `character.rs` builds the character from boxes. Load your own
   model and drive it from `CharacterState` (speed, grounded, velocity); keep `animate_rig`'s
   foot ray if you want ground contact.
2. **The pickups.** `spots()` in `collectible.rs` lists where they sit. The pickup itself is a
   distance check in `collect_nearby`: add your reward there. For trigger volumes with scripts see
   `samples/third_person` and `book/agents/physics.md`.
3. **The level.** `level.rs` builds blocks in code; the project's `assets/levels/main.level.ron`
   is the editor-authored level you can grow instead.
4. **Saved state.** Add fields to `PlayerCheckpoint` or register more saveable resources
   (`register_saveable_resource`); the pause menu and F5 / F9 already save them. Saves go to the
   save directory of the framework (`SaveDirectory`), under the slot `Chapter1` (menu) and `Quick`
   (F5 / F9).

New scripts go in `game/src/scripts/` and are registered at the `// REGIA:SCRIPTS` marker in
`game/src/lib.rs`. Hot reload of that crate is the editor host in `docs/specs/20-hot-reload.md`.
