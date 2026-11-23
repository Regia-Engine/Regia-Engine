// SPDX-License-Identifier: MPL-2.0
//! View-model binding (ADR-1811, `docs/specs/81-ui-designer.md` § 2): a `.ui.ron` v3 layout bound
//! to registered view models. One-way, one-time and inverted bindings, the visible / enabled /
//! value / input / selected targets, two-way writes driven by real widget edits (`ui.set_value`
//! through the agent bridge, the same path a person's input takes), the rule that only
//! registered view models are written, integer conversion, and click / submit commands.
//! Headless: no window, no GPU, fixed 100 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::agent_bridge::call;
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::prelude::*;
use regia_ui::{
    BindMode, Disabled, NodeName, NodeStyle, UiCommand, UiMigrators, ViewModelAppExt, ViewModels,
    parse_ui_ron,
};
use serde_json::json;

#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct HudVm {
    health: f32,
    dead: bool,
    ammo: u8,
}

#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct SettingsVm {
    volume: f32,
    muted: bool,
    quality: usize,
    name: String,
    dirty: bool,
}

/// Registered for reflection but not as a view model: two-way writes must not touch it.
#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct Plain {
    volume: f32,
}

/// A resource and a plain type with one short name: the registry cannot look the name up
/// uniquely, so a binding has to find the one that is in the world.
mod twin_a {
    use super::*;

    #[derive(Reflect, Debug, Default, Clone, PartialEq)]
    pub struct TwinVm {
        pub unrelated: u32,
    }
}

mod twin_b {
    use super::*;

    #[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
    #[reflect(Resource, Default)]
    pub struct TwinVm {
        pub value: f32,
    }
}

const LAYOUT: &str = r#"(
    version: 3,
    root: (
        children: [
            (name: "hp", widget: progress(0.0), bindings: [
                (source: "HudVm.health", target: progress),
                (source: "HudVm.dead", target: visible, invert: true),
            ]),
            (name: "ammo", text: (spans: ["-"]), bindings: [(source: "HudVm.ammo", format: "{:02}")]),
            (name: "ammo_once", text: (spans: ["-"]), bindings: [(source: "HudVm.ammo", mode: one_time)]),
            (name: "volume", widget: slider(0.0, 0.0, 1.0, 0.0),
                bindings: [(source: "SettingsVm.volume", target: value, mode: two_way)]),
            (name: "mute", widget: toggle(false),
                bindings: [(source: "SettingsVm.muted", target: checked, mode: two_way, invert: true)]),
            (name: "quality", widget: dropdown(["Low", "Mid", "High"], 0),
                bindings: [(source: "SettingsVm.quality", target: selected, mode: two_way)]),
            (name: "player", widget: text_input("Name", false), command: "profile.rename",
                bindings: [(source: "SettingsVm.name", target: input, mode: two_way)]),
            (name: "apply", widget: button, command: "settings.apply",
                bindings: [(source: "SettingsVm.dirty", target: enabled)]),
            (name: "plain", widget: slider(0.0, 0.0, 1.0, 0.0),
                bindings: [(source: "Plain.volume", target: value, mode: two_way)]),
            (name: "ammo_slider", widget: slider(0.0, 0.0, 1000.0, 0.0),
                bindings: [(source: "HudVm.ammo", target: value, mode: two_way)]),
        ],
    ),
)"#;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.register_view_model::<HudVm>();
    app.register_view_model::<SettingsVm>();
    app.register_type::<Plain>();
    app.insert_resource(Plain::default());
    app.init_resource::<SeenCommands>()
        .add_systems(PostUpdate, record_commands);
    let file = parse_ui_ron(LAYOUT, "test.ui.ron", &UiMigrators::default()).expect("layout");
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("settings".to_owned(), file);
    spawn_ui(app.world_mut(), "settings").expect("spawn");
    app
}

/// Every `UiCommand` of the run.
#[derive(Resource, Default)]
struct SeenCommands(Vec<UiCommand>);

fn record_commands(mut events: MessageReader<UiCommand>, mut seen: ResMut<SeenCommands>) {
    seen.0.extend(events.read().cloned());
}

fn node(app: &mut App, name: &str) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &NodeName)>();
    q.iter(app.world())
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node {name}"))
}

fn text_of(app: &mut App, name: &str) -> String {
    let e = node(app, name);
    match &app.world().get::<Text>(e).expect("text").spans[0].content {
        regia_ui::text::TextContent::Lit(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn set_value(app: &mut App, name: &str, value: serde_json::Value) {
    let e = node(app, name);
    call(
        app.world_mut(),
        "ui.set_value",
        &json!({ "node": e.to_bits(), "value": value }),
    )
    .expect("queued");
    app.update();
    app.update();
}

#[test]
fn register_view_model_records_fields_and_kinds() {
    let app = app();
    let models = app.world().resource::<ViewModels>();
    let hud = models.get("HudVm").expect("HudVm registered");
    let kinds: Vec<(&str, &str)> = hud.fields.iter().map(|f| (f.name.as_str(), f.kind)).collect();
    assert_eq!(
        kinds,
        vec![("health", "number"), ("dead", "bool"), ("ammo", "integer")]
    );
    assert_eq!(
        models
            .get("SettingsVm")
            .and_then(|m| m.field("name"))
            .map(|f| f.kind),
        Some("text")
    );
    assert!(
        models.get("Plain").is_none(),
        "a reflected resource is not a view model by itself"
    );
}

#[test]
fn one_way_bindings_follow_the_model() {
    let mut app = app();
    app.world_mut().resource_mut::<HudVm>().health = 0.75;
    app.world_mut().resource_mut::<HudVm>().ammo = 7;
    app.update();
    let hp = node(&mut app, "hp");
    assert_eq!(app.world().get::<ProgressBar>(hp).unwrap().value, 0.75);
    assert_eq!(text_of(&mut app, "ammo"), "07");
    // Visible is inverted `dead`: alive shows the bar.
    assert_ne!(
        app.world().get::<NodeStyle>(hp).unwrap().0.display,
        regia_ui::Display::None
    );

    app.world_mut().resource_mut::<HudVm>().dead = true;
    app.world_mut().resource_mut::<HudVm>().ammo = 3;
    app.update();
    assert_eq!(
        app.world().get::<NodeStyle>(hp).unwrap().0.display,
        regia_ui::Display::None
    );
    assert_eq!(text_of(&mut app, "ammo"), "03");
    // One-time: the first value stays.
    assert_eq!(text_of(&mut app, "ammo_once"), "7");

    // Showing again restores the authored display.
    app.world_mut().resource_mut::<HudVm>().dead = false;
    app.update();
    assert_eq!(
        app.world().get::<NodeStyle>(hp).unwrap().0.display,
        regia_ui::Display::Flex
    );
}

#[test]
fn enabled_target_toggles_the_disabled_marker() {
    let mut app = app();
    app.update();
    let apply = node(&mut app, "apply");
    assert!(
        app.world().get::<Disabled>(apply).is_some(),
        "dirty = false disables"
    );
    app.world_mut().resource_mut::<SettingsVm>().dirty = true;
    app.update();
    assert!(app.world().get::<Disabled>(apply).is_none());
}

#[test]
fn two_way_widget_edits_write_the_view_model() {
    let mut app = app();
    app.update();
    set_value(&mut app, "volume", json!(0.25));
    assert_eq!(app.world().resource::<SettingsVm>().volume, 0.25);
    // Toggle with invert: checked = true writes muted = false; unchecked writes true.
    set_value(&mut app, "mute", json!(false));
    assert!(app.world().resource::<SettingsVm>().muted);
    set_value(&mut app, "mute", json!(true));
    assert!(!app.world().resource::<SettingsVm>().muted);
    set_value(&mut app, "quality", json!(2));
    assert_eq!(app.world().resource::<SettingsVm>().quality, 2);

    // The written value is not echoed back as a change, and a game write still reaches the widget.
    app.world_mut().resource_mut::<SettingsVm>().volume = 0.5;
    app.update();
    let volume = node(&mut app, "volume");
    assert_eq!(app.world().get::<Slider>(volume).unwrap().value, 0.5);
}

/// Game rule for the test below: a volume above 0.9 is refused and set back to 0.25.
fn refuse_loud(mut vm: ResMut<SettingsVm>) {
    if vm.volume > 0.9 {
        vm.volume = 0.25;
    }
}

/// A game that refuses an edit in the frame it lands (the model goes back to its old value)
/// must see the widget go back too. The binding skipped the refresh because the model's raw
/// text equaled the one it remembered from before the edit, so the slider kept showing the
/// refused value.
#[test]
fn a_refused_two_way_edit_puts_the_widget_back() {
    let mut app = app();
    app.add_systems(
        Update,
        refuse_loud.after(regia_ui::viewmodel::apply_two_way_edits),
    );
    app.update();
    set_value(&mut app, "volume", json!(0.25));
    assert_eq!(app.world().resource::<SettingsVm>().volume, 0.25);
    set_value(&mut app, "volume", json!(0.95));
    assert_eq!(app.world().resource::<SettingsVm>().volume, 0.25);
    app.update();
    let volume = node(&mut app, "volume");
    assert_eq!(
        app.world().get::<Slider>(volume).unwrap().value,
        0.25,
        "the slider still shows the refused edit"
    );
}

#[test]
fn submitted_text_writes_back_and_sends_its_command() {
    let mut app = app();
    app.update();
    let player = node(&mut app, "player");
    app.world_mut()
        .write_message(UiEvent::new(player, UiEventKind::Submitted("Ada".to_owned())));
    app.update();
    assert_eq!(app.world().resource::<SettingsVm>().name, "Ada");
    let commands = &app.world().resource::<SeenCommands>().0;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "profile.rename");
    assert_eq!(commands[0].value, UiValue::Text("Ada".to_owned()));
    // The model drives the field too.
    app.world_mut().resource_mut::<SettingsVm>().name = "Grace".to_owned();
    app.update();
    let input = app.world().get::<TextInput>(player).unwrap();
    assert_eq!(input.text, "Grace");
    assert_eq!(input.caret, "Grace".len());
}

#[test]
fn clicks_send_commands() {
    let mut app = app();
    app.world_mut().resource_mut::<SettingsVm>().dirty = true;
    app.update();
    let apply = node(&mut app, "apply");
    app.world_mut()
        .write_message(UiEvent::new(apply, UiEventKind::Click));
    app.update();
    let commands = &app.world().resource::<SeenCommands>().0;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "settings.apply");
    assert_eq!(commands[0].node, apply);
    assert_eq!(commands[0].value, UiValue::None);
}

#[test]
fn two_way_into_a_plain_resource_is_refused_and_reported() {
    let mut app = app();
    app.update();
    set_value(&mut app, "plain", json!(0.9));
    assert_eq!(
        app.world().resource::<Plain>().volume,
        0.0,
        "not a view model: not written"
    );
    let status = ViewModels::status(app.world_mut());
    let plain = status
        .iter()
        .find(|s| s.path == "Plain.volume")
        .expect("status row");
    assert_eq!(plain.mode, BindMode::TwoWay);
    assert!(
        plain
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("binding_not_view_model")),
        "{plain:?}"
    );
}

#[test]
fn integer_fields_round_and_reject_out_of_range() {
    let mut app = app();
    app.update();
    set_value(&mut app, "ammo_slider", json!(41.6));
    assert_eq!(app.world().resource::<HudVm>().ammo, 42);
    set_value(&mut app, "ammo_slider", json!(300.0));
    assert_eq!(
        app.world().resource::<HudVm>().ammo,
        42,
        "300 does not fit u8: unchanged"
    );
    let status = ViewModels::status(app.world_mut());
    let row = status
        .iter()
        .find(|s| s.name.as_deref() == Some("ammo_slider"))
        .expect("status row");
    assert!(
        row.error
            .as_deref()
            .is_some_and(|e| e.starts_with("binding_type_mismatch")),
        "{row:?}"
    );
}

#[test]
fn unresolved_paths_are_reported_not_logged_per_frame() {
    let mut app = app();
    let label = app
        .world_mut()
        .spawn((
            UiNode,
            NodeName("ghost".into()),
            Text::default(),
            regia_ui::ViewBindings(vec![Bind::text("NoSuchVm.value", None)]),
        ))
        .id();
    app.update();
    app.update();
    let status = ViewModels::status(app.world_mut());
    let row = status.iter().find(|s| s.node == label).expect("status row");
    assert!(
        row.error
            .as_deref()
            .is_some_and(|e| e.starts_with("binding_unresolved")),
        "{row:?}"
    );
    assert_eq!(row.last, None);
}

#[test]
fn a_shared_short_name_resolves_to_the_resource_in_the_world() {
    let mut app = app();
    app.register_type::<twin_a::TwinVm>();
    app.register_view_model::<twin_b::TwinVm>();
    let mut bind = Bind::text("TwinVm.value", None);
    bind.target = regia_ui::BindTarget::Value;
    bind.mode = BindMode::TwoWay;
    let slider = app
        .world_mut()
        .spawn((
            UiNode,
            NodeName("twin".into()),
            Slider {
                value: 0.0,
                min: 0.0,
                max: 1.0,
                step: 0.0,
            },
            regia_ui::ViewBindings(vec![bind]),
        ))
        .id();
    app.world_mut().resource_mut::<twin_b::TwinVm>().value = 0.25;
    app.update();
    assert_eq!(app.world().get::<Slider>(slider).unwrap().value, 0.25);
    set_value(&mut app, "twin", json!(0.75));
    assert_eq!(app.world().resource::<twin_b::TwinVm>().value, 0.75);
    let status = ViewModels::status(app.world_mut());
    let row = status.iter().find(|s| s.node == slider).expect("status row");
    assert_eq!(row.error, None, "{row:?}");
}

#[test]
fn agent_lists_models_values_and_binding_status() {
    let mut app = app();
    app.world_mut().resource_mut::<HudVm>().ammo = 9;
    app.update();
    set_value(&mut app, "plain", json!(0.5));
    let out = call(app.world_mut(), "ui.view_models", &json!({})).expect("ui.view_models");
    let hud = out["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "HudVm")
        .expect("HudVm");
    let ammo = hud["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "ammo")
        .unwrap();
    assert_eq!(ammo["kind"], "integer");
    assert_eq!(ammo["value"], "9");
    let plain = out["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["source"] == "Plain.volume")
        .expect("plain binding");
    assert_eq!(plain["code"], "binding_not_view_model");
    assert_eq!(plain["mode"], "two_way");
    assert_eq!(out["errors"], 1, "{out}");
}

/// A view model with the field types the rest of this file does not cover.
#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct NumVm {
    tab: usize,
    gain: f64,
    offset: i16,
}

/// Spec 81 § 2.2 / § 2.3: the `selected` target drives `TabBar.active` both ways, and two-way
/// writes convert to `f64` and to signed integers (rounded to nearest, out of range refused with
/// `binding_type_mismatch`).
#[test]
fn tab_bars_and_f64_and_signed_fields_bind_both_ways() {
    let layout = r#"(version: 3, root: (children: [
        (name: "tabs", widget: tab_bar(["A", "B", "C"], 0),
            bindings: [(source: "NumVm.tab", target: selected, mode: two_way)]),
        (name: "gain", widget: slider(0.0, 0.0, 10.0, 0.0),
            bindings: [(source: "NumVm.gain", target: value, mode: two_way)]),
        (name: "offset", widget: slider(0.0, -100000.0, 100000.0, 0.0),
            bindings: [(source: "NumVm.offset", target: value, mode: two_way)]),
    ]))"#;
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.register_view_model::<NumVm>();
    let file = parse_ui_ron(layout, "num.ui.ron", &UiMigrators::default()).expect("layout");
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("num".to_owned(), file);
    spawn_ui(app.world_mut(), "num").expect("spawn");
    app.update();

    // Model → widget.
    app.world_mut().resource_mut::<NumVm>().tab = 2;
    app.update();
    let tabs = node(&mut app, "tabs");
    assert_eq!(app.world().get::<TabBar>(tabs).expect("tab bar").active, 2);
    // Widget → model.
    set_value(&mut app, "tabs", json!(1));
    assert_eq!(app.world().resource::<NumVm>().tab, 1);

    set_value(&mut app, "gain", json!(2.5));
    assert_eq!(app.world().resource::<NumVm>().gain, 2.5);

    set_value(&mut app, "offset", json!(-41.6));
    assert_eq!(app.world().resource::<NumVm>().offset, -42, "rounds to nearest");
    set_value(&mut app, "offset", json!(40000.0));
    assert_eq!(
        app.world().resource::<NumVm>().offset,
        -42,
        "40000 does not fit i16: unchanged"
    );
    let status = ViewModels::status(app.world_mut());
    let row = status
        .iter()
        .find(|s| s.name.as_deref() == Some("offset"))
        .expect("status row");
    assert!(
        row.error
            .as_deref()
            .is_some_and(|e| e.starts_with("binding_type_mismatch")),
        "{row:?}"
    );
}

/// What the game's clamp system saw and what every frame ended with.
#[derive(Resource, Default)]
struct ClampTrace {
    /// The largest volume the clamp found before clamping (the UI's write, when it ran first).
    seen_before_clamp: f32,
    /// The largest volume any frame ended with.
    max_at_frame_end: f32,
}

/// Game logic in `Update` after the two-way write: caps the volume at 0.5.
fn clamp_volume(mut vm: ResMut<SettingsVm>, mut trace: ResMut<ClampTrace>) {
    trace.seen_before_clamp = trace.seen_before_clamp.max(vm.volume);
    if vm.volume > 0.5 {
        vm.volume = 0.5;
    }
}

fn trace_frame_end(vm: Res<SettingsVm>, mut trace: ResMut<ClampTrace>) {
    trace.max_at_frame_end = trace.max_at_frame_end.max(vm.volume);
}

/// Spec 81 § 2.3: when the game writes a field in the same frame as a two-way write, the game's
/// value wins. The UI's 0.9 reaches the model, the game's clamp (after
/// `apply_two_way_edits`) overwrites it in that frame, no frame ends holding the UI's value, and
/// PostUpdate pushes the game's value back to the slider.
#[test]
fn the_games_same_frame_write_wins_over_a_two_way_write() {
    let mut app = app();
    app.init_resource::<ClampTrace>()
        .add_systems(
            Update,
            clamp_volume.after(regia_ui::viewmodel::apply_two_way_edits),
        )
        .add_systems(Last, trace_frame_end);
    app.update();
    set_value(&mut app, "volume", json!(0.9));
    let trace = app.world().resource::<ClampTrace>();
    assert_eq!(
        trace.seen_before_clamp, 0.9,
        "the two-way write reached the model"
    );
    assert_eq!(
        trace.max_at_frame_end, 0.5,
        "no frame ends with the UI's value: the game's write in the same frame wins"
    );
    assert_eq!(app.world().resource::<SettingsVm>().volume, 0.5);
    let volume = node(&mut app, "volume");
    assert_eq!(
        app.world().get::<Slider>(volume).unwrap().value,
        0.5,
        "PostUpdate pushes the game's value to the widget"
    );
    // The game's value is not written back over itself by the UI on later frames.
    app.update();
    app.update();
    assert_eq!(app.world().resource::<SettingsVm>().volume, 0.5);
    assert_eq!(app.world().get::<Slider>(volume).unwrap().value, 0.5);
}
