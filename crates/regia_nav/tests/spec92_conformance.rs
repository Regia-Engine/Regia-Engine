// SPDX-License-Identifier: MPL-2.0
//! Spec 92 conformance (`docs/specs/92-navmesh-offmesh.md`): clauses the earlier link tests did
//! not pin. The generator's default id base, the vertical half of the endpoint snap rule, a
//! Teleport's one-step crossing, an agent removed or a link removed mid-crossing, a neighbour on
//! the landing, the link cap in the file loader, link lists serialized before the v1 fields, and
//! the tile cache's revision on link edits. Fixed steps and deterministic inputs only.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, CrowdAgentState, MAX_NAV_LINKS, NavCrowd, NavCrowdConfig, NavLinkGenParams,
    NavLinkKind, NavLinkPhase, NavLinkSet, NavLinkState, NavMesh, NavMeshBuildParams, NavMeshBuilder,
    NavTileCache, NavTileCacheParams, OffMeshLink, generate_links,
};

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

/// Two 5 x 8 m floors at `x ∈ [-6, -1]` and `x ∈ [1, 6]` with a 2 m gap.
fn islands() -> Vec<[Vec3; 3]> {
    let mut t: Vec<[Vec3; 3]> = rect(-6.0, -4.0, -1.0, 4.0).into();
    t.extend(rect(1.0, -4.0, 6.0, 4.0));
    t
}

fn params() -> NavMeshBuildParams {
    NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    }
}

fn bare_mesh() -> NavMesh {
    NavMeshBuilder::new(params())
        .build(&islands())
        .expect("the islands build")
}

/// The gap link: 3 m long, crossed in 1 s.
fn gap_link() -> OffMeshLink {
    let mut l = OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
        .with_user_id(5)
        .with_radius(0.5);
    l.duration = 1.0;
    l
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

/// A crowd with one agent walking from the left floor to the right one, stepped until it is on
/// the link.
fn crowd_on_link(mesh: &NavMesh) -> (NavCrowd, regia_nav::CrowdAgentId) {
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    for _ in 0..600 {
        crowd.step(mesh);
        if crowd.agent(id).expect("agent").link_traversal().is_some() {
            return (crowd, id);
        }
    }
    panic!("the agent never reached the link");
}

/// § 3.2: generated user ids count up from `first_user_id`, default `1 << 48`.
#[test]
fn generated_ids_start_at_two_to_the_48_by_default() {
    let d = NavLinkGenParams::default();
    assert_eq!(d.first_user_id, 1u64 << 48);
    let links = generate_links(&bare_mesh(), &d, |_| true).expect("generates");
    assert!(
        !links.is_empty(),
        "the 2 m gap gets jumps at the default 2 m max gap"
    );
    for (i, l) in links.iter().enumerate() {
        assert_eq!(l.user_id, (1u64 << 48) + i as u64, "link {i}");
    }
}

fn from_state(mesh: &NavMesh, from_y: f32, radius: f32) -> NavLinkState {
    let link = OffMeshLink::new(Vec3::new(-3.0, from_y, 0.0), Vec3::new(3.0, 0.0, 0.0)).with_radius(radius);
    let mesh = mesh.clone().with_off_mesh_links(vec![link]);
    mesh.link_statuses()[0].state
}

/// § 3.1: an endpoint resolves within `radius` in X and Z and within `max(radius, max_climb)` in
/// Y. The floor is at y = 0 and the default `max_climb` is 0.5.
#[test]
fn the_vertical_snap_reach_is_the_larger_of_radius_and_climb() {
    let mesh = bare_mesh();
    assert!((mesh.config.agent.max_climb - 0.5).abs() < 1e-6);
    let unresolved_from = NavLinkState::Unresolved {
        from: false,
        to: true,
    };
    // A small radius still reaches `max_climb` vertically...
    assert!(matches!(
        from_state(&mesh, 0.45, 0.1),
        NavLinkState::Active { .. }
    ));
    // ...and no further.
    assert_eq!(from_state(&mesh, 0.9, 0.1), unresolved_from);
    // A radius above `max_climb` reaches its own height.
    assert!(matches!(from_state(&mesh, 0.9, 1.0), NavLinkState::Active { .. }));
    assert_eq!(from_state(&mesh, 1.4, 1.0), unresolved_from);
}

/// § 3.3: `duration` 0 on a Teleport is one step.
#[test]
fn a_teleport_with_no_duration_crosses_in_one_step() {
    let mut link = gap_link().with_kind(NavLinkKind::Teleport);
    link.duration = 0.0;
    let mesh = bare_mesh().with_off_mesh_links(vec![link]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let (mut started, mut finished) = (None, None);
    for k in 0..600u32 {
        crowd.step(&mesh);
        for e in crowd.link_events() {
            assert_eq!(e.kind, NavLinkKind::Teleport);
            match e.phase {
                NavLinkPhase::Started => started = Some(k),
                NavLinkPhase::Finished => finished = Some(k),
                other => panic!("unexpected {other:?}"),
            }
        }
        if let Some(t) = crowd.agent(id).expect("agent").link_traversal() {
            assert_eq!(t.total_steps, 1, "a Teleport with duration 0 is one step");
        }
        if crowd.agent(id).expect("agent").state() == CrowdAgentState::Arrived {
            break;
        }
    }
    let (s, f) = (started.expect("took off"), finished.expect("landed"));
    assert_eq!(f - s, 1, "landed on the next step");
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

/// § 3.3: an agent removed during a crossing reports no `Finished`.
#[test]
fn an_agent_removed_mid_crossing_reports_no_finished() {
    let mesh = bare_mesh().with_off_mesh_links(vec![gap_link()]);
    let (mut crowd, id) = crowd_on_link(&mesh);
    crowd.remove_agent(id).expect("removes");
    for _ in 0..120 {
        crowd.step(&mesh);
        assert!(crowd.link_events().is_empty(), "{:?}", crowd.link_events());
    }
    assert_eq!(crowd.stats().link_traversals, 0);
}

/// § 3.3: a link removed during a crossing does not stop it; the agent lands and walks on.
#[test]
fn a_link_removed_mid_crossing_still_lands() {
    let mut mesh = bare_mesh().with_off_mesh_links(vec![gap_link()]);
    let (mut crowd, id) = crowd_on_link(&mesh);
    mesh.set_off_mesh_links(Vec::new());
    let mut finished = false;
    for _ in 0..600 {
        crowd.step(&mesh);
        finished |= crowd
            .link_events()
            .iter()
            .any(|e| e.agent == id && e.phase == NavLinkPhase::Finished);
        if crowd.agent(id).expect("agent").state() == CrowdAgentState::Arrived {
            break;
        }
    }
    assert!(finished, "the crossing finished");
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived);
    assert!(a.position().x > 3.5, "{:?}", a.position());
}

/// § 6.6: a crossing agent is not pushed by a neighbour standing on the landing; it stays on its
/// schedule for the whole crossing and lands.
#[test]
fn a_neighbour_on_the_landing_does_not_push_the_crossing_agent() {
    let mesh = bare_mesh().with_off_mesh_links(vec![gap_link()]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .add_agent(&mesh, Vec3::new(1.5, 0.0, 0.1), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut on_link = 0;
    let mut landed = false;
    for _ in 0..600 {
        crowd.step(&mesh);
        landed |= crowd
            .link_events()
            .iter()
            .any(|e| e.agent == id && e.phase == NavLinkPhase::Finished);
        let a = crowd.agent(id).expect("agent");
        if let Some(t) = a.link_traversal() {
            on_link += 1;
            assert!(
                (a.position() - t.position()).length() < 1e-5,
                "exactly on schedule: {:?} vs {:?}",
                a.position(),
                t.position()
            );
        }
        if landed {
            break;
        }
    }
    assert!(on_link > 1, "the agent crossed over several steps");
    assert!(landed, "the agent landed");
}

/// § 3.5: a file with more than `MAX_NAV_LINKS` links is `nav_invalid_link`.
#[test]
fn the_file_loader_refuses_more_links_than_the_cap() {
    let mut text = String::from("(format_version: 1, links: [");
    for i in 0..=MAX_NAV_LINKS {
        let x = (i % 1000) as f32;
        text.push_str(&format!("(from: ({x}, 0.0, 0.0), to: ({x}, 0.0, 1.0)),"));
    }
    text.push_str("])");
    let err = NavLinkSet::from_ron_str(&text).expect_err("too many links");
    assert_eq!(err.code(), "nav_invalid_link", "{err}");
    // One fewer is fine.
    let last = text.rfind("(from:").expect("a link");
    let mut fits = text[..last].to_string();
    fits.push_str("])");
    assert_eq!(
        NavLinkSet::from_ron_str(&fits).expect("at the cap").len(),
        MAX_NAV_LINKS
    );
}

/// § 3.5 migration note: link lists serialized before the v1 fields (only `from`, `to`,
/// `bidirectional` and `area`) still read, with the § 3.1 defaults.
#[test]
fn link_lists_serialized_before_v1_read_with_the_defaults() {
    let v0 = "[(from: (0.0, 0.0, 0.0), to: (0.0, -2.0, 3.0), bidirectional: false, area: 3)]";
    let links: Vec<OffMeshLink> = ron::from_str(v0).expect("a v0 list reads");
    let mut expected = OffMeshLink::new(Vec3::ZERO, Vec3::new(0.0, -2.0, 3.0)).one_way();
    expected.area = 3;
    assert_eq!(links, vec![expected]);
}

/// § 3.4: the tile cache's `set_off_mesh_links` and a `set_link_enabled` that changes a link
/// increment `revision`; a switch that changes nothing does not.
#[test]
fn the_tile_cache_revision_moves_with_link_edits() {
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: params(),
            tile_size: 16,
        },
        &islands(),
    )
    .expect("builds");
    let r0 = cache.revision();
    cache.set_off_mesh_links(vec![gap_link()]);
    let r1 = cache.revision();
    assert!(r1 > r0, "replacing the links is a route change");
    assert_eq!(cache.set_link_enabled(5, true), 0, "already on");
    assert_eq!(
        cache.revision(),
        r1,
        "a switch that changed nothing keeps the revision"
    );
    assert_eq!(cache.set_link_enabled(5, false), 1);
    let r2 = cache.revision();
    assert!(r2 > r1);
    assert_eq!(cache.mesh().link_statuses()[0].state, NavLinkState::Disabled);
    assert_eq!(cache.set_link_enabled(5, true), 1);
    assert!(cache.revision() > r2);
}
