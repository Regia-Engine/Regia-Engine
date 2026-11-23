# `.assetids.ron` (asset id manifest, v1)

Proposed in ADR-8460 (plan row C-01). Written by `regia assets ids export` and the agent method
`assets.ids_export`, read by `regia assets ids import`, `regia cook --ids <file>` and `assets.ids_import`
(`book/agents/asset-ids.md`, `book/user-guide/asset-ids.md`); the type is
`regia_asset::asset_ids::AssetIdsManifest`. Frozen from its first release: a later shape change needs version 2,
a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a). No earlier version exists.
Registered in `cargo xtask check-formats` (`asset-ids`), the format corpus (`asset-ids/v1.ron`) and the fuzz-target
registry (`asset-ids`).

A source's asset id lives in its `.meta` sidecar. A project that keeps its binaries (textures, glTF files) out of
version control usually keeps their sidecars out too, so a cook on a fresh checkout mints new ids and every pak
changes. The manifest is the committed record of those ids: one row per source.

```ron
(
    format_version: 1,   // must be 1; a newer version is refused (asset_ids_invalid)
    entries: [           // one row per source, written sorted by path (a reader accepts any order)
        (path: "models/crate.glb", id: "5d0c8a3e-4b1f-4c62-9e07-3a41c2d87f10", importer: "gltf"),
        (path: "textures/rock_albedo.jpg", id: "2b6e91f4-07ad-4e35-8c19-d4a26b03e85f", importer: "jpg"),
        (path: "textures/rock_albedo.png", id: "2b6e91f4-07ad-4e35-8c19-d4a26b03e85f", importer: "png"),
    ],
)
```

- `path`: the source path relative to the project's `assets/` folder, `/`-separated. Refused: empty, absolute
  (a leading `/` or a drive `:`), `\`, an empty, `.` or `..` component, a control character, a name ending in
  `.meta`, a path listed twice, and two paths that differ only in letter case. Paths are case-preserving and
  case-sensitive on disk, but a case-insensitive checkout (Windows, macOS) holds one file for `Textures/Rock.png` and
  `textures/rock.png`, so the reader, the export and the import refuse two such rows with an error naming both
  (`asset_ids_invalid`; ASCII and Unicode simple case folding, no normalization; ADR-8553).
- `id`: the source's primary id (the `.meta` `id`); never nil. Sub-asset ids derive from it, so they need no row.
  Two paths may share an id (a JPEG that replaced a PNG keeps the PNG's id). The import refuses a shared id when
  both sources are on disk (the cook names its output `{id}.fckd`, so they would cook to one file; ADR-8553).
- `importer`: the importer the `.meta` names (`png`, `gltf`, `material_ron`, ...); never empty.
- Unknown fields are refused.

The writer emits exactly the text above: four-space indent, one row per line, rows sorted by path, strings
escaped as RON string literals, a final newline. Two exports of the same sidecars are byte-equal, and a changed
source changes one line.

Import writes, for each row whose source is on disk and has no sidecar, a `.meta` with the row's id and importer
(the sidecar a cook writes for a new source, current `.meta` version). It never replaces a sidecar: one that names
another id, or does not read, is a conflict (`asset_id_conflict`) and the import writes nothing. Two rows with one id
whose sources are both on disk, or a row about to be written whose id the sidecar of another source on disk already
names, are conflicts too: each such row is listed with a `reason` naming the other path, and nothing is written.
