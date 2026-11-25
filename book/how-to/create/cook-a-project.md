# Cook a project

<!-- index: Cook -->
<!-- index: Incremental cook -->
<!-- index: Pak -->
<!-- index: Cooked pack | see: Pak -->

Turn the files under `assets/` into the cooked cache and the `.fpak` archives a packaged
game loads. The same load path reads the cache in development and the paks in a package.
What each importer produces is in [Importing assets](../../user-guide/importing-assets.md).
How a pak is checked for a shipping build is in
[Cooked packs](../../user-guide/cooked-packs.md).

## Before you start

Run this from the project root (the directory that contains `Regia.toml` and `assets/`).
`regia cook` defaults `--project` to the current directory.

## Steps

1. Cook for this machine.

   ```console
   $ regia cook --project .
   ```

   `--quality` defaults to `final`: paks are compressed, and the provenance-policy gate runs
   when the project asks for it. For a fast iteration pass, compress nothing:

   ```console
   $ regia cook --project . --quality fast
   ```

   `--quality fast` does not change the texture encoding stored in each `.meta`. It turns
   off pak compression.

2. Cook another platform when you need its texture encoding or its pak. Accepted names are
   `linux`, `windows`, `android`, and `macos`. Linux, Windows, and macOS textures cook as
   BC. Android textures cook as ASTC. Omit `--platform` to cook for the host.

   ```console
   $ regia cook --project . --platform android
   ```

   macOS is a testing target of the engine (ADR-009, ADR-264). This cook flag writes a
   macOS cooked cache. It does not by itself make a shipping macOS package. Platform
   limits are in [macOS](../../user-guide/macos.md).

3. Rebuild from scratch when a cache should not be reused.

   ```console
   $ regia cook --project . --clean
   ```

   Without `--clean` the cook is incremental. An output is rebuilt when the source bytes,
   the import settings, or the importer version change. An unchanged output is left in place.

4. For a shipping desktop or Android pak set, cook with `--ship` before `regia package`.
   `--ship` stores desktop meshes in the renderer's vertex layout (Android meshes stay
   compact), writes `build/paks/<platform>/cooked_pack.json`, and fails the command when a
   pak entry would have to be transcoded at load. The rules are on the cooked-packs page.

   ```console
   $ regia cook --project . --platform linux --ship
   ```

   `regia cook --pack-report` prints that report for paks already on disk and does not cook.
   It exits non-zero when a pak has an error finding.

## Where the files go

| Path | What it is |
|---|---|
| `.regia/cooked/<platform>/` | Cooked cache for that platform, plus `asset_manifest` data and `asset_registry.bin`. |
| `build/paks/<platform>/*.fpak` | One pak per content chunk. With no chunk rules, `base.fpak`. |
| `<source>.meta` | Asset id and import settings. Commit these. |

A file the cook has no importer for is skipped with `unsupported source extension`. It is
not in the pak. The game can still read it by path through `assets://` if your code opens
it. Any other error line fails the cook.

## Finished check

- The command exits 0.
- `build/paks/<platform>/base.fpak` exists (or one `.fpak` per chunk, when
  [chunk rules](../../user-guide/content-chunks.md) assign assets).
- A second `regia cook --project .` with nothing changed prints no fatal lines and does not
  rewrite unchanged outputs.
- `regia assets query --type CookedMesh` lists at least the meshes you expected, when the
  project has a cooked mesh.

`regia cook --stats` prints terrain and texture-format statistics and does not cook.
`regia cook --ids ids/game.assetids.ron` restores missing `.meta` sidecars from an id
manifest before cooking, and stops with `asset_id_conflict` when a sidecar names a
different id. Nothing is cooked in that case. See
[Asset ids](../../user-guide/asset-ids.md).

## After an engine update

New engine builds still load older content and migrate it in memory. To rewrite the project
on disk, run `regia upgrade-project` with no `--write` and read the list, then
`regia upgrade-project --write`. The procedure and the version rule are in
[Content formats](content-formats.md).
