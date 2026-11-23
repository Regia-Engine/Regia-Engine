# `.rules.ron` (format_version 1)

A rule set: one match mode as data (ADR-8694). A project keeps its rule sets in `assets/rules/`;
the engine ships `tdm`, `ffa`, `ctf`, `koth` and `dom`, which reproduce the Rust defaults bit for
bit. A playlist row names a rule set by id in its `mode`; a bare skeleton id that names no rule set
runs that skeleton's Rust defaults, as before.

```ron
(
    format_version: 1,
    id: "ctf_long",
    name_key: "mode.ctf_long",
    extends: "ctf",
    teams: [
        (id: 1, name_key: "team.red", color: (196, 64, 64, 255), max_seats: 8),
        (id: 2, name_key: "team.blue", color: (64, 112, 196, 255), max_seats: 8),
    ],
    session: (
        flow: (start_delay_ticks: 300),
        phases: (time_limit_ticks: 36000, on_time_up: (kind: "overtime", ticks: 3600)),
        objectives: (capture_limit: 5, flag_touch_radius: 2.5, touch_height: 1.75),
        teams: (switch: (policy: between_matches)),
        respawn: (default: (kind: "delay", ticks: 180), spawn: Scored(enemy: 12, team_radius: 6.5), seed: 27),
    ),
    win_conditions: [],
    options: [
        (key: "capture_limit", kind: int, default: "5", min: "1", max: "10"),
        (key: "sky", kind: enum, default: "day", choices: ["day", "dusk", "night"]),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | `1`. Read before the rest of the file: any other value is `ruleset_version` (`no migrator from rules.ron vN`), never a RON parse error. |
| `id` | The id a playlist row's `mode` names: `[a-z0-9_-]`, 1 to 64 bytes. |
| `name_key` | Localization key of the mode's name. Inherited when empty. |
| `skeleton` | The `MatchMode` lobby id the mode runs on: `tdm`, `ffa`, `ctf`, `koth`, `dom` or `custom`. Required unless a parent sets it. |
| `extends` | The parent rule set's id. At most 4 ancestors (`ruleset_depth`); a chain that comes back is `ruleset_cycle`; a parent that is not loaded is `ruleset_unknown`. |
| `teams` | The fighting teams (`id` 1 to 255, unique; `name_key`; `color` as four bytes; `max_seats`). Absent keeps the parent's list (four unnamed teams at the root). |
| `session.flow` | Any `FlowSettings` field except `phases`: `max_teams`, `seats_per_team`, `seat_slot_limit`, `friendly_unpause_ticks`, `start_delay_ticks`, `pause_name_max`, `max_join_asks`, `max_pauses_per_seat`, `max_pause_ticks_per_seat`, `match_clock` (`session` or `per_match`). |
| `session.phases` | Any `PhaseSettings` field: `warmup_ticks`, `warmup_min_seats`, `countdown_ticks`, `time_limit_ticks`, `on_time_up` (`(kind: "end")`, `"draw"`, `"overtime"` with `ticks`, `"sudden_death"`), `rounds`, `post_match_ticks`, `forfeit_after_ticks`. |
| `session.objectives` | Any `ObjectiveSettings` field: `max_teams`, `max_objectives`, `flag_touch_radius`, `touch_height`, `flag_return_ticks`, `capture_needs_home_flag`, `capture_limit`, `hill_score_limit`, `hill_accrual_ticks`, `max_pending_events`. |
| `session.teams` | `switch` (`(policy: never)`, `(policy: between_matches)`, `(policy: when_unbalanced_by, n: 2)`; the policy is a bare word), `rebalance_between_rounds`, `assist_window_ticks`. |
| `session.respawn` | `default` and `teams` (respawn policies: `(kind: "instant")`, `"delay"` with `ticks`, `"waves"` with `period`, `"lives"` with `lives`, `"never"`), `spawn` (`First`, `RoundRobin`, `Random` or `Scored(...)` with any of the weights `enemy`, `enemy_cap`, `los`, `los_range`, `team`, `team_radius`, `recent`, `recent_ticks`, `priority`), `seed`. |
| `session.zones` | `mode` (`(kind: "domination")`, `"hardpoint"` with `rotation` and `rotate_ticks`, `"attack_defend"` with `attackers`, `defenders`, `order`) and `score_limit`. Zone ids are map data: the host sets hardpoint and attack/defend again after it installs the zones. |
| `session.waves` | Co-op waves: `waves` (the table: each row `(at: ..., groups: [...], alive_cap: 0)`; `at` is `(kind: "after_cleared", delay_ticks: 30)` (the default, no delay) or `(kind: "at_tick", tick: 600)`; each group `(archetype: "grunt", count: 4, spawn_tag: "east")`, `spawn_tag` optional), `difficulty` (rows `(players: 3, count_percent: 150, cap_percent: 125)`, ascending by `players`, percentages 1 to 1000), `players_team` (a fighting team), `shared_lives`, `fail_on_team_wipe`, `success` (`last_wave_cleared`, `objective_reached` or `either`). A wave table makes the mode co-op. |
| `session.race` | A race course: `checkpoints` (each `(id: 10, volume: (min: (x, y, z), max: (x, y, z)), next: [20])`; 2 to 256, the first is the start and finish, `next` names 1 to 8 other checkpoints), `laps` (1 to 1000), `finish_window_ticks`, `wrong_way_ticks`. |
| `session.zone` | A shrinking zone: `center` (`(x, z)`), `radius` (0 to 1,000,000), `phases` (each `(wait_ticks: 10, shrink_ticks: 100, end_radius: 10.0, damage_per_tick: 3)`, at most 64, each `end_radius` at most the radius before it), `seed`. Absent keeps the parent's zone; `Some(None)` turns it off; any patch turns it on, starting from the parent's schedule (or a 1000-unit circle at the origin with no phases). |
| `session.elimination` | `last_standing` and `alliances` (`[(teams: [1, 2])]`; each team a fighting team, in one alliance only). |
| `session.victory` | `conditions` (at most 16, none twice: `(kind: "destroy_all_tagged", tag: "capital")` (1 to 32 bytes), `(kind: "hold_for", objective: 7, ticks: 6000)` (at least 1 tick), `(kind: "score_threshold", score: 500)`, `(kind: "team_elimination")`, `(kind: "surrender")`) and `surrender` field by field (`threshold_percent` 1 to 100, `window_ticks` at least 1, `cooldown_ticks`). |
| `win_conditions` | Stock win rules in order: `(rule: "lives", lives: 3)` (1 to 10) or `(rule: "structures", stems: ["hq"])`. Absent keeps the parent's; empty is the default lives rule. |
| `options` | The option schema a playlist row fills: `key` (`[a-z0-9_]`), `kind` (`int`, `float`, `bool`, `enum`, `string`), `default`, `min` and `max` (`int`, `float`), `choices` (`enum`), and `bind` (optional, ADR-8881): the session field the value sets, one of `OPTION_BINDS` (`objectives.capture_limit`, `phases.time_limit_ticks`, ...; [match rules § Rule sets](../../book/agents/match-rules.md#rule-sets)), of the option's kind; on an optional field 0 is off. Not written while absent, so older v1 files read and write unchanged. Values are text, as a row's options are. A key a parent declared is replaced. `min_players` and `max_players` belong to the playlist and cannot be declared. |

Every field under `session` is optional: an absent field keeps the parent's value, and the root's
parent is the Rust default. A list (a wave table, a course, the phases, the alliances, the conditions) replaces the
parent's list whole. An optional setting may be written bare (`touch_height: 1.75`); to clear a
value a parent set, write `Some(None)`. Unknown fields anywhere are refused (`ruleset_parse` names
the field), including inside a team row, a round, spawn weights, a respawn row and every tagged value
(`kind` or `policy`; ADR-8799), also beside a kind that carries no fields (`(kind: "end", ticks: 30)`,
`(kind: "surrender", extra: 1)`; ADR-8879). A file is at most 256 KiB (`ruleset_too_large`). Floats are read straight to `f32` and
must be finite.

The resolved settings (mode, session settings, zone rules, win conditions and option values, never the
id or the name) have canonical bytes (`f32`s as their bits). Their FNV-1a 64 hash joins the content
manifest as the pack `@rules`, so peers whose rules differ are refused with `content_mismatch`.

A section setting that cannot run is `ruleset_invalid` with an indexed path, for example
`session.waves.waves[0].groups[1].count`, `session.race.checkpoints[2].next[0]`,
`session.zone.phases[1].end_radius`, `session.elimination.alliances[0].teams[1]` or
`session.victory.conditions[2].ticks`.

The genre sections (`waves`, `race`, `zone`, `elimination`, `victory`) were added inside v1 before it shipped
(ADR-8748); a build from before that refuses them as unknown fields. One file per mode is in
[the game-modes guide](../../book/user-guide/game-modes.md#co-op-race-battle-royale-and-strategy-modes).

Version 1 is the first version of this file. `regia upgrade-project` parses and checks a v1 file and
leaves it unchanged, and refuses any other `format_version`. `regia rules check <file>...` checks files
from the command line.

## Versions

| Version | Release | Change |
|---|---|---|
| 1 | v0.7.0 (format batch 2) | First version (ADR-8694), with the genre sections (ADR-8748), the strict nested readers and complete fingerprint (ADR-8799, ADR-8879) and option `bind` (ADR-8881), all added before the freeze. |

v1 is frozen with format batch 2 (ADR-10064): a later change needs a version bump, a migrator, an
`upgrade-project` row, a corpus file and a `format-hashes.ron` row (`docs/specs/12-assets.md` § 4a). Being the first
version, it has no migrator and no `upgrade-project` rewrite. The schema fingerprint
(`rules_ron_schema_fingerprint`) lists the file, every patch, every nested row type and every enum the file reaches,
tagged ones with their payload fields. Corpus: `rules-ron/v1.ron`, `rules-ron/v1-sections.ron` and
`rules-ron/v1-binds.ron` (`crates/regia_agent/tests/fixtures/format-corpus/`); fuzz target `ruleset_ron`.
