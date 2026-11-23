# Verify audio output recovery

Spawned parametric patches restart when output resumes. Their patch IDs and current smoothed
parameter values and targets survive. Layers restart from their beginning on the rebuilt bus
tree; their filter, pitch, volume and keyed crossfade are evaluated before playback starts.
Continue using the same ID with `Audio::patch_set_param` or `Audio::despawn_patch`.

For a host-owned 2D loop, call `Audio::start_persistent_loop(asset, params)` and keep the returned
`SoundInstanceId`. It uses the requested loop region, the asset's default, or the whole sound; sources need a finite,
positive duration.
Suspension freezes its cursor. Recovery preserves its ID, pause state and the destinations of
volume, pitch and pan tweens. Seek while suspended changes its recovery cursor. A stop while
suspended cancels recovery. Starting a persistent loop or patch while the mixer is down
(suspended, and no render-out take is running) waits for output. During a render-out take
those calls play in the take, including when the device output is suspended. `spawn_patch`
rejects an unknown bus in both cases.
Recovery fades loops in and respects the ordinary voice budget; an over-budget loop stays virtual.
If the device or bus rebuild fails, `resume_output` returns `false`; ownership stays retained,
the output stays suspended, and a later call retries without incrementing `resume_count` early.

Existing `play_asset`, `start_asset`, cue plays and voice-chat streams retain their previous
teardown behavior. Music scheduling is not recovered by this API. Beginning or ending a
render-out mixdown ends preview loops and patches that are already playing. A patch or
persistent loop started during the take plays in the take and ends with it. Suspending the
device output mid-take does not stop that audio. Hosts must not recreate patches in response to `resume_count`:
that would start duplicates. Use the explicit loop API for loops previously restarted by the host.

## Inspect through the protocol

Call `audio.list_emitters` with `{}` (read permission). Its additive `output` object contains:

```json
{
  "suspended": false,
  "suspend_count": 1,
  "resume_count": 1,
  "persistent_loops": 1,
  "persistent_patches": 1
}
```

Counts describe ownership, including suspended and virtual loops. `real_voices` and
`virtual_voices` describe registered voices; patch layers are separate from those counts.
After a lifecycle `Suspended`, expect `output.suspended = true`, `real_voices = 0` for the
persistent loop and unchanged persistent ownership. After `Resumed`, expect one additional
resume and the same ownership. A second resume does not increment it or duplicate playback.
Use `audio.render_check` on an offline host to check recovered output, then stop/despawn using
the original IDs and check silence. No new protocol method or error code is introduced.
A host without audio still uses the existing `F0013` unsupported response.

The deterministic dispatcher test is
`cargo test -p regia_agent --lib dispatched_output_state_tracks_persistent_loop_recovery`.
The sample assertions and 20-cycle checks are in
`cargo test -p regia_audio --test suspend_resume --test persistent_output --test curve_exports`.
They require no output hardware, window, golden image or wall-clock delay.

## Evaluate patch curves in a host

Import `regia_audio::{polyline, layer_outputs, LayerOutputs}`. These are the existing playback
functions, now exported at the crate root. `polyline` clamps to the end points and linearly
interpolates an ordered point list. `layer_outputs` takes a `PatchLayerDef` and a
`HashMap<String, SmoothedParam>`; use each parameter's current `value` for probes, or construct
a `SmoothedParam` with `value` and `target` equal to a fixed reference value. It adds volume
curves, multiplies pitch curves and takes the minimum filter cutoff. Unknown parameters are
skipped. The layer's keyed crossfade is separate from these curve outputs.

`LayerOutputs.volume_db` and `pitch` are `f32`; `cutoff_hz` is `Option<f64>`. The cutoff is
computed in `f32` and widened by the existing evaluator. A diagnostic that previously stored
`f32` cutoff values can narrow that field at its comparison boundary to retain its fingerprint
representation; do not change reference parameter values while replacing a duplicate evaluator.

Asset volume and loudness offsets add to the evaluated layer volume; asset pitch multiplies
its evaluated pitch. Those defaults apply both to initial playback and live parameter changes,
including the first block after output recovery.
