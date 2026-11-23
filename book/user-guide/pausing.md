# Pausing a game

Regia Engine can give your game a pause key. When the player presses it, everything in the game
world freezes, physics included, and a "Paused" screen appears. Pressing the key again, or choosing
Resume, carries on from the same moment. The pause is off until your game turns it on.

## Turning it on

Add a `[game]` table to `Regia.toml`:

```toml
[game]
pause = true               # the pause key works in this game (default false)
pause_key = "Pause"        # the keyboard key (default: Pause/Break; "none" for no key)
pause_gamepad = "Start"    # a gamepad button (default: none)
pause_screen = "default"   # "default", "none", or a UI file such as "ui/pause.ui.ron"
```

Only `pause = true` is needed; the other keys have the defaults shown. Key names are physical key
names (`Pause`, `Escape`, `KeyP`, `F10`). Gamepad buttons are `South`, `East`, `West`, `North`,
`LeftShoulder`, `RightShoulder`, `LeftTrigger`, `RightTrigger`, `LeftStick`, `RightStick`, `DPadUp`,
`DPadDown`, `DPadLeft`, `DPadRight`, `Start`, `Select` and `Mode`.

`regia::DefaultPlugins` reads the table. The key is an ordinary input action, `TogglePause`, in the
`Pause` input context, so players can rebind it in your controls menu like any other action. If your
own input config already binds `TogglePause`, your binding is used instead of the default.

From code you can do the same without `Regia.toml`:

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
# let world: &mut World = todo!();
use regia_framework::game_pause::{GamePause, GamePauseConfig, PauseScreen};

// Before `DefaultPlugins` (which adds `GamePausePlugin` and keeps this resource):
let mut pause = GamePauseConfig::enabled();
pause.screen = PauseScreen::None;
app.insert_resource(GamePause::new(pause));
app.add_plugins(regia::DefaultPlugins);

// Later: an options-menu switch, and an in-game menu button.
world.resource_mut::<GamePause>().set_enabled(true);
world.resource_mut::<GamePause>().toggle();
```

An app without `DefaultPlugins` adds `GamePausePlugin { config: Some(...), ..Default::default() }` itself.

## What freezes

The pause sets `Time::paused`. Gameplay time stops and the fixed-step loop runs no step, so physics,
animation, particles, AI and your own `FixedUpdate` systems all stand still. Frames keep drawing, and
UI and audio keep running on real time.

While paused, gameplay input is ignored: only the `Pause` context stays on, so actions like Jump or
Fire read as not pressed. The pause key, the screen's buttons and UI navigation still work. If your
game has its own menu context that should keep working, list it in
`GamePauseConfig::keep_contexts`. On desktop the mouse cursor is freed and shown; when the game
resumes, your game's cursor grab applies again.

Systems that should stop during a pause but do not use gameplay time can use the run conditions
`game_running` and `game_paused`, or read the `GamePaused` and `GameResumed` messages.

## The Paused screen

The default screen dims the frozen scene and shows "Paused", a hint such as "Press Pause to resume"
(it names whatever key the player bound), and two buttons: Resume and Quit to menu. Players can use
the mouse, the keyboard or a gamepad's d-pad. Screen readers announce it as a dialog. The text is
translated into English, German, French and Spanish, and you can change any line by defining its key
in your own localization files:

| Key | Default text |
|---|---|
| `pause-title` | Paused |
| `pause-hint` | Press { $key } to resume |
| `pause-hint-no-key` | Choose Resume to continue |
| `pause-resume` | Resume |
| `pause-quit-to-menu` | Quit to menu |
| `pause-dialog-description` | The game is paused. Nothing in the game world moves until you resume. |

Quit to menu ends the pause and sends `PauseScreenEvent::QuitToMenu`. Your game handles it, for
example by loading its menu level:

```rust
# use regia::prelude::*;
# use regia_framework::game_pause::PauseScreenEvent;
# use regia_framework::level_switch::LevelSwitch;
fn on_pause_menu(mut events: MessageReader<PauseScreenEvent>, mut switch: ResMut<LevelSwitch>) {
    for event in events.read() {
        if *event == PauseScreenEvent::QuitToMenu {
            switch.request("levels/menu.level.ron");
        }
    }
}
```

## Your own screen

- `pause_screen = "ui/pause.ui.ron"` shows a UI file from your `assets/ui` folder instead. Give its
  buttons the commands `pause.resume` and `pause.quit_to_menu` and they work like the built-in ones.
  If the file is missing, the built-in screen shows, so the player is never stuck.
- `pause_screen = "none"` shows nothing. Draw your own screen when `GamePaused` arrives and remove it
  on `GameResumed`.
- In code, `PauseScreen::custom(|world| { ...; root })` builds the screen and returns its root
  entity. The engine removes it when the game resumes.

## Multiplayer

In a networked session the pause is refused: one player cannot freeze everyone else's game. The
refusal sends `PauseRefused` with the reason `networked_session`, so you can tell the player. A pause
where the host freezes the game for everyone is planned for later.

## Agents

Agents use `game.pause`, `game.resume` and `game.pause_state`. See
[The pause key and the Paused screen (agents)](../agents/game-pause.md).
