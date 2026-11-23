# Shared cook cache: failed multi-output restore

The shared cook cache verifies an entire entry before installing its outputs. Entry publication is an atomic
single-file replacement, and each local output is installed with a sibling temporary file and rename. These
operations do not form a transaction across all of an action's files. If a later local install fails, earlier
verified files can already be present. The restore returns a miss, records an IO error and a warning, and does
not record a hit or a local input record that could justify skipping the import. Normal cooking then attempts
the import. An output directory that remains unwritable can also prevent that import from completing.

The restore primitive and its failure injection are internal-only. There is no agent method for forcing a
partial restore. This recipe adds regression coverage for the existing behavior; it adds no cache format,
rollback journal, or command.

## Deterministic regression

Run from a normal repository checkout:

```sh
cargo test -p regia_asset_pipeline --lib cook_cache::tests::restore_
cargo test -p regia_asset_pipeline --test cook_shared_cache
```

On the shared build hosts, use the lane's required resource wrapper and CPU/lavapipe configuration.

| Test | Independent check |
|---|---|
| `restore_second_install_failure_is_a_miss_and_a_healthy_retry_replaces_the_set` | Encodes a valid two-output entry with the existing writer and stores it through `DirBackend::put`. Starts with old local outputs, replaces the second destination with a nonempty directory holding old bytes and a sentinel, and forces its rename to fail. Asserts no returned outcome, zero hits, one miss and one IO error, no corrupt-entry count, no remembered key or local skip record, the complete first output, preserved unrelated bytes, and no leaked temporary. Removing the fixture's conflict allows a retry to restore exactly both expected payloads. |
| `restore_verifies_the_final_payload_before_exposing_any_output` | Flips only the final payload byte. A restore must reject it by hash, preserve the entire old local tree, claim no hit or skip record, and leave the corrupt read-only cache entry intact. An explicit fixture repair followed by a healthy restore yields the exact original payload bytes. |

The byte oracle uses the fixture's original payload vectors, independently of the decoder. Full directory
snapshots include unrelated files, directories and temporary files. Read-only cache snapshots must remain
identical after failed restores and successful retries; only explicit fixture writes repair corruption. All
fixtures use `regia_test_utils::unique_temp_dir` and atomic file writers. No timing or permissions-based
failure is required, so the conflict also works when tests run with elevated filesystem permissions.

## Existing agent observation

[`cook.cache_stats`](cook-cache.md) reads the last cook's summary; it does not perform a restore. Its existing
happy and error calls can be exercised with:

```sh
cargo test -p regia_agent --lib cook_cache::tests::stats_report_the_configuration_the_entries_and_the_last_cook
```

That handler test calls `cook.cache_stats` with normal configuration, a missing share, entries and a saved
summary, then invalid configuration and parameters. The existing lowercase domain error code is
`shared_cache_config_invalid` for a bad shared-cache table. Malformed `platform` or `scan` parameters use the
generic invalid-params response (JSON-RPC `-32602`, legacy diagnostic `F0002`). A missing share is a normal result with `reachable: false`. A restore IO failure is a cook
warning and summary error, not a new agent error code.
