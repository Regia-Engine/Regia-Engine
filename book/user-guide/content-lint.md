# Content lint

Geometry and material checks inspect authored glTF/GLB sources before cooking.
Geometry lint reports duplicate meshes, coincident triangles and rig-channel
warnings. Material lint checks factors and can explicitly repair metadata. Neither
check mutates cooked content. See the [geometry agent guide](../agents/gltf-lint.md)
and [material guide](../agents/gltf-material-lint.md) for their rules and commands.

## Resource policy

Both tools accept the same optional `settings.input_limits` object. Use the same
values in both policies when applying a project budget. The persistent shared
project-settings integration follows job 02; this change does not revise a frozen
project file. Defaults are:

| Setting | Default | Scope |
|---|---:|---|
| `max_encoded_bytes` | 268435456 | Aggregate admitted bytes read: JSON, headers and loaded buffer payloads. |
| `max_json_bytes` | 16777216 | Serialized JSON document/chunk and repaired JSON, before parsing/copy growth. |
| `max_external_bytes` | 268435456 | Sum of actual external-buffer file lengths, including trailing bytes. |
| `max_decoded_bytes` | 536870912 | Conservative decoded/normalized payload reservations and scratch. |
| `max_accessors` | 100000 | Declared accessor count, checked before building the parsed document. |
| `max_work` | 64000000 | JSON nodes, chunks, accessor components and normalization uses. |
| `max_copy_bytes` | 8589934592 | Bytes streamed into a repaired source. |

Zero is a valid ceiling: for example, metadata-only inspection can set external
and decoded bytes to zero. Existing stricter importer checks and geometry-specific
budgets still apply. Raising a shared limit does not bypass them. Reports expose
`input_limits` and `input_usage`, so automation can record the effective policy.
Counters describe deterministic admission reservations, not operating-system RAM
or allocator bookkeeping. JSON parser storage is bounded by JSON size and shape;
decoded payload accounting is separate. Reservations may conservatively count
shared accessors more than once and include normalization scratch and growth slack.

Material inspection reads the JSON and each GLB chunk header and seeks over the
binary tail. It never opens external buffers or decodes images. An oversized GLB
can therefore pass a small metadata budget. Repair copies binary/unknown chunks
through a 64 KiB block into an exclusive sibling temporary file, preserves file
permissions and checks for source changes before rename. Serialize writers to a
source: the final check and rename are not a filesystem compare-and-swap operation.

A budget refusal returns `content_lint_limit`, naming the exhausted setting.
The same code reports checked size overflow or a failed input-buffer reservation.
`content_lint_changed` means a source/external file changed during the operation;
retry against a stable source. Rust bounded APIs accept a cancellation probe and
return `content_lint_cancelled` between bounded IO, parse, material and normalization
units. Cancellation never publishes a partial fix or a successful partial report.
The synchronous agent methods stay request/response for a file at or under 1 MiB.
A larger file returns `content_lint_limit` and names `asset.content_lint_start`.
Save and import of `.gltf` or `.glb` queue one scan. The editor parses that scan off
the editor thread and publishes it on a later tick. See [Problems and thresholds](#problems-and-thresholds).

Run `cargo test -p regia_asset_pipeline --test content_lint_budget` for deterministic
resource assertions and `cargo test -p regia_editor --test content_lint_budget_agent`
for the actual agent workflow with `play.wait_until` and `play.expect`.

## Source lint policy

Geometry and material source lints can share one project policy. The file is
`config/source_lint.ron` (format `source-lint`, version 1). It does not replace
`config/validation.ron`, and it does not change the disk-free validation runner.

A missing policy file means the engine defaults. When the file is present, settings resolve
in this order. A present field replaces that field:

1. Engine defaults.
2. Project `defaults`.
3. The named target profile. Ask for `default` to use a profile of that name when one exists.
   Any other name must be a profile key.
4. Per-asset overlays, in file order. Globs are ASCII-case-insensitive. A later match wins.

Example: a mobile profile sets metallic to 0.2, and a later `assets/*.gltf` overlay sets it
to 0.9. `assets/hero.gltf` then uses 0.9. A path that matches neither glob keeps 0.2.

### What the report records

One evidence run covers the sources you pass in. The report version is 1. Each source is
`supported`, `unsupported`, or `incomplete`. Counts of triangles and meshes come from the
geometry lint. The material count comes from the document. A count that was not measured is
unknown. It is not stored as zero. A real zero is stored as a known zero.

The report is complete only when every requested source was classified and no finding cap
or linter budget stopped the run. Truncation and cancellation leave `complete` false and
list the omitted sources as incomplete. They are not treated as clean.

Findings use stable rule ids and locations. A suppression names one rule, one exact asset
path, and the BLAKE3 of that asset's bytes, plus a reason. It can also name one location.
It does not waive a different asset or a different rule. Suppressed rows stay in the report.

The same findings are written as compact JSON and as SARIF 2.1.0. Changing the target name
or a rule setting changes the evidence key, even when the numeric result would otherwise match.

### Authoring a suppression

```ron
(
    format_version: 1,
    suppressions: [(
        rule_id: "factor_missing",
        path: "assets/hero.gltf",
        content_hash: "<64 lowercase hex digits>",
        reason: "factors land in the next export",
    )],
)
```

`path` is exact. `Assets/Hero.gltf` is a different source. Uppercase hex is rejected.

Geometry and material call-local settings still exist for a single file at or under 1 MiB.
This policy is the project-wide layer the editor job and the cook and package steps read. See
[the agent page](../agents/content-lint.md) for start, cancel, status, read, cook, and package.

`asset.content_lint_rules` also returns `importer`, the extensions the glTF importer
cooks or keeps in a side table. Material texture, UV, color-space and extension-domain
checks are described in [Material coverage](material-coverage.md).

### Robust coincidence mode

The default coincidence check is fast but strict about its grid: two copies a few micrometers
apart can land in different cells and go unreported. It also skips strips and fans, reads only
the first UV set, and cannot tell a true copy from a differently skinned or shaded surface.

Set `coincidence_mode: Some(robust)` in a profile or asset overlay to run the robust check
instead. It compares neighboring cells, expands strips and fans, and compares the UV sets the
material really samples, plus normals, skin weights and morph targets. Each row says whether
the positions are an exact or a within-tolerance match and whether the other data agrees.
Only exact, fully equivalent copies are labeled `safe_delete_candidate`. Nothing is ever
deleted or welded for you.

Tolerances are in meters (`tolerance_meters`) or relative to the mesh size
(`tolerance_relative`, a fraction of the bounding-box diagonal). `max_candidate_pairs` bounds
the work; a mesh that needs more pairs fails with `gltf_lint_limit` instead of running for
minutes. The robust check also reports triangles with non-finite corners, repeated indices or
zero area, and leaves them out of the comparison.

The full field list is on [the glTF lint agent page](../agents/gltf-lint.md#robust-coincidence-mode-adr-8347).

## CLI, cook and package

The same adapter serves `regia assets validate`, `regia assets lint-geometry` and the cook and
package steps. It does not add a second validator. Cook and package never rewrite sources.

```text
regia assets lint-geometry assets/props --json --target default --report build/source-lint/geometry.json
regia assets lint-materials assets/props --profile compatibility --json
regia assets validate --strict --json --target default --changed changed.txt
regia cook --json
regia package --json
```

`lint-geometry` takes selected paths, `--changed`, `--target`, `--strict`, `--report`,
`--skip-staging` and `--cancel-file`. It has no `--fix`. A geometry warning
(`gltf_duplicate_mesh`, `gltf_coincident_triangles`, `gltf_rig_*`) fails the command even
without `--strict`. `--strict` also fails on any other unsuppressed warning.
`lint-materials` still accepts `--fix` and `--profile`. `--profile compatibility` is the
opt-in that records the same material rule ids the cook's source scan records, including
`factor_missing`.

`regia assets validate` runs the cooked-registry validators and then this scan. `--target`,
`--report` and `--cancel-file` apply to the scan. `--json` adds `source_lint` (`status`,
`report`, `evidence_key`, `rule_ids`, `findings`, `counters`). Exit 0 is clean, 1 is a
failure, 2 is a policy or target the scan cannot resolve.

Evidence is cached under `build/source-lint/`. The decode cache is keyed by the source bytes,
external buffer and image bytes, and the glTF importer version. The evidence cache also
includes the target and the resolved settings. A later edit that only changes a threshold
reuses the decode and writes a new evidence key. A warm run reports `source_parses` 0 and
`decoded_bytes` 0. The cache is a build file, not a frozen content format. Robust coincidence
mode needs attributes the decode cache does not keep, so a robust scan parses the source every
time and leaves the decode cache alone (ADR-8574).

`regia cook` and `regia package` run the scan before they publish cooked output. `cook: Off`
skips the scan (`lint.status` is `skipped`) and does not read `config/source_lint.ron`.
`cook: Warn` records geometry warnings and continues. `cook: Fail`, and `regia package` when
`config/validation.ron` does not name `cook`, stop before publication when an unsuppressed
geometry warning is present or the report is incomplete. Material warnings, including
`factor_missing` from the compatibility profile, are reported and do not by themselves stop
cook or package. `regia assets validate --strict` fails on any unsuppressed source warning.

A missing cooked registry still prints `validate: no asset registry for <platform>; skipped`
when the caller is cook and the file is absent. A file that exists but does not decode is an
error, and so is a required registry that is missing. Those errors do not say the check was
skipped. An invalid `config/source_lint.ron` fails every gate that runs.

`--cancel-file` that already exists stops the scan. The process prints one JSON object,
`{"ok":false,"error":{"code":"content_lint_cancelled","message":"..."}}`, and exits nonzero.
It does not publish a partial report.

## Problems and thresholds

The Problems panel keeps rows from other validators and replaces only rows whose validator
is `source_lint`. Each source-lint row uses the rule id as its code and a fix-preview link
as its hint. The panel does not rewrite the source.

Filter the list by severity, by validator, or by both. Choosing `warning` hides `error` and
`info`. Choosing validator `source_lint` hides every other source, including a row you added
by hand. `asset.content_lint_read` uses that same filter when you pass `severity` or `validator`.

Threshold editing writes `config/source_lint.ron`. A project-wide edit (`target` `project`)
stores the fields you set on project defaults. A named target such as `mobile` stores them
on that profile, creating the profile when it is new. Fields you leave blank stay on the
lower layer. The panel's provenance, and `asset.content_lint_policy`, name the layer that
supplied each field: `engine_defaults`, `project_defaults`, `target_profile`, or `asset_overlay`.

A burst of imports and saves queues one scan. Changing the file or the thresholds while a
scan is still running discards that scan's rows. Cancel finishes with `content_lint_cancelled`
and publishes nothing. A fix link is a preview. Applying it does not change the file. A
read-only session cannot edit thresholds or apply a fix.

## Texture budgets

Texture sources (`.png`, `.jpg`, `.jpeg`, `.bmp`, `.tga`, `.dds`, `.ktx2`) join the same
scan as glTF. The probe reads the container header. It does not decode pixels and it does
not resize the file. Five rule ids are separate from geometry reduction and from material
class checks:

| Rule | When it fires |
|---|---|
| `texture_wrong_compression` | A block family is not the one this platform's cook policy writes. Desktop expects BC. Android expects the `[cook.textures.android]` format (ASTC unless the project selects ETC2). |
| `texture_missing_mip` | A pass-through block chain keeps fewer levels than the pyramid of its surviving top mip. |
| `texture_impossible_dimensions` | A side is 0, a side is above 32768, the mip count cannot exist, or the expected RGBA size is above the decode budget (512 MiB unless a caller passes another ceiling). |
| `texture_usage_mismatch` | The container color space and the roles disagree, or one file is both an sRGB role (`ColorSrgb`, `Ui`) and a data role (`ColorLinear`, `NormalMap`, `Mask`). |
| `texture_budget_unknown` | The header cannot be measured. Strict evidence fails. The size is not reported as 0. |

A DDS pass-through keeps its last stored mip when `max_size` would otherwise drop it. A
64×64 file with levels 64 and 32 and `max_size: 8` reports 32×32. The `.meta` edit is the
existing importer setting. Reimport uses that setting. The linter does not write a new image.

Two sRGB roles on one sRGB texture are one shared source. The finding list does not rewrite
it. A color role and a data role on that same file are `texture_usage_mismatch`, and the
file bytes stay as they were.

`cook: Warn` records a texture warning and continues. `cook: Fail`, and package when
`config/validation.ron` does not name `cook`, also stop on an unsuppressed `texture_*`
warning or an incomplete texture report. Material warnings still do not stop those steps by
themselves. The project rationale is the platform policy plus each asset's `.meta`
(`usage`, `max_size`, `compress`). Those files already exist. `config/source_lint.ron` does
not grow a texture field.
