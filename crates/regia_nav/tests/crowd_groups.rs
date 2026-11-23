// SPDX-License-Identifier: MPL-2.0
//! Formation followers on a crowd (ADR-8358). Fixed steps, no wall clock.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, CrowdGroup, NavAgentParams, NavCrowd, NavCrowdConfig, NavMesh, NavMeshBuildParams,
    NavMeshBuilder, formation_heading, formation_offset_world,
};

fn params(radius: f32, speed: f32, accel: f32) -> CrowdAgentParams {
    let mut params = CrowdAgentParams::new(radius, 1.6, speed, accel);
    params.avoidance = false;
    params.separation = false;
    params.anticipate_turns = false;
    params
}

fn flat_mesh(half: f32) -> NavMesh {
    let tris = vec![
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, -half),
            Vec3::new(half, 0.0, half),
        ],
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, half),
            Vec3::new(-half, 0.0, half),
        ],
    ];
    let build = NavMeshBuildParams {
        cell_size: 0.5,
        cell_height: 0.2,
        border_size: 0.5,
        agent: NavAgentParams {
            radius: 0.05,
            height: 1.6,
            max_climb: 0.4,
            max_slope: 45f32.to_radians(),
        },
    };
    NavMeshBuilder::new(build).build(&tris).expect("flat mesh")
}

fn quad(x0: f32, x1: f32, z0: f32, z1: f32, out: &mut Vec<[Vec3; 3]>) {
    let a = Vec3::new(x0, 0.0, z0);
    let b = Vec3::new(x1, 0.0, z0);
    let c = Vec3::new(x1, 0.0, z1);
    let d = Vec3::new(x0, 0.0, z1);
    out.push([a, b, c]);
    out.push([a, c, d]);
}

fn door_mesh() -> NavMesh {
    let mut tris = Vec::new();
    quad(-16.0, 0.0, -12.0, 12.0, &mut tris);
    quad(0.0, 6.0, -0.5, 0.5, &mut tris);
    quad(6.0, 80.0, -12.0, 12.0, &mut tris);
    let build = NavMeshBuildParams {
        cell_size: 0.1,
        cell_height: 0.1,
        border_size: 0.0,
        agent: NavAgentParams {
            radius: 0.05,
            height: 1.6,
            max_climb: 0.4,
            max_slope: 45f32.to_radians(),
        },
    };
    NavMeshBuilder::new(build).build(&tris).expect("door mesh")
}

fn on_mesh(mesh: &NavMesh, point: Vec3) -> bool {
    mesh.polys.iter().any(|poly| {
        (poly.height() - point.y).abs() < 1.5 && poly.distance_sq_xz(point.x, point.z) <= 0.05 * 0.05
    })
}

fn crowd() -> NavCrowd {
    NavCrowd::new(NavCrowdConfig::default()).expect("config")
}

fn xz(a: Vec3, b: Vec3) -> f32 {
    let dx = a.x - b.x;
    let dz = a.z - b.z;
    (dx * dx + dz * dz).sqrt()
}

/// Three followers stay on a 4 m line behind a leader for 600 steps on open ground.
#[test]
fn a_group_keeps_its_shape_on_open_ground() {
    let mesh = flat_mesh(60.0);
    let mut crowd = crowd();
    let leader = crowd
        .add_agent(&mesh, Vec3::ZERO, params(0.25, 3.0, 8.0))
        .expect("leader");
    crowd
        .set_target(&mesh, leader, Vec3::new(50.0, 0.0, 0.0))
        .expect("target");
    let offsets = [
        Vec3::new(-2.0, 0.0, 2.0),
        Vec3::new(0.0, 0.0, 2.0),
        Vec3::new(2.0, 0.0, 2.0),
    ];
    let heading = formation_heading(Vec3::ZERO, Vec3::ZERO, Some(Vec3::new(50.0, 0.0, 0.0)));
    let mut followers = Vec::new();
    for offset in offsets {
        let at = formation_offset_world(Vec3::ZERO, heading, offset);
        let id = crowd
            .add_agent(&mesh, at, params(0.25, 6.0, 20.0))
            .expect("follower");
        crowd
            .set_agent_group(id, CrowdGroup { leader, offset })
            .expect("group");
        followers.push((id, offset));
    }
    crowd
        .set_agent_group(
            leader,
            CrowdGroup {
                leader,
                offset: Vec3::ZERO,
            },
        )
        .expect("leader group");
    let mut worst = 0.0f32;
    for _ in 0..600 {
        crowd.step(&mesh);
        let lead = crowd.agent(leader).expect("leader");
        let face = formation_heading(lead.velocity(), lead.position(), lead.corners().first().copied());
        for (id, offset) in &followers {
            let agent = crowd.agent(*id).expect("follower");
            assert!(
                on_mesh(&mesh, agent.position()),
                "left the mesh at {:?}",
                agent.position()
            );
            assert!(!agent.column(), "open ground forced a column");
            let wanted = formation_offset_world(lead.position(), face, *offset);
            worst = worst.max(xz(agent.position(), wanted));
        }
    }
    assert!(worst < 0.75, "max slot error {worst} m over 600 steps");
}

/// A shape wider than a doorway columns through it, stays on the mesh, and re-forms after.
#[test]
fn a_group_columns_through_a_door_and_reforms() {
    let mesh = door_mesh();
    assert!(
        on_mesh(&mesh, Vec3::new(3.0, 0.0, 0.0)),
        "the doorway should be walkable"
    );
    assert!(
        !on_mesh(&mesh, Vec3::new(3.0, 0.0, 3.0)),
        "beside the doorway should be off the mesh"
    );
    let mut crowd = crowd();
    let leader = crowd
        .add_agent(&mesh, Vec3::new(-8.0, 0.0, 0.0), params(0.2, 2.5, 12.0))
        .expect("leader");
    crowd
        .set_target(&mesh, leader, Vec3::new(70.0, 0.0, 0.0))
        .expect("target");
    let offsets = [Vec3::new(-2.0, 0.0, 2.0), Vec3::new(2.0, 0.0, 2.0)];
    let heading = formation_heading(
        Vec3::ZERO,
        Vec3::new(-8.0, 0.0, 0.0),
        Some(Vec3::new(70.0, 0.0, 0.0)),
    );
    let mut followers = Vec::new();
    for offset in offsets {
        let at = formation_offset_world(Vec3::new(-8.0, 0.0, 0.0), heading, offset);
        assert!(on_mesh(&mesh, at), "start slot off the pad: {at:?}");
        let id = crowd
            .add_agent(&mesh, at, params(0.2, 6.0, 24.0))
            .expect("follower");
        crowd
            .set_agent_group(id, CrowdGroup { leader, offset })
            .expect("group");
        followers.push((id, offset));
    }
    let mut saw_column = false;
    let mut exit_step = None;
    for step in 0..900 {
        crowd.step(&mesh);
        let lead = crowd.agent(leader).expect("leader");
        assert!(on_mesh(&mesh, lead.position()), "leader left the mesh");
        for (id, _) in &followers {
            let agent = crowd.agent(*id).expect("follower");
            assert!(
                on_mesh(&mesh, agent.position()),
                "follower left the mesh at {:?}",
                agent.position()
            );
        }
        let x = lead.position().x;
        if (0.5..5.5).contains(&x)
            && followers
                .iter()
                .any(|(id, _)| crowd.agent_column(*id).expect("column"))
        {
            saw_column = true;
        }
        if exit_step.is_none() && x > 8.0 {
            exit_step = Some(step);
            break;
        }
    }
    assert!(saw_column, "the doorway never put a follower in a column");
    let exit_step = exit_step.expect("the leader did not clear the doorway");
    let mut worst = f32::MAX;
    let mut best = f32::MAX;
    let mut best_step = 0usize;
    for step in 0..480 {
        crowd.step(&mesh);
        let lead = crowd.agent(leader).expect("leader");
        let face = formation_heading(lead.velocity(), lead.position(), lead.corners().first().copied());
        let mut step_worst = 0.0f32;
        for (id, offset) in &followers {
            let agent = crowd.agent(*id).expect("follower");
            assert!(
                on_mesh(&mesh, agent.position()),
                "follower left the mesh after the door"
            );
            let wanted = formation_offset_world(lead.position(), face, *offset);
            step_worst = step_worst.max(xz(agent.position(), wanted));
        }
        if step_worst < best {
            best = step_worst;
            best_step = step;
        }
        worst = step_worst;
    }
    let lead = crowd.agent(leader).expect("leader");
    let columns: Vec<bool> = followers
        .iter()
        .map(|(id, _)| crowd.agent_column(*id).expect("column"))
        .collect();
    assert!(
        worst < 1.0,
        "shape error {worst} m (best {best} at +{best_step}), exit step {exit_step}, lead {:?} state {:?} vel {:?} columns {columns:?}",
        lead.position(),
        lead.state(),
        lead.velocity(),
    );
}
