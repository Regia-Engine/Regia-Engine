# Report outbox: incremental log upload

The report outbox (`regia_crash::outbox`, ADR-8402) uploads the session logs a little at a
time: each flush sends only the bytes written since the last confirmed offset, in capped,
scrubbed parts, and a crash resends the crashed session's whole log once. Agents read its
state with `report.outbox_status`.

## `report.outbox_status`

Call it (MCP tool `report_outbox_status`) with `{}`. It needs `read` permission and works in a
read-only agent profile; it counts against the heavy-call rate limit. It reads the ledger and the
logs' sizes, never their contents, and sends nothing.

```json
{"jsonrpc":"2.0","id":1,"method":"report.outbox_status","params":{}}
```

| Field | Meaning |
|---|---|
| `configured` | The `CrashReporter` resource has `outbox: Some(..)`. |
| `enabled` | Configured and uploads are allowed (endpoint set, consent given, not a remote `http://` endpoint). |
| `ledger` | The ledger file: `CrashReporter::outbox.ledger`, else `outbox.ron` in the logs directory. |
| `part_cap` | The cap a part's scrubbed body stays under, in bytes (default 262144; at least 4096). |
| `max_rows` | Ledger rows kept before old ones age out (default 64). |
| `rows` | Rows in the ledger now. |
| `pending_bytes` | Bytes not yet confirmed, over every listed log. |
| `sources` | One entry per session log (`*.log`, `*.jsonl`) in the logs directory, sorted by path. |
| `last_error` | The most recent error of any flush (a part, a source, the ledger write), or `null`. |
| `load_error` | Why the ledger on disk could not be used (it started over), or `null`. |

Each `sources` entry:

| Field | Meaning |
|---|---|
| `path` | The log's host path (lossy UTF-8 in JSON). |
| `bytes` | Its size now. |
| `confirmed` | Bytes from the start the server confirmed (0 when the ledger has no live row for it, or the file is now shorter than the confirmed offset: rotated). |
| `pending_bytes` | `bytes - confirmed`. |
| `tracked` | The ledger has a live row for this file. |
| `crash_resend` | A crash marker's full resend of this log is under way. |
| `last_error` | Why its last part was not confirmed, or `null`. |

| Error code | JSON-RPC code | Meaning |
|---|---|---|
| `outbox_unavailable` | `-32001` | No logs directory is mounted or configured and the outbox names no ledger file. |

Unknown parameters use the protocol's existing `F0002` / `-32602` error; there is no path
parameter.

## How the upload works

The outbox runs on the crash reporter's startup thread, before the crash reports are sent, when
the game sets `CrashReporter::outbox` (it is `None` by default). Consent and endpoint rules are
the crash reporter's, unchanged: no endpoint or no consent sends nothing, reads nothing and
writes no ledger; a remote `http://` endpoint is refused unless `allow_cleartext` is set; an
`https://` endpoint needs the crate's `tls` feature.

- **Ledger.** `outbox.ron` (version 1, local state, one per install) has one row per log: path,
  identity (size, modification time, a 64-bit FNV-1a hash of the first 4096 bytes) and the last
  confirmed offset. A file shorter than its confirmed offset, or whose first bytes no longer hash
  the same, was rotated: its row is retired and a new row starts at byte 0. Rows age out by
  count (`max_rows`): retired rows and rows of logs no longer present go first. A ledger that
  does not parse, or has another version, starts over (`load_error`): its logs are sent again
  from byte 0. Duplicates are possible; gaps are not.
- **Parts.** A part holds at most `part_cap` bytes after scrubbing (`scrub_upload`: home folder
  to `~`, secrets to `[redacted]`, email addresses to `[email]`). It is cut after the last
  newline that fits, so a scrubbed pattern is not split across parts; a longer line is cut on a
  UTF-8 character boundary, and a private-key block is never split. The running session's log
  holds its unfinished last line back. Each part is a `text/plain` POST whose
  `X-Regia-Report-Name` is `<file>;<start>-<end>` (`;crash` added in a crash resend), so the
  collector can reassemble by offset. Every upload also sends the same value in the header's
  pre-rename name, `X-Ferrite-Report-Name`, so a collector that matches only the old header
  keeps working; that second header goes away in the first release after v0.6.0 (ADR-8670).
  Collectors should match `X-Regia-Report-Name` now.
- **Confirmation.** An offset moves only after a 2xx, and the ledger is written (atomically)
  after every confirmed part. A transport error or any other status leaves the part pending,
  stops that log for this flush (later parts would leave a gap) and records `last_error`; the
  next flush sends the same part again. One flush sends at most `max_parts_per_flush` parts (64).
- **Crash resend.** A `crash-*.txt` marker (written by `regia_platform::crash`; also a
  quarantined `.rejected` one) not seen before sets its session log back to byte 0 once. The
  session log is the one whose start-time name (`YYYY-MM-DD_HH-MM-SS[_N].log`) is the latest at
  or before the marker's. The marker's name is kept in the ledger, so the same crash never
  resends again, even after the reporter removed the report.

Rust hosts that send from elsewhere use `Outbox::open(ledger, settings)`, `Outbox::flush(&reporter,
&sources, &mut transport)` and `Outbox::status(&sources)`, with any
`regia_crash::transport::UploadTransport` (`HttpTransport` is the real one).

Verification: `cargo test -p regia_crash --test outbox` drives every rule through a fake
transport (no network, no clock): `second_upload_sends_only_new_tails`,
`a_rotated_log_starts_a_new_ledger_row`, `a_crash_marker_resends_the_session_once`,
`an_interrupted_part_is_resent_not_skipped`, `no_part_exceeds_the_cap`,
`nothing_uploads_without_consent`, the aging and corrupt-ledger cases and the private-key split.
`cargo test -p regia_agent --lib report_outbox` calls the real dispatcher in a read-only profile.
