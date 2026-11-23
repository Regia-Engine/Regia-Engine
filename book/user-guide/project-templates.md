# Which template shows what

`regia new --template <id>` offers six starting points. This page compares the three character
templates; `blank`, `quick` and `net_game` are described in [Your first project](../getting-started.md).
Every character template ships `game/tests/agent_smoke.rs`, which drives the game through the agent
protocol, and has a "Where to extend" section in its README.

| | Third Person | First Person | Top Down |
|---|---|---|---|
| Camera | Boom camera that shortens on collision | First-person rig on the pawn | Fixed-angle overhead, Q / E turn |
| Movement | Rapier character: run, jump, foot placement | Kinematic walk with wall sliding | Rapier character; click to walk or stick |
| Player action | Three-hit sword combo | Use (E) on a door or button | Attack |
| Enemies | Three grunts (state machine), topple on death | none | One navmesh agent under a behavior tree |
| Health | Player health, "You died", respawn | none | Player health, enemy health bar, respawn |
| Interaction | Coin pickups | Prompt, gate, button, lamp | Click targets |
| HUD | Counter, health, enemies, toast | Crosshair, prompt, toast | Status line, toast |
| Menus | Title, pause, settings | Pause, settings, F10 rebinding | none |
| Sound | Synthesized cues and an ambient bed | none | none |
| Save | F5 / F9; position, coins, defeated enemies, health (a dead save restores full health) | F5 / F9; position, doors, buttons | F5 / F9; position, health, enemy defeated |
| Engine crates shown | `regia_framework` (character, camera, save flow, game flow, settings), `regia_audio`, `regia_ui` | `regia_framework` (camera, save flow, game flow, settings), `regia_ui` | `regia_nav`, `regia_ai`, `regia_framework` |

The three share one rule: the saved state lives in resources (`register_saveable_resource`), and
the live entities are reconciled with it after a load, so a save never has to serialize a mesh.

Every template is generated from `crates/regia_templates/templates/<id>` and compiled into the
`regia` binary. A template is under 1 MB of text and ships no binary assets: meshes are
procedural and sounds are synthesized.

## Checking the templates

`cargo xtask templates-check` generates each template, builds it and runs its launcher for five
frames. `cargo test -p regia_cli --test package_third_person_smoke -- --ignored` runs
`regia package` once per character template. Inside a generated project, `cargo test` runs the
unit tests and the agent smoke.

See the [Third Person](third-person-template.md), [First Person](first-person-template.md) and
[Top Down](top-down-template.md) tours.
