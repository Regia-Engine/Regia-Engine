# Universal search, saved searches and the selection history

ADR-7766. Code: `regia_editor::universal_search`, `regia_editor::saved_searches`, `regia_editor::selection`
(history), `regia_editor::asset_search` (Tab completion).

## The window

Ctrl+T (the `search.everything` palette command, so it can be rebound; it is also the command palette's first row
with an empty query) opens **Search everything**: one query over

| Provider | Id | Prefix | Searches |
|---|---|---|---|
| Assets | `assets` | `a:` | the Content Browser's index (paths; `type:`/`t:`, `ext:` and `-word` filters as in the Content Browser) |
| Entities | `entities` | `e:` | the open level's entities by name; `kind:<name>`, `kind:none`, `kind:?`, `tag:<name>` and `type:<Component>` filters as in the Outliner |
| Commands | `commands` | `c:` | every command palette entry, menu items included (fuzzy over `Category: Title`) |
| Settings | `settings` | `s:` | every preference key and every `Regia.toml` key of the open project |
| Agent methods | `methods` | `m:` | every editor agent method by name (and by the words of its description) |

Results are grouped by provider, each heading with its match count (`+` while the provider is still searching). A prefix
token limits the search to its providers (`e: lamp`, or attached: `e:lamp`; several prefixes widen it); the provider
check boxes under the query do the same. Filter tokens go to the providers that understand them, and commands, settings
and methods match plain words only, so `crate tag:loot` lists entities alone. A query of prefix tokens alone lists
everything that provider has.

Keys, with the query field focused: Up and Down move through the results and **preview** the highlighted one; Enter
**commits** it; Escape closes the window and restores the selection and camera from before the first preview; Tab
completes the token under the cursor. A click on a row commits it. Every control (rows, provider toggles, the saved
searches side bar, Save) is a focusable widget, so Tab reaches each of them; the window scales with
`appearance.ui_scale` and draws only with theme colors.

| Result | Preview | Enter |
|---|---|---|
| asset | path, type and size | shows it selected in the Content Browser; opens a level (when the open level has no unsaved changes), a prefab (prefab editing) or a data asset; other types open from the Content Browser |
| entity | selects it and frames it in the viewport, with **no undo entry**; the selection history is paused | keeps it selected and records it in the selection history |
| command | its menu path and shortcut | runs it, as the palette does |
| setting | key, current value and description | opens Preferences filtered to the key, or Project Settings |
| agent method | its description | copies the name |

Committing anything other than the previewed entity puts the selection back first.

### Bounded work

A search examines at most 2048 candidates per editor frame across its providers (`universal_search::PROVIDER_BUDGET`)
and keeps the best 50 hits per provider (the count covers every match). Each examined candidate is one counted step:
a 20 000-entity level answers `e: needle` in 20 001 steps over 10 frames, and a finished search does no work per frame.
Starting a search copies the entity ids and takes the asset index's list; it reads no components and no files.

### Tab completion

Tab completes the last token (`asset_search::complete_token`):

- a word with no `:` completes to a provider prefix or a filter key (`ty` → `type:`);
- after `type:` or `t:`, an asset type (`level`, `prefab`, `mesh`, `texture`, `material`, `audio`, `other`);
- after `kind:`, `none`, `?` or a registered kind; after `tag:`, a tag held by the level's entities.

With several matches the token grows to their common start; when it cannot grow, it becomes the first match in order. The
Content Browser's search box completes `type:`, `t:` and `ext:` and the asset types through the same hook
(`asset_search::tab_complete` with `CompletionVocab::content_browser`).

## Saved searches

The side bar lists saved searches: **Mine** (the user config directory, `saved_searches.ron` next to
`editor_prefs.ron`) and **Project** (`config/saved_searches.ron`, shared through revision control). A click re-runs one
(its query and, when it was saved with some providers off, its toggles). Type a name and press **Save search** (check
**Share with the project** for the project file); saving a name that exists replaces its query. **Delete** removes one.

The file is format `saved_searches` version 1:

```ron
(
    format_version: 1,
    searches: [
        (name: "Lights", query: "e: kind:light", providers: []),
    ],
    favorites: ["view.toggle_grid"],
)
```

`favorites` (the palette favorites) is written to the personal file only. A file written by a newer editor is read but
never overwritten (the side bar shows why); a file that cannot be read or parsed is reported and left alone too.

## Selection history

The editor records the selection each frame it changes, keeping the last 50 (`selection::SelectionHistory`); an empty
selection is not recorded, and a search preview is not. **Alt+Left** selects the previous entry and **Alt+Right** the
next (the `selection.back` and `selection.forward` palette commands, which can be rebound; a user binding replaces the
default key). A new selection after going back drops the forward entries. The keys are read at the end of the frame, so
a panel that used Alt+arrow itself (a focused graph row moves its node) keeps it; nothing happens while a text field has
the focus or during play. **Selection history…** (`selection.history`) opens a list of the entries, newest first, with
Back and Forward buttons; a row selects its entry. Despawned entities are dropped from the entries.

## `editor.search`

`editor.search { query?, providers?, limit?, saved?, scope?, preview?, commit?, cancel? }` runs a search to the end and
returns `{ query, only, providers: [{ provider, title, count, note, hits: [{ index, label, detail, score, item }] }],
steps, frames, budget, previewed, committed }`. `providers` (ids) limits the providers as the window's toggles do;
`limit` is hits per provider (1 to 50, default 20); `saved` runs a saved search by name (`scope` `personal` or
`project`). `item` is `{ type: "asset", path }`, `{ type: "entity", entity }`, `{ type: "command", id }`,
`{ type: "setting", key, scope }` or `{ type: "method", name }`. `preview: { provider, index }` previews that hit as the
window does (an entity is selected and framed, no undo entry); `commit: { provider, index }` does what Enter does;
`cancel: true` restores the selection and camera from before the previews and returns `{ cancelled }`. `steps` counts
the candidates examined and `frames` how many editor frames the window would take at `budget` per frame. Mutating;
permission `edit_scene`.

```json
{"method": "editor.search", "params": {"query": "e: lamp", "preview": {"provider": "entities", "index": 0}}}
```

## `editor.saved_search_list`, `editor.saved_search_save`, `editor.saved_search_delete`

`editor.saved_search_list {}` returns `{ personal: [{ name, query, providers }], project, personal_file, project_file,
problems }` (read-only; permission `read`). `editor.saved_search_save { name, query, scope?, providers? }` saves (or
replaces) a search, as the side bar's Save does; `scope` is `personal` (default) or `project`; `providers` are provider
ids. `editor.saved_search_delete { name, scope? }` deletes one. Both return the lists plus `saved: { scope, search }` or
`deleted: { scope, name }`; mutating, permission `edit_scene`.

```json
{"method": "editor.saved_search_save", "params": {"name": "Lights", "query": "e: kind:light", "scope": "project"}}
```

## `editor.selection_history`

`editor.selection_history { action?, index? }`: `list` (default) records the current selection and lists the entries;
`back` and `forward` select the previous or next entry (Alt+Left, Alt+Right); `go` selects entry `index`. Returns
`{ action, moved, cursor, can_back, can_forward, entries: [{ entities, primary, label }], selection }`; `moved` is false
when there is no entry that way. Mutating; permission `edit_scene`.

```json
{"method": "editor.selection_history", "params": {"action": "back"}}
```

## Errors

| Code | Method | Meaning |
|---|---|---|
| `bad_query` | `editor.search`, `editor.saved_search_save` | `query` is not a string |
| `bad_providers` | `editor.search`, `editor.saved_search_save` | `providers` is not an array of provider ids |
| `bad_limit` | `editor.search` | `limit` is not a whole number from 1 to 50 |
| `bad_hit` | `editor.search` | `preview` or `commit` is not `{ provider, index }` |
| `no_such_hit` | `editor.search` | no hit at that provider and index for the query |
| `bad_saved` | `editor.search` | `saved` is not a string |
| `bad_scope` | `editor.search`, `editor.saved_search_save`, `editor.saved_search_delete` | `scope` is not `personal` or `project` |
| `saved_search_not_found` | `editor.search`, `editor.saved_search_delete` | no saved search has that name in that scope |
| `saved_search_bad_name` | `editor.saved_search_save` | the name is blank, longer than 64 characters, or starts or ends with a space |
| `saved_search_no_project` | `editor.saved_search_save`, `editor.saved_search_delete` | a project search with no project open |
| `saved_search_read_only` | `editor.saved_search_save`, `editor.saved_search_delete` | the file was written by a newer editor, or could not be read or parsed (it is left untouched) |
| `saved_search_io` | `editor.saved_search_save`, `editor.saved_search_delete` | the file could not be written |
| `bad_action` | `editor.selection_history` | `action` is not `list`, `back`, `forward` or `go` |
| `bad_index` | `editor.selection_history` | `go` without an `index`, or an index past the last entry |

See also the [command palette](editor-command.md) (menu paths, menu items as entries, favorites).

The Content Browser also retains its candidate list after the first Tab: subsequent Tab presses
cycle that list, and Shift+Tab leaves the field. Agents can request the same candidates with
`editor.content_index { complete: "t" }`; see [token completion](editor-content-index.md#token-completion).
