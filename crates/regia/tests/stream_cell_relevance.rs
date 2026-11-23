// SPDX-License-Identifier: MPL-2.0
//! A peer loads the stream cell it stands in. Replication keeps that cell, the one-cell margin
//! and entities with no cell, and drops the rest. Streaming off does not register the filter.
//! Fixed steps only: no sleeps.

use std::sync::Arc;

use regia::net::model::{NetId, NetMode, NetOwner, NetPlayerId, NetTick, PeerId};
use regia::net::plugin::NetPlugin;
use regia::net::profile::{NetProfile, NetProfileRef, NetProfiles, RelevanceShape};
use regia::net::relevance::{
    InterestEntity, NetInterest, NetRelevanceFilters, NetViewpoint, install_net_interest,
};
use regia::stream_relevance::{self, FILTER_NAME, MARGIN_CELLS};
use regia_app::prelude::*;
use regia_asset::content_hash_hex;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene, cook_scene};
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellMember, LevelStreaming, SourceKind, StreamBudgets, StreamTuning, StreamingSource,
};
use regia_scene::{ScenePlugin, Transform};
use regia_world::format::{
    CELL_BLOB_FORMAT_VERSION, CellBlob, CellRecord, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id,
};
use regia_world::{
    CellSource, CellState, MemoryCellSource, StreamTiming, WorldStreaming, WorldStreamingPlugin,
    open_streamed_from,
};

const CELL_M: f64 = 100.0;

fn level_id() -> LevelId {
    LevelId(Guid::from_u128(0xfeed))
}

fn app_with(mode: NetMode) -> App {
    let mut app = App::new();
    app.add_plugins(ScenePlugin)
        .add_plugins(WorldStreamingPlugin)
        .add_plugins(NetPlugin { mode })
        .add_plugins(stream_relevance::StreamCellRelevancePlugin);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

fn server_app() -> App {
    app_with(NetMode::DedicatedServer)
}

fn settings() -> LevelStreaming {
    LevelStreaming {
        enabled: true,
        cell_size_m: CELL_M,
        load_radius_m: 40.0,
        unload_radius_m: 60.0,
        hlod: None,
        budgets: Some(StreamBudgets {
            max_loads_per_step: 0,
            max_inflight: 4,
            max_pending_bytes: 1 << 30,
            apply_budget_entities: 100,
            ..StreamBudgets::default()
        }),
        tuning: StreamTuning {
            retry_after_steps: 10,
            ..StreamTuning::default()
        },
        ..LevelStreaming::default()
    }
}

fn blob_hash(bytes: &[u8]) -> [u8; 32] {
    let hex = content_hash_hex(bytes);
    assert_eq!(hex.len(), 64, "a content hash is 32 bytes of hex");
    let digits = hex.as_bytes();
    let mut out = [0_u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = (nibble(digits[i * 2]) << 4) | nibble(digits[i * 2 + 1]);
    }
    out
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("content hash is lowercase hex"),
    }
}

fn empty_scene() -> CookedScene {
    CookedScene {
        format_version: COOKED_SCENE_VERSION,
        strings: Vec::new(),
        entities: Vec::new(),
        resources: Vec::new(),
    }
}

struct Cooked {
    index: WorldIndex,
    source: MemoryCellSource,
}

fn cook_cell_0_0(app: &App) -> Cooked {
    let coord = CellCoord::new(0, 0);
    let mut file = SceneFile::new();
    file.entities.push(SceneEntity {
        guid: Guid::from_u128(1),
        name: Some("cell-root".to_string()),
        ..SceneEntity::default()
    });
    let (scene, warnings) = cook_scene(app.world(), &file).expect("cook cell");
    assert!(warnings.is_empty(), "{warnings:?}");
    let blob = CellBlob {
        format_version: CELL_BLOB_FORMAT_VERSION,
        coord,
        scene,
        anchor: [0, 0, 0],
        nav: None,
    };
    let id = cell_blob_id(level_id(), coord);
    let bytes = blob.to_bytes(id, Vec::new()).expect("encode cell");
    let record = CellRecord {
        coord,
        blob: id,
        blob_hash: blob_hash(&bytes),
        entity_count: 1,
        own_bytes: bytes.len() as u64,
        dep_bytes: 0,
        vram_bytes: 0,
        min_y: 0.0,
        max_y: 1.0,
        priority: 0,
        deps: Vec::new(),
        nav_tiles: 0,
        has_terrain_collision: false,
    };
    let source = MemoryCellSource::new();
    source.insert(id, bytes);
    let cells = vec![record];
    let index = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level: level_id().0,
        content_hash: WorldIndex::hash_cells(&cells),
        settings: settings(),
        cells,
        bounds: [coord, coord],
        hlod: None,
        replicated: Vec::new(),
        nav: None,
    };
    Cooked { index, source }
}

fn open_cell_0_0(app: &mut App) {
    let cooked = cook_cell_0_0(app);
    let source: Arc<dyn CellSource> = Arc::new(cooked.source);
    let world = app.world_mut();
    world
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    open_streamed_from(
        world,
        level_id(),
        LoadMode::Replace,
        cooked.index,
        &empty_scene(),
        source,
    )
    .expect("open streamed level");
}

fn wait_until_spawned(app: &mut App) {
    let mut last = None;
    for _ in 0..24 {
        app.update();
        last = app
            .world()
            .resource::<WorldStreaming>()
            .state(level_id(), CellCoord::new(0, 0));
        if last.as_ref().is_some_and(CellState::is_spawned) {
            return;
        }
    }
    panic!("cell 0_0 did not spawn: {last:?}");
}

fn spawn_source(app: &mut App, peer: PeerId, x: f32, z: f32) {
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(x, 0.0, z)),
        StreamingSource {
            kind: SourceKind::Player,
            view_bias: 0.0,
            ..StreamingSource::default()
        },
        NetOwner(peer),
    ));
}

fn filter_names(world: &World) -> Vec<String> {
    world
        .get_resource::<NetRelevanceFilters>()
        .map(|filters| filters.names().map(str::to_string).collect())
        .unwrap_or_default()
}

fn insert_profile(world: &mut World, name: &str, profile: NetProfile) {
    world
        .resource_mut::<NetProfiles>()
        .insert(name, profile)
        .expect("profile");
}

fn world_profile(filter: Option<&str>) -> NetProfile {
    NetProfile {
        relevance: RelevanceShape::World,
        linger_ticks: 0,
        custom_filter: filter.map(str::to_string),
        ..NetProfile::default()
    }
}

#[derive(Clone, Copy)]
struct Piece {
    id: u32,
    entity: Entity,
}

fn spawn_piece(world: &mut World, id: u32, at: Vec3, cell: Option<CellCoord>, profile: &str) -> Piece {
    let mut entity = world.spawn((
        Transform::from_translation(at),
        NetProfileRef(profile.to_string()),
    ));
    if let Some(cell) = cell {
        entity.insert(CellMember(cell));
    }
    Piece {
        id,
        entity: entity.id(),
    }
}

fn interest_of(pieces: &[Piece], structure: bool) -> Vec<InterestEntity> {
    let mut pieces = pieces.to_vec();
    pieces.sort_by_key(|piece| piece.id);
    pieces
        .into_iter()
        .map(|piece| InterestEntity {
            id: NetId(piece.id),
            slot: piece.id,
            entity: piece.entity,
            owner: PeerId::SERVER,
            depth: 0,
            dirty: true,
            structure,
            predicted: false,
            parent: None,
        })
        .collect()
}

fn held(interest: &NetInterest, peer: PeerId) -> Vec<u32> {
    interest.peers[&peer].held_ids().map(|id| id.0).collect()
}

fn filter_of<'a>(world: &'a World, name: &str) -> Option<&'a str> {
    world
        .resource::<NetProfiles>()
        .get(name)
        .and_then(|profile| profile.custom_filter.as_deref())
}

fn assert_stamped(world: &World) {
    assert_eq!(filter_of(world, "default"), Some(FILTER_NAME));
    assert_eq!(filter_of(world, "cells"), Some(FILTER_NAME));
    assert_eq!(filter_of(world, "near"), Some(FILTER_NAME));
    assert_eq!(filter_of(world, "game_state"), None);
    assert_eq!(filter_of(world, "held"), None);
    assert_eq!(filter_of(world, "mine"), Some(FILTER_NAME));
}

fn spawn_pieces(app: &mut App) -> Vec<Piece> {
    // Spawn order is not NetId order. The held list is still NetId order.
    let mut pieces = Vec::new();
    let world = app.world_mut();
    pieces.push(spawn_piece(
        world,
        3,
        Vec3::new(850.0, 0.0, 50.0),
        Some(CellCoord::new(8, 0)),
        "cells",
    ));
    pieces.push(spawn_piece(world, 2, Vec3::new(50.0, 0.0, 50.0), None, "cells"));
    pieces.push(spawn_piece(
        world,
        6,
        Vec3::new(60.0, 0.0, 50.0),
        Some(CellCoord::new(0, 0)),
        "near",
    ));
    pieces.push(spawn_piece(
        world,
        5,
        Vec3::new(250.0, 0.0, 50.0),
        Some(CellCoord::new(2, 0)),
        "cells",
    ));
    pieces.push(spawn_piece(
        world,
        1,
        Vec3::new(150.0, 0.0, 50.0),
        Some(CellCoord::new(1, 0)),
        "cells",
    ));
    pieces.push(spawn_piece(
        world,
        4,
        Vec3::new(150.0, 0.0, 150.0),
        Some(CellCoord::new(1, 1)),
        "cells",
    ));
    pieces.push(spawn_piece(
        world,
        7,
        Vec3::new(50.0, 0.0, 50.0),
        Some(CellCoord::new(0, 0)),
        "default",
    ));
    world.spawn((
        NetViewpoint {
            player: NetPlayerId {
                peer: PeerId(1),
                local: 0,
            },
        },
        Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
    ));
    pieces
}

#[test]
fn peer_in_cell_0_0_drops_cell_8_0_keeps_the_margin_and_an_entity_with_no_cell() {
    let mut app = server_app();
    open_cell_0_0(&mut app);
    spawn_source(&mut app, PeerId(1), 50.0, 50.0);
    spawn_source(&mut app, PeerId(2), 850.0, 50.0);
    wait_until_spawned(&mut app);
    assert!(filter_names(app.world()).iter().any(|name| name == FILTER_NAME));
    assert!(
        app.world()
            .resource::<WorldStreaming>()
            .state(level_id(), CellCoord::new(8, 0))
            .is_none()
    );

    assert!(app.world().contains_resource::<NetInterest>());
    insert_profile(app.world_mut(), "cells", world_profile(None));
    insert_profile(
        app.world_mut(),
        "near",
        NetProfile {
            relevance: RelevanceShape::Spatial {
                radius: 1.0,
                hysteresis: 0.0,
            },
            linger_ticks: 0,
            ..NetProfile::default()
        },
    );
    insert_profile(app.world_mut(), "mine", world_profile(Some("mine")));
    stream_relevance::sync_loaded_cells(app.world_mut());
    assert_stamped(app.world());
    let pieces = spawn_pieces(&mut app);
    let mut interest = app
        .world_mut()
        .remove_resource::<NetInterest>()
        .expect("interest");
    let peers = [PeerId(1), PeerId(2)];
    for tick in 0..2 {
        app.world_mut()
            .resource_mut::<regia::net::runtime::NetNode>()
            .tick = NetTick(0);
        stream_relevance::sync_loaded_cells(app.world_mut());
        app.world_mut().remove_resource::<NetInterest>();
        interest.update(
            app.world_mut(),
            &interest_of(&pieces, tick == 0),
            &peers,
            NetTick(tick),
        );
        stream_relevance::publish_dropped(app.world_mut());
        assert_eq!(held(&interest, PeerId(1)), vec![1, 2, 4, 7], "tick {tick}");
        assert_eq!(held(&interest, PeerId(2)), vec![2], "tick {tick}");
        let peer1 = stream_relevance::report(app.world(), PeerId(1)).expect("peer 1");
        assert_eq!(peer1.loaded, vec!["0_0".to_string()]);
        assert_eq!(peer1.margin, MARGIN_CELLS);
        assert_eq!(peer1.dropped, 2, "tick {tick}");
        let peer2 = stream_relevance::report(app.world(), PeerId(2)).expect("peer 2");
        assert!(peer2.loaded.is_empty());
        assert_eq!(peer2.margin, MARGIN_CELLS);
        assert_eq!(peer2.dropped, 4, "tick {tick}");
    }
    #[cfg(feature = "agent")]
    agent_agrees(app.world_mut());
    // A step without another snapshot preserves the last snapshot report.
    stream_relevance::publish_dropped(app.world_mut());
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer 1")
            .dropped,
        2
    );
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(99)).expect_err("unknown"),
        "peer_unknown"
    );
    // Moving the very same excluded entity into the one-cell margin admits it next snapshot.
    let far = pieces
        .iter()
        .find(|piece| piece.id == 3)
        .expect("far piece")
        .entity;
    app.world_mut().entity_mut(far).insert((
        CellMember(CellCoord::new(1, 0)),
        Transform::from_translation(Vec3::new(150.0, 0.0, 50.0)),
    ));
    app.world_mut()
        .resource_mut::<regia::net::runtime::NetNode>()
        .tick = NetTick(0);
    stream_relevance::sync_loaded_cells(app.world_mut());
    app.world_mut().remove_resource::<NetInterest>();
    interest.update(app.world_mut(), &interest_of(&pieces, false), &peers, NetTick(2));
    stream_relevance::publish_dropped(app.world_mut());
    assert_eq!(held(&interest, PeerId(1)), vec![1, 2, 3, 4, 7]);
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer 1")
            .dropped,
        1
    );
}

#[cfg(feature = "agent")]
fn agent_agrees(world: &mut World) {
    let host = *world.resource::<regia::agent::CellRelevanceHost>();
    let reply = host.query(world, 1).expect("host");
    assert_eq!(reply.loaded, vec!["0_0".to_string()]);
    assert_eq!(reply.margin, MARGIN_CELLS);
    assert_eq!(reply.dropped, 2);
    assert_eq!(
        regia::agent::cell_relevance_peer(world, 99).expect_err("unknown"),
        "peer_unknown"
    );
    world.remove_resource::<regia::agent::CellRelevanceHost>();
    assert_eq!(
        regia::agent::cell_relevance_peer(world, 1).expect_err("no host"),
        "stream_cell_filter_off"
    );
}

#[test]
fn streaming_off_leaves_the_filter_unregistered_and_a_far_entity_relevant() {
    let mut app = server_app();
    app.update();
    stream_relevance::sync_loaded_cells(app.world_mut());
    assert!(!filter_names(app.world()).iter().any(|name| name == FILTER_NAME));
    install_net_interest(app.world_mut());
    assert!(
        app.world()
            .resource::<NetProfiles>()
            .get("default")
            .expect("default profile")
            .custom_filter
            .is_none()
    );
    insert_profile(app.world_mut(), "far", world_profile(Some(FILTER_NAME)));
    let piece = spawn_piece(
        app.world_mut(),
        3,
        Vec3::new(850.0, 0.0, 50.0),
        Some(CellCoord::new(8, 0)),
        "far",
    );
    let mut interest = app
        .world_mut()
        .remove_resource::<NetInterest>()
        .expect("interest");
    interest.update(
        app.world_mut(),
        &interest_of(&[piece], true),
        &[PeerId(1)],
        NetTick(0),
    );
    assert_eq!(held(&interest, PeerId(1)), vec![3]);
    assert!(!filter_names(app.world()).iter().any(|name| name == FILTER_NAME));
}

#[test]
fn a_client_does_not_register_the_filter() {
    let mut app = app_with(NetMode::Client);
    open_cell_0_0(&mut app);
    assert!(app.world().resource::<WorldStreaming>().is_active());
    stream_relevance::sync_loaded_cells(app.world_mut());
    assert!(!filter_names(app.world()).iter().any(|name| name == FILTER_NAME));
    let stamped = app.world().get_resource::<NetProfiles>().is_some_and(|profiles| {
        profiles
            .get("default")
            .is_some_and(|profile| profile.custom_filter.is_some())
    });
    assert!(!stamped);
    #[cfg(feature = "agent")]
    assert_eq!(
        regia::agent::cell_relevance_peer(app.world(), 1).expect_err("client"),
        "net_not_server"
    );
}

#[test]
fn custom_filters_and_stream_cells_both_narrow_interest() {
    use regia::net::relevance::NetRelevanceFilter;
    struct Refuse;
    impl NetRelevanceFilter for Refuse {
        fn relevant(_: &World, _: Entity, _: PeerId) -> bool {
            false
        }
    }
    struct Allow;
    impl NetRelevanceFilter for Allow {
        fn relevant(_: &World, _: Entity, _: PeerId) -> bool {
            true
        }
    }
    let mut app = server_app();
    open_cell_0_0(&mut app);
    spawn_source(&mut app, PeerId(1), 50.0, 50.0);
    wait_until_spawned(&mut app);
    regia::net::add_net_relevance_filter::<Refuse>(app.world_mut(), "refuse");
    regia::net::add_net_relevance_filter::<Allow>(app.world_mut(), "allow");
    insert_profile(app.world_mut(), "refuse", world_profile(Some("refuse")));
    insert_profile(app.world_mut(), "allow", world_profile(Some("allow")));
    app.world_mut()
        .resource_mut::<regia::net::runtime::NetNode>()
        .tick = NetTick(0);
    stream_relevance::sync_loaded_cells(app.world_mut());
    let pieces = [
        spawn_piece(
            app.world_mut(),
            3,
            Vec3::ZERO,
            Some(CellCoord::new(8, 0)),
            "allow",
        ),
        spawn_piece(
            app.world_mut(),
            2,
            Vec3::ZERO,
            Some(CellCoord::new(0, 0)),
            "allow",
        ),
        spawn_piece(app.world_mut(), 1, Vec3::ZERO, None, "refuse"),
    ];
    let mut interest = app
        .world_mut()
        .remove_resource::<NetInterest>()
        .expect("installed automatically");
    interest.update(
        app.world_mut(),
        &interest_of(&pieces, true),
        &[PeerId(1)],
        NetTick(0),
    );
    stream_relevance::publish_dropped(app.world_mut());
    assert_eq!(held(&interest, PeerId(1)), vec![2]);
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer")
            .dropped,
        1
    );
    app.world_mut().insert_resource(interest);
    // Diagnostic evaluations between snapshots must not change the next report.
    let interest = app.world().resource::<NetInterest>();
    assert!(
        !interest
            .why(app.world(), PeerId(1), NetId(3))
            .expect("verdict")
            .relevant
    );
    app.world_mut()
        .resource_mut::<regia::net::settings::NetSettings>()
        .replication
        .send_every_ticks = 3;
    app.world_mut()
        .resource_mut::<regia::net::runtime::NetNode>()
        .tick = NetTick(1);
    stream_relevance::sync_loaded_cells(app.world_mut());
    stream_relevance::publish_dropped(app.world_mut());
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer")
            .dropped,
        1
    );
    // A real, empty snapshot replaces the previous count with zero.
    app.world_mut()
        .resource_mut::<regia::net::runtime::NetNode>()
        .tick = NetTick(3);
    stream_relevance::sync_loaded_cells(app.world_mut());
    assert!(
        !app.world()
            .resource::<NetInterest>()
            .why(app.world(), PeerId(1), NetId(3))
            .expect("diagnostic verdict")
            .relevant
    );
    stream_relevance::publish_dropped(app.world_mut());
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer")
            .dropped,
        0
    );
}

#[test]
fn scheduled_server_send_filters_candidates_without_manual_pipeline_installation() {
    use regia::net::replication::Replicated;
    use regia::net::runtime::{NetNode, server_add_peer};
    let mut app = server_app();
    open_cell_0_0(&mut app);
    spawn_source(&mut app, PeerId(1), 50.0, 50.0);
    wait_until_spawned(&mut app);
    insert_profile(app.world_mut(), "cells", world_profile(None));
    server_add_peer(app.world_mut(), PeerId(1));
    let far = app
        .world_mut()
        .spawn((
            Replicated {
                profile: Some("cells".into()),
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(850.0, 0.0, 50.0)),
            CellMember(CellCoord::new(8, 0)),
        ))
        .id();
    let margin = app
        .world_mut()
        .spawn((
            Replicated {
                profile: Some("cells".into()),
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(150.0, 0.0, 50.0)),
            CellMember(CellCoord::new(1, 0)),
        ))
        .id();
    let always = app
        .world_mut()
        .spawn((
            Replicated {
                profile: Some("cells".into()),
                ..Replicated::default()
            },
            Transform::IDENTITY,
        ))
        .id();
    app.world_mut().resource_mut::<NetNode>().tick = NetTick(0);
    app.world_mut()
        .resource_mut::<regia::net::settings::NetSettings>()
        .replication
        .send_every_ticks = 3;
    app.update();
    let report = stream_relevance::report(app.world(), PeerId(1)).expect("peer");
    assert_eq!(report.dropped, 1);
    let interest = app.world().resource::<NetInterest>();
    let held = &interest.peers[&PeerId(1)];
    assert!(!held.holds(*app.world().get::<NetId>(far).expect("far id")));
    assert!(held.holds(*app.world().get::<NetId>(margin).expect("margin id")));
    assert!(held.holds(*app.world().get::<NetId>(always).expect("always id")));
    app.update();
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer")
            .dropped,
        1
    );
    app.update();
    assert_eq!(
        stream_relevance::report(app.world(), PeerId(1))
            .expect("peer")
            .dropped,
        1
    );
}

#[test]
fn a_disabled_streamed_index_does_not_register_the_filter() {
    let mut app = server_app();
    let mut cooked = cook_cell_0_0(&app);
    cooked.index.settings.enabled = false;
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .open_level(level_id(), cooked.index, Arc::new(cooked.source))
        .expect("a disabled index is accepted by the low-level world API");
    assert!(app.world().resource::<WorldStreaming>().is_active());
    stream_relevance::sync_loaded_cells(app.world_mut());
    assert!(!filter_names(app.world()).iter().any(|name| name == FILTER_NAME));
    assert!(!app.world().contains_resource::<NetInterest>());
    install_net_interest(app.world_mut());
    insert_profile(app.world_mut(), "far", world_profile(None));
    let far = spawn_piece(
        app.world_mut(),
        3,
        Vec3::new(850.0, 0.0, 50.0),
        Some(CellCoord::new(8, 0)),
        "far",
    );
    let mut interest = app
        .world_mut()
        .remove_resource::<NetInterest>()
        .expect("interest");
    interest.update(
        app.world_mut(),
        &interest_of(&[far], true),
        &[PeerId(1)],
        NetTick(0),
    );
    assert_eq!(held(&interest, PeerId(1)), vec![3]);
}

/// w7-runtime-smalls-z item 3: a peer whose streaming source has no position (no `Transform`) or
/// a non-finite one fails closed: it holds an empty loaded set, so cell members are dropped for it
/// and only entities with no cell stay relevant. It used to get no entry, which left the whole
/// world relevant to it.
#[test]
fn a_source_with_no_or_a_nan_position_narrows_its_peer_to_an_empty_set() {
    let mut app = server_app();
    open_cell_0_0(&mut app);
    spawn_source(&mut app, PeerId(1), 50.0, 50.0);
    // Peer 3: a source with no Transform at all.
    app.world_mut().spawn((
        StreamingSource {
            kind: SourceKind::Player,
            view_bias: 0.0,
            ..StreamingSource::default()
        },
        NetOwner(PeerId(3)),
    ));
    // Peer 4: a source at a NaN position.
    spawn_source(&mut app, PeerId(4), f32::NAN, 50.0);
    wait_until_spawned(&mut app);
    insert_profile(app.world_mut(), "cells", world_profile(None));
    insert_profile(
        app.world_mut(),
        "near",
        NetProfile {
            relevance: RelevanceShape::Spatial {
                radius: 1.0,
                hysteresis: 0.0,
            },
            linger_ticks: 0,
            ..NetProfile::default()
        },
    );
    insert_profile(app.world_mut(), "mine", world_profile(Some("mine")));
    stream_relevance::sync_loaded_cells(app.world_mut());
    let pieces = spawn_pieces(&mut app);
    let mut interest = app
        .world_mut()
        .remove_resource::<NetInterest>()
        .expect("interest");
    let peers = [PeerId(1), PeerId(3), PeerId(4)];
    app.world_mut()
        .resource_mut::<regia::net::runtime::NetNode>()
        .tick = NetTick(0);
    stream_relevance::sync_loaded_cells(app.world_mut());
    app.world_mut().remove_resource::<NetInterest>();
    interest.update(app.world_mut(), &interest_of(&pieces, true), &peers, NetTick(0));
    stream_relevance::publish_dropped(app.world_mut());
    assert_eq!(held(&interest, PeerId(1)), vec![1, 2, 4, 7], "the placed peer");
    for peer in [PeerId(3), PeerId(4)] {
        let report = stream_relevance::report(app.world(), peer)
            .unwrap_or_else(|err| panic!("peer {} has no loaded-cell entry: {err}", peer.0));
        assert!(report.loaded.is_empty(), "peer {}: {:?}", peer.0, report.loaded);
        assert_eq!(
            held(&interest, peer),
            vec![2],
            "peer {} keeps only the entity with no cell",
            peer.0
        );
    }
}
