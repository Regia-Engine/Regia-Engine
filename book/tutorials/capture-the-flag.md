# Tutorial: capture the flag

This tutorial turns the [arena](arena.md) into a two-team capture-the-flag match with two bots
per side. You write no rules: the match is the engine's match core, configured by the shipped
`ctf` rule set and stepped from the arena's entities by `MatchSessionPlugin`. Every step is checked
through the agent protocol (`rules.match_get`, `play.expect`), the way an agent or a CI job would
check it.

Read [Game modes](../user-guide/game-modes.md) first if the words *rule set*, *seat* or
*contestant* are new.

## Run it

The arena is one of the engine's samples, so this page works in the engine folder:

```console
$ cargo run --release -p arena -- --mode ctf --no-local-player --bots 4 --seed 7
$ cargo run --release -p arena -- --mode ctf --bots 3
$ cargo run --release -p arena -- --mode koth --no-local-player --bots 4
$ cargo run --release -p arena -- --mode ctf --ruleset ctf_quick --no-local-player --bots 4
```

`--mode` takes `ffa` (the arena's own free-for-all, the default), `tdm`, `ctf` or `koth`. The first
command is the match this tutorial follows: four bots, two red and two blue, each team with a flag
at its end of the arena. The second puts you in as the fourth player (on red). `--ruleset` names
another rule set for the mode, here the arena's own `ctf_quick`; `--limit N` changes only the
capture or hill score limit.

## The rules come from a rule set

A team mode starts with `arena::modes::resolve_mode`, which asks the app's rule-set registry for
the mode's rule set: the shipped `ctf` here. `team_session` then builds the match from it: two
teams with a seat for every player, the flags on the board, and the rule set's phase settings
where it sets them, with the arena's warm-up and timer filling the fields it leaves unset
(`arena::modes::merged_phase_settings`).

```rust
use arena::ArenaConfig;
use arena::modes::{ArenaMatchMode, ArenaMode, resolve_mode, team_session};
use regia::framework::rules::mode::MatchMode;
use regia::prelude::*;

# fn main() -> Result<(), String> {
let world = World::new();
let mode = ArenaMatchMode::new(ArenaMode::Ctf);
let rules = resolve_mode(&world, &mode)?;
assert_eq!(rules.id, "ctf");

// Five player slots: the (absent) local player and four bots.
let session = team_session(&rules, &ArenaConfig::default(), &mode, 5, 60.0);
assert_eq!(session.board.mode, MatchMode::CaptureTheFlag);
assert_eq!(session.board.objectives.len(), 2, "one flag per team");
assert_eq!(session.flow.seated.len(), 5);
# Ok(())
# }
```

Slot `s` sits on team `1 + s % 2` in ledger slot `s / 2` (`arena::modes::seat_of_slot`), so the
teams alternate: slot 0 (you) and the even bots are red, the odd bots blue.

To play your own variant, write a rule set that extends `ctf`. The arena ships one,
`samples/arena/assets/rules/ctf_quick.rules.ron`, which wins on the first capture and sends a
dropped flag home after ten seconds:

```ron
(
    format_version: 1,
    id: "ctf_quick",
    name_key: "mode.ctf_quick",
    extends: "ctf",
    session: (
        objectives: (capture_limit: 1, flag_return_ticks: 600),
    ),
)
```

The arena loads it into the registry at start-up (`arena::modes::load_arena_rule_sets`), next to
the shipped defaults. Check a rule-set file with `regia rules check <file>` before you ship it.

## The level: team starts and flags

In a team mode the arena places four `PlayerStart`s behind each team's flag, each with a
`SpawnPoint` for its side (`SpawnPoint::for_team(1)` on the west, `2` on the east). A pawn that
respawns asks `choose_spawn` for a start, so it only ever comes back on its own side, picked by the
rule set's spawn policy.

Each flag is an entity with an `ObjectiveEntity` id (901 for red, 902 for blue) and a colored
pole. The arena never moves a flag itself: after each step the session's adapter writes the flag's
position onto that entity, so a carried flag follows its carrier and a dropped one stays where it
fell.

A dead pawn stays down for 1.5 seconds (`RESPAWN_DELAY_TICKS`) before it respawns. That matters
for the match: during that time the session sees the carrier dead, so the flag drops where it fell
and a defender can touch it to send it home.

## The bots play the mode

The bots do not know the rules either. Each fixed step, `arena::modes::drive_mode_bots` asks the
match core for one order per bot through `BotHooks`: a goal point and the enemy to shoot at. In
capture the flag, the first bot of each team attacks (runs to the enemy flag, then home with it)
and the second defends (stands on its own flag, or runs to a dropped one to return it). The arena
only walks each bot toward its goal and fires a rifle when the enemy is within 6 m. An attacker
without a flag runs for it and does not stop to shoot. Each shot is aimed true half the time, drawn
from the match's seeded generator (`MatchRng`): on a mirrored map, bots that always hit would trade
every fight evenly and nobody would ever score.

```rust
use arena::ArenaConfig;
use arena::modes::{ArenaMatchMode, ArenaMode, resolve_mode, team_session};
use regia::framework::rules::bots::{AiLevel, BotHooks, CtfRole, contacts_of};
use regia::framework::rules::objective::Contestant;
use regia::prelude::*;

# fn main() -> Result<(), String> {
let world = World::new();
let mode = ArenaMatchMode::new(ArenaMode::Ctf);
let session = team_session(&resolve_mode(&world, &mode)?, &ArenaConfig::default(), &mode, 4, 60.0);

// Two red bots (ids 1, 2) and two blue bots (ids 3, 4) at their ends of the arena.
let unit = |id, team, x| Contestant { id, team, position: [x, 1.6, 0.0], alive: true };
let units = [unit(1, 1, -10.0), unit(2, 1, -9.0), unit(3, 2, 10.0), unit(4, 2, 9.0)];
let hooks = BotHooks { level: AiLevel::Medium, world_seed: 0, free_for_all: false };
let bodies = hooks.bodies(&session.board, &units, |_| 1.0);
let orders = hooks.orders(&session.board, 0, &bodies, &contacts_of(&bodies), &[]);

let red_attacker = orders.iter().find(|(id, _)| *id == 1).map(|(_, order)| *order);
assert_eq!(red_attacker.and_then(|order| order.ctf), Some(CtfRole::Attack));
// Its goal is the blue flag, at the east end.
assert!(red_attacker.is_some_and(|order| order.goal[0] > 0.0));
# Ok(())
# }
```

## Drive it with the agent

Start a match the agent can reach (the arena advertises its endpoint in its own sample directory,
hence `--project samples/arena`). `--limit 1` ends the match on the first capture, so the walk-through
is short, and `--warmup 0` skips the default 3 s warm-up (`ArenaConfig::warmup_secs`), so the match
is live at once and the five-second step below already shows a stolen flag:

```console
$ cargo run -p arena -- --mode ctf --limit 1 --no-local-player --bots 4 --seed 7 --warmup 0 &
$ regia agent call play.pause --project samples/arena
```

**1. The mode and the rule set.** `ArenaModeStatus` on the `"match"` entity names them; if the rule
set did not resolve, `error` says why and the arena plays the free-for-all instead.
`ArenaBrainStatus.brain` is `mode`: the match core's orders drive the bots.

```console
$ regia agent call play.expect '{"assertions":[{"name":"ctf from the shipped rule set","condition":{"entity":{"component":"ArenaModeStatus"},"component":"ArenaModeStatus","path":"ruleset","op":"eq","value":"ctf"}}]}' --project samples/arena
```

**2. The session.** `rules.match_get` reads the hosted session: `mode` is `"ctf"`, `board.objectives`
holds the two flags (`entity_id` 901 and 902, `at_base: true`), `flow.seated` the five seats and
`board.team_scores` the score per team (index = team id).

```console
$ regia agent call rules.match_get --project samples/arena
$ regia agent call seeing.screenshot --out ctf-start.png --project samples/arena
```

**3. Play a while and look.** Step five seconds and read the session again:

```console
$ regia agent call play.step '{"frames":300}' --project samples/arena
$ regia agent call rules.match_get --project samples/arena
$ regia agent call seeing.screenshot --out ctf-5s.png --project samples/arena
```

A flag has left its base by now: its objective has `at_base: false`, and a `carrier_id` while a bot
carries it. In the screenshot the pole stands at the carrier, not at its base.

**4. The end.** Let the match run until the arena shows it is over, then read the result:

```console
$ regia agent call play.resume --project samples/arena
$ regia agent call play.wait_until '{"condition":{"entity":{"component":"ArenaState"},"component":"ArenaState","path":"phase","op":"eq","value":"MatchOver"},"timeout_ms":600000}' --project samples/arena
$ regia agent call rules.match_get --project samples/arena
```

`board.end_reason` is `capture_limit_reached` and one team's `board.team_scores` entry is 1. Without
`--limit`, the shipped `ctf` rule set plays to three captures; a match that runs out of time ends on
the timer, and the team with more captures wins.

The cargo test `the_ctf_tutorial_steps_pass_through_the_agent` (`samples/arena/tests/ctf_agent.rs`)
runs exactly these steps against a headless arena, with offscreen screenshots.

## Test it

The tutorial's match is also a functional test placed in the level (spec 41): `arena_ctf_bots`,
tagged `ctf`, passes when the session reaches post-match on its capture limit. The cargo test
`arena_ctf_bots_finish_a_match_headless` (`samples/arena/tests/modes.rs`) runs it in process with no
window, bounded by a frame count:

```console
$ cargo test -p arena --test modes
```

`the_ctf_match_is_the_same_on_two_runs` in the same file plays thirty seconds twice with one seed
and compares the sessions: the bots' orders and every rule are deterministic.

## Try it

- `--mode koth`: the same bots, one hill in the middle. The bots contest it, and a team scores one
  point per second while only it stands there. Two equal bots per side seldom clear the hill, so
  try `--mode koth --limit 3` or more bots.
- `--mode tdm`: no objectives; the first team to `ArenaConfig::score_limit` kills wins.
- Change `MODE_FIRE_RANGE` or `MODE_HIT_CHANCE` in `samples/arena/src/modes.rs` and watch how hard
  it gets to reach a guarded flag. At a hit chance of 1.0 the mirrored teams cancel out and the
  match ends on the timer.
- Write a `ctf_long` rule set (see [Making a mode without code](../user-guide/game-modes.md#making-a-mode-without-code)),
  add it to `ARENA_RULE_SETS`, and run it with `--ruleset ctf_long`.

For a client/server game with the same match core, read `samples/net_game`: two teams, team starts
and the replicated score views every client shows
([Showing the match on clients](../user-guide/game-modes.md#showing-the-match-on-clients)).
