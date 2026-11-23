# `Regia.toml` `[runtime]` table

Project settings the running game applies at start-up (`docs/specs/11` §§ 2-3, ADR-7070). Rust:
`regia_framework::project_runtime::ProjectRuntimeConfig`, read once by `ProjectRuntimePlugin` (part of
`regia::DefaultPlugins`) from the project root (`$REGIA_PROJECT` or the nearest `Regia.toml`).

```toml
[runtime]
pause_on_focus_loss = false
keep_screen_on = false
```

| Key | Default | Meaning |
|---|---|---|
| `pause_on_focus_loss` | `false` | `true` inserts `regia_platform::PauseOnFocusLoss(true)`: gameplay `Time` pauses while the primary window is out of focus and resumes on focus gain (only a pause it set itself is lifted). |
| `keep_screen_on` | `false` | `true` inserts `regia_platform::KeepScreenOn(true)`: Android keeps the display awake (`FLAG_KEEP_SCREEN_ON`); no effect on desktop. |

The table and both keys are optional, so manifests without them read as before: no version bump. A `false` key leaves a
value the game set in code alone. Unknown keys are ignored. A key of the wrong type logs `runtime_settings_invalid` and
the defaults apply. The agent's `settings.get` reports the live values under `runtime`.
