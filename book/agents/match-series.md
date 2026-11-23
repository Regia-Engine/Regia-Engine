# Series, end-of-match vote, win rule and the stats ledger

`regia_rules` keeps a best-of series, an end-of-match map vote, a win rule and a match-end ledger beside the match session (ADR-8406). The same types are re-exported from `regia_framework::rules`. A lockstep peer applies the same host inputs, so the series, the ballot and the ledger stay in step. `cargo xtask det-scan` scans the crate.

The camera orbit and the tactical view stay in spectator modes. This page is the vision rule only: a team seat sees its own team, its allies and neutrals; a finished match reveals every team; a neutral seat sees fighting teams when the host grants it; a seat with no lives left spectates its team.

## Series

Lengths are 1, 3, 5, 7 and 9. A round whose winner is 0, or outside the fighting teams, is replayed. The carry is five opaque integers the game sets. When a side reaches the wins it needs, the series ends and the carry clears.

`rules.series` reads `length`, `played`, `wins`, `carry`, `winner`, `max_teams`, `need`, `over`, `label`, `winner_label`, `row_text`, `lives` and `rule`. `recorded` is present when the call booked a round.

The host may send:

- `length`: 1, 3, 5, 7 or 9. Anything else is `series_length`.
- `reset`: back to 0-0 at the same length.
- `record`: `{ winner, carry }`. `carry` is five integers. Omitted, the carry is zeros.
- `lives`: starting lives, 1 through 10. Outside that range is `lives_range`. The default is 3.
- `rule`: `lives` or `structures`. The lobby words `clones` and `production` select the same rules.

`as` is `{ team, slot }` for a guest. Omitted, the caller is the host. A guest who sends `length`, `reset`, `record`, `lives` or `rule` is `match_not_permitted` and the series is unchanged. Order when several are set: `length`, then `reset`, then `record`, then `lives`, then `rule`.

A match that ends books the round from the session carry and opens the vote when the vote flag is on. The game calls `evaluate_win` with the seat and structure view. The stock rules are lives-out and named-structures-destroyed. A custom rule implements `WinCondition`.

## Vote

The vote is off until the host sets the flag. It offers the next maps in the rotation, wrapping once, three by default, for 20 seconds at the 60 Hz tick rate (1200 ticks). Ties keep rotation order. One ballot per voter; a later pick replaces the earlier one. Zero votes falls back to the first option.

`rules.vote_status` reads `open`, `closed`, `options`, `tally`, `winner`, `winner_map`, `opened_tick`, `close_tick` and `vote_count` at `now_tick` (default 0).

`rules.vote_cast` takes `voter`, `option` and optional `now_tick` and `as`. A guest may cast only the voter id of their own seat (`team` shifted 8 bits, then the slot). That check runs before the window, so a guest is `match_not_permitted` even when the vote is already closed. A closed or missing vote is `vote_closed`. An option that is not on the ballot is `vote_bad_option`. The host closes the vote and reads the winning map.

`vote_candidates` on the playlist is the same eligibility walk the rotation uses: from the current index, wrap once, skip a row whose `min_players` or `max_players` excludes the player count. A missing bound is no limit.

## Ledger

`rules.end_stats` reads `start_tick`, `over_tick`, `tick_hz`, `lines`, `length`, `mvp_index`, `mvp` and `teams`. `length` is `M:SS` or `H:MM:SS` at the ledger's tick rate (default 60). The timeline keeps eight moments: the first kill, then structure losses. A primary structure is `STRUCTURE LOST`. A producer is `PRODUCER LOST` plus the stem.

The host may send `note_kill` `{ tick, killer_team, victim_team }` and `note_structure` `{ tick, team, stem, producer, primary }`. A guest who sends either is `match_not_permitted`. `players` is an array of `{ team, name, kills, deaths, ships_lost, gathered, spent, units_made, buildings_made, is_ai }`. The MVP is most kills, then fewest deaths, then fewest ships lost, then the lowest team, then the name. An AI seat is skipped while a player exists. `teams` sums those rows. Teams outside 1..=4 never open the ledger.

Objective events do not carry kills. The game calls the kill and structure notes.

With no `MatchRules` resource, every method here answers `match_rules_unavailable`. A malformed parameter is `F0002`.
