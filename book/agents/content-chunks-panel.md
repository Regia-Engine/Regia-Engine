# Content Chunks panel (editor)

The editor's **Content Chunks** tab shows how the project's cooked assets split into
[content chunks](content-chunks.md) under the `[cook.chunks.*]` rules in `Regia.toml`: what the next
cook writes, computed with the same functions the cook and `regia assets chunks` use. Spec:
`docs/specs/40-content-chunks-dlc-asset-packs.md` § 6; ADR-1876.

## In the panel

- One row per chunk in mount order: name, delivery, asset count, a size bar and the chunks it
  `requires`. **Disk** bars show the pak's bytes (0 until the chunk paks are written by a cook);
  **Memory** bars show the members' uncompressed cooked bytes. **Platform** picks the cooked cache
  (empty = this machine's platform); **Refresh** recomputes after a cook or a rules edit.
- Reference problems (`chunk_base_reference`, `chunk_unlisted_reference`, ...) are listed under the
  table; errors among them fail the next cook.
- With `[cook.chunk_options] content = "closure"`, **Unshipped** lists the cooked assets no root
  reaches (the next cook packs them nowhere; ADR-8092). The panel uses the cook's startup level
  as an entry point, so it agrees with `regia cook` and `regia assets chunks`. The list shows 100
  paths per page with **Previous**/**Next**, the total, and a **Filter** (a substring of the path, or
  a glob under `assets/` such as `dlc/**` when it holds `*` or `?`). Click a path to open its
  references: an unshipped asset has no chunk, but its referrers tell whether something should
  reach it.
- Click a chunk to list its **direct assets** (selected by its own rule) with the selector that picked
  each (an `include` glob, or `tag:<tag>`); at most 100 are listed. Click a path to explain it.
- **Why is this asset here?** takes an `assets/...` path (the prefix may be left out) or an asset id
  and shows its chunk, its reason (`rule`, `follows`, `shared`, `pinned`, `default`), the rule
  selector, the referrer chain back to a direct asset, and the chunks holding a copy when the
  dependency is duplicated.
- In the Content Browser, right-click an asset and choose **Why is this in its chunk?**: the toast
  names the chunk and reason and the panel's why box holds the details.

- **Size map of `<chunk>`** lists the selected chunk's assets, largest first, with bars relative to
  the largest (at most 100 rows). Click a path to explain it.
- **References** under the why box lists what the explained asset references and what references it;
  an entry in another chunk names that chunk. Click a path to walk the graph.

**Project Settings** edits the rules: `[content] base_url`, `[cook.chunk_options] content` (**all** or
**closure**), and for each `[cook.chunks.<name>]` its include globs, **entry roots**, tags, requires,
delivery and priority, plus **Add chunk** and **Remove rule**. Keys the panel does not show (`pin`,
`follow`, `overrides`, `shared`, unknown keys) stay. **Preview chunk assignment** computes, on a
worker thread, what the unsaved settings would cook: shipped and unshipped counts, every declared root
(a root that matches nothing is red), and the assets whose chunk changes. A preview whose inputs
(`Regia.toml`, the cooked registry, any file under `assets/`) change while it computes is discarded
(`stale`), never shown. **Save** runs the cook's rule validation first and, with closure mode or any
entry root, resolves every root against the cooked registry: a root that matches nothing refuses the
save with `content_chunks_entry_unmatched` and nothing is written (without a cook the roots save
unchecked and the toast says so). Each save is one undo entry that restores the file's exact bytes.

## Agent methods

- `editor.content_chunks_state { platform?, chunk?, show?, refresh? }` (read-only) returns
  `{ loaded, platform, show, chunks: [{ name, delivery, assets, disk_bytes, memory_bytes, bar_bytes,
  requires, mount_order, by_type }], selected, direct_assets: [{ path, selector }],
  direct_assets_total, problems, content, unshipped, unshipped_total, unshipped_next_cursor, roots,
  roots_total, root_coverage, why, error }`. `unshipped_next_cursor` is the `cursor` of the second page
  of `editor.content_chunks_unshipped` (`null` when 100 or fewer). `content` is
  `all` or `closure` (`null` without a view); `unshipped` lists at most 100 paths of assets that ship
  in no chunk, sorted, and `unshipped_total` counts them all. `roots` lists at most 100 root records
  (`target`, `id`, `kind`, `declared_by`, `via`, `boot`; [Content closure roots](closure-roots.md)),
  `roots_total` counts them all, and `root_coverage` is `{ typed, binary, fallback, complete }`. A
  malformed or unreadable level or layout refuses the view with its `content_root_*` code. `chunk` selects a chunk, `show` is `disk` or `memory`
  (what `bar_bytes` and the bars show), `refresh: true` recomputes, and a new `platform` recomputes
  for that platform. `platform` must be empty (the host platform) or a host tag of 1-32 lowercase
  letters, digits, `_` or `-` (the `linux`/`windows`/`android` family); anything else is refused
  with `F0002` before it is joined into a filesystem path (ADR-1816).
- `editor.content_chunks_why { asset }` (read-only) explains an asset as the why box does:
  `{ asset, id, chunk, reason, rule?: { chunk, index, selector }, via: [path], duplicated_in? }`.

- `editor.content_chunks_size_map { chunk, limit? }` (read-only) returns
  `{ chunk, count, total_bytes, assets: [{ path, type, bytes }] }`, largest first (`limit` defaults to
  100; `count` and `total_bytes` are the whole chunk's).
- `editor.content_chunks_references { asset }` (read-only) returns
  `{ asset, chunk, dependencies: [{ path, chunk }], referrers: [{ path, chunk }] }`.
- `editor.content_chunks_set_rule { chunk?, content?, include?, entry?, tags?, requires?, delivery?,
  priority?, remove?, platform? }` (edit) changes one rule and/or `[cook.chunk_options] content` in
  `Regia.toml`: given fields replace the rule's (an empty list removes the key), a new name adds a
  rule, `remove: true` deletes it, `content` is `all` (the default; the key is removed) or `closure`.
  `chunk` is required unless `content` is given. `pin`, `follow`, `overrides`, `shared` and unknown
  keys stay. It validates as the cook does; with closure mode or any `entry` root it also resolves the
  roots against the cooked registry of `platform` (default the host) and refuses an `entry` that
  matches nothing (`content_chunks_entry_unmatched`) or closure mode without any root
  (`content_chunks_no_entry`); nothing is written then. A save writes atomically, pushes one undo entry
  ("Edit content chunk settings (agent)", document `Regia.toml`; `editor.undo` restores the exact bytes)
  and recomputes the panel. Returns `{ chunk, removed, saved, verified, content, preview, error }`:
  `verified` is false when no cook exists (the roots were not checked), `preview` is the checked
  preview (as `editor.content_chunks_preview`) or `null`, and `error` is a panel refresh failure after a
  successful save. A read-only agent profile is refused with `F0012`.
- `editor.content_chunks_preview { chunk?, content?, include?, entry?, tags?, requires?, delivery?,
  priority?, remove?, platform?, asset?, limit? }` (read-only) applies the same fields to `Regia.toml`
  in memory and returns what the next cook would do, with nothing written:
  `{ platform, content, valid, error_code, error, shipped, chunks: { name: count }, unshipped_total,
  unshipped, moves_total, moves: [{ path, from, to }], roots: [{ declared_by, pattern, matched }],
  unmatched, root_coverage, fingerprint, chain? }`. `valid: false` with an `error_code` is a settings
  edit `set_rule` would refuse. `moves` compares with the saved file (`from`/`to` `null` = unshipped).
  `asset` adds `chain`: the asset, each referrer back to the asset a rule or root selected, then the
  raw files that named that root, ending at its declaration (`[cook.chunks.dlc] entry[0]`). Lists hold
  at most `limit` rows (default 100, at most 1000). The result is computed from the CLI's own calls, so
  it equals `regia assets chunks` after the save. If an input changes while it computes, the call
  answers `content_chunks_preview_stale`.
- `editor.content_chunks_unshipped { filter?, cursor?, limit?, platform?, refresh? }` (read-only)
  pages through the unshipped assets in path order: `{ content, total, matched, filter, assets: [{ path,
  type, bytes, reason }], next_cursor }`. `filter` is a substring, or a glob under `assets/` when it
  holds `*` or `?`; `matched` counts the filtered list, `total` all unshipped assets. Pass `next_cursor`
  back as `cursor` for the next page (`null` on the last): the cursor is the last path returned, so every
  asset is on exactly one page. `reason` is `unreached`. `limit` is 1-1000 (default 100);
  `editor.content_chunks_references` opens an unshipped asset's referrers (its `chunk` is empty).

A running game answers the same question with `content.why` from the cooked `chunk-why.bin`.

## Errors

| Code | When |
| --- | --- |
| `content_chunks_no_project` | no project is open (start the editor with `--project <dir>`) |
| `content_chunks_no_cook` | the project has no cooked asset registry for that platform (cook first) |
| `content_chunks_bad_platform` | the panel's platform field holds a tag that is not 1-32 lowercase letters, digits, `_` or `-` (the agent method answers invalid params for the same tag) |
| `content_chunks_rules` | the `[cook.chunks.*]` rules do not parse, conflict or form a cycle (for `set_rule`, nothing was written) |
| `content_chunks_bad_chunk` | `chunk` names no chunk (for `set_rule` with `remove`, no rule) |
| `content_chunks_asset_not_found` | the asset is not a cooked asset of the project |
| `content_chunks_unshipped` | `editor.content_chunks_why` of a cooked asset that ships in no chunk (`content = "closure"` and no entry point reaches it) |
| `content_chunks_entry_unmatched` | `set_rule` or a Project Settings save: an `entry` glob matches no cooked asset or raw file (nothing written; the preview shows it with `matched: false`) |
| `content_chunks_no_entry` | `set_rule`: `content = "closure"` with no root at all (no startup level, no entry) |
| `content_chunks_preview_stale` | `Regia.toml`, the cooked registry or a file under `assets/` changed while a preview computed; request it again |
| `content_chunks_preview_failed` | the preview worker thread could not start or stopped without an answer |
| `content_root_missing`, `content_root_malformed`, `content_root_unreadable`, `content_root_limit` | a root the settings declare does not resolve ([Content closure roots](closure-roots.md)) |

`show` other than `disk` or `memory`, a missing `asset`, a `content` other than `all`/`closure`,
neither `chunk` nor `content` in `set_rule`, or a `limit` outside 1-1000, is `F0002` (bad parameters).

## Verify it

Open a project with an `arena` chunk rule after `regia cook`:

1. `editor.content_chunks_state {}` lists `base` and `arena` with sizes.
2. `editor.content_chunks_state { chunk: "arena", show: "memory" }`: `direct_assets` lists the
   level the rule selects, with its glob.
3. `editor.content_chunks_why { asset: "<a mesh the level uses>" }`: `reason` is `follows` and `via`
   ends at the level.
4. `editor.content_chunks_size_map { chunk: "arena" }`: the largest asset comes first.
5. `editor.content_chunks_references { asset: "<the same mesh>" }`: the level is a referrer, in `arena`.
6. `editor.content_chunks_set_rule { chunk: "arena", delivery: "external" }`, then step 1 again:
   `arena` reads `external`, and `Regia.toml` has the change.
7. `screenshot` the Content Chunks tab.

Closure content: add `[cook.chunk_options] content = "closure"` and `entry = ["levels/arena/**"]` to
the `arena` rule, plus a cooked asset nothing references, then
`editor.content_chunks_state { refresh: true }`: `content` is `closure` and `unshipped` lists that
asset; `editor.content_chunks_why` of it answers `content_chunks_unshipped`.

### Closure settings (CT-26, ADR-8543)

1. `editor.content_chunks_preview { content: "closure", chunk: "dlc", entry: ["dlc/boss.mesh"] }`:
   `valid` is `true`, `roots` lists `[cook.chunks.dlc] entry[0]` with `matched: true`, and `moves`
   names the assets that stop shipping (`to: null`). `Regia.toml` is unchanged.
2. The same with `entry: ["dlc/missing/**"]`: `valid` is `false`, `error_code` is
   `content_chunks_entry_unmatched`, `unmatched` names the entry; `set_rule` with those fields answers
   that code and writes nothing.
3. `editor.content_chunks_set_rule` with the step 1 fields: `verified: true`; `regia assets chunks
   --json` reports the same chunks and unshipped list as the preview.
4. `editor.content_chunks_unshipped { limit: 50 }`, then again with `cursor` = `next_cursor` until it
   is `null`: every unshipped asset once; `{ filter: "ui/*" }` narrows it.
5. `editor.undo`: `Regia.toml` is byte for byte what it was.
6. Gate an agent script on the preview: push it to the event log (`push_host` in a host) and wait with
   `play.wait_until { condition: { events_contain: { kind: "content_chunks_preview", contains:
   "[cook.chunks.dlc] entry[0]" } } }`, then `play.expect` the same condition.

### Required-edge diagnostics (ADR-8360)

The existing panel report includes `content_dependency_missing` with a root-to-target chain for
unknown required IDs and excluded required targets. An unreadable registry is
`content_registry_invalid`, rather than an empty/no-cook success. Assignment remains permissive
for inspection; the cook/package shipping check is mandatory regardless of advisory validation.
For structured JSON and the package job workflow, see [Content chunks](content-chunks.md#required-dependency-integrity-ct-24).
