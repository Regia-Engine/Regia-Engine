# Replay playback (ReplaySim)

<!-- index: Replay -->
<!-- index: ReplaySim -->

`regia_framework::replay_sim::ReplaySim` plays a Direct Order 27 `.do27replay` (version 2, 3 or 4)
for rendering, camera and VFX work. Playback never re-simulates: between two recorded full
snapshots the entity set is the last full's, unless sub-tick blending is on (below). It is an `ExternalSim`, so the
[sim bridge](../agents/framework.md) turns a replay into entities, and it is also a plain resource
that the agent protocol drives without a window ([agent page](../agents/replay.md)).

No DO27 data is in this repository. The test helper `replay_sim::SyntheticReplay` builds replay bytes
record by record for tests and tools.

This file is presentation playback. It does not re-simulate a match. A lockstep match recorded for
a bit-for-bit audit is a `.lsreplay`, checked with `regia net replay-audit`. A desync bundle is a
`.desync.ron`. Both are in [Lockstep and rollback](../how-to/online/lockstep-and-rollback.md).

V4 files load directly, including their extra header labels and the map-extent/training
records that presentation does not use. The labels are validated and discarded; those two
records are skipped using their defined lengths. Other unrecognized tags remain errors.
Keep the matching `FullCodec` when decoding full snapshots or verifying their hashes.
Existing public struct shapes and v2/v3 playback are unchanged (ADR-8191).

## Use it in an app

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
use regia_framework::replay_sim::ReplaySim;
use regia_framework::sim_bridge::SimBridgePlugin;

let mut sim = ReplaySim::new();
sim.load_path(std::path::Path::new("saved/replays/match.do27replay"))?;
sim.set_playing(true);
app.add_plugins(SimBridgePlugin::<ReplaySim>::new(sim));
```

- Nav beacons become entities of kind `"beacon"` (ids `2^32 | beacon_id`).
- Moments, peer commands and the abort flag arrive as `ReplayEvent` bridge events with the kinds
  `moment.hit`, `moment.down`, `moment.kill`, `command` and `abort`, each exactly once when playback
  crosses the tick.
- The clock is an integer accumulator over frame time, tick rate and `set_speed_permille` (1000 is
  real time), so the same frame times land on the same ticks at 30 fps or uncapped.
- `seek(tick)` moves to a tick (clamped to the end) and fires nothing, forwards or back.
  `step(ticks)` advances and queues the events of every tick crossed. Both are tick-exact and do not
  depend on the frame clock.
- `advance_clock(frame_time)` is the frame clock itself (the bridge calls it with `Time::sim_delta`; a
  player kept as a plain resource can call it from its own system). `tick_fraction()` is how far past
  `tick()` playback is. `hold_clock(d)` makes the next `d` of frame time not play (a render-out uses it
  to start a replay exactly on its frame 0).

## Slow motion: blending between ticks

At a quarter speed a 60 Hz replay stays on each tick for four 60 fps frames, so poses step.
`set_interpolation(max_gap_ticks)` (ADR-1519; 0 = off, the default) blends the snapshot between a
full and the next one when they are at most `max_gap_ticks` apart: each entity present in both moves
by the playback time's fraction of the gap, with a linear position and a normalized linear rotation on
the shorter arc. With 1, only fulls on consecutive ticks blend. Blending is presentation only
(ADR-1524): `presented_snapshot()`, which the bridge's `read_snapshot` uses, blends; `snapshot()` never
does, so `seek`, `step` and anything hashed from them are exact with blending on. At a tick that holds
a full nothing is blended; an entity only in the later full appears when playback reaches it.
`ReplaySnapshot::blend` says how far a presented snapshot moved (`next_full_tick`, `alpha`).

```rust,wrap
# let mut sim = regia_framework::replay_sim::ReplaySim::new();
sim.set_interpolation(1);
sim.set_speed_permille(250);
sim.set_playing(true);
```

The agent's `replay.play` does the same and can start the replay on a render-out's frame 0. The player
runs on the simulation clock, so a `render.out_start` `time_map` composes with its speed
([agent page](../agents/replay.md)).

## Fulls are opaque

The snapshot inside a full belongs to the sim build that wrote the replay: its layout and its state
hash change with each build. An outside reader cannot decode it. A game that links the matching sim
implements `FullCodec`:

```rust,wrap
use regia_framework::replay_sim::{FullCodec, ReplayEntity, ReplaySim};
struct MyCodec;
impl FullCodec for MyCodec {
    fn decode(&self, _snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> { Ok(Vec::new()) }
    fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> { Some(0) }
}
let sim = ReplaySim::with_codec(MyCodec);
```

`decode` returns render-ready `ReplayEntity` values (`id`, `kind`, `team`, `translation`,
`rotation`) in Regia Engine metres. Without a codec (`OpaqueCodec`) the entity list holds only beacons.

A version 2 full has no length prefix, so only the writing build knows where it ends. A codec for that
build can say so, and its version 2 files then load (`ReplaySim::load_bytes`/`load_path` and
`ReplayFile::parse_with`/`load_with` use it):

```rust,wrap
use regia_framework::replay_sim::{FullCodec, ReplayEntity};
struct MyCodec;
impl FullCodec for MyCodec {
    fn decode(&self, _snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> { Ok(Vec::new()) }
    fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> { Some(0) }
    fn v2_full_len(&self, body: &[u8]) -> Option<usize> { Some(body.len()) }
}
```

The default (`None`) keeps such files refused.

## Reading and auditing a file

`ReplayFile::parse(bytes)` and `ReplayFile::load(path)` read the header, the player and peer tables and
every record from their lengths and return a `ReplayError` for malformed input; they never panic.
`load` refuses files over 512 MiB (`MAX_REPLAY_BYTES`). Only the two bounded v4 extensions are skipped; other unknown tags fail (`unknown_tag`). A
version 2 full has no length, so a v2 file that contains one is refused (`v2_full`) unless it is read
with a codec that implements `v2_full_len` (`parse_with`, `load_with`). Guessing the boundary would
mis-read the file silently, so the reader never does.

`ReplayFile::verify(&codec)` returns a `VerifyReport`: the number of fulls, how many hashes were
`hash_checked`, the `hash_unchecked` count, `mismatches` (ticks), structural `defects` and `aborted`.
`is_ok()` means no mismatch and no defect. **A hash that nothing could check is skipped, not
passed**; only a codec whose `state_hash` returns `Some` makes the audit meaningful.

## Finding moments

Moments are the sim's derived combat observations (`hit`, `down`, `kill`) in emission order; the
index in the list is the Moment's id. Filter with `MomentQuery` and `ReplayFile::moments_matching`:

```rust,wrap
# let file: regia_framework::replay_sim::ReplayFile = todo!();
# use regia_framework::replay_sim::*;
let query = MomentQuery {
    kind: Some(MomentKind::Kill),
    min_importance: 80,
    ..MomentQuery::default()
};
for index in file.moments_matching(&query) { let moment = &file.moments[index]; /* ... */ }
```

Fields: `kind`, `from_tick`, `to_tick`, `entity` (attacker, victim or either slot), `attacker`,
`victim`, `fight`, `round`, `weapon` and `min_importance`. Positions are Regia Engine metres (DO27's frame
is metres, +Y up, forward -Z, so `sim_to_regia` is the identity on axes).

## Putting a camera on a kill

Combine this with [camera shot sequences](shot-sequences.md) and [render-out](render-out.md): find a
Moment, `seek` to a little before its tick, aim a shot at its position, and render.
`moments.get`'s `seek_tick` is that event tick. Seeking to it and stepping one tick does not
deliver the Moment; seek strictly before it. A Moment at tick 0 is delivered on the first step
after load, not after a seek. The agent recipe is in the [agent page](../agents/replay.md).

## Not included yet

Decoding a full without the writing sim's codec, and reading version 2 fulls without it. Blending
across long gaps between fulls is a straight line, not a re-simulation.
