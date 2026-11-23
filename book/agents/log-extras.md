# Peer logs and checkpoint traces

Hosts opt into `regia_core::log::session::PeerLog` for per-peer diagnostics. Create one
handle with a writable log root, a unique sortable session name (for example a UTC launch
stamp plus a fixed-width collision counter), a peer id and `SessionOptions`. Names use 1–96 ASCII
letters, digits, hyphens or underscores; `unclean-` and `pending-` are reserved prefixes (case insensitive). Each handle owns `<session>-peer<id>/session.log`
and named `<trace>.hash` files. Duplicate session/peer directories are rejected.

Call `set_tick` before each peer's diagnostics, `line` for an event, and
`hash_trace("world", tick, hash)` for a checkpoint. Each `.hash` line contains only the
decimal tick and 16 lowercase hexadecimal digits. Compare the same trace from two peers
with a text diff: the first unequal or missing row locates the first differing checkpoint.
Checkpoints do not alter the handle's human-log tick. They are neither rate limited nor
filtered; emit at the host's desired checkpoint interval. `max_hash_traces` bounds the
number of named files; there is no implicit log retention or file-size cap. The host owns
retention of completed session directories and schedules writes away from latency-sensitive
threads. The default writer is synchronous and returns `std::io::Result`. Hosts that need the
simulation thread to keep moving opt into one ordered worker (below). Always handle failures.

The process logger also accepts `regia_core::log::set_tick(tick)`. Its console and file
formatters stamp every physical line with `t=<tick>` (initially zero). Tracing ring messages
carry the same prefix. This is diagnostic state; it does not advance the simulation.
In-process peers use the handle's independent tick instead of racing the process stamp.
Custom extra layers retain their own formatting.

## Read and assert through the agent

No new protocol method or error code is introduced. Existing log filtering, cursor and
permission rules apply; see [Logs and engine events](events.md) and [Testing](testing.md).

```json
logs.read { "target": "regia_core::log::hash_trace", "contains": "peer=2 ", "limit": 100 }
```

A record's message is `t=37 peer=2 hash_trace name=world hash=0000000000001234`.
Ordinary peer lines use the caller's target and include `t=` and `peer=` in the message.
The bounded in-memory ring is observational; the files are the complete checkpoint trail.

```json
play.wait_until { "condition": { "logs_contain": "t=37 peer=2 hash_trace" } }
play.expect { "assertions": [{ "name": "checkpoint", "condition": { "logs_contain": "t=37 peer=2 hash_trace name=world hash=0000000000001234" } }] }
```

## Clean and unclean completion

Call `flush()` to flush and sync every file. After host teardown succeeds, call `finish()`;
it syncs files before atomically publishing the `clean` marker. Further writes fail, and
repeated successful `finish()` calls are harmless. Drop flushes buffers while holding the
session lease but **never** marks a session clean. A write/flush failure prevents clean
completion. Handle errors before claiming a successful shutdown.

The next creation checks the newest lexically older inactive session of the same peer.
An OS file lock excludes still-running sessions on every supported platform. An unclean
session produces an atomic `unclean-<session>-peer<id>/` directory with `report.txt` and
`lastlines.log`. The source files remain in place. `tail_bytes` and `tail_lines` configure
bounded recovery (defaults 512 KiB and 50 lines). Existing reports are not overwritten;
the newest clean session stops the search. `recovered()` returns newly published evidence.
An explicit `check_previous` performs the same check without starting a peer logger.

```json
logs.read { "target": "regia_core::log::unclean", "contains": "peer=2 " }
```

Recovery only reads locally supplied session directories; it uploads nothing. Use a private,
trusted log root. No game-specific process naming, grace-period timer or upload dependency
is needed.

## Developer environment gate

`regia_core::log::devenv::{var,var_os,is_set,flag}` preserves the compile-time `devtools`
gate. Without the feature the methods return unset without reading the environment.
`flag` means exactly `1`. The feature is disabled by default in **all** profiles, including
release; an explicitly enabled release development build still honors it. Shipping hosts
must keep `regia_core/devtools` disabled across their dependency graph. Regular player
settings such as log paths continue to use ordinary environment access. Existing engine
hooks are not automatically redirected; host developer hooks must call these helpers.

Acceptance: `regia_core --test log_extras`, the release `dev_env_probe` artifact, and
`regia_agent --test log_extras` (logs.read, wait_until, expect; no window or GPU).

The session-begin record includes `dev_env_enabled=true|false`, so an agent can inspect the
compiled gate with `logs.read { "target": "regia_core::log::session", "contains": "session begin" }`.

A `ready` marker is published only after the session lease is locked and the log is open.
Recovery skips incomplete creation attempts, so a concurrent launch cannot mistake the
create-to-lock interval for an abandoned session. The host may remove such incomplete
folders during its own retention pass.

`PeerLog` is an explicit diagnostic sink: its calls are not filtered by the process-wide
`REGIA_LOG` setting. Hosts choose which ordinary lines to emit; checkpoint files preserve
every successfully written row until the host removes the completed session directory.

## Ordered async writer

`SessionOptions::with_async_writer(AsyncWriterBounds { max_records, max_queued_bytes, max_message_bytes })`
installs one worker for that peer. `line`, `line_at` and `hash_trace` only enqueue. They capture
the tick and the per-peer sequence at enqueue, so a later `set_tick` cannot rewrite a record that
was already accepted. `line_at` stores the tick and accepts the record under one lock; use it when
several threads log for the same peer. The worker writes in acceptance order. `.hash` rows stay
`decimal tick`, one space, 16 lowercase hex digits.

Bounds apply to records and to bytes, not only to a count. A zero bound is raised to one. A human
line longer than `max_message_bytes`, or one that does not fit the queued-record or queued-byte
cap, is dropped. The call returns `Ok`. `dropped_lines`, `dropped_bytes` and `oversize_lines`
count those drops. A checkpoint that does not fit the queue, or whose file or log write fails, sets
`incomplete` and `errors`. Hitting `max_hash_traces` rejects only the new name; traces already
opened can still be appended, and `finish` can still publish `clean`. `flush` and `finish` drain records accepted before the call and sync.
`finish` publishes `clean` only when that drain succeeds and the session is not incomplete.
Dropped human lines do not by themselves refuse `clean`. Drop joins the worker and still does not
publish `clean`. A stuck sink stalls `flush`, `finish` and drop; it does not stall `line` or
`hash_trace`. There is no detached writer: evidence is the bytes the worker has finished.

`create_with_log_sink` sends session-log bytes to a caller-supplied `Write` on the worker.
Checkpoint files stay in the session directory. Production hosts use `create` and the files.

Counters are diagnostic. They do not advance the simulation clock or change a hash the host
computed. Read them with the existing log method. No new protocol method or error code.

```json
logs.read { "target": "regia_core::log::peer_async", "contains": "peer=1 ", "limit": 20 }
```

A counter message looks like:

```text
t=3 peer=1 peer_async queued_records=0 queued_bytes=0 written_records=4 written_bytes=180 dropped_lines=1 dropped_bytes=90 oversize_lines=1 errors=0 incomplete=0 complete=1 sequence=4
```

`complete=1` means `finish` published `clean`. `incomplete=1` means a checkpoint was lost or a
write failed. `sequence` is the last accepted per-peer sequence (the session-begin line is 1).

```json
play.wait_until { "condition": { "logs_contain": "peer=1 peer_async" } }
play.expect { "assertions": [
  { "name": "peer 1 checkpoint", "condition": { "logs_contain": "t=3 peer=1 hash_trace name=world hash=0000000000000041" } },
  { "name": "peer 2 checkpoint", "condition": { "logs_contain": "t=4 peer=2 hash_trace name=world hash=0000000000000042" } },
  { "name": "counters", "condition": { "logs_contain": "peer=1 peer_async" } }
] }
```

Two peers, after the host has the ticks and hashes:

```rust,wrap
# use regia_core::log::session::{AsyncWriterBounds, PeerLog, SessionOptions};
# use regia_core::profiling::__tracing as tracing;
# let logs = std::env::temp_dir().join("regia-book-log-extras");
let options = SessionOptions::default().with_async_writer(AsyncWriterBounds {
    max_records: 256,
    max_queued_bytes: 256 * 1024,
    max_message_bytes: 8 * 1024,
});
let left = PeerLog::create(&logs, "match-001", 1, options)?;
let right = PeerLog::create(&logs, "match-001", 2, options)?;
left.line_at(3, tracing::Level::INFO, "net", format_args!("alpha"))?;
left.hash_trace("world", 3, 0x41)?;
right.line_at(4, tracing::Level::INFO, "net", format_args!("beta"))?;
right.hash_trace("world", 4, 0x42)?;
left.finish()?;
right.finish()?;
```

`world.hash` on peer 1 is the line `3 0000000000000041`. Peer 2's file is `4 0000000000000042`.
Diff those files; do not parse the human log for the checkpoint.

Acceptance: `cargo test -p regia_core --test peer_log_async` and
`cargo test -p regia_agent --test peer_log_async` (logs.read, wait_until, expect; no window or GPU).

## Privacy of a published recovery report

Publication scrubs report fields and tail lines with `privacy::scrub_report` before the
`unclean-*` directory is renamed into place (ADR-8494). Home paths become `~`, secret-shaped
values become `[redacted]`, and email addresses become `[email]`. The peer's `session.log`
and `.hash` files are not rewritten. Ordinary ring lines written while the session was live
stay as the host logged them. Only the published report and the `regia_core::log::unclean`
notice are scrubbed.

`privacy.txt` is a line-oriented manifest:

```text
policy=regia.recovery.privacy
policy_version=1
sanitized=1
complete=1
invalid_utf8=0
partial_line=0
published_bytes=0
source_bytes=0
lines=0
```

`sanitized=1` means the scrubber ran. `complete=0` means the tail omitted a partial first
line, replaced invalid UTF-8, or dropped a line to stay inside the byte cap. `policy_version`
is `1`. A reader accepts only `report.txt`, `lastlines.log` and `privacy.txt`. A symlink, a
non-regular file, an unknown name, or a path that resolves outside the directory is refused.
`is_published_recovery_report` is true only for an `unclean-*` directory whose manifest names
this policy and version with `sanitized=1`. A `pending-*` directory is not a published report.
`tail_bytes` is applied after scrubbing and is clamped to 8 MiB (`RECOVERY_REPORT_BYTE_CAP`).
A line that would be cut in half is omitted whole.

The unclean ring message includes the same flags. Existing filters still match
`unclean_session session=`:

```json
logs.read { "target": "regia_core::log::unclean", "contains": "sanitized=1" }
play.wait_until { "condition": { "logs_contain": "sanitized=1 complete=1 policy_version=1" } }
play.expect { "assertions": [{ "name": "sanitized report", "condition": { "logs_contain": "sanitized=1 complete=1 policy_version=1" } }] }
```

No new protocol method or error code is added. Recovery still uploads nothing. This path does
not change telemetry field allowlists. A `session.log` that is a link or a directory makes
`check_previous` return an IO error, so the next `PeerLog::create` for that peer fails closed
instead of copying another file.

Acceptance adds `cargo test -p regia_core --test peer_recovery_privacy` beside
`cargo test -p regia_core --test log_extras` and `cargo test -p regia_agent --test log_extras`.
