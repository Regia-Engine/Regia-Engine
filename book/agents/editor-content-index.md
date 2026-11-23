# Content Browser index (`editor.content_index`)

The Content Browser lists the project's `assets/` folder from a cached index (ADR-866):

- The tree is walked once, not twice per frame as before. The folder list and the asset list come
  from that one walk, with the same rules as `editor.list_assets`: hidden (`.`) entries and `.meta`
  sidecars are skipped, and files under a `*.meta` folder are not assets.
- The view for the current folder and search is built once and reused until the folder, the search
  or the index changes.
- Only the rows in view are laid out, so a folder of thousands of assets scrolls as cheaply as a
  short one.
- Outside changes (a file copied in, a folder deleted) are found by comparing every walked folder's
  modification time, at most once a second. That costs one `stat` per folder; only the folders that
  changed are listed again (ADR-1329; before, the whole tree was walked again). The live asset
  watcher re-lists the folders of the files it reports. The browser's own Import, New, Delete and
  Reimport rescan at once, and the ↻ button beside the toolbar forces a rescan.

## `editor.content_index`

Read-only. Params: `folder` (a folder from the folder list such as `assets/props`; default
`assets/`), `search` (a query in the [asset browser](editor-asset-browser.md) syntax, any case; when
set it matches everywhere, best matches first), `limit` (paths returned, default 200, at most 5000),
`refresh` (check the folder times first and re-list the folders that changed; default false).

```json
{ "method": "editor.content_index", "params": { "search": "rock", "refresh": true } }
```

Returns:

- `root`, `assets` (how many assets are indexed), `folders` (how many folders are indexed);
- `visible` (the view's size) and `paths[]` (its first `limit` entries: `{ path, kind }`, kind as in
  `editor.list_assets`);
- `stale` (the refresh found a change);
- the counters `scans` (full walks), `stamp_checks` (folder-time checks), `views_built` (filtered
  views built) and `generation` (bumped by each walk or refresh);
- `dirs_read` (folders listed by walks and refreshes), `refreshes` (partial re-lists),
  `search_tested` (assets tested against a query) and `searches_narrowed` (searches that filtered the
  previous hits); see the [asset browser](editor-asset-browser.md) page.

The counters are how an agent checks the browser's cost without a clock. With an unchanged tree,
repeated calls and redraws leave `scans` and `views_built` alone; a new search adds one view and
no walk.

Errors: no project open is a `not_found` error with `data.index_error = "index_no_project"`. A search
that is not a valid query fails with `data.index_error` set to `search_bad_type` or
`search_empty_filter` ([asset browser](editor-asset-browser.md)).

`editor.list_assets` still walks the tree on every call, so it always answers from the disk. Its
`search` takes the same query syntax.

## Token completion

`editor.content_index { complete: "type:te" }` returns
`{ completion: "type:texture", candidates: ["type:texture"] }` without running a search or
requiring an open project. `completion` is null when no token can grow; `candidates` is a sorted,
deduplicated array of full queries. A non-string `complete` returns `bad_query` (JSON-RPC invalid parameters).

The Content Browser search box uses the same vocabulary and token parser. Tab completes the
last token and displays its candidates; further Tab presses cycle the retained candidates.
Editing the query starts a new completion sequence. Shift+Tab leaves the search field.
