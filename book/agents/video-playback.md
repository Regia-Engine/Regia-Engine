# Video playback

`regia_media` plays recorded video: a studio intro, a cutscene, a looping screen on a wall. It reads
WebM / Matroska files with an in-house demuxer, decodes through an isolation seam that checks hard
limits before every decoder call, keeps picture and sound on one clock, and exposes every player to
agents through `video.*` (specs/50, ADR-1593, ADR-1670).

**This version** ships the codec-independent player: demuxer, codec rule, isolation seam, state
machine, clocks (including the exact `Lockstep` rule), seek, loop, playlists, the CPU YUV reference and
frame hashes, the PCM feed to the mixer with its bus source, and the agent surface. It decodes the two reference codecs
(raw I420 video `V_UNCOMPRESSED`, float PCM audio `A_PCM/FLOAT/IEEE`) that the fixture muxer writes.
With the `regia` feature `media-codecs` it also decodes AV1 video and Opus and Vorbis audio
([Video codecs](video-codecs.md), ADR-1869); without it those files are refused with
`video_codec_unsupported` and a hint naming the feature. `VideoFramePlugin` publishes the picture
as a `DynamicTexture`. `video.frame_hash` still hashes the CPU reference conversion. Fullscreen
drawing is on [Boot movies, cues and the decode pool](video-boot-and-cues.md) and
[Video on screen](media-render.md): with `VideoOverlayPlugin`, `layer >= 0` draws above the UI and
`layer < 0` stays in the frame-graph pass under the UI.

| Method | Permission | What it does |
|---|---|---|
| `video.probe` | read | Container, duration, tracks (codec id, language, size, color, rate, channels, codec-rule verdict), cue count, chapter markers, `supported` and `reject_reason`. No player needed. |
| `video.list` | read | Every player (id, source, state, position, duration) and the last 256 events with sequence numbers. |
| `video.status` | read | One player: state, position, duration, frame index, rate, loop, clock, audio track, counters, last error. |
| `video.frame_hash` | read | FNV-1a 64-bit hash of the presented frame as RGBA8, or of a `region { x, y, w, h }`. |
| `video.open` | play | Open a source on a player: `source`, `autoplay?`, `clock?`, `phase_offset?`, `loop?`. |
| `video.play` / `video.pause` | play | Play or pause. |
| `video.seek` | play | `time` (seconds) or `frame`, `mode` `accurate` (default) or `keyframe`. |
| `video.step` | play | Present exactly `frames` more pictures of a paused player. |
| `video.set_rate` | play | Forward rate `0.25..=4.0`; `0` pauses. |
| `video.set_loop` | play | `mode` `off`, `loop` or `count` (with `count`, the total number of plays). |
| `video.set_volume` | play | Linear volume `0..=4`. |
| `video.select_track` | play | `kind: "audio"`, `index`, or `null` for no audio. `null` mutes until a later index select and, when the clock is `audio`, switches it to `game`. Reopening a player that was already open emits `tracks_changed`. A component `audio_track` of `None` still uses the default track. |
| `video.skip` | play | Close and report `Ended { skipped: true }`, like the `skip_video` action but whatever the player's `skippable` setting (a test and debug tool, ADR-5638). |
| `video.close` | play | Close; the frame shows the clear color. |
| `video.create_player` | edit | Add a `VideoPlayer` to `entity` or to a new entity called `name`. |
| `video.codecs` | read | The allowed codecs and this build's decoders ([Video codecs](video-codecs.md)). |

`player` is an entity id or a `Name`. Without `MediaPlugin`, `video.list` answers
`{ "installed": false, "players": [] }`. Open, play, pause, seek, step, the setters, skip, close,
`video.create_player`, `video.status` and `video.frame_hash` refuse with `video_not_installed`. `video.codecs` does not
look at the world. `video.probe` still opens the source and returns `video_source_not_found` or
`video_container_unsupported`, not `video_not_installed`.

## Open a clip

```json
→ video.create_player { "source": "movies/intro.video.ron", "name": "intro", "clock": "lockstep", "autoplay": false }
← { "entity": 4294967301 }
→ video.open { "player": "intro", "source": "movies/intro.video.ron", "clock": "lockstep", "autoplay": false }
← { "state": "ready" }
```

A `.video.ron` names the file and its defaults:

```ron
(format_version: 1,
 file: "movies/intro.webm",
 variants: { "android": "movies/intro_720.webm" },
 audio_bus: "sfx", volume: 1.0, loop_mode: Off, preload: Stream,
 clear_color: (0.0, 0.0, 0.0, 1.0))
```

`variants` keys are `linux`, `windows`, `android`, `macos` (any case). A `VideoPlayer.source` may also
point straight at a `.webm` or `.mkv`. Paths are project-relative; `..` and absolute paths are refused
(`video_source_not_found`).

`volume` and `loop_mode` in the `.video.ron` apply when the component is still at its defaults
(`volume` 1.0 and `loop_mode` Off). An explicit 1.0 or Off cannot name a different asset value.
`preload: WholeFile` reads the file into memory and refuses a file over 64 MiB (32 MiB on Android)
before allocating. `preload: FirstSecond` uses the same streaming reader as `Stream`.

`.videolist.ron` is `(format_version: 1, entries: ["a.video.ron", ...], loop_list: false, shuffle: None)`.
Opening it plays entry 0, then the next entry when the current one ends. `loop_list: true` returns to
the start and does not emit `list_ended`. `shuffle: Some(seed)` plays every entry once per round in a
seeded order. An entry that fails to open emits `open_failed` and is skipped; if every entry fails the
player is `failed`. An empty list fails to open (`invalid container: empty playlist`).

## Verify a frame exactly (lockstep)

With `clock: "lockstep"` media time advances by exactly one fixed step per rendered frame
(`MediaSettings::lockstep_dt_ns`, 1/60 s by default), times the rate, in integer nanoseconds. Step `n`
shows the picture with the greatest presentation time at or before `n * dt * rate`; nothing depends on
wall clock, threads or the audio device, and no frame is ever dropped. So a recorded hash is a stable
assertion:

```json
→ video.seek { "player": "intro", "frame": 48 }
← { "state": "paused", "target": { "frame": 48 } }
→ video.frame_hash { "player": "intro" }
← { "hash": "9c1f4e0a5b2d7731", "width": 1280, "height": 720, "frame_time": 2.0, "source": "cpu_reference" }
```

`video.step { frames: 30 }` on a paused player presents 30 more pictures. While playing, use the
engine's `step` and `wait_until` with `video.status` (`state == "ended"`, `position >= 2.0`).

`accurate` seeks land on the first picture at or after the target (equal to decoding from the start);
`keyframe` lands on the cue point at or before it. Give `time` or `frame`, not both: both is
`invalid_params` and nothing is seeked. A negative `time` clamps to 0; a time past the end clamps
to the last picture. A negative `frame` is `invalid_params` (`frame` must not be negative) and is
not clamped. A `frame` past the last frame is `video_seek_out_of_range`. Frame seeks need a
constant frame rate.

## States and events

`closed`, `opening`, `ready`, `playing`, `paused`, `seeking`, `buffering`, `ended`, `failed`. A `play`
during `opening` is remembered; `ended` keeps the last picture and `play` restarts from 0; `failed` is
terminal until the next `open`. `video.list` returns the recent events: `opened`, `open_failed`,
`closed`, `ended` (with `skipped`), `looped` (with `count`), `list_ended` (a playlist ran out), `buffering_started`,
`buffering_done`, `seeked`, `paused`, `resumed`, `tracks_changed`, `marker` (a chapter start), `text_cue` /
`text_cue_end` ([text cues](video-boot-and-cues.md)), `failed`. Events are emitted in `PostUpdate` sorted by entity
id; one player's events keep the order they happened in (a skip is `ended` then `closed`), so runs agree (ADR-5638).

Loops are seamless: `Loop` over a 10-frame clip in lockstep shows `0..9, 0..9, 0..4` over 25 steps;
`count: 2` plays twice and then ends.

One tick reports at most four `looped` events: a tick that crosses more wraps (a very short clip, a long tick)
reports the last four, and `count` is the running total (ADR-7563). A file that declares a duration under 1 ms is
`video_container_invalid`; a declared duration shorter than one frame period is ignored and the length comes from the
stream's end.

## Sound

A clip's audio track is decoded on the decode thread into a lock-free ring and played by a mixer stream on the
`.video.ron`'s `audio_bus` (default `sfx`, matched without case, so the default layout's `Sfx` bus). A missing bus
plays on `Master` with an `audio_bus_missing` warning. With `clock: "audio"` the picture follows the frames the
output actually mixed from that stream, so a stalled or suspended device freezes the video with it. An empty ring
plays silence and counts `stats.underruns`. `video.set_volume` changes the stream volume at once.

The stream follows the transport: while the player is not `playing` (paused, seeking, buffering, ready, ended)
it plays silence and takes nothing from the ring, so the audio decoded before a pause plays after it. An
`accurate` seek drops the audio between the cue point and the target. The rate resamples the sound (pitch
follows) between `0.5` and `2.0`; outside that range the sound is muted and a `clock: "audio"` player follows
game time until the rate returns.

A player that comes back into view after a decode pause (`VisibilityPolicy::PauseDecode`, or `PauseAll` on resume) seeks accurately to its clock:
the audio before the clock is dropped and the old picture stays until the picture due at the clock is decoded
(ADR-7563). While it waits, `stats.frames_repeated` counts every tick that holds the old picture, so a slow decoder
shows a burst of repeats after each un-hide: that is the landing, not stutter. The ring holds half a second plus the
audio of the pictures decoded ahead, so slideshows and time-lapses under 8 fps keep all their sound, and never more
than 8 MiB of samples (ADR-7635: a hostile rate and channel count cannot size a huge ring; every 48 kHz layout up to
8 channels is under it). A clip whose video track declares no `DefaultDuration` gets the 4 s ring, since its frame
rate is unknown until it plays (KI-AU-028, ADR-10070). Audio that still does not fit logs one `video audio feed full`
warning. A float PCM track
with no Audio element plays as mono 8000 Hz (the Matroska defaults).

## Shipping clips

Media is never cooked into paks: `.webm`, `.mkv`, `.video.ron` and `.videolist.ron` files under `assets/` ship
as raw files at the same paths. `regia package` also writes `media_index.ron` (`regia::PACKAGED_MEDIA_INDEX`,
format version 1) listing each one with its length and an FNV-1a stamp. On Android the runtime extracts exactly
those paths next to the paks at startup, skips files whose stamp is unchanged, and deletes extracted media the
index no longer lists, because an APK assets directory cannot be walked (ADR-5441).

## Diagnose a rejected file

```json
→ video.probe { "source": "movies/trailer.webm" }
← { "container": "webm", "supported": false, "reject_reason": "video_codec_patented",
    "tracks": [{ "kind": "video", "codec_id": "V_MPEG4/ISO/AVC", "verdict": "patented", ... }] }
```

Regia Engine plays royalty-free formats only: WebM / Matroska containers, AV1 (VP9 later) video, Opus or
Vorbis audio. H.264, H.265, AAC, MP3 and other pooled codecs are refused with `video_codec_patented`;
`.mp4`, `.mov`, `.avi` and transport streams with `video_container_unsupported`. Re-encode to AV1 in
WebM.

The demuxer treats files as untrusted: every length is checked against the file and a 64 MiB
allocation cap; a malformed element is skipped or the reader resynchronizes at the next cluster; a
truncated final cluster just ends playback early. The isolation seam drops a packet the decoder
rejects (`stats.frames_corrupt` counts them; decoding resumes at the next keyframe) and fails the
player after 30 rejected packets in a row or 2 s of media (`video_decode_failed`). Pictures over the
size cap (3840x2160 desktop, 1920x1080 Android) are `video_limit_exceeded`, as is a ninth open player.

## Error codes

All are `F0002` with `data.reason`:

| Reason | Meaning |
|---|---|
| `video_not_installed` | No `MediaPlugin` in this app. |
| `video_player_not_found` | No player with that id or name. |
| `video_source_not_found` | The file or `.video.ron` does not exist, or the path leaves the project. |
| `video_bad_state` | Not valid now (play while closed, seek while opening, step while playing, frame seek on a variable-rate clip). |
| `video_container_invalid` | Damaged or truncated EBML/Matroska, encrypted tracks, no video track, a declared duration under 1 ms. |
| `video_container_unsupported` | MP4/MOV/AVI/TS/FLV or an unknown DocType. |
| `video_codec_unsupported` | A codec without a decoder in this build (VP9 in every build, which has no decoder yet; AV1, Opus and Vorbis without the `media-codecs` feature), or an unknown codec id. |
| `video_codec_patented` | A patent-pooled codec. |
| `video_track_not_found` | No audio track with that index. |
| `video_rate_unsupported` | A rate outside `0.25..=4.0`, or negative. |
| `video_seek_out_of_range` | A frame seek past the last frame. |
| `video_limit_exceeded` | Picture size, open players, a full decode pool (ADR-2410), playlist length, a `WholeFile` preload over its cap, or the allocation cap. |
| `video_decode_failed` | Too many corrupt packets, no decodable picture, or a decoder bug. |
| `video_io_failed` | Reading the file failed. |
| `video_audio_bus_missing` | The audio bus is missing (warning; master is used). |
| `video_material_missing_video_slot` | A material without a `video` texture parameter (renderer milestone). |

`explain_error` gives the fix hint for each.

## Benchmark a decoder

`cargo run -p regia_media --release --example decode_bench -- [file.webm] [frames]` decodes through the
isolation seam and prints one RON line: decoder, size, frames, average and worst decode time per
picture, CPU reference conversion time, the largest picture, and the specs/50 § 10 budget for that size
(22 ms for 1080p30 on the min-spec CPU). Without a file it decodes a synthetic 1080p raw clip. The
benchmark lane runs it on the target devices to pick each codec's decoder (ADR-1593 § 2).

## Video on a sequence's clock

A sequence can play a video on its own clock, fullscreen, on a screen in the world or in a UI node, frame-exact
in render-out: see video sections in [Sequence players](sequence-player.md). Its session is an ordinary player
entity on the `External` clock, so `video.status` and `video.frame_hash` read it; the sequence owns its transport.

