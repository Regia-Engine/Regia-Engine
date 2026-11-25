# Content formats

<!-- index: Content format -->
<!-- index: Migrator -->
<!-- index: upgrade-project | see: Content format -->
<!-- index: Frozen format | see: Content format -->

Learn which files are frozen, where their byte layouts live, and how to move a project
forward when the engine bumps a format. This page is the task. The version-bump checklist
for someone changing a format is
[Changing a content format](../../changing-a-content-format.md).

## What is frozen

A content format is a file the cook, the editor, or the game reads as data: `.level.ron`,
`.prefab.ron`, `.material.ron`, `.table.ron`, `.meta`, a pak, and the rest of the registry.
Each one has a version field. A loader accepts every older version and migrates it in
memory. It rejects a newer version with an error that tells you to upgrade the engine.
The serde shape of a format changes only together with a version bump and a migrator
(`docs/specs/12-assets.md` § 4a).

The list of record is generated:

- [Content formats](../../reference/formats.md) names every format, its current version, and
  its migrators, and links the note under `docs/formats/`. That page is generated. Do not
  edit it by hand.
- `docs/formats/format-hashes.ron` holds the schema hashes the merge gate checks. One note
  per format sits beside it, for example
  [`docs/formats/scene-files.md`](../../../docs/formats/scene-files.md) and
  [`docs/formats/content-manifest.md`](../../../docs/formats/content-manifest.md). Read
  those notes. Do not paste their sample bytes into another document. A sample can lag
  the current version. The hash file and the loader's `*_FORMAT_VERSION` constant are the
  version of record.

`regia export usda` is not in that list. The USDA stage is an exchange file. It has no
format version and no migrator.

## Move a project to this engine

From the project root, with the `regia` you want the project to match:

```console
$ regia upgrade-project --project .
$ regia upgrade-project --project . --write
```

The first command changes nothing. It lists every file whose stored version is older than
this engine, and any staging directory a killed `--write` left behind
(`.<name>.<pid>.stage`). The second command rewrites those files in place and keeps a
`.bak` of each original (a later run keeps that backup and writes `.bak.2`).

`--write` is two-phase. Every file is migrated first. If any migrator fails, the project
is unchanged, no `.bak` is left from that run, and the process exits non-zero with every
error. Only a fully migrated set is renamed into place.

A format the registry does not migrate is left as it is. Loaders still upgrade that file
in memory when they know how. Cooked outputs that have no migrator are rebuilt with
`regia cook`, not with this command. The release notes for the lines you crossed name
which formats need a re-cook.

To point the Cargo dependencies at a new engine checkout at the same time, pass
`--engine path:/opt/regia-engine-<version>` on both commands. The dry run lists the pin
change. `--write` applies it. The project refuses, and writes nothing, when it needs a
newer engine than this `regia`, or an older line than this `regia` still upgrades.

## When you change a format

A shape change needs all of the following in the same change. The checklist, the corpus
rule, and the one-step-per-release rule (ADR-7724) are on
[Changing a content format](../../changing-a-content-format.md).

- Bump that format's `*_FORMAT_VERSION` by one from the last release, or fold an additive
  change into a version that has not shipped yet.
- Register a migrator from the previous version, in the loader and in
  `regia upgrade-project`.
- Keep a fixture of the old bytes, and add the new version to the corpus.
- Re-record `docs/formats/format-hashes.ron` with `cargo xtask check-formats --update`.

Do not hand-edit a generated reference page to match. The lead runs
`cargo xtask check-formats --update` and `cargo xtask manual`.

## Finished check

- `regia upgrade-project --project .` exits 0.
- After `--write`, a second run with no `--write` lists nothing to migrate.
- `regia cook --project .` exits 0. A format the upgrade command does not rewrite, and
  that the release notes tell you to re-cook, is fresh after this cook.
- The game loads the startup level. A "newer than this engine" error means the file was
  written by a newer `regia` than the one you are running. Use that newer binary, or
  restore the `.bak`.

Data tables (`.table.ron`, `.table.csv`), the asset registry, and content chunks are
formats with their own chapters:
[Data tables](../../user-guide/data-tables.md),
[Asset registry](../../user-guide/asset-registry.md),
[Content chunks](../../user-guide/content-chunks.md).
