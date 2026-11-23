# Weather

The `Weather` resource (`regia_vfx::weather`, ADR-429) drives the [volumetric cloud
layer](clouds.md): coverage, cloud type, density and wind, plus a precipitation value for games to
read. `VfxPlugin` installs the driver (`WeatherPlugin`); nothing happens until a `Weather` resource
exists. Each frame the driver steps it by the game's `Time` (so it pauses and scales with the game)
and writes the result into the `CloudSettings` resource. Coverage, type, density, wind, the
clock and the drift are always written. The genus presets `fair_cumulus`, `cumulus`,
`stratocumulus` and `cirrus` also write altitudes, `shape_scale_m`, `detail_scale_m` and look.
The other presets leave those fields alone. `shadow_map`, `fog_coupling` and `overcast_ibl`
stay as they are.

- **Presets**: `clear`, `fair`, `cloudy`, `overcast` (a low grey stratus deck) and `storm`
  (towering, dense cells in a strong wind), `rain`, `snow` and `blizzard`, and the genus presets
  `fair_cumulus`, `cumulus`, `stratocumulus` and `cirrus` (those four also rewrite the layer
  heights, noise tiles and look).
  `WeatherState::preset(name)`.
- **Transitions** ease with a smoothstep over a given number of seconds of game time. While one
  runs, coverage and cloud type are written in steps of 1/256 and density in 1/64-octave steps, so
  the CPU re-thresholds the weather map at most once per step (a cached noise field, written into
  the same texture: no texture or bind group is created). At rest the values are exact.
- **Wind** drift is integrated over the weather clock and handed to the renderer as
  `CloudSettings::drift_offset_m`: a wind change carries the clouds on from where they are and keeps
  the cloud history valid. (Setting `wind_m_per_s` on `CloudSettings` directly still moves the
  clouds by `wind × time_s`, so a change there jumps them.)
- **Weather cuts**: a change of more than 0.05 coverage, 0.1 cloud type or a 1.25× density ratio in
  one frame (for example `set_now` to another preset, or a `transition_s` of 0) is a cut. That frame
  marches the clouds and the cloud shadow map whole, like a camera cut. Smaller changes are followed
  by the reprojection: the history is clamped toward the fresh samples in proportion to how far
  the weather moved since it was marched, so a transition does not trail its old sky.
- **Precipitation** (`0..1`), with the temperature and the kind, gives what falls: `rain`, `snow` and
  `blizzard` are presets too. With the project's effects on it draws rain and snow, wets surfaces and
  moves the wind near the ground: see [Rain, snow and wet surfaces](weather-precipitation.md). With
  them off (the default) it stays a value to read, `Weather::rates(&config)` or
  `Weather::current().precipitation`.

```rust
use regia::prelude::*;
use regia_vfx::{Weather, WeatherState};

fn start_storm(mut weather: ResMut<Weather>) {
    // Two minutes from whatever the sky shows now to a storm.
    weather.transition_to(WeatherState::STORM, 120.0);
}

fn configure(app: &mut App) {
    app.insert_resource(Weather::new(WeatherState::FAIR));
}
```

## Agent methods

- `render.get_weather` → `{ active, current: { coverage, cloud_type, density, wind_m_per_s,
  precipitation, genus, temperature_c, precipitation_kind, gustiness }, target, transitioning,
  progress, remaining_s, clock_s, drift_m, paused, cloud_layer: { coverage, cloud_type, density,
  wind_m_per_s, drift_m }, presets, rates: { snow_fraction, rain_mm_per_h, snow_we_mm_per_h,
  snow_depth_mm_per_h, drop_lambda_per_mm, visibility_m }, config: { path, format_version, error } }`.
  `visibility_m` is null when nothing falls. Without a
  `Weather` resource `active` is false and `current` is the weather the cloud layer shows (the
  transition fields are absent).
- `render.set_weather { preset?, coverage?, cloud_type?, density?, wind_m_per_s?: [x, z],
  precipitation?, temperature_c?, precipitation_kind?, gustiness?, transition_s?, paused? }` →
  `{ before, after }`. Builds the target from `preset` (a built-in one or one of the project's;
  else the current target) with the given fields replaced and eases to it over `transition_s`
  seconds of game time (0 or absent: at once, a weather cut). With no `Weather` resource it creates
  one from the current cloud layer first, so a call that changes nothing moves nothing. The cloud
  layer is written straight away (`render.get_clouds` shows it before the next frame). `paused`
  stops or resumes the clock, the transition and the wind drift.
- Errors: an unknown preset, a wrong type (a string for a number, `wind_m_per_s` that is not two
  finite numbers, a non-boolean `paused`) or a negative `transition_s` answers `F0002` (invalid
  parameters) and changes nothing.

```json
{ "method": "render.set_weather", "params": { "preset": "overcast", "transition_s": 30 } }
{ "method": "render.get_weather" }
```

To watch a transition, step frames (`playtest` frame stepping or `wait_until`) and poll
`render.get_weather` until `transitioning` is false; a screenshot per step shows the sky change.

## Cost

Per frame, a held camera marches one low-resolution texel in sixteen and one sixteenth of the cloud
shadow map whatever the weather does; `regia_render::cloud_steady_work` gives the count and
`CLOUD_FRAME_SAMPLE_BUDGET` (about 4 M density samples, Desktop High at 1440p with the map on) is
the budget it is tested against. `cloud_work_counters()` counts what the renderer actually did
(view frames marched whole or reprojected, texels marched, map texels, weather map packs, uploads,
textures and weather cuts).

## Checks

- Unit (`regia_vfx::weather`): presets in range; a transition eases monotonically to the exact
  target; turning the wind at t = 1000 s moves the drift by at most `wind × dt` a frame and equals
  the integrated drift; a 10 s clear-to-overcast transition has no weather-cut frame and changes the
  weather map at most once per 1/256 step; `set_now` is a cut and keeps the layer's switches;
  paused weather holds; taking over a layer moves nothing; in an app the plugin follows game time.
- Unit (`regia_render::clouds`): the wind row is unchanged without a drift offset; the CPU weather
  sample follows the integrated drift; the cached weather field gives byte-identical maps;
  weather cuts against transition steps; the weather motion is 0 when still and 1 at a cut; the
  Desktop High 1440p frame fits the sample budget and the full march does not.
- Agent (`regia_agent::weather`): get without weather, set a preset at once, a transition with
  overrides and partial follow-up sets, every bad parameter answers F0002.
- Frames (`regia_vfx/tests/cloud_weather.rs`, GPU): `wind_change_keeps_the_clouds_in_place`
  (no history reset, no displacement, a small error against the full march),
  `weather_cut_marches_the_whole_frame` (SSIM ≥ 0.95 against a fresh full march),
  `coverage_transition_tracks_the_full_march` (half way and after settling),
  `weather_transition_stays_inside_the_work_budget` (counted: exactly the steady-state texels a
  frame, no whole-map march, no texture, bind group or pipeline created); ignored bench
  `cloud_weather_cost_at_1440p` (clouds and map ms, held and in transition, against 0.7 ms).

## Haze and rain from the atmosphere tables (`aerial_from_tables`)

On a frame that draws the physical sky (the atmosphere tables: Desktop High and Ultra on Earth,
any HDR single-sample frame on another planet), the sky shades from the sky-view table and the
ground from the aerial-perspective table. By default two things still split them at the horizon:

- the aerial table ends at 32 km, so ground farther out keeps the 32 km haze while the sky's last
  row holds the whole path through the air;
- the falling rain's or snow's extinction (`Weather::rates(&config).visibility_m`) reaches only the
  ground (forward pass, lite pass, shared terrain), with the analytic stand-in sky as its colour,
  and the sky stays as clear as on a dry day.

`RenderProfile::aerial_from_tables` (off in every preset, ADR-3294) moves both onto the tables:
past 32 km the ground's haze keeps thickening toward the sky's first row above the horizon, and the
extinction becomes one full-screen veil after the clouds over the sky, the clouds and the ground
alike, mixing by `exp(−σ d)` (`d` is the pixel's distance, the far plane for the sky) toward the
light the drops scatter: the average over the whole sphere of the horizon sky above, the lit
ground below and the sun's share, with a drawn cloud deck greying and dimming all of it and
blocking the sun. So heavy rain under a deck dims the sky and the ground together. Every term is table radiance in cd/m² times the HDR target's pre-exposure,
the same units the forward pass writes. Frames without the physical sky do not change.

Cost per pixel, in texel loads: the full path reads two aerial slices (8 loads; 12 past 32 km);
the Mobile (lite) path reads one slice (4; 8 past 32 km); the veil adds 4 while something falls.
No froxel volume either way.

- `render.get_weather` adds `aerial_tables: { enabled, frame_mode, fetches_per_pixel: { aerial,
  aerial_past_range, veil }, seam: { off, on, far_m, extinction_per_m, sun_elevation_deg } }`.
  `enabled` is the profile switch (null without a `RenderSettings` resource); `frame_mode` is what
  an atmosphere frame of this profile draws (`off`, `full`, or `lite` on a lite-forward profile).
  `seam` is the horizon seam both modes give, from a CPU model of the tables for the current
  `TimeOfDay` (Earth's air, the eye 2 m up, the farthest ground 50 km out) and the current
  precipitation: `Σ |sky − ground| / Σ |sky|` between the sky's last row and the farthest ground
  row over 16 azimuths. 0 is seamless.
- `render.set_weather { aerial_from_tables: bool }` turns the switch on or off (with or without a
  weather change in the same call) and answers `aerial_from_tables` too. It is a runtime switch:
  `render.get_profile` still names the preset. Errors: a non-boolean answers `F0002`; an app
  without `RenderSettings` answers `F0003` and changes nothing.

```json
{ "method": "render.set_weather", "params": { "preset": "storm", "aerial_from_tables": true } }
{ "method": "render.get_weather" }
```

To check it by eye, take a screenshot with the switch off and on at the same frame; the horizon
seam on the image is `regia_render::horizon_seam::measure_horizon_seam` on a linear readback and
its sky mask. The owner's plates come from `REGIA_AERIAL_PLATES=<dir> cargo test -p regia_render
--test aerial_tables` (off | on, noon clear, noon heavy rain, sunset).
