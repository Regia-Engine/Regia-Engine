# Probe file validation

The `.fgiv` storage regression corpus runs entirely on the CPU. It complements the seeded
mutation/query tests in `regia_asset/tests/fuzz.rs` (ADR-538) with strict-prefix checks and a
structured section-table matrix. The current format is v4; public writers for v1, v2 and v3
supply the legacy fixtures. No new agent method or file version is involved.

Run the two focused targets:

```sh
cargo test -p regia_asset --lib baked_gi
cargo test -p regia_asset --test fgiv_section_mutations
```

The integration target builds two-probe volumes through the public writers, checks every strict
prefix, and compares raw records, decoded SH and irradiance for all four lighting components.
It also exercises the existing v1–v3 migration and the refusal to migrate a current file.
The unit matrix uses the module's existing `encode_header` and postcard types to serialize
mutations for every supported version, so it does not duplicate the legacy file schemas.
Each structured file is 2304 bytes; each public-writer file is capped at 4096 bytes. Iteration
counts are fixed. Tests assert typed errors directly; catching a panic is not a passing result.

| Class | Contract and error |
|---|---|
| Prefix before the declared header ends | `BakedGiError::BadMagic`; includes fewer than 12 bytes and a header length past EOF. |
| Version outside the supported v1–v4 range | Existing `BakedGiError::UnsupportedVersion`; covered by the existing corruption/version unit tests. |
| A shortened serialized header | `BakedGiError::Header` from postcard. |
| Prefix through a required section | `BakedGiError::Payload`, even when the header itself is complete. |
| Offset plus length overflow, offset/end past EOF | `Payload`; checked before section slicing. Empty sections may end at EOF, not beyond it. |
| Nonempty section pointing into the declared header | `Payload`; includes a header length extended into an otherwise valid payload. |
| Section offset not divisible by 16 | `Payload`; all 15 misalignment residues are tested for every section and version. |
| Two nonempty sections alias or overlap | `Payload`; tested with matching hashes, every pair, and partial overlap where the record sizes permit an aligned one. |
| Meta, SH or moment record count/stride differs from the layout | `Payload`; zero, short, extra-byte and doubled lengths are tested with matching hashes. |
| Ground section length differs from columns × 4 | `Payload` before collecting ground heights, including an extra 1–3 bytes; ground without terrain is also `Payload`. |
| Header probe count differs from layout count | `Payload`, including `u32::MAX` in a tiny file. |
| Visibility resolution outside 1–32 or nonpositive/nonfinite distance clamp | `BakedGiError::Layout`; a supported resolution with the wrong moment stride is `Payload`. |
| Unknown SH/moment encoding | `Payload`. |
| Brick reserved bits 1–31 or inconsistent `first_probe` | `Layout`. v1 stores no brick flags; bit 0 is the supported interior flag from v2 onward. |
| Section content changed without updating the hash | `Payload`; every section is independently corrupted. |
| Aligned gaps, nonzero inter-section/tail padding, disjoint sections in another physical order | Accepted. Offsets locate the sections; the hash concatenates their contents in semantic order. |
| Extra bytes inside the declared header before the sections | Accepted by postcard when the known header decodes and no section aliases the declared header; tested with nonzero bytes through the exact first-section boundary. |
| Removal of unused final alignment padding | Accepted, with the same decoded volume. All strict prefixes are tested, including these valid prefixes. |
| Empty sections sharing an aligned in-file boundary | Accepted, including offset zero and EOF; empty ranges contain no header/payload bytes. |
| SH record's three padding bytes | Accepted and preserved on rewrite; they are hashed but do not affect decoded lighting. |
| Probe meta high bits | Defined wall codes, not reserved flags; existing wall-code tests remain applicable. |

`read` also retains the existing `BakedGiError::Io` for file I/O failures.

The range/count overflow protections from `sprint-fuzz-1` already exist in the integration base
(commit `29f3e6886`, ADR-538). ADR-4024 describes enforcement of the existing section alignment,
separation and exact ground-record rules. The writer, versions, quantization and lighting
calculation remain the same.

From a trusted development host with the existing `build` permission and the repository as its
project root, use the existing test jobs (see [Jobs and test runs](jobs.md)):

```sh
regia agent call agent.run_tests '{"package":"regia_asset","target":"fgiv_section_mutations"}'
regia agent call agent.job_status '{"job":"tests:1","since":0}'
```

Use the job id returned by `agent.run_tests`, then advance `since` to each response's `next`.
Require `finished: true`, `state: "done"`, `result.ok: true`, `result.failed: 0` and
`result.build_failed: false`. Run the unit matrix in a separate job with `target: "lib"` and
`filter: "baked_gi"`. Test-job errors retain the existing `F0002` code and reasons
`run_tests_bad_name`, `run_tests_busy`, `run_tests_no_manifest`, `run_tests_spawn_failed`;
job lookup uses `F0003` with `job_not_found`. `BakedGiError` variants above are Rust loader
errors, not new JSON-RPC error codes. Farm hosts must configure the cargo program/environment
with their existing build-slot wrapper and lavapipe rules before running test jobs.
