# Level switch with cancel

A game that loads its next level on a worker (a lobby preloading the picked map, a menu that
switches maps) must stop the old load when the player picks another map, and free what the old
load and the old map built before the next load fills memory. If it only forgets the old load (drops
the receiving end of the worker's channel), the old load runs to its end beside the new one and the
old map is freed while the new one loads: the process holds two maps at once. One DO27 rehearsal
went from 4.7 GB to 11.3 GB of RSS in 3 s that way.

The level switch keeps loads in a line (ADR-8027):

- A switch cancels the load in flight (its cancel token reads cancelled), hands it to a *retire
  worker* that waits for it to stop and frees what it built, and retires the current value the
  same way.
- The next load waits until no retire worker is running, then runs. A load switched away from
  before its turn never runs.
- Frees run on retire workers, never on a frame.

So at most one map is being built or freed at a time, as long as each load checks its token between
steps.

## Engine API

- `regia_core::load_switch::LoadSwitch<T>`: the generic switch, without ECS (DO27's shell and any
  host that loads its own banks). `switch_to(name, |token| ...)`, `cancel()`, `unload()`,
  `retire(value)`, `poll()` / `wait(timeout)` (an event: `Loaded { name }` or
  `Failed { name, error }`), `current()`, `take_current()`, `loading()`, `retiring()`,
  `wait_retired(timeout)`, `stats()`. The value cleans up in `Drop`; the switch decides where and
  when that drop runs. A load function calls `token.check()?` (or reads `token.is_cancelled()`)
  between steps.
- `regia_framework::level_switch::LevelSwitch` (resource, `LevelSwitchPlugin`, in the `regia`
  default plugins): `request(path)` reads and parses a level source on a worker; when it lands,
  `level_switch_poll` (`PreUpdate`) fills the `LevelLibrary` (the definitions the level binds and
  the levels it places), opens it with `LoadMode::Replace`, and retires the replaced level's parsed
  data (it leaves the library). `cancel()` stops the read and keeps the open level. A host with a
  bank of its own (CPU mesh copies in a `HostMeshStore`, see [Load memory](load-memory.md))
  passes the old bank to `LevelSwitch::retire` when the switch lands, so the next read waits for
  that free too.

## Agent methods

| Method | Params | Answers |
|---|---|---|
| `level.switch` | `path` (string, project-relative `.level.ron` or `.level.d`) **or** `cancel` (boolean `true`) | the `level.switch_status` answer |
| `level.switch_status` | none | `{ loading, current, last, retiring, requests, stats }` |

```json
{"jsonrpc":"2.0","id":1,"method":"level.switch","params":{"path":"levels/canyon.level.ron"}}
{"jsonrpc":"2.0","id":2,"method":"level.switch","params":{"path":"levels/bore.level.ron"}}
{"jsonrpc":"2.0","id":3,"method":"level.switch_status"}
→ { "loading": null, "current": "asset:…", "retiring": 0, "requests": 2,
    "last": { "outcome": "opened", "path": "…/levels/bore.level.ron", "level": "asset:…", "replaced": null },
    "stats": { "loads_started": 2, "loads_skipped": 0, "loads_cancelled": 1, "loads_landed": 1,
               "loads_failed": 0, "retires": 0, "max_wait_ms": 0, "last_wait_ms": 0 } }
{"jsonrpc":"2.0","id":4,"method":"level.switch","params":{"cancel":true}}
```

- `loading`: the source being read (`null` when idle).
- `current`: the open level (`asset:<guid>`), `null` without one.
- `last`: how the last switch ended: `outcome` `opened` (with `level` and `replaced`), `failed`
  (with `error`: the reader's reason and text, for example `level_not_found: …`, or the open's
  error) or `cancelled`. The open level stays on `failed` and `cancelled`.
- `retiring`: retire workers still freeing.
- `stats`: `loads_started` (reads that ran), `loads_skipped` (switched away from before their turn),
  `loads_cancelled`, `loads_landed`, `loads_failed`, `retires` (values and cancelled reads handed to a
  retire worker), `max_wait_ms` and `last_wait_ms` (the longest and the latest wait of a read for a
  free; measurements, not budgets).

Wait for a switch with `wait_until` on `level.switch_status` until `loading` is `null` and
`last.outcome` is `opened`.

Errors:

- `level_switch_bad_params` (INVALID_PARAMS): neither `path` nor `cancel`, both, or a `cancel`
  that is not a boolean.
- `F0003`: no `LevelSwitch` (the plugin is not installed) or no `LevelManager` (no scene plugin).
- `F0002` with `data.reason` `level_not_found` (no level at the path) or `path_outside_project`
  (absolute, `..` or a symlink out of the project), as in [Levels](levels-layers-instances.md).

## Tests

- `cargo test -p regia_core --test load_switch_rss`: `switch_cancels_old_load_rss_bounded` plays a
  switch mid-load with 512 MiB fake maps, the old way (the control: two maps live at once, the
  process peak grows by about two maps) and through `LoadSwitch` (one map; the peak grows by at most
  one map plus one chunk and 64 MiB). The process peak (`VmHWM`) is checked on Linux and Android.
- `cargo test -p regia_core --lib load_switch`: cancel mid-load, a load skipped before its turn, a
  load that ignores its token, twenty switches, failures and panics, unload and retire.
- `cargo test -p regia_framework --lib level_switch` and
  `cargo test -p regia_agent --lib switch_opens_cancels_and_reports`.
