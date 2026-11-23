// SPDX-License-Identifier: MPL-2.0
//! Multi-select inspector editing (ADR-3948, `docs/specs/19-editor.md` § 2 and § 8).
//!
//! Equal fields show one value, differing fields are mixed, and one edit is one undo for every
//! selected entity that has the component. Vector lanes and Euler axes stay per entity.

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_ecs::attrs::AssetFilter;
use regia_ecs::reflect::{AppTypeRegistry, DynamicTypePath, PartialReflect};
use regia_ecs::serialize::ReflectCodec;
use regia_editor::agent_ext::{self, test_support};
use regia_editor::command::{UndoStack, redo_world, undo_world};
use regia_editor::inspector;
use regia_editor::inspector::impls::InspectorEguiImpl;
use regia_editor::inspector::reflect_ui::InspectorUi;
use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneEntity, SceneFile};
use regia_scene::prefab::{PrefabInstance, PrefabLibrary, PrefabTemplate, instantiate_prefab};
use regia_scene::prelude::{EntityGuid, Name, Transform};
use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene};
use ron::value::RawValue;
use serde_json::{Value, json};

/// A unit enum the inspector edits as one value.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
enum Mood {
    #[default]
    Calm,
    Angry,
}

/// Fields the multi-select inspector has to merge.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Prop {
    speed: f32,
    title: String,
    #[reflect(@AssetFilter("*.ogg"))]
    voice: String,
    tint: [f32; 3],
    mood: Mood,
    hidden: bool,
    /// A flag: one bool, written whole, like an enum.
    locked: bool,
    offset: Vec3,
}

impl Default for Prop {
    fn default() -> Self {
        Self {
            speed: 0.0,
            title: String::new(),
            voice: String::new(),
            tint: [0.0, 0.0, 0.0],
            mood: Mood::Calm,
            hidden: false,
            locked: false,
            offset: Vec3::ZERO,
        }
    }
}

/// Added and removed across the selection in the property test.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
struct Note {
    text: String,
}

fn prop_a() -> Prop {
    Prop {
        speed: 1.0,
        title: "a".into(),
        voice: "a.ogg".into(),
        tint: [1.0, 0.0, 0.0],
        mood: Mood::Calm,
        hidden: false,
        locked: false,
        offset: Vec3::new(1.0, 2.0, 3.0),
    }
}

fn prop_b() -> Prop {
    Prop {
        speed: 1.0,
        title: "b".into(),
        voice: "a.ogg".into(),
        tint: [1.0, 0.0, 1.0],
        mood: Mood::Angry,
        hidden: false,
        locked: true,
        offset: Vec3::new(9.0, 2.0, 3.0),
    }
}

fn euler(x: f32, y: f32, z: f32) -> Quat {
    Quat::from_rotation_z(z.to_radians())
        * Quat::from_rotation_y(y.to_radians())
        * Quat::from_rotation_x(x.to_radians())
}

fn transform_at(rotation: Quat) -> Transform {
    Transform {
        translation: Vec3::ZERO,
        rotation,
        scale: Vec3::ONE,
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Mood>();
    app.register_type::<Prop>();
    app.register_type::<Note>();
    app.init_resource::<UndoStack>();
    agent_ext::install(app.world_mut());
    app
}

fn depth(app: &mut App) -> usize {
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth()
}

fn spawn_prop(world: &mut World, guid: u128, prop: Prop, rotation: Quat) -> Entity {
    world
        .spawn((
            EntityGuid(Guid::from_u128(guid)),
            Name::new(format!("e{guid}")),
            transform_at(rotation),
            prop,
        ))
        .id()
}

fn component<'a>(inspected: &'a Value, name: &str) -> &'a Value {
    inspected["components"]
        .as_array()
        .expect("components")
        .iter()
        .find(|item| item["component"] == name)
        .unwrap_or_else(|| panic!("no {name} in {inspected}"))
}

fn field<'a>(component: &'a Value, path: &str) -> &'a Value {
    component["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|item| item["path"] == path)
        .unwrap_or_else(|| panic!("no {path} in {component}"))
}

fn axis<'a>(field: &'a Value, name: &str) -> &'a Value {
    field["axes"]
        .as_array()
        .expect("axes")
        .iter()
        .find(|item| item["axis"] == name)
        .unwrap_or_else(|| panic!("no axis {name} in {field}"))
}

fn near(value: Option<f64>, want: f32) {
    let got = value.expect("number");
    assert!((got - f64::from(want)).abs() < 0.05, "{got} is not near {want}");
}

fn reason(err: &regia_agent::protocol::RpcError) -> Option<&str> {
    err.data.as_ref().and_then(|data| data["reason"].as_str())
}

#[test]
fn mixed_fields_edits_axes_and_partial_components() {
    let mut app = app();
    let world = app.world_mut();
    let a = spawn_prop(world, 1, prop_a(), euler(10.0, 0.0, 0.0));
    let b = spawn_prop(world, 2, prop_b(), euler(10.0, 40.0, 0.0));
    let c = world
        .spawn((
            EntityGuid(Guid::from_u128(3)),
            Name::new("c"),
            transform_at(euler(10.0, 0.0, 0.0)),
        ))
        .id();
    let entities = [a, b, c];

    let inspected = inspector::inspect_entities(world, &entities, Some(a));
    assert_eq!(inspected["count"], 3);
    assert_eq!(inspected["primary"], a.to_bits());
    let prop = component(&inspected, "Prop");
    assert_eq!(prop["present"], 2);
    assert_eq!(prop["selected"], 3);
    assert_eq!(prop["editable"], true);
    assert_eq!(inspector::presence_suffix(2, 3).as_deref(), Some("on 2 of 3"));

    let speed = field(prop, "speed");
    assert_eq!(speed["mixed"], false);
    assert_eq!(speed["value"].as_f64(), Some(1.0));
    let voice = field(prop, "voice");
    assert_eq!(voice["mixed"], false);
    assert_eq!(voice["value"], "a.ogg");
    let hidden = field(prop, "hidden");
    assert_eq!(hidden["mixed"], false);
    assert_eq!(hidden["value"], false);

    assert_eq!(field(prop, "title")["mixed"], true);
    assert!(field(prop, "title")["value"].is_null());
    assert_eq!(field(prop, "mood")["mixed"], true);
    assert_eq!(field(prop, "locked")["mixed"], true);

    let tint = field(prop, "tint");
    assert_eq!(axis(tint, "r")["mixed"], false);
    assert_eq!(axis(tint, "g")["mixed"], false);
    assert_eq!(axis(tint, "b")["mixed"], true);
    assert!(axis(tint, "b")["value"].is_null());

    let offset = field(prop, "offset");
    assert_eq!(axis(offset, "x")["mixed"], true);
    assert_eq!(axis(offset, "y")["mixed"], false);
    assert_eq!(axis(offset, "z")["mixed"], false);
    near(axis(offset, "y")["value"].as_f64(), 2.0);
    near(axis(offset, "z")["value"].as_f64(), 3.0);

    let transform = component(&inspected, "Transform");
    let rotation = field(transform, "rotation");
    assert_eq!(axis(rotation, "rx")["mixed"], false);
    assert_eq!(axis(rotation, "ry")["mixed"], true);
    assert_eq!(axis(rotation, "rz")["mixed"], false);
    near(axis(rotation, "rx")["value"].as_f64(), 10.0);
    near(axis(rotation, "rz")["value"].as_f64(), 0.0);

    // Give B its own y and z so a whole-vector write would show up.
    world.get_mut::<Prop>(b).expect("b").offset.y = 4.0;
    world.get_mut::<Prop>(b).expect("b").offset.z = 7.0;
    world.get_mut::<Transform>(b).expect("b").rotation = euler(10.0, 40.0, 5.0);
    world.get_mut::<Prop>(b).expect("b").tint[0] = 0.25;

    let before = depth(&mut app);
    let world = app.world_mut();
    let report =
        inspector::edit_value(world, &entities, "Prop", "offset.x", Box::new(5.0_f32)).expect("offset.x");
    assert_eq!(report.edited, vec![a.to_bits(), b.to_bits()]);
    assert_eq!(report.skipped, vec![c.to_bits()]);
    assert_eq!(report.undo_depth, before + 1);
    assert_eq!(world.get::<Prop>(a).expect("a").offset, Vec3::new(5.0, 2.0, 3.0));
    assert_eq!(world.get::<Prop>(b).expect("b").offset, Vec3::new(5.0, 4.0, 7.0));
    assert!(undo_world(world));
    assert_eq!(world.get::<Prop>(a).expect("a").offset, Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(world.get::<Prop>(b).expect("b").offset, Vec3::new(9.0, 4.0, 7.0));

    inspector::edit_value(world, &[a, b], "Transform", "rotation.ry", Box::new(15.0_f32)).expect("ry");
    let ra = inspector::inspect_entities(world, &[a], Some(a));
    let rb = inspector::inspect_entities(world, &[b], Some(b));
    near(
        axis(field(component(&ra, "Transform"), "rotation"), "rx")["value"].as_f64(),
        10.0,
    );
    near(
        axis(field(component(&ra, "Transform"), "rotation"), "ry")["value"].as_f64(),
        15.0,
    );
    near(
        axis(field(component(&ra, "Transform"), "rotation"), "rz")["value"].as_f64(),
        0.0,
    );
    near(
        axis(field(component(&rb, "Transform"), "rotation"), "rx")["value"].as_f64(),
        10.0,
    );
    near(
        axis(field(component(&rb, "Transform"), "rotation"), "ry")["value"].as_f64(),
        15.0,
    );
    near(
        axis(field(component(&rb, "Transform"), "rotation"), "rz")["value"].as_f64(),
        5.0,
    );
    assert!(undo_world(world));

    inspector::edit_value(world, &[a, b], "Prop", "mood", Box::new(Mood::Calm)).expect("mood");
    assert_eq!(world.get::<Prop>(a).expect("a").mood, Mood::Calm);
    assert_eq!(world.get::<Prop>(b).expect("b").mood, Mood::Calm);
    assert!(undo_world(world));
    assert_eq!(world.get::<Prop>(b).expect("b").mood, Mood::Angry);

    inspector::edit_value(world, &[a, b], "Prop", "tint[2]", Box::new(0.5_f32)).expect("tint");
    assert_eq!(world.get::<Prop>(a).expect("a").tint, [1.0, 0.0, 0.5]);
    assert_eq!(world.get::<Prop>(b).expect("b").tint, [0.25, 0.0, 0.5]);
    assert!(undo_world(world));

    inspector::edit_value(world, &[a, b], "Prop", "tint.b", Box::new(0.4_f32)).expect("alias");
    assert_eq!(world.get::<Prop>(a).expect("a").tint[2], 0.4);
    assert_eq!(world.get::<Prop>(b).expect("b").tint[0], 0.25);
    assert!(undo_world(world));

    inspector::edit_value(world, &[a, b], "Prop", "locked", Box::new(false)).expect("flag");
    assert!(!world.get::<Prop>(a).expect("a").locked);
    assert!(!world.get::<Prop>(b).expect("b").locked);
    assert!(undo_world(world));

    inspector::edit_value(world, &[a, b], "Prop", "voice", Box::new("b.ogg".to_string())).expect("asset");
    assert_eq!(world.get::<Prop>(a).expect("a").voice, "b.ogg");
    assert_eq!(world.get::<Prop>(b).expect("b").voice, "b.ogg");
    assert!(undo_world(world));

    let path = world.get::<Prop>(a).expect("a").reflect_type_path().to_string();
    let added = inspector::add_component_to(world, &entities, &path).expect("add");
    assert_eq!(added.edited, vec![c.to_bits()]);
    assert!(world.get::<Prop>(c).is_some());
    assert_eq!(depth(&mut app), 1);
    assert!(undo_world(app.world_mut()));
    assert!(app.world().get::<Prop>(c).is_none());

    let world = app.world_mut();
    let removed = inspector::remove_component_from(world, &entities, "Prop").expect("remove");
    assert_eq!(removed.edited, vec![a.to_bits(), b.to_bits()]);
    assert_eq!(removed.skipped, vec![c.to_bits()]);
    assert!(world.get::<Prop>(a).is_none());
    assert!(world.get::<Prop>(b).is_none());
    assert_eq!(depth(&mut app), 1);
    assert!(undo_world(app.world_mut()));
    assert_eq!(app.world().get::<Prop>(a).expect("a").title, "a");
    assert_eq!(app.world().get::<Prop>(b).expect("b").title, "b");

    let err =
        inspector::edit_value(app.world_mut(), &[c], "Prop", "speed", Box::new(1.0_f32)).expect_err("none");
    assert_eq!(err.code, "component_on_none");
}

thread_local! {
    static POKE: RefCell<String> = const { RefCell::new(String::new()) };
}

fn poke_f32(value: &mut dyn Any, _: &mut egui::Ui, _: &dyn Any, _: egui::Id, env: InspectorUi<'_>) -> bool {
    if POKE.with(|poke| *poke.borrow() != env.context.path()) {
        return false;
    }
    if let Some(slot) = value.downcast_mut::<f32>() {
        *slot = 42.0;
        return true;
    }
    false
}

fn poke_vec3(value: &mut dyn Any, _: &mut egui::Ui, _: &dyn Any, _: egui::Id, env: InspectorUi<'_>) -> bool {
    if env.context.path() != "offset" {
        return false;
    }
    let Some(slot) = value.downcast_mut::<Vec3>() else {
        return false;
    };
    slot.x = 5.0;
    env.context.record_lane("x");
    true
}

fn poke_quat(value: &mut dyn Any, _: &mut egui::Ui, _: &dyn Any, _: egui::Id, env: InspectorUi<'_>) -> bool {
    if env.context.path() != "rotation" {
        return false;
    }
    let Some(slot) = value.downcast_mut::<Quat>() else {
        return false;
    };
    *slot = euler(0.0, 15.0, 0.0);
    env.context.record_lane("ry");
    true
}

fn install(
    app: &mut App,
    type_id: std::any::TypeId,
    widget: fn(&mut dyn Any, &mut egui::Ui, &dyn Any, egui::Id, InspectorUi<'_>) -> bool,
) {
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let mut registry = registry.write();
    if let Some(registration) = registry.get_mut(type_id) {
        registration.insert(InspectorEguiImpl::new(widget));
    }
}

fn draw_multi(app: &mut App, entities: &[Entity], component: &str) -> Vec<inspector::Change> {
    let ctx = egui::Context::default();
    let mut changes = Vec::new();
    let world = app.world_mut();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        changes = inspector::component_ui_multi(
            ui,
            world,
            entities,
            Some(entities[0]),
            component,
            std::sync::Arc::from([]),
        );
    });
    output.textures_delta.clear();
    changes
}

#[test]
fn drawer_multi_edit_is_one_undo() {
    let mut app = app();
    let world = app.world_mut();
    let a = spawn_prop(world, 1, prop_a(), euler(10.0, 0.0, 0.0));
    let b = spawn_prop(world, 2, prop_b(), euler(10.0, 40.0, 5.0));
    world.get_mut::<Prop>(b).expect("b").offset = Vec3::new(9.0, 4.0, 7.0);

    // A mixed frame through the real drawers does not panic and does not write.
    let _ = draw_multi(&mut app, &[a, b], "Prop");
    assert_eq!(app.world().get::<Prop>(a).expect("a").speed, 1.0);
    assert_eq!(app.world().get::<Prop>(b).expect("b").title, "b");
    assert_eq!(depth(&mut app), 0);

    install(&mut app, std::any::TypeId::of::<f32>(), poke_f32);
    POKE.with(|poke| *poke.borrow_mut() = "speed".into());
    let changes = draw_multi(&mut app, &[a, b], "Prop");
    assert!(changes.iter().any(|change| change.path == "speed"), "{changes:?}");
    assert_eq!(app.world().get::<Prop>(a).expect("a").speed, 42.0);
    assert_eq!(app.world().get::<Prop>(b).expect("b").speed, 42.0);
    assert_eq!(depth(&mut app), 1);
    assert!(undo_world(app.world_mut()));
    assert_eq!(app.world().get::<Prop>(a).expect("a").speed, 1.0);
    assert_eq!(app.world().get::<Prop>(b).expect("b").speed, 1.0);
    assert_eq!(depth(&mut app), 0);

    install(&mut app, std::any::TypeId::of::<Vec3>(), poke_vec3);
    let changes = draw_multi(&mut app, &[a, b], "Prop");
    assert!(
        changes.iter().any(|change| change.path == "offset.x"),
        "{changes:?}"
    );
    assert!(!changes.iter().any(|change| change.path == "offset"));
    assert_eq!(
        app.world().get::<Prop>(a).expect("a").offset,
        Vec3::new(5.0, 2.0, 3.0)
    );
    assert_eq!(
        app.world().get::<Prop>(b).expect("b").offset,
        Vec3::new(5.0, 4.0, 7.0)
    );
    assert!(undo_world(app.world_mut()));

    install(&mut app, std::any::TypeId::of::<Quat>(), poke_quat);
    let changes = draw_multi(&mut app, &[a, b], "Transform");
    assert!(
        changes.iter().any(|change| change.path == "rotation.ry"),
        "{changes:?}"
    );
    let world = app.world_mut();
    let left = inspector::inspect_entities(world, &[a], Some(a));
    let right = inspector::inspect_entities(world, &[b], Some(b));
    near(
        axis(field(component(&left, "Transform"), "rotation"), "ry")["value"].as_f64(),
        15.0,
    );
    near(
        axis(field(component(&left, "Transform"), "rotation"), "rx")["value"].as_f64(),
        10.0,
    );
    near(
        axis(field(component(&right, "Transform"), "rotation"), "ry")["value"].as_f64(),
        15.0,
    );
    near(
        axis(field(component(&right, "Transform"), "rotation"), "rz")["value"].as_f64(),
        5.0,
    );
    assert_eq!(depth(&mut app), 1);
    assert!(undo_world(app.world_mut()));
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn scene_text(app: &mut App) -> String {
    let file = snapshot_scene(
        app.world_mut(),
        &SnapshotFilter {
            level: None,
            resources: Vec::new(),
        },
    )
    .expect("snapshot");
    file.to_ron_string().expect("ron")
}

#[test]
fn random_multi_edits_undo_to_the_same_scene() {
    let mut app = app();
    let world = app.world_mut();
    let a = spawn_prop(world, 11, prop_a(), euler(10.0, 0.0, 0.0));
    let b = spawn_prop(world, 12, prop_b(), euler(20.0, 30.0, 40.0));
    let entities = [a, b];
    let before = scene_text(&mut app);
    let again = scene_text(&mut app);
    assert_eq!(before, again, "a snapshot of an untouched world changes");

    let mut state = 0x3470_u64;
    let mut successes = 0_u32;
    let mut attempts = 0_u32;
    let mut structural_adds = 0;
    let mut structural_removes = 0;
    while successes < 40 && attempts < 200 {
        attempts += 1;
        let roll = xorshift(&mut state);
        let choice = roll >> 8;
        let depth_before = depth(&mut app);
        let world = app.world_mut();
        let result = match roll % 8 {
            0 => inspector::edit_value(world, &entities, "Prop", "speed", Box::new((roll % 50) as f32)),
            1 => inspector::edit_value(world, &entities, "Prop", "offset.x", Box::new((roll % 20) as f32)),
            2 => inspector::edit_value(world, &entities, "Prop", "offset.y", Box::new((roll % 20) as f32)),
            3 => {
                let mood: Box<dyn PartialReflect> = if choice.is_multiple_of(2) {
                    Box::new(Mood::Calm)
                } else {
                    Box::new(Mood::Angry)
                };
                inspector::edit_value(world, &entities, "Prop", "mood", mood)
            }
            4 => {
                let index = roll % 3;
                inspector::edit_value(
                    world,
                    &entities,
                    "Prop",
                    &format!("tint[{index}]"),
                    Box::new((roll % 10) as f32 * 0.1),
                )
            }
            5 => inspector::edit_value(
                world,
                &entities,
                "Transform",
                "rotation.ry",
                Box::new((roll % 90) as f32),
            ),
            6 => inspector::edit_value(
                world,
                &entities,
                "Prop",
                "title",
                Box::new(format!("t{}", roll % 7)),
            ),
            _ => {
                if choice.is_multiple_of(2) {
                    let result =
                        inspector::add_component_to(world, &entities, Note::default().reflect_type_path());
                    if result.as_ref().is_ok_and(|report| !report.edited.is_empty()) {
                        structural_adds += 1;
                    }
                    result
                } else {
                    let result = inspector::remove_component_from(world, &entities, "Note");
                    if result.as_ref().is_ok_and(|report| !report.edited.is_empty()) {
                        structural_removes += 1;
                    }
                    result
                }
            }
        };
        if result.is_ok() && depth(&mut app) > depth_before {
            successes += 1;
        }
    }
    assert!(
        successes >= 40,
        "only {successes} edits succeeded in {attempts} attempts"
    );
    assert!(depth(&mut app) > 0);
    assert!(structural_adds > 0 && structural_removes > 0);

    let mut guard = 0_u32;
    while depth(&mut app) > 0 {
        assert!(undo_world(app.world_mut()), "undo stopped early");
        guard += 1;
        assert!(guard < 100, "undo did not drain");
    }
    assert_eq!(scene_text(&mut app), before);
}

fn raw_ron(text: &str) -> Box<RawValue> {
    RawValue::from_ron(text).expect("ron").to_owned()
}

#[test]
fn prefab_multi_edit_revert_and_apply() {
    let mut app = app();
    let body = Prop {
        speed: 3.0,
        title: "body".into(),
        voice: "a.ogg".into(),
        tint: [0.0, 0.0, 0.0],
        mood: Mood::Calm,
        hidden: false,
        locked: false,
        offset: Vec3::ZERO,
    };
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut codec = ReflectCodec::new(&registry);
    let prop_ron = codec.to_ron_typed(&body).expect("prop ron");
    let transform_ron = codec.to_ron_typed(&Transform::IDENTITY).expect("transform ron");
    drop(codec);
    let prop_type = body.reflect_type_path().to_string();
    let transform_type = Transform::IDENTITY.reflect_type_path().to_string();
    let local = Guid::from_u128(1);
    let mut components = BTreeMap::new();
    components.insert(prop_type.clone(), raw_ron(&prop_ron));
    components.insert(transform_type, raw_ron(&transform_ron));
    let mut file = SceneFile::new();
    file.entities.push(SceneEntity {
        guid: local,
        name: Some("Body".into()),
        parent: None,
        components,
        prefab_instance: None,
    });
    let asset = Guid::from_u128(0x3470);
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry).expect("bake");
    assert!(template.warnings.is_empty(), "{:?}", template.warnings);
    drop(registry);
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);

    let mut children = Vec::new();
    let mut roots = Vec::new();
    for n in 0..2_u128 {
        let world = app.world_mut();
        let root = world.spawn(EntityGuid(Guid::from_u128(100 + n))).id();
        let instance = PrefabInstanceData::plain(SceneAssetRef(asset));
        let spawned =
            instantiate_prefab(world, &instance, root, &SpawnOptions::default(), 0).expect("instantiate");
        children.push(spawned.locals[&local]);
        roots.push(root);
    }
    let (child_a, child_b) = (children[0], children[1]);
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 3.0);

    let world = app.world_mut();
    inspector::edit_value(world, &[child_a, child_b], "Prop", "speed", Box::new(9.0_f32))
        .expect("edit children");
    assert_eq!(depth(&mut app), 1);
    for root in &roots {
        let instance = app.world().get::<PrefabInstance>(*root).expect("instance");
        assert_eq!(instance.overrides.len(), 1, "{:?}", instance.overrides);
        assert!(
            instance.overrides[0].path.ends_with("speed"),
            "{:?}",
            instance.overrides
        );
    }
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 9.0);
    assert_eq!(app.world().get::<Prop>(child_b).expect("b").speed, 9.0);

    assert!(undo_world(app.world_mut()));
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 3.0);
    assert_eq!(app.world().get::<Prop>(child_b).expect("b").speed, 3.0);
    for root in &roots {
        assert!(
            app.world()
                .get::<PrefabInstance>(*root)
                .expect("instance")
                .overrides
                .is_empty()
        );
    }
    assert!(redo_world(app.world_mut()));
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 9.0);
    assert_eq!(
        app.world()
            .get::<PrefabInstance>(roots[0])
            .expect("instance")
            .overrides
            .len(),
        1
    );

    inspector::revert_prefab_override(app.world_mut(), child_a, "Prop", "speed").expect("revert");
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 3.0);
    assert!(
        app.world()
            .get::<PrefabInstance>(roots[0])
            .expect("instance")
            .overrides
            .is_empty()
    );
    assert_eq!(app.world().get::<Prop>(child_b).expect("b").speed, 9.0);
    assert_eq!(
        app.world()
            .get::<PrefabInstance>(roots[1])
            .expect("instance")
            .overrides
            .len(),
        1
    );

    inspector::apply_prefab_override(app.world_mut(), child_b, "Prop", "speed").expect("apply");
    assert_eq!(app.world().get::<Prop>(child_b).expect("b").speed, 9.0);
    assert!(
        app.world()
            .get::<PrefabInstance>(roots[1])
            .expect("instance")
            .overrides
            .is_empty()
    );
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 3.0);

    // Apply is a command: undo restores the template as well as the instance marker.
    assert!(undo_world(app.world_mut()));
    assert_eq!(
        app.world()
            .get::<PrefabInstance>(roots[1])
            .expect("instance")
            .overrides
            .len(),
        1
    );
    let fourth = app.world_mut().spawn(EntityGuid(Guid::from_u128(201))).id();
    let restored = instantiate_prefab(
        app.world_mut(),
        &PrefabInstanceData::plain(SceneAssetRef(asset)),
        fourth,
        &SpawnOptions::default(),
        0,
    )
    .expect("instantiate after undo");
    assert_eq!(
        app.world()
            .get::<Prop>(restored.locals[&local])
            .expect("restored")
            .speed,
        3.0
    );
    assert!(redo_world(app.world_mut()));

    let world = app.world_mut();
    let third = world.spawn(EntityGuid(Guid::from_u128(200))).id();
    let spawned = instantiate_prefab(
        world,
        &PrefabInstanceData::plain(SceneAssetRef(asset)),
        third,
        &SpawnOptions::default(),
        0,
    )
    .expect("third");
    let third_body = spawned.locals[&local];
    assert_eq!(app.world().get::<Prop>(third_body).expect("third").speed, 9.0);
    assert_eq!(app.world().get::<Prop>(child_a).expect("a").speed, 3.0);
}

#[test]
fn agent_entities_and_inspect_selection() {
    let mut app = app();
    let world = app.world_mut();
    let a = spawn_prop(world, 1, prop_a(), euler(10.0, 0.0, 0.0));
    let b = spawn_prop(world, 2, prop_b(), euler(12.0, 40.0, 1.0));
    let bare = world.spawn(Transform::IDENTITY).id();

    let single = test_support::set_property(
        app.world_mut(),
        &json!({
            "entity": a.to_bits(),
            "component": "Prop",
            "path": "speed",
            "value": 4.0
        }),
    )
    .expect("single");
    assert!(single.get("entities").is_none(), "{single}");
    assert_eq!(single["applied"], true);
    assert_eq!(single["before"].as_f64(), Some(1.0));
    assert_eq!(single["after"].as_f64(), Some(4.0));
    assert_eq!(app.world().get::<Prop>(b).expect("b").speed, 1.0);
    assert!(test_support::undo(app.world_mut()));
    assert_eq!(app.world().get::<Prop>(a).expect("a").speed, 1.0);

    let before_depth = depth(&mut app);
    let many = test_support::set_property(
        app.world_mut(),
        &json!({
            "entities": [a.to_bits(), b.to_bits()],
            "component": "Prop",
            "path": "speed",
            "value": 6.0
        }),
    )
    .expect("many");
    assert_eq!(many["applied"], true);
    assert_eq!(many["undoable"], before_depth + 1);
    assert_eq!(many["entities"].as_array().expect("entities").len(), 2);
    assert_eq!(app.world().get::<Prop>(a).expect("a").speed, 6.0);
    assert_eq!(app.world().get::<Prop>(b).expect("b").speed, 6.0);
    assert!(test_support::undo(app.world_mut()));
    assert_eq!(app.world().get::<Prop>(a).expect("a").speed, 1.0);
    assert_eq!(app.world().get::<Prop>(b).expect("b").speed, 1.0);
    assert_eq!(depth(&mut app), before_depth);

    let dry = test_support::set_property(
        app.world_mut(),
        &json!({
            "entities": [a.to_bits(), b.to_bits()],
            "component": "Prop",
            "path": "title",
            "value": "same",
            "dry_run": true
        }),
    )
    .expect("dry");
    assert_eq!(dry["applied"], false);
    assert_eq!(depth(&mut app), before_depth);
    assert_eq!(app.world().get::<Prop>(a).expect("a").title, "a");

    let world = app.world_mut();
    world.get_mut::<Transform>(b).expect("b").rotation = euler(12.0, 40.0, 8.0);
    test_support::set_property(
        world,
        &json!({
            "entities": [a.to_bits(), b.to_bits()],
            "component": "Transform",
            "path": "rotation.ry",
            "value": 15.0
        }),
    )
    .expect("euler");
    let left = inspector::inspect_entities(app.world(), &[a], Some(a));
    let right = inspector::inspect_entities(app.world(), &[b], Some(b));
    near(
        axis(field(component(&left, "Transform"), "rotation"), "ry")["value"].as_f64(),
        15.0,
    );
    near(
        axis(field(component(&left, "Transform"), "rotation"), "rx")["value"].as_f64(),
        10.0,
    );
    near(
        axis(field(component(&right, "Transform"), "rotation"), "rz")["value"].as_f64(),
        8.0,
    );
    assert!(test_support::undo(app.world_mut()));

    let moved = test_support::set_transform(
        app.world_mut(),
        &json!({
            "entities": [a.to_bits(), b.to_bits()],
            "translation": [1.0, 2.0, 3.0]
        }),
    )
    .expect("transform");
    assert_eq!(moved["undoable"].as_u64().unwrap_or(0), (before_depth + 1) as u64);
    assert_eq!(
        app.world().get::<Transform>(a).expect("a").translation,
        Vec3::new(1.0, 2.0, 3.0)
    );
    assert_eq!(
        app.world().get::<Transform>(b).expect("b").translation,
        Vec3::new(1.0, 2.0, 3.0)
    );
    // The omitted rotation stayed per entity (the undo above restored the euler edit).
    let left = inspector::inspect_entities(app.world(), &[a], Some(a));
    near(
        axis(field(component(&left, "Transform"), "rotation"), "rx")["value"].as_f64(),
        10.0,
    );
    assert!(test_support::undo(app.world_mut()));

    // `mode: "set"` replaces the selection once per id, so a list needs `"add"`.
    test_support::select(
        app.world_mut(),
        &json!({"entities": [a.to_bits(), b.to_bits()], "mode": "add"}),
    )
    .expect("select");
    let inspected = test_support::inspect_selection(app.world_mut(), &json!({})).expect("inspect");
    assert_eq!(inspected["count"], 2);
    let prop = component(&inspected, "Prop");
    assert_eq!(field(prop, "speed")["mixed"], false);
    assert_eq!(field(prop, "title")["mixed"], true);
    assert_eq!(field(prop, "locked")["mixed"], true);

    let explicit = test_support::inspect_selection(app.world_mut(), &json!({"entities": []})).expect("empty");
    assert_eq!(explicit["count"], 0);
    assert!(explicit["components"].as_array().expect("components").is_empty());

    let invalid = test_support::inspect_selection(app.world_mut(), &json!({"entities": null}))
        .expect_err("explicit null is not an array");
    assert_eq!(reason(&invalid), Some("entities_not_array"));

    let both = test_support::set_property(
        app.world_mut(),
        &json!({
            "entity": a.to_bits(),
            "entities": [a.to_bits()],
            "component": "Prop",
            "path": "speed",
            "value": 1.0
        }),
    )
    .expect_err("both");
    assert_eq!(reason(&both), Some("entity_and_entities"));

    let empty = test_support::set_property(
        app.world_mut(),
        &json!({
            "entities": [],
            "component": "Prop",
            "path": "speed",
            "value": 1.0
        }),
    )
    .expect_err("empty");
    assert_eq!(reason(&empty), Some("entities_empty"));

    let not_array = test_support::set_property(
        app.world_mut(),
        &json!({
            "entities": 4,
            "component": "Prop",
            "path": "speed",
            "value": 1.0
        }),
    )
    .expect_err("not array");
    assert_eq!(reason(&not_array), Some("entities_not_array"));

    let none = test_support::set_property(
        app.world_mut(),
        &json!({
            "entities": [bare.to_bits()],
            "component": "Prop",
            "path": "speed",
            "value": 1.0
        }),
    )
    .expect_err("no component");
    assert_eq!(reason(&none), Some("component_on_none"));

    let missing = test_support::set_property(
        app.world_mut(),
        &json!({
            "entity": 999_u64,
            "component": "Prop",
            "path": "speed",
            "value": 1.0
        }),
    )
    .expect_err("missing entity");
    assert!(reason(&missing).is_none());
    assert!(missing.message.contains("does not exist"));
}

#[test]
fn multi_edits_dispatch_through_the_agent_inbox() {
    use regia_agent::protocol::Request;
    use regia_agent::server::Inbound;

    let project = regia_test_utils::unique_temp_dir("inspector-multi-agent");
    std::fs::create_dir_all(&project).expect("project");
    let mut app = app();
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
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
    let a = spawn_prop(app.world_mut(), 101, prop_a(), Quat::IDENTITY);
    let b = spawn_prop(app.world_mut(), 102, prop_b(), Quat::IDENTITY);
    let mut next_id = 0;
    let mut call = |app: &mut App, method: &str, params: Value| {
        next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(next_id)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("inbox");
        app.update();
        let response = answer.try_recv().expect("response within one frame");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    };
    let selected = json!({"entities": [a.to_bits(), b.to_bits()]});
    let merged = call(&mut app, "editor.inspect_selection", selected.clone());
    assert_eq!(field(component(&merged, "Prop"), "title")["mixed"], true);
    call(
        &mut app,
        "editor.set_property",
        json!({"entities": [a.to_bits(), b.to_bits()], "component": "Prop", "path": "speed", "value": 8.0}),
    );
    assert_eq!(depth(&mut app), 1);
    let merged = call(&mut app, "editor.inspect_selection", selected.clone());
    assert_eq!(field(component(&merged, "Prop"), "speed")["value"], 8.0);
    call(
        &mut app,
        "editor.set_transform",
        json!({"entities": [a.to_bits(), b.to_bits()], "translation": [2.0, 3.0, 4.0]}),
    );
    assert_eq!(depth(&mut app), 2);
    call(&mut app, "editor.undo", json!({}));
    call(&mut app, "editor.undo", json!({}));
    let merged = call(&mut app, "editor.inspect_selection", selected);
    assert_eq!(field(component(&merged, "Prop"), "speed")["value"], 1.0);
    assert_eq!(
        app.world().get::<Transform>(b).expect("transform").translation,
        Vec3::ZERO
    );
    drop(app);
    std::fs::remove_dir_all(project).expect("cleanup");
}

#[derive(Reflect, Clone, Debug, Default, PartialEq)]
enum EditMode {
    Tuned(f32),
    Count(i32),
    #[default]
    Other,
}

#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct VariantProp {
    mode: EditMode,
}

#[test]
fn missing_field_on_another_variant_leaves_the_whole_selection_unchanged() {
    let mut app = app();
    app.register_type::<EditMode>();
    app.register_type::<VariantProp>();
    let a = app
        .world_mut()
        .spawn(VariantProp {
            mode: EditMode::Tuned(1.0),
        })
        .id();
    let b = app
        .world_mut()
        .spawn(VariantProp {
            mode: EditMode::Other,
        })
        .id();
    let original = regia_editor::command::read_property(app.world(), a, "VariantProp", "mode.0")
        .expect("the field exists on the first variant");
    assert_eq!(original.try_downcast_ref::<f32>(), Some(&1.0));
    let error = inspector::edit_value(
        app.world_mut(),
        &[a, b],
        "VariantProp",
        "mode.0",
        Box::new(9.0_f32),
    )
    .expect_err("the field does not exist on every variant");
    assert_eq!(error.code, "path_unknown");
    assert_eq!(
        app.world().get::<VariantProp>(a).expect("a").mode,
        EditMode::Tuned(1.0)
    );
    assert_eq!(
        app.world().get::<VariantProp>(b).expect("b").mode,
        EditMode::Other
    );
    assert_eq!(depth(&mut app), 0);
}

#[test]
fn incompatible_variant_field_types_leave_the_whole_selection_unchanged() {
    let mut app = app();
    app.register_type::<EditMode>();
    app.register_type::<VariantProp>();
    let a = app
        .world_mut()
        .spawn(VariantProp {
            mode: EditMode::Tuned(1.0),
        })
        .id();
    let b = app
        .world_mut()
        .spawn(VariantProp {
            mode: EditMode::Count(3),
        })
        .id();
    let error = inspector::edit_value(
        app.world_mut(),
        &[a, b],
        "VariantProp",
        "mode.0",
        Box::new(9.0_f32),
    )
    .expect_err("the field has a different type on the other variant");
    assert_eq!(error.code, "value_mismatch");
    assert_eq!(
        app.world().get::<VariantProp>(a).expect("a").mode,
        EditMode::Tuned(1.0)
    );
    assert_eq!(
        app.world().get::<VariantProp>(b).expect("b").mode,
        EditMode::Count(3)
    );
    assert_eq!(depth(&mut app), 0);
}

#[test]
fn shared_euler_drawer_still_edits_a_single_entity_and_undoes() {
    let mut app = app();
    let a = spawn_prop(app.world_mut(), 901, prop_a(), euler(10.0, 20.0, 5.0));
    let before = *app.world().get::<Transform>(a).expect("transform");
    install(&mut app, std::any::TypeId::of::<Quat>(), poke_quat);
    let ctx = egui::Context::default();
    let mut changes = Vec::new();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        changes =
            inspector::component_ui_shared(ui, app.world_mut(), a, "Transform", std::sync::Arc::from([]));
    });
    output.textures_delta.clear();
    assert!(changes.iter().any(|change| change.path == "rotation.ry"));
    let inspected = inspector::inspect_entities(app.world(), &[a], Some(a));
    let rotation = field(component(&inspected, "Transform"), "rotation");
    near(axis(rotation, "rx")["value"].as_f64(), 10.0);
    near(axis(rotation, "ry")["value"].as_f64(), 15.0);
    near(axis(rotation, "rz")["value"].as_f64(), 5.0);
    assert_eq!(depth(&mut app), 1);
    assert!(undo_world(app.world_mut()));
    assert_eq!(*app.world().get::<Transform>(a).expect("transform"), before);
}

#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct MatrixProp {
    basis: glam::Mat3,
}

fn poke_matrix_column(
    value: &mut dyn Any,
    _: &mut egui::Ui,
    _: &dyn Any,
    _: egui::Id,
    env: InspectorUi<'_>,
) -> bool {
    if env.context.path() != "basis" {
        return false;
    }
    let Some(column) = value.downcast_mut::<Vec3>() else {
        return false;
    };
    // Only the first column changes, as with a user dragging that one widget.
    if column.y != 0.0 || column.z != 0.0 {
        return false;
    }
    column.x = 5.0;
    env.context.record_lane("x");
    true
}

#[test]
fn matrix_drawers_record_the_matrix_instead_of_invalid_vector_paths() {
    let mut app = app();
    app.register_type::<glam::Mat3>();
    app.register_type::<MatrixProp>();
    let left = glam::Mat3::IDENTITY;
    let right = glam::Mat3::from_diagonal(Vec3::splat(2.0));
    let a = app.world_mut().spawn(MatrixProp { basis: left }).id();
    let b = app.world_mut().spawn(MatrixProp { basis: right }).id();
    install(&mut app, std::any::TypeId::of::<Vec3>(), poke_matrix_column);
    let changes = draw_multi(&mut app, &[a, b], "MatrixProp");
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].path, "basis");
    let want = glam::Mat3::from_diagonal(Vec3::new(5.0, 1.0, 1.0));
    assert_eq!(app.world().get::<MatrixProp>(a).expect("a").basis, want);
    assert_eq!(app.world().get::<MatrixProp>(b).expect("b").basis, want);
    assert_eq!(depth(&mut app), 1);
    assert!(undo_world(app.world_mut()));
    assert_eq!(app.world().get::<MatrixProp>(a).expect("a").basis, left);
    assert_eq!(app.world().get::<MatrixProp>(b).expect("b").basis, right);
}

#[test]
fn sub_degree_euler_differences_are_mixed_and_edits_are_not_discarded() {
    let mut app = app();
    let a = spawn_prop(app.world_mut(), 311, prop_a(), euler(10.0, 0.0, 2.0));
    let b = spawn_prop(app.world_mut(), 312, prop_b(), euler(10.0, 0.1, 2.0));
    let before_a = *app.world().get::<Transform>(a).expect("a");
    let before_b = *app.world().get::<Transform>(b).expect("b");
    let inspected = inspector::inspect_entities(app.world(), &[a, b], Some(a));
    assert_eq!(
        axis(field(component(&inspected, "Transform"), "rotation"), "ry")["mixed"],
        true
    );
    let edited = transform_at(euler(0.0, 0.2, 0.0));
    let changes = [inspector::Change {
        path: "rotation.ry".into(),
        discrete: false,
    }];
    let report = inspector::commit_widget_changes(app.world_mut(), &[a, b], "Transform", &edited, &changes)
        .expect("fine rotation");
    assert_eq!(report.edited.len(), 2);
    assert_eq!(report.undo_depth, 1);
    for entity in [a, b] {
        let inspected = inspector::inspect_entities(app.world(), &[entity], Some(entity));
        let rotation = field(component(&inspected, "Transform"), "rotation");
        near(axis(rotation, "rx")["value"].as_f64(), 10.0);
        near(axis(rotation, "ry")["value"].as_f64(), 0.2);
        near(axis(rotation, "rz")["value"].as_f64(), 2.0);
    }
    assert!(undo_world(app.world_mut()));
    assert_eq!(*app.world().get::<Transform>(a).expect("a"), before_a);
    assert_eq!(*app.world().get::<Transform>(b).expect("b"), before_b);
}
