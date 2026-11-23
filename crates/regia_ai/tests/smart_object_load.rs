// SPDX-License-Identifier: MPL-2.0
//! `SmartObject::definition` (spec 39 § 4, ADR-2012) without an asset server: the persisted field
//! and what the loader does with definitions already in [`SmartObjectDefs`]. The reads through a
//! real cook and pak are `regia_asset_pipeline/tests/data_cook.rs`.

use std::sync::Arc;

use regia_ai::smart_object_load::{SmartObjectDefs, load_definition};
use regia_ai::{SmartObject, SmartObjectDef, SmartObjectDefinition, SmartObjectIndex, SmartObjectPlugin};
use regia_app::prelude::*;
use regia_asset::{AssetId, AssetRef};
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{AppTypeRegistry, FromReflect};
use serde::de::DeserializeSeed;

const CHAIR: &str = r#"(format_version: 1, slots: [(name: "left"), (name: "right")])"#;

fn def(text: &str) -> SmartObjectDef {
    SmartObjectDef::from_ron(text).expect("definition")
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    app
}

fn object_of(id: AssetId) -> SmartObject {
    SmartObject {
        definition: Some(AssetRef::new(id)),
        ..SmartObject::default()
    }
}

#[test]
fn a_reference_to_a_loaded_definition_materializes_its_slots() {
    let mut app = app();
    let id = AssetId::new_random();
    app.world_mut()
        .resource_mut::<SmartObjectDefs>()
        .insert(id, def(CHAIR));
    let object = app.world_mut().spawn(object_of(id)).id();
    app.update();
    app.update();
    assert_eq!(app.world().resource::<SmartObjectIndex>().slots().count(), 2);
    assert!(app.world().get::<SmartObjectDefinition>(object).is_some());
}

#[test]
fn objects_share_the_definition_they_name() {
    let mut app = app();
    let id = AssetId::new_random();
    app.world_mut()
        .resource_mut::<SmartObjectDefs>()
        .insert(id, def(CHAIR));
    let a = app.world_mut().spawn(object_of(id)).id();
    let b = app.world_mut().spawn(object_of(id)).id();
    app.update();
    let of = |e: Entity| {
        app.world()
            .get::<SmartObjectDefinition>(e)
            .expect("definition")
            .0
            .clone()
    };
    assert!(Arc::ptr_eq(&of(a), &of(b)));
}

/// The older route: a host parses and sets the component on an object with no reference.
#[test]
fn a_host_set_definition_is_left_alone() {
    let mut app = app();
    let host = Arc::new(def(CHAIR));
    let object = app
        .world_mut()
        .spawn((SmartObject::default(), SmartObjectDefinition(host.clone())))
        .id();
    app.update();
    app.update();
    let current = app
        .world()
        .get::<SmartObjectDefinition>(object)
        .expect("definition");
    assert!(Arc::ptr_eq(&current.0, &host));
    assert_eq!(app.world().resource::<SmartObjectIndex>().slots().count(), 2);
}

#[test]
fn a_replaced_definition_reaches_the_object() {
    let mut app = app();
    let id = AssetId::new_random();
    app.world_mut()
        .resource_mut::<SmartObjectDefs>()
        .insert(id, def(CHAIR));
    app.world_mut().spawn(object_of(id));
    app.update();
    app.update();
    app.world_mut()
        .resource_mut::<SmartObjectDefs>()
        .insert(id, def(r#"(format_version: 1, slots: [(name: "left")])"#));
    app.update();
    app.update();
    assert_eq!(app.world().resource::<SmartObjectIndex>().slots().count(), 1);
}

/// No server: a definition cannot be read, the app keeps running, and it asks only once.
#[test]
fn a_reference_with_no_asset_server_reports_and_does_not_retry() {
    let mut app = app();
    let id = AssetId::new_random();
    let object = app.world_mut().spawn(object_of(id)).id();
    for _ in 0..3 {
        app.update();
    }
    assert!(app.world().get::<SmartObjectDefinition>(object).is_none());
    let slot = app
        .world()
        .resource::<SmartObjectDefs>()
        .slot(id)
        .expect("slot")
        .clone();
    assert_eq!(slot.error.map(|e| e.reason), Some("definition_not_found"));
    assert_eq!(slot.generation, 0);
}

/// `load_definition` is the entry a tool calls directly, before any plugin ran.
#[test]
fn loading_works_on_a_bare_world() {
    let mut world = World::new();
    assert!(!load_definition(&mut world, AssetId::new_random()));
    assert!(world.contains_resource::<SmartObjectDefs>());
}

fn round_trip(app: &App, object: &SmartObject) -> SmartObject {
    let registry = app.world().resource::<AppTypeRegistry>().read();
    let text = ron::to_string(&TypedReflectSerializer::new(object, &registry)).expect("serialize");
    from_text(app, &text)
}

fn from_text(app: &App, text: &str) -> SmartObject {
    let registry = app.world().resource::<AppTypeRegistry>().read();
    let registration = registry
        .get(std::any::TypeId::of::<SmartObject>())
        .expect("registered");
    let mut deserializer = ron::Deserializer::from_str(text).expect("text");
    let dynamic = TypedReflectDeserializer::new(registration, &registry)
        .deserialize(&mut deserializer)
        .expect("deserialize");
    SmartObject::from_reflect(dynamic.as_ref()).expect("from reflect")
}

/// The field persists through the reflection path a scene file uses.
#[test]
fn the_reference_survives_a_scene_round_trip() {
    let app = app();
    let id = AssetId::new_random();
    let mut object = object_of(id);
    object.tags = vec!["furniture".to_owned()];
    let loaded = round_trip(&app, &object);
    assert_eq!(loaded.definition.map(|r| r.id), Some(id));
    assert_eq!(loaded.tags, ["furniture"]);
    assert_eq!(round_trip(&app, &SmartObject::default()).definition, None);
}

/// A scene written before the field existed loads with no reference.
#[test]
fn a_scene_older_than_the_reference_still_loads() {
    let app = app();
    let loaded = from_text(&app, r#"(tags: ["cover"], enabled: false)"#);
    assert_eq!(loaded.definition, None);
    assert_eq!(loaded.tags, ["cover"]);
    assert!(!loaded.enabled);
}
