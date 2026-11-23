// SPDX-License-Identifier: MPL-2.0
//! Steam Deck prep 2 (ADR-526) end to end against the mock backend: the Steam Input bridge lets go
//! of everything across a suspend and starts a fresh session on resume, a performance preset
//! drives the frame cap and fits the render scale to a gamescope-scaled output, and the
//! `steam.glyphs` / `steam.deck_perf` / `steam.set_deck_preset` agent methods.

use std::sync::Arc;

use serde_json::json;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::{
    ActionKind, Binding, InputConfig, InputContext, InputContexts, InputInjection, InputSource,
};
use regia_platform::{AppLifecycle, ButtonState, GamepadButton, GamepadButtonChanged, LifecycleEvent};
use regia_steam::deck_perf::GAMESCOPE_ENV;
use regia_steam::{
    DeckDisplay, DeckFrameCap, DeckFrameCapSetting, DeckPanel, DeckPerfPreset, DeckPerfSettings,
    DeckPerfState, DeckSensorConfig, MockSteam, SteamBackend, SteamInputSettings, SteamInputStatus,
    SteamPlugin, SteamSettings,
};

fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .context("Gameplay", InputContext::new(0))
        .binding_in("Gameplay", Binding::new("Jump", InputSource::key("Space")))
        .binding_in(
            "Gameplay",
            Binding::new("Jump", InputSource::gamepad_button(GamepadButton::South)),
        )
        .binding_in(
            "Gameplay",
            Binding::new("Jump", InputSource::gamepad_button(GamepadButton::North)),
        )
}

/// An app on the mock backend with the input resources the bridge reads and, when `deck`, a Deck
/// with an empty fake sysfs (the host's sensors are never read).
fn app_with(mock: &Arc<MockSteam>, deck: bool, perf: Option<DeckPerfSettings>) -> App {
    mock.set_deck(deck);
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(input_config())
        .insert_resource(InputContexts::default())
        .insert_resource(InputInjection::default())
        .insert_resource(SteamInputSettings {
            enabled: true,
            player: 0,
        })
        // Never the developer's own GAMESCOPE_WAYLAND_DISPLAY or REGIA_DECK_PRESET.
        .insert_resource(DeckDisplay::from_env(|_| None))
        .insert_resource(perf.unwrap_or_else(|| DeckPerfSettings::from_env(|_| None)));
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(0, "Gameplay");
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>)
            .with_achievements(Default::default()),
    );
    app.finish_plugins();
    app.insert_resource(DeckSensorConfig {
        sys_root: regia_test_utils::unique_temp_dir("deck_2_empty_sys"),
        interval_frames: 1,
    });
    app
}

fn lifecycle(app: &mut App, event: LifecycleEvent) {
    app.world_mut().resource_mut::<AppLifecycle>().last_event = Some(event);
    app.run_lifecycle();
    app.world_mut().resource_mut::<AppLifecycle>().last_event = None;
}

fn new_messages(app: &App, cursor: &mut MessageCursor<GamepadButtonChanged>) -> Vec<GamepadButtonChanged> {
    cursor
        .read(app.world().resource::<Messages<GamepadButtonChanged>>())
        .copied()
        .collect()
}

#[test]
fn suspend_releases_held_steam_input_and_resume_starts_a_fresh_session() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    let mut app = app_with(&mock, true, None);
    let mut cursor = MessageCursor::default();

    mock.set_input_digital("gameplay_jump", true);
    mock.set_input_digital("gameplay_ui_accept", true);
    app.update();
    assert_eq!(
        app.world().resource::<InputInjection>().value_for("Jump", 0),
        Some(Vec3::X)
    );
    assert_eq!(mock.input_init_calls(), 1);
    assert_eq!(mock.input_activations(), vec!["GAMEPLAY".to_owned()]);
    let pressed = new_messages(&app, &mut cursor);
    assert!(
        pressed
            .iter()
            .any(|m| m.button == GamepadButton::South && m.state == ButtonState::Pressed)
    );

    // The Deck goes to sleep with the buttons still down.
    lifecycle(&mut app, LifecycleEvent::Suspended);
    assert_eq!(
        app.world().resource::<InputInjection>().value_for("Jump", 0),
        None,
        "a held action does not survive the suspend"
    );
    let released = new_messages(&app, &mut cursor);
    assert!(
        released
            .iter()
            .any(|m| m.button == GamepadButton::South && m.state == ButtonState::Released),
        "the UI sees the accept button go up: {released:?}"
    );
    assert_eq!(
        *app.world().resource::<SteamInputStatus>(),
        SteamInputStatus::default()
    );

    // It wakes up with nothing pressed: Steam Input is initialized again and the set re-activated.
    mock.set_input_digital("gameplay_jump", false);
    mock.set_input_digital("gameplay_ui_accept", false);
    lifecycle(&mut app, LifecycleEvent::Resumed);
    app.update();
    assert_eq!(mock.input_init_calls(), 2);
    assert_eq!(
        mock.input_activations(),
        vec!["GAMEPLAY".to_owned(), "GAMEPLAY".to_owned()]
    );
    let status = app.world().resource::<SteamInputStatus>().clone();
    assert!(status.active);
    assert!(status.driven_actions.is_empty() && status.ui_down.is_empty());
    assert!(
        new_messages(&app, &mut cursor).is_empty(),
        "no phantom press after the resume"
    );
}

#[test]
fn a_preset_drives_the_frame_cap_and_fits_the_render_scale_to_gamescope() {
    let mock = MockSteam::shared();
    let perf = DeckPerfSettings {
        preset: DeckPerfPreset::BatterySaver,
        panel: DeckPanel::Lcd,
        drive_frame_cap: true,
    };
    let mut app = app_with(&mock, true, Some(perf));
    app.update();
    assert_eq!(
        app.world().resource::<DeckFrameCapSetting>().cap,
        DeckFrameCap::Fps30
    );
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(30.0));
    let state = *app.world().resource::<DeckPerfState>();
    assert_eq!(state.preset, DeckPerfPreset::BatterySaver);
    assert_eq!(state.knobs.render_scale, 0.6);
    assert!(!state.compositor_upscales);

    // The player picks 960×600 and gamescope's FSR filter: the window shrinks under gamescope.
    app.insert_resource(DeckDisplay::from_env(|k| {
        (k == GAMESCOPE_ENV).then(|| "gamescope-0".to_owned())
    }));
    app.world_mut().write_message(regia_platform::WindowResized {
        window: Entity::PLACEHOLDER,
        width: 960,
        height: 600,
    });
    app.update();
    let state = *app.world().resource::<DeckPerfState>();
    assert!(state.compositor_upscales);
    assert!(
        (state.knobs.render_scale - 0.8).abs() < 1e-6,
        "480 lines, as on the panel"
    );
    assert_eq!(state.knobs.internal_extent((960, 600)), (768, 480));

    // Switching preset writes its cap; the player's own cap choice afterwards is kept.
    app.world_mut().resource_mut::<DeckPerfSettings>().preset = DeckPerfPreset::GamescopeFsr1;
    app.update();
    assert_eq!(
        app.world().resource::<DeckFrameCapSetting>().cap,
        DeckFrameCap::Fps40
    );
    assert_eq!(app.world().resource::<DeckPerfState>().knobs.render_scale, 1.0);
    app.world_mut().resource_mut::<DeckFrameCapSetting>().cap = DeckFrameCap::Fps45;
    app.update();
    assert_eq!(
        app.world().resource::<DeckFrameCapSetting>().cap,
        DeckFrameCap::Fps45
    );
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(45.0));
}

#[test]
fn starting_on_balanced_keeps_a_cap_the_game_already_chose() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true, None);
    app.world_mut().resource_mut::<DeckFrameCapSetting>().cap = DeckFrameCap::Fps60;
    app.update();
    app.update();
    assert_eq!(
        app.world().resource::<DeckFrameCapSetting>().cap,
        DeckFrameCap::Fps60
    );
    assert_eq!(
        app.world().resource::<DeckPerfState>().preset,
        DeckPerfPreset::Balanced
    );
}

#[test]
fn off_a_deck_presets_change_nothing() {
    let mock = MockSteam::shared();
    let perf = DeckPerfSettings {
        preset: DeckPerfPreset::BatterySaver,
        panel: DeckPanel::Lcd,
        drive_frame_cap: true,
    };
    let mut app = app_with(&mock, false, Some(perf));
    app.update();
    assert_eq!(
        app.world().resource::<DeckFrameCapSetting>().cap,
        DeckFrameCap::Fps40
    );
    assert_eq!(
        app.world().resource::<DeckPerfState>().preset,
        DeckPerfPreset::Balanced
    );
}

#[test]
fn agent_methods_report_glyphs_and_perf_and_switch_presets() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true, None);
    app.update();
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();

    let (schema, glyphs) = extension.method("steam.glyphs").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    let result = glyphs(app.world_mut(), &json!({})).expect("steam.glyphs");
    assert_eq!(result["plugin"], true);
    // ADR-1332: before any pad reports in, a Deck shows its own controls' glyphs (the
    // `ActiveGlyphs` contract; deck-2 asserted "xbox" here, which was that contract unmet).
    assert_eq!(result["family"], "deck", "no pad seen yet, on a Deck");
    assert_eq!(result["buttons"].as_array().map(Vec::len), Some(17));
    assert!(result.get("prompts").is_none());

    let result = glyphs(
        app.world_mut(),
        &json!({ "family": "playstation", "action": "Jump" }),
    )
    .expect("steam.glyphs with an action");
    assert_eq!(result["context"], "Gameplay", "the player's top enabled context");
    assert_eq!(result["prompts"][0]["label"], "Cross");
    assert_eq!(result["prompts"][1]["texture_key"], "glyphs/playstation/triangle");
    assert_eq!(
        result["prompts"].as_array().map(Vec::len),
        Some(2),
        "the key binding has no glyph"
    );
    let result =
        glyphs(app.world_mut(), &json!({ "action": "Jump", "context": "Menu" })).expect("unknown context");
    assert_eq!(result["prompts"], json!([]));
    let err = glyphs(app.world_mut(), &json!({ "family": "gamecube" })).expect_err("bad family");
    assert!(err.message.contains("gamecube"));
    for (params, param) in [
        (json!({ "family": ["playstation"] }), "family"),
        (json!({ "family": 1 }), "family"),
        (json!({ "action": 1 }), "action"),
        (json!({ "context": 1 }), "context"),
        (json!({ "action": "Jump", "context": 1 }), "context"),
    ] {
        let err = glyphs(app.world_mut(), &params).expect_err("non-string");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS, "{params}");
        let data = err.data.expect("data");
        assert_eq!(data["code"], "F0002", "{params}");
        assert_eq!(data["param"], param, "{params}");
    }
    // A null family is no family: the family the UI shows now, on a Deck before any pad its own
    // controls (ADR-1332, as `{}` above). The "xbox" this asserted contradicted that (red on
    // integ-v048e, v049a and v049b alike; ADR-5993).
    let result = glyphs(app.world_mut(), &json!({ "family": null })).expect("null family");
    assert_eq!(result["family"], "deck");
    let result = glyphs(app.world_mut(), &json!({ "family": "playstation" })).expect("string family");
    assert_eq!(result["family"], "playstation");

    let (schema, perf) = extension.method("steam.deck_perf").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    let result = perf(app.world_mut(), &json!({})).expect("steam.deck_perf");
    assert_eq!(result["detected"], true);
    assert_eq!(result["preset"], "balanced");
    assert_eq!(
        result["presets"],
        json!(["battery", "balanced", "performance", "fsr1"])
    );
    assert_eq!(result["frame_cap_fps"], 40);
    assert_eq!(result["upscaler"], "tsr");
    assert_eq!(result["gamescope"], false);
    assert_eq!(result["output"], json!(null));

    let (schema, set) = extension.method("steam.set_deck_preset").expect("registered");
    assert!(schema.mutating);
    let result = set(app.world_mut(), &json!({ "preset": "performance" })).expect("set preset");
    assert_eq!(result["previous"], "balanced");
    assert_eq!(result["preset"], "performance");
    app.update();
    let result = perf(app.world_mut(), &json!({})).expect("steam.deck_perf");
    assert_eq!(result["preset"], "performance");
    assert_eq!(result["frame_cap_fps"], 60);
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(60.0));
    assert!(set(app.world_mut(), &json!({ "preset": "turbo" })).is_err());
    assert!(set(app.world_mut(), &json!({})).is_err());
}
