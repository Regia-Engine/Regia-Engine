# Spec 50: Video playback (`regia_media`)

Status: proposed (ADR-1579). Target rung: v0.4.0 (G11).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: none for video. Used as-is: `regia_render::upload_queue` (`UploadQueue`), `regia_render::render_out`,
`regia_audio` (`Buses`, offline backend in `backend.rs`, kira mixer), `regia_ui::node::UiImage`.

Source area: runtime-media/runtime-media-playback (primary), runtime-media/runtime-media-overview (vocabulary only).

## 1. Purpose and scope

Games need recorded video: a studio intro that plays while the first level loads, a cutscene between
levels, a looping screen on a wall, a clip inside a menu. This spec adds a **video player** that opens a
file, decodes picture and sound in software, keeps them in sync, and presents the picture as a streamed
GPU texture that the UI, a material, or a fullscreen overlay can show, with the sound routed into the
audio mixer.

What it is not:

- Not a streaming client. Network sources, adaptive bitrate, live capture devices and broadcast I/O are
  out of scope (§ 11). Files come from the virtual file system (`regia_asset`: loose files, paks,
  Android assets).
- Not an authoring or recording tool. Capturing the game to a file is the render-out path
  (`regia_render::render_out`); this spec only guarantees playback can run in lockstep with it (§ 7).
- Not a subtitle system. Dialogue and subtitles belong to spec 56; this spec only surfaces an optional
  text-cue hook (§ 2.8).
- Not the cinematic timeline. The sequencer may drive a player's clock (§ 2.5) but owns its own tracks.

**Codec rule (hard).** Regia ships and decodes only royalty-free formats:

| Layer | Allowed | Rejected at open with `codec_patented` |
|---|---|---|
| Container | WebM, Matroska subset (`.webm`, `.mkv`) | MP4/MOV family, transport streams |
| Video | AV1 (required), VP9 (optional, § 11) | H.264, H.265, and any other patent-pooled codec |
| Audio | Opus, Vorbis | AAC, MP3 variants of pooled codecs, anything proprietary |

Decoders may be written in C, C++, assembly or unsafe Rust (ADR-1593), chosen on quality, speed, power and
platform fit, and must be permissively licensed (§ 9). A **software decoder is the reference** for every
codec: it is mandatory for deterministic paths (render-out, golden tests, `Lockstep`). A platform hardware
decoder (Vulkan video, Media Foundation, VideoToolbox on the Mac test box, MediaCodec on Android) is an
optional playback path where the platform exposes it and wgpu can import the output without a CPU
readback; it falls back to software and never adds a codec the software path rejects (§ 9, § 11). Demuxers
and muxers stay Rust-only. Patent exposure of the royalty-free set below, including the AV1 patent grant,
is a counsel question before commercial release (`docs/07-licensing.md`), not a code question.

## 2. Regia terms and model

### 2.1 Nouns

| Noun | Kind | Meaning |
|---|---|---|
| `VideoSource` | asset (`.video.ron`) | Locator and playback defaults for one clip (§ 4). New. |
| `VideoList` | asset (`.videolist.ron`) | Ordered list of sources with advance rules. |
| `VideoPlayer` | component | One open session: source handle, requested state, rate, loop, volume, clock mode, visibility policy. |
| `VideoPlayerState` | component (runtime, `#[reflect(ignore)]`) | Machine state, position, counters. Never persisted. |
| `VideoFrame` | GPU resource | The streamed RGBA8 texture (sRGB view) a player writes; one per player. |
| `VideoFullscreen` | component | Marks a player whose frame is drawn fullscreen with a fit mode (§ 2.7). |
| `VideoEvent` | event | State and stream notifications (§ 3.4). |

One entity holds one player. A source can be shared by many players; each opens its own session and its
own decoder.

### 2.2 State machine

States: `Closed`, `Opening`, `Ready`, `Playing`, `Paused`, `Seeking`, `Buffering`, `Ended`, `Failed`.
`Seeking` and `Buffering` remember whether to return to `Playing` or `Paused`.

| From | Trigger | To | Event / effect |
|---|---|---|---|
| Closed | `open` | Opening | Reader and demuxer start on the decode pool. |
| Opening | headers parsed, first frame decoded | Ready | `Opened`; with `autoplay` the player goes straight on to Playing. |
| Opening | any open error | Failed | `OpenFailed { code }`. |
| Ready, Paused | `play` | Playing | Rate becomes the stored rate (default 1.0). |
| Playing | `pause` | Paused | Clock freezes; audio feed drains to silence in at most 20 ms. |
| Playing, Paused | `seek` | Seeking | Coalesces: a newer target replaces an older pending one. |
| Seeking | target frame decoded and presented | previous | `Seeked { time }`. |
| Seeking | driver `reposition(t)` (ADR-8831) | previous | The pending target is dropped; `Seeked { time: t }`; the feed seeks to `t` only. |
| Playing | frame queue empty and packets not yet available | Buffering | `BufferingStarted`; clock freezes. |
| Buffering | queue holds at least `resume_frames` | Playing | `BufferingDone`. |
| Playing | last frame presented, loop off | Ended | `Ended` exactly once; texture keeps the last frame. |
| Playing | last frame presented, loop on | Playing | `Looped { count }`; position wraps seamlessly (§ 2.6). |
| Ended | `play` | Playing | Equivalent to seek(0) then play. |
| any except Closed | `close` | Closed | Decoder stopped, queues freed, texture cleared (§ 2.7). `Closed` event. |
| Playing, Paused, Buffering | unrecoverable decode error | Failed | `Failed { code }` (§ 2.9). |
| Failed | `open` or `close` | Opening / Closed | Failed is terminal until reopened. |

Edge cases:

- A `play` request made while `Opening` is remembered and sets `autoplay` for this session, so a script
  that opens and plays in one frame starts playback when the clip is ready. `play` in `Closed` or `Failed`
  returns `bad_state`.
- `open` on an open player closes the old session first and reuses the entity.
- Zero-length or one-frame clips are valid: `Ended` fires after that frame is presented.
- A rate of `0.0` is pause; `play` with rate `0.0` is rejected with `rate_unsupported`.
  As built (ADR-5904): a component rate edit to `0.0` on a player with nothing to pause (closed, ended, failed)
  does nothing and warns nothing; an edit from `0.0` back to a forward rate does not resume (call `play`).

### 2.3 Tracks and metadata

A clip has at most one video track used per session and any number of audio tracks and text tracks
(WebM subtitle/caption/metadata tracks). `probe` and `Opened` report per track: kind, codec id, language
tag, default flag, duration, and for video width, height, display aspect, frame rate (nominal and
variable-rate flag), bit depth, chroma format, color primaries/transfer/matrix/range; for audio channels
and sample rate. Selection:

- Audio: default is the track flagged default, else the first whose language matches
  `MediaSettings.language`, else the first. The compare is the BCP-47 primary subtag plus a fixed
  ISO 639-1 / 639-2 table (`eng` matches `en` and `en-US`; `fra` and `fre` match `fr`; `deu` and `ger`
  match `de`). `und` matches nothing. `MediaSettings.language` defaults to `system_locale()` (for
  example `en-US`), or `eng` when the locale is missing, `C`, or `POSIX`.
- `VideoPlayer.audio_track: None` uses that default rule. `video.select_track` with `index: null`
  mutes (no audio feed) until a later index select. When the component clock is `Audio`, that null
  select switches the clock to `Game`. `Game`, `External` and `Lockstep` are left as set.
- Extra video tracks and unsupported tracks are ignored with one `warn!` per track; the clip still plays.
- Switching the audio track reopens the session at the current position. When the player was already
  open and the reopen does not fail, `TracksChanged` is queued (`video.list` kind `tracks_changed`).
- Chapters/cue points are exposed read-only as markers by time; the sequencer glossary term **Marker** is
  reused (`VideoEvent::Marker { name }` when playback crosses one).

### 2.4 Limits

| Limit | Default | Notes |
|---|---|---|
| Max picture size | 3840x2160 desktop, 1920x1080 on Android and when `SteamDeck=1` | Larger fails open with `limit_exceeded`. `DecodeCaps::for_host` takes the two flags; `for_platform` reads `SteamDeck`. |
| Max open players | 8 | Ninth `open` fails `limit_exceeded`. |
| Max concurrently decoding players | 4 desktop, 2 Android | An open past the cap is refused with `limit_exceeded` (ADR-2410). `priority` is stored and not read. Players do not queue. |
| Decoded frame queue | 4 frames (max 8) | Memory bound per player (§ 10). |
| Packet queue | 1.0 s or 8 MiB, whichever first | Demuxer reads ahead to this and stops. |
| Playlist length | 4096 entries | |
| Decode threads (total) | `(cores / 4)` clamped to `1..=4` on desktop, 2 on Android | Shared pool, not per player. The desktop ceiling constant is 4. A spent pool still gives a decoder its own calling thread. |
| Consecutive corrupt packets | 30 or 2 s of media | Then `Failed` (§ 2.9). |

As built (ADR-5638): there is no separate read-ahead packet queue; the demuxer reads packets on demand for the frame
queue and the audio ring, so memory is bounded by the frame queue, the ring budget and the 64 MiB allocation cap.

As built (ADR-7563): a nonzero declared `Duration` under 1 ms is `container_invalid` at open (zero still means "not
declared"), and a player ignores a declared duration shorter than the video track's `DefaultDuration`, learning the
length from the stream's end as for a file that declares none. Each player's PCM ring holds half a second plus the
audio of its decode-ahead window (`queue_frames` + 2 pictures at the clip's frame period), at most 4 s; audio that
still does not fit is counted and warned about once per session. As built (ADR-10070): a clip with no `DefaultDuration`
gets the 4 s ring (its frame rate is unknown until it plays).

### 2.5 Clocks, sync, rate

Each player has a **media time** `t` (seconds from clip start) driven by a clock mode:

| Mode | Master clock | Use |
|---|---|---|
| `Audio` | Frames consumed by the audio output (the audio clock, § 3.3) | Set by the caller. Opening does not switch to `Audio` because a track exists. |
| `Game` (the `VideoPlayer` default) | Game time delta times rate | Silent loops, world screens, and a player muted with `select_track` index null while its clock was `Audio` |
| `External` | A time written each frame by the sequencer or a script | Timeline-locked footage |
| `Audio` (default when an audio track is active) | Frames consumed by the audio output (the audio clock, § 3.3) | Normal playback |
| `Game` (default with no audio) | Game time delta times rate | Silent loops, world screens |
| `External` | A time written each frame by the sequencer (video sections, ADR-2862, through `PlayerCore::show_at`, which also rewinds) or a script | Timeline-locked footage |
| `Lockstep` | Render clock, one fixed step per rendered frame | Render-out, tests (§ 7) |

Presentation rule, every frame in `Update`: compute `t`; present the decoded frame with the greatest
presentation time `<= t + video_latency` (default latency 0). If the next frame's time is also `<= t`,
that frame is **late**: skip to the newest due frame and count `frames_dropped`. If no frame is due, the
previous one stays (`frames_repeated` counts consecutive holds beyond one frame period). Dropping never
happens in `Lockstep`. If the lead of the oldest queued frame over `t` exceeds 3 frame periods, the
video thread sleeps (decode throttling); if the queue is empty and `t` passes a frame time, the player
enters `Buffering` once the starve lasts more than `starve_ms` (default 100) so a short hiccup does not
flicker events.

Audio is master because humans hear a gap before they see a dropped frame. If the audio device stalls
or is suspended (Android pause, device change), the audio clock stops and the video freezes with it; on
resume nothing jumps. With audio present, a drift correction is unnecessary by construction: video follows
the clock, audio never resamples to chase video.

Rate: supported forward rates `0.25 ..= 4.0`. For `Audio` mode the feed time-stretches by resampling
(pitch changes) between `0.5 ..= 2.0`; outside it audio is muted and the mode switches to `Game` until
the rate returns. Reverse playback is unsupported (`rate_unsupported`); scrubbing by seek is the
substitute. `step_frames(n)` on a Paused player presents exactly `n` further frames (n may be negative
only through seek); it works in every clock mode.

Frame-exact positioning: `seek { time }` and `seek { frame }` (frame index by nominal rate, valid only
for constant-frame-rate clips; otherwise `bad_state` with hint) use `SeekMode::Accurate` (default:
land on the cue point at or before the target, decode forward discarding output until the first frame
with time `>=` target, present it) or `SeekMode::Keyframe` (present the cue point itself, fastest). The
catch-up decode runs within a per-frame budget (default 8 ms of decode, spread over frames); the old
frame stays visible and the state is `Seeking` until done. A target past the end clamps to the last
frame; negative targets clamp to 0 (no error). Clips without cue data seek by scanning forward from the
start (slow, one `warn!`).

### 2.6 Loop, playlist, start offset

- `LoopMode { Off, Loop, Count(u32) }`. A looping clip prepares its first frames (a second decoder
  context on the same reader) while the last second plays, so the wrap shows no stall; audio is trimmed
  or padded with silence to the **video track duration** so the loop length is the video's. The loop
  point is exact in `Lockstep`; in real time it is accurate to one frame period.
- `phase_offset` (seconds, default 0) lets several players of one source begin at different phases; the
  initial seek happens during `Opening`.
- A `VideoList` plays entries in order; opening a list begins at entry 0. When an entry reaches
  `Ended`, the next one opens on that update. There is no separate one-second prefetch. When the last
  entry ends, the list emits `ListEnded` once. With `loop_list` it returns to entry 0 and does not emit
  `ListEnded`. `shuffle: Some(seed)` orders a round with a seeded RNG (§ 7) without repeating an entry
  until all have played. An entry that fails to open emits `OpenFailed` and is skipped; if every entry
  fails, the player is `Failed`. An empty list fails to open.
- Looping and shuffle are independent of the clip: nothing loops a player whose clock mode is `External`.

### 2.7 Presentation

The decoder produces planar YUV (4:2:0 / 4:2:2 / 4:4:4, 8 or 10 bit). The renderer's **video convert**
pass uploads the planes (one `R8`/`R16` texture per plane) through the existing upload queue and writes
RGBA8 (sRGB-encoded) into the player's `VideoFrame` texture, using the matrix and range in the
clip's color metadata (default BT.709, limited range when absent; BT.601 and BT.2020 non-constant
luminance are supported). The transfer characteristic is not applied, so HDR is not tone-mapped (§ 11). The texture is double-buffered: the convert pass
writes the back texture and the player swaps on present, so a sampler never sees a half-updated frame.
The texture has one mip level, clamp addressing, linear filtering, and its size is the coded display size
(not padded). While `Closed` or before the first frame it holds the player's `clear_color` (default
black). Consumers:

- **UI.** `regia_ui` adds a component `UiVideo { player: Entity, fit: Fit }` next to `UiImage` (whose `id` is a
  renderer image id, not a source enum). Each frame a system asks the `VideoFrameSource` trait (§ 3, defined in
  `regia_render`) for the player's current texture, registers it with the UI renderer and writes the id into the
  node's `UiImage`; layout uses the clip's display aspect (fit, fill, stretch). Menu transport widgets bind to the player's actions
  (`play`, `pause`, `seek`) as ordinary buttons/sliders.
- **Material.** A material texture parameter of kind `video` binds a player. A mesh using it shows the
  same frame the UI shows. The editor's "Show on mesh" action on a video asset creates an unlit material with
  that parameter and a new player on the target entity. Replacement materials must declare a `video`
  parameter or the binding is rejected with `material_missing_video_slot`.
- **Fullscreen.** `VideoFullscreen { fit: Letterbox | Fill | Stretch, layer: i32, skippable: bool }`
  draws with `VideoOverlayPlugin`: `layer >= 0` is a window overlay above the UI and `layer < 0` stays
  in the frame-graph pass under the UI. Without that plugin every fullscreen player stays in the
  frame-graph pass. Several fullscreen videos are ordered by `layer`. `skippable` listens for the `skip_video` input action;
  a skip closes the player and fires `Ended { skipped: true }`. A **startup movie** list is a project setting
  (`[boot] movies = [...]` in `Regia.toml`): they play in order on the first presented frames while level
  loading runs on other threads, and the level is shown only after the list ends and loading completes.
- Stereo-packed frames are out of scope (§ 11).

Visibility: `VisibilityPolicy { Always, PauseDecode, PauseAll }`. With `PauseDecode` (default for a
player on a non-fullscreen entity) a player whose surface is outside every camera frustum, beyond
`max_distance`, or whose UI node is hidden stops decoding and uploading; its clock keeps running (in
`Audio`/`Game` modes) and on becoming visible it performs an accurate seek to the current time. `PauseAll`
freezes the clock too. Fullscreen players are always `Always`.

As built (ADR-7563): the accurate seek drops the decoded audio before the current time, and the picture shown before
the pause stays until the picture due at the clock is known (a later picture or the end has arrived), so an older
picture never flashes up; a decoder that has not landed after 120 ticks presents the newest due picture instead.

### 2.8 Text-track hook

Text tracks (WebVTT-in-Matroska or plain-text cues) are demuxed, not rendered. `VideoEvent::TextCue
{ track, start, end, text }` fires when playback enters a cue, with the raw UTF-8 text; ending is
`TextCueEnd`. Formatting, localization and display are spec 56's concern; nothing in `regia_media`
draws text.

### 2.9 Error handling for bad streams

Open-time errors fail fast with a code (§ 5.3): unreadable file, bad EBML header, no video track,
unsupported or patented codec (the message names the codec id and says "re-encode to AV1 in WebM"),
dimensions over the limit, truncated headers. After open, the demuxer and decoders must treat the
bitstream as untrusted:

- A malformed container element is skipped by its declared size when the size is plausible (bounded by
  remaining file length); otherwise the demuxer resynchronizes at the next cluster ID. All length
  fields are bounds-checked before allocation (no allocation larger than 64 MiB from file data).
- A packet the decoder rejects is dropped; decoding resumes at the next keyframe/sync point. The last
  good frame stays on screen; `frames_corrupt` increments; one `warn!` is logged per second at most.
- Audio errors mute the audio track for that packet (silence, clock keeps advancing in `Audio` mode
  because the feed pads with silence), never stall video.
- The player becomes `Failed` after 30 consecutive rejected packets or 2 s of media, and on I/O errors
  that persist for three retries. A truncated final cluster is not an error: playback ends early.
- No panic path for stream content: Rust decoders run under `catch_unwind` inside the decode task purely as
  a last guard; a caught panic counts as an unrecoverable decode error (`decode_failed`) and is a bug.
  Native (C/C++) decoders cannot be caught: hard caps (dimensions, packet size, frame-queue bytes, a time
  budget per packet) are enforced before every FFI call, and a decoder error marks the player `Failed`
  (ADR-1593 § 4). A native abort ends the process under `panic=abort`; desktop builds evaluate a
  child-process decoder (frames through shared memory) and adopt it if § 10 still holds, otherwise the
  limit is recorded.

## 3. API shape in Rust

Crate `regia_media`, **layer 2** (systems), depends on `regia_core`, `regia_ecs`, `regia_app`,
`regia_asset` and `regia_render` (the allowed layer-2 edge). It does not depend on `regia_audio`
(layer 2 crates do not depend on each other, `docs/02-architecture.md` § 1); the audio seam is two small
types in `regia_core` (§ 3.3). `regia_ui` never names `regia_media`: `UiVideo` (§ 2.7) reads frames through a trait in `regia_render`
(`VideoFrameSource`, implemented by `regia_media`), which keeps `regia_ui` on its allowed `render` and `input` edges;
`regia_agent` depends on `regia_media` (layer 3, new listed edge for `xtask check-layers`). Neither
`regia_editor` nor `regia_asset_pipeline` is depended on.

### 3.1 Modules

`source` (assets, RON), `container` (WebM/Matroska demux behind `trait Demuxer`), `codec` (`trait
VideoDecoder`, `trait AudioDecoder`), `clock`, `queue` (packet and frame queues), `player` (state
machine, systems), `present` (render hand-off), `list`, `agent_glue` (data types for `regia_agent`),
`reference` (CPU YUV to RGB and frame hashing, used by tests).

### 3.2 Key types

```rust
#[derive(Asset, Reflect, Serialize, Deserialize, Clone, Debug)]
pub struct VideoSource { pub format_version: u32, pub file: AssetPath, pub variants: BTreeMap<String /* linux | windows | android | macos */, AssetPath>,
    pub audio_bus: String, pub volume: f32, pub loop_mode: LoopMode, pub preload: Preload, pub clear_color: [f32; 4] }

#[derive(Component, Reflect, Clone, Debug)]
pub struct VideoPlayer { pub source: Handle<VideoSource>, pub autoplay: bool, pub rate: f32, pub volume: f32,
    pub loop_mode: LoopMode, pub clock: ClockMode, pub phase_offset: f64, pub audio_track: Option<u32>,
    pub visibility: VisibilityPolicy, pub priority: i32, pub spatial: bool }

pub enum ClockMode { Audio, Game, External, Lockstep }
pub enum SeekMode { Accurate, Keyframe }
pub enum Preload { Stream, FirstSecond, WholeFile }   // WholeFile reads the file into memory (<= 64 MiB, 32 MiB on Android). FirstSecond uses the streaming reader, the same as Stream.

pub trait Demuxer: Send { fn probe(&mut self) -> Result<MediaInfo, MediaError>;
    fn next_packet(&mut self) -> Result<Option<Packet>, MediaError>;
    fn seek(&mut self, to: Duration) -> Result<Duration /* cue landed on */, MediaError>; }
pub trait VideoDecoder: Send { fn decode(&mut self, p: &Packet) -> Result<Vec<DecodedPicture>, MediaError>;
    fn flush(&mut self); }
pub trait AudioDecoder: Send { fn decode(&mut self, p: &Packet, out: &mut Vec<f32>) -> Result<(), MediaError>; fn flush(&mut self); }

pub struct VideoCommands<'w> { /* SystemParam: open, play, pause, seek, step, set_rate, close, select_audio */ }
pub fn probe(reader: impl Read + Seek) -> Result<MediaInfo, MediaError>;   // no player needed (editor, agent, cook)
```

`MediaError` is a `thiserror` enum whose variants map one-to-one to the protocol codes of § 5.3 and carry
the asset path and track. `VideoPlayerState` exposes `state()`, `position()`, `duration()`, `frame_index()`,
and a `VideoStats` struct (`frames_presented`, `frames_dropped`, `frames_repeated`, `frames_corrupt`,
`packets_read`, `queue_depth`, `decode_ms_ema`, `underruns`).

As built (ADR-5638): `MediaError` variants carry a message naming the codec, limit or path where it is known, not
structured asset-path and track fields.

### 3.3 Audio seam (new, in `regia_core::media_seam`)

```rust
pub struct PcmFeed { /* single-producer single-consumer lock-free ring of f32, interleaved stereo or mono at the mix rate */ }
impl PcmFeed { pub fn new(channels: u8, capacity_frames: usize) -> (PcmProducer, PcmConsumer); }
pub struct AudioClock { /* Arc<AtomicU64> frames consumed + sample rate; Clone */ }
#[derive(Resource)] pub struct MediaAudioRegistry { /* feeds registered by regia_media, drained by regia_audio */ }
```

`regia_media` creates a feed per player with an audio track, resamples decoded audio to the mix rate on a
decode thread, and pushes into the ring (capacity 500 ms). `regia_audio` gains a **bus source** that
registers each consumer on the named bus (`VideoSource::audio_bus`, default a bus named `sfx`; a missing
bus falls back to the master bus with a `warn!`), mixes it on the audio thread (no allocation, silence on
underrun and a counted `underruns`) and applies the player's volume with the bus chain. `spatial`
is stored and not applied: the mixer does not pan and does not use an `AudioEmitter`. `regia_audio` publishes the
`AudioClock` as a resource: the device backend increments it per callback by the frames it consumed from
the output buffer; the offline backend increments it by the frames it renders. Both ends live behind
the `Audio` facade so gameplay keeps using one resource.

### 3.4 Events and plugin

`VideoEvent` (ECS event, entity-tagged): `Opened { info }`, `OpenFailed { code }`, `Closed`,
`Ended { skipped }`, `Looped { count }`, `ListEnded`, `BufferingStarted`, `BufferingDone`, `Seeked { time }`,
`Paused`, `Resumed`, `TracksChanged`, `Marker { name }`, `TextCue`, `TextCueEnd`, `Failed { code }`.
Events are emitted during `PostUpdate` in a fixed order (by entity bits, then in the order above) so tests
and replays agree.

As built (ADR-5638): within one player, events keep the order they happened in (a skip is `Ended { skipped: true }`
then `Closed`), which is as deterministic as the list order. `Opened` carries no payload; `video.probe` and
`video.status` give the track information.

As built (ADR-7563): one tick reports at most 4 `Looped` events. A tick that crosses more wraps (a very short clip, a
long tick) reports the last four; `count` is still the running total, so the last event of the tick carries it.

`MediaPlugin` registers the assets and their loaders, the reflection types, the player systems
(`PreUpdate`: collect commands and decoder results; `Update`: clock and frame selection; `PostUpdate`:
events, render hand-off), the decode pool resource `MediaPool`, and the agent glue. All types derive
`Reflect`; `VideoSource`, `VideoList` and the component fields also derive `Serialize`/`Deserialize`.
`MediaSettings` (resource) holds the limits of § 2.4 and per-tier defaults.

## 4. Files and formats

**Media files.** `.webm` (preferred) and `.mkv`; EBML/Matroska parsed with the subset of elements in
§ 2.3 plus Cues, SimpleBlock, BlockGroup and Tags; lacing is accepted, encryption elements are rejected
(`container_invalid`). The importer does not transcode; it probes and records.

**`.video.ron`** (asset `VideoSource`):

```ron
(format_version: 1,
 file: "movies/intro.webm",
 variants: { Android: "movies/intro_720.webm" },
 audio_bus: "sfx", volume: 1.0, loop_mode: Off, preload: Stream,
 clear_color: (0.0, 0.0, 0.0, 1.0))
```

`volume` and `loop_mode` apply at open when the component is still at 1.0 and `Off`. Any other component
value wins. An explicit component volume of 1.0 or loop `Off` cannot override the asset. `priority` and
`spatial` are stored and not read.

**`.videolist.ron`**: `(format_version: 1, entries: ["a.video.ron", ...], loop_list: false, shuffle: None)`.

Both follow the canonical-RON rules of spec 23 § 1. Project paths resolve through `regia_asset`; a path
outside `Movies/` (the conventional folder) is accepted (the pak includes any referenced file) but the
importer logs an informational note so the file is not forgotten by hand-packaged builds.

**Import and cook.** Dropping a `.webm` creates a `.video.ron` and a `.meta` (probe result cached: duration,
size, frame rate, track list, codec ids, cue count). Cook validates every referenced file with
`regia_media::validate`: after the probe, every top-level element must fit in the file, and every packet
is demuxed (no decode and no transcode). A skipped element or a resync fails with `container_invalid`.
The pass stops at 50,000,000 packets. The cook fails with the protocol codes on error, and copies the
file byte-for-byte into the pak, aligned to 4 KiB, stored uncompressed (already compressed content).
Files with a rejected codec fail the **import**,
not just the cook, with the re-encode hint; `.mp4`, `.mov`, `.avi` and similar extensions are refused with
`container_unsupported`. No encoder ships in v0.4.0; an optional tool-side encode wrapper
(`regia_asset_pipeline`, desktop only) is a follow-up (§ 11).

**Versioning.** `format_version` starts at 1. A future v2 ships a migrator from v1 registered with the
asset migration table and applied by `regia upgrade-project` (`docs/specs/12-assets.md` § 4a); loaders
reject unknown future versions with an error naming the file. Row for § 4a: `.video.ron` / `.videolist.ron`,
Regia Engine, usable for production from v0.4.0.

## 5. Agent-protocol surface

Player addressed by `player` = entity id or name. Access classes per spec 23: `read`, `play`, `edit`.

### 5.1 Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `video.probe` | read | `source` (asset path or id) | `{ container, duration, tracks: [...], cue_count, supported, reject_reason? }` |
| `video.list` | read | none | `[{ player, entity, source, state, position, duration }]` |
| `video.status` | read | `player` | `{ state, position, duration, frame_index, rate, loop, clock, audio_track, stats, last_error? }` |
| `video.frame_hash` | read | `player`, `region?` | `{ hash, width, height, frame_time }` (hash of the RGBA8 frame texture contents; FNV-1a 64-bit) |
| `video.open` | play | `player`, `source`, `autoplay?`, `clock?`, `phase_offset?`, `loop?` | `{ state }` |
| `video.play` / `video.pause` | play | `player` | `{ state }` |
| `video.seek` | play | `player`, `time` or `frame`, `mode?` | `{ state, target }` |
| `video.step` | play | `player`, `frames` | `{ frame_index, position }` |
| `video.set_rate` | play | `player`, `rate` | `{ rate }` |
| `video.set_loop` | play | `player`, `mode`, `count?` | `{ loop }` |
| `video.set_volume` | play | `player`, `volume` | `{ volume }` |
| `video.select_track` | play | `player`, `kind`, `index` or `null` | `{ tracks }` |
| `video.skip` | play | `player` | `{ skipped }` (same as the skip input action) |
| `video.close` | play | `player` | `{ state }` |
| `video.create_player` | edit | `entity`, `source`, component fields | `{ entity }` (undoable) |

The read-class methods work with the player stopped; the `play` class needs a running session. Property
edits (`rate`, `loop`, `clock`) are also reachable through `set_property`. `step` and `wait_until` use
the engine's fixed step (spec 23 § 3.5), so `step { frames: 30 }` advances 30 render steps; with
`clock: "Lockstep"` that advances exactly 30 steps of media time. As built (ADR-5638): `video.list` answers
`{ installed, players, events }` (the recent events with sequence numbers), and `video.skip` closes the player
whatever its `skippable` flag (the `skip_video` input action honors the flag). `wait_until` conditions can query
`VideoPlayerState` (`state == Ended`, `position >= 2.0`). Events reach agents through the log/event subscriptions of spec 23: `VideoEvent` payloads carry the player id.

### 5.2 Verification recipes (for the guide)

Screenshot a world screen at known media time (`video.open` with `clock: "Lockstep"`, `step`, `screenshot`);
assert `video.frame_hash` against a recorded hash; `wait_until state == Ended` with `step`; kill the audio
device in a test host and check the video freezes (clock stall).

### 5.3 Error codes (lowercase ascii/digits/underscore)

`player_not_found`, `source_not_found`, `bad_state`, `container_invalid`, `container_unsupported`,
`codec_unsupported`, `codec_patented`, `track_not_found`, `rate_unsupported`, `seek_out_of_range`
(only for `frame` seeks past the last known frame), `limit_exceeded`, `decode_failed`, `io_failed`,
`audio_bus_missing` (warning code, never fatal), `material_missing_video_slot`. On the wire each is `F0002` with
`data.reason` = the code prefixed `video_` (for example `video_bad_state`), as neighboring agent modules do; the book
page lists them. `explain_error` returns the long form with the fix hint.

### 5.4 Guide and coverage

Page: `book/agents/video-playback.md` (open a clip, show it on a wall, play a fullscreen intro, verify
with `frame_hash`, diagnose a rejected file). `book/COVERAGE.md` row: `video.* | 50-video-playback |
video-playback.md | probe, play/pause/seek/step lifecycle, rejected-codec and corrupt-stream errors,
lockstep frame hash`.

## 6. Editor surface

Brief; the protocol is primary. The content browser previews a `.video.ron` with transport buttons
(play, pause, scrub, single step), a loop toggle and a frame/time readout. A **clip report** panel shows one
row per track (codec id, language, rate or dimensions); a rejected track carries a reason line, and a "copy
as JSON" action emits the `video.probe` result. The inspector edits `VideoPlayer` through reflection.
"Show on mesh" and "Show on UI node" actions on a video asset create the bound player plus material or
`UiVideo`. Leaving PIE closes any players the editor opened for preview.

## 7. Determinism notes

- **Bit-exact:** AV1 reconstruction is normative in its specification, so a conforming software decoder
  produces identical YUV on every platform for one stream. The software decoder is the reference
  (ADR-1593): `Lockstep`, render-out and golden tests always use it, never a platform hardware decoder; the frame-selection logic is integer arithmetic on
  presentation timestamps in nanoseconds. Given `Lockstep`, frame `n` is presented at media time
  `start + n * dt * rate` in integer nanoseconds (`dt` the fixed step), choosing the greatest frame time
  `<=` that value; nothing depends on wall clock, thread timing, queue depth or the audio device.
- **Lockstep decoding** is blocking: the player waits (on the decode pool) until the needed frame
  exists, never drops frames, never enters `Buffering`, and takes audio samples from the feed in exact
  counts per step (`sample_rate * dt * rate` accumulated with an integer remainder). The same rule serves
  the sequencer (`External`) when its host requests exact alignment.

  As built (ADR-7563): the player never takes audio from its feed; the mixer stream that drains the feed does
  (ADR-1670). The exact per-step counts hold where the mixer itself is stepped: render-out's mixdown renders
  `floor((n + 1) * dt * rate_hz) - floor(n * dt * rate_hz)` frames for step `n`, the rule
  `regia_media::player::lockstep_audio_frames` states (at player rate 1 with feed rate equal to the mix rate the feed
  gives up exactly that many frames). A `Lockstep` player outside render-out (tests, a game stepping a fixed `dt`)
  is drained by the audio device in real time: its pictures stay exact, its audio follows the device.
- **Tolerance-based:** YUV to RGB on the GPU (±1 LSB per channel against the CPU `reference` module);
  Opus and Vorbis output (float decoders may differ in the last bits across targets: compare audio with a
  small RMS tolerance, default -90 dBFS); real-time `Audio` mode drop and repeat counts (they depend on
  load and are never asserted exactly).
- **Ordering:** events (§ 3.4) emit in a fixed order; playlist advance and shuffle use a seeded
  PCG-style RNG whose seed is in the data or the session seed; players decode independently so results are
  independent of pool scheduling.
- **Render-out:** the host sets `clock: Lockstep` on every player, the offline audio backend renders the
  audio, and the output frame's picture equals the player's presented frame for that step (so a recorded
  render can be compared frame by frame with the source).

## 8. Tests and acceptance

Tests that need no GPU use the CPU `reference` path, a synthetic muxed fixture (a tiny WebM generated by
a test-only muxer from a deterministic pattern; real encoded fixtures arrive from an offline encoder
kept out of the repo, plus a few KiB-sized committed clips), and the offline audio backend.

1. **Container probe.** Probing a fixture reports duration, size, frame rate and tracks; a fixture
   whose first video track is H.264 returns `codec_patented`; an `.mp4` file returns `container_unsupported`.
2. **State machine.** Every transition row in § 2.2 has a test; `play` queued during `Opening` takes
   effect; `Ended` fires exactly once; `Closed` clears the texture to `clear_color` (reference read-back).
3. **Lockstep stepping.** With `Lockstep` and a 24 fps fixture at a 60 Hz step, presented frame indices
   over 120 steps equal the table computed by hand from the integer rule; no drops, no repeats beyond the
   rule; repeat runs are bit-identical (frame hash sequence).
4. **Seek.** `Accurate` lands on the exact frame (hash equals decoding from 0 to that frame); `Keyframe`
   lands on the cue point; targets past the end clamp; a seek during a seek keeps only the last target.
5. **Loop.** `Loop` with a 10-frame fixture: the frame hash sequence over 25 steps is
   `0..9,0..9,0..4`; `Count(2)` ends after the second play; `Looped` counts match; a seamless wrap shows
   no `Buffering` event.
6. **Playlist.** Entry order, entry 0 start, `loop_list`, failing entry skipped, seeded shuffle gives the
   same order for the same seed and never repeats before exhaustion.
7. **A/V sync (offline audio).** With the offline backend rendering N audio frames per step, the audio
   clock equals the media clock to within one video frame period at every step; stalling the audio
   clock (no frames rendered) stops presentation; muting the audio track switches to `Game` mode.
8. **Audio routing.** Audio from a fixture with a known tone arrives on the named bus (checked via the
   offline backend's bus render); a missing bus falls back to master and warns; underrun inserts silence
   and bumps `underruns`.
9. **Corrupt streams.** A fuzz-style table: truncated header, truncated cluster, bit-flipped packets,
   absurd element sizes, 10,000 tiny clusters, zero-length tracks: none panics (`cargo test` plus a
   `cargo fuzz` target for the demuxer and decoder, owner-run); decode resumes at the next keyframe; 30
   consecutive bad packets yields `Failed(decode_failed)`; allocations never exceed the 64 MiB cap.
10. **Color.** The CPU reference converts a ramp of BT.709/BT.601 limited/full values to the expected RGB
    within ±1; GPU convert matches the reference within ±1 (**needs GPU**).
11. **Presentation.** A UI image and a material both show the same frame at one step (equal sampled
    pixels) (**needs GPU**); a player outside the frustum with `PauseDecode` performs no uploads and
    resumes at the right frame (counter checks, no wall clock).
12. **Fullscreen.** Letterbox/fill/stretch rectangles computed correctly for 16:9 into 4:3 and 21:9
    surfaces (pure test); `skip_video` closes and reports `skipped: true`; startup movies play before the
    first level frame (ordering test).
13. **Agent protocol.** `cargo test -p regia_agent` round-trips every method of § 5.1, the error codes,
    and the no-plugin degradation; `cargo xtask agent-coverage` and `check-layers` pass.
14. **Android (device).** An intro fullscreen at 720p30 on the reference device (`docs/06` § 8) runs for 60
    s of media time with `frames_dropped` below 5 percent, no ANR, with the app paused and resumed in
    the middle and the video continuing from the same media time (**needs device**).
15. **Goldens.** Golden images for a fixed lockstep frame on a world-screen material: TODO for the lead
    (the builder never creates goldens).

All timing assertions use counters, fixed steps and media time, never wall clock.

## 9. Platform notes

| Platform | Notes |
|---|---|
| Linux, Windows | Software decode (reference); optional hardware path where the platform exposes it; limits of § 2.4. Audio via the existing kira/cpal backend. |
| Android aarch64 | Software decode is the reference; MediaCodec is an optional playback path (§ 11). 2 decode threads total, reserving the other cores for the game; default picture cap 1080p, recommended authoring 720p30 via `variants`. Decode throttles on thermal pressure: first drop to half presented rate for non-fullscreen players, then pause `PauseDecode` players, then (fullscreen intro only) keep decoding but log. Players pause with the app; surfaces are recreated on resume while `VideoFrame` textures (device resources) survive and the last frame is re-presented. Packaged media is not cooked into paks: `regia package` writes a `media_index.ron` (every media file with a length+hash stamp) into the assets root because an APK assets directory cannot be walked; the umbrella extracts exactly the indexed paths into internal storage next to the paks (`<internal>/content/movies/...`, the layout the sources name) and roots `MediaFiles` there, so documented project media paths (`movies/intro.webm`) resolve; media the index no longer lists is pruned, and an Android package carries the index even when it ships no media (an empty list), so a version that drops its last clip prunes too (ADR-4826, amended by ADR-5441 and ADR-5795). `Preload::WholeFile` is rejected above 32 MiB. |
| macOS (testing only) | Same software path. |
| Deck | A process with `SteamDeck=1` uses the handheld picture cap (1920x1080) from `DecodeCaps::for_platform`. Without that variable a desktop build uses 3840x2160. |

**Dependencies and decisions (ADR-1593).** Each gets a `docs/03` row and a passing `cargo deny check`, and sits
behind an off-by-default cargo feature `video` of `regia_media` (the `regia` umbrella enables it in the default
game profile). Licences are permissive only (BSD, MIT, Apache-2.0, ISC, zlib, CC0-1.0, Unlicense; no LGPL or
GPL). A native library's licence and copyright go in `docs/ported-sources.toml` and `THIRD_PARTY_NOTICES.md`.
Native code sits in a `-sys` layer under a safe Rust API, never in `regia_core`, builds with `cc` from
`build.rs` under the NDK clang and `cargo xwin` with no system NASM or Meson, uses stack protection and
`_FORTIFY_SOURCE` (CFG on MSVC), and each decoder has a fuzz target whose corpus runs in CI. The decoder per
codec is chosen by a benchmark lane on the target devices (decode time per frame, power, memory); the rows
below are the candidates, not a fixed choice.

| Purpose | Choice | Licence | Note |
|---|---|---|---|
| AV1 decode | `dav1d` (BSD-2-Clause, C and assembly) or `rav1d` (BSD-2-Clause, its Rust port with unsafe and assembly) | BSD-2-Clause | Benchmark lane picks (ADR-1593). Assembly: arm64 on; x86 needs an explicit decision (a vendored assembler crate, or assembly off on x86). The § 10 budget is measured on the chosen build; if neither cross-compiles to Android aarch64 with the NDK and `cc` alone, the rung stops with an ADR. |
| WebM/Matroska demux | in-house parser (about 800 lines for the § 4 subset) | n/a | Decision (ADR-1585): in-house, since test 9 (untrusted input, 64 MiB allocation cap) is easier to guarantee on code we own. A crate may replace it only if it passes test 9 unchanged. Container parsers stay Rust-only (ADR-1593). |
| Vorbis decode | `lewton` | MIT/Apache-2.0 | Pure Rust; any decoder the benchmark lane prefers may replace it. |
| Opus decode | a permissively licensed native decoder (libopus, BSD-3-Clause) behind the same `-sys` rules, or a Rust decoder once audited | BSD-3-Clause | ADR-1593 removes the pure-Rust requirement, so Opus plays in v0.4.0; the first `regia_media` lane records the choice. `docs/03` § 7 notes that Opus decoding lives in `regia_media` while voice encoding stays game-side. `symphonia` (in the tree via kira, MPL-2.0) is not added as a direct dependency. |
| Resample | the engine's existing mixer resampler | n/a | No new dependency. |

No new GPU dependency: the convert pass is a WGSL shader in `regia_render`.

## 10. Performance budget

Min-spec machine class (`docs/06`): 4 cores, integrated GPU, 8 GiB.

| Item | Budget |
|---|---|
| Frame selection and events (`media_update` span) | < 0.05 ms per player per frame |
| Upload plus convert, 1080p (`video_convert` span, GPU) | < 0.3 ms, one pass per presented frame |
| CPU decode, 1080p30 AV1 8-bit | < 22 ms per frame average on one decode thread of the min-spec CPU, spread across 2 threads; 720p30 on Android big core < 15 ms (`video_decode` span, off the main thread) |
| Audio decode plus resample | < 1 ms per 20 ms of audio (`video_audio` span) |
| Memory per player, 1080p | 4 frames of 3.1 MiB YUV + 2 textures of 8 MiB (RGBA8) + 1 s packets (<= 8 MiB) < 40 MiB |
| Main-thread stall from media | none: no blocking calls outside `Lockstep` |
| Seek catch-up | < 8 ms of decode per frame; 1080p accurate seek completes within 30 frames of a cue |

If the measured decode time is above the frame period for 60 consecutive frames, the player emits one
`warn!` with the clip and the measured cost, so a too-heavy asset is visible in logs.

## 11. Out of scope / follow-ups

- Network and adaptive streaming, DRM, live device capture, professional capture hardware.
- Patented codecs (the shipped set stays AV1, VP9, Opus, Vorbis; a codec the software path rejects is never
  added through a platform decoder); VP9 decode (optional follow-up after AV1 lands); AV1 alpha
  channel. HDR is not tone-mapped: the CPU reference scales YUV to 8-bit RGB from the matrix and range and does not read the transfer characteristic.
- Reverse playback, variable-frame-rate frame-index seeks, stereo packed frames, panoramic projection.
- Crossfade between two players (a shader blend of two video textures). The sequencer video track is
  built (`.seq.ron` v4, ADR-2862).
- Whole-clip precache for large files beyond 64 MiB; an offline encode wrapper in the asset pipeline.
- Cluster-synchronized playback across machines.
- Subtitle rendering and localization (spec 56).
- Platform hardware decoders (Vulkan video, Media Foundation, VideoToolbox, MediaCodec) are in scope as an
  optional playback path per ADR-1593 § 2, built after the software reference passes its benchmark; they need
  wgpu import without CPU readback and are never used by `Lockstep` or render-out.

## 12. Provenance rows

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Video codec | AV1 bitstream syntax, decoding process, normative reconstruction | AV1 Bitstream and Decoding Process Specification, Alliance for Open Media (aomedia.org); AOMedia royalty-free patent license |
| Container | EBML, Matroska elements, cues, WebM profile (VP8/VP9/AV1, Opus/Vorbis only) | Matroska specification (matroska.org, IETF RFC 8794 for EBML) and WebM project container guidelines (webmproject.org) |
| Audio codec | Opus decoder, Opus mapping in Matroska/WebM; Vorbis I decoder | IETF RFC 6716 (Opus), Xiph.Org Vorbis I specification (xiph.org), Opus-in-Matroska codec mapping notes |
| Color | YUV to RGB matrices, limited and full range, chroma upsampling | ITU-R BT.709-6, ITU-R BT.601-7, ITU-R BT.2020-2; ITU-T H.273 code points as used by the AV1 color config |
| Sync | Audio-master clock with video following, drop/repeat selection by presentation time | Public A/V synchronization write-ups (audio-clock-master design); the engine-side rule is original |

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Layer, seam and manifest changes (decisions)

- Decision (ADR-1585): `regia_media` is a new layer-2 crate (`regia_core`, `regia_ecs`, `regia_app`, `regia_asset`,
  `regia_render`). Add it to the `docs/02` layer table and `xtask/src/layers.rs`; `regia_agent` gains the edge
  `regia_agent -> regia_media`. No `regia_media -> regia_audio` exception: the audio seam
  (`regia_core::media_seam`) stays in `regia_core`, since an exception invites more layer-2 coupling.
- Decision (ADR-1585): `regia_ui` reaches video only through the `regia_render` trait `VideoFrameSource`.
- Decision (ADR-1585): `Regia.toml` gains the additive `[boot] movies` array (absent means none). An additive
  optional key needs no version bump; the builder updates `docs/specs/21` § 2.
- The builder reads the real `UploadQueue` and `UiRenderer` image-registration APIs before binding names; the
  names here for those seams are placeholders.
- `regia_audio` needs a bus source that plays a `PcmConsumer`: implement it as a custom kira `Sound` (no allocation
  on the audio thread), additive to the crate.
- Formats to register: the lead runs `cargo xtask check-formats --update` and `cargo xtask manual`.

### Owner decisions

None.

## 14. Offline encoding: AV1 and Opus to WebM (ADR-3369)

The § 11 follow-up "an offline encode wrapper" is built: Regia writes AV1 + Opus WebM files itself, for
render-out and tools only (ADR-1593 § 1), behind the off-by-default `regia_media` feature `media-encode`
(forwarded by `regia`, `regia_dylib` and `regia_agent`; on by default in `regia_cli`).

- **Encoders.** AV1: `rav1e` 0.8.1 (BSD-2-Clause, Rust, `default-features = false` plus `threading`; its
  x86 assembly needs NASM, so it stays off as ADR-1593 § 5 requires). Opus: the `unsafe-libopus` the
  decoder already links, in libopus's restricted low-delay application (CELT only, so no packet can reach
  the SILK concealment path of ADR-1911), VBR, complexity 10. Input pictures are 8-bit 4:2:0; RGBA frames
  are converted with the BT.709 matrix to limited range (center-sited chroma) and tagged BT.709 primaries,
  sRGB transfer. Sound is mono or stereo at 8, 12, 16, 24 or 48 kHz.
- **Muxer.** `regia_media::mux::WebmWriter` (Rust, ADR-1593 § 1 keeps muxers Rust) streams clusters to any
  `Write + Seek`: EBML header, Segment with a SeekHead (Info, Tracks, Cues), Info with `MuxingApp`,
  `WritingApp` and the duration, Tracks with `CodecPrivate` (`av1C`, `OpusHead`), `CodecDelay` (the Opus
  pre-skip), `SeekPreRoll` (80 ms), `DefaultDuration` and `Colour`; a cluster per video keyframe (each with
  a cue point), split when a relative timestamp leaves `i16` or past 32 MiB; Cues at the end;
  `DiscardPadding` on the last Opus block. The Segment size, duration and Cues position are patched at
  `finish`; before that the file reads as a live segment up to its last closed cluster. Timestamps are 1 ms.
  The fixture writer `mux()` is unchanged.
- **Pipeline.** `encode::webm::WebmEncoder` spools encoded temporal units to `<out>.av1.part` while
  pictures arrive (flat memory), and at `finish` encodes the sound, interleaves both by time into
  `<out>.part` and renames it to `<out>`. Dropped without `finish`, it removes its spool: no file.
- **Render-out.** Format `webm` (alias `av1`), settings `encode` (`quality`, `bitrate_kbps`, `speed`,
  `keyframe_interval`, `audio_kbps`, `threads`); the render job carries them as `output.encode` (format v3).
  The encoder runs on its own thread behind the writer queue (`Slots`); the WAV mixdown closes first, then
  becomes the Opus track. Cancel drains the encoder: the file holds every frame delivered before the cancel.
- **CLI.** `regia media encode <frames dir | file.y4m> [file.wav] -o out.webm` with the same settings, then
  `regia_media::validate` (the cook's validator) on the result. The cook still copies and validates video and
  never transcodes.
- **Determinism.** `threads: 1` gives byte-identical files run to run; more threads may change the
  bitstream (the decoded pictures stay close), so deterministic pipelines pin one thread.
- **Not here:** 10-bit or HDR output, 4:4:4, two-pass rate control, VP9, surround sound, x86 assembly
  (`rav1e`'s NASM kernels; an opt-in feature like `av1-nasm` is the follow-up), and a hardware encoder path.
