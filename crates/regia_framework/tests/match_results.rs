// SPDX-License-Identifier: MPL-2.0
//! Audit gaps GR-10.1 to GR-10.4: a finished match leaves one `match-result` record, written
//! atomically and rotated, passed through the sinks, with player ids only (no names, ADR-8880).

use std::path::Path;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_framework::leaderboard::{
    Leaderboard, LeaderboardDisplay, LeaderboardPlugin, LeaderboardSort, LocalLeaderboards,
};
use regia_framework::match_results::{
    LeaderboardSink, LeaderboardSinkBoard, MatchIdSource, MatchPlayerKeys, MatchResult, MatchResultSinks,
    MatchResults, MatchResultsPlugin, MatchResultsSettings, ResultContext, ResultValue, build_result,
    date_days, find_result, flush_match_results, list_results, result_path, write_result,
};
use regia_framework::rules::match_flow::{FlowInput, SeatId};
use regia_framework::rules::objective::{EndReason, ObjectiveSettings};
use regia_framework::rules::playlist::PlaylistEntry;
use regia_framework::rules::session::{MatchSession, SessionSettings};
use regia_framework::rules::{MatchRules, MatchRulesPlugin};
use regia_test_utils::TempDir;

/// 2026-11-18 00:00:00 UTC.
const DAY: u64 = 1_794_960_000;

fn fixed_clock() -> u64 {
    DAY + 3_600
}

/// A capture-the-flag match on "harbor": three seats, one kill, one counter, team 1 wins 2-0.
fn played_session() -> MatchSession {
    let mut s = MatchSession::new(SessionSettings {
        objectives: ObjectiveSettings {
            capture_limit: 2,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    s.playlist.set(vec![PlaylistEntry::new("harbor", "ctf")]);
    for (team, slot) in [(1, 0), (1, 1), (2, 0)] {
        s.apply(&FlowInput::Seat { team, slot }).expect("seat");
    }
    s.start_now();
    for _ in 0..10 {
        s.step(&[]);
    }
    s.credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
    // A kill between two empty coordinates opens ledger rows nobody sits in; they are not players.
    s.credit_kill(SeatId::new(4, 1), SeatId::new(4, 2));
    s.add_counter(SeatId::new(1, 1), "revives", 2).expect("counter");
    s.board.team_scores[1] = 2;
    let tick = s.flow.match_tick;
    s.board.end_match(tick, 1, EndReason::CaptureLimitReached);
    s.step(&[]);
    assert!(s.match_end.is_some(), "the drained match-over is kept");
    s
}

fn keys() -> MatchPlayerKeys {
    let mut keys = MatchPlayerKeys::default();
    keys.bind(SeatId::new(1, 0), "steam:76561190000000001");
    keys.bind(SeatId::new(1, 1), "name:Bo");
    keys
}

fn app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins((MatchRulesPlugin, MatchResultsPlugin));
    app.insert_resource(MatchResultsSettings {
        root: Some(root.to_path_buf()),
        write_inline: true,
        ..MatchResultsSettings::default()
    });
    app.insert_resource(keys());
    app.world_mut().resource_mut::<MatchResults>().clock = fixed_clock;
    app
}

fn json_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for day in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(day.path()).into_iter().flatten().flatten() {
            out.push(f.path());
        }
    }
    out.sort();
    out
}

fn result_with(id: u128) -> MatchResult {
    MatchResult {
        match_id: Guid::from_u128(id),
        ..MatchResult::example()
    }
}

#[test]
fn a_finished_match_writes_one_result_with_teams_players_and_reason() {
    let dir = TempDir::new("match-results-one");
    let mut app = app(&dir);
    app.update();
    assert!(
        json_files(&dir).is_empty(),
        "nothing is written while no match ended"
    );

    let session = played_session();
    let end = session.match_end.expect("ended");
    app.world_mut().resource_mut::<MatchRules>().session = session;
    app.update();
    app.update();
    let files = json_files(&dir);
    assert_eq!(files.len(), 1, "one record per match: {files:?}");
    assert_eq!(
        files[0]
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str()),
        Some("2026-11-18"),
        "the UTC date folder"
    );
    let bytes = std::fs::read(&files[0]).expect("read");
    let result = MatchResult::from_json(&bytes).expect("parse");
    assert_eq!(result.format, "match-result");
    assert_eq!(result.version, 1);
    assert_eq!(files[0], result_path(&dir, "2026-11-18", result.match_id));
    assert_eq!(result.map_id, "harbor");
    assert_eq!(result.ruleset.id, "ctf");
    assert_eq!(result.ruleset.hash.len(), 64);
    assert_eq!(result.end_reason, EndReason::CaptureLimitReached);
    assert_eq!((result.winner, result.draw), (1, false));
    assert_eq!(
        result.end.step, end.step,
        "the step the match-over was drained on"
    );
    assert_eq!(result.end.tick, end.tick);
    assert!(result.end.tick > result.start.tick);
    assert_eq!(result.teams.len(), 4, "every team of the board");
    assert_eq!((result.teams[0].team, result.teams[0].score), (1, 2));
    assert_eq!(
        result.players.len(),
        3,
        "seated players only, no empty-coordinate rows"
    );
    assert!(result.players.iter().all(|p| p.team != 4));
    let ada = result.player("steam:76561190000000001").expect("bound key");
    assert_eq!((ada.team, ada.slot, ada.kills), (1, 0, 1));
    let unbound = result
        .player("seat:2.0")
        .expect("unbound seats are keyed by seat");
    assert_eq!(unbound.deaths, 1);
    let bo = result.players.iter().find(|p| p.slot == 1).expect("seat 1.1");
    assert_eq!(bo.counters.get("revives"), Some(&2));
    assert_eq!(result.ledger.over_tick, Some(result.end.tick));
    let state = app.world().resource::<MatchResults>();
    assert_eq!((state.written, state.failed, state.dropped), (1, 0, 0));
}

#[test]
fn a_crash_mid_write_leaves_no_partial_result() {
    let dir = TempDir::new("match-results-crash");
    let done = result_with(1);
    write_result(&dir, "2026-11-18", &done, 0, 0).expect("write");
    // A crash between the temp write and the rename: the temp file of `write_atomic` (its own
    // `<name>.tmp.<pid>.<n>`) holds part of the second record, and no `<id>.json` exists.
    let torn = result_with(2);
    let final_path = result_path(&dir, "2026-11-18", torn.match_id);
    let json = torn.to_json();
    let temp = final_path.with_file_name(format!(
        "{}.tmp.{}.7",
        final_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&temp, &json.as_bytes()[..json.len() / 2]).expect("temp");

    let page = list_results(&dir, None, 10).expect("list");
    assert_eq!(page.total, 1, "only complete files are listed");
    assert_eq!(page.skipped, 0, "a temp file is not a record");
    assert_eq!(page.results[0].match_id, done.match_id);
    let missing = find_result(&dir, torn.match_id).expect_err("never renamed");
    assert!(missing.starts_with("match_result_not_found"), "{missing}");
    let (date, read) = find_result(&dir, done.match_id).expect("complete");
    assert_eq!((date.as_str(), read), ("2026-11-18", done));
}

#[test]
fn results_rotation_keeps_the_newest_by_count() {
    let dir = TempDir::new("match-results-rotate");
    let dates = [
        "2026-11-14",
        "2026-11-15",
        "2026-11-16",
        "2026-11-17",
        "2026-11-18",
    ];
    for (i, date) in dates.iter().enumerate() {
        write_result(&dir, date, &result_with(10 + i as u128), 0, 3).expect("write");
    }
    let page = list_results(&dir, None, 10).expect("list");
    let kept: Vec<&str> = page.results.iter().map(|r| r.date.as_str()).collect();
    assert_eq!(kept, ["2026-11-18", "2026-11-17", "2026-11-16"], "newest first");
    assert!(
        !dir.join("2026-11-14").exists(),
        "an emptied date folder is removed"
    );
    assert!(!dir.join("2026-11-15").exists());

    // By age: two days back from the newest write.
    write_result(&dir, "2026-11-19", &result_with(99), 2, 0).expect("write");
    let page = list_results(&dir, None, 10).expect("list");
    let kept: Vec<&str> = page.results.iter().map(|r| r.date.as_str()).collect();
    assert_eq!(kept, ["2026-11-19", "2026-11-18", "2026-11-17"]);

    // `since` and the limit.
    let since = date_days("2026-11-18");
    let page = list_results(&dir, since, 1).expect("list");
    assert_eq!((page.total, page.results.len()), (2, 1));
    assert_eq!(page.results[0].match_id, Guid::from_u128(99));
}

#[test]
fn leaderboard_sink_submits_the_configured_board_from_a_result() {
    let dir = TempDir::new("match-results-board");
    let local = Arc::new(LocalLeaderboards::new(1));
    // Boards are made on the platform's site; the game only finds them.
    for board in ["kills", "wins"] {
        local.create_board(board, LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
    }
    let mut app = app(&dir);
    app.add_plugins(LeaderboardPlugin::default());
    app.insert_resource(Leaderboard::new(local.clone()));
    app.world_mut()
        .resource_mut::<MatchResultSinks>()
        .push(LeaderboardSink {
            player: "steam:76561190000000001".to_owned(),
            boards: vec![
                LeaderboardSinkBoard {
                    board: "kills".to_owned(),
                    value: ResultValue::Kills,
                },
                LeaderboardSinkBoard {
                    board: "wins".to_owned(),
                    value: ResultValue::Won,
                },
            ],
        });
    app.world_mut().resource_mut::<MatchRules>().session = played_session();
    // The sink queues on the main thread; the board system sends and the local service answers on
    // later frames. A fixed number of frames, no clock.
    for _ in 0..10 {
        app.update();
    }
    let kills = local.rows("kills");
    assert_eq!(kills.len(), 1, "{kills:?}");
    assert_eq!((kills[0].user, kills[0].score), (1, 1));
    let wins = local.rows("wins");
    assert_eq!(wins.first().map(|r| r.score), Some(1));
}

/// Owner question rules Q4, plan default "ids only" (ADR-8880): a stored record names players by
/// key only. A `name` on a player row is not part of `match-result` v1, so a reader refuses it
/// instead of carrying a display name along.
#[test]
fn a_record_with_a_player_name_is_refused() {
    let text = MatchResult::example().to_json();
    let named = text.replacen(
        "\"key\": \"steam:76561190000000001\",",
        "\"key\": \"steam:76561190000000001\",\n      \"name\": \"Ada\",",
        1,
    );
    assert_ne!(named, text, "the fixture names a steam key");
    let err = MatchResult::from_json(named.as_bytes()).expect_err("a player name is refused");
    assert_eq!(err.code(), "match_result_malformed", "{err}");
    assert!(MatchResult::from_json(text.as_bytes()).is_ok());
}

#[test]
fn records_hold_ids_only() {
    let session = played_session();
    let keys = keys();
    let ctx = ResultContext {
        match_id: Guid::from_u128(5),
        keys: Some(&keys),
        replay: None,
    };
    let result = build_result(&session, &ctx).expect("ended");
    let text = result.to_json();
    assert!(!text.contains("\"name\""), "no name field: {text}");
    assert!(
        !text.contains("Bo\""),
        "a name-derived key is not written as bound: {text}"
    );
    assert!(
        text.contains("name-hash:"),
        "a name-derived key is hashed: {text}"
    );
    assert!(
        result.player("steam:76561190000000001").is_some(),
        "a platform id is kept"
    );
    assert!(
        result.player("seat:2.0").is_some(),
        "an unbound seat is keyed by seat"
    );
    assert_eq!(
        MatchResult::from_json(text.as_bytes()).ok(),
        Some(result),
        "it reads back"
    );
}

#[test]
fn twin_peers_write_identical_records() {
    let source = MatchIdSource::Derived { seed: 0x5eed };
    let a = played_session();
    let b = played_session();
    let keys = keys();
    let build = |s: &MatchSession| {
        let ctx = ResultContext {
            match_id: regia_framework::match_results::match_id_for(s, source),
            keys: Some(&keys),
            replay: Some("harbor.regiareplay".to_owned()),
        };
        build_result(s, &ctx).expect("ended").to_json()
    };
    assert_eq!(
        build(&a),
        build(&b),
        "the record's content is the same on every peer"
    );
}

#[test]
fn the_io_pool_writes_and_a_failing_folder_is_retried_once() {
    let dir = TempDir::new("match-results-pool");
    let mut pooled = app(&dir);
    pooled
        .world_mut()
        .resource_mut::<MatchResultsSettings>()
        .write_inline = false;
    pooled.world_mut().resource_mut::<MatchRules>().session = played_session();
    pooled.update();
    flush_match_results(pooled.world_mut());
    assert_eq!(json_files(&dir).len(), 1);

    // A root that is a file: the folder cannot be made, the write fails, is tried once more, then dropped.
    let blocked = dir.join("not-a-folder");
    std::fs::write(&blocked, b"x").expect("file");
    let mut blocked_app = app(&blocked);
    blocked_app.world_mut().resource_mut::<MatchRules>().session = played_session();
    blocked_app.update();
    let state = blocked_app.world().resource::<MatchResults>();
    assert_eq!((state.written, state.failed, state.dropped), (0, 2, 1));
    assert!(state.last_error.is_some());
}

#[test]
fn match_result_json_v1_old_fixture_loads() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/match_results/v1.json");
    let bytes = std::fs::read(&path).expect("fixture");
    let result = MatchResult::from_json(&bytes).expect("v1 loads");
    assert_eq!(result, MatchResult::example());
    assert_eq!(
        result.to_json().as_bytes(),
        bytes.as_slice(),
        "v1 re-encodes to the same bytes"
    );
}
