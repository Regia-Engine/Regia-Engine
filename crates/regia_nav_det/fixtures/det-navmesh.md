# Deterministic navigation mesh

`regia_nav_det::navmesh` (re-exported as `regia_nav::det::navmesh`) is a baked navigation mesh for lockstep simulations: convex polygons for open ground,
tunnel corridors, rooms, stairs and doors, with links that remember how wide, how high and how steep each opening is.
Two bakes of the same parts give the same bytes on every peer and every architecture (ADR-8012 A-4, ADR-8174). It sits
beside the [deterministic navigation grid](det-grid.md): the grid keeps hover hulls on open terrain, the mesh carries
bodies on foot, arenas, big maps and tunnel corridors. Like the grid it lives in the `regia_nav_det` crate, which has no
ECS dependency (ADR-8877), and `cargo xtask det-scan` holds it to the lockstep rules ([Lockstep math and the determinism
scan](lockstep-math.md)).

The module is Direct Order 27's navmesh copied whole (tag regia-absorb-2), with its arithmetic unchanged, so a game that
runs that mesh today can switch to this one with no checksum change.

## Bake

`NavMesh::bake(&BakeParts) -> Result<NavMesh, BakeError>` runs at cook time. `BakeParts` holds:

| Part | Becomes |
|---|---|
| `ground: Option<GroundRaster>` | Walkable cells (`walk`, `height`, `cols` x `rows` of `step_m`) merged into rectangles: runs per row, then runs with the same span across rows. Corner floors from the nearest raster heights. `PolyKind::Ground`, no ceiling, unlimited width. |
| `corridors: Vec<Corridor>` | One quad per centerline segment (`points`, `half_width_m`, `ceiling_m`); consecutive quads share their cross edge exactly. `PolyKind::Tunnel`. |
| `rooms: Vec<Room>` | One axis-aligned floor (`min_x..max_x`, `min_z..max_z`, `floor_y`, `ceil_y`). Width: the narrow side. |
| `stairs: Vec<StairRun>` | One tilted quad from the lower edge center `from` (`y0`) to the upper `to` (`y1`). |
| `doors: Vec<DoorQuad>` | A narrow quad across a wall (`center`, `dir`, `width_m`, `depth_m`). The width is the filter. |
| `extent_m`, `arena` | Recorded on the mesh and in the bake hash. |

Then every pair of polygons is linked:

- **Shared edges.** Two collinear edges (within 5 cm) that overlap by at least 0.4 m link through the shared part.
- **Overlaps.** A vertex of one polygon inside another (rims count) at a floor within 2 m links them through a short
  portal across the overlap, as wide as the narrower passage. This joins tunnel junctions, ramp openings, stair
  landings and door rims. Polygons that must not connect (stacked rooms, walled neighbors) share neither edges nor
  vertices; the 2 m floor check is the second guard.

A link records `width`, `step` (floor difference at the portal's middle) and `clearance` (headroom), so one bake
serves every body. Bake errors (a string): an empty bake, an empty or mismatched raster, a corridor with fewer than 2
points, no width, a repeated point or a fold, an empty room, a room, door or stair without headroom or run, a door
narrower than 0.4 m.

## Query

| Call | What it does |
|---|---|
| `locate(feet, &agent)` | The polygon under the feet: inside it, floor within `step_up`/`step_down` of the feet, headroom for `agent.height`. Ties go to the lowest id. |
| `locate_near(x, z, &agent)` | The nearest polygon within `MESH_SNAP_M` (6 m) in XZ. |
| `straight_walkable(from, goal, &agent)` | Walks the segment polygon by polygon through portals the agent fits (at most 128 steps). |
| `find_path(from, goal, &agent)` | A* over polygons (integer decimeter costs, `(f, h, id)` tie-break, at most `NAVMESH_MAX_EXPANSIONS` = 4000 expansions), then a greedy string pull that re-proves every skipped leg. `None` when an endpoint is off the mesh, the goal is unreachable or the budget runs out. |
| `find_path_scaled(.., tunnel_pct)` | The same with a meter inside a tunnel polygon costing `tunnel_pct` percent (clamped to 10-100) of a meter of ground; the heuristic is scaled by the same share, so it stays admissible. |
| `route_for(is_hover_hull, mesh_armed, mesh_tunnels)` | Grid or mesh for one body: hover hulls use the mesh only when it has tunnel corridors. |

`MeshAgent::pilot()` is a 0.35 m capsule (1.8 m tall, 0.55 m step), `MeshAgent::for_radius(r)` a body of radius `r` with
the same step, `MeshAgent::hover(r)` a hull disc with a 4 m step. A `MeshPlan` has `waypoints` (feet, the first is the
start), `polys` (one per waypoint), `expansions` and `length`.

## The wire format

`to_bytes()` writes the cooked mesh, `from_bytes(&[u8]) -> Option<NavMesh>` loads it (format `nav-det-navmesh`,
[`docs/formats/nav-det-navmesh.md`](../../docs/formats/nav-det-navmesh.md)): magic `NVM2`, a little-endian `u32`
version (`NAVMESH_VERSION` = 2), the bake id, extent, arena flag, then each polygon with its vertices and links. Every
`f32` is written as its canonical bits. `from_bytes` refuses bad magic, any other version, truncation, trailing bytes,
non-finite polygon floats, a vertex count outside 3-16, a link to a missing polygon, and a bucket index past
`MAX_LOADED_BUCKETS` (about a 16 km square). Loading sorts each polygon's links, so a loaded mesh writes the same bytes
it was baked with. `bake_id()` is an FNV-1a hash of the version and every bake input: compare it with your cook
manifest.

## Determinism rules

- Iteration orders are fixed: polygons in input order (ground rectangles, corridors, rooms, stairs, doors), links sorted
  by neighbor, buckets sorted, A* ties broken by `(f, h, id)`.
- Floats come only from IEEE `+ - * /`, `regia_detsim::math::sqrt` and `regia_detsim::float::{det_min, det_max,
  det_clamp}`. No transcendental, no fused multiply-add, no SIMD type, no hash container, no thread.
- The wire and the bake hash write floats through `regia_detsim::hash::canonical_f32_bits`: `-0.0` is `+0.0`, every NaN
  is `0x7fc0_0000`. A raster with a `-0.0` or a NaN height bakes to the same bytes as one with `+0.0` and the canonical
  NaN.
- No query reads a clock: the expansion budget bounds the cost of a search.
- The bytes and the bake id are pinned on x86-64 and aarch64 by the golden `crates/regia_nav_det/goldens/navmesh_bake.detgold`
  (three bakes and 64 seeded queries; see [Determinism goldens](det-golden.md)).

## How a host arms it

The authority never bakes at match time. It loads the cooked bytes, compares the bake id, and swaps the mesh in only
when the id changes (a new id drops every route, like a grid rebuild). Each query picks grid or mesh with `route_for`,
retries the locate a little below the body's origin (`MESH_LOCATE_DROPS`: origins sit above the feet), and falls back
to the grid when the mesh has no route.

```rust
use std::sync::Arc;
use regia_nav_det::navmesh::{
    route_for, BakeParts, MeshAgent, MeshPlan, NavMesh, RouteKind, Room, MESH_LOCATE_DROPS,
};

/// The navigation half of a lockstep sim: an armed mesh (the grid is not shown).
#[derive(Default)]
struct DetNavHost {
    mesh: Option<Arc<NavMesh>>,
    mesh_id: u64,
    routes: Vec<(u32, MeshPlan)>,
}

impl DetNavHost {
    /// Arm the map's cooked mesh. Same bake id: nothing changes. New id: every route is dropped.
    fn arm(&mut self, cooked: &[u8]) -> Result<(), &'static str> {
        let mesh = NavMesh::from_bytes(cooked).ok_or("navmesh_refused")?;
        if mesh.bake_id() != self.mesh_id {
            self.mesh_id = mesh.bake_id();
            self.mesh = Some(Arc::new(mesh));
            self.routes.clear();
        }
        Ok(())
    }

    /// One route request. `None` means "use the grid".
    fn plan(&self, hover_hull: bool, from: [f32; 3], goal: [f32; 3], agent: &MeshAgent) -> Option<MeshPlan> {
        let mesh = self.mesh.as_ref()?;
        if route_for(hover_hull, true, mesh.has_tunnels()) == RouteKind::Grid {
            return None;
        }
        MESH_LOCATE_DROPS.iter().find_map(|dy| {
            mesh.find_path_scaled([from[0], from[1] + dy, from[2]], [goal[0], goal[1] + dy, goal[2]], agent, 75)
        })
    }
}

# fn main() {
let room = Room { min_x: -10.0, min_z: -10.0, max_x: 10.0, max_z: 10.0, floor_y: 0.0, ceil_y: 3.0, level: 0 };
let parts = BakeParts { rooms: vec![room], extent_m: 20.0, arena: true, ..BakeParts::default() };
let cooked = NavMesh::bake(&parts).expect("bake").to_bytes();

let mut host = DetNavHost::default();
host.arm(&cooked).expect("the cooked mesh loads");
// The body's origin sits 0.9 m above its feet; the route still starts on the floor below it.
let plan = host.plan(false, [-5.0, 0.9, 0.0], [5.0, 0.9, 0.0], &MeshAgent::pilot()).expect("a route");
assert_eq!(plan.waypoints.len(), 2);
// A hover hull on a mesh without tunnels stays on the grid.
assert!(host.plan(true, [-5.0, 0.9, 0.0], [5.0, 0.9, 0.0], &MeshAgent::hover(2.0)).is_none());
assert!(host.arm(b"junk").is_err());
# }
```

## The agent surface

There is no protocol method: the mesh has no runtime state of its own, it lives in the host's sim. A host that wants an
agent to see it reports `describe()` (bake id, polygon and link counts, extent) or `bake_id()` from its own method;
`nav.det_*` methods, if any, come with the deterministic navigation agents. Checks:

- `cargo test -p regia_nav_det --lib navmesh`: DO27's nine navmesh tests (asserts unchanged), `bake_bytes_identical_twice`,
  `canonical_float_bytes_match_do27_rule`, `from_bytes_refuses_a_newer_version`, `newer_version_is_refused`,
  `navmesh_layout_matches_the_writer`, `from_bytes_refuses_an_index_past_the_bucket_cap`,
  `from_bytes_never_panics_on_mutated_bytes`, `navmesh_bake_id_pinned_on_every_arch` (the golden; bless with
  `REGIA_BLESS=1` only when the change in bits is intended).
- `cargo test -p regia_agent --test fuzz_targets`: the fuzz target `nav.det_navmesh`.
- `cargo xtask check-formats` (`nav-det-navmesh`) and `regia upgrade-project` (`.detnav` files).

The errors a host may report from `arm` are its own; this page uses `navmesh_refused`.

## Driving the mesh from behavior trees

`regia_ai::DetNavMeshProvider` puts a baked mesh (shared through an `Arc`) behind the `PathProvider` seam, so the
`MoveTo` leaf and state-tree task plan on it (`with_provider("det_navmesh")`). The agent spec's radius, height and steps
become the `MeshAgent`; a failed search names `start_off_nav`, `goal_off_nav` or `unreachable`. When to pick the mesh
over the grid or the f32 navmesh: [Choosing a navigation stack](../user-guide/choosing-navigation.md).
