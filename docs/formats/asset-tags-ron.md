# `config/asset_tags.ron` (asset tag rules, v1) and `asset_registry.bin` (v1)

Spec: `docs/specs/content-data.md` § 2. Proposed in ADR-1466. Both files are registered in `cargo xtask check-formats` (`asset-tags`, `asset-registry`)
and `regia upgrade-project` since ADR-2316; the registry is a cook output, so a newer layout is cooked again rather
than migrated.

## Tag rules

```ron
(
    format_version: 1,            // must be 1
    rules: [                      // optional; applied in order
        (
            paths: ["assets/props/forest/**", "assets/trees/*.glb"],   // at least one glob
            tags: { "biome": "forest" },                               // key → value
        ),
    ],
)
```

- Unknown fields are errors (`deny_unknown_fields`), so a typo is reported instead of ignored.
- Globs match project-relative source paths: `*` any run of characters except `/`, `**` any run
  including `/` (`**/` also matches no folder), `?` one character except `/`; `dir/**` also matches
  `dir`. A sub-asset (`a.glb#mesh/X`) is matched by its source path (`a.glb`).
- Tag keys: 1 to 256 bytes, no `=`, no whitespace or control characters. Values: at most 256 bytes.
- Later rules override earlier ones key by key; rules override the automatic tags (`ext`,
  `folder`, `importer`).

## Registry file

`asset_registry.bin` in the cooked cache, `<pak stem>.registry.bin` next to a pak.

| Bytes | Content |
|---|---|
| 0..8 | magic `RGASREG\0` |
| 8..12 | format version, `u32` little endian (1) |
| 12.. | postcard `Vec<RegistryEntry>` sorted by path |

`RegistryEntry` (postcard field order): `id` (`AssetId`, 16 bytes), `path` (string), `type_path`
(string), `dependencies` (`Vec<AssetId>`, sorted, unique), `tags` (`BTreeMap<String, String>`),
`size` (`u64`). A reader rejects a newer version with `UnsupportedVersion`; the file is derived data,
so a cook rewrites it rather than migrating it.
