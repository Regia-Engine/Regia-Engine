# DO27 replay and Moments

`regia_framework::replay_sim::ReplaySim` plays a Direct Order 27 `.do27replay` (version 2, 3 or 4,
`spec/replay/do27-replay-v3.md` in the DO27 repo) for rendering and camera work. It is an
`ExternalSim`, so `SimBridgePlugin::<ReplaySim>` turns a replay into entities, and it is also a plain
resource the protocol drives without a window. These methods work on the world's `ReplaySim`;
`replay.load` creates one (with the opaque codec) when the app has none.

Playback never re-simulates. Between two fulls the entity set is the last full's (unless
`replay.play` blends between ticks, below), and nav beacons
(kind `"beacon"`, ids `2^32 | beacon_id`) come from the last nav record. The snapshot inside a full is
opaque to an outside reader (its layout and state hash belong to the sim build that wrote it), so a game
that links the matching sim installs a `FullCodec` (`ReplaySim::with_codec`) to get its entities and to
recompute the state hashes. Without one the entity list holds only beacons.

## `replay.load`

Params: `path` (required). Reads the file (at most 512 MiB), replaces any loaded replay, rewinds to
tick 0 paused. `path` is confined like `render.out_start`'s `out` (ADR-1769): relative to the project root, or absolute
under the project root, the system temp dir, an `output_roots` or `replay_roots` entry of
`.regia/agent.toml`, or a root the host added with the `regia_agent::ReplayRoots` resource (ADR-1965; the
showcase host's `--replay-root DIR`); `..` and a symlink leading out of them are `F0002` before anything is read,
with `data.allowed_roots` listing those kinds. List a replay store outside the project in `replay_roots`, not
`output_roots` (that one also allows writes). Needs the `assets` permission; `replay.seek` and `replay.step` need `play`;
`moments.list` and `moments.get` are read-only.

```json
{ "method": "replay.load", "params": { "path": "saved/replays/1760000000000-ironreach-tdm.do27replay" } }
```

Returns `path`, `bytes` (the file size), `header` (`version`, `protocol`, `tick_rate`, `lobby_preset`, `seed` as a string, `bootstrap`,
`map`, `mode`, `teams`, `input_delay`, `sim_git_sha`, `sim_abi`, `content_hash`, `config_hash`,
`players[]` with `peer_id`, `account_id`, `callsign`, `role`, `team`, and `peers[]`), `end_tick`,
`duration_secs`, `counts` (`inputs`, `fulls`, `navs`, `salvage`, `command_records`, `moments`,
`moment_ticks`), `abort` (`{tick, reason}` or null), `verify` (`ok`, `fulls`, `hash_checked`,
`hash_unchecked`, `mismatches`, `defects`, `moment_ticks`, `aborted`) and `position`.

`verify` is the audit `ironreach_replay --verify` does, as far as an outside reader can: at least one
full, non-decreasing input ticks, an applied-inputs record on every full's tick, no duplicate `peer_id`
(`defects[]`), and, when the player's codec can recompute a full's state hash, a bit-exact comparison
(`mismatches[]` lists the ticks). **`hash_unchecked` counts fulls nothing could check: skipped, not
passed.** `ok` is true when there is no mismatch and no defect; `aborted` only reports the trailer.

Errors (`INVALID_PARAMS`, `data.code` `F0002`, the message ends in a bracketed code): `truncated`, `bad_magic`, `unsupported_version`,
`bad_string`, `unknown_tag` (the record boundary is not defined), `v2_full` (a version 2 full has no length), `full_without_tick`,
`bad_moment`, `bad_nav`, `too_large`, `io`. A malformed file is always an error, never a crash.

**Version 2 files with fulls.** A version 2 full has no length prefix and no state hash: where it ends is
known only to the sim build that wrote it. An outside rule (looking for the next tag byte, assuming a size)
would split it in the wrong place without noticing, because snapshot bytes can hold any value, and every
later record would then be read as garbage. So the default player refuses such a file with `v2_full`
(ADR-1519). A game that links the writing build can load them: its `FullCodec` implements
`v2_full_len(body)` (how many bytes of `body` are the snapshot), and `replay.load` reads the file with the
codec of the world's `ReplaySim`. Those fulls have no hash, so `verify` counts them in `hash_unchecked`.
Otherwise convert the file to version 3 with DO27's own tools. Version 2 files without fulls load as before.

**Version 4 files.** Load the original file directly (ADR-8191); no v4-to-v3 rewrite is
needed. Extra player/team label strings are validated and discarded. Map-extent (`0x08`,
16-byte body) and training (`0x09`, tick plus length-prefixed body) records are skipped without
changing the last applied-input tick, snapshot hashes, events or duration. Their payload bytes
are never interpreted as record tags. Empty training bodies are safe; truncated bodies return
`truncated`. An unrecognized tag still returns `unknown_tag`: this format has no universal
record length. Versions above 4 return `unsupported_version`. V2/v3 behavior is unchanged.
The header response retains its existing fields and reports `version: 4`.

## `replay.seek`

Params: `tick` or `seconds` (at the header's tick rate). Clamped to `end_tick`. **Fires no events**, forwards or back, so
seeking over a Moment never plays it. Seeking is a pure function of the tick: the same tick gives the same
snapshot whatever came before. Returns `position` (`tick`, `end_tick`, `seconds`, `at_end`) and
`snapshot` (`tick`, `full_tick`, `nav_tick`, `entity_count`, `entities[]` capped at 100 with `id`, `kind`, `team`,
`position`, `rotation` as `[x,y,z,w]`, `truncated`, `decode_error`).
`decode_error` is `null` when the snapshot decoded; a player without a codec for full snapshots (the default
`OpaqueCodec`) reports a message starting with `no_full_codec` and an empty `entities[]`.

```json
{ "method": "replay.seek", "params": { "tick": 4280 } }
{ "method": "replay.seek", "params": { "seconds": 71.5 } }
```

`tick` and `seconds` must be non-negative; `tick` wins when both are given; a tick above `u32::MAX` or a
non-numeric value is `INVALID_PARAMS`.

## `replay.step`

Params: `ticks` (default 1) and `presented` (default false: the exact snapshot; true: the pose a bridge
presents, blended between ticks when `replay.play` turned `interpolate` on). Stops at the end. Returns
`from`, `advanced`, `clamped`, `position`, `snapshot`, `events_total`, `events_truncated` and
`events[]`: Moments (`type: "moment"`), peer commands (`type: "command"`: `peer_id`, `ok`, `kind`,
`text`) and aborts (`type: "abort"`) whose tick was crossed, each exactly once, in tick order.
`ticks` above 36000 is clamped (`clamped: true`; `advanced` is the ticks actually moved). The
response lists at most 1000 events; `events_total` is the count crossed and `events_truncated` is
true when the list is shorter. A later step does not repeat the events this response omitted.
Playing from the
frame clock (`ReplaySim::set_playing`) uses integer arithmetic, so the same frame times always land on the
same ticks at 30 fps or uncapped.

```json
{ "method": "replay.step", "params": { "ticks": 30 } }
{ "method": "replay.step", "params": {} }
```

`ticks: 0` is a valid probe: it advances nothing and reports the position. A negative, fractional or
non-numeric `ticks` is `INVALID_PARAMS`. Stepping past the end answers `advanced: 0` and no events.

## `replay.play`

Params, all optional: `from_tick` (seek there first, firing no events; default the current tick),
`speed` (0.001..=64, default 1 = real time), `interpolate` (default true), `max_gap_ticks` (1..=600,
default 1), `start` (`"now"`, the default, or `"render_out"`). Needs `play`.

Plays the replay on the **simulation clock**: each frame advances it by the frame's simulation delta
(`Time::sim_delta`, which is the frame delta unless a render-out `time_map` sets it apart) times `speed`
(exact integer arithmetic, so the same frame times land on the same ticks). With a
`SimBridgePlugin::<ReplaySim>` the bridge advances it; a player that is only a resource is advanced by the
agent (`driver` says which). Events are delivered as playback crosses their ticks, once each, as with
`replay.step`; playback stops at the end.

- **Slow motion.** With `interpolate` on, the pose a bridge presents between two fulls at most
  `max_gap_ticks` apart moves each entity present in both from the first pose toward the second by the
  playback time's fraction of the gap (linear position, normalized linear rotation on the shorter arc).
  This is presentation only: the tick, the events, the state hashes and every `replay.seek` /
  `replay.step` snapshot stay exact, with blending on or off. `replay.step { ticks: 0, presented: true }`
  returns the presented pose to check it. At a tick that holds a full nothing is blended. With the
  default gap of 1 the blend is between consecutive ticks only (a replay with a full every tick); a larger
  gap blends across sparser fulls, which is a straight line between them. An entity only in the later
  full appears when playback reaches it. Answers show `position.tick_fraction`, and a presented snapshot
  `snapshot.blend` (`{ next_full_tick, alpha }`; `null` in exact snapshots).
- **On a render-out.** Under `render.out_start` with a `time_map` the simulation delta is the job's
  simulation step, so the replay's `speed` composes with the map: a 2x replay under a 0.25 map plays at
  half the video's speed. `start: "render_out"` seeks to `from_tick` and waits there (not playing); the
  next render-out, or the running one if it has not reached frame 0, starts it on its frame 0. Frame 0
  then shows exactly `from_tick`, and frame `i` shows `from_tick + tick_rate · speed · sim_time(i)`
  (`render.out_progress` and the manifest report `replay_from_tick`; the manifest's `frame_times` has
  `sim_time_ns` per frame). `armed_for` says which job (`running_job` or `next_job`; `null` for `start: "now"`).
  After the job the replay keeps playing on the normal clock until `replay.pause`.

Returns `position` (with `tick_fraction`, `playing`, `speed`), `snapshot`, `interpolate`,
`max_gap_ticks`, `start`, `driver` (`sim_bridge` or `agent`) and `armed_for`.

```json
{ "method": "replay.play", "params": { "from_tick": 4280, "speed": 0.25 } }
{ "method": "replay.play", "params": { "from_tick": 4280, "speed": 1, "start": "render_out" } }
```

Recipe: a slow-motion kill cam.

```text
moments.get       { index: 12 }                                    -> seek_tick: 4310
replay.play       { from_tick: 4250, speed: 1, start: "render_out" }
render.out_start  { out: "renders/kill12", frames: 240, fps: 60,
                    time_map: [{frame: 50, scale: 1}, {frame: 80, scale: 0.2},
                               {frame: 200, scale: 0.2}, {frame: 230, scale: 1}] }
render.out_progress { }                                             -> replay_from_tick: 4250
```

## `replay.pause`

No params. Stops frame-clock playback where it is and drops a `start: "render_out"` arm. Returns
`position` and `disarmed` (true when an arm was dropped). Needs `play`.

## `moments.list` and `moments.get`

Moments are the sim's derived combat observations (kind `hit`, `down`, `kill`), in emission order. The
index is the Moment's id. Positions are Regia Engine metres: the replay stores sim metres and DO27's frame is
metres, +Y up, forward −Z, so the conversion (`replay_sim::sim_to_regia`) is the identity on axes.

`moments.list` params, all optional: `kind` (`"hit"`, `"down"`, `"kill"`, or a wire number as a string such as `"7"`), `from_tick`/`to_tick` (the stored tick), `entity` (attacker,
victim or either participant slot), `attacker`, `victim`, `fight`, `round`, `weapon`, `min_importance`,
`limit` (default 50, at most 1000), `offset`. Returns `total`, `matched`, `offset`, `returned`, `moments[]` with
`index`, `tick`, `drained_tick`, `kind`, `kind_wire`, `span` (`match`, `round`, `fight`), `participants`
(`[{role, entity_id}]`), `position`,
`damage`, `weapon`, `attacker`, `victim`, `importance`.

`moments.get { index }` returns one Moment plus `fight_moments` (indexes in the same fight and round) and
`seek_tick`, the Moment's event tick (`drained_tick`). `replay.seek` to that tick does not play it: a seek
sets the cursor on the tick and clears the fresh flag, and the next `replay.step` delivers events in
`(cursor, through]`. Seek strictly before `seek_tick`. A Moment at tick 0 is delivered on the first step
after load, not after a seek.

```json
{ "method": "moments.list", "params": { "kind": "kill", "min_importance": 80, "limit": 20, "offset": 0 } }
{ "method": "moments.get",  "params": { "index": 12 } }
```

`limit` above 1000 is clamped to 1000 and `offset` past the end returns an empty page (`returned: 0`);
`min_importance` above 255 saturates. An unknown `kind`, a non-string `kind` or `weapon`, or a negative,
fractional or non-numeric number is `INVALID_PARAMS`. `moments.get` without `index` is `INVALID_PARAMS`.

Recipe: find the kills, put the camera on one, and check the frame.

```text
replay.load   { path }
moments.list  { kind: "kill", min_importance: 80 }      -> pick index 12
moments.get   { index: 12 }                              -> position, seek_tick: 4310
replay.seek   { tick: 4280 }                             -> snapshot half a second early
camera / seeing.screenshot ...                           -> look at position
replay.step   { ticks: 30 }                              -> the kill's event, once
```

`NOT_FOUND` before a replay is loaded, or for a Moment index past the end.

## Errors at a glance

| Call | Condition | Code |
| --- | --- | --- |
| `replay.load` | no `path`, non-string `path`, malformed or unsupported file, unreadable file | `INVALID_PARAMS` (message ends in `[code]`) |
| `replay.load` | `path` outside the allowed roots, with `..`, or through a symlink leading out | `INVALID_PARAMS` `F0002`, `data.reason` `path_outside_project`, `data.path_reason`, `data.allowed_roots` |
| `replay.load` | a failure leaves the previously loaded replay and its position untouched | n/a |
| `replay.seek`, `replay.step`, `replay.play`, `replay.pause`, `moments.list`, `moments.get` | no replay loaded | `NOT_FOUND` |
| `replay.seek` | neither `tick` nor `seconds`; negative or non-finite values | `INVALID_PARAMS` |
| `replay.step` | `presented` not a boolean | `INVALID_PARAMS` |
| `replay.play` | `speed` outside 0.001..=64 or not a number; `from_tick` negative, fractional or above `u32::MAX`; `interpolate` not a boolean; `max_gap_ticks` outside 1..=600; `start` not `"now"` or `"render_out"` | `INVALID_PARAMS` (nothing changes) |
| `moments.get` | `index` at or past the Moment count | `NOT_FOUND` |

`replay.seek` and `replay.step` are repeatable: seeking to the same tick twice, or loading the same
file twice, gives byte-identical answers. The player crosses the same events in the same order for
any step size. Each `replay.step` response includes at most 1000 of the events that step crossed
and at most 36000 ticks.

## Format compatibility

Files DO27 has already written keep loading. `crates/regia_framework/tests/fixtures/replay/` holds
frozen files written from the byte layout by an encoder that shares no code with the reader
(ADR-1378), and `tests/replay_format_compat.rs` pins every field they parse to:

| Fixture | What it pins |
|---|---|
| `v3_every_record.do27replay` | a version 3 file with every record tag, a non-ASCII callsign, input extremes, an empty full, an unnamed Moment kind (reported as `other`) and a 32-byte weapon stem |
| `v2_no_fulls.do27replay` | a version 2 file: the short header loads with the version 3 fields empty, and `verify` reports `no full record` |
| `v4_future.do27replay` | historical v3 layout stamped v4 remains malformed: `truncated`; a separate v5 mutation pins `unsupported_version` |
| `v3_unknown_tag.do27replay` | a record tag this reader does not know is refused where it starts: `unknown_tag` |
| `v4_extensions.do27replay` | non-empty player/team labels and bounded extension records preserve the known records and playback; misleading tag bytes inside skipped payloads are ignored |

A test checks each file's length and hash, so an edited fixture fails first. A new layout adds a fixture; the old ones keep passing. Any prefix of a fixture loads as a shorter
file or is a structured error, never a crash.

Game-side API (`ReplaySim`, `FullCodec`, `MomentQuery`): [Replay playback](../user-guide/replay-sim.md).
