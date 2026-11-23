# Game modes

## An independent match core

Use `regia_rules` to run match modes in a simulation that owns its entities and
tick loop. It provides capture the flag, king of the hill, objective state,
pause, ready-up, observers and playlists without linking ECS, rendering or
platform crates. `session::MatchSession` combines the board, flow and playlist.
Apply the agreed inputs with `MatchSession::apply`, then call
`MatchSession::step` once per simulation tick with contestants in deterministic
entity order. Consume the returned events to update presentation.

Games using the framework can keep `regia_framework::rules` imports: these are
re-exports of the same types. `MatchRules` and `MatchRulesPlugin` remain framework
resources and integration helpers. The existing `rules.match_*` agent methods
still inspect and drive that resource; see [Match modes and match flow](../agents/match-rules.md).

For DO27's sim, link `regia_rules` instead of `regia_framework` (ADR-8239).
The extraction preserves every S-14 setting and computation. Keep
`start_delay_ticks = 0`, `seat_slot_limit = Some(5)` for the alpha, and the game's
mode-name parsing before the generic fallback. This dependency switch requires
no content migration.

## Who may ready, pause and leave

A networked match applies each input with the seat the session already knows.
`MatchSession::apply_from(issuer, input)` checks that seat. `apply` is the host,
and it stays the call a single-player or listen host uses.

A player may ready, unready, observe, leave, pause and resume only for their own
seat. The host may do those, and only the host may seat someone else, approve a
join, toggle the host pause, override AFK or open the ready gate. A refused
input does not change the match.

Leaving takes that seat out of the ready gate, so a disconnected player no
longer blocks the next start. Observers ask to join in a queue the host approves
oldest first, up to eight waiting asks. A pause names the seat that paused.
Empty display text shows that seat, and the name on the banner does not let
another player resume. Pause count and pause length limits default to off.
`match_clock` defaults to continuing the match tick across launches; a per-match
clock starts the tick at 0 when the next match launches, so king of the hill
waits a full period before the first point.

## Dedicated servers: match end and playlists

A dedicated server ends a match and loads the next map on its own. With the framework `net`
feature, the session follows the level's game rules: the match is over when those rules say so,
or when the match board says so. The server then waits `match_end_delay_s` and travels.

List maps in `server.toml` under `levels.rotation`, or point `levels.playlist` at a
`.playlist.ron` file. Use one of those keys, not both. A rotation entry is a map and the level's
own rules. A playlist row names a map, a mode and options. The mode becomes the next level's
game rules, and the options are available to those rules as match options. An empty mode keeps
the level's own rules.

If a later map is missing, the server logs `server_next_level_failed`, skips that row, and tries
the next one. It tries each following row once. The map it is already running stays when none of
the following rows load. The starting map must be present or the server exits at startup.

`server.rotation_get` shows the rows, the current index, whether the list loops, and the last row
that failed. `server.rotation_skip` moves to the next row. Both apply only while a dedicated host
is running.

## Warm-up, rounds and overtime

A competitive mode usually needs more than "first to the score limit". The match
core has the pieces built in, all off until you turn them on in
`FlowSettings::phases`:

- **Warm-up.** Players run around and nothing counts. It ends after a set time,
  or as soon as enough players are seated, whichever comes first, and the scores
  and flags reset when it does.
- **Countdown.** A short hold before play starts, and again before each round.
- **Time limit.** When the clock runs out the leader wins. On a tie you choose:
  end with no winner, call it a draw, play overtime until someone leads (with an
  optional cap that ends in a draw), or sudden death, where the next point wins
  whoever scores it.
- **Rounds.** For example best of five: first to three round wins. Each round
  ends on any win condition, the scores and flags reset between rounds, the round
  wins are kept, and teams can swap sides every few rounds.
- **Post-match.** An intermission after the result, then the ready-up gate opens.
- **Forfeit.** A team with no players for a set time loses.

Every duration is in simulation ticks, so set them from seconds once with
`ticks_from_secs` when you build the settings. The HUD reads the phase, the
round and the time left from the session (or from `rules.match_get`), and the
event feed announces each phase change and round result. Leaving every setting
off keeps a match exactly as it was. Details, the exact tie-break order and the
agent fields are in [Match modes and match flow](../agents/match-rules.md).
## Flags on multi-level maps

By default a flag is touched across the ground plane only, as in S-14. On a map with stacked floors, such as a
tunnel under a base, set `touch_height` in the objective settings. A unit then has to be within that height of the
flag to take or return it, and a carrier has to be within that height of its own base to capture. A unit on the
floor below can no longer reach through the ceiling.

A dropped flag waits for a teammate by default. Set `flag_return_ticks` to send it home after that many match ticks
on the ground. A pickup restarts the count. For flags that land where nobody can reach them, such as a kill volume,
out of bounds or a collapsed tunnel, step the match with `MatchSession::step_with` and answer `DropCheck::Return`
for those points: the flag goes home on that step. Both cases emit a `flag_auto_returned` event for the HUD feed.

`capture_needs_home_flag` (on by default) is the rule that your own flag must be home to score. Turn it off for a
faster mode in which a capture counts while your flag is away.

King-of-the-hill hills are boxes with a height, so they already separate floors.

## Teams and scoring

A match has four unnamed teams unless the session lists others. Each team has a
name key, a color and a seat cap. Players sit in seats. A new player with no
requested team joins the side that has the fewest players, then the side with
the lower team score. A party stays on one side when a side has room for all of
them.

By default a seated player cannot change team during a match. The session can
allow a change only between matches, or only when one side is ahead of the
other by a set number of players. A launch can also even the sides without
splitting a party. A party too big to move without making its new side the
fuller one stays where it is, so the sides can stay more than one player apart.
Both of those stay off until the session turns them on.

A player who changes team keeps their own score, pause allowance and place in
the join queue. A player who takes an empty seat starts from zero, and so does
a player who leaves and comes back. Each scoreboard row has an id that no other
player in the session gets, even one who later takes the same seat.

Each seat keeps its own kills, deaths, assists, objective points and score.
The team score on the board is still the capture or hill total. A capture
credits the carrier, and a hill point credits each teammate who was on the
hill once, without adding that point to the team total a second time. A kill also
credits anyone who hit that player within the assist window.

Teammates hurt each other unless the game turns friendly fire off, scales it,
or sends it back to the attacker. Hurting yourself is a separate switch and
stays on. Neutral players are not on a team, so they are not teammates.

## A recorded match and a bound on events

A 3600-step capture-the-flag raid, a pause and a king-of-the-hill match are recorded as the
golden `match_rules_ctf_koth_3600`. The same inputs on every peer, including contestants sorted by
`host_unit_order`, produce the same session. See [Match modes and match flow](../agents/match-rules.md)
for the legs and the step-cost table.

The board and the flow keep at most `max_pending_events` events (4096 unless a match sets another
cap). If a host steps the rules and does not drain, the oldest events are dropped and
`events_dropped` counts them. `rules.match_get` shows that count. Draining every step, which a
normal match does, drops nothing.

## Spawning and respawning

Mark spawn locations with `PlayerStart`. To give a start to one team, add a `SpawnPoint` with that
team's side: `SpawnPoint::for_team(1)`. Side `0` serves everyone. After a side swap, teams spawn on the
side they now play on. Tags (`"attack"`, `"vehicle"`) let a spawn ask require a kind of start, a higher
`priority` is tried first, and `enabled: false` takes a start out of play.

Choose how a start is picked with the session's spawn policy (`settings.respawn.spawn`):

- `first`: always the first eligible start (the default).
- `round_robin`: each start in turn.
- `random`: a seeded draw, the same on every peer.
- `scored`: the start farthest from enemies and out of their sight, near teammates, and not used a
  moment ago. Tune it with the weights `enemy`, `enemy_cap`, `los`, `los_range`, `team`, `team_radius`,
  `recent`, `recent_ticks` and `priority`.

To skip starts where something stands, insert `SpawnShape { block_test: true, .. }` with your pawn's
capsule size.

Choose when the dead come back per team (`settings.respawn.default` and `settings.respawn.teams`):
`instant`, `delay` (ticks), `waves` (everyone together on a period), `lives` (a fixed number, then
out) or `never`. The match reports `respawn_at` when a player may spawn again and `eliminated` when a
player is out.

For spawn protection, call `SpawnProtection { ticks: 120, ends_on_fire: true }.protect(world, pawn)`
after spawning a pawn: it takes no damage for 120 fixed steps or until it fires (send `ShotFired` from
your weapon). The agent method `rules.spawn_preview` shows every start, why it is or is not eligible,
and its score.

## Zone modes

A capture zone is an area on the map that a team takes by standing in it. Each
zone has a capture bar. One player fills it at the normal speed. More players
fill it faster, up to the zone's limit. When the bar is full, the zone belongs
to that team, and the owner earns a point at a steady rate while it holds the
zone. Players who are standing in the zone when it flips get the credit.

When players from two teams stand in a zone, the zone decides who wins the
push. By default any enemy freezes the bar. A zone can instead let the bigger
group push at its own speed, or push at the difference between the groups. A
team that walks onto a zone with another team's half-filled bar first drains
that bar, then starts its own. When a zone is empty, a half-filled bar can drain
back to neutral over time. A zone that is already owned stays owned.

Three modes use zones:

- **Domination**: several zones at once. The team that holds more of them
  scores faster. The first team to the score limit wins.
- **Hardpoint**: one zone is live at a time. After a set time the next zone in
  the list goes live, and the old one goes back to neutral.
- **Attack and defend**: the defenders start with every zone, and only the
  first one can be taken. When the attackers take it, it stays theirs and the
  next zone opens. The attackers win when the last zone falls. With a time
  limit, the defenders win when time runs out.

King of the hill is unchanged. It still has one hill, which scores once every
period.

## Making a mode without code

A mode is a rule set: a `.rules.ron` file in your project's `assets/rules/` folder. The engine ships
`tdm`, `ffa`, `ctf`, `koth` and `dom`, which hold the engine defaults. To make a longer
capture-the-flag, extend `ctf` and change only what differs:

```ron
(
    format_version: 1,
    id: "ctf_long",
    name_key: "mode.ctf_long",
    extends: "ctf",
    session: (
        flow: (start_delay_ticks: 300),
        phases: (time_limit_ticks: 36000, on_time_up: (kind: "overtime", ticks: 3600)),
        objectives: (capture_limit: 5, flag_return_ticks: 1800),
        respawn: (default: (kind: "delay", ticks: 180)),
    ),
    options: [
        (key: "sky", kind: enum, default: "day", choices: ["day", "dusk", "night"]),
    ],
)
```

Every setting you leave out keeps the value of the mode you extend. A mode can also set its own
teams (names, colors, seat caps), its win conditions (`(rule: "lives", lives: 5)`) and the options a
playlist row may set. Each option has a kind (`int`, `float`, `bool`, `enum` or `string`), a default
and, for numbers, a `min` and a `max`.

Use the mode by its id in a playlist row: `(map_id: "levels/harbor.level.ron", mode: "ctf_long",
options: { "sky": "night" })`. A row that sets an option the mode does not declare, or a value out
of range, is refused with the option's name. A row whose mode is a plain `ctf` or `koth` keeps
working as before.

An option can set a match setting directly with `bind`. Then a playlist row changes the match
without any game code:

```ron
options: [
    (key: "caps", kind: int, default: "5", min: "1", max: "10", bind: "objectives.capture_limit"),
    (key: "time_limit", kind: int, default: "0", min: "0", max: "216000", bind: "phases.time_limit_ticks"),
],
```

A row `options: { "caps": "8" }` plays to 8 captures. For a setting that can be off (a time limit, a
warm-up, a flag return timer), 0 turns it off. The settings you can bind are listed in
[Match modes and match flow](../agents/match-rules.md#rule-sets).

Check your files before you ship them:

```sh
regia rules check assets/rules/*.rules.ron
```

It prints `ok` with the mode and its content hash, or each error with a code and the field it is
about. In a running game, the agent methods `rules.ruleset_list`, `rules.ruleset_get` and
`rules.ruleset_validate` show the same, and `rules.match_setup { ruleset: "ctf_long" }` starts a
session with it.

In a networked match, every player must run the same rules. The mode's content hash is part of the
join check, so a player whose copy of a mode differs is told the content does not match and does not
join. See [Match modes and match flow](../agents/match-rules.md) for every field and error code.

### Co-op, race, battle-royale and strategy modes

The same file sets up the other kinds of match: `waves` for co-op, `race` for a course, `zone` for a
shrinking circle, `elimination` for last-team-standing play and alliances, and `victory` for strategy
win conditions. Each is a section under `session`. Leave out what you do not need. Lists replace the
parent's list whole. The sections below describe what each one does in a match.

A co-op mode: two waves, more enemies from three players up, and five lives shared by the party.

```ron
(
    format_version: 1,
    id: "coop_waves",
    skeleton: "custom",
    session: (
        respawn: (default: (kind: "delay", ticks: 60)),
        waves: (
            waves: [
                (groups: [(archetype: "grunt", count: 4, spawn_tag: "east")], alive_cap: 3),
                (
                    at: (kind: "after_cleared", delay_ticks: 5),
                    groups: [(archetype: "grunt", count: 2), (archetype: "brute", count: 1, spawn_tag: "gate")],
                ),
            ],
            difficulty: [(players: 3, count_percent: 150, cap_percent: 125)],
            shared_lives: 5,
            success: either,
        ),
    ),
)
```

A race: one lap of four gates. Ten ticks after the winner crosses the line, everyone still racing is
DNF.

```ron
(
    format_version: 1,
    id: "race_sprint",
    skeleton: "custom",
    session: (
        race: (
            checkpoints: [
                (id: 10, volume: (min: (-3.0, -1.0, -3.0), max: (3.0, 3.0, 3.0)), next: [20]),
                (id: 20, volume: (min: (97.0, -1.0, -3.0), max: (103.0, 3.0, 3.0)), next: [30]),
                (id: 30, volume: (min: (97.0, -1.0, 97.0), max: (103.0, 3.0, 103.0)), next: [40]),
                (id: 40, volume: (min: (-3.0, -1.0, 97.0), max: (3.0, 3.0, 103.0)), next: [10]),
            ],
            laps: 1,
            finish_window_ticks: 10,
        ),
    ),
)
```

A battle royale: solo teams with one life each, and a zone that shrinks twice. The last team standing
wins.

```ron
(
    format_version: 1,
    id: "br_zone",
    skeleton: "custom",
    session: (
        flow: (max_teams: 4, seats_per_team: 1),
        objectives: (max_teams: 4),
        respawn: (default: (kind: "never")),
        zone: (
            center: (0.0, 0.0),
            radius: 100.0,
            phases: [
                (wait_ticks: 10, shrink_ticks: 100, end_radius: 10.0, damage_per_tick: 3),
                (wait_ticks: 5, shrink_ticks: 20, end_radius: 2.0, damage_per_tick: 7),
            ],
            seed: 42,
        ),
        elimination: (last_standing: true),
    ),
)
```

Elimination with an alliance: teams 1 and 2 win together, even if one of them has already fallen.

```ron
(
    format_version: 1,
    id: "allied_elimination",
    skeleton: "custom",
    session: (
        flow: (max_teams: 3, seats_per_team: 1),
        objectives: (max_teams: 3),
        respawn: (default: (kind: "never")),
        elimination: (alliances: [(teams: [1, 2])]),
        victory: (conditions: [(kind: "team_elimination")]),
    ),
)
```

A strategy mode: a team that loses every capital is out. A team wins by holding the wonder or by
reaching 500 points. A team may surrender when two thirds of its players vote yes.

```ron
(
    format_version: 1,
    id: "strategy",
    skeleton: "custom",
    session: (
        flow: (max_teams: 2),
        objectives: (max_teams: 2),
        victory: (
            conditions: [
                (kind: "destroy_all_tagged", tag: "capital"),
                (kind: "hold_for", objective: 7, ticks: 6000),
                (kind: "score_threshold", score: 500),
                (kind: "surrender"),
            ],
            surrender: (threshold_percent: 66, window_ticks: 900),
        ),
    ),
)
```

A rule set that extends one of these can change a single field (`zone: (seed: 7)`) or turn a section
off (`zone: Some(None)`). A value that cannot run is refused with the field it is about, such as
`session.race.checkpoints[2].next[0]`.

## Server-authoritative games

A game where one host decides the match (a dedicated or listen server) does not
have to write the glue between its entities and the match core. Add
`MatchSessionPlugin` and install `MatchSessionRules` as the game rules:

- Joining players get a seat and spawn on their team. A player who asks for a
  team (a `TeamId` on the player entity) gets it when it has room. When every
  team is full, the player watches.
- Every entity with a team, a position and health takes part. Flags and the
  hill are entities too: tag one with `ObjectiveEntity` and it moves where the
  core says, so a carried flag follows its carrier.
- Kills, deaths, captures and hill points land on each player's score row. A
  player who leaves loses the seat and the row; a pawn left behind scores for
  nobody, and the next player in that seat starts with a fresh row.
- A pawn killed on a tick does not grab a flag on that same tick: the match
  steps after damage, whatever order your plugins were added in.
- A team change between matches moves the player's pawn to the new team on the
  next tick.
- The scoreboard shows one row per player and each team's score.
- The core's events (flag taken, captured, match over, phase changes) arrive as
  `MatchEvent` messages for sound, announcer lines and UI.

Units take part in the order they joined the match, never in the engine's
storage order, so two runs of the same match end the same way. A lockstep game
keeps stepping its own session instead and does not add the plugin.

## Showing the match on clients

In a client/server game only the server runs the match. Add
`MatchStateReplicationPlugin` to the server and to every client, and the clients
get what a HUD needs:

- The phase, the round, the clock, and who paused, on one shared match entity.
- Every team's score, and the state of every flag and hill (at base, carried,
  dropped), with how far a dropped flag's return timer has run.
- Each player's kills, deaths, assists, objective points and score, on that
  player's own entity.
- Kill feeds, captures, phase changes and the end of the match, as the same
  `MatchEvent` messages the server sees.

Install `ReplicatedMatchRules` as the client's game rules, and the scoreboard
shows the server's standings. A player who joins late sees the current scores
and objectives at once. Events from before they joined are not replayed.

A lockstep game runs the rules on every machine, so it does not need this.

## Scripting mode events

A logic graph can react to the match and change it, with no code. Put a graph on
the level's game-mode entity and mark that entity `MatchEventListener`. It then
hears every match event as an entity event named `match.` plus the event kind:
`match.flag_captured`, `match.flag_taken`, `match.hill_point`,
`match.player_scored`, `match.match_over`, `match.phase_changed` and the rest
(the logic graph editor lists them under Events as `Match: <kind>`). The player
who took or captured a flag, the flag itself and a scoring player's pawn hear
their own events too, if they have a graph.

Before the event arrives, the graph's blackboard gets the details:
`match_` plus the event kind plus `team`, `seat`, `score`, `objective`,
`unit`, `reason`, `round` or `tick`. For a capture these are
`match_flag_captured_team`, `match_flag_captured_unit` and so on, so a rule
reads the details of its own event even when other events arrive in the same
moment. "When the flag is captured,
open the gate" is one rule: the event `match.flag_captured`, the condition
`match_flag_captured_team` equals 1, and the action `logic::DisableEntity` on the
gate.

Five actions change the match:

- `rules::AddScore` adds points to a team, or to one player's score (that also
  sends `match.player_scored` with the reason `scripted`).
- `rules::EndMatch` ends the match, with a winning team and a reason such as
  `forfeit`.
- `rules::SetTeam` moves an entity to another team. A seated player moves only
  if the match's team-switch setting allows it.
- `rules::SetSpawnEnabled` takes a player start out of spawn selection, or
  puts it back. If every start is off, the first one is used anyway.
- `rules::LockObjective` locks a flag (nobody can take it, and a carried one
  does not score) or the hill (it scores no points) until it is unlocked.

These run on the host only. On a client they do nothing and the graph's trace
says why. A team score added this way does not end the match by itself: add
`rules::EndMatch` to the rule that should win it.

## Co-op waves

For a co-op mode against waves of enemies, fill `settings.waves` with a wave table. Each wave lists
groups of enemies by your own archetype id (a prefab key or enemy class), a count and a spawn tag,
and an alive cap. A wave starts after the previous one is cleared (with an optional delay) or at a
fixed match tick.

The match tells you when to spawn with `wave_spawn` events (archetype, count, spawn tag, wave). Spawn
that many enemies, remember each one's wave, and call `note_wave_down(wave, 1)` when one dies. The
match never spawns anything itself, and it keeps the number alive under the cap.

Scale for the party with `difficulty` rows: "from 3 players, 150 % enemies and a 125 % cap". The
players can share a pool of lives (`shared_lives`); when it is empty, the next death is out for good.
The match is lost when every player is down at once or when you report the defended objective
destroyed, and won when the last wave is cleared or when you report the objective reached. With a time
limit from the phase settings, running out of time ends the match without a winner.

## Races

For a race, fill `settings.race` with checkpoints: boxes with an id and the ids that may come next
(two or more make a branch). The first checkpoint is the start and finish line; returning to it ends a
lap. Set `laps`, and `finish_window_ticks` to end the race that long after the winner crosses the line
(the rest are DNF).

Bind each racer's vehicle to its seat (`unit` when seating), place the grid just past the start, and
pass the vehicles to the match step. The match reports checkpoints, laps (with the lap time and
whether it is a best), a missed checkpoint (the lap does not count until the racer goes back), a racer
going the wrong way, finishes with their place, and DNFs. `race_standings()` gives the live order for
your HUD. Each racer's best lap is kept on the score sheet as `best_lap_ticks`; `post_best_laps` sends the
local player's new best laps to an online leaderboard.

## Shrinking zones and elimination

A battle-royale match shrinks a circle over the map. Set `settings.zone` with a start center and
radius and a list of phases. Each phase waits, then shrinks the circle to a smaller radius around a
new center that lies inside the old circle. Players outside the circle take the phase's damage every
tick; the game applies it to their health. Every player's machine draws the same circles from the
zone's seed.

Turn on `settings.elimination.last_standing` and give each player one life
(`settings.respawn.default` of `never`, or `lives`). A team whose players are all out is eliminated
and gets a place: the first of 100 solo players out is 100th, and teams out at the same moment are
ordered by team number. The last team standing wins and is first. If the last teams fall together,
the match is a draw. The board fits up to 255 teams, so 100 solo players or 50 duos are fine.

## Strategy victory conditions

A strategy match can end in more ways than a score limit. List them in `settings.victory.conditions`:

- Destroy every unit with a tag (a capital, a commander): the game reports how many each team has
  left, and a team with none left is out.
- Hold an objective (a wonder, a relic) for a set time. Losing it starts the count again.
- Reach a score, such as economy or points the game feeds in.
- Eliminate the other teams.
- Surrender: a team votes, and when enough of its players say yes, it is out. A failed vote waits
  for a cooldown before the team may vote again.

Allied teams (`settings.elimination.alliances`) win together, including an ally who already fell.
The agent method `rules.match_report` feeds the game's counts, holders and scores, and
`rules.match_get` shows the zone, the placements and the victory state.

## Playlists and rotation

A playlist plays its rows in the order you list them. For a public server you can mix it up:

- **Shuffle.** `order: shuffle(seed: 7)` plays every row once per cycle, in a new order each cycle. The map that
  closed a cycle never opens the next one while another map fits.
- **Weights.** `order: weighted(seed: 7)` plays each row as many times per cycle as its `weight` (default 1, up to
  100; 0 leaves it out). Over a cycle every row plays exactly its weight.
- **Player counts.** `min_players` and `max_players` on a row keep a big map off a near-empty server and a small map
  off a full one. A row outside its range is skipped, and the next one that fits plays.
- **No repeats.** `no_repeat_last: 2` avoids the last two maps while another map fits.
- **Missing maps.** A row whose map or mode is not installed is skipped.

```ron
(
    format_version: 2,
    looping: true,
    order: shuffle(seed: 7),
    no_repeat_last: 2,
    entries: [
        (map_id: "levels/harbor.level.ron", mode: "ctf"),
        (map_id: "levels/canyon.level.ron", mode: "ctf", min_players: 8),
        (map_id: "levels/mesa.level.ron", mode: "koth", max_players: 6, weight: 2),
    ],
)
```

The seed decides the order, so every player in a lockstep match sees the same maps. When no row fits the players
you have, the current map plays again. The end-of-match map vote offers the next maps the rotation would play, by
the same rules. `server.rotation_get` lists the rows the last change of map skipped and why.

Lobbies show the running mode and playlist (`regia.mode`, `regia.playlist`), so a server browser can filter by mode
and matchmaking can look for a playlist. Older playlist files without these fields load unchanged. See
[Match modes and match flow](../agents/match-rules.md#rotation) for every field and reason code.

## Authoring modes in the editor

You can make and tune a mode without leaving the editor.

**Pick the level's rules.** In World Settings, Game Rules is a list. It shows the rules types your
game registers (and the engine's `DefaultRules` and `MatchSessionRules`), then your project's rule
sets: the shipped `tdm`, `ffa`, `ctf`, `koth` and `dom` and every file in `assets/rules/`. A rules
type your game registers in code shows up after you have played once in the editor. If you type a
name that is in neither list, the editor keeps it and shows a "custom" warning: the level will fall
back to the default rules when it loads. Choosing a value is one undo step.

**Edit a rule set.** Click **Edit rules…** next to Game Rules (or run "Open the Match Rules tab" from the
command palette), type a path such as `assets/rules/ctf_long.rules.ron` and click **Open or create**. The
form lists every field the format has. An empty field keeps the value of the mode you extend. Type a
value and leave the field to apply it. Every change is checked at once: a problem shows in red with
its code and the field it is about (for example `ruleset_option_range` when an option's default is
outside its `min` and `max`), and **Save** stays off until you fix it. Undo and redo work as for any
other edit, and saving never marks your level as changed.

**Edit a playlist.** In the same panel, open or create a `.playlist.ron`. Add rows, move them up and
down, remove them and switch looping. Each row is checked against its mode: an option the mode does
not declare, or a value out of range, blocks the save. A mode name the editor does not know is shown
as a warning only, because your game may handle it in code.

**See the mode in the level.** Show ▸ Icons ▸ Match rules draws player starts in their team's color
with their team and priority, flag bases with their touch radius (and height band), hill and capture
zone boxes, and while you play, each zone's capture progress and a score ring at every spawn point
(red is the point the spawn picker likes best).

**Drive the match while you play.** While play-in-editor runs, the Match menu on the play toolbar
(and the top of the Match Rules panel) has **Start now**, **Pause**, **Resume**, **End match**,
**Step** (a number of match ticks while paused), **+ Bot** and **− Bot** for a team, and **Next
row** to choose which playlist row starts next. Start now creates a match from your level's rule set
and your open playlist when the game has not made one.

Every one of these tools has an agent method, so a script or an agent can do the same steps: see
[Match rules in the editor](../agents/editor-match-rules.md).

## Which layer do I use

There are two ways to write a match, and most games need only one:

| Your game | Use | Why |
|---|---|---|
| Single player, a puzzle, a story, a simple score | The `GameRules` trait ([Gameplay framework](gameplay.md)) | A few hooks (level start, join, death, tick, over) and your own state. Nothing to configure. |
| A multiplayer mode: teams, flags, a hill, zones, rounds, ready-up | The match core (`regia::framework::rules`, this page) | Every rule is already written, tested and deterministic, and rule sets change it without code. |

The match core runs in one of two ways:

- **Server-authoritative** (a dedicated or listen server decides, clients show it): add
  `MatchSessionPlugin` on the server and `MatchStateReplicationPlugin` on both sides. The server
  steps the session from your entities; clients read replicated views. See
  [Server-authoritative games](#server-authoritative-games) and
  [Showing the match on clients](#showing-the-match-on-clients).
- **Lockstep** (every machine runs the same simulation from the same inputs): step a
  `MatchSession` yourself in your simulation, with contestants in a fixed order. Do not add the
  plugin. See [An independent match core](#an-independent-match-core).

A game can start with `GameRules` and move to the match core later: `MatchSessionRules` is itself a
`GameRules` implementation, so the hooks you already call keep working.

## Your first CTF match

A capture-the-flag match needs four things: the rules, the flags, the players and the steps. This
whole example runs as written. It uses the match core alone, with no entities, so you can see each
piece.

```rust
use std::collections::BTreeMap;

use regia::framework::rules::objective::{Contestant, RulesEvent};
use regia::framework::rules::teams::SeatRequest;
use regia::framework::ruleset_file::RuleSetRegistry;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// 1. The rules: the shipped `ctf` rule set, with no playlist options.
let registry = RuleSetRegistry::with_defaults();
let rules = registry.resolve("ctf", &BTreeMap::new())?;
let mut session = rules.session();
// Start the match now (no ready-up). Starting resets the board, so place the flags after it.
session.start_now();

// 2. The flags: one per team, 40 m apart. The ids are yours (they name the flag entities).
session.board.spawn_flag(901, 1, [-20.0, 0.0, 0.0])?;
session.board.spawn_flag(902, 2, [20.0, 0.0, 0.0])?;

// 3. The players: one seat on each team.
for team in [1, 2] {
    let request = SeatRequest {
        team: Some(team),
        ..SeatRequest::default()
    };
    session.assign_team(&request, &[])?;
}

// 4. The steps. Unit 1 (team 1) stands on team 2's flag, then carries it home. Unit 2 stays away.
let mut runner = Contestant { id: 1, team: 1, position: [20.0, 0.0, 0.0], alive: true };
let other = Contestant { id: 2, team: 2, position: [0.0, 0.0, 30.0], alive: true };
let mut captured = false;
for tick in 0..10 {
    if tick == 5 {
        runner.position = [-20.0, 0.0, 0.0];
    }
    let step = session.step(&[runner, other]);
    captured |= step
        .events
        .iter()
        .any(|event| matches!(event, RulesEvent::FlagCaptured { team: 1, .. }));
}
assert!(captured, "the carrier scored at home");
assert_eq!(session.board.score(1), 1);
# Ok(())
# }
```

In a real game the contestants are your players' pawns, in the same order every step, and the
positions come from your movement code. The capture limit (three by default) ends the match with a
`MatchOver` event. To play it with bots on a real level, follow the
[capture-the-flag tutorial](../tutorials/capture-the-flag.md).

## The same match on entities

On a server, let `MatchSessionPlugin` build the contestants from your entities. A pawn with
`TeamId`, `Transform` and `Health` takes part; `LedgerSlot` ties it to its seat's score row. An
entity with `ObjectiveEntity(id)` follows the flag or hill `id`, so a carried flag moves with its
carrier. Team starts are `PlayerStart`s with a `SpawnPoint` for their side.

```rust
use std::collections::BTreeMap;

use regia::app::enable_fixed_step;
use regia::core::math::Vec3;
use regia::framework::damage::Health;
use regia::framework::player::{PlayerStart, SpawnPoint};
use regia::framework::rules::teams::SeatRequest;
use regia::framework::rules::{LedgerSlot, MatchRules, MatchSessionPlugin, ObjectiveEntity, TeamId};
use regia::framework::ruleset_file::RuleSetRegistry;
use regia::prelude::*;
use regia_scene::Transform;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut app = App::new();
app.add_plugins((MinimalPlugins, MatchSessionPlugin));
enable_fixed_step(&mut app, 60.0);

let rules = RuleSetRegistry::with_defaults().resolve("ctf", &BTreeMap::new())?;
let mut session = rules.session();
session.start_now();
session.board.spawn_flag(901, 1, [-20.0, 0.0, 0.0])?;
session.board.spawn_flag(902, 2, [20.0, 0.0, 0.0])?;
let request = SeatRequest {
    team: Some(1),
    ..SeatRequest::default()
};
let seats = session.assign_team(&request, &[])?;
app.insert_resource(MatchRules { session });

let world = app.world_mut();
for (x, side) in [(-24.0, 1), (24.0, 2)] {
    world.spawn((Transform::from_translation(Vec3::new(x, 0.0, 0.0)), PlayerStart, SpawnPoint::for_team(side)));
}
for id in [901, 902] {
    world.spawn((Transform::default(), ObjectiveEntity(id)));
}
// Team 1's pawn, standing on team 2's flag.
let seat = seats[0];
let pawn = world
    .spawn((
        TeamId(seat.team),
        LedgerSlot(seat.slot),
        Transform::from_translation(Vec3::new(20.0, 0.0, 0.0)),
        Health::new(100.0),
    ))
    .id();

for _ in 0..3 {
    app.update();
}
let board = &app.world().resource::<MatchRules>().session.board;
assert!(board.objective(902).is_some_and(|flag| flag.carrier_id.is_some()), "the pawn took the flag");
# let _ = pawn;
# Ok(())
# }
```

Install `MatchSessionRules` as the game rules too, and joining players get seats and team spawns,
deaths reach the ledger and the scoreboard lists every player (see
[Server-authoritative games](#server-authoritative-games)).

## Rule sets and playlists in code

The `ctf_long` rule set from [Making a mode without code](#making-a-mode-without-code) loads and
resolves like this. A playlist row's options are checked against the rule set's option schema.

```rust
use std::collections::BTreeMap;

use regia::framework::ruleset_file::RuleSetRegistry;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut registry = RuleSetRegistry::with_defaults();
registry.load_text(
    "assets/rules/ctf_long.rules.ron",
    r#"(
    format_version: 1,
    id: "ctf_long",
    name_key: "mode.ctf_long",
    extends: "ctf",
    session: (
        flow: (start_delay_ticks: 300),
        phases: (time_limit_ticks: 36000, on_time_up: (kind: "overtime", ticks: 3600)),
        objectives: (capture_limit: 5, flag_return_ticks: 1800),
        respawn: (default: (kind: "delay", ticks: 180)),
    ),
    options: [
        (key: "sky", kind: enum, default: "day", choices: ["day", "dusk", "night"]),
    ],
)"#,
)?;

// A playlist row: `(map_id: "levels/harbor.level.ron", mode: "ctf_long", options: { "sky": "night" })`.
let row: BTreeMap<String, String> = [("sky".to_string(), "night".to_string())].into();
let rules = registry.resolve("ctf_long", &row)?;
assert_eq!(rules.chain, ["ctf", "ctf_long"]);
assert_eq!(rules.settings.objectives.capture_limit, 5);
assert_eq!(rules.settings.flow.start_delay_ticks, 300);

// A value the option does not allow is refused, with the option's name in the path.
let bad: BTreeMap<String, String> = [("sky".to_string(), "noon".to_string())].into();
let error = registry.resolve("ctf_long", &bad).err().ok_or("noon is not a sky")?;
assert!(error.path.contains("sky"), "{error:?}");

// Peers compare this hash at the content handshake.
let hash = rules.content_hash();
assert_ne!(hash, 0);
# Ok(())
# }
```

A project's own files load with `registry.load_project(project_root)` (every
`assets/rules/*.rules.ron`). The app keeps its registry as a resource, which the agent methods
`rules.ruleset_list` and `rules.ruleset_get` read.

## Phases in code

Warm-up, the time limit and rounds are tick counts in `PhaseSettings`. Convert seconds once, when
you build the settings:

```rust
use regia::framework::rules::phases::{PhaseSettings, TimeUp};
use regia::framework::rules::session::{MatchSession, SessionSettings};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
const HZ: u64 = 60;
let phases = PhaseSettings {
    warmup_ticks: Some(10 * HZ),
    time_limit_ticks: Some(180 * HZ),
    on_time_up: TimeUp::End,
    ..PhaseSettings::default()
};
phases.validate()?;
let mut session = MatchSession::new(SessionSettings::default());
session.set_phases(phases)?;
# Ok(())
# }
```

## Replication in code

The server adds `MatchStateReplicationPlugin` next to `MatchSessionPlugin`; a client adds the same
replication plugin and installs `ReplicatedMatchRules`. A HUD reads the replicated views, never
the session (a client has none):

```rust
use regia::framework::rules::{
    GameRulesResource, MatchStateReplicationPlugin, PlayerScoreView, ReplicatedMatchRules, TeamScoresView,
};
use regia::prelude::*;

/// A client: the replicated match views and a scoreboard from them.
fn client(app: &mut App) {
    app.add_plugins(MatchStateReplicationPlugin)
        .insert_resource(GameRulesResource {
            rules: Box::new(ReplicatedMatchRules),
        });
}

/// One row per player as `(team, kills, deaths)`, and the team totals.
fn scoreboard(world: &mut World) -> (Vec<(u8, u32, u32)>, Vec<u32>) {
    let mut rows = world.query::<&PlayerScoreView>();
    let players = rows.iter(world).map(|v| (v.team, v.kills, v.deaths)).collect();
    let mut teams = world.query::<&TeamScoresView>();
    let totals = teams.iter(world).next().map(|t| t.scores.clone()).unwrap_or_default();
    (players, totals)
}
# fn main() {
#     let mut app = App::new();
#     client(&mut app);
#     let _ = scoreboard(app.world_mut());
# }
```

`samples/net_game` is a complete client and server built this way: two teams, team starts, and
the scores every client reads from these views (its `net_game_two_clients_see_the_same_scores`
test runs a server and two clients and compares what each one sees).

## Match history

When the match ends, `MatchOver` arrives as a `MatchEvent`: read the final standings then if your
game keeps its own history. The engine can also keep a record of each match for you (see
[Match history and stats](#match-history-and-stats)).

```rust
use regia::framework::rules::objective::RulesEvent;
use regia::framework::rules::{MatchEvent, MatchRules, match_session_score_sheet};
use regia::prelude::*;

/// Each finished match as `(winning team, rows on the score sheet)`.
#[derive(Resource, Default)]
struct History(Vec<(u8, usize)>);

fn record_results(
    mut events: MessageReader<MatchEvent>,
    rules: Option<Res<MatchRules>>,
    mut history: ResMut<History>,
) {
    for MatchEvent(event) in events.read() {
        if let (RulesEvent::MatchOver { team, .. }, Some(rules)) = (event, rules.as_ref()) {
            let sheet = match_session_score_sheet(&rules.session);
            history.0.push((*team, sheet.rows.len()));
        }
    }
}
# fn main() {
#     let mut app = App::new();
#     app.init_resource::<History>().add_systems(Update, record_results);
# }
```

## Match history and stats

When a match ends, the host can keep a record of it: one small JSON file per
match in a history folder, sorted into one subfolder per day (UTC). The record
holds the map and the mode, how and when the match ended, who won (or that it
was a draw), each team's score and round wins, and each seated player's kills,
deaths, assists, objective points, score and the game's own named counters. It
also names the replay file when the match was recorded.

The game turns this on by adding the match results plugin and choosing the
history folder. Records older than a set number of days (90 by default) and
records beyond a set count (1000 by default) are removed after each new one.
Writing happens in the background, so a slow disk never stalls the match, and a
power cut mid-write leaves either the whole record or nothing.

Records never contain player names. Players are
identified by their platform account id, or by their seat when the game has
none. A server that identifies players by name stores a scrambled form of the
name instead. Records never contain IP addresses.

The same record can update leaderboards and platform stats (for example, add
this match's kills to a lifetime kills stat) and can be passed to the game's own
code, such as an upload to the game's website.

To look at the history, run `regia server results` (add `--since 2026-11-18`
for a day onward), or use the agent methods `rules.match_results` and
`rules.match_result` ([Match results](../agents/match-results.md)).

## File formats and versions

Three files hold a game's rules data. Each one names its version, and each version is frozen. A later change
comes with a new version, and older files keep loading.

| File | Version | Format reference |
|---|---|---|
| A mode: `assets/rules/<id>.rules.ron` | 1 | `docs/formats/rules-ron.md` |
| A server playlist: `<name>.playlist.ron` | 2 (version 1 still loads) | `docs/formats/playlist-ron.md` |
| A finished match: `<history>/<day>/<match id>.json` | 1 | `docs/formats/match-result.md` |

All three refuse a field they do not know, and the error names it. A misspelled setting is an error, never a
setting that silently keeps its default.

Playlist version 2 has the same fields as version 1. It also refuses an unknown field inside `order`. To rewrite
your version 1 playlists as version 2, run `regia upgrade-project --write`. Running it again changes nothing. A
server still on v0.6.0 cannot read a version 2 playlist, so keep version 1 files for those servers.

Rule sets and match records are at their first version, so there is nothing to upgrade.
`regia upgrade-project` checks each rule set and reports the version of each match record.
