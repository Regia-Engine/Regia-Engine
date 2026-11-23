# Net hover hulls (agents)

Hover hulls in multiplayer: `regia_framework::net_hover` (spec 73 § 9.2, ADR-8819). Guide topic `net-hover`. The module
is behind the `net` feature of `regia_framework`, which the `net` feature of `regia` and the agent server turn on.

A hover hull's step is `det_hover::tick_hull`, a pure function of the hull's tuning, its `DetHoverState`, its input, the
floor and `dt`. So a hull re-runs per entity, like a character: the server steps every hull in `regia_net`'s predicted
schedule, the owning client steps its own copy ahead of the server, and after a correction the client writes the
server's state back and re-runs the ticks since. Other clients interpolate the replicated `Transform`.

## Setting it up

<!-- ignore-reason: an outline across processes: `mode`, `peer`, `my_hull` and `Axes` are the game's own -->
```rust,ignore
app.add_plugins((NetPlugin { mode }, NetHoverPlugin));
app.insert_resource(DetHoverFloor::new(my_floor)); // the same static floor on every process

// Every process, before the first spawn: the tuning does not travel, only its key.
app.world_mut().resource_mut::<NetHoverHulls>().insert(my_hull);

// The server spawns it; a client gets a copy.
world.spawn((
    Replicated { transform: TransformReplication::Full, ..Replicated::default() },
    my_hull,
    NetHover::for_hull(&my_hull, 0),
    DetHoverState::at_rest([x, floor, z], yaw, &my_hull.params),
    NetPredicted,
    NetOwner(peer),
));

// Each fixed tick on the client, before NetSet::Predict.
fn write_input(mut q: Query<&mut InputCommand<HoverNetInput>>, axes: Res<Axes>) {
    for mut command in &mut q {
        command.value = HoverNetInput {
            throttle: axes.throttle,
            strafe: axes.strafe,
            steer: axes.steer,
            hop: axes.hop_pressed,
            jump: axes.jump_pressed,
        };
    }
}
```

- `NetHover` requires `DetHoverSteppedExternally`: `DetHoverPlugin` neither steps nor presents the hull (the predicted
  schedule does both), so the two plugins can run in one app.
- A hull with no owner belongs to the server and reads its own `DetHoverInput` there (AI writes it).
- `HoverNetInput` is quantized: 8 bits per axis (zero is exact), and the step runs on `quantized()` on both sides, so the
  client predicts with what the server receives. A missing command repeats the axes and clears `hop` and `jump`.
- The hull steps with `DetHoverConfig::dt`; set it to the net tick step. A `dt` more than 0.1 ms off it logs
  `net.hover_dt_mismatch` once and sets `stats.dt_mismatch`.
- A game with richer input implements `HoverCommand` on its own `NetInput` type and calls `install_net_hover::<I>`
  instead of adding the plugin. With characters or vehicles in the same game, use one input type for all of them.

## Hull tunings

`NetHover::hull` is `hull_key(&hull)`: FNV-1a 64 over the hull's 273 lockstep handshake bytes and its footprint, the
same on every target. Each process looks the tuning up in `NetHoverHulls` (which starts with `DetHoverHull::DEFAULT`).
A copy whose own `DetHoverHull` has another key takes the listed one. A key a process never listed is not guessed: the
hull does not step there, `stats.unknown_hull` counts it and `net.hovers` shows `fault: "hover_hull_unknown"`.

## Corrections and what is drawn

The predicted components are `DetHoverState` (on the wire at full `f32` precision: a rollback must restart from the
server's bits) and `Transform` (derived from the state; the rollback and the visual offset read it). One tolerance
decides, on the state alone:

| Field | Tolerance |
|---|---|
| position, hover height (`altitude`), `prev_terrain` | 0.01 m |
| yaw, roll, pitch, cushion pitch, ground pitch and roll, slope | 0.01 rad (wrapped) |
| velocity, `vertical_vel`, `coast_drop_left` | 0.05 m/s |
| yaw, roll and cushion pitch rates, `ground_climb` | 0.05 per second |
| `hop_cooldown` | 1/240 s |
| `airborne`, `jump_active`, `level_off`, `yaw_modern`, `thrusting` | exact |

Same-build peers step the same bits, so on one platform a hull never corrects unless the server's state really
differs (a lost input that repeated, a push, `net.force_correction`). Draw the `Transform` plus `NetVisualOffset`: the
offset takes the jump and decays with `prediction.smooth_half_life_ms`; a jump over `prediction.snap_distance_m` snaps.

On the server an outside move of the hull's `Transform` (knockback, `net.force_correction`) is taken into its state at
the next step (`stats.adopted_moves`); the rotation is always the step's.

## Effects that must not repeat

`tick_hull` changes nothing outside the hull, so `NetResimulating` suppresses nothing in the step. Hop and jump launches
are the effects: they reach `NetHoverEffects` (entity, tick, `Hop` or `Jump`) on the first run of a tick on a client and
on the server, never on a re-run. Drain it to play sounds and particles; past 256 undrained launches the oldest are
dropped (`dropped`). `stats.hops` and `stats.jumps` count first runs only; `stats.resim_steps` counts re-runs.

## Methods

### `net.hovers`

Read. Params: none.

```json
{ "hovers": [{ "entity": 4294967301, "net_id": 3, "role": "predicted", "hull_key": "8b0d6b1d4f3e2a19",
    "hull_known": true, "position": [12.0, 1.4, 30.2], "rendered_position": [12.0, 1.4, 30.2],
    "velocity": [0.0, 0.0, 18.5], "yaw": 0.12, "altitude": 1.4, "airborne": false, "fault": null }],
  "stats": { "steps": 600, "resim_steps": 0, "hops": 2, "jumps": 1, "recovered": 0, "unknown_hull": 0,
    "adopted_moves": 0, "dt_mismatch": false },
  "prediction": { "corrections": 0, "snaps": 0, "last_correction_m": 0.0 } }
```

`role` is `authority`, `predicted` or `proxy`. `prediction` is `null` on a server. `net.prediction` has the full
correction stats and `net.force_correction` moves a hull on the server.

## Error codes

| Code | When |
|---|---|
| `net_not_running` (`F0002`, `data.reason`) | `net.hovers` on a process where `NetHoverPlugin` / `install_net_hover` never ran |
| `hover_input_range` | a decoded `HoverNetInput` axis is -128 (the encoder never writes it): the command is refused |
| `hover_hull_unknown` | `net.hovers` `fault`: the hull's tuning key is in no `NetHoverHulls` list here, so it does not step |

## Tests

`cargo test -p regia_framework --features net --test net_hover` (150 ms round trip with 2 % loss, a clean link, a
server push, re-runs across a hop, an unlisted hull, the offline plugin) and `cargo test -p regia_framework --features net
--lib net_hover` (codecs, the tolerance, the hull key, restore-then-step, the cross-target pin).
