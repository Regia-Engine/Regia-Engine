# `config/validation.ron` (asset validation rules, v1)

Spec: `docs/specs/32-asset-validation-and-audit.md` § 4.4. Proposed in ADR-1668; read by `regia assets validate`,
the `cook` gate and `editor.asset_validate { rules }`. Frozen from its first release: a later shape change needs
version 2, a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a). No earlier version exists.
Registered in `cargo xtask check-formats` (`validation`, the `Rule` variants with their fields) since ADR-2316.

```ron
(
    format_version: 1,                // must be 1; a newer version is refused
    validate_on_save: true,           // default true
    cook: Warn,                       // Off | Warn | Fail; default Warn
    exclude: ["assets/dev/**"],       // path globs left out of gathered scopes
    roots: ["assets/levels/**"],      // reachability roots for the orphan check (default: scene assets)
    limits: (                         // the built-in limits; every key optional
        max_texture_dimension: 4096,
        texture_power_of_two: false,
        mesh_triangle_budget: 200000,
        max_path_len: 200,
    ),
    rules: [
        PathStyle(id: "lowercase", glob: "assets/**", style: SnakeCase, severity: Error),
        SizeBudget(id: "texture-cap", kind: Texture, max_bytes: 16777216, severity: Warning),
        SizeBudget(id: "level-closure", glob: "assets/levels/*", max_closure_bytes: 1073741824,
                   max_closure_assets: 5000, severity: Error),
        Forbid(id: "ui-not-dev", from: "assets/ui/**", to: "assets/dev/**", severity: Error),
        MaxDependencies(id: "prefab-fanout", glob: "assets/prefabs/**", max: 64, severity: Warning),
        NoCycles(id: "no-cycles", glob: "assets/**"),
    ],
)
```

- Every top-level key is optional. Unknown fields or variants are errors (`deny_unknown_fields`), reported as
  `invalid_config` with the line and column; rule ids must be unique and non-empty.
- Optional rule fields are written without `Some(..)`: `style: SnakeCase`, `max_bytes: 100`.
- `severity` is `Info`, `Warning` or `Error` (default `Error`). A `Warning` does not fail an asset unless the run
  sets `warnings_fail` (or the CLI runs `--strict`).
- Rules: `PathStyle { id, glob, style?, target?, prefix?, suffix?, allowed_chars?, max_len?, severity }` (`style`:
  `SnakeCase`, `KebabCase`, `LowerAscii`; `target`: `Stem` (default) or `Path`); `SizeBudget { id, glob?, kind?,
  max_bytes?, max_closure_bytes?, max_closure_assets?, severity }` (at least one limit); `Forbid { id, from, to,
  severity }`; `MaxDependencies { id, glob, max, severity }`; `NoCycles { id, glob, severity }`.
- Globs: `*` any run except `/`, `**` any run including `/`, `?` one character except `/`.
