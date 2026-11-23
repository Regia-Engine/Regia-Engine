# Asset packages: export with dependencies, import package

Spec addendum § 8.9, ADR-7788. A package is a folder: some assets of a project with every asset they use,
at the same project-relative paths (`assets/...`) and with their `.meta` sidecars (so asset ids stay), plus
the manifest `.regia_assetpkg.ron` v1 at the top (the format page is the repository's
`docs/formats/assetpkg-ron.md`). Another Regia project imports the folder.

In the editor: an asset's context menu **Export with dependencies…** picks a destination folder (with several
assets selected, **Export selection with dependencies…** in a selected row's menu or the selection bar writes one
package of the whole selection: `paths` lists them all); the
Content Browser toolbar's **Import package…** picks a package folder. Both run as the background job
`package:<n>` (View ▸ Asset Jobs).

The dependency closure follows the references the project's text assets make: a RON asset (level, prefab,
material, ...) that names another asset's path (with or without `assets/`) or its asset id, and a `.gltf`'s
external buffers and images (`uri`). A longer path that merely contains an asset's path does not count.

## `editor.export_package`

Mutating (`Permission::Assets`). Params: `paths` (required, project-relative assets under `assets/`),
`dest` (required, the package folder: an allowed request path, missing or empty, outside the project).

```json
{ "method": "editor.export_package", "params": { "paths": ["assets/props/crate.prefab.ron"], "dest": "/tmp/crate-package" } }
```

Returns `{ dest, manifest: ".regia_assetpkg.ron", roots, assets: [{ path, id, bytes }] }`.

## `editor.import_package`

Mutating (`Permission::Assets`). Params: `package` (required, the package folder), `cook` (default `true`).
Every file lands at its listed path with its `.meta`. The import is refused as a whole, copying nothing,
when a listed path already exists (`kind: "path"`), a listed id is already used by a project asset
(`kind: "id"`, `existing` names that asset), or a file's length or hash does not match the manifest
(`damaged`).

Returns `{ imported, refused, clashes: [{ path, kind, existing }], damaged, cooked }`.

## Errors

Errors carry `data.pkg_error`:

| Code | When |
|---|---|
| `pkg_bad_params` | `paths` empty or not strings, `dest` or `package` missing |
| `pkg_bad_path` | an export path, or a manifest entry, is not a plain path under `assets/` |
| `pkg_missing` | an export path does not exist |
| `pkg_dest_not_empty` | `dest` exists and is not empty |
| `pkg_bad_dest` | `dest` is inside the project |
| `pkg_no_manifest` | the package folder has no readable `.regia_assetpkg.ron` |
| `pkg_bad_manifest` | the manifest does not parse (or has unknown fields) |
| `pkg_version` | the manifest's `format_version` is newer than this build reads |
| `pkg_too_large` | the manifest is larger than 16 MiB |
| `pkg_io` | a read, copy or write failed (an import removes what it had copied); the background job fails with it when its worker thread cannot start |
| `pkg_clash` | (the background job only) the package was refused; the job's error lists the first clash |

A path outside the agent's allowed roots answers `path_outside_project` before any of these.
