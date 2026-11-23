# `match-result` (version 1)

One finished match, written by the host's `regia_framework::match_results::MatchResultsPlugin`
([match results](../../book/agents/match-results.md)) to `<root>/<utc-date>/<match_id>.json`, where `<root>` is
`MatchResultsSettings::root` and `<utc-date>` is `YYYY-MM-DD`. Read back by `rules.match_results`,
`rules.match_result` and `regia server results`, which treat the folder as untrusted (fuzz target
`match_result_json`).

The file is pretty-printed JSON (`serde_json`, two-space indent, a trailing newline). Unknown fields are refused in every
object, a `ledger.timeline` row included (ADR-10064).

| Field | Type | Meaning |
|---|---|---|
| `format` | string | `"match-result"`. Anything else is refused (`match_result_malformed`). |
| `version` | `u32` | 1. A higher version is refused (`match_result_newer`). |
| `match_id` | string | The host's 128-bit id, as a dashed lowercase GUID; also the file name. |
| `ruleset` | object | `{ id, hash }`: the playlist row's mode string; BLAKE3 (64 lowercase hex) of the canonical JSON `{ settings, mode, options }` of the session. |
| `map_id` | string | The playlist row's map id; empty when no row ran. |
| `start` | object | `{ step, tick }` (`u64`): the launch's lockstep step and match tick. |
| `end` | object | `{ step, tick }`: the step the match end was drained on and the match tick it ended on. |
| `end_reason` | string | `capture_limit_reached`, `hill_score_limit_reached`, `ended`, `time_limit`, `draw`, `forfeit`, `rounds_won`. |
| `winner` | `u8` | The winning team; `0` for none. |
| `draw` | bool | No winner, or the reason is `draw`. |
| `teams` | list | At most 256: `{ team: u8, score: u32, round_wins: u32 }`, by team. |
| `players` | list | At most 1024, by team then slot: `{ key: string, team: u8, slot: u8, kills: u32, deaths: u32, assists: u32, objective_points: u32, score: i64, counters: map string → i64 }`. There is no display name: records hold ids only (ADR-8880), and a row with a `name` key is refused. `key` is a platform id (`steam:<id>`), `name-hash:<32 hex>` for a name-derived key, or `seat:<team>.<slot>`. |
| `ledger` | object | `{ start_tick: u64, over_tick: u64 or null, tick_hz: u32, timeline }`; `timeline` is at most 4096 `{ tick, kind, team, foe_team, stem }` with `kind` `FirstKill`, `ProducerLost` or `StructureLost`. |
| `replay` | string, optional | The replay file name when the match was recorded. |

**Limits.** A file over 4 MiB is refused before it is read (`match_result_too_large`).

**Atomicity.** The file is written to `<name>.tmp.<pid>.<n>`, synced, renamed over `<match_id>.json` and the folder synced.
A reader lists only `<guid>.json` files in `YYYY-MM-DD` folders, so a torn write is never a record.

**Versions.** v1 is the first version, frozen with format batch 2 in v0.7.0 (ADR-10064): a later change needs a
version bump, a migrator, an `upgrade-project` row, a corpus file and a `format-hashes.ron` row
(`docs/specs/12-assets.md` § 4a). `regia upgrade-project` recognizes a record by its `format` key (other `.json`
files are skipped) and reports its version; being the first version, it has no migrator (the stub refuses every
`from`). Corpus: `match-result/v1.json` (`crates/regia_agent/tests/fixtures/format-corpus/`); fuzz target
`match_result_json`.

**Example.** `crates/regia_framework/tests/fixtures/match_results/v1.json` (`MatchResult::example()`).
