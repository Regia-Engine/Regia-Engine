---
name: regia-new-project
description: Create a new Regia game project from a template and use the right first-run path for that template.
---

# Create a Regia project

Runs the project part of "Setting Regia up" in `AGENTS.md` (questions 5 and 6) on a machine where Regia is
already built. Keep to the "Rules" section of `AGENTS.md`. Call `regia` by its absolute path
(`<engine>/target/release/regia`) until a new shell has it on `PATH`.

## 1. Ask what is missing

Ask only what the user has not said yet.

- **(Question 5 of 6 · template)** `regia new --help` lists the templates: `blank`, `first_person`,
  `third_person`, `top_down`, `quick` (one crate, one `main.rs`, `cargo build` / `cargo run`, no editor) and
  `net-game` (a dedicated server, a client and the shared game crate). Recommend one from what the user wants to
  make. Default `first_person`.
- **(Question 6 of 6 · project)** Name (default `My Game`) and projects folder (default `~/RegiaProjects`).

## 2. Create, build, open

1. `regia new "My Game" --template first_person --path ~/RegiaProjects`. The project directory is the
   snake_case name (`my_game`). It path-depends on the engine folder; never move the engine folder afterwards.
2. Branch by template:
   - `quick`: **(Consent · warm_build)** Say that the first build compiles the app crate (several minutes) and may
     download a few more crates, then in the project: `cargo build`.
   - `net_game`: **(Consent · warm_build)** the same, then in the project: plain `cargo build` (its default
     members are the game and the client; it has no launcher crate).
   - `blank`, `first_person`, `third_person`, `top_down` (the templates with a desktop launcher crate):
     **(Consent · warm_build)** Say that the first build compiles the game crates (several minutes) and may
     download a few more crates, then in the project: `cargo build -p <snake_case name>_launcher`.
3. Branch by template again:
   - `quick`: run it once with `cargo run`. Do not tell the user to use `regia run` or `regia editor`; quick apps do
     not create an editor project or the MCP project file.
   - `net_game`: `regia run` needs a launcher crate, which this template does not have: start the server and a
     client with the `cargo run -p <snake_case name>_server` and `cargo run -p <snake_case name>_client` lines of
     its `README.md`. Then open the editor with `regia editor`.
   - `blank`, `first_person`, `third_person`, `top_down`: run it once with `regia run` in the project. A window
     opens; the user closes it. Then open the editor with `regia editor`.

## 3. First check (modes 2 and 3)

For any non-`quick` template, follow `book/start/first-agent-check.md` (it also has the template's
`play.wait_until` and `play.expect` calls): start the game with `regia run`, then from a second terminal in the
project

```sh
regia agent call agent.hello
regia agent call seeing.screenshot --out first.png
regia agent call quit.request
```

Show the screenshot. In mode 1, offer the check instead of running it.

For `quick`, do not run the editor-connect check from this skill. Say that the quick template is a cargo-only
one-file app and recommend a full project template when the user wants an editor-connected mode.

## 4. What next

- `quick`: `book/user-guide/quick-start.md` and `book/agents/quick-app.md`.
- Mode 1 on any other template: the editor tour (`book/start/editor-tour.md`) and "Your first project"
  (`book/getting-started.md`).
- Modes 2 and 3 on any other template: `regia-connect-editor`.
- Shipping later from a full project: `regia package --platform linux` (`book/user-guide/packaging.md`).
