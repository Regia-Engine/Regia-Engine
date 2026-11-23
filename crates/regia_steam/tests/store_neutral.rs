// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/platform.rs, commit d6ed37677), ADR-8012.
// Adapted under the owner's 2026-11-16 exception; synthetic ids and Regia lifecycle.
//! P-21: a standalone store never starts Steam or changes user files.

use regia_app::prelude::*;
use regia_steam::store::{Platform, Store, StorePlugin, select};

#[test]
fn none_store_runs_without_steam() {
    let mut app = App::new();
    app.add_plugins(StorePlugin::new(Store::None));
    app.finish_plugins();
    app.update();
    assert!(!app.world().contains_resource::<regia_steam::Steam>());
    let mut platform = select(Store::None, None);
    assert_eq!(platform.store(), Store::None);
    assert_eq!(platform.account_id(), None);
    platform.set_presence("status", Some("Playing"));
    platform.open_overlay("friends");
    assert!(!platform.unlock_achievement("completed"));
    platform.cloud_save_written("saves/slot1");
    platform.shutdown();
    assert_eq!(platform.describe(), "none (DRM-free)");
}

fn steam_app(mock: &std::sync::Arc<regia_steam::MockSteam>) -> App {
    use regia_steam::achievements::{AchievementDef, AchievementsConfig};
    let mut app = App::new();
    app.insert_resource(regia_platform::AppLifecycle::default());
    app.add_plugins(
        regia_steam::SteamPlugin::new(regia_steam::AppId(480))
            .with_backend(mock.clone())
            .with_achievements(AchievementsConfig {
                achievements: vec![AchievementDef {
                    id: "completed".into(),
                    display: "achievement.completed".into(),
                    stat: None,
                    threshold: None,
                    hidden: false,
                }],
            }),
    );
    app.add_plugins(StorePlugin::new(Store::Steam));
    app.finish_plugins();
    app.update();
    app
}

#[test]
fn steam_adapter_reuses_the_client_and_forwards_calls() {
    let mock = regia_steam::MockSteam::shared();
    mock.set_steam_id(Some(regia_steam::SteamId(42)));
    let mut app = steam_app(&mock);
    assert_eq!(mock.init_calls(), 1);
    {
        let mut steam = app.world_mut().resource_mut::<regia_steam::Steam>();
        let mut p = select(Store::Steam, Some(&mut steam));
        assert_eq!(p.store(), Store::Steam);
        assert_eq!(p.account_id().as_deref(), Some("steam:42"));
        p.set_presence("status", Some("Playing"));
        p.open_overlay("friends");
        assert!(p.unlock_achievement("completed"));
        assert!(!p.unlock_achievement("unknown"));
        p.cloud_save_written("saves/slot1");
        assert_eq!(
            mock.presence_calls(),
            vec![("status".into(), Some("Playing".into()))]
        );
        assert_eq!(mock.overlay_calls(), vec!["friends"]);
        assert_eq!(mock.unlocked_calls(), vec!["completed"]);
        assert!(
            mock.cloud_files().is_empty(),
            "the notification never uploads files"
        );
        p.shutdown();
        assert_eq!(p.account_id(), None);
        assert_eq!(p.store(), Store::None);
        assert!(!p.unlock_achievement("completed"));
        p.shutdown();
    }
    assert!(!app.world().resource::<regia_steam::Steam>().is_ready());
    assert_eq!(mock.init_calls(), 1, "no second native client");
    assert_eq!(mock.shutdown_calls(), 1, "shutdown is idempotent");
}

#[test]
fn unavailable_stores_fall_back_without_starting_any_client() {
    use regia_steam::store::{GogPlatform, NonePlatform};
    for requested in [Store::None, Store::Steam, Store::Gog] {
        let p = select(requested, None);
        assert_eq!(p.store(), Store::None);
        assert_eq!(p.account_id(), None);
    }
    let gog = GogPlatform;
    assert_eq!(gog.store(), Store::Gog);
    assert!(gog.describe().contains("Galaxy SDK not linked"));
    assert_eq!(gog.account_id(), None);
    assert!(!gog.unlock_achievement("completed"));
    assert_eq!(NonePlatform.store(), Store::None);
    let mock = regia_steam::MockSteam::shared_unavailable();
    let mut app = steam_app(&mock);
    let mut steam = app.world_mut().resource_mut::<regia_steam::Steam>();
    assert_eq!(select(Store::Steam, Some(&mut steam)).store(), Store::None);
}

#[test]
fn none_and_gog_do_not_touch_an_existing_steam_client_or_user_files() {
    let dir = regia_test_utils::unique_temp_dir("store-neutral");
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let path = dir.join("settings");
    let old = b"input.deploy T\nvideo.fullscreen 1\n";
    std::fs::write(&path, old).expect("fixture");
    let mock = regia_steam::MockSteam::shared();
    let mut app = steam_app(&mock);
    for requested in [Store::None, Store::Gog] {
        let mut steam = app.world_mut().resource_mut::<regia_steam::Steam>();
        let mut p = select(requested, Some(&mut steam));
        p.set_presence("status", Some("Playing"));
        p.open_overlay("friends");
        assert!(!p.unlock_achievement("completed"));
        p.cloud_save_written(path.to_str().expect("fixture path"));
        p.shutdown();
    }
    assert!(mock.presence_calls().is_empty());
    assert!(mock.overlay_calls().is_empty());
    assert!(mock.unlocked_calls().is_empty());
    assert_eq!(mock.shutdown_calls(), 0);
    assert_eq!(std::fs::read(path).expect("fixture"), old);
    assert_eq!(std::fs::read_dir(&dir).expect("fixture directory").count(), 1);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn store_status_is_read_only_and_has_a_book_page() {
    use regia_agent::extension::AgentExtension;
    let mut app = App::new();
    app.add_plugins(StorePlugin::new(Store::Gog));
    app.finish_plugins();
    let extension = app.world().resource::<AgentExtension>().clone();
    let (schema, _) = extension.method("store.status").expect("registered");
    assert!(!schema.mutating);
    assert_eq!(schema.permission.name(), "read");
    let regia_agent::LocalReply::Done(Ok(out)) =
        regia_agent::call_local(app.world_mut(), "store.status", serde_json::json!({}), None)
    else {
        panic!("store.status must answer through dispatch");
    };
    assert_eq!(out["requested"], "gog");
    assert_eq!(out["active"], "none");
    assert_eq!(out["reason"], "gog_sdk_unavailable");
    assert_eq!(out["account_id"], serde_json::Value::Null);
    let gaps = regia_test_utils::book::extension_gaps(
        &regia_test_utils::book::workspace_root(),
        &extension.method_names(),
        "regia_steam",
        &regia_agent::mentions_token,
    );
    assert!(gaps.is_empty(), "{gaps:?}");
}

#[test]
fn steam_overlay_rejects_unknown_pages_and_nuls_before_backend_calls() {
    let mock = regia_steam::MockSteam::shared();
    let app = steam_app(&mock);
    let steam = app.world().resource::<regia_steam::Steam>();
    for bad in ["", "friends\0", "unknown", "https://example.org"] {
        assert!(steam.open_overlay(bad).is_err(), "{bad:?}");
    }
    assert!(mock.overlay_calls().is_empty());
}

#[test]
fn prefixes_classify_without_confusing_routing_with_authentication() {
    use regia_steam::store::*;
    for (text, kind, store) in [
        ("steam:42", AccountKind::Steam, Store::Steam),
        ("gog:7", AccountKind::Gog, Store::Gog),
        ("local:abc", AccountKind::Local, Store::None),
        ("bot:fixture", AccountKind::Bot, Store::None),
        ("BoT:fixture", AccountKind::Bot, Store::None),
        ("abc001", AccountKind::Hub, Store::None),
        ("", AccountKind::Hub, Store::None),
        ("STEAM:42", AccountKind::Hub, Store::None),
        ("🦀bot:x", AccountKind::Hub, Store::None),
    ] {
        assert_eq!(account_kind(text), kind, "{text}");
        assert_eq!(store_of(text), store, "{text}");
    }
    let all = [PREFIX_STEAM, PREFIX_GOG, PREFIX_LOCAL, PREFIX_BOT];
    for (i, a) in all.iter().enumerate() {
        assert!(a.ends_with(':') && a.is_ascii());
        assert!(!a.bytes().all(|b| b.is_ascii_hexdigit() || b == b':'));
        for b in &all[i + 1..] {
            assert!(!a.starts_with(b) && !b.starts_with(a));
        }
    }
    assert!(relay_allowed("steam:42", "steam:43"));
    assert!(relay_allowed("gog:1", "gog:2"));
    assert!(!relay_allowed("steam:42", "gog:1"));
    for other in ["local:abc", "bot:x", "abc001", ""] {
        assert!(!relay_allowed(other, other));
        assert!(!relay_allowed("steam:42", other));
        assert!(!relay_allowed(other, "gog:1"));
    }
}

#[test]
fn galaxy_and_local_ids_have_canonical_encodings() {
    use regia_steam::store::*;
    for id in [1, 52_000_111_222, u64::MAX] {
        let account = account_id_from_galaxy(id).expect("nonzero");
        assert_eq!(galaxy_from_account_id(&account), Some(id));
    }
    assert_eq!(account_id_from_galaxy(0), None);
    for bad in [
        "",
        "gog:",
        "gog:0",
        "gog:012",
        "gog:+5",
        "gog:abc",
        "GOG:5",
        "steam:5",
        "gog:18446744073709551616",
        "gog:１",
    ] {
        assert_eq!(galaxy_from_account_id(bad), None, "{bad}");
    }
    assert_eq!(
        local_account_id([0xab; 16]),
        "local:abababababababababababababababab"
    );
    for bytes in [[0; 16], [0xab; 16], [0xff; 16]] {
        assert!(is_local_account_id(&local_account_id(bytes)));
    }
    for bad in [
        "local:XYZ",
        "local:ABABABABABABABABABABABABABABABAB",
        "local:00",
        "local:ababababababababababababababababab",
    ] {
        assert!(!is_local_account_id(bad));
    }
    assert_eq!(parse_store("Steam"), Store::Steam);
    assert_eq!(parse_store(" gog "), Store::Gog);
    for text in ["", "none", "unknown", "0"] {
        assert_eq!(parse_store(text), Store::None);
    }
}

#[test]
fn status_tracks_live_shutdown_without_a_frame_or_cached_clone() {
    let mock = regia_steam::MockSteam::shared();
    mock.set_steam_id(Some(regia_steam::SteamId(42)));
    let mut app = steam_app(&mock);
    let status = |app: &mut App| {
        let regia_agent::LocalReply::Done(Ok(out)) =
            regia_agent::call_local(app.world_mut(), "store.status", serde_json::json!({}), None)
        else {
            panic!("store.status must answer");
        };
        out
    };
    let before = status(&mut app);
    assert_eq!(before["active"], "steam");
    assert_eq!(before["account_id"], "steam:42");
    app.world_mut().resource_mut::<regia_steam::Steam>().shutdown();
    let after = status(&mut app);
    assert_eq!(after["requested"], "steam");
    assert_eq!(after["active"], "none");
    assert_eq!(after["account_id"], serde_json::Value::Null);
    assert_eq!(after["reason"], "steam_unavailable");
    assert_eq!(mock.init_calls(), 1);
}

#[test]
fn offline_steam_stays_selected_but_a_disconnected_backend_does_not() {
    let mock = regia_steam::MockSteam::shared();
    mock.set_steam_id(Some(regia_steam::SteamId(42)));
    mock.set_offline(true);
    let mut app = steam_app(&mock);
    {
        let mut steam = app.world_mut().resource_mut::<regia_steam::Steam>();
        assert_eq!(select(Store::Steam, Some(&mut steam)).store(), Store::Steam);
    }
    mock.set_state(regia_steam::SteamState::Unavailable);
    // No frame pump: the adapter must consult live backend state, not a cached copy.
    let mut steam = app.world_mut().resource_mut::<regia_steam::Steam>();
    let p = select(Store::Steam, Some(&mut steam));
    assert_eq!(p.store(), Store::None);
    assert_eq!(p.account_id(), None);
    assert_eq!(mock.init_calls(), 1);
}

#[test]
fn store_status_reports_a_removed_selection_without_touching_steam() {
    let mut app = App::new();
    app.add_plugins(StorePlugin::default());
    app.world_mut()
        .remove_resource::<regia_steam::store::StoreSelection>();
    let regia_agent::LocalReply::Done(Ok(out)) =
        regia_agent::call_local(app.world_mut(), "store.status", serde_json::json!({}), None)
    else {
        panic!("store.status must answer");
    };
    assert_eq!(out, serde_json::json!({"plugin": false}));
    assert!(!app.world().contains_resource::<regia_steam::Steam>());
}

#[test]
fn store_status_does_not_mark_steam_changed() {
    let mock = regia_steam::MockSteam::shared();
    let mut app = steam_app(&mock);
    app.world_mut().clear_trackers();
    assert!(
        !app.world()
            .get_resource_ref::<regia_steam::Steam>()
            .expect("Steam")
            .is_changed()
    );
    let regia_agent::LocalReply::Done(Ok(_)) =
        regia_agent::call_local(app.world_mut(), "store.status", serde_json::json!({}), None)
    else {
        panic!("store.status must answer");
    };
    assert!(
        !app.world()
            .get_resource_ref::<regia_steam::Steam>()
            .expect("Steam")
            .is_changed()
    );
}
