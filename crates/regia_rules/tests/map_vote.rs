// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/map_vote.rs, commit c9513623f), ADR-8012.
// Mark: adapt. Named issues: the rotation ids are neutral (mesa, canyon, ridge, flats); ballots
// live in a BTreeMap; the rotation walk is Playlist::vote_candidates. There is no host override.

//! End-of-match map vote coverage through the public API.
//!
//! Casting, changing a vote, ties, timeouts and the host flag as implemented in
//! `regia_rules::map_vote`.
//!
//! This tree implements no host override for the map vote. The outcome is ballot-only
//! (`winner` / `winner_map` derive from tallied picks, ties broken by rotation order), and the
//! only host-side control is the opt-in `map_vote` flag: while the flag is off (the default)
//! `open_vote` returns `None`. The `host_override_*` tests pin that behavior.

use regia_rules::map_vote::{MAP_VOTE_DURATION_TICKS, MapVote, ServerMaps, VoteReject};

const ROTATION_TOML: &str =
    "[server]\nrotation = [\"mesa\", \"canyon\", \"ridge\", \"flats\"]\nmap_vote = true\n";

fn enabled() -> ServerMaps {
    ServerMaps::parse(ROTATION_TOML)
}

fn open_ballot() -> MapVote {
    enabled()
        .open_vote("mesa", 0)
        .expect("vote is on with a rotation")
}

#[test]
fn casting_records_one_ballot_per_voter_on_the_picked_option() {
    let mut vote = open_ballot();
    assert_eq!(vote.options, vec!["canyon", "ridge", "flats"]);
    vote.cast_vote(1, 0, 0).expect("ballot");
    vote.cast_vote(2, 2, 0).expect("ballot");
    vote.cast_vote(3, 2, 0).expect("ballot");
    assert_eq!(vote.vote_count(), 3);
    assert_eq!(vote.count(0), 1);
    assert_eq!(vote.count(1), 0);
    assert_eq!(vote.count(2), 2);
    assert_eq!(vote.tally(), vec![1, 0, 2]);
    assert_eq!(vote.winner(), Some(2));
    assert_eq!(vote.winner_map(), Some("flats"));
}

#[test]
fn casting_an_unknown_option_is_refused_and_changes_nothing() {
    let mut vote = open_ballot();
    vote.cast_vote(1, 0, 0).expect("ballot");
    assert_eq!(vote.cast_vote(2, 3, 0), Err(VoteReject::BadOption));
    assert_eq!(vote.cast_vote(2, usize::MAX, 0), Err(VoteReject::BadOption));
    assert_eq!(vote.vote_count(), 1, "a refused ballot adds nothing");
    assert_eq!(vote.tally(), vec![1, 0, 0]);
}

#[test]
fn recasting_replaces_the_earlier_pick_without_adding_a_ballot() {
    let mut vote = open_ballot();
    vote.cast_vote(7, 0, 0).expect("ballot");
    vote.cast_vote(7, 1, 1).expect("ballot");
    vote.cast_vote(7, 2, 2).expect("ballot");
    assert_eq!(vote.vote_count(), 1, "still one ballot for that voter");
    assert_eq!(vote.tally(), vec![0, 0, 1], "only the latest pick counts");
    assert_eq!(vote.winner(), Some(2));
}

#[test]
fn recasting_moves_the_tally_between_options() {
    let mut vote = open_ballot();
    vote.cast_vote(1, 0, 0).expect("ballot");
    vote.cast_vote(2, 0, 0).expect("ballot");
    assert_eq!(vote.winner(), Some(0));
    vote.cast_vote(2, 1, 1).expect("ballot");
    assert_eq!(vote.vote_count(), 2);
    assert_eq!(vote.tally(), vec![1, 1, 0]);
    assert_eq!(vote.winner(), Some(0), "a 1-1 tie keeps rotation order");
    vote.cast_vote(1, 1, 2).expect("ballot");
    assert_eq!(vote.tally(), vec![0, 2, 0]);
    assert_eq!(vote.winner(), Some(1));
}

#[test]
fn recasting_to_a_bad_option_keeps_the_earlier_pick() {
    let mut vote = open_ballot();
    vote.cast_vote(7, 0, 0).expect("ballot");
    assert_eq!(vote.cast_vote(7, 9, 1), Err(VoteReject::BadOption));
    assert_eq!(vote.vote_count(), 1);
    assert_eq!(vote.tally(), vec![1, 0, 0], "the refused pick replaces nothing");
    assert_eq!(vote.winner(), Some(0));
}

#[test]
fn two_way_tie_goes_to_the_lower_rotation_index() {
    let mut vote = open_ballot();
    vote.cast_vote(1, 2, 0).expect("ballot");
    vote.cast_vote(2, 0, 0).expect("ballot");
    assert_eq!(vote.tally(), vec![1, 0, 1]);
    assert_eq!(vote.winner(), Some(0));
    assert_eq!(vote.winner_map(), Some("canyon"));
}

#[test]
fn three_way_tie_falls_back_to_the_first_option() {
    let mut vote = open_ballot();
    vote.cast_vote(1, 0, 0).expect("ballot");
    vote.cast_vote(2, 1, 0).expect("ballot");
    vote.cast_vote(3, 2, 0).expect("ballot");
    assert_eq!(vote.tally(), vec![1, 1, 1]);
    assert_eq!(vote.winner(), Some(0));
    assert_eq!(vote.winner_map(), Some("canyon"));
}

#[test]
fn a_late_ballot_can_break_a_tie_before_the_window_closes() {
    let mut vote = open_ballot();
    vote.cast_vote(1, 0, 0).expect("ballot");
    vote.cast_vote(2, 2, 0).expect("ballot");
    assert_eq!(vote.winner(), Some(0));
    vote.cast_vote(3, 2, MAP_VOTE_DURATION_TICKS - 1).expect("ballot");
    assert_eq!(vote.tally(), vec![1, 0, 2]);
    assert_eq!(vote.winner(), Some(2));
    assert_eq!(vote.winner_map(), Some("flats"));
}

#[test]
fn the_window_is_open_until_the_close_tick_then_expired() {
    let vote = open_ballot();
    assert!(vote.is_open(0));
    assert!(vote.is_open(MAP_VOTE_DURATION_TICKS - 1));
    assert!(!vote.is_open(MAP_VOTE_DURATION_TICKS));
    assert!(vote.expired(MAP_VOTE_DURATION_TICKS));
    assert!(vote.expired(MAP_VOTE_DURATION_TICKS + 1));
    assert_eq!(vote.close_tick, vote.opened_tick + MAP_VOTE_DURATION_TICKS);
}

#[test]
fn ballots_after_the_window_are_refused_and_change_nothing() {
    let mut vote = enabled().open_vote("mesa", 100).expect("vote is on");
    vote.cast_vote(1, 1, 100).expect("ballot");
    assert_eq!(
        vote.cast_vote(2, 1, 100 + MAP_VOTE_DURATION_TICKS),
        Err(VoteReject::Closed)
    );
    assert_eq!(
        vote.cast_vote(1, 2, 100 + MAP_VOTE_DURATION_TICKS),
        Err(VoteReject::Closed),
        "even a change of pick is refused once closed"
    );
    assert_eq!(vote.vote_count(), 1);
    assert_eq!(vote.tally(), vec![0, 1, 0]);
}

#[test]
fn ballots_cast_before_the_timeout_still_decide_after_it() {
    let mut vote = open_ballot();
    vote.cast_vote(1, 2, 0).expect("ballot");
    vote.cast_vote(2, 1, MAP_VOTE_DURATION_TICKS - 1).expect("ballot");
    vote.cast_vote(3, 2, MAP_VOTE_DURATION_TICKS - 1).expect("ballot");
    assert!(vote.expired(MAP_VOTE_DURATION_TICKS));
    assert_eq!(vote.tally(), vec![0, 1, 2]);
    assert_eq!(vote.winner(), Some(2));
    assert_eq!(vote.winner_map(), Some("flats"));
}

#[test]
fn a_zero_vote_timeout_falls_back_to_the_first_option() {
    let vote = open_ballot();
    assert_eq!(vote.vote_count(), 0);
    assert!(vote.expired(MAP_VOTE_DURATION_TICKS));
    assert_eq!(vote.winner(), Some(0));
    assert_eq!(vote.winner_map(), Some("canyon"));
}

#[test]
fn host_override_no_vote_opens_while_the_host_leaves_the_flag_off() {
    let off = ServerMaps::parse("[server]\nrotation = [\"a\", \"b\", \"c\", \"d\"]\n");
    assert!(!off.map_vote);
    assert!(off.open_vote("a", 0).is_none());

    let explicit_off =
        ServerMaps::parse("[server]\nrotation = [\"a\", \"b\", \"c\", \"d\"]\nmap_vote = false\n");
    assert!(explicit_off.open_vote("a", 0).is_none());

    assert!(ServerMaps::default().open_vote("a", 0).is_none());
}

#[test]
fn host_override_no_vote_opens_without_a_rotation_even_when_on() {
    let on_but_empty = ServerMaps::parse("[server]\nmap_vote = true\n");
    assert!(on_but_empty.map_vote);
    assert!(on_but_empty.open_vote("a", 0).is_none());
}

#[test]
fn host_override_ballots_alone_decide_the_winner() {
    // No veto, force or pick API exists on MapVote. Whatever voter ids take part, the winner
    // is the tallied plurality with the rotation-order tiebreak.
    let mut vote = open_ballot();
    vote.cast_vote(0, 1, 0).expect("ballot");
    vote.cast_vote(1, 1, 0).expect("ballot");
    vote.cast_vote(2, 0, 0).expect("ballot");
    assert_eq!(vote.winner(), Some(1));
    assert_eq!(vote.winner_map(), Some("ridge"));
    vote.cast_vote(0, 0, 1).expect("ballot");
    vote.cast_vote(1, 0, 1).expect("ballot");
    assert_eq!(vote.winner(), Some(0));
    assert_eq!(vote.winner_map(), Some("canyon"));
}
