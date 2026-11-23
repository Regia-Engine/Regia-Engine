# Net rollback log (agents)

Game-owned prediction and reconciliation in `regia_net::rollback` (spec 120 § 9.5, ADR-7953). Guide topic
`net-rollback`. The game owns the step function and the state. The engine keeps a fixed ring of per-tick snapshots
and the inputs that produced them, and replays the game's step from a corrected tick. This path does not use the ECS.
The ECS predictor (`regia_net::predict`, [Net prediction (agents)](net-prediction.md)) is a separate path.

## The log

```rust,wrap
use regia_net::{PeerId, RollbackLog, TickInputs};

let mut log = RollbackLog::<(i64, i64), i64>::new(64);
let mut inputs = TickInputs::new();
inputs.insert(PeerId(1), 1);
log.record(1, (1, 1), inputs);
```

- `RollbackLog::new(capacity_ticks)` keeps at most that many snapshots of `S` plus the `TickInputs` stored with each.
  That is the memory bound. `0` keeps nothing: every later reconcile and amend returns `rollback_too_old`.
- `record(tick, state, inputs)` stores the state **after** stepping `tick`, and the inputs that step used. Recording
  a tick that is already stored replaces that slot and leaves newer ticks in place. A new tick past the capacity drops
  the oldest snapshot.
- `amend_inputs(tick, inputs)` replaces the inputs of a stored tick (a late real input arrived). The stored state stays
  until the next reconcile, which replays with the new inputs.
- `reconcile(server_tick, server_state, step)` writes `server_state` at `server_tick` and calls `step` once for each
  tick in `(server_tick, newest]`. The tick argument is the tick being produced. The call count is
  `newest - server_tick` when that range was stored with no gap. The returned reference is the corrected newest state.
  Correcting only the newest tick writes that state and does not count as a replay.

`step` is the game's function: `&mut S`, the tick, and that tick's `TickInputs`. The log clones the previous snapshot
and does not call into the ECS.

## Errors

Both codes are lowercase ascii. They are library results, not RPC errors of `net.rollback`.

| Code | When |
|---|---|
| `rollback_too_old` | `server_tick` (or the tick passed to `amend_inputs`) is older than every snapshot still in the ring, or the ring is empty. Stored snapshots and inputs are unchanged. The too-old counter increments. |
| `rollback_not_recorded` | The tick is not older than the ring but was never stored (a gap, or ahead of the newest record), or the ticks from the corrected one through the newest are not contiguous. Nothing is written, and the too-old counter does not increment. |

## Visual offset

`VisualOffset` hides the snap from the predicted pose to the corrected one. It counts ticks. It does not read a clock.
`VisualOffset::new(delta, span_ticks)` (or `from_correction(predicted, corrected, span_ticks)`) starts at the full
delta. `value` is `start * remaining / span` with division toward zero, so the magnitude never increases. After
`span_ticks` calls to `advance`, `value` is 0 and stays 0. `apply(simulated)` is `simulated + value` (saturating).
A span of 0, or a delta of 0, is already zero. Add `value` in the game's presentation step. This is not
`NetVisualOffset`, which decays by a half-life in milliseconds inside the ECS predictor.

## `net.rollback`

Read. No parameters. Example `{}`.

The host publishes the log's counters once it has them:

```rust,wrap
# let log = regia_net::RollbackLog::<(i64, i64), i64>::new(64);
# let mut state = regia_net::NetAgentState::default();
state.publish_rollback(log.counters());
```

`net.rollback` then returns:

| Field | Meaning |
|---|---|
| `published` | `false` when the app has no `NetAgentState`, or the host has not called `publish_rollback`. |
| `replays` | Successful reconciles that re-simulated at least one tick. |
| `depth` | The deepest of those: `newest_tick - server_tick`. |
| `too_old` | Times `reconcile` or `amend_inputs` returned `rollback_too_old`. |

Before a publish, and with no resource, every count is 0 and `published` is false.
