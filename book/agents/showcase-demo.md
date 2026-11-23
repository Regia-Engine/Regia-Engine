# Showcase demo: the interactive mode

`showcase --demo` runs the showcase scene as an interactive demo (ADR-3013): a free-fly camera, time-of-day and
weather sliders, a quality-tier picker that detects a tier and says why, a measured frame-rate overlay, a
screenshot key, a benchmark button that writes the [performance table](showcase-perf-table.md) and a credits
page read from the scene's asset manifest. It is offline: no accounts, no network, nothing is sent anywhere.

Every control is reachable three ways that end in the same code path (`showcase::demo::apply`): the on-screen
panel, the keys and the gamepad, and the `demo.*` agent methods below. A refusal is the same from all three.

The agent methods exist only with the showcase's `agent` feature (the default, for filming, tests and automation).
The shipped Regia Forest Demo is built `--no-default-features --features demo` and has no agent endpoint at all
(ADR-5419); measure a release candidate with its agent-on twin, the same commit built with default features.

## Run it

```sh
# The example scene (beside the binary, in the source tree, or generated into <output folder>/example_scene):
cargo run --release -p showcase -- --demo

# A scene of your own, starting at a tier instead of detecting one, with the output folder named:
cargo run --release -p showcase -- path/to/scene --demo --tier Medium --demo-out ~/demo-out

# Headless (agents and tests): screenshots render offscreen at --size.
cargo run -p showcase -- --demo --headless --size 1280x720 --project-dir /tmp/demo-project
```

| Flag | Meaning |
|---|---|
| `--demo` | The interactive demo. Opens a window unless `--headless` follows. The scene directory is optional. |
| `--demo-out DIR` | Where `screenshots/` and `benchmark/` go. Default: `<user data dir>/demo` (`~/.local/share/RegiaEngine/showcase/demo` on Linux, `%APPDATA%\RegiaEngine\showcase\demo` on Windows). |
| `--tier NAME` | Start at `Low`, `Medium`, `High`, `Ultra`, `Deck` (or `MobileLow`, `MobileHigh`); `auto` (the default) detects. |

The other showcase flags (`--size`, `--fullscreen`, `--project-dir`, `--exit-after-frames`, ...) work as in
[the showcase guide](showcase.md). The scene's scripted weather changes are not applied in the demo: the sliders
own the weather.

`--exit-after-frames N` quits with exit code 0. In a window it counts the frames drawn after the loading splash is
gone (`showcase::demo::ExitAfterSceneFrames`, ADR-5452), so `./run-demo.sh --exit-after-frames 30` shows 30 frames of
the forest on any cache, cold or warm; headless it counts every frame. Before ADR-5452 the windowed demo ignored the
flag and ran until the player quit.

## Controls

| Input | Keyboard and mouse | Gamepad |
|---|---|---|
| Move | W A S D | left stick |
| Look | hold the right mouse button and move | right stick |
| Up / down | E or Space / Q or Ctrl | right / left shoulder or trigger |
| Faster | Shift | left stick click |
| Panel on or off, credits | F1, F2 | Select (View) toggles the panel |
| Screenshot | F12 | North (Y / Triangle) |
| Benchmark, stop it | F9, Esc | Start |

The camera is the engine's free camera (`regia_framework::camera`, `CameraMode::Free`) on the showcase camera,
starting where the scene's camera looks.

## The quality picker

On the first frame with a GPU device the demo detects a tier with the engine's GPU table
(`regia_render::quality::detect`, the pick `regia_framework::auto_quality` makes on a first launch) and writes the
reason in words under the picker, for example:

> Picked High (desktop, high settings) because your graphics card, NVIDIA GeForce RTX 3080 Ti with 12 GB of
> video memory, is one the engine's GPU table lists from measurements (row `nvidia_rtx_3080_3090`). You can pick
> another tier at any time.

When the table's row was lowered (missing features, too little video memory, the platform) the words say so:
"It was then lowered: Ultra to High: 6144 MB VRAM < 8192 MB." The engine's one-line reason is kept as `reason`
for logs and bug reports. Picking a tier (or "Auto" again) switches the renderer's settings live; Medium is
Desktop High drawn at 67 % and upscaled.

The demo has no `GameSettings`, so the engine's `quality.get_detected` answers its pick as `host`:
`{ tier, source, detection }`, the same as `demo.state`'s `quality` (ADR-5131).

## Loading and the warm queue

The windowed demo opens on a loading splash ("REGIA FOREST DEMO - LOADING") with no time cap (ADR-5131). Behind it
the warm queue builds, on its workers: the splash's own pipeline (the first boot job; the window shows the splash
background until then), the UI overlay, the engine's canonical frame, the terrain draw with its grass and contact
map (`"showcase terrain"`, map tier), and then the first demo frame itself (`GameFrameWarm`, requested when the
splash finishes once the terrain is in and the boot tier is done). The window keeps presenting and answering the OS
the whole time, and the first scene frame compiles nothing on the frame thread. A terrain that fails to build (an
error, or a panic in its warm job in a non-shipping build) still ends the loading screen; the log (`showcase` target, `terrain: ...`) and
`ShowcaseStats::terrain_error` say why (`terrain warm job panicked` for a panic, ADR-5620).

Check on a cold cache (an empty `HOME`, or a fresh `REGIA_PIPELINE_CACHE_DIR`):

```json
{"jsonrpc":"2.0","id":1,"method":"render.pipeline_warm","params":{}}
```

After `boot_background.idle`, `pipelines_outside` is 0 and `outside_sites` is empty; `game_frame.last` holds the
first frame's warm (`pipelines`, `ms`, `timed_out: false`). On a software rasterizer the first frame's warm may take
a minute; the demo lets it run up to 600 s while it keeps building (`progress_grace` 120 s) instead of abandoning it
to the frame thread. The headless demo (`--headless`, the agent tests) builds the terrain inline and has no splash.

## What the sliders do on each tier

Medium, High and Ultra run the physical atmosphere: the time of day moves the sun through a drawn sky, and the
weather draws clouds. Low, Deck and the phone tiers draw a fixed sky, so there the time of day moves the sun's light
and shadows (it fades at the horizon, warms when low, and is gone at night) and cloud cover dims it. The panel says so
under the picker, and `demo.state`'s `quality.draws_sky` and `quality.sky_note` report it. The demo does not switch
the atmosphere on for those tiers: a benchmark row has to measure the tier as it ships.

The demo turns on the rain and snow its slider promises (weather effects are otherwise off unless a project's
`config/weather.ron` turns them on); `render.precipitation` reports what it draws, and `render.set_precipitation`
can turn it off again.

## The player's range (ADR-5419)

The panel keeps the player where the forest looks right; agent calls keep the whole range.

- **Hour:** held between sunrise and sunset of the chosen day and latitude (`showcase::demo::sun::daylight_hours`, the
  sun at least `PLAYER_MIN_SUN_ELEVATION` = 0 rad high; solar noon in a polar night). Moving the day slider pulls the
  hour into that day's daylight. About half an hour after sunset the ground renders black on every tier (the scene
  has no night lighting), so the slider stops at sunset.
- **Sky:** cloud cover up to `PLAYER_MAX_COVERAGE` = 0.6 (the `cloudy` preset), and the preset buttons clear, fair and
  cloudy (`PLAYER_WEATHER_PRESETS`). Under fuller cover the renderer still draws hard sun shadows.

`demo.set_time_of_day { "hour": 23 }` and `demo.set_weather { "preset": "overcast" }` still work for an agent; the
panel's slider then shows the state as it is.

## The frame-rate overlay

The overlay shows `60 fps (16.7 ms, 1 % low 48 fps) | High` (one decimal below 10 fps): frames over the total time of the last 120 frames,
their average frame time, and the frame rate of the 99th-percentile frame time. The frame time is the wall time
between two frames of the running app; nothing is estimated (before the second frame it says "measuring frame
rate").

## Screenshots

F12, the panel's button or `demo.screenshot` write a PNG to `<out>/screenshots/showcase-<unix seconds>-<n>.png`
(or the name an agent gives, inside that folder). The window captures its next presented frame as shown; a
headless app renders the current frame offscreen at `--size` and writes the file at once.

## The benchmark

F9, the button or `demo.benchmark` play every view of the scene's `perf_views.json` (the views file of the
performance table) at the running tier: 30 warm-up frames and 240 measured frames per view by default. Each view's
camera moves one 60 Hz step per frame, as in `cargo xtask showcase-table`, so every run shows the same pictures;
the measured value is the wall time between two presented frames. The flight input is ignored while it runs and
the camera comes back afterwards. The result is `<out>/benchmark/showcase-table.md` and `showcase-table.json` in
the [performance table's](showcase-perf-table.md) format (the same writer, `showcase::perf_table`); the JSON's
`generator` is `demo benchmark` and the Markdown's first paragraph says what a frame time is. A stopped run writes
its table too, with the views it did not finish `not run`. The panel's status line shows each view's average.

## Credits and `asset_manifest.json`

The credits page lists the scene's `asset_manifest.json` (format version 1), one row per asset:

```json
{
  "version": 1,
  "assets": [
    { "path": "heightfield.png", "title": "Example heightfield", "author": "Regia Engine contributors",
      "license": "LicenseRef-Regia-Sample", "source": "Generated by `showcase --write-example`" }
  ]
}
```

`path` is relative to the scene directory (`/`, no `..`) and unique; `title`, `author`, `license` and `source`
are required and non-empty; `notes` is optional; other keys are refused. The page also lists the scene's files
that have no row, so a missing license shows on screen; a scene without the file, or with a malformed one, shows
why instead. The engine's own third-party notices are in `THIRD_PARTY_NOTICES.md`, which the page names. The
example scene's manifest credits every file of `samples/showcase/example_scene`.

## Agent methods

The methods are registered on the app's agent extension when the demo runs (capability `showcase_demo`).
Mutating methods need the `edit_scene` permission.

| Method | Params | Result |
|---|---|---|
| `demo.state` | none | `{ page, camera, time_of_day, weather, quality: { tier, source, summary, draws_sky, sky_note?, picker, detection?, render_scale }, fps, screenshots, benchmark, credits, last_error?, frame }` |
| `demo.fly` | `move?: [strafe, forward]` (-1..=1), `look?: [yaw, pitch]` (radians per frame, -0.5..=0.5), `rise?` (-1..=1), `sprint?`, `frames?` (1..=600, default 1) | `{ frames, move, look, rise, sprint }` |
| `demo.set_time_of_day` | `hour?` (0..=24), `day_of_year?` (1..=365), `latitude_deg?` (-90..=90) | `{ hour, day_of_year, latitude_deg }` |
| `demo.set_weather` | `preset?`, `coverage?` (0..=1), `precipitation?` (0..=1), `wind_m_per_s?` (0..=30), `transition_s?` (0..=600) | `{ preset?, coverage, precipitation, wind_m_per_s, transition_s }` |
| `demo.set_quality` | `tier`: `auto` or a tier name | `{ tier, source, why?, reason? }` |
| `demo.screenshot` | `path?` (relative, `.png`) | `{ path, written, pending, width?, height? }` (`pending` in a window: the file is written with the next frame; `demo.state.screenshots.last` names it) |
| `demo.benchmark` | `action?` (`start` or `cancel`), `warmup?` (0..=3600), `frames?` (1..=3600), `out?` (relative folder) | start: `{ started, views, warmup, frames, tier, out }`; cancel: `{ cancelled, markdown, json }` |
| `demo.credits` | none | `{ engine, manifest, format_version?, assets, unlisted, error?, lines }` |
| `demo.show` | `page`: `controls`, `credits` or `hidden` | `{ page }` |

`demo.set_weather` changes the same `Weather` resource as `render.set_weather`; `render.get_weather` lists the
presets. `demo.set_time_of_day` writes the level's sun clock (`LevelSettings::atmosphere.time_of_day`).

A session on the showcase app (these sample-host extension requests are outside the runtime
schema checked by `agent-examples`; `samples/showcase/tests/demo.rs` exercises their handlers):

```json,ignore
{"method": "demo.set_quality", "params": {"tier": "auto"}}
{"method": "demo.set_time_of_day", "params": {"hour": 18.5}}
{"method": "demo.set_weather", "params": {"preset": "overcast", "wind_m_per_s": 12}}
{"method": "demo.fly", "params": {"move": [0, 1], "rise": 0.2, "frames": 120}}
{"method": "demo.screenshot", "params": {"path": "evening.png"}}
{"method": "demo.benchmark", "params": {"warmup": 10, "frames": 60}}
{"method": "demo.state", "params": {}}
```

## Error codes

A refused call carries one of these in the error's `data.code`; the panel shows the same code on its status line.
Bad parameter types answer `demo_bad_value` too. Calling a `demo.*` method in an app without the demo answers
`F0003`.

| Code | Meaning |
|---|---|
| `demo_bad_value` | A number is out of range or not finite, a parameter has the wrong type, or a path is absolute, holds `..` or lacks `.png`. |
| `demo_unknown_tier` | The tier name is not one of the picker's. |
| `demo_unknown_weather` | The weather preset is not one the engine knows. |
| `demo_unknown_page` | The page is not `controls`, `credits` or `hidden`. |
| `demo_bench_running` | A benchmark is already running. |
| `demo_bench_idle` | No benchmark is running (cancel). |
| `demo_no_views` | The scene has no `perf_views.json`. |
| `demo_views_malformed` | `perf_views.json` does not parse, or a view is shorter than warm-up plus frames at 60 Hz (`view_too_short`). |
| `demo_no_renderer` | No GPU device or renderer yet (detection, a screenshot before the first frame). |
| `demo_screenshot_failed` | The capture or the file write failed. |
| `demo_bench_write_failed` | The benchmark's table could not be written. |

## Tests

`samples/showcase/src/demo/*` hold the CPU tests (input mapping, words for every detection of the built-in table,
the frame-rate window, PNG encoding, the benchmark's views and dates, the manifest rules, the panel's commands).
`samples/showcase/tests/demo.rs` runs the demo in an app: every method through the agent server, a benchmark on a
fixed frame clock writing the table, the panel's buttons, and an offscreen screenshot on the headless device
(lavapipe on the CI hosts; without any adapter that test prints a note and passes).
`demo::sun` and `demo::ui` test the player's range (sunrise and sunset of the forest's day, longer summer and shorter
winter days, midnight sun and polar night, the day slider pulling the hour back, the cover cap and the three presets).
`samples/showcase/tests/warm_queue.rs` checks the loading screen's configuration and when it finishes (no GPU), and
that the warm terrain build creates no `regia_terrain` pipeline outside the queue while the inline build does
(headless device).

## Package acceptance

`cargo xtask demo-package` builds the shipped demo (the `showcase` binary, `shipping` profile,
`--no-default-features --features demo`) for Windows and Linux into `target/demo-package`
([Packaging the demo](../user-guide/demo-package.md)); `cargo xtask steam-demo-upload --app ID --depot-win ID
--depot-linux ID --dry-run` stages that package for steamcmd and never uploads
([Staging the demo for a Steam upload](steam-demo-upload.md)).

Before using an archive for a demo session, follow [complete-package verification](../user-guide/demo-package.md#complete-package-verification).
That check rejects missing targets, unlisted files, changed asset bytes and
inconsistent bundle/depot/archive manifests. Run the `demo.*` session above on
the verified artifact and keep its hashes with the screenshot and benchmark
evidence. Unit fixtures do not establish shipping or device acceptance. The
showcase does not install the optional crash-upload plugin or configure an
endpoint/consent; local crash logs are separate from uploads.
