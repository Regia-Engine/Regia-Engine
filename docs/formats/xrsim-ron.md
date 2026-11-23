# `*.xrsim.ron`: the XR simulator script

Owner: `regia_xr::sim` (spec 58, ADR-1676). Current `format_version`: **1**. A deterministic script of head and
controller poses, inputs and session states per fixed-step tick. The simulator backend (`NullXr`) plays it with no
headset and no GPU, so XR code is testable in CI; `SimRecorder` writes the same format from live poses and a
recording replays exactly (unit quaternions are taken bit for bit). Registered in `cargo xtask check-formats`
(`xrsim`) and `regia upgrade-project` since ADR-2316.

## Shape

```ron
(format_version: 1, fov_deg: 100.0, ipd_m: 0.063, refresh_hz: 90.0,
 ticks: [
  (tick: 0, head: (position: (0.0, 1.7, 0.0)), state: Some(Focused)),
  (tick: 10, head: (position: (0.0, 1.7, -0.2)), right: Some((position: (0.3, 1.2, -0.3))),
   inputs: [("grab", "right", Float(1.0))]),
 ])
```

| Field | Default | Meaning |
|---|---|---|
| `format_version` | required | Must be 1. |
| `fov_deg` | 100 | Full symmetric field of view, 10 to 170. |
| `ipd_m` | 0.063 | Interpupillary distance, within the platform range. |
| `refresh_hz` | 90 | Display refresh rate, 30 to 240. |
| `ticks` | required | Keyframes; ticks strictly increasing. |

Each tick: `tick` (u64), `head` (pose), `left` / `right` (optional pose; `None` is untracked), `inputs` (list of
`(action, sub-path, value)` applied at this tick and held until replaced) and `state` (optional session state the
runtime moves to at this tick: `Ready`, `Synchronized`, `Visible`, `Focused`, `Stopping`, `LossPending`, `Exiting`).

A pose is `(position: (x, y, z), orientation: (x, y, z, w))` in meters and a quaternion; `orientation` defaults to the
identity and is normalized on use. A value is `Bool(b)`, `Float(f)` or `Vec2(x, y)`.

## Validation

Newer versions are `FormatTooNew`; version other than 1, ranges above, non-finite poses, non-finite input
values (`Float` / `Vec2` of NaN or infinity) and non-increasing ticks are `invalid_value` errors naming the key or
tick. Unknown fields are rejected. Agent surface: the simulator drives in `book/agents/xr.md`.
