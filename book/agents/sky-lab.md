# Sky Lab: a day cycle with weather presets

`samples/sky_lab` renders the physical sky and the volumetric cloud layer for a fixed number of
frames while the sun moves from a start hour to an end hour and the weather blends from one preset
to another. It runs headless (a GPU adapter is needed; lavapipe works), writes PNG frames and a
JSON report, and has no window and no network. Use it to check a sky or cloud change over a whole
day instead of one pose, or as the smallest example of driving `TimeOfDay` and `CloudSettings` per
frame. For how clouds and the sky work see [Clouds](clouds.md) and [Atmosphere](atmosphere.md); the
walk-through for people is [Sky Lab (user guide)](../user-guide/sky-lab.md).

The sample has no agent-protocol methods of its own. It is driven from the command line, and its
output (`report.json`, PNG files) is what an agent reads back.

## Run it

```sh
cargo run -p sky_lab -- --out /tmp/sky_lab_run
cargo run -p sky_lab -- --frames 60 --size 640x360 --from clear --to overcast --start-hour 9 --end-hour 15
cargo run -p sky_lab -- --list
```

| Flag | Default | Meaning |
|---|---|---|
| `--frames N` | 120 | Frames to render, at least 1. |
| `--size WxH` | 320x180 | Image size, up to 4096 on a side. |
| `--from NAME`, `--to NAME` | `fair`, `storm` | Weather at the first and the last frame. |
| `--start-hour H`, `--end-hour H` | 5.5, 18.5 | Hour of day at the first and the last frame, wrapped into 0..24. |
| `--out DIR` | none | Write `frame_NNNN.png` and `report.json` under `DIR`. |
| `--every N` | 20 | A PNG every `N` frames; frame 0 and the last frame are always written when `--out` is set. |
| `--list` | | Print the preset names and exit. |

Exit codes: 0 done, 1 a frame failed to render or a file could not be written, 2 bad arguments (the
usage line is printed), 3 no GPU adapter.

## Presets

| Name | Coverage | Type | Layer (m) | Wind (m/s) |
|---|---|---|---|---|
| `clear` | 0 | cumulus | 1500 - 4000 | 6, 2 |
| `fair` | 0.3 | cumulus | 1500 - 4000 | 6, 2 |
| `broken` | 0.6 | cumulus | 1500 - 4000 | 9, 3 |
| `overcast` | 0.9 | stratus | 900 - 2400 | 4, 1 |
| `storm` | 0.85 | towering | 1200 - 6000 | 18, 6 |

`clear` has coverage zero, so the cloud pass is skipped. Between two presets, coverage, cloud type,
density, layer altitudes and wind interpolate with a smoothstep over the run. The renderer rebuilds
its weather map whenever coverage or type change, so a blend in progress rebuilds it every frame.

## The report

`report.json` has `width`, `height` and a `frames` array with one entry per frame:

```text
{ "frame": 60, "hour": 12.05, "sun_elevation_deg": 44.9, "coverage": 0.58, "mean_luma": 91.3 }
```

`mean_luma` is the mean of `0.2126 R + 0.7152 G + 0.0722 B` over the 8-bit image. The scene has a
fixed exposure, so luma follows the sun: it is lowest while the sun is below the horizon and highest
near midday. Everything is a function of the frame number, so two runs with the same flags give the
same hours, elevations and coverages. The luma values depend on the GPU driver.

## Checks

- `tests/smoke.rs` `a_day_of_120_frames_renders_and_the_sky_follows_the_sun`: 120 frames at 160x90
  render, every luma is finite, midday is brighter than the first and the last frame, the coverage
  ends higher than it starts, and consecutive frames differ.
- `tests/smoke.rs` `a_run_writes_frames_and_a_report`: PNG files appear every 40th frame and on the
  last, `report.json` parses, and no temp file is left behind.
- Unit tests in `src/lib.rs` and `src/lab.rs` (no GPU): preset names and ranges, blend end points,
  the hour wrap, the sun rising and setting over the default day, command-line parsing.

Both smoke tests print a note and pass when there is no GPU adapter, so a green run on a machine
without one proves nothing; run them on a lavapipe or a hardware machine.
