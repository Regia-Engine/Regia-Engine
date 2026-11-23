# Net lockstep replay (agents)

A lockstep match is its inputs: every participant steps the same frames from the same start. A
`.lsreplay` file (ADR-8513) keeps exactly that, written by the lockstep host or a seat as the match
runs, plus the state at intervals (Full records) and the agreed checkpoint hashes, so the match can
be re-run later and checked bit for bit. This page covers recording, the library folder, the audit
(`regia net replay-audit`, `net.replay_audit`) and listing (`net.replays`).

It is not the replication demo recorder (`.netdemo`, [Network replay (agents)](net-replay.md)) and not the desync
bundle ([Net desync (agents)](net-desync.md)): a bundle is the window around one divergence, a
`.lsreplay` is the whole match.

## Recording

Recording is opt-in: `LsReplaySettings { record: true, .. }`. The participant that records (a host
with a simulation, or a seat) makes an `LsReplayWriter` with an `LsReplayHeader::of::<Game>(seats,
tick_rate, seed, &settings)` and then:

1. `record_start(&state, |out| /* the game's state bytes */)` once before the first frame;
2. per tick, `record_frame(&frame)` with the agreed frame, then `record_state(tick, &state, save)`
   after stepping it: a Full every `full_every` ticks, the hashes every `hash_every_ticks`;
3. `finish(abort)` at the end, with `Some(ReplayAbortInfo::new(tick, "desync", reason))` when the
   match aborted.

| Setting | Default | Meaning |
|---|---|---|
| `record` | `false` | Record at all. |
| `tier` | `standard` | `standard`: 256 MiB cap, a Full twice a second; `low`: 64 MiB, a Full once a second. |
| `full_every_ticks` | the tier's rate | Ticks between Fulls (30 at 60 Hz on `standard`). |
| `hash_every_ticks` | 30 (the lockstep checkpoint interval) | Ticks between Hashes records; 1 names the exact diverging tick, a larger value the first checkpoint after it. |
| `max_bytes` | the tier's cap | At the cap the writer sheds Fulls first, then stops at a whole tick; the abort record is always written. |
| `keep_newest` | 20 | Replays the library keeps (`ReplayLibrary::rotate`). |

The inputs in the file are the bytes each seat sent (the game's `NetCodec`), never re-encoded. A
relay host has no simulation: it can record frames only, which the audit cannot check.

## The library

`ReplayLibrary::new(dir).keep_newest(n)` manages a folder of `.lsreplay` files:

- `list()`: readable replays newest first (by modification time), damaged files apart with their
  error code. Listing reads each header and walks the record frames without reading a payload.
- `rotate()`: deletes the replays older than the newest `n`; a replay holding an abort record (a
  diverged or aborted match) and a damaged file are never deleted.
- `import(path)`: decodes every record, refuses bytes the folder already holds (SHA-256, under any
  name) with `replay_duplicate`, and copies the file in with its date kept (`<stem>-2.lsreplay` beside
  a same-named one).
- `export(file_name, dir)` / `export_all(dir)`: copies a replay out with a `<stem>.json` sidecar
  (`regia-lsreplay-meta/1`: the game, version, seats, tick rate, seed, whether it aborted, the
  SHA-256).

A game publishes its folder for the agent with `NetAgentState::set_replay_dir("saved/replays")`.

## `regia net replay-audit`

`regia net replay-audit <file> [--json]` re-runs the replay through the `DesyncReplay` hook
registered for its game (the `regia` binary registers the engine's sample, `regia.desync_sample`; a
game's tool binary registers its own and calls `regia_net::lockstep::replay::audit_with`). It starts
from the first Full, steps every frame, compares the world hash and every subsystem hash of each
Hashes record bit for bit, and at each later Full compares the replayed state and then loads the
recorded one, so one divergence does not hide the next. Exit 0: bit-exact; exit 1: a hash differs, or
the file was refused.

```text
$ regia net replay-audit saved/replays/match.lsreplay
replay: saved/replays/match.lsreplay
game regia.desync_sample (protocol version 1, 4 seats, 60 Hz, seed 0x5eed)
replayed ticks 0..=239: 240 frames, 240 hash checkpoints and 4 of 5 fulls compared, 0 records skipped
first diverging tick 77: subsystem units, last agreed tick 76 (found by the hashes)
  every subsystem differing there: units, economy, orders
NOT bit-exact: 43 comparisons differ; resumed from the fulls at [119]
```

## `net.replays`

`net.replays {dir?}` lists a library folder (project-relative; the published folder, else
`saved/replays`).

```json
{ "dir": "saved/replays" }
→ { "dir": "saved/replays",
    "replays": [{ "file_name": "match-0412.lsreplay", "date": 1795046400, "length": 48211,
                  "game_id": "studio.tactics", "protocol_version": 3, "engine_version": "0.6.0",
                  "seats": [0, 1, 2, 3], "tick_rate": 60, "seed": 7, "records": 1290, "aborted": false }],
    "damaged": [{ "file_name": "cut.lsreplay", "code": "replay_truncated",
                  "reason": "the replay is truncated at byte 9120: a record payload needs 41 bytes, 7 are left" }] }
```

## `net.replay_audit`

`net.replay_audit {path}` runs the audit on a project-relative `.lsreplay` with the game's hooks
(`NetAgentState::set_replay_hooks(registry)`) or, for a game without one there, the engine's sample.

```json
{ "path": "saved/replays/match-0412.lsreplay" }
→ { "path": "saved/replays/match-0412.lsreplay", "bit_exact": false,
    "header": { "game_id": "regia.desync_sample", "protocol_version": 1, "seats": [0, 1, 2, 3], "tick_rate": 60 },
    "ticks": [0, 239], "frames": 240, "fulls": 5, "fulls_compared": 4, "hashes_compared": 240,
    "skipped": 0, "mismatches": 43, "first_diverging_tick": 77, "last_agreed_tick": 76,
    "subsystem": "units", "subsystems": ["units", "economy", "orders"], "found_by": "hashes",
    "resumed_at": [119], "abort": null, "summary": ["..."] }
```

| Field | Meaning |
|---|---|
| `bit_exact` | Every recorded hash matched the replay. |
| `first_diverging_tick`, `subsystem`, `subsystems` | The first tick whose recorded hashes differ, and the subsystems that differ there in the recorded order (`null` subsystem: only the world hash differs). |
| `last_agreed_tick` | The newest compared tick before it at which everything agreed. |
| `found_by` | `hashes` (a Hashes record) or `full` (a Full's world hash). |
| `resumed_at` | Ticks of the Fulls the audit reloaded after the replay had left the recorded states. |
| `skipped` | Records the audit could not use (frames before the first Full, hashes of a tick it did not step to). |
| `abort` | The abort record (`tick`, `code`, `reason`), when the match aborted. |

## Error codes

| Code | When |
|---|---|
| `replay_not_found` | No file at the path. |
| `replay_version_newer` | A newer `.lsreplay` format version than this build reads; the message names both versions. Open it with a newer build. |
| `replay_truncated` | The file ends inside its header or a record; the message names the byte offset. |
| `replay_game_mismatch` | The file was recorded by another game or another version of it than the hook steps; the message names both. |
| `replay_no_hook` | No hook is registered for the file's game; the message lists the registered ones. |
| `replay_hook_failed` | The hook could not load a Full or step a frame, or a loaded Full hashes differently from the file. |
| `replay_too_large` | A length (the file, the header, a record, a string, a list) is over its cap; nothing was allocated for it. |
| `replay_unknown_record` | A record kind version 1 does not define. |
| `replay_invalid` | A value out of range or an inconsistent record (seats not ascending, a frame missing, trailing bytes). |
| `replay_not_lsreplay` | Not a `.lsreplay` (the magic, or the file name for an import). |
| `replay_duplicate` | `ReplayLibrary::import`: the library already holds these bytes. |
| `replay_io` | A file or folder could not be read or written. |

A path outside the project is `F0002` (`path_outside_project`), as for every method.

## Caps

The reader checks every length against the bytes left and these caps before reading or allocating:
the file 256 MiB plus 4 KiB, the header 64 KiB, a Full 32 MiB, any other record 4 MiB, 1,024 seats, an
input 60,000 bytes, 64 subsystems, 256 peer commands per tick, a game id 128 bytes, a name 64 bytes, a
text 1,024 bytes. The fuzz target `net.lsreplay` and the `lsreplay_decode` test (20,000 seeded
mutations under a counting allocator) hold it to that.

## Checks

1. Record a match with `record: true` and `hash_every_ticks: 1`, then `net.replays`: it is listed first.
2. `net.replay_audit {path}`: `bit_exact` is `true`.
3. Change one input byte in a copy (or record with `sample_replay(4, 240, 60, 1, Some((77, 2)))`):
   `first_diverging_tick` is 77 and `subsystem` names the subsystem the input feeds.

The byte layout: `docs/formats/lsreplay.md`.
