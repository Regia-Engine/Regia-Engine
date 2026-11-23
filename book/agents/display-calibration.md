# Display calibration (agents)

The player's display calibration (black level, gamma, HDR nits) runs after the tonemap, VFX and
UI, on the finished frame. See the [user guide](../user-guide/display-calibration.md) for what
each value does. Guide topic: `agent.guide {"topic": "calibration"}`.

## Methods

- `render.get_calibration` returns `{ calibration, sdr_neutral, platform, platform_default,
  display, hdr_requested, persisted }`. `calibration` holds `{ black_level, gamma,
  paper_white_nits, peak_nits, min_nits }`. `platform` is `desktop`, `mobile` or `deck`.
  `display` is what the OS reports (`peak_nits` / `min_nits`, `null` when unknown).
  `persisted` is `true` when the player has saved a calibration and `null` without the settings
  plugin.
- `render.set_calibration` changes only the fields given; the others keep their value. Values
  are clamped to the player ranges. The change applies on the next frame. It is written to
  `graphics.calibration` only when `GameSettings` is already in the world; otherwise `persisted`
  is false and nothing is saved. `{"hdr": true}` requests HDR10 output only when `OutputSettings`
  is in the world. Without it, `hdr_requested` stays false, and the saved section still records
  the requested flag when `GameSettings` exists. `{"reset": true}` returns to the platform default
  and clears the saved section. Returns `{ before, after, sdr_neutral, persisted }`. Mutating;
  permission `edit_scene`.

Parameters are optional JSON numbers for `black_level`, `gamma`, `paper_white_nits`, `peak_nits` and
`min_nits`, and optional booleans for `hdr` and `reset`. Omitted fields keep their current value;
`reset` defaults to `false`. Values clamp to black level −1..=1, gamma 1.8..=2.6, paper white
80..=500 nits, peak 200..=10,000 nits, and minimum 0..=1 nit (also capped at 1% of peak).

```json
{"method": "render.set_calibration", "params": {"black_level": 0.35, "gamma": 2.3}}
```

## Screenshots and goldens

`seeing.screenshot` takes `calibration`:

- `"applied"` (default): what the player sees, with the calibration applied.
- `"neutral"`: the frame before the display stage. It is byte-identical to a capture at neutral
  calibration. **Use it for goldens and look comparisons**, so a player's or a test machine's
  calibration never leaks into a judgement of the content.

An HDR frame is captured as its SDR rendition (the black-level curve only for `"applied"`).
Offscreen captures (`target: "offscreen"`) have no display stage; `"applied"` applies the SDR
curve on the CPU, using the same function the shader mirrors.

## Typical loop

1. `render.get_calibration` to check whether the frame is neutral.
2. Compare looks with `seeing.screenshot {"calibration": "neutral"}`.
3. To preview how a dim display would be calibrated, `render.set_calibration`
   with the values `DisplayModel::solve` gives. The evidence sheet lists them in
   `docs/evidence/calibration/<date>/metrics.md`. Screenshot with `"applied"`.
4. `render.set_calibration {"reset": true}` when done.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
render.get_calibration {}       # { calibration, sdr_neutral, platform, ... }
```

## Errors

| Code | When |
|---|---|
| `F0002` | A `render.set_calibration` parameter has the wrong JSON type. |

## Failing call

This captured headless lavapipe call needs no physical display or colorimeter:

```text
$ regia agent call render.set_calibration '{"gamma":"dark"}' --project samples/boot_probe
error: method call failed: `gamma` must be a number (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```
