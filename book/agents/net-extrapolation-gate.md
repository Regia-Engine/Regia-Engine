# Net extrapolation gate (agents)

Spec 120 § 8.2, ADR-8522. A net profile can trade a little position error for far fewer transform updates: with
`extrapolation_gate_m` set, the server sends a moving entity only when the client's extrapolation of what it holds
has drifted past the gate. The gate is **off by default**: without it, every change is sent and a copy is within half
a `position_step_m` of the server.

## Turning it on

In a `.netprofile.ron` (format v2):

```ron
NetProfile(
    format_version: 2,
    extrapolation_gate_m: Some(0.02),   // 2 cm; None (the default) is off; (0, 10] metres
)
```

Or in code: `NetProfile { extrapolation_gate_m: Some(0.02), ..NetProfile::default() }`. A v1 profile loads with the
gate off; `regia upgrade-project` rewrites its `format_version` to 2. An out-of-range value fails to load with
`net.profile_invalid` naming `extrapolation_gate_m`.

## What the server does

On the acknowledged state channel (`net.state_channel` `wire: acked_deltas`) a gated entity whose only change since
the baseline the client acknowledged is its transform is **held back** while, for every record the client may be
showing, the record's position moved along the record's velocity for the time since its tick is within
`extrapolation_gate_m` of the server's position, and the rotation is within 0.5 degrees
(`regia_net::profile::EXTRAPOLATION_GATE_DEG`). "Every record the client may be showing" is the update records sent
since the acknowledged baseline that are still on their way, or the baseline itself when none is. When the gate is
exceeded the update goes out as usual and the copy is checked again at every send tick.

Never held back: a change to any replicated component or the parent, a teleport (`net_teleport`) or resend
(`net_resend`), a copy the client predicts or owns with prediction, and anything on the reliable tick stream
(`reliable_stream`), which has no acknowledged baseline.

## What the client does

Every state-channel record of a gated entity carries the **steady bit**:

- an interpolated proxy extrapolates a steady state with its velocity without the `max_extrapolate_ms` limit, so it is
  `Extrapolating`, never `Starved` (`net.proxy_timeline`);
- a copy without interpolation (`interpolation: None`, or a client with no interpolation clock) gets the
  `regia_net::replication::NetSteady` component and moves along the record's velocity every client tick to the
  estimated server tick (`ClientStateChannel::server_tick_estimate`: the newest state frame's tick plus the local ticks
  since). `NetSteady.shown_tick` is the tick it was last shown at. A record at rest, or one that is not steady, removes
  it.

So the shown error with a gate is up to the gate plus what accrues while a correcting record travels (the send interval
and the one-way delay), not `position_step_m`.

**Keep `NetVelocity` truthful.** The client moves a gated copy along the replicated velocity. If a game leaves
`NetVelocity` non-zero while the entity does not move (a character pushing against a wall), the server makes no new
version once the last record is acknowledged and the copy drifts until the transform or the velocity changes. Zero the
velocity when the entity is blocked, or leave the gate off for such entities.

## Velocity-predicted residuals

Independently of the gate, every position delta whose baseline has a velocity may code its residual against the
baseline moved along that velocity (one bit says which residual the server chose). The predictor assumes 60 Hz
(`regia_net::replication::PREDICTION_TICK_HZ`) whatever the server's rate, so both sides always decode the same
integers.

## Reading it

| Method | Field | Meaning |
|---|---|---|
| `net.profiles` | `profiles[].extrapolation_gate_m` | the gate in metres, `null` when off |
| `net.state_channel` | `peers[].gate_held` | update candidates the gate held back on this connection |
| `net.state_channel` | `transform_record.revision` | 3: steady bit and predicted residuals (a revision 2 build is refused with `protocol_mismatch`) |
| `net.state_channel` | `transform_record.prediction_tick_hz`, `transform_record.gate_rotation_deg` | 60 and 0.5 |
| `net.proxy_timeline` | `state` | a steady proxy past its newest state is `extrapolating`, never `starved` |

A check for an agent: with a gated profile on a moving crowd, `gate_held` grows and `records_sent` grows far more slowly
than with the gate off; `net.profiles` shows the gate on the profile the entities use.

## Errors

No new error codes. A bad `extrapolation_gate_m` is a profile load failure (`profile_invalid` from `net.profiles
{ name }`, `net.profile_invalid` in the log), and a peer built with transform record revision 2 is refused with
`protocol_mismatch`.

Measurements: `docs/perf/net-extrapolation-gate.md`; tests: `crates/regia_net/tests/extrapolation_gate.rs`.
