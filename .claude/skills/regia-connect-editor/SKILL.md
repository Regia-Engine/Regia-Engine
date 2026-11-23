---
name: regia-connect-editor
description: Connect this assistant to a running Regia editor or game over MCP (co-pilot or assistant-builds mode).
---

# Connect to the Regia editor

Runs the modes 2 and 3 part of `AGENTS.md` ("Setting Regia up", the paragraph on the editor connection, and
"Working modes"). Keep to the "Rules" section of `AGENTS.md`. The server and its tools are described in
`book/user-guide/agent-protocol.md`; the HTTP transport in `book/agents/mcp-http.md`; the windowless editor in
`book/agents/headless-editor.md`.

## 1. Mode and client

- The mode comes from **(Question 4 of 6 · mode)**: co-pilot (2) or assistant builds (3). If the user asked for
  this skill directly ("connect to my editor"), mode 2 unless they say otherwise.
- **(Question 4b · client)** Only when you cannot tell which assistant app you run in. Inside Claude Code it is
  Claude Code.

## 2. Consent and write

**(Consent · connect)** Tell the user the exact file first. For Claude Code it is `<project>/.mcp.json`: add only
a `regia` entry in the shape of `dist/mcp/regia.mcp.json.example`, with the absolute path of `regia` (your
environment may not have the user's `PATH`) and the absolute project path; keep any other servers in that file as
they are. For mode 3, add `--headless` to `args` when the user does not want to keep the editor open: the server
then starts a windowless editor when none answers. Any other client: give the user the same command line for its
MCP settings,

```sh
/absolute/path/to/regia mcp --project /absolute/path/to/my_game
```

or, for a client that cannot start processes, `regia mcp --project DIR --http` (the URL and bearer token are
written to `<project>/.regia/agent/mcp_http.json`). Write nothing outside the project.

## 3. Hand off

A client reads the project's MCP file when a session starts in that folder, and this session did not. Say,
plainly: "I wrote the editor connection into your project. Start a new session of your assistant in `<project>`
and approve the `regia` server when it asks." Then stop. If your client can add a server to the running session,
you may do that instead.

## 4. In the new session

1. Make sure the editor runs: `regia editor` in the project (or the `--headless` entry starts one).
2. Through the `regia` MCP server: call `agent.hello`, then take a screenshot of the editor viewport and show it.
3. Mode 2: "What would you like to change?" Mode 3: "What game should we make?" and a short list of decisions.
   In mode 3 keep a short decision log in the game's `docs/decisions.md`.

Every edit you make through the protocol lands on the editor's undo stack and in the Agent Changes panel, where
the user can see and revert it. What the protocol may change is set by the permission profiles in the project's
`.regia/agent.toml`. Goldens stay the user's: propose a new image, the user accepts it with
`regia golden accept <id>` at a terminal.

## 5. Disconnect or switch

- Back to mode 1: delete the `regia` entry from the project's MCP file.
- Switch between 2 and 3: nothing to rewrite; only how much you do on your own changes (and `--headless`).
- MCP call fails or the server is not listed: open `regia-troubleshoot`.
