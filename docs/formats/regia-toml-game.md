# `Regia.toml` `[game]` table

Game settings the running game reads at start-up. Today: the pause key and its screen (ADR-10104,
`docs/specs/14` Part B). Rust: `regia_framework::game_pause::GamePauseConfig`, read once by
`GamePausePlugin` (part of `regia::DefaultPlugins`) from the project root (`$REGIA_PROJECT` or the nearest
`Regia.toml`).

```toml
[game]
pause = false
pause_key = "Pause"
pause_gamepad = "none"
pause_screen = "default"
```

| Key | Default | Meaning |
|---|---|---|
| `pause` | `false` | `true` turns the pause on: the `TogglePause` action in the `Pause` input context pauses and resumes the game. |
| `pause_key` | `"Pause"` | The physical key name of the default `TogglePause` binding (`Pause` is Pause/Break). `"none"` (or empty) binds no key. |
| `pause_gamepad` | none | A gamepad button for `TogglePause`: `South`, `East`, `West`, `North`, `LeftShoulder`, `RightShoulder`, `LeftTrigger`, `RightTrigger`, `LeftStick`, `RightStick`, `DPadUp`, `DPadDown`, `DPadLeft`, `DPadRight`, `Start`, `Select`, `Mode`. `"none"` (the default) binds none. Another name is an error. |
| `pause_screen` | `"default"` | `"default"` (the built-in Paused screen), `"none"` (nothing; the game draws its own), or a `.ui.ron` path under `assets/` (`ui/pause.ui.ron`, `assets://ui/pause.ui.ron` or the template name `pause`). |

The table and every key are optional, so manifests without them read as before: no version bump, and the table is
not a frozen format (`Regia.toml` is the project manifest, not a § 4a content format; the `[runtime]` table set the
same rule, ADR-7070). Keys other than these four are ignored, so later game settings can join the table. A key of
the wrong type, or an unknown `pause_gamepad` name, logs `game_settings_invalid` and the pause stays off. When the
project's input config binds `TogglePause` itself, that binding is used and `pause_key` / `pause_gamepad` add
nothing. The agent's `game.pause_state` reports the live values.

A packaged game has no loose `Regia.toml`: `regia package` copies the table as the `game` object of `build_info.json`
(beside the executable; the APK assets root on Android), and `GamePausePlugin` reads it from there when no manifest
is found, the way `[boot]` travels (ADR-7893).
