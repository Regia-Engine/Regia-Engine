# Match results

`regia_framework::match_results` records every finished match on the host: one `match-result` file per match, a list of sinks that receive it (leaderboards, platform stats, a game's own), and two agent methods plus a CLI command that read the folder back. Match history, per-player stats, ranked play and dispute handling all start from this record. Format page: `docs/formats/match-result.md`.

## What gets recorded

`MatchResultsPlugin` watches the `MatchRules` session. When the session's `match_end` is set (the first `MatchOver` drained after a launch, whether the board or the host ended the match), the plugin builds one `MatchResult`:

| Field | Meaning |
|---|---|
| `format`, `version` | `"match-result"`, `1`. |
| `match_id` | The host's 128-bit id (`Guid`). See [Match ids and lockstep peers](#match-ids-and-lockstep-peers). |
| `ruleset` | `{ id, hash }`: the playlist row's mode string, and BLAKE3 (hex) of the canonical JSON of the session settings plus the row's mode and options. |
| `map_id` | The playlist row's map; empty when no row ran. |
| `start`, `end` | `{ step, tick }`: the lockstep step and match tick of the launch and of the end. |
| `end_reason` | `capture_limit_reached`, `hill_score_limit_reached`, `ended`, `time_limit`, `draw`, `forfeit` or `rounds_won`. |
| `winner`, `draw` | The winning team (`0` for none); `draw` is true when there is no winner or the reason is `draw`. |
| `teams` | One row per team of the board: `{ team, score, round_wins }`. |
| `players` | One row per seated player, by team then slot: `{ key, name?, team, slot, kills, deaths, assists, objective_points, score, counters }` from the session's ledgers. |
| `ledger` | The end-of-match ledger: `{ start_tick, over_tick, tick_hz, timeline }` (at most 4096 timeline events). |
| `replay` | The replay file name, when the game set `MatchResults::replay` during the match. |

A record never holds the wall clock: the date appears only in the folder name.

## Where and how it is written

Set `MatchResultsSettings::root` to the history folder (a dedicated server uses `<server dir>/matches`; `regia server` uses `build/server-dev/matches`). With no root the plugin still builds the record and runs the sinks, and writes no file.

- Path: `<root>/<utc-date>/<match_id>.json`, the date being the host's UTC day at the end of the match.
- The write runs on the IO task pool, never on the tick. It is atomic: a temp file of its own (`<name>.tmp.<pid>.<n>`), fsync, rename, directory fsync. A crash leaves a complete record or none; a temp file is never listed.
- A failed write logs `match_result_write_failed` and is tried once more before anything queued after it. A second failure drops the record (`match_result_dropped`). At most 16 records wait for the pool; a seventeenth pushes the oldest out (also `match_result_dropped`).
- After each write, date folders older than `keep_days` (default 90; `0` keeps every age) and the oldest records beyond `keep_count` (default 1000; `0` keeps every count) are removed, and emptied date folders go with them. Newest means the date folder, then the file's modification time, then the id.
- `write_inline = true` writes on the calling thread (tools and tests). `flush_match_results(world)` waits for every pending write (shutdown).

`MatchResults` counts `written`, `failed` and `dropped` and keeps `last`, `last_path` and `last_error`.

A host that keeps its session outside `MatchRules` calls `record_match(world, &session)` itself after the match ends.

## Player keys and privacy

A row's `key` is the seat's stable player key. The host binds seats in `MatchPlayerKeys`; with the `net` feature, `bind_net_player(world, seat, player, mode)` uses the server persistence identity rule (the authenticator's platform id such as `steam:<id>`, a `#<n>` suffix for a second local player, or with `platform_or_name` the player name as `name:<name>`). An unbound seat is keyed `seat:<team>.<slot>`.

- No IP address is ever recorded.
- No display name is ever recorded: records hold player ids only (ADR-8880; whether a record may carry names is an open owner question). A `name` key on a player row is not part of `match-result` v1 and a reader refuses it (`match_result_malformed`).
- A name-derived key (`name:<name>`) is written as `name-hash:<32 hex>` (BLAKE3 of the name). A hash of a short name can be guessed: a game that needs privacy for named players should use platform ids.

## Sinks

`MatchResultSinks` is an ordered list of `MatchResultSink` (`name`, `send(world, result)`). Every sink runs on the main thread when the record is built, before the write. A sink that fails logs `match_result_sink_failed` with its name, and the next sink still runs.

- `LeaderboardSink { player, boards }` submits, for the local player's row (`player` is that player's key), each `LeaderboardSinkBoard { board, value }` through the `Leaderboard` resource, clamped to 32 bits, with the board's declared upload method. The boards must exist on the service (find-only). A peer without that row (a dedicated host) submits nothing.
- `regia_steam::match_stats::PlatformStatsSink { player, stats }` adds each `PlatformStat { stat, value }` of the local player's row to a Steam stat (`Steam::stat_add`; threshold achievements unlock as usual). A zero value is skipped.
- A game adds its own (`sinks.push(MySink)`). An upload to a web service is a game's sink, never the engine's.

`ResultValue` is `kills`, `deaths`, `assists`, `objective_points`, `score`, `counter(name)` (`0` when absent), `won` (`1` when the player's team won), `team_score` or `round_wins`.

## Match ids and lockstep peers

`MatchResultsSettings::match_id` picks the id:

- `Random` (default): a fresh version 4 id per match, for a host writing alone.
- `Fixed(id)`: the host generated the id and sent it to its peers with the match settings.
- `Derived { seed }`: BLAKE3 of the seed, the ruleset hash, the map and the launch step. Every peer with the same seed derives the same id.

The record's content comes from the session and the player keys only, so every lockstep peer that shares the id writes the same bytes; only the date folder can differ.

## Agent methods

`rules.match_results { since?, limit? }` lists records newest first: `{ root, results, total, skipped, limit, pending, written, failed, dropped }`. Each result is `{ match_id, date, ruleset, map_id, end_reason, winner, draw, end, players }` (`players` is the row count). `since` is a UTC date `YYYY-MM-DD`: that date folder and later. `limit` is 1 to 200 (default 20). `total` counts complete records on or after `since`; `skipped` counts files with a record's name that did not read. A root that does not exist yet lists nothing.

`rules.match_result { match_id }` answers `{ date, result }` with the whole record.

| Code | When |
|---|---|
| `match_results_unavailable` | No `MatchResultsSettings`, or its `root` is not set. |
| `match_results_bad_since` | `since` is not a `YYYY-MM-DD` date. |
| `match_result_bad_id` | `match_id` is not a GUID. |
| `match_result_not_found` | No complete record with that id (rotated away, never written, or still pending). |
| `match_result_malformed` | The file is not a record: bad JSON, another `format`, or a list past its cap. |
| `match_result_newer` | The record's `version` is above 1. |
| `match_result_too_large` | The file is larger than 4 MiB. |
| `match_result_read_failed` | The file could not be read. |
| `F0002` | A parameter of the wrong type, `limit` outside 1..=200, or `match_id` missing. |

Log codes (not protocol errors): `match_result_write_failed`, `match_result_dropped`, `match_result_sink_failed`, `match_result_rotate_failed`.

## CLI

`regia server results [--since YYYY-MM-DD] [--limit N] [--dir DIR] [--json]` prints the same listing, one line per record and a summary line (`--json`: the page as JSON). Without `--dir` it reads `build/server-dev/matches` under `--project`. `--limit` is 1 to 1000 (default 20).

## Verify it

1. `rules.match_setup { start: true, playlist: [{ map_id: "harbor", mode: "ctf" }] }`, seat two players with `rules.match_input`, then end the match (`rules.match_objective` / `rules.match_step` until a capture limit, or the host's end).
2. Call `rules.match_results` again on later frames until `total` is 1 (the write runs on the IO pool; `pending` shows a write in flight).
3. `rules.match_result { match_id }` with the listed id: check `end_reason`, `winner`, `teams` and `players`; check no player row has a `name` key.
4. `regia server results --dir <root>` shows the same record.
