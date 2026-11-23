# config/audio.ron: the project bus tree (audio-buses v1)

Library: `regia_audio::bus` (`BusLayout::{parse, from_ron, to_ron, validate, default_tree, same_tracks}`,
`BusEffect`, `AudioSnapshot`, `SnapshotSend`, `migrate_audio_buses_ron`, `bus_layout_schema_fingerprint`,
`audio_buses_migration_sources`, `AUDIO_BUSES_FORMAT_VERSION`, `BusSend`, `SendTap`). Runtime: `Audio::{set_bus_layout,
apply_snapshot}`. Editor: the Audio Mixer dock writes it (`editor.audio_mixer_save`). Guide page:
`book/agents/audio-mixer.md`. Decisions: ADR-8105 (g) (the format row), ADR-8354 (the build), ADR-8590 (the send tap fold). Spec:
`docs/specs/16-audio.md` § 1.

`config/audio.ron` under the project root is the bus tree the runtime builds (the `regia` default plugins
read it at start-up) and the editor mixer edits. Without the file the spec's default tree is used:
`Master → { Music, Sfx → { World, Ui }, Voice, Ambience }` plus a `Reverb` send bus.

```ron
(
    version: 1,                   // absent = 0 (every file written before v0.6.0)
    buses: [                      // parents before their children; names unique
        (
            name: "Sfx",
            parent: Some("Master"),   // None for the root (exactly one, usually "Master")
            volume_db: 0.0,           // default 0
            mute: false,              // default false: start muted
            effects: [                // default []: the chain, applied in order
                Bypassed(Eq(low_hz: 200.0, low_db: 2.0, mid_hz: 1000.0, mid_db: -3.0,
                            high_hz: 5000.0, high_db: 1.0, quality: 0.7)),
                Compressor(threshold_db: -12.0, ratio: 2.0, attack_secs: 0.01, release_secs: 0.2, makeup_db: 1.0),
            ],
            sends: [                  // default []: sends into send buses
                (bus: "Echo", volume_db: -9.0),                   // tap absent = PostFader
                (bus: "Reverb", volume_db: -6.0, tap: PreFader),  // v1 fold: before the fader and mute
            ],
            bypass_medium: false,     // default false: voices here skip the propagation medium
        ),
    ],
    send_buses: [                 // default []: no hierarchy, output to the main mix
        (name: "Echo", effects: [Delay(delay_secs: 0.3, feedback_db: -9.0, mix: 1.0)]),
        (name: "Reverb", effects: [Reverb(feedback: 0.85, damping: 0.4, stereo_width: 0.7, mix: 1.0)]),
    ],
    snapshots: [                  // default []: named mixer states (v1)
        (
            name: "Paused",
            bus_volumes: {"Music": -12.0},           // absolute dB
            filter_cutoffs: {"World": 800.0},        // Hz, every low/high-pass on the bus
            bus_mutes: {"Music": true},              // true mutes, false unmutes a host-muted bus
            send_volumes: [(bus: "Sfx", send: "Echo", volume_db: 0.0)],
            blend_secs: 0.5,                         // default 0: the blend `Audio::apply_snapshot` uses
        ),
    ],
)
```

## Effects

`Eq(low_hz, low_db, mid_hz, mid_db, high_hz, high_db, quality)`, `LowPass(cutoff_hz, resonance)`,
`HighPass(cutoff_hz, resonance)`, `Reverb(feedback, damping, stereo_width, mix)`,
`Delay(delay_secs, feedback_db, mix)`, `Compressor(threshold_db, ratio, attack_secs, release_secs,
makeup_db)`, `Distortion(drive_db)`, `Convolution(ir, mix, predelay_secs)` (an IR registered with
`Audio::register_impulse_response`), and `Bypassed(<effect>)` (v1): the wrapped effect keeps its
parameters but is not built, so the bus passes its signal through that slot unchanged.

## Rules

- `version` above 1 is refused ("newer than this build reads"); 0 migrates by gaining `version: 1`.
- A parent must be defined earlier in `buses`; a bus is never its own parent; names are unique.
- Every send names a send bus of `send_buses`.
- Effect times (`delay_secs`, `predelay_secs`, `attack_secs`, `release_secs`) are finite and at least 0,
  also inside `Bypassed`; `Bypassed(Bypassed(_))` is refused.
- Snapshot names are unique and not blank; every bus a snapshot names exists; every
  `send_volumes` entry is a route the layout has; volumes, send levels and `blend_secs` are finite and
  `blend_secs` is at least 0.
- A send's `tap` is `PostFader` (the default when the field is absent: the send hears the bus after its
  effects, fader and mute) or `PreFader` (after its effects, before its fader and mute, ADR-8590). The writer
  leaves a `PostFader` tap out, so a layout without pre-fader sends writes the same text as before the field
  existed. Snapshots set send levels only; a tap is never part of a snapshot.

## Versions

| Version | Change |
|---|---|
| 0 | The versionless shape v0.5.0 and earlier shipped: `buses`, `send_buses`. |
| 1 | `version`; `snapshots` (`AudioSnapshot` with `bus_volumes`, `filter_cutoffs`, `bus_mutes`, `send_volumes`, `blend_secs`; the maps are ordered); the `Bypassed` effect (ADR-8105 (g), ADR-8354). Folded into v1 before v0.6.0 shipped (ADR-8590, additive, the version stays 1): the optional per-send `tap: PostFader \| PreFader`, default `PostFader`; every v1 file without it loads unchanged. |

Migration: `regia upgrade-project` finds `audio.ron` files that parse as a bus layout and inserts
`version: 1` as the first field, keeping every other byte (`migrate_audio_buses_ron`). A v0.5 build
reading a v1 file ignores `version` and `snapshots` but refuses a `Bypassed` effect and keeps its default
tree with a warning; it ignores a `tap` field (the send plays post-fader there).
