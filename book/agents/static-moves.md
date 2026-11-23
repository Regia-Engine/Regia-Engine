# Static moves at runtime

`regia_scene::components::Static` marks an entity as immobile: its transform is not expected to
change at runtime (`specs/14` A1). The renderer draws and caches a static exactly like any other
mesh (it already skips unchanged instance bytes and keeps a held caster's shadow pages; ADR-5906),
and only counts static-eligible batches and casters as diagnostics
(`regia_render::static_batch_eligible`, `DrawStats::static_batches`,
`ShadowCacheReport::static_casters`). A moved static therefore renders correctly; the marker is a
promise about the level, and the editor watches for broken promises in play and counts them where
an agent can read them back.

| Method | Permission | What it does |
|---|---|---|
| `editor.static_move_warnings` | read | How many `Static` entities moved at runtime since play started. |

The guard runs on the **play world only**. The edit world moves statics legitimately while a
scene is being built, so no check (and no counter) exists there; the count resets to 0 when play
starts.

## What counts as a move

Once per frame after `TransformPropagate`, the guard visits the play-world `Static` entities whose
`GlobalTransform` changed that frame (a level of held statics costs nothing) and compares each with
its previous observation:

- **Spawning is not a move.** An entity's first observation only records its pose, so a static
  spawned mid-play never warns.
- **One pose change, one event.** A changed world transform (the entity's own or a parent's)
  increments the counter; the snapshot then follows the new pose, so holding it stays quiet.
- **One warning per entity.** The entity's first move logs a `tracing::warn!` (entity name, what
  to do); later moves of the same entity only count, so an entity dragged every frame does not
  flood the log.
- **Only `Static` entities.** Everything else moves freely; a moving entity without the marker
  never appears here.

The play world's recent movers (one entry per entity, newest first move last, at most 8) live in
`regia_editor::static_moves::StaticMoveWarnings::recent`; the counter is mirrored into
`regia_editor::pie::PieStatus::static_move_warnings` because the play app cannot be a resource of
the edit world.

## `editor.static_move_warnings`

```json
→ { "method": "editor.static_move_warnings", "params": {} }
← { "jsonrpc": "2.0", "id": 1, "result": { "count": 2, "note": "Static entities moved at runtime
    since play started (specs/14 A1); remove the Static marker or stop moving them" } }
```

Params: none.

`count` is the number of move events since play started (0 while stopped, or when nothing moved).
A non-zero count means a `Static` entity's transform changed in the play world; remove the marker
from entities that really move, or stop moving the marked ones. `editor.play_state` does not
report this counter; poll `editor.static_move_warnings`.

## Fixing a warning

1. Find the mover: the play log line names the entity (`Static entity moved at runtime:
   entity = "Statue"`), and `StaticMoveWarnings::recent` holds the names.
2. Decide: if the entity is truly immobile, find the system or script moving it; if it really
   moves, remove the `Static` component (the inspector shows it under classification
   components). A moved static renders correctly: its instance data re-uploads and its shadow
   pages redraw, as for any moving mesh.

## Error codes

Plain JSON-RPC codes only (`F0002` on malformed params — this method takes none). A stopped
session answers `count: 0`, not an error.

## In Rust

This block is not compiled by `book-check`: it needs a play `World` from a running editor session,
and `regia_editor` is desktop-only and not a dependency of the book's snippet crate.

<!-- ignore-reason: needs a play World from a running editor session (`world` is the play world); regia_editor is desktop-only and not a dependency of the book's snippet crate -->
```rust,ignore
// The play app installs the guard (regia_editor::pie::start_play does this):
//   regia_editor::static_moves::install(&mut app);
// A system in the play world then just reads the resource:
let warnings = world.resource::<regia_editor::static_moves::StaticMoveWarnings>();
assert_eq!(warnings.count, 0, "no static moved this session");
```

See [Editor surface (M10 shell, M11 workflows)](editor.md) for `editor.play` / `editor.play_state`
and [Levels and play-in-editor](../user-guide/editor.md) for the play lifecycle.
