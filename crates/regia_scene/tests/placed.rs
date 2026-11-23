// SPDX-License-Identifier: MPL-2.0
//! Placed levels (spec 37 § 10 test 7): inline derived guids, filters, transforms, ignored
//! resources, inherited layers, cycle/depth/missing reasons, streamed mode and break.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_scene::layers::LayerMembership;
use regia_scene::level::{
    Level, LevelId, LevelLibrary, LevelManager, LevelSettings, LoadMode, open_level, snapshot_level,
};
use regia_scene::placed::{
    PlacedError, PlacedLevel, PlacedLevelLoaded, PlacedMember, PlacedMode, PlacedSpawned, break_placed,
    derive_guid,
};
use regia_scene::prelude::*;

const PROP: LevelId = LevelId(Guid::from_u128(0xa0));
const HOST: LevelId = LevelId(Guid::from_u128(0xb0));
const R: Guid = Guid::from_u128(0x10);
const C: Guid = Guid::from_u128(0x11);
const D: Guid = Guid::from_u128(0x12);
const I1: Guid = Guid::from_u128(0x21);
const I2: Guid = Guid::from_u128(0x22);

fn transform(x: f32, y: f32) -> String {
    format!(
        "\"regia_scene::transform::Transform\": (translation: ({x:?}, {y:?}, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),"
    )
}

fn entity(guid: Guid, parent: Option<Guid>, components: &str) -> String {
    let parent = parent
        .map(|p| format!("parent: Some(\"{p}\"),"))
        .unwrap_or_default();
    format!("(guid: \"{guid}\", {parent} components: {{ {components} }}),")
}

fn placed(level: LevelId, mode: &str, filter: &str) -> String {
    format!(
        "\"regia_scene::placed::PlacedLevel\": (level: \"asset:{}\", mode: {mode}, filter: (layers: [{filter}])),",
        level.0
    )
}

fn scene(entities: &[String], resources: &str) -> String {
    format!(
        "SceneFile(format_version: {}, entities: [{}], resources: {{ {resources} }})",
        SCENE_FORMAT_VERSION,
        entities.concat()
    )
}

fn prop_level() -> String {
    scene(
        &[
            entity(R, None, &transform(1.0, 0.0)),
            entity(C, Some(R), &transform(0.0, 2.0)),
            entity(
                D,
                Some(R),
                &format!(
                    "{} \"regia_scene::layers::LayerMembership\": (layers: [\"debug\"]),",
                    transform(0.0, 0.0)
                ),
            ),
        ],
        "\"regia_scene::level::LevelSettings\": (gravity: (0.0, -1.0, 0.0)),",
    )
}

fn host_level() -> String {
    scene(
        &[
            entity(
                I1,
                None,
                &format!(
                    "{} {} \"regia_scene::layers::LayerMembership\": (layers: [\"night\"]),",
                    transform(10.0, 0.0),
                    placed(PROP, "Inline", "")
                ),
            ),
            entity(
                I2,
                None,
                &format!(
                    "{} {}",
                    transform(-10.0, 0.0),
                    placed(PROP, "Inline", "(layer: \"debug\", include: false)")
                ),
            ),
        ],
        "",
    )
}

fn app_with(levels: &[(LevelId, String)]) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    for (id, text) in levels {
        let level = Level::from_ron_str(*id, text).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    }
    app
}

fn open(app: &mut App, level: LevelId) {
    open_level(app.world_mut(), level, LoadMode::Replace).expect("open");
    app.update();
}

fn by_guid(app: &mut App, guid: Guid) -> Option<Entity> {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &EntityGuid)>();
    q.iter(world).find(|(_, g)| g.0 == guid).map(|(e, _)| e)
}

fn members_of(app: &mut App, instance: Entity) -> Vec<Guid> {
    let world = app.world_mut();
    let mut q = world.query::<(&EntityGuid, &PlacedMember)>();
    let mut out: Vec<Guid> = q
        .iter(world)
        .filter(|(_, m)| m.instance == instance)
        .map(|(g, _)| g.0)
        .collect();
    out.sort();
    out
}

#[test]
fn inline_instances_spawn_stable_distinct_guids_with_filters_and_transforms() {
    let mut app = app_with(&[(PROP, prop_level()), (HOST, host_level())]);
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("instance 1");
    let i2 = by_guid(&mut app, I2).expect("instance 2");
    let mut expected1 = vec![derive_guid(I1, R), derive_guid(I1, C), derive_guid(I1, D)];
    expected1.sort();
    assert_eq!(members_of(&mut app, i1), expected1);
    let mut expected2 = vec![derive_guid(I2, R), derive_guid(I2, C)];
    expected2.sort();
    assert_eq!(members_of(&mut app, i2), expected2, "debug layer excluded");
    assert_eq!(app.world().get::<PlacedSpawned>(i1).map(|s| s.entities), Some(3));

    // The instance transform applies: C sits at R(1,0) + C(0,2) + I1(10,0).
    let c1 = by_guid(&mut app, derive_guid(I1, C)).expect("c in instance 1");
    let pos = app.world().get::<GlobalTransform>(c1).map(|g| g.translation());
    assert_eq!(pos.map(|p| (p.x, p.y)), Some((11.0, 2.0)));

    // Resources of the placed level are ignored: the host's settings win.
    assert!(app.world().resource::<LevelSettings>().gravity.y != -1.0);
    // Members inherit the instance's layers and keep their own.
    let d1 = by_guid(&mut app, derive_guid(I1, D)).expect("d");
    let layers = app.world().get::<LayerMembership>(d1).map(|m| m.layers.clone());
    assert_eq!(layers, Some(vec!["debug".to_owned(), "night".to_owned()]));

    // Stable across loads.
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("instance 1 again");
    assert_eq!(members_of(&mut app, i1), expected1);
}

#[test]
fn a_saved_host_holds_its_instances_and_not_their_members() {
    fn members(app: &mut App) -> usize {
        let world = app.world_mut();
        world.query::<&PlacedMember>().iter(world).count()
    }
    let mut app = app_with(&[(PROP, prop_level()), (HOST, host_level())]);
    open(&mut app, HOST);
    let before = members(&mut app);
    assert!(before > 0);

    let saved = snapshot_level(app.world_mut(), HOST).expect("snapshot");
    let guids: Vec<Guid> = saved.scene.entities.iter().map(|record| record.guid).collect();
    assert_eq!(guids, [I1, I2]);

    // Loading what was saved fills each instance once.
    app.world_mut().resource_mut::<LevelLibrary>().insert(saved);
    open(&mut app, HOST);
    assert_eq!(members(&mut app), before);
}

/// Lane v06-editor-b-reds: an entity the host level authors under an inline instance is not a
/// member of the placed level. Before the fix the fill tagged every entity under the instance, so
/// the authored child became a `PlacedMember`: a save dropped it (members are not saved) and a
/// Break listed it with the members (its undo despawned it).
#[test]
fn an_authored_child_of_an_inline_instance_is_not_a_member() {
    const LAMP: Guid = Guid::from_u128(0x31);
    let host = scene(
        &[
            entity(
                I1,
                None,
                &format!("{} {}", transform(10.0, 0.0), placed(PROP, "Inline", "")),
            ),
            entity(LAMP, Some(I1), &transform(0.5, 0.0)),
        ],
        "",
    );
    let mut app = app_with(&[(PROP, prop_level()), (HOST, host)]);
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("instance");
    let lamp = by_guid(&mut app, LAMP).expect("lamp");
    assert!(
        app.world().get::<PlacedMember>(lamp).is_none(),
        "the authored child is not tagged as a member"
    );
    let mut expected = vec![derive_guid(I1, R), derive_guid(I1, C), derive_guid(I1, D)];
    expected.sort();
    assert_eq!(
        members_of(&mut app, i1),
        expected,
        "the members are the placed level's"
    );
    assert_eq!(
        app.world().get::<ChildOf>(lamp).map(ChildOf::parent),
        Some(i1),
        "the lamp stays under the instance"
    );

    // A save keeps the authored child (and not the members).
    let saved = snapshot_level(app.world_mut(), HOST).expect("snapshot");
    let guids: Vec<Guid> = saved.scene.entities.iter().map(|record| record.guid).collect();
    assert_eq!(guids, [I1, LAMP]);

    // Break returns the members only and leaves the lamp alive at the instance's place.
    let broken = break_placed(app.world_mut(), i1).expect("break");
    assert_eq!(broken, expected);
    let lamp = by_guid(&mut app, LAMP).expect("the lamp survives the break");
    assert!(app.world().get::<ChildOf>(lamp).is_none());
}

fn chain_level(id: u128, next: Option<u128>) -> (LevelId, String) {
    let level = LevelId(Guid::from_u128(id));
    let mut entities = vec![entity(Guid::from_u128(id << 8 | 1), None, &transform(0.0, 0.0))];
    if let Some(next) = next {
        entities.push(entity(
            Guid::from_u128(id << 8 | 2),
            None,
            &format!(
                "{} {}",
                transform(0.0, 0.0),
                placed(LevelId(Guid::from_u128(next)), "Inline", "")
            ),
        ));
    }
    (level, scene(&entities, ""))
}

fn host_placing(level: u128) -> (LevelId, String) {
    (
        HOST,
        scene(
            &[
                entity(Guid::from_u128(0x77), None, &transform(0.0, 0.0)),
                entity(
                    I1,
                    None,
                    &format!(
                        "{} {}",
                        transform(0.0, 0.0),
                        placed(LevelId(Guid::from_u128(level)), "Inline", "")
                    ),
                ),
            ],
            "",
        ),
    )
}

fn error_of(app: &mut App) -> Option<&'static str> {
    let i1 = by_guid(app, I1)?;
    app.world().get::<PlacedError>(i1).map(|e| e.reason)
}

#[test]
fn cycles_depth_and_missing_levels_leave_the_host_loading() {
    // A level placing itself through another one.
    let mut app = app_with(&[
        chain_level(0x1000, Some(0x2000)),
        chain_level(0x2000, Some(0x1000)),
        host_placing(0x1000),
    ]);
    open(&mut app, HOST);
    assert_eq!(error_of(&mut app), Some("placed_cycle"));
    assert!(
        by_guid(&mut app, Guid::from_u128(0x77)).is_some(),
        "the host still loads"
    );

    // Nine nested placements: over the limit; nothing spawns for the instance.
    let mut levels: Vec<_> = (1..=9u128)
        .map(|i| chain_level(0x100 * i, (i < 9).then_some(0x100 * (i + 1))))
        .collect();
    levels.push(host_placing(0x100));
    let mut app = app_with(&levels);
    open(&mut app, HOST);
    assert_eq!(error_of(&mut app), Some("placed_limit"));
    let i1 = by_guid(&mut app, I1).expect("instance");
    assert!(members_of(&mut app, i1).is_empty());

    // Eight nested placements are fine.
    let mut levels: Vec<_> = (1..=8u128)
        .map(|i| chain_level(0x100 * i, (i < 8).then_some(0x100 * (i + 1))))
        .collect();
    levels.push(host_placing(0x100));
    let mut app = app_with(&levels);
    open(&mut app, HOST);
    assert_eq!(error_of(&mut app), None);

    // Missing level.
    let mut app = app_with(&[host_placing(0xdead)]);
    open(&mut app, HOST);
    assert_eq!(error_of(&mut app), Some("level_not_found"));
}

/// A level that IS in the library but whose scene fails to spawn reports the spawn reason, not
/// `level_not_found` (audit fix lane, scene correctness finding 4). A truly absent level keeps
/// `level_not_found`.
#[test]
fn a_found_level_that_fails_to_spawn_reports_the_spawn_reason() {
    // Two entities share one guid: the file parses, `validate()` fails at spawn time.
    let bad = LevelId(Guid::from_u128(0xc0));
    let mut level =
        Level::from_ron_str(bad, &scene(&[entity(R, None, &transform(1.0, 0.0))], "")).expect("level parses");
    let twin = level.scene.entities[0].clone();
    level.scene.entities.push(twin);
    assert!(level.scene.validate().is_err(), "the fixture must be invalid");

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    let host = host_placing(0xc0);
    let host = Level::from_ron_str(host.0, &host.1).expect("host parses");
    app.world_mut().resource_mut::<LevelLibrary>().insert(host);
    open(&mut app, HOST);
    assert_eq!(
        error_of(&mut app),
        Some("spawn_failed"),
        "the level was found; the failure is its spawn"
    );
    assert!(
        by_guid(&mut app, Guid::from_u128(0x77)).is_some(),
        "the host still loads"
    );

    // Streamed placement of the same level: the open failure reports the spawn reason too.
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let host = (
        HOST,
        scene(
            &[entity(
                I1,
                None,
                &format!("{} {}", transform(5.0, 0.0), placed(bad, "Streamed", "")),
            )],
            "",
        ),
    );
    // Rebuild the invalid level for this app: the first app still owns it.
    let mut level =
        Level::from_ron_str(bad, &scene(&[entity(R, None, &transform(1.0, 0.0))], "")).expect("level parses");
    let twin = level.scene.entities[0].clone();
    level.scene.entities.push(twin);
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    let host = Level::from_ron_str(host.0, &host.1).expect("host parses");
    app.world_mut().resource_mut::<LevelLibrary>().insert(host);
    open(&mut app, HOST);
    assert_eq!(
        error_of(&mut app),
        Some("spawn_failed"),
        "the streamed level was found; the failure is its open"
    );

    // A level that is not in the library at all keeps `level_not_found`.
    let mut app = app_with(&[host_placing(0xdead)]);
    open(&mut app, HOST);
    assert_eq!(error_of(&mut app), Some("level_not_found"));
}

#[test]
fn streamed_instances_load_after_the_owner_and_unload_with_it() {
    let host = (
        HOST,
        scene(
            &[entity(
                I1,
                None,
                &format!("{} {}", transform(5.0, 0.0), placed(PROP, "Streamed", "")),
            )],
            "",
        ),
    );
    let mut app = app_with(&[(PROP, prop_level()), host]);
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("owner");
    assert_eq!(
        app.world().resource::<LevelManager>().state(PROP),
        Some(regia_scene::level::LevelState::Active)
    );
    let spawned = app.world().get::<PlacedSpawned>(i1).copied().expect("filled");
    assert_eq!(spawned.mode, PlacedMode::Streamed);
    let loaded: Vec<PlacedLevelLoaded> = app
        .world_mut()
        .resource_mut::<Messages<PlacedLevelLoaded>>()
        .drain()
        .collect();
    assert_eq!(
        loaded,
        vec![PlacedLevelLoaded {
            owner: i1,
            level: PROP
        }]
    );
    let r = by_guid(&mut app, R).expect("streamed root keeps its own guid");
    assert_eq!(app.world().get::<ChildOf>(r).map(ChildOf::parent), Some(i1));
    let c = by_guid(&mut app, C).expect("c");
    let pos = app.world().get::<GlobalTransform>(c).map(|g| g.translation());
    assert_eq!(pos.map(|p| (p.x, p.y)), Some((6.0, 2.0)));

    app.world_mut().entity_mut(i1).despawn();
    app.update();
    assert_eq!(
        app.world().resource::<LevelManager>().state(PROP),
        None,
        "unloaded with its owner"
    );
}

#[test]
fn break_keeps_guids_and_world_positions_and_leaves_the_source_alone() {
    let mut app = app_with(&[(PROP, prop_level()), (HOST, host_level())]);
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("instance");
    let before = members_of(&mut app, i1);
    let guids = break_placed(app.world_mut(), i1).expect("break");
    assert_eq!(guids, before);
    app.update();
    assert!(app.world().get_entity(i1).is_err(), "instance entity removed");
    let c = by_guid(&mut app, derive_guid(I1, C)).expect("kept guid");
    assert!(app.world().get::<PlacedMember>(c).is_none());
    let pos = app.world().get::<GlobalTransform>(c).map(|g| g.translation());
    assert_eq!(pos.map(|p| (p.x, p.y)), Some((11.0, 2.0)));
    let source = app
        .world()
        .resource::<LevelLibrary>()
        .get(PROP)
        .expect("source")
        .scene
        .entities
        .len();
    assert_eq!(source, 3);
    // Breaking a plain entity is `not_placed`.
    let err = break_placed(app.world_mut(), c).expect_err("plain");
    assert_eq!(err.reason(), "not_placed");
}

/// Finding 2 (scene determinism audit, medium): inline member guids derive from the instance's
/// `EntityGuid`. Falling back to the process-local `Entity` bits made every member id depend on
/// how many entities were spawned before the placement, so two peers (one of which spawned an
/// extra client-only entity) derived different ids. A guid-less instance is now refused with
/// `missing_instance_guid` on both peers, and the same guid yields the same member set.
#[test]
fn inline_instances_without_a_guid_error_instead_of_using_entity_bits() {
    const INSTANCE: Guid = Guid::from_u128(0x5150);
    let mut expected = vec![
        derive_guid(INSTANCE, R),
        derive_guid(INSTANCE, C),
        derive_guid(INSTANCE, D),
    ];
    expected.sort();

    // Without a guid: refused on both peers, regardless of allocator state, and nothing spawns.
    let setup_bare = |extra_entity: bool| {
        let mut app = app_with(&[(PROP, prop_level())]);
        if extra_entity {
            app.world_mut().spawn_empty(); // a client-only camera on one peer only
        }
        let instance = app
            .world_mut()
            .spawn((PlacedLevel::inline(PROP), Transform::default()))
            .id();
        app.update();
        (app, instance)
    };
    for (mut app, instance) in [setup_bare(false), setup_bare(true)] {
        let err = app
            .world()
            .get::<PlacedError>(instance)
            .expect("a guid-less instance is refused");
        assert_eq!(err.reason, "missing_instance_guid");
        assert!(members_of(&mut app, instance).is_empty());
    }

    // With the same guid on both peers the member sets match, even though one peer spawned an
    // extra entity before the placement.
    let setup_guided = |extra_entity: bool| {
        let mut app = app_with(&[(PROP, prop_level())]);
        if extra_entity {
            app.world_mut().spawn_empty();
        }
        let instance = app
            .world_mut()
            .spawn((
                EntityGuid(INSTANCE),
                PlacedLevel::inline(PROP),
                Transform::default(),
            ))
            .id();
        app.update();
        (app, instance)
    };
    let (mut app_a, instance_a) = setup_guided(false);
    let (mut app_b, instance_b) = setup_guided(true);
    assert!(app_a.world().get::<PlacedError>(instance_a).is_none());
    assert!(app_b.world().get::<PlacedError>(instance_b).is_none());
    assert_eq!(members_of(&mut app_a, instance_a), expected);
    assert_eq!(members_of(&mut app_b, instance_b), expected);
}

/// Spec 37 § 6.4: the total entity budget of one instance (100,000, every nesting level
/// counted) and a cycle's error naming the whole chain.
#[test]
fn the_entity_budget_counts_every_nesting_level_and_a_cycle_names_its_chain() {
    use regia_scene::placed::{MAX_PLACED_ENTITIES, check_placement};
    const PIECE: LevelId = LevelId(Guid::from_u128(0xc0));
    const MANY: LevelId = LevelId(Guid::from_u128(0xc1));
    const FEWER: LevelId = LevelId(Guid::from_u128(0xc2));
    // A piece of 1,000 entities; one level places it 101 times (101 + 101,000 entities), the
    // other 99 times (99 + 99,000).
    let piece: Vec<String> = (0..1000u128)
        .map(|i| entity(Guid::from_u128(0xc000_0000 + i), None, ""))
        .collect();
    let placing = |count: u128, base: u128| -> String {
        let records: Vec<String> = (0..count)
            .map(|i| entity(Guid::from_u128(base + i), None, &placed(PIECE, "Inline", "")))
            .collect();
        scene(&records, "")
    };
    let app = app_with(&[
        (PIECE, scene(&piece, "")),
        (MANY, placing(101, 0xd000_0000)),
        (FEWER, placing(99, 0xe000_0000)),
    ]);
    let library = app.world().resource::<LevelLibrary>();
    let err = check_placement(library, MANY, &[HOST]).expect_err("over the budget");
    assert_eq!(err.reason(), "placed_limit");
    assert!(
        err.to_string().contains(&MAX_PLACED_ENTITIES.to_string()),
        "{err}"
    );
    assert_eq!(
        check_placement(library, FEWER, &[HOST]).expect("under the budget"),
        99 + 99 * 1000
    );

    // A -> B -> A: the error names the host and both levels, in order.
    let app = app_with(&[
        chain_level(0x1000, Some(0x2000)),
        chain_level(0x2000, Some(0x1000)),
    ]);
    let library = app.world().resource::<LevelLibrary>();
    let a = LevelId(Guid::from_u128(0x1000));
    let err = check_placement(library, a, &[HOST]).expect_err("cycle");
    assert_eq!(err.reason(), "placed_cycle");
    let chain = format!(
        "{} -> {} -> {} -> {}",
        HOST.0,
        Guid::from_u128(0x1000),
        Guid::from_u128(0x2000),
        Guid::from_u128(0x1000)
    );
    assert!(err.to_string().contains(&chain), "{err}");
}

/// ADR-1667 (n): a level streams through one instance at a time; a second streamed instance of
/// the same level is `placed_limit` and spawns nothing, while the first one is filled.
#[test]
fn a_second_streamed_instance_of_one_level_is_refused() {
    let host = (
        HOST,
        scene(
            &[
                entity(
                    I1,
                    None,
                    &format!("{} {}", transform(5.0, 0.0), placed(PROP, "Streamed", "")),
                ),
                entity(
                    I2,
                    None,
                    &format!("{} {}", transform(-5.0, 0.0), placed(PROP, "Streamed", "")),
                ),
            ],
            "",
        ),
    );
    let mut app = app_with(&[(PROP, prop_level()), host]);
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("i1");
    let i2 = by_guid(&mut app, I2).expect("i2");
    let state = |app: &App, e: Entity| {
        (
            app.world().get::<PlacedSpawned>(e).is_some(),
            app.world().get::<PlacedError>(e).map(|err| err.reason),
        )
    };
    let mut both = [state(&app, i1), state(&app, i2)];
    both.sort();
    assert_eq!(
        both,
        [(false, Some("placed_limit")), (true, None)],
        "one filled, one refused"
    );
    assert_eq!(
        app.world().resource::<LevelManager>().state(PROP),
        Some(regia_scene::level::LevelState::Active)
    );
}

/// Finding scene-ecs 2 (v0.5.0 rc1): every entity of a streamed placed level (not only its roots)
/// is a member of its instance, so a full-world snapshot (the editor's undo and autosave path)
/// writes none of them; written, they collide with the still-open streamed level on respawn.
#[test]
fn a_full_world_snapshot_skips_every_entity_of_a_streamed_placement() {
    let host = (
        HOST,
        scene(
            &[entity(
                I1,
                None,
                &format!("{} {}", transform(5.0, 0.0), placed(PROP, "Streamed", "")),
            )],
            "",
        ),
    );
    let mut app = app_with(&[(PROP, prop_level()), host]);
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("owner");
    let mut expected = vec![R, C, D];
    expected.sort();
    assert_eq!(
        members_of(&mut app, i1),
        expected,
        "the whole streamed level is a member"
    );

    let filter = regia_scene::spawn::SnapshotFilter {
        level: None,
        resources: Vec::new(),
    };
    let saved = snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    let guids: Vec<Guid> = saved.entities.iter().map(|record| record.guid).collect();
    assert_eq!(guids, [I1], "only the instance is written");
}

/// Found next to scene-ecs finding 2 (v0.5.0 rc1): a streamed placement's file is authored
/// relative to its instance. With the floating origin moved, the level open must not shift its
/// roots into local space as well: they go under the instance, which is already there.
#[test]
fn a_streamed_placement_after_an_origin_rebase_sits_on_its_instance() {
    let host = (
        HOST,
        scene(
            &[entity(
                I1,
                None,
                &format!("{} {}", transform(5.0, 0.0), placed(PROP, "Streamed", "")),
            )],
            "",
        ),
    );
    let mut app = app_with(&[(PROP, prop_level()), host]);
    app.insert_resource(regia_scene::streaming::OriginOffset([1000.0, 0.0, 0.0]));
    open(&mut app, HOST);
    let i1 = by_guid(&mut app, I1).expect("owner");
    let at = |app: &App, e: Entity| {
        app.world()
            .get::<GlobalTransform>(e)
            .map(|g| (g.translation().x, g.translation().y))
    };
    // The host's root opens in local space: 5 - 1000.
    assert_eq!(at(&app, i1), Some((-995.0, 0.0)));
    // C sits at I1 + R(1, 0) + C(0, 2), as without a rebase.
    let c = by_guid(&mut app, C).expect("c");
    assert_eq!(at(&app, c), Some((-994.0, 2.0)));
}

/// Finding scene-ecs 3 (v0.5.0 rc1): the placement walk sees placements authored as added children
/// of a prefab instance: they count toward the budget (with the added child itself), and a cycle
/// through one is refused before anything spawns.
#[test]
fn the_placement_walk_sees_placements_in_prefab_added_children() {
    use regia_scene::placed::check_placement;
    const PIECE: LevelId = LevelId(Guid::from_u128(0xc0));
    const OUTER: LevelId = LevelId(Guid::from_u128(0xc3));
    const LOOP: LevelId = LevelId(Guid::from_u128(0xc4));
    let piece: Vec<String> = (0..10u128)
        .map(|i| entity(Guid::from_u128(0xc000_0000 + i), None, ""))
        .collect();
    // One prefab-instance record whose added child places `level`.
    let host_of = |level: LevelId, record: u128, child: u128| -> String {
        format!(
            "SceneFile(format_version: {v}, entities: [(guid: \"{record}\", components: {{}}, prefab_instance: (prefab: \"asset:{prefab}\", added_children: [(guid: \"{child}\", parent: Some(\"{record}\"), components: {{ {placed} }})])),], resources: {{}})",
            v = SCENE_FORMAT_VERSION,
            record = Guid::from_u128(record),
            prefab = Guid::from_u128(0xfab),
            child = Guid::from_u128(child),
            placed = placed(level, "Inline", ""),
        )
    };
    let app = app_with(&[
        (PIECE, scene(&piece, "")),
        (OUTER, host_of(PIECE, 0xf000, 0xf001)),
        (LOOP, host_of(LOOP, 0xf100, 0xf101)),
    ]);
    let library = app.world().resource::<LevelLibrary>();
    assert_eq!(
        check_placement(library, OUTER, &[HOST]).expect("under the budget"),
        1 + 1 + 10,
        "the record, its added child and the level the child places"
    );
    let err = check_placement(library, LOOP, &[HOST]).expect_err("cycle");
    assert_eq!(err.reason(), "placed_cycle");
}

/// With every entity of a streamed placement a member of its instance, a snapshot of the streamed
/// level itself still holds its whole hierarchy (its roots too, which were left out before).
#[test]
fn a_snapshot_of_a_streamed_level_keeps_its_own_entities() {
    let host = (
        HOST,
        scene(
            &[entity(
                I1,
                None,
                &format!("{} {}", transform(5.0, 0.0), placed(PROP, "Streamed", "")),
            )],
            "",
        ),
    );
    let mut app = app_with(&[(PROP, prop_level()), host]);
    open(&mut app, HOST);
    let saved = snapshot_level(app.world_mut(), PROP).expect("snapshot");
    let records: Vec<(Guid, Option<Guid>)> = saved
        .scene
        .entities
        .iter()
        .map(|record| (record.guid, record.parent))
        .collect();
    assert_eq!(records, [(R, None), (C, Some(R)), (D, Some(R))]);
    let host = snapshot_level(app.world_mut(), HOST).expect("host snapshot");
    let guids: Vec<Guid> = host.scene.entities.iter().map(|record| record.guid).collect();
    assert_eq!(guids, [I1], "the host holds only the instance");
}
