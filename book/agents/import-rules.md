# Import folder rules (agents)

`assets.import_rules` resolves one source's **effective import settings** — the same resolution
the cook keys on — and says which layer set each field: the asset's `.meta` or a rule of the
project's `config/import_rules.ron` (see the
[user guide](../user-guide/import-rules.md) and
[`docs/formats/import-rules-ron.md`](../../docs/formats/import-rules-ron.md)).

```json
{"jsonrpc":"2.0","id":1,"method":"assets.import_rules","params":{"path":"assets/ui/icon.png"}}
{"jsonrpc":"2.0","id":1,"result":{
  "path": "assets/ui/icon.png",
  "importer": "png",
  "settings": "(compress: Some(false), max_size: Some(1024))",
  "matched_rules": [{ "index": 0, "glob": "assets/ui/**", "importer": "png" }],
  "fields": [
    { "field": "compress", "value": "Some(false)", "source": { "rule": 0, "glob": "assets/ui/**" } },
    { "field": "max_size", "value": "Some(1024)", "source": "meta" }
  ]
}}
```

- `path` (required): the project-relative source path, as in the asset manifest.
- `importer` (optional): resolve as this importer (`png`, `gltf`, `audio`, ...); default the
  `.meta`'s importer, else the extension's (`glb` cooks as `gltf`, sounds as `audio`).
- Fields no layer sets are absent from `fields`; the importer's default (and a standalone
  texture's file-name inference) fills them. A field's `source` is `"meta"` when the asset's own
  `.meta` sets it (explicit fields always win) or `{ rule, glob }` for the last matching rule
  that set it.
- `settings` is the merged object exactly as the importer's `from_meta_string` would parse it —
  pass it to nothing; it is the report of what the next cook uses.

Project health (every source at once, plus `import_rule_unused` / `import_rule_unknown_field`
findings and unparseable values) is `regia assets import-rules check [--json]` — prefer it over
per-path agent calls for audits.

## Error codes

| code | meaning |
|---|---|
| `invalid_config` | `config/import_rules.ron` does not parse or has a malformed rule (the cook fails the same way before cooking anything). |
| `meta_settings_invalid` | the asset's `.meta` `settings` string is not a parseable settings object. |

A missing `path`, an unsafe path (`..`, absolute) or a non-string `importer` is refused with the
universal `F0002` (`invalid_params`). A path that leads out of the project through a folder link is
`F0002` with `data.reason` `path_outside_project` (the shared request-path rule, ADR-5597).

For a 16-bit grayscale PNG also used as a glTF texture, `settings: "(heightfield: Some(true))"`
explicitly retains its standalone Heightfield output (ADR-8143). Without this opt-in a referenced
image cooks only as a shared material texture. The image's `.meta` can override the rule;
`heightfield: Some(false)` disables heightfield decoding. The existing `assets.import_rules`
response reports the field and its source. See [the heightfield example](cooked-packs.md#explicit-heightfield-sidecars-adr-8143).
