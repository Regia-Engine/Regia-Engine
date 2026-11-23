// SPDX-License-Identifier: MPL-2.0
//! End-to-end test of [`regia_steam::SteamPlugin`] against the mock backend: init and graceful
//! degradation, the frame pump, overlay input suspension and pause, rich-presence joins, the
//! achievement threshold auto-unlock and the stat flush on exit (specs/24 § 2).

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::InputContexts;
use regia_platform::{AppLifecycle, LifecycleEvent};

use regia_steam::{
    AchievementUnlocked, AppId, CloudConflict, JoinRequested, MockSteam, OverlayActive, OverlayClosed,
    OverlayOpened, Steam, SteamBackend, SteamDeck, SteamDeckDetected, SteamError, SteamEvent, SteamId,
    SteamPlugin, SteamSettings, SteamState,
};

/// The achievements config of the test game: `FIRST_BLOOD` unlocks at 1 kill.
const ACHIEVEMENTS: &str = r#"
    (
        achievements: [
            (id: "FIRST_BLOOD", display: "ach.first_blood", stat: "kills", threshold: 1),
        ],
    )
"#;

/// An app with the Steam plugin on the mock backend, a gameplay input context, and the
/// settings the overlay test wants.
fn app_with(backend: &std::sync::Arc<MockSteam>, settings: SteamSettings) -> App {
    let mut app = App::new();
    app.insert_resource(settings)
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(0, "Gameplay");
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(std::sync::Arc::clone(backend) as std::sync::Arc<dyn regia_steam::SteamBackend>)
            .with_achievements(
                regia_steam::achievements::AchievementsConfig::from_ron(ACHIEVEMENTS)
                    .expect("test achievements config"),
            ),
    );
    app.finish_plugins();
    app
}

/// Reads every message of `M` written since the app started.
fn messages<M: Message + Clone>(app: &App) -> Vec<M> {
    let mut cursor = MessageCursor::<M>::default();
    cursor
        .read(app.world().resource::<Messages<M>>())
        .cloned()
        .collect()
}

#[test]
fn ready_backend_initializes_and_the_deck_flag_is_reported() {
    let mock = MockSteam::shared();
    mock.set_deck(true);
    let app = app_with(&mock, SteamSettings::default());

    let steam = app.world().resource::<Steam>();
    assert_eq!(steam.state(), SteamState::Ready, "the mock is connected");
    assert!(steam.is_steam_deck(), "deck detection comes from the backend");
    assert_eq!(
        app.world().resource::<SteamDeck>(),
        &SteamDeck {
            detected: true,
            profile: <SteamDeck as Default>::default().profile
        },
        "the Deck resource mirrors the detection"
    );
    assert!(
        !messages::<SteamDeckDetected>(&app).is_empty(),
        "the message was raised"
    );
}

#[test]
fn steam_absent_degrades_to_unavailable_and_the_game_continues() {
    let mock = MockSteam::shared_unavailable();
    let mut app = app_with(&mock, SteamSettings::default());

    let steam = app.world().resource::<Steam>();
    assert_eq!(steam.state(), SteamState::Unavailable);
    assert!(!steam.is_ready());
    // Every call degrades; nothing panics, the game keeps running.
    assert_eq!(steam.unlock("FIRST_BLOOD"), Err(SteamError::Unavailable));
    assert_eq!(steam.stat_add("kills", 1), Err(SteamError::Unavailable));
    assert_eq!(
        steam.set_presence("status", Some("In the menus")),
        Err(SteamError::Unavailable)
    );
    // Unknown ids are caught by the config before they reach Steam.
    assert_eq!(
        steam.unlock("NOPE"),
        Err(SteamError::UnknownAchievement("NOPE".into()))
    );
    // An empty frame is a no-op.
    app.update();
    assert_eq!(app.world().resource::<Steam>().state(), SteamState::Unavailable);
}

#[test]
fn overlay_suspends_gameplay_input_and_pauses_and_close_restores() {
    let mock = MockSteam::shared();
    let settings = SteamSettings {
        pause_on_overlay: true,
        suspend_contexts: Some(vec!["Gameplay".to_owned()]),
        ..SteamSettings::default()
    };
    let mut app = app_with(&mock, settings);

    // Open: contexts suspend, time pauses, the message arrives.
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(app.world().resource::<OverlayActive>().0);
    assert!(
        !app.world().resource::<InputContexts>().is_enabled(0, "Gameplay"),
        "gameplay input is suspended while the overlay is open"
    );
    assert!(
        app.world().resource::<Time>().paused,
        "pause_on_overlay paused the game clock"
    );
    assert_eq!(messages::<OverlayOpened>(&app).len(), 1);

    // Steam repeats the callback; only edges matter.
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert_eq!(
        messages::<OverlayOpened>(&app).len(),
        1,
        "no duplicate open message"
    );

    // A game-initiated pause is never unpaused by the overlay: not modelled here, but the
    // restore only unpauses what the plugin paused (paused_by_overlay).
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    assert!(!app.world().resource::<OverlayActive>().0);
    assert!(
        app.world().resource::<InputContexts>().is_enabled(0, "Gameplay"),
        "suspended contexts are restored exactly"
    );
    assert!(!app.world().resource::<Time>().paused);
    assert_eq!(messages::<OverlayClosed>(&app).len(), 1);
}

#[test]
fn overlay_without_pause_only_suspends_input() {
    let mock = MockSteam::shared();
    // Defaults: suspend Gameplay, but no pause.
    let mut app = app_with(&mock, SteamSettings::default());
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(!app.world().resource::<InputContexts>().is_enabled(0, "Gameplay"));
    assert!(!app.world().resource::<Time>().paused, "pausing is opt-in");
}

#[test]
fn join_requested_carries_friend_and_connect_string() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());

    mock.inject(SteamEvent::JoinRequested {
        friend: SteamId(76561198000000000),
        connect: "+connect 10.0.0.1:27015".to_owned(),
    });
    app.update();

    let joins = messages::<JoinRequested>(&app);
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].friend, SteamId(76561198000000000));
    assert_eq!(joins[0].connect, "+connect 10.0.0.1:27015");
}

#[test]
fn a_join_with_an_oversized_connect_string_is_dropped() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    mock.inject(SteamEvent::JoinRequested {
        friend: SteamId(76561198000000000),
        connect: "x".repeat(regia_steam::MAX_CONNECT_BYTES + 1),
    });
    app.update();
    assert!(messages::<JoinRequested>(&app).is_empty());
}

#[test]
fn the_mock_refuses_the_cloud_names_the_real_backend_refuses() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    app.update();
    for name in ["", "nul\0"] {
        assert!(mock.cloud_write(name, b"x").is_err(), "{name:?}");
        assert!(mock.cloud_read(name).is_err(), "{name:?}");
        assert!(mock.cloud_delete(name).is_err(), "{name:?}");
        assert!(mock.cloud_exists(name).is_err(), "{name:?}");
    }
    assert!(mock.cloud_write("saves/slot.dat", b"x").is_ok());
}

#[test]
fn cloud_conflict_is_a_message_the_game_can_read() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());

    let mut cursor = MessageCursor::<CloudConflict>::default();
    mock.sync_remote_file("saves/Chapter1.frts", b"remote-bytes", true);
    app.update();

    let first: Vec<CloudConflict> = cursor
        .read(app.world().resource::<Messages<CloudConflict>>())
        .cloned()
        .collect();
    assert_eq!(
        first,
        vec![CloudConflict {
            slot: "Chapter1".into(),
            file: "saves/Chapter1.frts".into(),
            remote_newer: true,
        }]
    );
    // The remote bytes are actually stored, not only announced.
    let stored = mock.cloud_read("saves/Chapter1.frts").expect("cloud read");
    assert_eq!(stored.as_deref(), Some(&b"remote-bytes"[..]));

    // No new event: the next frame does not repeat the conflict.
    app.update();
    let second: Vec<CloudConflict> = cursor
        .read(app.world().resource::<Messages<CloudConflict>>())
        .cloned()
        .collect();
    assert!(
        second.is_empty(),
        "the conflict is one event, not a poll: {second:?}"
    );
}

#[test]
fn crossing_a_stat_threshold_unlocks_once_and_reports_it() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());

    // The first kill crosses FIRST_BLOOD's threshold: unlock + AchievementUnlocked.
    let kills = app
        .world()
        .resource::<Steam>()
        .stat_add("kills", 1)
        .expect("stat_add");
    assert_eq!(kills, 1);
    assert_eq!(
        mock.unlocked_calls(),
        ["FIRST_BLOOD"],
        "the threshold auto-unlock fired"
    );
    app.update();
    assert_eq!(
        messages::<AchievementUnlocked>(&app),
        [AchievementUnlocked {
            id: "FIRST_BLOOD".to_owned()
        }],
        "the unlock is surfaced to the game"
    );

    // More kills do not re-unlock (the mock records every unlock call).
    app.world()
        .resource::<Steam>()
        .stat_add("kills", 5)
        .expect("stat_add");
    assert_eq!(
        mock.unlocked_calls(),
        ["FIRST_BLOOD"],
        "already unlocked: no repeat"
    );

    // store_stats flushes; the result arrives as a message on the next pump.
    app.world()
        .resource::<Steam>()
        .store_stats()
        .expect("store_stats");
    app.update();
    assert_eq!(mock.store_calls(), 1);
    assert!(!messages::<regia_steam::StatsStored>(&app).is_empty());
}

#[test]
fn unlocked_threshold_achievements_are_not_requeried_on_later_stat_ticks() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    let steam = app.world().resource::<Steam>();
    steam.stat_add("kills", 1).expect("stat_add");
    let after_unlock = mock.achievement_query_count();
    assert!(after_unlock >= 1, "the first crossing queried Steam");
    steam.stat_add("kills", 1).expect("stat_add");
    steam.stat_add("kills", 1).expect("stat_add");
    assert_eq!(
        mock.achievement_query_count(),
        after_unlock,
        "an already-unlocked threshold is not re-queried"
    );
    assert_eq!(mock.unlocked_calls(), ["FIRST_BLOOD"]);

    // Clearing the achievement forgets it, so a later crossing unlocks again.
    steam.clear_achievement("FIRST_BLOOD").expect("clear");
    steam.stat_add("kills", 1).expect("stat_add");
    assert_eq!(mock.unlocked_calls(), ["FIRST_BLOOD", "FIRST_BLOOD"]);
}

#[test]
fn app_exit_flushes_pending_stats() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    app.world_mut()
        .resource::<Steam>()
        .stat_add("kills", 3)
        .expect("stat_add");
    assert_eq!(mock.store_calls(), 0, "nothing stored yet");

    // The platform layer signals exit; the Lifecycle schedule runs outside the frame.
    app.world_mut().resource_mut::<AppLifecycle>().last_event = Some(LifecycleEvent::WillTerminate);
    app.run_lifecycle();
    assert_eq!(
        mock.store_calls(),
        1,
        "stats are flushed before the app terminates"
    );
}

#[test]
fn explicit_app_id_beats_project_settings() {
    let plugin = SteamPlugin::new(AppId(480));
    let settings = SteamSettings {
        app_id: Some(AppId(1000)),
        ..SteamSettings::default()
    };
    assert_eq!(
        plugin.resolve_app_id(&settings, std::path::Path::new("/nonexistent")),
        Some(AppId(480))
    );
}

#[test]
fn overlay_close_keeps_a_game_initiated_pause() {
    let mock = MockSteam::shared();
    let mut app = app_with(
        &mock,
        SteamSettings {
            pause_on_overlay: true,
            ..SteamSettings::default()
        },
    );
    // The pause menu paused the game before the overlay opened.
    app.world_mut().resource_mut::<Time>().paused = true;
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    assert!(
        app.world().resource::<Time>().paused,
        "closing the overlay must not unpause the game's own pause"
    );
}

#[test]
fn overlay_close_unpauses_only_the_overlay_pause() {
    let mock = MockSteam::shared();
    let mut app = app_with(
        &mock,
        SteamSettings {
            pause_on_overlay: true,
            ..SteamSettings::default()
        },
    );
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(app.world().resource::<Time>().paused);
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    assert!(!app.world().resource::<Time>().paused);
}

#[test]
fn steams_own_achievement_callback_does_not_repeat_a_threshold_unlock() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    app.world()
        .resource::<Steam>()
        .stat_add("kills", 1)
        .expect("stat_add");
    app.update();
    // store_stats makes the real client fire UserAchievementStored for the same unlock.
    mock.inject(SteamEvent::AchievementStored {
        id: "FIRST_BLOOD".to_owned(),
    });
    app.update();
    assert_eq!(
        messages::<AchievementUnlocked>(&app).len(),
        1,
        "one unlock, one message"
    );
}

#[test]
fn float_stat_past_threshold_unlocks() {
    const FLOAT_ACHIEVEMENTS: &str = r#"
        (
            achievements: [
                (id: "MARATHON", display: "ach.marathon", stat: "distance", threshold: 100),
            ],
        )
    "#;
    let mock = MockSteam::shared();
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(std::sync::Arc::clone(&mock) as std::sync::Arc<dyn SteamBackend>)
            .with_achievements(
                regia_steam::achievements::AchievementsConfig::from_ron(FLOAT_ACHIEVEMENTS)
                    .expect("test achievements config"),
            ),
    );
    app.finish_plugins();

    // A float stat the game writes must cross thresholds exactly like an int stat: the write
    // succeeding while the achievement stays locked is the silent miss this guards against.
    app.world()
        .resource::<Steam>()
        .stat_set_f32("distance", 500.0)
        .expect("stat_set_f32");
    assert_eq!(
        mock.unlocked_calls(),
        ["MARATHON"],
        "a float stat past the threshold unlocks"
    );

    // A non-finite value is stored but unlocks nothing.
    app.world()
        .resource::<Steam>()
        .stat_set_f32("distance", f32::NAN)
        .expect("stat_set_f32 stores NaN");
    assert_eq!(mock.unlocked_calls(), ["MARATHON"], "NaN unlocks nothing");
}

#[test]
fn stat_add_applies_the_stat_even_when_the_threshold_unlock_is_refused() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    // Steam refuses the unlock (id missing on the partner site); the stat is already written.
    mock.reject_unlocks(true);
    let steam = app.world().resource::<Steam>();
    assert_eq!(
        steam.stat_add("kills", 1),
        Ok(1),
        "no Err after the stat was applied"
    );
    assert_eq!(mock.stat_i32("kills"), Some(1));
    assert_eq!(
        steam.stat_add("kills", 1),
        Ok(2),
        "a retry counts once more, not twice"
    );
}

#[test]
fn names_with_nul_or_empty_are_errors_not_aborts() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    let steam = app.world().resource::<Steam>();
    assert!(matches!(
        steam.stat_add("kills\0", 1),
        Err(SteamError::Rejected(_))
    ));
    assert!(matches!(steam.stat_i32(""), Err(SteamError::Rejected(_))));
    assert!(matches!(
        steam.unlock("FIRST\0BLOOD"),
        Err(SteamError::Rejected(_))
    ));
    assert!(mock.unlocked_calls().is_empty());
}

#[test]
fn shutdown_waits_for_the_stats_upload_acknowledgement() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    app.world()
        .resource::<Steam>()
        .stat_add("kills", 2)
        .expect("stat_add");
    app.world_mut().resource_mut::<Steam>().shutdown();
    assert_eq!(mock.store_calls(), 1);
    assert_eq!(
        mock.stats_waits(),
        1,
        "the upload is awaited before the client is dropped"
    );
    assert_eq!(mock.shutdown_calls(), 1);
}

#[test]
fn shutdown_with_the_overlay_open_restores_input_and_time() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(app.world().resource::<OverlayActive>().0);
    assert!(!app.world().resource::<InputContexts>().is_enabled(0, "Gameplay"));

    app.world_mut().resource_mut::<Steam>().shutdown();
    app.update();
    assert!(!app.world().resource::<OverlayActive>().0, "overlay cleared");
    assert!(
        app.world().resource::<InputContexts>().is_enabled(0, "Gameplay"),
        "Gameplay input restored"
    );
    assert!(!app.world().resource::<Time>().paused);
    assert_eq!(messages::<OverlayClosed>(&app).len(), 1);
}

#[test]
fn a_threshold_cross_reports_one_unlock_and_a_manual_unlock_reports_too() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    let steam = app.world().resource::<Steam>().clone();
    steam.stat_add("kills", 1).expect("stat_add");
    steam.stat_add("kills", 1).expect("stat_add");
    app.update();
    assert_eq!(
        messages::<AchievementUnlocked>(&app).len(),
        1,
        "one threshold cross is one message"
    );

    // A manual unlock of a fresh achievement is reported by the mock the way the real
    // store callback reports it.
    steam.clear_achievement("FIRST_BLOOD").expect("clear");
    steam.unlock("FIRST_BLOOD").expect("unlock");
    app.update();
    assert_eq!(messages::<AchievementUnlocked>(&app).len(), 2);
}

#[test]
fn stat_set_crosses_thresholds_and_stat_add_saturates() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    let steam = app.world().resource::<Steam>();
    steam.stat_set_i32("kills", 3).expect("set");
    assert_eq!(mock.unlocked_calls(), ["FIRST_BLOOD"]);
    steam.stat_set_i32("kills", i32::MAX - 1).expect("set");
    assert_eq!(
        steam.stat_add("kills", 10).expect("add"),
        i32::MAX,
        "no wrap to negative"
    );
}

#[test]
fn names_with_nul_are_rejected_not_forwarded() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    let steam = app.world().resource::<Steam>();
    assert!(matches!(
        steam.stat_add("k\0ills", 1),
        Err(SteamError::Rejected(_))
    ));
    assert!(matches!(steam.stat_i32("k\0"), Err(SteamError::Rejected(_))));
    let cloud = regia_steam::SteamRemoteStorage::new(steam);
    assert!(matches!(
        regia_steam::SaveBackend::save(&cloud, "a\0b", b"x"),
        Err(regia_steam::SaveError::InvalidSlot(_))
    ));
}

#[test]
fn mock_stat_add_stores_the_running_sum() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    let steam = app.world().resource::<Steam>();
    assert_eq!(steam.stat_add("score", 2).expect("add"), 2);
    assert_eq!(steam.stat_add("score", 3).expect("add"), 5);
    assert_eq!(mock.stat_i32("score"), Some(5), "the map holds the sum, not 0");
    assert_eq!(steam.stat_i32("score").expect("read"), 5);
}

#[test]
fn overlay_close_keeps_a_pause_the_game_already_held() {
    let mock = MockSteam::shared();
    let settings = SteamSettings {
        pause_on_overlay: true,
        ..SteamSettings::default()
    };
    let mut app = app_with(&mock, settings);
    app.world_mut().resource_mut::<Time>().paused = true;
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    assert!(
        app.world().resource::<Time>().paused,
        "the overlay did not take the pause, so closing must not release it"
    );
}

#[test]
fn a_failed_achievement_query_does_not_unlock_blind() {
    let mock = MockSteam::shared();
    let app = app_with(&mock, SteamSettings::default());
    mock.set_achievement_query_fails(true);
    let kills = app
        .world()
        .resource::<Steam>()
        .stat_add("kills", 1)
        .expect("stat_add still succeeds");
    assert_eq!(kills, 1);
    assert!(
        mock.unlocked_calls().is_empty(),
        "an Err query is unknown, not locked"
    );
}

#[test]
fn overlay_hint_is_off_after_shutdown() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    assert!(app.world().resource::<Steam>().overlay_enabled());
    app.world_mut().resource_mut::<Steam>().shutdown();
    // Even if the backend still answers (a stale handle), the handle itself reports off.
    mock.set_state(SteamState::Ready);
    assert!(
        !app.world().resource::<Steam>().overlay_enabled(),
        "no Shift-Tab hint without Steam"
    );
}

/// An app on `mock` with an explicit app id, so the plugin's `finish` runs the shipping restart
/// check (the path the `steamworks` feature used to send to the real SDK even with a mock).
fn app_with_app_id(mock: &std::sync::Arc<MockSteam>, id: u32) -> App {
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(
        SteamPlugin::new(AppId(id))
            .with_backend(std::sync::Arc::clone(mock) as std::sync::Arc<dyn SteamBackend>),
    );
    app.finish_plugins();
    app
}

#[test]
fn an_injected_backend_answers_the_restart_check_and_is_initialized_once() {
    // With or without the `steamworks` feature, an injected backend decides the restart: the
    // mock says "no restart", so `finish` goes on to initialize it, exactly once.
    let mock = MockSteam::shared();
    let mut app = app_with_app_id(&mock, 480);
    app.update();
    assert_eq!(
        mock.restart_checks(),
        vec![AppId(480)],
        "asked once, with the resolved id"
    );
    assert_eq!(mock.init_calls(), 1);
    assert_eq!(app.world().resource::<Steam>().state(), SteamState::Ready);
    assert!(messages::<AppExit>(&app).is_empty(), "no restart, no exit");
}

#[test]
fn a_backend_that_asks_for_a_restart_exits_before_initializing() {
    let mock = MockSteam::shared();
    mock.set_restart_required(true);
    let app = app_with_app_id(&mock, 480);
    assert_eq!(mock.restart_checks(), vec![AppId(480)]);
    assert_eq!(
        mock.init_calls(),
        0,
        "Valve's step: relaunch through Steam before SteamAPI init"
    );
    assert_eq!(app.world().resource::<Steam>().state(), SteamState::Restarting);
    assert_eq!(messages::<AppExit>(&app), vec![AppExit::Success]);
}

#[test]
fn the_restart_check_runs_exactly_when_an_app_id_resolves() {
    let mock = MockSteam::shared();
    mock.set_restart_required(true);
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(std::sync::Arc::clone(&mock) as std::sync::Arc<dyn SteamBackend>),
    );
    app.finish_plugins();
    // `from_project_settings` in the test's cwd may still find STEAM_APP_ID or a manifest;
    // only assert the coupling: a check happened exactly when an app id was resolved.
    let resolved = app.world().resource::<Steam>().state() == SteamState::Restarting;
    assert_eq!(mock.restart_checks().is_empty(), !resolved);
    assert_eq!(mock.init_calls(), u32::from(!resolved));
}

/// ADR-7349 (N05): the owner's overlay stayed only while Shift+Tab was held. While the overlay is
/// open the plugin holds `CursorRelease`, so a game that re-grabs the pointer every frame or on a
/// focus change cannot take it back; the hold ends with `OverlayClosed` (and with a shutdown).
/// w9-net-vehicle-steam-z 3: a second window whose cursor is hidden but not grabbed (a gamepad
/// game's second view) is held the same way — the hold is one global resource — and no window's
/// own `Window` values are rewritten by it.
#[test]
fn the_overlay_holds_the_cursor_through_regrabs_and_focus_churn_until_it_closes() {
    use regia_platform::{CursorGrab, CursorRelease, PrimaryWindow, Window, WindowFocused};
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    let window = app
        .world_mut()
        .spawn((
            Window {
                cursor_grab: CursorGrab::Locked,
                cursor_visible: false,
                ..Window::default()
            },
            PrimaryWindow,
        ))
        .id();
    // Hidden but ungrabbed: the hold must not treat it differently, and must not rewrite it.
    let hidden = app
        .world_mut()
        .spawn(Window {
            cursor_grab: CursorGrab::None,
            cursor_visible: false,
            ..Window::default()
        })
        .id();
    app.update();
    assert!(
        !app.world().resource::<CursorRelease>().0,
        "no hold before the overlay"
    );

    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(
        app.world().resource::<CursorRelease>().0,
        "the overlay holds the cursor"
    );
    assert_eq!(messages::<OverlayOpened>(&app).len(), 1);
    for frame in 0..6 {
        // The game re-writes its grab every frame and the window manager flaps focus.
        app.world_mut()
            .get_mut::<Window>(window)
            .expect("window")
            .cursor_grab = CursorGrab::Locked;
        app.world_mut().write_message(WindowFocused {
            window,
            focused: frame % 2 == 0,
        });
        // Steam repeats the callback while the overlay is up.
        mock.inject(SteamEvent::OverlayActivated { active: true });
        app.update();
        assert!(
            app.world().resource::<CursorRelease>().0,
            "frame {frame}: still held"
        );
        assert!(
            app.world().resource::<OverlayActive>().0,
            "frame {frame}: still open"
        );
        if frame >= 1 {
            // Messages live two frames: by now only repeats arrived, and they are not new opens.
            assert!(
                messages::<OverlayOpened>(&app).is_empty(),
                "frame {frame}: repeats are not new opens"
            );
        }
    }
    assert_eq!(
        app.world().get::<Window>(window).expect("window").cursor_grab,
        CursorGrab::Locked,
        "the game's own wish is kept on its component"
    );

    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    assert!(!app.world().resource::<CursorRelease>().0, "the close lets go");
    assert_eq!(messages::<OverlayClosed>(&app).len(), 1);

    // The hidden-but-ungrabbed window went through the same hold untouched.
    let (grab, visible) = {
        let component = app.world().get::<Window>(hidden).expect("hidden window");
        (component.cursor_grab, component.cursor_visible)
    };
    assert_eq!(
        (grab, visible),
        (CursorGrab::None, false),
        "the hold never rewrites a window's own values"
    );

    // A shutdown with the overlay open lets go too.
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(app.world().resource::<CursorRelease>().0);
    app.world_mut().resource_mut::<Steam>().shutdown();
    app.update();
    assert!(!app.world().resource::<CursorRelease>().0, "shutdown lets go");
}

#[test]
fn a_game_can_keep_its_cursor_while_the_overlay_is_open() {
    use regia_platform::CursorRelease;
    let mock = MockSteam::shared();
    let mut app = app_with(
        &mock,
        SteamSettings {
            release_cursor_on_overlay: false,
            ..SteamSettings::default()
        },
    );
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(app.world().resource::<OverlayActive>().0);
    assert!(!app.world().resource::<CursorRelease>().0, "opted out: no hold");
}

/// ADR-7349: `REGIA_STEAM_OVERLAY_TRACE` writes the callbacks (repeats included), the game's
/// messages, focus, Shift/Tab keys, the cursor wish and the hold, in frame order.
#[test]
fn the_overlay_trace_records_callbacks_keys_focus_and_the_cursor_hold() {
    use regia_platform::{
        ButtonState, CursorGrab, InputTimestamp, KeyboardInput, PrimaryWindow, Window, WindowFocused,
    };
    use regia_steam::SteamOverlayTrace;
    let dir = regia_test_utils::unique_temp_dir("steam-overlay-trace");
    let path = dir.join("overlay-trace.txt");
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, SteamSettings::default());
    app.insert_resource(SteamOverlayTrace::create(&path).expect("trace file"));
    let window = app
        .world_mut()
        .spawn((
            Window {
                cursor_grab: CursorGrab::Locked,
                ..Window::default()
            },
            PrimaryWindow,
        ))
        .id();
    app.update();
    let key = |physical: &str, logical: &str, state| KeyboardInput {
        window,
        physical_key: physical.to_owned(),
        logical_key: logical.to_owned(),
        state,
        repeat: false,
        timestamp: InputTimestamp::now(),
    };
    app.world_mut()
        .write_message(key("ShiftLeft", "Shift", ButtonState::Pressed));
    app.world_mut()
        .write_message(key("Tab", "Tab", ButtonState::Pressed));
    app.world_mut()
        .write_message(key("KeyW", "w", ButtonState::Pressed));
    mock.inject(SteamEvent::OverlayActivated { active: true });
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    app.world_mut().write_message(WindowFocused {
        window,
        focused: false,
    });
    app.world_mut()
        .write_message(key("Tab", "Tab", ButtonState::Released));
    app.update();
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    drop(app);

    let text = std::fs::read_to_string(&path).expect("trace text");
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("regia steam overlay trace 1; pid "),
        "{text}"
    );
    let expected = [
        "game asks cursor_grab=Locked cursor_visible=true",
        "cursor hold for the overlay: off",
        "steam callback GameOverlayActivated active=true (edge)",
        "steam callback GameOverlayActivated active=true (repeat)",
        "key ShiftLeft (Shift) down",
        "key Tab (Tab) down",
        "game OverlayOpened",
        "cursor hold for the overlay: on",
        "window focus out",
        "key Tab (Tab) up",
        "steam callback GameOverlayActivated active=false (edge)",
        "game OverlayClosed",
        "cursor hold for the overlay: off",
    ];
    let mut at = 0;
    for want in expected {
        let found = lines[at..]
            .iter()
            .position(|line| line.contains(want))
            .unwrap_or_else(|| panic!("`{want}` missing after line {at}:\n{text}"));
        at += found + 1;
    }
    assert!(!text.contains("KeyW"), "only Shift and Tab are traced:\n{text}");
    for line in &lines[1..] {
        assert!(line.starts_with("t=+") && line.contains(" frame="), "{line}");
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
