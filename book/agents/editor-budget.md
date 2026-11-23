# Budget meters (`editor.budget`)

The Kit panel and the prefab-mode banner show **budget meters**: what a model, or every mesh of
the prefab being edited, measures against a model class's limits. They are the same limits as
`editor.model_report` (prop, character, vehicle, building; ADR-234 defaults) and the same limit
names for overrides. Measuring reads the glTF's JSON and the image headers only: no buffer is read
and no texture is decoded, so it is cheap enough to run on every kit open.

In the **Kit panel** the line `Budget <class> · <tris>/<limit> tris · <materials>/<limit>
materials · <MiB>/<limit> MiB` is colored by the worst meter, with a class picker beside it and
the meters under **Budget meters (n over)** (open by default when something is over). The class
defaults from `kit.json`: a kit whose skeleton root has the `{"tincal_kit": 1}` tag or whose kind
is `pilot` is a character, `hull` a vehicle, `structure` a building, anything else a prop.

In **prefab mode** the same line sits in the banner; hover it for the meters. It is measured again
when the prefab, the class, the entity count or the undo history changes, and on the ↻ button.

## `editor.budget`

Read-only. Params:

| Param | Type | Meaning |
|---|---|---|
| `source` | string | `scene` (default): every `MeshSource` of the edit world, which is the open prefab in prefab mode. `kit`: the Kit panel's model. |
| `class` | string | `prop`, `character`, `vehicle`, `building` or `hero`. Default: from `kit.json` for a kit; the banner's class (prop until changed) for a scene. |
| `limits` | object | Overrides over the class limits: `max_triangles`, `max_node_triangles`, `max_nodes`, `max_materials`, `max_materials_per_node`, `max_joints_per_skin`, `max_texture_dimension`, `max_texture_bytes`. 0 turns a check off. |

```json
{ "method": "editor.budget", "params": { "source": "kit", "class": "vehicle", "limits": { "max_triangles": 80000 } } }
```

Returns `{ source, prefab, budget, reports_read }` (`prefab` and `reports_read` for a scene only;
`reports_read` counts the model files read since the editor started, a cache hit reads none):

- `budget.class`, `budget.limits`, `budget.status` (the worst: `ok`, `near`, `over`), `budget.over`
  (rows over plus LOD levels that do not reduce).
- `budget.rows[]`, always in this order: `triangles`, `node_triangles`, `nodes`, `materials`,
  `materials_per_node`, `joints_per_skin`, `texture_side`, `texture_bytes`. Each is
  `{ key, label, subject, value, limit, fraction, status }`. `subject` names the peak: the node,
  skin or `material/slot` (a scene prefixes the model path, `assets/tree.glb: trunk`). `status` is
  `off` (limit 0), `ok`, `near` (at least 80 % of the limit) or `over`.
- `budget.lods[]` (kits): `{ level, authored, triangles, ratio, reduces }`. `ratio` is the share of
  LOD0's triangles; `reduces` is false when a level has no fewer triangles than the level above.
- `budget.models`, `budget.instances`, `budget.errors[]` (model files that could not be measured;
  any error makes the status `over`).

`editor.open_kit` returns the same `budget` object (the kit's class, default limits) beside its
other fields.

What counts:

- A kit counts LOD0: triangles of `_LOD<n>` nodes with n > 0 are left out of `triangles` (they are
  in `lods[]`). The cockpit model is not part of the hull's budget.
- A scene counts each `MeshSource` instance: triangles and nodes once per instance, texture memory
  once per model file, materials once per distinct material. A material override (`MeshSource.material`)
  replaces the model's own materials. Built-in primitives are left out.
- Texture memory is the report's estimate (width × height × 4 bytes × 4/3 for mips, once per
  distinct image). An image the header pass cannot size (a data URI, a path leaving the model's
  folder) counts as 0 × 0.

## Error codes (`data.budget_error`)

- `budget_bad_source`: `source` is not `scene` or `kit`.
- `budget_bad_class`: `class` is not one of the five.
- `budget_bad_limits`: an unknown limit name or a value of the wrong type.
- `budget_no_kit`: `source: "kit"` with no kit open.
- `budget_not_measured`: the kit's model could not be measured (the kit's warnings say why).

## How to check a result

Open the kit or the prefab, then read `budget.status` and the `over` rows. A failing LOD chain
shows as a `lods[]` entry with `reduces: false`. The panel and the banner draw exactly these
rows, so the JSON is what a user sees.
