# `.regiademo` — demo recording (RON)

Owner: `regia_framework::demo` (ADR-1569; version 2 by ADR-1878). Current `format_version`: **2**.
Version 1 files load (migrated in memory) and `regia upgrade-project --write` rewrites them as version 2.

A demo is a recorded input stream with periodic keyframes, so a viewer can replay it and jump to any tick.
It is user data (written under `saved/demos/` by a game or `demo.record_stop`), not cooked content.

## Layout

RON text of one struct, followed by the line `// regia-demo-end`. A file without that last line is
`truncated` (cut off while copying or writing); a file whose text does not parse is `parse`.

```ron
(
    header: (
        format_version: 2,
        engine_version: "0.2.6",
        build_hash: "c0ffee1",     // game build identifier; "" = unknown
        content_hash: "a1b2c3",    // hash of the loaded content; "" = unknown
        seed: 42,                  // RNG seed the simulation was seeded with
        level: "arena",
        tick_rate_hz: 60,          // fixed steps per second (> 0)
        step_count: 600,           // must equal the number of `steps` rows
        keyframe_interval: 60,     // steps between keyframes (0: start and end only)
    ),
    actions: ["Fire", "Move"],     // tracked action names, in row order
    steps: [                       // one row per fixed step: one [x, y, z] per action
        [[0.0, 0.0, 0.0], [0.25, 0.0, 0.0]],
        // ...
    ],
    keyframes: [                   // strictly ascending ticks, none past step_count
        (tick: 0, state_hash: 1234567890, snapshot_hex: "0a01..."),
        // ...
    ],
    timestamps: [                  // v2: per step, input event arrival in µs since recording started
        [16667, 16901],            // empty list: no input that step
        // ...                     // or the whole list empty: no timestamps recorded
    ],
)
// regia-demo-end
```

- A keyframe's `state_hash` is the simulation state **before** step `tick` runs (tick 0 is the initial state,
  tick `step_count` the final one). `snapshot_hex` is the simulation's snapshot as lowercase hex; empty means a
  hash-only keyframe (checked during replay, never a seek start). With `regia_framework::demo_world`, the
  snapshot is the world snapshot of the tracked entities and resources (postcard, its own version 1 field).
- `timestamps` are analysis data. A replay is indexed by step and never reads them.

## Versions

| Version | Change | Migration |
|---|---|---|
| 1 | First version (ADR-1569). | — |
| 2 | Adds `timestamps` (ADR-1878). | v1 loads with empty timestamps; `regia upgrade-project` rewrites the file at v2. A v1 file that carries `timestamps` is `invalid`. |

## Limits and errors

At most 256 MiB. Error codes (`DemoError::code`, `demo_` prefixed on the agent protocol): `truncated`, `parse`,
`unsupported_version`, `invalid` (step count, row width, non-finite value, keyframe order or range, snapshot hex,
timestamp row count), `too_large`, `io`.

The binary form of the same data is the [`.frec` match record](frec.md).
