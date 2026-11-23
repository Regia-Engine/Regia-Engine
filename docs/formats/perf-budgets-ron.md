# `docs/perf/budgets.ron`: the CPU budget registry (v1)

Spec: `docs/06-testing-and-ci.md` § 9. Code: `xtask::perf_budgets`. Decision: ADR-2704. A repository file that
`cargo xtask perf-budgets` reads, never a project's, so `regia upgrade-project` does not know it.

Each entry ties one budget a spec states to one release command and one number in its output.

```ron
(
    format_version: 1,
    entries: [
        (
            name: "bindings_200_unchanged",
            spec: "spec 81 § 9",
            package: "regia_ui",
            command: "cargo bench -p regia_ui --bench bindings_200 -- --noplot",
            metric: "^bindings_200_unchanged\\s+time:\\s+\\[[^\\]]*\\]",
            stat: CriterionEstimate,
            budget: Ms(0.1),
        ),
        (
            name: "assetdb_size_map_5k_closure",
            spec: "spec 32 § 9",
            package: "regia_asset_pipeline",
            command: "cargo test -p regia_asset_pipeline --release --test perf_micro_assetdb -- --ignored --nocapture --test-threads=1",
            metric: "perf-micro assetdb_size_map_5k_closure: .*$",
            stat: P50,
            budget: Ms(50.0),
            note: "desktop, one core",
        ),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. Another version is refused. |
| `name` | Unique; lowercase ascii, digits, `_`, `.`, `-`. The key in the baselines. |
| `spec` | Where the budget is written, or `ADR-2704` for a number no spec states. |
| `package` | The crate measured; the command must say `-p <package>`. |
| `command` | `cargo test ... --release ...`, `cargo bench ...` or `cargo run --release ... --example ...`, split on whitespace (no shell). A command that several entries share runs once. |
| `metric` | Regex in multi-line mode. It must match exactly once in the command's combined stdout and stderr: no match is `metric_missing`, more than one is `metric_ambiguous`. Do not anchor a test's line with `^`: at `--test-threads=1` libtest prints `test name ... ` and the test's first line follows on the same line. |
| `stat` | What to read from the match, below. |
| `budget` | `Ns(f)`, `Us(f)`, `Ms(f)`, `S(f)`, `Count(f)` or `Unbudgeted` (only the baseline compares it). Reaching the budget exactly is within it. A count statistic takes `Count` or `Unbudgeted`; a time statistic takes a time or `Unbudgeted`. |
| `note` | Optional: the budget's conditions (reference machine, statistic chosen). |

| `stat` | Reads |
|---|---|
| `P50`, `P99`, `Max` | `p50=`, `p99=`, `max=` of a `perf-micro <name>: n= p50= p99= max=` line. A duration prints as `12.5µs`, `3ms`, `1.2s` or `800ns`. |
| `CriterionEstimate` | The middle number of criterion's `time: [low estimate high]`, which may sit on the line after the bench name. |
| `Capture` | The regex's named groups `value` and `unit` (`ns`, `us`, `µs`, `ms`, `s`), for a probe that prints its own line. Both groups are required. |
| `AllocsPerOp`, `BytesPerOp` | `allocs / ops` and `bytes / ops` of a `perf-count <name>: ops= allocs= bytes=` line. |

A bad entry fails the whole run before anything is spawned, naming every problem. Unknown fields are refused.

## The baselines

`docs/perf/budget-baseline-<host>.json` holds one machine's times, in nanoseconds. `docs/perf/budget-counts.json` holds the
per-operation counts, the same on every machine. Both have one shape, and a missing file reads as empty:

```json
{ "version": 1, "recorded_on": "the build server 0123abcde", "entries": { "assetdb_size_map_5k_closure": 41250000.0 } }
```

**Versioning**: the registry's serde field lists are hashed by `cargo xtask check-formats` (entry `perf-budgets`). A shape
change bumps `format_version` and rewrites this file in the same commit, since it is the only file of the format.
