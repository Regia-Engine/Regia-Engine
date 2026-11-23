# Asset registry (agents)

Every cook writes an **asset registry** next to the manifest: one record per cooked asset with its
id, path, cooked type, tags, cooked size and the ids it depends on (read from the cooked headers).
The `registry.*` methods search it without loading any asset and without opening any cooked file.
The format and the tag rules are in the [asset registry](../user-guide/asset-registry.md) user guide.

Where the running app reads it from:

- a dev run or the editor: `asset_registry.bin` in the cooked cache of the asset server's platform;
- a packaged build: every `*.registry.bin` next to the mounted paks (in name order, later files win),
  then the cache registry on top when there is one.

The registry is read on the first call and kept. A call reads it again when one of its files changed
on disk (size or modification time) or when it passes `refresh: true`. The live cook updates the
cache registry after each re-cook (only the re-cooked sources' records change), so in the editor a
query after a save sees the new tags and dependencies. Every answer carries `registry: { records,
generation, loads, sources }`; `loads` counts reads, so an agent can check that a repeated query did
not re-read the file.

## `registry.query`

Read-only. Every set condition must hold.

| Param | Meaning |
|---|---|
| `path` | Glob over registry paths: `*` stays inside a folder, `**` crosses folders, `?` is one character; `assets/props/**` also matches `assets/props`. |
| `path_contains` | Substring of the path, any case. |
| `type` | A cooked type path (`regia_asset::CookedTexture`) or its trailing segments (`CookedTexture`). |
| `tags` | List of `"key"` (the tag is present) or `"key=value"` (equal) filters, at most 64 (the protocol contract refuses more with `F0002`). Every filter is applied. |
| `limit` | Most records returned, 1 to 5000 (default 200). `total` still counts every match. |
| `refresh` | Read the registry files again first. |

```json
{ "method": "registry.query", "params": { "tags": ["biome=forest"], "type": "CookedMesh" } }
```

Returns `{ total, returned, entries: [{ id, path, type_path, tags, size, dependencies }], registry }`,
entries sorted by path; `dependencies` is the count here.

Sub-assets have their own records (`assets/tree.glb#material/Bark`) and carry their source's tags.

## `registry.describe`

`{ asset }` is an asset id or a project-relative path (`textures/bark.png` and
`assets/textures/bark.png` are the same). Returns the full record: `id, path, type_path, tags, size`,
`dependencies` and `referencers` (one step, each `{ id, path, type_path }`) and
`missing_dependencies` (ids named by the cooked header that the registry has no record for: an
asset outside this cook or pak).

Parameter types and limits: `path`, `path_contains` and `type` in `registry.query` are optional
strings (`path` is at most 1024 bytes); `tags` is an optional array of at most 64 strings; `limit`
is an optional integer from 1 through 5000 (default 200); `refresh` is an optional boolean (default
`false`). `asset` is a required string for `describe`, `dependencies` and `referencers`.
`transitive` is an optional boolean (default `false`) for the graph walks; their `limit` is an
optional integer from 1 through 5000 (default 200). `key` is an optional string for `tags`.

## `registry.dependencies` and `registry.referencers`

`{ asset, transitive?, limit? }`. Without `transitive` the direct edges; with it, everything
reachable, breadth-first, each record once, sorted by path within each step (cycles are safe).
`registry.referencers` walks the edges backwards: "what uses this texture?".

```json
{ "method": "registry.referencers", "params": { "asset": "assets/textures/moss.png", "transitive": true } }
```

Returns `{ asset, transitive, total, entries: [{ id, path, type_path }], missing }`.

## `registry.tags`

`{}` lists every tag key with how many records carry it; `{ key }` lists that key's values with
their counts. Automatic keys: `ext`, `folder`, `importer`.

## Errors

| Code | When |
|---|---|
| `registry_not_loaded` | The app has no asset server, or no registry file exists (cook the project). |
| `registry_unreadable` | A registry file exists but does not decode (cook again to rewrite it). |
| `registry_bad_query` | `asset` is empty. |
| `registry_bad_tag` | A tag filter key is empty or contains `=` or whitespace. |
| `registry_unknown_asset` | `asset` names no record. |
| `F0002` | A parameter has the wrong JSON type or violates its declared range, including a `limit` outside 1..=5000. |

## Workflow

1. Cook the project so its cooked cache contains `asset_registry.bin`.
2. Use `registry.query` to narrow by path, type or tags; adjust `limit` if `total` exceeds `returned`.
3. Use `registry.describe` for one record, then `registry.dependencies` or `registry.referencers`
   to follow its graph; use `registry.tags` to discover available keys and values.

## Failing call

With a cooked cache present, this captured call rejects an empty asset identifier:

```text
$ regia agent call registry.describe '{"asset":""}' --project samples/boot_probe
error: method call failed: `asset` (an asset id or a project-relative path) is required (-32602): {"code":"registry_bad_query","hint":"see agents/asset-registry in the book for the parameters"}
```

## Checking it from a shell

`regia assets query --tag biome=forest --json` and `regia assets query --refs textures/moss.png
--transitive` answer the same questions from the project's cooked cache, with the app not running.
