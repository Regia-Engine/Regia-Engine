# Cooked-texture format accounting (validation)

What `regia cook --platform <platform> --stats [--json]` reports about the cooked texture cache
(`regia_asset_pipeline::texture_stats`, `docs/specs/12-assets.md` § 7.1), and the independent
oracle tests that hold its accounting exact. This page is the agent guide for that surface; the
user-facing workflow is [Android texture compression](texture-compression.md).

## What the report is

Per cooked texture (a KTX2 container in the cache): source path, mip-0 dimensions, mip count,
KTX2 format name, its family (`bc`, `astc`, `etc2`, `uncompressed`, `other`, from the Vulkan
`VkFormat` ranges), ASTC block footprint, and the KTX2 bytes. Totals per family — ASTC split by
block size (`ASTC 4x4`, `ASTC 6x6`, …), largest byte total first, label ascending on equal bytes —
plus the paths of block-compressed textures in a family the platform does not cook to
(`wrong_platform`) and the paths whose bytes could not be read (`unreadable`, with the error).
The platform policy is fixed: Android cooks ASTC or ETC2 / EAC, desktop (`linux`, `windows`,
`macos`) cooks BC, an unknown platform flags nothing; uncompressed and unknown formats never
count as wrong-platform. Unreadable entries keep their input order.

## What the oracle tests guarantee

The library tests in `texture_stats.rs` validate the aggregation against an independently written
grouping (`BTreeMap` keyed by report label, sorted by the documented rule — the oracle never calls
`summarize`):

- **Order independence.** A mixed population (BC, ASTC 4x4/5x5/6x6/8x8, ETC2 + EAC, uncompressed,
  unknown) is run through five deterministic LCG shuffles per platform; family counts, exact byte
  sums, totals order, wrong-platform membership, path sorting and unreadable passthrough must be
  identical every time. Cache enumeration order cannot move a number or a warning.
- **Partition recombination.** Rows split into partitions and summarized separately sum exactly to
  the single summary (counts, bytes, totals per label, wrong-platform union, unreadable list) —
  nothing is double-counted — and the recombined raw rows reproduce the single summary exactly.
- **Edges.** Equal-byte ties order by label ascending; zero-byte rows stay listed with their
  family; an empty population reports nothing and still formats; long and non-ASCII offender
  paths are fully represented in the text report without corrupting or merging lines.
- **Row parsing.** `texture_row` on bounded fixtures written by the pipeline's own KTX2 writer
  reflects the header's format, mip count and dimensions and counts the complete container byte
  count; truncated, garbage and empty payloads return an error with detail.

Sensitivity to deliberate defects (an ascending totals sort, a one-byte drift in accumulation)
was demonstrated: the oracle, partition and pre-existing summary tests fail, then the defect was
reverted. No implementation change shipped in this lane.

## How an agent verifies it

There are no new agent methods and no new error codes. The tests are CPU-only library tests of
`regia_asset_pipeline`; run them through the existing test job:

```sh
regia agent call agent.run_tests '{"target": "lib", "package": "regia_asset_pipeline", "filter": "texture_stats"}'
```

`target` selects the test target (`lib` here), `package` the crate, `filter` the cargo test name
substring. The job's events list each `texture_stats::tests::*` case with its outcome; all 14
must be `ok` (the command above expands to
`cargo test --color never -p regia_asset_pipeline --lib --offline -- texture_stats`, per
[jobs](jobs.md) § `agent.run_tests`).

Stable error codes: none new from this lane. The existing `agent.run_tests` failures
(`run_tests_bad_name`, `run_tests_busy`, `run_tests_no_manifest`, `run_tests_spawn_failed`) are
documented in [jobs](jobs.md); the report itself has no error-code schema (cook errors are text,
per [Android texture compression](texture-compression.md)).
