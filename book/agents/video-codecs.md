# Video codecs

Video playback ([Video playback](video-playback.md)) decodes royalty-free formats only: AV1 video and
Opus or Vorbis audio in WebM or Matroska files (specs/50 § 1). The decoders are optional so a game
that plays no video does not compile them (ADR-1593, ADR-1869).

| Codec | Matroska id | Decoder | Cargo feature |
|---|---|---|---|
| AV1 (8, 10 and 12 bit; 4:2:0, 4:2:2, 4:4:4, monochrome) | `V_AV1` | `rav1d` (the Rust port of dav1d) | `regia_media/av1` |
| Opus (mono, stereo, surround families 1 and 255) | `A_OPUS` | `libopus` 1.3.1 translated to Rust (`unsafe-libopus`) | `regia_media/opus` |
| Vorbis | `A_VORBIS` | `lewton` | `regia_media/vorbis` |
| VP9 | `V_VP9` | none yet (allowed by the codec rule) | none |
| Raw I420 (test fixtures) | `V_UNCOMPRESSED` | `raw` | always built |
| Float PCM (test fixtures) | `A_PCM/FLOAT/IEEE` | `pcm` | always built |

Turn all three on in a game with the `regia` feature `media-codecs` (it implies `media`):

```toml
regia = { workspace = true, features = ["media-codecs"] }
```

`regia_dylib` forwards `media` and `media-codecs`, so hot-reload builds match. Every built-in decoder is
a software decoder, so it is also the deterministic reference: AV1 output is bit-identical on every
platform, which is why `video.frame_hash` in `Lockstep` can be compared across machines. Opus and
Vorbis are float decoders: compare their audio with a small tolerance, not bit for bit (specs/50 § 7).

## Ask a build what it decodes

`video.codecs` needs no player and no `MediaPlugin`:

```json
{ "method": "video.codecs" }
```

```json
{
  "codecs": [
    { "codec_id": "V_AV1", "codec": "av1", "kind": "video", "decoder": "rav1d", "feature": "av1", "deterministic": true },
    { "codec_id": "V_VP9", "codec": "vp9", "kind": "video", "decoder": null, "feature": null, "deterministic": false },
    { "codec_id": "V_UNCOMPRESSED", "codec": "raw_i420", "kind": "video", "decoder": "raw", "feature": null, "deterministic": true },
    { "codec_id": "A_OPUS", "codec": "opus", "kind": "audio", "decoder": "libopus", "feature": "opus", "deterministic": true },
    { "codec_id": "A_VORBIS", "codec": "vorbis", "kind": "audio", "decoder": "lewton", "feature": "vorbis", "deterministic": true },
    { "codec_id": "A_PCM/FLOAT/IEEE", "codec": "pcm_f32", "kind": "audio", "decoder": "pcm", "feature": null, "deterministic": true }
  ],
  "media_codecs": true,
  "av1_assembly": false,
  "av1_thread_budget": 4,
  "containers": ["webm", "mkv"]
}
```

- `decoder: null` means this binary cannot play that codec; `video.probe` then reports
  `reject_reason: "video_codec_unsupported"` and the error hint names the feature to enable.
- `media_codecs` is true when AV1, Opus and Vorbis all decode.
- `av1_assembly` is true when the AV1 decoder runs hand-written assembly: always on aarch64 (Android,
  the Mac), and on x86 with the `media-codecs-asm` feature. It changes speed, never the picture
  (see [Benchmark](#benchmark)).
- `av1_thread_budget` is the number of worker threads all AV1 players share: a quarter of the CPU's
  threads on desktop, clamped to 1..=4 (4 from 16 threads up), and 2 on Android (specs/50 § 9). Each player takes up to two; once they are taken, further players decode on their own
  session thread, so playback never waits for the budget.

## Recipe: check that a clip plays before shipping it

1. `video.codecs`: `media_codecs` is `true`.
2. `video.probe { "source": "movies/intro.webm" }`: `supported` is `true`, the video track's `codec_id`
   is `V_AV1`, and the audio track is `A_OPUS` or `A_VORBIS`.
3. `video.create_player { "source": "movies/intro.webm", "name": "intro", "clock": "lockstep", "autoplay": false }`,
   then `video.open { "player": "intro", "source": "movies/intro.webm", "clock": "lockstep", "autoplay": false }`.
4. `video.step { "player": "intro", "frames": 30 }` then `video.frame_hash { "player": "intro" }`: run the
   same steps on another machine or platform and the hashes match.
5. `video.status { "player": "intro" }`: `stats.frames_corrupt` is `0`.

## What the decoders do with bad data

Decoders run on the decode thread behind the isolation seam: sizes and packet bytes are checked before
every call, a picture over the caps never leaves it, and the AV1 decoder also gets the size cap
internally so an oversized stream is refused before it allocates.

- A corrupt AV1 packet is dropped and decoding resumes at the next keyframe; 30 in a row (or 2 s of
  media) fail the player with `video_decode_failed`.
- A corrupt Opus or Vorbis packet becomes silence of the packet's length, so the audio clock keeps
  moving.
- An Opus packet with a frame of one byte or less (a 1-2 byte packet, a 0-byte frame in a multi-frame
  packet) is refused before the decoder sees it, like a corrupt one: libopus would conceal it as a lost
  frame, and the SILK concealment of `unsafe-libopus` 0.2.0 pins the output at full scale (ADR-1911).
  When a stream switches from SILK or hybrid to CELT-only without the encoder's redundancy frame, libopus
  conceals a SILK transition; the decoder spots that concealment's full-scale output in the 2.5 ms
  crossfade, restarts cold and decodes the packet again (ADR-2306). Streams from a normal encoder decode
  unchanged.
- A bad Opus or Vorbis header (CodecPrivate) leaves the audio track out with a warning; the video
  still plays.

## Audio details

- Opus always decodes at 48 kHz; the feed to the mixer uses the decoder's rate and channel count, not
  the container's. The stream's pre-skip is dropped at the start (again after a seek back to the
  start), and the header's output gain is applied.
- Vorbis decodes at the rate in its own identification header.
- Surround layouts are reordered to the WAVE order (front left, front right, center, LFE, back, side)
  for both codecs.

## Limits

- No VP9 decoder yet, no hardware decoding (ADR-1593 § 2), no AV1 alpha (specs/50 § 11).
- After a seek into the middle of an Opus stream the first 80 ms may differ slightly from a straight
  decode (the decoder starts cold).
- The decoders are Rust, so a decoder bug panics: development builds catch it and fail the player
  with `video_decode_failed`; shipping builds (`panic=abort`) end the process. This is the recorded
  known limit of ADR-1869 § 7.

## Too heavy for the machine

A clip that takes longer to decode than its frame period for 60 pictures in a row sets
`stats.decode_too_slow` in `video.status` for the rest of the session and logs one warning with the
clip path and the measured cost. In `Audio` and `Game` clocks the player then drops pictures, so this
is the signal to author a smaller `variants` entry for that platform. `Lockstep` waits instead and
never drops.

## Benchmark

`cargo run -p regia_media --release --features video --example decode_bench -- movie.webm 300`
prints the decoder name, the average and worst decode time per picture, and whether that fits the
specs/50 § 10 budget (22 ms per 1080p picture on the min-spec CPU).

Plain x86 builds use no assembly, because it needs `nasm` on PATH and a plain `cargo build` must work
without it. That costs a lot:

| Machine, 1080p30, ms per picture | Plain | With `media-codecs-asm` |
|---|---|---|
| Ryzen 9 7950X3D, synthetic clip | 10.8 | 2.9 |
| Ryzen 9 7950X3D, noisy clip | 21.5 | 4.05 |
| Steam Deck, synthetic clip | 21 to 24 | 6.1 |
| Steam Deck, noisy clip | 42.5 | 8.9 |

Build a release that plays AV1 with `media-codecs-asm`; `video.codecs` confirms it with
`av1_assembly: true`. On Android the assembly is always on, and picture cost depends on which cores
the threads land on: pinned to two big cores of a Fairphone 5, 720p30 took 10 to 14 ms; from a plain
`adb shell`, where the threads landed on small cores, it took 30 to 40 ms. Measure phones from a
foreground app.
