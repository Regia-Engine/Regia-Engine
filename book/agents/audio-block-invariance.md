# Audio limiter block invariance

The master limiter carries its gain envelope from one audio callback to the next. The callback
size may change without changing the rendered samples or the output meter for the same input,
sample rate and settings timeline. The unit test `limiter::tests::output_and_meter_are_invariant_to_callback_partitioning`
checks 44.1, 48 and 96 kHz with callback lengths 1, 7, 64, 257 and a repeating mixed sequence
that ends in a short tail. Its stream includes silence, sub-ceiling stereo material, a transient
overload and non-finite samples. It changes settings and bypass at fixed sample offsets.

The current agent protocol can inspect and adjust the limiter, but does not control callback
partitioning. Use these existing calls to inspect the live output stage:

```json
{ "method": "audio.mix", "params": { "reset_meter": false } }
```

The reply includes `limiter` and `meter` (`frames`, `peak_in_db`, `peak_out_db`,
`max_reduction_db`, `limited_frames`, `nonfinite_samples`). To change settings:

```json
{ "method": "audio.set_limiter", "params": { "ceiling_db": -1.0, "release_ms": 120 } }
```

For an offline sample render, use the existing headless call:

```json
{ "method": "audio.render_check", "params": { "seconds": 0.5, "wav": false } }
```

These calls observe or change the same output stage; the private unit test is the check for
sample-for-sample callback invariance. Resetting the `audio.mix` meter clears its counters only.
The unit test `limiter::tests::resetting_meter_does_not_reset_signal_state` compares the samples
after a reset with a control limiter whose meter was not reset.

## Errors

- `F0013` (`UNSUPPORTED`): the audio plugin is not installed; `audio.render_check` also returns
  this when the output is not headless/offline.
- `F0002` (`INVALID_PARAMS`): `audio.set_limiter` values are outside `ceiling_db` −24..=0 or
  `release_ms` 1..=2000; the setting is left unchanged.

No agent method, error code or runtime setting was added for this test coverage.
