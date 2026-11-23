# `*.siminput.ron` (SimRecording, format_version 1, dev only)

Recorded inputs for simulated clients (spec 77 § 10.3, ADR-1663): `net.sim_clients { action: "add", input:
"recording", recording: "<path>" }` or `NetSession::add_simulated_clients(n, SimInput::Recording(frames))`. Each
simulated client in game sends one frame per tick as a game session message (`SessionMsg::Game`), looping. Read with
`regia_net::session::hub::SimRecording::from_ron`.

```ron
SimRecording(
    format_version: 1,
    frames: [[1, 0, 0, 0], [1, 0, 1, 0], [0, 0, 0, 0]],
)
```

| Field | Meaning |
|---|---|
| `frames` | One game message per tick (bytes, at most 60 KiB each), played in order and looped |

**Versioning.** Dev-only tool input, `format_version: 1`; other versions are refused. Not shipped content; a layout
change bumps the version. Registered in `cargo xtask check-formats` (`siminput`) and `regia upgrade-project` since
ADR-2316.
