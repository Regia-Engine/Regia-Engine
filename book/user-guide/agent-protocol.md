# Agent protocol

Regia Engine's defining feature: every capability a human has in the editor is available to an AI agent
through a typed, documented, testable API (`docs/specs/23-agent-native.md`). Agents are first-class
users.

## The idea

A dev build of any Regia Engine game runs a small server. An agent (or a script, or CI) connects and
inspects and drives the running app — query the world, edit entities, capture screenshots, play and
step deterministically, assert results, build and reload. Everything an agent authors is diffable
text (RON), and every agent action is observable, undoable and attributable.

## Driving it yourself

```console
$ regia agent call agent.hello --project my_game
$ regia agent call scene.query_entities '{"name":"Player*"}' --project my_game
```

`regia agent call <method> [json]` calls one method and prints the result. The app writes its
endpoint to `<project>/.regia/agent/endpoint.json` and a bearer token next to it.

## MCP

```console
$ regia mcp --project my_game --attach
```

`regia mcp` runs an MCP server over stdio (one MCP tool per protocol method, plus `regia://`
resources), so Claude Code and other MCP clients drive the game directly. It attaches to the
project's running app (a game or the editor). For CI and cloud agents, `regia mcp --headless
--project DIR` starts a **headless editor** when no app answers (no window, offscreen views), waits
until it listens, and takes it down again when the adapter ends, so nothing is left running; see
[Headless editor](../agents/headless-editor.md). You can also start one yourself with `regia editor
--headless --project DIR` (it prints a `{"event":"ready"}` line when it listens) and attach.

Golden images stay yours: an agent can run a golden's test and propose a new image with a reason, and
only you accept it, at a terminal, with `regia golden accept <id>`. See [Golden images](../agents/goldens.md).

### Connecting Claude Code

Register the server once per project, then start the game (or the editor) and ask the agent to
work with it:

```console
$ claude mcp add my-game -- regia mcp --project /path/to/my_game
```

The agent sees one tool per method (`query_entities`, `spawn`, `screenshot`, `step`, `expect`, …),
can read the agent guide through the `guide` tool, and acts with the permissions in
`.regia/agent.toml`. Other MCP clients take the same command line.

A client that cannot spawn a process connects over HTTP instead: `regia mcp --project my_game
--http` serves the same tools on a loopback port with a bearer token (see
[MCP over HTTP](../agents/mcp-http.md)). Long operations answer a job id that `job_status` polls
([Jobs and test runs](../agents/jobs.md)), and an agent can propose file changes for your review
in the editor's Agent Changes panel ([Staged review](../agents/staged-review.md)).

### A typical agent loop

1. **Discover**: `agent.hello` (project, protocol version, permissions), `agent.guide` (the
   agent pages of this book), `types.list` and `types.describe` (the reflected types).
2. **Look**: `scene.query_entities`, `scene.get_entity`, `seeing.screenshot`.
3. **Change**: `scene.spawn`, `scene.set_property` or a batch with `scene.apply_edits`
   (`dry_run` previews it); in the editor, `editor.*` methods that land on the undo stack.
4. **Verify**: `play.pause`, `play.inject_input`, `play.step`, then `play.expect` assertions and
   another screenshot; `seeing.compare_images` against a reference.
5. **Report**: `agent.changes` lists everything it changed, stamped with provenance, for a human
   to review.

## The surface

The protocol is described by one schema table (`agent.schema` returns it; the MCP tool list is
generated from it). Methods group into:

- **Discovery** — `agent.hello`, `types.describe`, `resources.list`, `errors.explain`, …
- **Scene** — `scene.query_entities`, `scene.get_entity`, `scene.set_property`, `scene.spawn`, …
- **Assets** — `assets.list`, `assets.describe`, `assets.load`, …
- **Seeing** — `seeing.screenshot`, `seeing.compare_images`, …
- **Play & test** — `play.step`, `play.inject_input`, `play.wait_until`, `play.expect`, …
- **Logs & events** — `logs.read`, `events.read`, …
- **Framework** — `scripts.*`, `camera.*`, `settings.*`, `sim.*`
- **Physics** — `physics.raycast`, `physics.overlap`, `physics.closest_point`
- **Gameplay** — `players.*`, `scene.possess`, `save.*`

Every method has parameters, a result shape, a mutating flag and a permission. The
[agent methods reference](../reference/agent-methods.md) lists all of them, generated from the
schema; the [agent guide](../agents/protocol.md) has example calls and results.

## Safety

The project gates the protocol with `.regia/agent.toml` permission profiles (`read`, `edit_scene`,
`assets`, `play`, …); every call lands in an audit log, and every agent-made change is stamped with
provenance and listed for review. Granting `build`/`play` amounts to code execution — grant them
only to agents you trust like a developer with a shell. See the
[agent guide's safety page](../agents/safety.md).

## Where the pages live

The agent guide — task-oriented pages an agent reads through the `agent.guide` method — is under
[`book/agents/`](../agents/protocol.md), one page per feature area. The
[coverage map](../COVERAGE.md) maps every method, feature, command and format to its page.
