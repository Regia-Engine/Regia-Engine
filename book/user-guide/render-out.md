# Rendering a video (render-out)

<!-- index: Render-out -->
<!-- index: Supersampling -->

Render-out turns the running scene into finished frames: a PNG sequence, or an `ffmpeg` pipe to an
FFV1, ProRes 4444 or DNxHR 444 movie, at any size and frame rate with 1x to 4x supersampling. The
job drives the game clock by exactly one frame step per rendered frame and reads no wall clock, so
the same request from the same world state gives the same frame hashes. A job continues the world
from where it stands, so a second job after the world moved differs. It is controlled through the agent
protocol; the methods and their parameters are on the [agent page](../agents/render-out.md).
In the editor, **Sequence ▸ Render movie…** renders a saved sequence the same way, without any agent call
([Render movie](../agents/render-movie.md)).

## What you need

- A renderer: a windowed app, or an app with `RenderPlugin { headless: true }`. Use a headless app
  when you can: a windowed app shares its TAA history between the window and the output size.
  The job uses the profile already on `RenderSettings`, including `Cinematic` when
  `gate_preset` honoured it. It does not switch tiers on its own. Set the tier and the upscaler
  first ([Quality and upscalers](../how-to/picture/quality-and-upscalers.md)).
- `ffmpeg` on `PATH` for the movie formats. Without it `render.out_start` fails with `F0019` and
  writes nothing; choose `png` and encode later with the arguments saved in the manifest.
- Loading finished before the job starts (`assets.load_progress`), because asynchronous builds that
  finish on different frames on different machines change early frames.

## The HUD for the output

With `hud: true` (the default) the game UI is laid out for the output size and orientation, not the
window's, at the supersampled density, so it matches the unsupersampled image. Author one `UiRoot`
per layout and tag it `UiVariant`:

```rust,wrap
use regia_ui::{UiLayoutVariant, UiVariant};
# use regia_ecs::prelude::Commands;
# use regia_ui::UiRoot;
fn add_layout_roots(mut commands: Commands) {
// landscape (16:9) root
commands.spawn((UiRoot::default(), UiVariant(UiLayoutVariant::Landscape)));
// native 9:16 root
commands.spawn((UiRoot::default(), UiVariant(UiLayoutVariant::Portrait)));
}
```

`layout` is `auto` (landscape when width >= height), `landscape` or `portrait`. An untagged root is
drawn in every layout. Hosts that capture frames themselves use the seam in `regia_render`:
`RenderOutSpec::new(width, height, layout)` (optionally `.supersampled(factor)`) with
`RenderOutHud::begin` and `end`. Build the spec with `new`; a struct literal no longer compiles
outside the repo.

## A deterministic video

1. Start the app headless and wait for loading.
2. Load a [shot sequence](shot-sequences.md) and, for a match, a [replay](replay-sim.md).
3. `render.out_start` with `frames` or `duration_s`, `fps` (30000 with `fps_den` 1001 is 29.97), the
   size, `supersample`, `format`, and `cuts`.
4. Poll `render.out_progress`; read `<name>_render_out.json` next to the output for the per-frame FNV-1a
   hashes, the step in nanoseconds, the sequence hash and the exact ffmpeg arguments.

A camera cut (frame 0 and every frame in `cuts`) restarts TAA/TSR, fog, shadow (terrain included),
probe and exposure history as in a fresh renderer. `warmup_frames` renders that many small,
undelivered frames first so particles, spawns and streaming settle.

Align `cuts` with the sequence's shot boundaries: frame = shot start x fps.

## Sound

Add `audio: true` to `render.out_start` to get `<name>_audio.wav` next to the frames: the game's
mix, rendered offline on the job's clock, exactly as long as the video, with every sound on the
frame it started on. `audio: { stems: ["Music", "Sfx", "Voice"] }` adds one WAV per bus
(`<name>_audio_Music.wav`, ...), recorded after the bus volume. `format` is `pcm16`, `pcm24` (the
default) or `float32`; `sample_rate` defaults to 48000. Start the sounds you want inside the job
(in its warm-up, a replay or the game logic): sounds already playing when the job starts are not
carried in, and the speakers stay silent while it runs. With a `time_map` the sound stays on the
render clock by default: effects keep their speed and pitch while the picture's simulation slows.
`timeline: "output"` puts it on the simulation clock and slows it like tape (lower pitch), and
`timeline: "sim"` writes the simulation-clock sound unresampled for an editor. Details:
[Render-out audio (agents)](../agents/render-out-audio.md).

## Depth, ids and motion for video tools

`aovs: ["depth", "id", "motion"]` writes three extra files per frame next to the image (OpenEXR by
default, or headerless raw with `aov_format: "raw"`): linear depth in meters, an instance id per
entity that stays the same for the whole render plus a class id per mesh asset, and backward motion
vectors in pixels. Video enhancement, relighting and rotoscoping tools take them as control inputs.
`format: "exr"` writes the image itself as scene-linear OpenEXR (before exposure and the tonemap).
The manifest's `aov` block states every encoding and the id tables. Details and limits:
[Render-out (agents)](../agents/render-out.md#aovs-depth-ids-and-motion-vectors-aovs-aov_format).

## Known limits

Not in render-out today: surround audio, a per-cut exposure choice, a video mode in the `regia render`
CLI (it saves one still image), and pointer hover reaching the HUD. Output is at most 8192
pixels on a side and 100 million rendered pixels per frame.
