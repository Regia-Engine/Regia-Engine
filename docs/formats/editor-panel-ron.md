# `.editorpanel.ron` (utility panel sidecar, v1)

Spec: ADR-8189, `docs/specs/19-editor.md` § 10.17. A sidecar anywhere under the project's `assets/` makes a
utility panel: the editor lists it under Window ▸ Utility Panels and in `editor.utility_panels { op: "list" }`
with the id `<path without .editorpanel.ron>` (`assets/tools/rename`). Registered in `cargo xtask check-formats`
and `regia upgrade-project` as `editor-panel` (the shape is guarded through `regia_core::editor_formats`, kept
equal to the editor's types by `regia_editor`'s `user_formats` tests). Fuzz target `editor.panel`
(`regia_editor::baseline_fuzz`); corpus fixture `crates/regia_agent/tests/fixtures/format-corpus/editor-panel/`
(loaded by `crates/regia_editor/tests/format_corpus.rs`). Guide: `book/agents/utility-panels.md`.

```ron
(
    format_version: 1,                          // must be 1 (a missing field reads as 1)
    title: "Rename selection with prefix",      // 1-64 characters, no control characters
    layout: "rename_with_prefix.ui.ron",        // the .ui.ron document the panel draws
    driver: Lua("rename_with_prefix.lua"),      // Lua("<file>.lua") or Logic("<file>.logic.ron")
    size: (360.0, 168.0),                       // optional, points per side, 64-2048; default (360, 200)
)
```

- `layout` and the driver path are relative to the sidecar's folder, `/`-separated, at most 256 bytes, and may
  not hold empty, `.` or `..` parts (a panel is a folder that can be copied as a whole). `layout` ends in
  `.ui.ron`; a `Lua` driver in `.lua`, a `Logic` driver in `.logic.ron`.
- Unknown fields are refused (`utility_panel_parse`), as is `format_version: 0`. A newer `format_version` is
  refused (`utility_panel_newer`) and left untouched; `regia upgrade-project` reports its version and never
  rewrites it. A field outside its range is `utility_panel_invalid`.
- The document is a `.ui.ron` of any version the runtime reads (`docs/formats/ui-ron.md`); a node with a
  `command` sends that command to the driver. The driver contract (logic graph inbox event and `editor::Call`
  action, or Lua `command` event and `editor.call` emits) is in `book/agents/utility-panels.md`.
