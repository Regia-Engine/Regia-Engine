# `config/import_rules.ron` (import folder rules, v1)

Spec: `docs/specs/12-assets.md` § 10. Proposed in ADR-3321; read by the cook
(`regia_asset_pipeline::import_rules`, resolved at cook time), `regia assets import-rules check`
and the agent method `assets.import_rules`. Frozen from its first release: a later shape change
needs version 2, a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a).
No earlier version exists. Registered in `cargo xtask check-formats` (`import-rules`).

Folder rules give every source under a path glob the same partial importer settings without
editing each `.meta`: "every texture under `assets/ui/` stays uncompressed", "every mesh under
`assets/props/` cooks convex collision". Precedence, low to high: the importer's defaults, the
file-name inference (a standalone texture's `_n` suffix), the matching rules in file order (later
rules win field by field), then the explicit fields of the asset's own `.meta`.

```ron
(
    format_version: 1,                        // must be 1; a newer version is refused
    rules: [
        (
            glob: "assets/ui/**",             // project-relative glob: *, ?, ** across folders
            importer: "png",                  // optional filter (png, gltf, audio, ...); any importer when absent
            settings: "(compress: Some(false), max_size: Some(512))",
                                              // partial settings object; the .meta settings syntax:
                                              // Option fields are written with Some(..)
        ),
        (
            glob: "assets/props/**",
            importer: "gltf",
            settings: "(collision: ConvexDecomposition, collision_hulls: (max_hulls: 4))",
        ),
    ],
)
```

- Every rule field except `glob` and `settings` is optional. Unknown fields are errors
  (`deny_unknown_fields`), reported as `invalid_config` with the line and column.
- `settings` is a RON object string using exactly the syntax of an importer's `.meta` `settings`
  (the importer parses the merged result with its own `from_meta_string`). The supported
  importers and their fields: textures (`png`, `jpg`, `jpeg`, `tga`, `bmp`, `dds`, `exr`, `hdr`:
  `usage`, `max_size`, `compress`, `quality`, `stream`, `stream_min_px`, `hdr_encoding`,
  `cube_face_px`, `normal_green`, `orm_channel_order`), `gltf` (`generate_lods`, `meshlets`,
  `drop_double_sided_on_closed`, `vertex_ao`, `skin_influences`, `fracture`, `collision`,
  `collision_hulls`, `voxel_far`, `scale`, `forward`), `fbx`
  (`scale`, `import_animation`, `sample_rate`, `uv_set`, `limits`), `obj` (`scale`, `limits`),
  `bvh` (unit/axis/tolerances/retarget fields), `audio` (`normalize`, `target_lufs`).
- Globs: `*` any run except `/`, `**` any run including `/`, `**/` zero or more whole folders
  (`**/props/` matches `props/` and `a/b/props/`, never `myprops/`; ADR-5586), `?` one
  character except `/`; matched ignoring ASCII case, like `config/validation.ron`.
- The rules are resolved at cook time; a `.meta` keeps only its explicit overrides. The matched
  rules of each source are hashed into its cook key, so editing a rule (or adding one that
  matches) recooks exactly the matching sources on the next incremental cook.
- `regia assets import-rules check` reports each source's effective settings and which rule set
  each field, plus the findings `import_rule_unused` (a rule matches nothing) and
  `import_rule_unknown_field` (a field the matched source's importer does not know), and flags
  merged values the importer cannot parse. Exit 1 on findings, 2 on a broken file.
- `assets.import_rules { path }` (agent protocol) returns one source's effective settings with
  the source of each field.
