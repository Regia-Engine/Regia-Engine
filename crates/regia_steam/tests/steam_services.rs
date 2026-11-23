// SPDX-License-Identifier: MPL-2.0
//! Steam leaderboards, app and DLC ownership and timeline markers against the mock backend
//! (spec 24 § 2a, ADR-2340): the game API (`Leaderboard`, `Steam::owns_app`,
//! `Steam::timeline_event`) and the agent methods (`steam.leaderboard_*`, `steam.owns_app`,
//! `steam.timeline_*`), with their error codes.

use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_framework::leaderboard::{
    Leaderboard, LeaderboardDisplay, LeaderboardError, LeaderboardMessage, LeaderboardPlugin,
    LeaderboardRange, LeaderboardSort, LeaderboardSpec, Lookup, UgcHandle,
};
use regia_platform::AppLifecycle;
use regia_steam::{
    AppId, AppOwnership, MockSteam, Steam, SteamBackend, SteamError, SteamId, SteamLeaderboardPlugin,
    SteamPlugin, SteamState, TimelineCall, TimelineClip, TimelineEvent, TimelineMode,
};
use serde_json::{Value, json};

const ME: u64 = 76_561_197_960_287_930;

fn app_with(mock: &Arc<MockSteam>, leaderboards: bool) -> App {
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins(SteamPlugin::new(AppId(480)).with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>));
    if leaderboards {
        app.add_plugins(SteamLeaderboardPlugin);
    }
    app.finish_plugins();
    app.update();
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    handler(app.world_mut(), &params)
}

fn reason(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn messages(app: &App) -> Vec<LeaderboardMessage> {
    let messages = app.world().resource::<Messages<LeaderboardMessage>>();
    messages.get_cursor().read(messages).cloned().collect()
}

#[test]
fn game_uses_steam_leaderboards_through_the_framework_resource() {
    let mock = MockSteam::shared();
    mock.set_steam_id(Some(SteamId(ME)));
    let store = mock.leaderboard_store();
    store.create_board(
        "fastest_lap",
        LeaderboardSort::Ascending,
        LeaderboardDisplay::TimeMilliseconds,
    );
    for (user, time) in [
        (11_u64, 61_000),
        (12, 62_000),
        (13, 64_000),
        (14, 65_000),
        (15, 66_000),
    ] {
        store.seed_score("fastest_lap", user, time, &[]);
    }
    store.set_friends([12, 15]);
    let mut app = app_with(&mock, true);
    assert_eq!(app.world().resource::<Leaderboard>().service_name(), "steam");
    assert_eq!(app.world().resource::<Leaderboard>().local_user(), Some(ME));

    {
        let mut boards = app.world_mut().resource_mut::<Leaderboard>();
        boards
            .declare(LeaderboardSpec::new("fastest_lap").ascending())
            .expect("declare");
        boards.submit("fastest_lap", 63_500, &[3, 1]).expect("submit");
        boards
            .request(
                "fastest_lap",
                LeaderboardRange::AroundUser { before: 1, after: 1 },
            )
            .expect("around");
    }
    app.update();
    app.update();
    let seen = messages(&app);
    assert!(
        seen.iter()
            .any(|m| matches!(m, LeaderboardMessage::Uploaded { outcome, .. }
            if outcome.changed && outcome.rank_new == 3 && outcome.rank_previous.is_none())),
        "{seen:?}"
    );
    let boards = app.world().resource::<Leaderboard>();
    let around: Vec<(u64, u32, i32)> = boards
        .entries("fastest_lap")
        .iter()
        .map(|e| (e.user, e.rank, e.score))
        .collect();
    assert_eq!(around, vec![(12, 2, 62_000), (ME, 3, 63_500), (13, 4, 64_000)]);
    assert_eq!(boards.board("fastest_lap").map(|s| s.lookup), Some(Lookup::Found));
    assert_eq!(boards.pending(), 0);

    // A slower lap with keep-best changes nothing; the friends view is ranked globally.
    {
        let mut boards = app.world_mut().resource_mut::<Leaderboard>();
        boards.submit("fastest_lap", 70_000, &[]).expect("slower");
        boards
            .request("fastest_lap", LeaderboardRange::Friends)
            .expect("friends");
    }
    app.update();
    let boards = app.world().resource::<Leaderboard>();
    let last = boards
        .board("fastest_lap")
        .and_then(|s| s.last_upload)
        .expect("outcome");
    assert!(!last.changed);
    assert_eq!(last.rank_previous, Some(3));
    let friends: Vec<(u64, u32)> = boards
        .entries("fastest_lap")
        .iter()
        .map(|e| (e.user, e.rank))
        .collect();
    assert_eq!(friends, vec![(12, 2), (ME, 3), (15, 6)]);
    assert_eq!(store.rows("fastest_lap")[2].score, 63_500);
}

#[test]
fn leaderboards_without_steam_fail_with_unavailable_and_detach_on_shutdown() {
    let mock = MockSteam::shared_unavailable();
    let mut app = app_with(&mock, true);
    assert_eq!(app.world().resource::<Leaderboard>().service_name(), "none");
    app.world_mut()
        .resource_mut::<Leaderboard>()
        .submit("points", 1, &[])
        .expect("queued");
    app.update();
    assert!(messages(&app).iter().any(|m| matches!(
        m,
        LeaderboardMessage::Failed {
            op: "upload",
            error: LeaderboardError::Unavailable,
            ..
        }
    )));

    // Attached while Steam runs, back to no service after Steam::shutdown.
    let ready = MockSteam::shared();
    let mut app = app_with(&ready, true);
    assert_eq!(app.world().resource::<Leaderboard>().service_name(), "steam");
    app.world_mut().resource_mut::<Steam>().shutdown();
    app.update();
    assert_eq!(app.world().resource::<Leaderboard>().service_name(), "none");
}

#[test]
fn ugc_attach_and_find_or_create_through_the_agent() {
    let mock = MockSteam::shared();
    mock.set_steam_id(Some(SteamId(ME)));
    let mut app = app_with(&mock, true);

    let found = call(
        &mut app,
        "steam.leaderboard_find",
        json!({ "name": "replays", "create": true, "sort": "descending", "display": "numeric", "method": "force_update" }),
    )
    .expect("find");
    assert_eq!(found["service"], "steam");
    assert_eq!(found["board"]["lookup"], "not_started");
    call(
        &mut app,
        "steam.leaderboard_upload",
        json!({ "name": "replays", "score": 4200, "details": [1, 2, 3] }),
    )
    .expect("upload");
    call(
        &mut app,
        "steam.leaderboard_attach_ugc",
        json!({ "name": "replays", "ugc": "18446744073709551615" }),
    )
    .expect("attach");
    call(
        &mut app,
        "steam.leaderboard_download",
        json!({ "name": "replays", "range": "global", "start": 1, "end": 10 }),
    )
    .expect("download");
    app.update();
    app.update();
    let out = call(
        &mut app,
        "steam.leaderboard_entries",
        json!({ "name": "replays" }),
    )
    .expect("entries");
    assert_eq!(out["board"]["lookup"], "found");
    assert_eq!(out["board"]["method"], "force_update");
    assert_eq!(out["board"]["attached_ugc"], "18446744073709551615");
    assert_eq!(out["board"]["pending"], 0);
    assert_eq!(out["local_user"], ME.to_string());
    assert_eq!(out["range"]["kind"], "global");
    assert_eq!(
        out["entries"],
        json!([{ "user": ME.to_string(), "rank": 1, "score": 4200, "details": [1, 2, 3], "ugc": "18446744073709551615" }])
    );
    assert_eq!(mock.leaderboard_store().board_names(), vec!["replays".to_owned()]);

    let list = call(&mut app, "steam.leaderboard_list", json!({})).expect("list");
    assert_eq!(list["plugin"], true);
    assert_eq!(list["boards"][0]["name"], "replays");
    assert_eq!(list["pending"], 0);

    // Force update with the declared method replaces even a lower score.
    call(
        &mut app,
        "steam.leaderboard_upload",
        json!({ "name": "replays", "score": 10 }),
    )
    .expect("force");
    app.update();
    let out = call(
        &mut app,
        "steam.leaderboard_entries",
        json!({ "name": "replays" }),
    )
    .expect("entries");
    assert_eq!(out["board"]["last_upload"]["changed"], true);
    assert_eq!(out["board"]["last_upload"]["score"], 10);
}

#[test]
fn async_failures_land_on_the_board() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true);
    // Find-only on a board Steam does not have: the upload fails with leaderboard_not_found.
    call(
        &mut app,
        "steam.leaderboard_upload",
        json!({ "name": "unknown_board", "score": 1 }),
    )
    .expect("queued");
    app.update();
    let out = call(
        &mut app,
        "steam.leaderboard_entries",
        json!({ "name": "unknown_board" }),
    )
    .expect("entries");
    assert_eq!(out["board"]["lookup"], "missing");
    assert_eq!(out["board"]["error"]["code"], "leaderboard_not_found");

    // An attach with no entry fails with no_entry; an injected platform error surfaces as such.
    mock.leaderboard_store()
        .create_board("pts", LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
    call(
        &mut app,
        "steam.leaderboard_attach_ugc",
        json!({ "name": "pts", "ugc": 7 }),
    )
    .expect("queued");
    app.update();
    app.update();
    let out = call(&mut app, "steam.leaderboard_entries", json!({ "name": "pts" })).expect("entries");
    assert_eq!(out["board"]["error"]["code"], "no_entry");
    mock.leaderboard_store()
        .fail_next(LeaderboardError::Platform("rate limited".into()));
    call(
        &mut app,
        "steam.leaderboard_upload",
        json!({ "name": "pts", "score": 5 }),
    )
    .expect("queued");
    app.update();
    let out = call(&mut app, "steam.leaderboard_entries", json!({ "name": "pts" })).expect("entries");
    assert_eq!(out["board"]["error"]["code"], "leaderboard_platform");
    assert!(mock.leaderboard_store().rows("pts").is_empty());
}

#[test]
fn leaderboard_agent_errors_have_stable_codes() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, false);
    let err = call(
        &mut app,
        "steam.leaderboard_upload",
        json!({ "name": "x", "score": 1 }),
    )
    .expect_err("no resource");
    assert_eq!(reason(&err), "leaderboard_not_running");
    assert_eq!(
        call(&mut app, "steam.leaderboard_list", json!({})).expect("list")["plugin"],
        false
    );

    let mut app = app_with(&mock, true);
    let cases = [
        ("steam.leaderboard_find", json!({ "name": "" }), "name_invalid"),
        (
            "steam.leaderboard_find",
            json!({ "name": "x", "sort": "sideways" }),
            "bad_value",
        ),
        ("steam.leaderboard_upload", json!({ "name": "x" }), "bad_value"),
        (
            "steam.leaderboard_upload",
            json!({ "name": "x", "score": 4_294_967_296_i64 }),
            "bad_value",
        ),
        (
            "steam.leaderboard_upload",
            json!({ "name": "x", "score": 1, "details": vec![0; 65] }),
            "details_too_long",
        ),
        (
            "steam.leaderboard_download",
            json!({ "name": "x", "start": 0 }),
            "range_invalid",
        ),
        (
            "steam.leaderboard_download",
            json!({ "name": "x", "start": 1, "end": 2000 }),
            "range_invalid",
        ),
        (
            "steam.leaderboard_download",
            json!({ "name": "x", "range": "everyone" }),
            "bad_value",
        ),
        (
            "steam.leaderboard_attach_ugc",
            json!({ "name": "x", "ugc": "-1" }),
            "bad_value",
        ),
        (
            "steam.leaderboard_entries",
            json!({ "name": "never_declared" }),
            "leaderboard_not_found",
        ),
    ];
    for (method, params, code) in cases {
        let err = call(&mut app, method, params.clone()).expect_err(method);
        assert_eq!(reason(&err), code, "{method} {params}");
    }
    assert_eq!(
        app.world().resource::<Leaderboard>().pending(),
        0,
        "rejected calls queue nothing"
    );
}

#[test]
fn ownership_answers_for_dlc_and_related_apps() {
    let mock = MockSteam::shared();
    mock.set_app(AppOwnership {
        app: AppId(1_234_560),
        subscribed: true,
        installed: true,
        dlc_installed: true,
        install_dir: Some("/steam/common/Game".into()),
    });
    let mut app = app_with(&mock, false);
    let steam = app.world().resource::<Steam>();
    assert_eq!(steam.dlc_installed(AppId(1_234_560)), Ok(true));
    assert_eq!(steam.owns_app(AppId(999)), Ok(false));
    assert_eq!(steam.app_install_dir(AppId(999)), Ok(None));
    assert!(matches!(steam.owns_app(AppId(0)), Err(SteamError::Rejected(_))));

    let out = call(&mut app, "steam.owns_app", json!({ "app_id": 1_234_560 })).expect("owns_app");
    assert_eq!(
        out,
        json!({ "app_id": 1_234_560, "subscribed": true, "installed": true, "dlc_installed": true, "install_dir": "/steam/common/Game" })
    );
    let err = call(&mut app, "steam.owns_app", json!({ "app_id": 0 })).expect_err("zero");
    assert_eq!(reason(&err), "app_id_invalid");
    let err = call(&mut app, "steam.owns_app", json!({})).expect_err("missing");
    assert_eq!(reason(&err), "bad_value");

    let offline = MockSteam::shared_unavailable();
    let mut app = app_with(&offline, false);
    assert_eq!(
        app.world().resource::<Steam>().owns_app(AppId(480)),
        Err(SteamError::Unavailable)
    );
    let err = call(&mut app, "steam.owns_app", json!({ "app_id": 480 })).expect_err("no steam");
    assert_eq!(reason(&err), "steam_unavailable");
}

#[test]
fn timeline_calls_reach_steam_and_are_logged() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, false);
    {
        let steam = app.world().resource::<Steam>();
        steam.timeline_mode(TimelineMode::LoadingScreen).expect("mode");
        steam.timeline_state("Chapter 2: The Docks", 0.0).expect("state");
        steam
            .timeline_event(
                &TimelineEvent::new("Boss down")
                    .icon("steam_achievement")
                    .priority(800)
                    .span(-12.0, 12.0)
                    .clip(TimelineClip::Featured),
            )
            .expect("event");
        assert!(matches!(
            steam.timeline_event(&TimelineEvent::new("x").priority(1001)),
            Err(SteamError::Rejected(_))
        ));
    }
    let calls = mock.timeline_calls();
    assert_eq!(calls.len(), 3, "a rejected event never reaches Steam: {calls:?}");
    assert_eq!(calls[0], TimelineCall::Mode(TimelineMode::LoadingScreen));
    assert!(
        matches!(&calls[2], TimelineCall::Event(e) if e.title == "Boss down" && (e.start_offset + 12.0).abs() < f32::EPSILON)
    );

    call(&mut app, "steam.timeline_mode", json!({ "mode": "playing" })).expect("mode");
    call(&mut app, "steam.timeline_state", json!({})).expect("clear");
    call(
        &mut app,
        "steam.timeline_event",
        json!({ "title": "Extraction", "duration": 30.0, "clip": "standard", "description": "squad out" }),
    )
    .expect("event");
    let log = call(&mut app, "steam.timeline", json!({})).expect("timeline");
    assert_eq!(log["mode"], "playing");
    assert_eq!(log["state"], Value::Null);
    assert_eq!(log["events"], 2);
    assert_eq!(log["last_event"]["title"], "Extraction");
    assert_eq!(log["last_event"]["icon"], "steam_marker");
    assert_eq!(mock.timeline_calls().len(), 6);
    assert!(matches!(
        mock.timeline_calls()[4],
        TimelineCall::ClearState { .. }
    ));

    for (method, params, code) in [
        ("steam.timeline_mode", json!({ "mode": "sleeping" }), "bad_value"),
        ("steam.timeline_event", json!({ "title": "" }), "steam_rejected"),
        (
            "steam.timeline_event",
            json!({ "title": "x", "duration": -1.0 }),
            "steam_rejected",
        ),
        (
            "steam.timeline_state",
            json!({ "description": "x", "delta": 7200.0 }),
            "steam_rejected",
        ),
    ] {
        let err = call(&mut app, method, params.clone()).expect_err(method);
        assert_eq!(reason(&err), code, "{method} {params}");
    }

    // Without Steam the calls are refused and counted.
    mock.set_state(SteamState::Unavailable);
    app.world_mut().resource_mut::<Steam>().shutdown();
    let steam = app.world().resource::<Steam>();
    assert_eq!(
        steam.timeline_mode(TimelineMode::Menus),
        Err(SteamError::Unavailable)
    );
    assert_eq!(steam.timeline_log().refused, 1);
    let err = call(&mut app, "steam.timeline_mode", json!({ "mode": "menus" })).expect_err("down");
    assert_eq!(reason(&err), "steam_unavailable");
}

#[test]
fn framework_leaderboard_plugin_runs_locally_without_steam() {
    let mut app = App::new();
    app.add_plugins(LeaderboardPlugin::local());
    app.finish_plugins();
    {
        let mut boards = app.world_mut().resource_mut::<Leaderboard>();
        boards
            .declare(LeaderboardSpec::new("points").create_if_missing())
            .expect("declare");
        boards.submit("points", 12, &[]).expect("submit");
        boards.request("points", LeaderboardRange::top(10)).expect("top");
        boards.attach_ugc("points", UgcHandle(5)).expect("ugc");
    }
    app.update();
    app.update();
    let boards = app.world().resource::<Leaderboard>();
    assert_eq!(boards.service_name(), "local");
    assert_eq!(boards.entries("points").len(), 1);
    assert_eq!(
        boards.board("points").and_then(|s| s.attached_ugc),
        Some(UgcHandle(5))
    );
}

#[test]
fn every_error_code_is_documented_on_the_book_page() {
    let page = std::fs::read_to_string(
        regia_test_utils::book::workspace_root().join("book/agents/steam-services.md"),
    )
    .expect("book/agents/steam-services.md");
    for code in [
        "bad_value",
        "leaderboard_not_running",
        "leaderboard_unavailable",
        "name_invalid",
        "leaderboard_not_found",
        "details_too_long",
        "range_invalid",
        "no_entry",
        "ugc_unsupported",
        "leaderboard_platform",
        "steam_unavailable",
        "steam_rejected",
        "app_id_invalid",
    ] {
        assert!(page.contains(&format!("`{code}`")), "{code} is not on the page");
    }
}
