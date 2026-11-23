# .detutility.ron: deterministic utility catalogue (v1)

ADR-8609 (gap AI-7). Code: `regia_detsim::ai::utility_catalogue` (`DetUtilityFile`, `UtilityCatalogue`),
re-exported as `regia_ai::det`. Guide: `book/agents/det-ai-core.md` ("Your own catalogue") and
`book/user-guide/deterministic-simulation.md` ("Authoring a utility catalogue"). Registry name
`ai-det-utility`. Version 1 is new: there is no migrator and `regia upgrade-project` has nothing to
rewrite (the version is registered so a version 2 can migrate). A `format_version` above 1 is refused as
`det_utility_newer_version` before any other field is read; 0 or a missing version is
`det_utility_unsupported_version`.

The file is sim data: every peer of a lockstep match must load the same bytes. A headless sim reads it
with `UtilityCatalogue::from_ron_str` (no asset system, no IO). The asset pipeline cooks it (importer
`det_utility`, type path `regia_detsim::ai::UtilityCatalogue`, registry tag `kind: det_utility`) to its
canonical text after the same checks, so a file that would be refused at load fails the cook.

## Schema

```
DetUtilityFile {
    format_version: u32,              // 1
    name: String = "",                // free text
    inputs: [InputDef],               // 0 to 256, index order
    options: [OptionFile],            // 0 to 64, index order (the tie order)
}
InputDef {
    name: String,                     // unique among inputs, 1 to 64 bytes
}
OptionFile {
    name: String,                     // unique among options, 1 to 64 bytes
    min_score: f32 = 0.0,             // finite; carried, not applied by the scorer
    considerations: [ConsiderationFile],  // 0 to 32, scored in order
}
ConsiderationFile {
    input: String,                    // the name of an entry of `inputs`
    curve: CurveFile,
    weight: f32 = 1.0,                // finite; ignored for a multiplier
    multiplier: bool = false,         // product rule: multiply in instead of adding
}
CurveFile {
    kind: Linear | Quadratic | InverseQuadratic | Logistic | InverseLogistic | Plateau | Band,
    m: f32 = 0.0,                     // slope (Linear), sharpness (Logistic, InverseLogistic)
    k: f32 = 0.0,                     // knee: logistic center, plateau start
    b: f32 = 0.0,                     // band center
    c: f32 = 0.0,                     // band half-width (at least 1e-4 when evaluated)
}
```

Unknown fields are refused (`det_utility_parse`), so a typo never silently drops a consideration.

## Order is sim state

An option's index is its position in `options`; an input's index is its position in `inputs`. A unit's
inputs are a `[f32]` slice in that order. Scores are pushed in option order, and `ArgMaxIdx` keeps the
last of equal best scores, so moving an option changes which option wins a tie. Reordering options or
inputs is a sim-version change even when no curve changes.

## Score

`score = clamp(Σ wᵢ·curveᵢ(xᵢ) / Σ |wᵢ| · Π multiplier curves, 0, 1)` over the additive and multiplier
considerations, every curve clamping its input and output to 0..1. The float operations and their order
are those of the closed-enum scorer (`regia_detsim::ai::score`), so the DO27 preset
(`UtilityCatalogue::do27_preset`) gives the same bits.

## Errors

| Code | Cause |
|---|---|
| `det_utility_parse` | not RON, a missing or unknown field, a wrong type |
| `det_utility_newer_version` | `format_version` above 1 |
| `det_utility_unsupported_version` | `format_version` 0 or missing |
| `det_utility_unknown_input` | a consideration names an input `inputs` does not list |
| `det_utility_duplicate_name` | two options or two inputs share a name |
| `det_utility_bad_name` | an empty name, or one over 64 bytes |
| `det_utility_non_finite` | NaN or infinity in `min_score`, a weight or a curve parameter |
| `det_utility_over_cap` | over 64 options, 256 inputs, 32 considerations per option, or 1 MiB of text |

## Curve mapping to `.utility.ron`

The ECS utility asset (`utility-ron.md`, `regia_ai::utility::ResponseCurve`: `kind`, `slope`, `exponent`,
`x_shift`, `y_shift`, `points`, evaluated as `y_shift + family(x - x_shift)`) has its own curve set. The
deterministic set maps onto it as follows, so an editor can show both. The shapes are equal; the bits are
not (the ECS curves run `f64` kernels, the deterministic ones `regia_detsim::math` in `f32`), so a
catalogue moved between the two is a new sim, never the same one.

| `.detutility.ron` | `.utility.ron` equivalent | Exact shape |
|---|---|---|
| `Linear` (`m`) | `Linear`, `slope: m` | yes |
| `Quadratic` | `Quadratic`, `slope: 1`, `exponent: 2` | yes |
| `InverseQuadratic` | `Quadratic`, `slope: -1`, `exponent: 2`, `x_shift: 1`, `y_shift: 1` | yes |
| `Logistic` (`m`, `k`) | `Logistic`, `slope: 1`, `exponent: m`, `x_shift: k` | yes |
| `InverseLogistic` (`m`, `k`) | `Logistic`, `slope: -1`, `exponent: m`, `x_shift: k`, `y_shift: 1` | yes |
| `Plateau` (`k > 0`) | `Piecewise`, `points: [(0, 0), (k, 1)]` | yes |
| `Plateau` (`k <= 0`) | `Linear`, `slope: 0`, `y_shift: 1` | yes |
| `Band` (`b`, `c`) | `Piecewise` sampled from the bell | no: the ECS set has no Gaussian |

The ECS `Logit` and `Step` kinds and arbitrary `Piecewise` curves have no deterministic form. The
deterministic set is closed so every peer evaluates the same bits.
