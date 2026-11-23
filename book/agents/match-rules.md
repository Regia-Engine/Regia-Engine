# Match modes and match flow

`regia_rules` holds the match core a lockstep game steps on every peer (ADR-8102, ADR-8239): objective entities
(flags and the hill), the capture-the-flag and king-of-the-hill skeletons, pause and friendly pause, ready-up that
starts the next match on an agreed step, observers and join requests, and the playlist. It is plain data with no
ECS, GPU or platform type in it, and it computes with `+ - *` and comparisons only, so two peers that apply the same
inputs on the same steps hold the same bits. `cargo xtask det-scan` scans the crate and checks its dependencies and clippy configuration
(`docs/ci/lockstep-closure.toml`, a `[[crate]]` row; see [Lockstep math](lockstep-math.md)).

The `rules::` paths below are the compatible `regia_framework::rules` exports.
Simulation-only callers use the same module names directly under `regia_rules`.

| Module | What it holds |
|---|---|
| `rules::mode` | `MatchMode`: `tdm`, `ffa`, `ctf`, `koth`, `custom`; `from_lobby_str`, `as_lobby_str`. |
| `rules::objective` | `Objective` (flag or hill, owned position), `HillVolume`, `Contestant` (one unit as the rules see it), `ObjectiveSettings`, `ObjectiveBoard` (objectives, `team_scores`, `match_over`, `end_reason`, events), `RulesEvent`. |
| `rules::ctf` | `ObjectiveBoard::{spawn_flag, place_flags, ctf_tick}`, `ctf_base_position`, `ctf_base_position_for_starts`. |
| `rules::koth` | `ObjectiveBoard::{install_hill_volume, koth_tick}`. |
| `rules::match_flow` | `MatchFlow`, `FlowSettings`, `FlowInput`, `SeatId`, `FlowStep`. |
| `rules::playlist` | `Playlist`, `PlaylistEntry` (map id, mode string, options). |
| `rules::session` | `MatchSession`: the flow, the playlist and the board stepped together; `SessionSettings`. |
| `rules` | `MatchRules` (the session as a resource for the agent) and `MatchRulesPlugin`, `RulesError`. |

## The step

Per lockstep step the host applies that step's inputs in the agreed order (`MatchSession::apply`), then calls
`MatchSession::step(units)` with every unit that can touch an objective, in its own deterministic entity order (the
first touching unit in that order wins a pickup). A step does one of three things:

- **Start** the next match when the agreed step has come: the next playlist row is taken, the board is reset for the
  row's mode, and a `launched` event names the row. The game loads the map and places the objectives.
- **Hold** while paused: the match tick stays frozen and the held steps count toward the friendly-pause hold.
- **Run**: the match tick advances by one, then the capture-the-flag and king-of-the-hill rules run at the new tick.

The flow keeps two clocks: `step` (the lockstep step, always advancing; inputs land on it) and `match_tick` (frozen
under a pause).

## Ready-up on an agreed step

`open_between` ends a match and pauses: every seated seat that does not observe has to ready. When the last one does
on step `T`, the next match is scheduled for step `T + start_delay_ticks` (`starts_at`, a `launch_scheduled` event).
Every peer applies the same ready on the same step, so every peer starts on the same step, whatever order the inputs
of one step arrived in. An `unready` during the countdown calls the start off (`launch_cancelled`); the next full
ready schedules a new one. `override_afk` makes the seats that did not ready observers and schedules the start.
`start_delay_ticks` is 180 (three seconds at 60 Hz) by default; `0` starts on the step the last ready lands on.

An observer's ready is a join request in `joins`, kept oldest first (the request step, then the seat). The host
approves the oldest ask of a team onto a seat of that team with `approve_join`. A further ask past `max_join_asks`
(8 by default) is `match_join_queue_full`; the same seat asking again does not take another slot. An observer sees
its own team, its allies and neutrals (`MatchFlow::viewer_sees`).

## Pause

`pause(name)` pauses a friendly match under a name (control characters dropped, cut to `pause_name_max`). The direct
`MatchFlow::pause` call still labels a blank name `host`. Inputs applied through `apply` or `apply_from` do not: a
blank name shows the seat label (`team T slot S`) and never the word `host`, and the seat is stored in `paused_by_seat`.
The banner name does not grant a resume, and `Resume.host` in the input is ignored. The pauser's seat and the host
issuer may resume at once; anyone else waits `friendly_unpause_ticks` held steps (3600, sixty seconds at 60 Hz, never
the wall clock). `host_set_paused` is the host's switch and is not tied to a seat, so a player leaving does not release
it. Between matches a resume is refused (`match_ready_to_start`): the ready gate starts the next match.

`max_pauses_per_seat` and `max_pause_ticks_per_seat` default to off (`None`). When set, a seat that has started that
many pauses, or held for that many ticks, is refused (`match_pause_budget`). A host issuer with no seat is not charged.
Spending the tick budget releases that seat's hold on the step that reaches it; that step stays held.

`match_clock` is `session` by default: `match_tick` continues across launches, so a mode that scores on a period keeps
the previous match's phase. `per_match` sets `match_tick` to 0 when a match launches, then the launch step still
advances it, so king of the hill's first point waits a full accrual period.

## Objectives

**Capture the flag.** One flag per fighting team, placed with `spawn_flag(entity_id, team, base)` or `place_flags`
(the symmetric layout: teams 1 and 2 face off on X, 3 and 4 on Z). Each run step: carried flags follow their
carriers and a dead or missing carrier drops the flag where it was; then captures resolve (a carrier at its own base
while its own flag is home scores one, and the captured flag goes home); then pickups and returns (an enemy takes a
flag from its base or the ground, a teammate returns a dropped flag). Reaching `capture_limit` ends the match
(`capture_limit_reached`).

**King of the hill.** One hill volume (`install_hill_volume`). On every accrual tick (`hill_accrual_ticks`, one
second by default; never tick 0) a team that is alone in the volume with at least one living unit scores one point;
contested or empty scores nothing. Reaching `hill_score_limit` ends the match (`hill_score_limit_reached`).

The host owns the entities: an objective carries the host's `entity_id` and its `position`, which the host mirrors
onto its entity after each step. Team counts, seats, caps, limits, radii and periods are settings
(`SessionSettings`); every peer of a match runs the same ones.

## Agent methods

All five work on the `MatchRules` resource. `rules.match_setup` inserts it; the others answer
`match_rules_unavailable` without it. The answers carry the whole session (`flow`, `playlist`, `board`, `current`),
`ready_counts` and the board's `mode` id.

| Method | What it does |
|---|---|
| `rules.match_get` | Read the session. |
| `rules.match_setup` | Replace the session: `settings`, `playlist` rows, `looping`; `start` starts the first row now, `between` opens the ready gate. |
| `rules.match_input` | Apply `input` and/or `inputs` on the current step, in order; stops at the first refusal. Answers `applied` and the `events`. |
| `rules.match_objective` | Place one `flag` (`team`, `position` or `offset`, `id`), the `hill` (`min`, `max`, `id`) or `place_flags` (`teams`, `offset`, `first_id`). Answers `placed` (the ids). |
| `rules.match_step` | Run `steps` steps (1..=100000) with the same `units` each step. Answers `last_step`, `launched_on` and the `events`. |
| `rules.match_report` | The host's reports for the strategy victory conditions: `tagged`, `holders`, `scores`, `defeat` (see [Strategy victory conditions](#strategy-victory-conditions)). Answers what `rules.match_get` answers. |

A flow input is an object tagged by `input`:

```text
{ "input": "seat", "team": 1, "slot": 0 }
{ "input": "ready", "team": 1, "slot": 0 }
{ "input": "unready", "team": 1, "slot": 0 }
{ "input": "observe", "team": 2, "slot": 3 }
{ "input": "leave", "team": 2, "slot": 1 }
{ "input": "approve_join", "team": 2, "slot": 4 }
{ "input": "pause", "name": "ada" }
{ "input": "resume", "name": "bev", "host": false }
{ "input": "host_pause", "on": true }
{ "input": "override_afk" }
{ "input": "open_between" }
{ "input": "surrender_vote", "team": 1, "slot": 0, "yes": true }
```

A ready-up that starts a capture-the-flag row five steps after the last ready:

```json
{ "method": "rules.match_setup", "params": { "settings": { "flow": { "start_delay_ticks": 5 } }, "playlist": [{ "map_id": "harbor", "mode": "ctf" }], "between": true } }
{ "method": "rules.match_input", "params": { "inputs": [{ "input": "seat", "team": 1, "slot": 0 }, { "input": "ready", "team": 1, "slot": 0 }] } }
{ "method": "rules.match_step", "params": { "steps": 10 } }
```

The last answer has `launched_on: [5]`, `mode: "ctf"` and `current.entry.map_id: "harbor"`.

### Error codes

| Code | When |
|---|---|
| `match_rules_unavailable` | No `MatchRules` resource (add `MatchRulesPlugin` or call `rules.match_setup`). |
| `F0002` | A malformed parameter: an unknown input, a missing field, `steps` out of range, `start` with `between`, not exactly one objective. |
| `match_seat_bad` | The team or slot is outside the settings. |
| `match_seat_limit` | The slot is at or past `seat_slot_limit`. |
| `match_seat_missing` | A ready from a seat that is not seated. |
| `match_live` | A ready while the match runs (ready-up is between matches). |
| `match_not_between` | `unready` or `override_afk` while the match runs. |
| `match_no_join_ask` | `approve_join` with no waiting request. |
| `match_join_team` | `approve_join` onto another team than the request's. |
| `match_already_paused` | `pause` while paused. |
| `match_not_paused` | `resume` while running. |
| `match_ready_to_start` | A resume between matches. |
| `match_unpause_held` | Not the pauser or the host, and the hold has not passed. |
| `match_flag_team` | A flag for a team outside the fighting teams. |
| `match_flag_exists` | The team already has a flag. |
| `match_hill_exists` | A second hill. |
| `match_objective_cap` | The board holds `max_objectives` objectives. |
| `match_objective_id` | The id is already an objective. |
| `match_not_permitted` | This issuer may not send that input. The state is unchanged. |
| `match_join_queue_full` | The join queue already holds `max_join_asks` requests. |
| `match_pause_budget` | That seat has used its pause count or its held-tick budget. |
| `match_phase_settings` | `rules.match_setup`: a phase setting cannot run (a zero time limit, overtime cap, warm-up or forfeit hold, zero rounds to win, or `max_rounds` below `rounds_to_win`). |
| `match_wave_settings` | `rules.match_setup`: `settings.waves` cannot run (see [Co-op waves](#co-op-waves)). |
| `match_race_settings` | `rules.match_setup`: `settings.race` cannot run (see [Races](#races)). |
| `match_shrink_zone_settings` | `rules.match_setup`: `settings.zone` cannot run. `data.reason` is the code; the message names the field (`center`, `radius`, `phases` or `end_radius`). |
| `match_victory_settings` | `rules.match_setup` or `rules.match_report`: a `destroy_all_tagged` tag is empty or longer than 32 bytes, or a report table already holds 1024 rows. |
| `match_surrender_off` | `surrender_vote` while the match has no `surrender` condition, is between matches or decided, or the team is out. |
| `match_surrender_cooldown` | `surrender_vote` while the team's cooldown after a failed ballot runs. |

A refused input or objective answers its code in `data.code` and again in `data.reason`; the inputs before it in the
same call stay applied.

## Who may send what

The session layer passes an `Issuer` (`seat`, `host`) into `MatchSession::apply_from`. It comes from the lockstep seat
map or the server connection and is never decoded from the input. `MatchSession::apply` and `MatchFlow::apply` are the
host issuer, so a host-side caller keeps the previous call. `rules.match_input` takes the same issuer as optional
`as`: `{ "team", "slot" }`. Omitted, the caller is the host.

| Input | Who may send it |
|---|---|
| `ready`, `unready`, `observe`, `leave` | That seat, or the host. |
| `pause`, `resume` | An issuer who has a seat, or the host. The seat is the issuer's seat; the input does not name one. |
| `seat` of the issuer's own seat | That seat, or the host. |
| `seat` of another seat, `approve_join`, `host_pause`, `override_afk`, `open_between` | The host only. |

Anything else is `match_not_permitted` and changes nothing. Permission is checked before the seat is validated, so a
guest naming another seat is refused even when that seat would also be out of range. `resume`'s `host` field is
deprecated serde compatibility and is ignored.

## Leaving and joining

`leave` takes the seat off `seated`, `ready`, `observers` and the join queue, then reconsiders the ready gate. A seat
that was on one of those ledgers emits `seat_left`. A friendly pauser who leaves releases the hold (`resumed` when the
match is live). A host pause, which has no `paused_by_seat`, stays. A seated player who disconnects can leave, so the
remaining seats can ready up without an AFK override.

Join asks queue in `joins`, sorted by the request step and then by seat, capped by `max_join_asks` (default 8).
`approve_join` takes the oldest ask of the offered seat's team and drops that asker and that seat from the observers.
It does not seat the asker; the game does. An empty queue is `match_no_join_ask`. Asks that are all for other teams
are `match_join_team`, naming the oldest ask's team.

## Events

`RulesEvent`s come back from every step and every input call, tagged by `kind`: `flag_taken`, `flag_dropped`,
`flag_returned`, `flag_captured`, `hill_point`, `match_over`, `paused`, `resumed`, `launch_scheduled`,
`launch_cancelled`, `launched`, `seat_left`. They are for the HUD feed, the announcer and the match log, and never feed back into
the rules.

Source: DO27's mode and flow code, adapted (ADR-8012 A-1, ADR-8102; `docs/ported-sources.toml`).

## Linking the core from a simulation

Link `regia_rules` for the plain-data core. `regia_framework::rules` re-exports
`ctf`, `koth`, `match_flow`, `mode`, `objective`, `playlist`, `session` and
`RulesError` under their existing paths. `MatchRules` and `MatchRulesPlugin` stay
in the framework; all five `rules.match_*` methods keep the same contract.
The existing `match_rules_step` tracing span now has the module target
`regia_rules::session`; update module-based tracing filters when switching.

DO27 switch note (ADR-8239): replace the sim's `regia_framework` dependency with
`regia_rules` and import `regia_rules::session::MatchSession` and the other
modules directly. Preserve S-14 compatibility values: `start_delay_ticks = 0`,
`seat_slot_limit = Some(5)` for the alpha, and parse the game's mode names before
falling back to `MatchMode::from_lobby_str`. No setting default, simulation
operation, error code or serialization shape changes in this extraction.

DO27 switch note (ADR-8332): pass the peer's real seat as the issuer on
`MatchSession::apply_from` (or `MatchFlow::apply_from`). `apply` remains the host
issuer. Compat values, which are the defaults: `match_clock = Session`,
`max_pauses_per_seat = None`, `max_pause_ticks_per_seat = None`. `max_join_asks`
defaults to 8, and a second ask no longer replaces the first. `Resume.host` in
the input bytes is not authority.

## Phases, time limits and rounds

`regia_rules::phases` (ADR-8485) adds warm-up, a countdown, a time limit with overtime or sudden death, rounds with
side swaps, a post-match intermission and forfeits. The settings are `FlowSettings::phases` (`PhaseSettings`), every
one off by default; with all of them off the phase machine does nothing and emits nothing, and a match runs exactly as
before (the S-14 capture-the-flag twin run hashes the same bits). Every clock is a tick count of steps the match ran:
a pause holds them all. A host converts seconds once, at setup, with `regia_detsim::tick::ticks_from_secs`.

| Setting | Default | What it does |
|---|---|---|
| `warmup_ticks` | `None` | Warm-up length. Nothing scores; the scores reset and the flags go home when it ends. |
| `warmup_min_seats` | `None` | The warm-up also ends when this many seated, non-observer seats are present. |
| `countdown_ticks` | `0` | The hold before live play, after the warm-up and between rounds. Nothing scores. |
| `time_limit_ticks` | `None` | Live ticks per match (per round with rounds on). |
| `on_time_up` | `{ "kind": "end" }` | `end`: the sole leader wins (`time_limit`), a tie ends with team 0. `draw`: a tie is a `draw`. `overtime` (`ticks` optional cap): a tie plays on until a sole leader; still tied at the cap is a `draw`. `sudden_death`: a tie plays on and the next point by any team wins, even a team that was behind. |
| `rounds` | `None` | `{ rounds_to_win, max_rounds, swap_sides_every, round_end_ticks }`. |
| `post_match_ticks` | `None` | Intermission after the match is decided, then the ready gate opens. `None` holds `post_match` until the host acts. |
| `forfeit_after_ticks` | `None` | A team that had a seat and has no seated, non-observer seat for this many ticks forfeits. |

The phases are `warmup`, `countdown`, `live`, `overtime`, `round_end` and `post_match`. The objective rules run in
`live` and `overtime` only. Each change emits `phase_changed` (`step`, `phase`, `round`). The tick that ends a hold
(warm-up, countdown, round end) does not score; play starts on the next tick.

**End conditions.** On each live tick, after the objective rules, the first condition that fires in this fixed order
decides it: a host-reported condition (`MatchSession::report_condition`, for a game, series or genre-kit rule), the
score limit (capture or hill), a forfeit, then the time limit. A later condition firing on the same tick is ignored.
`EndReason` gains `time_limit`, `draw`, `forfeit` and `rounds_won` and is now `#[non_exhaustive]`; `match_over` with
team 0 and reason `draw` is a draw.

**Rounds.** With `rounds` set, a condition that fires ends the round, not the match: a `round_over` event (`tick`,
`round`, `team`, `reason`) adds a round win (`round_wins`, by team id; team 0 adds none), and the board's own
`match_over` for that round is taken back. The match ends with `rounds_won` when a team reaches `rounds_to_win`, or
after `max_rounds` rounds on the most round wins (a tie is a `draw`). Otherwise `round_end` holds `round_end_ticks`,
then the next round starts: the scores are cleared and every flag goes home with its carrier dropped, but the
objectives stay placed and the round wins are kept. A host end (`ended`) and a forfeit always end the match.

**Sides.** Every `swap_sides_every` rounds the teams rotate sides: each present team (seated teams and the teams that
own a flag, in team order) takes the next one's side, so two teams swap. `PhaseState::side_of(team)` is the side a
team plays on; spawns and objective placement read it. At the first swap the session records each side's flag base
and moves every flag's base to its team's new side.

**Forfeits.** A team counts as in the match once it has had a seated, non-observer seat. Outside the warm-up and the
intermission, every tick it has none counts up; seating again resets it. At `forfeit_after_ticks` the team forfeits.
When at most one team is left, the match ends with `forfeit` (team 0 when none is left).

`rules.match_setup` takes `phases` (the same object; it replaces `settings.flow.phases`). Every method's answer adds
`phase`, `round`, `time_left_ticks` (`null` without a clock) and `round_wins`; the full state is `flow.phases`.

```json
{ "method": "rules.match_setup", "params": { "playlist": [{ "map_id": "harbor", "mode": "ctf" }], "start": true, "phases": { "warmup_ticks": 600, "countdown_ticks": 180, "time_limit_ticks": 18000, "on_time_up": { "kind": "overtime", "ticks": 3600 }, "rounds": { "rounds_to_win": 3, "max_rounds": 5, "swap_sides_every": 2, "round_end_ticks": 300 }, "post_match_ticks": 900 } } }
```

A host without a session (a sample with its own scores) drives the same machine: `PhaseState::start_match`, then per
tick `begin_tick` (it answers whether to play, reset, swap or open the ready gate), the game's own scoring, `end_tick`
with the team scores, and `enter_post_match` after ending the match. The arena sample does this.
## Multi-level maps and flag return

S-14 tests flag touches across XZ only. On stacked floors, a unit in a tunnel directly under a flag base can take,
return or capture the flag through the ceiling. A dropped flag also waits for a teammate's touch, so a flag dropped
where nobody can reach it stalls the match. Three `ObjectiveSettings` fields and one step variant cover this. Every
default is S-14's rule:

| Setting | Default | Effect |
|---|---|---|
| `touch_height` | `null` (XZ only) | A number `h` also requires `dy * dy <= h * h` between the unit and the flag (pickup, return) or the carrier and its own base (capture). |
| `flag_return_ticks` | `null` (never) | A dropped flag goes home at the end of the step on match tick `drop tick + flag_return_ticks` unless someone touched it first. A pickup clears the timer, and the next drop starts a new one. `0` returns it on the drop step unless a teammate re-picks it on that step. |
| `capture_needs_home_flag` | `true` | `false` scores a capture while the scoring team's own flag is away (carried or dropped). |

King-of-the-hill volumes are already 3-D boxes (`min`/`max` include a height band), so `touch_height` does not affect
them.

**Drop check.** `ObjectiveBoard::ctf_tick_with(tick, units, |point| -> DropCheck)` and
`MatchSession::step_with(units, |point| -> DropCheck)` ask the host about each flag lying on the ground. They ask
once per step, in board order, from the step the flag drops through every later step it still lies there. Answer
`DropCheck::Return` for a kill volume, out of bounds, inside collision or a sealed tunnel, and the flag goes home on
that step before anyone can touch it. `ctf_tick` and `step` always answer `Keep`, which is S-14's rule. Every peer
must return the same answer for the same point and step, so read map data, never local physics state.

The step order is: carriers sync (a dead or missing carrier drops the flag); the drop check runs; captures resolve;
pickups and returns resolve; then return timers expire.

An auto return emits `flag_auto_returned` with `tick`, `flag`, `flag_team` and `cause`. The cause is `timeout` or
`drop_check`. A lying flag shows its `dropped_tick` in `rules.match_get`. The key is absent while the flag is home or
carried. `RulesEvent` is now `#[non_exhaustive]`, so Rust callers that match it need a `_` arm.

**Agent surface.** `rules.match_setup` takes the three settings under `settings.objectives`. A malformed value, such
as a negative `flag_return_ticks`, answers `F0002`. Each `rules.match_step` unit takes an optional
`drop: "keep" | "return"` test hint. For `return`, the step's drop check returns a flag lying exactly at that unit's
position, which is where it lands when that unit dies carrying it. Any other `drop` value answers `F0002`.
`return_zones` (KI-GP-030, ADR-10070) takes up to 64 boxes `{ min: [x, y, z], max: [x, y, z] }` (inclusive): the
step's drop check returns any flag lying inside one, wherever it landed, so a test can stand in for a kill volume or
a sealed tunnel away from the carrier's last position. A box with min above max, a non-finite bound, a missing axis,
an unknown key or more than 64 boxes answers `F0002`. No new error codes.

```json
{ "method": "rules.match_setup", "params": { "settings": { "objectives": { "touch_height": 2.0, "flag_return_ticks": 1800 } }, "playlist": [{ "map_id": "tunnels", "mode": "ctf" }], "start": true } }
{ "method": "rules.match_step", "params": { "units": [{ "id": 7, "team": 1, "position": [20.0, -40.0, 0.0], "alive": false, "drop": "return" }] } }
{ "method": "rules.match_step", "params": { "return_zones": [{ "min": [10.0, -50.0, -5.0], "max": [30.0, -30.0, 5.0] }] } }
```

**Win conditions.** `MatchSession::report_win_condition(&rule, &view)` is the adapter from a `WinCondition` (the stock
`LivesOut` and `StructuresDestroyed`, the session's `win_rule`, or a game's own) to `report_condition` (KI-GP-030,
ADR-10070). When the rule ends the match it reports the outcome's winner with `Draw` (winner 0), `LastStanding` (rule
`lives`), `Destroyed` (rule `structures`) or `Ended` (any other rule), and returns the `Outcome`; otherwise, or once the
match is over, it returns `None` and changes nothing. Call it once per played step with the view every peer shares.

DO27 switch note (ADR-8506): the compat values are the defaults, `touch_height = None`, `flag_return_ticks = None`,
`capture_needs_home_flag = true`, plus `ctf_tick`/`step` with no drop check. The bits of S-14's 2000-step
capture-the-flag session are unchanged with those values (test `defaults_keep_the_s14_twin_run_bits`). For tunnel
and multi-floor maps we recommend a `touch_height` of about one floor's clearance (smaller than the floor-to-floor
height), a `flag_return_ticks` (for example 1800, thirty seconds at 60 Hz), and a drop check over the map's kill
volumes. That is a Stage B behavior change for DO27, and DO27 decides whether to adopt it.

## Teams, scores and friendly fire

A fighting team is a `TeamDef` on the session (`id`, `name_key`, `color` as four bytes, `max_seats`),
not a bare number. The default list is the four unnamed teams the score vector already counted
(`1` through `4`). Team `0` is neutral. `rules.match_get` returns `teams` (the definitions, the
switch policy, `rebalance_between_rounds`, `assist_window_ticks`), `ledgers` (one row per seat) and
`next_party`.

`rules.match_assign` seats a player or a party, or adds a named counter. The three forms exclude
each other. `{}` seats one player on the smallest team. A party is `group` (at most 64 objects;
the first is the request). Each object may set `team`, `slot`, `unit` and `from`. A counter is
`counter`: `{ team, slot, name, delta }` on a row that is already seated. The answer adds `seats`
(`{ team, slot }` for each player just seated). A counter answer has no `seats` key.

Automatic assignment walks the definitions in id order and picks the team with the fewest occupied
seats, then the lowest team score, then the lowest id. A party stays on one team when some team
has room for every member. An explicit `team` that cannot hold the party is `match_seat_bad` and
nobody moves. `from` names the seat being left. A team change follows `teams.switch`:

| Policy | When a seated player may change team |
|---|---|
| `never` | Never. This is the default. |
| `between_matches` | Only while the flow is between matches. |
| `when_unbalanced_by` | When the source team has at least `n` more seated players than the destination, counted before the move. |

A refused change is `match_team_switch_refused` and changes nothing. A player who is not seated yet
is not a switch. The same team is a slot move. `FlowInput::Seat` is unchanged and is not subject to
the switch. `rebalance_between_rounds` defaults to false. When it is true, a launch moves whole
parties off the fullest team until the counts differ by at most one, and it does not split a party.
It moves the smallest party first. It stops early when no party on the fullest team can move
without the destination ending at least as full as the source, so a large party can leave the
counts more than one apart (for example `(3, 1, 0, 0)` with a party of three on team 1).

A team change or a slot move (`from` set, or the launch rebalance) carries the player's ledger row,
its id, the pause budget (`pause_used`), a pending join ask (`joins`, with its step) and the pauser
seat to the new seat. A move never lands on a seat whose ledger row belongs to nobody seated (a
credit to an empty coordinate opened it): an automatic slot skips such a seat, and an explicit one
answers `match_seat_bad`. A pause budget left on the destination by a player who left is dropped.
Each move is appended to the session's `seat_moves` (`{ from, to, batch }`; one assign call is one
batch, so a swap is two moves of one batch). A launch clears the list before it rebalances.
`rules.match_assign` and `rules.match_step` then retag every entity whose `TeamId` and `LedgerSlot`
name a moved seat (`regia_framework::rules::retag_moved_seats`, which drains the list), so damage
after a move credits the moved row. A game that calls `assign_team` itself calls that function in
the same step.

Each seated row is a `PlayerLedger`: kills, deaths, assists, objective points, score, and up to 16
named counters. A new name past that cap, or an empty or longer-than-32-byte name, is
`match_stat_cap`. Score is an integer: one point per kill and one per objective credit. Assists do
not add score. A kill credits the killer and the victim, and each other contributor whose latest hit
is inside `assist_window_ticks` (300 match ticks by default). A hit exactly that many ticks earlier
still counts. The event kind is `player_scored` (`kill`, `death`, `assist` or `objective`).

A flag capture credits the carrier's ledger, and a hill point credits each bound living occupant,
once per unit id (a unit the host lists twice is credited once). Those credits do not write the team-score vector again. A unit with no ledger row gets no
credit and no `player_scored` event, so an unbound match keeps the previous team-score bits.

`FriendlyFire` is a framework resource, not a field of the rules core. `On` (the default) applies
teammate damage in full. `Off` ignores it. `Scaled` multiplies it. `Reflect` applies the scaled
amount to the source once. Self-damage is a separate switch and stays on unless the game turns it
off. Two entities are teammates only when both carry `TeamId` and the id is the same and not `0`.
`LedgerSlot` with `TeamId` is the seat damage credits. `sim_bridge::Team` stays a presentation
number; nothing copies it into `TeamId`.

Switch note (S-14 defaults): four unnamed teams, `switch` `never`, `rebalance_between_rounds`
false, assist window 300, friendly fire on, self-damage on. Explicit seat inputs and the team-score
counters are unchanged. Ledger rows appear when a seat is taken and disappear when it leaves, on
every peer that applies the same inputs.

Each ledger row has an `id`: `generation << 16 | team << 8 | slot`, where `generation` counts the
rows that coordinate has opened (`ledgers.generations`, from 1). It is never `0`, never reused in a
session, the same on every peer whatever order one step's seat inputs arrived in, kept across a
team change, and new after a leave and a rejoin. Seating a seat nobody held (`FlowInput::Seat` or
`rules.match_assign` without `from`) opens a zero row with a new id, even where a kill credit to
that empty coordinate had opened one; seating a seat that is already seated keeps its row. The score sheet's row id is the ledger row's id (a row from a
session saved before ids existed maps to `2^63 | team << 8 | slot`).

**Vacated coordinates (ADR-8881).** A coordinate a player left (`Leave`), or moved off (`rules.match_assign` with
`from`, the launch rebalance), is listed in the session's `vacated` until someone is seated there again. A credit to
a vacated coordinate books nothing and opens no ledger row: when a departed player's pawn still kills, the victim's
death is booked and the kill is not; when it dies, the killer's kill is booked and the death is not; a hit it landed
earlier gives no assist. A coordinate nobody ever held (a game's unseated bots) is credited as before.
`MatchSession::credits_seat(seat)` answers the rule. `vacated` is serialized only while it is not empty.

**Session shape (ADR-8881).** A serialized session (a `MatchRules` snapshot, a hot reload) starts with `version` (`MATCH_SESSION_VERSION`, now 1) and refuses keys this
build does not know. A session written before the key (v0.6) reads as version 1; version 0 or a newer version is
refused with a message that starts `match_session_version`. The `rules.match_*` answers leave `version` and `vacated` out, so
their shape is the one their schemas publish.

`MatchRules` (as one opaque value), `FriendlyFire` and `SelfDamage` are reflected resources, so hot
reload keeps them and `state_hash::insert_resource_text` with `"MatchRules"`, `"FriendlyFire"` or
`"SelfDamage"` adds them to a world snapshot. The `MatchRules` text covers the whole session, the
respawn book (`respawns`) and the spawn picker's generator, cursor and recent uses (`spawn`) included
(ADR-8704). `world_snapshot` on its own leaves every resource out, and so does `sim.state_hash`: a
determinism check that must cover the match adds the entry with `insert_resource_text`.

## Proof

`match_rules_ctf_koth_3600` is a deterministic golden (ADR-8503, ADR-8046). It plays 3600 lockstep
steps: three seats, a ready-up, a capture-the-flag raid with two deaths, a host pause, then a
king-of-the-hill match. The harness hashes the session every 60 steps (60 hashes). The golden is
`crates/regia_detsim/goldens/match_rules_ctf_koth_3600.detgold`, recorded on `x86_64-linux`.

The runners (`cargo xtask det-golden`, ADR-8081) check it with the other clean scenarios:

| Leg | Target | When |
|---|---|---|
| host | `x86_64-linux` | the reference recording |
| qemu | `aarch64-android` | the gate's cross-architecture check |
| android | `aarch64-android` | release candidate, on the test phone |
| mac | `aarch64-macos` | release candidate |
| windows | `x86_64-windows` | release candidate |
| deck | `x86_64-linux` | release candidate |

On the Pixel 9 Pro the same scenario hashed `b41b84ff4f19669b` (`aarch64-android`), equal to the
`x86_64-linux` reference (`agrees_with_reference` true). The golden has no `hash.aarch64-android`
line yet. The lead records that line. `qemu-aarch64` was not on PATH, so the qemu leg did not run.

`input_permutations_within_a_step_never_change_state` runs 1000 seeded cases. Permuting one step's
seats, or one step's readies, leaves the session unchanged, because the ledgers stay sorted.
Permuting contestants and then sorting with `objective::host_unit_order` (entity id, then team)
leaves the session unchanged. A pickup still goes to the first unit in that order.

Fuzz targets `match_flow_input_json` and `rules_match_params` read the JSON shapes on this page.
`cargo test -p regia_agent --test fuzz_targets match_flow_input_json_and_rules_match_params_survive_10k`
runs 10 000 seeded mutations of each.

## Cost

`cargo run --release -p regia_rules --example match_rules_cost` times one step at 8, 64, 512 and
4096 contestants and 2, 8 and 32 objectives, for capture the flag and king of the hill. King of the
hill places one hill; the objective axis is the requested count. Zones are not a rules-core
objective. The example writes JSON. The 0.05 ms budget at 64 contestants and 8 objectives on the
N150 laptop is a number to record, not a test failure. The rows below are release builds (64 timed
steps after 8 warmup). On that budget point the N150 measured 0.000991 ms (capture the flag) and
0.000133 ms (king of the hill).

| Host | Load | 64 contestants, 8 objectives, capture the flag | king of the hill |
|---|---|---|---|
| x86_64 Linux CPU host | 324.04 346.93 360.19 | 0.001400 ms | 0.000186 ms |
| N150 laptop | 23% processor, then 7% | 0.000991 ms | 0.000133 ms |
| Steam Deck | not measured | not measured | not measured |
| Pixel 9 Pro | 0.19 0.24 0.27 | 0.012348 ms | 0.000438 ms |

The Linux load and the Pixel 9 Pro load are the one-minute, five-minute and fifteen-minute averages
from `/proc/loadavg` while the example ran. The Linux host was busy. The N150 has no `/proc/loadavg`
(the example records `not measured` there); the percentages are processor load sampled just before
and just after the run. The Steam Deck did not answer
(`ssh -o BatchMode=yes -o ConnectTimeout=12 <host>`, no route). The Pixel 9 Pro row is a
release build of the same example (`cargo ndk -t arm64-v8a build -p regia_rules --example
match_rules_cost --release`), run on the test phone. At the budget grid point it measured 0.012348 ms
(capture the flag) and 0.000438 ms (king of the hill). The 0.05 ms figure stays the N150 recording
target.

Release milliseconds per step, capture the flag. Columns are requested objectives. Placed counts
matched the request (one flag per team).

| Host | 8 x 2 | 8 x 8 | 8 x 32 | 64 x 2 | 64 x 8 | 64 x 32 | 512 x 2 | 512 x 8 | 512 x 32 | 4096 x 2 | 4096 x 8 | 4096 x 32 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86_64 Linux CPU host | 0.000206 | 0.000331 | 0.000783 | 0.000737 | 0.001400 | 0.004822 | 0.004166 | 0.006458 | 0.008110 | 0.030051 | 0.040101 | 0.041103 |
| N150 laptop | 0.000320 | 0.000378 | 0.000620 | 0.000750 | 0.000991 | 0.002528 | 0.002786 | 0.004094 | 0.005455 | 0.017797 | 0.023936 | 0.045105 |
| Pixel 9 Pro | 0.001590 | 0.002361 | 0.003069 | 0.012089 | 0.012348 | 0.018001 | 0.054366 | 0.028534 | 0.027235 | 0.080675 | 0.083965 | 0.080987 |

Release milliseconds per step, king of the hill. Each cell placed one hill.

| Host | 8 x 2 | 8 x 8 | 8 x 32 | 64 x 2 | 64 x 8 | 64 x 32 | 512 x 2 | 512 x 8 | 512 x 32 | 4096 x 2 | 4096 x 8 | 4096 x 32 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86_64 Linux CPU host | 0.000061 | 0.000079 | 0.000083 | 0.000166 | 0.000186 | 0.000142 | 0.000893 | 0.000915 | 0.000923 | 0.007138 | 0.007206 | 0.006379 |
| N150 laptop | 0.000083 | 0.000081 | 0.000092 | 0.000136 | 0.000133 | 0.000133 | 0.000573 | 0.000573 | 0.000573 | 0.004072 | 0.004075 | 0.006689 |
| Pixel 9 Pro | 0.001699 | 0.000615 | 0.000565 | 0.000516 | 0.000438 | 0.000468 | 0.002108 | 0.002710 | 0.001921 | 0.017344 | 0.010084 | 0.009533 |

## Pending events

`ObjectiveSettings::max_pending_events` defaults to 4096 (a stored `0` means the same). The board
and the flow each keep that many pending events. Past the cap, the oldest event is dropped and
`events_dropped` increases. `rules.match_get` includes both counters (`board.events_dropped` and
`flow.events_dropped`). A host that drains every step, which `MatchSession::step` does, never
drops, so the counters stay 0 and the S-14 session bits are unchanged.

DO27 switch note (ADR-8503): leave `max_pending_events` at 4096. Keep draining every step. No
other setting default changes.

## Respawn

`SessionSettings.respawn` (ADR-8647) holds `default` (a `RespawnPolicy`), `teams` (per-team rows
`{ team, policy }`), `spawn` (the spawn-point policy, see `book/agents/runtime-gameplay.md`) and `seed`
(the random spawn stream). `rules.match_setup` accepts it under `settings.respawn`, for example
`{ "default": { "kind": "waves", "period": 300 }, "teams": [{ "team": 2, "policy": { "kind": "lives", "lives": 3 } }] }`.

| Policy | A seat that dies at tick `t` |
|---|---|
| `instant` (default) | comes back on the next step |
| `delay { ticks }` | comes back at `t + ticks` |
| `waves { period }` | comes back on the next multiple of `period` after `t`, with everyone else waiting for that wave |
| `lives { lives }` | comes back at once while lives remain; the death that uses the last life eliminates it |
| `never` | is eliminated |

The host reports deaths with `MatchSession::note_death(seat)`; the framework's damage step does it for a
pawn with `TeamId` and `LedgerSlot` when `MatchRules` exists. Nothing is booked until a death is
reported, so a session that never reports one sends the S-14 event stream unchanged. Each step releases
the seats that are due, in (due tick, seat) order, with a `respawn_at { tick, team, slot }` event; an
elimination sends `eliminated { tick, team, slot }`. A launch and a new round clear the book (lives refill).
The book belongs to the player, not the seat coordinate (ADR-8704). `FlowInput::Leave` drops the seat's
pending release, lives and elimination, and so does seating a seat nobody held (`FlowInput::Seat`, or
`rules.match_assign` without `from`). A team change or slot move (`assign_team`, the launch rebalance)
carries the rows to the new seat in the same step: the due tick stays, lives stay, and an eliminated
player stays eliminated.
`rules.spawn_preview` shows the settings (`respawn`) and the book (`respawns`: `pending`, `lives`,
`eliminated`). Arithmetic is integer and the book is sorted vectors, so every peer holds the same book.

## Capture zones

`regia_rules::zones` (ADR-8649) adds capture zones: control volumes with integer capture progress, an owner, decay,
a contest rule by numbers and owner scoring. Any number of zones may sit on one board; each counts against
`max_objectives`. A zone is an objective of kind `zone`. Its `zone` record on the board holds `settings`, `owner`
(`0` for neutral), `capturing_team`, `progress`, `locked`, `held_ticks` and `empty_ticks`, so `rules.match_get` shows
the progress of every zone. King of the hill is unchanged: one hill, sampled once per accrual period.

`rules.match_objective` takes two more forms (still exactly one per call):

| Param | Fields |
|---|---|
| `zone` | `min`, `max` (the box, with a height band), `id`, `capture_ticks` (600), `decay_ticks` (none), `contest` (`any_enemy_blocks`, `majority` or `weighted`), `max_rate_units` (1), `owner_scores_every` (60), `owner_scores` (true; false scores nothing for holding), `locked` (false). Answers `placed: [id]`. |
| `zone_rules` | `mode`: `domination`, `hardpoint` (`rotation`, `rotate_ticks`) or `attack_defend` (`attackers`, `defenders`, `order`); `score_limit` (none). Answers `placed: []`. |

Every zone runs on every match tick, in ascending entity id. The rules count the living units of each fighting team
inside the box. The host's unit order does not change the count. A locked zone does nothing. Otherwise:

- **Empty.** With `decay_ticks` set, partial progress loses one unit every `decay_ticks` empty ticks. At zero the zone
  has no capturing team again. An owned zone keeps its owner.
- **Contested.** Nothing moves.
- **Pushed** by one team at a rate of its unit count, capped at `max_rate_units`. If the team owns the zone, another
  team's partial progress unwinds. If another team has partial progress, that progress unwinds first, with no
  carry-over on that tick. Otherwise progress grows. When it reaches `capture_ticks` the zone flips: the team owns it,
  progress returns to zero, and a `zone_captured` event carries `zone`, `team` and `previous` (`0` for neutral).

The contest rule decides who pushes when more than one team is inside:

| `contest` | Who pushes |
|---|---|
| `any_enemy_blocks` | Nobody. Any enemy freezes the zone (default). |
| `majority` | The team with strictly more units than every other team, at its own count. A tie for the most freezes it. |
| `weighted` | The largest team, at its count minus every other team's units inside. Zero or less freezes it. |

An owned, unlocked zone counts one held tick per tick, from the tick it flipped. Every `owner_scores_every` held ticks
its owner gains one team point (`zone_point`). In attack/defend the defenders own the active zone from the start, so
turn `owner_scores` off there unless holding should score. `score_limit` ends the match on that point with reason
`zone_score_limit_reached`. A `zone_captured` credits each bound living occupant of the capturing team once in its
ledger.

The modes:

- **Domination** (the default): every zone as installed. Setting it restores each zone's installed `locked` flag.
- **Hardpoint**: `rotation[0]` is unlocked and the other listed zones are locked. After `rotate_ticks` zone ticks the
  next zone of the list unlocks (it wraps), sending `zone_activated`, and the old one locks and returns to neutral.
- **Attack/defend**: every zone of `order` starts owned by `defenders`, and only the first is unlocked. When the
  attackers take the active zone it stays theirs and locks, and the next unlocks (`zone_activated`). Taking the last
  ends the match for the attackers with reason `zones_captured`. With a phase time limit the defenders win at
  time-up: the phase clock reads them as the sole leader.

A playlist row may use mode `dom` (or `domination`). Hardpoint and attack/defend are zone rules on a `dom` or
`custom` row. A launch clears the board, the zones and the zone rules. A round start (rounds on) returns every zone to
neutral and restarts the mode.

Lobby id `dom` and `MatchMode::Domination` are new. `MatchMode` is not `#[non_exhaustive]`, so a host that matches
it exhaustively needs a `Domination` arm. `ObjectiveKind` is `#[non_exhaustive]` now: match it with a wildcard arm.

| Code | When |
|---|---|
| `match_zone_settings` | `zone_rules`: a zero `rotate_ticks`, an empty `rotation` or `order`, equal `attackers` and `defenders`, or a team outside the fighting teams. |
| `match_zone_unknown` | `zone_rules` names an id that is not a capture zone. |

A malformed `zone` or `zone_rules` (a missing box corner, an unknown `contest` or `mode`, a hardpoint without
`rotation` or `rotate_ticks`) is `F0002`.

```json
{ "method": "rules.match_objective", "params": { "zone": { "id": 1, "min": [-5, -1, -5], "max": [5, 9, 5], "capture_ticks": 300, "contest": "majority" } } }
{ "method": "rules.match_objective", "params": { "zone_rules": { "mode": "hardpoint", "rotation": [1, 2, 3], "rotate_ticks": 3600, "score_limit": 250 } } }
```

Switch note (S-14 defaults): no zone exists until the host installs one, so a board without zones serializes and
steps exactly as before (no `zone` or `zone_rules` key). King of the hill keeps its sampled rule and its one-hill
limit; the S-14 KOTH and CTF tests pass unchanged.

## Rule sets

A rule set is a match mode as data (ADR-8694): a `.rules.ron` file (format `rules-ron` v1,
[`docs/formats/rules-ron.md`](../../docs/formats/rules-ron.md)) that names a skeleton (`tdm`, `ffa`, `ctf`,
`koth`, `dom` or `custom`), may `extends` another rule set, and overrides any session setting field by field. It
also lists its teams, its win conditions and a typed option schema that playlist rows fill in. A designer or a mod
makes a mode without code.

The engine ships `tdm`, `ffa`, `ctf`, `koth` and `dom`. Each one writes out the Rust defaults and resolves to them
bit for bit. A project adds files under `assets/rules/`. A project file with a shipped id replaces the default; two
project files with one id are `ruleset_duplicate`. `RuleSetRegistry` (a framework resource that
`MatchRulesPlugin` inserts) holds them. A host loads the project with `RuleSetRegistry::load_project` before the
match.

Resolution walks the `extends` chain from the root down, over the Rust defaults. An absent field keeps the parent's
value. A rule set may have at most 4 ancestors. The result is plain settings for `MatchSession::new`, the
skeleton mode, the board's zone rules, the win conditions and the option values. It happens before the match,
never inside a step. Every float is read once, straight to `f32`. A row option of kind `float` is parsed with
`str::parse::<f32>`.

A playlist row names a rule set by id in its `mode`. A row whose `mode` names no rule set but is a skeleton id
runs that skeleton's Rust defaults and its options pass through unchecked, exactly as before. Row options are
checked against the schema. `min_players` and `max_players` belong to the playlist and always pass.
`MatchSession::rule_modes` maps each rule-set id the rows name to its skeleton, so the launch resets the board for
the right mode. A session without rule sets leaves the map empty and does not serialize it.

`ResolvedRules::content_hash` is FNV-1a 64 over the canonical bytes of the mode, the settings, the zone rules, the
win conditions and the option values. The id and the name are not part of it, so two ids with the same rules hash
the same. Each `f32` is its bit pattern. `NetSession::set_rules_hash` puts the hash in the content manifest as the
pack `@rules`, so peers whose rules differ are refused with `content_mismatch` (`pack:@rules: different`) on the
existing handshake path. For a playlist, `regia_rules::ruleset::combined_hash` hashes every rule set its rows
name.

| Method | Params | Answer |
|---|---|---|
| `rules.ruleset_list` | none | `rulesets`: `id`, `name_key`, the resolved `skeleton`, `extends`, `builtin`, `path`, the option keys, `content_hash` (16 hex digits) or `error`; `errors`: files that did not load (`file`, `code`, `path`, `message`) |
| `rules.ruleset_get` | `id`, `options?` | `ruleset` (as written, `null` for a bare skeleton id) and `resolved`: `id`, `name_key`, `chain`, `mode`, `settings`, `zones`, `win_conditions`, `schema`, `options`, `content_hash` |
| `rules.ruleset_validate` | `text`, `options?`, `load?` | `ok`, `id`, `loaded`, `resolved`. Resolves against the loaded rule sets. `load: true` also adds the text to the registry. |
| `rules.match_setup` | `ruleset`, `options?` (plus the other setup params) | The session answer plus `ruleset` (the resolved rules). `ruleset` excludes `settings` and `phases`. |

Options are an object of strings, the playlist row's shape. A number is refused with `F0002`, so a float never
passes through JSON's `f64`. Every rule-set error carries the field path in `data.path`, and the code in `data.code`
and `data.reason`.

| Code | When |
|---|---|
| `ruleset_parse` | The text is not RON of the rule-set shape, or it has an unknown field. |
| `ruleset_version` | `format_version` is not 1. The version is read before the rest of the file. |
| `ruleset_too_large` | The text is larger than 256 KiB. |
| `ruleset_invalid` | A value cannot run: a bad id, an unknown skeleton, a bad option schema or win condition, a non-finite float, a phase or zone setting out of range, or a genre section that cannot run (`data.path` is indexed: `session.waves.waves[0].groups[1].count`, `session.victory.conditions[2].ticks`). |
| `ruleset_duplicate` | Two project files declare one id. |
| `ruleset_unknown` | No rule set or skeleton has the id, or `extends` names a parent that is not loaded. |
| `ruleset_cycle` | The `extends` chain comes back to a rule set already on it. |
| `ruleset_depth` | The chain has more than 4 ancestors. |
| `ruleset_option_unknown` | A row sets an option the schema does not declare (`data.path` is `options.<key>`). |
| `ruleset_option_range` | A value or a schema default is outside `min`..`max`, or longer than 256 bytes. |
| `ruleset_option_kind` | A value does not parse as its kind, or an enum value is not one of its choices. |

```json
{ "method": "rules.ruleset_validate", "params": { "text": "(format_version: 1, id: \"ctf_long\", extends: \"ctf\", session: (objectives: (capture_limit: 5)))" } }
{ "method": "rules.match_setup", "params": { "ruleset": "ctf_long", "options": { "sky": "night" }, "playlist": [{ "map_id": "harbor", "mode": "ctf_long" }], "between": true } }
```

`regia rules check <file>...` parses, checks and resolves files from the command line (against the defaults, the
project's `assets/rules/` and the other named files) and prints each error with its code and path. `regia
upgrade-project` knows `.rules.ron`. The fuzz target `ruleset_ron` runs the reader, the check and the resolution on
mutated text.

The genre sections (ADR-8748) are `session.waves`, `session.race`, `session.zone`, `session.elimination` and
`session.victory`, with the session settings' field names and `kind` words (`(kind: "at_tick", tick: 600)`,
`(kind: "hold_for", objective: 3, ticks: 6000)`). Unknown fields in them are refused as elsewhere. `zone: Some(None)`
turns a parent's zone off; a zone patch over a parent without one starts from the default schedule. Lists replace
the parent's list whole. `rules.ruleset_get` and `rules.ruleset_validate` show the resolved sections under
`resolved.settings`, and `rules.match_setup { ruleset }` starts the session with them. Examples for each mode are
in [Game modes](../user-guide/game-modes.md#co-op-race-battle-royale-and-strategy-modes).

An option sets a session field when it names one in `bind` (ADR-8881): `(key: "caps", kind: int, default: "5",
min: "1", max: "10", bind: "objectives.capture_limit")`. The value (the row's, else the default) replaces the rule
set's own value before the settings are checked, so the content hash covers it and a playlist row changes the match
without game code. On an optional field (`phases.warmup_ticks`, `phases.time_limit_ticks`, `phases.post_match_ticks`,
`phases.forfeit_after_ticks`, `phases.warmup_min_seats`, `objectives.flag_return_ticks`, `zones.score_limit`) 0 is
off. The bindable fields and their option kinds (`regia_rules::ruleset::OPTION_BINDS`):

| `bind` | Kind |
|---|---|
| `flow.start_delay_ticks` | `int` |
| `phases.warmup_ticks`, `phases.warmup_min_seats`, `phases.countdown_ticks`, `phases.time_limit_ticks`, `phases.post_match_ticks`, `phases.forfeit_after_ticks` | `int` |
| `objectives.flag_touch_radius` | `float` |
| `objectives.flag_return_ticks`, `objectives.capture_limit`, `objectives.hill_score_limit`, `objectives.hill_accrual_ticks` | `int` |
| `objectives.capture_needs_home_flag` | `bool` |
| `teams.rebalance_between_rounds` | `bool` |
| `teams.assist_window_ticks`, `respawn.seed`, `zones.score_limit`, `race.laps`, `victory.surrender.window_ticks` | `int` |

A field outside the table, an option of another kind, or two options binding one field is `ruleset_invalid` at
`options[<n>].bind`. A value the field cannot hold (negative, or too large for it) is `ruleset_option_range` at
`options.<key>`. An option without `bind` sets nothing; a game reads it (`ResolvedRules::option`). Win conditions use
the stock rules (`lives`, `structures`); the session takes the first one as its win rule. Hardpoint and
attack/defend zone rules name zone ids, which are map data, so the host sets them again after it installs the zones.

Switch note (S-14 defaults): nothing changes until a host loads a rule set. A bare skeleton id in a row still
parses with `MatchMode::from_lobby_str` and falls back to `custom`, and an empty `rule_modes` keeps the session's
shape and bits. DO27-compat value: no rule set, `rule_modes` empty.

## Using the core from ECS

A lockstep game steps its own `MatchSession` on every peer and never needs this section. A
server-authoritative game (one host decides, clients replicate) can let the framework do the glue
(ADR-8646):

- `MatchSessionPlugin` steps the `MatchRules` session once per fixed tick, in its own
  `FixedUpdate` set `MatchRulesSet`, which the plugin orders after `DamageSet` (ADR-8723): a pawn
  killed this tick does not touch a flag this tick, whichever plugin was added first. A game that
  orders `MatchRulesSet` before `DamageSet` gets a schedule cycle, which bevy refuses. Order your
  own death handling before the set too. Nothing else in the fixed step moves: an app without the
  plugin keeps its schedule.
- Contestants are the entities with `TeamId`, `Transform` and `Health`. A contestant's id is its
  entity index, its position is the transform's translation, and it is alive while its health is
  above zero (a NaN health is not alive). With `LedgerSlot` too, the adapter binds the unit to its
  seat's ledger row, so a capture or a hill point credits that seat. Only a seat the session holds
  binds.
- Seat moves (`rules.match_assign` with `from`, a launch rebalance) reach the ECS side in the next
  fixed step: the pawn's `TeamId` and `LedgerSlot` and the player's seat in `MatchSessionAdapter`
  follow the move. A `Leave` sent to the session directly (`rules.match_input`) releases the seat
  in the next step, as `on_player_left` does at once.
- `MatchRules`, `MatchSessionAdapter`, `SpawnOrder<TeamId>` and `DeathCredited` are reflected and
  registered, so `state_hash::insert_resource_text` (with `"MatchRules"`, `"MatchSessionAdapter"`,
  `"SpawnOrder<TeamId>"`) and the entity snapshot carry everything the fixed step reads.
- Contestant order is the order `TeamId` arrived in the world (`SpawnOrder<TeamId>`, tracked from
  the moment the plugin is added), then the entity index. It is never the order archetypes are
  stored in, so inserting the same components in another order gives the same board. Add the
  plugin before any entity holds `TeamId`; tracking cannot start later.
- An entity tagged `ObjectiveEntity(id)` gets the position of the board's objective `id` (a flag
  or the hill) after each step. A carried flag's entity follows its carrier.
- Each `RulesEvent` of the step is sent as the ECS message `MatchEvent`, in the session's order.

`MatchSessionRules` is a `GameRules` implementation over the same session. Install it in
`GameRulesResource`, or name it in a level (`MatchSessionRules`; the plugin registers it when the
rules registry exists).

| Hook | What the adapter does |
|---|---|
| `on_player_joined` | Seats the player (`assign_team`; a `TeamId` on the player entity asks for that team, otherwise fewest seats, then lowest team score, then lowest id). The join flow tags the pawn it spawns, or the pawn a bot takeover keeps, with `claim_pawn_seat(world, player, pawn)` (`TeamId` and `LedgerSlot`; keyed by the player, so a join that spawns nothing tags no other pawn). Returns a spawn request at the default spawn point with `team: Some(<rules team id>)`. The pawn the join flow spawns gets `TeamId` and `LedgerSlot`. A refused seat returns no spawn: the player watches. |
| `on_player_left` | Sends `FlowInput::Leave` for the player's seat. The seat and its ledger row go, and every entity that wore the seat loses `LedgerSlot` (it keeps `TeamId`), so a pawn left behind credits no row. |
| `on_character_died` | Records a death on the character's seat. A kill the damage step already credited (both seats known) carries `DeathCredited`; the adapter removes the marker and records nothing more. |
| `is_match_over` | The board's `match_over`, or the post-match phase. |
| `scoreboard` | One row per seat from the ledger (score, kills, deaths, assists, objective). Each team heading is followed by the team's board score (`Team 2: 3`, or `<name>: 3`); a `loc:` heading is left as it is. |

`rules.get` answers `match_session: true` while the plugin is installed. `ui.scoreboard` on a board
with `from_rules` shows the adapter's sheet. No new methods and no new error codes: the session's
own methods (`rules.match_get`, `rules.match_assign`, ...) still read and drive it.

```json
rules.get {}
→ { "rules": "regia_framework::rules::session_adapter::MatchSessionRules", "match_over": false,
    "level_rules": null, "match_session": true }
```

The arena sample (`samples/arena`) runs its match this way: one team per player slot, the warm-up
and timer as phase settings, the score limit reported with `report_condition`, and `ArenaState`
kept only as the view its HUD and `play.expect` read.

## Replicated match state

A client/server game shows the server's match on its clients without running any rules there
(ADR-8681). Add `MatchStateReplicationPlugin` on every process, after `NetPlugin`, at the same
point of the registration order. It registers, in this order, `TeamId`, `MatchStateEntity`,
`MatchPhaseView`, `TeamScoresView`, `ObjectiveViews`, `PlayerScoreView` and the message
`MatchEventMsg`, so the protocol hash moves once. A lockstep game does not use it: every peer
steps its own session.

On a server with `MatchRules` (stepped by `MatchSessionPlugin` or by the game), the plugin spawns
one `MatchStateEntity`. That entity is replicated and marked `NetPersistAcrossTravel`, so a level
change keeps it. Each fixed tick, after `MatchRulesSet` and before the replication gather, it
writes these components, and only the ones that changed:

| Component | Holds |
|---|---|
| `MatchPhaseView` | phase, round, `match_tick`, `time_left_ticks`, `paused`, `pauser` seat, `paused_by`, `between`, `starts_at`, `flow_step`, `match_over`, `winner`, `end_reason`, `net_tick` |
| `TeamScoresView` | team scores and round wins by team id (at most `max_teams + 1` entries), team labels (id, name key, color) |
| `ObjectiveViews` | each flag, the hill and each capture zone: id, kind, team (a zone's owner), state (`at_base`, `carried` with the carrier and its seat, `dropped`, `hill`, `zone` with the owner, the capturing team and the lock; ADR-8731), position, `capture_progress` (thousandths of a dropped flag's return timer or of a zone's capture bar, in steps of 100) |
| `PlayerScoreView` | on each replicated entity with `TeamId` and `LedgerSlot`: its seat's ledger row (row id, kills, deaths, assists, objective points, score) |

A carried flag's view keeps its base position, so the list does not change while the carrier runs.
`objective_position` finds the flag on a client: the transform of the entity whose
`PlayerScoreView` names the carrier's seat. The seat is there when the carrier's ledger row is bound
to its unit (`MatchSessionPlugin` binds every pawn with `LedgerSlot`).

The clock fields are rewritten every `MatchStateSettings::clock_resync_ticks` (default 30 fixed
ticks) and at once on any other change (a pause, a phase change). Between two writes a client reads
`time_left_at(tick)` and `match_tick_at(tick)` with its newest applied server tick. They count down
only while the phase is live or overtime and the match is running.

Each `MatchEvent` the server sends also goes to every client as a reliable, ordered `MatchEventMsg`.
A client writes it as the same `MatchEvent` message, so a kill feed or an announcer reads one type
on a dedicated server, a listen server and a client. The payload is a format byte, a kind byte and
the fields. It is capped at `MatchStateSettings::max_event_bytes` (256 by default). Strings in an
event are cut to 128 bytes. A payload over the cap is dropped and counted in
`MatchStateCounters::events_oversized`. An unknown format or kind, a bad field or trailing bytes are
dropped and counted in `events_malformed`. A bad payload never panics. Every core event has a kind
byte, the respawn, capture-zone, co-op, race, shrinking-zone and victory events included (ADR-8731),
and every end reason has a code. `zone_damage` is the exception: it is the server's per-unit damage
instruction, its effect reaches clients through replicated health, and it is never sent.

Install `ReplicatedMatchRules` as a client's game rules. Its `scoreboard` builds the same sheet as
`match_session_score_sheet` on the server, from the views. `is_match_over` reads `MatchPhaseView`.
It has no rules logic.

Late join: the views arrive with the joiner's first snapshot. Events sent before the join are not
replayed. A client may see an event a snapshot before the view it changed, because messages and
snapshots travel separately.

`rules.match_get` on a client with no session answers from the views:

```json
rules.match_get {}
→ { "source": "replicated", "phase": "live", "round": 0, "match_tick": 1840,
    "time_left_ticks": 4160, "paused": false, "paused_by": "", "pauser": null,
    "between": false, "starts_at": null, "match_over": false, "winner": 0, "end_reason": "",
    "team_scores": [0, 1, 0, 0, 0], "round_wins": [], "teams": [ { "id": 1, "name": "", "color": [255, 255, 255, 255] } ],
    "objectives": [ { "id": 901, "kind": "flag", "team": 2, "state": "carried", "carrier": 7,
                      "carrier_seat": { "team": 1, "slot": 0 }, "position": [20.0, 0.0, 0.0], "capture_progress": 0 } ],
    "players": [ { "team": 1, "slot": 0, "row_id": 65793, "kills": 2, "deaths": 0, "assists": 0,
                   "objective_points": 1, "score": 3, "local": true } ],
    "replication": { "events_received": 12, "events_oversized": 0, "events_malformed": 0 } }
```

`match_tick` and `time_left_ticks` there are read at the client's newest server tick. With neither a
session nor a match-state entity the answer is still `match_rules_unavailable`. No new methods and
no new error codes.

Bandwidth: `cargo run --release -p regia_framework --features net --example match_state_bandwidth`
prints bytes per second per client for 64 players, 8 teams and 8 flags, with 30 Hz snapshots of a
60 Hz match. The numbers are in `docs/evidence/match-state-replication/bandwidth.json`: about 90
bytes per second per client with the default clock resync, 391 with a resync every tick (payload
only, framing not counted). They are a record, not a test bound.

## Co-op waves

`SessionSettings.waves` (ADR-8695, `regia_rules::waves`) turns a session into a co-op PvE match. An
empty `waves` table (the default) is off: the director never runs and the S-14 event stream is
unchanged. `rules.match_setup` accepts it under `settings.waves`, for example
`{ "waves": [{ "groups": [{ "archetype": "grunt", "count": 6, "spawn_tag": "east" }], "alive_cap": 4 }, { "at": { "kind": "after_cleared", "delay_ticks": 300 }, "groups": [{ "archetype": "boss", "count": 1, "spawn_tag": "arena" }] }], "shared_lives": 5 }`.

| Field | Meaning |
|---|---|
| `waves[].at` | `{ "kind": "after_cleared", "delay_ticks" }` (the default, delay 0): that many ticks after the previous wave is cleared; the first wave counts from the first step of the match. `{ "kind": "at_tick", "tick" }`: on the first step at or after that match tick, whether earlier waves are cleared or not. Waves start in table order: a row never starts before the row above it. |
| `waves[].groups[]` | `{ archetype, count, spawn_tag }`. The archetype is the game's id; the core never names a prefab. |
| `waves[].alive_cap` | Most enemies alive at once, over every wave, while this wave releases. `0` is no cap. |
| `difficulty[]` | `{ players, count_percent, cap_percent }`, ascending by `players`. The row with the largest `players` not above the seated count on the players' team scales each group count and the alive cap when a wave starts (rounded down, at least 1). No row is 100 %. |
| `players_team` | The players' team (default 1). Enemies hold no seat. |
| `shared_lives` | `null` (default) is off. A number is a pool the players' team shares: a death spends one and respawns by the team's respawn policy; a death with the pool empty eliminates the seat. |
| `fail_on_team_wipe` | Default true: the match fails when every seated player on the players' team is down at once (eliminated, or waiting for a respawn due after this tick; an instant respawn is never down). |
| `success` | `last_wave_cleared` (default), `objective_reached` or `either`. |

Each step the match plays, after the objectives, the director starts every wave that is due, then
releases groups in table order: a group goes out whole when the cap allows, otherwise as many as fit,
and a later group never jumps a held one. Each release is a `wave_spawn { tick, wave, group,
archetype, count, spawn_tag }` event. The game spawns that many enemies, tags each with the wave
index, and calls `MatchSession::note_wave_down(wave, count)` when they die. The alive count is
released minus reported, so it does not depend on when the game's entities appear. A wave whose
groups are all out and whose alive count is 0 sends `wave_cleared { tick, wave }`; a started wave
sends `wave_started { tick, wave }`.

Then the conditions, failures first: an objective reported destroyed
(`MatchSession::note_coop_objective(CoopObjective::Destroyed)`), then a team wipe, then success (every
wave cleared, or `CoopObjective::Reached`). The director sends `coop_ended { tick, won, cause }`
(`all_waves_cleared`, `objective_reached`, `team_wipe`, `objective_destroyed`) and the session ends
the match: `match_over` with the players' team on a win and team `0` on a failure, reason `ended`.
With a phase setting on, the end lands on the next step through the end-condition order, and a time
limit ends a co-op match the phase machine's way (`time_limit`, team `0` with no score), which a game
treats as a failure. The director waits during the warm-up, countdown and round-end holds; a launch
and a new round clear it and refill the pool. `WavesWin` wraps the result as a `WinCondition` (rule
word `waves`).

`rules.match_get` shows `waves` when the table is on: `settings`, `state` (`opened_at`, `progress`:
each started wave's `planned`, `released` and `alive` per group, `alive_cap`, `cleared_at`;
`lives_left`, `objective_destroyed`, `objective_reached`, `ended`) and the totals `alive`, `cleared`
and `total`. Settings the director cannot play answer `match_wave_settings` with the check in
`data.message`: `empty_wave`, `zero_count`, `archetype_length` (1 to 64 bytes), `spawn_tag_length`
(at most 64), `too_many_waves` (256), `too_many_groups` (64), `players_team_zero`,
`difficulty_not_ascending`, `difficulty_percent` (1 to 1000), `too_many_difficulty_rows` (64).

Switch note: waves are off by default; DO27 has no co-op mode and keeps `waves` empty.

## Races

`SessionSettings.race` (ADR-8695, `regia_rules::race`) is a checkpoint course. No checkpoints (the
default) is off.

| Field | Meaning |
|---|---|
| `checkpoints[]` | `{ id, volume: { min, max }, next }`. The first is the start and finish line. `next` lists the ids that may follow (more than one is a branch); a lap ends on returning to the start. |
| `laps` | Laps to finish (default 1, at most 1000). |
| `finish_window_ticks` | Once the first racer finishes, every racer still running this many ticks later is DNF. `0` (default) waits for everyone. |
| `wrong_way_ticks` | Consecutive steps losing ground on the segment before a `racer_wrong_way` event (default 30; `0` is off). |

A racer is a seat and a unit. A seated seat whose ledger row is bound to a unit (`rules.match_assign`
with `unit`) is entered on the next step; a host can also call `MatchSession::race_enter(seat, unit)`.
Racers start at the start line on lap 0, so the grid goes just past the start gate, and lap 1 starts
on the first step the race runs. Each step the race reads each racer's unit position from the step's
units and converts it once to whole millimeters; the rest is integer: a racer passes a checkpoint on
the step it enters the volume. Entering one of the expected `next` checkpoints counts
(`racer_checkpoint`, or `racer_lap { lap, lap_ticks, best }` at the start); entering any other sends
`racer_missed_checkpoint { checkpoint, expected }` and counts nothing, so a lap that skipped a gate
does not count until the racer goes back for it.

A seat that leaves the match (`leave`) stops racing at once: a racer still running is DNF on the spot
(`racer_dnf`), never takes a place, and never holds the race open, so the race ends once every racer
has finished or is DNF even when nobody finished (the winner team is then `0`). A finished racer who
leaves keeps the place. A seat move (`rules.match_assign` with `from`, or a rebalance) carries the
racer's progress to the new seat, as the ledger row moves; a row with a place is never replaced, so a
mover onto the seat of a racer who finished and left drops its own row. A new player on a seat that a
leaver left without a place races from the start line (ADR-8802).

The wrong-way count reads the signed distance along the current segment, not clamped to it: a racer
behind the segment origin who keeps going backward keeps counting.

The live standings (`MatchSession::race_standings`) put finished racers first by place, then running
racers by laps (most first), checkpoint list index (highest first; list branch alternates in the order
the ranking should prefer), the fraction of the segment still to run to the nearest next gate (least
first; a dot product, compared exactly by cross multiplication), and seat; DNF racers last. Racers who
finish on the same step are placed by how far past the finish gate they are along the last segment,
in millimeters (the scalar projection over the integer square root of the segment's squared length,
compared exactly), then by seat (`racer_finished { place }`). After the window, `racer_dnf`. When every racer has
finished or is DNF, the session ends the match with the winner's team (`match_over`, reason `ended`).
Each new best lap is written to the seat's ledger counter `best_lap_ticks`, and
`regia_framework::rules::post_best_laps(leaderboard, board, local_seat, events)` is the leaderboard sink: it uploads the
local seat's new best laps (score = lap ticks; declare the board ascending). `RaceWin` is the `WinCondition` (rule word
`race`).

`rules.match_get` shows `race` when the course is on: `settings`, `state` (per racer `lap`,
`checkpoint`, `checkpoint_index`, `remaining`, `lap_times`, `best_lap_ticks`, `finished_at`, `place`,
`dnf`, `wrong_way`) and `standings` in position order. A course the race cannot run answers
`match_race_settings`: `too_few_checkpoints` (2), `too_many_checkpoints` (256), `laps_range`,
`duplicate_id`, `next_count` (1 to 8), `next_unknown`, `next_self`, `volume_bad` (a non-finite corner
or `min` above `max`).

Both genres can run in one session (for example a co-op race against waves); they step after the
objectives in the order waves, race, win conditions. `waves_and_race_bit_identical_on_two_peers`
plays both on two sessions fed the same inputs (one with its unit list reversed) and compares every
step's events and the serialized session.

## Shrinking zones and elimination

`SessionSettings.zone` (GR-11.3) is a `ZoneSchedule`: a start `center` (`[x, z]`), a `radius` and a list
of `phases`, each `{ wait_ticks, shrink_ticks, end_radius, damage_per_tick }`, plus a `seed`. It is
`null` by default, and then nothing below runs. The zone starts on the first match tick the match plays
(after a warm-up, with phases on). Each phase holds its circle for `wait_ticks`, then shrinks it over
`shrink_ticks` to `end_radius` around a new center. The center is drawn inside the current circle, so
the next circle fits inside it, from a `DetRng` stream of `seed` and the start tick: every peer with
the same settings draws the same circles. After the last phase the circle holds.

The circle is kept in whole centimeters. The shrink is an integer interpolation and the inside test is
a squared distance across XZ, so no square root or trigonometry runs. A live unit outside the current
circle gets one `zone_damage { tick, unit, amount }` event per match tick, with the running phase's
`damage_per_tick` (the last phase's once closed). The game applies it: the rules core never owns
health. Each stage change sends `zone_phase { tick, phase, stage, center, radius, target_center,
target_radius }`; `stage` is `waiting`, `shrinking` or `closed`.

`SessionSettings.elimination` is `{ last_standing, alliances }`, off by default. With
`last_standing: true`, a team that has played in the match (a contender) is out when none of its
playing seats is left uneliminated: every seat ran out of lives under its respawn policy (see
[Respawn](#respawn)), or left. A team out sends `team_eliminated { tick, team, place, cause }`. Teams
that go out on one tick go out in team-id order. A team's `place` is the count of contenders still
standing when it went out, itself included, so the first of 100 solo teams is placed 100th.

When a team is out and the teams still standing are all on one side, that side wins. A side is an
alliance (`alliances: [{ teams: [1, 2] }]`) or a team in none. The step sends `victory { tick, teams,
reason }` with every contender of the side, then `match_over` with the side's lowest standing team and
`last_standing`. The fallen allies of the winning side share the win and the first place. When the
last sides go out on one tick, nobody is left, and the match ends as a `draw`. With phases on, the end
is reported to the phase machine (`MatchSession::report_condition`) and lands on the next step; with
rounds on it ends the round, and a new round clears the book.

`rules.match_get` adds `zone` (`null`, or `stage`, `phase`, `phases`, `started_at`, `stage_since`,
`center`, `radius`, `target_center`, `target_radius`, `damage_per_tick` and the `schedule`),
`placements` (`[{ team, place }]`, best first, then by team id) and `victory` (`conditions`,
`surrender`, `alliances`, `last_standing`, `contenders`, `out`, `winners`, `decided`, `state`). The
other `rules.match_*` answers leave these keys out.

A battle-royale session for 100 solo players:

```json
{ "method": "rules.match_setup", "params": { "start": true, "settings": {
  "flow": { "max_teams": 100, "seats_per_team": 1 }, "objectives": { "max_teams": 100 },
  "respawn": { "default": { "kind": "never" } },
  "elimination": { "last_standing": true },
  "zone": { "center": [0, 0], "radius": 800, "seed": 7, "phases": [
    { "wait_ticks": 3600, "shrink_ticks": 3600, "end_radius": 400, "damage_per_tick": 1 },
    { "wait_ticks": 1800, "shrink_ticks": 1800, "end_radius": 100, "damage_per_tick": 3 } ] } } } }
```

Bounds: teams, seats and places are `u8`, so 100 solo teams or 50 duos fit the board and the score
vectors. A schedule holds at most 64 phases and radii up to 1,000,000 world units.

## Strategy victory conditions

`SessionSettings.victory` (GR-11.4) is `{ conditions, surrender }`. Each condition is tagged by `kind`:

| Condition | Ends the match when |
|---|---|
| `{ "kind": "destroy_all_tagged", "tag": "capital" }` | A team that had units with the tag has none left: it is out (`cause: destroyed`), and the last side standing wins (`destroyed`). A team that never reported the tag stays in. |
| `{ "kind": "hold_for", "objective": 7, "ticks": 18000 }` | A team has held the objective for `ticks` match ticks in a row (`held`). A change of holder, or nobody holding, starts the count again. |
| `{ "kind": "score_threshold", "score": 5000 }` | A team's reported score reaches it (`score_reached`). On one step the best score wins; a tie goes to the lower team id. |
| `{ "kind": "team_elimination" }` | As `last_standing`: a team whose seats are all eliminated is out, and the last side standing wins. |
| `{ "kind": "surrender" }` | A team's surrender vote passes: it is out (`cause: surrendered`), and the last side standing wins (`surrendered`). |

The host reports what only its simulation knows with `rules.match_report` (or the `MatchSession`
methods `report_tagged`, `report_holder`, `report_score` and `defeat_team`):

```json
{ "method": "rules.match_report", "params": {
  "tagged": [{ "team": 2, "tag": "capital", "count": 0 }],
  "holders": [{ "objective": 7, "team": 1 }],
  "scores": [{ "team": 1, "score": 4200 }],
  "defeat": [4] } }
```

Each table holds at most 256 rows per call; every table is checked before any row lands, including
the session's cap of 1024 rows per table (new keys counted once; updates of a held key add nothing): a
call that would pass it answers `match_victory_settings` and changes nothing. Teams the host defeats
on one match tick (several `defeat` entries, several calls, or `MatchSession::defeat_team` calls
between two steps) are placed together in team-id order, whatever order they arrive in; a pending
`team_eliminated` is rewritten, and one already taken is sent again only when its place moved. The session
evaluates on its next played step, after the objective rules: surrender ballots, then
`destroy_all_tagged`, then seat eliminations, then the last side standing, then `hold_for` and
`score_threshold` in list order. Every win is shared with allies (`settings.elimination.alliances`):
`victory.teams` lists the side and `match_over` carries its lowest standing team. Teams still
standing that lost a hold or score victory share the place after the winners.

Surrender is a team vote: `{ "input": "surrender_vote", "team": 1, "slot": 0, "yes": true }` through
`rules.match_input`. Only the seat itself (or the host) may send it (`match_not_permitted`
otherwise); a seat that is not playing, or is eliminated, answers `match_seat_missing`. The first vote opens the team's ballot (`surrender_vote` event). It passes when the yes
votes reach `surrender.threshold_percent` of the team's voting seats (playing, not eliminated; rounded
up, at least one; default 100). It fails when the no votes make that unreachable or when
`surrender.window_ticks` (default 1800) pass, with `surrender_failed { tick, team, until }`; the team
may not vote again before `until` (`surrender.cooldown_ticks` later, default 10800):
`match_surrender_cooldown`. A vote without a `surrender` condition, outside play or after the match is
decided answers `match_surrender_off`. Ballots are sorted slot lists, so votes landing in one step in
any order give the same state.

The core stays plain data: integer ticks, sorted vectors, no hash-map iteration. The stock lives and
structures rules of `win_condition` are unchanged; these conditions are a data-driven set beside them.

## Rotation

A playlist plays its rows in list order unless it sets another order (ADR-8897). Public servers and matchmaking
need more than a fixed list, so a playlist can shuffle, weight its rows, gate rows on the player count, skip rows
whose content is missing, and avoid repeats.

| Field | Where | Default | What |
|---|---|---|---|
| `order` | `Playlist`, `.playlist.ron`, `rules.match_setup` | `in_order` | `in_order`; `shuffle(seed: N)` plays every row once per cycle in a fresh order per cycle; `weighted(seed: N)` plays each row `weight` times per cycle in a shuffled order. |
| `no_repeat_last` | same | `0` | Avoid the last `n` rows played while another row fits. A shuffle guards at least the last row, so the last row of a cycle never opens the next one. |
| `weight` | row | `1` | Plays per weighted cycle, `0..=100` (`0` leaves the row out of the weighted cycle). The other orders ignore it. |
| `min_players`, `max_players` | row | none | The row is skipped while the player count is outside `min..=max`. A row without the field reads the `min_players` / `max_players` option, as the map vote did. |
| `playlist_id` (`Playlist::id`) | `rules.match_setup` | empty | Published as the `regia.playlist` lobby key. A dedicated host uses the `.playlist.ron` file name. |

Cycle `n` of a shuffled or weighted order is a Fisher-Yates shuffle by stream `n` of `DetRng::seed_from(seed)`, so
lockstep peers with the same rows, seed and history take the same rows. The rotation state (`cycle`,
`cycle_number`, `recent`) is part of the session and is not written while it is at its default, so an in-order
playlist keeps S-14's shape.

On a launch the session calls `Playlist::take_next_for(player_count, available)`. A row outside its player gate, or
whose `map_id` or `mode` is in `MatchSession::missing_content` (the host fills it from its installed content), is
passed over with a `row_skipped` event before the `launched` event. In order, a passed row waits until the rotation
comes around again; in a cycle it is dropped for that cycle. When no row fits, the playlist does not move and the
current row reruns: `launched` carries no index and the board keeps its mode, as at the end of a list that does not
loop. A list that does not loop ends after its last row (in order) or its one cycle.

The end-of-match map vote offers the next eligible rows (`Playlist::vote_candidates(n, players)`), walking the
rotation the way the next launches will: the rest of the cycle, then the next one. The vote and the rotation share
one eligibility rule. In order, the ballot walks once from the index, and rows the repeat guard holds back
(`no_repeat_last`) come after the other eligible rows (ADR-8881), so the first option is the row the next launch
plays.

`row_skipped` carries `step`, `index`, `map_id` and `reason`:

| Reason | When |
|---|---|
| `too_few_players` | `player_count` is below the row's minimum. |
| `too_many_players` | `player_count` is above the row's maximum. |
| `missing_content` | The row's map or rule set is in `missing_content` (the dedicated host: the level is not in the pak). |
| `recent` | In order, the row is among the last `no_repeat_last` rows played and another row fits. |
| `load_failed` | Dedicated host only: the row's level failed to load (`server.rotation_get`). |
| `ruleset_*` | Dedicated host only: the row's rule set does not resolve with its options (`server.rotation_get`, ADR-8881; [Net session players](net-session-players.md)). |

`row_skipped` is a logic-graph event (`match.row_skipped`: the row's index as `round`, the reason, the launch step
as `tick`) and travels to clients as replicated match event kind 38 (ADR-8881).

`rules.match_setup` takes `order` (`"in_order"`, `{ "shuffle": { "seed": 7 } }`, `{ "weighted": { "seed": 7 } }`),
`no_repeat_last` (`0..=255`), `playlist_id`, and rows with `weight`, `min_players` and `max_players`. A weight above
100 or a minimum above the maximum is `F0002`. `server.rotation_get` adds `order` and `skipped`, the rows the last
travel passed over (`index`, `map_id`, `mode`, `reason`), `rules_hash`, the playlist's rule-set hash, and
`rules_in_manifest`, whether the session manifest carries it (ADR-8881).

```json
{ "method": "rules.match_setup", "params": { "order": { "shuffle": { "seed": 7 } }, "no_repeat_last": 2, "playlist_id": "public-ctf", "looping": true, "playlist": [{ "map_id": "harbor", "mode": "ctf" }, { "map_id": "canyon", "mode": "ctf", "min_players": 8 }, { "map_id": "mesa", "mode": "koth", "max_players": 6 }], "start": true } }
{ "method": "rules.match_step", "params": { "steps": 1 } }
```

The host publishes the running mode and playlist as the lobby keys `regia.mode` and `regia.playlist`
(`Lobbies::publish_session` after each launch), and a browser or matchmaker filters on them
(`LobbyQuery::with_mode`, `LobbyQuery::with_playlist`; [Lobbies](net-lobbies.md)).

Switch note (S-14 defaults): `order: in_order`, `no_repeat_last: 0`, rows without `weight` or gates, an empty
`missing_content`: the rotation takes the rows exactly as S-14 did. DO27-compat value: those defaults.

## Samples that play a mode

Two samples play a mode through the core, so an agent can watch a real match (GR-8.3):

- **`samples/arena --mode ctf|koth|tdm`** resolves the shipped rule set of that id through the
  app's `RuleSetRegistry` (`--ruleset <id>` for another one, such as the arena's `ctf_quick`;
  `--limit N` for the capture or hill limit), seats slot `s` on team `1 + s % 2`, places team
  `PlayerStart`s with `SpawnPoint`s, the flags (ids 901 and 902) or the hill (id 950), and steps the
  session with `MatchSessionPlugin`. Its bots take their orders from `BotHooks::orders`, fire inside
  6 m (a CTF attacker without a flag does not stop to shoot) and hit half the time, drawn from
  `MatchRng`, so a seed replays the same match. A dead pawn
  stays down for 90 fixed steps before it respawns at its team's start, so a carried flag drops
  where its carrier fell. Without `--mode` the arena plays its own free-for-all, unchanged.
- **`samples/net_game`** seats each joining player on the team with fewer players (`TeamId` 1 or 2)
  and starts its avatar at a `PlayerStart` of that team (round-robin spawn policy). Clients read the
  rows and the team totals from `PlayerScoreView` and `TeamScoresView` only.

What an agent reads in the arena:

| Where | Fields |
|---|---|
| `ArenaModeStatus` (component on the `"match"` entity; `play.expect`, `scene.get_entity`) | `mode` (`ffa`, `tdm`, `ctf`, `koth`), `ruleset` (the resolved id), `rules_hash` (the resolved rules' content hash), `error` (`<code>: <message>` when the asked rule set did not resolve; the arena then plays the free-for-all) |
| `ArenaBrainStatus.brain` | `mode` in a team mode |
| `ArenaState` | `phase` and `scores` by slot (the kills of each slot's seat) |
| `rules.match_get` | `mode`, `board.objectives` (`carrier_id`, `at_base`), `board.team_scores`, `board.end_reason`, `flow.seated`, `ledgers` |

```json
{ "method": "play.expect", "params": { "assertions": [
  { "name": "ctf from the shipped rule set",
    "condition": { "entity": { "component": "ArenaModeStatus" }, "component": "ArenaModeStatus", "path": "ruleset", "op": "eq", "value": "ctf" } } ] } }
```

No new agent methods and no new error codes: the existing `rules.*` and `play.*` methods read
both samples. The CTF match is the functional test `arena_ctf_bots` (tag `ctf`; `arena::smoke::install_ctf`),
run in process by `cargo test -p arena --test modes`. The walk-through is
[Tutorial: capture the flag](../tutorials/capture-the-flag.md).
