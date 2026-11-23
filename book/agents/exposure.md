# Exposure and tonemap (agents)

The post stack's exposure and tonemap settings (`PostSettings`) are readable and settable on the
running app. A map's own look (`level.set_look`, [Map look](look.md)) is applied on top of these
values every frame: its `exposure_bias_ev` adds to `exposure_compensation`, and its `min_ev100` /
`max_ev100`, `meter`, `contrast` and `saturation` replace the ones set here while the map sets them.
Guide topic: `agent.guide {"topic": "look"}`.
The exposure the eye actually applied (the meter, exposure zones, the exposure log) is
`render.get_exposure`: see [Exposure zones](exposure-zones.md).

## Methods

- `render.get_exposure_settings` returns every setting in the table below, plus `tonemaps` (the operator
  names `tonemap` accepts) and `camera_ev100` (the EV100 the aperture, shutter and ISO give).
- `render.set_exposure_settings` takes any subset of the settings, validates all of them, and then applies
  them on the next frame. It returns `{ before, after }`. At least one field is required. The
  change is not persisted. Mutating; permission `edit_scene`.

| Setting | Meaning |
|---|---|
| `auto_exposure` | Histogram auto exposure on or off. Off holds `exposure_ev`. |
| `exposure_ev` | The fixed EV100 used when auto exposure is off (-20 to 30). |
| `exposure_compensation` | EV added on top of auto or fixed exposure; positive brightens (-16 to 16). |
| `min_ev100`, `max_ev100` | The EV100 range auto exposure may pick (-20 to 30). `min_ev100` must not exceed `max_ev100`. |
| `tonemap` | `agx` (default), `aces_fitted` or `neutral`. Case and `-` / `_` do not matter. |
| `use_physical_camera` | Derive EV100 from `aperture`, `shutter_seconds` and `iso` instead of `exposure_ev`. |
| `aperture` | Aperture in f-stops (0.5 to 128). |
| `shutter_seconds` | Shutter time in seconds (1e-6 to 3600). |
| `iso` | ISO sensitivity (1 to 1e6). |
| `contrast`, `saturation` | The display look after the tonemap (0.05 to 8 and 0 to 8; engine default 1.2 and 1.4). |
| `bloom` | Bloom on or off. |
| `bloom_intensity` | Blend toward the blurred image, 0 to 1 (default 0.04). |
| `vignette` | Vignette strength, 0 to 1. |
| `film_grain` | Film grain strength, 0 to 1. Keep 0 for deterministic goldens. |
| `meter` | A partial object of `low_percentile`, `high_percentile`, `center_weight` (all 0 to 1), `bright_threshold` (cd/m², 0 to 1,000,000) and `bright_bias_ev` (-4 to 4). A field left out keeps its value. |
| `band_bounds_final_exposure` | Off (default): the meter is clamped to `min_ev100`..`max_ev100`, then `exposure_compensation` and the meter's `bright_bias_ev` shift it, so a frame of sunlit snow can settle under the band floor. On: both shifts apply first and the band clamps last, so the band bounds the final auto exposure. A map's look does not change it. |

Errors: `F0002` (`-32602`, `invalid_params`) for an unknown field, a value out of range or of the
wrong type, an unknown tonemap name, `min_ev100` above `max_ev100`, or no field at all. Nothing is
applied when any value is refused. `F0003` (`-32001`, `not_found`) means this app has no
`RenderSettings` resource (usually it has no `RenderPlugin`).

`band_bounds_final_exposure` (ADR-7969) is for a map whose band was tuned as the range the frame
may print at: with a band of EV100 10.5 to 11.5, a +1 EV snow bias and +1 EV compensation, a frame
of sunlit ice metering EV100 12.5 or more prints at EV100 9.5 with the switch off (1 EV under the
floor, clipped highlights) and at 10.5 to 11.5 with it on. Inside the band the switch changes
nothing. A level file cannot set it yet (the scene format gains the field with its v0.6 bump): the
host sets it once with `render.set_exposure_settings` or on `PostSettings` in code.

```json
{"method": "render.set_exposure_settings", "params": {"band_bounds_final_exposure": true}}
```

Settings this method does not own, and where they live: the post stack switch and the anti-aliasing
mode (`render.set_profile`), the color grade, LUT and local exposure (`level.set_look`), and motion
blur and depth of field (the camera's `PhysicalCamera`).

```json
{"method": "render.set_exposure_settings", "params": {"tonemap": "aces_fitted", "auto_exposure": false, "exposure_ev": 12.5}}
```

Captured on lavapipe (llvmpipe Vulkan): an unsupported tonemap name is rejected before any settings change.

```json
{"jsonrpc":"2.0","id":1,"method":"render.set_exposure_settings","params":{"tonemap":"film"}}
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"`tonemap` must be one of agx, aces_fitted, neutral","data":{"code":"F0002","hint":"check the method's parameters in the agent guide"}}}
```

Judge a change by eye with `seeing.screenshot` (target `offscreen`) after a few frames, and pin
`auto_exposure` off and `film_grain` to 0 for goldens.
