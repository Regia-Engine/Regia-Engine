# Net assets and derived messages (agents)

Two pieces of `regia_net` that remove hand-written net code (specs 70 § 6.1 and 71 § 3, ADR-2415): the
`#[derive(NetMessage)]` macro for net message types, and replicated `Handle<T>` fields, which travel as the asset's
stable key. Guide topic `net-assets`. The message model itself is on [Net model (agents)](net-model.md), the
replicated entities on [Net replication (agents)](net-replication.md).

## `#[derive(NetMessage)]`

<!-- ignore-reason: the derive needs an app with `NetPlugin` and a registered message to mean anything; shown for its attributes -->
```rust,ignore
use regia_net::{NetMessage, ValidateContext, Verdict};

#[derive(Clone, Debug, NetMessage)]
#[net(direction = "to_server", channel = "unreliable", rate = "10/60", validate = Self::check)]
struct Fire {
    target: Entity,
    #[replicate(quantize(min = 0.0, max = 1.0, bits = 8))]
    power: f32,
}

impl Fire {
    fn check(&self, _cx: &ValidateContext) -> Verdict {
        if self.power > 0.0 { Verdict::Accept } else { Verdict::Reject("no power") }
    }
}
```

The derive writes the `NetMessage` impl, the marker trait of its direction (`ToServerMessage`, `ToOwnerMessage` or
`BroadcastMessage`) and the codec: the fields travel in declaration order through the same field codecs as
`#[derive(Replicate)]`, with the same `#[replicate(..)]` field attributes, so the bytes are identical to a hand-written
impl that encodes the fields in order (a test compares them field type by field type).

| Key | Meaning |
|---|---|
| `direction` | `"to_server"`, `"to_owner"` or `"broadcast"` (required). |
| `channel` | `"reliable_ordered"` (default), `"reliable_unordered"`, `"unreliable"` or `"unreliable_sequenced"`. |
| `rate` | `"count/per_ticks"`, the per-peer flood limit (default: the session's). |
| `always_admit`, `align_to_view`, `skip_predicting_owner` | Flags (bare, or `= true` / `= false`). |
| `id` | The stable message id, for example `"game.fire"`. Default: the module path plus the type name. |
| `version` | The schema version, a `u32` of at least 1 (default 1). |
| `validate` | A `fn(&Self, &ValidateContext) -> Verdict` (default: accept). |
| `codec` | `"manual"` when the type implements `ReplicateCodec` itself (for example with `#[derive(Replicate)]`). |

The id names the message in the registry, in the protocol hash and in `net.messages {type}`; give one with `id` so a
rename or a move of the type does not change the protocol. The version is part of the protocol hash, so a client and
a server built with different versions of a message cannot join each other. `stable_id` is a 64-bit hash of the id
(the first 8 bytes of BLAKE3), the same on every build and platform.

`net.messages` rows carry `version` and `stable_id` (hex):

```json
{ "method": "net.messages", "params": { "type": "game.fire" } }
→ { "types": [{ "type": "game.fire", "direction": "to_server", "channel": "unreliable", "sent": 0, "received": 3,
    "delivered": 3, "rejected": 0, "rate_limited": 0, "not_owner": 0, "unresolved": 0, "dropped_overflow": 0,
    "late": 0, "bytes_sent": 0, "bytes_received": 9, "version": 1, "stable_id": "3f0c9b41d27a65e8" }] }
```

## Replicated asset handles

A replicated component may hold `Handle<T>` fields (also inside `Option` and `Vec`). A handle is a slot in one
process's asset storage, so it never travels as is: the sender's **asset-server context** turns it into the asset's
stable key, its guid (128 bits) or its project path, and each receiver turns the key back into a handle of its own
asset storage, loading the asset when its resolver has a loader.

<!-- ignore-reason: a fragment of a host's setup: `world`, `asset_server` and `id` come from the surrounding system -->
```rust,ignore
use std::sync::Arc;
use regia_net::assets::{NetAssetTable, NetAssets};

let table = Arc::new(NetAssetTable::new());
world.insert_resource(NetAssets::new(table.clone()));
// Whenever the game loads an asset it may replicate:
let mesh = asset_server.load_mesh(id)?;
table.insert_id(&mesh, id); // or table.insert_path(&mesh, "assets/props/crate.glb")
```

`NetAssetTable::with_loader(f)` resolves a key it does not hold by calling `f(asset type, key)`, which loads through the
receiver's asset server and answers the new handle's slot; the table remembers it. Any type implementing
`regia_net::codec::AssetResolver` can stand in for the table.

What happens to a handle:

| Case | On the wire | On the receiver |
|---|---|---|
| Invalid handle | none (1 bit) | `Handle::INVALID` |
| The sender's resolver has its key | guid or path | the receiver's handle for that key |
| The sender's resolver does not know it | none, counted as `unmapped` | `Handle::INVALID` |
| The receiver's resolver does not know the key | guid or path | `Handle::INVALID`, listed with its code |

A key that does not resolve is never a decode error: the rest of the component still applies, the handle stays
unset, `net.asset_missing` is logged once per key (a warning with the code) and `net.assets` lists the key.

| Code | Why the handle stayed unset |
|---|---|
| `asset_unknown` | The receiver's resolver knows no asset with that guid or path. |
| `asset_wrong_type` | The key names an asset of another type than the field's `Handle<T>`. |
| `asset_load_failed` | The resolver's loader knows the asset but could not load it. |
| `asset_no_resolver` | The receiver has no asset-server context (`NetAssets` without a resolver). |

## Methods

| Method | Mode | Params | Result |
|---|---|---|---|
| `net.assets` | read | `{ limit? }` (1-1024, default 100) | `{ resolver, unmapped, missing: [{ key, code, count, first_tick }], missing_total, missing_overflow }` |

```json
{ "method": "net.assets", "params": {} }
→ { "resolver": true, "unmapped": 0, "missing": [{ "key": "00000000000000000000000000000abc",
    "code": "asset_unknown", "count": 1, "first_tick": 42 }], "missing_total": 1, "missing_overflow": 0 }
```

`key` is the guid as 32 hex digits, or `path:` plus the path. At most 1,024 distinct keys are listed; later ones only
count in `missing_overflow`. To check a level replicates its assets, run a loopback session, step a few ticks and
read `net.assets` on the client: `missing_total` 0 and `unmapped` 0 on the server.

## `net.set_owner` is a development-build method

`net.set_owner` (on [Net model (agents)](net-model.md)) changes ownership from outside the game's rules, so it
exists in development builds only (spec 70 § 9). A shipping build (no debug assertions) answers `F0002` with
`data.reason` `net_dev_only` before it reads its parameters.

## Errors

| Reason | When |
|---|---|
| `net_not_running` | `net.assets` on an app with no net runtime. |
| `net_dev_only` | `net.set_owner` in a shipping build. |

`net.assets` with a `limit` outside 1-1024 is an invalid-params error.

## Handles in net messages

A `Handle<T>` field inside a net message (hand-written or `#[derive(NetMessage)]`) travels exactly like one in a
component (ADR-2665): `NetSender` encodes it through the world's `NetAssets` context and each receiver decodes it through
its own. A key that does not resolve leaves the field unset, the message is still delivered, and `net.assets` lists the
key with its code under the message's id (`asset_unknown`, `asset_wrong_type`, `asset_load_failed`,
`asset_no_resolver`). A handle the sender's resolver does not know travels as none and counts as `unmapped`.

On a server, a message from a client never starts a load, even with `NetAssetTable::with_loader`: a key resolves only to
an asset the server already holds, and any other is `asset_unknown`. A client chooses those bytes, and must not choose
what the server reads from disk. A custom `AssetResolver` that loads should implement `resolve_loaded` the same way (its
default calls `resolve`).

## Handles the asset server loaded

With the `net` runtime, every mesh, image, texture and material the world's `regia_asset::AssetServer` loaded is
registered by its guid in a `NetAssetServerTable` (ADR-2665), at the start of each frame's receive and send. When the
world has no `NetAssets` resolver, that table becomes it, so a `Handle<CpuMesh>` (or `CookedImage`,
`CookedTexture`, `CookedStandardMaterial`) loaded through the asset server replicates with no registration by the game.
A resolver the game installed itself is left alone, and the server table is then not filled; a game that wants both
adds its own handles to `NetAssetServerTable::table()`. The receiver resolves a key only when its own asset server has
loaded that asset (load the level's assets on both sides); there is no on-demand load during a decode. The read API this
uses is `regia_asset::Assets::{id_of, slot_count, entries_from}` (handle to guid; `handle_for_id` and
`AssetServer::{id_for_path, path_for_id}` give the other directions).

A message encodes when it is sent, so a handle loaded in the same frame, after that frame's receive, is not registered yet:
it travels as none and counts as `unmapped`. Load before you send, send on the next frame, or call
`regia_net::assets::register_asset_server_handles` from an exclusive system first. A replicated component has no such
gap, since it encodes after the registration in the frame's send.

To check: run a loopback session with the asset server on both sides, step a few ticks and read `net.assets` on the
client (`missing_total` 0) and on the server (`unmapped` 0).

## Not yet covered

- Asset types outside the asset server's four slabs (audio, game types) still need the game to register them in a
  table; a handle loaded in the same frame a message carries it travels as none (it is registered at the next
  receive).
