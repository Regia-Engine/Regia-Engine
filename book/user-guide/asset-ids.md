# Asset ids for sources not in version control

<!-- index: Asset id -->
<!-- index: .assetids.ron | see: Asset id -->

Every source asset gets a stable id the first time it is cooked; the id is stored in the source's
`.meta` sidecar, and levels, materials and paks refer to the asset by it. Many teams keep large
binaries (textures, glTF files) out of version control and fetch them from a store, and their
sidecars with them. A cook on a fresh checkout then finds no sidecar, mints a new id for every
such source, and every pak changes: a patch ships the whole game again.

The asset id manifest `.assetids.ron` is the small text file you commit instead
([format](../../docs/formats/assetids-ron.md)):

```ron
(
    format_version: 1,
    entries: [
        (path: "models/crate.glb", id: "5d0c8a3e-4b1f-4c62-9e07-3a41c2d87f10", importer: "gltf"),
        (path: "textures/rock_albedo.png", id: "2b6e91f4-07ad-4e35-8c19-d4a26b03e85f", importer: "png"),
    ],
)
```

## Export

```sh
regia assets ids export --project .                   # writes ./asset_ids.assetids.ron
regia assets ids export --project . --out ids/game.assetids.ron
regia assets ids export --project . --out -           # print it
```

One row per `.meta` under `assets/` whose source is on disk, sorted by path. Exporting twice gives
the same bytes, and a new or changed source changes one line, so the file diffs cleanly. A sidecar
that does not parse stops the export (`asset_ids_bad_meta`): fix it or delete it and cook first.

## Import

```sh
regia assets ids import asset_ids.assetids.ron --project . [--dry-run] [--json]
regia cook --project . --ids asset_ids.assetids.ron   # import, then cook
```

Import writes the sidecars the manifest records that are missing. It skips a row whose source is
not on disk, keeps a sidecar that already has the recorded id, and **never** replaces a sidecar
that names another id: it prints `asset_id_conflict` with each conflicting path and writes nothing.
Decide which id is right (usually the manifest's, if the sidecar was minted by a cook that ran
without it), delete the wrong sidecar or fix the manifest row, and import again. `regia cook --ids`
stops on the same conflict before it cooks.

## Checking a patch

After a fresh-checkout cook with `--ids`, compare the build with the previous one:

```sh
regia assets pak-diff dist/old/content dist/new/content
```

Unchanged sources read `unchanged`; only the paks that hold a changed source ship. The agent
methods are `assets.ids_export` and `assets.ids_import` ([agent guide](../agents/asset-ids.md)).
