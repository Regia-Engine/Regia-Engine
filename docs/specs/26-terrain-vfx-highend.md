# Spec 26 — Terrain & tunnels, GPU VFX & decals, desktop high-end renderer

Pulled forward from Tier 2 because Direct Order 27 needs them (spec 25 § 5). New crates:
`regia_terrain` and `regia_vfx` (layer 2, depend on `regia_render`); high-end features live in
`regia_render` behind the `Desktop High/Ultra` profiles. Each section starts with a short design ADR
citing public sources (`01-cleanroom.md` § 5).

## 1. Terrain (`regia_terrain`)
- **Source of truth:** a 16-bit grayscale PNG heightfield + `world_width` + height scale/offset, read by
  the same math the game's simulation uses (DO27: `TerrainSampler`, bilinear). Regia Engine never resamples
  the heights the sim collides with; render geometry may add detail *on top* (displacement/normal
  detail). On a smooth heightfield the base rendered surface matches the bilinear sampler within 1 cm
  (the 513² map over 256 m, 10k random points). A non-coplanar cell is two triangles, so its centre can
  leave the bilinear surface by up to a quarter of the corner span: a 2×2 cell with heights 0, 0, 0, 1
  differs by 0.25 m at the centre. The renderer stays triangulated (ADR-5083). *(ADR-8019 A-7: a lockstep host on
  `regia_world_geom` Stage B adopts the triangulated seam lattice as its sim ground, index `i / (n − 1)`, so the sim
  and the drawn LOD-0 surface are one surface (spec 121 § 3); the bilinear sampler stays for hosts that keep their own.)*
- Rendering: CDLOD-style quadtree (Strugar, public paper) with morphing between LODs, vertex-shader
  height sampling, tiles up to 4097² samples; normals from heightfield + detail normal maps.
  Coarse vertices read a box-filtered height pyramid at the level of their cell (continuous with the
  morph), the quadtree is deep enough that a leaf cell is at most one heightfield texel, and shading
  normals read the pyramid at the pixel footprint — no saw-tooth on sharp walls at any distance (ADR-044).
  Pyramid level `k` is the GPU mip size (`max(1, n >> k)`); a texel averages the two parent texels under it, and the
  last texel of an odd side the four parent texels centred on it, so no sample of an odd (`2^n + 1`) side is dropped and
  every texel keeps its position (ADR-3367).
- Materials: splat-mapped layered material, up to 8 layers (weight maps), triplanar option per layer
  for cliffs, height-blended transitions, distance-based macro variation; wet/dry parameter.
  Altitude bands reaching the map's lowest/highest ground stay open there (no fallback caps on crests);
  the anti-tiling cell is at most about one texture repeat per layer; steep faces read as rock: strata
  whose bed coordinate is altitude + a per-map dip plane + lateral noise drift, three bed tones, broken
  ledges, a fractured-rock detail normal, and talus at the foot. Near-field relief lighting covers
  heightfield self-shadowing, concavity occlusion and crust. Specular is GGX, with discrete-facet glints on
  smooth ice/snow only. On the shared frame the sky light is the planet's sky/ground hemisphere: the same
  bake meshes get, passed in `SharedFrameInfo::sky_ambient`. A host may override it with `SkyAmbient` (ADR-044).
- Holes mask (for tunnel entrances); physics heightfield collider (for cosmetic debris only in DO27;
  built in M14 together with the debris physics subset, `specs/15`).
- **Scatter:** seeded procedural placement (rules: density, slope/height/layer filters, exclusion
  splines/volumes, scale/rotation jitter, alignment to normal) → GPU-instanced with per-cell culling,
  distance fade, LOD. Same seed ⇒ same placement on every client (DO27 requirement).
  Implemented as `.scatter.ron` v2 (ADR-047, `docs/formats/terrain.md` § 2): a set of classes, each
  with weighted glTF meshes (authored `_LODn`), a density map and noise, slope/altitude/layer-mask
  filters, exclusion paths, tilt/sink, tint, base blend and contact AO. Placement is per (class,
  cell) and streams in 64 m tiles around the camera; instances go through the frame renderer's
  GPU cull (frustum + Hi-Z) as one instanced indirect draw per (mesh, material) batch. A soft
  contact-occlusion map under each instance is sampled by the terrain colour pass. Rocks cast as
  bounded shadow casters (`ShadowCast::Local`): their own sun cascade and the next, only into
  the shadow pages they cover; per-class `cast_shadows` / `shadow_max_lod` / `shadow_distance`. *(ADR-5629: the scatter ADR is ADR-047, not
  ADR-049. `placement_bits_are_pinned_for_every_platform` pins the placement bits of a fixed set, so a run on another
  OS or CPU checks the every-client rule. It found that a mesh `scale_bias` other than 1 went through the platform
  `powf` and placed different scale bits on x86_64 Linux and aarch64 macOS; placement now uses `det_powf` and
  `det_acos`. The painted and clump placements still call platform `powf`/`acos`/`sin`/`cos`, a v0.5.x gap.)*
- **Water** (for shoreline maps): planar water with depth-based colour, screen-space refraction,
  reflections (SSR + probe fallback), shoreline foam; no simulation. Superseded by
  [`water.md`](water.md) (ADR-1620): water bodies in the runtime frame (ADR-1399), the terrain's
  plane drawn there, and a deterministic swell with surface and ray queries.
- Editor: import heightfield PNG, layer painting, scatter rule editing with live preview; sculpting is
  optional for DO27 (its maps come from generators) — sculpt tools are Tier 2.

## 2. Tunnels and carved interiors
- Tunnel render meshes are the game's collision triangles + render detail, imported as glTF with a
  provenance link to the collision source; a CI check compares the render mesh's "collision layer"
  against the sim's triangles. *(Status, ADR-5629: `regia_terrain::tunnel` keeps a `CollisionProvenance` (the
  collision triangle count) with each tunnel, but no CI check compares the render mesh with the sim triangles yet;
  a v0.5.x gap.)*
- Interior lighting: local lights + probe GI (§ 4); occlusion of sky/sun inside volumes via
  `InteriorVolume` components (fade sky light, switch reverb snapshot — audio spec). *(Status, ADR-5629: the
  renderer's `regia_render::InteriorVolume` drops the sky light inside its box (ADR-4757 for the aerial term); the
  audio side is its own components, `AudioReverbZone` and `AudioInteriorVolume`, which a game puts on the same
  entity. There is no single shared component.)*

## 2a. Terrain mesh layers (tunnels, overhangs, stacked levels)

Status: proposed (ADR-7974, design lane do27-terrain-mesh-layers-design, 2026-11-15; **rewritten by ADR-8019**, lane
world-geometry-design, 2026-11-16). Nothing below is built yet. Line: v0.6 (the DO27-absorb plan, phase 4). Citations
are `file:line` at `55eff2751`.

**What ADR-8019 changed.** Sim collision is in scope. The canonical geometry, the query, the wall push, segments,
sections, portals, zones, blockers, the sim ground and D1 edits live in the GPU-free crate `regia_world_geom`
(spec 121), which DO27's lockstep sim links. This section keeps the presentation half (the seam, streaming, LOD,
lighting, picks) and reads every geometric answer from that crate, so render, query and sim agree by construction.
Rewritten: § 2a.1, § 2a.3, § 2a.7 to § 2a.13; § 2a.4 items 2, 7 and 8.

### 2a.1 Need and scope

A heightfield holds one height per XZ point. Tunnels under the map, overhangs, arches, cliff shelves and maps with
3 to 5 stacked levels need geometry the heightfield cannot hold. The owner's directive (2026-11-16) puts all of it in
the engine, from the description to the frame and to the lockstep query (ADR-8019). A **terrain mesh layer** is a
static mesh the terrain owns, cooked from one source together with its canonical sim triangles. It:

1. meets the heightfield at named holes with a stitched seam: watertight, no lip, no z-fight, at every LOD;
2. streams and changes LOD with the terrain;
3. casts and receives shadows, joins the GI bake, the path tracer and picks;
4. is drawn from the same cook that writes the canonical triangles `regia_world_geom` answers from (spec 121): the
   floor under and the ceiling over a point, the wall push, segments, sections and zones. The sim, the camera, foot
   planting, decals and footstep audio read that one answer.

Out of scope here: the query itself, sim collision, blockers and D1 edits (spec 121); deformable volumes (DO27 stage
D2, the voxel core design, which reuses the blocker and edit calls); layer-to-layer seams. Two layers that meet are
one authored mesh, or the author welds them.

### 2a.2 What exists

- **Named holes.** `NamedHole` (`crates/regia_terrain/src/holes.rs:82`, ADR-1595) has an id, `open` and a
  `HoleShape`. Id errors are at `holes.rs:50-61`. `<map>.holes.ron` is v2 (`holes.rs:925`). The GPU cut is a
  per-pixel discard where one of at most 64 coverage windows (256² texels, `holes.rs:411-414`) reaches 50 %. The
  discard runs at the morphed world XZ (`gpu/terrain.wgsl:820-847`). Whole triangles deep inside a hole are dropped by
  index (`build_node_indices`, `holes.rs:1180`). Each window has two unused words (`gpu_words`, `holes.rs:774`).
- **Tunnels.** `MeshTunnel` (`mesh_tunnel.rs:16-27`) is a hole mask, a render mesh, collision triangles and a
  `CollisionProvenance` (`tunnel.rs:73`). `from_collision` (`mesh_tunnel.rs:52`) builds the render mesh from the game's
  triangles plus displacement detail (`tunnel_mesh_from_collision`, `tunnel.rs:93`). Nothing at run time builds a
  collider from it: `collision_trimesh` (`mesh_tunnel.rs:102`) is called only by tests. The tunnel acceptance scene
  hides the mouth seam with hand-placed lip strips 1 cm under the ground
  (`crates/regia_terrain/tests/tunnel_scene/README.md`, "Lip").
- **CDLOD.** Strugar's quadtree with per-vertex morphing (`cdlod.rs:1-29`). Neighbors differ by at most one level,
  and their shared edges match (`cdlod.rs:12-29`), which `lod_seams::lod_seam_report` (`lod_seams.rs:217`) measures on
  the CPU mirror of the vertex shader (`eval_node_vertex_filtered`, `cdlod.rs:526`). Shadow cascades select their own
  nodes (`select_nodes_min_level`, `cdlod.rs:410`). `terrain_surface_height` (`cdlod.rs:558`) is the triangulated
  LOD-0 surface.
- **Streaming.** The heightfield is resident. Texture tiles stream through `TileCache` around the `StreamViews`
  sources (`stream_hooks.rs:25`, spec 78 § 13.1). Collision is tiled when `TerrainTileCollision`
  (`crates/regia_scene/src/streaming.rs:1241`, default `tile_m` 128) is on: tiles hang off the terrain's static body
  and are held by cells and sources (`crates/regia_physics/src/terrain_tiles.rs:1-26`, `build_tile` `:744`). Scatter
  streams 64 m tiles (`scatter/field.rs:47`). Nothing streams meshes per terrain tile.
- **Joins today.** A `MeshRenderer` (`crates/regia_render/src/runtime.rs:588`) casts shadows by default
  (`ShadowCast::All`, `frame.rs:422`). `world_pick` casts every `MeshRenderer` filtered by `MapLayer`
  (`crates/regia_scene/src/components.rs:122`, a `u8` newtype). Terrain is always layer 0 and is marched separately
  with holes skipped (`crates/regia_pick/src/world.rs:678-747`). The path tracer takes every drawn mesh instance from
  `FrameExport` (`crates/regia_render/src/scene_export.rs:68`, `regia_pt::extract::from_export_with` `extract.rs:853`).
  Terrain goes in as a `ReferenceHeightfield` whose hole samples are NaN (`gpu/shared.rs:2102-2125`), which removes
  whole cells. The GI bake gathers `GiMobility::Static` entities with a `MeshSource`
  (`crates/regia_agent/src/bake_export.rs:165`) and builds terrain from a heightfield or a tagged mesh
  (`crates/regia_pt/src/bake_desc.rs:1121-1190`). There is no separate baked AO.
- **Queries.** `terrain.raycast` (`crates/regia_agent/src/dispatch.rs:981`) and `terrain.get` (`:987`,
  `terrain_runtime.rs:407`) answer one heightfield height. The water query `surface_at`
  (`water_body/query.rs:33`) is the model for a per-point surface query. There is no query across stacked surfaces.
- **Volume meshes.** `SdfField` (`volume_mesh.rs:114`) and `mesh_chunk` (`:1149`) do render-only dual contouring that
  is crack-free between chunks (`volume_mesh.rs:26-38`).

### 2a.3 Model

A map's layers come from one source file, `<map>.meshlayers.ron` (§ 2a.10). It sits next to the map and is found the
way `holes_ron_path` finds holes (`holes.rs:988`). The scene format does not change. Run-time types (sketch):

```rust
/// The mesh layers of one terrain, on the terrain entity (inserted by the level-terrain load when
/// `<map>.meshlayers.ron` or its cooked form exists). Owns the presentation half and shares the always-resident
/// world geometry (spec 121) with the sim.
#[derive(Component, Clone, Debug)]
pub struct TerrainMeshLayers {
    /// The canonical geometry, query index, sections, portals, zones and blocker plugs (`regia_world_geom`).
    pub geom: Arc<WorldGeom>,
    /// The geometry state the presentation draws: blockers, D1 edits and the generation. An engine-native game owns
    /// it here; a lockstep host (DO27) owns it in its sim and mirrors it each tick with `sync_state`.
    pub state: GeomState,
    /// Layers in id order (deterministic).
    pub layers: Vec<TerrainMeshLayer>,
    /// The seam lattice step, meters: the cooked sim lattice pitch (§ 2a.4; spec 121 § 3).
    pub lattice_step: f32,
    /// Chunk side, meters: a CDLOD node size (`node_world_size` at `chunk_level`).
    pub chunk_level: u32,
    /// Bumped when a layer, a mouth, a blocker or a terrain height under a mouth changes (`GeomState::generation`
    /// plus presentation-only changes).
    pub generation: u64,
}

/// One mesh layer: a tunnel system, an overhang, an arch, a shelf, or one level of a stacked map.
#[derive(Clone, Debug)]
pub struct TerrainMeshLayer {
    /// 1..=64 ASCII letters, digits, `_`, `-`, `.` (the named-hole id rule, `valid_hole_id`).
    pub id: String,
    /// Named holes this layer fills (each hole is filled by at most one layer).
    pub fills: Vec<String>,
    /// Chunks: one per CDLOD node at `chunk_level` that the layer touches, sorted by (z, x).
    pub chunks: Vec<LayerChunk>,
    /// One mouth per filled hole (§ 2a.4), sorted by hole id.
    pub mouths: Vec<LayerMouth>,
    /// Pick layer of the chunk entities (`MapLayer`); default 0.
    pub map_layer: u8,
    /// Chunk entities cast shadows; default true.
    pub cast_shadows: bool,
    /// Build engine-physics colliders from the canonical triangles, for hosts that do not lockstep (§ 2a.8); default
    /// false (DO27: false; its sim calls `regia_world_geom`).
    pub collider: bool,
}

/// One chunk: a CDLOD node and its LOD meshes (LOD 0 full detail; cooked layers only have more).
#[derive(Clone, Debug)]
pub struct LayerChunk {
    pub node: QuadNode,
    pub lods: Vec<AssetId>,            // CookedMesh v3 sub-assets, unchanged format
    pub max_error_m: Vec<f32>,         // geometric error of each LOD (meshopt result)
    pub bounds: Aabb,
    pub material: AssetId,
}

/// Where a layer meets the heightfield at one named hole.
#[derive(Clone, Debug)]
pub struct LayerMouth {
    pub hole: String,
    /// The lattice cut: seam-lattice triangles removed from the terrain (§ 2a.4), as sorted
    /// (row, first column, last column, triangle mask) runs.
    pub cut: Vec<LatticeRun>,
    /// The ring: closed loops of seam-lattice vertices (integer lattice coordinates) on the cut's boundary.
    pub rings: Vec<Vec<[i32; 2]>>,
    /// The band: ring + zipper strip + skirt + throat, indices into `band_vertices`;
    /// ring vertices are flagged so the draw can attach them (§ 2a.4).
    pub band: BandMesh,
    /// The ground collar: LOD-0 lattice triangles that a cell-granular cut drops outside `cut` (§ 2a.6).
    pub collar: Vec<[[f32; 3]; 3]>,
}
```

A layer that fills no hole is an overhang, an arch or a shelf. It has chunks and no mouths, and may cross the ground
like any rock. A stacked map is several layers. Their joins to the surface are mouths, and their joins to each other
are authored. `MeshTunnel` becomes a one-layer case: `TerrainMeshLayer::from_mesh_tunnel(&MeshTunnel, hole_id)` reuses
its render mesh, collision triangles and provenance. Its `HolesMask` is not used, because the hole is the named one.

```rust
impl TerrainMeshLayers {
    /// Mirrors a sim-owned state (blockers, D1 edits, generation) into the presentation. Closed blocker plugs are
    /// drawn, open ones are not; a changed generation re-places the affected bands and collars.
    pub fn sync_state(&mut self, state: &GeomState);
}
```

### 2a.4 Seam stitch (cook) and crack-free LOD (run time)

**Which side moves.** The heightfield never moves. It is what the sim collides with (§ 1). The layer moves, in two
places. At cook time a generated zipper strip joins the authored mesh to a ring that the terrain's own lattice
defines. At run time the ring's vertices are re-placed on the terrain surface the current pass draws.

1. **Seam lattice.** `s` is the cooked sim lattice pitch of spec 121 § 3: the heightfield's sample pitch
   `width_mm / (lattice_n − 1)` with vertices on integer millimetres (`x_mm(i)`), equal to
   `node_world_size(level 0) / grid_size` (cook check `lattice_mismatch`). It does not depend on cliff relief
   (ADR-8084): cliff relief's dense nodes double the drawn grid (`lod_seams.rs:14-16`), and that grid nests in this
   lattice. Its displacement is presentation that the sim never reads, and it never moves a ring vertex. Its triangles use the engine's diagonal split
   (`holes.rs:1206-1214`). CDLOD's coarser grids are nested in it: every edge of a level-`l` triangle, before the
   morph, is a union of lattice edges, and every level-`l` triangle is a union of lattice triangles. A coarse cell's
   diagonal runs through fine cells of the same diagonal orientation.
2. **Lattice cut (cook).** For each filled hole, the cut is the set of lattice triangles whose centroid is inside
   the named hole's shape (`HoleShape::contains`). The same cut, as a per-tile bitmap, goes into the cooked file's
   canonical section, so the sim's ground is open on exactly the cells the terrain discards (spec 121 § 3 and § 5.3). The **ring** is the cut's boundary: closed loops of lattice edges
   whose vertices are lattice vertices. The cook fails with `mouth_not_simple` when a loop touches itself at a vertex.
   It reports the cut's largest distance from the exact shape, at most one lattice diagonal.
3. **Zipper (cook).** The author's mesh has an open boundary loop near each mouth, picked as the loop whose XZ
   projection is nearest the ring (`seam_loop_missing` when there is none within `max_stitch_gap_m`, default 1 m). The
   cook joins the authored loop to the ring with a zipper strip: two polylines triangulated by advancing along both by
   arc length (Turk and Levoy 1994). It refuses folded strips (`seam_strip_folds`: a strip triangle's normal faces
   away from its neighbors'). It also refuses a strip or loop that rises above the LOD-0 ground outside the cut
   (`seam_above_ground`, 1 cm slack), because the terrain would z-fight with it or be pierced by it there. Authored
   vertices never move. The strip, a **skirt** and the **throat** make the
   mouth's band:
   - The skirt is a vertical strip hanging `skirt_m` (default 0.25 m) under the ring, facing out of the hole (the
     skirt idea of chunked LOD, Ulrich 2002). It is only ever seen through a sub-pixel sliver where the terrain's
     discard and the band's edge rasterize differently (they are not one shared triangle edge).
   - The throat is every layer triangle within the hole's footprint less than `throat_m` (default 2 m) under the
     ground. It is moved from the body into the band, so a closed hole hides it.
4. **Terrain cut in lattice mode (run time).** A filled hole's GPU window switches to lattice mode in one of the two
   spare words (internal layout, no file format). The texels are lattice cells, one byte each: bit 0 = lower triangle
   cut, bit 1 = upper. The discard tests the pixel's **reference XZ**: the pre-morph grid position, interpolated as a
   varying. The drawn triangle is an affine image of its reference triangle, so the cut boundary in the drawn image is
   exactly the image of the ring's lattice edges, at every level and morph factor. Windows of other holes keep the
   world-XZ coverage test. The index culling (`build_node_indices`) drops a node triangle over a filled hole only when
   every lattice triangle under it is cut. Cliff relief fades to zero within two lattice cells of a ring, so the CPU
   mirror stays exact there.
   *As built (ADR-8470):* word 6 = 1 marks a lattice window, words 0-1 its corner, 2-3 its size in cells, 4-5 one
   over the pitch; bit 2 of a cell byte marks the two-cell band where relief is zero (a mask bit, not a uniform).
5. **Ring attach (run time, every pass).** For each pass the terrain draws (color, depth, velocity and each shadow
   cascade; each has its own `LodSelection`), the terrain draw places each ring vertex `v` as follows. It finds the
   selected node over `v` and the level-`l` reference triangle containing `v` (any one, if `v` is on an edge). It
   computes `v`'s barycentric weights `b` there and sets `drawn(v) = Σ b_i · P(c_i)`. `P` is the morphed, filtered
   vertex position the shader gives that triangle's corners (`eval_node_vertex_filtered`, the mirror `lod_seams` already
   trusts). Every ring segment is one lattice edge, so it lies inside one reference triangle at any level. Its drawn
   image is therefore a straight segment on the drawn terrain triangle. The ring and the cut coincide exactly, and the
   ring lies on the drawn surface: **gap 0, lip 0**, to f32 rounding, at every LOD and every LOD pair. CDLOD's own
   border guarantee keeps the drawn surface continuous across node borders (`cdlod.rs:12-29`), so a ring crossing a
   border is continuous too. A fully morphed vertex collapses its fine triangles onto the coarse edge. The ring points
   in such a triangle then map onto that edge, which the neighboring triangles draw, and the part of the cut inside
   it collapses the same way, so the identity still holds. The terrain draw keeps last frame's ring positions for
   the velocity pass. Cost: ring vertices × passes within draw range. A 12 × 20 m mouth at `s` = 0.5 m has about 150
   ring vertices, and 64 mouths (the window cap) stay under 10k a pass. It gets a `tracing` span.
6. **Who draws the band.** The terrain draw does, in every one of its passes, because only it knows each pass's
   selection. It uses the layer's material, with vertices from a small per-pass buffer. Body chunks are ordinary mesh
   entities (§ 2a.5). The authored loop is locked in every body LOD (§ 2a.5), so the band meets every body LOD
   exactly.
   *As built (ADR-8569):* the band is drawn by each pass's own terrain pipeline (no band pipeline), from five vertex
   sets (camera, cascades 0-3) after the hole windows in the hole words; it takes the ground material until the
   presentation carries a layer material. *ADR-8824:* "every pass" includes texture views (probe captures,
   mirrors, screens, stereo eyes), which place the band on their own selection and cascades in a second camera's five
   sets, and the precipitation occlusion tile (the camera's set). `TerrainDrawStats::band_draws` and
   `terrain.seam_report`'s `band_draws` count the band draws of each pass.
7. **Open and close.** A closed filled hole draws no band and no lattice window, so the ground is whole, and the
   query counts the ground there (spec 121 § 5.3 item 3). The body chunks stay. A collapse is not a closed hole: it
   is a blocker plug in the section (spec 121 § 5.7), drawn while active. Opening it at run time (`terrain.set_hole { open: true }`, ADR-1595) shows band and cut together on
   the next prepare. A filled hole's **shape is fixed by the cook**: `TerrainHoles::edit` refuses a shape change with
   `HolesError::FilledByLayer` (new variant; Upgrade note). Removing the hole removes the mouth.
8. **Terrain edits.** Sim craters and berms are `GeomState::apply_ground_edit` (spec 121 § 6): integer millimetres,
   stopped at a layer roof plus a minimum cover. The returned dirty rectangle goes to `TerrainHeightEdits` (spec 25
   § 7.4) as absolute millimetre rectangles of the same lattice. Edits under a mouth need nothing for the draw, because
   the ring re-attaches every pass. The band's LOD-0 positions (pick, export) and the collar are rebuilt for the dirty
   rectangle, and `generation` is bumped.

Rejected seam methods (each in the ADR): today's coverage cut plus authored lip strips; forcing LOD 0 around mouths;
a ring at fixed world XZ that samples the height; skirts alone; an exact-polygon cut with crossing vertices.

### 2a.5 Streaming and LOD ownership

- **Chunks are CDLOD nodes.** At cook, the layer is cut on the quadtree node grid at `chunk_level`: the coarsest level
  whose node is at most `chunk_m` (default 64 m, the scatter tile). Cut edges become chunk borders. A chunk is
  "in" the terrain tiles its node rectangle overlaps. Texture tiles, collision tiles (`tile_m`) and chunks need not
  align. The owner relation is computed from rectangles, as spec 78 § 13.1 maps cells to tiles.
- **LOD by the terrain's metric.** Chunk LOD `i` is drawn while the chunk's `QuadNode::rect_distance`
  (`cdlod.rs:187`) is in `[range(i-1), range(i))`, with `range(l) = range_factor · node_world_size(l)`: the distance
  at which the terrain itself draws level `i`. LOD `i` is simplified with meshopt (already a pipeline dependency, as for
  HLOD proxies, spec 78 § 11.1) to an error of at most the terrain's level-`i` cell (`node_world_size(i) / grid_size`).
  Chunk borders and authored mouth loops are locked (meshopt `LockBorder`), so neighbors at different LODs share their
  border and the band meets every LOD. At most 4 LODs. Switches are discrete. The dithered cross-fade of spec 78 § 11.2
  is an opt-in follow-up.
- **Residency.** `TerrainLayerStream` is a resource like `TerrainTileStream`. It wants a chunk while its rectangle is
  within `visible_radius` of any `StreamViews` source. It loads LOD meshes through the asset server, never on the
  render thread. It spawns one entity per resident chunk (`MeshRenderer`, the layer's material, `MapLayer`,
  `TerrainLayerChunk { layer, node }`) and despawns it when unwanted. The **coarsest LOD of every chunk stays
  resident** for the life of the layers, as HLOD root proxies do (spec 78 § 11.3), so a layer never shows a hole
  while it streams. Budget: `budget_bytes` beyond the coarsest set; nearest first, ties in (layer, z, x) order.
- **What is always resident:** the query index (§ 2a.7), every mouth (band, cut, rings, collar), the layer table and
  the coarsest LODs.

### 2a.6 Lighting, path tracer, bake and pick

- **Shadows.** Chunk entities cast through the ordinary mesh path (`ShadowCast::All`, or none when `cast_shadows` is
  false, through `NotShadowCaster`). The band casts in the terrain's shadow passes with that cascade's attach (§ 2a.4
  step 5), so the mouth casts and receives with no seam leak. The ray-traced sun's terrain proxy already casts holes as
  ground (`trace_proxy.rs:15`). Its layer chunks join as meshes.
- **The ground collar.** Consumers that cut the heightfield by whole cells (a NaN sample removes every cell it
  touches) would open a gap between the lattice cut and those cells. Those consumers are the physics heightfield, the
  path tracer's `ReferenceHeightfield` and the bake's heightfield. For them each mouth carries a collar: the LOD-0
  lattice triangles of those removed cells that lie outside the lattice cut. They add it as a mesh. The raster draw
  never draws the collar.
- **Path tracer.** Resident chunk entities enter `FrameExport` like any drawn mesh. For a reference render, the
  export also asks `TerrainMeshLayers` for LOD 0 of every chunk in the export range, plus every open mouth's band
  (LOD-0 positions) and collar.
- **Baked GI (probe volumes, lightmap scene, AO).** `collect_statics` (`bake_export.rs:165`) adds every layer chunk
  at LOD 0, each open mouth's band and its collar as static meshes in the existing mesh list. The bake description
  does not change shape. The GI the tunnel interior receives is the probe volume of the do27-tunnel-interior-gi lane.
  Lightmaps on layer chunks need stable entry keys (`meshlayer:<layer>/<x>_<z>`); they are a follow-up (§ 2a.13).
  DDGI voxelizes the chunk entities as meshes.
- **Pick.** Chunk entities are `MeshRenderer`s with the layer's `MapLayer`, so `world_pick_layers` filters them with
  no change. The terrain branch of `regia_pick` (`world.rs:718-747`) uses the lattice test for filled holes. It also
  casts the open mouths' bands (LOD-0 positions) and reports them as `PickSurface::Terrain` (terrain-owned geometry,
  layer 0). A ray into an open mouth hits the band or the layer, never the cut ground.
- *As built (ADR-8537):* pick and `terrain.raycast` as above (`HolesMask::contains_drawn`,
  `TerrainMeshLayers::raycast_bands`); the reference trace adds each open mouth's band and collar
  (`add_reference_pieces`), not yet LOD 0 of every chunk; the bake join is deferred (its sources are files: the band and
  collar have none, the chunk LODs sit in the cooked cache); shadows need no code.
- *As built (ADR-8872):* the reference trace also re-points every chunk the frame drew at its cooked LOD 0
  (`reference_chunks_lod0`: the stream's reader; the range is the drawn set, so trace and raster hold the same chunks).

### 2a.7 Surface query

The query is `regia_world_geom`'s (spec 121 § 5.3). The terrain exposes it with the spec 26 shape and adds nothing
to its arithmetic:

```rust
impl TerrainMeshLayers {
    /// The floor under and the ceiling over `(x, z)` near `near_y`: `self.geom.surface_at(&self.state, ..)`.
    /// CPU-only, no allocation, the same bits on every platform (spec 121 § 2, ADR-8012 A-4).
    pub fn surface_at(&self, x: f32, z: f32, near_y: f32) -> SurfaceAt;
    /// `surface_at` with `tolerance_m` (default 0).
    pub fn surface_at_with(&self, x: f32, z: f32, near_y: f32, tolerance_m: f32) -> SurfaceAt;
}
```

- **One surface set.** The answer covers the ground lattice (cut by open mouths on the cooked lattice cut), every
  canonical layer triangle and every active blocker plug. The query mesh is the canonical triangles, not the render
  mesh: render may add bounded detail, and spec 121 § 10 `render_off_collision` fails the cook past 0.05 m.
- **Walls are answered, not skipped.** A near-vertical triangle is never a floor or a roof; walls are what the wall
  push and segments test (spec 121 § 5.4, § 5.5). The interior camera (W-26) sweeps a sphere with them.
- **The ground is the sim's ground.** The sim adopts the lattice triangulation (ADR-8019 A-7: index `i / (n − 1)`,
  the terrain's fixed diagonal), so the query's ground, the ring's surface and the sim's floor are one surface.
  Hosts that keep a bilinear sim of their own still have `TerrainMap::sample_bilinear` (§ 1 bounds the difference).
- **A closed hole** counts as ground. Off the map there is no ground crossing, but layers still answer. The ground
  faces up, so it is never an `over`.
- **Agent method `terrain.surface_at`.** Spec 121 § 12 (it adds `support` and `section` to each result).

### 2a.8 Colliders (optional, engine-physics hosts)

`collider: true` builds `ColliderShape::TriMesh` pieces (`trimesh_from_triangles`,
`crates/regia_physics/src/components.rs:427`) from the **canonical triangles** (spec 121 § 9.2), one per chunk, plus
each open mouth's band and collar and each active blocker plug. They serve hosts that do not lockstep (rapier is not
bit-identical across arches without its enhanced-determinism mode, and is not lockstep collision). With
`TerrainTileCollision` on, a chunk's piece exists while any collision tile under its rectangle is held. It is built in
the same fixed step as the tile, counts toward `max_tiles_per_step`, sits on the terrain's static body (so rebases move
it and hits report the terrain entity) and is covered by the `terrain_collision` gate. Without tiling, one trimesh per
layer goes on the terrain's body. A hole that closes or a blocker that clears removes its pieces. DO27 leaves
`collider` false: its sim calls `regia_world_geom`.

*As built (ADR-8537):* `TerrainMeshLayers::collider_pieces` gives the canonical triangles per layer and chunk (never the
render LODs or the band); `regia_physics::TerrainLayerColliders` makes them follow the held tiles on the terrain's body
(the host fills it). The cooked `collider` flag and the untiled body are deferred (see the ADR).

*As built (ADR-8872):* the cooked chunk section carries `collider` (version 3; `terrain-mesh-layers-cooked` v2, format
batch 2); `TerrainMeshLayers::cooked_collider_pieces` gives the flagged layers' pieces. Without `TerrainTileCollision`,
`TerrainLayerColliders` builds one `TriMesh` per layer (`TerrainColliderPiece::layer`) on the terrain's body. The host
still inserts the component (the heightfield collider is the host's).

### 2a.9 Sources of layer meshes

Every source cooks to canonical triangles first; render, cut, sections, zones, portals and nav input derive from them
(spec 121 § 9.1).

- **Passages** (`Passages { path }`, new): a passage description (centrelines, widths, ceilings, openings,
  junctions; `<map>.passages.ron`, imported from DO27's `do27-tunnels/1` by `regia terrain passages import`). Its
  pinned mesher (Stage A's `compat::layout`, spec 121 § 4) gives the canonical triangles; the render mesh shares its
  section planes and adds bounded detail (W-07).
- **glTF** (`Gltf { path, node?, collision_node? }`): the render mesh, plus optional collision triangles from a node
  of the same file (the canonical triangles; without the node, LOD 0 is canonical). The § 2 provenance check compares
  the counts.
- **Collision triangles** (`Collision { path, detail: TunnelDetail }`): the `MeshTunnel::from_collision` path. The
  render mesh is the triangles plus detail displacement, with boundary-loop vertices left undisplaced so the zipper
  meets them; displacement past the 0.05 m bound fails the cook (DO27's v0.5.0 default of 0.05 plus noise 0.12 does).
  The run-time builder (`mesh_layer::build_layer`, the pure CPU function the cook calls) lets a host hand triangles
  live; a run-time layer gets LOD 0 only.
- **Volume** (`Volume { sdf, iso, cell_m }`): `mesh_chunk` meshes each chunk at cook. Triangles above the LOD-0 ground
  outside filled holes are clipped away, and the clip loop at each filled hole is zippered like an authored loop.
  Built last (optional lane), and the source the D2 voxel core will feed through the same contract.
  *As built (ADR-10071):* there is no volume file yet (`<map>.voxfield` is proposed, spec 122), so the host passes
  the field: `cook_mesh_layers_with_volumes` takes a `VolumeSource` (an `SdfField`, its box, a fingerprint) per `sdf`
  name; without one the layer is still `cook_not_built`. The clip runs everywhere (a cave's dome over its mouth goes
  too), a crossing edge is split linearly in the height over the lattice ground with the new vertex on the ground,
  and a filled hole's footprint is its clip loop's XZ polygon (`fills` take the loops in `(z, x)` order;
  `meshlayers_volume_mouths` when they do not match or a loop is not star-shaped). `cook_with_layers` makes the
  clipped LOD-0 mesh the canonical triangles (kinds by normal, 45 degrees); chunks and LODs are § 2a.5's.

### 2a.10 Formats

The formats are spec 121 § 9 (registry names `terrain-mesh-layers` and `terrain-mesh-layers-cooked`, both proposed in
`docs/formats/proposed-formats.ron` until W-06 builds them):

| File | Kind | Version | Migrator / `regia upgrade-project` |
|---|---|---|---|
| `<map>.meshlayers.ron` | source (`MeshLayersDocument`: `version`, `chunk_m`, `geom`, `sections`, `layers: [(id, source, fills, map_layer, cast_shadows, collider, material, max_lods, throat_m, skirt_m, max_stitch_gap_m, blockers, look)]`) | 1 (new) | none at v1; registered with `VersionSource::Probe` so a v2 adds its step there; a newer file is refused (`NewerVersion`) |
| cooked `<map>.meshlayers` (`.fckd`, postcard: header, canonical section `WorldGeomCooked`, presentation section `TerrainMeshLayersCooked`) | cooked | 1 (new) | a cache: a version mismatch recooks; `upgrade-project` lists it as derived data |
| `<map>.passages.ron` | source of the `Passages` layer source (a `TunnelLayout` with its version) | 1 (the `terrain-mesh-layers` family version; the layout keeps its own) | part of `terrain-mesh-layers` (ADR-8112): one `upgrade-project` row, its own fuzz target |
| chunk LOD meshes `meshlayers/<layer>/<x>_<z>/lod<i>` | cooked sub-assets | `CookedMesh` v3, unchanged | none |
| `<map>.holes.ron` | source | stays v2 | none: fills are named by the layers file; the holes file does not change |
| scene / `.level.ron` | source | unchanged | none: the layers file is found next to the map like the holes file |

Both new formats get `format-hashes.ron` rows (`cargo xtask check-formats --update` by the lead), a fuzz target for
each parser (`regia_agent --test fuzz_targets`), a corpus fixture and a `docs/formats/terrain.md` section (§ 12 source,
§ 13 cooked) when W-06 builds them. The GPU window's lattice mode is an internal buffer layout and is not a format.

As built (W-07, ADR-8175): until the chunk and LOD lanes (W-14, W-16) land, the presentation section is
`PresentationCooked` from `regia_world_geom::render` (GPU-free): per layer the render mesh (every canonical face of a
`Passages` layer subdivided `look.subdivision` times, displacement 0, `RenderSource` provenance per triangle, ranges
per passage) and the mouth cut quads. Still v1 while unshipped; the fingerprint records the shape.

### 2a.11 Agent surface (rule 8)

- `terrain.surface_at`, `terrain.mesh_layers`, `terrain.bake_report`, `terrain.blockers`, `terrain.segment`,
  `terrain.ground_edit`: spec 121 § 12.
- `terrain.seam_report { hole?, pass? }` → per open mouth and pass (`main`, `cascade0..3`) `{ max_gap_m, max_lip_m,
  worst_at, ring_vertices }`. It measures the ring against the drawn terrain on the CPU mirror, as `terrain.parity`
  uses `lod_seams`. Errors: `unknown_hole`, `hole_not_filled`.
- Book: `book/agents/terrain-mesh-layers.md` with `book/COVERAGE.md` rows; user guide
  `book/user-guide/terrain-mesh-layers.md`.

### 2a.12 Acceptance (each fails on the base)

- Query and sim: spec 121 § 13 (Stage A pins, the Stage B query table on x86-64 and aarch64, the known issues that
  become tests: mouth hop ≤ 0.05 m per tick, a crater over a covered tube moves its floor by zero, the rubble floor,
  one-generation blockers).
- Query (presentation): on a flat terrain with an open filled hole over a tunnel, a point under the mouth answers
  `under` = the tunnel floor and `over` = the tunnel roof or none (inside the cut). The same point with the hole closed
  answers the ground. A 3-level synthetic stack answers the right floor and ceiling at 5 heights.
- Seam (CPU mirror): over a camera sweep through every LOD pair, `terrain.seam_report` gives `max_gap_m` and
  `max_lip_m` ≤ 1e-4 m at every pass. The ring equals the lattice cut's boundary as integer edges, and the cooked
  canonical cut bitmap equals the GPU window's lattice cells.
- Seam (GPU, lavapipe): the tunnel scene's crack detector, with the lip strips removed and a layer in their slot,
  finds no void pixel along the cut on a drive that crosses every morph band.
- Streaming: chunks load and unload with the sources, the coarsest LOD never leaves, and LOD follows `rect_distance`
  at the terrain's ranges.
- Joins: a pick ray into an open mouth hits the band or the layer, never the ground. The reference export and the bake
  export contain the chunks, band and collar. A chunk entity casts in the shadow pass. An active blocker plug is drawn,
  picked and exported; a cleared one is none of these.
- Bake: every spec 121 § 10 check fails on its fixture.
- Sample: a multi-level map (3 stacked levels, 2 mouths, an arch, a collapsible section) runs headless with the agent
  checks above.

### 2a.13 Not in this design

Layer-to-layer seams; lightmaps on layer chunks (stable entry keys `meshlayer:<layer>/<x>_<z>`, a follow-up once a
map needs them); the dithered LOD cross-fade; deformable layers (the D2 voxel core, behind spec 121's blocker and edit
calls); exact curved cuts (the cut is on the seam lattice, within one lattice diagonal of the named hole's shape).

## 3. GPU VFX (`regia_vfx`) and decals
- Emitter assets (`.vfx.ron`) composed of modules in stages — **Spawn** (rate, bursts, per-distance),
  **Init** (shapes: point/sphere/box/cone/mesh surface/skinned mesh; velocity, lifetime, size, color,
  rotation), **Update** (gravity, drag, curl noise, vortex, attractors, size/color/alpha over life curves,
  depth-buffer collision, kill volumes), **Render** (billboard + flipbook + soft particles, stretched
  billboard, mesh particles, ribbons/trails, light-emitting particles with a budget).
  *Status (v7 asset, ADR-2800): every module above exists. v6 added scene wind (`wind_response`, `wind_drag`) and event spawns: child emitters fired by a particle's spawn, death or depth collision, through a bounded event buffer and a separate child ring (ADR-2642). v7 adds mesh-surface and skinned-mesh init shapes (area alias table at registration; skinned spawns sampled on the CPU from the current palette, capped per frame, reaching the GPU through the emit stage), the along-normal velocity, box and sphere kill volumes (inside or outside, one rule on the GPU and the CPU) and ribbons as a render mode (CPU-simulated strips through the trail pipelines). As built (v8 asset, ADR-7210): a `Mesh` particle with `render.mesh` draws the registered mesh asset (the mesh-shape registry), one indexed indirect draw per registered mesh over the mesh list; without it, the built-in cube. A missing or refused mesh draws nothing and is reported (`missing_mesh` / `bad_mesh`). *As built (ADR-7261):* a `lit` emitter's mesh asset is shaded by the frame's sun and ambient (face normals, front faces only; the built-in cube stays unlit), the pipeline warm compiles the mesh-asset pipeline with the other effect pipelines, and a compute pass buckets the mesh list by mesh so each mesh's draw covers only its own instances.*
- Simulation in compute; indirect draws; bitonic sort for alpha-blended emitters (desktop), CPU-ordered
  emitters on mobile; LOD and a global particle budget per profile ("smoke budget" in DO27).
  *As built (ADR-7210): a profile without the alpha sort ranks the allocated emitter slots back to front per view on
  the CPU each frame and an 8-bit GPU radix pass orders the alpha list by that rank; particles keep their
  gather order inside an emitter.*
- `VfxSystem` asset groups emitters with timing; spawned by the sim-bridge event router, scripts, or
  animation notifies; pooled. *(Status, ADR-5629: effects spawn through `SpawnEffect` and `vfx.spawn`; the sim-bridge
  `Vfx` and `Decal` reactions parse but are not applied yet (`book/systems/sim-bridge.md`), so hosts spawn the
  effect themselves.)*
- **Decals:** clustered decals (projected boxes) for craters, scorch, impacts — pooled, aged (fade over
  time), budget per profile; affect base color, normal, roughness.
- Editor: module stack panel, curve/gradient editors, live preview, per-emitter GPU cost.
  *Status (ADR-3171): built as the VFX Editor dock tab and `editor.vfx_open`/`_state`/`_edit`/`_preview`. A module is a field group off its default; disable (A/B) stashes its values in the editor (not saved, no format change). Per-emitter GPU time is the emitter's live-particle share of the timed VFX passes (timestamps are per pass).*
- **Runtime look (vfx-look, ADR-050, ADR-052; ADR-5629 corrects the old ADR-049 citation):** on a single-sample `Rgba16Float` frame every VFX draw goes
  into the pre-exposure HDR target divided by the exposure the post stack applies this frame
  (`LayerCamera::exposure`, auto exposure included): decals in their own pass, a scene copy
  refreshed after them, then particles and trails (so a scorch sits under the fire and heat haze
  bends the frame after decals). MSAA frames stay on the display pass (exposure 1) with a resolved
  depth. Particle lights follow their burst's luminance and reach the renderer's local lights; the
  strongest also lights lit puffs. `.vfx.ron` v4: `FireballAnim` body sheet, `heat_haze`. The LOD
  follows the camera the frame hook rendered with when no ECS camera is extracted, and grows alpha
  puffs (`rate^-1/3`) instead of only thinning them.
- **As built (ADR-7431, v0.6 follow-ups):** a significance newcomer that outranks a dwell-held instance (its
  hysteresis bonus counted) takes its slot on its first ranking; a frame without the forward-decal binding is handed
  no feed and the runtime publishes an empty one, so a frame that gets the binding back never shades a stale copy; the
  ambient-life fallback tier follows `RenderProfilePreset::platform_default`; the CPU probe can model the main ring
  (`ProbeSettings::particle_capacity`) and keeps the GPU's event set on a whole-pool update window.

## 4. Desktop high-end renderer track
Target: DO27's floor tier (1080p60 on RTX 3060 Ti) and mid tier (1440p90 on RTX 4070) with a look that
competes with current AAA engines. Not a clone of any engine's proprietary techniques; each item is
built from public research and gets an ADR.

| Feature | Approach (to confirm in ADR) | Profiles | Milestone |
|---|---|---|---|
| GPU-driven rendering | Persistent scene buffers, compute frustum + two-pass Hi-Z occlusion culling, multi-draw-indirect | Desktop High+ | M15a |
| High-detail geometry | Cluster (meshlet) hierarchies built at cook with meshopt (clusters + simplification DAG), per-cluster LOD selection and culling, visibility-buffer shading; regular LOD meshes elsewhere | Desktop High+ | M15d = `specs/27` § 4.2 stage 3 |
| Global illumination | Dynamic irradiance probe grid (DDGI-style, Majercik et al. 2019) traced against signed-distance fields (per-mesh SDFs + terrain heightfield) — hardware ray queries when wgpu supports them stably; screen-space GI/AO on top; baked fallback for Mobile/Low | Desktop High+ (probes), all (baked) | M15b (dynamic probes, screen-space); M15d (baked fallback) = after `specs/27` § 4.2 stage 2 |
| Reflections | SSR + probe GI specular + reflection probes; RT reflections when hardware ray queries are stable | Desktop High+ | M15b (ray-traced: M15d = with `specs/27` § 4.2 stage 2b, hardware ray queries) |
| Shadows | Sun: cached, high-resolution paged shadow maps (virtual-texture-style pages, invalidated by moving casters) with CSM fallback; local lights: cached atlas | Desktop High+ | M15b |
| Many lights | Clustered (exists) + stochastic light sampling for very high counts (ReSTIR-style, public paper) — Ultra only | Desktop Ultra | M15d = `specs/27` § 4.2 stage 2b |
| Atmosphere & sky | Physically based atmosphere (Hillaire 2020, LUTs), time of day from map data; planet presets Earth / Mars / Airless and a lit sky body, chosen per map in `LevelSettings.atmosphere` (ADR-040) | High/Ultra for Earth; any HDR frame for Mars/Airless (ADR-039, ADR-040) | M15c |
| Volumetrics | Raymarched volumetric clouds (public talks), froxel volumetric fog with light scattering and shadows | Desktop High+ | M15c |
| Materials | Layered standard material: base + up to 4 masked layers (paint, dust, wear, wetness), clearcoat, anisotropy | All (layers capped on mobile) | M5 |
| Upscaling / AA | In-house temporal upscaler (TAAU), port of AMD FSR 2/3 (MIT, attribution) as an option; DLSS/XeSS only if vendor SDK interop with wgpu becomes practical (Tier 3) | Desktop | M15a (temporal upscaler); M15d = `specs/27` § 4.2 stage 4 (TSR-grade upscaler, FSR port as baseline) |
| Output | HDR output (HDR10/scRGB where the surface supports it), ultrawide/32:9 with horizontal-plus FOV, VRR, uncapped frame rate, frame cap | Desktop | M15a |
| Cosmetic destruction | Pre-fractured meshes (cook-time Voronoi fracture) + rapier debris, triggered by sim events, despawned on budget | Desktop (reduced on mobile) | M14 |

**Milestones (ADR-009).** M15a–c are what DO27's gating tiers need and run before Gate C; M15d holds
the showcase extras and runs after DO27's launch (`05-milestones.md`, Phase G). M15a comes first
because the GPU-driven scene buffers and the temporal upscaler are what the later parts build on, and
what the floor tier's frame rate depends on most.

## 5. Rescope rule
If an item can't meet the floor-tier budget within its milestone, the builder writes an ADR with the
measured numbers and proposes the cheaper technique (e.g. probes traced once at level load instead of
every frame, or cascaded instead of paged sun shadows) rather than missing the milestone. Gate C can still pass with simpler techniques if the owner's
side-by-side review accepts the look.

## 6. Acceptance
- **Terrain (M13):** sim/render height agreement test; 8 km² DO27 map at floor-tier budget (terrain
  ≤ 2.5 ms GPU at 1080p); scatter determinism across two machines; tunnel entrance holes; water map.
- **VFX (M14):** `samples/vfx_lab` with every module; 100k live particles ≤ 2 ms GPU desktop High; 10k on
  Mobile High ≤ 2 ms; decal pool aging; DO27 event set (muzzle, tracer, impact, explosion, exhaust, dust,
  sparks, weather) authored as `.vfx.ron` by generators. *(Status, ADR-5629: the set is in code,
  `regia_vfx::showcase::showcase_effects`, and serializes through `VfxSystemDef::to_ron`; the `.vfx.ron` files from
  DO27's generators are DO27-side work.)*
- **High-end renderer (M15a–d)** — common to every part: golden scenes per feature; GPU timings per pass
  recorded; each feature can be toggled live for A/B in the editor and via the agent protocol; and after
  each part the DO27 benchmark replay (rendered by the `shell-ferrite` slice as it stands) still holds
  floor tier ≥ 60 fps locked (p99) and mid tier ≥ 90 fps on the High preset. A feature that breaks this
  ships disabled in that preset or is rescoped under § 5.
  - **M15a:** the GPU-driven path matches the CPU path on the M5 golden scenes (same SSIM threshold); a
    stress scene of 100,000 instances with 500 materials runs at ≥ 60 fps on the floor tier; the Deck
    profile holds a stable 30–40 fps at 800p with temporal upscaling; ultrawide, VRR, uncapped and capped
    frame rates verified; HDR output verified, or deferred by ADR if wgpu cannot do it (`05` risks).
  - **M15b:** sun shadows stay stable under camera motion and time-of-day change (golden sequence); a
    tunnel interior lit only by local lights and probe GI shows no sky leak (golden); GI and SSR on/off
    goldens.
  - **M15c:** atmosphere goldens at four times of day driven from DO27 map data; a fog volume with
    shadowed light shafts; a cloud layer golden.
  - **M15d (after the alpha, as the `specs/27` § 4.2 programme; plus `specs/27` § 2 numbers against the reference path tracer and the § 3 plate verdict):** each feature's cost measured on the floor, mid and showcase tiers;
    showcase tier 4K60 measured (best effort); cluster geometry holds 60 fps on the floor tier in a scene
    with at least ten times the source triangles the regular LOD path can hold at that frame rate.

## C-20 source import and overview bake (ADR-8063)

`regia_terrain::source` reads external schema-2 JSON and grayscale 8/16-bit PNG
counts, retaining rectangular dimensions, square-cell pitch and sample origin.
Meters are `(count - zero_counts) * meters_per_count`; the scale is configurable.
JSON stores it in `units`; PNG takes caller settings and an explicit datum keyword
(default `terrain_zero_counts`). Missing datum rejects by default, with explicit
fixed-count or center-sample fallback available. Invalid or duplicate metadata
never falls back. Existing cooked heightfields and PNG/RAW edit semantics stay
unchanged. This source mapping is separate from the existing square TerrainMap.

`overview::bake_overview` area-filters into a requested output size, flips north-up,
optionally smooths with a separable five-tap Gaussian approximation, and applies
per-axis finite-difference Lambert shading, percentile elevation and optional
contours/border. Settings and source/output budgets are explicit. It never edits
the source and is a CPU presentation bake, not a lockstep query implementation.
Agent methods `terrain.inspect_source` and `terrain.bake_overview` expose the
same behavior with project-relative paths; details and error codes are in
`book/agents/terrain-source.md`. Synthetic acceptance covers datum conversion,
rectangular mapping, invalid metadata, resource limits, shading, north-up output,
repeatability and dispatcher permissions; no golden image is generated or blessed.

### CT-18 source resource budgets (ADR-8362)

JSON counts are charged before decoding each sample and before bounded storage
growth, independent of field order. Checked dimension multiplication, a shared
ignored-value/key budget and a depth ceiling bound hostile inputs. The engine
ceilings are 128 MiB encoded, 16,777,216 samples, JSON depth 32 and 65,536 ignored
values/keys. Caller budgets and the host's `TerrainSourcePolicy` can only lower
those limits; existing inspect/bake methods report `effective_limits` and stable
limit codes. PNG retains its predecode dimension checks. Native observer counters
support cooperative cancellation with no partial source publication. Source
schema, frozen cooked data, direct f32 token rounding and meter operations remain
unchanged. See `book/agents/terrain-source.md` and `book/user-guide/terrain-sources.md`.
## 7. Cooked photoreal terrain (DO27 CP_TERRAIN, ADR-8024)

A host whose package step already builds a terrain's final ground and photoreal material (DO27 builds both at
package time: mirrored heights with fractional parts and shell sinks, holes, a macro bake with tint, cavity AO,
normal and backdrop transforms) ships them cooked, and the player installs them without rebuilding a material or
writing files. The cooked map carries, under stable ids derived from the map id:

| Sub-asset | Payload | Runtime |
|---|---|---|
| `terrain/index` | `TerrainIndex` v2: v1's fields plus `heights`, `holes`, `material` (each optional) | `PakTileSource::load_terrain_index` (v1 migrates: the three are `None`) |
| `terrain/heightfield` | the tiled R16 heightfield (§ 8 of `docs/formats/terrain.md`), the final heights quantized | stitched as before (`cooked_samples`) |
| `terrain/heights` | `terrain-final-heights` v1: the exact f32 heights, as residuals over the R16 base or raw | `TerrainSharedDraw::from_cooked` builds the map from them |
| `terrain/holes` | the `.holes.ron` text (frozen `terrain-holes`, its own migrator) | `from_cooked` cuts them |
| `terrain/material/<kind>/<i>`, `terrain/material/macro_coarse_<albedo|normal>` | one KTX2 `CookedTexture` per blob of `CookedMaterialKtx2` | `PakTileSource::load_cooked_material` → `CookedMaterialKtx2<PakPayload>` → `set_material_cooked` |

- **Heights are exact.** The final heights (metres, row 0 = −Z, already mirrored and sunk by the host) are the
  ground: the R16 heightfield is their quantization with the index's `height_scale`/`height_offset` (the host's, or
  the heights' range), and `terrain/heights` restores every f32 bit. Residual encoding stores `r = h - base(s)`
  where `base(s) = s / 65535 · scale + offset` (the `TerrainMap::new` arithmetic); the cook keeps it only when
  `base(s) + r` gives back the bits of `h` for every sample, else it writes the heights raw. Either way
  `from_cooked`'s map equals the host's heights bit for bit, so the render surface and the simulation agree
  (§ 1 "never resamples"). With final heights the index's scale/offset win over `from_cooked`'s arguments.
- **No copy of the material.** `CookedMaterialKtx2` is generic over its blob type (`B: AsRef<[u8]>`, default
  `Vec<u8>`); `set_material_cooked` accepts any of them. Blobs read from an uncompressed pak entry are views of the
  mapped pak (`PakPayload`), so loading a cooked material allocates per blob, never per byte.
- **Package-time adapter.** `regia_asset_pipeline::write_photoreal_terrain` takes the final heights, optional holes
  and an optional `CookedMaterialKtx2` (from `cook_terrain_material` over the host's own bake) with its
  `MaterialParams` and macro grid, writes every sub-asset above into a cooked cache directory and returns the
  manifest entries (paths `<map rel path>#terrain/...`, so chunk rules that place the map place its terrain). It
  owns the map id's `terrain/*` ids: the standard `.terrain.layers.ron` cook must not cook the same map id.
- **Formats.** `terrain-index` v1 → v2 rides the v0.6.0 bump (ADR-7724 rule: one step) and is registered with
  `check-formats` (it shipped unregistered at v1); `terrain-final-heights` is new at v1. v1 indexes keep loading,
  the terrain cook key does not move (a v1 index needs no re-cook), and `regia upgrade-project` rewrites a v1
  index `.fckd` in a dev cache (probe by type tag, header kept). Desktop and Android cook the same heights and holes; material blobs are whatever
  `cook_terrain_material` produced for the platform.
- **Agent.** `assets.terrain_tiles` (the cooked-index report) adds `final_heights`, `holes` and `material`
  (`book/agents/terrain-cooked-photoreal.md`).

### Custom decal images (P-17, ADR-8121/8203, ADR-8829)

The v0.7 format batch 2 (ADR-8829) advances `.decal.ron` from v2 to v3 with an optional
relative cooked RGBA8 image path. v1/v2 migrate; existing atlas selections keep
their meaning. Library loading resolves images atomically and prepares one
matching atlas for projected and forward decals. The current renderer supports
four custom 64×64 pages in addition to its four built-ins. See
[the format contract](../formats/decal-ron.md) for limits and host integration,
and [the agent guide](../../book/agents/custom-decal-textures.md) for verification.
