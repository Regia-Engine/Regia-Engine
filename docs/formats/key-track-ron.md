# `.track.ron` (key track, format_version 1)

Owner: `regia_cli::key_track` (ADR-8403). A scripted list of key, pointer and wheel steps for
`regia play-gate replay`. Read by `PreparedTrack::parse`. The clock is virtual: the same file and
the same log replay the same edges. Nothing sleeps.

```ron
(
    format_version: 1,
    steps: [
        Section(name: "boot"),
        Tap(target: "input.confirm", millis: 200),
        Expect(pattern: "player ready", within_millis: 1000, optional: false),
        Press(target: "key:Escape"),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. Any other number is refused (`track_version`). It is read before the rest of the file, so a newer file whose steps changed shape still gets `track_version`, never `track_parse` (ADR-8553) |
| `steps` | In order. Empty is a valid no-op |
| `Section` | `name` is stored on later events until the next section |
| `Wait` | Adds `millis` to the clock. No edges |
| `Expect` | Matches one log line at or before `clock + within_millis` (default 1000). Does not move the clock. `optional: true` records a note on a miss; `false` stops the replay (`track_expect_missed`). The pattern is a `regex` crate expression. The next expect cannot reuse the matched line |
| `Tap` | Down at the clock, up `millis` later (default 200), then the clock advances by `millis` |
| `Press` | Down, if that target is not already held. A second press records nothing |
| `Release` | Up, if that target is held. A target that is not held records nothing |
| `ReleaseAll` | Up for every target still held, in press order |
| `Hold` | Down for each target that is not already held, advance `millis`, then up for those targets |
| `Note` | A note at the clock. No edges |
| `TapN` | `count` taps (at least 1). Pulse `i` starts `i * gap_millis` later (default gap 300) and holds for 200. The clock then advances by `(count - 1) * gap_millis + 200` |
| `Mouse` | Pointer counts `dx`, `dy` at the clock. The clock advances by `millis` (default 300) |
| `Wheel` | `notches` at the clock. The clock does not move |

Targets are opaque strings (`input.confirm`, `key:Escape`). A missing duration field takes the default in the table. An unknown field, on the file or on a step, is refused (`track_parse`, naming the field; ADR-8553): `Tap(target: "input.confirm", milis: 5)` is an error, never a 200 ms hold. The defaults are part of the file format, and a file can set another duration.

**Versioning.** v1 is the first version, so `regia upgrade-project` leaves a v1 file as it is. A newer file is refused. Registered in `cargo xtask check-formats` (`key-track`, ADR-8403). The fuzz test is `crates/regia_cli/tests/fuzz.rs` (`key_track_ron_never_panics`; it also feeds newer files with foreign bodies).

This format has to merge before the v0.6 format freeze.
