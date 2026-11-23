// SPDX-License-Identifier: MPL-2.0
//! Runtime `.navlinks.ron` / `.navlanes.ron` loading and scene link anchors (ADR-2420): the
//! loaders round-trip and fail with typed codes, `NavLinkAnchor` follows its entity, and
//! `NavAuthoredLinksPlugin` keeps the navmesh's links equal to the files' plus the anchors'.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{
    MAX_NAV_FILE_BYTES, NavAuthoredLinksPlugin, NavAuthoredLinksStatus, NavLaneDef, NavLaneGraph, NavLaneSet,
    NavLinkAnchor, NavLinkFiles, NavLinkKind, NavLinkSet, NavMeshBuildParams, NavTileCache,
    NavTileCacheParams, OffMeshLink,
};
use regia_scene::Transform;
use regia_test_utils::unique_temp_dir;

/// Writes `text` to `dir/name` atomically and returns the path.
fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, &path).expect("rename");
    path
}

fn links() -> Vec<OffMeshLink> {
    let mut drop = OffMeshLink::new(Vec3::new(0.0, 2.0, 0.0), Vec3::new(0.0, 0.0, 3.0)).with_user_id(7);
    drop.kind = NavLinkKind::Drop;
    drop.bidirectional = false;
    let mut climb = OffMeshLink::new(Vec3::new(4.0, 0.0, 0.0), Vec3::new(4.0, 3.0, 0.0)).with_user_id(8);
    climb.kind = NavLinkKind::Climb;
    climb.duration = 1.5;
    vec![drop, climb]
}

fn lane_set(x: f32) -> NavLaneSet {
    let mut a = NavLaneDef::new(&[Vec3::new(x, 0.0, 0.0), Vec3::new(x + 10.0, 0.0, 0.0)]);
    a.next = vec![1];
    let b = NavLaneDef::new(&[Vec3::new(x + 10.0, 0.0, 0.0), Vec3::new(x + 10.0, 0.0, 10.0)]);
    NavLaneSet {
        format_version: regia_nav::NAV_LANES_FORMAT_VERSION,
        join_distance: 0.0,
        lanes: vec![a, b],
    }
}

#[test]
fn the_loaders_round_trip_files() {
    let dir = unique_temp_dir("nav-files");
    let text = NavLinkSet::to_ron_string(&links()).expect("writes");
    let path = write(&dir, "level.navlinks.ron", &text);
    assert_eq!(NavLinkSet::load(&path).expect("loads"), links());
    assert_eq!(NavLinkSet::from_bytes(text.as_bytes()).expect("reads"), links());

    let set = lane_set(0.0);
    let path = write(&dir, "level.navlanes.ron", &set.to_ron().expect("writes"));
    assert_eq!(NavLaneSet::load(&path).expect("loads"), set);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unreadable_files_are_typed_errors() {
    let dir = unique_temp_dir("nav-files-bad");
    let missing = dir.join("none.navlinks.ron");
    assert_eq!(
        NavLinkSet::load(&missing).err().map(|e| e.code()),
        Some("nav_link_io")
    );
    assert_eq!(
        NavLaneSet::load(&missing).err().map(|e| e.code()),
        Some("nav_lane_io")
    );
    let not_utf8 = [b'(', 0xff, 0xfe, b')'];
    assert_eq!(
        NavLinkSet::from_bytes(&not_utf8).err().map(|e| e.code()),
        Some("nav_link_io")
    );
    assert_eq!(
        NavLaneSet::from_bytes(&not_utf8).err().map(|e| e.code()),
        Some("nav_lane_io")
    );
    let huge = vec![b' '; MAX_NAV_FILE_BYTES + 1];
    assert_eq!(
        NavLinkSet::from_bytes(&huge).err().map(|e| e.code()),
        Some("nav_link_io")
    );
    let garbage = write(&dir, "garbage.navlinks.ron", "(format_version: 1, links: [oops])");
    assert_eq!(
        NavLinkSet::load(&garbage).err().map(|e| e.code()),
        Some("nav_link_parse")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn appending_lane_sets_shifts_their_indices() {
    let mut all = lane_set(0.0);
    let mut other = lane_set(50.0);
    other.lanes[1].opposite = None;
    other.join_distance = 0.5;
    all.append(other).expect("fits");
    assert_eq!(all.lanes.len(), 4);
    assert_eq!(all.lanes[0].next, vec![1]);
    assert_eq!(all.lanes[2].next, vec![3]);
    assert_eq!(all.join_distance, 0.5);
    all.graph().expect("a valid graph");
}

#[test]
fn an_anchor_makes_its_link_at_the_entity_pose() {
    let anchor = NavLinkAnchor {
        user_id: 3,
        kind: NavLinkKind::Climb,
        ..NavLinkAnchor::new(Vec3::new(0.0, 3.0, -1.0))
    };
    let pose = Transform {
        translation: Vec3::new(10.0, 0.0, 5.0),
        rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        ..Default::default()
    }
    .compute_affine();
    let link = anchor.link(&pose);
    assert!((link.from - Vec3::new(10.0, 0.0, 5.0)).length() < 1.0e-5);
    // -Z turned a quarter about +Y is -X.
    assert!(
        (link.to - Vec3::new(9.0, 3.0, 5.0)).length() < 1.0e-5,
        "{:?}",
        link.to
    );
    assert_eq!((link.user_id, link.kind), (3, NavLinkKind::Climb));
    let mut newer = anchor;
    newer.format_version = 2;
    assert!(newer.migrate().is_err());
}

/// Two 5 x 8 m floors with a 2 m gap; no links.
fn islands_cache() -> NavTileCache {
    let rect = |x0: f32, x1: f32| {
        [
            [
                Vec3::new(x0, 0.0, -4.0),
                Vec3::new(x1, 0.0, -4.0),
                Vec3::new(x1, 0.0, 4.0),
            ],
            [
                Vec3::new(x0, 0.0, -4.0),
                Vec3::new(x1, 0.0, 4.0),
                Vec3::new(x0, 0.0, 4.0),
            ],
        ]
    };
    let mut tris: Vec<[Vec3; 3]> = rect(-6.0, -1.0).into();
    tris.extend(rect(1.0, 6.0));
    let params = NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    };
    NavTileCache::build(params, &tris).expect("builds")
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(NavAuthoredLinksPlugin);
    app.insert_resource(islands_cache());
    app
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(PreUpdate);
    app.world_mut().run_schedule(FixedUpdate);
}

fn cache_links(app: &App) -> Vec<OffMeshLink> {
    app.world().resource::<NavTileCache>().off_mesh_links().to_vec()
}

fn crosses(app: &App) -> bool {
    app.world()
        .resource::<NavTileCache>()
        .query()
        .find_path(
            Vec3::new(-4.0, 0.0, 0.0),
            Vec3::new(4.0, 0.0, 0.0),
            Vec3::splat(1.0),
        )
        .is_some_and(|p| p.waypoints.last().is_some_and(|w| (w.x - 4.0).abs() < 0.1))
}

#[test]
fn anchors_put_their_links_on_the_navmesh_and_follow_their_entity() {
    let mut app = app();
    step(&mut app);
    assert!(cache_links(&app).is_empty());
    assert!(!crosses(&app), "no way over the gap yet");
    let anchor = NavLinkAnchor {
        user_id: 4,
        radius: 0.5,
        ..NavLinkAnchor::new(Vec3::new(3.0, 0.0, 0.0))
    };
    let at = |z: f32| Transform {
        translation: Vec3::new(-1.5, 0.0, z),
        ..Default::default()
    };
    let e = app.world_mut().spawn((anchor, at(0.0))).id();
    step(&mut app);
    let links = cache_links(&app);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].user_id, 4);
    assert!((links[0].to - Vec3::new(1.5, 0.0, 0.0)).length() < 1.0e-5);
    assert!(crosses(&app), "the anchor's link crosses the gap");
    let revision = app.world().resource::<NavTileCache>().revision();
    // Nothing moved: nothing is applied again.
    step(&mut app);
    assert_eq!(app.world().resource::<NavTileCache>().revision(), revision);
    assert_eq!(app.world().resource::<NavAuthoredLinksStatus>().applied, 1);
    // It moves with its entity.
    app.world_mut().entity_mut(e).insert(at(2.0));
    step(&mut app);
    assert!((cache_links(&app)[0].from - Vec3::new(-1.5, 0.0, 2.0)).length() < 1.0e-5);
    // An invalid anchor stays off, and is counted.
    app.world_mut().spawn((
        NavLinkAnchor {
            radius: 0.0,
            ..NavLinkAnchor::default()
        },
        at(-2.0),
    ));
    step(&mut app);
    assert_eq!(cache_links(&app).len(), 1);
    assert_eq!(
        app.world().resource::<NavAuthoredLinksStatus>().invalid_anchors,
        1
    );
    // Removed, its link goes.
    app.world_mut().entity_mut(e).remove::<NavLinkAnchor>();
    step(&mut app);
    assert!(cache_links(&app).is_empty());
}

#[test]
fn files_load_at_run_time_before_the_anchors() {
    let dir = unique_temp_dir("nav-files-app");
    let links_path = write(
        &dir,
        "a.navlinks.ron",
        &NavLinkSet::to_ron_string(&links()).expect("writes"),
    );
    let lanes_a = write(&dir, "a.navlanes.ron", &lane_set(0.0).to_ron().expect("writes"));
    let lanes_b = write(&dir, "b.navlanes.ron", &lane_set(50.0).to_ron().expect("writes"));
    let mut app = app();
    app.world_mut().spawn((
        NavLinkAnchor {
            user_id: 1,
            ..NavLinkAnchor::new(Vec3::new(3.0, 0.0, 0.0))
        },
        Transform::default(),
    ));
    app.insert_resource(NavLinkFiles {
        links: vec![
            links_path.display().to_string(),
            dir.join("missing.navlinks.ron").display().to_string(),
        ],
        lanes: vec![lanes_a.display().to_string(), lanes_b.display().to_string()],
    });
    step(&mut app);
    let ids: Vec<u64> = cache_links(&app).iter().map(|l| l.user_id).collect();
    assert_eq!(ids, vec![7, 8, 1], "the files' links, then the anchors'");
    let status = app.world().resource::<NavAuthoredLinksStatus>().clone();
    assert_eq!(
        (status.file_links, status.anchor_links, status.file_lanes),
        (2, 1, 4)
    );
    assert_eq!(status.errors.len(), 1);
    assert_eq!(status.errors[0].code, "nav_link_io");
    let graph = app.world().resource::<NavLaneGraph>();
    assert_eq!(graph.len(), 4);
    assert_eq!(graph.lane(2).expect("lane").next, vec![3]);
    // Clearing the files keeps only the anchor.
    app.insert_resource(NavLinkFiles::default());
    step(&mut app);
    let ids: Vec<u64> = cache_links(&app).iter().map(|l| l.user_id).collect();
    assert_eq!(ids, vec![1]);
    let _ = std::fs::remove_dir_all(&dir);
}
