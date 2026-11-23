# Asset provenance and repair

Every source asset's `.meta` may carry a provenance block: who made it (`tool`: `hand`, `agent`,
`generator`, `import`), where it came from (`origin`), the generator and seed, the agent client,
session and model, a timestamp, the review state (`reviewer`: `pending`, `owner` or a name) and the
project's own `extensions` fields (spec 12 § 1a, spec 23 § 5). This page covers the project policy over
those blocks, how they show in the editor, and how the asset database keeps ids when files move or
sub-assets are renamed (ADR-2428).

## The policy: `config/provenance.ron`

A project may add `config/provenance.ron` (format: [`docs/formats/provenance-ron.md`](../../docs/formats/provenance-ron.md)):

```ron
(
    format_version: 1,
    fields: [
        (name: "licence"),
        (name: "foreign_game_file", values: ["false"]),
        (name: "third_party_mod_file", values: ["true", "false"]),
    ],
    kinds: { "*": (required: ["origin", "tool"]), "png": (required: ["licence"]) },
    tools: [hand, agent, generator, import],
    origins: ["dcc:*", "https://polyhaven.com/**"],
    licences: ["CC0-1.0", "owner"],
    review: (required_for: [agent, generator], accepted: [owner, named]),
    exclude: ["assets/dev/**"],
)
```

Each source with a `.meta` is checked; every problem is a finding with a rule:

| Rule | Severity | When |
|---|---|---|
| `missing_field` | error | A field the asset's kind requires (the `.meta` importer id; `*` is every kind) is absent: an unset or empty string, `tool: unknown`, `reviewer: pending` for `reviewer`, or an extension key without a value. |
| `tool_not_allowed` | error | `tools` is not empty and the asset's tool is not in it. |
| `origin_not_allowed` | error | `origins` is not empty and the asset's origin matches none of the globs. |
| `licence_not_allowed` | error | `licences` is not empty and the licence field (`licence_field`, default `licence`) holds another value. |
| `value_not_allowed` | error | A declared field with a `values` list holds another value. |
| `unreviewed` | error | The tool is in `review.required_for` and the review state is not in `review.accepted` (default `[owner, named]`). |
| `undeclared_field` | warning | With `warn_undeclared: true`, an `extensions` key the policy does not declare. |
| `unreadable_meta` | error | The `.meta` does not parse. |

Where the policy applies:

- `regia assets validate` adds the `provenance_policy` validator (one row per source; sub-assets are
  `not_validated`, their source carries the block). Findings print as `error provenance_policy <asset>:
  <rule>: <text>`.
- `regia cook --quality final` and `regia package` refuse while any source has an error finding. The
  message starts with `provenance_policy:` and names the first 20 findings. A cook that creates a new
  source's `.meta` checks it at the next final cook or package.
- An invalid policy file stops all three with `invalid_config` (`regia assets validate` exits 2).
- A project **without** `config/provenance.ron` has no policy: nothing above runs, and cooks behave as
  they did before. The older `[provenance] require_review_for_shipping` gate in `Regia.toml` (see
  [Staged review](staged-review.md)) still applies on its own.

## The editor

The Content Browser's list rows end with a badge: `agent · review pending`, `gen · ✓ ana`,
`hand · ✓ owner`, and `⚠ policy N` in front when the policy has N errors for the asset. The tooltip lists
the origin, generator, client, model, timestamp, extension fields and each finding. Grid tiles carry a
corner mark (`⚠` for a policy error, `•` for a pending review) and the same text in their tooltip. The
badges are read from the `.meta` files once per content-index generation and only for the rows on screen.

## Moved sources: orphaned `.meta` files

Every cook (and `editor.asset_repair_metas`) first looks for orphaned `.meta` files under `assets/`, sidecars
whose source is gone, and re-pairs them with sources that have no `.meta`:

1. **Content hash**: the asset database recorded each source's BLAKE3 when it was cooked; the one
   meta-less source with the same file extension and those bytes gets the `.meta` (renamed and moved files
   both work, and the `.meta` may have moved somewhere else).
2. **File name**: only when the database has no row for the id (a fresh clone or a deleted
   `.regia/assetdb`): the one meta-less source with the orphan's file name, when no other orphan has it.

The `.meta` bytes are written next to the source and the orphan removed, so the GUID, and every reference
to it, survive. Several candidates leave the orphan in place (`ambiguous`, with the candidates); a source
that already has a `.meta` is never touched. The cook prints `asset_meta_repaired <old meta> -> <source>`
and `asset_meta_orphaned <meta> (id ...): no_match | ambiguous | unreadable_meta` warnings.

## Renamed sub-assets

A glTF (or FBX) sub-asset keeps the id its `.meta` stores for its path. When a node, material, clip,
skeleton, LOD group or material variant set is renamed between imports, the importer matches the vanished
entries with the new ones:

1. Only entries of one group match: same kind (the first path segment), same tail (a mesh's
   `primitive_<i>`) and same cooked type. Index-named and derived sub-assets (`texture/<i>`, `model`,
   `prefab/sockets`, collision sets, meshlets, hulls, fracture chunks) never match.
2. **Content**: a vanished and a new entry whose cooked payloads hash the same, when no other entry of
   the group shares the hash.
3. **Sole rename**: a group left with exactly one vanished and one new entry.

A match keeps the old id (`asset_subasset_remapped <file>#<new> keeps the id <id> of #<old> (rule
content | sole_rename)`): the `.meta`, the manifest and the model's part list all name it, so levels and
prefabs that reference it still load. To re-map by hand when the rules cannot decide (two materials
renamed at once with identical content), edit the entry's `path` in the `.meta` to the new name and keep
its `id`; the next import takes it as stored. A vanished sub-asset no rule matches warns
`asset_subasset_missing <file>#<path> (id <id>) is gone after re-import; referenced by: <files>`, listing
up to 50 RON, JSON, TOML, CSV, Lua and text files under `assets/` and `config/` that still name its id or
`file#path`.

## Methods

Editor methods (the policy and the asset database are desktop-only pipeline code, so a game build has no
such methods); each works on the project open in the editor and answers `no_project` without one.

### `editor.asset_provenance`

`{ asset?, limit? }`. With `asset` (a source path, `assets/` optional): `{ asset, meta, id, importer,
provenance, tool, review, reviewer, policy, needs_review, findings: [{ source, rule, severity, text }] }`
(`meta: false` when the source has no `.meta` yet). Without it, the project summary: `{ policy, assets,
unreadable, by_tool, by_review, errors, warnings, findings, truncated }`, at most `limit` findings
(1 to 1000, default 200).

```json
{ "method": "editor.asset_provenance", "params": { "asset": "assets/props/crate.glb" } }
```

### `editor.asset_repair_metas`

`{ dry_run? }` (permission `assets`, mutating). Runs the re-pair above and saves the asset database:
`{ applied, repaired: [{ from_meta, to_source, id, rule }], orphans: [{ meta, id, reason, candidates }],
warnings }`. `dry_run: true` reports without writing.

### `editor.asset_subasset_changes`

`{ source?, code?, limit? }`. The events of recent cooks from `.regia/assetdb/subasset_changes.json`
(newest last, at most 500 kept): `{ count, changes: [{ code, source, path, old_path?, id, rule?,
referrers? }] }`. `code` filters to `asset_subasset_missing` or `asset_subasset_remapped`.

## Errors

Every error carries `data.code`, `data.reason` and `data.hint`.

| Reason | Code | Methods | Meaning |
|---|---|---|---|
| `asset_not_found` | `F0003` | `editor.asset_provenance` | No source file at `asset`. |
| `meta_unreadable` | `F0002` | `editor.asset_provenance` | The source's `.meta` does not parse. |
| `provenance_policy_invalid` | `F0002` | `editor.asset_provenance` | `config/provenance.ron` does not parse or names an undeclared field. |
| `path_outside_project` | `F0002` | `editor.asset_provenance` | `asset` is absolute, has `..` or leaves the project. |
| `bad_limit` | `F0002` | `editor.asset_provenance`, `editor.asset_subasset_changes` | `limit` is not a whole number from 1 to 1000. |
| `bad_dry_run` | `F0002` | `editor.asset_repair_metas` | `dry_run` is not a boolean. |
| `no_project` | `F0003` | all three | No project is open in the editor. |
| `asset_db_unreadable` | `F0012` | `editor.asset_repair_metas` | The asset database directory cannot be created or read. |
| `asset_db_unwritable` | `F0012` | `editor.asset_repair_metas` | The re-paired database cannot be saved. |
| `bad_code` | `F0002` | `editor.asset_subasset_changes` | `code` is neither change code. |

The cook and validate codes: `provenance_policy` (refusal and validator), `asset_meta_repaired`,
`asset_meta_orphaned`, `asset_subasset_remapped`, `asset_subasset_missing`.
