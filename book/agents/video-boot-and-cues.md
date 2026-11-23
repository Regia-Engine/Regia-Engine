# Boot movies, cues and the decode pool

This page covers the parts of video playback (spec 50, ADR-2410) that sit around the player itself:
startup movies and the `skip_video` action, fullscreen video above the UI, pausing decode while a player
is not seen, text cues on the caption board, the decode-pool limit and thermal throttling, and the
import and cook of `.webm` files. The player, its clocks and the `video.*` methods are on
[Video playback](video-playback.md); textures, `UiVideo` and `media.state` are on
[Video on screen](media-render.md).

## Boot movies

List the movies in `Regia.toml`; they play in order, fullscreen above the UI, before the first level is shown:

```toml
[boot]
movies = ["movies/studio.video.ron", "movies/intro.webm"]
skippable_after = 1.5   # seconds of each movie before skip_video works (default 1.0)
```

Paths are project paths (resolved under the project root like `video.open`). An absent table or an empty
list means no boot movies. A table that does not parse is logged and ignored: a typo never stops the game
from starting.

Add `regia_framework::boot_movies::BootMoviesPlugin` after `regia_media::MediaPlugin` (the framework's
`media` feature; the `regia` umbrella's `media` feature turns it on). Level loading keeps running on other
threads; a level reveal waits on the run condition `regia_media::boot_movies_done`.

- Each movie gets its own entity `boot_movie_<index>` with a `VideoPlayer` (autoplay, never loops,
  always decodes) and a `VideoFullscreen` at layer `BOOT_MOVIE_LAYER` (1000).
- A movie that does not open (missing file, bad codec) is recorded as `failed` with its code, and the next one starts.
- **Skip.** The `skip_video` action (default bindings: Escape, Space, Enter, gamepad South and Start, in
  the consuming `Video` context that is on while a boot movie or a skippable fullscreen video plays) sends
  `regia_media::SkipVideo`. It skips the current movie once it has played `skippable_after` seconds of
  media time; an earlier press is refused (`boot_skip_too_early` in the log, `refused_skips` in
  `media.boot_state`) and forgotten. The same message skips every other fullscreen player with
  `skippable: true` (`Ended { skipped: true }`). A project that binds `skip_video` itself keeps its own
  bindings; the defaults are only added when the action is bound nowhere.

```json
{"method": "media.boot_state"}
→ {"installed": true, "movies": ["movies/studio.video.ron", "movies/intro.webm"], "skippable_after": 1.5,
   "done": false, "current": {"index": 1, "player": 4294967301, "position": 0.4},
   "records": [{"index": 0, "source": "movies/studio.video.ron", "outcome": "played", "code": null}],
   "refused_skips": 0}
{"method": "media.boot_skip"}
→ {"queued": true, "boot": true}
```

Verify a boot sequence: set `clock: Lockstep` on the sequence in a test host, `step` until
`media.boot_state` shows `done: true`, and read `records` (order, outcomes).

## Packaged boot list (v0.5.1, ADR-7893)

`regia package` stores `[boot]` in `build_info.json` beside the desktop executable or at the APK
assets root. With no loose manifest, `BootMoviesPlugin` reads that object. The precedence is an
explicit `BootMovies` resource or plugin config > loose manifest > package > no movies. An empty
loose list disables the package list; a malformed loose manifest warns and disables it. Missing
metadata is harmless; malformed metadata warns and uses no movies. Android keeps the extracted
media root, so a packaged `movies/intro.webm` opens without a loose `Regia.toml`. Desktop packages
resolve these paths under their staged `assets/` directory.

Put packaged movies under the project's `assets/` directory and use asset-relative entries such
as `movies/intro.webm`. To use that same list with a loose development manifest, insert
`MediaFiles::with_root(project_root.join("assets"))` before adding the boot plugin. The plugin
preserves an explicitly configured media root; it does not rewrite movie paths.

`media.boot_state.configuration` is `{ source, location }`, also present when `installed` is
false. `source` is `code`, `manifest`, `packaged`, `default`, or `invalid`. `location` names the
manifest, build info path, or `apk:assets/build_info.json`; code/default sources have no location.
No new protocol errors are introduced. To check a package, call `media.boot_state`, verify its
origin and list, then use `wait_until` or fixed `step` calls until `done`, and inspect `records`
for `played` or an intentional `skipped` outcome.

## Fullscreen above the UI

A `VideoFullscreen` with `layer >= 0` covers the UI; one with `layer < 0` draws under it. Add
`regia_media::overlay::VideoOverlayPlugin` (after `VideoFramePlugin`): it installs the window overlay layer
`video_above_ui` at order 100 (the UI is order 0) and the publish step sends `layer >= 0` players to it.
The frame-graph fullscreen pass keeps drawing the `layer < 0` ones under the UI. A letterboxed picture
paints its bars black, so nothing of the UI shows through. Without the plugin every fullscreen player
draws in the frame-graph pass, under the UI, as before.

## Pausing decode while not seen

`VideoVisible(bool)` on the player entity says whether its surface is seen; the host, a culling system
or `media.set_visible` writes it (no component means visible). The player's `visibility` policy decides:

| Policy | Invisible player |
|---|---|
| `Always` | Decodes as usual. |
| `PauseDecode` (default) | Stops pulling, presenting and uploading pictures; the clock keeps running. When it is seen again it re-seeks to the current media time and presents the frame the presentation rule picks there, so its frame index is the one a never-hidden player shows on that tick. Pictures discarded by the re-seek are not counted as drops. |
| `PauseAll` | Stops the clock too; on return it continues from the same media time. |

Fullscreen players always decode. `video.status` and `media.decoders` show `decode_paused`.

```json
{"method": "media.set_visible", "params": {"player": "wall_screen", "visible": false}}
→ {"player": 4294967299, "visible": false, "policy": "PauseDecode"}
```

## Text cues

Text tracks (`S_TEXT/UTF8`, `S_TEXT/WEBVTT` and the other `S_TEXT/*` ids) are demuxed while the clip
plays. When playback enters a cue the player emits `VideoEvent::TextCue { track, start, end, text }`
(raw UTF-8; `end` from the block duration, else the next cue of the track, else the clip end) and when
it leaves one, by time, a seek, a loop wrap or a close, `TextCueEnd { track, start }`. `MediaPlugin` routes
them to the caption board through the public caption messages (`regia_app::caption::CaptionStart` with
source `Video`, the track's language and the cue's length, then `CaptionStop`); add
`regia_ui::captions::CaptionPlugin` to show them. Formatting and styling are the caption board's
(spec 56). A clip keeps at most 4096 cues of at most 4096 bytes of text each.

```json
{"method": "media.cues", "params": {"player": "intro"}}
→ {"player": 4294967297, "cues": [{"track": 2, "start": 1.0, "end": 3.0, "text": "Long ago..."}],
   "active": [{"track": 2, "start": 1.0}], "dropped": 0}
```

## Decode pool and thermal throttling

`MediaSettings::max_decoders` (4 on desktop, 2 on Android) bounds the players decoding at once
(opening, playing, seeking or buffering and not paused by visibility or thermal policy; ready, paused,
ended and closed players do not decode). The check runs at open: an open while the pool is full
fails with `OpenFailed { code: "limit_exceeded" }` (`video_limit_exceeded` on the wire, message
"decode pool full"); closing a player frees its slot. `max_players` (8) still bounds open players.

With `MediaSettings::thermal_throttle` (on by default) the player reads `regia_platform::ThermalState`:
at `Moderate` non-fullscreen players present every other tick, at `Severe` and above `PauseDecode`
players also stop decoding; fullscreen players keep full rate and the first severe frame logs a warning.
`thermal.set_override` forces a reading on a desktop.

```json
{"method": "media.decoders"}
→ {"max_decoders": 4, "max_players": 8, "in_use": 2, "decode_paused": 1, "thermal_throttle": true,
   "holders": [{"player": 4294967297, "state": "playing", "decode_paused": false, "half_rate": false}, ...]}
```

## Import and cook

The cook reads `.webm` and `.mkv` files without transcoding. Each file is validated as a whole:
every top-level element must fit inside the file, every packet must demux without a skipped element, the
clip needs a video track with packets, and the codec rule holds. A royalty-free codec without a decoder
in the cooking build (AV1 in a build without `media-codecs`) is accepted. The cooked payload
(`regia_media::CookedVideo`, version 1) is the clip report followed by the file byte for byte at a 4 KiB
offset. A `.webm` without a `.video.ron` next to it gets `<stem>.video.ron` with the defaults.
`.video.ron` and `.videolist.ron` cook by parsing (a future `format_version` fails). `.mp4`, `.mov`,
`.avi` and the other foreign containers are refused.

Cook errors start with their code:

| Code | Cause |
|---|---|
| `container_invalid` | Truncated or damaged file (an element runs past the end, malformed elements, no video packet). |
| `container_unsupported` | Another container family (`.mp4`, `.mov`, `.avi`, transport streams). |
| `codec_patented` | A patent-pooled codec on a video or audio track: re-encode to AV1 with Opus or Vorbis in WebM. |
| `codec_unsupported` | An unknown video codec id. |

`media.validate { source }` runs the same checks from a running game or editor:

```json
{"method": "media.validate", "params": {"source": "movies/intro.webm"}}
→ {"source": "movies/intro.webm", "file": "movies/intro.webm", "valid": true,
   "report": {"container": "webm", "bytes": 52344, "duration_ns": 2000000000, "width": 1280, "height": 720,
              "frame_rate": 30.0, "cue_count": 2, "markers": 0,
              "tracks": [{"index": 0, "kind": "video", "codec_id": "V_AV1", "language": "und", "packets": 60}],
              "packets": 60, "last_packet_ns": 1966000000}}
```

## Errors

All are `F0002` with `data.reason`:

| Reason | Methods | Meaning |
|---|---|---|
| `media_not_installed` | `media.boot_skip`, `media.decoders` | No `MediaPlugin` in this app. |
| `media_player_not_found` | `media.set_visible`, `media.cues` | No video player with that id or `Name`. |
| `video_source_not_found`, `video_container_invalid`, `video_container_unsupported`, `video_codec_patented`, `video_codec_unsupported`, `video_io_failed` | `media.validate` | The file's own refusal (see the table above); `video_io_failed` when the file cannot be read. |

## Not covered yet

- The GPU-converted texture into material `video` slots (materials use the CPU copy); it needs the
  frame owner's dynamic-texture seam.
- Automatic visibility from camera frustums and UI node visibility: the host writes `VideoVisible`.
- Queueing players past the decode pool by priority (spec 50 § 2.4): the extra open is refused instead.
