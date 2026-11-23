# Plant wind attributes (`_WIND`, `_WIND_PIVOT`), layout 1

Written by the plant export (`regia procgen plant`, `regia_asset_pipeline::plant_export`, spec 68 § 7). The layout is
Regia Engine's own (ADR-1585). The engine does not read these attributes yet: the planned vertex-stage sway of the
scatter forest page will. Each LOD mesh of a plant export carries mesh `extras` `{"regia_wind_layout": 1}`, so a later
layout change can be detected and converted on import.

## Attributes

Both are glTF 2.0 application-specific vertex attributes (names start with `_`), `FLOAT`, one value per vertex, on every
primitive of every `<name>_LOD<n>` mesh.

| Attribute | Type | Component | Meaning |
|---|---|---|---|
| `_WIND` | `VEC4` | x | Branch level: 0 trunk, 1 to 3 branch orders (orders above 3 write 3), 3 for leaves |
| | | y | Phase 0 to 1: a hash of the plant seed and the branch's first skeleton node, so neighboring branches sway out of step |
| | | z | Stiffness 0 to 1: the branch's base radius over the trunk's base radius (1 = as stiff as the trunk) |
| | | w | Leaf flutter weight 0 to 1: 1 on leaf and card vertices, 0 on bark |
| `_WIND_PIVOT` | `VEC3` | xyz | Object-space position of the base of the vertex's branch (for a leaf: the base of the branch it sits on) |

Billboard vertices (the crossed quads on the last LOD) write `_WIND = (0, 0, 1, 0)` and the plant's base as the pivot.

## Rules

- Units are meters in the glTF frame (+Y up), the same space as `POSITION`.
- A branch is one skeleton axis: it starts at the root or where a lateral branch leaves its parent, and its tube starts
  inside the parent (at the parent's node), so the pivot of a lateral branch lies on the parent's axis.
- Values are deterministic: the same plant graph, parameters and seed give the same attribute bytes (spec 68 § 9).

## Compatibility

No exported tree from another tool was at hand when this page was written, so no comparison with other layouts is
recorded. An importer that meets a different layout (no `regia_wind_layout` extra, or another value) converts it or
drops the attributes; it never reads them as layout 1.
