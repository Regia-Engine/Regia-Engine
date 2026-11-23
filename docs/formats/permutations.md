# `permutations.json` — cooked shader-permutation list (format version 1)

Written by every cook to `.regia/cooked/<platform>/permutations.json` and shipped by `regia package` as
`content/permutations.json` (ADR-2026, master plan § 5.5a).
Rust type: `regia_render::PermutationManifest`; the full shape and an example are on the agent page
[`book/agents/permutation-policy.md`](../../book/agents/permutation-policy.md).

- `format_version` (1), `platform`, `graphs` (distinct graph codes), `profiles[]`.
- Each profile: `profile` (preset name), `target { format, samples, instanced }`, `entries[] { key (16 hex
  digits), name, double_sided, passes[] }`, `pipelines`, `modules`, `code_bytes`, `estimated_compile_ms`,
  `budget { max_pipelines, max_compile_ms }`, `over_budget`.

**Derived data** (a cook output, not an authored source). Its shape is registered in `cargo xtask check-formats`
(`permutations`) and `regia upgrade-project` since ADR-2316, so a field rename fails the gate. Nothing
authors it, a cook writes it when the list changes, and `regia package` enumerates it from the asset
database when a cache has none. A runtime that cannot read it (older package, damaged file, newer version)
runs without the check.

Migration notes:

- v1 (v0.1.28): first version. A reader refuses a `format_version` newer than its own
  (`PermutationManifestError::Version`); the fix is a recook with the reading engine. A future shape
  change bumps the version; older files are simply recooked, no migrator is needed.
