# Release verify, diff, and upload plan

`regia release` checks a directory of release chunks, compares two releases, and prints an
upload order (ADR-8557). It reads the manifests written by `regia engine chunk`. It does not
replace `regia engine verify` or `regia engine chunk verify`.

```bash
regia release verify <dir> [--deep] [--tiers a,b] [--json]
regia release diff <old> <new> [--platform P] [--tier T] [--json] [--markdown]
regia release upload-plan <dir> [--dry-run] [--retries N]
```

`verify` is shallow unless `--deep` is set. A shallow check compares each manifest with the
files on disk, including sizes and SHA-256. A deep check also decompresses every chunk, checks
every member name, and when the manifest records them, member hashes and `content_key`.
`--tiers` limits the check to those tier names. A manifest with no tier is always checked.

`diff` reports chunks added, removed and changed, and a file that moved between chunks once.
It also reports the bytes to download per platform and tier. Exit 0 after it prints, including
when the two releases differ. `--markdown` prints a table. `--json` prints schema 1.

`upload-plan` always prints JSON. Groups are chunks, then other payload, then manifests.
Empty groups are omitted. The plan is retry-safe. This command does not contact a host.

`--json` documents use schema 1, with `ok`, `command`, and on failure `code` and `message`.

Exit codes: verify exits 0 when the directory is clean and 1 when a check fails (a missing
directory is 1). Diff and upload-plan exit 2 when an input cannot be read.

## Agent methods

`release.verify` and `release.diff` are editor-host methods (ADR-10356): a host that installs the
editor's agent surface answers them (the editor and its headless agent host). They are not in the
runtime schema table, so an app that runs only the runtime agent answers method-not-found.

`release.verify` takes `dir` (required), `deep` and `tiers`, plus `require_signature` (when
true, a manifest with no detached signature is a problem) and `trust_keys` (the public keys,
`ed25519:` plus 64 hex digits, allowed to sign; empty accepts the key in the signature), the agent
forms of `--require-signature` and `--trust-key`. Permission `Read`. It does not
change the world. A missing `dir` is `F0002`. An empty `dir`, or a `dir` that is not a
directory, is `release_dir`, and the working directory is not scanned in its place. A directory
that can be read answers success with `ok` true or false and `problem_count`.

`release.diff` takes `old` and `new` (both required), plus `platform` and `tier`. An empty
string for `tier` is a real tier. A missing parameter is `F0002`. An empty, missing or
unreadable input is `release_input`. A diff that finds changes still answers success. The
result has `pairs` and `tiers`.

The agent method for the upload plan is `release.upload_dry_run` (an editor-host method too; see
[Release sign and upload](release-sign.md)).

## Problem checks

These names appear in a verify report's `problems` (`check`). They are not protocol error codes.

| Check | When |
|---|---|
| `release_dir` | The directory is missing. On the agent this is also the protocol code. |
| `sidecar_sha256` | A `.sha256` sidecar does not match the file. |
| `json_tsv_mismatch` | The JSON manifest and its TSV disagree. |
| `missing_asset` | A listed chunk file is not in the directory. |
| `size_mismatch` | A file's size is not the size the manifest records. |
| `sha256_mismatch` | A file's SHA-256 is not the hash the manifest records. |
| `tar_members` | Deep check: the archive's member names are not the manifest's file list, or the archive cannot be listed. |
| `member_sha256` | Deep check: a member's payload hash is not the hash a file entry records. |
| `content_key_mismatch` | Deep check: the uncompressed tar's SHA-256 is not `content_key`. |
| `duplicate_path` | One path is listed more than once. |
| `link_target` | A link target is not stored. |
| `link_path_is_file` | A link path is also a file. |
| `unsafe_path` | A path is absolute, escapes the tree, or contains a backslash. |
| `shared_chunk_mismatch` | The same chunk name does not match across manifests. |
| `tex_tier_mismatch` | A texture chunk is listed under the wrong tier. |
| `metadata_mismatch` | Manifest headers that should agree do not. |
| `archive_too_large` | A chunk is over the 1.9 GiB cap. |
| `orphan_archive` | A `.tar.zst` is not listed by any manifest. |
| `chunk_group_gap` | A numbered group is missing a part. |
| `manifest_parse` | The manifest is not format 1, or it cannot be read as JSON. |
| `missing_manifest_file` | A manifest names a sibling file that is absent. |
| `io_error` | A path could not be read. |
| `zstd_missing` | Deep check: the `zstd` program is not on `PATH`. |
| `zstd_failed` | Deep check: `zstd` exited non-zero. |
| `tar_invalid` | Deep check: the uncompressed bytes are not a tar archive this reader accepts. |

`release_input` is the protocol code for a diff whose input cannot be read. `plan_invalid`,
`pattern_unmatched` and `manifest_gate` belong to the plan helpers and are not agent codes.

`regia release verify` also accepts `--require-signature` and repeatable `--trust-key`.
A signature that is present is checked either way. Signing and filesystem upload are in
[Release sign and upload](release-sign.md) (ADR-8687).
