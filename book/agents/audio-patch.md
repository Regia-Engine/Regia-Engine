# Audio patches: one-shot, granular and bound layers (agents)

Read the parametric audio patches a game has spawned and fire their one-shot layers (ADR-7226,
`docs/specs/16-audio.md` § 3a, `docs/formats/audio-patch-v2.md`). Both methods need
`regia_audio::AudioPlugin` (`regia::DefaultPlugins` installs it); without it they return
`UNSUPPORTED` (`F0013`).

A patch (`.audiopatch.ron`) is a set of **parameters** (`speed`, `throttle`, `gear`) and
**layers** driven by them through curves. Format v2 has four layer kinds:

- `loop`: one looping sound;
- `keys`: a loop set crossfaded (equal power) by one parameter;
- `one_shot`: a sound, round-robin from a list, played when a parameter rises or falls through a
  threshold, crosses a multiple of a step, or when triggered by name (`Manual`);
- `granular`: overlapping Hann-windowed grains read from a source sound at a parameter-driven
  position and density.

A parameter may be **bound** to a reflected field (`Rover.speed`) of a component on the entity
carrying `regia_audio::AudioPatchSource`: each frame the binding system sets the parameter from the
field. A game spawns patches with `Audio::spawn_patch`; agents cannot spawn them.

## Read the patches

`audio.patch_state` (read-only).

```json
{ "method": "audio.patch_state" }
→ { "patches": [{
      "id": 1, "name": "rover", "bus": "Sfx",
      "parameters": [
        { "name": "speed", "value": 12.5, "target": 14.0, "binding": "Rover.speed" },
        { "name": "gear", "value": 2.0, "target": 2.0, "binding": null }
      ],
      "layers": [
        { "name": "motor", "kind": "loop", "volume_db": -9.0, "pitch": 1.13,
          "fired": 0, "skipped": 0, "grains": 0, "grains_dropped": 0 },
        { "name": "gravel", "kind": "granular", "volume_db": -12.0, "pitch": 1.0,
          "fired": 0, "skipped": 0, "grains": 812, "grains_dropped": 0 },
        { "name": "shift", "kind": "one_shot", "volume_db": -6.0, "pitch": 1.0,
          "fired": 2, "skipped": 0, "grains": 0, "grains_dropped": 0 }
      ] }] }
```

- `value` is the smoothed parameter; `target` is where it slews to (set by the game, a binding or
  clamped to the declared range).
- `binding` is `Component.field` for a bound parameter, `null` otherwise. A bound parameter whose
  component is missing or whose field cannot be read keeps its last target; the game log warns
  once per entity and binding.
- `fired` and `skipped` count a one-shot layer's shots; a shot inside the layer's `cooldown_secs`,
  or one the mixer refused, is skipped.
- `grains` counts a granular layer's grains as of the last rendered block; `grains_dropped` counts
  grains skipped because 64 were already sounding (lower the density or the grain length).
- `volume_db` and `pitch` are the layer's outputs now (base plus curves).

## Fire a one-shot layer

`audio.patch_trigger` (`play` permission) fires a one-shot layer now, whatever its trigger. Use it
to audition a layer, or for `Manual` layers a game fires by name (a horn, a door clunk).

```json
{ "method": "audio.patch_trigger", "params": { "id": 1, "layer": "horn" } }
→ { "id": 1, "layer": "horn", "fired": true, "total_fired": 1, "skipped": 0 }
```

- The layer's cooldown still applies: inside it, `fired` is `false` and `skipped` goes up.
- The shot plays at the layer's current volume and pitch, through its filter when it has one.

## Errors

| Code | When | What to do |
|---|---|---|
| `F0013` | No `AudioPlugin` in the app. | Add `regia_audio::AudioPlugin`. |
| `F0002` | `id` missing, negative or not whole; `layer` missing or empty. | Fix the parameters. |
| `patch_unknown` | No spawned patch has that `id`, or it has no layer of that name. | `audio.patch_state` lists the patches and layers. |
| `patch_not_one_shot` | The layer is a `loop`, `keys` or `granular` layer. | Pick a layer whose kind is `one_shot`. |

## Verify a patch change

1. `audio.patch_state` before and after the game moves a parameter: the bound parameter's `target`
   follows the field, and `fired` on an `Every` layer goes up once per step crossed.
2. `audio.patch_trigger` on each one-shot layer, then `audio.mix` to see the bus meter move.
3. For a granular layer, `grains` keeps rising while it plays and `grains_dropped` stays 0.
