# Video on screen: frames, UI, materials

[Video playback](video-playback.md) opens, decodes and times a clip. This page covers the half that puts the
picture on screen (specs/50 § 2.7, ADR-2044): the frame source, the `UiVideo` widget, the material `video`
parameter, the fallback frame, the bounded decode ring, the GPU convert pass and fullscreen video. All of it is off until you add
it, so a game without video renders exactly as before.

## Turn it on

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
use regia::media::{MediaPlugin, VideoFramePlugin};
use regia_ui::video::UiVideoPlugin;

app.add_plugins((MediaPlugin, VideoFramePlugin, UiVideoPlugin));
```

`VideoFramePlugin` does four things:

1. Installs the frame source (`regia_media::MediaFrameSource`) in `regia_render::VideoFrames`, the seam the UI
   and materials read pictures through (`regia_render::VideoFrameSource`; `regia_ui` never names the media crate).
2. Turns on the **decode ring** for players it opens: the worker thread decodes ahead into a ring bounded by
   4 pictures **and** 32 MiB (`VideoFrameSettings::ring`, or `MediaSettings::ring_budget`). The worker waits when
   the next picture would not fit; a single picture larger than the budget is still allowed alone, so a tiny
   budget slows decoding but never stops it. A seek drops the queued pictures.
3. Publishes every newly presented picture as a sRGB RGBA8 texture (one `regia_render::DynamicTexture` per
   player) and as bytes the UI copies. The conversion is the CPU fast path (`regia_render::video::yuv_to_rgba8`,
   fixed-point, within ±1 of the reference). It runs only when the picture changes.
4. Applies transport controls (`VideoControl`: play, pause, toggle, seek, loop, skip) queued by widgets, scripts
   and agents, in `Update` before the clock ticks.

## Show a video in the UI

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_media::{ClockMode, VideoPlayer};
use regia_ui::video::{UiVideo, UiVideoAction, UiVideoControl};

let player = commands.spawn(VideoPlayer { clock: ClockMode::Audio, ..VideoPlayer::new("movies/intro.video.ron") }).id();
let screen = commands.spawn((UiNode::default(), UiVideo::new(player.to_bits()))).id();
commands.spawn((Button, UiVideoControl { video: screen, action: UiVideoAction::Toggle }));
commands.spawn((Slider::default(), UiVideoControl { video: screen, action: UiVideoAction::SeekSlider }));
commands.spawn((Toggle::default(), UiVideoControl { video: screen, action: UiVideoAction::Loop }));
```

- The node gets a `UiImage` with the picture (aspect kept unless `keep_aspect` is false) and a `UiVideoShown`
  with the serial, shown frame time, fallback flag and upload count.
- Buttons send `Play`, `Pause`, `Toggle` or `Skip` on click. `Skip` closes the player and reports
  `Ended { skipped: true }`; a fullscreen player with `skippable: false` refuses with `video_not_skippable`.
- A slider with `SeekSlider` seeks to its value (0 to 1) times the clip's duration, and follows the playback
  position. A `Loop` toggle turns looping on and off.
- **Audio sync:** give the player `ClockMode::Audio` (the `VideoPlayer` default is `Game`). The picture then
  follows the frames the audio output consumed, so pausing, seeking or stalling the sound moves the picture with
  it. Without an audio stream or `AudioClock` the player follows game time. `media.state` reports
  `audio_clock: true`.

## Show a video on a mesh

Put `VideoMaterial { player, slot }` on an entity with a `MeshRenderer`. The plugin rewrites the material once
so its `video` slot (`VideoSlot::Emissive`, the default, or `VideoSlot::Albedo`) samples the player's texture
(`regia_render::video_material`); later pictures update the texture, not the material. An emissive binding
with a black emissive color gets white at intensity 1 so the screen shows. An entity with `VideoMaterial` and no
`MeshRenderer` reports `video_material_missing_video_slot` in `media.state`.

## The fallback frame

The player's texture holds its clear color (the `.video.ron` `clear_color`, black by default) when there is no
picture to show:

| Situation | Texture |
|---|---|
| No picture yet (opening), closed, open failed, failed | clear color (`fallback: true`) |
| Buffering for fewer than `stall_ticks` ticks (default 30) | the last picture holds |
| Buffering for `stall_ticks` ticks in a row | clear color at the picture's size (`fallback: true`) |
| Pictures arrive again | the next picture (`fallback: false`) |

Ticks are counted per update, never wall clock, so tests and replays see the same frames.

## GPU convert (optional)

The convert pass (`regia_render::VideoConvertPass`, shader `video_convert.wgsl`) converts YUV planes to RGBA8 on
the GPU in a compute shader at the frame's `overlay` slot, into a double-buffered target per player. Enable it on
the renderer and share its queue:

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
# let frame_renderer: &mut regia_render::FrameRenderer = todo!();
let jobs = regia_render::VideoConvertJobs::default();
frame_renderer.enable_video_convert(&jobs);
app.insert_resource(regia_media::frame_source::GpuVideoJobs(jobs));
app.world_mut().resource_mut::<regia_media::VideoFrameSettings>().gpu_convert = true;
```

It matches the CPU path within ±1 per channel (tested on the lavapipe reference). It keeps at most one pending
picture per player. `jobs.view(player)` gives a sRGB view for `UiRenderer::add_image_view`, so the UI can show
the GPU result without a CPU copy. Material slots still use the CPU texture in this version (see Limits).

## Fullscreen video

A player with a `VideoFullscreen { fit, layer, skippable }` component is drawn over the whole view by the
fullscreen pass (`regia_render::VideoFullscreenPass`, shader `video_fullscreen.wgsl`) after the post stack and
under the UI. `fit` is `Letterbox` (whole picture, black bars), `Fill` (view covered, picture cropped) or
`Stretch`; `layer` orders several fullscreen videos (lower first; at most 8 a frame). The pass is off until you
enable it and share its queue:

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
# let frame_renderer: &mut regia_render::FrameRenderer = todo!();
let queue = regia_render::VideoFullscreenQueue::default();
frame_renderer.enable_video_fullscreen(&queue);
app.insert_resource(regia_media::frame_source::FullscreenVideos(queue));
```

A picture uploads once per new frame; a closed or skipped player (`video.skip`) leaves the screen. With an empty
queue the pass draws nothing, so the frame is byte-identical. `media.state` reports the queue as `fullscreen`
(`players`, `queued`, `drawn`, `uploads`, `over_limit`).

## Agent methods

| Method | Class | What it does |
|---|---|---|
| `media.state` | read | Per player: `state`, texture `width`/`height`/`serial`, `frame_time` of the shown picture, `fallback`, `stall_ticks`, `position`, `duration`, `looping`, `audio_clock`, `ring` (`queued`, `bytes`, `peak_bytes`, `budget_bytes`, `max_frames`, `full_waits`, `starved_polls`, `dropped_by_seek`), `controls_applied`, `controls_pending`, `last_control_error`, `publishes`, `gpu_submits`, `ui_nodes`, `materials`; plus the GPU queue (`gpu`). `{ "installed": false }` without `VideoFramePlugin`. |
| `media.play` | play | Queue play (from `ended`, restart at 0). |
| `media.pause` | play | Queue pause. |
| `media.seek` | play | Queue an accurate seek to `time` seconds (clamped to the clip). |
| `media.set_loop` | play | Queue `loop: true` or `false`. |

The play methods use the same queue as the UI widgets and reply `{ player, queued: true, pending }`; the control
applies in the next update. A player must have been published once (one update after it is spawned) before
controls reach it.

### Recipe: verify a seek is frame-exact

```json
{ "method": "video.create_player", "params": { "source": "movies/intro.webm", "name": "wall", "clock": "lockstep", "autoplay": false } }
{ "method": "play.step", "params": { "frames": 1 } }
{ "method": "media.seek", "params": { "player": "wall", "time": 1.5 } }
{ "method": "play.step", "params": { "frames": 1 } }
{ "method": "media.state", "params": { "player": "wall" } }
```

`frame_time` is the presentation time of the first picture at or after 1.5 s, `state` is `paused`, and
`video.frame_hash` hashes the same picture.

### Errors

`F0002` with `data.reason`:

| Reason | Meaning |
|---|---|
| `media_not_installed` | No `VideoFramePlugin` in this app. |
| `media_player_not_found` | No video player with that entity id or name, or it has not been published yet. |
| `media_control_queue_full` | 32 controls are already waiting for this player; let an update run. |

`media.state` also reports per-control failures as `last_control_error` (for example `video_bad_state` for a
play on a failed player, `video_not_skippable` for a skip of a fullscreen player marked not skippable). Material
bindings report `video_material_missing_video_slot`.

## Limits

- The material path converts on the CPU (one conversion per new picture, on the main thread; span
  `video_convert_cpu`). Feeding the GPU-converted texture to material slots needs a renderer change
  (a GPU copy into a `DynamicTexture`); the UI can already use the GPU view.
- 8, 10 and 12-bit 4:2:0, 4:2:2 and 4:4:4 are converted; HDR transfer is not tone-mapped (specs/50 § 11).
- With `VideoOverlayPlugin`, fullscreen `layer >= 0` draws above the UI (`VIDEO_OVERLAY_ORDER` 100; the UI
  is order 0) and `layer < 0` stays in the frame-graph pass under the UI. Without that plugin every
  fullscreen player stays in the frame-graph pass. `BootMoviesPlugin` plays `[boot] movies` and binds
  `skip_video` (Escape, Space, Enter, gamepad South and Start) when the project has not bound the action.
  See [Boot movies, cues and the decode pool](video-boot-and-cues.md).
