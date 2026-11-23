# GPU error payloads

The telemetry layer emits uncaptured wgpu errors as `regia::gpu_error` events. The nested `data`
object includes `kind` (`validation`, `internal` or `out_of_memory`), the current `frame`, the
current tracing span name as `pass`, the repeat count, and a `message` bounded to 2048 UTF-8 bytes.
The message is kept as a complete UTF-8 prefix, so structured log readers can parse it without
losing the event's attribution fields. `regia::device_lost` records the loss reason and frame.

## Inspecting an error

Use the existing `read_logs` agent call with `target: "regia::gpu_error"` to inspect recent GPU
errors; use `regia://logs/recent` when reading the recent log resource. A device loss uses target
`regia::device_lost`. These are existing structured log surfaces, not separate GPU agent methods.

For regression coverage, run `cargo test -p regia_telemetry --lib gpu` through the existing
`run_tests` agent call. The tests exercise `on_error` with validation and internal errors while
capturing structured tracing events, checking message limits, JSON validity, UTF-8 boundaries,
frame and pass attribution, and tail containment. The existing device test can be run with
`cargo test -p regia_telemetry --test device`; it exercises an actual validation callback and
deliberate device destruction when a headless adapter is available.
