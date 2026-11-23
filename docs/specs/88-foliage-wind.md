# 88 — Foliage wind and the dynamic-caster shadow class

Status: proposed (ADR-2028; adopters, the `Wind` node and the painted wind flag ADR-2304). Plan item: "foliage wind and grass on a dynamic-caster shadow class" (`master-plan.md`
§ 3.4 v0.4.0, review N18 / D5; `versions.md` v0.4.0; `22-tier2.md` § T2.5 foliage). Crates: `regia_render` (wind, shadow
class), `regia_agent` (surface), `regia_terrain` (grass field, stage 2). No reviewed public-spec exists for this item: the
Regia docs are the source.

Built baseline: material graphs with a vertex stage (`material_vertex_offset`, the same in the forward, depth, shadow and
velocity passes); the wrapped material clock in graph slot 0 `xy` (ADR-1238); paged sun cascades and the local-light atlas
cache (`paged_shadow.rs`, M15b), which redraw a page only when a caster's bounds or pose change; the terrain grass field
(`regia_terrain::gpu::grass`) with its own per-class sway, whose shadow pass draws the rest pose so retained cascades stay
valid.

## 1. Purpose

A plant that sways in the vertex shader keeps its bounds and its model matrix, so the shadow page cache never sees it move:
its shadow freezes in the pose its page was first drawn with (`master-plan.md` § 9: "wind fights cached shadows"). This
spec gives the engine:

1. **One scene wind** every time-reading material graph reads (a vector in metres per second on world X and Z).
2. **A ready-made foliage sway** (`FoliageSway`), a vertex stage with a CPU mirror, so a host gets plausible tree, shrub and
   reed motion without writing WGSL.
3. **A dynamic-caster shadow class**: casters whose vertex stage reads the clock are redrawn in the nearest cached
   cascades (and local-light tiles) every frame, behind a quality-tier default and a runtime switch; static casters keep
   their cache.
4. **Stage 2 (grass):** the terrain grass field joins the class (its shadow sways in the refreshed cascades).

## 2. API shape

### 2.1 Wind (`regia_render::foliage_wind`, re-exported at the crate root)

```rust,ignore
impl FrameRenderer {
    pub fn set_foliage_wind(&mut self, wind_m_per_s: Option<[f32; 2]>); // None = slot as the material carries it
    pub fn foliage_wind(&self) -> Option<[f32; 2]>;
}
pub const FOLIAGE_WIND_MAX_M_PER_S: f32 = 40.0;   // longer vectors are shortened to it
pub fn sanitize_wind(wind: [f32; 2]) -> [f32; 2];  // non-finite → 0, clamp the length
```

Graph parameter slot 0 is `[time, previous time, wind x, wind z]`: the renderer writes `zw` into every graph with
`uses_time` when a wind is set, beside the clock it already writes into `xy`. Graphs that do not read `zw` are unchanged.

### 2.2 Foliage sway

```rust,ignore
pub struct FoliageSway { pub stiffness: f32, pub height_m: f32, pub flutter_m: f32,
                         pub gust_period_s: f32, pub max_wind_m_per_s: f32,
                         pub style: WindStyle }                // Classic (default) | Natural, § 2.6
impl FoliageSway {
    pub fn graph_code(surface_wgsl: &str) -> GraphCode;   // the surface + FOLIAGE_SWAY_VERTEX
    pub fn graph(&self, surface_wgsl: &str) -> SurfaceGraph;
    pub fn apply(&self, graph: &mut SurfaceGraph);         // slots 14-15, uses_time, max_vertex_offset
    pub fn max_offset(&self) -> f32;                       // bound for culling and shadow pages
    pub fn offset(&self, world, normal, red, origin, time, wind) -> [f32; 3]; // CPU mirror of the stage
}
pub const FOLIAGE_SWAY_SLOT: usize = 15;        // [stiffness, height_m, flutter_m, gust_period_s]
pub const FOLIAGE_SWAY_LIMIT_SLOT: usize = 14;  // [max_wind_m_per_s, style (0 classic, 1 natural), 0, 0]
pub const FOLIAGE_SWAY_VERTEX: &str;            // the WGSL stage: a call to regia::foliage_sway::offset
```

The motion itself is one function in the shader library, `regia::foliage_sway::offset`
(`shaders/foliage_sway.wgsl`), shared with the visibility path (spec 98 § 8d, ADR-3916).

Motion of the default `Classic` style (Regia's formulation, § 10): `h` = height above the object origin over `height_m`, clamped to 0..1; `gust` =
`0.7 + 0.2 sin(ωt + φ) + 0.1 sin(3ωt + 1.7φ)` with `ω = 2π / gust_period_s` evaluated on `fract(t / period)` and `φ` from
the origin's X and Z; `amp = min(stiffness · speed · gust, height_m / 2)`; offset = downwind `amp h²`, down
`amp² h³ / (2 height_m)`, plus a flutter along the normal of `flutter_m · red · speed / (speed + 5) · sin(7ωt + ψ)` with
`ψ` from the rest position. `max_offset` bounds the length of every offset.

### 2.3 Dynamic-caster shadow class

```rust,ignore
pub enum DynamicCasterShadows { Tier /* default */, Off, On { cascades: u8, local_lights: bool } }
pub struct DynamicCasterPolicy { pub cascade_mask: u8, pub local_lights: bool, pub unpaged: bool }
pub struct DynamicCasterReport { pub policy, pub animated_casters: u32, pub skinned_casters: u32,
                                 pub shared_boxes: u32, pub dynamic_pages: u32, pub dynamic_atlas_tiles: u32 }
impl FrameRenderer {
    pub fn set_dynamic_caster_shadows(&mut self, mode: DynamicCasterShadows);
    pub fn dynamic_caster_shadows(&self) -> DynamicCasterShadows;
    pub fn dynamic_caster_report(&self) -> DynamicCasterReport; // also ShadowCacheReport::dynamic
}
```

A draw is in the dynamic class when its ready graph has a vertex stage and `uses_time`, or when it is a
vertex-shader-skinned draw with a joint-palette bound whose palette changed since the last frame (ADR-5418, counted
again in `skinned_casters`; a held pose is not dynamic). The caches bound a
skinned caster by the sphere of its palette's object box for the frame (ADR-248), as a rigid caster is bounded by its
mesh sphere, so a pose that moves the box dirties pages in any mode, and the class refreshes the pose changes inside it.
Compute-skinned twins and skins without a bound stay unbounded: a frame with one redraws whole cascades. Each frame the
policy is resolved from the mode and the frame quality:

| Frame quality | `Tier` resolves to |
|---|---|
| Unpaged sun shadows (Mobile Low, Mobile High) | `unpaged`: nothing to refresh, every cascade and tile is drawn each frame |
| Paged, 4 cascades (Desktop High, Ultra, Cinematic) | cascades 0 and 1, plus local-light tiles when the atlas is on |
| Paged, fewer cascades (Desktop Low, Deck) | cascade 0 |

In a refreshed cascade, a dynamic caster dirties the pages its current footprint and the footprint its pages were last
drawn with cover, every frame; it is not counted as a move (`caster_events`). Outside the mask it is compared like any
other caster (bounds and pose), so a far page holds the pose it was last drawn with. With `local_lights`, atlas tiles whose
light sphere reaches a dynamic caster's bounds are redrawn every frame. `Off` reproduces the cache behavior before this
spec.

### 2.4 Stage 2: grass

Renderer side (built): `SharedPassDraw::dynamic_shadow_boxes(&self) -> &[[[f32; 3]; 2]]` and
`dynamic_shadow_box_cascades(&self) -> u8`, provided methods (default no boxes, every cascade). On a paged frame, each
reported world box joins the class as a dynamic caster in the cascades both the policy and the draw name: its sun pages
there are redrawn every frame, and the shared draw is asked for those regions through `draw_shadow_region`. Boxes never
reach the local-light atlas, because `SharedPass::Shadow` draws cascades only: a tile redrawn for a box would not contain
its content. `DynamicCasterReport::shared_boxes` counts the boxes that joined.

Terrain side (built): `SharedFrameInfo` carries the class's cascade mask (`dynamic_shadow_cascades`, 0 when the class is
off or the frame is unpaged) and the scene wind (`foliage_wind`). The grass field (`regia_terrain::gpu::grass`):

- **Sways its shadow** in the cascades the mask names and it draws into (`vs_grass_shadow_sway`, the colour pass's swayed
  blade); other cascades keep the rest pose (`vs_grass_shadow`). Only once the sway clock has changed since the layer was
  bound: a still clock (a host that never calls `set_time`, every existing golden) keeps the rest-pose shadows and the same
  bytes as before.
- **Reports its boxes and their cascades** (`GrassLayer::dynamic_shadow_boxes` and `dynamic_shadow_cascades`, through
  `TerrainSharedDraw`, main camera only): the High grass tier casts into one cascade, so on Desktop High the class's
  second cascade keeps its pages. The boxes are the shadow patches merged into one box per 16 m cell of their corner (cells in row order), each the
  ground's height range under its patches plus the blade headroom. The renderer plans casters before the shared draw
  prepares, so the boxes it uses are the previous frame's: for a held camera they are the same; a moving camera changes the
  grass's cascade key (it holds the eye) and redraws the whole cascade anyway.
- **Folds the mask into its cascade key**, so turning the class on or off redraws the cascade once.
- **Reads the scene wind** when one is set: the blades sway along its direction, and the sway scale is multiplied by
  `speed / 5 m/s`, clamped to 0..2 (`grass_wind`). Calm air (`[0, 0]`) stops the grass. Without a scene wind the field
  sways in the terrain wind (`TerrainWind`, spec 82 § 4, ADR-1853) alone. With both (direct `foliage_wind` path), the scene wind replaces the terrain
  wind's direction and multiplies its strength; through the weather coupling (`couple_scene_wind`, spec 99 § 3.3) `TerrainWind::steered_by` instead *sets* strength to `speed / 5` (1/32 steps, 0..=4), so zero terrain strength does not freeze it there; frequency and gust stay the terrain wind's (`effective_wind`), so a
  terrain wind of strength 0 stays still under any scene wind. The patch culling headroom grows with the combined factor
  above 1, and each dynamic box is padded by the sway's sideways peak (amplitude × scale × (1 + gust)) so a blade at the
  edge of the field cannot lean into a page the class does not refresh.
- `GrassReport` gains `shadow_sway_cascades` (cascades whose grass shadow sways this frame) and `dynamic_boxes`.

### 2.5 Adopters: plain materials, scatter and the material graph (ADR-2304)

```rust,ignore
impl FoliageSway {
    pub fn adopt(&self, material: &LabMaterial) -> Result<LabMaterial, SwayAdoptError>;
    pub fn of_material(material: &LabMaterial) -> Option<FoliageSway>;
    pub fn still(material: &LabMaterial) -> LabMaterial;
    pub fn plain(material: &LabMaterial) -> Option<LabMaterial>; // an adopted material's maps back in the standard slots
}
pub const FOLIAGE_SURFACE: &str;              // the standard surface as a graph surface function
pub const FOLIAGE_SURFACE_FLAGS_SLOT: usize = 1; // [normal map, packed ORM, vertex occlusion, 0]
pub enum SwayAdoptError { VertexStageTaken, SlotsTaken } // codes sway_vertex_stage_taken, sway_slots_taken
// regia_terrain
pub const PAINTED_FLAG_STILL: u16 = 1;        // PaintedInstance::flags bit 0
pub struct ScatterBrush { /* ... */ pub wind: bool } // default true; false writes PAINTED_FLAG_STILL
// regia_matgraph: Node::Wind (Vec2), .matgraph v3, .matfunc v2
```

- **Plain material** (no graph): becomes a graph material with surface `FOLIAGE_SURFACE` and the sway stage. Its albedo,
  normal, occlusion/ORM and emissive maps move into graph texture slots 0..3 (the shared or dynamic image, else the RGBA8
  bytes with the cooked block-compressed chain); the standard slots are cleared. The surface reads base colour, metallic,
  roughness, normal scale, occlusion strength and emissive from the material's standard uniform block, so a later change of
  those fields (the scatter field's per-class, per-variant tint of `base_color`) still applies. The normal's Z is rebuilt
  from X and Y. Slot 1 holds the flags; alpha mode, cutoff, double-sided and the other state stay the material's.
- **Graph material without a vertex stage:** keeps its surface, textures and parameters and gains the stage, unless its code
  reads `material.graph[14]` or `[15]` (`SlotsTaken`). **With its own vertex stage:** refused (`VertexStageTaken`).
  **Already swaying:** the new values, same code (same pipelines).
- `still` drops the stage, zeroes slots 14-15 and keeps the surface (time-reading only if the surface itself reads the clock).
  `plain` gives an adopted (`FOLIAGE_SURFACE`) material's maps back to the standard slots for CPU readers: the scatter
  field's impostor bake uses it, so a swaying mesh bakes the same far cards as the plain one.
- **Scatter.** A `ScatterMeshAsset` whose material (or a part's) sways draws swaying for every seeded instance. A painted
  instance with `PAINTED_FLAG_STILL` draws a still copy of that material (one per tint variant; one for shared materials,
  prewarmed with the others). A material that does not sway ignores the flag. The bit was reserved and written 0, so older
  `.scatterpaint.bin` files (format 1, unchanged) follow the wind.
- **Material graph.** `Wind` is a `Vec2` node, `material.graph[0].zw` in the surface and the vertex stage; reading it makes
  the graph time-reading (`uses_time`), so the renderer writes slot 0. In the velocity pass the previous-frame offset uses
  this frame's wind.
- **Not done: the shared prepare ahead of the caster plan.** The prepare consumes the caster plan's outputs
  (`shadow_regions`, the cascade count and size, the plan it mutates); moving it means splitting the terrain prepare into a
  pre-plan box step and a post-plan region step. Mesh casters are planned in their own frame, and the grass's cascade key
  (eye, patches, mask) redraws its whole cascade whenever its boxes change, so the late boxes match the drawn ones (ADR-2304).

### 2.6 Natural style (ADR-2446)

`FoliageSway::style` (`regia_render::WindStyle`, stored as slot 14 `y`) picks the motion. `Classic` (the default) is the
periodic gust of § 2.2 and its code path is unchanged, so default frames and goldens do not move. `Natural` takes the gust
from a travelling two-octave value-noise field thresholded so the air is exactly calm part of the time, slows tall
plants (sway frequency `0.55 / sqrt(height)`, clamped), takes the phase from position noise instead of a plane wave, and
gives each plant a flutter band 2.6 to 4.6 times the sway, picked from its stiffness and height bits and gated by the gust.
The same `max_offset` bounds both styles. The natural field is not periodic, so it jumps once per material-clock wrap
(3600 s, ADR-1238). The grass field's `TerrainWind::style` is the same switch (spec 82). Agent: `material.set_sway
{ sway: { style } }` (a wrong name answers `F0002`).

## 3. Files and formats

No new file format. `FoliageSway`'s values live in an existing surface graph's parameter block
(slots 14 and 15); slot 0 `zw` was unused and is now a built-in (documented on `SurfaceGraph::uses_time` and the
guide page). § 2.5 (ADR-2304): the `.matgraph` `Wind` node is a node-set addition: `.matgraph` 2 → 3 and `.matfunc` 1 → 2
(its fingerprint lists the node set), no-op migrators, `regia upgrade-project` support for both, fuzz target `matgraph-wind`;
the cooked graph stores generated WGSL and is unchanged. `PAINTED_FLAG_STILL` uses the reserved painted-instance `flags`
(scatter-paint format 1 unchanged).

## 4. Agent-protocol surface

- `render.foliage_wind` (read): `{ wind_m_per_s, speed_m_per_s, max_m_per_s, dynamic_casters: { mode, cascades,
  local_lights }, last_frame: { paged, cascade_mask, refreshed_cascades, local_lights, animated_casters, skinned_casters,
  shared_boxes, dynamic_pages, dynamic_atlas_tiles } }` (`skinned_casters`, the bounded skinned casters among
  `animated_casters`: ADR-5418). `cascades` and `local_lights` are `null` unless the mode is `on`.
- `render.set_foliage_wind { wind_m_per_s: [x, z] | null }` → `{ before, after }`. With a `SceneWind` resource the call
  also sets it (`Manual` with the value, `Off` for `null`; spec 99 § 3.3). A refused call (`F0002`, `F0003`) changes
  neither (ADR-5656).
- `render.set_dynamic_caster_shadows { mode: "tier" | "off" | "on", cascades?: 1-4, local_lights?: bool }` →
  `{ before, after }`.

- `material.sway { entity }` (read) → `{ entity, sway: { stiffness, height_m, flutter_m, gust_period_s, max_wind_m_per_s,
  max_offset_m, style } | null, graph, vertex_stage, animated }` (§ 2.5; `style` `"classic"` | `"natural"`, § 2.6).
- `material.set_sway { entity, sway: { ...fields? } | null }` → `{ entity, before, after }`; missing fields keep the current
  sway's (else the default's); `null` removes the stage; the entity gets its own material copy.
- `terrain.scatter_stroke` gains `wind` (boolean, default true).

Errors: `F0002` (bad parameters, message names the field), `F0003` (no `RuntimeRenderer`, hint says what to add; an entity
without a mesh), `sway_vertex_stage_taken`, `sway_slots_taken` (§ 2.5).

## 5. Guide page

`book/agents/foliage-wind.md`: the scene wind, the ready-made sway, the three methods, the tier table, the cost model, and
checks an agent can run (set a wind, step frames, read `dynamic_pages`; `off` freezes the shadow while the plant sways).

## 6. Determinism

The sway is a pure function of the rest position, normal, vertex colour, object origin, the material clock and the wind. The
clock is already deterministic for a given gameplay time (ADR-1238); the wind is host state set by a call. The CPU mirror
uses the same formula in `f32` (bit-identical results across runs on one machine; not bit-identical to the GPU). The page
cache decision depends only on the caster list, the policy and the cascade matrices, so two runs of the same frames redraw
the same pages. A gust period that divides 3600 s loops across the clock's hourly wrap without a seam.

## 7. Tests and acceptance

Unit (no GPU): the stage composes and validates in every host (forward, instanced, depth, shadow, velocity); calm air and the
root give no offset; the top bends downwind and drops; no offset exceeds `max_offset` over randomized inputs and the bound is
not loose by more than 2×; continuity across the clock wrap for a dividing period; same inputs, same bits; neighbours gust
out of step; `apply` sets slots, `uses_time` and the bound; sanitizing; the tier table. Page cache: a dynamic caster in a
refreshed cascade dirties its pages (and only its) every frame and is not a move event; with the mask clear it keeps every
page; a moving dynamic caster dirties old and new footprints; dropping the class keeps the held pose's pages; the atlas
refresh dirties only the tiles a dynamic caster reaches and reports them. Agent: defaults, set/clamp/clear the wind, bad
wind and bad modes answer `F0002`, no renderer answers `F0003`.

Integration (lavapipe, `crates/regia_render/tests/foliage_wind.rs`): a paged Desktop High frame of a ground plane and a
swaying card under a low sun. With the class on, a frame at another clock value redraws the card's pages and its shadow on the
ground changes; with `Off` the pages are kept and the shadow pixels do not change while the card itself still moves; the report
counts the animated caster. With a shadowed point light beside the card, its atlas tile is redrawn every frame only with
`local_lights`. A shared draw's box refreshes the cascades it reports (none for an empty mask) and no atlas tile. The frame's
CPU cost with the class on and off is printed (lavapipe numbers are correctness only).

Grass (lavapipe, `crates/regia_terrain/tests/grass_shadow_sway.rs`, stage 2): a swaying summer field on the Ultra grass tier
under a paged Desktop High frame. A still clock renders the same bytes with the class on and off and reports no swaying
cascade and no box; a running clock sways cascade 0's grass shadow with the class on (`shadow_sway_cascades` 1, boxes > 0,
`shared_boxes` equal to them, `dynamic_pages` > 0) and not with it off, and the two frames differ at the same clock; two rigs
with the same clock render the same bytes; a scene wind changes the frame; on the High grass tier (one shadow cascade) a class
refreshing two cascades redraws exactly the pages a one-cascade class does. Unit: shadow patches merge per cell in a stable
order and every patch lies inside a box; the wind's direction and factor (`grass_wind`). An ignored bench prints the
frame cost with the class on and off.

Adopters (§ 2.5): unit tests compose `FOLIAGE_SURFACE` with and without the stage in every host; a plain material moves its
maps and keeps its state; re-adopting keeps the key; graphs with their own stage or slots 14-15 are refused; `still` keeps
the surface. `crates/regia_terrain/tests/scatter_foliage_wind.rs`: the brush writes the flag only with wind off; seeded and
windy painted instances sway, still ones draw the shared still copy; a plain material ignores the flag; the flag survives a
save. Agent: `material.sway` / `set_sway` round trip, ranges and codes; `terrain.scatter_stroke { wind: false }`. Matgraph:
`Wind` compiles in both stages with `uses_time`; v1/v2 graphs and v1 functions migrate; `upgrade-project` rewrites both.
Sample: `cargo run -p terrain_lab -- --foliage-wind <dir>` (a gust moves the frame, the class counts the trees);
`--bench --size 2560x1440` prints the GPU shadow time with the class on and off.

Acceptance (lead, real GPU): on the 5080 at 1440p Desktop High, a scene with swaying foliage over a quarter of cascade 0
stays within the budget below; by-eye capture of a tree shadow moving with the tree.
Measured 2026-11-03 on an RTX 5090 (`docs/perf-log.md`, "Foliage sway adopters"): the sample's wood (43 adopted casters,
38 dynamic pages per frame) adds 0.027 ms of shadow time at 1440p; measured 2026-11-11 on the RTX 5080 (same scene, two rounds):
0.045-0.057 ms (ADR-5656). Still open: a scene sized to the quarter of cascade 0 this budget names (the sample's share of cascade
0's pages is not reported).

## 8. Performance budget

- CPU: classification is one flag per draw; the cache does no extra work per static caster. Target ≤ 0.05 ms for 10k casters.
- GPU (Desktop High, 1440p, 5080): ≤ 0.5 ms extra shadow time when dynamic casters cover a quarter of cascade 0's pages and a
  tenth of cascade 1's. The cost is proportional to the pages redrawn (every caster and the terrain in them), reported as
  `dynamic_pages`. A dirty page in cascade 0 also redraws the near sun tile (when a mesh keeps it live), as any dirty page
  there always has. Lavapipe measures correctness and page counts only; the GPU numbers need the lead's run.
- Mobile: zero (unpaged; already redrawn each frame).

## 9. Platform notes

Linux, Windows and Android aarch64 compile the same code; nothing is platform-specific. The WGSL stage uses no loops, no
derivatives and no storage writes (FXC-safe under ADR-201; validated through the same composer as every graph). Android's
tiers are unpaged, so only the wind reaches them. macOS builds as before (testing only).

## 10. Provenance

The bend and flutter split follows the main-plus-detail idea in Sousa, "Vegetation Procedural Animation and Shading",
GPU Gems 3 ch. 16 (2007), and the per-blade sway of Pelzer, "Rendering Countless Blades of Waving Grass", GPU Gems ch. 7
(2004); the separation of cached static and per-frame dynamic shadow content follows the caching discussion in Eisemann,
Schwarz, Assarsson and Wimmer, "Real-Time Shadows" (2011). The formulas above and the code are Regia's own.

## 11. Out of scope

A per-object wind field or wind volumes, wind from the cloud or weather systems (the host copies a value), branch hierarchies
baked into vertex data (pivot painting), impostors, a separate dynamic-only depth layer per cascade
(the class redraws whole pages; a split layer is a later optimization if the page cost dominates), SDF and probe updates for
swaying plants (the baked probes and the mesh SDFs use the rest pose).
