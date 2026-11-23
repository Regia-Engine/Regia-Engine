# `editor_window_state.ron` (main window state, v1)

Spec: ADR-7753 (6). Per user: `<user config>/regia/editor_window_state.ron`, one record per project path (the 64 most recent). Registered in `cargo xtask check-formats` and `regia upgrade-project` as `editor-window-state` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.window_state` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-window-state/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    projects: {                                 // project path → record
        "/home/user/games/arena": (
            monitor: Some("DP-1"),              // optional
            position: Some((120, 80)),          // optional; outer position
            size: (1600, 900),                  // inner size
            fullscreen: false,                  // optional
            maximized: true,                    // optional
            level: Some("assets/maps/arena.level.ron"),  // optional; the open level
            asset_tabs: ["assets/materials/rock.matgraph.ron"],  // optional; open documents
        ),
    },
    order: ["/home/user/games/arena"],          // optional; most recent first
)
```

- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
