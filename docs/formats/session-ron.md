# `.session.ron` (session harness file, format_version 2)

Owner: `regia_cli::session` (spec 41 § 2.8; v2 by ADR-2137). Names the roles of a multi-process playthrough (one game,
or a server plus clients, optionally one device instance) for `regia test session <file>`. Read by `Session::parse`.

```ron
(
    format_version: 2,
    build: Cargo(package: "arena", profile: "dev"),
    roles: [
        (name: "server", kind: Server, args: ["--port", "{port}"]),
        (name: "client", kind: Client, count: 2, args: ["--connect", "{server}"], render: true),
        (name: "phone", kind: Client, remote: Some("attached")),
    ],
    tests: (filter: ["net_*"], tags: ["smoke"]),
    script: [(role: "client[0]", call: "functest.run", params: Some({"name": "join"}))],
    limit_frames: 600,
    max_restarts: 1,
    allow_errors: ["audio device"],
    keep: (screenshots_on_failure: true, logs: false),
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1 or 2. A newer version is refused, so an older harness never starts a device role as a local process |
| `build` | `Cargo(package, profile: "dev")`, `Packaged(path)` (relative to the file) or `Remote(address)` (one running device instance) |
| `roles` | In start order; at least one. `name` matches `[a-z0-9_-]+` and is unique; `kind` is `Server`, `Client` or `Game` |
| role `count` | Instances, 1 to 64 (default 1) |
| role `args`, `env`, `driver`, `render` | Arguments (`{server}`, `{port}`, `{param:key}` expanded), extra environment, a registered `SessionDriver`, rendering on |
| role `remote` | v2: `127.0.0.1:<port>` or `attached`: a device instance in a local session; count 1, at most one role, not with a `Remote` build |
| `tests` | Functional tests each role runs: name globs `filter`, all-of `tags` |
| `script` | Harness-driven agent calls: `role` or `role[n]`, a protocol `call`, optional `params` |
| `limit_frames`, `max_restarts` (default 2), `allow_errors`, `keep` | Frame hint, restarts with `--recover`, tolerated log errors, what to keep |

**Versioning.** v1 → v2 only adds the optional role field `remote`: a v1 file reads unchanged, and
`regia upgrade-project --write` rewrites its `format_version` (comments and layout stay). Registered in
`cargo xtask check-formats` (`session`, ADR-2316) through an automation-layer edge from `xtask` to the `regia_cli`
library; the fuzz test is `crates/regia_cli/tests/fuzz.rs`.
