# Reflection

Reflection lets the engine look inside your types at runtime: list a component's fields, read and
write them by name, and turn them into text and back. Regia Engine uses `bevy_reflect` for this
(through `regia_ecs`), and almost every tool in the engine depends on it:

| Feature | Uses reflection to |
|---|---|
| Levels and prefabs | save and load components as canonical RON keyed by type path |
| Editor inspector | show and edit every field, with ranges, units and tooltips |
| Agent protocol | describe types (`types.describe`), read entities, `scene.set_property`, `play.expect` on field values |
| Save games | serialize `Saveable` entities and saveable resources |
| Hot reload | snapshot the world before swapping the game library and restore it after |
| Runtime UI | bind a label to a field (`bind: (source: "Score.value", …)`) |

A component that is not reflected and registered still works in your systems, but none of the
above can see it.

## Making a type reflectable

Derive `Reflect`, list the reflected traits the engine should be able to call, and register the
type with the app:

```rust
use regia::prelude::*;

/// A door that opens toward a target angle.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct Door {
    /// Current opening angle, radians.
    pub angle: f32,
    /// Whether the door is locked.
    pub locked: bool,
    /// Opening speed, radians per second.
    pub speed: f32,
}

/// Global door settings.
#[derive(Resource, Reflect, Debug, Clone, Default)]
#[reflect(Resource, Default)]
pub struct DoorRules {
    /// Doors close themselves after this many seconds.
    pub auto_close_secs: f32,
}

fn register(app: &mut App) {
    app.register_type::<Door>().register_type::<DoorRules>();
}
```

- `#[reflect(Component)]` lets the engine insert, read and remove the component by type;
  `#[reflect(Resource)]` does the same for resources.
- `#[reflect(Default)]` lets the engine construct the type when a file or an agent names it.
- Doc comments become field descriptions in the inspector and in `types.describe`; write them.
- Nested structs, enums, `Vec`, `Option`, maps and the math types (`Vec3`, `Quat`) reflect too,
  as long as each nested type derives `Reflect`.
- Fields you do not want saved or shown can be skipped with `#[reflect(ignore)]`.

Engine types are registered by their plugins. A game type you forget to register shows up in
the agent protocol's `errors.explain` and the editor as unknown; register every type in the
plugin that owns it.

## Editor attributes

Attributes attach editing metadata to fields. The inspector and the agent schema read them; the
runtime ignores them:

```rust
use regia::prelude::*;
use regia_ecs::attrs::{Category, Range, Step, Tooltip, Units};

/// A spotlight-like door lamp.
#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
pub struct DoorLamp {
    /// Brightness of the lamp.
    #[reflect(@Range(0.0..=2000.0), @Step(10.0), @Units("lm"), @Tooltip("Luminous flux"))]
    pub intensity: f32,
    /// Opening angle that switches the lamp on.
    #[reflect(@Range(0.0..=180.0), @Units("deg"), @Category("Trigger"))]
    pub trigger_angle: f32,
}
```

| Attribute | Effect |
|---|---|
| `@Range(a..=b)` | Sliders clamp to it; `types.describe` emits `minimum`/`maximum` on that field's schema (`attrs.range` is only the attribute on the type itself) |
| `@Step(x)` | Drag and step increment; `types.describe` emits `multipleOf` on a field's schema and `attrs.step` on the type |
| `@Units("m")` | Display units (`"m"`, `"deg"`, `"s"`, `"dB"`, `"lm"`, …) |
| `@Tooltip("…")` | Hover help |
| `@DisplayName("…")` | Label instead of the field name |
| `@Category("…")` | Groups fields under a collapsible Inspector header |
| `@Hidden` | Not shown in the inspector (still saved) |
| `@ReadOnly` | Shown, not editable |
| `@Multiline` | Multi-line text box |
| `@ColorHdr` | Color picker that allows values above 1 |
| `@AssetFilter("mesh")` | Asset picker filter |
| `@EditCondition("expr")` | Greys the field while `expr` over its sibling fields is false (`"cast_shadows"`, `"kind == Spot"`); see [the Inspector agent page](../agents/inspector.md) for the grammar |
| `@EditConditionHides` | With `@EditCondition`: hide instead of grey |
| `@Toggle` | On a struct field with `enabled: bool`: a header checkbox; the body greys while off |

`regia_ecs::describe::field_attrs::<T>("field")` reads them back in code.

## Canonical text

`regia_ecs::serialize` turns any reflected value into **canonical RON**, the form levels,
prefabs and save files use: struct fields in declaration order, map keys sorted, floats written
in their shortest round-trip form, a fixed indentation. Writing the same value twice produces the
same bytes, which is why re-saving an unchanged level leaves `git diff` empty and why agent edits
review cleanly. `to_postcard` / `from_postcard` give the compact binary form cooked data uses.

## Reflection and the agent protocol

The agent protocol's discovery methods are reflection made visible:

```console
$ regia agent call types.list '{"component":true}'
$ regia agent call types.describe '{"type":"my_game::Door"}'
$ regia agent call scene.set_property '{"entity":42,"component":"my_game::Door","path":"angle","value":1.2}'
```

`types.describe` returns the fields with their types, doc comments and editor attributes, so an
agent can edit a type it has never seen. The exact parameters are in the
[agent guide's protocol](../agents/protocol.md) and [scene](../agents/scene.md) pages.

## Who registered what

Every registration made while a game plugin is building is recorded with its owner
(`regia_ecs::owner`). Hot reload uses this to remove exactly the game's types, systems and
observers before loading the new library, so the engine's own registrations are never touched
([Hot reload](../user-guide/hot-reload.md)).
