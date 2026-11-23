---
name: regia-new-project
description: Create a new Regia game project from a template, build it, run it once and open it in the editor.
---

# Create a Regia project

Runs the project part of "Setting Regia up" in `AGENTS.md` (questions 5 and 6) on a machine where Regia is
already built. Keep to the "Rules" section of `AGENTS.md`. Call `regia` by its absolute path
(`<engine>/target/release/regia`) until a new shell has it on `PATH`.

## 1. Ask what is missing

Ask only what the user has not said yet.

- **(Question 5 of 6 · template)** `regia new --help` lists the templates: `blank`, `first_person`,
  `third_person`, `top_down`, `quick` (one crate, one `main.rs`) and `net-game` (a dedicated server, a client and
  the shared game crate). Recommend one from what the user wants to make. Default `first_person`.
- **(Question 6 of 6 · project)** Name (default `My Game`) and projects folder (default `~/RegiaProjects`).

## 2. Create, build, open

1. `regia new "My Game" --template first_person --path ~/RegiaProjects`. The project directory is the
   snake_case name (`my_game`). It path-depends on the engine folder; never move the engine folder afterwards.
2. **(Consent · warm_build)** Say that the first build compiles the game crates (several minutes) and may download
   a few more crates, then in the project: `cargo build -p my_game_launcher`.
3. Run it once: `regia run` in the project. A window opens; the user closes it.
4. Open the editor: `regia editor` in the project.

## 3. First check (modes 2 and 3)

Follow `book/start/first-agent-check.md` (it also has the template's `play.wait_until` and `play.expect`
calls): start the game with `regia run`, then from a second terminal in the project

```sh
regia agent call agent.hello
regia agent call seeing.screenshot --out first.png
regia agent call quit.request
```

Show the screenshot. In mode 1, offer the check instead of running it.

## 4. What next

- Mode 1: the editor tour (`book/start/editor-tour.md`) and "Your first project" (`book/getting-started.md`).
- Modes 2 and 3: `regia-connect-editor`.
- Shipping later: `regia package --platform linux` (`book/user-guide/packaging.md`).
