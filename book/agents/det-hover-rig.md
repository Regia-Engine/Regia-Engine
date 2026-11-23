# Hover feel-target rig

`regia_detsim::handling` flies a hover hull and reports how it feels: time to cruise, coast and
brake, yaw, strafe, lean, a crest, a slope and the hop (ADR-8393). It calls
[`regia_detsim::hover`](det-hover.md). The caller passes `HoverParams`. There is no hull-class
table in the engine. `HoverParams::DEFAULT_HULL` is the sample hull; a game passes its own.

The rig is a measurement tool. It is not on the per-tick path. One tracing span,
`handling_measure`, covers one call to `measure` or `measure_with`.

| Function | What it does |
|---|---|
| `measure(params, gravity)` | The feel-target `Report` on that world, current thrust floor. |
| `measure_with(params, gravity, legacy_floor)` | The same. `legacy_floor` uses the old 0.6 low-g thrust floor. |
| `fly(params, state, gravity, secs, input, ground)` | Integrate `secs` at the lockstep tick. `input(t)` is `[throttle, strafe, steer]`. `ground(x, z)` is the floor height. |
| `world_params` / `world_params_for` | Scale `thrust_accel` for the world. The current floor keeps authored thrust below Earth gravity. |
| `boost_params` | Copy with `turbo_scale` applied (or `BOOST_SCALE` 1.375 when `turbo_scale` is 0). |
| `format_report` / `write_report_atomic` | One `name=value` line, written by a temporary file and a rename. |

`WORLDS` is the six gravities the rig knows (Earth 9.81 through Iapetus 0.223). `DT` is the
lockstep tick.

## Check it

```sh
cargo test -p regia_detsim --test handling_audit -- --nocapture
cargo run -p regia_detsim --example handling_telemetry
```

`rig_reports_the_feel_targets` checks the engine default hull: cruise is its own forward cap, the
hop is the engine's 3 m pulse, and a lighter world with the current thrust floor reports the same
flat-ground cruise. It prints one line:

```text
regia-handling-rig cruise_m_s=... t_to_cruise_s=... coast_dist_m=... brake_time_s=... hop_apex_m=... strafe_speed_m_s=...
```

`handling_audit_table` prints the tier table (driven hull and plain throttle). Set
`REGIA_HANDLING_AUDIT_OUT` to a file path to write that table. The write is atomic.

There is no agent-protocol method. The rig keeps no runtime state. The `regia-handling-rig` line
is the agent surface.
