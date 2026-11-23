# `config/source_lint.ron` (source-lint policy, v1)

Spec: `docs/specs/32-asset-validation-and-audit.md` § 4.6 and § 13. Proposed in ADR-8248.
Read by `regia_asset_pipeline::source_lint`. The disk-free `Validator` trait and the frozen
`config/validation.ron` file are unchanged. This is a new v1 file: a later shape change needs
version 2, a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a).
No earlier version exists. Registered in `cargo xtask check-formats` as `source-lint`.

The evidence report is not this file. Its version is `SOURCE_LINT_REPORT_VERSION` (1). The
check-formats fingerprint includes both the policy structs and the report structs, so a report
field change fails the format check until that version is coordinated.

```ron
(
    format_version: 1,
    defaults: (
        geometry: Some((max_triangles: Some(100000))),
        materials: Some((default_roughness: Some(0.5))),
    ),
    profiles: {
        "mobile": (materials: Some((default_metallic: Some(0.2)))),
        "default": (materials: Some((default_roughness: Some(0.3)))),
    },
    assets: [
        (glob: "assets/*.gltf", materials: Some((default_metallic: Some(0.9)))),
    ],
    suppressions: [
        (
            rule_id: "factor_missing",
            path: "assets/hero.gltf",
            content_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            reason: "authored in the next pass",
            location: Some("/materials/0/pbrMetallicRoughness/metallicFactor"),
        ),
    ],
)
```

## Precedence

Resolution order, later layers win field by field:

1. Engine defaults (the geometry and material linter defaults).
2. `defaults` in this file.
3. The named target profile. The target `default` is always valid. A profile named `default`
   applies only when that key exists. Any other target must name a profile.
4. `assets`, in file order. A glob matches the project-relative path after both sides are
   ASCII-lowercased, using the import-rules glob. A present field replaces that field.
   `name_rules` is one field: `Some` replaces the whole list.

`default_volume_thickness` and `default_attenuation_distance` are nested options. Omit the
field to inherit. `Some(None)` clears the lower layer. `Some(Some(v))` sets a value.

A missing file is the engine default, not an error. Unknown fields are errors. Numeric
settings are checked when a target is resolved, with the same rules as the linters.

## Suppressions

A suppression matches one rule id, one exact path (not a glob, not case-folded), and the
lowercase BLAKE3 hex of that source (64 digits). The reason is required. `location`, when
set, must also match the finding location. The first match in file order supplies the reason.
A duplicate identity is a policy error. A suppression for one asset or rule does not waive
another. A stale hash does not apply. Suppressed findings stay in the report.

## Report version

`produce` writes report version 1. A material finding may include `effect`
(`preserves_appearance` or `changes_appearance`). Geometry findings omit it.
A missing `effect` is unknown: readers do not invent a repair class. `complete` is false when the run is truncated, cancelled,
finding-capped, or a geometry budget stops early. A missing metric is unknown (`known: false`,
no `value`), never zero. A measured zero is `known: true` with `value: 0`. The same findings
encode as compact JSON and SARIF 2.1.0. The evidence key is the BLAKE3 of the target name,
the processed-source hash, the resolved-settings hash and the tool hash.

## Robust geometry fields (ADR-8347)

The geometry overlay also accepts `coincidence_mode` (`legacy`, the default, or `robust`),
`tolerance_meters` (meters), `tolerance_relative` (fraction of the mesh bounding-box diagonal),
`uv_tolerance` (UV units), `attribute_tolerance` (normal, weight and morph-delta units),
`min_area_square_meters` (square meters), `max_candidate_pairs` and `max_attribute_elements`.
All are optional and layer like the other geometry fields. They were added to v1 before v1
was released. An omitted field inherits the lower layer, and the legacy predicate stays the
default.
