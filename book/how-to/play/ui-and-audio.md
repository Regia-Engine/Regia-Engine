# UI and audio for a playing game

<!-- index: Runtime UI -->
<!-- index: Theme -->
<!-- index: HUD -->
<!-- index: Sound cue -->
<!-- index: Audio bus -->
<!-- index: Caption -->
<!-- index: Subtitle -->
<!-- index: Focus ring -->
<!-- index: Cursor grab -->

This is the procedure for a HUD, a menu, a theme, a sound cue, and captions
on a game that already runs. Widget and bus details stay on
[Runtime UI](../../user-guide/ui.md) and [Audio](../../user-guide/audio.md).
The UI design record is `docs/specs/ui-default-design.md`
(`docs/specs/18-runtime-ui.md` is the runtime). Audio is
`docs/specs/16-audio.md`. Spoken lines are
`docs/specs/56-dialogue-and-subtitles.md` and
`docs/specs/36-localization-exchange-dialogue-subtitles.md`.

## A HUD the game can update

1. `UiPlugin` is part of `DefaultPlugins` when the `ui` feature is on. Confirm
   the feature before you debug a missing tree.
2. Author `assets/ui/hud.ui.ron`. The frozen shape is version 11. A file at an
   older version still loads. `regia upgrade-project --write` renumbers it.
   Put `safe_area` on a child of the root. `spawn_ui` uses the root's style
   and children only. A score label binds to reflected state:

   `bind: (source: "Score.value", format: "Score: {:03}")`

   That paints `Score: 007` without a system per label. The sample is on
   [Runtime UI](../../user-guide/ui.md#authoring).
3. Give buttons `interactable: true` so keyboard, gamepad, pointer, and touch
   can reach them. The focus ring shows in navigation mode only. A pointer
   click hides it and keeps focus. Set `FocusRingPolicy::Always` when the game
   should show the ring for the mouse too.
4. Keep interactive controls inside the safe area. `SafeArea` reports cutouts
   and system insets on notched phones and handhelds.

The third-person template's HUD (coin counter, toast, pause menu, settings)
is game code in `game/src/hud.rs` and `game/src/menu.rs`. Copy that shape, or
replace it with a `.ui.ron` document. The pause menu entity is named
`pause-menu`. `Time.paused` is true while it is open.

## Pick a theme, and know what is not in the box

Built-in themes are `regia-dark` (the default), `regia-light`,
`regia-high-contrast`, and `regia-light-high-contrast`. Select one with
`ActiveTheme` or `ui.theme_select`. There is no `Regia.toml` `[ui] theme` key.

The player's high-contrast setting selects the matching high-contrast theme.
Reduced motion makes a theme swap instant. `ThemeMode::follow` plus the OS
scheme (`SystemThemeChanged` on Windows, macOS, and Android) tracks light and
dark without host code.

These items from the default-design audit are not built. Do not write a
how-to step that depends on them:

- No JetBrains Mono files ship under `regia_ui/assets/fonts`. `font.mono`
  falls back to the platform monospace.
- No default icon artwork ships. The atlas and `Icon::new("gear")` work after
  you load an `IconSheet`. `<icon=name>` selects a font family named
  `icon-<name>`. It is not a glyph in a built-in sheet.
- There is no Checkbox widget. Radio, spin box, foldable, split, tabs, toast,
  progress, and scroll draw with button or panel looks. They have no theme
  type of their own.
- Toasts do not yet draw role-colored bars or pause on hover.
- Elevation tokens resolve on the themed node. Nothing draws them as a shadow.
- `focus_mode` (`all`, `click`, `none`) is not a setting. The focus ring rule
  above is what ships.

The open list, and the rows that are built, are recorded in
`docs/specs/ui-default-design.md` § 10. The
[Runtime UI](../../user-guide/ui.md)
page repeats it for readers of that chapter.

## A button that plays a sound

1. Put the wave under `assets/audio/` and a cue under your cue set. Cues route
   to a named bus (`Master`, `Music`, `Sfx`, `Voice`). Names are
   case-sensitive. A snapshot crossfades a set of bus volumes (menu versus
   play). See [Audio](../../user-guide/audio.md#buses-and-snapshots).
2. Play the cue from the widget's press, or from a gameplay message. The
   template synthesizes swing, hit, and pickup cues in `game/src/sound.rs`
   and counts them on `SoundStats`.
3. Leave the master limiter at the default 0 dBFS ceiling unless a mix is
   clipping. A lower ceiling is `Audio::set_master_limiter`. Do not use the
   limiter as a volume knob. Lower the bus.
4. For a 3D hit, add an emitter at the impact. Music, UI, and radio bypass
   the propagation medium. Voice and world sounds do not. See
   [Audio](../../user-guide/audio.md#spatial-audio).

Codec features for video are off by default and are not part of this how-to.
The editor video page covers them.

## Captions and a second language

1. Write the line in `.dialogue.ron`, the speaker in `.voice.ron`, and the
   cue times in a `.subs.ron` beside the sound. Play it with
   `regia_framework::dialogue::play_line`. The subtitle uses the caption board
   and the player's caption options. See [Dialogue](../../user-guide/dialogue.md).
2. Put the text key in the source locale's `.ftl`. Switch languages with the
   localization workflow, not by editing the widget string. See
   [Localization](../../user-guide/localization.md).
3. A recording for another language lives under
   `assets/localized/<locale>/` and replaces the base path for that locale.
   `GameSettings::audio_language` may differ from the text language.

## Cursor grab while the menu is closed

A look-driven game sets `Window.cursor_grab` to `Locked` while playing and to
`None` while a menu is open. Apply that on the main thread (`NonSendMarker`).
The platform fallbacks, which you should tell the player about in a settings
tooltip if you show the mode by name:

- `Locked` is requested as `Confined` on Windows and on X11, because that
  grab mode is not supported there. Wayland keeps `Locked`.
- `Confined` is requested as `Locked` on macOS.
- A grab the OS rejects is retried on a later frame. The fallback is logged
  once per process.

Details and the component are on [Input](../../user-guide/input.md).

## Finished check

You are done when all of these are true:

- The HUD shows a bound value that changes when the reflected source changes,
  including the literal text around the placeholder.
- A button activates from the gamepad or keyboard with a visible focus ring,
  and a mouse click does not leave the ring up unless you set `Always`.
- Switching to `regia-light` or a high-contrast theme restyles text and
  buttons without a code change. Reduced motion swaps with no cross-fade.
- The attack or confirm cue is audible on the bus you named, and pausing the
  game ducks or stops it the way your snapshot says.
- A spoken line shows a caption that follows `GameSettings` caption options,
  and a second locale shows that locale's string.
- In play, the cursor is grabbed. In the pause menu, it is free. On Windows
  or X11 a request for `Locked` still confines the cursor.

An agent reads the tree with the UI methods on
[Runtime UI](../../user-guide/ui.md#driving-the-ui-from-an-agent), the mix with
`audio.mix`, and the caption board with `captions.state`.

## When it does not work

- The document spawns and the safe area does nothing. `safe_area` is on the
  root. Move it to a child.
- A label stays on the placeholder. The bind path does not match a reflected
  field, or the format string has more than one placeholder.
- Focus never shows. The player is still in pointer mode. Press a key or a
  gamepad button, or set `FocusRingPolicy::Always`.
- The cue is silent. The bus name does not match (`sfx` is not `Sfx`), or the
  asset failed to cook. Check the cook log before changing gain.
- Theme select appears to do nothing. The call ran before `UiPlugin`, or the
  name is not a loaded theme. `ui.theme_select` reports the miss.

## Where to read next

- [Runtime UI](../../user-guide/ui.md) for widgets, text, and the screen reader.
- [Audio](../../user-guide/audio.md) for snapshots, reverb, and voice chat.
- [Localization](../../user-guide/localization.md) for gather, pseudo-localization,
  and PO exchange.
- [Dialogue](../../user-guide/dialogue.md) for who says a line to whom.
- [Combat and abilities](combat-abilities.md) for the health value the HUD binds.
