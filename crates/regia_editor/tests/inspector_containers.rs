// SPDX-License-Identifier: MPL-2.0
//! The ported reflection inspector (bevy-inspector-egui, `docs/ported-sources.toml`) keeps
//! Regia Engine's edit contract for every container kind: each edit becomes a `SetProperty` on the
//! undo stack with the reflect path of the edited field, structural edits are their own undo
//! steps, and the agent's `editor.set_property` reaches the same paths (`specs/19` §§ 1–2).

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::attrs::{AssetFilter, Range, Step};
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectMut, ReflectPath, TypeRegistry};
use regia_editor::agent_ext;
use regia_editor::command::{UndoStack, undo_world};
use regia_editor::inspector::impls::InspectorEguiImpl;
use regia_editor::inspector::reflect_ui::{apply_list_op, clone_value, switch_variant};
use regia_editor::inspector::{self, Change, InspectorUi, ListOp};
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

/// One stop of a patrol route.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
struct Waypoint {
    position: Vec3,
    #[reflect(@Range(0.0..=20.0), @Step(0.1))]
    speed: f32,
    wait: f32,
}

/// What the squad does: unit, struct and tuple variants.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
enum Mode {
    #[default]
    Idle,
    Patrol {
        radius: f32,
        loops: u32,
    },
    Chase(f32),
}

/// A tuple struct.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
struct Tag(f32);

/// A nested struct.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
struct Inner {
    strength: f32,
    label: String,
}

/// A component with every container kind the inspector draws.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Squad {
    waypoints: Vec<Waypoint>,
    slots: [u8; 3],
    pair: (f32, bool),
    tag: Tag,
    mode: Option<Mode>,
    behaviour: Mode,
    stats: BTreeMap<String, f32>,
    // A `HashSet`: bevy_reflect 0.20 reflects `BTreeSet` as an opaque value, not a `Set`.
    tags: HashSet<String>,
    base_color: [f32; 3],
    #[reflect(@AssetFilter("*.ogg"))]
    voice: String,
    inner: Inner,
}

fn squad() -> Squad {
    Squad {
        waypoints: vec![
            Waypoint {
                position: Vec3::new(1.0, 0.0, -2.0),
                speed: 2.0,
                wait: 0.5,
            },
            Waypoint {
                position: Vec3::new(6.0, 0.0, 3.0),
                speed: 4.0,
                wait: 1.0,
            },
        ],
        slots: [1, 2, 3],
        pair: (0.25, true),
        tag: Tag(7.0),
        mode: Some(Mode::Patrol {
            radius: 12.0,
            loops: 3,
        }),
        behaviour: Mode::Chase(4.5),
        stats: BTreeMap::from([("armor".to_string(), 25.0), ("hp".to_string(), 100.0)]),
        tags: HashSet::from(["elite".to_string(), "night".to_string()]),
        base_color: [0.8, 0.3, 0.1],
        voice: "assets/sfx/bark.ogg".into(),
        inner: Inner {
            strength: 3.0,
            label: "alpha".into(),
        },
    }
}

fn app_with_squad() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Squad>();
    app.init_resource::<UndoStack>();
    agent_ext::install(app.world_mut());
    let entity = app
        .world_mut()
        .spawn((Name::new("Squad"), Transform::default(), squad()))
        .id();
    (app, entity)
}

fn read(app: &App, entity: Entity) -> Squad {
    app.world().get::<Squad>(entity).cloned().expect("squad")
}

fn depth(app: &mut App) -> usize {
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth()
}

// --- Path recording through the real drawer -------------------------------------------------

thread_local! {
    /// The reflect path the poke widget edits this frame.
    static POKE: RefCell<String> = const { RefCell::new(String::new()) };
    /// The value the drawer showed at `values[3]` (the clone cache test).
    static SEEN: RefCell<Option<f32>> = const { RefCell::new(None) };
}

/// A test widget for `f32`/`u8` that sets the value when the drawer reaches the target path:
/// the drawer's own traversal decides the recorded path.
fn poke(value: &mut dyn Any, _: &mut egui::Ui, _: &dyn Any, _: egui::Id, env: InspectorUi<'_>) -> bool {
    if env.context.path() == "values[3]"
        && let Some(v) = value.downcast_ref::<f32>()
    {
        SEEN.with(|seen| *seen.borrow_mut() = Some(*v));
    }
    let hit = POKE.with(|p| *p.borrow() == env.context.path());
    if !hit {
        return false;
    }
    if let Some(v) = value.downcast_mut::<f32>() {
        *v = 42.0;
        return true;
    }
    if let Some(v) = value.downcast_mut::<u8>() {
        *v = 42;
        return true;
    }
    false
}

fn install_poke(app: &mut App) {
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let mut registry = registry.write();
    for type_id in [std::any::TypeId::of::<f32>(), std::any::TypeId::of::<u8>()] {
        if let Some(registration) = registry.get_mut(type_id) {
            registration.insert(InspectorEguiImpl::new(poke));
        }
    }
}

/// Runs one headless inspector frame on the squad with the poke aimed at `path`.
fn drawer_frame(app: &mut App, entity: Entity, path: &str) -> Vec<Change> {
    POKE.with(|p| *p.borrow_mut() = path.to_string());
    let ctx = egui::Context::default();
    let mut changes = Vec::new();
    let world = app.world_mut();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        changes = inspector::component_ui(ui, world, entity, "Squad", &[]);
    });
    output.textures_delta.clear();
    changes
}

#[test]
fn drawer_records_reflect_paths_into_every_container_and_undoes() {
    let (mut app, entity) = app_with_squad();
    install_poke(&mut app);
    let original = read(&app, entity);

    // A frame without an edit records nothing.
    assert!(drawer_frame(&mut app, entity, "nowhere").is_empty());
    assert_eq!(read(&app, entity), original);

    type Check = fn(&Squad) -> bool;
    let cases: [(&str, &str, Check); 8] = [
        ("list → struct field", "waypoints[1].speed", |s| {
            s.waypoints[1].speed == 42.0
        }),
        ("list → struct → vector", "waypoints[0].wait", |s| {
            s.waypoints[0].wait == 42.0
        }),
        ("array element", "slots[2]", |s| s.slots[2] == 42),
        ("tuple field", "pair.0", |s| s.pair.0 == 42.0),
        ("tuple struct field", "tag.0", |s| s.tag.0 == 42.0),
        ("Option → struct variant field", "mode.0.radius", |s| {
            s.mode
                == Some(Mode::Patrol {
                    radius: 42.0,
                    loops: 3,
                })
        }),
        ("enum tuple variant field", "behaviour.0", |s| {
            s.behaviour == Mode::Chase(42.0)
        }),
        ("nested struct field", "inner.strength", |s| {
            s.inner.strength == 42.0
        }),
    ];
    for (what, path, check) in cases {
        let before_depth = depth(&mut app);
        let changes = drawer_frame(&mut app, entity, path);
        assert_eq!(
            changes,
            vec![Change {
                path: path.to_string(),
                discrete: false
            }],
            "{what}: recorded path"
        );
        assert!(check(&read(&app, entity)), "{what}: the world got the edit");
        assert_eq!(depth(&mut app), before_depth + 1, "{what}: one undo entry");
        assert!(undo_world(app.world_mut()), "{what}: undo");
        assert_eq!(
            read(&app, entity),
            original,
            "{what}: undo restored the component"
        );
    }

    // Map values have no reflect path: they are drawn at the map's path and an edit of any of
    // them is recorded on the whole map (the poke hits both values here).
    let changes = drawer_frame(&mut app, entity, "stats");
    assert_eq!(
        changes,
        vec![Change {
            path: "stats".to_string(),
            discrete: false
        }]
    );
    assert!(read(&app, entity).stats.values().all(|v| *v == 42.0));
    assert!(undo_world(app.world_mut()));
    assert_eq!(read(&app, entity), original, "undo restored the map");
}

/// FU-6 (`docs/evidence/editor-a11y-2026-11.md`): a headless inspector draw of a component with
/// a boolean field and a `Vec` field reports no unnamed widget — the boolean checkbox is named
/// after its field, the painted list buttons + − ↑ ↓ after their action and element.
#[test]
fn bool_fields_and_list_buttons_carry_spoken_names() {
    /// A component with a boolean field and a `Vec` of booleans.
    #[derive(Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Default)]
    struct Beacon {
        on: bool,
        channels: Vec<bool>,
    }
    let mut registry = TypeRegistry::default();
    registry.register::<Beacon>();
    let mut value = Beacon {
        on: true,
        channels: vec![true],
    };
    let mut context = regia_editor::inspector::InspectorContext::new("Beacon");
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        regia_editor::inspector::InspectorUi::new(&registry, &mut context).ui_for_reflect(&mut value, ui);
    });
    output.textures_delta.clear();
    let update = output.platform_output.accesskit_update.expect("accesskit is on");
    let names = regia_editor::a11y_audit::widget_names(&update);
    assert_eq!(
        regia_editor::a11y_audit::unnamed_widgets(&update),
        Vec::<String>::new(),
        "{names:?}"
    );
    for want in [
        "on",
        "channels[0]",
        "Add element",
        "Remove element 0",
        "Move element 0 up",
        "Move element 0 down",
    ] {
        assert!(names.iter().any(|(_, name)| name == want), "{want} in {names:?}");
    }
}

// --- Structural edits (the drawer's buttons) -----------------------------------------------

/// Applies `edit` to a clone of the squad like the drawer does and pushes the recorded change.
fn edit_clone(
    app: &mut App,
    entity: Entity,
    path: &str,
    discrete: bool,
    edit: impl FnOnce(&TypeRegistry, &mut dyn PartialReflect),
) {
    let mut edited = clone_value(app.world().get::<Squad>(entity).expect("squad")).expect("clone");
    {
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let target: &mut dyn PartialReflect = if path.is_empty() {
            edited.as_mut()
        } else {
            path.reflect_element_mut(edited.as_mut()).expect("path")
        };
        edit(&registry, target);
    }
    inspector::apply_changes(
        app.world_mut(),
        entity,
        "Squad",
        edited.as_ref(),
        &[Change {
            path: path.to_string(),
            discrete,
        }],
    );
}

fn list_op(app: &mut App, entity: Entity, op: ListOp) {
    edit_clone(app, entity, "waypoints", true, |registry, value| {
        let ReflectMut::List(list) = value.reflect_mut() else {
            panic!("waypoints is a list");
        };
        apply_list_op(registry, list, op).expect("list op");
    });
}

#[test]
fn list_add_remove_move_are_separate_undo_steps() {
    let (mut app, entity) = app_with_squad();
    let original = read(&app, entity);
    let start = depth(&mut app);

    list_op(&mut app, entity, ListOp::AddElement(1));
    let added = read(&app, entity);
    assert_eq!(added.waypoints.len(), 3);
    assert_eq!(added.waypoints[1], Waypoint::default(), "the default waypoint");

    list_op(&mut app, entity, ListOp::MoveElementUp(2));
    assert_eq!(read(&app, entity).waypoints[1], original.waypoints[1]);
    list_op(&mut app, entity, ListOp::MoveElementDown(0));
    assert_eq!(read(&app, entity).waypoints[0], original.waypoints[1]);
    list_op(&mut app, entity, ListOp::RemoveElement(2));
    assert_eq!(read(&app, entity).waypoints.len(), 2);
    assert_eq!(depth(&mut app), start + 4, "structural edits never coalesce");

    for _ in 0..4 {
        assert!(undo_world(app.world_mut()));
    }
    assert_eq!(
        read(&app, entity),
        original,
        "undo shrank the list back (exact apply)"
    );

    // Removing to empty and adding into an empty list.
    for _ in 0..2 {
        list_op(&mut app, entity, ListOp::RemoveElement(0));
    }
    assert!(read(&app, entity).waypoints.is_empty());
    list_op(&mut app, entity, ListOp::AddElement(0));
    assert_eq!(read(&app, entity).waypoints, vec![Waypoint::default()]);
    for _ in 0..3 {
        assert!(undo_world(app.world_mut()));
    }
    assert_eq!(read(&app, entity), original);
}

#[test]
fn enum_and_option_variant_switches_use_defaults_and_undo() {
    let (mut app, entity) = app_with_squad();
    let original = read(&app, entity);

    edit_clone(&mut app, entity, "behaviour", true, |registry, value| {
        let ReflectMut::Enum(e) = value.reflect_mut() else {
            panic!("enum");
        };
        switch_variant(registry, e, "Patrol").expect("switch");
    });
    assert_eq!(
        read(&app, entity).behaviour,
        Mode::Patrol {
            radius: 0.0,
            loops: 0
        }
    );

    edit_clone(&mut app, entity, "mode", true, |registry, value| {
        let ReflectMut::Enum(e) = value.reflect_mut() else {
            panic!("option is an enum");
        };
        switch_variant(registry, e, "None").expect("switch to None");
    });
    assert_eq!(read(&app, entity).mode, None);
    edit_clone(&mut app, entity, "mode", true, |registry, value| {
        let ReflectMut::Enum(e) = value.reflect_mut() else {
            panic!("option is an enum");
        };
        switch_variant(registry, e, "Some").expect("switch to Some");
        assert_eq!(e.variant_name(), "Some");
    });
    assert_eq!(read(&app, entity).mode, Some(Mode::Idle), "Some(default)");

    for _ in 0..3 {
        assert!(undo_world(app.world_mut()));
    }
    assert_eq!(read(&app, entity), original);
}

#[test]
fn map_and_set_insert_remove_undo_exactly() {
    let (mut app, entity) = app_with_squad();
    let original = read(&app, entity);

    edit_clone(&mut app, entity, "stats", true, |_, value| {
        let ReflectMut::Map(map) = value.reflect_mut() else {
            panic!("map");
        };
        map.insert_boxed(Box::new("speed".to_string()), Box::new(9.0f32));
        map.remove(&"armor".to_string());
    });
    let edited = read(&app, entity);
    assert_eq!(edited.stats.get("speed"), Some(&9.0));
    assert!(!edited.stats.contains_key("armor"), "the removed key is gone");

    edit_clone(&mut app, entity, "tags", true, |_, value| {
        let ReflectMut::Set(set) = value.reflect_mut() else {
            panic!("set");
        };
        set.insert_boxed(Box::new("veteran".to_string()));
        set.remove(&"night".to_string());
    });
    let edited = read(&app, entity);
    assert!(edited.tags.contains("veteran") && !edited.tags.contains("night"));

    assert!(undo_world(app.world_mut()));
    assert!(undo_world(app.world_mut()));
    assert_eq!(
        read(&app, entity),
        original,
        "undo removed the inserted key and element"
    );
}

#[test]
fn leaf_edits_on_nested_paths_coalesce_like_drags() {
    let (mut app, entity) = app_with_squad();
    let original = read(&app, entity);
    let start = depth(&mut app);
    for speed in [5.0f32, 6.0, 7.0] {
        edit_clone(&mut app, entity, "waypoints[0].speed", false, |_, value| {
            value.apply(&speed);
        });
    }
    assert_eq!(read(&app, entity).waypoints[0].speed, 7.0);
    assert_eq!(depth(&mut app), start + 1, "a scrub is one undo entry");
    edit_clone(&mut app, entity, "base_color", false, |_, value| {
        value.apply(&[0.1f32, 0.2, 0.3]);
    });
    assert_eq!(read(&app, entity).base_color, [0.1, 0.2, 0.3]);
    assert!(undo_world(app.world_mut()));
    assert!(undo_world(app.world_mut()));
    assert_eq!(read(&app, entity), original);
}

// --- Agent: editor.set_property into nested lists -------------------------------------------

fn set(app: &mut App, params: Value) -> Result<Value, String> {
    agent_ext::test_support::set_property(app.world_mut(), &params).map_err(|e| format!("{e:?}"))
}

#[test]
fn agent_set_property_reaches_nested_lists_and_undoes() {
    let (mut app, entity) = app_with_squad();
    let original = read(&app, entity);
    let id = entity.to_bits();
    assert!(
        agent_ext::all_methods()
            .iter()
            .any(|(schema, _)| schema.name == "editor.set_property"),
        "the method is on the agent surface"
    );

    // A leaf inside a list element.
    let result = set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "waypoints[1].speed", "value": 9.5}),
    )
    .expect("set speed");
    assert_eq!(result["before"], json!(4.0));
    assert_eq!(result["after"], json!(9.5));
    assert_eq!(result["applied"], json!(true));
    assert_eq!(read(&app, entity).waypoints[1].speed, 9.5);

    // An enum variant field inside an Option.
    set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "mode.0.radius", "value": 7.5}),
    )
    .expect("set radius");
    assert_eq!(
        read(&app, entity).mode,
        Some(Mode::Patrol {
            radius: 7.5,
            loops: 3
        })
    );

    // An array element.
    set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "slots[0]", "value": 9}),
    )
    .expect("slot");
    assert_eq!(read(&app, entity).slots, [9, 2, 3]);

    // A whole list element, in the JSON form the dry run reports.
    let preview = set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "waypoints[0]", "value": {}, "dry_run": true}),
    );
    assert!(preview.is_err(), "an incomplete waypoint is rejected");
    let current = set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "waypoints[0].wait", "value": 0.5, "dry_run": true}),
    )
    .expect("dry run");
    assert_eq!(current["applied"], json!(false));
    let list = set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "waypoints", "value": [], "dry_run": true}),
    )
    .expect("list preview");
    let before = list["before"]
        .as_array()
        .cloned()
        .expect("the list serializes as an array");
    assert_eq!(before.len(), 2);
    let mut first = before[0].clone();
    first["wait"] = json!(3.0);
    set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "waypoints[0]", "value": first}),
    )
    .expect("set a whole element");
    assert_eq!(read(&app, entity).waypoints[0].wait, 3.0);

    // A shorter whole list replaces exactly.
    set(
        &mut app,
        json!({"entity": id, "component": "Squad", "path": "waypoints", "value": [before[1].clone()]}),
    )
    .expect("shrink the list");
    let shrunk = read(&app, entity);
    assert_eq!(shrunk.waypoints.len(), 1);
    assert_eq!(shrunk.waypoints[0].position, original.waypoints[1].position);

    // Errors: a path that does not exist and a value of the wrong type.
    assert!(
        set(
            &mut app,
            json!({"entity": id, "component": "Squad", "path": "waypoints[7].speed", "value": 1.0})
        )
        .is_err()
    );
    assert!(
        set(
            &mut app,
            json!({"entity": id, "component": "Squad", "path": "slots[0]", "value": "x"})
        )
        .is_err()
    );

    // Every applied edit is its own undo step; undoing them all restores the component.
    for _ in 0..5 {
        assert!(undo_world(app.world_mut()));
    }
    assert_eq!(read(&app, entity), original);
}

// --- Long containers are paginated (ADR-7306) ------------------------------------------------

/// A component with a list far over the page threshold.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Samples {
    values: Vec<f32>,
}

/// Frames of the `Samples` drawer in one context, with optional pointer events; returns the text
/// drawn by the last frame with each text's position.
fn samples_frame(
    ctx: &egui::Context,
    app: &mut App,
    entity: Entity,
    events: Vec<egui::Event>,
    changes: &mut Vec<Change>,
) -> Vec<(String, egui::Pos2)> {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 20_000.0),
        )),
        events,
        ..Default::default()
    };
    let world = app.world_mut();
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            changes.extend(inspector::component_ui(ui, world, entity, "Samples", &[]));
        });
    });
    output.textures_delta.clear();
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        if let egui::Shape::Text(text) = &clipped.shape {
            texts.push((text.galley.text().to_string(), text.pos));
        }
    }
    texts
}

#[test]
fn a_long_list_lays_out_one_page_and_the_pager_moves_it() {
    use regia_editor::inspector::reflect_ui::{PAGE_SIZE, PAGE_THRESHOLD, page_range};
    assert_eq!(page_range(PAGE_THRESHOLD, 3), 0..PAGE_THRESHOLD, "short: whole");
    assert_eq!(page_range(1_000, 2), 200..300);
    assert_eq!(page_range(1_050, 99), 1_000..1_050, "clamped to the last page");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Samples>();
    app.init_resource::<UndoStack>();
    install_poke(&mut app);
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Samples"),
            Samples {
                values: (0..100_000).map(|i| i as f32).collect(),
            },
        ))
        .id();
    let ctx = egui::Context::default();
    let mut changes = Vec::new();
    // The poke edits element 150 when the drawer reaches it: page 0 never does.
    POKE.with(|p| *p.borrow_mut() = "values[150]".to_string());
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    let texts = samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert!(changes.is_empty(), "element 150 is not on the first page");
    assert!(
        texts.iter().any(|(t, _)| t == "0..=99 of 100000"),
        "the pager says which elements show"
    );
    let index_labels = texts
        .iter()
        .filter(|(t, _)| t.parse::<usize>().is_ok_and(|i| i < 100_000))
        .count();
    assert!(
        index_labels <= 2 * PAGE_SIZE,
        "one page of index labels and values, not 100 000: {index_labels}"
    );

    // Next page: elements 100..=199, and the poke reaches element 150 through its real path.
    let next = texts
        .iter()
        .find(|(t, _)| t == "\u{25b6}")
        .map(|(_, pos)| *pos + egui::vec2(4.0, 6.0))
        .expect("next-page button drawn");
    let press = |pressed| egui::Event::PointerButton {
        pos: next,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    POKE.with(|p| p.borrow_mut().clear());
    samples_frame(
        &ctx,
        &mut app,
        entity,
        vec![egui::Event::PointerMoved(next)],
        &mut changes,
    );
    samples_frame(&ctx, &mut app, entity, vec![press(true)], &mut changes);
    samples_frame(&ctx, &mut app, entity, vec![press(false)], &mut changes);
    POKE.with(|p| *p.borrow_mut() = "values[150]".to_string());
    let texts = samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert!(
        texts.iter().any(|(t, _)| t == "100..=199 of 100000"),
        "the next page shows"
    );
    assert_eq!(
        changes,
        vec![Change {
            path: "values[150]".to_string(),
            discrete: false
        }],
        "an edit on a later page records the element's own path"
    );
    assert_eq!(read_samples(&app, entity)[150], 42.0);
}

fn read_samples(app: &App, entity: Entity) -> Vec<f32> {
    app.world()
        .get::<Samples>(entity)
        .map(|s| s.values.clone())
        .unwrap_or_default()
}

// --- An unchanged component is not cloned again on every repaint (ADR-7326) -----------------

#[test]
fn an_unchanged_repaint_does_not_clone_the_component_and_an_edit_clones_it_once() {
    use regia_editor::inspector::reflect_ui::clone_concrete_calls;
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Samples>();
    app.init_resource::<UndoStack>();
    agent_ext::install(app.world_mut());
    install_poke(&mut app);
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Samples"),
            Samples {
                values: (0..20_000).map(|i| i as f32).collect(),
            },
        ))
        .id();
    let ctx = egui::Context::default();
    let mut changes = Vec::new();
    POKE.with(|p| p.borrow_mut().clear());
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    let first = clone_concrete_calls();
    for _ in 0..5 {
        samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    }
    assert_eq!(
        clone_concrete_calls() - first,
        0,
        "five repaints of an unchanged 20 000-element component clone nothing"
    );
    assert!(changes.is_empty());

    // An agent edit moves the component's change tick: the next repaint clones it once and
    // shows the new value; the one after clones nothing again.
    let depth_before = depth(&mut app);
    set(
        &mut app,
        json!({"entity": entity.to_bits(), "component": "Samples", "path": "values[3]", "value": 7.5}),
    )
    .expect("set");
    let before = clone_concrete_calls();
    SEEN.with(|seen| *seen.borrow_mut() = None);
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert_eq!(
        clone_concrete_calls() - before,
        1,
        "one clone after editor.set_property"
    );
    assert_eq!(
        SEEN.with(|seen| *seen.borrow()),
        Some(7.5),
        "the repaint shows the agent's value"
    );
    let before = clone_concrete_calls();
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert_eq!(clone_concrete_calls() - before, 0, "steady again");

    // An edit in the drawer is applied as its own undo step and drops the cached value.
    POKE.with(|p| *p.borrow_mut() = "values[5]".to_string());
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    POKE.with(|p| p.borrow_mut().clear());
    assert_eq!(read_samples(&app, entity)[5], 42.0);
    assert_eq!(depth(&mut app), depth_before + 2, "each edit is one undo step");
    let before = clone_concrete_calls();
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert_eq!(
        clone_concrete_calls() - before,
        1,
        "the edited value is read back once"
    );
    let before = clone_concrete_calls();
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert_eq!(clone_concrete_calls() - before, 0);

    // An undo writes the component back: the repaint shows the old value.
    assert!(undo_world(app.world_mut()));
    let before = clone_concrete_calls();
    samples_frame(&ctx, &mut app, entity, vec![], &mut changes);
    assert_eq!(
        clone_concrete_calls() - before,
        1,
        "an undo moves the change tick"
    );
    assert_eq!(read_samples(&app, entity)[5], 5.0);
}
