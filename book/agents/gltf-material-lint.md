# glTF material authoring lint

Check source material metadata before cooking. The default profile, `standard`,
keeps a valid glTF appearance: factor multiplication, omitted metallic and
roughness factors, and surface tint stay as authored. The opt-in profile
`compatibility` restores the older ORM, dielectric and volume-tint conventions.
Those conventions change appearance and require that explicit selection.
The tool does not validate geometry or image bytes. It checks material texture
indices, UV channels, slot color space and the extension domains below. The
normal importer still performs its own checks.

```sh
regia assets lint-materials assets/props --json
regia assets lint-materials assets/props --fix --json --skip-staging
regia assets lint-materials assets/props --profile compatibility --fix
regia assets lint-materials assets/props/part.glb --settings material-policy.json --fix
```

Text output starts with `profile: <name> — <summary>` and prints each finding as
`effect code pointer message`. `effect` is `preserves_appearance` or
`changes_appearance`. JSON output is
`{ok, profile, policy, importer, files:[{path, ok, report}]}`. `profile` and `policy`
name the effective profile. A file failure has `error:{code,message}` instead
of `report`. `--profile` overrides a `profile` field inside `--settings`.

Supply one or more `.gltf`/`.glb` files or directories. Directories are recursive;
symlink directories are not followed. `--skip-staging` excludes underscore-prefixed
entries beneath each directory root. Check exits unsuccessfully on findings or
file errors. Fix exits successfully when all selected files are repaired or clean.
An empty selection fails. Multiple files are processed independently: a failure
does not roll back earlier files.

## Migration from previous defaults

Releases that followed ADR-8061 applied the compatibility conventions on every
default lint and fix. A textured `metallicFactor` of 0.35 and `roughnessFactor`
of 0.7 was rewritten to 1 and 1. A missing untextured `metallicFactor` was
written as 0. A non-white base color was copied into `attenuationColor` and
replaced with white whenever `KHR_materials_volume` was present, including a
thickness of 0.

Restore that behavior with `--profile compatibility` or `"profile": "compatibility"`
in the settings object. Leave the profile unset, or set `"profile": "standard"`,
to keep valid authored appearance. See the
[material safe-policy guide](../user-guide/material-safe-policy.md).

## Agent workflow

The editor advertises `gltf_material_lint` in `agent.hello` capabilities and
registers these methods with `agent.schema`:

```json,ignore
{"method":"asset.gltf_material_lint","params":{"path":"assets/props/part.gltf"}}
{"method":"asset.gltf_material_fix","params":{"path":"assets/props/part.gltf","settings":{"profile":"compatibility"}}}
```

Both take an optional `settings` object using the policy below. `profile` lives
inside `settings`. Inspection requires Read permission; repair requires Assets.
Only `asset.gltf_material_fix` is mutating; read-only clients cannot invoke it.
Check refuses a `fix` parameter. Paths must stay within the project, including
resolved symlinks.

The result is `{ok, path, profile, compatible, extensions, texture_roles, extension_nodes, extension_bytes, findings, remaining, changed, input_limits, input_usage}`.
`profile` is `standard` or `compatibility`. Every finding has
`{code, material, pointer, message, effect}`. `material` is the zero-based index
or null for a root declaration. `pointer` is a JSON pointer. `effect` is
`preserves_appearance` or `changes_appearance`. Check returns its original
findings as `remaining`; fix reports the post-repair findings there. `ok` is true
only when `remaining` is empty and `compatible` is true. `compatible` is false
when `extensionsRequired` names an extension the importer cannot read. That is a
result field, not an RPC error, and the extension is not removed. A second fix
of the same file and settings returns `changed:false`.

This check reads a source file. It does not advance the play clock, so
`play.wait_until` and `play.expect` do not observe it. The workflow assertion is
the method result: `ok`, `changed`, `profile`, and each finding's `effect`.
The editor dispatcher test `agent_default_profile_preserves_valid_textured_factors`
runs that check and a second fix through the real agent inbox.

## Policy

Policy files and agent settings use the same JSON object. All fields are optional;
unknown fields and invalid numbers are refused. The defaults are:

```json
{
  "profile": "standard",
  "default_metallic": 0.0,
  "default_roughness": 1.0,
  "default_emissive_strength": 1.0,
  "default_volume_thickness": null,
  "default_attenuation_distance": null,
  "classify_untextured": false,
  "name_rules": []
}
```

`standard` is the default. It clamps factors into [0, 1], declares material
extensions, and writes the specification IOR 1.5 when transmission omits
`KHR_materials_ior`. It does not rewrite a valid factor product, does not insert
an omitted metallic or roughness factor, and does not move surface tint into
attenuation. An omitted `metallicFactor` keeps the glTF semantic value 1. An
omitted `roughnessFactor` keeps the glTF semantic value 1. A thickness of 0, or
an omitted thickness, stays a thin surface: no attenuation color is invented.

`compatibility` is opt-in. It forces both metallic-roughness factors to 1 when
a packed map is present, fills a missing untextured factor from
`default_metallic` and `default_roughness` (0 and 1), and, when
`KHR_materials_volume` is present, copies a non-white base color into
`attenuationColor` if that color is absent and replaces the base RGB with white.
That migration runs for a thickness of 0 as well. Those actions are labeled
`changes_appearance`.

Name rules and `classify_untextured` are explicit artistic selections on every
profile. They do not require `compatibility`. Name rules are optional,
first-match-wins, case-insensitive substring matches. Each requires a nonempty
`contains`; `metallic`, `roughness` and `emissive_strength` are optional. For
example:

```json
{"name_rules":[
  {"contains":"coated","metallic":0.0,"roughness":0.8},
  {"contains":"bare metal","metallic":1.0,"roughness":0.4},
  {"contains":"indicator","emissive_strength":12.0}
]}
```

An ORM map takes priority over name rules. A matching `metallic:0` changes an
explicit untextured metalness of 1 to 0 and is labeled `changes_appearance`.
Other explicit untextured factors survive unless the compatibility profile
resets an ORM factor. Set `classify_untextured:true` to apply the dielectric-at-1
guard to unmatched names using `default_metallic`. A rule's emissive strength is
a minimum for an emissive material; raising strength above the energy-equivalent
value is `changes_appearance`. Nonemissive materials remain dark.

Optional volume defaults supply missing dimensions only while a compatibility
tint migration runs. Thickness is in mesh coordinates (meters before node
transforms) and nonnegative; attenuation distance is in world-space meters and
positive. Both must be finite. Null leaves the glTF defaults (thickness 0,
infinite attenuation distance). Existing authored values win. The tool does not
infer thickness from geometry.

HDR normalization divides out-of-range nonnegative emissive color by its peak
and multiplies `emissiveStrength` by that peak. Each nonnegative channel's
color-times-strength product matches the authored product within
`EMISSIVE_PRODUCT_REL_TOLERANCE` (1e-9 relative to `max(1, |product|)`). Negative
channels are clamped to 0. That product-preserving repair is
`preserves_appearance`. An emissive texture with no strength receives the
documented default strength.

Factor defaults must be in [0, 1]. Emissive strengths must be finite and
nonnegative. No project name list, brightness target or volume dimension is
built into the tool.

The factor and volume rules follow the public glTF 2.0 material model
(<https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#materials>) and
KHR_materials_volume, section Base Color and Absorption
(<https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_volume/README.md#base-color-and-absorption>).
ADR-8250 records the conflict with the previous default repair.

## Rules and errors

| Code | Effect | Meaning and fix |
|---|---|---|
| `factor_missing` | `preserves_appearance` when the written value equals the glTF default; `changes_appearance` when it does not | Compatibility, or an explicit name rule, writes a missing factor. Standard leaves an omitted factor omitted. |
| `factors_in_range` | `preserves_appearance` | Clamp metallic, roughness, base RGBA, transmission and volume attenuation color to [0, 1]. Normalize HDR emissive RGB by its peak and clamp negative components. |
| `orm_factors` | `changes_appearance` | Compatibility only. Set metallic and roughness factors to 1. Metallic 1 with roughness 1 is already correct. |
| `metallic_name_policy` | `changes_appearance` | A name rule or opt-in fallback classifies untextured metallic 1 as dielectric; set it to 0. |
| `emissive_strength` | `preserves_appearance` when strength equals the energy product; `changes_appearance` when a name-rule minimum raises it | Supply a finite nonnegative strength. HDR repair multiplies the existing strength by the RGB peak. |
| `transmission_base_white` | `changes_appearance` | Compatibility only. Copy a non-white base RGB into `attenuationColor` when absent, then set the base RGB to white. Alpha and existing volume values stay. |
| `transmission_ior` | `preserves_appearance` | Add the standard IOR 1.5 when transmission has no IOR extension. |
| `extensions_used` | `preserves_appearance` | Append missing material-extension declarations, including nested `textureInfo` extensions such as `KHR_texture_transform`. Retain existing declarations and `extensionsRequired`. |

The `effect` field is the source of truth. A code such as `factor_missing` can
preserve or change appearance depending on the value that was written.

Handler errors use `F0002` with lowercase `data.reason`:
`gltf_material_params` (bad method parameters), `gltf_material_settings`
(invalid policy, including an unknown `profile`), `gltf_material_invalid`
(invalid material JSON/GLB), `gltf_material_io` (file operation failure),
`content_lint_limit` (a shared admission ceiling), `content_lint_changed`
(the source changed during the pass), `content_lint_cancelled` (the caller
stopped the pass), or `path_outside_project` (the shared path policy).
Permission failures use the dispatcher's `F0012`.

Malformed material shapes and types are never guessed: no part of that document
is rewritten. JSON and GLB metadata use the explicit shared limits below; binary tails
are skipped during inspection and streamed during repair.
Fix uses an exclusive sibling temporary file and rename, retains file permissions,
and checks for a changed source before replacement. Serialize writers to each
source; the check is not a filesystem compare-and-swap transaction. Direct
symlink source fixes are refused. Unchanged files retain their exact bytes.

Fix modifies material JSON and the `extensionsUsed` list. It retains unknown
fields and extensions, GLB non-JSON chunks, accessor declarations and external
URIs. It never reads or writes external buffer or image files. JSON formatting
may change on repair. Source fixes are file operations, outside the editor undo
stack; keep authored sources in version control.

Rust hosts use `regia_asset_pipeline::gltf_material_lint::{lint_materials,
lint_material_file, MaterialLintSettings, MaterialLintProfile, MaterialNameRule,
MaterialLintReport, MaterialFinding, MaterialRepairEffect, MaterialLintError}`.
`EMISSIVE_PRODUCT_REL_TOLERANCE` and `MAX_FINDINGS_PER_MATERIAL` bound the
emissive product check and the finding count. This desktop pipeline API does not
enter runtime or simulation dependencies. No cooked format or migrator changes.

## Shared input budgets (ADR-8226)

Both CLI policy files and agent `settings` accept `input_limits`, with the defaults
and counter definitions in [Content lint resource policy](../user-guide/content-lint.md#resource-policy).
The report includes the effective `input_limits` and `input_usage`. A large GLB
with small JSON needs only its JSON and chunk-header read budget for check mode.
Missing external buffers/images do not prevent factor inspection; none are opened.

`content_lint_limit` replaces the old size-refusal code and names the exhausted
setting. `content_lint_changed` refuses a source changed during inspection or fix.
`lint_material_file_bounded` accepts a `ContentBudget` cancellation probe and returns
`content_lint_cancelled`; cancellation removes staging files and does not publish
partial changes. These codes are lowercase `data.reason` in agent errors. The
synchronous agent methods retain their existing request/response model.

Run `cargo test -p regia_asset_pipeline --test content_lint_budget` and
`cargo test -p regia_editor --test content_lint_budget_agent` for allocation/read
counters, cancellation, changed-file checks and a dispatcher workflow verified by
`play.wait_until`/`play.expect`. `content.gltf_bounded` is the fixed-ceiling byte fuzz
harness in the editor and guided fuzz registry; no external IO is available to it.

## Material coverage (ADR-8392)

Texture index, UV channel, slot color space and extension numeric checks are
findings. They are not repaired. The only automatic metadata write added for
coverage is `extensionsUsed` declaration completion. Unsupported rendering
features, including `KHR_texture_transform`, are never stripped.

`texture_roles` lists each used texture slot. `color_space` is `srgb`, `linear`,
or `normal`, from the slot. The image file name is not consulted. A finding
`texture_color_space` is recorded only when one in-range texture index is
required in two of those spaces. An out-of-range index is `texture_index`, and
only when the `textures` array is present. A missing `textures` array is a
partial authoring document, not a bad index.

`uv_channel` points at the material `texCoord` (or the overriding
`KHR_texture_transform` `texCoord`) and names the mesh and primitive that lack
`TEXCOORD_n`, or whose accessor is missing, out of range, or not `VEC2`.
A negative or fractional `texCoord` is the same code and is left unchanged. A
non-number is `gltf_material_invalid` and the document is not rewritten.

`volume_thickness` (`thicknessFactor` < 0), `attenuation_distance`
(`attenuationDistance` <= 0) and `ior_domain` (`ior` < 1) stay in `remaining`.
Omitted `attenuationDistance` (the spec default is infinity), thickness 0, and
`ior` of 1 or 1.5 are not findings. `extension_dependency` reports
`KHR_materials_volume` without `KHR_materials_transmission` and does not add
transmission.

`extensions` lists each name in `extensionsUsed` or `extensionsRequired` after
declaration completion. `status` is `cooked`, `side_table`, or `unsupported`.
`required` follows `extensionsRequired`. Unknown optional extensions stay
byte-identical and `compatible` stays true. A required name with status
`unsupported` sets `compatible` false. The same table is `importer` on
`asset.content_lint_rules` and on the CLI JSON root. It is
`gltf_normalize::extension_support`.

The extension walk stops at `MAX_EXTENSION_DEPTH` (32) or `MAX_EXTENSION_BYTES`
(65536). Past either bound the finding is `extension_nesting`, the counters
stop at the cap, and the unread tail is not changed. `extension_nodes` and
`extension_bytes` are the counters.

New finding codes are not `data.reason` values. `compatible: false` is not an
RPC error. This check still does not advance the play clock. Assert `ok`,
`compatible`, and the finding `code` on the method result. `play.expect` and
`play.wait_until` remain the way to assert a gameplay flag such as
`Time.paused == false`. The editor test `content_lint_agent` calls
`play.expect` after reading the catalog.

CLI JSON is `{ok, profile, policy, importer, files:[{path, ok, report}]}`.
Text output prints `compatible:` and one `extension` line per name.
`ok` for the process and for each file is `remaining` empty and `compatible`.
