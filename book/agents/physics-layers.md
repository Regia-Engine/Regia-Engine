# Physics layer matrix (editor)

ADR-7233. Code: `regia_editor::physics_matrix`; the file is `config/physics.ron` (`regia_physics::LayerFile`,
spec 15 § 3).

The project's collision layers are edited in **Edit ▸ Project settings… ▸ Physics layers (config/physics.ron)** and by
two editor agent methods. Up to 32 named layers; a collider picks one by index (`Collider::layer`,
`CollisionLayer::new(index)`). The **collision matrix** is symmetric (the section shows its lower triangle); the
**query matrix** says which layers a query belonging to a layer (`QueryFilter::from_layer`) sees.

Every change rewrites the file at once in canonical RON (comments are lost; the file is created when missing) and is
one undo step (`editor.undo`). Pairs are written canonically: a collision pair with the lower layer index first, then
sorted; query pairs sorted by (query layer, seen layer). A game loads the file the editor wrote unchanged:

<!-- ignore-reason: a fragment of an App setup function (it needs `app` and a `?`-returning caller); LayerFile::load and PhysicsLayers::from_file are covered by regia_physics' layer tests -->
```rust,ignore
let file = regia_physics::LayerFile::load("config/physics.ron".as_ref())?;
app.insert_resource(regia_physics::PhysicsLayers::from_file(&file)?);
```

Colliders store a layer *index*, so a layer can be renamed freely, but only the **last** layer can be removed
(removing a middle one would move the colliders of every later layer onto another row). The section's only remove
button is the last layer's; `remove_layer` of any other layer is refused with `layer_not_last` and writes nothing. To
retire a middle layer, rename it (for example to `unused`) and switch its pairs off: the indices stay put. There is no
index remap with a content migrator (ADR-7267).

This is the project's one physics-layers editor (ADR-7267): `editor.project_settings_get` `pages.physics` is the
`editor.physics_layers_get` result.

## `editor.physics_layers_get`

```json
{}
→ { "path": "config/physics.ron", "exists": true, "max_layers": 32,
    "layers": ["world", "props"],
    "collides": [["world", "world"], ["world", "props"]],
    "queries": [["world", "world"], ["props", "world"]] }
```

Without the file it returns the engine default (one self-colliding `default` layer) with `exists: false`.

## `editor.physics_layers_edit`

```json
{ "edits": [
    { "op": "rename_layer", "from": "default", "to": "world" },
    { "op": "add_layer", "name": "props" },
    { "op": "set_collides", "a": "props", "b": "world", "on": true },
    { "op": "set_query", "from": "props", "seen": "world", "on": true } ] }
→ the editor.physics_layers_get result after the edits
```

Ops: `add_layer { name }`, `remove_layer { name }` (last layer only; its pairs go too), `rename_layer { from, to }`
(pairs follow), `set_collides { a, b, on }` (symmetric), `set_query { from, seen, on }`. The batch applies in order,
all or nothing, and is validated the way the runtime builds the table; a refused batch writes nothing. An empty
`edits` array writes nothing and adds no undo step.

Error codes (in `error.data.code`):

| Code | When |
|---|---|
| `bad_physics_edit` | `edits` is not an array, an unknown `op`, a missing or mistyped field (`on` must be a boolean) |
| `bad_layer_name` | a name that is empty, longer than 64, has leading or trailing spaces, or a character other than letters, digits, `_`, `-`, `.` and inner spaces |
| `duplicate_layer` | adding or renaming to a name that exists |
| `unknown_layer` | a pair or op names a layer that does not exist |
| `layer_not_last` | removing a layer that is not the last |
| `too_many_layers` | a 33rd layer |
| `physics_config_invalid` | the existing file cannot be read, parsed or built (it is never overwritten) |
| `physics_config_write` | the file could not be written |
| `no_project` | no project is open |

Verify a change in a running game the way the runtime sees it: load the file as above and drop a body on one layer
onto a collider on the other (`crates/regia_editor/tests/physics_matrix.rs` does exactly this), or ask
`physics.raycast` from a layer.
