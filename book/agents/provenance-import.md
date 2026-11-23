# Pack provenance records into .meta provenance

A game that kept its asset provenance outside the engine has one TOML record per content pack,
`provenance/<pack>.toml`: a `schema` number (1 to 3), the `pack_id`, a `status` (for example
`approved` or `quarantine`) and one `[[files]]` row per file with `path`, `sha256`, `byte_len`,
`origin`, `tool`, `generator`, an optional `seed`, `created_utc`, `reviewer`,
`foreign_game_file` and `third_party_mod_file`. Voice rows carry more fields (`line_id`, `variant`,
`text`, `voice`, `tts`, `take_approved_utc`). `assets.provenance_import` moves every row into the
provenance block of its source's `.meta` sidecar (ADR-8834), so the project's
`config/provenance.ron` policy, `regia assets validate` and the final cook see it
([asset provenance](asset-provenance.md)). The method needs the `assets` permission. The CLI is
`regia assets provenance-import RECORD... [--project DIR] [--under DIR] [--dry-run] [--json]`.

The `.meta` format does not change (it stays v2): what the block has no field for goes into its
`extensions` map, which already holds project-defined fields.

## `assets.provenance_import`

```json
{"jsonrpc":"2.0","id":1,"method":"assets.provenance_import","params":{"records":["provenance"],"under":"packs/base","dry_run":true}}
{"jsonrpc":"2.0","id":1,"result":{"applied":false,"records":2,"rows":3,
  "written":["packs/base/mesh/crate.obj","packs/base/voice/intro.ogg"],"kept":[],
  "missing_source":[],"missing_meta":["packs/base/tex/rock.png"],
  "duplicates":[],"conflicts":[]}}
```

- `records` (required, at most 256): project-relative record files, or folders; a folder reads
  every `*.toml` directly inside it, sorted by name. A `*.toml` there that is a symbolic link counts
  when it leads to a regular file inside the folder; a FIFO, socket or device, or a link that
  dangles, leaves the folder or leads to anything else, is `provenance_import_unreadable` (never
  skipped). A folder named `*.toml` is skipped.
- `under` (optional): the folder under `assets/` that holds the packs' content root; record paths
  are relative to it. Default: `assets/` itself.
- `dry_run` (optional, default `false`): report `written` without writing (`applied: false`).

Every path in the answer is relative to `assets/` and sorted.

### How a row maps

| record field | `.meta` provenance |
|---|---|
| `origin` | `origin` |
| `generator` | `generator` (`"none"` is no generator) |
| `seed` | `seed` when 0 or more; a negative seed leaves `seed` empty and is kept exactly as the extension `seed_signed` |
| `created_utc` | `created_utc` |
| `reviewer` | `owner`; empty or `pending` is pending; any other name is a named reviewer |
| `tool` | the block's `tool` is `hand` for the label `hand`, `generator` when the row names a generator, `import` otherwise; the label itself is the extension `source_tool` |
| `sha256`, `byte_len`, `foreign_game_file`, `third_party_mod_file` | extensions with those names (`sha256` lowercase, the rest as text) |
| `client`, `session`, `model` (if a row has them) | the built-in fields of those names |
| `pack_id`, `status` | extensions `pack_id`, `pack_status` on every row |
| any other row field (voice rows: `line_id`, `variant`, `text`, `voice`, `tts`, `take_approved_utc`) | an extension under its own name: strings as they are; numbers, booleans and dates as written; arrays as TOML inline text |
| any other pack-level field `x` | the extension `pack_x` on every row of the pack |

To make the policy check the migrated fields, declare them in `config/provenance.ron` (for
example `pack_status` with `values: ["approved"]`, or `foreign_game_file` with `values: ["false"]`).

### Unknown fields

Unknown fields are **kept**, never dropped. A record is refused (`provenance_import_invalid`) only
when a field cannot be kept: its name is not an extension key (lowercase ascii, digits and `_`,
1 to 64 bytes), it would overwrite a key the import sets (`byte_len`, `foreign_game_file`,
`pack_id`, `pack_status`, `seed_signed`, `sha256`, `source_tool`, `third_party_mod_file`, or any
`pack_` name on a row), or its value is a table.

### Skips, duplicates and conflicts

- A row whose source is not on disk is skipped (`missing_source`).
- A row whose source has no `.meta` yet is skipped (`missing_meta`): import or cook the source first,
  or write the sidecars with `assets.ids_import`. The import never creates a `.meta`.
- Rows that name one source (the same path in any letter case, in one record or across records)
  are listed in `duplicates` as `{ path, records, identical }`. Identical rows migrate once; rows
  that differ are a conflict.
- A `.meta` that already carries the migrated block is `kept` and not rewritten, so a second run
  writes nothing and leaves the same bytes.
- **All or nothing.** The call fails with `provenance_import_conflict` and writes nothing when any
  row conflicts; `data.conflicts` lists `{ path, record, kind, detail }` and `data.duplicates` the
  duplicates. The writes are staged: every new `.meta` is written to a temporary file beside its
  original first (a failed write removes them and changes nothing), then each is renamed over its
  original; a `.meta` that changed in the meantime, or a failed rename, puts back the sidecars
  already replaced (`applied: false`).
- **Paths stay under `assets/`.** The source and its `.meta` must resolve under `assets/`, symbolic
  links followed. A row (or `under`) that leads out through a link is `provenance_import_invalid`,
  and nothing is read or written outside. Conflict kinds:

| kind | meaning |
|---|---|
| `hash_mismatch` | the source's SHA-256 is not the recorded one |
| `byte_len_mismatch` | the source's length is not the recorded one |
| `meta_unreadable` | the `.meta` does not read or parse |
| `provenance_differs` | the `.meta` already has another provenance block (remove it to import, or stop importing the row) |
| `duplicate_differs` | two or more rows for one source differ |
| `source_unreadable` | the source exists but cannot be read |
| `meta_changed` | the `.meta` changed while the import ran (run it again), or it is the same file as another row's `.meta` (a link), so writing one would change the other |
| `meta_link` | the `.meta` is a symbolic link; the import neither writes through a link nor replaces it (give the source its own `.meta`) |

## Error codes

| code | meaning |
|---|---|
| `provenance_import_conflict` | rows conflict (see the kinds above); nothing was written (`data.conflicts`, `data.duplicates`). |
| `provenance_import_invalid` | a record does not parse, names a schema above 3, has an unsafe path (empty, absolute, `..`, `\`, a `.meta` name, a Windows device name such as `NUL` or `COM1.obj`, a segment of only dots and spaces such as `.. ` or `...`, a segment ending in a dot or a space) or a hash that is not 64 hex digits, misses or mistypes a field, or carries a field that cannot be kept; or `under` is unsafe; or a source or its `.meta` resolves outside `assets/` through a symbolic link (nothing is written). |
| `provenance_import_unreadable` | a record file or folder does not read (missing, larger than 16 MiB, not UTF-8, a FIFO or device, or a folder entry link that dangles or leaves the folder). |
| `provenance_import_write_failed` | a `.meta` could not be written. |
| `path_outside_project` | a `records` entry leads outside the project (`F0002`, the shared request-path rule). |

A missing or empty `records`, or a parameter of the wrong type, is the universal `F0002`.

## Verify

```text
assets.provenance_import { records: ["provenance"], dry_run: true }   # what would be written
assets.provenance_import { records: ["provenance"] }                  # applied: true
assets.provenance_import { records: ["provenance"] }                  # written: [], kept: [...]
editor.asset_provenance { asset: "voice/intro.ogg" }                  # the block, extensions included
```

`cargo test -p regia_asset --test provenance_import` holds the acceptance tests
(`voice_row_survives_migration`, `unknown_fields_are_kept_and_unkeepable_ones_refused`,
`duplicate_row_is_reported`, `migration_is_idempotent`).
