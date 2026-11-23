# mods:// (`mods.list`, `mods.mount`, `mods.unmount`, `mods.order`, `mods.hash`, `mods.read`)

`mods://` is the file system of a game's mods (`regia_asset::mods_vfs::ModsVfs`, ADR-2418). A mod is a folder, a
`.fpak` archive or an in-memory file map. Mods mount in a **declared load order**: the first is the base, and where two
mods hold the same path the later one wins. Every such override is reported. The mount is **read-only** and
**confined**:

- A path is relative: `mods://textures/rock.ktx2`, or `textures/rock.ktx2`. A leading `/` or `\`, a drive letter
  (`C:`) or a `..` segment is refused with `mods_path_escape`; another scheme, a backslash, an empty segment, a control
  character, `:` or a reserved device name (`NUL`, `COM1`) with `mods_path_invalid`.
- A mod folder may not hold a symbolic link (on Windows, any reparse point such as a junction) anywhere below its root:
  the mount is refused with `mods_symlink`. The root itself may be a link (Workshop and launcher folders often are).
  A read walks the path again, so a link swapped in after the mount is refused too.
- On Linux, Android and macOS a file is opened without following a link and without blocking, must be a regular
  file, and must be the same file (device and inode) the mount indexed: a file or folder swapped in under its name,
  even with the same bytes, is `mods_changed`, and a FIFO put in its place is refused at once instead of hanging the
  read. Windows checks links by path only.
- Only the files indexed when the mod mounted can be read. A file added later is `mods_not_found`; one changed or
  removed later is `mods_changed` (remount the mod to take the change).
- A pak whose manifest names a path outside the mod is refused whole.

## Content hashes and lobby matching

Each mod has a content hash: BLAKE3 (derived key `regia mods file listing v1`) over its files sorted by path, each as
the path length (`u64` LE), the path, the size (`u64` LE) and the file's own BLAKE3. The mod id and the source kind are
not in it, so the same files as a folder, a pak or a Workshop download hash the same. The **content-set hash** is
BLAKE3 (derived key `regia mods content set v1`) over the number of mods (`u64` LE) and each content hash in load order:
stable across runs and machines, different for a different order. The empty set has a hash too.

Lobbies match on it under the key `mods.content_set` (64 hex digits; `ModsVfs::lobby_entry`). The game puts it in the
lobby data when it creates the lobby and filters lobby lists on it:

<!-- ignore-reason: a fragment of lobby code: `mods`, `spec` and `query` are the host's own lobby values -->
```rust,ignore
let (key, value) = mods.lobby_entry();
spec.data.insert(key.into(), value.clone());                 // LobbySpec when creating
let query = query.with_string(key, Compare::Eq, &value);     // LobbyQuery when listing
```

`regia_steam::SteamWorkshopPlugin` keeps the key of a lobby this player owns up to date when the mounted set changes.
`mods.hash { "publish": true }` writes it once into the current lobby.

## Methods

| Method | Params | Result |
|---|---|---|
| `mods.list` | `conflicts`? (default true) | `{ order, mods: [{ id, order, kind, origin, files, bytes, overridden, content_hash }], file_count, conflict_count, conflicts: [{ path, winner, overridden }], conflicts_truncated, content_set_hash }` |
| `mods.mount` | `id`, `path`, `kind`? (`auto`, `dir`, `pak`) | `{ mod, order, conflict_count, content_set_hash }` |
| `mods.unmount` | `id` | `{ order, content_set_hash }` |
| `mods.order` | `order`? (every mounted id once, first = base) | `{ order, content_set_hash, previous_hash }` |
| `mods.hash` | `publish`? | `{ content_set_hash, lobby_key, mods: [{ id, content_hash }], published?: { ok, lobby?, error? } }` |
| `mods.read` | `path`, `mod`? | `{ path, mod, bytes, blake3, text }` |

`mods.mount`, `mods.unmount` and `mods.order` need the `assets` permission; `mods.hash` needs `play` (it may write lobby
data). `mods.mount`'s `path` is confined like `replay.load`'s: relative to the project root, or absolute under the
project root, the system temp dir, or an `output_roots` or `replay_roots` entry of `.regia/agent.toml`
(a refusal is `F0002` with `data.reason` `path_outside_project` and `data.allowed_roots`). `kind: auto` mounts a
`.fpak` file, or a folder holding nothing but one `.fpak`, as that pak, and any other folder as a folder. A mod id is
`[a-z0-9_.-]`, 1 to 64 bytes. `mods.list` lists at most 256 overridden paths; `conflict_count` is exact. `mods.read`
returns `text` for UTF-8 files of at most 64 KiB, else `null`. Without a `ModsVfs` resource the read methods answer an
empty set and `mods.mount` adds one.

```text
mods.mount   {"id":"base","path":"mods/base"}
mods.mount   {"id":"bigger_guns","path":"mods/bigger_guns"}
mods.list    {}
→ { "order": ["base","bigger_guns"], "file_count": 41, "conflict_count": 1,
    "conflicts": [{ "path": "data/units.ron", "winner": "bigger_guns", "overridden": ["base"] }], ... }
mods.read    {"path":"mods://data/units.ron"}
→ { "path": "mods://data/units.ron", "mod": "bigger_guns", "bytes": 9, "blake3": "…", "text": "(hp: 250)" }
mods.order   {"order":["bigger_guns","base"]}
mods.hash    {"publish":true}
```

## Error codes

Refusals carry the code in `data.code`.

| Code | Meaning |
|---|---|
| `mods_path_invalid` | Not a valid `mods://` path (scheme, backslash, empty segment, control character, `:`, reserved name), or a file in a mod folder with such a name. |
| `mods_path_escape` | The path leaves the mount: absolute, a drive letter, or `..`. |
| `mods_symlink` | A symbolic link or reparse point below a mod's root, at the mount or on a read. |
| `mods_not_found` | No mounted mod serves that file or directory. |
| `mods_unknown_mod` | No mod with that id is mounted. |
| `mods_duplicate` | A mod with that id is already mounted. |
| `mods_id_invalid` | A mod id that is not `[a-z0-9_.-]`, 1 to 64 bytes. |
| `mods_io` | Reading a mod folder failed (missing, not a folder, a special file). |
| `mods_pak` | A mod's pak could not be opened or read. |
| `mods_too_large` | More than 65,536 files or 8 GiB in one mod, one file over 1 GiB (`max_file_bytes`; a pak entry's declared size counts, before anything is decompressed, ADR-5870), a pak entry that decompresses to more than 1 GiB (the same `max_file_bytes`, read from the pak's table of contents before anything is decompressed, ADR-5869, ADR-5993), folders deeper than 32, or more than 256 mods (`ModsLimits`). |
| `mods_read_only` | A write: `mods://` is read-only (`ModsVfs::write`). |
| `mods_order_invalid` | An order that does not name every mounted mod exactly once. |
| `mods_changed` | A file changed or vanished after its mod mounted. |

## Lua mods

A Lua mod mounted at `mods://` builds its file set from its own files with `ModFiles::from_mods_vfs(vfs, id,
max_bytes)`: `luamod.ron` and the `.lua` files, under the mod size cap, read through the confined mount. It works the
same on Android, where there is no mod directory to read.
