# Wheel surface effects: smoke, dust, spray, skid marks and ruts

A moving wheel leaves marks and raises smoke and dust (`regia_framework::vehicle_fx`, logic in
`regia_vfx::vehicle_fx`, ADR-3274). Add `VehicleFxPlugin` after `PhysicsPlugin`, `VehiclePlugin` and
`VfxPlugin`; it needs the `client` feature of `regia_framework`. It reads each `VehicleController`'s wheels every
fixed step and **does not touch the vehicle code**: the slip comes from the contact point moving between steps, the
wheel's heading and its spin, so it works with every drivetrain.

## What a wheel does

| Surface | Smoke | Dust | Spray | Mark |
|---|---|---|---|---|
| `asphalt`, `concrete` | when the combined slip is over `smoke_slip_threshold` (0.25), density `(slip - threshold) / (1 - threshold) x clamp(speed / 20, 0.2, 1) x effect_scale` (speed in m/s, the faster of ground and wheel-roll speed) | | | **skid** while slip is over `skid_slip_threshold` (0.3; ends under 80 % of it) |
| `gravel`, `dirt`, `sand`, `grass`, `snow` | | rises with speed and slip | | **rut** while the wheel rolls over `min_speed` |
| `mud` | | | by speed | **rut** |
| `water` | | | by speed | none |

Combined slip is `hypot((wheel_speed - v_long) / max(|v_long|, 1), v_lat / max(|v_long|, 1))`, at most 2. The three
built-in effects are `tire_smoke`, `wheel_dust` and `wheel_spray`; they register themselves on first use, one
instance per wheel and effect, with the intensity from the logic.

**Marks.** A strip is a run of overlapping projected stamps laid out as `DecalStrip` lays them (same `spacing`,
carry between segments, stamp length and orientation). The polyline gets a new vertex when the contact moved at
least `spacing`, or turned more than `max_turn` over at least a quarter of it. At most `max_stamps` stamps are
alive across all wheels (the oldest retires first); a stamp also retires after `mark_lifetime_s`, its decal fading
out over the last 30 % of that. The cap comes from the quality tier (24 on MobileLow, 48 on Low, 96 on Medium,
128 on High, 512 on Ultra) and never exceeds half the decal pool; a limit set through the agent sticks until the
tier moves.

**Surface lookup**, first match wins: a `WheelSurfaceOverride` on the ground entity; the ground entity's
`PhysicsMaterialRef` name (`gravel_rough` is gravel; words: asphalt, tarmac, road, concrete, pavement, gravel,
pebble, rock, sand, mud, snow, ice, grass, water, puddle, dirt, soil); the host's `terrain_probe` (a function from
a world position to a surface, which a game backs with its terrain layers); `default_surface` (asphalt).

Determinism: fixed steps and fixed inputs only; no clock, no randomness; two runs on the same wheel samples agree
bit for bit. After the warm-up a logic step does not allocate.

## Read: `vfx.vehicle_fx`

```json
{}
→ { "enabled": true, "default_surface": "asphalt", "terrain_probe": false,
    "config": { "spacing": 0.5, "max_turn": 0.14, "smoke_slip_threshold": 0.25, "skid_slip_threshold": 0.3,
                "min_speed": 0.8, "mark_lifetime_s": 60.0, "max_stamps": 128, "effect_scale": 1.0 },
    "stamps": { "live": 48, "emitted": 900, "retired": 852, "retired_by_cap": 852, "cap": 48 },
    "count": 4,
    "wheels": [ { "key": 4294967310, "surface": "asphalt", "slip": 0.58, "slip_ratio": 0.2,
                  "slip_lateral": 0.54, "speed": 15.0, "mark": "skid", "smoke": 0.31, "dust": 0.0,
                  "spray": 0.0, "smoke_rate": 37.2, "dust_rate": 0.0, "spray_rate": 0.0,
                  "new_stamps": 1, "active": ["tire_smoke"] }, ... ] }
```

## Change: `vfx.vehicle_fx_set`

Every field is optional and validated before anything changes. Returns `{ before, after }`, each as above.

| Field | Range | Meaning |
|---|---|---|
| `enabled` | bool | `false` stops the effects and clears the marks |
| `default_surface` | a surface name | the surface when nothing names one |
| `spacing` | 0.05 to 5 | stamp spacing, meters |
| `max_turn` | 0.01 to 1 | turn that starts a new vertex, radians |
| `smoke_slip_threshold` | 0.01 to 0.95 | slip over which a hard surface smokes |
| `skid_slip_threshold` | 0.01 to 2 | slip over which a hard surface takes a skid |
| `min_speed` | 0 to 20 | lowest speed for ruts, dust and spray, m/s |
| `mark_lifetime_s` | 0.5 to 3600 | stamp life, seconds |
| `max_stamps` | 0 to 8192 | stamp cap across all wheels |
| `effect_scale` | 0 to 2 | multiplies every effect intensity |
| `clear_marks` | bool | `true` retires every stamp |

## Error codes

Reported in `error.data.reason`:

| Reason | When |
|---|---|
| `no_vehicle_fx` | The app has no `VehicleFxState` (add `VehicleFxPlugin`); `F0016`. |
| `unknown_field` | A parameter the method does not take (`F0002`). |
| `invalid_value` | A value of the wrong type or out of range (`F0002`). |
| `unknown_surface` | `default_surface` is not one of the surfaces above (`F0002`). |

## Check it

1. Drive the car straight on asphalt: `vfx.vehicle_fx` shows `slip` near 0, `mark: "none"`, `stamps.emitted` 0.
2. Brake hard from speed with ABS off: the wheels show `mark: "skid"` and `active: ["tire_smoke"]`, and `stamps.live` grows by one per `spacing` meters.
3. Drive onto gravel (`WheelSurfaceOverride` or a `gravel` physics material): `wheel_dust` rises with speed and ruts follow the wheels.
4. Hold a slide: `stamps.live` stops at `stamps.cap` and `retired_by_cap` counts up.

Surface effects with a screenshot need a GPU; the counts above do not.
