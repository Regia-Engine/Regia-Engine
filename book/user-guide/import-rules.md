# Import folder rules

`config/import_rules.ron` (v1) gives every source under a path glob the same importer settings
without editing each `.meta` — folder-level import policy for a large project. It is read by the
cook (the rules are merged at cook time), by `regia assets import-rules check`, and by the agent
method `assets.import_rules`. The full format reference is
[`docs/formats/import-rules-ron.md`](../../docs/formats/import-rules-ron.md); the design record is
ADR-3321.

```ron
(
    format_version: 1,
    rules: [
        (
            glob: "assets/ui/**",            // project-relative; * and ? stay in a folder, ** crosses
            importer: "png",                 // optional; any importer when absent
            settings: "(compress: Some(false), max_size: Some(512))",
        ),
        (
            glob: "assets/props/**",
            importer: "gltf",
            settings: "(collision: ConvexDecomposition)",
        ),
    ],
)
```

## How settings combine

From weakest to strongest:

1. the importer's defaults (and, for a standalone texture's usage, its file name: `_n` → normal
   map and so on),
2. the matching rules, in file order — a later rule overwrites an earlier one's field,
3. the explicit fields of the asset's own `.meta` `settings`, which always win.

The `settings` object uses exactly the syntax an importer's `.meta` `settings` string uses — note
the `Some(..)` around Option values. The covered importers and their fields: textures (`png`,
`jpg`, `jpeg`, `tga`, `bmp`, `dds`, `exr`, `hdr`), `gltf` (also applied to FBX and OBJ sources
after conversion), `bvh`, and `audio`.

The rules resolve at cook time: the `.meta` keeps only what a human wrote there, moving a source
to another folder simply picks up the rules its new path matches, and editing a rule recooks
exactly the matching sources on the next incremental cook (the matched rules are part of the
cook's cache key). A malformed `config/import_rules.ron` fails the cook before anything cooks,
like a bad `[cook.textures]` table.

## Checking a project

```sh
regia assets import-rules check            # text report; exit 1 on findings, 2 on a broken file
regia assets import-rules check --json     # machine-readable (agents, CI)
regia assets import-rules check --path assets/ui/**   # limit the reported sources
```

The report lists, per source with any rule or explicit settings, the effective settings and which
rule (or the `.meta`) set each field, followed by the findings:

- `import_rule_unused` — a rule matches no source in the project,
- `import_rule_unknown_field` — a rule sets a field the matched source's importer does not know
  (including importers that take no import rules at all, like material `.ron`),
- a "value problem" line when the merged settings do not parse as the importer's settings struct
  (for example `(compress: false)` where `(compress: Some(false))` was meant) — the cook would
  otherwise log a warning and silently cook with the defaults.
