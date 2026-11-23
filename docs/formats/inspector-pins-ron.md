# `inspector_pins.ron` (pinned Inspector fields, v1)

Spec: the UI parity addendum § 7.2 (P-113). Per user: `<user config>/regia/inspector_pins.ron`. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `inspector-pins` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.inspector_pins` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/inspector-pins/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    pins: {                                     // optional; component type path → pinned field paths
        "regia_scene::transform::Transform": ["translation", "scale"],
    },
)
```

- Unknown fields are errors (`deny_unknown_fields`), but a newer version is reported as newer first.
- An unreadable file is remembered and never overwritten: pinning is refused with `pins_file_unreadable` until it parses.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
