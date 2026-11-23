# Audio patch format — `.audiopatch.ron` v2

Published with ADR-7226 (`specs/16` § 3a). Version 2 is the current version: loaders write it, and
version 1 files ([`audio-patch-v1.md`](audio-patch-v1.md), frozen) load through the `v1 → v2`
migrator. `regia upgrade-project` rewrites a v1 file's version number in place (comments and layout
kept) after the loader has accepted it.

Version 2 adds, all optional:

- `binding` on a parameter: the parameter follows a reflected component field;
- `one_shot` on a layer: a one-shot layer;
- `granular` on a layer: a granular layer.

A v1 file has none of them, so its meaning does not change: the migrator only stamps the version.
A file that says `version: 1` but uses one of them is refused ("set `version: 2`").

The runtime type is `regia_audio::AudioPatchDef`; parse with `AudioPatchDef::from_ron`, save with
`AudioPatchDef::to_ron`, resolve sounds with `AudioPatch::from_def`.

## Schema

Everything from v1 stays as it was (`name`, `bus`, `parameters` with `min`, `max`,
`smoothing_secs`; layers with `sound`, `keys`, `parameter`, `volume_db`, `volume_curves`,
`pitch_curves`, `filter_curves`). The example below is
`crates/regia_audio/tests/fixtures/rover.audiopatch.ron`:

```ron
(
    version: 2,
    name: "rover",
    bus: Some("Sfx"),
    parameters: [
        (
            name: "speed",
            min: 0.0,
            max: 30.0,
            smoothing_secs: 0.1,
            // Optional (v2). Every frame the binding system reads `Rover.speed` on the entity that
            // carries `AudioPatchSource { patch }` and sets the target to value * scale + offset.
            binding: Some((component: "Rover", field: "speed", scale: 1.0, offset: 0.0)),
        ),
        (name: "gear", min: 0.0, max: 5.0, smoothing_secs: 0.02),
    ],
    layers: [
        // A v1 loop layer.
        (
            name: "motor",
            sound: Some("audio/rover/motor.ogg"),
            volume_db: -9.0,
            pitch_curves: [(parameter: "speed", points: [(0.0, 0.8), (30.0, 1.6)])],
        ),
        // A granular layer (v2).
        (
            name: "gravel",
            volume_db: -12.0,
            volume_curves: [(parameter: "speed", points: [(0.0, -40.0), (5.0, 0.0)])],
            filter_curves: [(parameter: "speed", points: [(0.0, 900.0), (30.0, 9000.0)])],
            granular: Some((
                sound: "audio/rover/gravel.ogg", // the source grains read from (looping)
                grain_secs: 0.04,                // grain length; default 0.05
                density_hz: 50.0,                // grains per second; default 40
                position: 0.1,                   // where grains start, 0..1 of the source; default 0
                // Outputs added to `position` (sum clamped to 0..1).
                position_curves: [(parameter: "speed", points: [(0.0, 0.0), (30.0, 0.8)])],
                // Ratios multiplied into `density_hz`.
                density_curves: [(parameter: "speed", points: [(0.0, 0.5), (30.0, 2.0)])],
                position_jitter: 0.05,           // ± this fraction of the source per grain; default 0
                seed: 27,                        // jitter sequence seed; default 0
            )),
        ),
        // A one-shot layer (v2): a clunk for every whole gear, alternating two sounds.
        (
            name: "shift",
            volume_db: -6.0,
            one_shot: Some((
                sounds: ["audio/rover/clunk_a.ogg", "audio/rover/clunk_b.ogg"],
                trigger: Every(parameter: "gear", step: 1.0),
                cooldown_secs: 0.15,             // default 0
            )),
        ),
        // A one-shot layer the game fires by name (Audio::patch_trigger, audio.patch_trigger).
        (
            name: "horn",
            one_shot: Some((sounds: ["audio/rover/horn.ogg"], trigger: Manual)),
        ),
    ],
)
```

## Rules

- **One kind per layer.** A layer sets exactly one of `sound`, `keys`, `one_shot` and `granular`
  (`sound` together with `keys` is a key set, as in v1). The loader rejects none or several.
- **Bindings.** `component` is the short type name of a registered component with
  `#[reflect(Component)]`; `field` is a reflect path inside it (`speed`, `velocity.x`). The leaf may
  be `f32`, `f64`, an integer (`i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`) or `bool` (0 or 1).
  The value becomes `value * scale + offset` (`scale` defaults to 1, `offset` to 0), then the
  parameter clamps and slews as usual. The binding runs in `PostUpdate` before `AudioSync`. A
  missing component, an unknown name or path, or another leaf type leaves the parameter unchanged
  and warns once per entity and binding.
- **One-shot triggers** watch the smoothed parameter between two ticks:
  `Rising(parameter, threshold)` fires when it goes from below the threshold to at or above it;
  `Falling(parameter, threshold)` from above to at or below; `Every(parameter, step)` once for each
  multiple of `step` crossed in either direction (at most 4 per tick; `step` must be positive);
  `Manual` only when triggered by name. The watched parameter must be declared. Sounds play in
  list order, wrapping. A shot inside `cooldown_secs` of the last one is skipped. Each shot starts at
  the layer's volume (`volume_db` plus its volume curves) and pitch, through the layer's low-pass
  when it has `filter_curves`, and plays to its end (a despawn does not cut it).
- **Granular layers.** A grain is `grain_secs · sample_rate` frames (rounded) read from the
  source, looped, at the layer's pitch (linear interpolation), shaped by the periodic Hann window
  `w(a) = 0.5 − 0.5·cos(2π·a / G)`. Grains start every `sample_rate / density` frames, the first on
  the first frame; with `grain_secs · density_hz = 2` neighbouring windows sum to exactly 1, so a
  steady source plays at its own level. A grain reads the position, pitch and density when it
  starts; changes never jump inside a grain. The volume ramps over 128 frames. At most 64 grains
  sound at once; a grain due while 64 sound is skipped. `grain_secs` and `density_hz` must be
  positive, `position_jitter` non-negative, curve parameters declared. The density is capped at
  one grain per frame.
- Version handling: a version above 2 is a load error naming the version; version 1 migrates.

## Version history

| Version | Change | Migration |
|---|---|---|
| 1 | Loop and key-set layers, parameter curves (`audio-patch-v1.md`). | — |
| 2 | `binding`, `one_shot`, `granular` (ADR-7226). | Stamp the version; a v1 file using a v2 field is refused. |
