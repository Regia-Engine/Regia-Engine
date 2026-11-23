# Lua script components (`lua.components`, `lua.props.get`, `lua.props.set`, `lua.props.list`)

A Lua mod can declare a **script component**: a named set of typed properties that attaches to entities, shows
up in the editor inspector, saves in the scene and hot-reloads with the mod (ADR-3356 in `docs/decisions.md`,
`docs/specs/84-lua-mods.md` § 14). A designer picks an entity and edits the values per entity; a script reads
them while the game runs.

## Declaring

A mod declares a component once, in its entry chunk or a required module:

```lua
regia.component("mover", {
    props = {
        speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0, step = 0.1 },
        active = { type = "bool",   default = true },
        label  = { type = "string", default = "mover", multiline = false },
        offset = { type = "vec3",   default = regia.vec3(0, 1, 0) },
        tint   = { type = "color",  default = { 1.0, 0.5, 0.25, 1.0 } },
        target = { type = "entity" },                     -- default: no reference
        mesh   = { type = "asset",   filter = "mesh" },
        mode   = { type = "enum",    options = { "walk", "run" }, default = "walk" },
    },
    on_update = function(e, dt)
        local p = e:props("mover")
        if p.active then
            e:set_position(e:position() + p.offset * (p.speed * dt))
        end
    end,
})
```

Property types: `number` (with `min`/`max`/`step`), `bool`, `string` (with `multiline`), `vec3`, `color`
(linear RGBA list, edited with the color widget), `entity` (an entity reference or none), `asset` (a project
asset path, with a `filter` in the inspector's `@AssetFilter` syntax) and `enum` (a fixed `options` list of at most 64
items, repeats included, each at most 256 bytes).
Names are 1-64 bytes of lowercase ascii, digits and `_`; a name is unique across every loaded mod, and a mod may
only touch components it declared itself. A mod declares at most 64 components of at most 64 properties each, and
every declaration needs a non-empty `props` table. One declaration's names, options, filters and defaults hold at most
64 KiB (`MAX_DECL_BYTES`). A bad declaration, or one over a limit, fails the load, restart or reload with
`lua_bad_value`.

The optional `on_update(entity, dt)` runs once per frame **per entity carrying the component**, under the mod's
instruction budget; an error disables the mod like any other call. In one update, all of a mod's declaration passes
share one budget and one call (the global `on_update` is its own call): each pass with entities charges that budget
64 instructions, plus one per 8 entities the lookup looked at (1 with the engine's component index), plus one per
entity. A pass with no entities makes no call and costs nothing.

## Attaching and editing from Lua

```lua
local walker = regia.spawn("walker", regia.vec3(0, 0, 0))
walker:add("mover")               -- every property starts at its default
walker:set_prop("mover", "speed", 5.0)   -- clamped to the declared range
local p = walker:props("mover")   -- a plain table of the current values
walker:has("mover")               -- true
walker:remove("mover")            -- detaches it
```

One script component per entity. Attaching, reading and writing from Lua require the entity to be alive; a
dead handle answers `nil`/`false`.

## The inspector and the scene

In the editor the component draws as its own inspector section with typed widgets (drag values with ranges,
checkboxes, the colour picker, the asset drop-down, enum drop-downs). Edits are undoable and go through the
same `SetProperty` commands as any component. Values persist in the scene file (`regia_script_lua::components::
LuaComponent`, a reflected component) and survive save/load like any other component.

A **hot reload** reconciles stored values against the new declaration: values of properties still declared
survive (including every designer edit), new properties get their defaults, removed properties go, and a
property whose type changed falls back to its default. A reload that fails rolls its component writes back with
everything else.

## Agent methods

Registered by `LuaModsPlugin` on the world's `AgentExtension` (the `lua_mods` capability), alongside the
[Lua mods](lua-mods.md) methods. `entity` is the entity's bits, shown in the inspector as `id`.

- `lua.components` (read) — `{ mod? }` → every declared component with its full property schema (`type`,
  `default`, `min`/`max`/`step`, `options`, `filter`, `multiline`, `has_update`). Properties are listed
  sorted by name (a Lua table keeps no declaration order); `options` keep the order the list wrote them in. A
  declaration needs a `props` table with at least one property.
- `lua.props.get` (read) — `{ entity, component }` → the entity's values and the declaration's defaults as
  JSON. `vec3` is `{ "vec3": [x, y, z] }`, an entity reference is `{ "entity": bits }`, a colour is
  `[r, g, b, a]`, no reference is `null`.
- `lua.props.set` (edit scene) — `{ entity, component, props: { name: value } }` merges validated edits:
  numbers clamp to the declared range, unknown properties and wrong types are refused (`F0002`), an
  undeclared component or an entity that does not carry it is `F0003`. Returns the merged values.
- `lua.props.list` (read) — `{ entity }` → the declared components the entity carries, with their values.

```json
// -> { "id": 1, "method": "lua.props.set", "params": { "entity": 4294967297, "component": "mover",
//      "props": { "speed": 5.0, "mode": "run" } } }
// <- { "plugin": true, "entity": 4294967297, "component": "mover",
//      "props": { "active": true, "label": "mover", "mesh": "", "mode": "run", "offset": { "vec3": [0, 1, 0] },
//                 "speed": 5.0, "target": null, "tint": [1.0, 0.5, 0.25, 1.0] } }
```

Errors carry the usual lowercase codes: `lua_bad_value` for a script-side declaration or value error,
`F0002` for bad parameters, `F0003` for a missing component or entity.

## Editor

The editor loads a project's Lua mods from `mods/<id>/` next to `Regia.toml` when a project opens, with the
directory watch on, so editing a script reloads it (and reconciles the edited values) while the level stays
open. The inspector shows the component only while its mod is loaded; without the `LuaModsPlugin` the
component is not registered and scenes containing it skip it with the scene's unknown-type policy.
