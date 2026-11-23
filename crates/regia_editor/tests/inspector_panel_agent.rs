// SPDX-License-Identifier: MPL-2.0
//! The Inspector panel's agent surface through the dispatcher (spec addendum § 7.1-7.3):
//! `editor.inspect_selection { search, modified_only }`, `editor.inspector_pin`,
//! `editor.inspector_lock`, `editor.inspector_open_pinned`, `editor.component_types`.

use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::agent_ext;
use regia_editor::command::UndoStack;
use regia_editor::editor_prefs::EditorConfigPaths;
use regia_editor::inspector::pinned::{InspectorLock, InspectorPins, PINS_FILE, PinnedInspectors};
use regia_editor::selection::{SelectMode, Selection};
use regia_render::PointLight;
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

struct Rig {
    app: App,
    inbox: crossbeam_channel::Sender<Inbound>,
    next: u64,
}

impl Rig {
    fn new(config: &std::path::Path) -> Self {
        let project = regia_test_utils::unique_temp_dir("inspector-panel-agent");
        std::fs::create_dir_all(&project).expect("project");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<PointLight>();
        app.init_resource::<UndoStack>();
        app.insert_resource(EditorConfigPaths::at(config));
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
}

fn lamp(world: &mut World, name: &str, intensity: f32) -> Entity {
    world
        .spawn((
            Name::new(name),
            Transform::default(),
            PointLight {
                intensity,
                ..PointLight::default()
            },
        ))
        .id()
}

fn field_paths(inspected: &Value, component: &str) -> Vec<String> {
    inspected["components"]
        .as_array()
        .expect("components")
        .iter()
        .filter(|c| c["component"] == component)
        .flat_map(|c| c["fields"].as_array().cloned().unwrap_or_default())
        .map(|f| {
            f["path"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches('.')
                .to_owned()
        })
        .collect()
}

#[test]
fn inspect_selection_search_and_modified_only_filter_fields() {
    let config = regia_test_utils::unique_temp_dir("inspector-panel-agent-config");
    let mut rig = Rig::new(&config);
    let default_intensity = PointLight::default().intensity;
    let entity = lamp(rig.app.world_mut(), "Lamp", default_intensity + 100.0);
    let ids = json!([entity.to_bits()]);

    let all = rig.call("editor.inspect_selection", json!({ "entities": ids }));
    assert!(field_paths(&all, "PointLight").len() > 2, "{all}");
    assert!(all.get("filter").is_none(), "no filter: the answer is unchanged");

    let int = rig.call(
        "editor.inspect_selection",
        json!({ "entities": ids, "search": "int" }),
    );
    assert_eq!(field_paths(&int, "PointLight"), ["intensity"], "{int}");
    assert!(
        field_paths(&int, "Transform").is_empty(),
        "no Transform field matches int"
    );
    let color = rig.call(
        "editor.inspect_selection",
        json!({ "entities": ids, "search": "lgtclr" }),
    );
    assert_eq!(field_paths(&color, "PointLight"), ["color"], "{color}");

    let modified = rig.call(
        "editor.inspect_selection",
        json!({ "entities": ids, "modified_only": true }),
    );
    assert_eq!(field_paths(&modified, "PointLight"), ["intensity"], "{modified}");
    assert_eq!(modified["filter"]["modified_only"], true);

    let bad = rig
        .send(
            "editor.inspect_selection",
            json!({ "entities": ids, "search": 3 }),
        )
        .expect_err("search must be a string");
    assert!(bad.message.contains("search"), "{bad:?}");
}

#[test]
fn pins_lock_and_pinned_windows_dispatch() {
    let config = regia_test_utils::unique_temp_dir("inspector-panel-agent-pins");
    let mut rig = Rig::new(&config);
    let a = lamp(rig.app.world_mut(), "A", 10.0);
    let b = lamp(rig.app.world_mut(), "B", 20.0);

    let pinned = rig.call(
        "editor.inspector_pin",
        json!({ "type": "PointLight", "path": "intensity", "on": true }),
    );
    assert_eq!(pinned["type"], "regia_render::lights::PointLight");
    assert_eq!(pinned["pinned"], true);
    assert_eq!(pinned["changed"], true);
    assert!(
        rig.app
            .world()
            .resource::<InspectorPins>()
            .is_pinned("regia_render::lights::PointLight", "intensity")
    );
    let text = std::fs::read_to_string(config.join(PINS_FILE)).expect("pins file written");
    assert!(
        text.contains("format_version: 1") && text.contains("intensity"),
        "{text}"
    );
    let unknown = rig
        .send(
            "editor.inspector_pin",
            json!({ "type": "NoSuchThing", "path": "x" }),
        )
        .expect_err("unknown type");
    assert_eq!(
        unknown.data.as_ref().and_then(|d| d.get("reason")),
        Some(&json!("type_unknown"))
    );

    let mut selection = Selection::default();
    selection.select(a, SelectMode::Replace);
    rig.app.world_mut().insert_resource(selection);
    let locked = rig.call("editor.inspector_lock", json!({ "on": true }));
    assert_eq!(locked, json!({ "locked": true, "entity": a.to_bits() }));
    assert_eq!(rig.app.world().resource::<InspectorLock>().entity, Some(a));
    let unlocked = rig.call("editor.inspector_lock", json!({ "on": false }));
    assert_eq!(unlocked["locked"], false);

    let opened = rig.call("editor.inspector_open_pinned", json!({ "entity": a.to_bits() }));
    assert_eq!(opened["opened"], true);
    let again = rig.call("editor.inspector_open_pinned", json!({ "entity": b.to_bits() }));
    assert_eq!(again["windows"], json!([a.to_bits(), b.to_bits()]));
    rig.app.world_mut().despawn(a);
    let after = rig.call(
        "editor.inspector_open_pinned",
        json!({ "entity": b.to_bits(), "close": true }),
    );
    assert_eq!(
        after["windows"],
        json!([]),
        "a's window closed with its entity, b's by close"
    );
    assert!(rig.app.world().resource::<PinnedInspectors>().windows.is_empty());
    let gone = rig
        .send("editor.inspector_open_pinned", json!({ "entity": a.to_bits() }))
        .expect_err("despawned");
    assert_eq!(
        gone.data.as_ref().and_then(|d| d.get("reason")),
        Some(&json!("entity_gone"))
    );
}

#[test]
fn component_types_lists_categories_and_greys_present_types() {
    let config = regia_test_utils::unique_temp_dir("inspector-panel-agent-types");
    let mut rig = Rig::new(&config);
    let entity = lamp(rig.app.world_mut(), "Lamp", 1.0);
    let listed = rig.call(
        "editor.component_types",
        json!({ "query": "pointlight", "entities": [entity.to_bits()] }),
    );
    let types = listed["types"].as_array().expect("types");
    let light = types
        .iter()
        .find(|t| t["type_path"] == "regia_render::lights::PointLight")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(light["category"], "Render");
    assert_eq!(light["addable"], false);
    assert_eq!(light["reason"], "already on this entity");
    let fresh = rig.app.world_mut().spawn(Name::new("Empty")).id();
    let listed = rig.call(
        "editor.component_types",
        json!({ "query": "pointlight", "entities": [fresh.to_bits()] }),
    );
    let light = listed["types"]
        .as_array()
        .expect("types")
        .iter()
        .find(|t| t["type_path"] == "regia_render::lights::PointLight")
        .cloned()
        .expect("listed");
    assert_eq!(light["addable"], true);
    assert!(light.get("reason").is_none());
}

/// Item 3: on a prefab instance the baseline is the prefab's value, not the type default: a
/// light whose prefab sets a non-default intensity is unmodified until the instance changes it.
#[test]
fn modified_only_compares_a_prefab_instance_with_the_prefab() {
    use std::collections::BTreeMap;

    use regia_core::id::Guid;
    use regia_ecs::reflect::{AppTypeRegistry, DynamicTypePath};
    use regia_ecs::serialize::ReflectCodec;
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneEntity, SceneFile};
    use regia_scene::prefab::{PrefabLibrary, PrefabTemplate, instantiate_prefab};
    use regia_scene::prelude::EntityGuid;
    use regia_scene::spawn::SpawnOptions;
    use ron::value::RawValue;

    let config = regia_test_utils::unique_temp_dir("inspector-panel-agent-prefab");
    let mut rig = Rig::new(&config);
    let light = PointLight {
        intensity: PointLight::default().intensity * 3.0,
        ..PointLight::default()
    };
    let registry = rig.app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut codec = ReflectCodec::new(&registry);
    let light_ron = codec.to_ron_typed(&light).expect("light ron");
    drop(codec);
    let local = Guid::from_u128(1);
    let mut components = BTreeMap::new();
    components.insert(
        light.reflect_type_path().to_string(),
        RawValue::from_ron(&light_ron).expect("ron").to_owned(),
    );
    let mut file = SceneFile::new();
    file.entities.push(SceneEntity {
        guid: local,
        name: Some("Bulb".into()),
        parent: None,
        components,
        prefab_instance: None,
    });
    let asset = Guid::from_u128(0x7758);
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry).expect("bake");
    drop(registry);
    rig.app
        .world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert(template);
    let world = rig.app.world_mut();
    let root = world.spawn(EntityGuid(Guid::from_u128(500))).id();
    let instance = PrefabInstanceData::plain(SceneAssetRef(asset));
    let spawned =
        instantiate_prefab(world, &instance, root, &SpawnOptions::default(), 0).expect("instantiate");
    let bulb = spawned.locals[&local];
    let ids = json!([bulb.to_bits()]);

    let unchanged = rig.call(
        "editor.inspect_selection",
        json!({ "entities": ids, "modified_only": true }),
    );
    assert!(
        field_paths(&unchanged, "PointLight").is_empty(),
        "equal to the prefab although not to the type default: {unchanged}"
    );
    rig.app
        .world_mut()
        .get_mut::<PointLight>(bulb)
        .expect("light")
        .radius = 42.0;
    let changed = rig.call(
        "editor.inspect_selection",
        json!({ "entities": ids, "modified_only": true }),
    );
    assert_eq!(field_paths(&changed, "PointLight"), ["radius"], "{changed}");
}
