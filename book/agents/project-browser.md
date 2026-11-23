# Project browser: tags, thumbnails, search (agents)

The project browser is the editor's start page when no project is open (spec 19 § 6). Each recent project is a row:
a thumbnail, its name, its path (the button that opens it), when it was last opened, the engine version that opened
it, and its tags. A search box and tag chips filter the list. ADR-7856.

## What the editor stores

`recent_projects.txt` keeps the list of paths, unchanged. Everything else is in a sidecar next to it,
`recent_projects_meta.ron`, version 1, keyed by project path:

```ron
(
    version: 1,
    projects: {
        "/home/me/games/forest": (tags: ["demo", "mobile"], last_opened: 1763200000, engine_version: "0.2.23"),
    },
)
```

Tags are lowercase, have no spaces (a space becomes `-`), no comma and no colon, at most 32 characters, at most 16 per
project. A missing sidecar reads as empty. A sidecar with a newer `version` is read as empty and never
overwritten (`recent_meta_unsupported`). Removing a project from the list drops its sidecar entry too.
A sidecar that exists but cannot be read or does not parse (torn by a crash, or a typo from a hand edit) is shown as
no tags and is never overwritten either: opening a project leaves it alone, and a tag edit or Remove from list is
refused with `recent_meta_unreadable` (the recent list is not changed) until the file is fixed or deleted.

## Thumbnails

A row shows, first match wins: the manifest's `[project] icon = "<path under the project>"`, `icon.png` at the project
root, then the last viewport capture saved under the project's `.regia/` folder (`project_thumbnail.png`, 128 px box,
aspect kept). A path that leaves the project is ignored. With none, the row shows the project's initial.

## Search and filters

Words match the name or the path (case-insensitive, every word must match). `tag:name` terms require that tag. The
chips under the box add tags the same way; a project must hold every chosen tag.

## `editor.recent_projects { query?, tag? }`

```json
{"method": "editor.recent_projects", "params": {"query": "tag:mobile forest"}}
```

Answers `{ projects: [paths], entries: [{ path, name, tags, last_opened, engine_version, thumbnail }], tags: [every tag
in use] }`, newest first, filtered like the browser. `last_opened` is Unix seconds (`null` for a project the sidecar
does not know), `thumbnail` the picture's path or `null`. `projects` keeps the old shape (paths only). A `query` or
`tag` that is not a string is `invalid_params`.

## `editor.recent_projects_edit { path, tags? | remove? }`

Sets a recent project's whole tag list (`tags`, replaces the old one) or takes it off the list (`remove: true`, the
folder is untouched). Not both. Answers `{ tags }` or `{ removed: true }`.

Error codes (lowercase `data.reason`): `recent_not_found` (not in the recent list), `recent_tag_invalid` (too long, a
comma, a colon, a control character, or more than 16 tags), `recent_meta_unsupported`, `recent_meta_unreadable`
(the sidecar exists but does not read or parse; nothing is written), `recent_write_failed`.

## Keyboard

Every control is a focusable widget, so Tab reaches all of them. Up and Down move the highlighted row, Enter opens it
(from the search box, or when nothing else has focus). The highlighted row shows a tag field: type a tag and press
Enter or **Add tag**. Show in file manager and Remove from list are buttons per row, each with a spoken name that
includes the project's name. Everything scales with Preferences ▸ Appearance ▸ UI scale and takes its colors from the
theme.
