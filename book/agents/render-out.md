# Render-out (agents)

`render.out_*` renders the running scene to finished frames: any size, any frame rate, supersampled,
with the game HUD laid out for the output, to a PNG sequence or (through `ffmpeg`) to an FFV1, ProRes
4444 or DNxHR 444 movie. The clock advances by exactly one frame per rendered frame and nothing reads
a wall clock, so the same request from the same world state gives the same frame hashes
([what repeats](#what-repeats)). Guide topic:
`agent.guide {"topic": "render_out"}`. Design: ADR-267.

Frames are made as fast as they render, not in real time. A job needs a renderer (a windowed app, or
`RenderPlugin { headless: true }`) and an extracted camera and mesh, like `seeing.screenshot`
`target: "offscreen"`. Use a headless app when you can: a windowed app shares its TAA history between
the window and the output size.

## Methods

- `render.out_start { out, frames | duration_s, width?, height?, fps?, fps_den?, supersample?, format?,
  name?, hud?, layout?, cuts?, warmup_frames?, settle_frames?, calibration?, subframes?, shutter_angle?,
  bit_depth?, time_map?, aovs?, aov_format?, timings? }`
  starts a job and answers at once with
  `{ job, status, request, render_size, hud, ffmpeg }`.
  - `out` is the output directory (created). A relative path is joined to the project root: the project directory of the running game or editor or, in an app without an asset server (the headless showcase, a test app), the project directory the agent was given (`AgentSettings::project_dir`, the showcase's `--project-dir`; ADR-1561). Only an app with neither joins it to the working directory; an absolute path must lie under an allowed output root: the project root, the system temp dir, or a directory listed in `output_roots = ["/abs/path", ...]` in `.regia/agent.toml`. `..` components, NUL, a drive or UNC prefix, a device name, an absolute path under no allowed root, and a symlink leading out of every allowed root are refused with `F0002` / `invalid_params`, `data.reason = "path_outside_project"` and `data.path_reason` naming the shape (the error names the rule, not any host path; see [Request paths](safety.md#request-paths)). `.` is the project root itself. `name` is the file stem (default `frame`).
  - `width`/`height` are the delivered size (default 1920×1080, at most 8192). `fps` is the rate
    numerator (default 30) and `fps_den` the denominator: 29.97 is `fps: 30000, fps_den: 1001`. The
    step is `fps_den / fps` seconds rounded to the nanosecond and is the same for every frame.
  - `supersample` (1..=4) renders at that many times the width and height and averages down in linear
    light; the HUD is drawn at the same density, so it matches the unsupersampled image. At most 100
    million rendered pixels.
  - `format`: `png` (default; `<name>_000000.png`, 8-bit RGBA), `ffv1` (`<name>.mkv`, lossless),
    `prores` (`<name>.mov`, ProRes 4444, 10-bit) or `dnxhr` (`<name>.mov`, DNxHR 444). The movies need
    `ffmpeg` on `PATH`; without it the call fails with `F0019` and nothing is written. `exr`
    (`<name>_000000.exr`) writes the scene-linear frame instead of the display image: see
    [Scene-linear EXR frames](#scene-linear-exr-frames-format-exr). For a deeper display image use
    `bit_depth: 16` (16-bit PNG).
  - `hud` (default true) draws the game UI; `layout` is `auto` (by orientation), `landscape` or
    `portrait`. Author one `UiRoot` per layout and tag it `UiVariant` (see the DO27 host checklist).
  - `cuts` lists frame numbers (0-based) that start with a camera cut; frame 0 always does (with `settle_frames`, the first settle frame does instead). A cut
    drops TAA/TSR history, fog history, the static-screen cache, shadow cascades (the terrain's too),
    irradiance probes and the metered exposure, as in a fresh renderer. With auto exposure on, the cut
    frame is then exposed by its own meter, not at the meter's initial EV: see
    [Exposure at a cut](#exposure-at-a-cut).
  - `warmup_frames` steps and renders that many small, undelivered frames before frame 0, so
    particle effects, spawns and streaming settle.
  - `settle_frames` (0..=`warmup_frames`, default 0) renders the last that many warm-up frames at the
    delivered size and supersampling, and the job's opening camera cut moves from frame 0 to the
    first of them, so frame 0 has that many frames of temporal history (TAA, alpha-hashed foliage, fog;
    ADR-1449). Without it frame 0 is the first frame after the cut: a one-frame still is a single
    jittered TAA sample, and foliage drawn with the hashed alpha test under TAA shows as a raw dot
    stipple. Use 16-32 for stills. Frame 0's game time is the same either way (the settle frames are
    warm-up frames); `cuts` may still list frame 0.
  - `calibration` is `neutral` (default: before the player's display calibration) or `applied`.
  - `timings` (boolean, default false): record the wall milliseconds of each delivered frame's render and
    readback in the manifest as `render_ms` (one number per delivered frame, in order; `null` when off).
    It times the one-render 8-bit path (no `subframes`, no `bit_depth` 16, no `exr`): the other paths leave
    the list empty. PNG encoding runs on the writer threads and is not in the number. The showcase
    performance table ([Showcase performance table](showcase-perf-table.md)) reads it.
  - `subframes` (1..=64, default 1) and `shutter_angle` (degrees, more than 0 and at most 360, default
    180): motion blur. See [Motion blur](#motion-blur-subframes-and-shutter_angle).
  - `bit_depth` is 8 (default) or 16. See [16-bit frames](#16-bit-frames-bit_depth-16).
  - `time_map` scales the simulation time each frame covers (slow motion, speed ramps) while the
    output frame rate stays fixed. See [Slow motion and speed ramps](#slow-motion-and-speed-ramps-time_map).
  - `audio` (`true` or `{ sample_rate?, format?, stems?, timeline? }`) also writes a WAV mixdown of
    the master output and optional per-bus stems, rendered offline on the job's render clock (or the
    simulation clock with `timeline: "output"` or `"sim"`). See [Render-out audio](render-out-audio.md)
    (ADR-1520, ADR-1554).
  - `aovs` (any of `"depth"`, `"id"`, `"motion"` and the material AOVs `"normal"`, `"normal_view"`,
    `"roughness"`, `"metalness"`, `"base_color"`, `"material_id"`, at most 9 entries) and `aov_format`
    (`"exr"`, the default, `"raw"`, or `"png"` / `"png16"` for the material AOVs that fit) write
    per-frame AOV files next to each frame. See
    [AOVs](#aovs-depth-ids-and-motion-vectors-aovs-aov_format) and
    [Material AOVs](#material-aovs-normal-roughness-metalness-base_color-material_id).
  `render.out_start` waits for material-graph builds already queued before it answers. If a deep queue outlasts your wait you get `F0006`, the job has still started, and asking again gets `F0018`: pass `timeout_ms`, or read the running job with `render.out_progress` (ADR-2728).
  Mutating; permission `edit_scene`. One job at a time (`F0018`). `F0009` if a `play.step` is running,
  `F0011` when the app has no renderer. Limits: `frames` or `duration_s` gives 1 to 1,000,000 frames,
  `fps` is a whole number from 1 to 1,000,000, `fps_den` from 1 to 100,000, and `fps / fps_den` is at
  most 1000, `warmup_frames` at most 10,000,
  `settle_frames` at most `warmup_frames`,
  and `name` may not contain a path separator or start with a dot.
- `render.out_progress { job?, hashes?, hashes_from? }` returns `{ job, status, frame, frames,
  fraction, game_time_s, sim_time_s, replay_from_tick, sequence_hash, camera_cuts,
  exposure_metered_cuts, out, hashes?, error?, manifest? }` (a running job also reports `phase`,
  `arming`, `warmup`, `rendering` or `closing`; `subframe`, the next subframe of the current frame; and
  `sim_step_ns` and `time_scale`, the simulation step and scale of the frame `frame`). During
  `closing` every frame is made and the app is back on its own clock, while a worker thread finishes
  the outputs (the WAVs are synced there, which can take a minute on a busy disk); the app keeps
  answering, and `status` stays `running` until every file is in place and the manifest is written. `game_time_s` is the render clock: the video time of frame `frame`
  after frame 0, `frame` steps, whatever the `time_map`. `sim_time_s` is the simulation clock: the same
  without a `time_map`, else the sum of the simulation steps before frame `frame`. `replay_from_tick`
  is the tick a replay armed with `replay.play { start: "render_out" }` started from on frame 0 (`null`
  when none was).
  `camera_cuts` counts the job's cuts; `exposure_metered_cuts` how many of them metered their frame
  (all of them with auto exposure on, none with a fixed exposure). `status` is
  `running`, `cancelling`, `done`, `cancelled` or `failed` (with `error`). With `hashes: true` it adds
  `hashes: { from, hashes: [..] }` (up to 4096 per call). The latest eight finished jobs are kept.
  Read-only. A running job also reports `warming`: `true` until the job's first render has finished (on a
  cold host that render compiles pipelines and uploads the scene; see below).
  **A poll during a long step is answered (ADR-2582).** The app answers requests between frames, and one
  step of a job can hold it for many seconds (a cold host's first render took 30 s on an RTX 5090). A poll
  of the running job that the app has not answered within 250 ms (`regia_agent::BOARD_ANSWER_AFTER`) while
  the app thread is inside a render-out step is answered by the connection from what the job published as
  the step began: the same fields as above, plus `app_busy: true` and `busy_ms` (how long the step has run).
  A poll for another job, or outside a step, waits for the app as before. The app does not run a poll
  answered this way (the audit log records it with the outcome `board`). Scoped tool tokens wait for the
  app. A waiting poll looks again every 250 ms, so a step that began after it was queued still answers it. During
  a stall long enough to fill the request queue a poll is still answered, without an audit row, and
  `render.out_cancel` still wakes a job waiting on its stalled output (ADR-2728). Polls answered from
  the board stay queued until the app drains them, marked so dispatch skips them; the skip costs no
  per-frame budget, so one draining frame clears any number of them and the cancel behind them
  dispatches at once (ADR-4894).
- `render.out_cancel { job? }` (permission `edit_scene`) stops the job at the next frame. What was written stays; a movie is
  finalized so it plays; the game clock, pause state and time scale are handed back. A job in its
  `closing` phase has no frames left to stop: it answers `{ job, status: "closing", ending }`, where
  `ending` is the status its frames ended with (`done`, `cancelled` or `failed`; the closing can still
  turn it into `failed`, so read the final status from `render.out_progress`). A finished job answers
  `{ job, status, finished: true }` with its final status. Without `job` the call means the running job,
  else the last finished one (ADR-1965).
  A cancel also reaches a job whose output is stalled (an `ffmpeg` that stopped reading, a hung disk):
  the app waits for room in a full output only until a `render.out_cancel` (or `agent.job_cancel`) is
  queued (over the WebSocket or the local transport), then keeps the frame it made, holds both clocks still for one app frame and handles the
  request. A refused or mistargeted cancel changes nothing: the frame is handed over next and is still
  frame N (ADR-2396). A movie whose `ffmpeg` still reads nothing ten seconds after its job ended is
  stopped and the job ends `failed` with what `ffmpeg` said; frame writers stuck on a hung disk are left
  to finish on their own after the same grace, and their unfinished frames are not kept.

```json
{"method": "render.out_start", "params": {"out": "/tmp/shot1", "frames": 120, "fps": 30, "width": 1080, "height": 1920, "supersample": 2, "layout": "portrait", "format": "png", "cuts": [60], "warmup_frames": 10}}
{"method": "render.out_progress", "params": {"hashes": true}}
{"method": "render.out_cancel", "params": {}}
```

Examples of the rest of the surface:

```json
{"method": "render.out_start", "params": {"out": "/tmp/clip", "duration_s": 2.5, "fps": 24, "format": "ffv1", "name": "clip"}}
{"method": "render.out_start", "params": {"out": "/tmp/pan", "frames": 240, "fps": 60, "width": 3840, "height": 2160, "subframes": 8, "shutter_angle": 180, "bit_depth": 16, "hud": false}}
{"method": "render.out_progress", "params": {"job": 3, "hashes": true, "hashes_from": 100}}
{"method": "render.out_cancel", "params": {"job": 3}}
```

### The first job of a headless host

A headless host (the showcase without `--window`, a render farm host) draws only through offscreen captures, so
nothing warms the pipelines its first render-out frame needs: that frame compiles them on the app thread, and the
app answers nothing meanwhile (30 s on the showcase scene with an RTX 5090; a poll timed out with `F0006`). The boot
warm that `render.pipeline_warm` reports does not cover it. Ask for the offscreen warm and wait for it before the
first job (ADR-1965):

```json
{"method": "render.pipeline_warm", "params": {"offscreen": {"width": 1920, "height": 1080}}}
{"method": "render.pipeline_warm", "params": {}}
```

Without the warm the first job still answers its polls during that render (`warming: true`, `app_busy: true`,
frame 0; ADR-2582), but the frame takes as long as before.

`width` and `height` are the job's render size (its size times `supersample`). The renderer moves to a thread of
its own and renders the frame there (its pipelines, the scene's uploads, the size's targets) while a stand-in holds
its place and the app keeps answering; `render.out_start` answers `F0018` until it is back. `offscreen_warm.state`
goes `running`, then `done` (`returning` for the one app frame it takes to put the renderer back) with
`offscreen_warm.last` = `{ size, ms, pipelines, renderer_back, error }`. Frames are the same with and without the
warm (two jobs in one host still repeat). A host with a window warms through its own window path and answers
`idle`. On the showcase scene under lavapipe the first job's longest wait for an answer went from 33 s to 11 s (the
second job's is 3 s). A headless host keeps no pipeline cache across launches unless `REGIA_PIPELINE_CACHE_DIR` names
one: set it so a second launch compiles less, and allow a first launch on new binaries a longer wait for
`agent.hello`.

## Errors

| Call | Condition | Code |
| --- | --- | --- |
| `render.out_start` | `out` refused by the [Request paths](safety.md#request-paths) rule | `INVALID_PARAMS` `F0002`, `data.reason` `path_outside_project`, `data.path_reason` (nothing is created) |
| `render.out_start` | missing `out`; `frames`/`duration_s` absent, zero or above 1,000,000; a size, `fps`, `supersample`, `warmup_frames`, `settle_frames` (above `warmup_frames` too) or `cuts` entry out of range or of the wrong type; unknown `format`, `png_compression`, `layout` or `calibration`; `writers` outside 0..=16; a `name` that is not 1..=64 letters, digits, `_`, `-`, `.` (not leading `.`) | `INVALID_PARAMS` (nothing is created) |
| `render.out_start` | `fps / fps_den` above 1000 (`fps` 1001 with the default denominator, or `fps` 30000 with `fps_den` 1). `fps` 1000, and `fps` 30000 with `fps_den` 1001, stay inside the cap | `INVALID_PARAMS`: "the frame rate is above 1000 fps" (nothing is created) |
| `render.out_start` | `subframes` outside 1..=64; `shutter_angle` not a number in (0, 360]; `bit_depth` not 8 or 16; `bit_depth: 16` with a movie `format` (png and raw are fine) or with `calibration: "applied"` | `INVALID_PARAMS` (nothing is created) |
| `render.out_start` | `bit_depth: 16` with `hud` on (the default) in an app that has a HUD | `INVALID_PARAMS`: pass `hud: false` |
| `render.out_start` | `time_map` not an array of `{frame, scale, ease?}`; more than 4096 keys; a `frame` above 1,000,000; a `scale` outside 0..=100; an unknown `ease`; two keys on one frame | `INVALID_PARAMS` (nothing is created) |
| `render.out_start` | a `time_map` so fast that one frame would run more than 64 fixed steps (`MAX_SUBSTEPS_LIMIT`). The shutter doubles the simulation time counted toward that cap, so the same `scale` hits it sooner; the cap stays 64 | `INVALID_PARAMS`: lower the peak `scale` or raise `fps` (nothing is created) |
| `render.out_start`, `render.out_job` | an `aovs` entry that is not an AOV name (`depth`, `id`, `motion`, `normal`, `normal_view`, `roughness`, `metalness`, `base_color`, `material_id`) | `INVALID_PARAMS` `unknown_aov` (nothing is created) |
| `render.out_start`, `render.out_job` | `aov_format` `"png"` or `"png16"` with `depth`, `id`, `motion` or `material_id` in `aovs` (they need exact values) | `INVALID_PARAMS` `aov_format_unsupported` (nothing is created) |
| `render.out_start` | `aovs` not an array; `aov_format` not `"exr"`, `"raw"`, `"png"` or `"png16"`, or given without `aovs`; `aovs` with a movie `format` | `INVALID_PARAMS` (nothing is created) |
| `render.out_start` | `format: "exr"` with `bit_depth: 16` or `calibration: "applied"`; `format: "exr"` with `hud` on in an app that has a HUD | `INVALID_PARAMS` |
| `render.out_progress` | a `format: "exr"` job whose view runs without the post stack (no scene-linear frame) | the job ends `failed` with an `error` that says so |
| `render.out_start` | a job is already running, or the offscreen warm has the renderer (`render.pipeline_warm` `offscreen_warm.state` `running`) | `INVALID_REQUEST` `F0018` |
| `render.out_start` | a `play.step` is running | `INVALID_REQUEST` `F0009` |
| `render.out_start` | no `RuntimeRenderer` (add `RenderPlugin`) | `UNSUPPORTED` `F0011` |
| `render.out_start` | movie format without `ffmpeg` on `PATH` | `UNSUPPORTED` `F0019` |
| `render.out_start` | `audio` asked for and the app has no `Audio` resource; bad `audio` keys ([Render-out audio](render-out-audio.md#errors)) | `UNSUPPORTED` `F0013`; `INVALID_PARAMS` |
| `render.out_start`, `render.out_job` | the host opened no GPU on purpose (`regia_agent::GpuDisabled`: a headless editor started with `--no-gpu`, or one that found no adapter); nothing is created and no device is opened (ADR-5180) | `UNSUPPORTED` with the host's code (`editor_headless`) |
| `render.out_progress` | no job was ever started, or `job` names no running or kept job | `NOT_FOUND` |
| `render.out_cancel` | no job was ever started, or `job` names no running or kept job | `NOT_FOUND` |

`render.out_cancel` is idempotent: cancelling a job already cancelling answers `cancelling` again, a
closing job answers `closing`, a finished one its final status, and none of them changes anything. `render.out_progress` is read-only, and `hashes_from` past the end returns an empty
window. Parsing is a pure function of the request: the same request always gives the same step, cut
list and frame count.

## Determinism and the manifest

Each frame's FNV-1a hash (of the delivered RGBA bytes) is in `render.out_progress` and, with the
request, the step in nanoseconds, the sequence hash and the exact ffmpeg arguments, in
`<name>_render_out.json` next to the output. The manifest's `schema` is `regia.render_out/2`:

- `files` is `{ "frames": [...], "audio": [...], "aov": [...] }`: the frame files (or the movie);
  the WAVs of an `audio` job, the mix and then each stem ([Render-out audio](render-out-audio.md));
  and per AOV `{ aov, files }` with frame `i` at index `i` ([AOVs](#aovs-depth-ids-and-motion-vectors-aovs-aov_format)).
  Both lists are empty when the job wrote none. Version 1 (`ferrite.render_out/1`, up to v0.2.1) had <!-- rename-keep: history -->
  the frame list as `files` itself.
- `frame_times` has one row per delivered frame: `{ frame, game_time_ns, game_time_s, sim_time_ns,
  sim_time_s, sim_step_ns, time_scale }` (the render clock, the simulation clock, the simulation step
  the frame covers and its `time_map` scale). Without a `time_map` both clocks are equal.
- `clock` says how the frames were timed and where the app stood when the job started: `{ mode:
  "fixed_step", wall_clock: false, step_clock, job_time_origin: "zero", app_game_time_at_start_ns,
  app_sim_time_at_start_ns, app_frame_at_start, live_effects, effect_seeds, particle_clock }` (ADR-1656,
  ADR-1881, ADR-2633, ADR-2687). `live_effects` is `restored` for a job that held the world and `moved_on`
  otherwise. `effect_seeds` is `job_stream`: effects spawned without a seed during the job draw from a seed
  stream that restarts with every job. `particle_clock` is `restarted` when nothing was alive in the particle
  pool at the start, judged by the lifetimes the pool spawned, so its clock and spawn ring started over, and
  `continued` otherwise. `job_time_origin` says the job
  ran `Time::elapsed` and `Time::sim_elapsed` from zero; the `app_*` values are where the app's clocks
  stood before that. `app_sim_time_at_start_ns` tells how far the world had got. `app_frame_at_start`
  counts app updates only: a paused app runs them at its host's speed, so it differs between runs and
  says nothing about the frames.
- `game_time_s` and `sim_time_s` are the two clocks after the last delivered frame, and
  `replay_from_tick` the armed replay's start tick (`null` when none was). To encode a PNG sequence
  later, feed it to `ffmpeg` with the arguments in the manifest.

### What repeats

A job's frames, AOVs and audio are a function of three things (ADR-1881): **the world state when the
job starts**, **the request**, and the **project, settings and GPU driver**. The world state is what
the app's simulation holds: entities and their components, physics, game state, a game's random
streams, particles alive, the position of a playing shot, sequence or replay, a weather transition, and
any timeline a host runs for itself. A job does not rewind it. It continues from where the world stands
and moves it on like any other stretch of the app. One exception: a job whose `time_map` holds the world
on every frame (every key at scale 0) puts the live effects back when its frames end (below).

Nothing else reaches the frames. A job restarts all of this, so what the app did before the job does not
show:

- the wall clock, the app's frame count and how long the app has run are not read;
- `Time::elapsed` and `Time::sim_elapsed` run from zero for the job and carry on afterwards from where
  they stood plus what the job ran, so anything posed from them (the material clock's wind sway, an
  `Update` system reading `Time::elapsed`) starts at the same phase in every job;
- the fixed loop has no time banked, so the same fixed steps land on the same frames at any `fps`;
- the `regia_vfx::Weather` cloud clock and wind drift run from where the weather
  started and carry on the same way afterwards;
- every mesh and every instance of a `RuntimeScatter` field picks its LOD afresh (ADR-1881, ADR-2672), so a job
  does not inherit levels or cross-fades from where the camera was before it. A host that drives its own
  `ScatterField` calls `ScatterField::restart_lod` itself;
- ambient life's animation clock, drifts and bird flights restart when the job restarts the game clock
  (`book/agents/ambient-life.md`), and the automatic birds move back near the job's camera;
- all temporal render state restarts at the job's opening camera cut: TAA and TSR history and jitter,
  screen noise phases, fog, clouds, shadows, probes and the metered exposure.
- every frame of the job is complete (ADR-2582, `FrameRenderer::set_complete_frames`, on for the job's
  length): work the runtime renderer normally moves off the frame lands on the frame that needs it, not on
  one chosen by the CPU's or the GPU's speed. Material-graph pipelines and sky rebuilds build inline (what
  was queued before the job is waited for at its start), emissive light fits run on the frame thread,
  texture streaming uploads every set it wants in that frame without a fade, the stochastic SSR pass never
  goes dormant on a counter readback, and the GPU cull keeps submission order. On a fast card that work could
  land partway through the first job and stay for the second, so two identical jobs in one host gave different
  color frames while depth, id and motion repeated; a software rasterizer is slow enough that it always landed
  between frames.
- the history that only picks how *new* effects look (ADR-2687): effects spawned without a seed during the
  job draw from a seed stream that restarts with every job (the app's own stream carries on after the job
  where it stood); when no particle can be alive, the particle clock (the heat-haze phase) and the spawn
  ring (the slots new particles take, the blend sort's last tie-break) start over as in a new context; an
  idle `CameraShake`'s noise clock starts over; the camera director's shake seeds and the shake field's
  emit seeds start over when no shake is running or on its way.
- the particles step once per game frame, in whichever render of the frame comes first (ADR-2687). In a
  windowed app the job draws what the window stepped; when the window did not draw (minimized, occluded,
  its surface not ready) the job's render takes the step, so the job's particles do not depend on whether
  the window managed to draw.
- the effects layer's warm frames (ADR-2741): a new effects context counts as live for a few frames with
  nothing in it, so the frame builds its targets before the first effect, and each frame it is live moves
  the particle clock. A job gives it none (the app gets its own back at the end), so the first job in a
  fresh host reaches its first spawn at the same clock, and the same heat-haze phase, as every later job. A window
  resized during the job arms warm frames for the window's targets; they wait for the job's end (ADR-2808).
  A job that starts while the context still builds on a warm worker installs it first, so its early
  effects are drawn and it starts the way later jobs do; the compile only makes that job start later (ADR-2814).
- the effect instance slots (ADR-2814): with no instance live they start over at a job's start, so effects
  spawned in the same order take the same slots, whichever retired first in the job before.
  Before, Red-Rouge's first held pair on a software rasterizer differed from the crash's impact on (15 of
  24 color frames equal) while later pairs repeated.

| Two runs of one request | Hashes |
| --- | --- |
| Two fresh apps | equal |
| One app, the replay armed or seeked to the same tick before each job | equal, for a world posed from the replay |
| One app, the world did not move: a `time_map` of scale 0 holds the simulation, or the scene is posed from the clocks alone | equal |
| One app, the first job moved the world (the fixed loop ran a game or a host timeline) | differ: the second job continues the world. `clock.app_sim_time_at_start_ns` differs too, which is the tell |
| One app, the world held and a live effect running (a `vfx.crash`, its fires) | equal: the particles move on the render clock inside each job, and a held job puts them back when its frames end (ADR-2633) |
| One app, no hold, nothing alive at either start, the game spawns an effect (seeded or not) at the same job time | equal (ADR-2687) |
| One app, no hold and a live effect running | the color frames differ (the second job continues the effect); depth, id and motion can repeat, because the AOVs do not draw particles |
| Different GPUs or drivers | differ |

**Live effects under a hold (ADR-2633).** Red-Rouge's v0.2.13 retest (0 of 24 color frames equal between two
held jobs, depth, id and motion 24 of 24) came from a `vfx.crash` left running in the host. Live effects run on
the render clock (ADR-1524), so a held job still shows its smoke drifting and its fires flickering. When its frames end, however the job ended, these go
back to where the job found them: the effect instances (ages, spawn counters, random streams), the particle
pool on the GPU, `vfx.crash` sequences, the fires and their clock, the debris clock, the decal pool, the camera
shake, a weather transition with its clock and drift, surface wetness and snow, and the fall of rain and snow. A job with any other `time_map`, or none, leaves them where its last frame did: the manifest's
`clock.live_effects` says which (`restored` or `moved_on`). A paused app also holds the fire and debris clocks
between jobs, so the idle frames a host runs while clients talk to it do not reach a later job.
The camera director's shakes and every camera's received shakes (`regia_framework`) play on the same clock
and go back the same way (ADR-2687).

Give both jobs some `warmup_frames` when you compare them: a fresh host's first rendered frame can show a
world that is still arriving (in the showcase, frame 0 of a held shot differs from the rest without a
warm-up), which a later job never sees.

The showcase's director is a timeline of the scene (the track, the spawns, the weather changes), so a
second job in one showcase host renders later frames of it, the fourth row. To compare two takes of one
shot there, use a fresh host for each. The tests are
`crates/regia_agent/tests/render_out_deterministic.rs`, `render_out_replay_scene.rs` and
`samples/showcase/tests/render.rs` (`two_jobs_repeat_their_color_*`: where the AOVs of two jobs in one
showcase host repeat, the color frames must too; set `REGIA_GPU_UUID` to run them on a card).

## Exposure at a cut

Auto exposure reads a histogram of each frame two frames late (`FrameRenderer::EXPOSURE_READBACK_FRAMES`).
A cut restarts the meter, so in a game the two frames after a cut print at the meter's initial EV: the
camera's or the map's fixed EV when one is set, otherwise the top of the auto-exposure band (EV100 15, a
sunny day). Before ADR-1455 a render-out did the same: a one-frame still was that first frame, and on
the showcase's hazy morning it printed 1.26 EV darker than the meter's own reading (EV100 15.0 against
13.74). A render-out now meters every cut before delivering it. After the cut it renders the cut's game
frame two more times at the delivered size. These renders are not delivered, and the clock and the
particles stay still while they run. Then it cuts again with `CutExposure::Fade`, which restarts every
temporal history but keeps the meter's reading. The delivered frame is exactly the first frame after a
cut, exposed for itself. A job also makes auto exposure wait for its histogram readbacks
(`set_wait_for_exposure_readback`), so the meter reads the same histograms on the same frames in every
run, and the hashes stay deterministic. Cost: two renders per cut. A view with a fixed exposure (a
camera EV, a profile without auto exposure) cuts once, as before. The manifest and
`render.out_progress` report `exposure_metered_cuts`. `camera_cuts` counts a metered cut once.
`render.exposure_log` shows each frame's EV (`crates/regia_agent/tests/render_out_cut_exposure.rs`).

## Motion blur (`subframes` and `shutter_angle`)

With `subframes` N above 1, each delivered frame is the average, in linear light, of N renders taken
while a virtual film shutter is open. The shutter opens at the frame's own time and stays open for
`shutter_angle / 360` of a frame (180 = half a frame, the film default; 360 = the whole frame). Subframe
k is rendered at the middle of its slice of that interval. The game clock steps between the subframes
and closes the frame with the rest of the step, so every frame still advances the clock by exactly one
step, and the subframe times are whole nanoseconds computed from the frame number alone: the same
request gives the same hashes, and frame 100 has the same subframe times whether the job started at
frame 0 or not. A camera cut applies before the first subframe of its frame.

Everything that moves with the game clock blurs: transforms changed in `Update`, camera paths and shot
sequences, particles (the VFX step is each subframe's delta), physics bodies (interpolated poses). Things
moved only in `FixedUpdate` without interpolation step at the fixed rate and blur only when the fixed rate
is above the frame rate. The supersample blocks of all subframes are summed and quantized once, so
`supersample: 2, subframes: 8` is a 32-sample average per pixel. Cost: N full renders per delivered frame.
`subframes: 1` (the default) is the classic frame, byte for byte.

How many subframes: an edge moving S pixels per frame is smeared over `S · shutter_angle / 360` pixels and drawn as N
copies along that streak. Too few subframes show as separate steps instead of a smooth smear (16 subframes over an
80-pixel streak leave visible 5-pixel steps), so pick N near the streak length in pixels of the fastest thing you care
about, and add supersampling for the rest. A slow pan needs few; fast debris close to the camera needs many.

## 16-bit frames (`bit_depth: 16`)

With `bit_depth: 16` the frame is rendered into a half-float target instead of an 8-bit one; the post
stack keeps its tonemapped image in half floats too, so nothing is quantized to 8 bits before the file.
The frames are 16-bit RGBA PNGs (sRGB-encoded like the 8-bit ones). A half float holds 11 significant
bits, so a smooth sky or mist gradient has at least 8 distinct levels between two 8-bit codes instead of
one hard step; a 10-bit video master made from them keeps the extra depth. Limits: PNG only (the movie
formats take 8-bit frames), `calibration: "neutral"`, and no HUD (the UI overlay draws 8-bit; an app
with a HUD needs `hud: false`). On the SMAA tiers (Mobile High and the Deck's non-temporal fallback) the
SMAA pass blends in 8 bits, so the file has 8-bit precision there; the TAA, TSR and FXAA tiers are deep
end to end. The frame hash is FNV-1a over the 16-bit values as little-endian bytes.

The manifest and the echoed `request` carry `subframes`, `shutter_angle` and `bit_depth`.

## Slow motion and speed ramps (`time_map`)

The output timeline never changes: frame `i` is `i / fps` seconds into the video. `time_map` changes how
much *game* time each frame covers. It is a list of keys `{ "frame": F, "scale": S, "ease": E }` (frames
are the job's delivered frames, 0-based; `scale` 0..=100, 1 = real time, 0.25 = quarter speed, 0 = a
frozen frame; at most 4096 keys). Before the first key and after the last the scale holds. Between two
keys it ramps into the later key's scale with that key's `ease`: `smooth` (the default, smoothstep, so the
speed eases out of and into each key), `linear`, or `hold` (the earlier scale up to the key's frame, then
a jump). Frame `j` covers `round(step · scale(j + 0.5))` nanoseconds of game time (the scale at the
frame's middle), and frame `i` is at the sum of the steps before it. Warm-up and settle frames use the
scale before the first key. Everything is whole nanoseconds computed from the request, so the same
request gives the same game times and the same hashes. No `time_map` (or a map of ones) is the plain
timeline, byte for byte.

```json
{"method": "render.out_start", "params": {"out": "/tmp/kill_cam", "frames": 180, "fps": 60, "hud": false,
  "time_map": [{"frame": 30, "scale": 1}, {"frame": 60, "scale": 0.2}, {"frame": 120, "scale": 0.2}, {"frame": 150, "scale": 1}]}}
```

That is real time for half a second, a smooth half-second ramp down to one fifth speed, one second of slow
motion, and a ramp back. The echoed `request.time_map` lists the keys as parsed.

**Two clocks.** A job with a `time_map` runs two clocks:

| Clock | Step per frame | Read through | Who runs on it | Reported as |
|---|---|---|---|---|
| render | `step` (1 / fps), always | `Time::delta`, `Time::elapsed`, `RealTime` | VFX, shot players and `render.sequence`, HUD and UI animation, the material clock, `Update` systems, the shutter and TAA | `game_time_s`, `frame_times[].game_time_*` |
| simulation | `round(step · scale)` | `Time::sim_delta`, `Time::sim_elapsed`, `FixedUpdate`'s `Time` | the fixed loop (physics, gameplay), sim bridges (`read_snapshot`), replays | `sim_time_s`, `frame_times[].sim_time_*`, `sim_step_ns` |

The job drives the render clock exactly as without a map and sets the simulation clock through the
`regia_app::SimStep` resource each frame (it puts back a host's own one at the end). It never uses
`Time::time_scale`. At 0.25 a burst of particles plays at its normal speed while the ship that fired it
moves at a quarter.

- **It is not motion blur.** `subframes` and `shutter_angle` still sample inside each frame on the
  render clock; the simulation moves by the same share of the frame's simulation step between
  subframes, so in simulation time the blur shortens, as with a high-speed camera. The two parameters
  combine freely.
- **The simulation keeps its fixed rate.** A slowed frame runs fewer fixed steps (`FixedUpdate`), never a
  shorter one, so the game's results do not depend on the render's speed; what moves between fixed steps
  is presentation that interpolates (physics poses, `FixedTime::overstep_fraction`, a blending replay).
  A fast stretch can need more fixed steps per frame than the host's `max_substeps`: the job raises it for
  its own length and puts it back afterwards; above 64 the request is refused.
- **Replays follow the simulation clock.** `replay.play { from_tick, speed, start: "render_out" }` starts
  the replay on frame 0 at `from_tick`, so frame `i` shows tick `from_tick + tick_rate · speed ·
  sim_time(i)`: the replay's `speed` and the map multiply. Poses are blended between ticks for
  presentation only ([replay](replay.md#replayplay)).
- **Gameplay in `Update`.** A system that moves gameplay in `Update` with `Time::delta` stays at render
  speed under a map. Move it to `FixedUpdate`, or read `Time::sim_delta`.

## AOVs: depth, ids and motion vectors (`aovs`, `aov_format`)

`aovs` writes control inputs for video tools (enhancement, relighting, rotoscoping) next to each
delivered frame: `<name>_depth_000000.exr`, `<name>_id_000000.exr`, `<name>_motion_000000.exr`
(`.raw` with `aov_format: "raw"`). They are drawn by their own pass, not read out of the frame
renderer: the frame's meshes and the terrain are drawn once more through the job's camera, with no
TAA jitter, one sample at each delivered pixel center (no supersampling: an id cannot be averaged),
at the middle subframe of a motion-blurred frame (`subframes / 2`). The HUD is never in an AOV.

| AOV | Channels (EXR names) | Type | Encoding |
| --- | --- | --- | --- |
| `depth` | `Z` | `f32` | Linear view depth in meters: the distance from the camera plane along the view direction (not the distance to the eye). Sky: `+inf`. |
| `id` | `id.instance`, `id.class` | `u32` | `instance`: one id per entity drawn (the terrain counts as one). An entity keeps its id for the whole job, through moves, LOD swaps, and leaving and coming back into view. New ids of a frame are numbered in a fixed order (the terrain first, then entities by their `Entity` bits), so the same project and request give the same ids. `class`: one id per mesh asset (every instance of one mesh shares it; the terrain has its own). Sky: 0. |
| `motion` | `motion.u`, `motion.v` | `f32` | Backward motion in delivered pixels, `+u` right, `+v` down: where the surface under the pixel was in the previous delivered frame, minus where it is now (the engine's TAA convention). The motion is between delivered frames, not between subframes. The sky gets the camera's rotation. 0 on frame 0 and on every `cuts` frame, and exactly 0 wherever the surface and the camera are bit-identical to the previous delivered frame (a held world, ADR-1965: no rounding noise from projecting one position twice). |

EXR files are single-layer scanline OpenEXR with ZIP compression (lossless), written by the
pure-Rust `exr` crate; `f32` and `u32` channels are stored as they are, never as half floats. Raw
files have no header: the channels interleaved per pixel in the order above, little-endian 32-bit,
rows top to bottom. A frame is written whole: a frame counts as `written` only when its image and
each of its AOV files are on disk, and a failed write removes all of that frame's files.

The manifest's `aov` block records everything a tool needs:

- `encodings`: format, size, sampling, file pattern, per AOV its channels, type, unit, meaning and
  background value, and for `raw` the byte order, layout and bytes per file.
- `ids`: the id tables, `instances: [{id, kind: "entity" | "terrain", entity_bits?, label}]` and
  `classes: [{id, label}]`.
- `motion_reset_frames`: the frames whose motion is 0 (frame 0 and the cuts).
- `frame_hashes`: per AOV, the FNV-1a hash of each frame's raw bytes (the same for `exr` and
  `raw`), so two runs can be compared without the files.
- `input_hashes`: per frame, the FNV-1a hash of everything the AOV pass uploaded (camera, last frame's
  camera, every instance's matrix, last matrix, ids and held flag; ADR-1965). Two jobs with equal
  `input_hashes` and unequal `frame_hashes` differ on the GPU; unequal `input_hashes` mean the world
  or the camera differed.
- `terrain`: `{ drawn, sample_stride }` when a terrain was drawn.

The manifest's `files.aov` lists the files: `[{ aov, files: [...] }]` per AOV in request order,
frame `i` at index `i`, the same index as `files.frames` and `frame_times`. AOV frame `i` is
delivered frame `i`, so its render and simulation times are `frame_times[i]`. Under a `time_map`
(ADR-1524) the motion AOV covers one delivered frame on each clock: the camera, shots and VFX move
by the render step, simulated motion (physics, gameplay in `FixedUpdate` or on `Time::sim_delta`,
replays) by the frame's simulation step, so at scale 0.5 a simulated object's motion vectors are
half as long (ADR-1554).

## Material AOVs: normal, roughness, metalness, base_color, material_id

`aovs` also takes six material AOVs (ADR-3256), written in the same frame sequence and under the same
naming scheme as the others (`<name>_normal_000000.exr`, `<name>_roughness_000000.png`, ...), sorted
into the request after `depth`, `id` and `motion`. They come from a material pass that draws the same
list of meshes and the terrain as the id pass, with the same camera, no jitter, one sample at each
delivered pixel center, and does not depend on the quality tier, TAA or the upscaler. Every material
AOV is 0 on the sky.

| AOV | Channels (EXR names) | Type | Encoding |
| --- | --- | --- | --- |
| `normal` | `normal.x`, `normal.y`, `normal.z` | `f32` | Unit surface normal in world space: the vertex normal carried through the entity's normal matrix (inverse transpose of the model's upper 3×3), interpolated, renormalized and flipped toward the camera on a back face. The terrain's normal comes from its heightfield. |
| `normal_view` | `normal_view.x`, `.y`, `.z` | `f32` | The same normal rotated into view space: `+x` right, `+y` up, the camera looks down `-z`. |
| `roughness` | `roughness` | `f32` | Perceptual roughness: the material's factor, times the green channel of a packed ORM map (`orm_packed`), clamped to 0..1. The terrain is 1. |
| `metalness` | `metalness` | `f32` | The material's factor, times the blue channel of a packed ORM map, clamped to 0..1. The terrain is 0. |
| `base_color` | `base_color.r`, `.g`, `.b` | `f32` | Linear albedo before lighting: the factor times the sRGB-decoded base-color texture. Vertex color is not applied. The terrain is a neutral 0.5 grey. |
| `material_id` | `material_id` | `u32` | One id per material asset drawn (the terrain has its own), assigned the first time it is seen in the frame's sorted draw order and kept for the whole job. The manifest's `aov.ids.materials` is `[{ id, label }]`. |

`aov_format` `"png"` (8-bit) and `"png16"` (16-bit) write `normal`, `normal_view`, `roughness`,
`metalness` and `base_color` as PNG (grayscale for one channel, RGB for three): normals as
`n * 0.5 + 0.5` (the sky stays 0), `base_color` sRGB-encoded, roughness and metalness as they are.
`depth`, `id`, `motion` and `material_id` need exact values and are refused with
`aov_format_unsupported`; use `exr` or `raw` for them. With `png` or `png16` every AOV of the job must be one of the five.

The pass reads a material's factors, base-color texture, packed ORM map and alpha-mask cutoff. It
does not read normal maps, material graphs, shared or dynamic image slots, vertex color, the
deformation of a skinned mesh's normal (the rest-pose normal is used) or morph targets; a
translucent material is not drawn, as in every AOV. The same job run twice writes byte-identical
files, and the manifest's `aov.frame_hashes` has a hash per material AOV. A `.render_job.ron`
takes the same names in its `output.aovs` (and `aov_format`); no field was added, so the job format
stays at version 2. The CLI path is `regia render --job` with such a job; there is no per-AOV flag.

A shot fade ([camera paths](camera-paths.md), ADR-1522) is drawn by the compositor after the UI, on
8-bit display frames only, and never in an AOV: depth, ids and motion are the same with and without
a fade. Scene-linear (`exr`) and 16-bit frames are taken before the compositor and carry no fade.
The manifest's `fade` block, `{ in_frames, in_aovs: false, frames: [{ frame, amount, color }] }`,
lists the frames a fade was visible on, so a tool can apply it to a plate that does not carry it.

```json
{"method": "render.out_start", "params": {"out": "/tmp/plate", "frames": 96, "fps": 24, "format": "exr", "hud": false, "aovs": ["depth", "id", "motion"], "settle_frames": 16, "warmup_frames": 16}}
```

Known issues (AOV pass, ADR-1521):

- Alpha-masked materials (foliage cards) are tested against their cutoff with the base-colour
  texture's alpha, so a card has the shape of its leaves. The test samples the full-resolution
  texture without mipmaps, so far-away cards can shimmer between frames. A material graph's own
  alpha is not evaluated (its base-colour texture slot is used when there is one).
- Translucent materials (blend, premultiplied glass, additive), particles, water and decals have no
  AOV; the pixel shows what is behind.
- The terrain is a grid of its heightfield (at most 2048 samples per side; a larger map is drawn
  every `sample_stride`-th sample), built once per job: terrain holes and terrain edits during a
  job are not in the AOVs, and its depth can differ from the rendered terrain by its LOD.
- Skinned meshes are skinned on the CPU with the frame's four strongest influences; morph targets
  are ignored.

## Scene-linear EXR frames (`format: "exr"`)

`format: "exr"` writes each frame as `<name>_000000.exr` with `R`, `G`, `B` channels in `f32`:
the post stack's input, after TAA or the upscaler and before exposure, bloom, the tonemap and the
display encoding, in the frame's photometric units (cd/m², Rec. 709 primaries). A supersampled
frame is box-averaged in linear light; subframes are averaged. The manifest's `color` block names
the space and unit. Limits: `bit_depth` 8 (the default), `calibration: "neutral"`, and no HUD (an
app with a HUD needs `hud: false`). A view without the post stack has no scene-linear frame: the
job fails on its first frame with an error that says so. The frame hash is FNV-1a over the `f32`
values as little-endian bytes. Exposure is not applied: the EV each frame was shown at is in
`render.exposure_log`.

## Frame files, speed and `raw` (`format`, `png_compression`, `writers`)

Frame files (`format: "png"` or `"raw"`) are encoded and written by a pool of worker threads, not by
the thread that renders. The render thread renders, hashes and hands the frame over; it waits only when
the queue is full (back-pressure), so memory stays flat: at most `writers` frames wait and `writers`
more are being written. `writers` is 0..=16; 0 (the default) is automatic: half the cores, at most 8.
A file appears under its final name only when it is complete (it is written to `<file>.part` and
renamed). Progress reports `frame` (handed over) and `written` (the unbroken run on disk from frame 0).

- **`png_compression`**: `"fast"` (the default), `"balanced"` or `"best"`. Fast deflate is several times
  quicker than best and the files are somewhat larger; best is the smallest file the encoder makes.
  The level never changes the pixels or the hash.
- **`format: "raw"`**: no compression at all. One file per frame, `<name>_000000.raw`: tightly packed
  RGBA, rows top to bottom, no header. 8-bit frames are one byte per channel; with `bit_depth: 16` each
  channel is a 16-bit little-endian code. The manifest has a `raw` block with `pixel_format`
  (`rgba8` or `rgba16`), `width`, `height`, `channels` (4), `channel_order`, `bit_depth`, `byte_order`
  (`little` for 16-bit), `row_stride_bytes`, `frame_bytes` and `file_pattern`, so an external encoder
  can read the files without guessing. Alpha is always 255.
- **The hash does not depend on any of this.** It is FNV-1a over the delivered RGBA values before any
  encoding, so PNG at every level, raw, and any number of writers give the same hashes and the same
  decoded pixels (`crates/regia_agent/tests/render_out_frames.rs`).
- **Cancel and failure guarantee.** Every frame handed over before a cancel is written: after a cancel
  the files are exactly frames 0 to `frame - 1`, in order, none missing, none after, no `.part` file.
  If a write fails (disk full), the job fails and keeps only the unbroken run from frame 0 (a later
  file that finished first is removed); `frame`, `files` and the manifest's `frame_hashes` all stop at
  that run.
- **No GPL tool is required.** Only the `ffv1`, `prores` and `dnxhr` formats pipe to an `ffmpeg` found
  on `PATH`, and they are optional; the default needs nothing. A deliverable can be made from the PNG or
  raw frames with any external encoder whose licence you are happy with: read the PNG sequence, or read
  the raw files with the layout from the manifest.

## Rules

- Wait for loading to finish before starting (`assets.load_progress`); asynchronous builds that finish
  on different frames on different machines change early frames. Use `warmup_frames` for effects.
- `play.pause`, `play.resume`, `play.step` and `play.set_time_scale` are refused during a job (`F0018`):
  the job holds the clock, every frame, and a running step refuses the start (`F0009`).
- Pointer hover state does not reach the HUD during a job (open item from the ADR-202 audit).

Workflow and the HUD layouts from the game side: [Rendering a video](../user-guide/render-out.md).

## Render jobs

To render takes of a shot list or a sequence (folders per take, an automatic cut list, a job file
that `regia render --job` runs headless), see [render jobs](render-jobs.md) (`render.out_job`,
ADR-1616).

## WebM movies without ffmpeg (`format: "webm"`)

`format: "webm"` (alias `"av1"`) encodes the frames to AV1 and the audio mixdown to Opus in one
`<name>.webm`, in-process, with no external program (ADR-3369). It needs a build with the `media-encode`
feature (`regia/media-encode`); without it the job is refused with `data.reason`
`media_encode_unavailable`. The optional `encode` block sets `quality` or `bitrate_kbps`, `speed`,
`keyframe_interval`, `audio_kbps` and `threads`; `threads: 1` makes the file byte-identical run to run.
The picture must be even-sized, 16..=8192 pixels a side; the mixdown must be at a rate Opus takes
(48 kHz, the default). A cancelled job leaves a playable movie of every delivered frame. Settings, limits
and error codes: [Video encoding](video-encode.md).
