// SPDX-License-Identifier: MPL-2.0
//! Multi-level walkable surfaces (W-18, ADR-8076): the builder keeps every walkable span of a
//! column, not just the top one, so a tunnel under the ground, the ground under a bridge and the
//! lower floor of a two-storey room all get navmesh polygons. It is a builder setting
//! (`NavMeshBuilder::build_layered`); the other builds keep the top surface only, as before.

use regia_core::math::Vec3;
use regia_nav::{NavBuildError, NavMesh, NavMeshBuilder, NavObstacles, NavPoly, NavQuery};

/// An axis-aligned horizontal quad `[x0, x1] x [z0, z1]` at height `y`, as two triangles.
fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// A quad over `[x0, x1] x [z0, z1]` rising linearly along X from `y0` to `y1`.
fn ramp_x(x0: f32, x1: f32, z0: f32, z1: f32, y0: f32, y1: f32) -> [[Vec3; 3]; 2] {
    [
        [
            Vec3::new(x0, y0, z0),
            Vec3::new(x1, y1, z0),
            Vec3::new(x1, y1, z1),
        ],
        [
            Vec3::new(x0, y0, z0),
            Vec3::new(x1, y1, z1),
            Vec3::new(x0, y0, z1),
        ],
    ]
}

/// A vertical wall in the plane `z = z`, from `x0` to `x1`, between heights `y0` and `y1`.
fn wall_z(x0: f32, x1: f32, z: f32, y0: f32, y1: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y0, z), Vec3::new(x1, y0, z), Vec3::new(x1, y1, z)],
        [Vec3::new(x0, y0, z), Vec3::new(x1, y1, z), Vec3::new(x0, y1, z)],
    ]
}

/// Flat ground at y = 0 over `[-12, 12]^2` with a closed tunnel under it: floor at y = -4 and a
/// ceiling at y = -1 over `x in [-8, 8], z in [-2, 2]` (1 m of cover), side walls included.
fn sealed_tunnel() -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    tris.extend(quad(-12.0, 12.0, -12.0, 12.0, 0.0));
    tris.extend(quad(-8.0, 8.0, -2.0, 2.0, -4.0));
    tris.extend(quad(-8.0, 8.0, -2.0, 2.0, -1.0));
    tris.extend(wall_z(-8.0, 8.0, -2.0, -4.0, -1.0));
    tris.extend(wall_z(-8.0, 8.0, 2.0, -4.0, -1.0));
    tris
}

/// Ground at y = 0 over `[-20, 20]^2` with an open trench `x in [4, 12], z in [-2, 2]` holding a
/// ramp from the tunnel floor (y = -4 at x = 4) up to the ground (y = 0 at x = 12). The tunnel
/// runs under the ground from x = -8 to the ramp's foot: floor y = -4, ceiling y = -1.
fn tunnel_with_a_ramp_mouth() -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    // The ground, leaving the trench open.
    tris.extend(quad(-20.0, 20.0, -20.0, -2.0, 0.0));
    tris.extend(quad(-20.0, 20.0, 2.0, 20.0, 0.0));
    tris.extend(quad(-20.0, 4.0, -2.0, 2.0, 0.0));
    tris.extend(quad(12.0, 20.0, -2.0, 2.0, 0.0));
    // The ramp in the trench.
    tris.extend(ramp_x(4.0, 12.0, -2.0, 2.0, -4.0, 0.0));
    // The tunnel: floor, ceiling and side walls.
    tris.extend(quad(-8.0, 4.0, -2.0, 2.0, -4.0));
    tris.extend(quad(-8.0, 4.0, -2.0, 2.0, -1.0));
    tris.extend(wall_z(-8.0, 12.0, -2.0, -4.0, 0.0));
    tris.extend(wall_z(-8.0, 12.0, 2.0, -4.0, 0.0));
    tris
}

/// Ground at y = 0 over `[-12, 12]^2` and a bridge deck at y = 3 over `x in [-3, 3]`, spanning
/// the whole Z extent (so the deck splits nothing on the ground below).
fn bridge_over_ground() -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    tris.extend(quad(-12.0, 12.0, -12.0, 12.0, 0.0));
    tris.extend(quad(-3.0, 3.0, -12.0, 12.0, 3.0));
    tris
}

/// The polygons whose XZ footprint holds `(x, z)`, with their heights.
fn polys_over(mesh: &NavMesh, x: f32, z: f32) -> Vec<(usize, f32)> {
    mesh.polys
        .iter()
        .enumerate()
        .filter(|(_, p)| p.contains_xz(x, z))
        .map(|(i, p)| (i, p.height()))
        .collect()
}

fn has_poly_at(mesh: &NavMesh, x: f32, z: f32, y: f32) -> bool {
    polys_over(mesh, x, z).iter().any(|&(_, h)| (h - y).abs() < 1e-3)
}

/// A navmesh keeping up to four walkable layers.
fn layered(tris: &[[Vec3; 3]]) -> Result<NavMesh, NavBuildError> {
    NavMeshBuilder::default().build_layered(tris, &[], &[], 4)
}

fn query(mesh: &NavMesh) -> NavQuery<'_> {
    NavQuery {
        mesh,
        obstacles: None,
    }
}

/// The acceptance test of W-18: a floor under another walkable surface is walkable too. On the
/// base (top span only) the ground hides the tunnel and the tunnel floor gets no polygon.
#[test]
fn a_tunnel_floor_under_ground_gets_navmesh() {
    let mesh = layered(&sealed_tunnel()).expect("the level builds");
    // The ground over the tunnel stays.
    assert!(
        has_poly_at(&mesh, 0.0, 0.0, 0.0),
        "{:?}",
        polys_over(&mesh, 0.0, 0.0)
    );
    // The tunnel floor has polygons along its length.
    for x in [-6.0, -3.0, 0.0, 3.0, 6.0] {
        assert!(
            has_poly_at(&mesh, x, 0.0, -4.0),
            "no tunnel floor polygon at x = {x}: {:?}",
            polys_over(&mesh, x, 0.0)
        );
    }
    // The 1 m of cover between the ceiling and the ground is not walkable: nothing at y = -1.
    assert!(!has_poly_at(&mesh, 0.0, 0.0, -1.0));
    // A point in the tunnel locates on the tunnel floor, not on the ground above it.
    let (poly, point) = query(&mesh)
        .find_nearest(Vec3::new(0.0, -3.8, 0.0), Vec3::new(1.0, 1.0, 1.0))
        .expect("the tunnel floor is located");
    assert!((mesh.polys[poly as usize].height() + 4.0).abs() < 1e-3);
    assert!((point.y + 4.0).abs() < 1e-3, "{point:?}");
    // The tunnel side walls erode the floor by the agent radius: the floor polygons stay inside.
    let radius = mesh.config.agent.radius;
    for p in mesh.polys.iter().filter(|p| (p.height() + 4.0).abs() < 1e-3) {
        assert!(
            p.min.z >= -2.0 + radius - 0.26 && p.max.z <= 2.0 - radius + 0.26,
            "{p:?}"
        );
    }
}

/// An agent can walk from the ground, down the ramp and into the tunnel under that ground: the
/// tunnel floor is linked to the ramp although the ground covers it.
#[test]
fn a_path_leads_from_the_ground_into_a_covered_tunnel() {
    let mesh = layered(&tunnel_with_a_ramp_mouth()).expect("the level builds");
    assert!(has_poly_at(&mesh, -4.0, 0.0, -4.0));
    assert!(has_poly_at(&mesh, -4.0, 0.0, 0.0));
    let path = query(&mesh)
        .find_path(
            Vec3::new(16.0, 0.0, 0.0),
            Vec3::new(-4.0, -4.0, 0.0),
            Vec3::new(1.0, 0.5, 1.0),
        )
        .expect("a path into the tunnel");
    let first = path.waypoints.first().expect("waypoints");
    let last = path.waypoints.last().expect("waypoints");
    assert!(first.y.abs() < 1e-3, "starts on the ground: {first:?}");
    assert!((last.y + 4.0).abs() < 1e-3, "ends on the tunnel floor: {last:?}");
    // Every polygon on the corridor under the covered stretch is a tunnel polygon (none is the
    // ground above it).
    for &id in &path.polys {
        let poly = &mesh.polys[id as usize];
        if poly.max.x <= 4.0 && poly.min.z >= -2.0 && poly.max.z <= 2.0 {
            assert!((poly.height() + 4.0).abs() < 1e-3, "{poly:?}");
        }
    }
    // The path never jumps: consecutive waypoints differ in height by no more than the slope
    // allows over their horizontal distance.
    for pair in path.waypoints.windows(2) {
        let dx = ((pair[1].x - pair[0].x).powi(2) + (pair[1].z - pair[0].z).powi(2)).sqrt();
        assert!(
            (pair[1].y - pair[0].y).abs() <= dx + 0.6,
            "jump between {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }
}

/// The ground under a bridge is walkable and joins the ground beside the bridge, so an agent can
/// walk under the deck.
#[test]
fn the_ground_under_a_bridge_joins_the_ground_beside_it() {
    let mesh = layered(&bridge_over_ground()).expect("the level builds");
    assert!(has_poly_at(&mesh, 0.0, 0.0, 3.0), "the deck stays");
    assert!(has_poly_at(&mesh, 0.0, 0.0, 0.0), "the ground under the deck");
    let path = query(&mesh)
        .find_path(
            Vec3::new(-8.0, 0.0, 0.0),
            Vec3::new(8.0, 0.0, 0.0),
            Vec3::new(1.0, 0.5, 1.0),
        )
        .expect("a path under the bridge");
    for p in &path.waypoints {
        assert!(p.y.abs() < 1e-3, "the path stays on the ground: {p:?}");
    }
    // Straight under the deck: no detour around it.
    assert!(path.length < 16.0 + 1e-2, "length {}", path.length);
    assert_eq!(mesh.region_count, 2, "the ground is one region, the deck another");
}

/// A surface too low to stand under (less than the agent's height) stays unwalkable, as before.
#[test]
fn a_floor_under_a_low_slab_stays_unwalkable() {
    let mut tris = Vec::new();
    tris.extend(quad(-12.0, 12.0, -12.0, 12.0, 0.0));
    tris.extend(quad(-3.0, 3.0, -3.0, 3.0, 1.2));
    let mesh = layered(&tris).expect("builds");
    assert!(has_poly_at(&mesh, 0.0, 0.0, 1.2));
    assert!(
        !has_poly_at(&mesh, 0.0, 0.0, 0.0),
        "{:?}",
        polys_over(&mesh, 0.0, 0.0)
    );
}

/// Three stacked floors (0, 3 and 6 m) over the same footprint each get polygons.
#[test]
fn three_stacked_floors_each_get_polygons() {
    let mut tris = Vec::new();
    for y in [0.0, 3.0, 6.0] {
        tris.extend(quad(-6.0, 6.0, -6.0, 6.0, y));
    }
    let mesh = layered(&tris).expect("builds");
    for y in [0.0, 3.0, 6.0] {
        assert!(
            has_poly_at(&mesh, 0.0, 0.0, y),
            "{:?}",
            polys_over(&mesh, 0.0, 0.0)
        );
    }
    assert_eq!(mesh.region_count, 3);
}

/// The layered build is deterministic: the same level gives the same polygons twice, and the
/// triangle order does not matter.
#[test]
fn a_layered_build_does_not_depend_on_the_triangle_order() {
    let tris = tunnel_with_a_ramp_mouth();
    let a = layered(&tris).expect("builds");
    let b = layered(&tris).expect("builds");
    let mut reversed = tris.clone();
    reversed.reverse();
    let c = layered(&reversed).expect("builds");
    let polys = |m: &NavMesh| -> Vec<NavPoly> { m.polys.clone() };
    assert_eq!(polys(&a), polys(&b));
    assert_eq!(polys(&a), polys(&c));
    assert_eq!(a.region_count, c.region_count);
}

/// A dynamic obstacle carves the layer it stands on: a box across the covered tunnel blocks the way
/// in, while the same box's footprint on the ground above (out of its height range) stays walkable.
#[test]
fn an_obstacle_in_a_covered_tunnel_carves_the_tunnel_not_the_ground() {
    let mesh = layered(&tunnel_with_a_ramp_mouth()).expect("the level builds");
    assert!(mesh.layer_count() > 1);
    let mut obstacles = NavObstacles::default();
    obstacles.add_box(Vec3::new(-1.0, -4.5, -3.0), Vec3::new(1.0, -2.0, 3.0));
    let blocked = NavQuery {
        mesh: &mesh,
        obstacles: Some(&obstacles),
    };
    let extents = Vec3::new(1.0, 0.5, 1.0);
    // Into the tunnel past the box: no way round it under the ground.
    assert!(
        blocked
            .find_path(Vec3::new(16.0, 0.0, 0.0), Vec3::new(-6.0, -4.0, 0.0), extents)
            .is_none()
    );
    // Short of the box: still reachable.
    assert!(
        blocked
            .find_path(Vec3::new(16.0, 0.0, 0.0), Vec3::new(2.0, -4.0, 0.0), extents)
            .is_some()
    );
    // The ground over the box keeps its polygons.
    let path = blocked
        .find_path(Vec3::new(0.0, 0.0, 8.0), Vec3::new(0.0, 0.0, -8.0), extents)
        .expect("a path over the tunnel");
    assert!(path.waypoints.iter().all(|p| p.y.abs() < 1e-3));
    // Without the obstacle the far end of the tunnel is reachable.
    assert!(
        query(&mesh)
            .find_path(Vec3::new(16.0, 0.0, 0.0), Vec3::new(-6.0, -4.0, 0.0), extents)
            .is_some()
    );
}

/// Layers are opt-in: `build` keeps the top surface only, as `build_layered` with one layer does.
#[test]
fn the_default_builder_keeps_the_top_surface_only() {
    let tris = sealed_tunnel();
    let default = NavMeshBuilder::default().build(&tris).expect("builds");
    let one = NavMeshBuilder::default()
        .build_layered(&tris, &[], &[], 1)
        .expect("builds");
    assert_eq!(default.layer_count(), 1);
    assert_eq!(default.polys, one.polys);
    assert!(!has_poly_at(&default, 0.0, 0.0, -4.0));
    // Each layer past the cap is dropped: two layers keep the ground and the tunnel.
    let two = NavMeshBuilder::default()
        .build_layered(&tris, &[], &[], 2)
        .expect("builds");
    assert_eq!(two.layer_count(), 2);
    assert!(has_poly_at(&two, 0.0, 0.0, -4.0));
    // Three stacked floors under a cap of two: the top and one more.
    let mut floors = Vec::new();
    for y in [0.0, 3.0, 6.0] {
        floors.extend(quad(-6.0, 6.0, -6.0, 6.0, y));
    }
    let capped = NavMeshBuilder::default()
        .build_layered(&floors, &[], &[], 2)
        .expect("builds");
    assert_eq!(capped.layer_count(), 2);
}

/// The inside of a closed box taller than the agent never joins the floor around it: its walls
/// close the floor under its lid, so a path goes round the box, not through it.
#[test]
fn a_closed_box_stays_an_obstacle_with_layers() {
    let mut tris = Vec::new();
    tris.extend(quad(-12.0, 12.0, -12.0, 12.0, 0.0));
    // A 3 m box over [-3, 3]^2: lid, bottom and four walls.
    tris.extend(quad(-3.0, 3.0, -3.0, 3.0, 3.0));
    tris.extend(quad(-3.0, 3.0, -3.0, 3.0, 0.0));
    tris.extend(wall_z(-3.0, 3.0, -3.0, 0.0, 3.0));
    tris.extend(wall_z(-3.0, 3.0, 3.0, 0.0, 3.0));
    for x in [-3.0f32, 3.0] {
        tris.push([
            Vec3::new(x, 0.0, -3.0),
            Vec3::new(x, 0.0, 3.0),
            Vec3::new(x, 3.0, 3.0),
        ]);
        tris.push([
            Vec3::new(x, 0.0, -3.0),
            Vec3::new(x, 3.0, 3.0),
            Vec3::new(x, 3.0, -3.0),
        ]);
    }
    let mesh = layered(&tris).expect("builds");
    assert!(has_poly_at(&mesh, 0.0, 0.0, 3.0), "the lid stays walkable");
    let path = query(&mesh)
        .find_path(
            Vec3::new(-8.0, 0.0, 0.0),
            Vec3::new(8.0, 0.0, 0.0),
            Vec3::new(1.0, 0.5, 1.0),
        )
        .expect("a path round the box");
    // A path may run along the box's sides (1e-3 for rounding, as in the property tests).
    for pair in path.waypoints.windows(2) {
        for s in 0..=40 {
            let p = pair[0].lerp(pair[1], s as f32 / 40.0);
            assert!(
                !(p.x > -3.0 + 1e-3 && p.x < 3.0 - 1e-3 && p.z > -3.0 + 1e-3 && p.z < 3.0 - 1e-3),
                "{p:?} goes through the box"
            );
        }
    }
    assert!(path.length > 16.5, "a detour, length {}", path.length);

    // Nothing under the lid leads out: the walls close it.
    let extents = Vec3::new(1.0, 0.5, 1.0);
    let under = Vec3::new(0.0, 0.0, 0.0);
    assert!(
        query(&mesh)
            .find_path(under, Vec3::new(-8.0, 0.0, 0.0), extents)
            .is_none()
    );

    // Control: the same lid and bottom without walls is a 3 m table; an agent under it walks out.
    let table: Vec<[Vec3; 3]> = tris[..6].to_vec();
    let mesh = layered(&table).expect("builds");
    let path = query(&mesh)
        .find_path(under, Vec3::new(-8.0, 0.0, 0.0), extents)
        .expect("a path out from under the table");
    assert!(
        path.waypoints.iter().all(|p| p.y.abs() < 1e-3),
        "{:?}",
        path.waypoints
    );
}
