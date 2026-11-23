# `config/asset_collections.ron` (Content Browser collections, v1)

Spec: `docs/specs/19-editor.md` § 2 (Content Browser) and the w12 UI parity addendum § 8.1-8.3. Proposed in
ADR-7759. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `asset-collections`. Fuzz target
`asset.collections`; corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/asset-collections/`.

The project's shared file is `config/asset_collections.ron`. Each user also has a personal file with the same shape in
the project's user-local folder: `.regia/asset_collections.ron` (`.regia/` is ignored by version control, ADR-7752
rule 3; ADR-7902). Editors before ADR-7902 kept it in the user config directory,
`<user config>/regia/content/<project folder>-<key>.asset_collections.ron` (`<key>`: the 64-bit FNV-1a hash of the
project's canonical path in hex); the editor moves such a file to `.regia/` once, byte for byte, when `.regia/` has
none, and leaves a file it cannot read where it is.

```ron
(
    format_version: 1,                 // must be 1
    collections: [                     // optional; display order
        // A static collection: asset ids (from each asset's .meta), or project-relative paths for files
        // with no .meta. Adding an asset never moves its file.
        (name: "Hero props", assets: ["7c9e6679-7425-40de-944b-e07fc1f90ae7", "assets/props/crate.glb"]),
        // A dynamic collection: a saved Content Browser search.
        (name: "Needs review", query: Some("is:review type:mesh")),
    ],
    favorites: ["assets/props"],       // optional; favorite folders, display order
    folder_colors: {"assets/props": "signal"},  // optional; folder → theme color token
    hide_empty_folders: false,         // optional; default false
)
```

- Unknown fields are errors (`deny_unknown_fields`).
- Collection names: not blank, at most 128 bytes, no control characters, unique in the file ignoring case.
- A collection has `assets` or `query`, never both.
- `query` uses the Content Browser search syntax (words, `-word`, `type:`, `ext:`, `tag:`, `is:`, `meta:`); it must
  parse when the collection is created.
- Folder color tokens: `signal`, `warn`, `error`, `strong`, `muted` (the editor theme's tokens, so a theme change
  recolors them).
- The editor reads favorites and `hide_empty_folders` from the personal file. Folder colors from the shared file apply
  to everyone; the personal file's colors win.
- A file that does not parse (a newer `format_version`, a merge conflict) is reported in the Collections section and
  by `editor.collection_list` (`errors`) and is ignored until fixed; the editor never writes it: a change to that scope
  fails with `collection_file_unreadable` (`favorites_file_unreadable`) until the file parses (ADR-7798).
- Every change reads the file again and applies to what is on disk now, so collections another user or another
  editor wrote meanwhile are kept.
