# `.meta` sidecar (`meta`, v2)

Every source under `assets/` has a `<file>.meta` next to it: its asset id, importer, importer settings,
sub-asset ids and provenance (`docs/specs/12-assets.md` § 1, `docs/specs/23-agent-native.md` § 5). The
loader is `regia_asset::MetaSidecar`; the JSON Schema is [`meta.schema.json`](meta.schema.json) and the
example [`meta.example.ron`](meta.example.ron).

## Versions

| Version | Change | Migration |
|---|---|---|
| 1 | Frozen at M4. | — |
| 2 | The importer setting `lossless_clips` (ADR-7819). | v1 → v2 drops a `lossless_clips` field from `settings` (a v1 glTF importer ignored it, a v1 BVH importer refused it), so the switch is off; every other field keeps its value text and order. `regia upgrade-project --write` rewrites only the version number of a file whose settings never named the key (comments kept). |

A file with a newer `format_version` is refused with `unsupported meta format version`.

## `lossless_clips`

```ron
(
    format_version: 2,
    id: "67e55044-10b1-426f-9247-bb680e5fe0c8",
    importer: "gltf",
    settings: "(lossless_clips: true)",
)
```

`lossless_clips: true` keeps every animation clip the source produces lossless at cook: each clip (and a
BVH's retargeted copy) is a `cooked-clip` v3 payload with a `Raw` body, bit-exact to the clip the importer
decoded. Off (the default, or `false`) the clip's joint tracks are compressed (ADR-7227,
`docs/specs/17-animation.md` § 1). The glTF, FBX, USD and BVH importers read it; a folder rule in
`config/import_rules.ron` can set it for many sources. Import-time key reduction (the BVH importer's
`rotation_tolerance_degrees` and `translation_tolerance`) still runs: set those to 0 as well to keep every
authored key. A value other than `true` or `false` is a malformed setting, handled as each importer handles one (BVH: a cook error; glTF, FBX and USD: a warning and the default settings, so the clips compress). Flipping the switch recooks the
source on the next incremental cook; a source that leaves it off keeps its cache key.
