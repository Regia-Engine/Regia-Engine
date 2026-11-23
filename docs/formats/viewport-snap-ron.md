# `.regia/viewport_snap.ron` (viewport snap steps, v1)

Spec: ADR-7756 (job 3209). Per project, user-local: the gizmo's move, rotate and scale snap steps. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `viewport-snap` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.snap_steps` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/viewport-snap/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    distance: 0.5,                              // meters
    angle_deg: 15.0,                            // degrees
    scale: 0.1,
)
```

- A step that is not a finite number above zero keeps the current step.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
