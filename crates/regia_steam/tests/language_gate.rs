// SPDX-License-Identifier: MPL-2.0
//! `Steam::language` (the client UI language) is empty unless Steam is `Ready`, and differs from
//! `Steam::game_language` (the per-game language used for Localization).

use regia_app::prelude::*;
use regia_steam::{MockSteam, SteamPlugin, SteamState};

fn steam_with(mock: std::sync::Arc<MockSteam>) -> regia_steam::Steam {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SteamPlugin::default().with_backend(mock)));
    app.finish_plugins();
    app.update();
    app.world().resource::<regia_steam::Steam>().clone()
}

#[test]
fn ui_language_needs_ready_and_is_not_the_game_language() {
    let mock = MockSteam::shared();
    mock.set_language("english");
    mock.set_game_language("japanese");
    let steam = steam_with(mock);
    assert_eq!(steam.state(), SteamState::Ready);
    assert_eq!(steam.language(), "english");
    assert_eq!(steam.game_language(), "japanese");

    let mock = MockSteam::shared_unavailable();
    mock.set_language("english");
    let steam = steam_with(mock);
    assert_ne!(steam.state(), SteamState::Ready);
    assert_eq!(steam.language(), "");
}

#[test]
fn float_stats_check_name_and_state() {
    let steam = steam_with(MockSteam::shared());
    assert!(steam.stat_set_f32("", 1.0).is_err());
    assert!(steam.stat_set_f32("a\0b", 1.0).is_err());
    assert!(steam.stat_f32("").is_err());
    steam.stat_set_f32("distance", 2.5).expect("set");
    assert_eq!(steam.stat_f32("distance").expect("get"), 2.5);

    let steam = steam_with(MockSteam::shared_unavailable());
    assert!(steam.stat_set_f32("distance", 1.0).is_err());
    assert!(steam.stat_f32("distance").is_err());
}
