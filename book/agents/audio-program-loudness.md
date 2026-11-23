# Program loudness and compressed clips

Two things that ship together (ADR-7227): a **live loudness meter** of the whole output
(`docs/specs/audio-loudness.md` § 6) with 5.1 loudness, and **animation clips compressed at cook**
(`docs/specs/17-animation.md` § 1, the `cooked-clip` v3 format).

## Program loudness

The master limiter's effect carries a program-loudness tap (`regia_audio::program_loudness`). It reads every
output block after the limiter, which is what the player hears, and measures it with the BS.1770-4 K-weighting and
true-peak interpolator of `regia_audio::LoudnessMeter`. Every 100 ms of output it publishes five figures:

| Figure | Meaning |
|---|---|
| `momentary_lufs` | loudness of the last 400 ms |
| `short_term_lufs` | loudness of the last 3 s |
| `integrated_lufs` | gated loudness (−70 LUFS absolute gate, −10 LU relative gate) since the meter was turned on or reset |
| `loudness_range_lu` | EBU Tech 3342 loudness range since then |
| `true_peak_db` | the largest true peak since then (dBTP) |

On the audio thread the tap never allocates and never locks. The offline meter keeps every 400 ms gating block. The
tap keeps a fixed histogram instead: 0.01 LU bins of block count and summed power. The absolute gate is exact. The
relative gate keeps the whole bin that holds its threshold, so a block up to 0.01 LU below the threshold can still
count. On a 1 kHz sine, pink noise and a −20/−40 dBFS level step the live figures match the offline function within
0.01 LU, and the true peak within 0.001 dB (`crates/regia_audio/tests/program_loudness.rs`).

The meter is **off by default**: it costs a K-weighting and a true-peak interpolator per channel per sample. Turning
it on, or resetting it, starts a new measurement at the next audio block. While it is off, a read returns `enabled:
false` and nothing measured.

`audio.program_loudness` (needs `regia_audio::AudioPlugin`, else `UNSUPPORTED` / `F0013`):

```json
{ "method": "audio.program_loudness", "params": { "enable": true } }
→ { "enabled": true, "seconds": 0.0, "momentary_lufs": null, "short_term_lufs": null,
    "integrated_lufs": null, "loudness_range_lu": 0.0, "true_peak_db": -100.0,
    "headless": true, "hop_ms": 100.0 }

{ "method": "audio.program_loudness" }
→ { "enabled": true, "seconds": 5.0, "momentary_lufs": -20.1, "short_term_lufs": -20.1,
    "integrated_lufs": -20.1, "loudness_range_lu": 0.0, "true_peak_db": -20.0, ... }
```

| Param | Type | Meaning |
|---|---|---|
| `enable` | boolean, optional | `true` turns the meter on (a new measurement), `false` turns it off; omitted leaves it as it is |
| `reset` | boolean, optional | start a new measurement before reading (default `false`) |

Both apply before the reading is taken. A loudness figure with nothing measured yet is `null`. Bad params return
`F0002`. A headless app measures what `Audio::render_offline` renders. From Rust, use
`Audio::{program_loudness, set_program_loudness_enabled, reset_program_loudness}`.

Known limit: during a render-out mixdown, `Audio::program_loudness` reads the mixdown mixer's own meter, which
starts off.

### In the editor

The **Audio Mixer** dock has a **Program** row above the buses: bars for M (momentary), S (short-term), I
(integrated) and TP (true peak), the loudness range, the measured seconds and a **Reset** button. While the dock is
open it turns on the meter of the world it pumps: the play world while you play, otherwise the edit world. Switching
worlds starts a new measurement. `editor.audio_mixer_get` reports the same reading under `program` (`enabled`,
`seconds`, the five figures, `null` before anything is measured).

### 5.1 loudness

`LoudnessMeter::new` and `measure_loudness` take 1, 2 or **6** channels. Six are 5.1 in the order L, R, C, LFE, Ls,
Rs, weighted per BS.1770-4 Table 3 (`regia_audio::channel_weights`, `FIVE_ONE_WEIGHTS`): 1.0 for each front
channel, 1.41 (+1.5 dB) for each surround, and 0 for the LFE, which is left out of the loudness. The true peak still
covers every channel, the LFE included. Any other channel count returns `UnsupportedChannels`. The program meter
measures the stereo output.

## Animation clips compressed at cook

The cook writes each glTF, FBX and BVH animation clip (and each retargeted clip) as a **`cooked-clip` v3** payload
with its joint tracks compressed. `regia_animation::compress` drops keys within the spec 17 tolerances (5 mm
translation, 5 mrad rotation, and the rotation error propagated over the joint's chain, using the clip's skeleton).
It quantizes what is left: rotations smallest-three in 8 bytes, translations and scales to 16 bits over the track's
range. A rotation track that slerps keeps every key and its slerp flag; only its keys are quantized. Morph weights
(which may leave 0..=1), notifies, curves, property tracks and root motion are stored as authored. Loading
decompresses the joint tracks into an ordinary `AnimationClip`, so nothing downstream changes. On a dense
12-joint test clip the payload is under a quarter of the lossless one. The same input always gives the same bytes.

`AnimationClip::to_cooked_payload` still writes losslessly: a v3 payload with a `Raw` body.
`AnimationClip::compressed_cooked_payload(bytes)` returns the compressed tracks of a compressed payload, for
`compress::measure_compression_error`.

**Upgrading:** v1 and v2 clips still load. `regia upgrade-project --write` rewrites them as lossless v3 payloads; the
migrator never compresses, so old content plays exactly as before. Re-cook a source to compress its clips. A cooked
cache keeps its v2 clips until their source is cooked again.
