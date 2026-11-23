# The pause key and the Paused screen (agents)

Topic `game-pause` (ADR-10104). A game can let the player freeze it with a key: the Pause/Break key by
default, rebindable, optionally a gamepad button. The engine ships a default "Paused" screen that a
game can replace. The player-facing page is [Pausing a game](../user-guide/pausing.md).

## Methods

| Method | Params | Answer |
|---|---|---|
| `game.pause` | `{}` | `{ paused: true, changed, frame, screen }`: `changed` is false when the game was already paused; `screen` is what shows (`default`, `asset`, `custom`, or null for `none`). |
| `game.resume` | `{}` | `{ paused: false, changed, frame }`: `changed` is false when the game was not paused. |
| `game.pause_state` | `{}` | `{ enabled, paused, source, since_frame, frame, time_paused, action, key, gamepad, binding_label, screen, screen_setting, screen_shown, screen_error, networked, would_refuse, last_refusal, pauses, resumes, refusals }` |

`game.pause` does what the player's key does. `play.pause` is different: it only freezes time for
tests. It shows no screen, leaves input alone and works in every game.

`source` is who paused: `input` (the key, a gamepad button or a rebinding of them), `screen` (a screen
button), `code` (`GamePause::pause`/`toggle`) or `agent`. `binding_label` is the key name the screen's
hint shows ("Pause", "P", "Menu"), for the device the player last used. `would_refuse` is the reason a
pause would be refused now, null when it would succeed or the game is already paused.

## Errors

All refusals are `F0002` with `data.reason`:

| Reason | When | Fix |
|---|---|---|
| `game_pause_not_installed` | No `GamePausePlugin` on this process. | `regia::DefaultPlugins` adds it; a custom app adds `regia_framework::game_pause::GamePausePlugin`. |
| `game_pause_off` | The game did not turn the pause on. | `[game] pause = true` in `Regia.toml`, or `GamePauseConfig::enabled()` in code. |
| `networked_session` | A networked session runs (a `regia_net` `NetMode` other than `Offline`, or `NetworkedSession(true)`). A peer cannot freeze the other players' worlds. | Pause in a single-player session. The host-authoritative pause (the server freezes the world for everyone) is a later step of ADR-10104. |

While a render-out runs, `game.pause` and `game.resume` answer `F0018`: the render-out owns the clock,
the same rule as `play.pause`.

## What a pause does

- `Time::paused` is set. The gameplay delta is zero and the fixed loop runs no step, so physics,
  animation, AI and everything else that reads gameplay time stands still. UI and audio read
  `RealTime`. If something else had frozen time first (`play.pause`, the focus-loss pause), resuming
  leaves it frozen.
- Every player's input contexts shrink to `Pause` plus the game's `keep_contexts`, so gameplay
  actions read idle. UI navigation works: the keyboard and pointer reach the UI directly, and the
  gamepad d-pad and South button drive the screen's focus. Resuming puts the contexts back.
- The OS cursor is released (`CursorRelease`): free and visible on desktop. Touch devices have no
  cursor. On resume the game's own grab applies again, unless an overlay (Steam) is holding the
  release.
- The screen shows. `GamePaused` / `GameResumed` messages are sent, and the run conditions
  `game_paused` / `game_running` gate systems.

## The screen

`pause_screen = "default"` is the built-in overlay: a dimmed backdrop, a centered "Paused" title, the
hint "Press Pause to resume" (the bound key, updated after a rebinding), and Resume and Quit to menu
buttons. It is a `Dialog` in the accessibility tree (read it with `ui.a11y_tree`), sits on a UI-only
input layer, and focus starts on Resume. Strings come from the game's localization when it defines a
`pause-*` key, otherwise from the bundled en, de, fr and es tables.

The buttons carry the UI commands `pause.resume` and `pause.quit_to_menu`. Any node of a replacement
screen with those commands works the same way. Both end the pause and send `PauseScreenEvent`
(`Resume` or `QuitToMenu`); the game loads its menu level on `QuitToMenu`.

`pause_screen = "none"` shows nothing; the game draws its own screen from `GamePaused`. A `.ui.ron`
path (`ui/pause.ui.ron`) spawns that template. If the template is not loaded, the built-in screen
shows and `screen_error` starts with `pause_screen_missing`. In code,
`PauseScreen::custom(|world| ...)` builds the screen and returns its root; the pause despawns it on
resume.

## Checking it from an agent

```json
{"method": "game.pause_state"}
{"method": "game.pause"}
{"method": "ui.a11y_tree"}
{"method": "game.resume"}
```

To press the real key, use `play.inject_input` with the key `Pause` (or the rebound key), then read
`game.pause_state` on the next frame. In a networked test, `game.pause` answers `networked_session` and
nothing freezes.

## Tests

`cargo test -p regia_framework --lib game_pause` (key toggle, gameplay input ignored, cursor, the
default, custom, `none` and asset screens, screen buttons, networked refusal, rebinding, saved
overrides, settings round trip) and `cargo test -p regia_agent --lib game_pause`.
