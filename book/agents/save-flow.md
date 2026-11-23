# Save flow and character move-to

The save flow (`regia_framework::save_flow`, spec 91) is the layer a game uses on top of
`SaveGame`: a **save store** decides where save bytes live, any system can ask for a save or load
with a message, quicksave has its own slot and keys, and two saveable resources carry slot
information and the game's own data. `character.move_to` walks a character to a point.

Every `save.*` method goes through the same store the game uses, so an agent that saves while the
game keeps its saves in Steam Remote Storage writes to Steam too.

## In a game

```rust,wrap
use regia_framework::prelude::*;
# use regia_app::prelude::App;
# use regia_ecs::prelude::{MessageReader, MessageWriter};
# let mut app = App::new();

app.add_plugins((SavePlugin::default(), SaveFlowPlugin));

// Any system: ask, then read the answer next frame.
fn on_menu(mut requests: MessageWriter<SaveRequest>) {
    requests.write(SaveRequest::Save { slot: "Chapter2".into() });
}
fn toast(mut outcomes: MessageReader<SaveOutcome>) {
    for outcome in outcomes.read() {
        // outcome.op (Save / Load / Delete), outcome.slot, outcome.quick, outcome.result
    }
}

```

<!-- ignore-reason: requires the regia_steam host feature and an initialized Steam client -->
```rust,ignore
// Steam cloud saves instead of files (regia_steam):
app.insert_resource(SaveStore::new(SteamRemoteStorage::new(&steam)));
```

- **Store.** `SaveStore` holds a `SaveStorage` (`write`, `read`, `delete`, `list` of bytes, plus a
  `kind` name). Without one, slots are `SaveDirectory/<slot>.save` files (`kind` `file`), the same
  files `save.save` always wrote. `regia_steam` ships `SteamRemoteStorage` (`steam_remote`, files
  `saves/<slot>.frts`) and `LocalSaveBackend` (`user_dir`). The bytes are the unchanged save
  container (magic `FRTS`, version, payload, CRC-32), so a slot moves between stores as is. The
  autosave on suspend goes to the store too.
- **Requests.** `SaveRequest::{Save, Load, Delete, QuickSave, QuickLoad}` are drained once per
  frame in `Last`, in the order they were written; each gets one `SaveOutcome`.
- **Quicksave.** `QuickSaveConfig { slot: "Quick", save_action: "QuickSave", load_action:
  "QuickLoad" }`. Bind the two actions (the templates use F5 and F9) and pressing them sends the
  requests.
- **Slot names** keep ASCII letters, digits, `-` and `_`; anything else becomes `_` (`"Chapter 1"`
  is slot `Chapter_1`). A Windows device name gets a trailing `_` on every platform (`"con"` is slot `con_`), so a
  slot saves the same everywhere. An empty name is refused.
- **A load is all or nothing.** Every component and resource payload is decoded before the world changes; one that no
  longer decodes, or a save older than the format with a migration step missing, fails the load (`F0001`, the message
  says which component or version) and the running game keeps its state. A component or resource type the game no
  longer registers is skipped with a warning.
- **`SaveInfo`** (`display_name`, `level`, `play_time_secs`, `save_count`) is saved in every
  slot. Play time counts gameplay time (pauses do not count); `save_count` goes up by one per save
  and the file keeps the count it was written with.
- **`SaveSections`** (`regia_framework::save_game`) is for a system that collects its own save data with the whole
  world in hand: `sections.register(key, write, read)`. `write` runs before the entities are serialized and `read`
  after the world is restored, with `None` when the save has no such section. Every save and load runs them, and the
  bytes are one more saved resource, so the save format does not change. Lua mods keep their state this way
  ([Lua mods](lua-mods.md)). Use `sections.register_required(key, write, read)` for state that
  must not be omitted: a write error fails serialization before the file/cloud save is replaced.
  Lua uses required registration, so a held-state `lua_too_large` fails the save; `lua.list`
  `last_save` shows the error and `held_state` remains available for a retry (ADR-7951).
- **`SaveBlobs`** stores your own bytes by key (`set`, `get`, `set_value` / `get_value` through
  postcard). Use it for state you serialize yourself, such as a quest log or script state you want
  to keep.

Both resources are ordinary saveable resources inside save format version 1: files written
before them load unchanged, and an older engine skips them with a warning.

A load respawns `Saveable` entities from their **reflected** components only. Meshes and physics
bodies are not reflected, so do not mark a rendered, simulated player `Saveable`; keep its state in
a saveable resource and put it back after the load, as the `third_person` and `top_down`
templates do with `PlayerCheckpoint`.

## Methods

### `save.quick_save`

Saves the world to the quick slot.

```json
{}
→ { "slot": "Quick", "entity_count": 3, "resource_count": 4, "bytes": 812, "store": "file",
    "copy": "current" }
```

### `save.quick_load`

Loads the quick slot (the game's migrations run). Same result shape. `copy` is `"previous"` when
the slot's current file was damaged or missing and its previous good generation (`<slot>.prev`)
loaded instead ([Player data](../user-guide/player-data.md), ADR-3102). `F0003` when there is no
quick save yet.

### `save.delete`

```json
{ "slot": "Chapter1" }
→ { "slot": "Chapter1", "deleted": true }
```

`deleted` is `false` when the slot did not exist (not an error). `F0002` without `slot`.

### `save.status`

```json
{}
→ { "store": "file", "quick_slot": "Quick", "slots": ["Chapter1", "Quick"],
    "slots_with_previous": ["Quick"], "last_load": { "slot": "Quick", "copy": "previous" },
    "info": { "display_name": "", "level": "main", "play_time_secs": 412.5, "save_count": 7 },
    "blob_keys": ["quest"] }
```

`slots_with_previous` lists the slots that keep a previous good generation (the file store keeps
one per slot; the Steam stores keep none). `last_load` is the slot and copy (`current` or
`previous`) of the last load, `null` before the first. `info` is `null` without `SaveFlowPlugin`. Use it to check which store a build uses before a save
test, and to see the play time and save count move.

### `character.move_to`

Gives a character a `MoveTo`: each frame its `CharacterInput` points at the target (horizontal
distance only; slower inside 1.5 m), and inside `accept_radius` the input is released, the
component is removed and a `MoveToArrived` message is sent. It walks a straight line; obstacles
are not avoided.

```json
{ "entity": 4294967304, "x": 4.0, "y": 0.0, "z": -3.0, "accept_radius": 0.5 }
→ { "entity": 4294967304, "target": [4.0, 0.0, -3.0], "accept_radius": 0.5 }
```

Optional `sprint` (boolean, default false); `accept_radius` defaults to 0.25 m. Check arrival with `play.wait_until` on the character's position, or
with `play.expect` that the entity has no `MoveTo` component left.

## Errors

| Code | When |
|---|---|
| `F0003` | No save plugin in the app (`save.*`), no quick save yet (`save.quick_load`), a slot that does not exist (`save.load`), the entity is not a character with `CharacterInput`, or the app has no `CharacterControlPlugin` (`character.move_to`). |
| `F0002` | A missing or empty `slot` (`save.delete`); a missing entity or coordinate, or a negative `accept_radius` (`character.move_to`). |
| `F0001` | The store failed to write or read (the message names the slot file; see `logs.read`, target `regia_framework`), or a load found a payload that does not decode or a missing migration step (the world is unchanged). |

## Verify a save round trip

1. `save.status` → note `store` and `info.save_count`.
2. Change something with `scene.set_property`.
3. `save.quick_save` → `save.status` shows `save_count` one higher.
4. Change it again, `save.quick_load`, then `play.expect` the saved value.
5. `save.delete { "slot": "Quick" }` to clean up.

On Android `SaveDirectory` defaults to the app's internal data path plus `saves` (the `user://`
host; `run-as <package> ls files/saves` shows the files of a debuggable build). Before ADR-4929 it
was a relative `saves`, so a suspend's autosave logged `autosave failed: ... Read-only file system`.
