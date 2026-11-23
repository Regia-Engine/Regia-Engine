# Player data: saves, settings and other files on the player's machine

A shipped game writes a few files on the player's machine and reads them back on the next launch.
Regia Engine's rule (ADR-3102): **a player's own files never cost them their progress or crash the
game.** Every write is atomic (a temp file, synced, then a rename), so a crash or power loss leaves
the old file or the new one, never half of each. On the read side, a damaged file is never written
over while it still holds the player's data. Saves keep one previous good generation, and a damaged
settings or input file is moved aside.

`user://` is `<data dir>/RegiaEngine/<exe name>/`: `%APPDATA%\RegiaEngine\<exe>\` on Windows,
`$XDG_DATA_HOME/RegiaEngine/<exe>/` on Linux, and the app's internal data directory on Android
(`regia_platform::paths::user_dir`). `cache://` is the matching cache directory.

## What the runtime writes

| File | Written by | Read back by | Format | If it is damaged today |
|---|---|---|---|---|
| `user://saves/<slot>.save` | `SaveGame::save`, the save flow (`FileSaveStorage`) | `SaveGame::load`, `save_flow::load_from_store`, slot lists | `FRTS` container, save format v1, CRC-32 footer (`docs/formats/save-format.md`) | The previous copy loads (below) |
| `user://saves/<slot>.prev` | Every save (the old file becomes it once the new one is in place, ADR-5573) | A load whose `.save` is damaged or missing | Same as `.save` | Only read as a fallback; a damaged one is reported |
| `user://settings.ron` | `SettingsPlugin` (on a change, rate-limited) | `SettingsPlugin` at start-up | RON, no `format_version` (fields default) | Moved to `settings.ron.bad-<n>`, defaults load |
| `user://input_overrides.ron` | A rebind (`InputRebinder`) | `InputPlugin` at start-up | RON, `format_version` 2 (v1 migrates) | Moved to `input_overrides.ron.bad-<n>`, project bindings load |
| `user://input_profiles/<name>.ron` | `regia_input::save_profile` | `regia_input::load_profile` (the player picks it) | Same as `input_overrides.ron` | `RebindError::ProfileCorrupt`; the file is kept |
| `user://known_hosts.ron` | `NetLink` (first-use pin), `net.trust` / `net.forget` | `NetLink::connect`, `net.*` | RON, `format_version` 1, mode 0600 | Connections through the store are refused (`net_known_hosts_unreadable`), the file is kept |
| `user://saves/<slot>.sav` | `regia_steam::LocalSaveBackend` (a game that picks it) | The same backend | The save container | The save container's CRC check refuses it; no previous copy |
| Steam Remote Storage `saves/<slot>.frts` | `SteamRemoteStorage` (a game that installs it) | The same | The save container | Steam's own conflict handling; no previous copy |
| `cache://pipelines/pipelines-<backend>-<vendor>-<device>.fpc` | The renderer | The renderer at start-up | `FPCH` v1 with a hash | Discarded with a warning, rebuilt |
| `cache://pak-verified.txt` | Pak verification | Pak verification | Lines keyed by build id | Paks are hashed again, the record rewritten |
| `<local data>/content/content-state.ron` and chunk files | The content manager (downloads) | The content manager | RON v1; chunk hashes from the manifest | Chunks are hashed again; a bad `.part` is deleted |
| `user://logs/<date>.log` | The log sink | The report outbox when the game turns it on (opt-in upload of the new tail, scrubbed; ADR-8402); old logs are pruned | Text | Not read back |
| `user://logs/crash-<time>.txt` | The crash handler | The crash reporter (opt-in upload), `diagnostics.crash_reports` | Text | Sent as bytes |
| `<feedback dir>/feedback-*.zip`, `announced.txt` | The feedback system | The feedback system (names only) | Zip; one name per line | `announced.txt` garbage re-announces crashes (see [Feedback](feedback.md)) |
| Android `crash/pending.bin` → `<time>.crash.bin` | The native crash handler | The next launch | Binary with a checksum | Deleted and reported as rejected |
| `user://logs/outbox.ron` | The report outbox (`CrashReporter::outbox`) | The report outbox, `report.outbox_status` | RON, `version` 1, written atomically | Started over with a warning: the logs are sent again from byte 0 (see [Report outbox](../agents/report-outbox.md)) |

Dev builds also write agent discovery files (`.regia/agent/endpoint.json`, `token`, `audit.jsonl`
in the project, or `user://agent/`); shipping builds do not. The editor's own files
(`editor_prefs.ron`, `editor_keymap.ron`, `editor_layout.ron`, `recent_projects.txt`,
`.regia/autosave/`) are editor data, not player data, and are not covered here.
`assets/conversations/conversation.ron` is project content read from the game's assets, not a
player file.

## Saves keep one previous good generation

A save to a file slot writes the new bytes to a temp file and syncs it. Then, if the current
`<slot>.save` passes its check (magic and CRC-32), it is renamed to `<slot>.prev`, replacing the
older one. Then the temp file is renamed to `<slot>.save`. A current file that fails the check is
not kept as `.prev`, so a damaged copy never replaces a good one.

What the player sees when a slot is loaded:

| The slot's `.save` is... | What loads | What the game is told |
|---|---|---|
| fine | the save | `SaveResult::copy == SaveCopy::Current` |
| truncated, garbage, empty, or has a bit flip (CRC fails), or its header version is damaged | `<slot>.prev`, the save before | `SaveCopy::Previous`; one warn line: `save slot <slot>: the current copy <path> is damaged (<reason>); loaded the previous copy <path>` |
| missing while `<slot>.prev` exists (the game stopped between the two renames) | `<slot>.prev` | `SaveCopy::Previous`; the warn line says `is missing` |
| damaged, and there is no good `.prev` | nothing; the world is unchanged | `SaveError::Corrupt` (the message says the previous copy is missing or damaged too) |
| from a newer engine (`format_version` higher than this build) | nothing | `SaveError::Deserialize("... newer than this engine supports ...")`; no fallback, the player rolled back to an older build |
| older, and the game registers no migration for it | nothing | `SaveError::Deserialize("... needs a migration ...")`; no fallback |
| beside a stray temp file (`<slot>.save.tmp.<pid>.<n>`, an interrupted write) | the save; the temp file is ignored | as for a fine save |

A load never writes, renames or deletes a file. `SaveOutcome::result` (the answer to a
`SaveRequest`) carries the same `SaveResult`, so a game can say "your last save was damaged; the
save before it was loaded". `LastSaveLoad` (a resource) holds the slot and copy of the last load.

A deliberate delete (`SaveRequest::Delete`, `save_flow::delete_from_store`, `SaveGame::delete`)
removes both copies. A slot whose only file is `.prev` is still listed and can be loaded or deleted.

A failed save (disk full, permission denied, a read-only folder) returns `SaveError::Io` and
leaves `.save` and `.prev` exactly as they were. Nothing retries it: a `SaveRequest` gets one
`SaveOutcome` with the error, and the autosave on suspend logs it once.

**Steam.** `SteamRemoteStorage` keeps no `.prev`, so no extra cloud file is uploaded. Steam
Auto-Cloud, set up from the `steamcloud.md` document Regia writes, syncs all of `user://` with
pattern `*`, so `.prev` and `.bad-<n>` files sync too. Count them in the file quota.

## Settings and input files are moved aside, never overwritten

| `settings.ron` is... | What loads | Files afterwards | Log |
|---|---|---|---|
| fine | the player's settings | unchanged | — |
| truncated, garbage, empty, not UTF-8, or the wrong shape | defaults (a first run) | `settings.ron.bad-1` holds the old bytes; the next change writes a new `settings.ron` | one warn line naming both files |
| from a newer game (top-level fields this build does not know, `format_version` among them) | the fields this build knows | `settings.ron` stays; a copy goes to `settings.ron.bad-<n>` (only once per content) | one warn line naming the field and the copy |
| unreadable (permission denied) or damaged where it cannot be moved (a read-only folder) | defaults | unchanged, never written over this session | one warn line; `SettingsPersistence::blocked()` says why |

| `input_overrides.ron` is... | What loads | Files afterwards |
|---|---|---|
| fine, or version 1 (no `format_version`) | the player's rebinds | unchanged |
| truncated, garbage, empty, not UTF-8, or a newer `format_version` | the project's bindings | `input_overrides.ron.bad-1` holds the old bytes; the next rebind saves normally |
| damaged where it cannot be moved | the project's bindings | unchanged; rebinds are not saved this session (`InputRebinder::persist_blocked`) |

At most five copies are kept per file (`.bad-1` to `.bad-5`). When all five exist, the oldest is
dropped and the rest shift down by one. A stray temp file from an interrupted write is ignored.

A settings write that fails (disk full, permission denied) keeps the old file and sends a
`SettingsWriteFailed { path, error }` message, once per attempt. Writes happen only after a change
and at most once per `SettingsPersistence::min_interval`, so nothing retries every frame. The
first failure of a streak logs one warn line. A failed rebind save is in
`InputRebinder::persist_error`.

## Trusted servers (`known_hosts.ron`)

The trust store fails closed (ADR-2416). A damaged or newer `known_hosts.ron` makes `NetLink`
refuse connections that rely on it (`net_known_hosts_unreadable`, `net_known_hosts_newer`), and
nothing writes over it. A game can offer a "reset trusted servers" action with
`KnownHosts::load_or_recover`. It moves the file to `known_hosts.ron.bad-<n>`, logs one warn line,
and returns an empty store, so the next connection to each server is first use again. The old pins
stay in the copy.

## Agents

- `save.list_slots` reports `has_previous` for each slot.
- `save.status` reports `slots_with_previous` and `last_load` (`{ "slot", "copy" }`, `copy` is
  `current` or `previous`; `null` before the first load).
- `save.load` and `save.quick_load` report `copy`.

See [Save flow (agents)](../agents/save-flow.md). To check the fallback in a running game: save
twice, damage `<slot>.save` (truncate it), call `save.load`, and expect `copy: "previous"` and the
first save's values.

## Testing

The sweeps live in `crates/regia_framework/tests/player_data.rs` (saves, settings),
`crates/regia_input/tests/player_data.rs` and `crates/regia_net/tests/player_data.rs`. Each case
asserts which data loaded, which files exist afterwards, and the warn line. The Windows read-only
file cases are `#[cfg(windows)]` tests in the framework file:

```text
cargo test -p regia_framework --test player_data windows_
```
