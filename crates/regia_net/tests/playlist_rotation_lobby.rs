// SPDX-License-Identifier: MPL-2.0
//! Playlist rotation on the network side (ADR-8897): the host publishes its session's mode and
//! playlist as `regia.mode` / `regia.playlist`, a browser and the matchmaker filter on them, and a
//! `.playlist.ron` written before the new row fields still loads. Local hub, fixed pumps.

use std::sync::Arc;

use regia_net::formats::PlaylistFile;
use regia_net::lobby::{LobbyError, LobbyId, LobbyQuery, keys, rotation_keys, session_rotation_keys};
use regia_net::{Lobbies, LocalLobbyHub};
use regia_rules::playlist::{PlaylistEntry, PlaylistOrder};
use regia_rules::session::MatchSession;

fn lobbies(hub: &LocalLobbyHub, id: u64) -> Lobbies {
    let mut l = Lobbies::new(Arc::new(hub.member(id, &format!("p{id}"))));
    let manifest = regia_net::session::ContentManifest::build("e", "g", 1, 0x77, &[]);
    l.set_identity("example.arena", &manifest);
    l
}

fn listed(l: &mut Lobbies, query: LobbyQuery) -> Vec<LobbyId> {
    l.list(query).expect("list");
    l.pump();
    let mut ids: Vec<LobbyId> = l
        .last_list()
        .and_then(|r| r.as_ref().ok())
        .map(|v| v.iter().map(|i| i.id).collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

#[test]
fn lobby_data_carries_mode_and_playlist_and_the_filter_matches_them() {
    assert!(keys::ALL.contains(&keys::MODE) && keys::ALL.contains(&keys::PLAYLIST));
    assert_eq!((keys::MODE, keys::PLAYLIST), ("regia.mode", "regia.playlist"));
    assert!(rotation_keys("", "").is_empty(), "empty values publish no key");

    let hub = LocalLobbyHub::new();
    let mut session = MatchSession::default();
    session.playlist.id = "public-ctf".into();
    session.playlist.set(vec![
        PlaylistEntry::new("harbor", "ctf"),
        PlaylistEntry::new("quarry", "koth"),
    ]);
    session.start_now();
    let data = session_rotation_keys(&session);
    assert_eq!(data[keys::MODE], "ctf");
    assert_eq!(data[keys::PLAYLIST], "public-ctf");

    let mut host = lobbies(&hub, 1);
    host.publish_session(&session).expect("publish");
    host.create(host.default_spec()).expect("create");
    host.pump();
    let ctf_lobby = host.current().expect("in lobby");
    let snap = host.snapshot().expect("snapshot");
    assert_eq!(snap.data[keys::MODE], "ctf");
    assert_eq!(snap.data[keys::PLAYLIST], "public-ctf");

    let mut other = lobbies(&hub, 2);
    other.set_rotation_keys("koth", "").expect("keys");
    other.create(other.default_spec()).expect("create");
    other.pump();
    let koth_lobby = other.current().expect("in lobby");
    assert!(!other.snapshot().expect("snap").data.contains_key(keys::PLAYLIST));

    let mut browser = lobbies(&hub, 3);
    assert_eq!(
        listed(&mut browser, LobbyQuery::default().with_mode("ctf")),
        [ctf_lobby]
    );
    assert_eq!(
        listed(&mut browser, LobbyQuery::default().with_mode("koth")),
        [koth_lobby]
    );
    assert_eq!(
        listed(&mut browser, LobbyQuery::default().with_playlist("public-ctf")),
        [ctf_lobby]
    );
    assert!(listed(&mut browser, LobbyQuery::default().with_mode("dom")).is_empty());

    // The host moves to the next row and republishes: the key changes in place.
    session.start_now();
    host.publish_session(&session).expect("republish");
    host.pump();
    assert_eq!(host.snapshot().expect("snap").data[keys::MODE], "koth");
    assert_eq!(
        listed(&mut browser, LobbyQuery::default().with_mode("koth")),
        [ctf_lobby, koth_lobby]
    );

    // The matchmaker joins only a lobby of the asked playlist.
    let mut seeker = lobbies(&hub, 4);
    let mut request = seeker.default_request();
    request.query = LobbyQuery::default().with_playlist("public-ctf");
    request.rounds.truncate(1);
    request.create_if_none = false;
    seeker.start_matchmaking(request).expect("matchmaking");
    seeker.pump();
    seeker.pump();
    assert_eq!(seeker.current(), Some(ctf_lobby));

    // A bad value is refused and changes nothing.
    assert_eq!(
        other.set_rotation_keys("bad\0mode", ""),
        Err(LobbyError::ValueTooLong)
    );
    assert_eq!(other.rotation_keys()[keys::MODE], "koth");
}

#[test]
fn old_playlist_rows_without_the_new_fields_still_load() {
    // A v1 file as written before ADR-8897: no order, no guard, rows without weight or gates.
    let old = r#"(
    format_version: 1,
    looping: true,
    entries: [
        (map_id: "levels/harbor.level.ron", mode: "ctf", options: {"time": "dusk"}),
        (map_id: "levels/quarry.level.ron", mode: ""),
    ],
)"#;
    let file = PlaylistFile::from_ron(old).expect("an old file loads");
    assert_eq!(file.order, PlaylistOrder::InOrder);
    assert_eq!(file.no_repeat_last, 0);
    assert_eq!(file.entries[0].weight, 1);
    assert_eq!(file.entries[0].min_players, None);
    assert_eq!(file.entries[1].max_players, None);
    assert_eq!(file.entries[0].options["time"], "dusk");
    // upgrade-project moves it to v2 (ADR-10064) by the version number alone: every other byte,
    // the absent ADR-8897 fields included, is kept.
    assert_eq!(
        regia_net::formats::migrate_playlist_ron(old).expect("upgrade-project accepts it"),
        old.replacen("format_version: 1", "format_version: 2", 1),
        "only the version number is rewritten"
    );
    // Writing it back keeps the old shape: the new fields at their defaults are not written.
    let text = file.to_ron().expect("write");
    for absent in ["weight", "min_players", "max_players", "order", "no_repeat_last"] {
        assert!(
            !text.contains(absent),
            "{absent} is not written at its default:\n{text}"
        );
    }

    // A session playlist saved before ADR-8897 loads with the defaults and writes back unchanged.
    let saved = r#"(entries:[(map_id:"a",mode:"ctf",options:{})],index:0,looping:false)"#;
    let playlist: regia_rules::playlist::Playlist = ron::from_str(saved).expect("old playlist");
    assert_eq!(playlist.order, PlaylistOrder::InOrder);
    assert_eq!(playlist.entries[0].weight, 1);
    assert_eq!(ron::to_string(&playlist).expect("ron"), saved);

    // The new fields read back, and a bad row is refused with its row number.
    let new = r#"(
    format_version: 1,
    looping: true,
    order: shuffle(seed: 7),
    no_repeat_last: 2,
    entries: [
        (map_id: "levels/harbor.level.ron", mode: "ctf", weight: 3, min_players: 4, max_players: 16),
        (map_id: "levels/quarry.level.ron", mode: "koth"),
    ],
)"#;
    let file = PlaylistFile::from_ron(new).expect("new fields load");
    assert_eq!(file.order, PlaylistOrder::Shuffle { seed: 7 });
    assert_eq!(file.no_repeat_last, 2);
    assert_eq!(
        (
            file.entries[0].weight,
            file.entries[0].min_players,
            file.entries[0].max_players
        ),
        (3, Some(4), Some(16))
    );
    let back = PlaylistFile::from_ron(&file.to_ron().expect("write")).expect("round trip");
    assert_eq!(back, file);
    let inverted = new.replace(
        "min_players: 4, max_players: 16",
        "min_players: 9, max_players: 2",
    );
    let err = PlaylistFile::from_ron(&inverted).expect_err("min above max");
    assert!(err.contains("row 0"), "{err}");
}

/// A row whose effective minimum is above its effective maximum never plays. The field check
/// misses that pair when one bound is the S-15 option (the bound the gate actually reads).
#[test]
fn an_inverted_player_gate_written_through_options_is_refused() {
    let cases = [
        (
            "field min, option max",
            r#"(
    format_version: 1,
    entries: [
        (map_id: "levels/a.level.ron", mode: "ctf", min_players: 8, options: {"max_players": "2"}),
    ],
)"#,
        ),
        (
            "options only",
            r#"(
    format_version: 1,
    entries: [
        (map_id: "levels/a.level.ron", mode: "ctf", options: {"min_players": "9", "max_players": "2"}),
    ],
)"#,
        ),
    ];
    let mut failures = Vec::new();
    for (label, text) in cases {
        match PlaylistFile::from_ron(text) {
            Err(err) if err.contains("row 0") && err.contains("min_players") => {}
            other => failures.push(format!("{label}: {other:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "an impossible player gate must be refused with the row number: {failures:?}"
    );
}

/// The game-modes guide documents playlists with a real `.playlist.ron` example in a ```ron
/// block. `regia_framework`'s `every_rule_set_in_the_game_modes_guide_loads_and_resolves`
/// classifies guide blocks by their rule-set `id` and leaves the rest to this test, so the
/// example stays validated by the crate that owns the format.
#[test]
fn the_game_modes_guide_playlist_example_loads() {
    let page = include_str!("../../../book/user-guide/game-modes.md");
    let mut found = 0;
    for block in page.split("```ron").skip(1) {
        let text = block.split("```").next().unwrap_or_default();
        if !text.contains("format_version") || text.lines().any(|line| line.trim_start().starts_with("id:")) {
            continue; // rule-set examples: regia_framework's ruleset_file tests load those
        }
        let file = PlaylistFile::from_ron(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert!(file.looping);
        assert_eq!(file.order, PlaylistOrder::Shuffle { seed: 7 });
        assert_eq!(file.no_repeat_last, 2);
        assert_eq!(file.entries.len(), 3, "the guide lists its three rows");
        assert_eq!(file.entries[0].weight, 1, "a row without weight plays once");
        assert_eq!(
            (file.entries[1].min_players, file.entries[1].weight),
            (Some(8), 1)
        );
        assert_eq!(
            (file.entries[2].max_players, file.entries[2].weight),
            (Some(6), 2)
        );
        found += 1;
    }
    assert!(found >= 1, "the guide's playlist example is present and loads");
}

/// Every `.playlist.ron` example in the game-modes guide loads, at the current version (ADR-10064).
/// The rule-set examples on the same page are checked by regia_framework's
/// `every_rule_set_in_the_game_modes_guide_loads_and_resolves`, which skips these blocks.
#[test]
fn every_playlist_in_the_game_modes_guide_loads() {
    let page =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../book/user-guide/game-modes.md");
    let page = std::fs::read_to_string(&page).expect("the game-modes guide");
    let mut checked = 0;
    for block in page.split("```ron").skip(1) {
        let text = block.split("```").next().unwrap_or_default();
        if !(text.contains("format_version") && text.contains("entries:") && !text.contains(" id:")) {
            continue;
        }
        let version = regia_core::version::peek_ron_version(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(
            version,
            regia_net::formats::PLAYLIST_RON_FORMAT_VERSION,
            "the guide writes the current version:\n{text}"
        );
        PlaylistFile::from_ron(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        checked += 1;
    }
    assert_eq!(checked, 1, "the guide's one playlist example");
}
