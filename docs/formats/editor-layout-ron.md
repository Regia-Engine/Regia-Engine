# `editor_layout.ron` and named layouts (dock layout, v3)

Spec: `docs/specs/19-editor.md` § 2 (Layout), ADR-2321, ADR-7753. One shape for three places: the user default `<user config>/regia/editor_layout.ron`, the project's `.regia/editor_layout.ron`, and the named layouts `<user config>/regia/layouts/<name>.editor_layout.ron` (`<name>.workspace.editor_layout.ron` for a workspace). Registered in `cargo xtask check-formats` and `regia upgrade-project` as `editor-layout` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.layout` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-layout/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 3,                          // 1, 2 or 3
    root: Split(                                // Tabs(tabs, active) or Split(axis, fraction, first, second)
        axis: Horizontal,                       // Horizontal | Vertical
        fraction: 0.2,
        first: Tabs(tabs: ["outliner"], active: 0),
        second: Tabs(tabs: ["viewport", "content"], active: 0),
    ),
    known_panels: ["viewport", "outliner", "content", "problems"],  // optional
    floating: [                                 // optional (v2); floating windows
        (tabs: ["problems", "content#1"], active: 0, rect: Some((40.0, 60.0, 480.0, 320.0))),
    ],
    instances: {                                // optional (v3); each panel instance's own state by tab id
        "outliner": (filter: "kind:light"),
        "outliner#1": (filter: "tag:enemy", columns: ["visible_locked", "type", "tags"]),
        "content#1": (folder: "assets/textures", filter: "rock"),
        "inspector#1": (locked: Some("guid:000000000000000000000000000000a1")),
    },
)
```

- v2 → v3 (ADR-8105 (a), P-003) added panel instances: a tab id may be `<panel>#<n>` with `n` 1 to 7 (one digit, no leading zero) for `outliner`, `content` and `inspector` only; the bare id is instance 0, so every v1 and v2 file reads as v3 with one instance of each panel, and `regia upgrade-project` rewrites only its version number. `instances` (default none) holds each instance's state by tab id; every field defaults: `filter` (the search text; Outliner, Content Browser), `folder` (relative to the project root, `assets/textures`; empty is `assets/`; Content Browser), `columns` (the shown column names; an Outliner past the first, whose columns are not in the per-user view state's `main` tree), `locked` (`guid:<32 hex>`; Inspector; a guid the open level lacks restores unlocked). A suffix on another panel or out of range is dropped like an unknown id; an `instances` key naming no tab is not written.

- v1 → v2 added `floating` (default none): a v1 file reads as it is; `regia upgrade-project` rewrites only its version number.
- Unknown panel ids are dropped when the dock is built; nesting deeper than the limit, or too many tabs or windows, is refused.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
