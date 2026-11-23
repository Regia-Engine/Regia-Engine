# Agent benchmark

The benchmark measures how well a model builds and checks things through the agent protocol and
`regia mcp`, with no human help (`docs/specs/23-agent-native.md` § 8; target: at least 80% of the
tasks). It is a set of 25 tasks in `bench/agent_tasks/`. Each has a prompt for the model, a world to
start in and machine-checkable success criteria, so a run is scored without a person looking at it.

The model run needs an API key and is yours to start. What ships with the engine is the proof that
the set itself is sound: every task has an **oracle**, a scripted reference solution, and the tool
checks that the oracle passes every task and that an empty run fails every task.

```sh
cargo xtask agent-bench --oracle            # every task, solved by its oracle: 25 of 25 must pass
cargo xtask agent-bench --oracle --empty    # no oracle: every task must FAIL (the checks can fail)
cargo xtask agent-bench --oracle --task scatter-rocks-off-road --json
```

Both run in one process with no window and no GPU. Each task gets a fresh headless app (the
`setup.target`: plugins plus the real agent server), and the setup, the oracle and the checks are
ordinary protocol calls over the WebSocket, the same ones `regia mcp` forwards.

## Running a model against the set

For each task:

```sh
cargo xtask agent-bench --serve scatter-rocks-off-road
# prints the prompt, the project directory and how to attach, then keeps the app running
```

1. Register the printed project with your MCP client, for example
   `claude mcp add bench -- regia mcp --project <project> --attach`.
2. Give the model the printed `prompt` and nothing else. The task's `call_budget` is the number of
   protocol calls it may use; count the tool calls of the session.
3. When the model says it is done, score it from another terminal:
   `cargo xtask agent-bench --check-only --attach <project> --json`. Stop the `--serve` process
   (Ctrl-C, or create the `stop` file in the project's `.regia/agent/`).

`--check-only` evaluates only the checks, against the engine the model drove. It reads the baselines and the setup's
variables (entity ids, say) that `--serve` saved at the end of the setup. It defaults to the task
`--serve` prepared; `--task <id>` repeats. The report lists each check with the reason it failed, and
a success rate. Failures that are not the model's fault (a missing tool, a vague guide page, an
error message that does not say what to do) are protocol or documentation bugs: fix those, not the
task.

`--attach` also takes a `ws://` URL with `--token-file <file>` for an engine you started yourself;
then no baselines or variables are available: `image_differs` checks fail, and so do checks that
name a setup variable.

## Adding a task

1. Write `bench/agent_tasks/<id>.toml` (format: `docs/formats/agent-task.md`). Pick a `setup.target`
   that has the plugins the task needs. A task uses only methods that exist; the call budget is the
   oracle's length plus some slack.
2. Write `bench/agent_tasks/<id>.oracle.jsonl`: the calls a careful agent would make.
3. Make the checks about the world, not about the calls: a count of entities, a position, a
   `play.expect` result, a validation result. A check that a lazy or empty run also satisfies is a bug.
4. Run `cargo xtask agent-bench --oracle --task <id>` (must pass) and
   `cargo xtask agent-bench --oracle --empty --task <id>` (must fail). Develop calls with
   `cargo xtask agent-bench --probe <target> < calls.jsonl`, which runs a JSONL file of calls against
   a fresh target and prints every answer.
5. Keep the set at 25 and the spread over scene, terrain, materials, lighting, UI, audio, navigation,
   physics, sequencer and scripting.

A task that needs an agent method that does not exist is dropped from the set and the method is
listed as a gap; it joins when the method lands.

## What is not covered

- No screenshot task: the targets have no renderer, so every check reads engine state. The
  `image_differs` predicate is for a windowed target.
- The conformance of `regia mcp` itself is a separate test, `cargo test -p regia_cli --test
  mcp_conformance` (handshake, tool list, error mapping, resources, cancellation). It runs over
  stdio. An HTTP conformance run needs an HTTP client crate (`reqwest`) that is not approved.

## The tasks

| Task | What it asks | Areas | Target | Checks |
|---|---|---|---|---|
| `accessible-ui-profile` | Set up an accessible compact UI | ui | `ui` | 3 |
| `ball-lands-on-floor` | Drop a ball and let it come to rest | physics, scene | `physics` | 2 |
| `campfire-emitter` | Give the campfire a looping sound emitter | audio, scene | `audio` | 4 |
| `clearcoat-car-paint` | Author a red clear-coated car paint material | materials | `materials` | 5 |
| `close-the-gate` | Close the corridor with a navigation obstacle | nav | `nav` | 2 |
| `crates-in-the-zone` | Put exactly three crates inside the trigger zone | physics, scene | `physics` | 2 |
| `despawn-debug-markers` | Remove the debug markers, keep the spawn points | scene | `scene` | 2 |
| `door-functional-test` | Open the door and prove it with a functional test | scripting, scene | `scripting` | 3 |
| `duck-the-music` | Duck the music bus and mute the ambience | audio | `audio` | 3 |
| `elevator-halfway` | Park the elevator halfway with a sequence | sequencer, scene | `sequencer` | 2 |
| `exposure-and-saturation` | Brighten the level look and mute its colors | lighting | `lighting` | 2 |
| `fix-the-broken-material` | Fix the broken reference in a material graph | materials | `materials` | 3 |
| `flatten-a-building-pad` | Raise a building pad on the terrain | terrain | `terrain` | 3 |
| `golden-hour-sun` | Make the sun lower and warmer | lighting, scene | `lighting` | 3 |
| `halve-the-lamps` | Dim the three lamps to half | lighting, scene | `lighting` | 4 |
| `hero-slides-in` | Slide the hero in with a sequence | sequencer, scene | `sequencer` | 3 |
| `lamp-row` | Extend one street lamp into a row of five | scene | `scene` | 3 |
| `light-theme-large-text` | Switch the UI to the light theme with larger text | ui | `ui` | 2 |
| `move-player-to-spawn` | Put the player at Spawn Point B, facing east | scene | `scene` | 2 |
| `reopen-the-route` | Reopen a blocked route | nav | `nav` | 2 |
| `scatter-rocks-off-road` | Scatter thirty rocks on the terrain, none on the road | scene, terrain | `terrain` | 3 |
| `settle-the-crates` | Drop five crates and let them all fall asleep | physics | `physics` | 2 |
| `storm-wind` | Set a strong wind blowing along Z | terrain | `terrain` | 3 |
| `tile-grid` | Lay a 3 by 3 floor of tiles | scene | `scene` | 3 |
| `wall-blocks-the-ray` | Build a wall in the path of a ray | physics, scene | `physics` | 2 |

The areas cover scene editing, terrain, materials, lighting, UI, audio, navigation, physics, sequences and scripting.
