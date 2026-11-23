# Steam cloud save fault replay

The cloud save adapter uses `SaveBackend` errors directly and maps them to framework save outcomes.
The tests use `MockSteam`; they do not need a Steam account or SDK.

## Replay commands

From the workspace root:

```sh
cargo test -p regia_steam --test cloud_save_store
cargo test -p regia_steam --test cloud_save_game
```

The `cloud_save_store` harness installs `SteamRemoteStorage` through `SaveStore`. Its normal-path
replay uses these mock calls:

| Mock call | Effect | Expected observation |
|---|---|---|
| `MockSteam::shared()` | Start with cloud storage available. | `SaveBackend::save`, `load`, `delete`, and `slots` use `saves/<slot>.frts`. |
| `MockSteam::set_state(SteamState::Unavailable)` | Take the service offline between requests. | One `SaveOutcome` carries an error; the existing cloud bytes and other slots remain. |
| `MockSteam::set_state(SteamState::Ready)` | Restore the service and retry. | One successful outcome follows the failure; the unrelated slot remains unchanged. |
| `MockSteam::sync_remote_file(name, bytes, false)` | Add a remote listing entry, including malformed names. | `slots()` ignores names outside `saves/`, names without `.frts`, and the empty slot name. |

Its lane-local `ScriptedBackend` implements `SteamBackend` and arms one failure at a time with
`fail_next(CloudOp::{Write, Read, Delete, List}, message)`. A write failure with the message
`quota exceeded` models quota refusal before commit; read, delete and list failures use
`read refused`, `delete refused` and `list refused`. `go_unavailable_next(CloudOp::Write)` makes
the initial list succeed and the write fail with `SaveError::Unavailable`, simulating loss of
cloud availability during the request. Each injected failure is consumed by its matching
operation, so the next operation demonstrates recovery. `CloudOp::Exists` is available for the
delete preflight as well.

The exact error mapping is:

| Injected backend error | `SaveBackend` error | Framework error | `SaveOutcome.result` |
|---|---|---|---|
| Unavailable | `SaveError::Unavailable`, displayed as `Steam Remote Storage is unavailable` | `SaveError::Io(NotConnected, "saves/A.frts")` | `Err("save IO error at saves/A.frts: Steam Remote Storage is unavailable")` |
| Rejected with `quota exceeded` | `SaveError::Io(Other, "Steam rejected the operation: quota exceeded")` | `SaveError::Io(Other, "saves/A.frts")` | `Err("save IO error at saves/A.frts: Steam rejected the operation: quota exceeded")` |
| Read rejected with `read refused` | `SaveError::Io(Other, "Steam rejected the operation: read refused")` | `SaveError::Io(Other, "saves/A.frts")` | Not sent through `SaveFlow` in this replay |
| Delete rejected with `delete refused` | `SaveError::Io(Other, "Steam rejected the operation: delete refused")` | `SaveError::Io(Other, "saves/A.frts")` | Not sent through `SaveFlow` in this replay |
| List rejected with `list refused` | `SaveError::Io(Other, "Steam rejected the operation: list refused")` | `SaveError::Io(Other, "saves")` | Not sent through `SaveFlow` in this replay |

`SaveError::Unavailable` and `SaveError::InvalidSlot` framework mappings are also pinned by the
`cloud_store::errors_keep_their_meaning` unit test. `SaveOutcome` reports a failed request as one
`Err(String)` and the later retry as one `Ok(())`; the quota replay checks both exact outcome
count and message.

## Remote write guarantee

The adapter forwards one write call to the backend. It does not create a backup or promise rollback
if a remote implementation partially changes a file before returning an error. The adapter can
preserve the old value only when the backend honors its `cloud_write` contract that a rejected
write leaves prior bytes intact. The mock's unavailable-state failure occurs before mutation and
does not establish transactional behavior for other backends.
