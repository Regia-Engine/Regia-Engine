# Net desync diagnosis (agents)

A lockstep match stays correct only while every simulation steps to the same state. `net.lockstep`
([Net lockstep (agents)](net-lockstep.md)) says that a checkpoint's hashes differed and who was resynced. This page is
the next step (spec 120 § 7.6, ADR-8455): seeing what the probe found, getting the `*.desync.ron` bundle, replaying it
offline to the first diverging tick and subsystem, and planting a divergence on purpose to check that all of this
works.

## `net.desync`

`net.desync {endpoint?}` reports every lockstep host and seat the game published (`publish_host` / `publish_seat`
for `net.lockstep`, and `NetAgentState::publish_desync(label, DesyncStatus)` for the diagnosis). Without `endpoint` it
lists all of them.

```json
{ "endpoint": "host" }
→ { "endpoints": [{ "label": "host", "reports": 1,
      "last": { "checkpoint": 120, "voter": { "seat": 2 }, "reference": { "seat": 0 },
                "first_diverged_tick": 100, "last_agreed_tick": 99,
                "subsystem": "economy", "subsystems": ["economy"] },
      "rows": [{ "voter": { "seat": 0 }, "rows": { "names": ["units", "economy", "orders", "rng"],
                  "rows": [{ "tick": 99, "world": 81, "hashes": [1, 2, 3, 4] }] } }],
      "bundle": "saved/desync/game-host-120.desync.ron",
      "replay": { "game_id": "studio.tactics", "ticks": [91, 125], "first_diverging_tick": 100,
                  "voter": { "seat": 2 }, "subsystem": "economy", "subsystems": ["economy"],
                  "last_agreed_tick": 99, "peers": [], "live": null, "matches_live": true },
      "pending_injections": [] }] }
```

| Field | Meaning |
|---|---|
| `reports`, `last` | Probe reports so far and the newest one (the live verdict): the diverged voter, the voter it was compared with, the first diverged tick and subsystem, the last agreed tick. |
| `rows` | The newest probe's subsystem rows, per voter in ascending order: the diverged voters' and the reference's, around each first diverged tick. |
| `bundle` | The path of the newest bundle the game wrote (`DesyncStatus::with_bundle`), or `null`. |
| `replay` | The newest offline replay verdict the game ran (`ReplayRegistry::replay`), or `null`; `matches_live` says whether it names the same voter, tick and subsystem as the live probe. |
| `pending_injections` | Injections queued by `net.desync_inject` that the game has not taken yet. |

## `net.desync_inject` (development and test builds)

`net.desync_inject {subsystem, tick, mask?, endpoint?}` queues a flip of one subsystem at a chosen tick for one
lockstep host or seat, or for every published one. The game takes it in its step with
`NetAgentState::take_desync_inject(label)` and applies it with `DesyncInject::flip` (the same hook the engine's tests
use). A release build refuses the method with `desync_inject_disabled`: a shipped match never plants a divergence.

```json
{ "endpoint": "seat2", "subsystem": "economy", "tick": 100 }
→ { "queued_for": ["seat2"], "inject": { "subsystem": "economy", "tick": 100, "mask": 6510731453856192150 } }
```

## Checks

Plant a divergence and see it named, diagnosed and replayed:

1. `net.desync_inject {endpoint: "seat2", subsystem: "economy", tick: 100}`.
2. Step past the next hash tick (`hash_every`, 30 by default): `play.step` or wait.
3. `net.lockstep {endpoint: "host"}`: `desyncs` is 1 and seat 2 shows `resyncs` 1.
4. `net.desync {endpoint: "host"}`: `last.first_diverged_tick` is 100 and `last.subsystem` is `economy`; once the game
   wrote and replayed its bundle, `replay.matches_live` is `true`.
5. Offline, on any machine with the build that wrote it: `regia net desync-replay <bundle>` names tick 100 and
   `economy`.

## The bundle and the offline replay

The host writes a bundle (`DesyncBundle::from_host(&host, snapshot)`) when the probe reports
(`HostEvent::DesyncProbed`); `snapshot` is the game's own state at the verdict's last agreed checkpoint. A diverged seat
can write its own (`DesyncBundle::from_seat`) when it gets `SeatEvent::Resync`, before it takes the majority's state,
with its state at the `from` of the `SeatEvent::SelfCheck` before it. The format is
`docs/formats/desync-ron.md`.

`regia net desync-replay <bundle> [--json]` loads the snapshot into the `DesyncReplay` hook registered for the bundle's
game, steps every frame (seats ascending), hashes after every step and compares each peer's rows with the replay's
(peers ascending): the earliest tick a peer differs at is the first diverging tick. The `regia` binary registers the
engine's sample hook (`regia.desync_sample`); a game replays its bundles from its own tool binary with a
`ReplayRegistry` holding its hook (see [Networking](../user-guide/networking.md#desync-bundles)). Exit code 0 when it
replayed (with or without a divergence), 1 when the bundle was refused or the hook failed.

## Error codes

| Code | When |
|---|---|
| `desync_unknown_endpoint` | No published lockstep host or seat has that label (`net.desync`, `net.desync_inject`). |
| `desync_no_endpoint` | `net.desync_inject` with nothing published to inject into. |
| `desync_inject_disabled` | `net.desync_inject` in a release build. |
| `desync_inject_invalid` | `net.desync_inject` without a subsystem name (1 to 64 bytes) or a tick, or with a zero `mask`. |
| `desync_bundle_too_large` | `regia net desync-replay`: the bundle is over 64 MiB. |
| `desync_bundle_parse` | The file is not a bundle (bad RON, a hex string that is not whole bytes). |
| `desync_bundle_version` | The bundle's `format_version` is not 1; replay it with the engine version that wrote it. |
| `desync_bundle_invalid` | A count, length or order is out of range (a frame gap, a seat listed twice, too many rows). |
| `desync_bundle_game_mismatch` | The hook replays another game than the bundle's. |
| `desync_bundle_game_version` | The bundle was written by another version of the game (protocol version or protocol id); the message names both. |
| `desync_replay_no_hook` | No `DesyncReplay` hook is registered for the bundle's game; the message lists the registered ones. |
| `desync_replay_failed` | The hook could not load the snapshot or step a frame; the message names the tick. |
| `desync_bundle_io` | The file could not be read or written. |
