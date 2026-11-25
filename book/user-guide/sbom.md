# Bill of materials and licence audit

<!-- index: SBOM -->
<!-- index: CycloneDX -->
<!-- index: THIRD_PARTY_NOTICES -->

A game that ships on Regia Engine carries third-party code and binary assets. This page shows how to
list them, and how the repository keeps that list honest. Decision record: ADR-2760.

`regia package` also writes `sbom.json` into the package (names, versions, and licence identifiers
of the crates that platform resolves). That file is the copy that ships. `cargo xtask sbom` is the
CycloneDX document you archive for the engine or for one game's feature set. Both are described
below and on [Packaging](packaging.md). The licence of the engine itself is
[Licensing](licensing.md).

## The bill of materials

```text
cargo xtask sbom [--profile engine|game] [--out FILE] [--timestamp]
                 [--features CRATE/FEATURE,...] [--all-features]
```

Writes a [CycloneDX](https://cyclonedx.org/) 1.5 JSON document built from the resolved dependency
trees, the same ones `cargo xtask notices` reads:

- **Components of type `library`:** every third-party crate with its version, `pkg:cargo/` package URL,
  SPDX licence expression, repository and the SHA-256 checksum from `Cargo.lock`.
- **Ported source:** one component per entry of `docs/ported-sources.toml` (`regia:kind` is `ported-source`).
- **Components of type `data`:** one per row of `docs/asset-licences.toml` (`regia:kind` is `asset`), with
  its origin, whether it ships, and how many tracked files the row covers.
- **Dependencies:** the graph between the crates; the root depends on what the workspace crates use.

Profiles:

| Profile | Covers |
|---|---|
| `engine` (default) | Everything Regia Engine ships: engine, editor and CLI with every optional feature, and all asset rows. This is the set `THIRD_PARTY_NOTICES.md` covers (ADR-5556): the engine is delivered with every feature selectable. |
| `game` | What a shipped game links: runtime crates and samples, no editor or pipeline crate or their dependencies (the `check-layers` rule), ported sources that live only in editor or pipeline crates, and only the asset rows that ship. Each crate is resolved with its default features. The `regia` umbrella also resolves the optional `media-codecs`/`media-codecs-asm` features, so the codec crates a media-enabled game can link (`rav1d`, `unsafe-libopus`, `lewton`) are present. A crate behind any other off-by-default feature is not listed unless the game turns that feature on (ADR-5903). |

The `game` bill describes one build, so name the optional features your game turns on: `--features
regia_steam/steamworks,regia_net/net-quic` (comma-separated or repeated, always `crate/feature` with a runtime crate) adds
them to that crate's tree. Without them, a game that links `regia_steam` gets no `steamworks-sys` in its bill, because
a build without `regia_steam/steamworks` does not contain it. `--all-features` lists every optional feature of every
runtime crate, the most a game could link. The `engine` profile takes neither flag. The document records the feature set
in `metadata.properties` as `regia:features`: `all`, or `default + ` followed by the added features.

The output is deterministic (sorted, no timestamp, the serial number comes from the content), so two runs on
one lockfile are byte-identical. `--timestamp` adds `metadata.timestamp` for a release record. Without
`--out` the document goes to stdout.

## Asset licences

Every binary asset tracked in the repository outside `docs/`, `spikes/` and `vendor/` (fonts, images, audio,
video, glTF, LUTs, cooked fixtures; the extension list is `ASSET_EXTENSIONS` in `xtask/src/notices.rs`) has a
row in `docs/asset-licences.toml`: a path or glob, `origin` (`authored`, `generated` or `third_party`), the
source, an SPDX `licence` and whether the file `ships`.

`cargo xtask notices --check` fails when a tracked binary file has no row, when a row matches no file or a
field is empty. A row with `licence = "unknown"` is a warning that lists the files; `--strict` turns the
warnings into failures. Never guess a licence: write `unknown` and ask the owner.

A file whose row is `origin = "third_party"` and `ships = true` also needs a `[[notice]]` row (ADR-8888): a
`name`, the `licence`, the `files` it covers, and either the `licence_file` that ships beside them or `ported`
(the `project` of a `docs/ported-sources.toml` entry). `cargo xtask notices` writes these rows into
`THIRD_PARTY_NOTICES.md` as "Bundled fonts and assets", with each TrueType/OpenType font's own copyright record
(name ID 0 of its `name` table) followed by the other `Copyright` lines of its licence file, and the full SIL Open
Font License 1.1 text once (ADR-10066: the licence file must carry each covered font's own line, or the check
fails, so a sidecar cannot state another year or holder than the font); `cargo xtask demo-notices` adds the ones the demo's crates
compile in. Both fail when a shipping third-party file has no `[[notice]]` row or a row is incomplete.

## Notices check without network

`THIRD_PARTY_NOTICES.md` lists every crate any feature combination can link, resolved with `cargo tree --all-features`.
That resolve reads the manifest of every optional crate, which an offline host or a cold `CARGO_HOME` does not have. So
`cargo xtask notices` writes a **resolve seal** into the file's header: a hash of the crate table together with
everything the table is resolved from. That is `Cargo.lock`, `.cargo/config.toml`, the dependency, feature, patch and
workspace tables of every workspace member and path dependency, the list of shipped crates with their feature
arguments, and the generator's own source (ADR-5903). Comments, lints, test targets and dev-dependencies are not part of
it.

`cargo xtask notices --check` recomputes those inputs from local files and `cargo metadata --no-deps --offline`. When
they match the seal, it reads the committed table back and runs no `cargo tree`: no network and no downloaded manifest,
in under a second (0.35 to 0.42 s, against 46 to 47 s for the full resolve, measured on a loaded build host on
2026-11-11). When they do not match (a lane changed a dependency or a feature list and has not regenerated the
file), the check resolves as before, asking cargo `--offline` first. It passes if only the seal line differs and prints
that `cargo xtask notices` refreshes the seal. On a host that cannot resolve, it fails and names that fix instead of
printing a bare download error. A hand edit to a table row breaks the seal, so it never hides a stale table.

## Other audits

`cargo deny check` (licences, advisories, bans, sources, all features) and the unused-dependency audit are
described in `docs/03-workspace-and-dependencies.md` § 4e.
