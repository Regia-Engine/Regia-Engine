# Source lint policy and jobs

Read-only discovery and one editor job for the project source-lint policy (ADR-8248, ADR-8391).
The editor advertises the `content_lint` capability. Parsing of `.gltf` and `.glb` sources runs
off the editor thread. The Problems panel and `asset.content_lint_read` share one filter.

## Catalog and effective policy

`asset.content_lint_rules` takes no parameters. Permission: `read`. It returns `ok`,
`format_version`, `rules` (`id`, `producer`, `summary`, `unit`), sorted by id, and `importer`
(`name`, `status`). `status` is `cooked` or `side_table`. That list is the glTF
importer capability. An extension absent from it is unsupported.

`asset.content_lint_policy` takes optional `path` and optional `target`. Permission: `read`.

| Parameter | Meaning |
|---|---|
| `path` | Project-relative asset path. Optional. The file need not exist. Omit `path` to resolve project and profile defaults with no asset overlay. |
| `target` | Profile name. Optional. Omitted means `default`. `default` is always valid. Any other name must be a profile key. An empty `target` is an error. The value `project` is not a profile name on this method. |

```json,ignore
{"jsonrpc":"2.0","id":1,"method":"asset.content_lint_policy","params":{"path":"assets/hero.gltf","target":"mobile"}}
```

The result includes `ok`, `format_version`, `target`, `path` when you passed one, `precedence`
(always `engine_defaults`, `project_defaults`, `target_profile`, `asset_overlay`), `geometry`,
`materials`, `provenance`, `findings`, and `work` when a scan for that `target` has been published.

`provenance.geometry` names the layer that supplied `quantization`, `separation`,
`exact_separation`, `uv_distance`, `anisotropy_ratio`, `anisotropy_floor`, `max_vertices`,
`max_triangles`, `max_channels`, `max_animation_elements`, and `max_findings`.
`provenance.materials` names the layer for `default_metallic`, `default_roughness`,
`default_emissive_strength`, `default_volume_thickness`, `default_attenuation_distance`,
`classify_untextured`, and `name_rules`. `materials.profile` stays `engine_defaults`.

A missing `config/source_lint.ron` is the engine default, not an error. The frozen `source-lint`
v1 file and `SourceLintReport` are unchanged.

## Jobs

`asset.content_lint_start` takes optional `paths` and optional `target`. Permission: `read`.
It returns a job id and does not wait. One worker runs at a time. A second start while that
worker runs sets `queued` and does not start another thread. Save and import of `.gltf` or
`.glb` queue one coalesced scan; the next editor tick starts it when the worker is idle.

| Parameter | Meaning |
|---|---|
| `paths` | Project-relative `.gltf` or `.glb` paths. Optional. Omitted uses the coalesced queue. |
| `target` | Profile name. Optional. Omitted means `default`. |

`asset.content_lint_status` takes optional `id`. Permission: `read`. It does not block.
Idle is `content_lint_idle`. While the worker runs, `state` is `running`.

`asset.content_lint_cancel` takes optional `id`. Permission: `read`. Omit `id` to cancel the
current job. The code after the worker stops is `content_lint_cancelled`. No findings are published.

`asset.content_lint_read` takes optional `target`, `path`, `severity`, and `validator`.
Permission: `read`. It does not block. `severity` is `error`, `warning`, or `info`. `validator`
`source_lint` is the Problems source for these rows. The same two filters are what the panel shows.

The editor publishes a result only when the source generation, the settings generation, and the
BLAKE3 content hashes still match. A mismatch is `content_lint_stale` and adds no Problems rows.

## Thresholds and fix previews

`asset.content_lint_thresholds` requires the `assets` permission and is mutating. `target` is
required. `geometry`, `materials`, and `path` are optional. `target` `project` writes project
defaults; the returned view then resolves `default`. Any other name writes or creates that profile.
The write is the existing `config/source_lint.ron` overlay. A field you omit stays on the lower layer.

`asset.content_lint_fix_preview` takes `path` and optional `rule_id` and `location`. Permission:
`read`. It returns `previews` for published findings. Each preview href is
`content-lint://fix-preview?path=...&rule=...&location=...`.

`asset.content_lint_apply_fix` takes the same `path`, `rule_id`, and `location`. It requires
`assets` and is mutating, and it does not write the source. The result is `ok: false` with code
`content_lint_preview_only` and a `fix_preview` link.

Synchronous `asset.gltf_lint` and `asset.gltf_material_lint` stay request/response for a file at
or under 1 MiB. Above that they return `content_lint_limit` and name `asset.content_lint_start`.
`asset.gltf_material_fix` is not capped by that ceiling.

## Errors

`data.reason` is one of:

| Reason | When |
|---|---|
| `content_lint_params` | Unknown field, wrong type, or a missing required parameter. |
| `content_lint_policy` | The policy text or a resolved setting is unusable. |
| `content_lint_target` | `target` is empty or not a profile. |
| `content_lint_io` | The policy or a source cannot be read, or the worker did not start. |
| `content_lint_report` | A report could not be encoded. |
| `content_lint_limit` | A synchronous read is over the 1 MiB ceiling. |
| `content_lint_cancelled` | The job was cancelled. Nothing was published. |
| `content_lint_stale` | The source or the settings changed before publish. |
| `content_lint_idle` | No job has published findings for this read. |
| `content_lint_preview_only` | Apply was asked and did not write. |
| `path_outside_project` | `path` leaves the project. |

A read-only session is `F0012` (`PERMISSION_DENIED`) on `asset.content_lint_thresholds` and
`asset.content_lint_apply_fix`. The file bytes stay as they were.

## `play.expect` and `play.wait_until`

A headless workflow starts `asset.content_lint_start`, lets the editor tick publish the rows,
then puts the read counters on a reflected component. `play.wait_until` sees that condition.
`play.expect` checks the same condition and that gameplay is not paused:

```json,ignore
{"jsonrpc":"2.0","id":2,"method":"play.expect","params":{"assertions":[{"name":"running","condition":{"resource":"Time","path":"paused","op":"eq","value":false}}]}}
```

Insert `Time` before that assertion. `play.wait_until` is the wait; `play.expect` evaluates now.
An editor test that only checks the paused flag calls `play.expect` and does not pump
`play.wait_until`. Acceptance: `cargo test -p regia_editor --test content_lint_jobs`.

## Robust geometry controls

The `geometry` overlay in `config/source_lint.ron` also takes the robust coincidence controls
(ADR-8347): `coincidence_mode` (`legacy` or `robust`), `tolerance_meters`, `tolerance_relative`,
`uv_tolerance`, `attribute_tolerance`, `min_area_square_meters`, `max_candidate_pairs` and
`max_attribute_elements`. They merge like every other field. `asset.content_lint_policy` shows
the result as `geometry.mode` and `geometry.robust`. Units and defaults are in
[glTF geometry and rig lint](gltf-lint.md#robust-coincidence-mode-adr-8347).

```ron
(
    format_version: 1,
    profiles: {
        "review": (geometry: Some((coincidence_mode: Some(robust), tolerance_meters: Some(0.0002)))),
    },
)
```

Robust mode adds five geometry rules to `asset.content_lint_rules`:

| Rule | Severity | Location |
|---|---|---|
| `gltf_coincident_equivalent` | warning | `/meshes/<m>/primitives/<p>/triangles/<t>/keeper/<kp>/<kt>` |
| `gltf_coincident_advisory` | info | same |
| `gltf_mesh_nonfinite` | info | `/meshes/<m>/primitives/<p>/nonfinite` |
| `gltf_mesh_degenerate` | info | `/meshes/<m>/primitives/<p>/degenerate` |
| `gltf_mesh_zero_area` | info | `/meshes/<m>/primitives/<p>/zero_area` |

An advisory row means the positions agree but material, UV, normal, skin or morph data differs,
or some of it could not be read. It is never a safe-delete candidate. The message records the
agreement (`exact` or `tolerance`), the separation and tolerance in meters, the differences,
`safe_delete_candidate`, and the fingerprint. Robust rows are not projected back by
`gltf_report_from_findings`.

## Cook and package jobs

There is no new cook or package method. `agent.cook` and `agent.package` still run
`regia cook --json` and `regia package --json`. The job result's `report` is that object.
A finished scan adds `report.lint.status` (`skipped`, `clean`, `warnings` or `failed`) and
`report.lint.report` (the evidence JSON path, or null when the gate did not write one).

<!-- not-a-request -->
```json
{"ok":true,"pak_dir":"build/paks/linux","lint":{"status":"warnings","report":"build/source-lint/report.json"}}
```

A scan that stops the step is one object. `report.error.code` is `content_lint_failed`,
`content_lint_policy`, `content_lint_target`, `content_lint_io`, `content_lint_report` or
`content_lint_cancelled`. `report.lint` repeats `status` and `report`. A cancelled
`regia assets lint-geometry --json --cancel-file` prints only
`{"ok":false,"error":{"code":"content_lint_cancelled","message":"..."}}` and exits nonzero.

`play.expect` checks the session after the job. It does not read the lint file. Poll
`agent.job_status` for `report.lint`, then assert the session is still running:

```json,ignore
{"jsonrpc":"2.0","id":3,"method":"play.expect","params":{"assertions":[{"name":"running","condition":{"resource":"Time","path":"paused","op":"eq","value":false}}]}}
```

```json,ignore
{"jsonrpc":"2.0","id":4,"method":"play.wait_until","params":{"assertions":[{"name":"running","condition":{"resource":"Time","path":"paused","op":"eq","value":false}}]}}
```

Cook and package pass `fix: false`. They do not repair sources. `cook: Fail` and package
stop on an unsuppressed geometry warning or an incomplete report. Material warnings stay in
the report and do not by themselves stop those steps. `regia assets validate --strict` fails
on any unsuppressed source warning. See [Content lint](../user-guide/content-lint.md).

## Texture evidence

There is no new method. `asset.content_lint_rules` lists the texture rule ids with the
geometry and material ids. `asset.content_lint_start` and `asset.content_lint_read` include
`.png`, `.jpg`, `.jpeg`, `.bmp`, `.tga`, `.dds` and `.ktx2` sources. The rule ids are
`texture_budget_unknown`, `texture_impossible_dimensions`, `texture_missing_mip`,
`texture_usage_mismatch` and `texture_wrong_compression`. They are lowercase words with
underscores. A texture finding's `effect` is `preserves_appearance`.

`asset.content_lint_apply_fix` still returns `content_lint_preview_only` and does not write
the image. A `.meta` `max_size` change is an importer edit. The next scan sees it because
the evidence key includes that fingerprint. Unknown width, height and decoded bytes stay
unknown (`known: false`). They are not zero.

`cook: Fail` and package also stop on an unsuppressed `texture_*` warning or an incomplete
report. `play.expect` and `play.wait_until` still check the session after `agent.job_status`
reports `lint.status` and `lint.report`. Acceptance:
`cargo test -p regia_asset_pipeline --test texture_lint`.
