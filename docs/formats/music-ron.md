# `.music.ron` v2

A music track: tempo, meter, stems, stingers and transition rules (`regia_audio::MusicDef`,
`docs/specs/16-audio.md` § 3a, ADR-2517). Frozen format (`docs/specs/12-assets.md` § 4a),
registered with `cargo xtask check-formats` as `music`.

```ron
(
    version: 2,
    name: "combat",
    bpm: 132.0,
    beats_per_bar: 4,            // default 4
    segment_bars: Some(8),       // default None: the length of the longest stem
    stems: [
        (
            name: "drums",
            sound: "audio/music/combat_drums.ogg",
            volume_db: 0.0,                         // default 0
            intensity: [(0.0, -60.0), (1.0, 0.0)],  // default none: the base volume
        ),
    ],
    stingers: [                  // default none
        (
            name: "victory",
            sound: "audio/music/victory.ogg",
            volume_db: -3.0,     // default 0
            sync: NextBar,       // default NextBar
        ),
    ],
    transitions: [               // default none
        (
            to: "calm",          // a track name, or "*" for every track without its own rule
            sync: EndOfSegment,  // Immediate, NextBeat, NextBar (default), EndOfSegment
            crossfade_secs: 2.0, // default 0: equal power, from the switch
            segment: Some((      // default None: a sound between the two tracks
                sound: "audio/music/combat_to_calm.ogg",
                volume_db: 0.0,
            )),
        ),
    ],
)
```

Rules. `bpm` is finite, greater than 0 and at most 1000. A track has at least one stem.
`segment_bars` is at least 1. Stinger names are unique within a track. `crossfade_secs` is finite
and not negative. Each breaks `MusicAsset::from_def` with a message naming the track (a bad tempo
is `Invalid`, not an unregistered track).

With a `segment`, the crossfade is an equal-power fade from this track into the segment, and the
next track starts at full level when the segment's sound ends. If the segment is shorter than
`crossfade_secs`, the outgoing track's fade ends when the segment ends.

## Versions and migration

- **v1** (from M8 to v0.2.14) has no `version` field and only `name`, `bpm`, `beats_per_bar` and
  `stems`.
- **v2** adds `version`, `segment_bars`, `stingers` and `transitions`. All are optional, so v1 reads
  unchanged. `MusicDef::migrate_ron` and `regia upgrade-project --write` stamp the version and leave
  everything else (a `.bak` is kept).

A newer version than this engine reads is refused: "upgrade the engine".
