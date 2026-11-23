# Audio: mixer, limiter, render check

Inspect the bus mix, set bus levels and the master limiter, and listen to the mix offline
(ADR-544). All four methods need `regia_audio::AudioPlugin` (`regia::DefaultPlugins`
installs it); without it they return `UNSUPPORTED` (`F0013`). No new error codes.

## Read the mix

`audio.mix` (read-only) lists every bus in layout order, the master limiter and the output
meter.

```json
{ "method": "audio.mix" }
→ { "buses": [
      { "name": "Master", "parent": null, "layout_db": 0.0, "base_db": 0.0, "target_db": 0.0,
        "current_db": 0.0, "applied_db": 0.0, "muted": false, "bypass_medium": false, "sends": [] },
      { "name": "World", "parent": "Sfx", "layout_db": 0.0, "base_db": -6.0, "target_db": -6.0,
        "current_db": -6.0, "applied_db": -6.0, "muted": false, "bypass_medium": false,
        "sends": [{ "bus": "Reverb", "volume_db": -60.0 }] }, … ],
    "snapshot": null,
    "limiter": { "enabled": true, "ceiling_db": 0.0, "release_ms": 80.0 },
    "meter": { "frames": 96000, "peak_in_db": 3.1, "peak_out_db": 0.0, "would_clip": true,
               "max_reduction_db": 3.1, "limited_frames": 2210, "nonfinite_samples": 0 },
    "headless": true, "real_voices": 12, "virtual_voices": 0,
    "stall_watchdog": { "enabled": true, "stall_frames": 240, "min_stall_ms": 3000,
                        "max_backoff": 16, "recoveries": 0, "failed_resumes": 0,
                        "frozen_frames": 0, "frozen_ms": 0, "backoff": 1,
                        "retry_pending": false },
    "output": { "suspended": false, "resume": "idle", "resume_failures": 0,
                "resume_retry_in_frames": null } }
```

- `layout_db` is the bus's level in the layout, `base_db` the host's (settings sliders,
  `audio.set_bus`), `target_db` / `current_db` the snapshot blend, and `applied_db` the last
  level sent to the mixer, with ducking and mute applied (`-100` while muted, `null` before the
  first frame). A voice on a bus hears the sum of the `applied_db` of the bus and its parents.
- `meter` covers everything since the last reset: `peak_in_db` is the loudest sample entering
  the limiter (above 0 means it would have clipped, `would_clip`), `peak_out_db` what left it,
  `max_reduction_db` the deepest gain reduction. `nonfinite_samples` above 0 means a NaN or
  infinite sample reached the output stage; the limiter turned it into silence, but something
  upstream is broken.
- `reset_meter: true` starts a new window after the reply.
- `stall_watchdog` (ADR-8830, `null` without the plugin's `AudioStallWatchdog` resource) watches
  the output's sample clock on a live device output. When it stands still for `stall_frames`
  engine frames and at least `min_stall_ms` of engine time (the unscaled frame delta, so a
  fast frame rate does not shorten the wait) while the output is not suspended (the device stream
  died without an error, e.g. a failed reopen after an unplug), the watchdog suspends and resumes
  the output once: `recoveries` counts those cycles, `failed_resumes` the reopens that found no
  device (retried after `stall_frames * backoff` frames and `min_stall_ms * backoff` ms, never
  while the window is hidden or the app paused). Each cycle doubles `backoff` up to
  `max_backoff`; a running clock for as long resets it. `frozen_frames` and `frozen_ms` are the
  current still streak. The offline (headless) output, a suspended output
  and a render-out mixdown are never touched, so a headless run always reads `recoveries: 0`.
  A recovery ends every playing voice, like any suspend (spec 16 § 4).
- `output.suspended` is true while the output stream is torn down (an Android suspend, every
  window hidden, a throttled software renderer). When a `Resumed` cannot re-open the device (a
  Bluetooth headset still reconnecting), the engine retries every 30 frames, up to 20 times:
  `output.resume` reads `retrying` (with `resume_failures` and `resume_retry_in_frames`), then
  `idle` once the output is back, or `gave_up` after the last retry (the next suspend/resume
  cycle starts again). The first failure logs one warning.

## Change a bus

`audio.set_bus` (`play` permission) sets the host volume and/or the mute of one bus.

```json
{ "method": "audio.set_bus", "params": { "bus": "Music", "volume_db": -12, "fade_ms": 500 } }
→ { "before": { "name": "Music", "base_db": -3.0, … }, "after": { "base_db": -12.0, … } }
```

`volume_db` is -100..=24, `fade_ms` 0..=10000. With no fade the mixer still ramps the change
over about two frames, so a jump does not click. An unknown bus returns `NOT_FOUND` (`F0003`)
listing the buses; out-of-range values return `INVALID_PARAMS` (`F0002`) and change nothing.
Mute is a level of -100 dB (silence to the mixer), not minus infinity, so unmuting fades back
cleanly.

## The master limiter

Every output (device or offline) runs a stereo-linked peak limiter after all buses and send
buses. It is on by default with a 0 dBFS ceiling and an 80 ms release: a mix that stays below
full scale passes through bit for bit, and one that goes over is turned down instead of
hard-clipped. It also replaces NaN or infinite samples with silence.

```json
{ "method": "audio.set_limiter", "params": { "ceiling_db": -1.0, "release_ms": 120 } }
→ { "before": { "enabled": true, "ceiling_db": 0.0, "release_ms": 80.0 },
    "after":  { "enabled": true, "ceiling_db": -1.0, "release_ms": 120.0 } }
```

`ceiling_db` is -24..=0, `release_ms` 1..=2000. `enabled: false` bypasses it (kira's own hard
clip at full scale then limits the output).

## Listen offline

`audio.render_check` (`play` permission) renders the current mix on the offline backend and
measures it. Game state does not advance during the render: voices keep playing, and nothing
ticks (no emitter movement, no ducking changes). It needs the offline backend (a headless run
or a machine without an output device). On a device it returns `UNSUPPORTED` (`F0013`): use the
meter of `audio.mix` there.

```json
{ "method": "audio.render_check", "params": { "seconds": 1.0, "wav": true } }
→ { "frames": 48000, "sample_rate": 48000, "peak_db": -0.4, "rms_db": -14.2,
    "left": { "peak_db": -0.4, "rms_db": -13.9, "peak": 0.955, "rms": 0.2 }, "right": { … },
    "max_step": 0.031, "max_step_ms": 412.5, "clipped_samples": 0, "nonfinite_samples": 0,
    "clean": true, "meter": { … }, "lufs": -16.8, "true_peak_db": -0.3,
    "wav": "/tmp/regia_audio_mix/render-4242-0.wav" }
```

- `clipped_samples` counts samples within 0.01 dB of full scale; `clean` is true when there are
  none and nothing was non-finite.
- `max_step` is the largest jump between two consecutive samples, at `max_step_ms` into the
  render. A sine of amplitude `a` at `f` Hz never steps more than `2π f a / 48000` (0.029 for
  440 Hz at 0.5). A step far above what the content can make is a click: a sound cut off
  mid-waveform, a parameter that jumped.
- The render starts a new meter window: `meter` in the reply covers the render only.
- `lufs` is the EBU R128 integrated loudness of the render (ITU-R BS.1770-4 K-weighting and
  gating, `regia_audio::loudness`, ADR-1563); `null` for a render shorter than 0.4 s or silent.
  `true_peak_db` is the 4× oversampled peak in dBTP, never below `peak_db`. Compare `lufs`
  against the −23 LUFS reference (`regia_audio::REFERENCE_LUFS`) when balancing a mix.
- `seconds` is 0.001..=10 (default 0.5). `wav: true` writes a 16-bit stereo WAV to `out_dir`
  (default the scratch output directory — `<temp>/regia_audio_mix` on desktop, the app-private
  data directory on Android); listen to it before signing off on a mix. `out_dir` follows the
  `audio.loopgen_render` rule: relative to the project, or absolute inside the project, the system temp
  directory or an `output_roots` entry, no `..`, and no symlink leading out of them;
  anything else (or a non-string) is `invalid_params` (ADR-1816).

## Recipes

- *Is the mix clipping?* `audio.mix { "reset_meter": true }`, play the scene for a few
  seconds, then `audio.mix`: `meter.would_clip` and `max_reduction_db` show how hard the limiter
  worked. More than about 6 dB of reduction pumps audibly: lower the loud bus with
  `audio.set_bus`.
- *Does stopping a sound click?* Headless: start it, `audio.render_check { "seconds": 0.2 }`,
  stop it, `audio.render_check` again. `max_step` must stay near the content's own step. Stops,
  pauses and resumes get at least a 10 ms ramp (`regia_audio::DECLICK_FADE`) once a sound has
  been heard.
- *Is a bus really muted?* `audio.mix`: the bus's `muted` is true and `applied_db` -100; a
  `render_check` with only that bus playing has `peak_db` -100.
