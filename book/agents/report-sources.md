# Discover local report sources

Call `diagnostics.report_sources` (MCP tool `report_sources`) with `{}` to find the
engine's configured local diagnostic artifacts. It requires `read` permission and
works in a read-only agent profile. It counts against the heavy-call rate limit. It does not upload, delete, flush logs or read
report bodies. No renderer device or feedback plugin is required.

```json
{"jsonrpc":"2.0","id":1,"method":"diagnostics.report_sources","params":{}}
```

The result contains:

| Field | Meaning |
|---|---|
| `directories` | `logs`, `crashes`, `feedback`, `pipeline_cache`: configured paths or `null` when unavailable. A configured directory may not exist yet. |
| `artifacts` | Path-sorted, deduplicated regular files with `kind`, `path`, `bytes`, `modified_unix_ms`, `pipeline_key`. |
| `count` | Number of returned artifacts. |
| `issues` | `{path, code, message}` for incomplete reads or invalid cache headers. |
| `truncated` | A directory had more entries than the scan budget; the inventory is incomplete. |

Kinds are `log`, `crash`, `feedback`, `pipeline_cache`. Logs include `.log`, `.jsonl`,
`.summary.json` and `.summary.partial.json`, plus the exact names `summary.json`
and `summary.partial.json` when a host supplies a per-session root. Crashes include `crash-*.txt` and
native `.crash.bin` records and preserved `native-crash-*.dmp` files (including
the fallback left in the feedback directory when bundle creation fails). Hidden
`.native-crash-*.dmp` files are still in progress and are excluded. Feedback includes `feedback-*.zip` (crash bundles
included). Caches match `pipelines-*.fpc`. Temporary and rejected files, symlinks,
Windows reparse points, directories and unrelated files are skipped. Scans do not
recurse. A missing directory is empty, not an error.

`pipeline_key` is `null` except for a cache with a successfully parsed header. Its
fields are `backend`, `vendor`, `device`, `driver`, `driver_info`, `wgpu_version`
and `shader_hash`. This checks the existing FPCH magic/version/key syntax only;
it does not validate the payload length, checksum, driver compatibility or trust.
Malformed caches remain in `artifacts`, with `pipeline_key: null` and an issue.

The agent considers at most 4096 entries per root (including unrelated entries),
uses one lookahead entry to detect truncation, and
reads at most 64 KiB of each cache prefix. A truncated scan's subset depends on
filesystem enumeration order. Complete results have stable path ordering, but
files may change while they are inspected; metadata is not a content snapshot.
The API is not a file-open authorization boundary. JSON paths use lossy UTF-8
for native names that cannot be represented; Rust callers retain `PathBuf`.

| Issue code | Meaning |
|---|---|
| `read_directory` | A configured directory could not be opened. |
| `read_entry` | Directory enumeration failed for an entry. |
| `read_metadata` | A recognized entry could not be statted, including one removed during the scan. |
| `read_header` | A cache prefix could not be read. |
| `invalid_header` | Invalid/truncated cache key header, or a key outside the prefix budget. |

Unknown parameters and malformed parameter objects use the protocol's existing
`F0002` / JSON-RPC `-32602` error. There is no arbitrary-path agent parameter.
Existing permission and envelope errors apply; issue codes above are successful
result diagnostics, not JSON-RPC errors.

Rust hosts use `ReportSourceDirs::from_world(world).list(&ReportSourceOptions::default())`
from `regia_framework::report_sources` (the existing `client` feature). Custom hosts
can construct `ReportSourceDirs` and choose both scan limits explicitly. The
sources follow the mounted `logs://`, process-global crash configuration,
`FeedbackConfig::dir`/the normal feedback resolver, and the renderer's
`pipeline_cache::directory()` including its enable/directory environment overrides.
No platform path formulas are repeated in this API. Upload consent, scrubbing,
file-size limits and retry policy remain the host's responsibility.

Verification: `regia_framework --test report_sources` seeds each artifact kind,
corrupt headers, missing roots, duplicate roots, symlinks and scan limits.
`regia_agent --lib report_sources` calls the real dispatcher in a read-only profile
and checks that a caller-supplied path is refused.
