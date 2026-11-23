# Release chunks

`regia engine chunk` plans, writes and checks release chunks under a byte cap (ADR-8404,
`release-chunks` format version 1). The format page is `docs/formats/release-chunks.md`. This
format is part of the one v0.6 format bump and must merge before the v0.6 freeze.

```bash
regia engine chunk plan SOURCE [--max-chunk-bytes SIZE] [--bucket-raw SIZE] [--rules FILE] [--compressed [--scratch DIR]] [--json]
regia engine chunk cut SOURCE --out DIR [labels] [--zstd-level N] [--json]
regia engine chunk pack SOURCE --out DIR [--previous DIR] [labels] [--json]
regia engine chunk verify DIR [--cap SIZE] [--json]
```

`SIZE` is a byte count or `64KiB`, `512MiB`, `2GiB`. The default cap is 1.9 GiB. A cap of 0 or a
cap at or above 2 GiB is `chunk_cap_invalid`. `--rules` is a TOML file; its first matching glob
replaces the built-in directory layout. Labels are `--platform`, `--tag`, `--product-version`,
`--source-commit`, `--libc-required` and `--tier`. zstd defaults to level 3, one thread.
`--zstd-long` passes `--long=27`. `--preserve-exec` stores an executable file as mode 0755.

`--json` prints one document, schema 1:

```json
{"schema": 1, "ok": true, "command": "plan", "groups": []}
```

Each plan group has `group`, `files`, `estimated_chunks`, `raw_bytes` and `compressed_bytes`.
Without `--compressed`, `estimated_chunks` is estimated from the uncompressed tar size (a zstd cut
can write fewer chunks) and `compressed_bytes` is `null`. With `--compressed` the plan cuts the
tree with the same zstd settings into a fresh directory under `--scratch` (default: the system
temp directory), reports the chunks and compressed bytes that cut wrote per group, and removes
the directory, also on failure (ADR-8846). It costs one compression pass, needs the `zstd`
program and scratch space for the compressed tree, and fails like `cut` (for example
`chunk_over_cap`). The library call is `regia_release_chunks::plan_tree_compressed`.

A failure is `ok: false` with `code` and `message`, and the process exits non-zero. Without
`--json` the error is `code: message` on stderr.

## Error codes

| Code | When |
|---|---|
| `chunk_over_cap` | One file's compressed form is above the cap, or a written chunk would be. Nothing over the cap is left behind. |
| `chunk_cap_invalid` | The cap is 0 or at least 2 GiB, or a size flag does not parse. |
| `io_error` | A path could not be read or written. |
| `zstd_missing` | The `zstd` program is not on `PATH`. |
| `zstd_failed` | `zstd` exited non-zero. |
| `zstd_level_invalid` | The level is outside 1..=19, or the thread count is 0. |
| `manifest_invalid` | The JSON is not this format, or it has an unknown field. |
| `release_chunks_newer` | `format_version` is newer than this build. The message names the version. |
| `path_rejected` | A path is empty, absolute, `.` or `..`, or it contains a tab, newline, CR, NUL or backslash. A symlink target that is absolute or contains `..` is the same code. |
| `path_too_long` | A relative path is longer than 4096 bytes. |
| `not_regular` | Pack found a symlink or a special file. Cut stores symlinks and skips specials. |
| `rules_invalid` | The rules file is not a table of globs, or a group name is not `[A-Za-z0-9_-]+`. |
| `tar_invalid` | An archive member could not be written or read. |
| `sha256_mismatch` | A chunk file's SHA-256 does not match the manifest. |
| `size_mismatch` | A chunk file's size does not match the manifest. |
| `content_key_mismatch` | The uncompressed tar's SHA-256 does not match `content_key`. |
| `member_mismatch` | The archive's member names are not the manifest's `files`. |
| `link_mismatch` | A link target is not stored in any chunk. |
| `verify_failed` | Verification could not finish the check. |
| `tier_chunk_differs` | A group that is not in the tier list differs from the previous manifest. |
| `reuse_mismatch` | A previous chunk with the same content key is missing its asset. |

There is no agent-protocol method. The surface is this command and the JSON document.
