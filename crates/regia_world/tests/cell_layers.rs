// SPDX-License-Identifier: MPL-2.0
//! Data layers govern streamed cell members (spec 78 § 10.1, ADR-7411; deep-zai-terrain-stream #2):
//! a cell entity whose layer is Parked or Absent arrives parked, and a switch back to Live shows it.

mod common;

use common::*;
use regia_core::id::Guid;
use regia_scene::components::{EntityGuid, Visibility};
use regia_scene::cooked::cook_scene;
use regia_scene::format::{SCENE_FORMAT_VERSION, SceneFile};
use regia_scene::layers::{LayerDefs, LayerDefsLibrary, LayerParked, LayerState, LayerTable};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::stream_grid::CellCoord;
use regia_world::CellState;
use regia_world::format::{CELL_BLOB_FORMAT_VERSION, CellBlob, WorldIndex};

const DEFS_ID: Guid = Guid::from_u128(0xd0);
const RULES: LevelId = LevelId(Guid::from_u128(0x1e));
const NIGHT_LAMP: u128 = 0x4401;
const DAY_SIGN: u128 = 0x4402;

/// One cell at (0, 0) with a `night` member and a member of no layer.
fn world_with_members(app: &regia_app::App) -> World {
    let mut world = build(app, 1, 1, 1, 1, settings(250.0, 300.0));
    let text = format!(
        r#"SceneFile(format_version: {v}, entities: [
            (guid: "{}", name: "lamp", components: {{
                "regia_scene::transform::Transform": (translation: (10.0, 0.0, 10.0)),
                "regia_scene::layers::LayerMembership": (layers: ["night"]),
            }}),
            (guid: "{}", name: "sign", components: {{
                "regia_scene::transform::Transform": (translation: (20.0, 0.0, 10.0)),
            }}),
        ], resources: {{}})"#,
        Guid::from_u128(NIGHT_LAMP),
        Guid::from_u128(DAY_SIGN),
        v = SCENE_FORMAT_VERSION,
    );
    let file = SceneFile::from_ron_str(&text).expect("cell scene");
    let (scene, warnings) = cook_scene(app.world(), &file).expect("cook");
    assert!(warnings.is_empty(), "{warnings:?}");
    let record = &mut world.index.cells[0];
    let blob = CellBlob {
        format_version: CELL_BLOB_FORMAT_VERSION,
        coord: record.coord,
        scene,
        anchor: [0, 0, 0],
        nav: None,
    };
    let bytes = blob.to_bytes(record.blob, Vec::new()).expect("encode");
    record.blob_hash = *blake3::hash(&bytes).as_bytes();
    record.own_bytes = bytes.len() as u64;
    record.entity_count = 2;
    world.source.insert(record.blob, bytes);
    world.index.content_hash = WorldIndex::hash_cells(&world.index.cells);
    world
}

/// A plain level that binds `night` at `initial`, opened next to the streamed one.
fn open_rules_level(app: &mut regia_app::App, initial: &str) {
    let defs = LayerDefs::from_ron_str(
        r#"LayerDefs(format_version: 1, layers: [
            (name: "night", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
        ])"#,
    )
    .expect("defs");
    let text = format!(
        r#"SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [], resources: {{
            "regia_scene::layers::LevelLayers": (
                defs: ["asset:{DEFS_ID}"],
                entries: [
                    (layer: "night", initial: {initial}, editor_visible: true, editor_resident: true, peers: Everyone),
                ],
            ),
        }})"#
    );
    let world = app.world_mut();
    world
        .get_resource_or_init::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs);
    let level = Level::from_ron_str(RULES, &text).expect("level");
    world.get_resource_or_insert_with(LevelLibrary::new).insert(level);
    open_level(world, RULES, LoadMode::Additive).expect("open rules level");
}

fn find(app: &mut regia_app::App, id: u128) -> regia_ecs::prelude::Entity {
    let world = app.world_mut();
    let mut query = world.query::<(regia_ecs::prelude::Entity, &EntityGuid)>();
    query
        .iter(world)
        .find(|(_, guid)| guid.0 == Guid::from_u128(id))
        .map(|(entity, _)| entity)
        .expect("the member spawned")
}

fn request(app: &mut regia_app::App, state: LayerState) {
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request("night", state, false, regia_scene::layers::Side::Local)
        .expect("request");
    steps(app, 2);
}

fn run(initial: &str) -> regia_app::App {
    let mut app = app();
    let world = world_with_members(&app);
    open(&mut app, &world, 1);
    open_rules_level(&mut app, initial);
    steps(&mut app, 2);
    let expected = match initial {
        "Parked" => LayerState::Parked,
        "Absent" => LayerState::Absent,
        _ => LayerState::Live,
    };
    assert_eq!(
        app.world().resource::<LayerTable>().effective("night"),
        Some(expected),
        "the rules level binds `night`"
    );
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(state(&app, CellCoord::new(0, 0)), Some(CellState::Visible));
    app
}

#[test]
fn a_cell_member_of_a_parked_layer_arrives_parked_and_unparks_on_live() {
    let mut app = run("Parked");
    let lamp = find(&mut app, NIGHT_LAMP);
    let sign = find(&mut app, DAY_SIGN);
    assert!(
        app.world().get::<LayerParked>(lamp).is_some(),
        "a member of a Parked layer is parked when its cell spawns"
    );
    assert_eq!(app.world().get::<Visibility>(lamp), Some(&Visibility::Hidden));
    assert!(
        app.world().get::<LayerParked>(sign).is_none(),
        "a non-member is left alone"
    );

    request(&mut app, LayerState::Live);
    assert!(app.world().get::<LayerParked>(lamp).is_none());
    assert_ne!(
        app.world().get::<Visibility>(lamp),
        Some(&Visibility::Hidden),
        "unparking restores the visibility from before the park"
    );
}

#[test]
fn a_cell_member_of_an_absent_layer_is_not_live_and_returns_on_live() {
    let mut app = run("Absent");
    let lamp = find(&mut app, NIGHT_LAMP);
    assert!(
        app.world().get::<LayerParked>(lamp).is_some(),
        "a cell member has no template record: it parks rather than spawning live (ADR-7411)"
    );
    request(&mut app, LayerState::Live);
    let lamp = find(&mut app, NIGHT_LAMP);
    assert!(app.world().get::<LayerParked>(lamp).is_none());
}

#[test]
fn a_cell_member_of_a_live_layer_spawns_normally() {
    let mut app = run("Live");
    let lamp = find(&mut app, NIGHT_LAMP);
    assert!(app.world().get::<LayerParked>(lamp).is_none());
}
