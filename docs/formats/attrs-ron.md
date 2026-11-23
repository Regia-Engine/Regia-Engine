# `*.attrs.ron` (attribute list, v1)

Spec: `docs/specs/53-abilities-attributes-effects.md` § 4.2. Format id `attrs` (ADR-1695; ADR-5639), version 1
(`regia_framework::abilities::ATTRIBUTES_FORMAT_VERSION`). Read by `AbilityAssets::load_text` /
`load_file` and `abilities.add_attributes`; validated by `abilities.validate`.

```ron
(
    format_version: 1,              // must be 1; a newer version is refused (`format_too_new`)
    description: "Hero stats",      // optional
    attributes: [                   // declaration order is kept
        (name: "health_max", base: 100.0, min: 1.0, max: 9999.0),
        (name: "health", base: 100.0, min: 0.0, max: 9999.0, max_from: Some("health_max")),
        (name: "strength", base: 10.0, min: 0.0, max: 100.0),
        (name: "armor", base: 0.0, min: 0.0, max: 1000.0,
         derived: Some((inputs: [("strength", 2.0)], offset: 0.0))),
    ],
)
```

| Field | Type | Meaning |
|---|---|---|
| `format_version` | `u32` | 1 |
| `description` | string, optional | what the list is for |
| `attributes[].name` | string | `[a-z][a-z0-9_]{0,47}`, unique per entity; the id is FNV-1a 64 of the name |
| `attributes[].base` | `f32` | starting base (clamped to `[min, max]`) |
| `attributes[].min`, `max` | `f32` | the clamp range, `min <= max` |
| `attributes[].max_from` | `Option<String>` | the upper bound is `min(max, other.current)` every step |
| `attributes[].derived` | `Option<(inputs: [(name, weight)], offset)>` | `base = offset + sum(input.current * weight)` every step |

- Unknown top-level fields are errors (`deny_unknown_fields`) reported with a line and column.
- Validation codes: `attribute_duplicate`, `attribute_range`, `attribute_cycle` (a `max_from` or
  derived cycle), `attribute_name`, `attribute_not_finite`, `format_too_new`, `unknown_field`,
  `parse_error`.
- Written canonically by `regia_framework::abilities::to_ron` (pretty RON, trailing newline); a
  resave is byte-identical.
- Migrations: none (v1 is the first version; `attributes_migration_sources()` is empty). Any shape
  change bumps the version and adds a step in `regia_framework::abilities::migrate`.

## Cooked form

Cooked with the other ability formats as `abilities-cooked` (ADR-7216); see [the effect format](effect-ron.md#cooked-form).
