# Video panel

The editor's **Video** panel (`docs/specs/50-video-playback.md`, ADR-2720; panel id `video`, Window ▸ panels):
open a video asset, watch it, scrub it frame by frame, read its streams and decode counters, and edit the boot
movie order. Editor only. It changes **no format**: the boot list is the `[boot] movies` array of `Regia.toml`
(`regia_framework::boot_movies`), written back as it is read.

## Opening

Double-click a `.webm`, `.mkv` or `.video.ron` in the Content Browser, type its project-relative path in the panel,
or call `editor.video_open`. The panel opens it the way a game's `VideoPlayer` does (`open_player_pooled`) on the
`Lockstep` clock, so seeks are frame exact and what you see is what the runtime decodes. Pictures reach egui through
the CPU frame path (`VideoPlayerState::frame_rgba`).

A file that cannot play leaves the panel in an **error state** (shown in red, `state: "error"`): a missing file, a
damaged container, a refused codec, a full decode pool. It never panics.

## Transport

Play, Pause, previous and next frame, and a frame slider. Seeking is accurate: seeking to frame `k` shows frame `k`.
A frame past the last clamps to the last; a time before 0 clamps to 0. Stepping past either end stays on the end.

## Information

- **Stream**: video codec, size, frame rate, duration, container, every audio track (codec, channels, rate,
  language, default flag) and every caption or subtitle track with the cues read so far (cues are read as the
  decoder reaches them, so step or play to the end to see them all).
- **Decode stats**: decode time per packet (moving average, ms), frames presented, dropped (never on this clock),
  corrupt, packets read, and the decode pool (`in_use/max`, `MediaSettings::max_decoders`).

## Boot movies

The panel lists `[boot] movies` of the project's `Regia.toml` in play order, checks that each entry opens (red code
beside the ones that do not), and reports where the `skip_video` binding comes from:

- `project`: `config/input.ron` binds the action,
- `default`: nothing binds it, so `BootMoviesPlugin` adds Escape, Space, Enter and the gamepad South and Start,
- `unreadable`: `config/input.ron` exists but does not parse; the binding is unknown (`skip_binding_ok: false`).

Up, Down, Remove and Add edit the list; **Save** rewrites the `[boot]` table and keeps every other key of the
manifest. A list that did not change is never written, so the file keeps its bytes (and comments). A save that
changes the list re-serializes the manifest: comments and key order are not preserved.

## Agent methods

All four answer the same state object (`editor.video_state`'s):

```json
{ "method": "editor.video_open", "params": { "path": "movies/intro.webm" } }
→ { "open": true, "source": "movies/intro.webm", "state": "paused", "frame": 0, "total_frames": 120,
    "position_s": 0.0, "duration_s": 4.8,
    "info": { "container": "webm", "codec": "V_AV1", "width": 1920, "height": 1080, "fps": 25.0, "duration_s": 4.8,
              "total_frames": 120,
              "audio": [ { "index": 1, "codec": "A_OPUS", "language": "eng", "channels": 2, "sample_rate": 48000,
                           "default": true } ],
              "text": [ { "index": 2, "codec": "S_TEXT/UTF8", "language": "eng", "default": true,
                          "cues": [ { "start_s": 0.0, "end_s": 0.4, "text": "hello" } ] } ],
              "markers": [] },
    "stats": { "decode_ms": 1.2, "frames_presented": 1, "frames_dropped": 0, "frames_repeated": 0,
               "frames_corrupt": 0, "packets_read": 3, "queue_depth": 0, "decode_too_slow": false },
    "pool": { "in_use": 1, "max": 4 },
    "boot": { "movies": [ { "index": 0, "path": "movies/studio.video.ron", "problem": null } ],
              "skippable_after": null, "dirty": false, "skip_binding": "default", "skip_binding_ok": true } }
```

| Method | Params | Does |
|---|---|---|
| `editor.video_open` | `path` | Opens the asset; fails with a `video_panel_*` code and keeps the error state. |
| `editor.video_seek` | at most one of `frame`, `time`, `step`; optional `transport` (`play`/`pause`), `ticks` (0..=3600 steps of 1/60 s); at least one param | Moves the playhead; `frame` clamps to the last frame. |
| `editor.video_state` | none | The state above; before anything is open, `open: false`. |
| `editor.video_boot` | optional `move` (`[from, to]`), `remove` (an index), `add` (a project-relative `.webm`, `.mkv` or `.video.ron`), `save` (default false) | The boot list's Up / Down, Remove, Add and Save, applied in that order (ADR-7307). Without `save` only the panel's list changes (`boot.dirty`). |

### Errors

`data.reason` is one of: `video_panel_no_project`, `video_panel_bad_path` (not a relative `.webm`, `.mkv` or
`.video.ron` inside the project), `video_panel_not_open`, `video_panel_bad_index`, `video_panel_no_manifest`,
`video_panel_manifest_invalid`, `video_panel_io_failed`, `video_panel_open_failed` (the file did not open and
the player recorded no error), or the media code prefixed: `video_panel_source_not_found`,
`video_panel_container_invalid`, `video_panel_container_unsupported`, `video_panel_codec_unsupported`,
`video_panel_codec_patented`, `video_panel_limit_exceeded` (the decode pool is full), `video_panel_bad_state`,
`video_panel_seek_out_of_range`, `video_panel_decode_failed`. Malformed params are `invalid_params`.
After a failed open, `editor.video_seek` answers the open error until a file opens.
