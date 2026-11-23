# Scene: finding, inspecting and editing entities

Find entities with `scene.query_entities`, read them with `scene.get_entity`, edit them with
`scene.set_property` (and friends). Several edits go through `scene.apply_edits` as one
transaction. Schemas of any component come from `types.describe`.

## Finding entities

```json
{ "name": "Rock*", "component": "Transform", "limit": 50 }
{ "component": "Transform", "near":   { "point": [0, 0, 0], "radius": 2.0 } }
{ "component": "Transform", "box":    { "min": [9, -1, -1], "max": [11, 1, 1] } }
```

`name` is a glob (`*` = any run of characters), `component` matches a short or full type name.
Spatial filters need `Transform`. Results list each entity's component short names and, when it has a
`Transform`, the translation. Take ids from here — they are stable for the entity's lifetime.

`kind` filters by [entity kind](entity-kinds.md): a kind name, `"none"` (no kind) or `"unclassified"` (no kind and two
role families). Every row names its `kind` (or null), and `scene.get_entity` adds `kind`, `role` and `unclassified`.

## Reading and writing values

`scene.get_entity` returns every component's reflected value:

```json
{ "entity": 4294967296, "components": ["Transform"] }
→ { "components": { "Transform": { "type": "regia_scene::transform::Transform",
    "value": { "translation": [1.0, 2.0, 3.0], "rotation": [0, 0, 0, 1], "scale": [1, 1, 1] } } } }
```

Maths types serialize as arrays; other structs serialize as objects keyed by field name. Write with
`scene.set_property`, using the same shapes:

```json
{ "entity": 4294967296, "component": "Transform", "path": "translation", "value": [1.0, 5.0, -2.0] }
{ "entity": 4294967296, "component": "Transform", "path": "translation.y", "value": 5.0, "dry_run": true }
```

`path` is a dot path of **reflection field names** (it works even when the value serializes as an
array). `dry_run: true` returns `before`/`after` without applying. The response of a real edit
carries `before`, `after` and `applied: true`, and the change is stamped into `agent.changes`.

A component's field names, docs, ranges and units: `types.describe` with `{"type": "Transform"}`.

## Structural edits

```json
scene.spawn        { "name": "Agent Rock", "parent": 17, "components": { "Transform": {...} } }
scene.despawn      { "entity": 17 }
scene.duplicate    { "entity": 17, "parent": 3 }
scene.reparent     { "entity": 17, "parent": 3 }          // null detaches
scene.add_component    { "entity": 17, "component": "Marker", "value": {...} }   // value optional
scene.remove_component { "entity": 17, "component": "Marker" }
```

Only **reflected** components can be created, read or edited; `types.list` with `component: true`
lists them. `scene.despawn` is recursive: children still attached through `ChildOf` go with the
parent, detached entities survive. `scene.duplicate` copies reflected components (not the
relationship components themselves).

## Graybox shapes

`scene.spawn_primitive` spawns a parametric shape — a `Primitive` component the renderer meshes —
with a `Transform`, in one call:

```json
scene.spawn_primitive { "shape": "ramp", "params": { "size": [6.0, 1.5, 3.0] },
                        "transform": { "translation": [0.0, 0.75, -8.0], "euler_deg": [0.0, 90.0, 0.0] },
                        "name": "Loading ramp", "color": [0.6, 0.6, 0.65] }
→ { "entity": 4294967310, "shape": "ramp", "vertices": 18, "triangles": 8 }
```

`shape` is one of `cuboid`, `sphere`, `capsule`, `cylinder`, `cone`, `conical_frustum`, `plane`,
`torus`, `tetrahedron`, `ramp`, `prism`. Every shape is centered on the entity origin and about one
meter by default. `params` overrides fields of the shape by name with the JSON shapes
`scene.get_entity` shows (`types.describe` with `{"type": "PrimitiveShape"}` lists them):
`size` (`[x, y, z]`; plane `[x, z]`), `radius`, `height`, `length` (capsule middle), `resolution`,
`segments`, `caps`, `sectors`/`stacks` (sphere), `longitudes`/`latitudes` (capsule),
`minor_radius`/`major_radius` (torus), `vertices` (tetrahedron corners), `points` + `depth`
(prism outline in XY, extruded along Z). Sizes must be positive and at most 10 km, resolutions at
most 512; a bad value fails with `invalid_params` naming the field, and nothing is spawned.
`transform` takes `translation`, `rotation` (quaternion `[x, y, z, w]`) **or** `euler_deg`
(`[pitch, yaw, roll]`), and `scale` (a number or `[x, y, z]`).

To change a shape later, edit it like any component — `scene.set_property` with
`{ "component": "Primitive", "path": "shape", "value": {...} }` or `"path": "color"` — and the mesh
rebuilds on the next frame. The editor's **Create ▸ Shape** menu does the same as this method.

## Transactions

`scene.apply_edits` validates every edit first; if any of them does not validate, nothing applies:

```json
{ "edits": [
  { "op": "add_component", "entity": 17, "component": "Marker", "value": { "value": 9.0 } },
  { "op": "set_property", "entity": 17, "component": "Marker", "path": "value", "value": 4.0 },
  { "op": "spawn", "name": "Added" }
], "dry_run": false }
```

Ops: `set_property`, `add_component`, `remove_component`, `spawn`, `despawn`, `reparent`. A component
added by an earlier edit of the same transaction satisfies `set_property` validation. An edit whose
entity, or whose `reparent` or `spawn` parent, an earlier edit in the same list despawns fails
validation (`F0014`) and nothing is applied. There is no runtime undo: `dry_run: true` validates
and reports without applying. If an edit fails while it is being applied after every edit
validated, earlier edits of that call stay (`applied_all: false`, `failed_at`).

## Whole-scene snapshot

`scene.serialize` returns the whole scene as canonical RON (also served as the MCP resource
`regia://scene/current`) — useful for diffing what a session of edits changed.
Opaque resource storage is excluded (Bevy 0.20 stores resources on entities). Reflected
resource values, such as the level's layer binding, remain included so their changes stay
observable; `resources.list` also lists resources directly. Actual scene
entities with no serializable components remain included and counted, so an empty-entity
leak stays visible. Opening and closing an editor panel must not add empty scene rows (ADR-7956).

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
scene.serialize {}                  # { ron, entities, components }
```
