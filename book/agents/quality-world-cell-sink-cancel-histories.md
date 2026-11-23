# Checking cell sink cancellation histories

`CellSink` completion is an internal streaming primitive. Its private decode checkpoints and terminal
channel are available only in `regia_world` unit tests; there is no scheduler or sink agent method.
These regressions exercise spec 78 § 9 using tiny valid cell blobs and the real async compute pool.

Run the histories with:

```sh
cargo test -p regia_world --lib source::tests::cell_sink -- --nocapture
```

The tests initialize `regia_core::tasks` with two async workers. Each history can hold one worker at a
private, per-sink checkpoint while the other worker decodes an independent cell. The histories
serialize with each other so their barriers cannot exhaust that pool. Channels acknowledge every
checkpoint; bounded receives diagnose a broken handshake. There are no synchronization sleeps,
elapsed-time assertions, random seeds, filesystem fixtures or golden images.

The terminal observer waits for one result and then producer disconnection before forwarding that
result to the real runtime. This checks the exact ticket and detects duplicate outcomes, including
ones sent after the first result was consumed. Fixed planning and completion steps check the slot
and pending-byte ledger independently of decoder timing.

| Regression | What it checks |
|---|---|
| `cell_sink_terminal_histories_release_one_slot_and_the_next_cell_uses_it` | Cancel before answering, drop an unanswered sink, explicitly abandon, report a read failure, reject a checksum mismatch, and fail parsing a header/hash-valid incomplete blob. Every request has exactly one terminal result. The queued second cell acquires the single available slot and completes; bytes and slots return to zero. |
| `cell_sink_cancel_at_decode_checkpoints_does_not_suppress_another_cell` | Cancel after the source answers, before decode starts or after parsing before publication. The canceled result is abandoned; the other cell finishes while the first is held; the canceled cell can subsequently reload. |
| `cell_sink_old_completion_cannot_replace_an_active_generation_or_release_its_slot` | An old ready result queued before cancellation, or an abandoned decode completed after cancellation, arrives while a replacement ticket is loading. It increments stale discards, preserves the replacement, and releases only the old reservation. Another queued cell can use that reservation. |

The raw sink outcomes distinguish `cell_corrupt` (checksum failure), `cell_decode` (malformed cell
payload), `cell_unavailable` (read of an
unmounted chunk), and abandonment. The runtime reports an unanswered or explicitly abandoned current
request as `cell_missing`. A canceled stale result does not produce a current failure or an applied
cell. All codes use lowercase ASCII and underscores.

For existing host-facing inspection, use `stream.status`, `stream.cells` and `stream.trace` from the
[streaming runtime guide](streaming-runtime.md). `stream.cells` exposes a failed cell's `reason`;
`stream.trace` shows `cancelled`, `failed` and `applied` events. `stream.retry` accepts a known failed
cell and reports `stream_unknown_cell` for an unknown coordinate. `stream.budget_set` reports
`stream_bad_budget` for invalid budgets; methods requiring an open streamed level report
`stream_not_enabled` without one. These methods inspect normal runtime behavior; they cannot pause
a decoder or expose an individual sink's terminal channel.

The existing agent handler tests exercise those happy and error calls without introducing a new
agent method:

```sh
cargo test -p regia_agent --lib stream::tests::the_read_methods_describe_a_running_world
cargo test -p regia_agent --lib stream::tests::budgets_timing_retry_and_rebase_change_the_runtime
```

For mutation validation, remove only the decoder's final cancellation check after the
`BeforePublish` checkpoint, run the checkpoint regression, and require it to reject the resulting
`Ready` outcome. Restore the check and rerun the histories. This is a local test of the oracle;
never commit the mutation.

During verification, omitting that final check failed with `BeforePublish: Ready`. A second local
mutation, leaving `answered` false before spawning decode, failed with `at most one terminal
outcome` when Drop sent another result. Both mutations were restored and all three regressions
passed. These are oracle checks; no production defect was reproduced.

These CPU tests make no physical-device, performance, render-gate, HLOD, teleport or persistence
claim. They add no public API, content format, feature flag or agent method.
