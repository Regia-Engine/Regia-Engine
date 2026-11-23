# Regia patches to kira 0.12.5 (MIT OR Apache-2.0, upstream tesselode/kira)

Portions ported from kira (https://github.com/tesselode/kira), version 0.12.5.
Copyright (c) Andrew Minnich and contributors. MIT OR Apache-2.0; used under MIT.
The complete vendored source distribution is under `vendor/kira/`; the licence texts are in `LICENSE`.

Upstream 0.12.5 (and 0.12.6, same code) restarts its cpal output stream from a thread of its own when the
default device changes or the stream reports `DeviceNotAvailable`/`StreamInvalidated`, and unwraps the restart.
When the device cannot be reopened (an unplug race) that thread panics: an abort under Regia's shipping
`panic = "abort"`, and permanent silence otherwise (the renderer was lost with the failed stream). Changes, all in
`src/backend/cpal/desktop/stream_manager.rs`, each marked `Regia patch`:

1. `start_stream`: when `build_output_stream` or `play` fails, the renderer that the dropped data callback sends back
   is put into `State::Idle` again (upstream left the manager `Empty`), and the error is returned.
2. `check_stream`: both restart paths (disconnection error, default-device change) no longer unwrap; a failed start
   leaves the manager idle. A disconnection error queued behind a failed restart does not stop a stream that is not
   running (upstream `stop_stream` panics then).
3. `check_stream`: while the manager is idle after a failed restart, every check (500 ms) tries the default device
   again (`restart_on_default_device`).

Regia's own `AudioStallWatchdog` (ADR-8830) still covers a stream that stops rendering without an error.
Entries in `docs/decisions.md`: ADR-8830.
