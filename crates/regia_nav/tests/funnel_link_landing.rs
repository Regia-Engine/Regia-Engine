// SPDX-License-Identifier: MPL-2.0
//! Finding 13 of sprint-deep-zai-nav-ai (PLAUSIBLE): after an off-mesh link, `find_path`
//! restarts the funnel at the link's authored landing point, which the snap radius lets lie off the
//! landing polygon. A funnel started off its first polygon could orient a portal backwards and
//! bend at the wrong portal end, so the path would leave the corridor. This sweep lands a link at
//! every point of a grid around an L-shaped corridor and checks that every leg after the first
//! corner stays inside the corridor, that the first corner is a point of the corridor, and that
//! the path from the authored landing is no longer than stepping onto the landing polygon and
//! walking from there (a wrong-end bend would be a detour). On the v0.5.0 candidate: 618 landings,
//! 493 off the landing polygon, no violation; 4 paths differ from the clamped walk by seeing past a
//! corner (shorter, still in the corridor). The finding did not reproduce, so `find_path` is
//! unchanged. Fixed inputs only.

use regia_core::math::Vec3;
use regia_nav::{NavMesh, NavMeshBuildParams, NavMeshBuilder, NavQuery, OffMeshLink};

fn rect(x0: f32, z0: f32, x1: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

/// The take-off floor west of the gap, and an L of 1 m wide strips: east along z in [0, 1], then
/// south along x in [3, 4]. A zero agent radius keeps the strips their authored width.
fn base_mesh() -> NavMesh {
    let mut tris: Vec<[Vec3; 3]> = Vec::new();
    tris.extend(rect(-8.0, -4.0, -3.0, 4.0));
    tris.extend(rect(0.0, 0.0, 4.0, 1.0));
    tris.extend(rect(3.0, -5.0, 4.0, 0.0));
    let mut params = NavMeshBuildParams {
        cell_size: 0.25,
        border_size: 0.5,
        ..Default::default()
    };
    params.agent.radius = 0.0;
    NavMeshBuilder::new(params)
        .build(&tris)
        .expect("the floors build")
}

/// Whether `p` lies on one of `polys` (XZ, boundary included with a small tolerance). The
/// polygons of a grid mesh are axis-aligned rectangles, so their bounds are their shape.
fn on_polys(mesh: &NavMesh, polys: &[u32], p: Vec3) -> bool {
    const TOL: f32 = 1e-3;
    polys.iter().any(|&i| {
        mesh.polys.get(i as usize).is_some_and(|poly| {
            p.x >= poly.min.x - TOL
                && p.x <= poly.max.x + TOL
                && p.z >= poly.min.z - TOL
                && p.z <= poly.max.z + TOL
        })
    })
}

#[test]
fn the_path_after_an_off_polygon_link_landing_stays_in_its_corridor() {
    let base = base_mesh();
    let start = Vec3::new(-6.0, 0.0, 0.5);
    let goal = Vec3::new(3.5, 0.0, -4.5);
    let extents = Vec3::new(1.0, 2.0, 1.0);
    let mut cases = 0;
    let mut off_polygon = 0;
    let mut violations = Vec::new();
    let mut differs = Vec::new();
    for zi in 0..=20 {
        for xi in 0..=36 {
            let to = Vec3::new(-2.0 + xi as f32 * 0.25, 0.0, -2.0 + zi as f32 * 0.25);
            let link = OffMeshLink::new(Vec3::new(-3.5, 0.0, 0.5), to);
            let mesh = base.clone().with_off_mesh_links(vec![link]);
            let query = NavQuery {
                mesh: &mesh,
                obstacles: None,
            };
            let Some(path) = query.find_path(start, goal, extents) else {
                continue;
            };
            let Some(landing) = path.waypoints.iter().position(|w| *w == to) else {
                continue;
            };
            cases += 1;
            // The corridor after the link: the path's polygons past the take-off floor.
            let after: Vec<u32> = path
                .polys
                .iter()
                .copied()
                .filter(|&p| mesh.polys[p as usize].max.x > -2.0)
                .collect();
            if !on_polys(&mesh, &after, to) {
                off_polygon += 1;
            }
            let rest = &path.waypoints[landing + 1..];
            // What an agent standing at the landing would walk (the crowd clamps the landing onto
            // the landing polygon before it walks on). A funnel that bent at the wrong end of a
            // portal would make the path from the authored landing longer than stepping onto the
            // polygon and walking from there: that is a detour. A different but no longer path
            // (the authored point sees past a corner the clamped point must turn) is counted only.
            if let Some(&landing_poly) = after.first() {
                let clamped = mesh.polys[landing_poly as usize].closest_point_xz(to.x, to.z);
                if let Some(walk) = query.find_path(clamped, goal, extents)
                    && walk.polys.first() == Some(&landing_poly)
                    && walk.waypoints.get(1..) != Some(rest)
                {
                    let mut from_to = 0.0;
                    let mut prev = to;
                    for &w in rest {
                        from_to += (w - prev).length();
                        prev = w;
                    }
                    let bound = (clamped - to).length() + walk.length + 1e-3;
                    if from_to > bound {
                        violations.push(format!(
                            "to {to}: a detour of {from_to} m past {bound} m: after the landing {rest:?}, from \
                             the clamped landing {:?}",
                            walk.waypoints
                        ));
                    }
                    differs.push(format!(
                        "to {to} (clamped {clamped}): after the landing {rest:?}, from the landing {:?}",
                        walk.waypoints
                    ));
                }
            }
            if let Some(&first) = rest.first()
                && !on_polys(&mesh, &after, first)
            {
                violations.push(format!("to {to}: first corner {first} is off the corridor"));
            }
            for pair in rest.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                for s in 0..=40 {
                    let p = a + (b - a) * (s as f32 / 40.0);
                    if !on_polys(&mesh, &after, p) {
                        violations.push(format!(
                            "to {to}: leg {a} -> {b} leaves the corridor at {p} (waypoints {:?})",
                            path.waypoints
                        ));
                        break;
                    }
                }
            }
        }
    }
    eprintln!(
        "finding 13 sweep: {cases} landings, {off_polygon} off the landing polygon, {} violations, {} differ \
         from a walk from the clamped landing",
        violations.len(),
        differs.len()
    );
    for d in differs.iter().take(6) {
        eprintln!("  differs: {d}");
    }
    assert!(cases > 100, "the sweep found paths: {cases}");
    assert!(off_polygon > 20, "the sweep lands off the polygon: {off_polygon}");
    assert!(
        violations.is_empty(),
        "{} violations, first: {:#?}",
        violations.len(),
        &violations[..violations.len().min(8)]
    );
}
