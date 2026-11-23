# UI runtime theme switch (agents)

UI series D15, ADR-865 and ADR-1309. Builds on the theme resolver (D6, ADR-289): writing `ActiveTheme` swaps the
theme on the next frame and cross-fades every themed node's colors over the theme's `motion.base` (160 ms in the
built-in themes), along its `ease.standard` curve, blended in OKLab. This page covers what makes the swap complete
on screen, how to shape the fade, the change messages and the `ui.theme_mode` method.

## What follows the theme

- **Background, border and text colors** of every themed node (a node with `Classes` or a `ThemeOverride`)
  cross-fade together. A second switch during a fade starts from the colors on screen, so nothing jumps.
- **Label text.** The themed node's own `Text` and its label children (childless `Text` children without a look of
  their own) take the theme's text color every frame of the fade. Glyphs are recolored without a reshape.
  Only theme-owned spans change: spans at the default color (`TextStyle::default().color`) or at the color the theme
  wrote last. Authored colors (a red warning) stay. Add `KeepTextColor` to a `Text` to opt it out.
- **High contrast and reduced motion.** The player's `UiAccessibility::high_contrast` selects
  `<theme>-high-contrast` when loaded, else `regia-high-contrast`. The built-ins ship both `regia-high-contrast`
  (dark) and `regia-light-high-contrast`, so a light-scheme player stays light. `reduced_motion` makes the swap
  instant. They are copied into `ActiveTheme` when the setting changes. On the first frame only settings that are on
  are copied, so a host that preset `ActiveTheme::high_contrast` keeps it.
- **Scheme preference.** `ThemeMode { follow, dark, light, system_scheme }` selects `dark` or `light` (default
  `regia-dark` / `regia-light`) for a `dark`, `light` or `system` preference. With `follow: None` (the default)
  the host selects themes directly.
- **OS appearance.** The platform writes `SystemThemeChanged { window, dark }` and `system_scheme` follows it: on
  Windows and macOS when the window opens and whenever the OS appearance changes (winit); on Android from the
  activity's night mode when the window opens, on every resume and on every configuration change. Linux reports none
  yet: there the host writes `system_scheme` itself, or an agent passes `system_scheme`.
- **Settings menu.** A game stores the player's choice in `GameSettings` `accessibility.color_scheme` (`Dark`,
  `Light`, `System`, or `None` to leave it to the game); the settings plugin copies it into `ThemeMode::follow` and
  persists it with the other settings.

Idle frames never bump the theme generation; a change costs one generation.

## Shaping the swap fade

The `ThemeTransition` resource shapes the next swap (a running fade keeps the settings it started with):

| Field | Default | Meaning |
|---|---|---|
| `duration` | `None`: the new theme's `motion.base` | Fade length in seconds, clamped to 0..10; `Some(0.0)` swaps at once |
| `ease` | `None`: the new theme's `ease.standard` (linear if missing) | `Easing::Linear` or `Easing::CubicBezier([x1, y1, x2, y2])` |
| `space` | `BlendSpace::Oklab` | `Oklab` moves lightness evenly; `Linear` is the plain linear-light blend |

Why OKLab: a linear-light blend from a dark surface to a white one is already at half the light's luminance a
quarter of the way in, which reads as most of the way to light: the screen seems to flash. OKLab interpolates
perceptual lightness, so the fade brightens evenly. A fill fading from or to transparent keeps its color and changes
only its alpha. State transitions (hover, pressed, selected) keep their own `transition` timing; a node whose state
changes mid-swap leaves the swap fade and animates to its new look.

Reduced motion (`ActiveTheme::reduced_motion` or the player's setting) still makes every swap instant.

## Change messages

- `ThemeChanged { from, to, generation, fade_secs }`: written in the frame a new theme generation applies (any
  `ActiveTheme` change, a density change or a theme set change). `from` is `None` for the first generation.
- `ThemeFadeFinished { theme, generation }`: written in the frame the cross-fade ends. An instant swap writes it in
  the same frame as its `ThemeChanged`; a swap replaced mid-fade by another writes it only for the last one.

A game switches its own non-UI colors (clear color, post-process tint) on these, in step with the UI.

## Method

`ui.theme_mode { theme?, follow?, system_scheme?, dark?, light?, high_contrast?, reduced_motion?, transition? }`
(permission `edit_scene`). With no parameters it only reports.

| Parameter | Meaning |
|---|---|
| `theme` | Select a loaded theme; following a scheme stops. |
| `follow` | `dark`, `light`, `system` or `off`. |
| `system_scheme` | `dark`, `light` or `unknown` (reads as dark): stands in for the OS setting. |
| `dark`, `light` | The themes the two schemes select. |
| `high_contrast`, `reduced_motion` | The live theme flags (`ui.accessibility` changes the player's settings instead). |
| `transition` | `{ duration_ms?, ease?, space? }` for the next swap. `duration_ms`: 0 to 10000, or `null` for the theme's `motion.base`. `ease`: `"theme"` (the theme's `ease.standard`), `"linear"`, `"cubic-bezier(x1, y1, x2, y2)"` or `[x1, y1, x2, y2]` (x values in 0..1). `space`: `"oklab"` or `"linear"`. Fields left out are kept. |

Result: `{ active, requested, previous, generation, applies_at_generation, animating, fade_seconds, transition,
fade, follow, system_scheme, dark, light, high_contrast, reduced_motion, themes }`.

- `active` is the theme nodes resolve against (after the high-contrast and fallback rules); `previous` the one
  before the last generation.
- `animating` counts nodes with any color animation running (swap or state transition).
- `fade_seconds` is the length the next swap would run.
- `transition` echoes the settings: `{ duration_ms (null = theme), ease ("theme", "linear" or [x1, y1, x2, y2]),
  space }`.
- `fade` describes the last swap: `{ fading, nodes, seconds, remaining_seconds, ease, space }`.

Wait for a swap to settle before a screenshot with the `ui.theme_fading` atom:

```json
{"method": "play.wait_until", "params": {"condition": {"predicate": "ui.theme_fading", "is": false}}}
```

The atom `{ predicate: "ui.theme_fading", is? }` holds when whether a swap fade runs equals `is` (default `true`).
It is broken (`bad_flag`) when `is` is not a boolean and (`ui_not_installed`) without the UI plugin.

```json
{"method": "ui.theme_mode", "params": {"theme": "regia-light"}}
{"method": "ui.theme_mode", "params": {"follow": "system", "system_scheme": "light"}}
{"method": "ui.theme_mode", "params": {"high_contrast": true}}
{"method": "ui.theme_mode", "params": {"transition": {"duration_ms": 400, "ease": "linear", "space": "oklab"}, "theme": "regia-light"}}
```

## Errors

| Code | Meaning |
|---|---|
| `unknown_theme` | no loaded theme has that name (`themes` lists them) |
| `bad_theme` | a theme parameter is not a non-empty string |
| `bad_follow` | `follow` is not `dark`, `light`, `system` or `off` |
| `bad_scheme` | `system_scheme` is not `dark`, `light` or `unknown` |
| `bad_flag` | `high_contrast` or `reduced_motion` is not a boolean |
| `theme_and_follow` | both `theme` and a `follow` preference were given |
| `bad_transition` | `transition` is not an object, or has a field other than `duration_ms`, `ease`, `space` |
| `bad_duration` | `transition.duration_ms` is not a number from 0 to 10000 (or `null`) |
| `bad_ease` | `transition.ease` is not `"theme"`, `"linear"`, a `cubic-bezier(...)` string or four numbers with x in 0..1 |
| `bad_space` | `transition.space` is not `"oklab"` or `"linear"` |
| `ui_not_installed` | the app has no runtime UI themes |

## Known limits

- The OS light/dark setting reaches the engine on Windows, macOS and Android; on Linux the host feeds
  `ThemeMode::system_scheme`.
- Geometry (radius, border width, and a theme's `height`, `padding_x` and `gap`, which reach layout since ADR-9340)
  changes at the first frame of a swap; only colors fade.
- Images, icons with baked colors, and explicit `Background` components do not cross-fade.
- A text color tween (`ui.tween`) and a theme fade on the same text at the same time: the theme wins for that frame.
