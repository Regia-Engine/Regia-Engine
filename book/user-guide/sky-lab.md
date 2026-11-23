# Sky Lab: a day and a change of weather

Sky Lab (`samples/sky_lab`) is a small headless sample for the sky. It answers one question: what
do the sun, the sky and the clouds look like across a whole day, and across a change of weather?
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
it likes. The engine has no weather system yet: nothing changes the layer except the code you write.

Two things to know when changing weather at runtime:

- Moving coverage or cloud type rebuilds the weather map (128x128 by default). That is cheap, but it
  happens on every frame of a slow blend.
- The wind drifts the weather by `wind_m_per_s * time_s`. Changing the wind speed while `time_s` is
  large makes the whole cloud field jump, because the drift is the product. Blend the wind and keep
  `time_s` as the integral of the speed over time if the jump matters to you.

## What to look for

- Dawn and dusk: the sun below the horizon still lights the upper layer for a while; the frames are
  dark but not black.
- Midday: the brightest frames, a blue sky between the clouds, the ground lit by the sun.
- Overcast: the layer is low and thick; the sun disk is hidden and the light is flat.

The agent-oriented description (flags, the report fields, exit codes and the tests) is in
[Sky Lab (agent guide)](../agents/sky-lab.md).
