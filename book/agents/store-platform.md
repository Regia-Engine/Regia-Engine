# Store-neutral platform

`regia_steam::store` supplies the `Platform` trait, `NonePlatform`, a borrowed
`SteamPlatform`, and `GogPlatform`. `Store::None` is the default. GOG is an
explicit unavailable stub: no Galaxy SDK, credentials, or network calls.

## Host setup

Add `StorePlugin::new(requested)` to publish the selection. For `Store::Steam`,
add the existing `SteamPlugin` with the game's app id and settings, finish plugins,
and keep updating the app so its callback pump runs. For None and GOG, omit
`SteamPlugin`. `StorePlugin` never starts a store client itself. This remains true
in a binary compiled with the `steamworks` feature.

<!-- ignore-reason: a host's startup lines around its own store decision (`requested`); the surrounding binary is the host's -->
```rust,ignore
use regia_steam::{Steam, SteamPlugin, AppId};
use regia_steam::store::{Store, StorePlugin, select};
let requested = Store::None; // The host's package/config decision.
let mut app = regia_app::App::new();
if requested == Store::Steam {
    app.add_plugins(SteamPlugin::new(AppId(480)));
}
app.add_plugins(StorePlugin::new(requested));
app.finish_plugins();
let mut steam = app.world_mut().get_resource_mut::<Steam>();
let platform = select(requested, steam.as_deref_mut());
platform.set_presence("status", Some("Playing"));
```

`select(requested, Option<&mut Steam>)` borrows the current resource and returns a
`Box<dyn Platform>` for the duration of that borrow. It never creates or clones a
native client. An absent/unavailable Steam resource and any GOG request select
None. A Steam adapter observes the backend's live state, including shutdown.
`Platform::shutdown` flushes Steam through the existing resource and is idempotent.
None and GOG calls do not stop a separately installed Steam plugin.

The trait exposes `store`, `describe`, `account_id`, `set_presence`,
`open_overlay`, `unlock_achievement`, `cloud_save_written`, and `shutdown`.
None calls are inert and achievement unlock returns false. Steam presence uses
existing key/value semantics (`None` removes a key); achievement ids must be in
the existing achievements configuration. The allowed overlay pages are `friends`,
`community`, `players`, `settings`, and `achievements`; other names are ignored
by the trait and rejected by `Steam::open_overlay`. The cloud notification is a
hook only: use the existing save backend or configured Auto-Cloud for actual
synchronization. It never reads or uploads the named file.

## `store.status`

Permission: `read`. Nonmutating; no parameters. Registered by `StorePlugin` on
`AgentExtension`, discoverable through the normal agent schema.

This request needs a host with `StorePlugin`. The static built-in example checker
does not load that plugin; the dispatcher tests in `store_neutral` verify the call.

```json,ignore
{"jsonrpc":"2.0","id":1,"method":"store.status","params":{}}
```

Result fields: `plugin`, `requested` and `active` (`none`, `steam`, `gog`),
`account_id` (store-prefixed string or null), `description` (log-safe, no player
name), and `reason`. Reasons are `none_requested`, `steam_ready`,
`steam_unavailable`, or `gog_sdk_unavailable`. These are status values, not RPC
errors. With no selection resource the registered handler returns
`{"plugin":false}`. No new RPC error codes are introduced.

A ready Steam request reports `active: "steam"`; shutting it down changes the
next status to `active: "none"`, `reason: "steam_unavailable"`, and a null id.
Reading status does not initialize Steam, write a file, flush stats, or mark the
Steam ECS resource changed.

## Identity and transport

`account_kind` classifies `steam:`, `gog:`, `local:`, case-insensitive `bot:`,
and otherwise `Hub`. `store_of` and `relay_allowed` retain the source's prefix
classification behavior. **They are routing hints, not identity validation or
authorization.** A transport must authenticate the peer independently.
`account_id_from_galaxy` and `galaxy_from_account_id` require canonical nonzero
64-bit decimal ids. `local_account_id` formats caller-provided 16 bytes;
`is_local_account_id` requires 32 lowercase hexadecimal digits. The host owns
entropy, persistence, package selection, and configuration. These helpers do
not access a clock, filesystem, network, or simulation state.

## Verification

`cargo test -p regia_steam --test store_neutral` covers standalone startup, SDK-free
fallback, existing-client isolation, forwarding, invalid overlay input, shutdown,
identity edge cases, dispatcher status, and extension book coverage. No account,
SDK, GPU, or golden image is required for the default-feature tests. Actual Steam
account/overlay proofs remain the owner-run [Steam real run](../user-guide/steam-real-run.md).
