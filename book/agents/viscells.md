# Indoor cells and portals

A map can say which rooms can see which: `<map>.viscells.ron` lists **cells** (oriented boxes)
and **portals** (convex quads between two cells). Each frame the renderer walks the portals from
every view and leaves out of the colour pass the draws of cells no portal chain reaches (spec 25
§ 7.7, ADR-7586). It is stage 1 of indoor occlusion for tunnels and bunkers.

## The data

- `regia_scene::components::VisCell(u16)` on an entity says which cell it belongs to. Cell 0 is
  outdoors. An entity without `VisCell` takes its **nearest tagged ancestor's** (stage 2, as
  `MapLayer` does): tag a building's root and its parts follow; a part tagged itself keeps its own
  and passes it on to its children. **An entity with no `VisCell` anywhere up its chain always
  draws.** The inheritance is resolved once per hierarchy edit (a `VisCell` or `ChildOf` added,
  changed or removed), not per draw.
- `<map>.viscells.ron` (format `viscells`, version 1, next to the map: `viscells_ron_path`) is
  loaded with `regia_render::viscells::VisCells::from_ron` or `load_viscells_ron` (no file: no
  cells, nothing is culled). Insert the result as a resource to turn culling on; remove it to
  turn it off. A worked example is in the
  [DO27 interface contract](../user-guide/do27-interface-contract.md#6-viscell-and-portals-for-indoor-occlusion).

```ron
(
    version: 1,
    cells: [
        (id: 0, name: "outdoor", outdoor: true, bounds: []),
        (id: 1, name: "room_a", bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
        (id: 2, name: "room_b", bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0), rotation_radians: 0.0)]),
    ],
    portals: [
        (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
        (a: 1, b: 0, quad: [(-1.0, 0.5, 4.0), (1.0, 0.5, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)]),
    ],
)
```

## The rules (and their error codes)

A file that breaks a rule is refused whole; `render.set_viscells` returns the code and changes
nothing.

| Code | Meaning |
|---|---|
| `viscells_parse` | Not RON, or not the document's shape (an unknown field, a wrong type). |
| `viscells_version_newer` | `version` is newer than this engine reads (1). Read before the body. |
| `viscells_version_zero` | `version: 0`. Absent means 1. |
| `viscells_too_many` | More than 4096 cells, 16384 portals or 64 boxes in a cell. |
| `viscells_duplicate_cell` | Two cells share an id. |
| `viscells_cell_no_bounds` | An indoor cell (id above 0) has no boxes. |
| `viscells_outdoor_bounds` | Cell 0 lists boxes: outdoors is everything outside every box. Listing cell 0 is optional. |
| `viscells_outdoor_not_zero` | A cell other than 0 says `outdoor: true`. |
| `viscells_bad_box` | A box has a non-finite value or a half extent at or below 0. |
| `viscells_unknown_cell` | A portal names a cell the file does not list (0 is always known). |
| `viscells_self_portal` | A portal joins a cell to itself. |
| `viscells_portal_not_quad` | A portal does not list exactly four corners. |
| `viscells_portal_not_finite` | A portal corner is not finite. |
| `viscells_portal_not_planar` | The four corners are off one plane by more than 1 % of the longest diagonal (at least 1 cm). |
| `viscells_portal_not_convex` | Not a convex quad: a bow tie, a dent, a repeated or collinear corner, no area. Either winding is fine. |

Boxes turn by `rotation_radians` about +Y (default 0). Portals are two-way. A view is in the cell
whose box holds its eye (the smallest box wins on overlap, the lower id on a tie).

**Doorways (ADR-7639).** Room boxes may stop at the wall faces: the wall's thickness around a door
belongs to no box. An eye there (walking through the door) is in the portal's **doorway** when it
stands over the quad (its outline grown by the portal's tolerance) and no further from the
portal's plane than the portal's `doorway_depth`. The depth is derived when the file loads: the
larger of the distances from the portal's centre to side `a`'s nearest box and to side `b`'s (0
for outdoors), that is the wider of the gaps the boxes leave on each side, plus the portal's
tolerance (1 % of its longest diagonal, at least 1 cm) (ADR-7646; not their sum, so the doorway
does not reach past the wall into ground behind the door that is in no box). An eye in a doorway is in **both** of the portal's cells (in every
cell of every doorway it stands in) and sees the union of what each sees; `views[].eye_cells`
lists them. Only an eye in no box and no doorway is in cell 0. Keep the gap the boxes leave around
a door about the wall's thickness: a portal far from its cells' boxes makes a deep doorway.

## What the renderer does

For each extracted view (window and texture views) it starts in the eye's cell with the view
frustum less its near plane, so a door nearer than the camera's near distance (walking through it)
still leads to the next room. A portal of the current cell whose quad, clipped to the current frustum, keeps any area
makes the cell behind it reachable, and the walk goes on there with the frustum narrowed to the
clipped quad (planes through the eye and each clipped edge, the portal's plane as the near plane).
A walk is bounded, and both bounds draw too much, never too little. A chain narrows through at
most 32 portals: a chain at 32 that still sees a portal onward stops narrowing, and every cell
connected to the one beyond (through any portals, whatever the frustum) counts as reachable
(`depth_capped`, ADR-7639). A view makes at most 4096 cell entries; at that cap every cell counts
as reachable (`capped`). When the eye stands on a portal's plane (within 2 cm) the walk passes
through without narrowing.

The reached set is the **union over the frame's views** (the frame has one draw list). Draws of
an entity whose `VisCell` is not reached leave the colour pass after the mesh LOD and the
visibility filter (`Visibility::Hidden` entities never reach the cull), so their
`ViewVisibility` is false. **Shadow views are not culled in stage 1**: a culled draw that casts
shadows stays as a shadow-only draw, so a room behind a wall still casts into the sun's cascades;
a culled draw that casts nothing leaves the frame. A `VisCell` naming a cell the file does not list
draws (`unknown_cell_draws`).

Stage 2 culls more than the colour draws (ADR-7673):

- **Point and spot lights** of an unreached cell (their own `VisCell` or an ancestor's) leave the
  frame, with their shadow-map update, unless their range touches a reached cell: a box of a reached
  indoor cell, or, when the outdoors is reached, anything outside one of the light's own cell boxes.
  A light whose range might light what a view sees stays, so culling a light never changes a pixel
  of a reached cell (`cell_lights`, `culled_lights`). Lights without an entity (area-light
  stand-ins, particle lights) are never culled.
- **Glass panes** whose colour draw was culled add no glass instance (`culled_glass`).
- Morph targets, mask decals, render overrides, scalar overlays, shading models and skinning are
  built from the frame's draws after the cull, so they follow it already.

Not cell-culled: fog's local lights (picked inside the extraction), emissive fitted lights of a culled
draw that still casts shadows, water bodies, VFX particles and decals, procedural instance sets and quick-draw meshes (they run in no fixed order
with the cull), and the sun's cascades (shadow views). Terrain and its scatter have no entity per
draw; they are culled through the outdoor frusta instead (see
[Terrain and scatter culling](#terrain-and-scatter-culling)).

## Agent methods

- `render.viscells { geometry? }` returns `loaded`, `version`, `cell_count`, `portal_count`,
  `cull`, `debug_draw`, `debug_segments` (lines the overlay draws), `cells` (id, name, boxes,
  portal indices) and `portals` (a, b, quad, normal, `doorway_depth`) unless `geometry: false`, `frame` (the last
  cull: `cell_draws`, `culled_draws`, `shadow_only_draws`, `unknown_cell_draws`, `visits`,
  `cell_lights`, `culled_lights`, `culled_glass`),
  `reachable` (the union, ascending ids) and `views` (per view: `eye`, `eye_cell`, `eye_cells`
  (the cells the walk started in: two or more in a doorway), `reachable`, `visits`,
  `portals_passed`, `capped`, `depth_capped`).
- `render.set_viscells { text?, clear?, cull?, debug_draw? }` installs a viscells.ron text
  (validated first; the codes above), removes the cells (`clear`, not with `text`), turns culling
  off and on (`cull: false` keeps the walk and its report), and shows the debug draw. F0003 when
  `debug_draw` is passed without a renderer; F0002 for a bad parameter or `text` with `clear`.

## The debug draw

`render.set_viscells { "debug_draw": true }` (or `debug.set_gizmos { "group": "viscells",
"enabled": true }`) outlines every cell box (green: reached last frame, red: not reached, yellow:
the first view's own cells) and every portal (cyan outline and both diagonals). It reads the
previous frame's walk. Check a new portal set with it before trusting the cull: a portal that
misses its doorway pops the room behind it.

## Recipe: check a tunnel's cells

1. `render.set_viscells { "text": "<the map's viscells.ron>", "debug_draw": true }`.
2. Move the camera into a cell (`scene.set_property` on its `Transform`), let a frame render, then
   `render.viscells { "geometry": false }`: `views[0].eye_cell` is the cell, `reachable` what it
   sees through the portals, `frame.culled_draws` how many draws left the colour pass.
3. `seeing.screenshot` with the overlay on, then again with `{ "cull": false }`: the two frames
   differ only where culled geometry was visible through a wall the cells do not model.

## Audio mouths from the portals

`regia_render::viscells::audio_zone_mouths(cells, cell_zones, zones, out)` turns the portals into
the zone-local mouths `AudioZonePortals` wants (spec 25 § 7.6): give each cell's audio zone (an
index of yours) and each zone's world centre and rotation; every portal that leaves a zone becomes
one `AudioZoneMouth { zone, position, radius, portal }` of that zone (the portal's centroid in the
zone's frame, its farthest corner as the radius). A portal between two cells of one zone is no
mouth. Copy each mouth into a `ZonePortal { position, radius }` on its zone's `AudioZonePortals`.

## Not built yet

Local-light shadow views of unreached cells, per-view draw lists, and the sets listed above as not
cell-culled. The sun's cascades drop the draws of sun-sealed cells and of unreached cells whose shadow
no view sees (below). See spec 25 § 7.7.

## Cells from the mesh-layer bake

`regia terrain cook` writes `<map>.viscells.ron` beside the document (ADR-8371, plan row W-21).
`demo.meshlayers.ron` becomes `demo.viscells.ron` (`viscells_ron_path`). The file is VisCell version 1:
no new field. Cell 0 is the outdoors. Each cooked section is the next cell (`section.id + 1`), named
`<layer>/<passage>`, with one oriented box from that section's zone. A mouth ring is a portal between
that section's cell and cell 0. A junction cut, a cover change and a chord cut are portals between the
two sections. The runtime loads the file with `VisCells::from_ron` and culls as before. Nothing is
hand-authored.

`regia terrain cook --json` lists `viscells` (the path), `vis_cells` (id, name, outdoor) and
`vis_portals` (a, b, and the cooked portal id). `CookReport` carries the same two lists;
`terrain.bake_report` (lane W-28) reads them. There is no `terrain.bake_report` method yet.

The cook refuses the file, and the cook, with these codes (the message names the section):

| Code | Meaning |
|---|---|
| `viscell_leak` | A wall has a boundary gap of 0.5 m or more, or two sections still meet with every portal closed. |
| `viscell_bad_portal` | A cooked portal is not one or more convex quads. |
| `viscell_bad_box` | A section has no zone, or a half extent that is not above 0. |
| `viscell_too_many` | More than 4096 cells or 16384 portal quads. |

<!-- ignore-reason: a fragment; `inputs` are the cook inputs of a real map -->
```rust,ignore
let out = regia_world_geom::cook::cook(&inputs)?;
let cells = regia_render::viscells::VisCells::from_ron(&out.viscells_ron)?;
assert_eq!(cells.locate([1.0e5, 0.0, 0.0].into()), 0);
```

## Terrain and scatter culling

An eye inside a sealed cell sees the outdoors only through the portals it can see into cell 0
(plan row W-22, ADR-8413). A second walk for each window view (ADR-8843; the first window view
only before v0.7), with the view frustum widened by `OUTDOOR_EDGE_SLACK_PX` (2 px, the terrain's own
edge slack), records the narrowed frustum of each entry into cell 0 (at most 64) in the view's entry
of `ExtractedFrame::outdoor_portal_views`, and their union in `ExtractedFrame::outdoor_portals`
(`regia_render::viscells::OutdoorPortals`). With it:

- **The terrain draw** leaves out of its camera passes (depth, colour, velocity) the heightfield
  nodes in the view frustum whose box lies more than `OUTDOOR_NODE_MARGIN_M` (0.5 m) outside every
  outdoor frustum. Sun cascades are not touched.
- **The runtime scatter** leaves out the instance blocks that lie in no outdoor frustum after
  widening by the instances' radius and shadow reach. A caster block whose shadow can reach a mouth
  of a reached cell stays, because the sun lights the cell's inside through its mouths.

The list is unrestricted, so the terrain and scatter draw exactly what they drew without cells, when
the eye is outdoors or in a doorway to cell 0, when the walk hit its visit or depth cap, when more
than 64 entries reached cell 0, without cells, and with `cull: false`. A restricted list with no
frustum means no outdoors is in view. A list is used only on a frame whose eye matches the eye it was
made for bit for bit, so a render-to-texture camera is never culled through the window view's list.

A host that drives `FrameRenderer` itself calls
`regia_render::viscells::outdoor_portals(cells, eye, frustum_planes, [width, height], &mut scratch, &mut list)` and
hands the result to `FrameRenderer::set_outdoor_portals(Some(&list))`. For the scatter it calls
`ScatterField::extract_meshes_through(&view, &list, &mut out)` in place of `extract_meshes`.

`render.stats` reports what was culled in `interior`:

| Field | Meaning |
|---|---|
| `restricted` | The extracted frame's union list is restricted (every window view is inside). |
| `frusta`, `mouths` | Outdoor frusta and mouth spheres in the list. |
| `terrain.restricted` | The terrain's last main-view prepare used the list. |
| `terrain.nodes_drawn` | Nodes the camera passes drew. |
| `terrain.nodes_culled` | Nodes in the view frustum that the outdoor frusta left out. |
| `scatter.emitted` | Instances the runtime scatter emitted last frame. |
| `scatter.portal_culled` | Instances in blocks the outdoor frusta left out. |

`terrain` is `null` without a terrain draw and `scatter` is `null` without a `RuntimeScatter`. The
terrain's own counters are `TerrainDrawStats::camera_nodes` and `portal_culled_nodes`. The scatter's
are `ScatterFrameStats::emitted` and `portal_culled`.

<!-- ignore-reason: a fragment; `cells`, `eye`, `frustum` and `renderer` come from the host's frame -->
```rust,ignore
use regia_render::viscells::{OutdoorPortals, TraversalScratch, outdoor_portals};

let (mut scratch, mut list) = (TraversalScratch::default(), OutdoorPortals::default());
outdoor_portals(&cells, eye, frustum.planes(), [1920, 1080], &mut scratch, &mut list);
renderer.set_outdoor_portals(Some(&list));
```

## Sun sealing: buried cells leave the sun's cascades

Stage 3 (ADR-8414, plan row W-23). A cell is **sun-sealed** when both hold:

1. **The sun walk does not reach it.** Every portal quad is projected along the sun's direction. From
   the outdoors (cell 0), a portal passes sunlight when its projection overlaps the region the light
   came through by more than 1 cm² (`SUN_PORTAL_MIN_AREA_M2`); the cell behind it is sun-reached and
   the walk goes on with the overlap. A portal edge-on to the sun passes none. The walk does not test
   which way a portal faces or the order along the ray, so it reaches more cells than light does,
   never fewer. Its caps are the view walk's, and both count more cells as reached.
2. **Its host says it is covered.** `regia_render::viscells::SunCover` lists the cells whose boxes lie
   under cover that stays in the sun's cascades (the terrain over a buried tube), for one sun
   direction. A cover for another sun (more than about 0.06° away) seals nothing. Without a
   `SunCover`, nothing is sealed. `box_covered` is the test a host runs per box: it sweeps the box's
   footprint toward the sun until the ray has risen to the cover's top, and asks for the lowest
   cover height over that rectangle (or a gap: a hole, the map's edge).

The cover rule is there because a cell's walls are often its own draws: a tube's shell is what keeps
its inside dark. Taking those draws out of the cascades without other cover above would let the sun
in. With cover, every ray from a sealed draw toward the sun crosses the cover first, so no receiver's
sun shadow changes.

A rigid draw whose `VisCell` (own or inherited) is a sun-sealed cell, and whose mesh bounds lie inside
one of that cell's boxes (grown by 5 cm, `SUN_SEAL_SLACK_M`), gets `SUN_SEALED_SHADOW`:
`ShadowCast::Local { max_cascades: 0, .. }`. It is drawn into no sun cascade, no near sun tile and no
traced sun scene. Local lights still shadow it. A shadow-only draw (its colour already culled) in a
frame with no local light leaves the frame. A draw that sticks out of its box keeps casting: it may be
the cover. Skinned draws keep casting.

Terrain nodes and terrain scatter do not carry a `VisCell`, and the cover rule means they can never lie
inside a sealed box (the cover is above it), so their cascade lists do not change.

**Agent.**

- `render.viscells` adds `frame.sun_reached` (the cells the sun walk reaches, cell 0 included),
  `frame.sun_walk_visits`, `frame.sun_sealed_cells`, `frame.sun_sealed_draws`,
  `frame.sun_dropped_draws` and `sun_cover` (`{ cells, sun_direction }` or `null`).
- `render.set_viscells { "sun_cover": [1, 4], "sun_direction": [0.3, -1.0, 0.0] }` installs a cover.
  `sun_cover: null` or `[]` clears it. F0002 for an id that is not a number from 0 to 65535, or for a
  missing, zero or non-finite `sun_direction`. A refused call changes nothing.
- `render.stats` adds `shadows`: `sun_casters_per_cascade` (mesh casters in each sun cascade's caster
  lists in the last frame), `sun_sealed_cells` and `sun_sealed_draws`.

**Recipe: check a buried tube.**

1. Install the cells and the cover:
   `render.set_viscells { "text": "<viscells.ron>", "sun_cover": [<buried cell ids>], "sun_direction": <the sun's direction> }`.
2. Let a frame render, then call `render.viscells { "geometry": false }`. The tube's cell is missing
   from `frame.sun_reached`, and `frame.sun_sealed_draws` counts its chunks.
3. Call `render.stats`. `shadows.sun_casters_per_cascade` is lower by the sealed chunks than with
   `sun_cover: null`.
4. Take `seeing.screenshot` with the cover and without it. An outdoor view must be the same.

## Grass, impostors, water and split-screen (ADR-8843)

The same outdoor frusta cull the terrain's grass, the scatter impostors and the water bodies, for
every window view (plan v0.7 item E; v0.6.0 Known issue KI-RN-008):

- **Every window view gets its own list.** `ExtractedFrame::outdoor_portal_views`
  (`OutdoorPortalViews`) holds one `OutdoorPortals` per window view whose walk starts inside a cell
  and hits no cap. The window paths call `FrameRenderer::set_outdoor_portals(frame.outdoor_portals_for(view))`
  before drawing each view, so each split-screen player's terrain, grass, impostors and water are
  culled through that player's own portals. A view without an entry is unrestricted.
- **The union.** `ExtractedFrame::outdoor_portals` is the union of the entries, restricted only when
  every window view has a restricted entry, with the first window view's eye. The frame has one
  draw list, so the scatter emits through the union: a block is drawn when any player can see it.
- **Grass.** A selected patch whose blade box (its ground range, its heights plus the class's blade
  headroom, widened by the headroom sideways) lies more than 0.5 m outside every outdoor frustum
  leaves the camera passes. A patch inside the shadow distance stays for the sun's cascades with
  `PATCH_NO_CAMERA`; any other patch leaves the list. The GPU cull skips the camera blades of a
  `PATCH_NO_CAMERA` patch.
- **Impostors.** Each uploaded impostor tile has a box holding every card it can draw. On a
  restricted frame the cull reads a bit per instance slot and skips the slots of every tile that
  no outdoor frustum admits. Impostors draw nothing into the cascades, so nothing else changes.
- **Water.** The water layer takes the list through `TranslucentLayer::set_outdoor_portals`. A lake
  or river body whose vertex box no outdoor frustum admits is not drawn, and on the ring grid of a
  swell or ocean plane the chunks no frustum admits are left out (with or without the ring frustum
  cull). The underwater fog is not culled.

An eye outdoors, a capped walk, no cells or `cull: false` draw exactly what they drew before.

`render.stats` `interior` gains:

| Field | Meaning |
|---|---|
| `views` | One `{ view, restricted, frusta }` per window view that has its own list. |
| `terrain.grass_patches_drawn` | Grass camera patches of the last view the terrain drew. |
| `terrain.grass_patches_culled` | Grass patches the outdoor frusta took out of the camera passes. |
| `terrain.impostors_kept` | Live impostor instances the GPU cull walks (every one when unrestricted). |
| `terrain.impostors_culled` | Live impostor instances in tiles no outdoor frustum admits. |
| `water.restricted` | The water's last view was culled through its list. |
| `water.bodies_drawn`, `water.bodies_culled` | Lake and river bodies drawn and left out. |
| `water.ring_chunks_culled` | Ring-grid chunks the outdoor frusta left out. |

`water` is `null` without a `WaterState`. The same counts are `OutdoorCullCounts` (from
`FrameRenderer::outdoor_cull_counts`) and `WaterFrameStats::{portal_restricted, bodies_drawn,
portal_culled_bodies, portal_culled_ring_chunks}`. No new error codes.

A host that draws its views itself makes one list per view with `outdoor_portals` and hands each to
`FrameRenderer::set_outdoor_portals` before that view's `render_into`.

## Shadow views per cell and the terrain's cover (ADR-8874)

Plan v0.7 item E; v0.6.0 Known issues KI-RN-007 and KI-RN-004.

**Shadow views per cell.** A sun shadow only matters where a view can see it: inside the boxes of the
cells the frame's views reach, and outdoors through the frusta their walks record into cell 0 (the same
lists the terrain, grass and water use). A rigid draw of a cell no view reaches is swept along the sun:
its world box moved along the light's travel for every distance, grown by 1 m (`SHADOW_VIEW_MARGIN_M`,
for the cascades' filter kernels and texel snapping) and by 2 cm per metre travelled
(`SHADOW_VIEW_SPREAD`, the penumbra). When that sweep touches no reached cell's box and no outdoor
frustum, the draw gets `SUN_SEALED_SHADOW` (no sun cascade, no near sun tile, no traced sun scene;
local lights still shadow it), or leaves the frame when it is shadow-only and the frame has no local
light. No cover is needed: the shadow it no longer casts falls only where no view looks.

Typical wins: a deeper tunnel under the one the eye is in (its shadow runs further down), a side
gallery the sun's shadows run away from. A gallery above the eye's tube keeps casting (its shadow falls
into the tube). Every recorded outdoor frustum counts, including one that only the terrain's two-pixel
edge slack reaches (`OUTDOOR_EDGE_SLACK_PX`): the ground drawn there keeps its shadows. The cull
stands down (nothing changes) while any view sees the outdoors unrestricted: a window view with no
restricted outdoor list (an eye outdoors or in a doorway to it, a capped walk, an overflowing frustum
list), or a render-to-texture view that reaches cell 0, through its edge slack too, or whose walk hit
the depth cap (those record no outdoor frusta). It runs after the sun seal, so a sealed draw is not
counted twice.
It is on by default; `VisCellShadowViews { cull: false }` turns it off.

**The terrain's cover.** With `regia_terrain::TerrainSunCover` inserted, the terrain fills `SunCover`
itself for the frame's sun (between the extraction and the cell cull, only when the sun, the cells, a
terrain or the setting change). A cell is covered when every one of its boxes passes `box_covered`
against the terrain:

- the lowest the terrain can be drawn over the swept footprint is the lower of two bounds: the minimum
  of the min-max pyramid's nodes over it, grown by `TerrainSunCover::grow_m` (default: two cells of
  the coarsest level a shadow pass draws, `coarsest_shadow_cell_m`, the root node's decimated grid),
  and, for each shadow level whose box-filtered vertices read samples further than that growth (the
  root and the level below it by default), the lowest filtered read of a vertex of a triangle that can
  overlap the footprint, over its whole morph (ADR-10161). The drawn cover never dips below it;
- a hole of `Terrain::holes`, an open mesh-layer mouth's lattice cut, or a sweep that leaves the map
  is a gap: not covered.

Any `grow_m` is safe; a smaller one moves more levels onto the slower drawn-mesh bound. Cliff relief
(`TerrainRelief::cliff_relief`) is not yet accounted for: see the known issue in ADR-10161.

**Agent.**

- `render.viscells` adds `frame.shadow_view_active`, `frame.shadow_view_tested` (rigid sun casters of
  unreached cells), `frame.shadow_view_culled_draws`, `frame.shadow_view_dropped_draws`,
  `shadow_views` (the switch) and `terrain_sun_cover` (`{ grow_m, fills, cells_tested, cells_covered,
  last_grow_m }` or `null`).
- `render.set_viscells { "shadow_views": false }` turns the shadow-view cull off.
- `render.set_viscells { "terrain_sun_cover": true }` turns the terrain's cover on; a host `sun_cover`
  replaces it; `false` turns it off and clears the cover it filled. F0002 for a switch that is not a
  boolean, and for `sun_cover` together with `terrain_sun_cover: true`.

**Recipe: check the shadow views from inside a tube.**

1. Install the cells: `render.set_viscells { "text": "<viscells.ron>" }`, put the camera inside a tube
   and let a frame render.
2. `render.viscells { "geometry": false }`: `frame.shadow_view_active` is true and
   `frame.shadow_view_culled_draws` counts the casters of cells below or beside it.
3. `render.stats`: `shadows.sun_casters_per_cascade` is lower than after
   `render.set_viscells { "shadow_views": false }`.
4. Take `seeing.screenshot` with the switch on and off: the frames must be the same.
