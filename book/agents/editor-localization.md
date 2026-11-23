# Editor Localization panel (agents)

The editor's **Localization** panel (ADR-3260) shows the project's Fluent files as a table: one row per key, one column per
locale (the source locale first). It reads what `regia loc` reads (`assets/localization/localization.ron` and each
`assets/localization/<locale>/*.ftl`, `docs/specs/content-data.md` § 1.2), so the table, the CLI and the runtime agree.
It does not edit the locale list; that is Project Settings' job.

Each cell has a **state**:

| State | Meaning |
|---|---|
| `source` | The source locale's own text. |
| `translated` | A translation (made against the current source text, or with no recorded source). |
| `stale` | A translation whose `# regia-source:` hash (written by `regia loc import` and by this panel) differs from the current source text. |
| `placeholder` | Only the `# TODO translate` copy that `regia loc gather` wrote. |
| `missing` | The locale's files do not define the key. |

Filters: `all`, `missing`, `untranslated` (missing or placeholder), `stale`, and `long` (the translation has more
characters than the source text times `long_ratio`, default 1.5). Progress per locale is `regia_ui::loc_po::coverage`,
the same numbers as `localization.coverage`.

In the panel: click a cell to edit it below the table, then **Apply**; **Find usages** lists where the selected key is
used; **Gather** runs the gather pipeline; **Pseudo (viewport)** pseudo-localizes the viewport's runtime text.
The **Exchange** row exports a locale for a translator and imports the file they return (see
[Export and import](#export-and-import)). **Compile** validates the locales and writes the shipping bundles
(see [Compile](#compile)). The **context** column shows each key's translator context and can be edited (see
[Translator context](#translator-context)). The toolbar's **Preview language** picks the language the viewport and play
show (see [Preview language](#preview-language)).

## Methods

| Method | What it does |
|---|---|
| `editor.loc_table { locale?, filter?, search?, long_ratio?, page?, page_size? }` | The table. `rows[].cells` has one entry per shown locale (the source and `locale`, or every locale): `{ state, text, file, line, long }`. `coverage` lists `{ locale, messages, translated, untranslated, placeholders, stale, percent }` per target locale. `issues` lists parse errors and duplicate keys (`loc_parse_error`, `loc_duplicate_key`). Pages are 0-based, 50 rows by default, at most 500. The panel shows what the call asked for. |
| `editor.loc_set { key, locale, text }` | Set one translation (or, in the source locale, the source text). Answers `{ key, locale, file, created, changed, warnings, undo_depth }`. |
| `editor.loc_gather { dry_run? }` | Run `regia loc gather`'s pipeline: the `.ron` and `.rs` text scan of `gather.roots`, the conversations, the dialogue lines, subtitle cues and voice names. Each locale's `gathered.ftl` gains the keys it lacks. Answers `{ files_scanned, keys, bad_keys, files: [{ locale, path, added }], written }`. |
| `editor.loc_usages { key }` | Where the key is used: `usages: [{ file, line, kind, via }]` with `kind` one of `ui`, `scene`, `conversation`, `dialogue`, `code`, `asset`. With a loaded asset registry (`graph: "registry"`), assets that reach those files through the asset graph are listed too, with `line: 0` and `via` naming the file that names the key. `defined_in` lists the locale files that define it. |
| `editor.loc_pseudo { on? }` | Pseudo-localization (spec § 1.8) of the viewport: the edit world's runtime `Localization` now, the play world's while the panel draws. Answers `{ pseudo, viewport_localization }`; `viewport_localization` is `false` when no world has a runtime `Localization`. |
| `editor.loc_export { locale, format?, only?, path? }` | Write `locale`'s translator file: `format` `po` (default) or `xliff`, `only` `all` (default), `untranslated` or `stale`, to `path` under the project (default `.regia/loc/<locale>.po` or `.xlf`). The same bytes as `regia loc export`. Answers `{ file, locale, format, only, entries, skipped_attributes, skipped_terms, tm_pre_filled, tm_near }`. |
| `editor.loc_import { path, locale?, dry_run? }` | Read a returned PO or XLIFF file (the format is read from its content; `locale` overrides its `Language`). Answers `{ file, format, locale, imported, added, changed, missing, skipped, warnings, refused, files, written, undo_depth }`: `added` are keys the locale had no real translation for, `changed` keys whose translation changes, `missing` source keys still untranslated after the import, `files` the files it writes. With `dry_run: true` nothing is written. Otherwise the files are written as one undo step, the same bytes as `regia loc import`. |

| `editor.loc_compile { locales?, strict?, out? }` | Validate the project's locales and write the shipping bundles (`loc/<locale>.ftl` and `loc/index.ron`) under `out` (default `<project>/build`), with the same library call as `regia loc compile`: the same bytes. `locales` picks the locales to ship (default: those of `localization.ron`, else every locale); their fallback chains and the source locale ship too. `strict: true` fails on warnings too. Answers `{ ok, strict, dir, locales, errors, warnings, findings: [{ code, severity, locale, key, file, line, message }] }`. A failed validation writes nothing and answers `ok: false` with the findings (it is a result, not an error). |
| `editor.loc_set_comment { key, text }` | Set a key's translator context (the comment above its message). Answers `{ key, file, changed, context, undo_depth }`. One undo step; the same text again changes nothing. |
| `editor.preview_locale { locale? }` | Set the preview language of the edit viewport's UI and of play: `ui.set_locale` on both worlds. `source` (or an empty string) resets; omitted reports. Answers `{ locale, source, locales, edit_world, shown }`: `locale` is the pick (`null` for Source), `shown` the locale the edit world's runtime `Localization` now has. The play world follows the next time the editor draws. Changes no file. |

## What an edit writes

`editor.loc_set` rewrites only the edited entry, and every other byte of the file stays the same:

- When the locale defines the key, its definition that the runtime uses (the last one, files in name order) is changed
  in its own file. Its value lines are replaced; attributes (`.title = ...`) and other comments stay. The
  `# TODO translate` marker is removed. Outside the source locale, the `# regia-source: <hash>` line of the current
  source text is updated, or added directly above the message, so the entry leaves the `stale` filter.
- When the locale lacks the key, the message is appended to the locale's file named like the source file that defines
  it (`fr/main.ftl` for a key of `en/main.ftl`), which is created when absent (`created: true`).
- A multi-line `text` becomes a multi-line value (continuation lines indented by four spaces). Line endings follow the
  file.

The write is atomic (a temporary file, then a rename) through the same writer `regia loc gather` and `regia loc import` use.
Each set, and each gather that writes, is one step on the editor's undo stack: `editor.undo` writes the old bytes back
(or removes a file the step created), and `editor.redo` writes the new ones again. If the file was changed on disk after
the step, undo leaves it alone and the panel reports `loc_panel_conflict`. Writing the same text again changes nothing
and adds no step (`changed: false`).

A translation that drops or adds a `$variable`, or lacks a plural variant the locale needs, is written with a warning
(`warnings: [{ code: "loc_variable_mismatch" | "loc_plural_missing", message }]`); `regia loc validate` reports the same.

## Export and import

The Exchange row has a locale picker (the target locales), a format picker (`po`, `xliff`), a filter (`all`,
`untranslated`, `stale`), then the export file and **Export**, then the import file and **Import…**. Each file is a
path under the project or an absolute path with its own **…** picker (a save dialog for Export, an open dialog for
Import), so choosing a returned translation file never changes where Export writes (ADR-7716). The agent methods
already keep them apart: `editor.loc_export { path }` is where it writes, `editor.loc_import { path }` what it reads.

- **Export** writes the locale's file (the export field, or `.regia/loc/<locale>.po` / `.xlf` when it is empty). The text is
  built by `regia_asset_pipeline::loc_exchange::export`, the function `regia loc export` calls, so the bytes are the
  CLI's: source references from the gather scan, translation-memory pre-fills and near-match notes, the same header.
- **Import…** reads the import file (it stays disabled while the field is empty) and plans the import without writing (`loc_exchange::plan_import`, as
  `regia loc import --dry-run`). The report lists the keys it **adds**, **changes** and the source keys still
  **missing**, and every skipped entry with its reason (`loc_po_stale`, `loc_variable_mismatch`, ...). **Apply**
  writes `imported.ftl` (and `gathered.ftl` when matching placeholders are removed) as one undo step; **Cancel** drops
  the plan. A file for another locale is refused (`loc_po_language_mismatch`) and nothing is written.

Undo writes the files' old bytes back (or removes an `imported.ftl` the import created).

## Compile

**Compile** (next to **Gather**; **Strict** makes warnings fail it too) runs `regia_asset_pipeline::loc_exchange::compile_bundles`,
the function `regia loc compile` and `regia package` call: the validator over every locale (parse errors, variable
mismatches, plural variants, the dialogue and glossary checks), then one normalized bundle per shipped locale and
`loc/index.ron` with each bundle's content hash. The bundles are written atomically under `<project>/build/loc`. A
failing validation writes nothing. The panel lists each finding with its file and line as a link; the link opens the
file at that line in your code editor (the `code.editor_command` preference, as the Output Log's links do). Because
the bundles are what a packaged game loads (`Localization::load_bundles`), a test loads them and compares every key in
every shipped locale with the runtime that reads the loose `.ftl` files: the texts are equal.

## Translator context

A key's **context** is what a translator needs to know that the text does not say: where it appears, what a variable
stands for. It is the `#` comment lines directly above the message in the Fluent file, leaving out the tool markers
(`# source: file:line`, `# TODO translate`, `# regia-source: <hash>`, and `##` group comments). A leading `note:` is
dropped (`# note: shown in the pause menu` reads as `shown in the pause menu`). The grid's **context** column shows it;
click a context cell (or select any cell) and edit it in the **Context for translators** box under the grid, then
**Apply context**. The panel writes one `# note: <line>` per line of text directly above the message in the **source**
locale's file, keeps the tool markers above it and every other byte of the file, and pushes one undo step.

Exports carry the context: each line is a PO extracted comment (`#. note: <line>`) and an XLIFF note
(`<note category="regia">note: <line></note>`), after the `variables:` line, so the translator's tool shows it.

## Preview language

The **Preview language** drop-down in the main toolbar, next to the play controls, lists **Source** and the project's
locales. A pick runs `ui.set_locale` on the runtime `Localization` of the edit world (the viewport's UI) and of the play
world, and the editor keeps both on the pick every frame, so a play session started later shows it too. **Source** puts
both back on the project's source locale (a running play session on the next editor frame, also when Source comes from
`editor.preview_locale`) and leaves the game free to choose its own again. The pick is a session
setting: it writes no file and is not undoable. While a locale other than the source previews, the **text** and **loc
key** fields of localized nodes in the UI Designer are read-only (a translation is on screen; edit the source text with
Source previewing). The picker is a drop-down with a text label, reachable and operable from the keyboard.

## Errors

Errors are `invalid_params` (or `not_found` for the `*_not_found` codes) with `data.reason`:

| Code | When |
|---|---|
| `loc_panel_no_project` | No project is open. |
| `loc_panel_bad_config` | `localization.ron` does not parse. |
| `loc_panel_io_failed` | A localization file or directory cannot be read or written. |
| `loc_panel_locale_not_found` | `locale` is not one of the project's locales. |
| `loc_panel_key_not_found` | Neither the locale nor the source locale defines `key`; for `editor.loc_set_comment`, the source locale does not. |
| `loc_panel_bad_key` | `key` is not a Fluent message id. |
| `loc_panel_empty_text` | `text` is empty. |
| `loc_panel_bad_text` | `text` is not one valid Fluent value (a parse error, or a line that would start an attribute or another message). |
| `loc_panel_bad_filter` | `filter` is not one of the five names, or the export's `only` is not `all`, `untranslated` or `stale`. |
| `loc_panel_bad_ratio` | `long_ratio` is below 1. |
| `loc_panel_bad_page` | `page` is past the last page, or `page_size` is outside 1..=500. |
| `loc_panel_no_locales` | Gather found no locale directory. |
| `loc_panel_gather_failed` | A conversation file does not parse. |
| `loc_panel_conflict` | (Undo or redo, on the panel's error line) a file changed on disk since the step. |
| `loc_panel_bad_format` | The export's `format` is not `po` or `xliff`. |
| `loc_panel_bad_locale` | The export's `locale` is not a locale tag (ASCII letters, digits, `-`, `_`). |
| `loc_panel_export_failed` | The export cannot be built: the locale is the source locale, or the source locale has no files. |
| `loc_panel_import_failed` | The file is not PO or XLIFF, has no `Language` and no `locale` was given, or the localization cannot be read. |
| `loc_panel_import_refused` | The file was refused (its `Language` is not the import's locale); nothing was written. |
| `loc_panel_no_import` | (Panel) Apply without a planned import. |
| `loc_panel_compile_failed` | Compile could not run: a localization file cannot be read, the source locale has no files, or a bundle cannot be written. (A failed validation is not this error; it answers `ok: false`.) |

## Example

```json
{"method": "editor.loc_table", "params": {"locale": "fr", "filter": "untranslated"}}
{"method": "editor.loc_set", "params": {"key": "menu-quit", "locale": "fr", "text": "Quitter"}}
{"method": "editor.loc_usages", "params": {"key": "menu-quit"}}
{"method": "editor.undo", "params": {}}
{"method": "editor.loc_set_comment", "params": {"key": "menu-quit", "text": "Button in the pause menu"}}
{"method": "editor.loc_compile", "params": {"strict": true}}
{"method": "editor.preview_locale", "params": {"locale": "fr"}}
{"method": "editor.preview_locale", "params": {"locale": "source"}}
```

The panel's tests (`crates/regia_editor/src/loc_panel/tests.rs`) run these steps on a fixture project with the source
locale `en` and the targets `fr` and `de`.
