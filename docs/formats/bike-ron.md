# .bike.ron: two-wheel bike v1

Library: `regia_physics::vehicle_bike::asset` (`BikeAsset::{from_ron, to_ron, validate, into_config}`,
`migrate_bike`, `BIKE_FORMAT_VERSION`, `BIKE_VERSION_TABLE`, `bike_schema_fingerprint`,
`bike_migration_sources`). Tune one: the Vehicle Tuning panel (`asset: "bike"`, `book/agents/vehicle-tuning.md`).
Step the model: `physics.bike_step` (`book/agents/bike.md`). Decision: ADR-8630 (model: ADR-4017).

A `.bike.ron` is a display name, a `BikeConfig` (the two-wheel balance model's lengths and rates) and an
optional note. Fields are named, never positional. Every `config` field is optional: a missing one takes
`BikeConfig::default()`'s value, and a file without a `config` block is the default bike. Angles are radians,
lengths meters, speeds m/s.

```ron
(
    format_version: 1,
    name: "city",
    config: (
        wheelbase: 1.4,
        gravity: 9.81,
        max_lean: 1.05,
        max_lean_rate: 3.0,
        upright_speed: 2.0,
        upright_rate: 4.0,
        lean_response: 5.0,
        contact_reach: 0.7,
    ),
    notes: Some("a light commuter: quick to lean"),
)
```

The example is the canonical text `BikeAsset::to_ron` writes (every field, four-space indent, LF line ends, a
final LF) and the frozen corpus file `crates/regia_agent/tests/fixtures/format-corpus/bike/v1.ron`.

## Top level

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. Another value fails with `bike_version`, which names the file's version; the loader reads the version before the body, so a newer file's unknown shape does not hide it. No older layout exists. |
| `name` | A display name. Empty or only whitespace fails with `bike_bad_name` (field `name`). |
| `config` | The `BikeConfig` fields below, each optional. |
| `notes` | Free text a tool may show (`None` when omitted). The step does not read it. |

## `config` (`BikeConfig`)

| Field | Default | Valid | Meaning |
|---|---|---|---|
| `wheelbase` | 1.5 | 0.2 to 10 | Distance between the wheel contacts, meters (`L` in the lean law). |
| `gravity` | 9.81 | 0.1 to 30 | Gravity, m/s² (`g`). |
| `max_lean` | 1.05 | 0.05 to 3.15 | A lean past this, radians, is a fall. |
| `max_lean_rate` | 2.5 | 0.1 to 20 | Clamp on the lean rate, rad/s. |
| `upright_speed` | 2.0 | 0 to 150 | Below this speed, m/s, the lean springs to 0 and the steer term is off. |
| `upright_rate` | 4.0 | 0.1 to 40 | The upright spring's rate below `upright_speed`, 1/s. |
| `lean_response` | 4.0 | 0.1 to 40 | How fast the lean tracks `atan(v² tan(steer) / (L g))`, 1/s. |
| `contact_reach` | 0.7 | 0 to 5 | A wheel ray at or under this length, meters, is that wheel in contact. |

The ranges are `BikeConfig::FIELD_RANGES`, the same check `bike_step` makes. A value that is not finite or is
outside its range fails with `bike_bad_input` (the code `physics.bike_step` answers for the same
configuration), and the error names the field (`BikeAssetError::field_name`).

## Errors

| Code | When |
|---|---|
| `bike_parse` | Not RON, a positional tuple, no `format_version`, or an unknown or misspelt field name (the message names it). |
| `bike_version` | `format_version` is not 1. |
| `bike_bad_name` | `name` is empty or only whitespace. |
| `bike_bad_input` | A `config` field is not finite or is outside its range; the error names the field. |

RON `implicit_some` is on, so `notes: "text"` and `notes: Some("text")` are the same.

`regia upgrade-project` knows the suffix `.bike.ron` and leaves a v1 file as it is; a newer file is reported
with its version. `migrate_bike` rewrites a v1 file in canonical form. The fuzz target is `bike_ron`.
