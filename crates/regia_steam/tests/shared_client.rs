// SPDX-License-Identifier: MPL-2.0
//! The one shared Steam client (ASK 13): `SteamStatus`, idempotent shutdown, rich presence,
//! the overlay event, single initialization and the `steam.status` agent method, all on the
//! mock backend so nothing needs Steam running.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_platform::{AppLifecycle, LifecycleEvent};
use serde_json::json;

use regia_steam::{
    AppId, MockSteam, OverlayOpened, Steam, SteamBackend, SteamEvent, SteamId, SteamPlugin, SteamState,
    SteamStatus,
};

fn app_with(mock: &Arc<MockSteam>) -> App {
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(SteamPlugin::new(AppId(480)).with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>));
    app.finish_plugins();
    app
}

#[test]
fn initializes_exactly_once_and_publishes_status() {
    let mock = MockSteam::shared();
    mock.set_steam_id(Some(SteamId(76561190000000001)));
    let mut app = app_with(&mock);
    app.update();
    app.update();
    assert_eq!(mock.init_calls(), 1, "one init for the whole session");
    let status = app.world().resource::<SteamStatus>();
    assert!(status.running && !status.offline && !status.overlay_open);
    assert_eq!(status.user_id, Some(SteamId(76561190000000001)));
    assert_eq!(status.app_id, Some(AppId(480)));
    // The mock has no native client; hosts on the mock get `None`, never a second init.
    assert!(mock.raw_client().is_none());
}

#[test]
fn offline_mode_and_unavailable_are_distinguished() {
    let mock = MockSteam::shared();
    mock.set_offline(true);
    let mut app = app_with(&mock);
    app.update();
    let status = app.world().resource::<SteamStatus>().clone();
    assert!(status.running && status.offline);

    let absent = MockSteam::shared_unavailable();
    let mut app = app_with(&absent);
    app.update();
    let status = app.world().resource::<SteamStatus>();
    assert!(!status.running && !status.offline && status.user_id.is_none());
}

#[test]
fn overlay_state_reaches_the_status_resource_and_the_event() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock);
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(app.world().resource::<SteamStatus>().overlay_open);
    let mut cursor = MessageCursor::<OverlayOpened>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<OverlayOpened>>())
            .count(),
        1
    );
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    assert!(!app.world().resource::<SteamStatus>().overlay_open);
}

#[test]
fn presence_can_be_set_and_cleared() {
    let mock = MockSteam::shared();
    let app = app_with(&mock);
    let steam = app.world().resource::<Steam>();
    steam.set_presence("status", Some("In a match")).expect("set");
    steam
        .set_presence("connect", Some("+connect 10.0.0.1"))
        .expect("set");
    steam.set_presence("status", None).expect("unset");
    steam.clear_presence();
    assert_eq!(
        mock.presence_calls(),
        vec![
            ("status".to_owned(), Some("In a match".to_owned())),
            ("connect".to_owned(), Some("+connect 10.0.0.1".to_owned())),
            ("status".to_owned(), None),
        ]
    );
}

#[test]
fn shutdown_is_idempotent_and_runs_on_terminate() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock);
    app.world_mut().resource_mut::<Steam>().shutdown();
    app.world_mut().resource_mut::<Steam>().shutdown();
    assert_eq!(app.world().resource::<Steam>().state(), SteamState::Unavailable);
    assert!(app.world().resource::<Steam>().unlock("x").is_err());
    app.update(); // the pump after shutdown must not panic and reports not running
    assert!(!app.world().resource::<SteamStatus>().running);

    // Termination on a live session shuts the backend down once.
    let mock = MockSteam::shared();
    let mut app = app_with(&mock);
    app.world_mut().resource_mut::<AppLifecycle>().last_event = Some(LifecycleEvent::WillTerminate);
    app.run_lifecycle();
    app.run_lifecycle();
    assert_eq!(mock.shutdown_calls(), 1);
    assert_eq!(mock.store_calls(), 1, "stats flushed before shutdown");
}

#[test]
fn agent_method_reports_status() {
    let mock = MockSteam::shared();
    mock.set_steam_id(Some(SteamId(42)));
    let mut app = app_with(&mock);
    app.update();
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (schema, handler) = extension.method("steam.status").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    let result = handler(app.world_mut(), &json!({})).expect("status");
    assert_eq!(result["plugin"], true);
    assert_eq!(result["running"], true);
    assert_eq!(result["user_id"], "42");
    assert_eq!(result["app_id"], 480);
    assert_eq!(result["state"], "Ready");
}

#[test]
fn shutdown_drops_queued_callbacks_and_clears_the_status() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock);
    app.update();
    assert_eq!(app.world().resource::<SteamStatus>().app_id, Some(AppId(480)));
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.world_mut().resource_mut::<Steam>().shutdown();
    app.world_mut().resource_mut::<Steam>().shutdown();
    assert_eq!(mock.shutdown_calls(), 1, "a second shutdown is a no-op");
    app.update();
    assert!(
        !app.world().resource::<regia_steam::OverlayActive>().0,
        "no overlay after shutdown"
    );
    assert!(!app.world().resource::<regia_app::Time>().paused);
    let status = app.world().resource::<SteamStatus>();
    assert!(!status.running);
    assert_eq!(status.app_id, None);
}

#[test]
fn calls_after_shutdown_never_reach_the_backend() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock);
    app.world_mut().resource_mut::<Steam>().shutdown();
    let steam = app.world().resource::<Steam>();
    assert!(steam.stat_add("kills", 1).is_err());
    assert!(steam.stat_set_i32("kills", 1).is_err());
    assert!(steam.store_stats().is_err());
    assert!(steam.set_presence("status", Some("x")).is_err());
    assert_eq!(mock.store_calls(), 1, "only the flush inside shutdown");
}

#[test]
fn shutdown_before_init_is_a_no_op_on_the_mock() {
    let mock = MockSteam::shared();
    mock.shutdown();
    assert_eq!(mock.shutdown_calls(), 0);
    assert_eq!(mock.initialize(None), Ok(SteamState::Ready));
}
