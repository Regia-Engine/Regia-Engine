# {{name}}

A Regia Engine project from the **First Person** template (`docs/specs/21` § 3, `docs/02-architecture.md` § 10):
a first-person camera with WASD/mouse-look movement, a crosshair, a gate and a button you use with
E, a pause menu with settings, and quicksave. It is a small starting point: add your own pawn,
weapons and world from here.

## Open it in the editor

From an engine checkout that can build `regia-editor`:

```
regia-editor --project <this directory>
```

## Run the desktop launcher

```
cargo run -p {{crate_name}}_launcher
```

The first build compiles the engine through the path dependency written by `regia new`
(`{{engine}}`). Move the project and the dependency breaks until you point it at the engine again.

## What is in the box

| Part | File | What it does |
|---|---|---|
| Input, camera, save | `game/src/lib.rs` | A **first-person camera** (`CameraRig` in `CameraMode::FirstPerson` welded to the pawn), WASD / left stick movement with sliding collision, mouse / right stick look scaled by the settings sensitivity, and the save: F5 quicksave, F9 quickload (`SaveFlowPlugin`) with `PlayerCheckpoint` and `WorldState` as saved resources. |
| Level | `game/src/level.rs` | A floor, a wall with a gate in it, a button pillar behind the gate and a lamp: procedural boxes, no assets. `Solid` boxes block the player; `blocked` is the check. |
| Interaction | `game/src/interact.rs` | Look at the gate or the button within 2.6 m, read the **prompt**, press **E**. `WorldState` (open doors, buttons on) is saved; `sync_from_state` makes the live `Door` and `Button` match it, so a load restores them. |
| HUD and pause | `game/src/hud.rs` | The **crosshair**, the prompt (`PromptLabel` carries the string for agents), a "Saved" / "Loaded" toast and the Esc **pause menu** (Resume, Save, Load, Settings, Quit). |
| Settings | `game/src/menu.rs` | Master, music and effects volume, mouse sensitivity, invert Y and the graphics preset, persisted through `SettingsPlugin`. |
| Controls screen | `game/src/controls.rs` | F10 opens a frosted panel where the player rebinds each action per device (`regia_ui::RebindPanel`). |
| Tests | `game/tests/agent_smoke.rs` | 120 headless frames, scripted input that moves the pawn, a gate that opens with E and closes again on a load, and the pause flow. |

Controls: WASD move, mouse look, E use, F5 quicksave, F9 quickload, Esc pause, F10 controls.

## Where to extend

- **More interactables.** Spawn an entity with `Interactable { id, prompt_off, prompt_on }` and a
  `Door`, a `Button` or your own component, and add a branch for it in `interact::use_focus`
  (a pickup, a switch, a conversation). Keep its state in `WorldState` (or another saved
  resource) so a load restores it.
- **Weapons and enemies.** The `third_person` template shows health, damage events and an enemy
  state machine; `top_down` shows a navmesh enemy with a behavior tree. Both are written to be
  copied into this crate.
- **The world.** `level::setup_level` builds boxes in code. Replace it with the editor-authored
  `assets/levels/main.level.ron`, and use `regia_physics` colliders instead of `Solid` when you
  need slopes, stairs or jumping.
- **The HUD.** `hud::setup_hud` is plain `regia_ui` nodes; swap the crosshair for a texture, add
  ammo or health text next to the prompt.
- **Saved state.** Add fields to `PlayerCheckpoint`, or register more saveable resources with
  `register_saveable_resource`; the pause menu and F5 / F9 already save them.

## Game code

New scripts go in `game/src/scripts/` and are registered at the `// REGIA:SCRIPTS` marker
(`editor.create_script`, or by hand). Hot reload of that crate is the editor host in
`docs/specs/20-hot-reload.md`.

## Release builds

`regia new` writes `.cargo/config.toml` so a plain `cargo build` does not embed this
machine's directories in the game binary (ADR-10370). It appends this machine's home
directory as the last `--remap-path-prefix`. If you set `RUSTFLAGS`, restate those
flags there, with the home directory last. Read the project directory from the
`CARGO_MANIFEST_DIR` environment variable; the compile-time `env!("CARGO_MANIFEST_DIR")`
embeds the absolute path.
