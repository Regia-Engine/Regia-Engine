# .smartobject.ron: smart object definition (v1)

Spec: `docs/specs/39-smart-objects-v2.md` § 3, ADR-1638. Code: `regia_ai::smart_object_def` (`SmartObjectDef`).
Guide: `book/agents/smart-objects.md`. Version 1 is new: there is no migrator and `regia upgrade-project` has nothing to
do. A `format_version` other than 1 is refused (`unsupported_version`) before the body is read. `//` comments are read.

An object entity carries `SmartObjectDefinition(Arc<SmartObjectDef>)`; the engine spawns one child slot per `slots`
entry (named `<object>/slot_<name>`, role = slot name) and keeps them in step with the definition.

## Schema

```
SmartObjectDef {
    format_version: u32,                    // 1
    activity: [String] = [],                // tags every slot offers (Activity.Sit)
    activity_merge: Union | SlotFirst = Union,
    user_rule: DefinitionAndSlot | SlotOnly | SlotElseDefinition = DefinitionAndSlot,
    user_query: Option<TagQuery> = None,    // the requester must match (tag names, e.g. AnyOf([Has("Role.Guard")]))
    conditions: [ConditionDef] = [],        // object-level, evaluated by search with check_conditions
    behaviors: [BehaviorDef] = [],          // defaults for every slot
    slots: [SlotDef],                       // <= 64
}
SlotDef {
    name: String,                           // unique, [A-Za-z0-9_]{1,32}
    offset: OffsetDef = identity,           // local transform
    activity: [String] = [],
    initial_tags: [String] = [],            // runtime tags a new slot starts with
    enabled: bool = true,
    user_query: Option<TagQuery> = None,
    conditions: [ConditionDef] = [],
    behaviors: [BehaviorDef] = [],          // for a family also in the definition, this entry wins
    entrances: [EntranceDef] = [],          // <= 8
}
OffsetDef   { translation: (f32, f32, f32) = (0,0,0), rotation: (f32, f32, f32, f32) = (0,0,0,1) }  // xyzw, normalized at load
EntranceDef { offset: OffsetDef, kind: Enter | Exit | Both = Both, rank: i8 = 0 }                   // higher rank is preferred
ConditionDef { path: String, params: { String: String } = {} }
BehaviorDef  { family: String, data: String = "" }                                                  // data is RON text decoded on demand
```

Load errors (all `bad_definition` except the version): a duplicate or malformed slot name, more than 64 slots or 8
entrances for a slot, a non-finite offset, a zero rotation. A tag not declared in the loaded tag table is dropped with a
`tag_unknown` warning and the load continues.

## Example

```ron
SmartObjectDef(
    format_version: 1,
    activity: ["Activity.Sit"],
    behaviors: [(family: "sit", data: "(pose: \"default\")")],
    slots: [
        (name: "left", offset: (translation: (-0.4, 0.0, 0.0)),
         entrances: [(offset: (translation: (-0.4, 0.0, 1.0)), kind: Both, rank: 0)]),
        (name: "right", offset: (translation: (0.4, 0.0, 0.0))),
    ],
)
```

## Referencing a definition

`SmartObject::definition` (`Option<AssetRef<SmartObjectDef>>`, persisted in scenes as `Some((id: "<guid>"))`) names a
definition by asset id, so an object keeps its definition when the file moves. `regia_ai::smart_object_load` resolves the id
through the asset server's manifest, reads the file (the source text when there is one, else the pak entry), and sets
`SmartObjectDefinition`. A project needs a cook for the id to exist. Objects naming one id share one loaded definition; a
changed file is read again in dev builds, and one that does not load keeps the definition in use and reports why
(`definition_error` in `smartobject.list`). A host may still parse a file and set `SmartObjectDefinition` itself on an object
with no `definition`.

## Cooked form

`regia cook` validates a definition (errors fail the cook) and stores its canonical RON text
(`SmartObjectDef::to_ron`) as a cooked asset of type `regia_ai::smart_object_def::SmartObjectDef`, which gives it an id and a
pak entry. There is no separate cooked shape, so no cooked version: a definition is small and loads once. The registry tags the
asset `kind=smart_object`. `regia package` ships the raw file as well, for a host that reads it itself.
