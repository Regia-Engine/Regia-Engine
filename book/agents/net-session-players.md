# Net session players (agents)

What a session does with remote players once they are in (spec 77 § 3.2, § 9, § 10.2; ADR-2413): the game rules
hear about joins and leaves, a leaving peer's entities are handed on, and ping and loss per player are a resource,
an agent method and an in-game overlay. Server side unless noted. Sessions themselves are on
[Net session](net-session.md).

## Game rules

With `regia_framework`'s `net` feature, `RulesPlugin` adds `NetSessionBridgePlugin` (add it yourself in an app
without `RulesPlugin`). Then, on the server:

- **Join.** A remote player reaching `InGame` calls `GameRules::on_player_joined(ctx, player)` with its `NetPlayer`
  entity, **once per player for the whole session**: a travel or a reconnect resume reaches `InGame` again and does
  not call it again. A returned `SpawnRequest` spawns the character the way a local join does (transform, the
  character components, `Team` when set), owned by the player's peer (`NetOwner`); `NetRulesJoined { pawn }` on the
  player entity names it. Return `None` to spawn nothing (a spectator). Replicating the character (`Replicated`) is
  the game's choice.
- **Leave.** Leaving the session (`Gone`, or one player kicked from a split-screen peer) calls
  `GameRules::on_player_left(ctx, player)` once, after `SessionRules::on_player_left_reason` and before the entity is
  despawned, and only for a player whose join was called (a peer that leaves while loading never joined).
- Local players keep the `PlayerManager` flow. Clients never call these hooks.
- **Remote players are `Player`s (spec 70 § 4).** With a `PlayerManager` resource, a joining peer's `NetPlayer`
  entity gets `Player { kind: Remote, controller, .. }` at its welcome (before `InGame`) and a `PlayerController`
  entity of its own; it takes the next free player index and no local split-screen seat. Leaving frees the index and
  despawns the controller (after `on_player_left`). The controller persists across travel. The client's `LocalPeer`
  is its own peer id from the welcome (`None` before it and after a disconnect).
- **Possession sets the owner (spec 70 § 5.2).** `PlayerController::possess(pawn)` on a remote player's controller
  makes the pawn owned by that player's peer (`NetOwner`) in the same frame's `PostUpdate`, before the next
  replication send; possessing another pawn or `unpossess()` removes the owner it gave (an owner the game changed
  since is left alone). `NetPossessionOwner` on the controller names the pawn. Local players and bots are not
  touched.

Other crates can hook in the same way: `NetPlayerHooks::add_welcomed` / `add_joined` / `add_left` take a
`fn(&mut World, Entity, NetPlayerId)`; the session calls every `joined` hook each time a player reaches `InGame`
(a hook that must run once keeps its own mark, as the rules bridge does with `NetRulesJoined`).

## Owned-entity hand-off

When a peer reaches `Gone`, after the leave hooks of its players ran, every entity whose **own** `NetOwner` names it
is handed on. `SessionRules::owned_hand_off(world, entity, peer, live)` decides; the default rule is:

1. The entity has `NetSuccessor(peer)` and that peer is still in the session (any state but `Gone`, the grace
   included): that peer owns it now (`NetOwner(successor)`).
2. Otherwise, an entity marked `NetDespawnWithOwner` is despawned (KI-GP-033, ADR-10070).
3. Otherwise (no successor, a successor that left or is unknown, or `NetSuccessor(PeerId::SERVER)`): the server owns
   it now (`NetOwner` is removed).

A rule may return `OwnedHandOff::Despawn` to despawn the entity instead, and a rule that names a peer not in the
session hands to the server. Entities that borrow ownership (`NetOwnerLink`, a transform parent) follow their owner
and are not visited. The character the game rules spawned for the player at its join carries `NetDespawnWithOwner`,
so it leaves with the player (a kick, a quit or a timeout). To keep it, remove the marker in `on_player_left` (it
then stays, server-owned) or give it a live `NetSuccessor`. A takeover bot that drives a dropped player's character
lifts the marker while it drives, so the character stays with the bot if its player is gone, and puts it back when
it hands the character back. Each hand-off logs `net.owned_hand_off` and is kept in `NetHandOffs` (the last 256).

## Ping and loss

`NetPlayerStats` (resource, refreshed every 250 ms, `refresh_ms`) holds one `PlayerLinkStats` per remote player on a
server, or `server` (the link to the server) on a client. The numbers are the transport's own estimates, nothing new
is measured: a datagram peer's connection `LinkStats` (smoothed round trip, RTT variation as jitter, loss of what the
server sent, `loss_from_player: None`), or a loopback or simulated client's meters (both directions). Players of one
peer share its link's numbers.

## Stats overlay

`NetStatsOverlay { visible }` switches the in-game overlay, **off by default**. With the `client` feature,
`NetStatsOverlayPlugin` (added by the bridge) draws a panel in the top-right corner with the runtime UI: the session
role, then per player (or for the server link) state, ping ± jitter and loss to / from the player in percent. The
text is rebuilt only when the stats refresh (4 Hz); hidden, it costs one resource read per frame. There is no
default key: bind one to `NetStatsOverlay::toggle`.

## Methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.player_stats` | read | — | `{ role, now_ms, refreshed_ms, players: [{ player: "peer.local", peer, local, state, ping_ms, jitter_ms, loss_to_player, loss_from_player, simulated }], server }` |
| `net.stats_overlay` | play | `{ visible? }` (absent flips it) | `{ visible, lines }` |
| `net.handoffs` | read | — | `{ total, records: [{ entity, from, to: server\|peer\|despawn, peer }] }` |

`net.player_stats` reads the transport live; `refreshed_ms` is the last refresh of the resource the overlay draws.

## Errors

`F0002` with `data.reason`:

| Reason | When |
|---|---|
| `net_not_running` | The app has no `NetSession` (add `regia_net::NetSessionPlugin`). |
| `invalid_value` | `net.stats_overlay`: `visible` is not a boolean. |

## Connectionless server bots

`regia_framework::net_bots::NetBotsPlugin` fills a server to `min_players`, capped by `max_bots`, and
`regia_ai::net_bots::ServerBotAiPlugin` drives each pawn with a behaviour tree, perception and navigation.
Bots have no connection or input stream. Their server-owned pawns write `CharacterInput` before the same
character step used for remote players, so replication, relevance, lag compensation and `GameRules` apply
normally. Bot state still replicates to real clients; “zero send bandwidth” means there is no bot link or
per-bot transport queue.

A human join replaces a filler bot only after `join_delay_ticks` remains stable; a vacancy is filled after
`leave_delay_ticks`. Initial fill is immediate. During reconnect grace a bot drives the dropped player's
existing pawn, preserving its transform, movement state, game score and owner. Resume removes the bot driver
and returns that same pawn to the player's command stream. If grace expires, the bot becomes an ordinary
filler and remains subject to policy. Difficulty is deterministic for `seed`: `reaction_ticks`,
`aim_error_radians` and `perception_radius` can be changed per bot.

`session.state` has a combined `players` array (`player`, `entity`, `pawn`, `is_bot`, `takeover_of`) in
addition to connection-oriented `peers`. `net.bots` requires the `play` permission and a running server.

| Action | Params | Result |
|---|---|---|
| `list` (default) | — | `{ bots, players }` |
| `add` | optional difficulty fields | `{ added, bots, players }` |
| `remove` | `id` | `{ removed, bots, players }`; a takeover hands back, a filler is despawned |
| `set_difficulty` | `id`, any of `reaction_ticks`, `aim_error_radians`, `perception_radius` | updated rows |

```json
{ "method": "net.bots", "params": { "action": "add", "reaction_ticks": 4, "aim_error_radians": 0.05 } }
{ "method": "net.bots", "params": { "action": "set_difficulty", "id": 1, "perception_radius": 25 } }
{ "method": "net.bots", "params": { "action": "remove", "id": 1 } }
```

Additional `F0002` reasons:

| Reason | When |
|---|---|
| `net_not_server` | The session is idle or a client. |
| `bots_unavailable` | `NetBotsPlugin` is not installed. |
| `bot_limit` | Adding would exceed `max_bots`. |
| `bot_unknown` | `id` does not name an active bot. |
| `player_slot_unavailable` | The framework cannot allocate a player slot or bot id. |
| `invalid_value` | An action, id or difficulty value is invalid. |

`server.toml` accepts additive `[bots]` keys `min_players`, `max_bots`, `join_delay_ticks`,
`leave_delay_ticks`, `seed`, `reaction_ticks`, `aim_error_radians` and `perception_radius`. The net_game
server accepts `--bots N`, which sets both `min_players = N` and `max_bots = N`.

## Match end and rotation

A dedicated host ends the match from the session rules and then travels to the next playlist row
(ADR-8345). `RulesPlugin` (feature `net`) installs `GameRulesSessionRules` when the app has not
called `set_session_rules`. An explicit `set_session_rules` wins, before or after the plugin.

`GameRulesSessionRules::match_over` is true when the world holds `MatchRules` and
`session.board.match_over` is set, or when `GameRules::is_match_over` is true. After a travel,
`on_travel_finished` calls `GameRules::on_level_start`.

The host waits `levels.match_end_delay_s` on its own clock, then calls `next_level`. A row that
fails to load is logged as `server_next_level_failed` with the map and mode, emitted as
`HostEvent::NextLevelFailed`, and skipped. The walk tries each following row once and stops when
it would return to the current row. It does not spin. The current level stays when every following
row fails.

`levels.rotation` is a list of map-only rows (`mode` empty: the level's own rules). `levels.playlist`
is a `.playlist.ron` of map, mode and options rows. The two keys together are a config error on
`levels.playlist`. A playlist row's mode is written to `LevelSettings.game_rules` and its options
to the `MatchOptions` resource. An empty mode leaves the level's own rules.

`server.rotation_get` returns `{ rows: [{ map_id, mode, options }], index, looping, order, skipped, last_failure,
rules_hash, rules_in_manifest }`. `order` is `in_order`, `shuffle` or `weighted`, and `skipped` lists the rows the last travel passed over as
`{ index, map_id, mode, reason }` with `reason` `too_few_players`, `too_many_players`, `missing_content` (the level
is not in the pak), `load_failed`, or the `ruleset_*` code of a row whose rule set does not resolve (ADR-8897,
ADR-8881, [match rules § Rotation](match-rules.md#rotation)).

**Rule sets on a dedicated host (ADR-8881).** With `NetSessionBridgePlugin` (or any `HostGameHooks::set_rule_sets`
source) the host reads the game's rule sets (`RuleSetRegistry`: the shipped defaults plus `assets/rules/`) and
computes the playlist's combined rules hash (`RuleSetLibrary::playlist_rules`); `rules_hash` shows it as 16 hex
digits (null without rule sets, or when no row names a rule set). With `HostGameHooks::set_manifest_rules(true)`
(`rules_in_manifest: true`; default off) the host puts the hash in the session manifest before it hosts
(`NetSession::set_rules_hash`), so a client whose rule sets differ is refused with `content_mismatch`
(`pack:@rules: different`). Turn it on only when the game's clients set the same hash (`playlist_rules` over the
same playlist): a client that sets none is refused too. Only rows whose `mode` names a rule set are resolved: an empty mode, a game's registered rules name and a
bare skeleton id with no rule set run as before. A row whose rule set does not resolve with its options is passed
over on a travel (`ruleset_option_range`, `ruleset_option_kind`, `ruleset_invalid`, `ruleset_unknown` for a missing
parent, ...); a starting row that does not resolve stops the start with exit code 4 (content). A row option the rule
set does not declare is left to the game (`MatchOptions`) and is not part of the hash.
`server.rotation_skip` asks the host to travel on its next tick and returns `{ skipped: true }`.
Both need a dedicated host. Without one they return `server_no_rotation`.

```json
{ "method": "server.rotation_get", "params": {} }
{ "method": "server.rotation_skip", "params": {} }
```
