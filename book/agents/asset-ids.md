# Asset ids for sources not in version control

A source's asset id lives in its `.meta` sidecar. A project that keeps its binaries (textures, glTF
files) out of version control usually keeps their sidecars out too, so a cook on a fresh checkout
mints new ids, every pak changes, and a patch ships every pak whole. The asset id manifest
`.assetids.ron` v1 ([`docs/formats/assetids-ron.md`](../../docs/formats/assetids-ron.md), ADR-8460)
is the committed record: one row per source with its path under `assets/`, id and importer. The
[user guide](../user-guide/asset-ids.md) has the CLI side (`regia assets ids export | import`,
`regia cook --ids`). Both methods need the `assets` permission.

## `assets.ids_export`

```json
{"jsonrpc":"2.0","id":1,"method":"assets.ids_export","params":{"out":"asset_ids.assetids.ron"}}
{"jsonrpc":"2.0","id":1,"result":{"file":"asset_ids.assetids.ron","written":true,"entries":2928,"rows":[]}}
```

- `out` (optional): the project-relative manifest to write; default `asset_ids.assetids.ron` at the
  project root.
- `dry_run` (optional, default `false`): write nothing and list the rows in `rows`
  (`{ path, id, importer }`, sorted by path).
- Every `.meta` under `assets/` whose source is on disk is a row; sidecars of deleted sources,
  AppleDouble `._*` files and `regia upgrade-project` staging folders are skipped. The text is
  byte-stable: export twice, get the same bytes; one changed source changes one line.

## `assets.ids_import`

```json
{"jsonrpc":"2.0","id":2,"method":"assets.ids_import","params":{"file":"asset_ids.assetids.ron"}}
{"jsonrpc":"2.0","id":2,"result":{"file":"asset_ids.assetids.ron","applied":true,
  "written":["models/crate.glb","textures/rock.png"],"kept":["materials/steel.material.ron"],
  "missing_source":[],"conflicts":[]}}
```

- `file` (required): the project-relative manifest to read.
- `dry_run` (optional, default `false`): report `written` without writing (`applied: false`).
- A row whose source is not on disk is skipped (`missing_source`); a sidecar that already names
  the recorded id is `kept` and not rewritten; a missing sidecar is written with the recorded id and
  importer (the sidecar a cook writes for a new source). `regia cook --ids <file>` runs the same
  step before it cooks.
- A sidecar that names another id, or does not read, is never replaced: the call fails with
  `asset_id_conflict`, writes **nothing**, and `data.conflicts` lists
  `{ path, recorded, existing, reason?, shared_with? }` (`existing` is `null` for an unreadable sidecar).
- Two sources on disk never get one id: two rows with one id whose sources both exist, or a row whose id
  the sidecar of another source on disk already names, are conflicts too (`existing` is `null`,
  `shared_with` is the other path and `reason` says why). A shared id is fine while the replaced source is gone (a JPEG that replaced a PNG).
- Two rows whose paths differ only in letter case are `asset_ids_invalid`: a Windows or macOS checkout
  holds one file for both.

To check a recook kept the ids, compare the two builds with `assets.pak_diff` (see
[Content chunks](content-chunks.md)): unchanged sources read `unchanged`, and only the paks of the
changed sources ship.

## Error codes

| code | meaning |
|---|---|
| `asset_id_conflict` | `assets.ids_import`: a source already has a sidecar with another id, or one that does not read, or two sources on disk would get one id (they would cook to one `{id}.fckd`; each row's `reason` names the other path); nothing was written (`data.conflicts`). |
| `asset_ids_unreadable` | `assets.ids_import`: the manifest file does not read (missing, not a regular file). |
| `asset_ids_invalid` | `assets.ids_import`: the manifest does not parse, names a newer version, or breaks a rule (an empty, absolute, `..` or `\` path, a path twice, two paths that differ only in letter case, a nil id, an empty importer, an unknown field). |
| `asset_ids_bad_meta` | `assets.ids_export`: a sidecar under `assets/` does not parse; the export never drops an id silently, so it stops. |
| `asset_ids_write_failed` | the manifest or a sidecar could not be written. |
| `path_outside_project` | `out` or `file` leads outside the project (`F0002`, the shared request-path rule). |

A missing `file` or a parameter of the wrong type is the universal `F0002`.
