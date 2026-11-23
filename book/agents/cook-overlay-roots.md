# Overlay asset roots (`cook.overlay_roots`)

A project cook normally reads buffers and images from `assets/` next to each `.gltf`. An **overlay
asset root** is another directory with the same layout. When a glTF names a relative file that is
not already under `assets/`, the cook copies it from the earliest overlay root that has it, then
cooks the one tree. A file already in the project is left alone. The feature is **off by default**:
a project without the table cooks as before, and no machine default directories are used.

## Turning it on

In `Regia.toml`:

```toml
[cook.overlay_roots]
paths = ["../payloads", "/mnt/share/assets"]   # relative paths are relative to the project
```

`paths` is the only key. Each entry is a non-empty string. A path that is not a directory is
skipped with a warning (`overlay root <path> is not a directory; skipped`), not a failed cook.
The earliest existing directory in the list supplies a missing file.

On one machine the environment variable `REGIA_COOK_OVERLAY_ROOTS` replaces the whole list. It
uses the platform path separator (`:` on Unix, `;` on Windows). A value that is empty or only
whitespace does not replace the table.

`regia cook` prints one line when the feature is on: `cook: overlay fill N file(s), N bytes from N
root(s)`. `regia cook --json` adds `overlay_fill` with `configured`, `files` and `bytes`. There is
no separate command-line flag.

A bad `[cook.overlay_roots]` table (an unknown key, a `paths` value that is not an array of
non-empty strings, or a value that is not a table) fails the cook with an error on `Regia.toml`
before anything is cooked. The agent reports that as `overlay_roots_config_invalid`.

## What is filled

The cook walks `assets/` for files whose name ends in `.gltf` (any ASCII case; `.glb` is skipped
because its buffers are embedded). Directory entries are sorted, and directory symlinks are not
followed. For each document it reads `buffers`, then `images`. A URI is filled only when all of
these hold:

- It is a relative path. `data:` URIs, any URI containing `:`, and a URI that starts with `/` are
  skipped (the cook's importer does not open those as sidecars).
- Percent-encoding is decoded the same way the importer decodes it (`my%20file.bin` fills
  `my file.bin`). A `%` sequence that is not UTF-8 is skipped.
- `\` is a separator, on every host.
- The destination stays inside `assets/`. A URI that leaves that directory is skipped. A URI that
  steps up and lands back inside `assets/` is filled at the folded path.
- The destination is not already a file, a directory, or a symlink. The project file wins.

The lookup key is that folded path relative to `assets/`. The earliest overlay root where that
path is a file supplies the bytes. A symlink whose target leaves its overlay root is not copied.
A symlink that stays inside the root is copied as bytes, not linked, so the project does not gain
the link. A regular file is hard-linked when the volume allows it, and copied otherwise. The
publish is a temp file then a hard link into place, so a reader sees no file or the whole file.

The same missing path named by two glTF files is filled once.

## Report

`CookReport::overlay_fill` is the summary. When the feature is on, the same JSON is written to
`.regia/cooked/<platform>/overlay_fill.json` (never packed), next to the cook ledger. A cook with
the feature off removes that file, so a later read does not report a fill the last cook did not
do. `root_index` counts existing directories only: 0 is the earliest one that existed.

## Agent

`cook.overlay_roots { platform?, path? }` reads the configuration and that summary. It does not
walk `assets/`, does not copy files, and does not create a missing root.

```json
{ "platform": "linux", "path": "tables/weapons.table.ron" }
```

The result is `{ platform, configured, roots, missing_roots, last_fill, resolved }`. `last_fill` is the
summary object (`configured`, `roots`, `missing`, `files`, `bytes`, `errors`, `filled`:
`relative_path`, `root_index`, `bytes`) or `null` when the file is absent or not a JSON object.
`platform` defaults to the host. It must be a short tag of ascii letters, digits, `-` and `_`,
at most 32 characters.

`path` (optional, relative to `assets/`, `/` or `\` separated, at most 1024 bytes) asks where the
runtime and the live cooker find that source (see below). `resolved` is `null` without `path`,
else `{ path, found, source, root_index, file }`: `source` is `"project"` (the file is under
`assets/`, `root_index` is `null`) or `"overlay"` (`root_index` as in `overlay_fill.json`), and
`file` is the host path. A path no root holds is `found: false` with the other keys `null`. A
`path` with `..`, a leading `/`, a drive or that is empty is `overlay_path_invalid`.

| Code | Meaning |
|---|---|
| `overlay_path_invalid` | `path` is not a plain path relative to `assets/` (a `..`, a leading `/`, a drive, empty, or over 1024 bytes). |
| `overlay_roots_config_invalid` | `Regia.toml` `[cook.overlay_roots]` has an unknown key or a bad value. `regia cook` fails the same way before cooking. Fix: `paths = ["..."]`. |

A missing `Regia.toml` is `configured: false`, not an error.

## What the runtime sees (ADR-8845)

The fill copies into `assets/` before `regia cook` discovers sources. The cooked content and a
later run of the same project see those files.

The **live cooker** (`LiveCooker`, `cook_sources`; the editor and `regia run --dev`) fills the
same holes the same way before it cooks a `.gltf`, so it writes the cooked files a batch cook
would write. It also watches the existing overlay roots: a file that appears in or changes under
an overlay root fills the holes it can and re-cooks the glTFs that read that path. A bad table
fails a live cook step the same way it fails `regia cook`. The live cooker does not write
`overlay_fill.json` (that stays the last batch cook's summary).

The **runtime** reads cooked assets by id, so a filled glTF needs nothing more. A source the game
reads directly at run time in a dev tree (a data table, a curve table, a chooser, a table source,
a UI animation pack, a loose color LUT) is looked up with `AssetServer::source_path`, in the cook's order:

1. the project's `assets/` (anything there wins, as in the fill);
2. the existing overlay roots, earliest first.

A path with `..`, a leading `/` or a drive is not served from an overlay root, and neither is a
symlink whose target leaves its overlay root. A packaged game reads through its paks and the VFS
first, as before. With `dev-assets`, the runtime's source watcher also watches the overlay roots,
and a data table served from an overlay root reloads when that file changes (a copy in a later
root that an earlier one shadows does not). `AssetServer::source_roots` gives the roots it read
when it opened. A bad table at run time is a warning and only `assets/` is searched.
