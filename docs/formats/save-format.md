# Save-file format

Spec: `docs/specs/14-scene-and-framework.md` § B7.

## On-disk layout

Every save file is a binary blob with this structure (little-endian):

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Magic bytes `b"FRTS"` (`0x46 0x52 0x54 0x53`) |
| 4 | 4 | Format version (`u32` LE), current = 2: the version the payload is in (`SaveFileRaw.version`; `save_raw` of an older payload writes that older version, so its migrations run on load; a newer one is refused) |
| 8 | N | Postcard-encoded [`SaveFileRaw`] payload (variable length) |
| 8+N | 4 | CRC-32 (IEEE 802.3) of bytes [8..8+N) |

Total file size = 12 + N bytes, at most `MAX_SAVE_FILE_BYTES` (1 GiB): a larger file is refused before it is read
(`SaveError::Corrupt`), and slot listing skips it.

## Payload: `SaveFileRaw`

The payload is a single [`postcard`] serialisation of:

```rust
struct SaveFileRaw {
    version: u32,           // format version at write time
    game_version: String,   // e.g. "1.0.0"
    engine_version: String, // e.g. "0.1.0"
    timestamp_secs: f64,    // seconds since UNIX epoch
    thumbnail: Option<Vec<u8>>, // PNG screenshot (None = absent, always serialised)
    entities: Vec<SavedEntity>,
    resources: Vec<SavedResource>,
    entity_ids: Vec<SaveEntityId>, // format 2 (ADR-3239); absent in format 1 payloads
}

enum SaveEntityId {            // postcard: variant index, then the fields
    Runtime(u32),              // 0: a runtime-spawned saved entity (ordinal among them)
    Level { guid: u128, level: u128 }, // 1: EntityGuid + LevelMember level (0 without one)
    Lost,                      // 2: a reference format 1 could not keep
}

struct SavedEntity {
    // Component type short path (e.g. "my_game::Health") → postcard bytes.
    components: BTreeMap<String, Vec<u8>>,
}

struct SavedResource {
    type_path: String,  // full type path, e.g. "my_game::Score"
    data: Vec<u8>,      // typed postcard payload (ReflectCodec::to_postcard_typed)
}
```

### Component serialisation

Each reflected component on a [`Saveable`] entity is encoded with the engine's typed postcard
codec (`ReflectCodec::to_postcard_typed`, the same pair as `from_postcard_typed`) and stored
keyed by the component's short type path. Only registered, reflected components are saved;
unregistered types and the `Saveable` marker itself are skipped. Binary loading rules apply:
missing fields take defaults, unknown fields warn (the layout is positional, so rebuild cooked
data when types change).

### Resource serialisation

Resources are collected automatically: a resource type implements the `SaveableResource` marker
trait and is registered once with `app.register_saveable_resource::<T>()`, which registers the
type for reflection and adds it to the collected list. `SaveGame::save` encodes every collected
resource that is present in the world (full type path → typed postcard bytes); `SaveGame::load`
decodes each entry and replaces the resource. Absent resources are skipped; unregistered types
found in a file warn and are skipped on load.

### Sections

A system that collects its own save data registers a section with `SaveSections::register` (ADR-1943). A section is
stored as one more `SavedResource`: `type_path` is the section's key and `data` is whatever bytes it wrote. The layout
above does not change, so the format stays at version 1, and an engine without the section warns and skips the entry.
Lua mod state is the section `regia_script_lua::LuaState` (format `luastate`, `docs/specs/84-lua-mods.md` § 11).

### Entity references (format 2, ADR-3239)

A saved entity's **save id** is its position in `entities`. `entity_ids` holds one entry per saved entity in the same
order, then one per *external* entity (a level entity a saved component points at without being saved), sorted by
`(level, guid)`. Saved entities are ordered too: those with an `EntityGuid` by `(level, guid)`, then runtime entities by
entity index and generation, so two saves of one world are byte-identical apart from `timestamp_secs`.

Inside a component payload, every `Entity` (in a field, `Option`, enum, list, array or map value) is written as the
entity whose index is the target's save id and whose generation is 1; `Entity::PLACEHOLDER` means "no entity". Only
types that register `ReflectMapEntities` (and bevy's `ChildOf`) are written with references; `Children` is never saved.

On load every reference must name an entry of the table (else `SaveError::Corrupt`, `save_entity_ref_out_of_range`);
the table must not be shorter than `entities` unless it is empty, and externals must not be `Runtime`
(`save_entity_ids_invalid`). All saved entities are spawned before any reference is remapped; externals resolve by
guid and level against the live world. Full rules: `docs/specs/91-character-save-templates.md` § 10.

An **empty** `entity_ids` means the references were never converted (a format 1 payload, or one `regia upgrade-project`
rewrote): the loader converts them with the app's type registry, every non-placeholder `Entity` becoming `Lost`.

## Atomic writes

Files are written atomically:

1. Serialise the payload and compute the CRC footer.
2. Write the full blob to a temp file of this write's own, `<slot>.save.tmp.<pid>.<n>`, created new
   (an existing entry or a symbolic link at that name fails the write instead of being followed), so
   two writers of one slot never share a temp file.
3. `fsync` the temp file; a failure fails the save (`SaveError::Io`).
4. `rename` the temp file over the target `<slot>.save`.
5. `fsync` the parent directory (best effort).

If the process crashes after step 2, the orphaned temp file stays behind; it never shadows a save.
The fixed name earlier releases used, `<slot>.save.tmp`, is removed on the next save when it is a
plain file. No partial `.save` file is ever observed.

## Validation on load

1. Read the file; reject if fewer than 12 bytes.
2. Check magic = `b"FRTS"`; reject with [`SaveError::Corrupt`].
3. Read format version from bytes [4..8).
4. Read stored CRC from the last 4 bytes; compute CRC over bytes [8..len−4).
5. Reject on CRC mismatch.
6. Deserialise the payload with [`postcard::from_bytes`]: format 1 with the format 1 layout (no `entity_ids`), later
   versions with the current one.
7. If `file_version < SAVE_FORMAT_VERSION`, run every registered [`SaveMigrations`] hook from
   `file_version` up to (but not including) the current version, in order. Each hook mutates
   the [`SaveFileRaw`] in place and bumps `raw.version`. Every step must have a hook, except the steps the engine
   handles itself (1 → 2, the entity id table): a missing one fails the load with `SaveError::Deserialize` before
   anything is applied. Then a payload with an empty `entity_ids` has its references converted (above).
8. Decode every component and resource payload. One that does not decode fails the load with
   `SaveError::Deserialize`, and the live world is left as it was; every entity reference is checked against the
   table; only then are the current saveable entities despawned, the saved ones spawned, the references remapped
   and the components inserted. A type the app does not register is
   skipped with a warning (a type the game removed).

## Migration

Games register migration functions with [`SaveMigrations::register(from_version, fn)`]. The
function signature is `fn(&MigrationCtx, &mut SaveFileRaw)`: the context carries the version
being migrated **from** and the running app's type registry, so hooks can decode, change and
re-encode component payloads through `SaveFileRaw::map_component(registry, short_path, visit)`.
Migrations run sequentially on load when the on-disk version is older than
`SAVE_FORMAT_VERSION`; each hook bumps `raw.version`.

Example: a game whose `Health` field switches from whole points to tenths registers a
migration for version 0→1 that calls `map_component` with the registry and multiplies `hp`
by 10. A game that renames a component from `OldHealth` to `Health` walks `raw.entities` and
renames the key in `components` (no registry needed).

## Extension

- The format version is included in the payload (not only the header) so that migration hooks
  can read the *pre-migration* version and adjust the data accordingly.
- `SaveFileRaw` is `#[non_exhaustive]` so new fields can be added without breaking host code. postcard is
  positional, so a new field still needs a format version and a reader for the older layout (format 2's
  `entity_ids` is read only from version 2 payloads).

## Version history

| Version | Change |
|---|---|
| 1 | Initial layout. `Entity` fields inside components were raw entity bits. |
| 2 | Adds `entity_ids` (stable save ids) and writes `Entity` fields as save ids (ADR-3239). v1 loads through the built-in converter; `regia upgrade-project` rewrites v1 files as v2 with an empty table. |

## Slot naming

Slot names are sanitised to `[A-Za-z0-9_-]` only; unsafe characters are replaced with `_`. A name
that is a Windows device name (`con`, `nul`, `com1`, ...) gets a trailing `_` on every platform, and
`SaveGame::slot` turns an empty name into `_`.
Slots are stored as `<sanitized_name>.save` in the [`SaveDirectory`], which defaults to
`user://saves/`.

## CRC-32

CRC-32 (IEEE 802.3) with polynomial `0xEDB8_8320`, initial value `0xFFFF_FFFF`, final XOR
complement. The CRC is computed over the postcard payload only (bytes [8..8+N)), excluding
magic, version, and the CRC footer itself.

[`postcard`]: https://docs.rs/postcard/
[`Saveable`]: https://docs.rs/regia_framework/latest/regia_framework/save_game/struct.Saveable.html
[`SaveableResource`]: https://docs.rs/regia_framework/latest/regia_framework/save_game/trait.SaveableResource.html
[`SaveFileRaw`]: https://docs.rs/regia_framework/latest/regia_framework/save_game/struct.SaveFileRaw.html
[`SaveMigrations`]: https://docs.rs/regia_framework/latest/regia_framework/save_game/struct.SaveMigrations.html
[`SaveError::Corrupt`]: https://docs.rs/regia_framework/latest/regia_framework/save_game/enum.SaveError.html
[`SaveDirectory`]: https://docs.rs/regia_framework/latest/regia_framework/save_game/struct.SaveDirectory.html
[`ReflectSerializer`]: https://docs.rs/bevy_reflect/latest/bevy_reflect/serde/struct.ReflectSerializer.html
[`postcard::from_bytes`]: https://docs.rs/postcard/latest/postcard/fn.from_bytes.html