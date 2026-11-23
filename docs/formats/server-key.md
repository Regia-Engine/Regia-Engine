# `server.key` (format_version 1)

A dedicated server's static key (ADR-1901). RON, created on first start at `[net] identity_file`, relative to the server directory.
Machine-local secret, not content: it is not cooked and not in a pak. Its shape is registered in `cargo xtask check-formats`
(`server-key`) and `regia upgrade-project` since ADR-2316, so a field rename fails the gate.

```ron
(
    format_version: 1,
    private_key: "<64 hex digits>",
    public_key: "<64 hex digits>",
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. A newer version is refused |
| `private_key` | X25519 private key |
| `public_key` | Its public key, the one clients pin. Must match `private_key`, or the file is refused |

On Unix the file is written with mode 0600, and a file other users can access stops the host with exit code 2. On Windows the
file is created with a DACL protected from inheritance that grants the server's account, `SYSTEM` and `Administrators` only
(ADR-4828, hardened per audit fixrev-15-k net2-platform H2/H3), and a DACL that grants any other principal access stops the
host the same way.

Keep the file across updates: a new key breaks every client's pin. If it leaks, delete it, restart, and give clients the new key.
No migration exists yet; a future version bump adds one with the bump.
