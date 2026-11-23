# Video encoding (AV1 and Opus in WebM)

Regia Engine writes compressed video by itself: AV1 pictures and Opus sound in a WebM file, with no
external tool (ADR-3369, specs/50 § 14). Two ways in:

- **Render-out** `format: "webm"`: a `render.out_start` job (or a `.render_job.ron`) encodes its frames
  while it renders, and its audio mixdown becomes the Opus track. See [Render-out](render-out.md).
- **`regia media encode`**: a frames directory or a `.y4m` file (plus an optional `.wav`) to a `.webm`,
  for boot movies, cutscenes and trailers. The command then runs the cook's video validator on the
  result, so a file it writes is one the cook accepts.

Encoding is for render-out and tools only: a shipped game never encodes (ADR-1593 § 1). The encoders
sit behind the Cargo feature `media-encode`:

| Crate | Feature | What it adds |
|---|---|---|
| `regia_media` | `media-encode` (off) | `regia_media::encode`: `rav1e` (AV1, Rust) and `unsafe-libopus` (Opus) |
| `regia_agent` | `media-encode` (off) | render-out format `webm` |
| `regia` | `media-encode` (off) | both of the above; `regia_dylib` forwards it |
| `regia_cli` | `media-encode` (**on**) | `regia media encode` |

Render-out needs `regia_agent/media-encode` or the forwarding `regia/media-encode` feature.
Enabling only `regia_media/media-encode`, including through another workspace member, does not
compile the agent's writer: `render.out_start` still returns `media_encode_unavailable` (ADR-4199).

A game that renders trailers from inside the app turns it on in its dev builds:

```toml
regia = { workspace = true, features = ["agent", "media-encode"] }
```

Without the feature, `render.out_start` with `format: "webm"` and `regia media encode` answer
`media_encode_unavailable`. The cook never transcodes: it copies and validates `.webm` files
([Video playback](video-playback.md)).

## Settings

The same settings block everywhere: `encode` in `render.out_start`, `output.encode` in a render job,
flags of `regia media encode`. Every key is optional.

| Key | Range | Default | Meaning |
|---|---|---|---|
| `quality` | 1..=255 | 80 | Constant quality: the AV1 quantizer, lower is better and larger. Used when `bitrate_kbps` is 0. |
| `bitrate_kbps` | 0 or 50..=200000 | 0 | Target video bitrate, single pass, instead of constant quality. |
| `speed` | 0..=10 | 6 | Speed preset: 0 is slowest and smallest, 10 fastest. |
| `keyframe_interval` | 1..=600 | 240 | Longest run of frames between keyframes (the encoder may add one on a scene cut). Each keyframe opens a cluster with a cue point, so seeking lands on it. |
| `audio_kbps` | 16..=510 | 128 | Opus bitrate. |
| `threads` | 0..=64 | 0 | Encoder threads. 0 picks half the cores (1..=8). **1 is deterministic**: two encodes of the same input are byte-identical. |

Pictures must be at least 16 pixels a side, even in both dimensions (4:2:0 halves them), at most 8192
a side and 8192 x 4352 pixels, at 1..=240 fps. Sound must be mono or stereo at 8000, 12000, 16000,
24000 or 48000 Hz (render-out's default mixdown is 48 kHz stereo).

What is written: AV1 8-bit 4:2:0 in BT.709 limited range with the sRGB transfer (the colors of an 8-bit
render-out frame), Opus in its CELT-only low-delay mode (2.5 ms of pre-skip), a SeekHead, Info with
the exact duration, Tracks with `CodecPrivate` (`av1C`, `OpusHead`), `CodecDelay`, `SeekPreRoll` and
`Colour`, one cluster per keyframe, Cues, and `DiscardPadding` on the last Opus packet so the sound is
exactly as long as the input. Timestamps are in milliseconds (the WebM convention).

## Render-out

```json
{"jsonrpc":"2.0","id":1,"method":"render.out_start","params":{
  "out":"renders/trailer","name":"trailer","width":1920,"height":1080,"fps":24,"frames":480,
  "format":"webm","audio":true,
  "encode":{"quality":60,"speed":4,"keyframe_interval":48}}}
```

The job writes `renders/trailer/trailer.webm` and, with `audio`, `trailer_audio.wav` beside it (the
mixdown the Opus track was made from). The encoder runs on its own thread behind the same bounded
queue as the `ffmpeg` formats, so the app thread never encodes; encoded pictures are spooled to
`trailer.webm.av1.part` while the job runs. When the job ends (done, cancelled or failed) the encoder is
drained and the movie written: **a cancelled job leaves a complete, playable `.webm` of every frame
delivered before the cancel**, like the `ffmpeg` formats. A process killed mid-job leaves no `.webm`,
only `.part` files. `render.out_start` echoes the settings under `request.encode` (with `threads`
resolved). A take of a render job writes its own `.webm` without sound (the job's WAV is on the
timeline folder), as for the other movie formats.

`format: "webm"` refuses `bit_depth` 16 and `aovs` like the other movie formats. `encode` with another
format is `invalid_params`.

## `regia media encode`

```sh
regia media encode renders/trailer/ renders/trailer/trailer_audio.wav -o movies/trailer.webm --fps 24 --quality 60
regia media encode intro.y4m -o movies/intro.webm --speed 4 --keyframe-interval 48 --json
```

Pictures: a directory of PNG frames (sorted by file name; render-out's `<name>_000000.png` frames work
as they are) at `--fps`/`--fps-den`, or a YUV4MPEG2 file (8-bit 4:2:0; its header sets the rate). An
optional `.wav` (PCM 16/24/32 or float 32) becomes the Opus track. Flags: `--quality`,
`--bitrate-kbps`, `--speed`, `--keyframe-interval`, `--audio-kbps`, `--threads`, `--json`. The
command prints what it wrote and the validator's verdict; `--json` prints
`{ "encode": {...}, "validate": {...} }`. The `.webm` then goes into the project like any video.

## Error codes

Render-out answers `F0002` with `data.reason` set to one of these; the CLI prints `<code>: <message>`.

| Code | When |
|---|---|
| `media_encode_unavailable` | The build has no encoder (feature `media-encode` off). |
| `encode_bad_size` | A side below 16 pixels (or zero). |
| `encode_odd_size` | An odd width or height. |
| `encode_too_large` | Over 8192 a side or 8192 x 4352 pixels. |
| `encode_bad_rate` | A frame rate of 0 or above 240 fps. |
| `encode_bad_quality` | `quality` 0. |
| `encode_bad_bitrate` | `bitrate_kbps` outside 0 or 50..=200000. |
| `encode_bad_speed` | `speed` above 10. |
| `encode_bad_keyframe_interval` | `keyframe_interval` 0 or above 600. |
| `encode_bad_threads` | `threads` above 64. |
| `encode_audio_format` | `audio_kbps` outside 16..=510, more than two channels, or a sample rate Opus does not take. |
| `encode_bad_input` | An unknown `encode` key or a wrong type, a malformed `.y4m` or `.wav`, frames of different sizes, no pictures. |
| `encode_io` | A file could not be read or written. |
| `encode_failed` | The encoder failed. |

## How it is checked

`crates/regia_media/tests/encode.rs` (CPU only, `--features media-encode,video`) encodes 48 pattern
frames at 128x72 and reads them back through Regia's own demuxer and decoders: every picture decodes,
packet and picture times and the declared duration are exact, the worst luma PSNR at quality 80 is
above the stated floor, a higher quality gives a higher PSNR and a bigger file, 2 s of stereo sound
decodes to within one Opus packet of the input, `threads: 1` is byte-identical run to run, the file
passes the cook validator, an unfinished encode leaves nothing and an early finish plays, and hostile
settings are refused with their codes. `crates/regia_media/src/mux.rs` tests the streaming writer
(cues, seeks, a live file read up to its last closed cluster), and `regia_cli`'s
`commands::media_encode` tests encode a PNG directory plus a WAV and a `.y4m` and run the validator.
