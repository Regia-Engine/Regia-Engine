# Samples browser (agents)

**Help ▸ Samples…** (palette: `help.samples`) lists the starting points that ship with the editor and copies one
into a folder you choose. It works offline: every template is built from sources bundled in the editor, and the
samples and extension examples come from the engine checkout the editor runs from (`REGIA_ENGINE_DIR`, or the
checkout around the executable; ADR-10544). Without a checkout (an installed or downloaded editor) only the project
templates are listed.

A copy never edits the sample in place. **Open** copies the selected entry into `<folder>/<name>` and, when the copy
is a project (it has a project manifest), opens the copy as the editor's project.

## What is listed

| Kind | Id | Source | Description |
|---|---|---|---|
| `template` | `template/<id>` | `blank`, `first_person`, `third_person`, `top_down`, `net_game` | built into the editor |
| `sample` | `sample/<dir>` | `samples/<dir>/` of the checkout | `[package] description` of its `Cargo.toml`, else the first paragraph of `README.md` |
| `extension` | `extension/<id>` | `extensions/<id>/` of the checkout | `name` and `description` of its `regia-plugin.toml` |
| `extension_sample` | `extension_sample/<extension>/<dir>` | `<extension>/samples/<dir>/` of a loaded extension | first paragraph of `<dir>/README.md` |

A picture named `preview.png` in a sample's folder is its preview; without one the row shows the name's initial.
The `quick` template is not offered (the editor's scaffold does not make it); use `regia new --template quick`.

A copy leaves out `target/`, `.git/`, `Saved/` and the editor's per-project state folder, and skips symbolic links.
A sample that uses the engine workspace's shared dependencies keeps its `Cargo.toml` as it is: it builds inside the
engine checkout, and the copy is for browsing and editing assets and levels.

## Extension samples

An extension bundles samples by putting folders under `samples/` next to its manifest. No manifest field is needed.
The Extensions panel lists them under the selected extension, each with an **Open sample** button that copies the
sample into the folder named in **Copy into** (the Samples window shares that folder).

## `editor.samples { action, id?, parent?, name?, open?, extension? }`

```json
{"method": "editor.samples", "params": {"action": "list"}}
{"method": "editor.samples", "params": {"action": "create", "id": "template/third_person", "parent": "/home/me/games", "name": "forest", "open": true}}
```

`action: "list"` answers `{ samples: [{ id, kind, name, description, project, preview, extension }], engine_checkout }`
(`extension` names the extension that bundles an `extension_sample`; `extension: "<id>"` as a parameter lists only that
extension's samples). `action: "create"` needs `id` and `parent` (made when missing), takes `name` (the new folder's
name; the project name for a template; default the sample's own name) and `open`, and answers
`{ root, project, opened, warning }`.

Error codes (lowercase `data.reason`): `unknown_sample` (not in the list), `sample_name_invalid` (empty, a separator, a
leading dot, padding), `sample_destination_exists` (the folder exists and is not empty), `sample_destination_inside_source`
(the chosen folder is inside the sample), `sample_source_missing` (the sample's folder is gone), `sample_copy_failed` (a
write failed; what was written is removed), `sample_template_failed`, `sample_open_failed` (the copy was made but did not
open; it stays). A missing or non-string parameter is `invalid_params`.

## Keyboard

Every control is a focusable widget, so Tab reaches all of them. Up and Down move the selection, Enter in the Folder or
Name field is **Open**, and the palette command `help.samples` opens the window. **Browse…** opens an OS folder dialog;
an agent passes `parent` instead. Everything scales with Preferences ▸ Appearance ▸ UI scale and takes its colors from
the theme.

## Project thumbnails

Closing the editor (window close, Save and exit, `quit.request`) and **Save all** render the main viewport in its game
look and save it as the project's picture (`project_thumbnail.png` under the project's `.regia/` folder, 128 px box). The
project browser's rows show it (`editor.recent_projects`' `thumbnail`) unless the project has an `icon`. Without a render
device (a headless run without an adapter) nothing is saved and the close is not held up.
