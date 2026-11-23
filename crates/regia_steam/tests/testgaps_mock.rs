// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit: the mock backend against the documented Steam semantics the engine relies on.
//!
//! Known differences between `MockSteam` and `RealBackend` (the real one needs a Steam client):
//! - reading a stat that was never set returns 0 on the mock; the real client errors for a name
//!   missing from the app's stat schema (there is no schema in the mock);
//! - `unlock` of an id the partner site does not know succeeds on the mock (the engine's own
//!   `Steam::unlock` checks `config/achievements.ron` first);
//! - the mock reports `AchievementStored` at once on `unlock`; the real client reports it after
//!   the stats upload;
//! - `logged_on` follows the mock's offline switch, the real one follows the Steam connection.

use regia_steam::{AppId, MockSteam, SteamBackend, SteamError, SteamEvent, SteamState};

fn ready() -> std::sync::Arc<MockSteam> {
    let mock = MockSteam::shared();
    mock.initialize(Some(AppId(480))).expect("init");
    mock
}

/// The trait view of a mock. `MockSteam` also has inherent `achievement_unlocked`, `stat_i32` and
/// `set_stat_i32` *seeding/assertion* helpers with the same names (see `plugin.rs`), so every
/// call here that means the backend semantics goes through the trait explicitly.
fn backend(m: &MockSteam) -> &dyn SteamBackend {
    m
}

fn is_unavailable<T: std::fmt::Debug>(r: Result<T, SteamError>) -> bool {
    matches!(r, Err(SteamError::Unavailable))
}

#[test]
fn every_call_on_an_unavailable_backend_is_unavailable_not_a_silent_success() {
    let m = MockSteam::shared_unavailable();
    assert!(is_unavailable(m.unlock("A")));
    assert!(is_unavailable(m.clear_achievement("A")));
    assert!(is_unavailable(backend(&m).achievement_unlocked("A")));
    assert!(is_unavailable(m.reset_all_stats(true)));
    assert!(is_unavailable(m.add_stat_i32("k", 1)));
    assert!(is_unavailable(backend(&m).set_stat_i32("k", 1)));
    assert!(is_unavailable(m.set_stat_f32("k", 1.0)));
    assert!(is_unavailable(backend(&m).stat_i32("k")));
    assert!(is_unavailable(m.stat_f32("k")));
    assert!(is_unavailable(m.store_stats()));
    assert!(is_unavailable(m.set_rich_presence("status", Some("x"))));
    assert!(is_unavailable(m.cloud_write("a", b"x")));
    assert!(is_unavailable(m.cloud_read("a")));
    assert!(is_unavailable(m.cloud_delete("a")));
    assert!(is_unavailable(m.cloud_exists("a")));
    assert!(is_unavailable(m.cloud_list()));
    assert!(m.lobbies().is_none() && m.p2p().is_none() && m.leaderboards().is_none());
    assert!(!m.logged_on());
    // Error precedence matches the real backend (real.rs checks NUL before the client lookup):
    // a NUL rich-presence key is `Rejected` even while unavailable, not `Unavailable`.
    assert!(
        matches!(
            m.set_rich_presence("sta\0tus", Some("x")),
            Err(SteamError::Rejected(_))
        ),
        "NUL is rejected before the state check, as in the real backend"
    );
}

#[test]
fn an_unlock_is_stored_once_and_again_only_after_a_clear() {
    let m = ready();
    assert_eq!(backend(&m).achievement_unlocked("WIN"), Ok(false));
    m.unlock("WIN").expect("unlock");
    m.unlock("WIN").expect("repeat unlock is not an error");
    assert_eq!(backend(&m).achievement_unlocked("WIN"), Ok(true));
    let stored = |events: Vec<SteamEvent>| {
        events
            .into_iter()
            .filter(|e| matches!(e, SteamEvent::AchievementStored { id } if id == "WIN"))
            .count()
    };
    assert_eq!(stored(m.pump()), 1, "one event for two unlocks");
    m.clear_achievement("WIN").expect("clear");
    assert_eq!(backend(&m).achievement_unlocked("WIN"), Ok(false));
    m.unlock("WIN").expect("unlock again");
    assert_eq!(stored(m.pump()), 1, "a cleared achievement announces again");
    assert_eq!(
        m.unlocked_calls(),
        ["WIN", "WIN", "WIN"],
        "every call is recorded"
    );
}

#[test]
fn a_rejecting_account_fails_the_unlock_and_records_nothing() {
    let m = ready();
    m.reject_unlocks(true);
    assert!(matches!(m.unlock("WIN"), Err(SteamError::Rejected(_))));
    assert_eq!(backend(&m).achievement_unlocked("WIN"), Ok(false));
    assert!(m.pump().is_empty());
    m.set_achievement_query_fails(true);
    assert!(matches!(
        backend(&m).achievement_unlocked("WIN"),
        Err(SteamError::Rejected(_))
    ));
}

#[test]
fn stats_add_saturates_and_reset_clears_stats_but_keeps_achievements_unless_asked() {
    let m = ready();
    assert_eq!(
        backend(&m).stat_i32("kills"),
        Ok(0),
        "documented difference: unknown stat reads 0"
    );
    assert_eq!(m.add_stat_i32("kills", 5), Ok(5));
    assert_eq!(m.add_stat_i32("kills", -7), Ok(-2), "negative deltas are allowed");
    backend(&m).set_stat_i32("kills", i32::MAX - 1).expect("set");
    assert_eq!(
        m.add_stat_i32("kills", 10),
        Ok(i32::MAX),
        "saturates, never wraps"
    );
    m.set_stat_f32("dist", 2.5).expect("f32");
    assert_eq!(m.stat_f32("dist"), Ok(2.5));
    m.unlock("WIN").expect("unlock");

    m.reset_all_stats(false).expect("reset stats");
    assert_eq!(
        (backend(&m).stat_i32("kills"), m.stat_f32("dist")),
        (Ok(0), Ok(0.0))
    );
    assert_eq!(
        backend(&m).achievement_unlocked("WIN"),
        Ok(true),
        "achievements survive a stats-only reset"
    );
    m.reset_all_stats(true).expect("reset all");
    assert_eq!(backend(&m).achievement_unlocked("WIN"), Ok(false));
    assert_eq!(m.reset_calls(), [false, true]);
}

#[test]
fn store_stats_reports_the_upload_on_the_next_pump() {
    let m = ready();
    m.store_stats().expect("store");
    m.store_stats().expect("store");
    assert_eq!(m.store_calls(), 2);
    let events = m.pump();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SteamEvent::StatsStored { ok: true }))
            .count(),
        2
    );
    assert!(m.pump().is_empty(), "pump drains");
}

#[test]
fn cloud_files_follow_the_real_listing_rules() {
    let m = ready();
    assert_eq!(m.cloud_list(), Ok(vec![]));
    m.cloud_write("b.sav", b"2").expect("write");
    m.cloud_write("a.sav", b"1").expect("write");
    assert_eq!(
        m.cloud_list(),
        Ok(vec!["a.sav".to_owned(), "b.sav".to_owned()]),
        "sorted"
    );
    m.cloud_write("a.sav", b"new").expect("overwrite");
    assert_eq!(m.cloud_read("a.sav"), Ok(Some(b"new".to_vec())));
    assert_eq!(
        m.cloud_read("missing"),
        Ok(None),
        "a missing file is None, not an error"
    );
    assert_eq!(m.cloud_exists("missing"), Ok(false));
    assert_eq!(
        m.cloud_delete("missing"),
        Ok(()),
        "deleting a missing file is fine"
    );
    m.cloud_delete("a.sav").expect("delete");
    assert_eq!(m.cloud_exists("a.sav"), Ok(false));
    assert_eq!(m.cloud_list(), Ok(vec!["b.sav".to_owned()]));
    m.cloud_write("empty", b"").expect("empty file");
    assert_eq!(m.cloud_read("empty"), Ok(Some(vec![])));
    for bad in ["", "a\0b"] {
        assert!(
            matches!(m.cloud_write(bad, b"x"), Err(SteamError::Rejected(_))),
            "{bad:?}"
        );
        assert!(matches!(m.cloud_read(bad), Err(SteamError::Rejected(_))));
        assert!(matches!(m.cloud_delete(bad), Err(SteamError::Rejected(_))));
        assert!(matches!(m.cloud_exists(bad), Err(SteamError::Rejected(_))));
    }
}

#[test]
fn rich_presence_with_a_nul_is_rejected_like_the_real_backend() {
    let m = ready();
    assert!(matches!(
        m.set_rich_presence("sta\0tus", Some("x")),
        Err(SteamError::Rejected(_))
    ));
    assert!(matches!(
        m.set_rich_presence("status", Some("x\0y")),
        Err(SteamError::Rejected(_))
    ));
    assert!(m.presence_calls().is_empty(), "a refused call is not recorded");
    m.set_rich_presence("status", Some("In menus")).expect("ok");
    m.set_rich_presence("status", None).expect("clear key");
    assert_eq!(
        m.presence_calls(),
        [
            ("status".to_owned(), Some("In menus".to_owned())),
            ("status".to_owned(), None)
        ]
    );
}

#[test]
fn shutdown_is_idempotent_and_a_no_op_before_init() {
    let fresh = MockSteam::shared();
    fresh.shutdown();
    assert_eq!(fresh.shutdown_calls(), 0, "before init");
    assert_eq!(fresh.state(), SteamState::Ready);
    let m = ready();
    m.shutdown();
    m.shutdown();
    assert_eq!(m.shutdown_calls(), 1);
    assert_eq!(m.state(), SteamState::Unavailable);
    assert!(is_unavailable(m.unlock("A")));
}

#[test]
fn logged_on_needs_ready_and_online() {
    let m = ready();
    assert!(m.logged_on());
    m.set_offline(true);
    assert!(!m.logged_on());
    m.set_offline(false);
    assert!(m.logged_on());
    m.set_state(SteamState::Unavailable);
    assert!(!m.logged_on());
}
