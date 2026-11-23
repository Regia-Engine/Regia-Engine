# First Person template

`regia new <name> --template first_person` writes a small first-person game: walk around a room,
look at a gate and a button, read the prompt, press E. It is meant to be read in one sitting and
grown into your own game.

```
$ regia new "My Game" --template first_person
$ cd my_game
$ cargo run -p my_game_launcher     # play
$ cargo test -p my_game             # unit tests and the agent-protocol smoke
```

Controls: WASD move, mouse look, E use, F5 quicksave, F9 quickload, Esc pause, F10 controls.

## The parts

| Part | File | What it does |
|---|---|---|
| Input, camera, save | `game/src/lib.rs` | A first-person `CameraRig` welded to the pawn, movement that slides along walls, look scaled by the settings sensitivity, and the save: `SaveFlowPlugin` with `PlayerCheckpoint` and `WorldState` as saved resources ([Save flow](../agents/save-flow.md)). |
| Level | `game/src/level.rs` | A floor, a wall with a gate, a button pillar and a lamp, all procedural boxes. `Solid` boxes block the player. |
| Interaction | `game/src/interact.rs` | `Interactable`, `Door`, `Button`: the thing you look at within 2.6 m is the focus; E flips it in `WorldState`; a load puts every door and button back. |
| HUD and pause | `game/src/hud.rs` | A crosshair, the prompt, a "Saved" / "Loaded" toast and the pause menu (Resume, Save, Load, Settings, Quit). |
| Settings | `game/src/menu.rs` | Volumes, sensitivity, invert Y and the graphics preset, saved to the user's settings file. |
| Controls screen | `game/src/controls.rs` | F10: rebind each action per device. |
| Tests | `game/tests/agent_smoke.rs` | 120 headless frames, scripted input that moves the pawn, the gate opening with E and closing again on a load, and the pause flow. |

## What agents can read

These reflected components work with `scene.query_entities` and `play.wait_until`
(see [Playtest](../agents/testing.md)):

- `PromptLabel.text` on `hud-prompt`: the prompt on screen (empty when nothing is in reach).
- `ToastLabel.text` on `hud-toast`: "Saved", "Loaded" or "Save failed".
- `Door.open` on `Gate`, `Button.on` on `ButtonPillar`, and the `WorldState` resource.
- `MenuState.screen` (`settings`, `pause` or `none`).

To open the gate as an agent: place the `player` at `(0, 1.6, -2)` with `scene.set_property`, wait for
the prompt `Press E to open the gate`, then `play.inject_input` the key `KeyE`. The smoke test does
exactly this.

## Replace first

1. The interactables: add an entity with `Interactable` and a branch in `interact::use_focus`.
2. The level: `level::setup_level` builds boxes; grow `assets/levels/main.level.ron` in the editor.
3. The saved state: add fields to `PlayerCheckpoint` or register more saveable resources.

The [Third Person template](third-person-template.md) shows health, damage and enemies; the
[Top Down template](top-down-template.md) shows a navmesh enemy. [Which template shows what](project-templates.md)
compares all three.

## Starter lighting

The First Person level spawns one `Sun` directional light at startup: direction
`[0.3, -1.0, -0.4]`, color `[1.0, 0.97, 0.92]`, and 90,000 lux, with shadows enabled.
Edit it in `game/src/level.rs`. Existing generated projects keep their own source;
copy this spawn from a fresh template to add the v0.5.1 lighting fix (ledger B04).
