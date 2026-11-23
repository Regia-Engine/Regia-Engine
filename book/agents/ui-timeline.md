# UI timeline animations (agents)

ADR-273 T04, ADR-1842. Code: `regia_ui::anim_timeline`. Format: `.uianim.ron` (v2; frame events since ADR-8900).

A timeline animation is a named set of keyframe tracks. Each track animates one tween property (`opacity`,
`background`, `border_color`, `text_color`, `inset_*`, `width`, `height`, `translate_x`, `translate_y`, `scale`) through
keys `{ time, value, ease? }` with strictly ascending times in seconds. A key's `ease` shapes the segment that ends at it.
Colors are linear `[r, g, b, a]`. Before its first key a track holds its first value; after the last key the animation
holds the end values.

`mode` is `once` (default), `loop` or `ping_pong`; `loops` (1 to 64) is the number of passes for the last two. An
endless loop is not offered: playback compiles to a finite tween plan, which keeps it reproducible with `play.step`.
`speed` (0.05 to 20) scales time. `reverse` plays the keys backwards and keeps each segment's ease as authored.

The plan is the one `ui.tween` plays, so reduced motion, pause and the `ui.tweening` wait atom behave the same.

**Frame events** (v2): an animation's `events: [{ time, name, payload? }]` (time in seconds of one pass, 0 to the
animation's length) fire a `UiAnimEventFired { entity, name, payload, at }` message exactly once each time playback
crosses them: once per `loop` / `ping_pong` pass, in reverse order on a backward pass, and also when one frame jumps
over several events or reduced motion jumps to the end. They come in timeline order, before the tween's
`UiTweenFinished`; a replaced tween fires no more. The last 64 are kept for the `events` action.

## File

```ron
(
    format_version: 2,
    animations: {
        "pop": (
            tracks: [
                (property: "opacity", keys: [(time: 0.0, value: 0.0), (time: 0.2, value: 1.0, ease: "quad_out")]),
                (property: "scale", keys: [(time: 0.0, value: 0.9), (time: 0.2, value: 1.0, ease: "back_out")]),
            ],
            events: [(time: 0.1, name: "sfx", payload: "pop")],
        ),
    },
)
```

A file with a newer `format_version` is refused; a v1 file (or one without the field) reads as v2 with no events.
`UiAnimLibrary::from_ron` validates every animation.

## Methods

- `ui.play_anim { action?, name?, animation?, entity?, reverse?, speed?, time?, clear? }`:
  - `list` (default): `{ count, animations: [{ name, duration, tracks, mode, loops, speed, events }] }` (`events` is
    the count).
  - `define { name, animation }`: stores an animation; returns `{ defined, duration }`.
  - `play { entity, name | animation, reverse?, speed? }`: starts it on the node with its frame events; returns
    `{ entity, playing, duration, events }`.
  - `sample { name | animation, time, reverse?, speed? }`: returns `{ time, duration, values, events }` without touching
    any node; `events` lists `{ name, payload, at }` for every event a playback from 0 has fired by `time`, in
    order (the same event cursor the tween system moves; at or past `duration`, every event; ADR-10132).
  - `events { clear? }`: the fired-event log, `{ total, events: [{ entity, name, payload, at }], cleared }`, oldest
    first (the last 64; `total` counts every one since the log was cleared); `clear: true` empties it after reading.
    `clear` must be a boolean for every action (`invalid_params` naming `clear`).

  Mutating; permission `edit_scene`.

```json
{"method": "ui.play_anim", "params": {"action": "sample", "name": "pop", "time": 0.1}}
{"method": "ui.play_anim", "params": {"action": "events", "clear": true}}
```

## Errors

| Code | Meaning |
|---|---|
| `bad_action` | `action` is not list, define, play, sample or events |
| `bad_animation` | the animation, its name, `speed`, a track or a frame event is invalid |
| `unknown_animation` | no animation with that `name` is defined |
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `bad_flag` | `reverse` is not a boolean |
| `bad_time` | `time` is not a non-negative number |

## Authoring and packs

The [UI animation editor](ui-anim-designer.md) edits these files; `.uianim.ron` is cooked into the pak, and
`regia_ui::anim_pack::load_uianim_file` loads a library by path (source first, then the pak) into the `UiAnimLibrary`
that `ui.play_anim` reads.
