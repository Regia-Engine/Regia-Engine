# Save entity references

An `Entity` is an index into one running world. A component that stores one (an AI target, the
character carrying an item, the riders of a vehicle, an owner) used to lose it on a save, so such
components could not be saved. Since save format 2 (ADR-3239, spec 91 § 10) every saved entity has
a stable **save id** and the references between saved entities, and from saved entities to level
entities, survive a save and a load.

## In a game

Register `MapEntities` on every component that holds an `Entity`, as for scenes:

<!-- ignore-reason: a fragment of a game's app setup (`app` comes from the game's `main`) -->
```rust,ignore
use regia_ecs::ecs::entity::{EntityMapper, MapEntities};

#[derive(Component, Reflect, Clone)]
#[reflect(Component, MapEntities)]
struct AiTarget {
    target: Entity,
}

impl MapEntities for AiTarget {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        self.target = mapper.get_mapped(self.target);
    }
}

app.register_type::<AiTarget>();
// Mark the pawn and its target `Saveable`, then save and load as usual.
```

- **What is followed.** Every `Entity` reflection reaches inside the component: struct and tuple
  fields, `Option`, other enums, `Vec` and other lists, arrays and map values. `Entity::PLACEHOLDER`
  means "no entity" and stays as it is.
- **Targets.** A saved (`Saveable`) entity, or a level entity (one with an `EntityGuid`, found again
  by its guid and its `LevelMember` level after the level loads). Hierarchies work too: bevy's
  `ChildOf` is followed and `Children` is rebuilt from it.
- **A target that is not saved** (not `Saveable` and no `EntityGuid`, or already despawned): inside
  an `Option` the innermost `Option` is saved as `None`; a plain `Entity` field means the component
  is not saved, with the warning `save_entity_ref_unsaved`.
- **A target missing at load** (a level entity a content update removed): `Option` becomes `None`;
  a plain field skips the component with the warning `save_entity_ref_missing`. Nothing loads
  pointing at a dead or unrelated entity.
- **A type that holds an `Entity` without registering `MapEntities`**, or holds one as a map key or
  set element, is not saved (`save_entity_ref_unmapped`).
- **Order.** Level entities first, by level and guid, then runtime entities in spawn order: two saves
  of the same world are byte-identical apart from the timestamp.
- **Old saves.** Format 1 files load through a built-in converter: files without entity fields load
  unchanged; any entity field they held (format 1 never kept them) loads as `None` or as a skipped,
  reported component. `regia upgrade-project` rewrites `.save` v1 files as v2.

The notes of the last save or load are in the resource
`regia_framework::save_entity_refs::SaveEntityRefReport` and in the log (`regia_framework` target).

Not followed: an `Entity` inside a value reflected as one opaque value (its fields are hidden from
reflection), and references held by saveable *resources*.

## Methods

### `save.inspect`

A slot's saved entities with their save ids and references, read through the app's save store
(files in the `SaveDirectory` when none is installed). The world is not changed.

```json
{ "slot": "Chapter2" }
→ {
  "slot": "Chapter2", "version": 2, "current_version": 2, "legacy_refs": false,
  "entities": [
    { "id": 0, "key": "level:000000000000000000000000000000d0", "components": ["EntityGuid", "Tag"], "refs": [] },
    { "id": 1, "key": "runtime:0", "components": ["AiTarget", "Tag"],
      "refs": [{ "component": "AiTarget", "field": "target", "target": 3, "target_key": "level:00000000000000000000000000000077" }] }
  ],
  "externals": [{ "id": 3, "key": "level:00000000000000000000000000000077" }],
  "last_report": { "op": "save", "notes": [
    { "code": "save_entity_ref_unsaved", "entity": 2, "component": "AiTarget", "field": "target" } ] }
}
```

- `id` is the save id (position in the file); `key` is `runtime:<n>`, `level:<guid>` or `lost` (a
  reference a format 1 save could not keep). `externals` are level entities saved components point
  at without being saved themselves.
- `field` is the path inside the component: `target`, `seats[1].rider`, `0` for a tuple field.
- `legacy_refs` is `true` for a format 1 file (its references were converted on read).

Errors: `F0003` when the save plugin is missing or the slot does not exist, `F0002` without `slot`,
`F0001` when the file cannot be read; `data.reason` then carries the save code when there is one
(`save_entity_ref_out_of_range`, `save_entity_ids_invalid`).

## Codes

| Code | Level | Meaning |
|---|---|---|
| `save_entity_ref_unsaved` | warning | A plain `Entity` field points at an entity that is neither saved nor a level entity; the component is not saved. |
| `save_entity_ref_missing` | warning | At load, a plain field's target is missing; the component is skipped. |
| `save_entity_ref_cleared` | debug | An `Option<Entity>` had no target and was set to `None` (save or load). |
| `save_entity_ref_unmapped` | warning | The component type holds an `Entity` but does not register `MapEntities`, or holds one as a map key or set element; not saved. |
| `save_entity_ref_out_of_range` | load error | A reference names a save id past the file's table (a damaged or hostile file); the load is refused and the world is unchanged. |
| `save_entity_ids_invalid` | load error | The id table is shorter than the entity list or holds a runtime id among the externals; the load is refused. |

## Recipe: check that links survive

1. Spawn the pawn and its target with `Saveable`; `save.save { "slot": "T" }`.
2. `save.inspect { "slot": "T" }`: the pawn's `refs` name the target's `id`, `last_report.notes` is
   empty.
3. `save.load { "slot": "T" }`, then `play.expect` on the pawn's component: the target is the entity
   that now carries the target's other components.
