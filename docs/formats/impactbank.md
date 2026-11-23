# .impactbank.ron: contact-sound bank v1

Library: `regia_audio::impacts` (`ImpactBankDef::{from_ron, to_ron, validate}`, `ImpactBank`, `ImpactSet`,
`ImpactPair`, `ImpactCurves`, `ImpactBudget`). Runtime: `regia_framework::physics_audio::PhysicsAudioPlugin`. Agent:
`audio.impacts_set { ron }` loads one, `audio.impact_probe { ron, ... }` evaluates one without loading it. Guide page:
`book/agents/physics-audio.md`. Decision: ADR-3270.

A bank names the sounds physics contacts play: impact one-shots, a scrape loop, a roll loop and break one-shots per
physics material (the names colliders carry in `PhysicsMaterialRef`) and per material pair, plus the level curves and
the voice budget. Every field except `format_version` is optional and takes the default below.

```ron
(
    format_version: 1,
    materials: {
        "wood": (
            impacts: ["impact/wood_1", "impact/wood_2", "impact/wood_3", "impact/wood_4"],
            scrape: Some("impact/wood_scrape"),
            roll: Some("impact/wood_roll"),
            breaks: ["impact/wood_break_1"],
            volume_db: -3.0,
        ),
        "default": (impacts: ["impact/thud_1", "impact/thud_2"]),
    },
    pairs: [(a: "metal", b: "concrete", set: (impacts: ["impact/clank_1", "impact/clank_2"]))],
    default_material: "default",
    destruction_materials: { "brick": "concrete" },
    curves: (reference_impulse_ns: 10.0),
    budget: (max_new_impacts_per_step: 8),
    bus: "World",
    max_distance_m: 40.0,
)
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. Another value fails with `impactbank_version`; no older files exist. |
| `materials` | Material name → set. Names are not empty (`impactbank_name`). |
| `pairs` | `(a, b, set)`: the set two materials play together, in either order; checked before either material's own set. |
| `default_material` | The material whose set plays when neither material (nor their pair) has one. Default `"default"`. |
| `destruction_materials` | Destruction `fx_tag` → material for break sounds; an unlisted tag is its own material. |
| `curves` | The level and pitch curves (below). |
| `budget` | `cooldown_steps` (6, ≤ 600), `max_new_impacts_per_step` (8, 1..=256), `fade_steps` (6, 1..=600), `max_loop_voices` (16, ≤ 256). |
| `bus` | Bus the sounds play on, not empty. Default `"World"`. |
| `max_distance_m` | Range of the emitter, metres, > 0. Default 40. |

A set (`ImpactSet`): `impacts` (one-shot variants, at most 64, names not empty: `impactbank_sounds`), `scrape` and
`roll` (`Some("sound")` or `None`), `breaks` (one-shots for destruction; empty uses `impacts`), `volume_db`
(−60..=12, default 0), `pitch` (0.25..=4, default 1), `pitch_jitter` (0..=0.5, default 0.05) and `volume_jitter_db`
(0..=12, default 1.5). Sound names are `SoundAssets` names.

Curves (`ImpactCurves`, every level in dB relative to the set, `value_db = db_per_doubling × log2(value / reference)`);
out of range or not finite fails with `impactbank_value`:

| Field | Default | Range | Meaning |
|---|---|---|---|
| `min_impulse_ns` | 0.05 | ≥ 0 | Impulses below this (N·s) make no impact. |
| `min_normal_speed_m_s` | 0.25 | ≥ 0 | Approach speeds below this make no impact (no retrigger at rest). |
| `reference_impulse_ns` | 10 | > 0 | The impulse that plays at the set's level. |
| `impulse_db_per_doubling` | 4 | 0..=24 | Level per doubling of the impulse. |
| `reference_speed_m_s` | 5 | > 0 | The approach speed that plays at the set's level. |
| `speed_db_per_doubling` | 2 | 0..=24 | Level per doubling of the approach speed. |
| `max_boost_db`, `max_cut_db` | 12, 40 | 0..=48, 0..=96 | The impact terms stay within `−max_cut_db..=max_boost_db`. |
| `speed_pitch_exponent` | 0.1 | 0..=1 | Impact pitch `(speed / reference)^k`, within 0.8..1.25. |
| `scrape_min_speed_m_s` | 0.05 | ≥ 0 | Slower slips do not scrape. |
| `scrape_reference_power_w` | 20 | > 0 | Slip speed × normal force that scrapes at the set's level. |
| `scrape_db_per_doubling` | 3 | 0..=24 | Scrape level per doubling of that power. |
| `scrape_pitch_reference_m_s`, `scrape_pitch_exponent` | 1, 0.3 | > 0, 0..=1 | Scrape pitch `(slip / reference)^k`, within 0.5..2. |
| `roll_min_speed_m_s` | 0.05 | ≥ 0 | Slower rolling does not roll. |
| `roll_reference_speed_m_s` | 2 | > 0 | Rolling speed (angular speed × radius) at the set's level. |
| `roll_db_per_doubling`, `roll_pitch_exponent` | 6, 0.5 | 0..=24, 0..=1 | Roll level and pitch `(speed / reference)^k` within 0.5..2. |
| `roll_slip_ratio` | 0.25 | 0..=1 | A contact rolls while its slip is at most this fraction of its rolling speed. |
| `loop_floor_db`, `loop_max_db` | −40, 6 | −96..=0, 0..=24 | Quieter loops are silent; louder ones are capped. |

Migration: none at v1 (`impactbank_migration_sources()` is empty). A shape change needs a version bump, a migrator and
`regia upgrade-project` support (spec 12 § 4a). The format is registered in `cargo xtask check-formats` and
`regia upgrade-project` as `impactbank`, and fuzzed by the `impactbank` target.
