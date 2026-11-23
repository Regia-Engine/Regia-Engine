# `.regia/editor_view.ron` (editor view state, v1)

Spec: ADR-7204, ADR-7275. Per project, user-local (`.regia/` is not under version control): the recent levels, each level's camera, camera bookmarks and locked entities, and the UI designer preview. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `editor-view-state` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.view_state` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-view-state/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1; missing reads as 1
    recent_levels: ["assets/maps/arena.level.ron"],
    levels: {
        "assets/maps/arena.level.ron": (
            camera: Some((target: (0.0, 1.0, 0.0), distance: 12.0, yaw: 0.6, pitch: -0.4, speed: 8.0, fov_deg: 60.0)),
            bookmarks: {1: (target: (4.0, 0.0, -2.0), distance: 6.0, yaw: 1.2, pitch: -0.3, speed: 8.0, fov_deg: 60.0)},
            locked: [],                         // entity GUIDs
        ),
    },
    ui_designer: Some((size: (1920.0, 1080.0), safe_area: (0.0, 0.0, 0.0, 0.0), theme: None)),  // optional
)
```

- Every field is optional (`#[serde(default)]`); a camera pose that is not finite or out of range is ignored.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
