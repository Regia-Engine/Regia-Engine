# Match rules in the editor

The editor authors match modes as data and drives the play-in-editor match (ADR-8738). It builds on the
rule-set format (`.rules.ron`, [Match modes and match flow](match-rules.md)) and the playlist format
(`.playlist.ron`); the editor adds a picker, a form, a playlist editor, overlays and a Match strip, all
reachable through the methods below. The user guide's [Authoring modes in the editor](../user-guide/game-modes.md)
section walks through the same tools.

| Method | What it does |
|---|---|
| `editor.match_rules_state` | The Match Rules panel: the open rule set's form rows and problems, the open playlist's rows and problems, the Game Rules choices and the play-in-editor match. |
| `editor.rules_open` | Opens a project `.rules.ron` (`create: true` starts a new one) and brings the panel forward; refuses to drop unsaved edits. |
| `editor.rules_set_field` | Writes one field of the open rule set as one undo step, then checks and resolves it. |
| `editor.rules_save` | Writes the open rule set atomically; refused while it has a problem. |
| `editor.playlist_edit` | Opens, edits (add, remove, move, set a row, looping) and saves a `.playlist.ron`. |
| `editor.match_control` | Starts, pauses, resumes, steps, ends the play-in-editor match; bot seats; the next row. |

`editor.capture_panel { panel: "match_rules" }` (or `"world_settings"`) renders the panel offscreen as a PNG,
which is how the user guide's screenshots are taken. `editor.world_settings_get` also answers `game_rules` (below), and `editor.set_viewport { icon_filters:
{ match_rules } }` switches the overlays. Undo and redo are `editor.undo` and `editor.redo`: rule-set and
playlist edits are on the same stack as level edits, but they never mark the level dirty (they are file
edits).

## Game Rules picker

World Settings draws `LevelSettings.game_rules` as a combo: the names `GameRulesRegistry::registered_names`
lists (the edit world's registry, the play world's registry as last seen during play, so a game crate's own
rules types appear after one play session, and the engine's `DefaultRules` and `MatchSessionRules`), then the
project's rule sets (the shipped `tdm`, `ffa`, `ctf`, `koth`, `dom` and `assets/rules/*.rules.ron`). A pick is
one undo step. A name in neither list stays as typed (the reflected text row below the combo edits it) and is
marked custom.

`editor.world_settings_get` answers:

```json
"game_rules": {
  "current": "ctf_long",
  "status": "rule_set",
  "warning": null,
  "registered": ["DefaultRules", "MatchSessionRules", "my_game::ArenaRules", "..."],
  "rule_sets": ["ctf", "ctf_long", "dom", "ffa", "koth", "tdm"]
}
```

`status` is `none` (no name: the game's own rules), `registered`, `rule_set` or `custom`. A custom name has
`warning: "game_rules_unregistered"`: the level falls back to `DefaultRules` when it loads.

## Rule-set form

`editor.rules_open { path, create?, skeleton?, extends?, discard? }` opens `path` (project-relative, ending in
`.rules.ron`). With `create: true` and no file, a new rule set starts with the file stem as its id, the given
skeleton (`tdm`, `ffa`, `ctf`, `koth`, `dom`, `custom`; default `custom`) and the given parent. Without `path`
it only brings the panel forward. Opening replaces the open rule set and is not an undo step, so while the
open rule set has unsaved edits (a new rule set that was never saved counts) it is refused with
`match_rules_dirty` and nothing changes: save first, or pass `discard: true` to drop the edits (the panel's
"Discard and open" button). The method is a mutating edit (`edit_scene` permission), like
`editor.open_level`.

The form lists every field of the format, in order: the file's own fields (`id`, `name_key`, `skeleton`,
`extends`, `teams`, `win_conditions`, `options`), then every field of every session section
(`session.flow.*`, `session.phases.*`, `session.objectives.*`, `session.teams.*`, `session.respawn.*`,
`session.zones.*`). Each row of `rules.fields` is `{ path, value, inherited }`; an unset field is `null` and
`inherited: true` (the parent's value applies, or the engine default at the root).

`editor.rules_set_field { field, value }` writes `value` (JSON of the field's type) at `field`. Paths name
list entries with `[n]` (`options[0].default`, `teams[1].color`); `n` equal to the list's length appends.
`null` (or no `value`) clears the field. After each edit the rule set is checked on its own and then resolved
against the project's library; the problems are `rules.errors: [{ code, path, message }]` with the format's
own codes (`ruleset_option_range`, `ruleset_option_kind`, `ruleset_unknown`, `ruleset_cycle`,
`ruleset_invalid`, ...; see [Rule sets](match-rules.md)). `rules.can_save` is false while one is listed.

`editor.rules_save` writes the file atomically (a temp file in the same folder, then a rename) and reloads
the project's library, so the picker lists the id at once. While a problem is listed it answers
`match_rules_save_blocked` with `data.errors` and writes nothing. A rule set with no unsaved edits is not
written at all: the file keeps its bytes (comments, layout, the fields it leaves out) and the status line
says it is unchanged. A new file starts with a one-line comment naming the format.

Files in the project's `assets/rules/` folder that do not load (a parse error, an unknown version, a file
over 256 KiB, a link to something that is not a regular file) are left out of the library, so the picker does
not list them. `editor.match_rules_state` names each one in `load_errors: [{ file, code, path, message }]`
(`file` is project-relative, `code` is the format's `ruleset_*` code) and the panel lists them above the
rule-set form. Reads are capped while reading, not only by the size the file system reports.

The form keeps JSON as its value text. A section that holds a value JSON cannot write (`Some(None)`, which
clears a value a parent set) cannot be edited in the form: `match_rules_field_unsupported`; edit that file as
text.

## Playlist editor

`editor.playlist_edit { op, ... }`:

| `op` | Parameters | Effect |
|---|---|---|
| `open` | `path`, `create?`, `discard?` | Opens a project `.playlist.ron` (a new, empty one with `create`). Refused with `match_rules_dirty` while the open playlist has unsaved edits, unless `discard: true`. |
| `add` | `row: { map_id, mode, options }`, `at?` | Inserts a row (at the end by default). |
| `remove` | `index` | Removes a row. |
| `move` | `from`, `to` | Moves a row. |
| `set_row` | `index`, `row` | Merges the given row fields into the row. |
| `set_looping` | `looping` | Starts over after the last row, or not. |
| `save` | | Writes the file atomically; a playlist with no unsaved edits is not rewritten. |

Every edit is one undo step. Each row is validated against the rule set its `mode` names, with the row's
options: an option the schema does not declare (`ruleset_option_unknown`), a value out of range
(`ruleset_option_range`) or of the wrong kind (`ruleset_option_kind`) blocks the save. An empty `mode` plays
the level's own rules. A mode that names no rule set and no skeleton is a game's own mode: it is listed as
`playlist_mode_unknown` with `blocking: false`. `playlist.problems` lists `{ row, code, path, message,
blocking }`; `save` answers `playlist_save_blocked` with `data.problems` while a blocking problem is listed
or the playlist has no row.

The panel's playlist grid edits each row in place: the map, the mode and the options (JSON text, `{"key":
"text"}`) each commit on focus loss as one `set_row`, the same op and undo step as the agent's.

## Play-in-editor Match strip

The Match Rules panel and the play toolbar's Match menu draw the same strip: Start now, Pause, Resume, End
match, Step N ticks, + Bot and − Bot per team, Next row. Each control calls `editor.match_control`:

| `action` | Parameters | Effect on the play world's `MatchRules` session |
|---|---|---|
| `start` | | Starts the next playlist row now. A play world without a session gets one first: the level's rule set (`game_rules` resolved through the project's library, else team deathmatch) over the open playlist's rows (else one row of that mode). A row that names a rule set it does not resolve against (an undeclared option, a value out of range), the rows the playlist's save refuses, makes `start` fail with that row's `ruleset_*` code, and no session is created. |
| `pause` / `resume` | | The host pause switch. |
| `end` | | Ends the match; the flow waits for every seat to ready. |
| `step` | `steps` (1 to 100000) | While paused: resume, run `steps` session steps over the play world's contestants, pause again. |
| `add_bot` | `team` | Seats a bot on the team's lowest free slot (a seat with no pawn). |
| `remove_bot` | `team` | Unseats the team's last bot. |
| `next_row` | `row` | The playlist row the next launch starts. |

The call answers `{ queued: true, id, action }`; the control runs on the editor's next frame.
`editor.match_rules_state`'s `match.last_control` is then `{ id, action, ok, code, message, events, tick }`,
and `match.live` mirrors the session every frame: `tick`, `phase`, `paused`, `between`, `over`, `mode`,
`team_scores`, `objectives` (with each zone's `owner`, `capturing`, `progress` and `capture_ticks`),
`playlist_index`, `playlist_rows`, `seated` and `spawn_heat` (`{ position, score, eligible }` per spawn
point, the picker's scores for the play world's first player, as `rules.spawn_preview` answers them).
`match.bots` lists the bot seats the strip added.

Example: pause, run one second of match ticks and check the clock. Each control runs on the editor's
next frame, so read the state after it (the `id` in `match.last_control` is the control's).

```json
{"method": "editor.match_control", "params": {"action": "pause"}}
{"method": "editor.match_control", "params": {"action": "step", "steps": 60}}
{"method": "editor.match_rules_state", "params": {}}
```

## Overlays

Show ▸ Icons ▸ Match rules (`icon_filters.match_rules`, on by default) draws:

- every player start with a `SpawnPoint` team in that team's color, labeled `team N · pP` (`· off` when
  disabled); a start for every team (team 0) keeps the player-start color;
- every flag base: its touch radius in its team's color, the touch height band when `touch_height` is set,
  and a line to the flag while it is away (`flag N · team T · home|carried|dropped`);
- every hill and zone box in its owner's color, and a zone's capture progress as an arc in the capturing
  team's color (`zone N · owner O · progress/capture_ticks`);
- during play, the spawn-score heat: a ring per spawn point from blue (lowest eligible score) to red
  (highest), gray when the picker refuses the point, labeled with its score.

Team colors come from the play session during play, else the edit world's session, else the level's rule set,
else the engine's four default teams. The objectives come from the play session during play, else from a
`MatchRules` session in the edit world (one an agent set up with `rules.match_setup` and
`rules.match_objective`).

## Errors

Rule-set problems keep their `ruleset_*` codes and refused match inputs keep the rules' `match_*` codes
([Match modes and match flow](match-rules.md)). The panel's own codes:

| Code | When |
|---|---|
| `match_rules_no_project` | The editor has no project directory. |
| `match_rules_bad_path` | The path is not a project-relative `.rules.ron` / `.playlist.ron` path. |
| `match_rules_dirty` | An open would drop the open rule set's or playlist's unsaved edits: save, or pass `discard: true`. |
| `match_rules_read_failed` | The file is missing (without `create`) or does not read. |
| `match_rules_not_open` | No rule set is open. |
| `match_rules_bad_field` | The field path names no field, or the value does not fit it. |
| `match_rules_field_unsupported` | The section holds a cleared value (`Some(None)`) the form cannot keep. |
| `match_rules_save_blocked` | Save refused: `data.errors` lists the problems. |
| `match_rules_write_failed` | The atomic write failed. |
| `playlist_not_open` | No playlist is open. |
| `playlist_bad_op` | Unknown `op`, or a parameter it needs is missing. |
| `playlist_bad_row` | A row index out of range, a row that does not read, or more than 1024 rows. As a row problem (blocking, any mode): an empty `map_id`, a `weight` above 100, or a `min_players` bound above the `max_players` bound (field or row option; path `max_players`), the rows `.playlist.ron` refuses to read. |
| `playlist_save_blocked` | Save refused: `data.problems` lists the blocking rows (or there is no row). |
| `playlist_mode_unknown` | Warning on a row: its mode names no rule set and no skeleton. |
| `game_rules_unregistered` | Warning: `game_rules` names neither a registered rules type nor a rule set. |
| `match_control_not_playing` | No play session runs (or it ended before the control ran). |
| `match_control_bad_action` | Unknown `action`, a missing parameter, or a team outside the fighting teams. |
| `match_control_no_session` | The play world has no match session (`start` creates one). |
| `match_control_not_paused` | `step` while the match runs. |
| `match_control_no_seat` | `add_bot` on a team with no free seat. |
| `match_control_no_bot` | `remove_bot` on a team with no bot seat. |
| `match_control_bad_row` | `next_row` past the playlist. |

## Known limits

- Hill and zone boxes draw from the session's board; they are not level entities yet, so they have no box
  handles in the viewport. Place and resize them through `rules.match_objective` (follow-up).
- A step while paused shows `resumed` and `paused` among its events.
