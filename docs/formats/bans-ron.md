# `bans.ron` (BanList, format_version 2)

The dedicated or listen server's ban list (spec 77 § 7.3, ADR-1663), next to the server binary (`regia package
--server` writes an empty one). Read and written by `regia_net::session::bans::BanStore`; edited through the console
(`ban`, `unban`), the agent (`server.ban`, `server.unban`) or `NetSession::ban`.

```ron
BanList(
    format_version: 2,
    entries: [
        (id: 1, kind: PlatformId, value: "steam:76561190000000000", until: None, reason: "cheating", added: 1793000000),
        (id: 2, kind: Address, value: "203.0.113.7", until: Some(1793600000), reason: "spam", added: 1793000000),
        (id: 3, kind: Address, value: "198.51.100.0/24", until: None, reason: "botnet", added: 1793000000),
        (id: 4, kind: Address, value: "2001:db8:1:2::dead:1/128", until: None, reason: "one bot", added: 1793000000),
    ],
)
```

| Field | Meaning |
|---|---|
| `id` | Entry id, unique in the file (new entries take the highest id + 1) |
| `kind` | `PlatformId` (the session authenticator's platform id), `Address` (an IPv4 address, an IPv6 address or CIDR range `addr/prefix`, RFC 4632; IPv4-mapped IPv6 peers match IPv4 entries) or `Name` (exact player name after trimming; the weakest kind, logged as such) |
| `value` | The platform id, address or range, or name. **Address, v2:** a bare IPv4 address bans that address; a bare IPv6 address bans its whole /64 (a subscriber rotates addresses inside it for free, ADR-3283); write `addr/128` to ban exactly one IPv6 address. Any other prefix bans the range it names. Engine-written block bans spell the prefix out (`203.0.113.7`, `2001:db8:1:2::dead:1/64`) |
| `until` | Expiry as a Unix time in seconds, or `None` for permanent. Expired entries never match and are dropped on the next save |
| `reason` | Text sent to the banned player with the `banned` refusal (with the expiry) |
| `added` | When the entry was added, Unix seconds |

The join check (spec 77 § 4 step 5) runs after the password and the authenticator (which supplies the platform id)
and before the reconnect token, so a ban added during a player's reconnect grace refuses the resume.

**Writes.** Every change rewrites the file atomically: `bans.ron.tmp` next to it, then a rename. A crash between the
two leaves the old file intact. A file that is unreadable at start keeps an empty in-memory list, logs an error and is
never overwritten until a reload (SIGHUP or `reload`) reads it successfully.

**Versioning.** `format_version: 2` since ADR-4936. **v1 → v2:** a bare IPv6 `Address` entry meant one exact
address under v1; the migration (loader and `regia upgrade-project` step) rewrites each one to its `/128` form,
preserving that meaning — in v2 a bare IPv6 entry is a /64 block ban, so an un-migrated v1 file would otherwise
silently widen every such ban. v1 files are migrated in memory on load and become v2 on the next save; files
with a newer version are refused. Registered in `cargo xtask check-formats` and `regia upgrade-project` since
ADR-2316 (spec 12 § 4a): any shape change, an added field with a serde default included, bumps `format_version`
and ships a migrator (a version rewrite when older files still read).
