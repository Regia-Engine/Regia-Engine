# Audio patch format — `.audiopatch.ron` v1

> **Superseded by [v2](audio-patch-v2.md)** (ADR-7226): v1 files still load and migrate (the
> migrator only stamps the version); new files are written as v2.

Published in M8 (`specs/16` § 3a). **Format version 1 is frozen**: after the first shipped content
build, loaders must keep accepting files in this exact shape; new fields get defaults and a version
bump (v2 arrives only with a migrator, `specs/12` § 4a).

A patch is a small graph of looping layers whose volume, pitch and filter cutoff are driven by
**parameters** through polylines. Typical use: a vehicle engine whose loop set slides from idle to
redline with `speed`, a hover whine that follows `throttle`. The runtime type is
`regia_audio::AudioPatchDef`; parse with `AudioPatchDef::from_ron`.

## Schema

```ron
(
    // Required. Must be 1. Loaders reject other versions.
    version: 1,
    // Required. Patch name, used in logs and the editor.
    name: "hover_engine",
    // Optional. Bus the layers play on. Default: "Sfx".
    bus: Some("Sfx"),

    // Required. Parameters the game feeds (`patch.set("speed", v)` or a reflection binding).
    parameters: [
        (
            name: "speed",          // unique, referenced by layer curves
            min: 0.0,               // parameter range; curves are clamped to it
            max: 100.0,
            smoothing_secs: 0.1,    // slew toward a new target; default 0.1. > 0 avoids zipper noise
        ),
        (
            name: "throttle",
            min: 0.0,
            max: 1.0,
            smoothing_secs: 0.1,
        ),
    ],

    // Required. One or more layers. All layers of a patch start together and loop.
    layers: [
        (
            name: "loop_set",       // unique within the patch

            // Exactly one of `sound` / `keys`:
            // - `sound`: one looping source for the whole layer
            sound: None,
            // - `keys`: a crossfaded loop set keyed by `parameter` (engine sample sets)
            keys: [
                (value: 0.0,   sound: "audio/engine/idle.ogg",  gain_db: 0.0, pitch: 1.0),
                (value: 50.0,  sound: "audio/engine/mid.ogg",   gain_db: 0.0, pitch: 1.0),
                (value: 100.0, sound: "audio/engine/redline.ogg", gain_db: 0.0, pitch: 1.0),
            ],
            // Which parameter drives the key crossfade (required with `keys`, ignored otherwise).
            parameter: Some("speed"),

            volume_db: -6.0,        // base layer volume
            // Volume curves: dB offsets added to `volume_db`, per parameter.
            volume_curves: [
                (parameter: "throttle", points: [(0.0, 0.0), (1.0, 6.0)]),
            ],
            // Pitch curves: ratios multiplied into the layer pitch (1 = no shift).
            pitch_curves: [
                (parameter: "speed", points: [(0.0, 0.6), (100.0, 1.4)]),
            ],
            // Filter cutoff curves in Hz; a low-pass is created per layer that has one.
            filter_curves: [
                (parameter: "speed", points: [(0.0, 1200.0), (100.0, 20000.0)]),
            ],
        ),
    ],
)
```

## Rules

- `keys` are given in any order; the runtime sorts them by `value` and crossfades equal-power
  between neighbouring keys. Outside the key range the nearest key's weight is 1. A layer with
  `keys` must name a declared parameter in `parameter`; the loader rejects it otherwise.
- Curve points are `(parameter value, output)` polylines, linearly interpolated and clamped at both
  ends (see `specs/16` § 3a and the OpenAL-style distance curves in `spatial.rs` for the same
  convention elsewhere).
- Sounds referenced by `sound`/`keys` are looping sources on one bus; a patch never pans by itself.
  Spatialise by attaching the patch to an emitter position in game code (spawn per emitter).
- Parameter values are clamped to `[min, max]` and slewed for `smoothing_secs` on the game thread;
  the mixer receives short tweens, so the audio thread never steps a value discontinuously.
- Version handling: `version != 1` is a load error. The u32 version field follows the engine-wide
  versioning rule (`04-conventions` § 7).

## Example

`assets/audio/hover_engine.audiopatch.ron` — the file above is a complete, loadable example: a
three-key engine loop set crossfaded by `speed` (0–100), pitched 0.6→1.4 over the same range, and
brightened by `throttle`.
