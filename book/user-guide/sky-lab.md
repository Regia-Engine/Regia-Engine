# Sky Lab: a day and a change of weather

<!-- index: Sky Lab -->
<!-- index: Time of day -->
<!-- index: Cloud settings -->
<!-- index: Weather -->
<!-- index: Precipitation -->
<!-- index: Surface weather -->
<!-- index: Scene wind -->
<!-- index: Rain -->
<!-- index: Snow -->

Sky Lab (`samples/sky_lab`) is a small headless sample for the sky. It answers one question: what
do the sun, the sky and the clouds look like across a whole day, and across a change of cloud
cover? Rain, snow, wet ground, and the wind near the ground are a separate system, in
[Weather in a project](#weather-in-a-project) below.
The scene is a plain, a ridge to the north and the sky. The camera stands 3 m above the ground and
looks north, away from the midday sun, tilted up so clouds fill the top half of the frame.

## A first run

```sh
cargo run -p sky_lab -- --out /tmp/sky_lab_run
```

This renders 120 frames at 320x180. The sun climbs from 5:30 to a midday peak and sets by 18:30,
while the clouds change from a few scattered cumulus (`fair`) to a dense, windy storm layer
(`storm`). The tool writes `frame_0000.png`, `frame_0020.png` and so on, plus `report.json` with
the hour, the sun's elevation, the cloud coverage and the mean brightness of each frame. Open the
first, middle and last PNG side by side to see the change.

Other starting points:

```sh
# An overcast afternoon, larger images, a frame every 10.
cargo run -p sky_lab -- --from overcast --to overcast --start-hour 13 --end-hour 17 --size 640x360 --every 10 --out /tmp/sky_overcast

# Sunset with the sky clearing.
cargo run -p sky_lab -- --from broken --to clear --start-hour 16 --end-hour 20 --out /tmp/sky_sunset
```

## Using the same pieces in a game

The sample sets two things every frame: a `TimeOfDay` for the sun and a `CloudSettings` for the
layer. A game sets the same resources:

```rust
use regia::prelude::*;
use regia::render::{CloudSettings, TimeOfDay};

fn advance_the_day(mut time: ResMut<TimeOfDay>, mut clouds: ResMut<CloudSettings>, dt: Res<Time>) {
    time.hour = (time.hour + dt.delta_secs() * 0.01).rem_euclid(24.0);
    clouds.time_s += dt.delta_secs();
}
```

The presets and the blend function live in the sample crate (`sky_lab::WeatherPreset`,
`sky_lab::blend_clouds`); they are ordinary `CloudSettings` values, so a game can copy the numbers
it likes. Sky Lab only changes the cloud layer. It does not wet the ground or draw rain.

Two things to know when changing that cloud layer at runtime:

- Moving coverage or cloud type rebuilds the cloud map (128×128 by default). That is cheap, but it
  happens on every frame of a slow blend. It does not change surface wetness.
- The wind drifts the clouds by `wind_m_per_s * time_s`. Changing the wind speed while `time_s` is
  large makes the whole cloud field jump, because the drift is the product. Blend the wind and keep
  `time_s` as the integral of the speed over time if the jump matters to you. That product is the
  cloud layer's clock. It is not `SceneWind`.

## Weather in a project

`docs/specs/99-weather-precipitation.md` is the project weather. It decides what falls, how
exposed surfaces get wet or collect snow, and the wind 10 m above the ground. Sky Lab's cloud
blend does not run it. Changing a Sky Lab preset does not change `SurfaceWeather`.

Three effects stay off until `assets/config/weather.ron` turns them on: drawn precipitation,
surface weather, and weather-driven scene wind. The file format is `docs/formats/weather-ron.md`
(read as `config://weather.ron`). A missing file leaves every effect off. `regia new` writes the
file with all three on for the `blank`, `first_person`, `third_person`, and `top_down` templates.
`net_game` and `quick` do not ship it. With the effects off, a `Weather` kept for its clouds
renders the same frames as before, including a map that sits in `overcast` (that preset's
precipitation would otherwise drizzle). Turned on, they still do nothing until the weather has
precipitation, a wind, or a surface state that is not dry.

| Preset | With the effects on |
|---|---|
| `overcast` | 2 mm/h of rain. Surfaces wet. Puddles do not fill. |
| `rain` | 6 mm/h. Puddles fill in about 85 minutes from dry. |
| `storm` | 50 mm/h, and visibility of 3 km. |
| `snow` | 1.2 cm/h of snow. The ground is covered in about 2.5 hours. |
| `blizzard` | 5 cm/h of snow, and visibility of 400 m. Cover in about 35 minutes. |

`precipitation_kind` defaults to `auto`: snow at or below 0 °C, rain at or above 2.5 °C, and a
blend between those temperatures. `rain`, `snow`, and `sleet` force the kind. Rates and the
checks an agent can run are on
[Rain, snow, wet surfaces and the wind near the ground](../agents/weather-precipitation.md).

`SceneWind` is the wind near the ground. Mode `Weather` means the weather writes it.
`Manual` holds a value you set. `Off` reads as no wind. Grass, rain, snow, and the foliage wind
follow it. Cloth does not: cloth wind stays the host's value.
`SurfaceWeather` is wetness, puddles, and snow cover. Mode `Simulated` integrates over game
time. Mode `Manual` holds the numbers (a wet street with no weather clock).
`WeatherAir` is the extra extinction while rain or snow is falling. The frame applies it where
it applies fog.

Terrain layers answer the same surface state. Format v5 carries a per-layer `weather` block
(`porosity`, `snow`, `porosity_from_orm`) and the stack's `puddles`. See
[Photoreal terrain](terrain-photoreal.md). A roof stays dry when the precipitation occlusion
view is on. `render.surface_exposure` compares a point under the roof with a point beside it.

Not in this version, from spec 99 § 16: lightning, drops on the lens, streaks running down
walls, hail, footprints and other snow deformation, rain shafts under single clouds, and
regional weather volumes. The weather is one state for the whole scene. The path tracer and
baked GI stay dry. Local lights do not light the rain and snow. Rain does not fall onto water
bodies, and the sea state is not driven from this page. The sea-state table is a water extra
and is off by default (`docs/specs/100-water-extras.md`).

## What to look for

- Dawn and dusk: the sun below the horizon still lights the upper layer for a while; the frames are
  dark but not black.
- Midday: the brightest frames, a blue sky between the clouds, the ground lit by the sun.
- Overcast: the layer is low and thick; the sun disk is hidden and the light is flat.

The agent-oriented description (flags, the report fields, exit codes and the tests) is in
[Sky Lab (agent guide)](../agents/sky-lab.md).
