# closure_roots sample

A two-level project for the shared root resolver (CT-25, ADR-8477), used by
`crates/regia_cli/tests/closure_root_parity.rs` and described in `book/agents/closure-roots.md`.

- `levels/a.level.ron` is the startup level (chunk cook) and the entry level (named profile). It
  names a mesh, two textures through RON escapes (`\u{63}`, `\u{e9}`), a menu layout, a spawn
  table in an unknown format, and level `b` (a travel target).
- `ui/hud.ui.ron` loads at start and shows `textures/hud.png`.
- `ui/menus/pause.ui.ron` has a node *named* `textures/secret.png`: a typed non-reference string,
  so `secret.png` does not ship.
- `config/spawn.table` is JSON the resolver has no typed reader for: it is read with the labeled
  fallback scan and reported under root coverage.
- `textures/dyn/spawned.png` is loaded by name at runtime: nothing references it, so the profile
  declares it with `include` and the chunk cook with `entry`.

The cooked assets (meshes, textures, the levels' `*.procgen/` bakes) are written by the test.

`levels/side.level.ron` is opened by name from a level-select screen; it ships only when it is
declared (`include = ["levels/side.level.ron"]` or `entry = ["levels/side.level.ron"]`).
