# Trace asset access

Set `REGIA_ASSET_TRACE=/writable/path/assets.log` before launching the runtime.
Unset or empty disables tracing. The first traced operation or trace query samples
this setting once; restart to change it. The parent directory must already exist.

Call `assets.access_trace { "limit": 64 }` with read permission to inspect recent
accesses. This also works without an `AssetServer` resource. The query is read-only
and never reads the log file or accepts a file path. `guide` topic
`asset-access-trace` describes the same surface.

The result contains `enabled`, `sink_available`, `sink_error`, `total`, `written`,
`evicted` and `events`. Each event has `sequence`, `outcome`, `operation`, `path`
and `path_truncated`. `limit` is an unsigned integer, defaults to 256 and clamps
to 256; zero returns counters only. Invalid parameter types, negative numbers and
fractions return the protocol's `invalid_params` error (F0002). No feature-specific
RPC error is added.

To check a missing asset, ask the host to load it, then find an event whose
`outcome` is `miss`. `miss resolve` means no manifest entry; `miss open` means the
cooked file could not be found. `err` means a different I/O or pak payload failure.
An `ok` open is an I/O success, not proof that the asset decoded successfully.

## Coverage

- Cooked-cache reads made by `AssetServer` and its asynchronous `PayloadReader`.
- `AssetServer::id_for_path` lookups, including failed path-based loads (`resolve`).
- Physical `PakReader::open` attempts (`open`), and its public `read_raw`,
  `read_decompressed` and `read_shared` payload attempts (`pak`). Shared reads
  produce one row even when they use the decompressor internally. In-memory pak
  readers also report payload accesses; constructing them is not a physical open.

This is engine asset diagnostics. It does not intercept arbitrary application
files, general platform VFS reads, metadata probes, manifest-file reads, or
third-party libraries. A cached handle that does not read a payload emits no open.

## Log and retention

The sink appends one line per completed traced operation:

```text
ok open "/project/.regia/cooked/linux/00000000-0000-0000-0000-000000000007.fckd"
miss resolve "assets/missing.png"
ok pak "/content/base.fpak#00000000-0000-0000-0000-000000000007"
```

The third field is a Rust debug-quoted string: quotes, backslashes, newlines and
control characters are escaped, so one access stays one line. Unicode display
text stays readable; non-UTF-8 filesystem paths use replacement characters. Loose
file paths are made absolute without canonicalizing missing files. Pak payloads
use the reader's diagnostic name plus `#<asset-id>`. Logical manifest paths retain
the caller's spelling. These diagnostic rows are not a frozen content format.

The file receives full paths, with no history cap or rotation. Use a separate sink
per process: the mutex serializes threads within one process only. Rows follow
completion order and contain no timestamps; concurrent scheduling can change their
order between runs. No simulation values or replay checksums are affected.

The agent retains only the newest 256 rows and at most 4096 UTF-8 bytes per path.
`path_truncated` signals shortening; `evicted` counts rows no longer retained.
`sequence`, `total`, `written` and `evicted` are process-local saturating counters.
Disabled tracing creates no file, retains no rows and avoids formatting paths.

## Sink failures

A failed sink never changes an asset read's result. `sink_error` reports
`asset_trace_open` if the configured file could not be opened, or
`asset_trace_write` if a row could not be fully written. `sink_available` then
becomes false; recent rows and counters continue. A write failure may leave a
partial final line. The process does not retry the sink: fix the path or storage
and restart. Rows use unbuffered file writes, without a durability `fsync` per row.

Validation: `regia_asset` integration test `access_trace` exercises misses,
successes, error classification, disabled/empty environment, failed opens/writes,
concurrency, escaping, retention, append semantics and pak reads. `regia_agent`
integration test `asset_access_trace` queries a real missed open through the
permission-checking dispatcher with read-only permissions.
