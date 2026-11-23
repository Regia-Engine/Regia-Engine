# .physmat.ron: physics material v1

Library: `regia_physics::material` (`PhysicsMaterial::{new, from_ron, to_ron, validate, apply_to}`, `PhysicsMaterials`,
`PhysicsMaterialRef`, `combine`). Check without registering: agent method `physics.material_check`; register:
`physics.material_register`. Guide page: `book/agents/physics-spec15.md`. Decision: ADR-2424.

A `.physmat.ron` is a named bundle of the surface fields a `Collider` carries inline. Every field except
`format_version` and `name` is optional and defaults to what `Collider::default()` holds (friction 0.5, no bounce,
density 1, both combine rules Average), so the default material changes nothing.

```ron
(
    format_version: 1,
    name: "wet rubber",
    friction: 1.1,
    restitution: 0.35,
    density: 1.4,
    friction_combine: Max,
    restitution_combine: Average,
)
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. Another value fails with `physmat_version`; no older files exist. |
| `name` | Registry name colliders reference; not empty (`physmat_name`). |
| `friction` | `0..=2`; 0 is ice. Out of range or not finite: `physmat_value`. |
| `restitution` | `0..=1`. |
| `density` | kg/m³ for automatic mass, `0..=100`; 0 makes a massless collider. |
| `friction_combine`, `restitution_combine` | `Average`, `Min`, `Multiply` or `Max`: how the value combines with the other collider's. When the two colliders name different rules the higher priority one applies (Average < Min < Multiply < Max), which is rapier's behavior; `regia_physics::material::combine` computes it. |

Use: put `PhysicsMaterialRef("wet rubber")` next to a `Collider`. In `PreUpdate`, before the fixed step, the named
material's values are copied into the collider (shape, layer and events stay). A reference that names no
registered material is left alone and counted in `PhysicsMaterials::unresolved`. A collider without a reference is
never touched, so scenes that do not use materials are bit-identical.

Migration: none at v1 (`physmat_migration_sources()` is empty). A shape change needs a version bump, a migrator and
`regia upgrade-project` support (spec 12 § 4a). The format is registered in `cargo xtask check-formats` and
`regia upgrade-project` as `physmat`.
