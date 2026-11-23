# `.editor_profile.ron` (editor feature profile, v1)

Spec: ADR-7753 (5). The project's active profile is `.regia/editor_profile.ron` (absent = Full); saved profiles are `<user config>/regia/profiles/<name>.editor_profile.ron`. Registered in `cargo xtask check-formats` and `regia upgrade-project` as `editor-profile` (ADR-7902; the shape is guarded through `regia_core::editor_formats`, kept equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.profile` (`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-profile/` (loaded by `crates/regia_editor/tests/format_corpus.rs`).

```ron
(
    format_version: 1,                          // must be 1
    name: "Level art",                          // 1-64 characters
    hidden_areas: [networking, scripting],      // optional; 3d 2d animation ui networking terrain procgen audio scripting
    hidden_panels: ["profiler"],                // optional; panel ids
    hidden_kinds: [],                           // optional; Create entity kinds
)
```

- Areas are RON identifiers, not strings (`networking`, not `"networking"`); `3d` and `2d` are written the way the
  editor saves them (save a profile to see the form). Panel ids and kinds are strings.
- Unknown panel ids and kinds are kept and reported; each list holds at most 512 entries.
- A file with a newer `format_version` is refused and left untouched (the editor never overwrites it); `regia upgrade-project` reports its version and never rewrites it.
