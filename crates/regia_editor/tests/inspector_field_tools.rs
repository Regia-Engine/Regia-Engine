// SPDX-License-Identifier: MPL-2.0
//! The Inspector's field tools through the agent dispatcher (spec addendum § 7.4-7.6; P-114,
//! P-115, P-119, P-120, P-122): `editor.property_copy/paste`, `editor.component_copy/paste`,
//! `editor.reset_property`, `editor.list_move/list_duplicate`, and the conditional and raw state
//! in `editor.inspect_selection`. Every edit is one undo step.

use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::agent_ext;
use regia_editor::command::UndoStack;
use regia_editor::editor_prefs::EditorConfigPaths;
use regia_render::{DirectionalLight, PointLight, PostVolume, PostVolumeShape};
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

/// A component with a list (list moves and duplicates) and a hidden field (raw view).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Route {
    waypoints: Vec<f32>,
    #[reflect(@regia_ecs::attrs::Hidden)]
    secret: u32,
}

struct Rig {
    app: App,
    inbox: crossbeam_channel::Sender<Inbound>,
    next: u64,
}

impl Rig {
    fn new() -> Self {
        let project = regia_test_utils::unique_temp_dir("inspector-field-tools");
        std::fs::create_dir_all(&project).expect("project");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<PointLight>();
        app.register_type::<DirectionalLight>();
        app.register_type::<PostVolume>();
        app.register_type::<Route>();
        app.init_resource::<UndoStack>();
        app.insert_resource(EditorConfigPaths::at(project.join("config")));
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self { app, inbox, next: 0 }
    }

    fn send(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(self.next)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("inbox");
        self.app.update();
        let response = answer.try_recv().expect("response within one frame");
        match response.error {
            Some(error) => Err(error),
            None => Ok(response.result.expect("result")),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.send(method, params)
            .unwrap_or_else(|err| panic!("{method}: {err:?}"))
    }

    fn refused(&mut self, method: &str, params: Value) -> String {
        let err = self.send(method, params).expect_err("refused");
        err.data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    fn undo_depth(&self) -> usize {
        self.app.world().resource::<UndoStack>().undo_depth()
    }

    fn undo(&mut self) {
        self.call("editor.undo", json!({}));
    }
}

fn lamp(world: &mut World, name: &str, intensity: f32, radius: f32) -> Entity {
    world
        .spawn((
            Name::new(name),
            Transform::default(),
            PointLight {
                intensity,
                radius,
                ..PointLight::default()
            },
        ))
        .id()
}

fn bits(entity: Entity) -> u64 {
    entity.to_bits()
}

#[test]
fn a_copied_value_pastes_onto_another_entity_as_one_undo_step_and_a_wrong_type_is_refused() {
    let mut rig = Rig::new();
    let a = lamp(rig.app.world_mut(), "a", 1234.0, 3.0);
    let b = lamp(rig.app.world_mut(), "b", 10.0, 7.0);
    let copied = rig.call(
        "editor.property_copy",
        json!({ "entity": bits(a), "component": "PointLight", "path": "intensity" }),
    );
    assert_eq!(copied["clipboard"]["type"], "f32");
    assert_eq!(copied["clipboard"]["ron"], "1234.0");
    assert_eq!(copied["agent_call"]["method"], "editor.set_property");
    assert_eq!(copied["agent_call"]["params"]["path"], "intensity");
    assert_eq!(copied["agent_call"]["params"]["value"], json!(1234.0));
    let depth = rig.undo_depth();
    rig.call(
        "editor.property_paste",
        json!({ "entity": bits(b), "component": "PointLight", "path": "intensity" }),
    );
    let world = rig.app.world();
    assert_eq!(world.get::<PointLight>(b).map(|l| l.intensity), Some(1234.0));
    assert_eq!(rig.undo_depth(), depth + 1, "one undo step");
    // An f32 into a bool is refused.
    assert_eq!(
        rig.refused(
            "editor.property_paste",
            json!({ "entity": bits(b), "component": "PointLight", "path": "cast_shadows" }),
        ),
        "type_mismatch"
    );
    rig.undo();
    assert_eq!(
        rig.app.world().get::<PointLight>(b).map(|l| l.intensity),
        Some(10.0)
    );
}

#[test]
fn components_copy_paste_values_and_paste_as_new() {
    let mut rig = Rig::new();
    let a = lamp(rig.app.world_mut(), "a", 500.0, 4.0);
    let b = lamp(rig.app.world_mut(), "b", 1.0, 1.0);
    let bare = rig
        .app
        .world_mut()
        .spawn((Name::new("bare"), Transform::default()))
        .id();
    assert_eq!(
        rig.refused("editor.component_paste", json!({ "entity": bits(b) })),
        "clipboard_empty"
    );
    rig.call(
        "editor.component_copy",
        json!({ "entity": bits(a), "component": "PointLight" }),
    );
    // Values onto an entity that has the component.
    rig.call(
        "editor.component_paste",
        json!({ "entity": bits(b), "component": "PointLight" }),
    );
    let pasted = rig.app.world().get::<PointLight>(b).cloned().expect("light");
    assert_eq!((pasted.intensity, pasted.radius), (500.0, 4.0));
    // As new: refused where it exists, added where it does not (one undo step).
    assert_eq!(
        rig.refused(
            "editor.component_paste",
            json!({ "entity": bits(b), "as_new": true })
        ),
        "component_present"
    );
    assert_eq!(
        rig.refused("editor.component_paste", json!({ "entity": bits(bare) })),
        "component_missing"
    );
    let depth = rig.undo_depth();
    rig.call(
        "editor.component_paste",
        json!({ "entity": bits(bare), "as_new": true }),
    );
    assert_eq!(
        rig.app.world().get::<PointLight>(bare).map(|l| l.intensity),
        Some(500.0)
    );
    assert_eq!(rig.undo_depth(), depth + 1);
    rig.undo();
    assert!(
        rig.app.world().get::<PointLight>(bare).is_none(),
        "undo removes the pasted component"
    );
    // A field value is not a component; a named type guard refuses another type.
    rig.call(
        "editor.property_copy",
        json!({ "entity": bits(a), "component": "PointLight", "path": "radius" }),
    );
    assert_eq!(
        rig.refused(
            "editor.component_paste",
            json!({ "entity": bits(bare), "as_new": true })
        ),
        "not_a_component"
    );
    rig.call(
        "editor.component_copy",
        json!({ "entity": bits(a), "component": "PointLight" }),
    );
    assert_eq!(
        rig.refused(
            "editor.component_paste",
            json!({ "entity": bits(b), "component": "DirectionalLight" })
        ),
        "type_mismatch"
    );
}

#[test]
fn reset_property_restores_a_field_or_the_whole_component_to_default() {
    let mut rig = Rig::new();
    let a = lamp(rig.app.world_mut(), "a", 999.0, 9.0);
    let default = PointLight::default();
    let depth = rig.undo_depth();
    rig.call(
        "editor.reset_property",
        json!({ "entity": bits(a), "component": "PointLight", "path": "intensity" }),
    );
    let light = rig.app.world().get::<PointLight>(a).cloned().expect("light");
    assert_eq!(light.intensity, default.intensity);
    assert_eq!(light.radius, 9.0, "only the field resets");
    assert_eq!(rig.undo_depth(), depth + 1);
    rig.call(
        "editor.reset_property",
        json!({ "entity": bits(a), "component": "PointLight", "all": true }),
    );
    assert_eq!(rig.app.world().get::<PointLight>(a), Some(&default));
    assert_eq!(
        rig.refused(
            "editor.reset_property",
            json!({ "entity": bits(a), "component": "PointLight" })
        ),
        "path_required"
    );
    assert_eq!(
        rig.refused(
            "editor.reset_property",
            json!({ "entity": bits(a), "component": "PointLight", "path": "radius", "all": true })
        ),
        "path_and_all"
    );
    rig.undo();
    rig.undo();
    assert_eq!(
        rig.app
            .world()
            .get::<PointLight>(a)
            .map(|l| (l.intensity, l.radius)),
        Some((999.0, 9.0))
    );
}

#[test]
fn list_move_and_duplicate_are_one_undo_step_each() {
    let mut rig = Rig::new();
    let e = rig
        .app
        .world_mut()
        .spawn(Route {
            waypoints: vec![1.0, 2.0, 3.0],
            secret: 7,
        })
        .id();
    let route = |rig: &Rig| {
        rig.app
            .world()
            .get::<Route>(e)
            .map(|r| r.waypoints.clone())
            .unwrap_or_default()
    };
    let depth = rig.undo_depth();
    let moved = rig.call(
        "editor.list_move",
        json!({ "entity": bits(e), "component": "Route", "path": "waypoints", "from": 2, "to": 0 }),
    );
    assert_eq!(moved["len"], 3);
    assert_eq!(route(&rig), vec![3.0, 1.0, 2.0]);
    let duplicated = rig.call(
        "editor.list_duplicate",
        json!({ "entity": bits(e), "component": "Route", "path": "waypoints", "index": 1 }),
    );
    assert_eq!(duplicated["len"], 4);
    assert_eq!(route(&rig), vec![3.0, 1.0, 1.0, 2.0]);
    assert_eq!(rig.undo_depth(), depth + 2);
    assert_eq!(
        rig.refused(
            "editor.list_move",
            json!({ "entity": bits(e), "component": "Route", "path": "waypoints", "from": 9, "to": 0 })
        ),
        "index_out_of_range"
    );
    assert_eq!(
        rig.refused(
            "editor.list_duplicate",
            json!({ "entity": bits(e), "component": "Route", "path": "secret", "index": 0 })
        ),
        "not_a_list"
    );
    rig.undo();
    assert_eq!(route(&rig), vec![3.0, 1.0, 2.0]);
    rig.undo();
    assert_eq!(route(&rig), vec![1.0, 2.0, 3.0]);
}

fn field<'a>(inspected: &'a Value, component: &str, path: &str) -> &'a Value {
    inspected["components"]
        .as_array()
        .and_then(|components| components.iter().find(|c| c["component"] == component))
        .and_then(|c| c["fields"].as_array())
        .and_then(|fields| fields.iter().find(|f| f["path"] == path))
        .unwrap_or_else(|| panic!("{component}.{path} in {inspected:#}"))
}

#[test]
fn inspect_selection_reports_false_edit_conditions_and_the_raw_view() {
    let mut rig = Rig::new();
    let sun = rig
        .app
        .world_mut()
        .spawn(DirectionalLight {
            cast_shadows: false,
            ..DirectionalLight::default()
        })
        .id();
    let volume = rig
        .app
        .world_mut()
        .spawn(PostVolume {
            shape: PostVolumeShape::Unbound,
            ..PostVolume::default()
        })
        .id();
    let route = rig.app.world_mut().spawn(Route::default()).id();
    let inspected = rig.call(
        "editor.inspect_selection",
        json!({ "entities": [bits(sun), bits(volume), bits(route)] }),
    );
    let shadow = field(&inspected, "DirectionalLight", "static_shadow");
    assert_eq!(shadow["enabled"], false);
    assert_eq!(shadow["condition"], "cast_shadows");
    assert!(
        field(&inspected, "DirectionalLight", "illuminance")
            .get("enabled")
            .is_none()
    );
    assert_eq!(
        field(&inspected, "PostVolume", "blend_distance")["enabled"],
        false
    );
    assert!(field(&inspected, "PostVolume", "weight").get("enabled").is_none());
    assert!(field(&inspected, "Route", "secret").get("hidden").is_none());
    let raw = rig.call(
        "editor.inspect_selection",
        json!({ "entities": [bits(route)], "raw": true }),
    );
    assert_eq!(field(&raw, "Route", "secret")["hidden"], true);
    // The condition follows the sibling: casting shadows enables the field.
    rig.call(
        "editor.set_property",
        json!({ "entity": bits(sun), "component": "DirectionalLight", "path": "cast_shadows", "value": true }),
    );
    let inspected = rig.call("editor.inspect_selection", json!({ "entities": [bits(sun)] }));
    assert!(
        field(&inspected, "DirectionalLight", "static_shadow")
            .get("enabled")
            .is_none()
    );
}
