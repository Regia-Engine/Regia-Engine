// SPDX-License-Identifier: MPL-2.0
//! Steam Deck 3 (ADR-1332) end to end against the mock backend: the `steam.layout` agent method
//! (controls sheets and the controller-only check) and the glyph family following Steam's virtual
//! pad on a Deck through the plugin, and Steam Input's controller type taking over the glyphs.

use std::sync::Arc;

use serde_json::json;

use regia_app::prelude::*;
use regia_input::{ActionKind, Binding, InputConfig, InputContext, InputContexts, InputSource};
use regia_platform::{AppLifecycle, GamepadButton, GamepadConnected};
use regia_steam::{
    ActiveGlyphs, ControllerKind, DeckDisplay, DeckPerfSettings, DeckSensorConfig, GlyphFamily, MockSteam,
    SteamBackend, SteamInputSettings, SteamInputStatus, SteamPlugin, SteamSettings,
};

fn pad(button: GamepadButton) -> InputSource {
    InputSource::gamepad_button(button)
}

fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Crouch", ActionKind::Button)
        .action("Slide", ActionKind::Button)
        .action("Console", ActionKind::Button)
        .action("Accept", ActionKind::Button)
        .context("Gameplay", InputContext::new(0))
        .binding_in("Gameplay", Binding::new("Jump", InputSource::key("Space")))
        .binding_in("Gameplay", Binding::new("Jump", pad(GamepadButton::South)))
        .binding_in("Gameplay", Binding::new("Crouch", pad(GamepadButton::East)))
        .binding_in("Gameplay", Binding::new("Slide", pad(GamepadButton::East)))
        .binding_in("Gameplay", Binding::new("Console", InputSource::key("Backquote")))
        .context("Menu", InputContext::consuming(10))
        .binding_in("Menu", Binding::new("Accept", pad(GamepadButton::South)))
}

/// An app on the mock backend; `deck` makes the mock report a Steam Deck. The host's own sensors
/// and environment are never read.
fn app_with(mock: &Arc<MockSteam>, deck: bool, config: Option<InputConfig>) -> App {
    mock.set_deck(deck);
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default())
        .insert_resource(DeckDisplay::from_env(|_| None))
        .insert_resource(DeckPerfSettings::from_env(|_| None));
    if let Some(config) = config {
        app.insert_resource(config);
    }
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>)
            .with_achievements(Default::default()),
    );
    app.finish_plugins();
    app.insert_resource(DeckSensorConfig {
        sys_root: regia_test_utils::unique_temp_dir("deck_3_empty_sys"),
        interval_frames: 1,
    });
    app
}

#[test]
fn steam_layout_reports_sheets_and_issues() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true, Some(input_config()));
    app.update();
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (schema, layout) = extension.method("steam.layout").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    assert!(!schema.mutating);

    let result = layout(app.world_mut(), &json!({ "family": "playstation" })).expect("steam.layout");
    assert_eq!(result["plugin"], true);
    assert_eq!(result["ok"], true);
    assert_eq!(result["family"], "playstation");
    assert_eq!(result["sheets"][0]["context"], "Menu", "highest priority first");
    assert_eq!(result["sheets"][1]["context"], "Gameplay");
    let gameplay = &result["sheets"][1];
    assert_eq!(gameplay["rows"][0]["control"], "south");
    assert_eq!(gameplay["rows"][0]["label"], "Cross");
    assert_eq!(gameplay["rows"][0]["texture_key"], "glyphs/playstation/cross");
    assert_eq!(gameplay["rows"][1]["actions"], json!(["Crouch", "Slide"]));
    assert_eq!(gameplay["keyboard_only"], json!(["Console"]));
    assert_eq!(result["errors"], 1);
    let codes: Vec<&str> = result["issues"]
        .as_array()
        .map(|issues| issues.iter().filter_map(|i| i["code"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(codes, ["no_gamepad_binding", "control_conflict"]);
    assert_eq!(result["issues"][0]["severity"], "error");
    assert_eq!(result["issues"][0]["action"], "Console");
    assert_eq!(result["issues"][1]["control"], "east");

    // One context, the active family (the Deck's own controls), the console allowed.
    let result = layout(
        app.world_mut(),
        &json!({ "context": "Gameplay", "keyboard_only_ok": ["Console"] }),
    )
    .expect("one context");
    assert_eq!(result["family"], "deck");
    assert_eq!(result["sheets"].as_array().map(Vec::len), Some(1));
    assert_eq!(result["sheets"][0]["rows"][0]["label"], "A");
    assert_eq!(result["errors"], 0);
    assert_eq!(
        result["issues"].as_array().map(Vec::len),
        Some(1),
        "the conflict stays"
    );

    let err = layout(app.world_mut(), &json!({ "context": "Driving" })).expect_err("unknown context");
    assert!(err.message.contains("Driving"));
    let err = layout(app.world_mut(), &json!({ "family": "gamecube" })).expect_err("bad family");
    assert!(err.message.contains("gamecube"));
    assert!(layout(app.world_mut(), &json!({ "keyboard_only_ok": "Console" })).is_err());
    assert!(layout(app.world_mut(), &json!({ "keyboard_only_ok": [1] })).is_err());
}

/// rc1-rev-net-k 9: a `family` or `context` of the wrong JSON type is the documented wrong-type
/// error (`F0002`, as `steam.glyphs` answers), not a default sheet that looks valid. Null is omitted.
#[test]
fn steam_layout_refuses_non_string_params() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true, Some(input_config()));
    app.update();
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (_, layout) = extension.method("steam.layout").expect("registered");
    for (params, param) in [
        (json!({ "family": ["playstation"] }), "family"),
        (json!({ "family": 1 }), "family"),
        (json!({ "context": 1 }), "context"),
        (json!({ "context": { "name": "Gameplay" } }), "context"),
    ] {
        let err = layout(app.world_mut(), &params).expect_err("non-string");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS, "{params}");
        let data = err.data.expect("data");
        assert_eq!(data["code"], "F0002", "{params}");
        assert_eq!(data["param"], param, "{params}");
    }
    let result = layout(app.world_mut(), &json!({ "family": null, "context": null })).expect("nulls");
    assert_eq!(result["family"], "deck");
    assert_eq!(result["sheets"].as_array().map(Vec::len), Some(2));
}

#[test]
fn steam_layout_without_an_input_config_says_so() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, false, None);
    app.update();
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (_, layout) = extension.method("steam.layout").expect("registered");
    let result = layout(app.world_mut(), &json!({})).expect("no config is not an error");
    assert_eq!(result["ok"], false);
    assert_eq!(result["family"], "xbox");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|e| e.contains("InputConfig"))
    );
}

#[test]
fn a_deck_keeps_deck_glyphs_for_the_steam_virtual_pad() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true, Some(input_config()));
    app.update();
    let connect = |app: &mut App, name: &str| {
        app.world_mut().write_message(GamepadConnected {
            device: 0,
            name: name.to_owned(),
        });
        app.update();
        app.world().resource::<ActiveGlyphs>().kind
    };
    assert_eq!(connect(&mut app, "PS5 Controller"), ControllerKind::PlayStation);
    assert_eq!(
        connect(&mut app, "Steam Virtual Gamepad"),
        ControllerKind::SteamDeck
    );
    assert_eq!(
        app.world().resource::<ActiveGlyphs>().family(),
        GlyphFamily::SteamDeck
    );
    assert_eq!(
        connect(&mut app, "Nintendo Switch Pro Controller"),
        ControllerKind::Switch
    );
    assert_eq!(
        connect(&mut app, ""),
        ControllerKind::Switch,
        "an empty name changes nothing"
    );

    // Off a Deck the virtual pad is a generic pad: Xbox glyphs.
    let mock = MockSteam::shared();
    let mut desktop = app_with(&mock, false, Some(input_config()));
    desktop.update();
    assert_eq!(
        connect(&mut desktop, "Steam Virtual Gamepad"),
        ControllerKind::Generic
    );
    assert_eq!(
        desktop.world().resource::<ActiveGlyphs>().family(),
        GlyphFamily::Xbox
    );
}

#[test]
fn steam_input_controller_type_picks_the_glyphs_while_the_bridge_is_active() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true, Some(input_config()));
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(0, "Gameplay");
    app.insert_resource(SteamInputSettings {
        enabled: true,
        player: 0,
    });
    let kind = |app: &App| app.world().resource::<ActiveGlyphs>().kind;

    // Active bridge, no controller yet: the Deck's own glyphs.
    app.update();
    assert!(app.world().resource::<SteamInputStatus>().active);
    assert_eq!(kind(&app), ControllerKind::SteamDeck);

    // A DualSense through Steam Input (ESteamInputType 13): PlayStation glyphs, even though the
    // game sees Steam's Xbox-named virtual pad connect in the same frame.
    mock.set_input_controllers(1);
    mock.set_input_controller_type(Some(13));
    app.world_mut().write_message(GamepadConnected {
        device: 0,
        name: "Microsoft X-Box 360 pad 0".to_owned(),
    });
    app.update();
    assert_eq!(kind(&app), ControllerKind::PlayStation);

    // An unknown type changes nothing; the Deck's built-in controls (14) switch back.
    mock.set_input_controller_type(Some(99));
    app.update();
    assert_eq!(kind(&app), ControllerKind::PlayStation);
    mock.set_input_controller_type(Some(14));
    app.update();
    assert_eq!(kind(&app), ControllerKind::SteamDeck);

    // With the bridge off the name path decides again.
    app.insert_resource(SteamInputSettings {
        enabled: false,
        player: 0,
    });
    mock.set_input_controller_type(Some(10));
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "PS5 Controller".to_owned(),
    });
    app.update();
    assert!(!app.world().resource::<SteamInputStatus>().active);
    assert_eq!(
        kind(&app),
        ControllerKind::PlayStation,
        "from the name, not type 10 (Switch)"
    );
}
