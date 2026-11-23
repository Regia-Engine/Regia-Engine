// SPDX-License-Identifier: MPL-2.0
//! Playlist rotation (ADR-8897, audit gaps GR-5.4 to GR-5.6): shuffled and weighted orders drawn
//! from `DetRng` streams, player gates and missing content skipped with a reason, the repeat
//! guard, and the map vote sharing the rotation's eligibility rule. Fixed inputs only.

use regia_rules::match_flow::FlowSettings;
use regia_rules::objective::RulesEvent;
use regia_rules::playlist::{Playlist, PlaylistEntry, PlaylistOrder, RowSkip, SkipReason};
use regia_rules::session::{MatchSession, SessionSettings};

fn rows(ids: &[&str]) -> Vec<PlaylistEntry> {
    ids.iter().map(|id| PlaylistEntry::new(*id, "tdm")).collect()
}

fn take_rows(list: &mut Playlist, n: usize) -> Vec<u32> {
    (0..n)
        .map(|_| list.take_next().map(|(index, _)| index).expect("a row"))
        .collect()
}

fn session() -> MatchSession {
    MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    })
}

fn skipped_events(events: &[RulesEvent]) -> Vec<(u32, String, SkipReason)> {
    events
        .iter()
        .filter_map(|event| match event {
            RulesEvent::RowSkipped {
                index,
                map_id,
                reason,
                ..
            } => Some((*index, map_id.clone(), *reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn shuffle_is_identical_on_two_peers_and_never_repeats_the_last_row_first() {
    const ROWS: usize = 5;
    const CYCLES: usize = 6;
    let mut guarded_boundaries = 0;
    for seed in 0..48u64 {
        let make = || Playlist {
            looping: true,
            order: PlaylistOrder::Shuffle { seed },
            entries: rows(&["a", "b", "c", "d", "e"]),
            ..Playlist::default()
        };
        let mut peer_a = make();
        let mut peer_b = make();
        let plays_a = take_rows(&mut peer_a, ROWS * CYCLES);
        let plays_b = take_rows(&mut peer_b, ROWS * CYCLES);
        assert_eq!(plays_a, plays_b, "seed {seed}: two peers take the same rows");
        assert_eq!(peer_a, peer_b, "seed {seed}: and keep the same state");

        for (cycle, plays) in plays_a.chunks(ROWS).enumerate() {
            let mut sorted = plays.to_vec();
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                [0, 1, 2, 3, 4],
                "seed {seed} cycle {cycle}: every row once"
            );
            if cycle > 0 {
                let last = plays_a[cycle * ROWS - 1];
                assert_ne!(
                    plays[0], last,
                    "seed {seed} cycle {cycle}: the last row never opens a cycle"
                );
                // The guard did work when the drawn cycle opened with the last row.
                let drawn = make().build_cycle(u32::try_from(cycle).expect("small"));
                if drawn[0] == last {
                    guarded_boundaries += 1;
                }
            }
        }
        // A different seed is a different order (checked on one pair so the test stays exact).
        if seed == 1 {
            let mut other = Playlist {
                order: PlaylistOrder::Shuffle { seed: 2 },
                ..make()
            };
            assert_ne!(take_rows(&mut other, ROWS * CYCLES), plays_a);
        }
    }
    assert!(
        guarded_boundaries > 0,
        "some drawn cycle opened with the last row and was guarded"
    );

    // A one-row shuffle still plays (the guard is soft).
    let mut single = Playlist {
        looping: true,
        order: PlaylistOrder::Shuffle { seed: 9 },
        entries: rows(&["solo"]),
        ..Playlist::default()
    };
    assert_eq!(take_rows(&mut single, 3), [0, 0, 0]);

    // The default order is S-14's: list order, no drawn cycle, no new fields serialized.
    let mut plain = Playlist {
        looping: true,
        entries: rows(&["a", "b", "c"]),
        ..Playlist::default()
    };
    assert_eq!(take_rows(&mut plain, 4), [0, 1, 2, 0]);
    assert!(plain.cycle.is_empty() && plain.recent.is_empty());
}

#[test]
fn weighted_order_follows_weights_over_a_seeded_cycle() {
    let weights = [3u32, 1, 2, 0];
    let mut entries = rows(&["harbor", "depot", "ridge", "never"]);
    for (row, weight) in entries.iter_mut().zip(weights) {
        row.weight = weight;
    }
    let make = |seed| Playlist {
        looping: true,
        order: PlaylistOrder::Weighted { seed },
        entries: entries.clone(),
        ..Playlist::default()
    };
    let per_cycle: usize = weights.iter().map(|w| *w as usize).sum();
    let mut peer_a = make(77);
    let mut peer_b = make(77);
    let plays = take_rows(&mut peer_a, per_cycle * 4);
    assert_eq!(plays, take_rows(&mut peer_b, per_cycle * 4), "peers agree");
    for (cycle, chunk) in plays.chunks(per_cycle).enumerate() {
        let mut counts = [0u32; 4];
        for row in chunk {
            counts[*row as usize] += 1;
        }
        assert_eq!(
            counts, weights,
            "cycle {cycle} plays every row exactly its weight"
        );
    }
    assert!(!plays.contains(&3), "weight 0 never plays");
    assert_ne!(
        take_rows(&mut make(78), per_cycle * 4),
        plays,
        "another seed draws another order"
    );

    // A weight above the cap plays as the cap.
    let mut heavy = Playlist {
        order: PlaylistOrder::Weighted { seed: 1 },
        entries: rows(&["a", "b"]),
        ..Playlist::default()
    };
    heavy.entries[0].weight = 1_000_000;
    assert_eq!(
        heavy.build_cycle(0).len(),
        regia_rules::playlist::MAX_ROW_WEIGHT as usize + 1
    );

    // A weighted list that does not loop ends after its one cycle.
    let mut once = Playlist {
        looping: false,
        ..make(5)
    };
    assert_eq!(take_rows(&mut once, per_cycle).len(), per_cycle);
    assert_eq!(once.take_next(), None);
}

#[test]
fn rows_outside_the_player_gate_are_skipped_with_a_reason() {
    let mut big = PlaylistEntry::new("canyon", "ctf");
    big.min_players = Some(8);
    let mut small = PlaylistEntry::new("mesa", "tdm");
    small.max_players = Some(4);
    let mut option_gate = PlaylistEntry::new("flats", "koth");
    option_gate.options.insert("min_players".into(), "6".into());
    let mut list = Playlist {
        looping: true,
        entries: vec![
            big.clone(),
            small.clone(),
            option_gate,
            PlaylistEntry::new("ridge", "tdm"),
        ],
        ..Playlist::default()
    };

    let pick = list.take_next_for(6, |_| true);
    assert_eq!(
        pick.row
            .as_ref()
            .map(|(index, row)| (*index, row.map_id.as_str())),
        Some((2, "flats")),
        "the option bound is the same gate"
    );
    assert_eq!(
        pick.skipped,
        vec![
            RowSkip {
                index: 0,
                reason: SkipReason::TooFewPlayers
            },
            RowSkip {
                index: 1,
                reason: SkipReason::TooManyPlayers
            },
        ]
    );
    assert_eq!(SkipReason::TooFewPlayers.code(), "too_few_players");
    assert_eq!(SkipReason::TooManyPlayers.code(), "too_many_players");

    // The vote offers the rows the rotation would take, by the same rule.
    let vote: Vec<String> = list
        .vote_candidates(4, 6)
        .into_iter()
        .map(|row| row.map_id)
        .collect();
    assert_eq!(
        vote,
        ["ridge", "flats"],
        "walks from the next row; gated rows are left out"
    );

    // No row fits: no row, the state stays, and the skips say why.
    let mut gated = Playlist {
        looping: true,
        entries: vec![big, small],
        ..Playlist::default()
    };
    let before = gated.clone();
    let pick = gated.take_next_for(6, |_| true);
    assert_eq!(pick.row, None);
    assert_eq!(gated, before, "a pick with no row changes nothing");
    assert_eq!(pick.skipped.len(), 2);

    // In a session: the skips are events before the launch, and with no fitting row the current
    // row reruns (S-14's `launched` with no index; the board keeps its mode).
    let mut s = session();
    let mut wide = PlaylistEntry::new("canyon", "ctf");
    wide.min_players = Some(8);
    s.playlist.set(vec![PlaylistEntry::new("harbor", "koth"), wide]);
    s.playlist.looping = true;
    s.player_count = 4;
    s.start_now();
    let first = s.drain_events();
    assert!(skipped_events(&first).is_empty());
    s.start_now();
    let events = s.drain_events();
    assert_eq!(
        skipped_events(&events),
        vec![(1, "canyon".to_string(), SkipReason::TooFewPlayers)],
        "the gated row is reported"
    );
    let launched_index = events.iter().find_map(|event| match event {
        RulesEvent::Launched { index, .. } => Some(*index),
        _ => None,
    });
    assert_eq!(
        launched_index,
        Some(Some(0)),
        "the walk came around to the fitting row"
    );
    let skip_at = events
        .iter()
        .position(|e| matches!(e, RulesEvent::RowSkipped { .. }))
        .expect("skip");
    let launch_at = events
        .iter()
        .position(|e| matches!(e, RulesEvent::Launched { .. }))
        .expect("launch");
    assert!(skip_at < launch_at, "skips come before the launch they explain");

    let mut stuck = session();
    let mut only_big = PlaylistEntry::new("canyon", "ctf");
    only_big.min_players = Some(8);
    stuck.playlist.set(vec![PlaylistEntry::new("harbor", "koth")]);
    stuck.start_now();
    stuck.playlist.entries.push(only_big);
    stuck.playlist.entries[0].min_players = Some(8);
    stuck.playlist.looping = true;
    stuck.player_count = 2;
    let mode = stuck.board.mode;
    stuck.start_now();
    let events = stuck.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RulesEvent::Launched { index: None, .. }))
    );
    assert_eq!(stuck.board.mode, mode, "the current row reruns");
    assert_eq!(
        stuck.current.as_ref().map(|row| row.entry.map_id.as_str()),
        Some("harbor")
    );
    assert_eq!(skipped_events(&events).len(), 2);
}

#[test]
fn a_row_with_missing_content_is_skipped() {
    let mut list = Playlist {
        entries: vec![
            PlaylistEntry::new("lost_map", "tdm"),
            PlaylistEntry::new("harbor", "lost_rules"),
            PlaylistEntry::new("depot", "ctf"),
        ],
        ..Playlist::default()
    };
    let installed = ["harbor", "depot", "tdm", "ctf"];
    let pick = list.take_next_for(0, |row| {
        installed.contains(&row.map_id.as_str()) && installed.contains(&row.mode.as_str())
    });
    assert_eq!(pick.row.map(|(index, _)| index), Some(2));
    assert_eq!(
        pick.skipped,
        vec![
            RowSkip {
                index: 0,
                reason: SkipReason::MissingContent
            },
            RowSkip {
                index: 1,
                reason: SkipReason::MissingContent
            },
        ]
    );
    assert_eq!(SkipReason::MissingContent.code(), "missing_content");

    // In a session: the host lists missing map and rule-set ids.
    let mut s = session();
    s.playlist.set(vec![
        PlaylistEntry::new("lost_map", "tdm"),
        PlaylistEntry::new("harbor", "lost_rules"),
        PlaylistEntry::new("depot", "ctf"),
    ]);
    s.missing_content.insert("lost_map".into());
    s.missing_content.insert("lost_rules".into());
    s.start_now();
    let events = s.drain_events();
    assert_eq!(
        skipped_events(&events),
        vec![
            (0, "lost_map".to_string(), SkipReason::MissingContent),
            (1, "harbor".to_string(), SkipReason::MissingContent),
        ]
    );
    assert_eq!(s.current.as_ref().map(|row| row.index), Some(2));

    // A shuffled cycle drops a missing row for that cycle; the others still play once each.
    let mut shuffled = Playlist {
        looping: true,
        order: PlaylistOrder::Shuffle { seed: 4 },
        entries: rows(&["a", "b", "gone", "c"]),
        ..Playlist::default()
    };
    let mut plays = Vec::new();
    for _ in 0..6 {
        let pick = shuffled.take_next_for(0, |row| row.map_id != "gone");
        plays.push(pick.row.expect("a row").0);
    }
    assert!(!plays.contains(&2));
    let mut first_three = plays[..3].to_vec();
    first_three.sort_unstable();
    assert_eq!(first_three, [0, 1, 3]);
}

#[test]
fn the_repeat_guard_avoids_recent_rows_while_another_fits() {
    // In order, a recent row the walk reaches again is passed over with `recent`.
    let mut list = Playlist {
        looping: true,
        no_repeat_last: 1,
        entries: rows(&["a", "b", "c"]),
        ..Playlist::default()
    };
    assert_eq!(take_rows(&mut list, 1), [0]);
    list.index = 0; // a host rewinds the list
    let pick = list.take_next_for(0, |_| true);
    assert_eq!(pick.row.map(|(index, _)| index), Some(1));
    assert_eq!(
        pick.skipped,
        vec![RowSkip {
            index: 0,
            reason: SkipReason::Recent
        }]
    );
    assert_eq!(SkipReason::Recent.code(), "recent");
    // The guard is soft: when only the recent row fits, it plays.
    let mut single = Playlist {
        looping: true,
        no_repeat_last: 3,
        entries: rows(&["solo"]),
        ..Playlist::default()
    };
    assert_eq!(take_rows(&mut single, 3), [0, 0, 0]);

    // A guard of 2 over a shuffle: no row within two plays of itself across many cycles.
    let mut shuffled = Playlist {
        looping: true,
        no_repeat_last: 2,
        order: PlaylistOrder::Shuffle { seed: 11 },
        entries: rows(&["a", "b", "c", "d"]),
        ..Playlist::default()
    };
    let plays = take_rows(&mut shuffled, 40);
    for window in plays.windows(3) {
        assert_ne!(window[0], window[1]);
        assert_ne!(window[0], window[2]);
        assert_ne!(window[1], window[2]);
    }

    // `start_at` tells the guard which row the host started on.
    let mut started = Playlist {
        looping: true,
        order: PlaylistOrder::Shuffle { seed: 3 },
        entries: rows(&["a", "b", "c"]),
        ..Playlist::default()
    };
    for row in 0..3 {
        let mut copy = started.clone();
        copy.start_at(row);
        assert_ne!(copy.take_next().map(|(index, _)| index), Some(row));
    }
    started.order = PlaylistOrder::InOrder;
    started.start_at(1);
    assert_eq!(started.take_next().map(|(index, _)| index), Some(2));
}

#[test]
fn the_map_vote_offers_the_rows_the_rotation_reaches() {
    let make = || Playlist {
        looping: true,
        order: PlaylistOrder::Shuffle { seed: 21 },
        entries: rows(&["a", "b", "c", "d", "e"]),
        ..Playlist::default()
    };
    let mut list = make();
    list.take_next();
    let offered: Vec<String> = list
        .vote_candidates(3, 0)
        .into_iter()
        .map(|row| row.map_id)
        .collect();
    let mut walk = list.clone();
    let taken: Vec<String> = (0..3).map(|_| walk.take_next().expect("row").1.map_id).collect();
    assert_eq!(offered, taken, "the ballot is the next three rows of the cycle");

    // The session opens its ballot from the same walk with its player count.
    let mut s = session();
    s.maps.map_vote = true;
    s.playlist = make();
    s.playlist.entries[1].min_players = Some(10);
    s.player_count = 2;
    s.start_now();
    let expected: Vec<String> = s
        .playlist
        .vote_candidates(s.maps.option_count, 2)
        .into_iter()
        .map(|row| row.map_id)
        .collect();
    s.board.end_match(0, 1, regia_rules::objective::EndReason::Ended);
    s.step(&[]);
    let vote = s.vote.as_ref().expect("vote opened");
    assert_eq!(vote.options, expected);
    assert!(
        !vote.options.contains(&"b".to_string()),
        "a gated row is not on the ballot"
    );
}

/// The end-of-match ballot is opened after the launch that consumed a row, including the launch
/// that closed a cycle. The next launch then applies the repeat guard to the new cycle. The ballot
/// has to name that same row (`vote_candidates` "in the order the rotation would reach them").
#[test]
fn the_ballot_at_a_cycle_boundary_is_the_row_the_next_launch_plays() {
    let mut mismatches = Vec::new();
    for seed in 0..64u64 {
        let mut list = Playlist {
            looping: true,
            order: PlaylistOrder::Shuffle { seed },
            entries: rows(&["a", "b", "c"]),
            ..Playlist::default()
        };
        for _step in 0..12 {
            let at_boundary = !list.cycle.is_empty() && list.index as usize >= list.cycle.len();
            if at_boundary {
                let offered = list
                    .vote_candidates(1, 0)
                    .into_iter()
                    .map(|row| row.map_id)
                    .next();
                let mut walk = list.clone();
                let taken = walk.take_next().map(|(_, row)| row.map_id);
                if offered != taken {
                    mismatches.push((seed, offered, taken));
                }
            }
            if list.take_next().is_none() {
                break;
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "the ballot's first map is not the next launch: {mismatches:?}"
    );
}

/// rules-review-fixes-b (playlist review 2): a weighted ballot is the walk the launches take, and
/// a cycle that repeats one row by its weight ends the walk instead of looping.
#[test]
fn a_weighted_ballot_follows_the_launches_and_ends_on_a_one_row_cycle() {
    let mut heavy = PlaylistEntry::new("a", "tdm");
    heavy.weight = regia_rules::playlist::MAX_ROW_WEIGHT;
    let one_row = Playlist {
        looping: true,
        order: PlaylistOrder::Weighted { seed: 5 },
        entries: vec![heavy],
        ..Playlist::default()
    };
    let offered: Vec<String> = one_row
        .vote_candidates(3, 0)
        .into_iter()
        .map(|row| row.map_id)
        .collect();
    assert_eq!(offered, ["a"], "one row, offered once");

    for seed in 0..32u64 {
        let mut entries = rows(&["a", "b", "c"]);
        entries[0].weight = 3;
        entries[2].weight = 2;
        let mut list = Playlist {
            looping: true,
            order: PlaylistOrder::Weighted { seed },
            no_repeat_last: 1,
            entries,
            ..Playlist::default()
        };
        for _step in 0..14 {
            let offered: Vec<String> = list
                .vote_candidates(3, 0)
                .into_iter()
                .map(|row| row.map_id)
                .collect();
            let mut walk = list.clone();
            let mut taken: Vec<String> = Vec::new();
            for _ in 0..64 {
                if taken.len() == 3 {
                    break;
                }
                let row = walk
                    .take_next()
                    .expect("a looping list always has a row")
                    .1
                    .map_id;
                if !taken.contains(&row) {
                    taken.push(row);
                }
            }
            assert_eq!(
                offered, taken,
                "seed {seed}: the ballot is the next distinct launches"
            );
            list.take_next().expect("a row");
        }
    }
}

/// rules-review-fixes-b (match-state replication review A): a forfeit for a team that does not lead
/// on points is kept on the session, and a serialized session (snapshot, hot reload) keeps it.
#[test]
fn a_forfeit_winner_is_kept_on_the_session() {
    use regia_rules::objective::EndReason;
    let mut s = session();
    s.start_now();
    s.step(&[]);
    s.board.team_scores[1] = 5;
    s.board.team_scores[2] = 1;
    assert_eq!(s.winner, None, "a running match names no winner");
    s.report_condition(2, EndReason::Forfeit);
    s.step(&[]);
    assert!(s.board.match_over);
    assert_eq!(
        s.winner,
        Some(2),
        "the forfeit names team 2 while team 1 leads 5-1"
    );
    let text = serde_json::to_string(&s).expect("session json");
    let back: MatchSession = serde_json::from_str(&text).expect("reads back");
    assert_eq!(back.winner, Some(2), "the named winner survives a snapshot");
    // Nested state (series, victory) has its own `winner` fields: check the session's own key.
    let fresh = serde_json::to_value(session()).expect("session json");
    assert!(
        fresh.get("winner").is_none(),
        "a session that never ended keeps its shape"
    );
    let ended = serde_json::to_value(&s).expect("session json");
    assert_eq!(ended.get("winner"), Some(&serde_json::json!(2)));
}

/// R-04 (3), ADR-8881: an in-order ballot applies the repeat guard the launches apply. Rows among
/// the last `no_repeat_last` played come after every other eligible row, so the ballot's first map
/// is the row the next launch plays. Without a guard the ballot is the wrap-once walk, as before.
#[test]
fn an_in_order_ballot_applies_the_repeat_guard() {
    let mut gated = PlaylistEntry::new("b", "tdm");
    gated.options.insert("min_players".into(), "4".into());
    let mut list = Playlist {
        looping: true,
        no_repeat_last: 2,
        entries: vec![
            PlaylistEntry::new("a", "tdm"),
            gated,
            PlaylistEntry::new("c", "tdm"),
        ],
        ..Playlist::default()
    };
    // Two players: `a`, then `b` is gated and `c` plays. Recent: a, c.
    let played: Vec<u32> = (0..2)
        .map(|_| {
            list.take_next_for(2, |_| true)
                .row
                .map(|(index, _)| index)
                .expect("a row")
        })
        .collect();
    assert_eq!(played, [0, 2]);
    // Four players now: the launch passes the recent `a` and plays `b`.
    let offered: Vec<String> = list
        .vote_candidates(3, 4)
        .into_iter()
        .map(|row| row.map_id)
        .collect();
    let mut walk = list.clone();
    let next = walk
        .take_next_for(4, |_| true)
        .row
        .map(|(_, row)| row.map_id)
        .expect("a row");
    assert_eq!(next, "b");
    assert_eq!(
        offered,
        ["b", "a", "c"],
        "the ballot leads with the row the next launch plays; recent rows come last"
    );
    // No guard: the plain wrap-once walk from the index.
    list.no_repeat_last = 0;
    let offered: Vec<String> = list
        .vote_candidates(3, 4)
        .into_iter()
        .map(|row| row.map_id)
        .collect();
    assert_eq!(offered, ["a", "b", "c"]);
}
