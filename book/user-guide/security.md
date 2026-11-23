# Security

This page says what the engine defends against and what it trusts. It lists what was tested, not a promise that nothing is left: each line below has a test named in the full table, `docs/security/threat-model.md` in the repository, with the attacker, what they control, what is at stake, the code, and the risk that remains.

## What the engine trusts

- **You and your own processes.** Any process of your user can read the agent token and the project.
- **A project's files.** A project is code. `.regia/agent.toml` decides what an agent may do and where it may write, and project files name programs to run (`generators/*.toml`, `[editor] code_editor`, `[[tool]]`). Open a project, run its generators and point an agent at it only if you trust it. A clone you have not read deserves the care an unknown program does.
- **Dynamic extensions and the hot-reload game library.** They run in the host process with its rights. Their stamps catch a stale build, not a hostile one.
- **Third-party decoders and Lua 5.1.** They are fuzzed where the engine reaches them; their internals are not reviewed here.

## What is tested at each boundary

| Boundary | What was tested |
|---|---|
| Agent server | Loopback only, a bearer token on every connection, any `Origin` refused, connection and message limits, one deadline over the handshake. Each method checks its permission, and `read_only` denies every method not marked as a read. |
| MCP over HTTP | Loopback by default; another address needs `--allow-remote`. The bearer token is required on every bind. Foreign `Origin` and, on loopback, foreign `Host` are refused, the body is read after the token, and connections and sessions have limits. Under `--allow-remote` it speaks plain `http://`, so put it behind a tunnel or a TLS proxy. |
| Planted links in `.regia/agent/` | A link at the token, the endpoint, the audit log or their temp names is replaced, not followed; a linked agent folder is refused. A client reads the token only from a small regular file. On Windows these files take the project folder's ACL. |
| Project paths | One rule on every host, table-tested as pure logic because a level written on Linux gets opened on Windows: no drive or UNC form, `..`, stream, device name, or trailing dot or space. Links leading out are refused. An `output_roots` entry cannot be a whole filesystem. |
| Mods and Lua | A reviewed library list, source text only, an instruction budget and a memory cap a mod cannot catch, a budgeted pattern matcher, and `mods://` paths confined to the mount, with links and swapped files refused. Limits on files, bytes and depth; a pak's declared size is checked against the byte limit before it is decompressed. |
| Tools | No shell, an empty environment, a timeout and an output limit. A tool needs its grant. A tool has no OS isolation. |
| Network | No state and no curve work before a cookie, authenticated records, replays refused, per-source limits. A pinned key is checked before any secret leaves. A client with no pin takes the key it is given. |
| Content delivery | Plain `http://` to another machine needs an override, a chunk is hashed before it is installed, and manifests and bodies have limits. Manifests are not signed. `regia content serve` is a development tool: do not expose it. |
| Saves, replays and demos | `SaveGame::load` checks the version and the size and decodes before the world changes; replays and demos have size limits. A save is not authenticated, so a game must not treat one as proof of anything. |
| Paks and cooked assets | Header and range checks, per-block and per-entry output limits, and a ratio limit. The hash finds damage; it does not say who made the file. |
| Importers | The mesh importer refuses a glTF URI that leaves the project, and each format's tested limits are listed in the full table. Cooking unknown content is running a tool over it. |
| Crash upload | Off without an endpoint and consent, remote `http://` refused unless the game opts in, redirects not followed, size limits. The log lines in a report go through a scrubber, a list of patterns that reduces what leaves. The engine ships no collector. |
| DCC feed | Off by default, loopback unless `allow_remote`, a token, an operator prompt for unknown clients, frame limits, and texture paths confined to the project. No TLS. |
| `regia engine verify` | Each listed file's size and hash, links refused, and paths under the project rule. The manifest travels with the tree and is not signed, so this finds damage and partial replacement. It says nothing about who wrote the pair. |

## Reporting

Report a security problem privately to the project owner rather than in a public issue.

Agents: paths refused for these reasons answer `F0002` with `data.path_reason`; see [Safety](../agents/safety.md).
