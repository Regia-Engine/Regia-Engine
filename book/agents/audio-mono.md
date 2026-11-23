# Mono audio (agents)

ADR-2646. A player who hears with one ear, or plays with one earbud, misses sounds panned to the other side.
`Audio::set_mono_output(true)` downmixes the whole output to mono: both channels carry `(left + right) / 2`. The
engine-wide accessibility review is `docs/accessibility-review.md`.

## Where it acts

In the master output stage on kira's main track, after every bus and send bus and before the master limiter
([audio mix](audio-mix.md)). Non-finite samples are silenced first. Equal halves keep a centered sound (most
dialogue) at its level; a sound panned hard to one side reaches both ears 6 dB down, and the sum is never louder
than the louder input. Off, the stage is bit-exact as before. The setting survives an output rebuild (resume on
Android). It also applies to the offline backend, which render-out uses.

## Persistence

`GameSettings.accessibility.mono_audio` (missing in older files: off) is saved with the other settings and
copied into the output stage when the settings change.

## Method

- `audio.mono { enabled? }`. With no parameters it reports. With `enabled` it changes `GameSettings` (persisted
  like `settings.set`) and the live output stage. Returns `{ enabled, live, persisted }` (`live`: an `Audio`
  resource exists). Mutating; permission `edit_scene`.

```json
{"method": "audio.mono", "params": {}}
{"method": "audio.mono", "params": {"enabled": true}}
```

## Errors

| Code | Meaning |
|---|---|
| `audio_mono_bad_params` | The parameters are not `{ enabled? }`, or `enabled` is not a boolean |

`enabled` is an optional boolean; `{}` or `null` reads the current value. Unknown keys are rejected.
The response reports `enabled`, `live` (an `Audio` resource exists), and `persisted` (settings exist).
Workflow: read `audio.mono`, set `enabled: true`, check both live and persisted state, then set it back
to `false` when the accessibility check is complete.

Captured failing call (lavapipe; `enabled` is not a boolean):

```console
$ regia agent call audio.mono '{"enabled":1}' --project samples/arena
error: method call failed: bad `audio.mono` parameters: invalid type: integer `1`, expected a boolean (-32602): {"code":"audio_mono_bad_params","hint":"pass { enabled: true } or { enabled: false }, or nothing to read"}
```
