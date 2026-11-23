// SPDX-License-Identifier: MPL-2.0
//! Spec 96 (ADR-1704): every method the Steam plugin registers on `AgentExtension` has a row in
//! the extension table of `book/COVERAGE.md` naming `regia_steam`, and a page that names it; and the
//! one method no other test called (`steam.deck`) answers on the mock backend.

use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_platform::AppLifecycle;
use regia_steam::{AppId, MockSteam, SteamBackend, SteamPlugin};

fn steam_app() -> App {
    let mock = MockSteam::shared();
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(SteamPlugin::new(AppId(480)).with_backend(Arc::clone(&mock) as Arc<dyn SteamBackend>));
    app.finish_plugins();
    app.update();
    app
}

#[test]
fn every_steam_extension_method_has_a_book_page() {
    let app = steam_app();
    let names = app.world().resource::<AgentExtension>().method_names();
    assert!(names.contains(&"steam.status"), "{names:?}");
    let gaps = regia_test_utils::book::extension_gaps(
        &regia_test_utils::book::workspace_root(),
        &names,
        "regia_steam",
        &regia_agent::mentions_token,
    );
    assert!(
        gaps.is_empty(),
        "undocumented Steam methods:\n{}",
        gaps.join("\n")
    );
}

#[test]
fn steam_deck_reports_on_the_mock_backend() {
    let mut app = steam_app();
    let extension = app.world().resource::<AgentExtension>().clone();
    let (schema, handler) = extension.method("steam.deck").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    let out = handler(app.world_mut(), &serde_json::json!({})).expect("steam.deck");
    assert_eq!(out["plugin"], true);
    for key in [
        "detected",
        "cap_fps",
        "power_reason",
        "controller",
        "glyph_family",
    ] {
        assert!(out.get(key).is_some(), "{key}: {out}");
    }
}
