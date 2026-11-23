# `known_hosts.ron` (format_version 1)

A player's trusted server keys (ADR-2416): the trust-on-first-use store of a `regia_net` client. RON, in the app's user
directory (`user://known_hosts.ron`) when the game sets `LinkOptions::known_hosts` to it. Machine-local, not content: it is
not cooked and not in a pak. Its shape is registered in `cargo xtask check-formats` (`known-hosts`) and `regia
upgrade-project`, so a field rename fails the gate.

```ron
(
    format_version: 1,
    hosts: {
        "203.0.113.7:7777": (
            key: "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a",
            source: FirstUse,
        ),
        "lobby:night-raid": (
            key: "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
            source: Manual,
        ),
    },
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. A newer version is refused (`net_known_hosts_newer`) and the file is left as it is |
| `hosts` | Host id to entry, sorted by id. A host id is the server address or the game's stable name (`LinkOptions::host_id`), trimmed and lowercased, at most 256 bytes, no control characters |
| `key` | The server's X25519 public key, 64 hex digits: the one `net.crypto` reports as `server_key` |
| `source` | `FirstUse` (recorded by the first connection) or `Manual` (`net.trust`, or a game's own UI) |

Limits: at most 4096 hosts and 4 MiB. A file that fails to parse, lists one host twice with two keys, or breaks a limit is
refused with `net_known_hosts_unreadable`; a client then does not connect through the store, and nothing overwrites the file.
Fix or delete it by hand.

Writes are atomic (a temporary file, then a rename). On Unix the file is written with mode 0600: it decides whom the player
trusts, so other users must not change it. On Windows it is created with a DACL that grants the player's account, `SYSTEM`
and `Administrators` only (ADR-4828).

No migration exists yet; a future version bump adds one with the bump.
