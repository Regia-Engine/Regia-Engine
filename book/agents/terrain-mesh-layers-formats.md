# World geometry formats: `<map>.meshlayers.ron`, the passages file and the cooked `<map>.meshlayers`

The native world geometry (spec 121 Stage B) starts from one source document per map and cooks it into one file whose
canonical section is what a lockstep sim loads and what peers compare in the match handshake. This page covers the
formats, the `regia terrain passages` command, the error codes and how to check a cook from a script or an agent
(ADR-8112, plan row W-06). The run-time query methods (`terrain.surface_at`, `terrain.mesh_layers`,
`terrain.bake_report`, `terrain.blockers`, ...) arrive with lanes W-11 and W-28 (spec 121 § 12); until then the
surface is the CLI and the crate API below.

The byte-level description is [`docs/formats/terrain.md`](../../docs/formats/terrain.md) §§ 12-13.

## The three files

| File | Registry name | What it holds |
|---|---|---|
| `<map>.meshlayers.ron` | `terrain-mesh-layers` (v1) | The layers (one source each: `Passages`, `Gltf`, `Collision`, `Volume`), the world geometry settings (`geom`), the authored sim ground mode (`Full` or `Nested(max_level)`), the ground tile size and the section rule |
| `<map>.passages.ron` | `terrain-mesh-layers` (same family, v1) | A passage description: centrelines, widths, ceilings, openings, junctions; for an imported file also DO27's baked sampled block, mouth holes and triangle count |
| `<map>.meshlayers` | `terrain-mesh-layers-cooked` (v2, a cache; v1 files from v0.6.0 still load) | `RGML` magic, a header (version, input hash, copies of the settings and ground mode), the canonical section (everything the run time reads: settings, lattice constants, ground mode, tile levels, pins, overrides, the `i32` mm triangles in 1024 m tiles, the ground counts, cut bitmaps, sections, portals, zones, plugs) and the presentation section |

`content_hash` is the BLAKE3 of the canonical section's hashed prefix: its version and its head. The head holds one
BLAKE3 per 64 KiB block of the ground counts, and the load checks every block, so the hash covers the whole section.
Change any setting, lattice constant, tile level, pin, override, cut bit, triangle or count and the hash moves, so two
peers that would answer a query differently can never pass
the handshake. A render-only change (the presentation section, the header's input hash) does not move it.

### Version 2 and v0.6.0 files (ADR-8816)

Version 2 stores the ground counts as little-endian `u16` after the head, with no length prefixes. Version 1 (v0.6.0)
stored every count as a varint and hashed the whole section. Loading the 2.56 km Switchbacks map (16.7 M counts) took
79 ms on the Steam Deck and 85 to 90 ms on the Android test device at version 1; at version 2 it takes 28 ms and
about 49.5 ms.

- A version 1 file still loads. The loader migrates it in memory: the same geometry, the same answers, and the
  `content_hash` of the map cooked at version 2. It is slower than a version 1 load was (one extra encode).
- `regia upgrade-project --write` rewrites a version 1 `<map>.meshlayers` at version 2 and keeps a `.bak`. The cook
  pipeline cooks a version 1 cache again on its own.
- A changed count byte is now refused at load (`geom_corrupt`, naming the block). At version 1 such a file loaded
  with another `content_hash`.
- A host that wants the shortest load can pass `CountBodyCheck::Deferred` to `WorldGeom::from_cooked_with`. The
  load then skips the count hashes (about 20 ms of the 49.5 ms on the Android test device). The host must run
  `regia_world_geom::cooked::check_count_body` on the file's canonical section, on any thread, and must not let the
  map into a match until it returns `Ok`.

```rust
use regia_world_geom::WorldGeom;
use regia_world_geom::cooked::{CountBodyCheck, check_count_body, split_file_ref};

pub fn load(bytes: &[u8]) -> Result<WorldGeom, regia_world_geom::GeomLoadError> {
    // Queries can run as soon as this returns.
    let geom = WorldGeom::from_cooked_with(bytes, CountBodyCheck::Deferred)?;
    // Owed before the map joins a match (run it on a worker and wait for it there).
    check_count_body(split_file_ref(bytes)?.canonical)?;
    Ok(geom)
}
```

The engine's own level load (`regia_terrain::load_mesh_layers`) already does this: it runs the check on a worker
thread beside the load and joins it before it returns.

Measure a device with `cargo bench -p regia_world_geom --features stage_b --bench index_build`. It prints the load of
the section and of the whole file, the deferred load, the body check alone, and the device's cost of allocating the
counts.

## Importing a map's passages from DO27

```sh
regia terrain passages import maps/switchbacks/tunnels.json            # writes maps/switchbacks/switchbacks.passages.ron
regia terrain passages import tunnels.json -o content/sb.passages.ron --json
regia terrain passages export content/sb.passages.ron -o tunnels.json  # byte-identical to the imported file
```

`import --json` prints `{ out, map_id, passages, openings, junctions, triangle_count }`. Then name the file in the
document:

```ron
(
    version: 1,
    ground_tile_cells: 64,
    layers: [(id: "tubes", source: Passages(path: "sb.passages.ron"), fills: ["north_mouth"])],
)
```

## Cooking from code

`regia_world_geom::cook::cook(&CookInputs { document, ground, terrain, read_file })` returns the file and a report per
layer (`triangles`, `welded_vertices`, `trimmed_triangles`, `degenerate_triangles`). The cook:

1. samples each passages layout on the map's terrain and meshes it with the pinned Stage A mesher;
2. **welds** every floor vertex up to 0.15 m over the ground onto it (with every triangle vertex at that position);
3. **trims**: a floor strip under 2 m wide within 0.5 m of another floor gets the no-support flag;
4. converts every vertex with the pinned `m_to_mm` and files the triangles into 1024 m tiles.

5. cooks the **render half** of every `Passages` layer into the presentation section (below).

`WorldGeom::from_cooked(&bytes)` loads a file: it refuses a newer version first, checks every count and index, and
converts the triangles to `f32` metres with `mm_to_m` once. `content_hash()` is the handshake value.

## The render half (`Passages` layers)

The cook also writes each `Passages` layer's render mesh into the cooked file's presentation section (ADR-8175, plan
row W-07). It is cooked from the same conditioned canonical triangles the sim stands on, so it shares their section
planes (flat floor, vertical walls, flat ceiling, the lips, risers and headers), but every canonical face is split
`look.subdivision` times along each edge into its own triangles (`subdivision²` per face; 2 to 16, default 4). The
collision displacement is 0: every render vertex lies on its face up to the millimetre rounding. **No canonical
(collision) triangle is ever in the draw list**: each render triangle names the canonical face it subdivides
(`RenderSource { passage, face, piece }`, where `face` is the layer's `TriRef::triangle`), and is never that face.
Each opening on a ramp also gets a mouth cut quad (half the ramp width plus 2 m each side, 8 m behind the top to 2 m
past it). The presentation section is not part of `content_hash`: a render-only change never splits a match.

```ron
layers: [(id: "tubes", source: Passages(path: "sb.passages.ron"), material: "rock/basalt.mat.ron",
          look: (subdivision: 4, uv_tile_m: 4.0))],
```

A `look.detail` whose absolute `normal_displace` plus `noise_amplitude` is above 0.05 m fails `render_off_collision`.
A smaller non-zero displacement is still `cook_not_built` (displaced detail is lane W-16's), and `subdivision: 1` is
`meshlayers_invalid` (it would draw the canonical triangles).

Read it from code with `regia_world_geom::cooked::split_file` and `regia_world_geom::render::PresentationCooked::decode`
(empty bytes are an empty section).

## Sections, zones, portals, plugs

Since lane W-08 (ADR-8210) the cook cuts every `Passages` layer into **sections** and writes them, their **portals**,
one **zone** each and the **blocker plugs** into the canonical section (so they are part of `content_hash`). Read them
with `WorldGeom::sections()`, `portals()`, `zones()` and `blockers()`, or in the cook report.

- **Where a section ends**: at every cover change (a section is covered, with a lid over every floor, or an open cut),
  at the sample nearest each junction the layout lists, at both ends of a blocker run, and otherwise when the
  centreline would leave the straight chord between the section's ends by more than `sections.section_chord_m`
  (default 4 m). Every canonical triangle carries its section.
- **Portals** join two cells: a section cut inside a passage (the cross-section at the cut), an open passage end (a
  passage that starts or ends inside another; two passages that start at one point share one portal), and a sky portal for every open-cut section (outdoor side `None`;
  `mouth` names the opening whose ramp it is). Each has a nav node (`nav_mm`, the floor point a path crosses it at) and
  the blocker that closes it, if any. With every portal closed, no section reaches another (VisCells, W-21).
- **Zones**: one oriented box per section around its free space, with the covered exposure band `[8.5, 10.5]` EV100
  and a 25 m blend, and a reverb preset: `open` for an open cut, `gallery` for a gallery, `tube` otherwise.
- **Blocker plugs**: a layer's `blockers: [(passage, from_s_m, to_s_m, initially_active)]` makes that run one
  collapsible section. Its plug is the passage cross-section over `sections.plug_length_m` (default 6 m) at the
  section's middle, cooked as `Blocker` triangles appended after every passage triangle of the layer (so passage
  triangle indices never move) and inactive unless `initially_active`. A run on an unknown passage, overlapping
  another, or crossing a cover change or junction is `meshlayers_invalid`. Flipping a plug is W-13's
  `GeomState::set_blocker`.
- **Skylight shafts** (openings of kind `skylight`, `skylight_*` or `shaft`) are open cuts: the cook opens the ramp's
  climb to the sky before meshing, so no lid stands in the shaft column and a body falling in from the plain meets
  none (DO27 known issue K8). `skylight_segments_opened` in the report counts them.

<!-- ignore-reason: a fragment; `bytes` is a cooked `<map>.meshlayers` the host already read -->
```rust,ignore
let geom = regia_world_geom::WorldGeom::from_cooked(&bytes)?;
for s in geom.sections() {
    let zone = &geom.zones()[usize::from(s.id)];
    println!("{} {} covered={} portals={:?} reverb={}", s.id, s.source, s.covered, s.portals, zone.reverb_preset);
}
```

## Cooking from the CLI

```sh
regia terrain passages import bore.json --json          # a do27-tunnels-v2/1 description -> bore.passages.ron
regia terrain cook bore.meshlayers.ron --ground-y -5 --width-m 512 --json   # writes bore.meshlayers
```

The files the document names are read relative to it. The ground is flat at `--ground-y` (-511 to 1535 m; the
heightfield input arrives with the map terrain lanes). `--json` prints the cook report. For the baker's three-passage
fixture (a curved tube, a straight gallery, a ramp to the surface at -5 m):

```json
{
  "out": "bore.meshlayers",
  "content_hash": "2ea4e40a2ae8044dfb5c4aa83c0b7c8259cac725465d4f29bb29fb36c0d5fa73",
  "bytes": 137871,
  "layers": [{
    "id": "tubes", "triangles": 494, "welded_vertices": 16, "trimmed_triangles": 4, "degenerate_triangles": 0,
    "render_triangles": 7904,
    "render_sections": [
      {"passage": "main", "triangles": 4048}, {"passage": "spur", "triangles": 1760}, {"passage": "ramp_w", "triangles": 2096}
    ],
    "skylight_segments_opened": 0, "sections": 8, "plug_triangles": 0
  }],
  "sections": [
    {"id": 0, "source": "tubes/main", "covered": true, "triangles": 140, "portals": [0, 5], "blocker": null, "reverb": "tube"},
    "... 6 more ...",
    {"id": 7, "source": "tubes/ramp_w", "covered": false, "triangles": 56, "portals": [4, 7], "blocker": null, "reverb": "open"}
  ],
  "portals": [
    {"id": 0, "sides": [0, 1], "mouth": null, "nav_mm": [-4851, -40000, 39496], "blocker": null},
    "... 6 more ...",
    {"id": 7, "sides": [7, null], "mouth": "west", "nav_mm": [-110000, -5000, 45000], "blocker": null}
  ]
}
```

`render_sections` lists the render triangles of each passage; `sections` (id, source, covered, triangles, portals,
blocker, reverb) and `portals` (id, sides, mouth, nav node, blocker) list what the section cut made, and each layer
reports `sections`, `plug_triangles` and `skylight_segments_opened` (`terrain.bake_report` reads the same report).
The same cook writes `<map>.viscells.ron` beside the document (lane W-21, ADR-8371; see
[Cells from the mesh-layer bake](viscells.md#cells-from-the-mesh-layer-bake)). `--json` adds `viscells` (that
path), `vis_cells` and `vis_portals`. A cook error exits non-zero with the code in the message.

## DO27's `do27-tunnels-v2/1` descriptions (`tunnel_bake`)

`regia_world_geom::tunnel_bake` is DO27's offline one-description baker rewritten in Rust: `validate_description`
(every rule of the format, a message that names the fix), `sample` (the pinned Stage A sampler; no ground means
unknown overburden, every segment an open cut), `build_meshes` (collision = the pinned mesher, render = the
subdivision above), `build_terrain_cut`, `build_nav` (the centreline graph), `check_consistency` (five rules:
`collision_inside_render`, `nav_on_collision_floor`, `mouth_cut_matches_collision`, `junctions_join_members`,
`render_covers_floor`) and `bake` / `bake_to_dir`, byte-stable in integer millimetres. The sample step is 1 to 20 m.

`regia terrain passages import bore.json [--map-id bore]` also takes such a description (a JSON object with
`passages` and no `format` key): it is validated, then written as a Regia-authored `<map>.passages.ron` (no sampled
block; the looks, gameplay block and level tags are not part of a passages file). A refused description is
`passages_import` with the validation message and `tunnel_description_invalid` in it.

## The ground: lattice, mouths, ring pins and the ground mode (W-10, ADR-8214)

The sim ground is the heightfield's own sample lattice: `n` samples per side over the map width, vertex `i` at
`x_mm(i)`, an integer millimetre within 0.5 mm of the drawn sample `-W/2 + i * W / (n - 1)` (the index is `i / (n - 1)`
everywhere, the same as the terrain draw). Its `f32` position is read through `mm_to_m` only, so a lattice vertex and
a canonical triangle vertex at the same millimetre have the same bits. Each cell splits on the fixed `(x0, z1)-(x1, z0)`
diagonal. A vertex's height is `offset_m + count * step_m`, or its override (a ring pin or a hanging vertex) in
millimetres. Cliff relief and every other render setting never reach the lattice. `regia_world_geom::ground::GroundLattice`
reads it from a cooked section (`ground_at(x, z)`: the height, `Cut` inside an open mouth, or `OffMap`).

**Mouths are named holes.** A `Passages` layer's mouths are its layout's openings (hole id = opening id). The footprint is
one quadrilateral per open-cut ramp segment, `width / 2 + 2.5 m` to each side along the sample's own normal, so a bend
leaves no wedge, and the top runs 1 m past the ramp top. A layer cuts only the holes it lists in `fills`:

```ron
layers: [(id: "tubes", source: Passages(path: "sb.passages.ron"), fills: ["sb_north", "sb_south"])],
```

The cut is the lattice triangles whose centroid is inside the footprint, stored as a bitmap per ground tile. Its
boundary, the **ring**, is closed loops of lattice edges (diagonals included). Every ring vertex is **pinned** to
`m_to_mm` of its count height, and every cut triangle on the ring becomes a canonical floor triangle (the **collar**)
with those pinned vertices. Each ring edge is then an edge of exactly one canonical triangle and one uncut lattice
triangle, with equal integer endpoints. The render hole is `HoleShape::lattice_rings(&loops)` (the ring loops as one
even-odd polygon), so the drawn cut and the sim cut agree cell for cell. The zipper from the collar to the authored
tunnel mesh is W-14's.

The cook then thins that seal (ADR-8334, ADR-8396). A run of three or more ring edges that fits one chord (within
0.45 m inside, 0.25 m outside, at most 16 m, pins on one height line) gets one two-triangle strip at the pins, and the
collar triangles it holds are dropped; a ring sample still off every floor gets a 1 mm wedge. Where a ramp ends off
the ground (above or below it, as on a flat acceptance datum), the ring past its top edge gets an approach strip: a
band at the pins and a rise to the ramp floor's own top-edge corners, so a body drives from the ground onto the ramp
without a step. A passage floor at least 1 m above the ring is kept over it. `collar_triangles` counts all of these
seal triangles.

**The ground mode is authored** (`ground:` in the document). `Full` keeps every ground tile (`ground_tile_cells`
cells a side, default 64) at full pitch. `Nested(max_level: k)` gives each tile its own level `0..=k` (pitch
`s * 2^level`) under the seam rule:

1. the coarsest level within `geom.pitch_error_m` (default 0.05 m) of the full-pitch ground;
2. over the memory budget is an error, never a silently coarser level;
3. neighbors at most one level apart;
4. hanging vertices on a pitch border take the coarse edge's height (an override), so the ground steps at most 1 mm
   there;
5. no pitch border within `geom.fine_band_m` (default 16 m) of a mouth ring or a layer's footprint.

The budget is 35 MB on maps up to 2.56 km wide and 64 MB beyond (counts, overrides, pins, cut bitmaps, canonical
triangles, tile levels). A `Full` map over it fails with `ground_over_budget` and names the smallest
`Nested(max_level: k)` that fits. The refusal is a bake-check report (`CookError::Checks`, the same `terrain.bake_report`
shape as every other check): its `ground_over_budget` finding carries `fits` (`"Nested { max_level: k }"`, absent when
no `Nested` mode fits) or, for a `Nested` map, `tiles` (the first 32 tiles held finer than `max_level`), and
`memory_bytes` is the bytes the authored mode needs (ADR-8436). A Washboard-sized map (2050 m, 6561 samples, 86 MB at full pitch) must be authored `Nested`.

To see the ground a document cooks to, give the import the document and the heightfield:

```sh
regia terrain passages import tunnels.json -o content/sb.passages.ron --json \
    --meshlayers content/sb.meshlayers.ron --heightfield content/sb_height.png --world-width-m 1280
```

The summary then carries `ground`: `mode`, `lattice_n`, `ground_tile_cells`, `tiles_per_side`, `levels` (tiles per
level), `tile_levels` (every tile, ascending index `b * tiles_per_side + a`), `hanging_vertices`, `ring_pins`,
`mouths` (`id`, `rings`, `ring_vertices`, `cut_triangles`, `collar_triangles`), `sim_bytes`, `budget_bytes`,
`max_pitch_step_m` (the largest step across a pitch border, `Nested` only) and `content_hash`. That cook samples the
layout on the given heightfield, not DO27's baked block; the map's real cook still checks `passages_drift`.
`--height-scale-m` (default 2048) and `--height-offset-m` (default 0) map PNG counts to metres.

## Error codes

| Code | Where | Meaning |
|---|---|---|
| `meshlayers_newer_version` | document, passages file | written by a newer engine (read before any other field) |
| `meshlayers_parse` | document, passages file | not RON, or an unknown field |
| `meshlayers_invalid` | document, passages file, cook | a value out of range (the message names the field); a blocker run on an unknown passage, overlapping another, or crossing a cover change or junction |
| `geom_bad_settings` | `GeomSettings::normalized` | a setting is not finite or out of range |
| `passages_import` | `regia terrain passages import` | the JSON is not `do27-tunnels/1` or is missing a block |
| `passages_unsampled` | `regia terrain passages export` | a Regia-authored file has no sampled block to export |
| `passages_drift` | cook | an imported sampled block differs from the layout sampled on this terrain: re-import |
| `input_missing` | cook, `regia terrain check-layers` | a file the document names cannot be read, or a sweep was requested with no patches and no on-foot grid (never an empty pass) |
| `ground_invalid` | cook | the heightfield is not an `n x n` lattice with a finite width, offset and step |
| `cook_not_built` | cook | `Gltf`/`Collision` sources (W-16) or `Volume` (W-32). A non-zero displacement whose absolute sum is at most 0.05 m is still `cook_not_built` (W-16). A larger sum is `render_off_collision` |
| `tunnel_description_invalid` | `tunnel_bake` | a `do27-tunnels-v2/1` description failed validation (the message names the field and the fix) |
| `tunnel_bake_disagrees` | `tunnel_bake::bake` (strict) | two outputs disagree; the message lists each failing rule with its measured value |
| `mouth_unknown` | cook | a layer `fills` a hole id its source has no mouth for |
| `mouth_not_simple` | cook | a mouth's ring touches itself at a lattice vertex (also two mouths whose cuts touch at a vertex only) |
| `mouth_off_map` | cook | a mouth's cut reaches the map edge |
| `ground_over_budget` | cook, `regia terrain check-layers` | the authored ground mode exceeds the sim-geometry budget (a `CookError::Checks` refusal); `Full`: the finding's `fits` names the `Nested(max_level)` that fits, `Nested`: its `tiles` lists the tiles held fine (ADR-8436) |
| `ground_pitch_seam` | cook (`Nested`) | a seam rule fails: a tile coarser than `pitch_error_m` allows, neighbors two levels apart, a step over 1 mm across a pitch border, or a pitch border within `fine_band_m` of a mouth or layer |
| `geom_newer_version` | load | a newer cooked file or canonical section |
| `geom_corrupt` | load | bad magic, trailing bytes, a count or index out of range, or header copies that differ from the canonical section |
| `geom_extent_too_large` | cook, load | a coordinate at or beyond 2^24 mm from the origin |
| `viscell_leak` | cook | a section wall has a gap of at least 0.5 m, or two sections still meet with every portal closed (the message names the section) |
| `viscell_bad_portal` | cook | a cooked portal is not one or more convex quads |
| `viscell_bad_box` | cook | a section zone cannot be an indoor cell box |
| `viscell_too_many` | cook | more cells or portal quads than `<map>.viscells.ron` version 1 allows |
| `render_off_collision` | cook | absolute `normal_displace` plus `noise_amplitude` is above 0.05 m (the drawn surface leaves the collision) |
| `nav_off_floor` | cook | a nav sample is more than 0.5 m off the collision floor |
| `mouth_cut_mismatch` | cook | the mouth ring and the cut disagree as integers (millimetres) |
| `junction_open` | cook | member floors at a junction step by more than `step_bound_mm`, or two stated endpoints differ |
| `lane_too_narrow` | cook | a passage is under 2 m wide (no on-foot lane) |
| `hull_blocked` | cook | half the passage width is under a hull radius (2.5, 2.6, 3.6, 3.9, 4.1, 4.5 m, or the 0.35 m pilot), or the ceiling is under 10 m |
| `drawn_solid_off_wall` | cook | a drawn solid is more than 0.5 m from the collision wall, or shorter than the sim lane |
| `hidden_face_in_wall` | cook | a face sits under a floor that is fused into a wall, within 0.5 m of that wall |
| `not_watertight` | cook | a leak flood fill from an open cell reaches the border |
| `above_ground_outside_cut` | cook | a canonical triangle of a covered section sits above LOD-0 ground outside a cut |
| `ground_inside_span` | cook | LOD-0 ground lies inside a covered section's floor-to-lid span |
| `seam_gap` | cook | an LOD seam is over 1e-4 m, or a long cap (edge over 8 m, midpoint over 0.02 m off the ground, both ends on the ground) |
| `portal_blocked` | cook | a portal joins a cell to itself, or joins nothing and names no mouth |
| `zone_mismatch` | cook | a zone names a missing section, or a sample lies outside every zone |
| `ground_vs_render` | cook | the authored ground and the render ground disagree (passes when no disagreement is authored) |
| `ground_pitch_seam` | cook | neighbouring ground tiles differ by more than one level |
| `ground_over_budget` | cook | the authored ground mode's sim bytes exceed 35 MB on a map up to 2.56 km, or 64 MB on a larger map (34 000 000 and 64 000 000 bytes). The finding's `fits` and `measured` name the smallest `Nested { max_level: k }` that fits a `Full` map; a `Nested` map's finding lists `tiles`. The cook does not switch mode (ADR-8436) |
| `column_over_max_floors` | cook | a column has more floors than `max_floors` or more than the span capacity (16) |
| `header_mismatch` | cook | a stated header copy (claimed `max_floors`) differs from the canonical section |
| `lattice_mismatch` | cook | a stated lattice `n` differs from the cooked lattice |
| `drift_triangles` | cook | a stated cooked triangle count differs from the stated layout count |
| `drift_manifest` | cook | a manifest row's size differs from the file it names |
| `drift_mouth` | cook | a stated layout mouth differs from the stated file mouth |
| `drift_portal` | cook | a stated portal station differs from the layout station |
| `spawn_not_flat` | cook | a spawn ring rises by more than its limit |
| `determinism_table` | cook | the canonical BLAKE3 differs from the stated `content_hash` |
| `ramp_facet_slope_over_limit` | cook | a floor triangle's horizontal-over-vertical slope is over the limit. An authored `waive` row may cover named triangles when its hash matches those cooked triangles |
| `shaft_lid_present` | cook | a covered section of a skylight or shaft ramp still has a lid above the ramp start (K8; the cook already cuts the lid, this check fails if one remains) |

## `terrain.bake_report` and `regia terrain check-layers`

`regia terrain check-layers <map> [--ground-y <m>] [--width-m <m>] [--json]` cooks a `<map>.meshlayers.ron`
in memory, or, when `<map>` is a cooked file, cooks the sibling `<map>.ron`. It writes nothing. A missing path
or a cooked file with no sibling source exits non-zero with `input_missing` and does not report a pass.
`--json` prints `terrain.bake_report`. Without it, each failed check is one line: `code at location: measured`.
A cook that fails a check exits with that check's code. `regia terrain cook --json` includes the same report
on `bake_report`. Lane W-28 owns the `terrain.bake_report` agent method; this page is the data shape.

```json
→ {
  "map": "probe",
  "passed": false,
  "ground_mode": "Full",
  "memory_bytes": 32,
  "content_hash": "<64 lowercase hex digits, or empty when the cook stopped early>",
  "waived": 0,
  "checks": [
    {
      "code": "drift_manifest",
      "passed": false,
      "waived": false,
      "location": "layer.bin",
      "measured": "layer.bin 10 vs 11",
      "triangles": [],
      "triangle_hash": ""
    }
  ]
}
```

`code` is lowercase ascii, digits and `_`. `measured` is text (no float field). `triangles` and `triangle_hash`
are set when the check names cooked triangles. `triangle_hash` is the BLAKE3 of those triangles' cooked bytes
(index, kind, flags, world millimetres), not of a file name. A `waive` entry in `<map>.meshlayers.ron` names
`check`, `triangles` and `triangle_hash`; a hash that does not match the cooked triangles does not waive.
`ground_mode` is the authored mode (`Full` or `Nested { max_level: N }`). The cook never switches it.
`memory_bytes` is the sim-ground bytes of that mode (spec 121 § 11: counts, overrides and pins, cut bitmaps, canonical
triangles, tile levels; 0 when the cook stopped before the ground). Two fields appear only on a failed
`ground_over_budget` finding: `fits` (a `Full` map: the smallest `"Nested { max_level: k }"` within the budget; absent
when none fits) and `tiles` (a `Nested` map: up to 32 ground tiles held finer than `max_level`, ascending). ADR-8436.

## Checking it

- `cargo test -p regia_world_geom --test meshlayers_formats`: `do27_json_imports_to_the_same_canonical_mesh`,
  `do27_json_round_trips_byte_identically`, `settings_change_moves_content_hash`,
  `every_canonical_field_moves_content_hash`, `runtime_reads_only_the_canonical_section`, `newer_versions_are_refused`.
- `cargo test -p regia_world_geom --lib units`: `m_to_mm_inverts_mm_to_m_below_2_23`.
- `cargo test -p regia_world_geom --features fixtures --test passage_render`: `collision_triangles_never_reach_the_draw_list`,
  `render_shares_the_collision_section_planes`, `cook_twice_gives_identical_bytes`,
  `passages_collision_bytes_unchanged_by_the_render_cook`, `render_boundary_is_the_canonical_boundary_split`,
  `displaced_detail_and_subdivision_one_are_refused`.
- `cargo test -p regia_world_geom --features fixtures --test tunnel_bake`: the baker's nine cases.
- `cargo test -p regia_world_geom --features fixtures --test passage_sections`: `switchbacks_cuts_35_sections` (37 by
  the spec's rule), `portals_close_every_section`, `skylight_shaft_lid_is_cut`, `collapsible_section_gets_one_plug`,
  `zones_contain_their_samples`, `cook_twice_gives_identical_bytes`, `section_bytes_are_pinned_across_hosts`,
  `chord_rule_cuts_only_where_the_centreline_leaves_the_chord`, `two_passages_from_one_point_share_one_portal`.
- `cargo test -p regia_cli --bin regia terrain`: `cook_reports_the_render_triangles_per_section`,
  `check_layers_missing_file_is_input_missing`.
- `cargo test -p regia_world_geom --features fixtures --test bake_checks`: one fixture per spec 121 § 10 check
  (`triangle_count_drift_fails_cook`, `stale_manifest_row_fails_verify`, `mouth_drifts_from_layout_fails_cook`,
  `portal_report_drift_fails_cook`, `spawn_ring_rise_over_limit_fails`, `drawn_solid_within_half_meter_of_wall`,
  `ramp_facet_slope_over_limit_fails_cook`, `column_capacity_exceeded_fails_cook`,
  `open_floor_edge_seam_report_flags_a_long_cap_on_uneven_ground`, `junction_member_floors_within_step_bound`,
  `sweep_without_patches_or_onfoot_grid_is_an_error`, and the § 10 codes in the table above).
- `cargo test -p regia_world_geom --features fixtures --test viscells_from_bake`: `sealed_sections_separate_by_flood_fill`,
  `every_mouth_is_a_portal_to_cell_0`, `every_junction_cut_is_one_portal`, `leak_check_fails_a_gap`,
  `viscells_cook_twice_identical_bytes`.
- `cargo test -p regia_terrain --test viscells_from_bake`: `switchbacks_viscells_load_in_the_runtime`.
- `cargo test -p regia_world_geom --test lattice_ground` (W-10): `lattice_vertices_are_integer_mm_and_shared_with_the_rings`,
  `rings_are_the_cut_boundary`, `ramp_bend_mask_has_no_wedge`, `ground_is_continuous_across_former_patch_edges`,
  `full_mode_over_budget_names_the_nested_mode`, `pitch_seam_hop_at_full_speed_le_5cm_per_tick`,
  `ground_pitch_seam_check_measures_the_step`, `cook_twice_gives_identical_bytes`. Each prints its measures; run with
  `-- --nocapture`.
- `cargo test -p regia_terrain --test lattice_cut`: `visual_grid_shift_is_the_half_texel_at_the_origin`,
  `mouth_holes_match_the_render_mask`.
- `cargo test -p regia_agent --test fuzz_targets` (targets `terrain.mesh_layers`, `terrain.passages`,
  `terrain.mesh_layers_cooked`) and `cargo xtask check-formats`.
