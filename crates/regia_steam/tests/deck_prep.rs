// SPDX-License-Identifier: MPL-2.0
//! Deck readiness end to end against the mock backend and a fake sysfs: the floating keyboard
//! hook, the power policy lowering the frame cap, and the resume reset (ADR-368).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_input::InputContexts;
use regia_platform::{AppLifecycle, LifecycleEvent};
use regia_steam::{
    DeckFrameCap, DeckFrameCapSetting, DeckPowerPolicy, DeckSensorConfig, KeyboardMode, KeyboardRequest,
    KeyboardState, MockSteam, SteamBackend, SteamPlugin, SteamSettings, TextFieldFocus,
};

fn deck_app(mock: &Arc<MockSteam>) -> App {
    mock.set_deck(true);
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings().with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>),
    );
    app.finish_plugins();
    // Never read the host's real battery and temperature sensors.
    app.insert_resource(DeckSensorConfig {
        sys_root: regia_test_utils::unique_temp_dir("deck_prep_empty_sys"),
        interval_frames: 1,
    });
    app
}

fn request() -> KeyboardRequest {
    KeyboardRequest {
        mode: KeyboardMode::SingleLine,
        field: (10, 20, 300, 40),
    }
}

#[test]
fn text_focus_shows_and_dismisses_the_keyboard_on_a_deck() {
    let mock = MockSteam::shared();
    let mut app = deck_app(&mock);
    app.world_mut().write_message(TextFieldFocus {
        request: Some(request()),
    });
    app.update();
    assert_eq!(mock.keyboard_requests(), vec![request()]);
    assert!(app.world().resource::<KeyboardState>().open);
    app.world_mut().write_message(TextFieldFocus { request: None });
    app.update();
    assert_eq!(mock.keyboard_dismissals(), 1);
    assert!(!app.world().resource::<KeyboardState>().open);
    // Losing focus with no keyboard up dismisses nothing.
    app.world_mut().write_message(TextFieldFocus { request: None });
    app.update();
    assert_eq!(mock.keyboard_dismissals(), 1);
}

#[test]
fn off_a_deck_the_keyboard_hook_does_nothing() {
    let mock = MockSteam::shared();
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings().with_backend(Arc::clone(&mock) as Arc<dyn SteamBackend>),
    );
    app.finish_plugins();
    app.world_mut().write_message(TextFieldFocus {
        request: Some(request()),
    });
    app.update();
    assert!(mock.keyboard_requests().is_empty());
    assert!(!app.world().resource::<KeyboardState>().open);
}

#[test]
fn a_hot_sensor_lowers_the_frame_cap_below_the_players_choice() {
    let mock = MockSteam::shared();
    let mut app = deck_app(&mock);
    let root = regia_test_utils::unique_temp_dir("deck_prep_hot");
    let hw = root.join("class/hwmon/hwmon0");
    std::fs::create_dir_all(&hw).expect("create the fake hwmon dir");
    std::fs::write(hw.join("temp1_input"), "95000\n").expect("write the fake APU temperature");
    app.insert_resource(DeckSensorConfig {
        sys_root: root.clone(),
        interval_frames: 1,
    });
    app.world_mut().resource_mut::<DeckFrameCapSetting>().cap = DeckFrameCap::Fps60;
    app.update();
    assert_eq!(
        app.world().resource::<FramePacing>().target_fps,
        Some(60.0),
        "one hot sample is not enough"
    );
    app.update();
    assert_eq!(app.world().resource::<DeckPowerPolicy>().cap, DeckFrameCap::Fps30);
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(30.0));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn resume_dismisses_the_keyboard_and_reapplies_the_cap() {
    let mock = MockSteam::shared();
    let mut app = deck_app(&mock);
    app.world_mut().write_message(TextFieldFocus {
        request: Some(request()),
    });
    app.update();
    assert!(app.world().resource::<KeyboardState>().open);
    // Something else changed the pacing while the machine slept.
    app.world_mut().resource_mut::<FramePacing>().target_fps = None;
    app.world_mut().resource_mut::<AppLifecycle>().last_event = Some(LifecycleEvent::Resumed);
    app.run_lifecycle();
    assert!(!app.world().resource::<KeyboardState>().open);
    assert_eq!(mock.keyboard_dismissals(), 1);
    app.update();
    assert_eq!(
        app.world().resource::<FramePacing>().target_fps,
        Some(40.0),
        "the cap is applied again after the resume"
    );
}
