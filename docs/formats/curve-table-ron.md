# `.curves.ron` and `.curves.csv` (curve table, format `curve_table` v1)

Named float curves in one file (spec `docs/specs/51-curve-tables-and-choosers.md` §§ 2.1, 4.1, 4.2, ADR-1649,
`regia_framework::curve_table`, agent guide `book/agents/curve-tables.md`).

## RON form

```ron
(
    format_version: 1,
    interp: Linear,                         // table default: Step | Linear | Cubic
    curves: {
        "falloff": (pre: Hold, post: Hold, keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
        "xp": (interp: Cubic, post: Linear, keys: [(1, 0), (2, 100), (x: 3, y: 250, out_tangent: 160.0)]),
    },
)
```

- `curves` keeps file order. Names are `[A-Za-z0-9_.-]{1,64}` and unique.
- A curve: `pre` and `post` (`Hold` default, `Linear`, `Cycle`, `CycleOffset`, `PingPong`), `interp` (optional,
  overrides the table's), `keys` (1 to 4,096, strictly increasing finite `x`).
- A key is `(x, y)`, or the named form `(x: .., y: .., interp: .., in_tangent: .., out_tangent: ..)` when it
  carries a per-key interpolation or explicit tangents. (The spec's sketch `(3, 250, out_tangent: 160.0)` mixes
  positional and named fields, which RON cannot express; ADR-1649.) Integers are accepted for numbers.
- Optional fields take the bare value (`interp: Cubic`, not `Some(Cubic)`).
- Unknown fields are refused (`curve_bad_file`). Comments are allowed.
- The canonical writer (`CurveTable::to_ron_string`, `curves.export`) writes every curve's `pre` and `post`, a key
  as `(x, y)` when it has nothing else set, floats in shortest round-trip form; writing a table read from that text
  gives the same bytes.

## CSV form (RFC 4180, the data-table reader)

```
name,0,10,30
falloff,1,0.6,0
ramp,0,,5
```

- The first header is `name`; every other header is an `x` (finite, strictly increasing left to right).
- One curve per record; cells are `y`; a blank cell means the curve has no key at that `x`.
- BOM, CRLF or LF, and quoted fields are accepted. Import takes the table interpolation (`step`, `linear`,
  `cubic`); CSV carries no extrapolation, per-key interpolation or tangents (they take their defaults).
- Export writes the sorted union of every curve's `x` as the axis, blanks where a curve has no key; reading it back
  gives the same keys.

## Versions

v1 is the first version (`CURVE_TABLE_FORMAT_VERSION`, `curve_table_migration_sources()` is empty,
`curve_table_schema_fingerprint()` for `cargo xtask check-formats`). A newer `format_version`, or 0, is refused
with `curve_unsupported_version`. A future v2 adds a step to the shared `regia_core::version` loop and
`regia upgrade-project` support (spec 12 § 4a).

## Cooked form

`regia cook` validates a table (a table with errors fails the cook; warnings do not) and stores it in the pak as a
cooked asset of type `regia_framework::curve_table::CurveTable`: a little-endian `u32` version
(`CURVE_TABLE_COOKED_VERSION`, 1), then postcard of the table interpolation and the curves in file order, from either
source form. `CurveTable::from_cooked` runs the same validation as text, so a damaged pak gets the error its text form
would, and bakes derived tangents the same way, so a cooked table samples bit-identically to its source. A version this
build does not read is `curve_unsupported_version`; damaged bytes are `curve_bad_file`.

The cooked form is a cache. A source file wins when one is readable (a dev tree always runs what is on disk), and
`regia package` ships these tables in the pak only. There is nothing to migrate: a pak of another cooked version is
cooked again. The registry tags the asset `kind=curve_table`.
