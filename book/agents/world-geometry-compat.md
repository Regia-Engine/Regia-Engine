# World geometry, Stage A: tunnel layouts, meshes and the floor query

`regia_world_geom` is the GPU-free, ECS-free crate a lockstep sim links for its world geometry: the floor
under a body, the ceiling over it, the push out of a wall and the rock between two points (spec 121,
ADR-8019). It has two stages. **Stage A** (this page, ADR-8050) is `regia_world_geom::compat`: Direct Order
27's tunnel code copied with its arithmetic unchanged, so DO27 can swap its own modules for the crate and
keep its lockstep checksums. **Stage B** (the native core: cooked integer-millimetre triangles, the lattice
ground, blockers and ground edits) comes later behind the `stage_b` feature, and its agent methods with it
(lane W-28, spec 121 § 12).

## What Stage A gives a host

| Module | Items | Use |
|---|---|---|
| `compat::layout` | `TunnelLayout`, `Passage`, `PassageKind`, `Opening`, `Junction`, `CrawlwayVolume`, `SampledLayout`, `MouthHole`; `TunnelLayout::sample`, `mesh`, `export_json`, `bake_check`; `SampledLayout::mesh`, `triangles`, `mouth_holes` | Describe passages as splines (x, floor y, z in metres), sample them every 5 m, and mesh floors, walls, ceilings, lips and mouths |
| `compat::mesh` | `TunnelCollisionMesh` (`new`, `welded`, `with_layer`, `support_floors`, `floors_at`, `sample_floor_at`, `ceiling_above`, `segment_blocker`, `is_portal_at`, ...), `MeshLayers`, `clamp_altitude_under_ceiling`, `separate_disc_from_walls`, `separate_disc_from_walls_band`, `TunnelNavOverlay` | The triangle mesh with its 16 m XZ index and its support floors (trim left out, welded to the ground); the ceiling clamp; the disc wall push; the deck-aware nav overlay |
| `compat::surface` | `column`, `Column` (`spans`, `span_at`, `highest_mesh_span`), `Span`, `surface_under`, `ceiling_over`, `BODY_REACH_M`, `MAX_SPANS`, `WELD_M`, `TRIM_M`, `TRIM_WIDTH_M` | The one "what is under me, what is over me" query: the free spans of a column, bottom up, and the span a point is in |
| `compat::ground` | `query_ground`, `sample_floor_with_tunnels`, `SampleFloorTunnels`, `GroundSupport`, `STEP_BOUND_MM`, `step_ok`, `HoleMask` | The query as one call (`column(..).span_at(y)`), and the floor a placed body gets: a tunnel floor with room for the hull, else the heightfield |
| `compat::state` | `TunnelsState<N = ()>` (`support`, `body_support`, `move_floor`, `place_floor`, `nearest_floor`, `confine`, `underground`, `below_ground_at`, `rock_between`, `rock_hidden`, `rock_hidden_lit`, `separate_from_walls`, `clamp_altitude`, `next_waypoint`, `depth_at`, the camera's `camera_floor`, `mesh_spans`, `camera_clip`), `TunnelNavBake`, `NoNavBake`, `sample_place_floor`, `nav_floor_y`, `floor_with_skylight_prop` | The sim's tunnel arm: the flag, every body's floor and lid, the walls that confine it, the rock between two points, the nav overlay; the host's navmesh plugs in through `TunnelNavBake` |
| `compat::hover_ground` | `sample_move_floor`, `ground_hover_hull`, `ground_tilt_under`, `ground_tilt_at`, `ground_tilt`, `HoverGroundBody`, `GroundDelta`, `NoGroundDelta`, `TiltParams`, `LEDGE_M`, `HULL_WALL_M` | The hover-hull ground rules (wall rule, ledge rule, ground tilt), one implementation for host and client; the host's body, crater book and tilt tuning come in through the three seams |
| `compat::sense` | `hear_range_m`, `satellite_shows`, `can_salvage`, `InsideStructure`, `may_build_inside`, `UNDERGROUND_HEAR_MULT` | Who hears, sees and takes what underground |
| `compat::sampler`, `compat::terrain_patch` | `HeightGrid`, `TerrainSampler` (`shaft_at`), `FloorSample`, `FloorSurface`, `TerrainPatch` | The heightfield the query reads (5 m cells, `i / n` index: the Stage A convention) |
| `compat::pack` | `parse_tunnels`, `load_tunnels_file`, `load_tunnels`, `TunnelsFile`, `tunnel_mouth_issues`, `MouthCheckMap`, `baked_holes` | The baked tunnel file (`do27-tunnels/1`), the runtime loader and the mouth checks |

The names are DO27's on purpose: they are the switch contract. The query modules (`mesh`, `ground`,
`sampler`, `surface`, `state`, `hover_ground`) are copied from DO27's `regia-absorb-2` tag (its protocol 1.58,
the T3 surface query; ADR-8167); `layout`, `pack` and `sense` did not change there.

## The T3 rules in plain words

- **One column per XZ.** The terrain and every mesh layer go into one list of free spans, bottom up. A span is
  a floor and the lowest lid over it (none under the sky). A mesh floor with no lid under the ground cuts the
  ground (an open cut: the trench floor is the ground there). Ground that lies inside a lidded span is carved
  space: it is lifted onto the lid, and a skylight shaft patch leaves no ground in that column at all.
- **`span_at(y)`.** A point is in the span with the highest floor at or under it. A point above that span's lid
  is in rock, and the nearer face wins: the lid (ties go to the lower span) or the next floor up. With no floor
  under it, the lowest floor over it answers, so the host's wall and ledge rules see a rising floor.
- **The waist reach.** A body asks at its waist, `BODY_REACH_M` (0.9 m) over its origin
  (`TunnelsState::body_support`, `move_floor`). A floor that rises over the waist in one tick is a wall. A shell or
  a camera asks at its own height (`TunnelsState::support`).
- **Ground wins a tie.** Two floors at one height are one surface; of a mesh floor and the ground at the same
  height, the ground stands (a welded lip and the terrain are one surface).
- **The lid gap.** A lid counts as over a floor only when it is more than 0.05 m above it.
- **Built once at load.** A narrow floor strip (under 2 m across) lying within 0.5 m of another floor is trim (a
  threshold sill) and carries nothing; a floor vertex up to 0.15 m proud of the ground is welded onto it
  (`TunnelCollisionMesh::welded`). The drawn triangles are not changed.
- **No history.** The answer is a pure function of the point: no last floor, no sticky surface, no step band.
- **Craters only on the ground.** `sample_move_floor` adds the host's `GroundDelta` on a heightfield floor and
  never on a mesh floor.

Removed at T3 (each with an Upgrade line in the release notes): `sample_floor_ext`,
`TerrainSampler::skylight_height_at`, `SUPPORT_RISE_M`, `PORTAL_HYSTERESIS_M`, `DECK_SNAP_M`, the
`SampleFloorTunnels::previous` field, the `sticky` argument of `sample_floor_with_tunnels` and every
`previous` / `prev_floor` argument.

## Arming a map's tunnels

A host keeps its own layout registry. `load_tunnels` uses the baked file under
`<root>/assets_regia/maps/<map>/tunnels.json` when it is present, parses and a terrain is given, and the
host's spline layout otherwise. Both paths build the same mesh bit for bit when the file is the current
export:

<!-- ignore-reason: a sketch of a host's map load; `my_layout_for`, `root` and `terrain` come from the game -->
```rust,ignore
use regia_world_geom::compat::mesh::{TunnelCollisionMesh, TunnelNavOverlay};
use regia_world_geom::compat::pack::load_tunnels;

let armed = load_tunnels(
    root,
    "my_map",
    Some(&terrain),
    |file, terrain| file.collision_mesh(terrain),
    |id, terrain| {
        let layout = my_layout_for(id)?;
        let sampled = layout.sample(terrain);
        Some(TunnelCollisionMesh::new(sampled.triangles(&layout.openings, terrain)))
    },
);
let nav = armed.as_ref().map(TunnelNavOverlay::bake);
```

A file that is truncated, carries `NaN` or `Infinity`, names another format or lacks its sampled block is
logged (`tracing`) and ignored, and the spline layout arms the map instead. So is a file that would make the mesher
misbehave (ADR-8166, review rev-b L6): a number that is not finite or is over `MAX_TUNNEL_NUMBER` (1e6) in
magnitude (a JSON number past the `f32` range narrows to an infinity), a position more than
`MAX_TUNNEL_HALF_EXTENT_M` (8192 m) from the map origin on any axis, a width or height outside
`0..=MAX_PASSAGE_SIZE_M` (256 m), a side normal that is not a unit vector, a `covered`, `wall_left` or
`wall_right` list whose length is not the passage's segment count, or counts over `MAX_TUNNEL_SAMPLES`,
`MAX_TUNNEL_CONTROL_POINTS`, `MAX_TUNNEL_ITEMS` and `MAX_TUNNEL_TRIANGLES`. Before these bounds one far
coordinate made the collision mesh ask for exabytes and abort the process.

## Tunnel navigation: arming `TunnelsState<NavMesh>`

`TunnelsState<N>` carries a navmesh beside the collision mesh. The world-geometry crate never bakes it: a
host passes a `TunnelNavBake` to the `*_with_nav` constructors (`for_layout_with_nav`, `from_layout_with_nav`,
`from_baked_with_nav`), and the plain constructors leave `navmesh` at `None` (`TunnelsState<()>`, the form a
headless server without the nav crate links). The crate edge is one way: `regia_nav` depends on
`regia_world_geom`, never the reverse.

`regia_nav::det::tunnel_bake::TunnelNavMeshBake` is that bake for the deterministic navmesh
(`regia_nav::det::navmesh::NavMesh`; spec 121 § 4.2, ADR-8215). From the sampled layout it makes one corridor
per passage (tube, gallery and ramp: the sample floor points, half the width, the ceiling), and from the
terrain a ground raster (`navmesh_ground`): one cell per terrain sample at the base height, walkable where the
central-difference slope is at or under `hover_cliff` (default 0.78, the grid's hover class). The baked mouth
patches are not read: the mesh walks the ramp corridors through the open cut, so the ground the ramp tops link
to stays flat. With no terrain there is no navmesh. `NavTunnelsState` names the armed type:

<!-- ignore-reason: the Switchbacks layout needs regia_world_geom's off-by-default `fixtures` feature; the same code runs as the doc example of `regia_nav::det::tunnel_bake` -->
```rust,ignore
use regia_nav::det::navmesh::{MeshAgent, PolyKind};
use regia_nav::det::tunnel_bake::{NavTunnelsState, TunnelNavMeshBake};
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};

let terrain = TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0)
    .with_zero_counts(16384);
let tunnels =
    NavTunnelsState::for_layout_with_nav(switchbacks(), Some(&terrain), &TunnelNavMeshBake::default());
let navmesh = tunnels.navmesh.as_deref().expect("a terrain gives a navmesh");
assert!(navmesh.has_tunnels());
// A hull route: the hover agent, a tunnel metre at 75 % of a ground metre.
let hull = MeshAgent::hover(2.5);
if let Some(plan) = navmesh.find_path_scaled([-300.0, -256.0, 150.0], [300.0, -256.0, 150.0], &hull, 75) {
    let underground = plan.polys.iter().any(|p| navmesh.poly_kind(*p) == Some(PolyKind::Tunnel));
    println!("{} waypoints, underground {underground}", plan.waypoints.len());
}
```

The bake is a pure function of the sampled layout and the terrain, so arming from the baked file
(`from_baked_with_nav`) gives the same navmesh bytes and `bake_id` as arming from the splines
(`switchbacks_baked_file_builds_the_byte_identical_sim` in `regia_nav`'s `tests/det_tunnel_nav.rs`), and
`switchbacks_navmesh_bake_id_is_pinned` holds the Switchbacks bake id and byte hash on every target. The same
module implements the grid's `GridGround` for `TerrainSampler`, so a host builds the deterministic grid and the
tunnel layer (`regia_nav::det::layer::TunnelLayer::build`) from the terrain it arms the tunnels with; see
[Deterministic navigation grid](det-grid.md) and [Deterministic navigation mesh](det-navmesh.md). The tunnel
plans of both (64 `plan_layered`, 64 navmesh plans for hulls and pilots on the tunnel test world) are pinned by
the golden `crates/regia_nav/goldens/tunnel_plans.detgold`.

## Determinism

Runtime calls use `f32` `+ - * /` in a fixed order. Since ADR-8166 (review rev-b L1, L2) the copied code calls
`regia_detsim::math` (libm 0.2.16) for `sqrt` and for the trig of the mouth-hole test, and
`regia_detsim::float::{det_min, det_max, det_clamp}` for every float `min`, `max` and `clamp`, the same change
Direct Order 27's DET_FLOAT makes on its side. The crate is on the `cargo xtask det-scan` lockstep closure (see
[Lockstep math and the determinism scan](lockstep-math.md)) with the full lockstep `clippy.toml`, and no module
waives it. One pinned site remains:

| Site | When it runs | Stage B |
|---|---|---|
| `tunnel_mouth_issues` duplicate ids (`HashMap` iteration) | validation only; the issue order varies | the bake checks of lane W-09 |

`regia_world_geom::QUERY_BITS_VERSION` (now `2`: the T3 query, ADR-8167) names the bits of the mesher, the hole list and the queries. A
lockstep handshake sends it beside the map's content so a peer on an older Regia patch is refused rather than
desyncing; any change that moves a query bit bumps it (spec 121 § 14 rule 2).

The cross-arch pin `tests/mesh_bits.rs` hashes, per fixture map and terrain, the mesh and nav overlay, the mouth
holes (`mouth_holes`: centre, half sizes and yaw bits) and a fixed query table: `query_ground`, `ceiling_above`,
`separate_disc_from_walls_band` and `segment_blocker` at points around every control point and mouth top, and
`hole_at` on a 0.25 m grid over each mouth. Every host must produce the recorded hashes. Its second test,
`stage_a_bit_table_is_pinned_on_every_arch`, hashes every public query of `compat::state` and
`compat::hover_ground` (`support` to `ground_tilt_at`) at the same points on The Switchbacks, The Cinder Steps and
the four-deck stack. DO27's T3 reference vectors (`tests/fixtures/do27/t3-vectors/`, format
`do27-t3-vectors/1`) are checked by `tests/t3_vectors.rs`: the mesh-only files bit for bit, the canyon file by the
span chosen (and, with the copied sampler, its heights too).

The source's `math::atan2` takes a signed-zero input as `+0` and returns the canonical NaN; the copied `atan2`
sites (the ground tilt, the mouth-hole yaw) add those two steps around `regia_detsim::math::atan2_libm` (the bare
libm `atan2f`), so their bits equal the source's for every input. They do not call `regia_detsim::math::atan2`: since
`MATH_BITS_VERSION` 1 that is the correctly rounded `atan2` (ADR-8876), which differs from `atan2f` by one ulp on some
inputs, and Stage A keeps the source's bits.

## Fixtures

DO27's three armed layouts (The Switchbacks, Atacama Rail, The Cinder Steps) and the synthetic meshes
(`synthetic_gallery`, `synthetic_four_decks`, `synthetic_building_decks`) are test data, compiled only for the
crate's own tests or with the off-by-default `fixtures` feature (`compat::fixtures::for_map`).

## The on-foot grid (`compat::onfoot`)

A walker (a pilot on foot) does not use the hull's disc. `compat::onfoot` keeps the on-foot collision the
source sim used, with its arithmetic unchanged:

- **The grid.** A sparse XZ grid of 0.75 m cells (`CELL_M`), keyed in a `BTreeMap` so every peer visits cells in
  the same order. A cell holds an optional walkable top and up to four disjoint vertical wall spans
  (`MAX_WALL_SPANS`), sorted, joined when they overlap or leave a gap of 0.10 m or less (`SPAN_JOIN_M`). Keeping
  spans apart is what lets a walker cross a tube seam whose hidden slab end sits under the floor and whose roof
  facet is 13 m overhead. A cell blocks a walker only when one of its spans stands in the body band (above one
  step, below the head; a lip under 0.30 m thick and no higher than a step plus 0.35 m is stepped over).
- **The span cap is watched.** A fifth disjoint span merges the closest pair (the walls stay closed). The source
  did that silently; here `PilotCell::add_wall` returns `true` and `PilotGrid::span_cap_hits` counts it (also
  across `merge_grid`), and the loader logs the total. A high count means a lining is ribbed finer than the cap.
  `PilotGrid::wall_span_cap` is that cap for `stamp_wall` (default `MAX_WALL_SPANS`). `add_wall_capped` and
  `stamp_wall_capped` take an explicit cap, clamped to `1..=MAX_WALL_SPANS`. Shipping stamps leave the default.
  The field is not behind `stage_b`. A test may set it to 1 before stamping (see Proving a swap).
- **Stamping.** `stamp_triangle` keeps upward faces (normal y >= 0.55) as floors and steep faces (|normal y| < 0.45,
  at least 5 cm tall) as walls; a canyon-sized floor triangle (over 96 m) is the heightfield and is skipped.
- **The T3 rule: 0.30 m.** On a map with mesh layers, a drawn floor within 0.30 m (`FLOOR_MATCH_M`) of the sim
  floor nearest it (the heightfield or any tunnel floor at that XZ) is dropped, so a walker stands on the one
  surface the sim answers there instead of a 0.75 m staircase of the drawn lining floor.
- **Doors.** A mesh named like a door leaf (`door`, `socket`, `moving`) is not stamped; its slab carves a mouth
  through the wall spans (merged with a nested inner panel), and the jambs stay.
- **Moving.** `slide_move` samples the step every 0.2 m and bisects to the first blocked point, then pushes out of
  a piece the walker started inside. `resolve_feet` slides, then stands the feet on the tunnel floor, a grid top
  or a box top within one step up or 0.55 m down. `pilot_floor_y` is the floor a walker stands on: the grid deck,
  the skylight rubble prop (`compat::state::floor_with_skylight_prop`), the host's building floors and its box
  tops within one step.

The game data is the host's (spec 121 § 4.2):

| Seam | What the host gives |
|---|---|
| `OnFootParams` | capsule radius, walker height, door width, building rim radius (no default; the test fixture uses 0.35, 1.85, 1.10, 9.0 m) |
| `OnFootSolids` | `pilot_solids()` (the world and building grids), its standing buildings (art key, centre, yaw), its loose boxes (a `force_block` box is never a floor), its fixed stage boxes, and its rock push |
| `building_key(stem, variant)` | the art key; a host with a second art set per stem passes `variant: true` |
| `door_local(socket, params)` | the host's door socket for the art, or `None` for the notch on the building rim |
| `mesh::GltfGridSource` | the glTF text and buffer ranges of each set piece and building (`GltfFile` from disk, `GltfMemory` from memory); load time only, no query reads a file |
| `mesh::load(params, set_pieces, buildings, terrain, tunnels)` | the host lists the set-piece sources (`gltfs_in` and `usable_gltf` help with its directory walk) and its `(key, source)` buildings |

Every float `min`, `max` and `clamp` goes through `regia_detsim::float::det_*` and every `sqrt`, `sin` and `cos`
through `regia_detsim::math`; `tests/mesh_bits.rs` `onfoot_bit_table_is_pinned_on_every_arch` pins the stamped
grid, `resolve_feet`, `slide_move` and `pilot_floor_y` over a fixed walk on the gallery and the Switchbacks fixture.

## Proving a swap

Stage A is `regia_world_geom::compat` plus the navmesh bake behind `TunnelNavBake`. These tests are the bars a
swap is checked against. A failure is a moved bit or a blocked route. The counts below are the source's.

| Test | Bar |
|---|---|
| `an_empty_solid_set_fails_the_sweep` | A sweep with no solids is `EmptySolids`. It is never a pass. |
| `sweep_without_patches_or_onfoot_grid_is_an_error` | A loaded map with an empty on-foot grid is `MissingOnFootGrid`. A non-empty grid whose terrain has neither mouth holes nor patches is `MissingPatches` (K13). |
| `sweep_switchbacks_and_cinder_steps_are_clear` | Switchbacks and The Cinder Steps: no probe failures, hull `blocked == 0`, worst push under 0.0005 m, a finite lid of at least 10 m, hull lane at least 10 m, pilot full-block count 0, pilot clear run at least 2 m. The class-row count is printed (`SWEEP_CLASS_ROWS`); it is not pinned at 156. |
| `a_pilot_on_foot_walks_the_whole_tunnel_system_west_to_east` and `_east_to_west` | 688 of 688 waypoints, both directions, more than 120 s underground, no one-tick jump over 2 m. Sim walls only (empty on-foot grid). |
| `a_player_hull_from_the_plain_drives_into_the_north_middle_gallery` and `a_pilot_on_foot_from_the_plain_walks_into_the_north_middle_gallery` | 69 of 69, jump at most 1 m. |
| `fused_spans_block_the_gallery_walk` | With `wall_span_cap` forced to 1 the gallery walk stops before waypoint 40 and within 15 m of the lining seam near `(-137.6, 76.1)`. With 4 it passes. The shipped constant `MAX_WALL_SPANS` stays 4. |
| `a_hull_in_the_tube_passes_under_a_wreck_and_a_building_on_the_canyon_floor` and `a_hull_on_the_canyon_floor_passes_over_a_wreck_and_a_hull_in_the_tube` | The tube run stays underground and passes the spot. The canyon run passes over it. A second body on the other floor does not move the path (compat has no entity discs, so the two runs match). |
| `stage_a_layers_hold_on_the_canyon_fixture` | Same XZ window under `(-407.8, 72.4)`. The tube body is underground every tick, stays under the lid (`tube floor + 12 m`) and above the floor, hop at most 0.05 m. The canyon body, placed on the heightfield with no tunnel hint, is never underground and stays above that lid. |
| `stage_a_bits_are_the_same_with_and_without_stage_b` | `tests/fixtures/stage_a_bits.ron` matches a live recompute of the mesh, query and on-foot tables. The fold of those words with `QUERY_BITS_VERSION` and the navmesh ids is `0x9678bcc22e3e02c7` (`QUERY_BITS_VERSION` 7). The test builds the crate with and without `--features stage_b` (the feature is empty) and compares. Child builds use a sibling of `CARGO_TARGET_DIR`. |
| `stage_a_mouth_hop_at_full_speed_le_5cm_per_tick` | Mouth and passage hops at most 0.05 m (measured about 0.001 m). The meter is `tests/support/hop_meter.rs` (`HOP_LIMIT_M` 0.05, 17 samples, ten halvings), shared with the layer test and later rows. |
| `gallery_floor`, `mesh_bits`, `t3_vectors`, `onfoot_grid`, `det_tunnel_nav` | The W-01..W-04 pins. Unchanged by this lane. |

`stage_b` is an empty feature. Nothing in `compat` is `cfg`-gated on it. A later Stage B change that moves a pinned word bumps `QUERY_BITS_VERSION` in the same commit (spec 121 § 14 rules 1-2).

The sweep stands Switchbacks on the -256 m canyon (the tubes are under that floor). The Cinder Steps stand on a flat +36 m shield: on the canyon plane those tubes are open air, so the mesher emits no lids and each ramp lip falls to the plane across the core. +36 m covers the tubes and leaves the ramp mouths as open cuts.

A walker stands on `pilot_floor_y`. A hull rides 1 m over the floor. Where that floor changes by more than 0.75 m in one tick the hull keeps its world height and falls back to the ride, so the gallery mouth is not a one-tick snap onto the new floor. The 1 m jump bar is unchanged.

### What Direct Order 27 runs

Regia does not claim the swap. Direct Order 27 swaps the W-01..W-04 modules on a branch of its own and runs these, unchanged, until they match:

- `switchbacks_tunnel_fight_lockstep_checksum_is_pinned`, checksum `0xbd96_cd18_09dc_958d` (`ir-core/tests/tunnels_switchbacks.rs` line 1187 at tag regia-absorb-3).
- `a_net_client_predicting_its_hull_through_the_tunnels_never_shows_a_vertical_jump` (same file, line 510).
- `determinism_fuzz_short_fixtures_agree_every_tick`.
- The 3600-tick two-seat replay hash.
- LOCKSTEP_050, 12 checkpoints per map, in both builds: bore_twelve final `1bf9154f9164c5e0`, switchbacks final `a73431db19d8cd80`.
- Its `tunnel_t3.rs` and `tunnel_sweep.rs`.
- The shells test in `ir-sim/src/state.rs` (line 23186 on the T3 handover; that file's comments moved between tags, so find the test by name on the swap branch).
- The T3 vectors. The four files already in this repo, with the checksums in `crates/regia_world_geom/tests/fixtures/do27/t3-vectors/README.md`: `switchbacks_mesh.json` SHA-256 `10b8c8c276dab1d1dc762f20f7802ddcb161abd6ca1eec89296ca974e3ca606e`, `cinder_steps_mesh.json` `debdee24cff297f5cdbe89178556ee97bcc132ab2d9f35dd4e62657a909780f6`, `four_decks_mesh.json` `936ec2cec6268808756c8adbdf645cf1e1a51c72ed8c2832ee1e71e2c113c188`, `switchbacks_canyon.json` `7417c5b12cc5bfa6a396277f5f1b006cd7f1cf57efe132a22d6b17b7e9ad1b25`.
- The regenerated edge-row file, beside those four: `crates/regia_world_geom/tests/fixtures/regia/t3-edge-vectors/cinder_steps_mesh.json`, SHA-256 `9b9296529656d1d520ede9513cff947bc10201f5fdfdbb15ebf0f3d08fbc4fc1`. W-11 owns generation. This lane names the path and the checksum. The file is not in this branch.

The swap is not done until those match.

## Agent protocol

No method yet: the crate holds no engine or ECS state, so there is nothing for an agent to query at
runtime. Lane W-28 adds the `terrain.*` geometry methods with Stage B (spec 121 § 12). An agent checks Stage
A with the crate's tests:

```text
cargo test -p regia_world_geom
  compat::layout::tests::switchbacks_mesh_is_byte_identical_from_the_baked_description
  tests/gallery_floor.rs (5 tests)
  tests/runtime_loader.rs::the_runtime_loader_arms_every_map_byte_identically
  tests/runtime_loader.rs::a_baked_file_with_a_far_coordinate_is_refused
  tests/mesh_bits.rs::fixture_meshes_hash_to_the_pinned_bits_on_every_arch (mesh, holes, queries; every arch)
  tests/mesh_bits.rs::stage_a_bit_table_is_pinned_on_every_arch (state and hover_ground queries; every arch)
  tests/t3_vectors.rs::t3_vectors_match_the_compat_query (the four T3 vector files)
  tests/stage_a_hop.rs::stage_a_mouth_hop_at_full_speed_le_5cm_per_tick (0.001 m; the bar is 0.05 m)
  tests/hover_ground_seams.rs (the body accessors, the crater on the ground only)
  tests/tunnels_switchbacks.rs::switchbacks_baked_file_builds_the_byte_identical_sim
  tests/onfoot_grid.rs (span-cap report, the 0.30 m drawn-floor drop, the skylight prop)
  tests/mesh_bits.rs::onfoot_bit_table_is_pinned_on_every_arch (the on-foot grid and walk; every arch)
  tests/tunnel_sweep.rs (empty solids, missing grid or patches, Switchbacks and Cinder Steps clear)
  tests/tunnel_walks.rs (688/688 both ways, gallery 69/69, fused span cap, hull under and over)
  tests/stage_a_layers.rs::stage_a_layers_hold_on_the_canyon_fixture
  tests/stage_a_bits.rs::stage_a_bits_are_the_same_with_and_without_stage_b (two extra builds)
cargo xtask det-scan --json
```

## Errors

`parse_tunnels` returns a `String` naming the field (`tunnels.json format ...`, `tunnels.json missing
sampled`, `non-finite number (NaN)`, `truncated JSON`, `tunnels.json sampled.passages[0].samples[3].p: ... m is
outside the map`, `tunnels.json sampled.passages[0].covered has 9 entries for 10 segments`). `tunnel_mouth_issues` returns `MapIssue` rows with
the check names `tunnel_mouth`, `inside_map`, `on_heightfield` and `unique_id`.
