# RPGO: baked procedural output v4

Library: `regia_procgen::pgout` (`encode`, `encode_cells`, `decode`, `read_table`, `PgoutFile`, `migrate`,
`replace_cell`). Writers: `regia_procgen::world::bake` (agent `procgen.bake`, `regia procgen bake`). Spec 62 § 8,
spec 66 § 2 (cells). Decisions: ADR-7214 (v4, set layers), ADR-7750 (the prefab-override section, folded into v4 by
ADR-7724 (h)).

A `Baked` generator's output, next to the level as `<level>.procgen/<generator guid>.pgout` (a `PerTier` bake writes
`<guid>.<Tier>.pgout`). The cook stores the bytes unchanged in the pak as `regia_procgen::pgout::PgoutBytes`. Little
endian, f32/f64 IEEE; strings are `len: u32` + UTF-8.

| Bytes | Content |
|---|---|
| 4 | magic `RPGO` |
| 4 | version (u32, 4) |
| 32 + 32 | graph hash, parameter hash |
| 4 | seed (u32) |
| 32 + 32 | input hash, output hash (the whole output's `RunOutput::hash`, cells merged) |
| 4 | quality tier (u8) + 3 pad bytes |
| 4 | cell count (u32, at least 1) |
| 60 per cell | `i: i32, j: i32, tier: u8` + 3 pad, `offset: u64, len: u64`, the cell's output hash (32) |
| rest | the cell sections, in table order, tiling the rest of the file exactly |

An untiled bake has one cell `(0, 0)` tier 0. Each cell section holds, in order: instance sets, prefab records
(prefab, name, translation, rotation, scale, tags), tables, point sets, masks, generated meshes, terrain edit records,
road decals, the data layers of the instance sets that have one (`count: u32`, then `set_index: u32` + layer name),
and then the optional prefab-override section.

## The prefab-override section (ADR-7750)

Written only when at least one prefab record of the cell carries `spawn.prefab` overrides (spec 63 § 13.2):

| Bytes | Content |
|---|---|
| 4 | overridden record count (u32, at least 1) |
| per record | the record's index into the cell's prefab records (u32, strictly increasing, in range), override count (u32, at least 1), then per override: component (string), field (string), value |

The value is a one-element attribute column, the encoding the tables and point attributes of the same section use:
type name (string), `len: u64` (1), then the value (`bool` u8, `int` i64, `float` f64, vectors and quaternions as f64
components, `transform` as t3, r4, s3 f64, `name` and `asset` as strings). The record is `exec::PrefabFieldOverride`,
the same record a live run hands to the instance; a loaded bake applies it the same way
(`prefab_overrides::apply_prefab_overrides`), so a baked level spawns the same component values as a live run.

A section that ends after the set layers has no overrides: every version 4 file written before ADR-7750 is that layout
and reads unchanged. A section that has the override section is never written for an output without overrides, so the
encoding of a given output is unique. The reader refuses (`pgout_corrupt`) an empty override section, a record with no
override, an index out of order or out of range, and a value column whose length is not 1. The output hash covers the
overrides (an output without them hashes as before), so a hand edit inside the section fails the cell hash check.

## Versions and migration

| Version | Release | Layout |
|---|---|---|
| 1 | v0.2.6 | one untiled cell; table entry `i, j, offset, len` (no tier, no cell hash) |
| 2 | v0.2.7 | the current table; sections end after the masks |
| 3 | v0.2.8 to v0.5 | sections end after the road decals |
| 4 | v0.6 | set layers; optional prefab-override section (ADR-7750) |

`decode` reads versions 1 to 4. `migrate` (`regia upgrade-project`, row `pgout`) rewrites versions 1 to 3 as version 4
with the same header and hashes; their prefab records have no overrides, so the result has no override section. A
version 4 file, with or without the section, comes back unchanged, so running the upgrade twice changes nothing.

Refusal by older builds: a v0.5.x build reads versions 2 and 3 only and refuses every version 4 file with
`pgout_version` ("version 4; this build reads 3"). A v0.6 development build from before ADR-7750 reads version 4 but
refuses a file with an override section with `pgout_corrupt` ("cell section has trailing bytes"); re-bake with the
current build.

`cargo xtask check-formats` fingerprints a description of the layout (`pgout_schema_fingerprint()`, format `pgout`),
which names the optional section as `prefab_overrides?`. Corpus: `crates/regia_agent/tests/fixtures/format-corpus/pgout/`
holds `v1.pgout`, `v2.pgout`, `v3.bin`, `v4.bin` (no override section) and `v4-overrides.bin` (two prefab records, the
second with two overrides). The fuzz target `pgout` (`agent.fuzz`) decodes and re-encodes mutated copies of the
registry fixture, which carries the override section.
