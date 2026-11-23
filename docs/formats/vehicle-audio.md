# .vehicleaudio.ron: vehicle sound v1

Library: `regia_audio::vehicle_audio` (`VehicleAudioDef::{from_ron, to_ron, validate, migrate_to_latest,
placeholder}`, `VehicleAudioModel`, `vehicle_audio_schema_fingerprint`, `vehicle_audio_migration_sources`).
ECS: `regia_framework::vehicle_audio::VehicleAudio`. Agent: `audio.vehicle_audio_set { ron }`,
`audio.vehicle_audio_probe { ron }`. Guide page: `book/agents/vehicle-audio.md`. Decision: ADR-3271.

A `.vehicleaudio.ron` describes how a vehicle sounds: engine loops recorded at known engine speeds (an
on-load and an off-load set), an optional whine, tire squeal, rolling noise per surface, suspension bumps,
wind and the gear-change one-shot. Only `format_version`, `name` and `engine.on_load` are required; every
other field has the default shown below.

```ron
(
    format_version: 1,
    name: "hatchback",
    bus: None,                       // default "World"
    engine: (
        on_load: [
            (rpm: 1000.0, sound: "car/on_1000.ogg"),
            (rpm: 2500.0, sound: "car/on_2500.ogg"),
            (rpm: 4500.0, sound: "car/on_4500.ogg", gain_db: -1.0),
        ],
        off_load: [(rpm: 1000.0, sound: "car/off_1000.ogg"), (rpm: 4000.0, sound: "car/off_4000.ogg")],
        volume_db: 0.0,
        off_load_db: -4.0,
        min_pitch: 0.5,
        max_pitch: 2.0,
        throttle_weight: 0.6,
        peak_torque_nm: 300.0,
        redline_rpm: 7000.0,
        limiter: (margin_rpm: 120.0, cut_db: -14.0, rate_hz: 16.0, min_throttle: 0.5),
        shift: Some((sound: "car/shift.ogg", gain_db: -6.0, dip_db: -8.0, dip_secs: 0.12, on_downshift: false)),
        on_load_slot: Exhaust,
        off_load_slot: Engine,
    ),
    whine: Some((sound: "car/whine.ogg", recorded_rpm: 5000.0, gain_curve: [(1000.0, -40.0), (7000.0, -16.0)], off_load_share: 0.4)),
    tires: (
        squeal: Some((sound: "car/squeal.ogg", gain_db: -8.0, slip_angle_peak_deg: 8.0, slip_ratio_peak: 0.12,
                      threshold: 0.6, full: 1.6, pitch_spread: 0.15)),
        rolling: [
            (surface: "default", sound: "car/roll_asphalt.ogg", gain_curve: [(0.0, -100.0), (1.0, -40.0), (30.0, -18.0)], pitch_per_mps: 0.01),
            (surface: "gravel", sound: "car/roll_gravel.ogg", gain_curve: [(0.0, -100.0), (1.0, -30.0), (30.0, -12.0)]),
        ],
        bump: Some((sound: "car/bump.ogg", gain_db: -6.0, rate_threshold_mps: 0.6, full_rate_mps: 3.0, cooldown_secs: 0.15)),
    ),
    wind: Some((sound: "car/wind.ogg", gain_curve: [(0.0, -100.0), (5.0, -50.0), (40.0, -14.0)], pitch_per_mps: 0.005)),
    smoothing: (rpm_secs: 0.03, load_secs: 0.08, gain_secs: 0.05, tire_secs: 0.06),
    emitters: (engine: (0.0, 0.3, -1.4), exhaust: (0.4, -0.25, 2.1)),
)
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. Another value (or 0) fails with `vehicle_audio_version`; no older files exist. |
| `name` | Not empty. |
| `bus` | Bus of every voice; default `World`. |
| `engine.on_load`, `engine.off_load` | Loops with the rpm they were recorded at, strictly rising, at most 8 per set; `on_load` needs at least one, an empty `off_load` lets the on-load set play alone. A layer plays at `rpm / recorded rpm` clamped to `min_pitch..=max_pitch` (0.05..=1 and 1..=8). |
| `engine.volume_db`, `off_load_db` | Engine level, and the off-load set's extra level (−100..=24 dB). |
| `engine.throttle_weight`, `peak_torque_nm` | Load target = `throttle_weight × throttle + (1 − throttle_weight) × clamp(torque / peak_torque_nm, 0, 1)`. |
| `engine.redline_rpm`, `limiter` | The rev limiter cuts to `cut_db` at `rate_hz` within `margin_rpm` of the redline with at least `min_throttle`. |
| `engine.shift` | One-shot per upshift (and per downshift with `on_downshift`), with a dip of the engine to `dip_db` for `dip_secs`. |
| `engine.on_load_slot`, `off_load_slot` | `Engine` or `Exhaust` (a `Wheel` slot fails with `vehicle_audio_value`). |
| `whine` | A loop at `rpm / recorded_rpm`, level `gain_curve(rpm)` dB, `off_load_share` of it off load. |
| `tires.squeal` | Level `gain_db` × the squeal amount, which is 0 at or below `threshold` of combined slip `√((|α|/slip_angle_peak_deg)² + (|κ|/slip_ratio_peak)²)` and 1 at `full` (> `threshold`). |
| `tires.rolling` | Per surface (`default` is the fallback, else the first): level `gain_curve(contact speed m/s)` dB, pitch `1 + pitch_per_mps × speed`. |
| `tires.bump` | One-shot when a wheel compresses faster than `rate_threshold_mps`, louder up to `full_rate_mps`, once per `cooldown_secs`. |
| `wind` | Level `gain_curve(chassis speed m/s)` dB. |
| `smoothing` | One-pole time constants in seconds (0 = none, at most 10). |
| `emitters` | Chassis-space offsets in meters (forward −Z, up +Y). |

Curves are `(x, dB)` points with strictly rising `x` and levels at most 24 dB; a bad point fails with
`vehicle_audio_value` naming it (`whine.gain_curve[1]`). Sound names starting `placeholder:` are generated in
code (`book/agents/vehicle-audio.md`); every other name is looked up in `SoundAssets`, and a missing one leaves
its layer silent (reported as `missing_sounds`).

Migration: v1 is the first version; `vehicle_audio_migration_sources()` is empty and `regia upgrade-project`
lists the format with nothing to rewrite.
