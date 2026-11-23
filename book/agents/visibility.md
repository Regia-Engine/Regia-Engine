# Visibility: hidden entities and what the frame drew

`Visibility` decides whether an entity draws, and `ViewVisibility` says whether it was on screen
last frame (spec 14 § A1, spec 37 § 13, ADR-7200).

## Hiding things

`Visibility::Hidden` on an entity, or on any of its ancestors, keeps it out of every draw: the
colour pass, shadows, visibility-buffer clusters, mask decals, glass, occlusion proxies and
procgen instance sets. A mesh child does not need a `Visibility` of its own: `MeshRenderer`
brings `InheritedVisibility`, which the scene resolves from the parent each frame.
`Visibility::Visible` shows an entity under a hidden parent. A change shows on the next frame.

Lights and static decals hide the same way (ADR-7670). A hidden `PointLight`, `SpotLight`,
`RectLight`, `TubeLight` or `DirectionalLight` lights nothing: no shadow, no fog light, and the sun
is the lowest-entity directional light that is not hidden. The light components bring
`InheritedVisibility` too, so a light under a hidden group is off. A hidden `StaticDecal` stops
drawing and `vfx.decal_budget` stops counting it, while its decal keeps its place in the pool.

Everything that sets `Visibility` hides this way: the sequencer's `visible` track, a networked
proxy kept hidden until its spawn tick is shown, and a script or the agent.

Parked layer members (`LayerParked`, spec 37) draw nothing either. Their terrain is withheld
from the renderer while every `Terrain` in the world is parked or hidden, and comes back when one
goes live (the same draw, not rebuilt). Their thruster plumes, hover washes and crash sequences
stop their effects. A `LayerMembership` on an entity without an `EntityGuid` (one a script
spawned) is switched like the others: an `Absent` layer parks it, because it has no template to
come back from.

## What the frame drew: `render.visible_entities`

Culling writes `ViewVisibility` every frame. It is true when one of the entity's draws passed some
view's layer mask and frustum and the GPU occlusion result does not hold it:

- **Skinned meshes** are tested where their pose puts them, not at their bind pose: an arm posed
  into the view counts, a character posed out of it does not. Skins whose bound cannot be proven
  (dual-quaternion or eight-influence skinning, odd weights) always count.
- **Occlusion.** A mesh the depth prepass's two-phase cull drew in neither phase (behind a wall,
  inside the frustum) is not visible. The cull's result is read back without stalling the frame,
  so it lags: a mesh uncovered on frame N reads true from frame N + 1 when the GPU keeps up, later
  under load. The cull runs only with two-phase occlusion on
  (`render.set_occlusion { "two_phase": true }`; on in Desktop High and Ultra) and at least 2048
  opaque draws; otherwise, and for blended draws, only the frustum counts.
- **Visibility-buffer clusters.** A mesh drawn as clusters is not visible when the cluster cull
  drew none of its clusters (all behind the depth already drawn). The cull's per-cluster
  visibility bits are read back the same way, with the same lag.
- **Several views.** In split screen each window view has its own occlusion result, and an entity
  is visible when any view sees it: a box behind a wall in the left half but in plain sight in the
  right half is visible. A render-to-texture camera never culls by occlusion, so for that view
  only the frustum counts; it does not switch occlusion off for the others.
- **Each view culls against its own last frame.** Every window view keeps its own depth pyramid
  and its own record of which clusters it drew, so a wall only the right half sees hides what is
  behind it in the right half, even when the left half cannot see that wall. Each extra view costs
  one pyramid (`pyramid_bytes`, about 5.3 bytes per pixel of the view); removing a view frees it.
  Its visibility-buffer level-of-detail history is its own too, so a view change does not reset
  the other view's chosen detail (`render.get_visbuffer`, `book/agents/visbuffer.md`).
- **A view that stops drawing gives its history back.** A window view that has not drawn for 16
  rounds (a round ends when a view draws again: one frame of split screen, or one frame of a single
  view) is retired: its pyramid, cluster record and level-of-detail history are freed and its
  `pyramid_bytes` drops to 0, with no call from the host. If it draws again it starts as a new view:
  the first frame culls nothing by occlusion, and the frame after culls as before.
- `frame.occluded` is how many entities had a draw in a frustum but were hidden by occlusion.
  `occlusion` holds the readback's counters (`recorded`, `read`, `dropped` when the GPU fell three
  frames behind, `stale`, `cleared`, `published`, the keys in the current result `occluded`, and
  `result_seq`) and `pyramid_bytes` (the depth pyramids the renderer keeps for the views'
  occlusion history), summed over the views, or `null` when the app has no readback.
- `views` has one line per view of the frame, in the frame's view order: `view` (its index),
  `target` (`"window"` or `"texture"`, with `asset_id` for a texture), `occlusion_folded` (its
  occlusion result counted; false for a texture view or a window view not culled yet), `in_view`
  (entities with a draw in its frustum), `occluded` (of those, the ones its occlusion result hides),
  `pyramid_bytes` (that view's own depth pyramid; 0 for a texture view or a view not drawn yet)
  and `occlusion` (its own readback counters, or `null`).

```json
{ "method": "render.visible_entities", "params": { "limit": 16 } }
```

returns

```json
→ { "scope": "view_visibility", "camera": null, "visible": 3, "listed": 3, "truncated": false,
  "limit": 16, "entities": [4294967301, 4294967302, 4294967310],
  "frame": { "runs": 120, "views": 1, "draws": 41, "visible": 3, "entities": 40, "writes": 0,
             "occluded": 2 },
  "occlusion": { "recorded": 118, "dropped": 0, "read": 117, "stale": 0, "cleared": 1,
                 "published": 0, "occluded": 2, "result_seq": 119, "pyramid_bytes": 2764800 },
  "views": [ { "view": 0, "target": "window", "asset_id": null, "occlusion_folded": true,
               "in_view": 5, "occluded": 2, "pyramid_bytes": 2764800,
               "occlusion": { "recorded": 118, "dropped": 0, "read": 117, "stale": 0, "cleared": 1,
                              "published": 0, "occluded": 2, "result_seq": 119,
                              "pyramid_bytes": 2764800 } } ] }
```

- `camera` (optional): a camera entity id. The test then runs that camera's frustum against the
  last extracted frame instead of reading `ViewVisibility`.
- `limit` (optional, default 64, at most 4096) bounds the `entities` list, not `visible`.
- `frame.writes` is how many `ViewVisibility` values changed last frame. It is 0 on a steady frame.

Wait for one entity to come on screen with `play.wait_until` on its `ViewVisibility` (a reflected
component; the predicates are in [Testing](testing.md)):

```json
{ "method": "play.wait_until",
  "params": { "condition": { "entity": { "name": "Door" }, "component": "ViewVisibility",
                             "path": "0", "op": "eq", "value": true },
              "timeout_ms": 5000 } }
```

To wait for a count (anything at all on screen, or a crowd), call `render.visible_entities` with
`limit: 0` once per frame step and read `visible`.

Errors:

| Code | When |
|---|---|
| `F0002` | `limit` is not an integer from 0 to 4096, or `camera` is not an unsigned integer |
| `F0003` | `camera` is not an entity with a `Camera` and a placement |

## Tasks

- **Check a hidden mesh is gone.** Set `Visibility::Hidden` (for example with `scene.set_component`),
  step one frame, and call `render.visible_entities`: its id is no longer listed.
- **Check a cutscene hides a prop.** Play the sequence and poll `render.visible_entities` with
  `play.wait_until` until the prop's id leaves the list.
- **Check a wall hides what is behind it.** With two-phase occlusion on, step two or three frames
  and call `render.visible_entities`: the meshes behind the wall are off the list and counted in
  `frame.occluded`. Move the wall away, step one more frame than you would without occlusion,
  and they are back.
- **Check split screen.** With two window cameras, step three frames and read `views`: each
  window view has `occlusion_folded: true` and its own `occluded`. A mesh is off the top-level list
  only when no view sees it.
- **Check a hidden decal or terrain.** `Visibility::Hidden` on a `StaticDecal` stops its draw
  (`vfx.decal_budget` stops counting it) while its decal keeps its place in the pool; a hidden
  terrain leaves the game and the editor viewport.
- **Check a hidden light is off.** Set `Visibility::Hidden` on the light (or its group), step one
  frame and take a screenshot: the frame matches one without the light.
- **Check a parked layer.** Request the layer `Parked` (`level.set_layer`) and confirm its members
  leave the list on the next frame.
