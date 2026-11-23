# .utility.ron: utility AI definition (v1)

ADR-2336, ADR-2565. Code: `regia_ai::utility` (`UtilityAiDef`), loaded by `regia_ai::utility_ecs`. Guide:
`book/agents/utility-ai.md`. Version 1 is new: there is no migrator and `regia upgrade-project` has nothing to do. A
`format_version` other than 1 is refused (`unsupported_version`) before the body is read. `//` comments are read.

A reasoner entity names a file by its path under `assets/` (`UtilityReasoner::path`). The file ships raw in a package, like
`.statetree.ron`; it is not cooked.

## Schema

```
UtilityAiDef {
    format_version: u32,                    // 1
    description: String = "",               // free text, never parsed
    seed: u64 = 0,                          // the WeightedRandom generator's seed
    selection: SelectionMode = Best,
    actions: [ActionDef],                   // 1 to 65536, unique names
}
SelectionMode = Best | WeightedRandom(top_n: usize, margin: f32)   // top_n >= 1, margin finite and >= 0
ActionDef {
    name: String,                           // unique, not empty; reported by ai.utility_explain
    considerations: [ConsiderationDef] = [],  // up to 64; none means the action scores 0 and never wins
    weight: f32 = 1.0,                      // finite, >= 0
    cooldown: f32 = 0.0,                    // seconds of fixed-step time; finite, >= 0
    momentum: f32 = 0.0,                    // bonus while current; finite, >= 0
}
ConsiderationDef {
    key: String,                            // a blackboard key; not empty
    min: f32 = 0.0,                         // normalizes to 0
    max: f32 = 1.0,                         // normalizes to 1; finite, > min
    curve: ResponseCurve = identity,
}
ResponseCurve {
    kind: Linear | Quadratic | Logistic | Logit | Step | Piecewise = Linear,
    slope: f32 = 1.0,
    exponent: f32 = 2.0,
    x_shift: f32 = 0.0,
    y_shift: f32 = 0.0,
    points: [CurvePoint] = [],              // Piecewise only: at least 2, x strictly increasing
}
CurvePoint { x: f32, y: f32 }
```

The formula of each curve kind is in the book page. Load errors, as `UtilityAiError::reason`: `parse`,
`unsupported_version`, `empty`, `too_many_actions`, `empty_name`, `duplicate_name`, `too_many_considerations`, `bad_value`,
`bad_range`, `bad_curve`. A file that fails to load keeps the definition in use and reports why (`ai.utility_list`,
`definitions[].error`). A path outside `assets/` is `bad_path`; a file that is not there is `definition_not_found`.

## Example

See `docs/formats/utility.example.ron` (the fuzz fixture is a byte copy of it).

## Migration note

Before v0.1.28 the text form existed without a loader, under the same shape and version (ADR-2336). Files written then load
unchanged. A shape change from here on needs a version bump and a migrator in `regia_ai::utility::migrate_ron`
(`docs/specs/12-assets.md` § 4a).
