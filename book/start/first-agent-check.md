# Checking your game with an agent

This page makes a First Person project, builds and runs it, then checks it from a second terminal
through the agent protocol: wait for the game to be ready, assert what is on screen, take a
screenshot and quit. It is the shortest complete loop an AI agent (or a CI script) runs against a
game, and it needs nothing beyond the tools from [Installing Regia Engine](install.md).

The commands on this page are the ones the engine's own test runs, in this order
(`crates/regia_cli/tests/getting_started_walkthrough.rs`), so they stay correct.

## 1. Create, build and run

```console
$ regia new "My Game" --template first_person
$ cd my_game
$ cargo build -p my_game_launcher
$ regia run
```

`regia new` writes the project into `my_game/` ([A tour of the template](../user-guide/first-person-template.md)).
`cargo build` compiles the engine and the game; the first build takes a few minutes, later builds
only compile your crates. `regia run` cooks the assets and starts the launcher in a window. It
keeps running until the game quits, so leave this terminal alone.

## 2. Check it from a second terminal

Open a second terminal in the `my_game` directory:

```console
$ regia agent call agent.hello
$ regia agent call play.wait_until '{"condition":{"entity":{"name":"Gate"},"component":"Door","path":"open","op":"eq","value":false},"timeout_ms":60000}'
$ regia agent call play.expect '{"assertions":[{"name":"gate shut","condition":{"entity":{"name":"Gate"},"component":"Door","path":"open","op":"eq","value":false}},{"name":"nothing in reach","condition":{"entity":{"name":"hud-prompt"},"component":"PromptLabel","path":"text","op":"eq","value":""}}]}'
$ regia agent call seeing.screenshot --out shot.png
$ regia agent call quit.request
```

What each one does, and what to look for in its output:

- **`agent.hello`** connects. The game writes its endpoint and a per-run token to `.regia/agent/` in the
  project, and `regia agent call` reads them. If it says it cannot connect to a running app, the game is
  still starting: run it again.
- **`play.wait_until`** blocks until the condition holds, checked once per frame, or until `timeout_ms`
  passes. Here it waits until the level has spawned its gate, closed. Look for `"satisfied": true`; on a
  timeout it is `false` and `detail` says what it last saw.
- **`play.expect`** checks a list of named conditions against the world as it is now, without waiting.
  Look for `"pass": true`; `results` has one entry per assertion, so a failure names the one that broke.
  "Nothing in reach" holds because the player starts 8 m from the gate and the prompt shows only within
  2.6 m.
- **`seeing.screenshot --out shot.png`** writes the window's last frame to `shot.png` and prints its size.
- **`quit.request`** asks the game to quit as if its window were closed. `"will_exit": true` means it
  exits at the end of the frame.

Back in the first terminal, `regia run` returns with exit status 0.

Conditions name an entity by `name` (or by id), a reflected component, a field path and a comparison; the
template's [agent-readable state](../user-guide/first-person-template.md#what-agents-can-read) lists what
this game exposes. [Playtest](../agents/testing.md) has the full condition grammar, and
[Agent protocol and MCP](../user-guide/agent-protocol.md) shows the same calls from an AI agent through
`regia mcp`.

## Where next

- Put these calls in a test that runs without a window: the template's `game/tests/agent_smoke.rs`
  drives the same game headless (`cargo test -p my_game`).
- [Running and iterating](running.md) covers logs, the live asset loop and packaging.
