// SPDX-License-Identifier: MPL-2.0
//! Data layers end to end (spec 37 § 10 test 6): initial states at load, the
//! `Absent -> Parked -> Live -> Parked -> Absent` cycle, recursion, parent clamping, the
//! lowest-state rule for multi-layer entities, determinism of ids and `LayerChanged` order.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_scene::layers::{
    LEVEL_LAYERS_PATH, LayerChanged, LayerDefs, LayerDefsLibrary, LayerMembership, LayerParked, LayerPeers,
    LayerState, LayerTable, LevelLayers, Side, validate_level,
};
use regia_scene::level::{Level, LevelId, LevelLibrary, LevelMember, LoadMode, open_level};
use regia_scene::prelude::*;

const DEFS_ID: Guid = Guid::from_u128(0xd0);
const LEVEL: LevelId = LevelId(Guid::from_u128(0x1e));

const ROOT: u128 = 0x100;
const A1: u128 = 0x201;
const A2: u128 = 0x202;
const BOSS: u128 = 0x301;
const NIGHT: u128 = 0x401;
const BOTH: u128 = 0x501;

fn guid(v: u128) -> Guid {
    Guid::from_u128(v)
}

fn defs() -> LayerDefs {
    LayerDefs::from_ron_str(
        r#"LayerDefs(format_version: 1, layers: [
            (name: "quest_a", kind: Runtime, debug_color: (0.8, 0.3, 0.1), parent: None),
            (name: "quest_a_boss", kind: Runtime, debug_color: (0.9, 0.1, 0.1), parent: Some("quest_a")),
            (name: "night", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
        ])"#,
    )
    .expect("defs")
}

fn record(id: u128, parent: Option<u128>, layers: &[&str]) -> String {
    let parent = parent
        .map(|p| format!("parent: Some(\"{}\"),", guid(p)))
        .unwrap_or_default();
    let membership = if layers.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = layers.iter().map(|l| format!("\"{l}\"")).collect();
        format!(
            "\"regia_scene::layers::LayerMembership\": (layers: [{}]),",
            names.join(", ")
        )
    };
    format!(
        r#"(guid: "{}", {parent} components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            {membership}
        }}),"#,
        guid(id)
    )
}

fn level_text() -> String {
    format!(
        r#"SceneFile(format_version: {v}, entities: [
            {root} {a1} {a2} {boss} {night} {both}
        ], resources: {{
            "regia_scene::layers::LevelLayers": (
                defs: ["asset:{defs}"],
                entries: [
                    (layer: "night", initial: Absent, editor_visible: true, editor_resident: true, peers: Everyone),
                    (layer: "quest_a", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone),
                ],
            ),
        }})"#,
        v = SCENE_FORMAT_VERSION,
        root = record(ROOT, None, &[]),
        a1 = record(A1, Some(ROOT), &["quest_a"]),
        a2 = record(A2, Some(ROOT), &["quest_a"]),
        boss = record(BOSS, Some(A1), &["quest_a", "quest_a_boss"]),
        night = record(NIGHT, None, &["night"]),
        both = record(BOTH, None, &["quest_a", "night"]),
        defs = DEFS_ID,
    )
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs());
    let level = Level::from_ron_str(LEVEL, &level_text()).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    app.update();
    app
}

/// `(guid, entity, parked)` of every spawned guid-carrying entity, by guid.
fn spawned(app: &mut App) -> Vec<(u128, Entity, bool)> {
    let world = app.world_mut();
    let mut query = world.query::<(Entity, &EntityGuid, Option<&LayerParked>)>();
    let mut out: Vec<_> = query
        .iter(world)
        .map(|(e, g, p)| (g.0.as_u128(), e, p.is_some()))
        .collect();
    out.sort();
    out
}

fn guids(app: &mut App) -> Vec<u128> {
    spawned(app).into_iter().map(|(g, ..)| g).collect()
}

fn parked(app: &mut App) -> Vec<u128> {
    spawned(app)
        .into_iter()
        .filter(|(_, _, p)| *p)
        .map(|(g, ..)| g)
        .collect()
}

fn request(app: &mut App, layer: &str, state: LayerState, recursive: bool) -> bool {
    let changed = app
        .world_mut()
        .resource_mut::<LayerTable>()
        .request(layer, state, recursive, Side::Local)
        .expect("known layer");
    app.update();
    changed
}

fn drain_changes(app: &mut App) -> Vec<(String, LayerState, LayerState)> {
    app.world_mut()
        .resource_mut::<Messages<LayerChanged>>()
        .drain()
        .map(|m| (m.layer, m.from, m.to))
        .collect()
}

#[test]
fn initial_states_apply_at_load() {
    let mut app = app();
    assert_eq!(
        guids(&mut app),
        vec![ROOT, A1, A2, BOSS],
        "night members are absent at load"
    );
    let table = app.world().resource::<LayerTable>();
    assert_eq!(table.effective("night"), Some(LayerState::Absent));
    assert_eq!(table.effective("quest_a_boss"), Some(LayerState::Live));
    let night = table.layers().find(|l| l.name == "night").expect("night");
    assert_eq!(night.member_count, 2);
}

#[test]
fn absent_parked_live_parked_absent_cycle() {
    let mut app = app();
    assert!(request(&mut app, "night", LayerState::Parked, false));
    assert_eq!(guids(&mut app), vec![ROOT, A1, A2, BOSS, NIGHT, BOTH]);
    assert_eq!(parked(&mut app), vec![NIGHT, BOTH]);
    let night_entity = spawned(&mut app)
        .into_iter()
        .find(|(g, ..)| *g == NIGHT)
        .map(|(_, e, _)| e);
    let hidden = night_entity.and_then(|e| app.world().get::<Visibility>(e).copied());
    assert_eq!(hidden, Some(Visibility::Hidden));

    assert!(request(&mut app, "night", LayerState::Live, false));
    assert!(parked(&mut app).is_empty());
    // Parked <-> Live toggles only: same entities.
    let after_live = night_entity.and_then(|e| app.world().get::<Visibility>(e).copied());
    assert_eq!(after_live, Some(Visibility::Inherited));

    assert!(request(&mut app, "night", LayerState::Parked, false));
    assert_eq!(parked(&mut app), vec![NIGHT, BOTH]);
    assert!(request(&mut app, "night", LayerState::Absent, false));
    assert_eq!(guids(&mut app), vec![ROOT, A1, A2, BOSS]);
    assert!(
        !request(&mut app, "night", LayerState::Absent, false),
        "already absent"
    );
}

#[test]
fn children_are_clamped_by_parents_and_recursion_reaches_descendants() {
    let mut app = app();
    // Parent parked: the boss (child layer) is parked too, though it still requests Live.
    request(&mut app, "quest_a", LayerState::Parked, false);
    assert_eq!(parked(&mut app), vec![A1, A2, BOSS]);
    let table = app.world().resource::<LayerTable>();
    assert_eq!(table.requested("quest_a_boss"), Some(LayerState::Live));
    assert_eq!(table.effective("quest_a_boss"), Some(LayerState::Parked));

    request(&mut app, "quest_a", LayerState::Live, false);
    assert!(parked(&mut app).is_empty());
    // Recursive absent: both layers requested Absent.
    request(&mut app, "quest_a", LayerState::Absent, true);
    assert_eq!(guids(&mut app), vec![ROOT]);
    assert_eq!(
        app.world().resource::<LayerTable>().requested("quest_a_boss"),
        Some(LayerState::Absent)
    );
    // Non-recursive live on the parent: the boss stays absent (it requested Absent itself).
    request(&mut app, "quest_a", LayerState::Live, false);
    assert_eq!(guids(&mut app), vec![ROOT, A1, A2]);
    request(&mut app, "quest_a_boss", LayerState::Live, false);
    assert_eq!(guids(&mut app), vec![ROOT, A1, A2, BOSS]);
    // The respawned boss is parented to the live A1 again.
    let (a1, boss) = {
        let s = spawned(&mut app);
        let find = |g| {
            s.iter()
                .find(|(x, ..)| *x == g)
                .map(|(_, e, _)| *e)
                .expect("spawned")
        };
        (find(A1), find(BOSS))
    };
    assert_eq!(app.world().get::<ChildOf>(boss).map(ChildOf::parent), Some(a1));
}

#[test]
fn a_multi_layer_entity_follows_its_lowest_layer() {
    let mut app = app();
    request(&mut app, "night", LayerState::Live, false);
    assert!(guids(&mut app).contains(&BOTH));
    request(&mut app, "quest_a", LayerState::Parked, false);
    assert!(
        parked(&mut app).contains(&BOTH),
        "quest_a parked, night live: parked"
    );
    request(&mut app, "night", LayerState::Absent, false);
    assert!(!guids(&mut app).contains(&BOTH), "night absent: absent");
}

#[test]
fn the_same_sequence_gives_the_same_ids_and_events() {
    let run = || {
        let mut app = app();
        let mut log = Vec::new();
        for (layer, state, recursive) in [
            ("night", LayerState::Parked, false),
            ("quest_a", LayerState::Absent, true),
            ("night", LayerState::Live, false),
            ("quest_a", LayerState::Live, true),
            ("night", LayerState::Absent, false),
        ] {
            request(&mut app, layer, state, recursive);
            log.push((spawned(&mut app), drain_changes(&mut app)));
        }
        log
    };
    let first = run();
    assert_eq!(first, run());
    // Events are in layer-name order within a frame.
    let (_, events) = &first[1];
    assert_eq!(
        events.iter().map(|(l, ..)| l.as_str()).collect::<Vec<_>>(),
        vec!["quest_a", "quest_a_boss"]
    );
}

#[test]
fn validation_reports_unbound_layers_and_parent_mismatch() {
    let level = Level::from_ron_str(LEVEL, &level_text()).expect("level");
    let defs = defs();
    // BOSS (child of A1) is in quest_a and quest_a_boss: no mismatch. Add a child of A1 with no layers.
    let mut scene = level.scene.clone();
    let mut orphan = scene.entities[0].clone();
    orphan.guid = guid(0x999);
    orphan.parent = Some(guid(A1));
    orphan.components.remove("regia_scene::layers::LayerMembership");
    scene.entities.push(orphan);
    let problems = validate_level(&scene, &[&defs]);
    assert!(
        problems.iter().any(|p| p.reason == "layer_parent_mismatch"),
        "{problems:?}"
    );
    let empty = LayerDefs {
        format_version: 1,
        layers: Vec::new(),
    };
    assert!(
        validate_level(&level.scene, &[&empty])
            .iter()
            .any(|p| p.reason == "layer_unknown")
    );
    assert!(
        validate_level(&level.scene, &[&defs, &defs])
            .iter()
            .any(|p| p.reason == "layer_ambiguous")
    );
    // Levels audit L5: a membership of the wrong type reads as "no layers" at run time, so the
    // entity stays visible when its layer is `Absent`; the validator names it.
    let mut wrong = level.scene.clone();
    let key = "regia_scene::layers::LayerMembership";
    let at = wrong
        .entities
        .iter()
        .position(|e| e.components.contains_key(key))
        .expect("a member");
    let raw = ron::value::RawValue::from_boxed_ron("(layers: \"night\")".into()).expect("raw");
    wrong.entities[at].components.insert(key.to_string(), raw);
    let problems = validate_level(&wrong, &[&defs]);
    let found = problems
        .iter()
        .find(|p| p.reason == "layer_membership_invalid")
        .expect("the bad membership");
    assert!(!found.fatal);
    assert!(
        found.detail.contains(&wrong.entities[at].guid.to_string()),
        "{found:?}"
    );
}

/// The parked marker of an entity.
fn marker(app: &App, entity: Entity) -> Option<LayerParked> {
    app.world().get::<LayerParked>(entity).copied()
}

fn entity_of(app: &mut App, id: u128) -> Entity {
    spawned(app)
        .into_iter()
        .find(|(guid, ..)| *guid == id)
        .expect("spawned")
        .1
}

#[test]
fn descendants_without_a_membership_follow_their_member() {
    let mut app = app();
    let member = entity_of(&mut app, A2);
    // Model parts arrive at run time: they carry no membership of their own.
    let part = app.world_mut().spawn(ChildOf(member)).id();
    let tip = app.world_mut().spawn(ChildOf(part)).id();

    request(&mut app, "quest_a", LayerState::Parked, false);
    app.update();
    assert!(
        marker(&app, tip).is_some_and(|m| m.inherited),
        "the subtree follows"
    );
    assert!(marker(&app, part).is_some_and(|m| m.inherited));
    assert!(!marker(&app, member).expect("member").inherited);

    // Attached while the member is parked: follows on the next frame.
    let late = app.world_mut().spawn(ChildOf(part)).id();
    app.update();
    assert!(marker(&app, late).is_some_and(|m| m.inherited));

    // Moved out of the subtree: stops following.
    app.world_mut().entity_mut(late).remove::<ChildOf>();
    app.update();
    assert!(marker(&app, late).is_none());

    request(&mut app, "quest_a", LayerState::Live, false);
    app.update();
    for entity in [member, part, tip] {
        assert!(marker(&app, entity).is_none());
    }
}

#[test]
fn a_descendant_with_a_membership_governs_itself() {
    let mut app = app();
    let parent = entity_of(&mut app, A2);
    let mut member_child = |layer: &str, id: u128| {
        app.world_mut()
            .spawn((
                EntityGuid(guid(id)),
                LayerMembership {
                    layers: vec![layer.into()],
                },
                ChildOf(parent),
            ))
            .id()
    };
    let clamped = member_child("quest_a_boss", 0x901);
    let live = member_child("night", 0x902);

    request(&mut app, "night", LayerState::Live, false);
    request(&mut app, "quest_a", LayerState::Parked, false);
    app.update();
    assert!(marker(&app, parent).is_some(), "the parent member is parked");
    assert!(
        marker(&app, clamped).is_some_and(|m| !m.inherited),
        "its own layer parks it, clamped by that layer's parent"
    );
    assert!(
        marker(&app, live).is_none(),
        "a parked parent does not park a child whose own layer is Live"
    );

    // Going Live clears only what the parent's switch owns.
    request(&mut app, "night", LayerState::Parked, false);
    request(&mut app, "quest_a", LayerState::Live, false);
    app.update();
    assert!(marker(&app, parent).is_none());
    assert!(
        marker(&app, live).is_some_and(|m| !m.inherited),
        "the child stays parked by its own layer"
    );
}

#[test]
fn reopening_a_level_under_its_id_starts_its_layers_over() {
    let mut app = app();
    let initial_guids = guids(&mut app);
    let initial_parked = parked(&mut app);
    request(&mut app, "quest_a", LayerState::Absent, false);
    request(&mut app, "night", LayerState::Live, false);
    app.update();
    assert_ne!(guids(&mut app), initial_guids);

    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("reopen");
    app.update();
    assert_eq!(guids(&mut app), initial_guids);
    assert_eq!(parked(&mut app), initial_parked);
    let table = app.world().resource::<LayerTable>();
    assert_eq!(table.requested("night"), Some(LayerState::Absent));
    assert_eq!(table.requested("quest_a"), Some(LayerState::Live));
}

#[test]
fn a_level_carrying_its_definitions_needs_no_library() {
    let mut level = Level::from_ron_str(LEVEL, &level_text()).expect("level");
    let mut binding = LevelLayers::from_scene(&level.scene)
        .expect("bound")
        .expect("parses");
    binding.defs.clear();
    binding.embedded = vec![defs()];
    let raw = ron::to_string(&binding).expect("ron");
    level.scene.resources.insert(
        LEVEL_LAYERS_PATH.to_owned(),
        ron::value::RawValue::from_boxed_ron(raw.into_boxed_str()).expect("raw"),
    );
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    app.update();
    let table = app.world().resource::<LayerTable>();
    assert!(table.warnings().is_empty(), "{:?}", table.warnings());
    assert_eq!(table.effective("night"), Some(LayerState::Absent));
    assert!(!guids(&mut app).contains(&NIGHT));
}

// A parent in `night` with a plain template child, and children in `quest_a` on both sides of the
// parent's guid: the despawn takes them all, and processing order must not decide who returns.
const PARENT: u128 = 0x700;
const LOW_CHILD: u128 = 0x600;
const PLAIN_CHILD: u128 = 0x701;
const HIGH_CHILD: u128 = 0x800;

fn family_app() -> App {
    let text = format!(
        r#"SceneFile(format_version: {v}, entities: [{parent} {low} {plain} {high}], resources: {{
            "regia_scene::layers::LevelLayers": (
                defs: ["asset:{defs}"],
                entries: [
                    (layer: "night", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone),
                    (layer: "quest_a", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone),
                ],
            ),
        }})"#,
        v = SCENE_FORMAT_VERSION,
        parent = record(PARENT, None, &["night"]),
        low = record(LOW_CHILD, Some(PARENT), &["quest_a"]),
        plain = record(PLAIN_CHILD, Some(PARENT), &[]),
        high = record(HIGH_CHILD, Some(PARENT), &["quest_a"]),
        defs = DEFS_ID,
    );
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs());
    let level = Level::from_ron_str(LEVEL, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    app.update();
    app
}

#[test]
fn a_despawned_member_brings_back_its_whole_template_subtree() {
    let mut app = family_app();
    assert_eq!(guids(&mut app), vec![LOW_CHILD, PARENT, PLAIN_CHILD, HIGH_CHILD]);
    assert!(request(&mut app, "night", LayerState::Absent, false));
    assert!(guids(&mut app).is_empty(), "the despawn takes the subtree");

    assert!(request(&mut app, "night", LayerState::Live, false));
    assert_eq!(
        guids(&mut app),
        vec![LOW_CHILD, PARENT, PLAIN_CHILD, HIGH_CHILD],
        "a plain child and a member child return, on either side of the parent's guid"
    );
    let rows = spawned(&mut app);
    let parent = rows.iter().find(|row| row.0 == PARENT).map(|row| row.1);
    for (id, entity, _) in rows {
        if id != PARENT {
            let link = app.world().get::<ChildOf>(entity).map(ChildOf::parent);
            assert_eq!(link, parent, "child {id:#x} is back under its parent");
        }
    }
}

#[test]
fn a_member_with_no_template_record_parks_instead_of_vanishing() {
    let mut app = app();
    const RUNTIME: u128 = 0x901;
    app.world_mut().spawn((
        Transform::default(),
        EntityGuid(guid(RUNTIME)),
        LevelMember::from_level(LEVEL),
        LayerMembership {
            layers: vec!["quest_a".to_owned()],
        },
    ));
    assert!(request(&mut app, "quest_a", LayerState::Absent, false));
    assert!(guids(&mut app).contains(&RUNTIME), "nothing could respawn it");
    assert!(parked(&mut app).contains(&RUNTIME));

    assert!(request(&mut app, "quest_a", LayerState::Live, false));
    assert!(guids(&mut app).contains(&RUNTIME));
    assert!(!parked(&mut app).contains(&RUNTIME));
}

/// A level whose scene the caller spawned itself (play-in-editor) and only recorded.
fn caller_spawned_app(table: Option<LayerTable>, bind: bool) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs());
    let level = Level::from_ron_str(LEVEL, &level_text()).expect("level");
    let opts = regia_scene::spawn::SpawnOptions {
        level: Some(LEVEL),
        persistent: false,
        unknown_types: regia_scene::spawn::UnknownTypePolicyOption::Preserve,
    };
    regia_scene::spawn::spawn_scene(app.world_mut(), &level.scene, &opts).expect("spawn");
    let mut manager = app
        .world_mut()
        .remove_resource::<LevelManager>()
        .expect("manager");
    manager.record_spawned(&level);
    app.world_mut().insert_resource(manager);
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    if let Some(mut table) = table {
        if bind {
            table.bind_to_active_levels(app.world());
        }
        app.world_mut().insert_resource(table);
    }
    app.update();
    app
}

#[test]
fn a_recorded_level_despawns_and_respawns_its_absent_members_like_an_opened_one() {
    let mut app = caller_spawned_app(None, false);
    let manager = app.world().resource::<LevelManager>();
    assert_eq!(manager.current(), Some(LEVEL));
    assert_eq!(manager.state(LEVEL), Some(regia_scene::level::LevelState::Active));
    assert!(manager.load_generation(LEVEL) > 0);
    // The table is built from the level's binding: night starts absent, so its members go.
    assert_eq!(guids(&mut app), vec![ROOT, A1, A2, BOSS], "despawned, not parked");
    assert!(parked(&mut app).is_empty());
    assert_eq!(app.world().resource::<LayerTable>().despawned_count(), 2);

    assert!(request(&mut app, "night", LayerState::Live, false));
    assert_eq!(
        guids(&mut app),
        vec![ROOT, A1, A2, BOSS, NIGHT, BOTH],
        "respawned from the library"
    );
    assert_eq!(app.world().resource::<LayerTable>().despawned_count(), 0);
    let night = entity_of(&mut app, NIGHT);
    assert_eq!(
        app.world().get::<LevelMember>(night).map(LevelMember::level),
        Some(LEVEL)
    );
}

#[test]
fn a_bound_table_keeps_its_layers_instead_of_rebuilding_them() {
    // A layer the level's binding does not have survives only when the table is bound.
    let table = || {
        let mut table = LayerTable::default();
        table.insert_layer("night", None, LayerState::Absent, LayerPeers::Everyone);
        table.insert_layer("extra", None, LayerState::Live, LayerPeers::Everyone);
        table
    };
    let mut app = caller_spawned_app(Some(table()), true);
    let bound = app.world().resource::<LayerTable>();
    assert!(bound.contains("extra"), "bound: not rebuilt");
    assert!(!bound.contains("quest_a"), "bound: the binding was not read");
    assert_eq!(bound.requested("night"), Some(LayerState::Absent));
    assert_eq!(
        guids(&mut app),
        vec![ROOT, A1, A2, BOSS],
        "the night members despawned"
    );

    // The same table unbound is rebuilt from the level's binding on the first frame.
    let mut app = caller_spawned_app(Some(table()), false);
    let rebuilt = app.world().resource::<LayerTable>();
    assert!(!rebuilt.contains("extra"));
    assert!(rebuilt.contains("quest_a"));
    assert_eq!(guids(&mut app), vec![ROOT, A1, A2, BOSS]);
}

/// Spec 37 § 4.1 and § 4.5: every definition-asset reason of `validate_defs`, with its fatality
/// (`layer_bad_name`, `layer_duplicate`, `layer_parent_invalid` are fatal, an editor-only parent
/// of a runtime layer is reported but not fatal), and a valid definition has no problem at all.
#[test]
fn validate_defs_names_each_definition_problem() {
    use regia_scene::layers::validate_defs;
    let reasons = |text: &str| -> Vec<(&'static str, bool)> {
        let defs = LayerDefs::from_ron_str(text).expect("parses");
        let mut out: Vec<(&'static str, bool)> =
            validate_defs(&defs).iter().map(|p| (p.reason, p.fatal)).collect();
        out.sort();
        out
    };
    assert_eq!(reasons("LayerDefs(format_version: 1, layers: [])"), vec![]);
    assert!(validate_defs(&defs()).is_empty(), "the fixture is valid");
    assert_eq!(
        reasons(r#"LayerDefs(format_version: 1, layers: [(name: "Night Props")])"#),
        vec![("layer_bad_name", true)]
    );
    let long = "a".repeat(49);
    assert_eq!(
        reasons(&format!(
            r#"LayerDefs(format_version: 1, layers: [(name: "{long}"), (name: "{}")])"#,
            "a".repeat(48)
        )),
        vec![("layer_bad_name", true)],
        "49 characters is too long, 48 is fine"
    );
    assert_eq!(
        reasons(r#"LayerDefs(format_version: 1, layers: [(name: "night"), (name: "night")])"#),
        vec![("layer_duplicate", true)]
    );
    assert_eq!(
        reasons(r#"LayerDefs(format_version: 1, layers: [(name: "a", parent: Some("ghost"))])"#),
        vec![("layer_parent_invalid", true)]
    );
    assert_eq!(
        reasons(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "a", parent: Some("b")), (name: "b", parent: Some("a")),
            ])"#
        ),
        vec![("layer_parent_invalid", true), ("layer_parent_invalid", true)],
        "both layers of a cycle are their own ancestors"
    );
    assert_eq!(
        reasons(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "blockout", kind: EditorOnly), (name: "props", kind: Runtime, parent: Some("blockout")),
            ])"#
        ),
        vec![("layer_editor_parent", false)]
    );
    // An editor-only child of a runtime layer is fine.
    assert_eq!(
        reasons(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "props", kind: Runtime), (name: "notes", kind: EditorOnly, parent: Some("props")),
            ])"#
        ),
        vec![]
    );
}

/// Spec 37 § 4.3: `any_in_state` / `all_in_state` read effective states (a child clamped by its
/// parent counts at the clamped state); an unknown name never matches, and `all_in_state` of an
/// empty list is false.
#[test]
fn state_queries_read_effective_states_and_refuse_unknown_names() {
    let mut table = LayerTable::default();
    table.insert_layer("quest_a", None, LayerState::Live, LayerPeers::Everyone);
    table.insert_layer(
        "quest_a_boss",
        Some("quest_a"),
        LayerState::Live,
        LayerPeers::Everyone,
    );
    table.insert_layer("night", None, LayerState::Absent, LayerPeers::Everyone);
    assert!(table.all_in_state(&["quest_a", "quest_a_boss"], LayerState::Live));
    assert!(!table.any_in_state(&["night"], LayerState::Live));
    assert!(table.any_in_state(&["night", "quest_a"], LayerState::Live));
    assert!(!table.all_in_state(&["night", "quest_a"], LayerState::Live));
    assert!(
        !table.any_in_state(&["ghost"], LayerState::Absent),
        "unknown never matches"
    );
    assert!(!table.all_in_state(&["quest_a", "ghost"], LayerState::Live));
    assert!(!table.all_in_state(&[], LayerState::Live));
    assert!(!table.any_in_state(&[], LayerState::Live));

    // The parent parks: the child's requested state stays Live, its effective state is Parked.
    assert_eq!(
        table.request("quest_a", LayerState::Parked, false, Side::Local),
        Ok(true)
    );
    assert_eq!(table.requested("quest_a_boss"), Some(LayerState::Live));
    assert!(table.any_in_state(&["quest_a_boss"], LayerState::Parked));
    assert!(!table.any_in_state(&["quest_a_boss"], LayerState::Live));
    // Requesting the state a layer already has is Ok(false); an unknown layer is an error.
    assert_eq!(
        table.request("quest_a", LayerState::Parked, false, Side::Local),
        Ok(false)
    );
    assert!(
        table
            .request("ghost", LayerState::Live, false, Side::Local)
            .is_err()
    );
}

/// Spec 37 § 4.2: the `LayerSwitch` set runs in `PostUpdate` before transform propagation, so a
/// member respawned by a switch has its world transform in the same frame (it would read the
/// identity until the next frame if the switch ran after propagation).
#[test]
fn a_respawned_member_has_its_world_transform_in_the_same_frame() {
    let mut app = app();
    assert!(!guids(&mut app).contains(&NIGHT));
    assert!(request(&mut app, "night", LayerState::Live, false), "one update");
    let night = spawned(&mut app)
        .into_iter()
        .find(|(g, ..)| *g == NIGHT)
        .map(|(_, e, _)| e)
        .expect("respawned");
    let world = app.world().get::<GlobalTransform>(night).map(|g| g.translation());
    assert_eq!(world.map(|p| (p.x, p.y, p.z)), Some((1.0, 0.0, 0.0)));
}

/// A member spawned without an `EntityGuid` (a script's runtime member) is switched too
/// (ADR-7200): `Parked` hides it, `Live` shows it again with the visibility it had, and `Absent`
/// parks it (it has no template record to come back from).
#[test]
fn a_member_without_a_guid_is_switched_too() {
    let mut app = app();
    let member = app
        .world_mut()
        .spawn((
            Transform::default(),
            Visibility::Visible,
            LayerMembership {
                layers: vec!["quest_a".to_owned()],
            },
        ))
        .id();
    app.update();
    let parked = |app: &App| app.world().get::<LayerParked>(member).is_some();
    let visibility = |app: &App| app.world().get::<Visibility>(member).copied();
    assert_eq!(
        app.world().resource::<LayerTable>().effective("quest_a"),
        Some(LayerState::Live)
    );
    assert!(!parked(&app), "a live layer leaves it alone");

    assert!(request(&mut app, "quest_a", LayerState::Parked, false));
    assert!(parked(&app), "parked with its layer");
    assert_eq!(visibility(&app), Some(Visibility::Hidden));
    assert_eq!(
        app.world().get::<InheritedVisibility>(member).map(|v| v.0),
        Some(false),
        "hidden in the hierarchy, so the renderer leaves it out"
    );

    assert!(request(&mut app, "quest_a", LayerState::Live, false));
    assert!(!parked(&app), "live again");
    assert_eq!(
        visibility(&app),
        Some(Visibility::Visible),
        "its own visibility back"
    );

    assert!(request(&mut app, "quest_a", LayerState::Absent, false));
    assert!(app.world().get_entity(member).is_ok(), "nothing could respawn it");
    assert!(parked(&app), "an absent layer parks a guid-less member");
}

/// Finding scene-ecs 1 (v0.5.0 rc1): with the floating origin moved, a layer respawn puts its
/// fresh roots into local space exactly like the level open does (spec 78 § 12), and a member
/// respawned under a live (adopted) parent keeps its parent-relative transform.
#[test]
fn a_respawn_after_an_origin_rebase_lands_in_local_space() {
    const NIGHT_CHILD: u128 = 0x601;
    let offset = [1000.0, 0.0, -500.0];
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs());
    let text = level_text().replacen(
        "], resources:",
        &format!("{} ], resources:", record(NIGHT_CHILD, Some(ROOT), &["night"])),
        1,
    );
    let level = Level::from_ron_str(LEVEL, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    app.insert_resource(regia_scene::streaming::OriginOffset(offset));
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    app.update();
    let local = |app: &mut App, id: u128| {
        let entity = entity_of(app, id);
        app.world()
            .get::<Transform>(entity)
            .map(|t| (t.translation.x, t.translation.y, t.translation.z))
    };
    // Every record is authored at (1, 0, 0): a root opens at 1 - offset.
    let shifted = Some((1.0 - 1000.0, 0.0, 500.0));
    assert_eq!(local(&mut app, ROOT), shifted, "the open shifts roots");
    assert!(!guids(&mut app).contains(&NIGHT), "night starts absent");

    assert!(request(&mut app, "night", LayerState::Live, false));
    assert_eq!(local(&mut app, NIGHT), shifted, "a respawned root is shifted");
    assert_eq!(local(&mut app, BOTH), shifted, "every respawned root is shifted");
    assert_eq!(
        local(&mut app, NIGHT_CHILD),
        Some((1.0, 0.0, 0.0)),
        "a member under a live parent stays parent-relative"
    );
    assert_eq!(
        local(&mut app, ROOT),
        shifted,
        "the adopted parent is not shifted twice"
    );
    // And the world position is where the open put the rest of the level.
    let night = entity_of(&mut app, NIGHT);
    let root = entity_of(&mut app, ROOT);
    let world_x = |app: &App, e: Entity| app.world().get::<GlobalTransform>(e).map(|g| g.translation().x);
    assert_eq!(world_x(&app, night), world_x(&app, root));
}

/// Next to scene-ecs findings 1 and 2 (v0.5.0 rc1): in a level a placed instance streams, a layer
/// respawn puts its fresh roots back under the instance (the file is authored relative to it, so
/// no floating-origin shift) and makes every respawned entity a member of the instance.
#[test]
fn a_respawn_in_a_streamed_placement_goes_back_under_its_instance() {
    use regia_scene::placed::PlacedMember;
    const HOST: LevelId = LevelId(Guid::from_u128(0xb0));
    const INSTANCE: u128 = 0x901;
    const NIGHT_CHILD: u128 = 0x601;
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs());
    let text = level_text().replacen(
        "], resources:",
        &format!("{} ], resources:", record(NIGHT_CHILD, Some(ROOT), &["night"])),
        1,
    );
    let level = Level::from_ron_str(LEVEL, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    let host = format!(
        r#"SceneFile(format_version: {v}, entities: [(guid: "{instance}", components: {{
            "regia_scene::transform::Transform": (translation: (5.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            "regia_scene::placed::PlacedLevel": (level: "asset:{level}", mode: Streamed),
        }})], resources: {{}})"#,
        v = SCENE_FORMAT_VERSION,
        instance = guid(INSTANCE),
        level = LEVEL.0,
    );
    let host = Level::from_ron_str(HOST, &host).expect("host");
    app.world_mut().resource_mut::<LevelLibrary>().insert(host);
    app.insert_resource(regia_scene::streaming::OriginOffset([1000.0, 0.0, 0.0]));
    open_level(app.world_mut(), HOST, LoadMode::Replace).expect("open");
    app.update();
    let instance = entity_of(&mut app, INSTANCE);
    assert!(!guids(&mut app).contains(&NIGHT), "night starts absent");

    assert!(request(&mut app, "night", LayerState::Live, false));
    for id in [NIGHT, BOTH] {
        let entity = entity_of(&mut app, id);
        assert_eq!(
            app.world().get::<ChildOf>(entity).map(ChildOf::parent),
            Some(instance),
            "respawned root {id:#x} under the instance"
        );
        let x = app
            .world()
            .get::<GlobalTransform>(entity)
            .map(|g| g.translation().x);
        // The instance opened at 5 - 1000; the record sits 1 m along X from it.
        assert_eq!(x, Some(-994.0), "respawned root {id:#x} on its instance");
    }
    for id in [ROOT, NIGHT, BOTH, NIGHT_CHILD] {
        let entity = entity_of(&mut app, id);
        assert_eq!(
            app.world().get::<PlacedMember>(entity).map(|m| m.instance),
            Some(instance),
            "{id:#x} is a member of the instance"
        );
    }
}

/// Finding scene-ecs 4 (v0.5.0 rc1): a definition reference that is not an asset id is reported,
/// not dropped silently (its layers would just be missing).
#[test]
fn a_definition_reference_that_is_not_an_asset_id_is_reported() {
    let text = level_text().replacen(
        &format!("defs: [\"asset:{DEFS_ID}\"]"),
        &format!("defs: [\"asset:not-a-guid\", \"asset:{DEFS_ID}\"]"),
        1,
    );
    assert_ne!(text, level_text(), "the binding was rewritten");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs());
    let level = Level::from_ron_str(LEVEL, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    app.update();
    let table = app.world().resource::<LayerTable>();
    assert_eq!(
        table.effective("night"),
        Some(LayerState::Absent),
        "the valid one binds"
    );
    let warnings: Vec<&String> = table
        .warnings()
        .iter()
        .filter(|w| w.contains("not-a-guid"))
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", table.warnings());
}
