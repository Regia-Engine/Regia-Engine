# Custom handles and snap targets

For asset tools (Tincal engine request F6): handles beyond translate, rotate and scale, and snapping to the
features of a mesh. Everything is in `regia_editor::handles`.

## Handles

```rust,ignore
use regia_editor::handles::{Handle, HandleDrag, DragPhase, SnapSet, handles};

let list = [
    Handle::Point { pos: pivot },                                   // grab point, camera-facing plane
    Handle::Arrow { origin: pivot, dir: Vec3::Y * 0.5 },            // axis-constrained, length = dir
    Handle::Arc { center: pivot, axis: Vec3::Z, from: -1.2, to: 1.2, radius: 0.4 }, // limit dial
    Handle::Plane { origin: pivot, normal: Vec3::Y, size: 0.3 },    // in-plane drag
];
if let Some(drag) = handles(ui, &view, viewport_rect, egui::Id::new("joint"), &list, &snaps) {
    match drag.phase {
        DragPhase::Hover => {}                     // pointer over handle `drag.handle`
        DragPhase::Begin | DragPhase::Drag | DragPhase::End => {
            // world-space movement: `drag.delta` since the last frame, `drag.total` since Begin
            pivot = list[drag.handle].anchor() + drag.total;   // or use `drag.position`
            // arcs: `drag.angle` (clamped to from..=to) and `drag.angle_delta`
        }
    }
}
```

`view` is the `ViewCamera` the viewport renders with and `rect` its screen rect. Give each group its own
`id` (it keys the drag state). Call `handles` after the viewport image and the built-in gizmo; when it returns
`Some` the viewport should not also pick or start a gizmo drag.

- Picking is by screen distance (10 px): the nearest handle wins; a plane yields to a point or line on it.
- `Arc` angles are radians, zero at `arc_reference(axis)`, counter-clockwise looking down the axis. The dial stays
  continuous across ±π and clamps to `from..=to`.
- Arrows with a zero `dir` are inert. Points and planes snap to the nearest snap point; an arrow takes the snap
  point's projection onto its axis; arcs do not snap.
- The pure math is public and tested on its own: `ray_plane`, `ray_line_param`, `ray_arc_angle`, `angle_about`,
  `segment_distance`, `hit_test`. `HandleState::update` is the same state machine without egui, fed by `PointerInput`.

## Snap targets

```rust,ignore
use regia_editor::handles::{SnapConnector, SnapKinds, snap_targets};

world.entity_mut(hand).insert(SnapConnector { center: Vec3::new(0.0, 0.0, 0.05), normal: Vec3::Z });
let snaps = snap_targets(&world, model_root, SnapKinds::FACE_CENTERS | SnapKinds::CONNECTORS);
let under_cursor = snaps.nearest(&view, pane_size, pointer, 10.0);
```

`snap_targets` walks the entity and its descendants (`Children`) and returns `SnapSet { points, truncated }`:

| Kind | Meaning |
|---|---|
| `FACE_CENTERS` | Centroid of each flat face. Triangles that share an edge and agree within about 1° are one face, so a box has 6. |
| `EDGE_MIDPOINTS` | Midpoint of each feature edge (boundary or crease). Diagonals inside a flat face are skipped; a box has 12. |
| `BOUNDS_CENTERS` | Center of each mesh's box (through its transform) and, with more than one mesh, of the subtree. |
| `CONNECTORS` | A `SnapConnector` component's `center` (and outward `normal`), local space, through the entity's `GlobalTransform`. |

Rigid meshes use `GlobalTransform`; skinned meshes use their current `SkinJoints` palette (the pose on screen).
Vertices closer than 0.1 mm are welded. The set is capped at 8192 points (`truncated`). Snap points are not
occlusion-tested; filter by `normal` against the view direction if you need front faces only.
`GlobalTransform` is derived by the scene schedule, so run it (or set it) before calling in a headless tool.
