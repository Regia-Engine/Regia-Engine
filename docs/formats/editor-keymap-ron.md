# `editor_keymap.ron` (editor shortcuts, v1)

Spec: `docs/specs/19-editor.md` § 2 (Keymap), ADR-2321. Per user: `<user config>/regia/editor_keymap.ron`; `editor.keymap_export` writes the same shape anywhere. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `editor-keymap` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.keymap` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-keymap/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    bindings: {                                 // optional; command id → chord
        "view.toggle_grid": "Ctrl+G",
        "layout.reset": "Ctrl+Alt+Shift+R",
    },
)
```

- Bindings of command ids this build does not know are kept for write-back.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
