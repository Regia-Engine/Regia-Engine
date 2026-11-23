# `editor_prefs.ron` (editor preferences, v1)

Spec: `docs/specs/19-editor.md` § 2 (Preferences), ADR-2321. The file is per user, in the editor's user config directory: `<user config>/regia/editor_prefs.ron`. `book/agents/editor-preferences.md` lists every key. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `editor-prefs` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.prefs` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-prefs/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    values: {                                   // optional; key → typed value
        "appearance.theme": Text("regia-light"),
        "appearance.ui_scale": Float(1.25),
        "autosave.interval_minutes": Int(10),
        "layout.save_on_exit": Bool(false),
    },
)
```

- A value is `Bool`, `Int`, `Float` or `Text`; a key this build does not know is kept and written back.
- A value of the wrong kind or out of range is dropped (the default applies) and reported.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
