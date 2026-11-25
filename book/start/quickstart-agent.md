# Quickstart for an agent

<!-- index: agent.hello -->
<!-- index: MCP -->

Connect to a project and make one call. The tools have to be on your `PATH`
([Quickstart for a person](quickstart-human.md), [Installing Regia Engine](install.md)). The longer
checked loop (a first-person project, a condition, a screenshot, quit) is
[Checking your game with an agent](first-agent-check.md). The protocol itself is
[Agent protocol and MCP](../user-guide/agent-protocol.md).

A running app writes its endpoint and a per-run token under `<project>/.regia/agent/`.
`regia agent call` and `regia mcp` read those files. Nothing here sends a credential to a third
party.

## Stdio MCP

Start the game (`regia run` in the project) or the editor, and leave it running. In another
terminal:

```console
$ regia mcp --project my_game --attach
```

`regia mcp` speaks MCP on standard input and output: one tool per protocol method, plus `regia://`
resources. `--attach` uses the app that is already running.

When no app is running, a headless editor (no window, offscreen views) is enough for scene edits:

```console
$ regia editor --headless --project my_game
$ regia mcp --headless --project my_game
```

`regia editor --headless` prints a `{"event":"ready"}` line when it listens.
`regia mcp --headless` starts that editor if nothing answers, and stops it when the MCP process
ends. See [Headless editor](../agents/headless-editor.md).

## HTTP

A client that cannot spawn a process uses the same tools over loopback HTTP:

```console
$ regia mcp --project my_game --http
$ regia mcp token --project my_game
$ regia mcp token --project my_game --url
```

The token command prints the bearer token, then the loopback URL. Read them that way. Do not parse
`.regia/agent/mcp_http.json` yourself: a project folder is not trusted. The worked HTTP session
(initialize, `protocolVersion` `2025-06-18`, the session header) is
[MCP over HTTP](../agents/mcp-http.md). Bind stays on `127.0.0.1` unless you pass `--allow-remote`.

## One worked call

With `regia run` or the editor still running, from a second terminal in the project directory:

```console
$ regia agent call agent.hello
```

`agent.hello` is the connect check. The result names the protocol version (`protocol`), the
engine version, the app, the operating system, the process id, and the methods the app answers.
The project path and the token's permission list are `project.info`. A line that says it cannot
connect means the app is not listening yet:
wait and run the same command again.

The same call is an MCP tool once `regia mcp` is attached. The tool name is the method's MCP name
from [Agent methods](../reference/agent-methods.md).

## Finished check

`agent.hello` returns a result and does not say it cannot connect. If the code in
`error.data.code` is set, the meaning and the fix are on
[Errors](../agents/protocol.md#errors) and in the method's row of
[Agent methods](../reference/agent-methods.md). `errors.explain` returns the long form. Do not
invent a code: the schema is the list.

## Where next

- [Checking your game with an agent](first-agent-check.md): `play.wait_until`, `play.expect`,
  `seeing.screenshot`, `quit.request`.
- [Safety](../agents/safety.md): which methods mutate, and `.regia/agent.toml`.
- [Feature catalog](../manual/catalog.md) before calling a method for a feature this version
  marks partial.
