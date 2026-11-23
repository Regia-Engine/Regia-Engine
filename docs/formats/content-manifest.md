# Content manifest (`content-manifest.ron`), chunk map (`chunk-map.bin`), install record (`content-state.ron`)

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.3, § 4. Decision record: ADR-1667.
Guide: `book/agents/content-chunks.md`.

## `content-manifest.ron` (format `content-manifest`, v1)

Written by `regia cook` / `regia package` next to the paks (`build/paks/<platform>/`), staged into the
package's `content/` and, when a build has non-install chunks, into `dist/<platform>/chunks/`.

```ron
ContentManifest(
    format_version: 1,
    build_id: "1.2.0-7-3fa94c0d1e2b",    // <[project] version>-<build_number>-<12 hex of the cook's asset manifest hash>
    platform: "linux",
    compatible_with: [],                 // other base build ids this build accepts for DLC
    chunks: [
        (name: "base", file: "base.fpak", size: 412345678, hash: "<blake3 hex>", delivery: Install,
         requires: [], overrides: false, mount_order: 0, assets: 9120),
        (name: "arena", file: "arena.fpak", size: 98765432, hash: "<blake3 hex>", delivery: OnDemand,
         requires: [], overrides: false, mount_order: 1, assets: 1410),
    ],
)
```

- `build_id`, each `compatible_with` entry and a `base@<build_id>` in `requires`: one path segment of the
  chunk URL, so at most 128 bytes with no `/`, `\`, space, ASCII control character or `..`; a manifest
  that breaks this is refused (`Format`, audit v0.2.12). The cook maps other characters of
  `[project] version` to `_`. No shape change: valid v1 manifests still load.
- `name`: lowercase letters, digits, `_` and `-`, starting with a letter, at most 40 characters, one
  chunk per name.
- DLC entries: `<chunk>.chunk.ron` (one `ChunkEntry`) and `<chunk>.chunk-map.bin` (chunk-map v1, that
  chunk's ids) next to the manifest join it at runtime (`read_from_dir_with_dlc`); an entry must be
  named after its file, record `base@<build_id>` and pass the rules above, or it is skipped.
- `file`: a bare file name (`<chunk>.fpak`); paths and `..` are refused.
- `size`: the file's byte length. `hash`: BLAKE3 of the whole file, lowercase hex.
- `delivery`: `Install`, `FastFollow`, `OnDemand`, `External`.
- `requires`: chunks this one may reference (`base` implicit). A DLC chunk names the base it was
  cooked against as `base@<build_id>`; mounting checks the running build id equals it or is listed in
  `compatible_with` (`chunk_base_mismatch` otherwise).
- `mount_order`: topological order of `requires`, `base` first (0), ties by chunk name. Stored so a
  runtime does not recompute it.
- `blocks` (optional, omitted when empty): BLAKE3 of each 1 MiB block, for resumable transfers.

Version rules: additive fields keep v1; an incompatible change bumps `format_version` and ships a
migrator. A newer manifest is refused with `manifest_version`.

The Regia.toml side (additive keys, the old `[cook] chunks = { base = ["**"] }` shorthand still
valid):

```toml
[cook.chunks.arena]
include = ["levels/arena/**"]
tags = ["arena"]
priority = 10
delivery = "on_demand"
requires = []
follow = true
overrides = false

[cook.chunk_options]
shared = "base"

[content]
android_pack_limit = 50
```

## `chunk-map.bin` (v1)

The 8-byte magic `RGCHMAP\0`, the version as a little-endian `u32`, then postcard of
`ChunkMap { chunks: Vec<String>, entries: Vec<(AssetId, u32)> }`: chunk names, and one
`(asset id, index into chunks)` pair per asset sorted by id. A duplicated asset (`shared =
"duplicate"`) is listed under its first claiming chunk by name. A newer version is `manifest_version`.

## `content-state.ron` (v1)

In the app's private data directory (`<local data dir>/content/`), next to fetched chunks:

```ron
(
    format_version: 1,
    chunks: {
        "arena": (build_id: "1.2.0-7-3fa94c0d1e2b", hash: "<blake3 hex>", size: 98765432, modified: 1793300000),
    },
)
```

A cache: a missing, unreadable or newer file reads as empty and everything re-verifies. A record whose
hash differs from the manifest marks the chunk `NeedsUpdate`; a matching size and modification time
skip re-hashing.

## `<chunk>.chunk.ron`

`regia content dlc` writes the `ChunkEntry` of the DLC chunk (the tuple of one `chunks` element above,
as a RON struct) next to its pak.
