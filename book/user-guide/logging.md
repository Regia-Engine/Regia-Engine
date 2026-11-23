# Logging

Regia writes ordinary engine diagnostics through `tracing`. A game that wants a per-peer
session log and diffable checkpoints opts into `regia_core::log::session::PeerLog`. The
files are local evidence. Nothing in this path uploads them or opens a network connection.

## What the host owns

Create one `PeerLog` per peer, under a private log root, with a sortable session name and
a peer id. Hold it until shutdown. Call `set_tick` or `line_at` with the tick the simulation
already computed, then `line` for a human event and `hash_trace` for a checkpoint the host
already hashed. The logger stores those numbers. It does not step the simulation or compute
a new hash.

Call `flush` when the bytes must be on disk and the session is still open. Call `finish`
only after teardown succeeded. `finish` syncs, then publishes a `clean` marker. Dropping
the handle flushes and does **not** publish `clean`. The next launch of that peer reports
the newest inactive older session that has no `clean` marker.

Keep the log root on storage the host trusts. The host deletes old session directories.
Recovery does not.

## Synchronous writer

The default writer performs each write on the caller and returns `std::io::Result`. A
failure rejects later writes and `finish`. That is the right choice when the caller is
already a diagnostic thread.

## Ordered worker

Latency-sensitive threads opt in:

```rust,wrap
# use regia_core::log::session::{AsyncWriterBounds, SessionOptions};
let options = SessionOptions::default().with_async_writer(AsyncWriterBounds {
    max_records: 256,
    max_queued_bytes: 256 * 1024,
    max_message_bytes: 8 * 1024,
});
```

Each session then has one worker. Producers enqueue. `flush` and `finish` drain what was
accepted and sync. Drop joins the worker. There is no background thread left running after
the handle is gone.

The queue is capped by record count and by bytes. A human line that is too long or does
not fit is dropped and counted (`dropped_lines`, `dropped_bytes`, `oversize_lines`). The
call still returns `Ok`, so a full diagnostic queue does not change simulation control
flow. A checkpoint that does not fit the queue, or that fails to write, marks the session incomplete.
Hitting `max_hash_traces` rejects only that new name. Names already opened can still be
appended, and `finish` can still publish `clean`.
`finish` then returns an error and does not publish `clean`. Treat that session as unclean
evidence.

`line_at(tick, ...)` is the call to use when more than one thread logs for the same peer.
It stores that tick with the record. A later tick change does not rewrite it.

## Two peers

```rust,wrap
# use regia_core::log::session::{PeerLog, SessionOptions};
# use regia_core::profiling::__tracing as tracing;
# let root = std::env::temp_dir().join("regia-book-peer-log");
# let options = SessionOptions::default();
let left = PeerLog::create(&root, "match-001", 1, options)?;
let right = PeerLog::create(&root, "match-001", 2, options)?;
left.line_at(3, tracing::Level::INFO, "net", format_args!("alpha"))?;
left.hash_trace("world", 3, 0x41)?;
right.line_at(4, tracing::Level::INFO, "net", format_args!("beta"))?;
right.hash_trace("world", 4, 0x42)?;
left.finish()?;
right.finish()?;
```

Peer 1's `world.hash` contains `3 0000000000000041`. Peer 2's contains
`4 0000000000000042`. Compare those files with a text diff. The first unequal row is the
first checkpoint that differs. The human `session.log` is not the diff source.

## Reading the counters

Agents read the existing `logs.read` method. The target `regia_core::log::peer_async`
carries `queued_records`, `queued_bytes`, `written_records`, `written_bytes`,
`dropped_lines`, `dropped_bytes`, `oversize_lines`, `errors`, `incomplete`, `complete`
and `sequence`. `complete=1` means `finish` published `clean`. `incomplete=1` means a
checkpoint was lost or a write failed. Checkpoint rows remain on
`regia_core::log::hash_trace`. No new agent method or error code is added.

Details and the `play.wait_until` / `play.expect` workflow are in
[Peer logs and checkpoint traces](../agents/log-extras.md).
