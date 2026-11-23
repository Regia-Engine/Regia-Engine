# Audio cue format — `.cue.ron` v1, v2 and v3

Published in M8 (`specs/16` § 2). **Version 1 is frozen** for shipped content: loaders keep
accepting this shape; new fields get serde defaults and a version bump with a migrator
(`specs/12` § 4a). **v2 (ADR-272)** adds three optional fields (`version`, `filter_cutoff_hz`,
`filter_offset_octaves`); a file without them is v1 and plays exactly as before. **v3
(ADR-1697)** adds one optional field (`caption`, § "v3 captions" below); v1 and v2 files load
unchanged.

A cue is a randomised sound container: a list of weighted variations plus the rules used to pick,
vary and limit playback. The runtime types are `regia_audio::SoundCueDef` (file form) and
`regia_audio::SoundCue` (loaded); parse with `SoundCueDef::from_ron`.

## Schema

```ron
(
    // Required. Cue name; gameplay plays by it (`audio.play_cue("impact_flesh")`).
    name: "impact_flesh",
    // Optional. Bus every pick plays on. Default: Master.
    bus: Some("World"),

    // Required. At least one variation.
    sounds: [
        (sound: "audio/impacts/flesh_a.ogg", weight: 1.0),
        (sound: "audio/impacts/flesh_b.ogg", weight: 1.0),
        (sound: "audio/impacts/flesh_c.ogg", weight: 0.5),  // half as likely as the others
    ],

    // Pick mode. `Random` (default) = weighted; `Shuffle` = bag (all variants before a repeat
    // cycle); `Sequential` = top to bottom, wrapping.
    mode: Random,
    // Never repeat any of the last N picks (applies to all modes). Default 0.
    no_repeat_last: 2,

    // Random pitch ratio range applied per play. Default 1.0..1.0.
    pitch_min: 0.95,
    pitch_max: 1.05,
    // Random volume range in dB applied per play. Default 0.0..0.0.
    volume_db_min: -2.0,
    volume_db_max: 0.0,

    // Maximum simultaneous instances of this cue. Default: unlimited.
    max_concurrent: Some(4),
    // What happens past the limit: `Oldest` (default) steal the oldest, `Quietest` steal the
    // quietest, `None` refuse the new play.
    steal: Oldest,

    // Minimum time between plays. Default: zero. `std::time::Duration` serializes as
    // `(secs, nanos)` — 50 ms is `(secs: 0, nanos: 50000000)`.
    cooldown: (secs: 0, nanos: 50000000),

    // Base volume of every pick in dB, before the random range. Default 0.
    volume_db: 0.0,

    // v2, all optional. Format version; absent = 1. Newer than the engine is a load error.
    version: 2,
    // Base low-pass cutoff in Hz for every pick. Default: none (no filter).
    filter_cutoff_hz: Some(6000.0),
    // Octaves added to the cutoff: cutoff = base * 2^offset, clamped to 20..20000 Hz. Default 0.
    // With no `filter_cutoff_hz` an offset starts from fully open (20 kHz), so a negative value
    // darkens the cue.
    filter_offset_octaves: -0.5,

    // v3, optional. Sound caption this cue sends once per play (§ "v3 captions"). Absent = none.
    caption: Some((
        // Fluent key of the caption text; the caption board resolves it when it accepts the
        // message (regia_audio cannot resolve text).
        key: "audio-caption-door-slams",
        // Caption kind: Sound (default, a non-speech sound caption), Speech or Narration.
        kind: Sound,
        // Priority 0 to 255 (default 0) for the caption board's arbitration.
        priority: 0,
        // Planned length in milliseconds; absent = the picked sound's length.
        planned_ms: Some(700),
    )),
)
```

## Rules

- Weights are relative; they need not sum to 1. A weight of 0 excludes a variation from `Random`
  (it still plays in `Sequential`).
- `no_repeat_last` never forbids the *only* remaining variant (a 2-variation cue with
  `no_repeat_last: 2` still alternates).
- Pitch/volume ranges are drawn uniformly per play, after `volume_db`.
- `cooldown` starts when a play is accepted; refused plays (limit with `None`, cooldown) do not
  restart it. Durations are `std::time::Duration` in RON struct form: `(secs: u64, nanos: u32)`.
- Positional plays (`play_at`, emitters) apply spatialisation on top of the cue's own volumes.

- A cue with neither `filter_cutoff_hz` nor a nonzero `filter_offset_octaves` plays on the bus
  directly, as in v1. Otherwise each pick plays through its own low-pass track under the bus.
- Per-event variation: `Audio::play_cue_seeded(name, seed)` / `play_cue_event(name, event_id, tick)`
  draw the pick and the pitch/volume ranges from a generator seeded by the event
  (`cue_event_seed(event_id, tick)`), so one event replays the same variation. `play_cue` keeps
  using the shared audio generator.
- `regia upgrade-project --write` stamps `version: 3` into v1/v2 cue files (the migrator only
  stamps the version; every newer field is optional, so nothing else changes).

## v3 captions (ADR-1697)

An optional `caption: Some(CueCaptionDef)` on a v3 cue sends a caption once per play. There is
no caption message to send: `regia_framework::conversation::cue_caption_system` drains the pending
plays with `Audio::take_cue_captions` and writes `regia_app::caption::CaptionStart`/`CaptionStop`
messages, which the caption board (`regia_ui::captions::CaptionBoard`, spec 56) takes; the board
resolves the Fluent `key` to text and `regia_audio` itself never resolves text. The caption's
fields (`key`, `kind: Sound | Speech | Narration` (default `Sound`), `priority: 0..255`
(default 0), `planned_ms: Option<u32>` (absent = the picked sound's length, or 2 s when the sound
reports no usable length)) mirror `regia_audio::CueCaptionDef`. Each drained play carries the cue
name, the caption, the voice `SoundInstanceId` it started (the caption ends when that voice ends)
and the picked sound's length in seconds. A cue without `caption` behaves exactly as v1/v2. The
caption rules are also in [`conversation-ron.md`](conversation-ron.md) ("cue captions").

## Example

`assets/audio/impacts/impact_flesh.cue.ron` — the file above is a complete, loadable example: three
weighted flesh impacts, never repeating the last two, ±5 % pitch jitter, at most 4 concurrent
instances stealing the oldest, 50 ms cooldown.
