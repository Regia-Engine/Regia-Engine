# .destructionfx.ron — destruction effects table v1

Library: `regia_framework::destruction_fx` (`DestructionFxTable::{from_ron, to_ron, validate}`,
`migrate_destruction_fx`). Runtime: `DestructionFxPlugin` (planning in every build, playback with the
`destruction-fx` feature). Guide page: `book/agents/destruction-fx.md`. Decision: ADR-2734.

A table maps the `fx_tag` of each destruction moment (`regia_physics::destruction::DestructionFx`:
`BondBreak`, `ChunkDetach`, `Collapse`, with a position, a magnitude and the break event's seed) to a
sound cue, a VFX system and a decal, with caps per tag and per step.

```ron
(
    format_version: 1,
    name: "concrete_site",
    max_per_step: 24,
    fallback: Some("default"),
    rows: [
        (
            tag: "concrete",
            kinds: [BondBreak, ChunkDetach, Collapse],
            sound: Some("concrete_crack"),
            vfx: Some("dust_puff"),
            decal: Some((half_size: (0.35, 0.35, 0.25), color: (0.1, 0.09, 0.08, 0.8), lifetime_secs: 30.0)),
            max_per_step: 8,
            min_magnitude: 5.0,
            scale_jitter: 0.25,
        ),
        (tag: "default", kinds: [Collapse], sound: Some("rumble")),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 1; others fail with `destruction_fx_version`. |
| `name` | Display name. |
| `max_per_step` | Most plays per fixed step over every tag (default 32; 0: no cap). |
| `fallback` | Optional tag whose row plays for tags with no row; must name a row. |
| `rows[].tag` | The `fx_tag`; not empty, each tag once. |
| `rows[].kinds` | Moments that play (default empty: all three). |
| `rows[].sound` | Optional sound cue name (`regia_audio` cue registered with the app's `Audio`), played with the event's seed. |
| `rows[].vfx` | Optional VFX system name in the app's `VfxScene` library. |
| `rows[].decal` | Optional decal: `half_size` (each > 0), linear `color`, `lifetime_secs` (> 0). |
| `rows[].max_per_step` | Most plays of this tag per step (default 8; 0: only the table cap). |
| `rows[].min_magnitude` | Moments weaker than this play nothing (the bond's break impulse in N s, or the chunk's mass in kg; >= 0). |
| `rows[].scale_jitter` | Seeded decal size variation, 0 to 1 (`1 ± jitter`). |

Rules checked by `validate`: at most 1024 rows; tags unique and not empty; `fallback` names a row;
numbers finite and in range. Errors: `destruction_fx_parse`, `destruction_fx_version`,
`destruction_fx_invalid`.

## Versioning

v1 is the first shape; `destruction_fx_migration_sources()` is empty. A later shape change bumps
`format_version` and adds a migrator. `cargo xtask check-formats` guards the shape (registry name
`destructionfx`); `regia upgrade-project` reports the files' versions; fuzz target `destructionfx`.
