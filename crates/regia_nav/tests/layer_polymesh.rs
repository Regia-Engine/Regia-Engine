// SPDX-License-Identifier: MPL-2.0
//! Layered polymesh acceptance (plan row W-19): one polymesh per walkable layer, pilot-only
//! climbs, a hull's own erosion, passage costs, and a level cap that fails loudly.

use regia_core::math::Vec3;
use regia_nav::{
    FLANK_AREA_COST, NAV_AREA_DEFAULT, NAV_AREA_FLANK, NAV_AREA_TUNNEL, NavAgentParams, NavAreaCosts,
    NavBuildError, NavLayerLink, NavMesh, NavMeshBuildParams, NavMeshBuilder, NavPath, NavQuery,
    NavQueryFilter, TUNNEL_AREA_COST, pilot_layer_links,
};
use regia_world_geom::compat::mesh::synthetic_four_decks;

fn floor_quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

fn two_floors(upper: f32) -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    tris.extend(floor_quad(-8.0, 8.0, -8.0, 8.0, 0.0));
    tris.extend(floor_quad(-8.0, 8.0, -8.0, 8.0, upper));
    tris
}

fn mesh_for(agent: &NavAgentParams, tris: &[[Vec3; 3]], links: &[NavLayerLink]) -> NavMesh {
    let params = NavMeshBuildParams {
        agent: agent.clone(),
        ..Default::default()
    };
    NavMeshBuilder::new(params)
        .build_layered(tris, &[], &[], 8)
        .expect("the floors are walkable")
        .with_off_mesh_links(pilot_layer_links(agent, links))
}

fn path_between(mesh: &NavMesh, from: Vec3, to: Vec3, filter: Option<&NavQueryFilter>) -> Option<NavPath> {
    let query = NavQuery {
        mesh,
        obstacles: None,
    };
    let extents = Vec3::new(1.5, 0.6, 1.5);
    match filter {
        Some(filter) => query.find_path_filtered(from, to, extents, filter),
        None => query.find_path(from, to, extents),
    }
}

/// The fixture's collision triangles are the cooked mesh the bake reads.
fn cooked_four_decks() -> Vec<[Vec3; 3]> {
    synthetic_four_decks()
        .triangles
        .into_iter()
        .map(|tri| {
            [
                Vec3::new(tri.a[0], tri.a[1], tri.a[2]),
                Vec3::new(tri.b[0], tri.b[1], tri.b[2]),
                Vec3::new(tri.c[0], tri.c[1], tri.c[2]),
            ]
        })
        .collect()
}

#[test]
fn four_stacked_floors_each_get_polys() {
    let params = NavMeshBuildParams {
        agent: NavAgentParams::pilot(),
        ..Default::default()
    };
    let mesh = NavMeshBuilder::new(params)
        .build_layered(&cooked_four_decks(), &[], &[], 8)
        .expect("the four-deck fixture bakes");
    assert!(mesh.layer_count() >= 4, "layers {}", mesh.layer_count());
    assert!(mesh.contours.len() >= 4, "contours {}", mesh.contours.len());

    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    for y in [0.0, -4.0, -8.0, -12.0] {
        let (_poly, on) = query
            .find_nearest(Vec3::new(0.0, y, 0.0), Vec3::new(1.5, 0.75, 1.5))
            .unwrap_or_else(|| panic!("no polygon on the deck at y = {y}"));
        assert!((on.y - y).abs() < 0.25, "deck {y} snapped to {}", on.y);
    }

    // The east ramp from y = 0 to y = -4. The part under the deck above stays walkable once the
    // headroom clears the pilot, around x = 7, y = -3.
    let (_poly, ramp) = query
        .find_nearest(Vec3::new(7.0, -3.0, 0.0), Vec3::new(0.75, 0.6, 0.75))
        .unwrap_or_else(|| panic!("no polygon on the ramp"));
    assert!(
        (ramp.y - (-3.0)).abs() < 0.6,
        "ramp snapped to {}, not the slope",
        ramp.y
    );
    assert!(ramp.y < -1.0 && ramp.y > -3.8, "ramp height {}", ramp.y);
}

#[test]
fn ladder_link_routes_a_pilot() {
    let pilot = NavAgentParams::pilot();
    let hover = NavAgentParams::hover(2.5);
    let ladder = [NavLayerLink::ladder(
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.0, 15.0, 0.0),
    )];
    let from = Vec3::new(-2.0, 0.0, 0.0);
    let to = Vec3::new(-2.0, 15.0, 0.0);
    let floors = two_floors(15.0);

    assert!(
        path_between(&mesh_for(&pilot, &floors, &[]), from, to, None).is_none(),
        "a 15 m gap is not a step"
    );
    assert!(pilot_layer_links(&hover, &ladder).is_empty());

    let mesh = mesh_for(&pilot, &floors, &ladder);
    assert_eq!(mesh.off_mesh_links.len(), 1);
    assert!(path_between(&mesh, from, to, None).is_some());
    assert!(path_between(&mesh, from, to, Some(&NavQueryFilter::pilot())).is_some());
    assert!(path_between(&mesh, from, to, Some(&NavQueryFilter::hull())).is_none());
    assert!(path_between(&mesh_for(&hover, &floors, &ladder), from, to, None).is_none());
}

#[test]
fn deck_stairs_link_routes_a_pilot() {
    let pilot = NavAgentParams::pilot();
    let hover = NavAgentParams::hover(2.5);
    let stairs = [NavLayerLink::deck_stairs(
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.0, 3.2, 0.0),
    )];
    let from = Vec3::new(-2.0, 0.0, 0.0);
    let to = Vec3::new(-2.0, 3.2, 0.0);
    let floors = two_floors(3.2);

    assert!(path_between(&mesh_for(&pilot, &floors, &[]), from, to, None).is_none());
    let mesh = mesh_for(&pilot, &floors, &stairs);
    assert_eq!(mesh.off_mesh_links[0].kind, regia_nav::NavLinkKind::Climb);
    assert!(path_between(&mesh, from, to, None).is_some());
    assert!(path_between(&mesh, from, to, Some(&NavQueryFilter::hull())).is_none());
    assert!(path_between(&mesh_for(&hover, &floors, &stairs), from, to, None).is_none());
}

fn push_quad(tris: &mut Vec<[Vec3; 3]>, areas: &mut Vec<u8>, quad: [[Vec3; 3]; 2], area: u8) {
    for tri in quad {
        tris.push(tri);
        areas.push(area);
    }
}

fn passage(radius: f32, width: f32) -> Option<NavPath> {
    let half = width * 0.5;
    let mut tris = Vec::new();
    tris.extend(floor_quad(-18.0, -2.0, -14.0, 14.0, 0.0));
    tris.extend(floor_quad(2.0, 18.0, -14.0, 14.0, 0.0));
    tris.extend(floor_quad(-2.0, 2.0, -half, half, 0.0));
    let params = NavMeshBuildParams {
        cell_size: 0.25,
        agent: NavAgentParams::hover(radius),
        ..Default::default()
    };
    let mesh = NavMeshBuilder::new(params)
        .build(&tris)
        .expect("the pads stay walkable");
    NavQuery {
        mesh: &mesh,
        obstacles: None,
    }
    .find_path(
        Vec3::new(-8.0, 0.0, 0.0),
        Vec3::new(8.0, 0.0, 0.0),
        Vec3::splat(3.0),
    )
}

#[test]
fn hull_classes_get_their_own_mesh() {
    for radius in [2.5_f32, 3.6, 4.5] {
        let open = 2.0 * (radius + 1.0);
        assert!(
            passage(radius, open - 1.0).is_none(),
            "radius {radius} routed a passage narrower than {open} m"
        );
        assert!(
            passage(radius, open + 1.5).is_some(),
            "radius {radius} missed a passage wider than {open} m"
        );
    }
}

#[test]
fn max_nav_levels_is_a_setting() {
    let mut tris = Vec::new();
    for level in 0u8..5 {
        tris.extend(floor_quad(-3.0, 3.0, -3.0, 3.0, f32::from(level) * 3.0));
    }
    let builder = NavMeshBuilder::new(NavMeshBuildParams {
        agent: NavAgentParams::pilot(),
        ..Default::default()
    });
    let mesh = builder
        .build_with_nav_level_cap(&tris, &[], &[], 5)
        .expect("five levels fit the setting");
    assert_eq!(mesh.layer_count(), 5);
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    for level in 0u8..5 {
        let y = f32::from(level) * 3.0;
        let (_poly, on) = query
            .find_nearest(Vec3::new(0.0, y, 0.0), Vec3::new(1.0, 0.5, 1.0))
            .unwrap_or_else(|| panic!("missing level at {y}"));
        assert!((on.y - y).abs() < 0.2, "level {y} snapped to {}", on.y);
    }

    let dropped = builder
        .build_layered(&tris, &[], &[], 4)
        .expect("drop mode still bakes");
    assert_eq!(dropped.layer_count(), 4);

    match builder.build_with_nav_level_cap(&tris, &[], &[], 4) {
        Err(NavBuildError::InvalidInput(reason)) => assert_eq!(
            reason,
            "max_nav_levels exceeded: a column has more walkable levels than the setting"
        ),
        other => panic!("expected a loud level-cap error, got {other:?}"),
    }
}

#[test]
fn tunnel_costs_less_than_open_ground() {
    let costs = NavAreaCosts::passages();
    assert_eq!(costs.get(NAV_AREA_TUNNEL), TUNNEL_AREA_COST);
    assert_eq!(costs.get(NAV_AREA_FLANK), FLANK_AREA_COST);
    assert_eq!(costs.get(NAV_AREA_DEFAULT), 1.0);

    let mut tris = Vec::new();
    let mut areas = Vec::new();
    push_quad(
        &mut tris,
        &mut areas,
        floor_quad(-8.0, -2.0, -8.0, 8.0, 0.0),
        NAV_AREA_DEFAULT,
    );
    push_quad(
        &mut tris,
        &mut areas,
        floor_quad(2.0, 8.0, -8.0, 8.0, 0.0),
        NAV_AREA_DEFAULT,
    );
    push_quad(
        &mut tris,
        &mut areas,
        floor_quad(-2.0, 2.0, -6.0, -2.0, 0.0),
        NAV_AREA_DEFAULT,
    );
    push_quad(
        &mut tris,
        &mut areas,
        floor_quad(-2.0, 2.0, 2.0, 6.0, 0.0),
        NAV_AREA_TUNNEL,
    );

    let mut params = NavMeshBuildParams::default();
    params.agent.radius = 0.3;
    let mut mesh = NavMeshBuilder::new(params)
        .build_with_triangle_areas(&tris, &areas, &[])
        .expect("both corridors bake");
    mesh.set_area_costs(costs);
    let path = NavQuery {
        mesh: &mesh,
        obstacles: None,
    }
    .find_path(
        Vec3::new(-5.0, 0.0, 0.0),
        Vec3::new(5.0, 0.0, 0.0),
        Vec3::splat(2.0),
    )
    .expect("a corridor is open");
    assert!(
        path.waypoints.iter().any(|point| point.z > 1.0),
        "the path did not take the tunnel: {:?}",
        path.waypoints
    );
    assert!(
        path.waypoints.iter().all(|point| point.z > -1.0),
        "the path took the open corridor: {:?}",
        path.waypoints
    );
}

#[test]
fn locate_body_retries_below_the_origin() {
    let tris = floor_quad(-4.0, 4.0, -4.0, 4.0, 0.0).to_vec();
    let mesh = NavMeshBuilder::default().build(&tris).expect("floor");
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let extents = Vec3::new(1.0, 0.5, 1.0);
    let body = Vec3::new(0.0, 1.5, 0.0);
    assert!(query.find_nearest(body, extents).is_none());
    let (_poly, on) = query
        .locate_body(body, extents)
        .expect("the third drop reaches the floor");
    assert!(on.y.abs() < 0.25, "snapped to {}", on.y);
}
