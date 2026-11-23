# `.uianim.ron` (UI animation library, format_version 2)

Owner: `regia_ui::anim_timeline` (ADR-273 T04, ADR-1842). Named keyframed animations of UI node properties, compiled
to the existing tween plans. Read by `UiAnimLibrary::from_ron`, written by `UiAnimLibrary::to_ron_string`; the agent
surface is `ui.play_anim` (`book/agents/ui-timeline.md`).

```ron
(
    format_version: 2,
    animations: {
        "pop": (
            tracks: [
                (property: "opacity", keys: [(time: 0.0, value: 0.0), (time: 0.2, value: 1.0, ease: "quad_out")]),
                (property: "scale", keys: [(time: 0.0, value: 0.9), (time: 0.2, value: 1.0, ease: "back_out")]),
            ],
            events: [(time: 0.1, name: "sfx", payload: "pop"), (time: 0.2, name: "done")],
        ),
        "blink": (
            tracks: [(property: "opacity", keys: [(time: 0.0, value: 1.0), (time: 0.5, value: 0.2)])],
            mode: ping_pong,
            loops: 3,
            speed: 2.0,
        ),
    },
)
```

| Field | Meaning |
|---|---|
| `format_version` | 2. Absent reads as 1; a v1 file reads as v2 with no events; a newer version is refused |
| `animations` | Name → animation |
| animation `tracks` | At most 32; each animates one tweenable `property` (`opacity`, `scale`, a colour, ...) |
| track `keys` | At most 1024, times ascending; `value` is a number or a four-number colour `(r, g, b, a)` that fits the property |
| key `ease` | Optional easing name for the segment that ends at this key (`linear` when absent) |
| animation `mode` | `once` (default), `loop` or `ping_pong` |
| animation `loops` | Repeats for `loop` / `ping_pong`, 1 to 64 (default 1) |
| animation `speed` | Playback rate, 0.05 to 20 (default 1) |
| animation `events` | v2, optional (not written when empty): at most 256 frame events `(time, name, payload?)`; `time` from 0 to the animation's length (its latest key), `name` 1 to 256 bytes, `payload` up to 256 bytes (default empty), no control characters; any order |

Every animation is validated on load (`AnimError`): an unknown property, unsorted or non-finite keys, a value that does
not fit the property, or a limit exceeded refuses the whole file.

**Frame events (v2, ADR-8900).** `UiAnimDef::compile_events` places each event on the playback timeline once per
pass (a `loop` / `ping_pong` animation repeats them; a backward pass meets them at `length - time`, in reverse
order; speed divides the times; each pass starts where the compiled plan's previous pass ends, so the times never
decrease and an event at the end of the last pass is at the plan's duration, ADR-10132), and `UiAnimDef::to_tween` hands them to the `UiTween`. The tween system sends one
`UiAnimEventFired { entity, name, payload, at }` message per event, exactly once per crossing, in timeline order and
before the tween's `UiTweenFinished`: a frame step that jumps over several events (a long frame, reduced motion
jumping to the end) sends every one of them, and a stopped or replaced tween sends no more. The last 64 are kept in
the `UiAnimEventLog` resource (`ui.play_anim { action: "events" }`).

**Versioning.** Registered in `cargo xtask check-formats` (`uianim`) and `regia upgrade-project` (a file without
`format_version` reads as v1) since ADR-2316 (spec 12 § 4a). Version 2 (v0.7, format batch 2, ADR-8900) adds the
optional `events` list; `regia upgrade-project` rewrites a v1 file's `format_version` number (every other byte kept)
or re-saves a file without the field at v2. A v0.6 build refuses a v2 file with "format_version 2 is newer than the
supported 1". A shape change bumps `format_version` and ships a migrator.
