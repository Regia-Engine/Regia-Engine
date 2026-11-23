# `.editor_theme.ron` (saved editor themes, v1)

Spec: ADR-7838 (3264). A saved editor theme is `<user config>/regia/themes/<name>.editor_theme.ron`, written by
`editor.theme_save` and the Theme page of Preferences, read by `editor.theme_import` and `appearance.custom_theme`, and
shared with `editor.theme_export`. Registered in `cargo xtask check-formats` and `regia upgrade-project` as
`editor-theme` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by
`regia_editor`'s `user_formats` tests). Fuzz target `editor.theme` (`regia_editor::baseline_fuzz`); corpus fixture
`crates/regia_agent/tests/fixtures/format-corpus/editor-theme/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    name: "Studio Night",                       // optional; the theme's name
    tokens: {                                   // optional; the `editor.get_theme` JSON shape
        "flat": true,
        "colors": { "ground": "#101418", "signal": "#ff8800" },
    },
)
```

- Absent token keys keep the Regia Engine default; a token value that does not parse refuses the file (`theme_parse`).
- A file with a newer `format_version` is refused (`theme_newer`) and never rewritten; `regia upgrade-project` walks
  only the project, so a user-config theme is never rewritten by it.
- Version 1 is the first; no migrator yet.
