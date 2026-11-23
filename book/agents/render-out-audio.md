# Render-out audio (agents)

`render.out_start { audio }` writes the game's sound beside the frames: a stereo WAV mixdown of the
master output and, optionally, one WAV per bus (stems). The sound is rendered offline, one block per
frame on the job's render clock (or its simulation clock, see [Time maps](#time-maps-timeline)), so
it lines up with the frames sample for sample and two runs of the same request give the same bytes.
Every `format` gets the same samples: `floor(frames * rate * fps_den / fps)`, on the frame path of any
format, subframe count or bit depth.
Design: ADR-1520, ADR-1554 (the clocks). The frames side is on [Render-out](render-out.md).

```json
{"method": "render.out_start", "params": {"out": "/tmp/shot1", "frames": 240, "fps": 24, "audio": {}}}
{"method": "render.out_start", "params": {"out": "/tmp/shot2", "frames": 300, "fps": 30, "audio": {"sample_rate": 48000, "format": "float32", "stems": ["Music", "Sfx", "Voice", "Reverb"]}}}
{"method": "render.out_start", "params": {"out": "/tmp/slowmo", "frames": 120, "fps": 30, "time_map": [{"frame": 30, "scale": 1}, {"frame": 60, "scale": 0.25}], "audio": {"timeline": "output"}}}
{"method": "render.out_progress", "params": {}}
```

## The `audio` parameter

`audio` is `true` or `{}` (the defaults), `false`, `null` or absent (no audio), or an object:

| Key | Values | Default |
| --- | --- | --- |
| `sample_rate` | integer Hz, 8000..=192000 | 48000 |
| `format` | `"pcm16"`, `"pcm24"` (integer PCM) or `"float32"` (IEEE float, the mixer's own samples) | `"pcm24"` |
| `stems` | bus names (regular or send buses of the app's layout), at most 32, no repeats | none |
| `timeline` | `"render"`, `"output"` or `"sim"` (see [Time maps](#time-maps-timeline)) | `"render"` |

Other keys are refused, so a typo (`"stem"`) is an error, not a silent default.

Files, in `out`:

- `<name>_audio.wav`: the master output, after every bus, send bus and the master limiter. This is
  what a player hears.
- `<name>_audio_<bus>.wav` per stem. A regular bus is recorded **after its fader**: its volume,
  mute, ducking and snapshot are in the stem, and its child buses are in it too (the `Sfx` stem holds
  `World` and `Ui`). A send bus (`Reverb`) is recorded at the end of its effect chain. The default bus
  tree is `Master → { Music, Sfx → { World, Ui }, Voice, Ambience }` plus the `Reverb` send bus. The
  stems are taps: the mix is the same, bit for bit, with or without them. Stems are recorded before the
  master limiter, so a set of stems summed is the mix only while the limiter is idle.

All files are stereo, little-endian WAV (`fmt ` tag 1 for PCM, 3 with a `fact` chunk for float).
Integer samples are the mixer's samples clamped to ±1 and rounded (no dither). Each file is written
to `<file>.part` while the job runs. When the job ends a worker thread cuts each file to the frames on
disk, syncs it and renames it, so a file under its final name is complete on disk, and the app keeps
answering however slow the disk is (`render.out_progress` `phase: "closing"`, ADR-1881). Read the WAVs
once `status` leaves `running`.

`agent.schema` gives `render.out_start`'s `audio` a real schema (type `boolean` or `object`; ADR-1572): `sample_rate`
with its range, `format` and `timeline` as enums, and `stems` as an array of bus names whose `enum` lists the default
layout (`Master`, `Music`, `Sfx`, `World`, `Ui`, `Voice`, `Ambience`, `Reverb`), taken from the engine's bus layout. A
project's `config/audio.ron` may name other buses; the mixer checks each stem against the layout it runs and answers
`INVALID_PARAMS` for an unknown one. `aovs`, `time_map` and `cuts` carry `items` the same way.

## How the sound follows the frames

The job swaps the mixer's output for a fresh offline mixer at `sample_rate` for its length
(`regia_audio::Audio::begin_mixdown`). That mixer advances only when the job renders a block:

- The frame `render.out_start` runs in and every warm-up frame render one block of the warm-up step
  (on the timeline's clock) and throw it away, so at frame 0 the mixer is where the job's clock is.
- After each delivered frame (after its last subframe with `subframes`), the job renders that frame's
  block. Frame `i` holds samples `o(i)` to `o(i+1)`, with `o(i) = floor(i * sample_rate * fps_den / fps)`.
  At 29.97 fps and 48 kHz that is 1601 or 1602 samples a frame, and frame 30000 starts at sample
  48,048,000 exactly; the WAV is never longer or shorter than the video.
- A sound started on frame `i` (in `Update`, by a replay, by an emitter) starts on the first sample of
  frame `i`'s block. With `subframes`, a sound started on any subframe of a frame starts at the frame.

No wall clock is read and the block sizes are integers from the request, so the samples depend only
on the game: run the same request twice and the WAV hashes match (`audio.mix.hash` in the progress and
the manifest).

The job's audio and the mixer:

- **Sounds playing when the job starts are not carried into the WAV**, nor back out of it: the
  swap ends every voice, as an output resume does (`docs/specs/16-audio.md` § 4). Autoplaying
  emitters restart on the next frame (in the warm-up, if there is one). One-shots and music that
  the game started before the job are silent. Start the sound you want during the job or its
  warm-up (a replay, a shot sequence, the game logic). When the job ends the output comes back
  (the device, or the headless renderer), with its buses rebuilt, and voices end again.
- While the job runs the device plays nothing (the stream stays open). The slow-frame audio pause
  (`AudioThrottle`) is off during a job: a render-out frame may take seconds.
- **Streamed sounds** (`streaming: true`) are decoded on a background thread. A job that renders far
  faster than real time can outrun the decoder; the stream then holds silent and plays late. Use
  preloaded sounds for anything a deterministic render needs, or render slower (supersampling, large
  frames). `Audio::stream_stats` (and `streams` in `audio.list_emitters`) reports underruns.

## Time maps (`timeline`)

A `time_map` (slow motion, speed ramps; [Render-out](render-out.md)) scales the **simulation** clock
(physics, gameplay, replays) and leaves the **render** clock (VFX, shots, HUD animation) at one output
step a frame (ADR-1524). `timeline` picks the clock the mixer follows. Let `o(i) = floor(i *
sample_rate * fps_den / fps)`, `g(t) = floor(t * sample_rate / 1e9)` and `G(i)` the simulation time of
frame `i` (`frame_times[i].sim_time_ns` in the manifest).

- `timeline: "render"` (default): the mixer steps by the render clock, `o(i+1) − o(i)` samples for
  frame `i`, whatever the map. Sound effects keep their speed and pitch in slow motion (a 0.25 shot
  sounds as the game does in real time), the WAV is exactly as long as the video, and the file is the
  same, sample for sample, as the same job without a map. `Audio` itself (fades, ducking, the music
  clock) ticks on `Time::delta`, the render clock, so this is the timeline the game's audio logic
  agrees with.
- `timeline: "output"`: the mixer steps by the simulation clock, `g(G(i+1)) − g(G(i))` samples for
  frame `i`, and each block is resampled to the frame's output length `o(i+1) − o(i)`. This is
  varispeed, like slowing a tape: at scale 0.25 the sound is four times longer and two octaves lower;
  at scale 2 it is twice as fast and an octave higher. The WAV stays exactly as long as the video. A
  slowed block is interpolated linearly from the last sample of the frame before (continuous across
  frames); a sped-up block is box-filtered (each output sample is the average of the input it covers,
  with exact weights). At scale 0 (a freeze) the frame is silent.
- `timeline: "sim"`: the mixer steps by the simulation clock and the blocks are written as the mixer
  made them, unresampled. The WAV holds the sound at its own pitch and is as long as the simulation
  time (`sim_time_s`); frame `i` starts at sample `g(G(i))`. Use it to conform or time-stretch the
  sound in an editor.

Length is exact: a job of `N` frames writes `floor(N * sample_rate * fps_den / fps)` samples on every
timeline, so one second at 24 fps and 24 kHz is 24000 samples (ADR-1656; the unit tests pin it for several
rates, frame rates and frame counts, NTSC included). The one exception is `timeline: "sim"` under a real
`time_map`: its length is `floor(sim_time_ns * sample_rate / 1e9)` of the nanosecond simulation clock, which
can be one sample under the real-valued duration. Read `audio.samples` rather than computing it from the
duration when the take has a map.

Without a time map the three timelines are the same file. The block's `clock` says which clock the
mixer followed (`"render"` or `"sim"`).

## Progress and the manifest

`render.out_start` answers `audio: { sample_rate, format, timeline, clock, frames, samples, file, stems: [{
bus, file }] }` (`null` without audio), and `render.out_progress` reports the same block for a running job
(`frames` and `samples` written so far, the final ones during `closing`). The echoed `request.audio` is the validated request
(`sample_rate`, `format`, `bits`, `channels: 2`, `stems`, `timeline`, `clock`).

A finished job's `render.out_progress` and the manifest (`<name>_render_out.json`, schema
`regia.render_out/2`) carry the block below, and the manifest lists the WAVs (the mix, then each stem)
in `files.audio`:

```json
"audio": {
  "sample_rate": 48000, "format": "pcm24", "bits": 24, "channels": 2, "timeline": "render", "clock": "render",
  "frames": 240, "samples": 480000, "duration_s": 10.0,
  "frame_sample_offset": "floor(i * sample_rate * fps_den / fps)",
  "mix": { "file": "shot1/frame_audio.wav", "samples": 480000, "hash": "…" },
  "stems": [{ "bus": "Music", "file": "shot1/frame_audio_Music.wav", "samples": 480000, "hash": "…" }]
}
```

`samples` counts sample frames (one left and one right sample each). `hash` is FNV-1a 64 over the
file's data chunk (the sample bytes), so it does not depend on the header. `frame_sample_offset` is
the formula of the sample frame delivered frame `i` starts at (`floor(frame_times[i].sim_time_ns *
sample_rate / 1e9)` for the `sim` timeline with a time map). An entry is `null` and the block has an
`error` when a file could not be finished.

Cancel and failure: the WAVs are cut to the frames that are on disk, so after a cancel the files hold
frames 0 to `frame - 1` exactly, like the frame files. A failed audio write fails the job.

To mux a PNG sequence with the mix, give ffmpeg both inputs, for example
`ffmpeg -framerate 30 -i frame_%06d.png -i frame_audio.wav -c:v libx264 -c:a aac -shortest out.mp4`
(any encoder whose licence suits you works; nothing in the engine needs it).

## Errors

| Call | Condition | Code |
| --- | --- | --- |
| `render.out_start` | `audio` not a boolean or object; an unknown key; `sample_rate` outside 8000..=192000 or not an integer; unknown `format` or `timeline`; `stems` not an array, more than 32, a repeat, or a name that is not 1..=64 letters, digits, `_`, `-`, `.` (not leading `.`) | `INVALID_PARAMS` (nothing is created) |
| `render.out_start` | the WAV would pass 4 GiB (the RIFF size limit): lower `sample_rate`, use `pcm16`, or render fewer frames | `INVALID_PARAMS` |
| `render.out_start` | a stem names no bus of the app's layout | `INVALID_PARAMS` (the message names the bus) |
| `render.out_start` | `audio` asked for and the app has no `Audio` resource (add `regia_audio::AudioPlugin`) | `UNSUPPORTED` `F0013` |

## Limits

- Stereo only; no surround layouts.
- No dither when quantizing to 16 or 24 bits.
- Events start on the frame they fire in, not inside it (at 30 fps, up to 33 ms early relative to a
  subframe of a shutter job).
- Music and one-shots playing before the job are not carried in (see above).
