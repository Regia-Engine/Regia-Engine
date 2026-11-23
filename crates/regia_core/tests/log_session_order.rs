// SPDX-License-Identifier: MPL-2.0
//! Recovery orders session identifiers independently of their peer directory suffix.
use regia_core::log::session::{PeerLog, SessionOptions};
use regia_test_utils::TempDir;

#[test]
fn a_prefix_session_name_does_not_hide_a_newer_unclean_session() {
    let root = TempDir::new("peer-prefix-order");
    let options = SessionOptions::default();
    let older = PeerLog::create(&root, "a", 0, options).expect("older session");
    older.finish().expect("clean older session");
    drop(older);
    let newer = PeerLog::create(&root, "a-", 0, options).expect("newer session");
    drop(newer);
    let next = PeerLog::create(&root, "b", 0, options).expect("next session");
    assert_eq!(
        next.recovered().map(|found| found.session.as_str()),
        Some("a--peer0"),
        "the newer session id must win over the older clean directory"
    );
}

#[test]
fn a_newer_clean_prefix_session_still_stops_recovery_of_an_older_unclean_one() {
    let root = TempDir::new("peer-prefix-clean");
    let options = SessionOptions::default();
    let older = PeerLog::create(&root, "a", 0, options).expect("older live session");
    let newer = PeerLog::create(&root, "a-", 0, options).expect("newer session");
    newer.finish().expect("clean newer session");
    drop(newer);
    drop(older);
    let next = PeerLog::create(&root, "b", 0, options).expect("next session");
    assert!(next.recovered().is_none());
    assert!(!root.join("unclean-a-peer0").exists());
}
