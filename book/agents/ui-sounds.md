# UI sounds (agents)

ADR-273 T12, ADR-1842. Code: `regia_ui::ui_sounds`.

`UiSounds` maps six cues to audio cue names. The UI turns events into cues: pointer enter is `hover`, a click is `press`,
focus arriving is `focus`, a toggle change is `toggle`, any other value change or a submitted text is `confirm`, and a
context click is `cancel`. For each event with a sound in the table the UI sends one `UiSoundRequest`; the `regia` facade
plays it with `Audio::play_cue`, so the audio cue's cooldown, variation and steal rules apply. An unknown audio cue is
ignored (logged at debug level). At most `max_per_frame` requests go out per frame (default 4); the rest are counted in
`dropped`.

A node can carry `UiSoundOverride { muted, sounds }` to silence itself or to replace a cue (an empty name silences that
cue only). The table starts empty: no sound plays until a game or theme fills it.

## Methods

- `ui.sounds { enabled?, max_per_frame?, sounds?, clear_log? }`: edits the table (`sounds` maps a cue name to an audio
  cue name, `null` removes it) and always reports `{ enabled, max_per_frame, sounds, recent, dropped }`. `recent` lists the
  last 64 requests `{ cue, sound, entity }`, which is how a test checks a UI sound without audio hardware. Mutating;
  permission `edit_scene`.

```json
{"method": "ui.sounds", "params": {"sounds": {"hover": "ui_hover", "press": "ui_press"}}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_flag` | `enabled` or `clear_log` is not a boolean |
| `bad_max` | `max_per_frame` is not a whole number from 1 to 64 |
| `bad_sounds` | `sounds` is not an object of known cue names to strings or null |

`enabled` and `clear_log` are optional booleans. `max_per_frame` is an integer from 1 through 64 (default 4). Each
`sounds` value is a string of at most 256 bytes or `null` to remove that cue; valid keys are `hover`, `press`, `focus`,
`confirm`, `toggle` and `cancel`.

## Failure example

Captured CLI output from `regia agent call ui.sounds '{"max_per_frame":0}'`:

```text
error: method call failed: `max_per_frame` must be a whole number from 1 to 64 (-32602): {"code":"bad_max","hint":"4 is the default"}
```

## Workflow

Set one or more cue names, cause the corresponding UI event, then call `ui.sounds` with `clear_log: false` to inspect
`recent`. The log records requests even when no audio device is available; clear it before a later isolated check.
