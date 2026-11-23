# Android encoder lifecycle evidence

B08 keeps the original acceptance: 50 native home/resume cycles, 10 rotations,
no crash, a clean surface audit, and total PSS growth at most 5% from the normal
pre-cycle baseline. ADR-4657 is an experiment until those measurements pass.

The renderer's experiment cuts inside observed render/compute pass beginnings,
including shadow and post chains. Existing `render.frame_work` counters retain
their documented scope; they do not count every raw call from other crates.
The internal `frame pass segments` debug event reports observed passes, segments
and the largest segment. No new protocol method or error code is introduced.
With zero encode workers, Android finishes those segments synchronously; the
pass budget remains active. Desktop's zero-worker behavior remains one encoder.

Use only the assigned TEST device and hold its exclusive device lock. The lead
runs the existing native harness with the original cycle count explicitly set:

```text
cargo xtask android-pass --package com.regiaengine.samples.arena --project samples/arena --device 54101FDAP0015H --apk <candidate.apk> --regia <matching-regia-cli> --out <evidence-directory> --cycles 50 --rotations 10 --thermal-minutes 10 --uninstall
```

The audited v0.2.19 `android-pass` default was 20 cycles, which does not satisfy
B08; from lane `v05-android-lifecycle-b` the default is the original 50 (keep the
explicit `--cycles 50` anyway). Actual display rotation matters: on the TEST Pixel,
`sensorLandscape` left `mRotation=1` while `user_rotation` alternated 1/3.
No resize marker does not prove a continuous rotation. The v0.2.19 harness
accepted this case; from lane `v05-android-lifecycle-b`, `android-lifecycle` reads
display 0's actual `mRotation` (`dumpsys window displays`) before and after each
request, starts opposite the actual rotation, pins `user_rotation` to it before
turning auto-rotate off, and fails the run with "the request was ignored" when the
display did not reach the requested rotation.

To make a sensor-locked activity follow the request, the rotation phase also sets
`wm fixed-to-user-rotation enabled` (proposed ADR-4904) and puts the original mode back
(`default` on an untouched device) before the settings and before final PSS. On
the TEST Pixel 9 Pro (2026-11-09) this rotated the arena for real: actual
`mRotation` 3, 1, 3, ... matched each of the ten requests, with the same PID. The
two older methods, `wm set-ignore-orientation-request true` and direct
`wm user-rotation`, did not actuate the candidate reliably. With the first one,
the base measured +24% PSS, against +2.9% with `fixed-to-user-rotation`; the
override itself is the likely cause, though that is not proven. Do not use them
for acceptance. Do not count an unchanged
display as a pass, add portrait preconditioning, or move the baseline.

Record source/APK hashes, baseline and final PSS samples, PID,
surface creation/destruction totals, temperatures, workload/profile and crashes.
Keep the harness's existing warm-up and 0.05 allowance. Do not move the baseline
past the growth, add preconditioning cycles, or change the allowance.

For a separate agent inspection, attach with `regia install-android --attach
--no-logcat --project samples/arena --device 54101FDAP0015H`. Read
`lifecycle.get`, `memory.mark`, `memory.report`, `render.frame_work`,
`render.graph` and `thermal.get`; save `seeing.screenshot` after the representative
shadow/post frame returns. `render.frame_work` may initially return a null frame;
allow actual frames to advance and read it again.

`play.wait_until` and `play.expect` can check the supported `Time` snapshot:

```json
{"condition":{"resource":"Time","path":"paused","op":"eq","value":false},"timeout_ms":20000}
```

```json
{"assertions":[{"name":"game time resumed","condition":{"resource":"Time","path":"paused","op":"eq","value":false}}]}
```

Then explicitly inspect `lifecycle.get`: `audit.clean` must be true, violations
must remain zero, created minus dropped must equal live surfaces, and live must
equal windows after resume. A client must assert those returned fields itself;
`lifecycle.get` is not a supported `resource` predicate snapshot on this candidate.
Synthetic `lifecycle.drill {"cycles":50}` can supplement this inspection but
does not replace Android surface destruction or native PSS measurements.

Keep heap-profiling and acceptance runs separate: heapprofd changes process
memory. Attribute retained allocations using the unstripped library from the same
APK. Explicit device destruction in a renderer regression checks cleanup; it is
not evidence of recovery from a real driver fault. Uninstall the test package and
verify restoration of every setting changed by the run.
