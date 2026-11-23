// SPDX-License-Identifier: MPL-2.0
//! The editor tutorials of the book as doc tests (ADR-9810, AGENTS.md rule 8): each test reads a
//! `book/tutorials/*.md` page at run time, takes the steps of its `sh` fences
//! (`regia_test_utils::tutorial`) and runs them, in page order, against the real
//! `regia-editor --headless --no-gpu` process through `regia_agent`'s client. A step the editor
//! refuses, a `# expect` that does not hold or an `# until` that never holds fails the test with
//! the page line, so the page cannot drift from the editor.
//!
//! `regia new NAME --template T` steps run as `editor.new_project` (the editor's File ▸ New
//! Project writes the same files); `regia package` steps are the CLI's and run in
//! `regia_cli`'s `book_tutorials` test. Every wait is on the editor's stdout, on an answer or on
//! the process, bounded by `HANG_BOUND` (a bound on a hang, not on speed).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use regia_agent::client::Client;
use regia_test_utils::process::{Running, wait_within};
use regia_test_utils::tutorial::{
    AgentStep, TutorialStep, agent_steps, read_page, refuse_mutating_until, tutorial_steps,
};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

/// The longest wait for an editor that stopped making progress (a loaded gate is slow).
const HANG_BOUND: Duration = Duration::from_secs(600);
/// Most repeats of one `# until` step (each is one editor frame or more); `# until` names only
/// read calls (`refuse_mutating_until`).
const UNTIL_CALLS: usize = 5_000;

/// The bootstrap project the editor starts on, and the folder new projects go in.
struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: &str) -> Self {
        let dir = unique_temp_dir(tag);
        let boot = dir.join("bootstrap");
        std::fs::create_dir_all(boot.join(".regia")).expect("bootstrap .regia");
        std::fs::write(
            boot.join("Regia.toml"),
            "[project]\nname = \"Bootstrap\"\nid = \"com.example.bootstrap\"\n",
        )
        .expect("manifest");
        std::fs::write(
            boot.join(".regia/agent.toml"),
            "permissions = [\"read\", \"edit_scene\", \"assets\", \"play\"]\n",
        )
        .expect("agent.toml");
        Self(dir.canonicalize().expect("canonical"))
    }

    fn boot(&self) -> PathBuf {
        self.0.join("bootstrap")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.0.join("editor.err")).unwrap_or_default()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running headless editor, its stdout lines and a client.
struct Editor {
    running: Running,
    lines: Receiver<String>,
    client: Client,
}

impl Editor {
    fn start(ws: &Workspace) -> Self {
        let err = std::fs::File::create(ws.0.join("editor.err")).expect("stderr log");
        let binary = std::env::var_os("REGIA_EDITOR_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_regia-editor").into());
        let home = ws.0.join("home");
        let mut child = Command::new(binary)
            .args(["--headless", "--no-gpu", "--project"])
            .arg(ws.boot())
            .current_dir(&ws.0)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("APPDATA", home.join("appdata"))
            .env_remove("REGIA_AGENT_DIR")
            .env_remove("FERRITE_AGENT_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("start the headless editor");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let running = Running(child);
        loop {
            let line = lines
                .recv_timeout(HANG_BOUND)
                .unwrap_or_else(|e| panic!("no ready line ({e}); stderr:\n{}", ws.log()));
            if serde_json::from_str::<Value>(&line).is_ok_and(|v| v["event"] == "ready") {
                break;
            }
        }
        let mut client = Client::connect_project(&ws.boot()).expect("connect to the ready editor");
        client.set_read_timeout(Some(HANG_BOUND)).expect("read timeout");
        Self {
            running,
            lines,
            client,
        }
    }

    /// Waits for the exit line and the process; the status must be success.
    fn wait_exit(&mut self, ws: &Workspace) -> Value {
        let exit = loop {
            let line = self
                .lines
                .recv_timeout(HANG_BOUND)
                .unwrap_or_else(|e| panic!("no exit line ({e}); stderr:\n{}", ws.log()));
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && value["event"] == "exit"
            {
                break value;
            }
        };
        let status = wait_within(&mut self.running.0, HANG_BOUND).expect("wait for the editor");
        assert!(status.success(), "editor exit {status}: {exit}\n{}", ws.log());
        exit
    }
}

/// One answered agent step.
struct Answered {
    method: String,
    answer: Value,
}

fn call(editor: &mut Editor, ws: &Workspace, line: usize, method: &str, params: Value) -> Value {
    editor.client.call(method, params.clone()).unwrap_or_else(|e| {
        panic!(
            "page line {line}: {method} {params} failed: {e}\nstderr:\n{}",
            ws.log()
        )
    })
}

fn run_agent_step(
    editor: &mut Editor,
    ws: &Workspace,
    step: &AgentStep,
    vars: &BTreeMap<String, String>,
) -> Value {
    let text = step
        .params_with(|name| vars.get(name).cloned())
        .unwrap_or_else(|e| panic!("{e}"));
    let params: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("page line {}: parameters {text}: {e}", step.line));
    let Some(check) = &step.check else {
        return call(editor, ws, step.line, &step.method, params);
    };
    let want: Value = serde_json::from_str(&check.value)
        .unwrap_or_else(|e| panic!("page line {}: expected value {}: {e}", step.line, check.value));
    let rounds = if check.until { UNTIL_CALLS } else { 1 };
    let mut last = Value::Null;
    for _ in 0..rounds {
        last = call(editor, ws, step.line, &step.method, params.clone());
        if last.pointer(&check.pointer) == Some(&want) {
            return last;
        }
    }
    panic!(
        "page line {}: {} answered {last}, where {} should be {want}",
        step.line, step.method, check.pointer
    );
}

/// Runs every step of `page` against one headless editor; answers the agent steps' answers and
/// the root of the project the page made.
fn run_page(page: &str, tag: &str) -> (Workspace, PathBuf, Vec<Answered>) {
    let text = read_page(page).unwrap_or_else(|e| panic!("{e}"));
    let steps = tutorial_steps(&text).unwrap_or_else(|e| panic!("{page}: {e}"));
    assert!(!agent_steps(&steps).is_empty(), "{page} has no agent steps");
    let ws = Workspace::new(tag);
    let mut editor = Editor::start(&ws);
    // `# until` repeats its call: the running editor's schema must mark it a read call (ADR-7451).
    let schema = call(&mut editor, &ws, 0, "agent.schema", json!({}));
    let mutating: BTreeMap<String, bool> = schema["methods"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| Some((m["name"].as_str()?.to_string(), m["mutating"].as_bool()?)))
        .collect();
    refuse_mutating_until(page, &steps, |method| mutating.get(method).copied())
        .unwrap_or_else(|e| panic!("{e}"));
    let mut vars = BTreeMap::new();
    let mut answered = Vec::new();
    let mut root = ws.boot();
    let mut ended = false;
    for step in &steps {
        match step {
            TutorialStep::Cli { line, args } if args.first().map(String::as_str) == Some("new") => {
                let name = args
                    .get(1)
                    .unwrap_or_else(|| panic!("{page}:{line}: regia new needs a name"));
                let template = args
                    .iter()
                    .position(|a| a == "--template")
                    .and_then(|i| args.get(i + 1))
                    .map_or("blank", String::as_str);
                let made = call(
                    &mut editor,
                    &ws,
                    *line,
                    "editor.new_project",
                    json!({ "name": name, "template": template, "path": ws.0.display().to_string() }),
                );
                root = PathBuf::from(made["root"].as_str().unwrap_or_default());
                assert!(root.join("Regia.toml").is_file(), "{page}:{line}: {made}");
            }
            // `regia package` and the other CLI steps run in regia_cli's book_tutorials test.
            TutorialStep::Cli { .. } | TutorialStep::Cd { .. } => {}
            TutorialStep::Agent(step) => {
                assert!(!ended, "{page}:{}: an agent step after agent.shutdown", step.line);
                let answer = run_agent_step(&mut editor, &ws, step, &vars);
                if let Some((name, field)) = &step.keep {
                    let kept = answer
                        .get(field)
                        .unwrap_or_else(|| panic!("{page}:{}: answer {answer} has no `{field}`", step.line));
                    vars.insert(name.clone(), kept.to_string());
                }
                if step.method == "agent.shutdown" {
                    editor.wait_exit(&ws);
                    ended = true;
                }
                answered.push(Answered {
                    method: step.method.clone(),
                    answer,
                });
            }
        }
    }
    if !ended {
        let _ = editor.client.call("agent.shutdown", json!({ "save": false }));
        editor.wait_exit(&ws);
    }
    (ws, root, answered)
}

fn methods(answered: &[Answered]) -> Vec<&str> {
    answered.iter().map(|a| a.method.as_str()).collect()
}

fn answer<'a>(answered: &'a [Answered], method: &str) -> &'a Value {
    &answered
        .iter()
        .rfind(|a| a.method == method)
        .unwrap_or_else(|| panic!("no {method} step"))
        .answer
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn the_first_scene_tutorial_runs_through_the_agent() {
    let (_ws, root, answered) = run_page("editor-first-scene.md", "book-tutorial-first-scene");
    let order = methods(&answered);
    for needed in [
        "editor.spawn",
        "editor.add_component",
        "editor.set_property",
        "editor.save_level",
        "editor.play",
        "editor.stop",
    ] {
        assert!(
            order.contains(&needed),
            "the page lost its {needed} step: {order:?}"
        );
    }
    let play = order.iter().position(|m| *m == "editor.play");
    let stop = order.iter().position(|m| *m == "editor.stop");
    assert!(play < stop, "play before stop: {order:?}");
    // The level on disk holds what the page placed.
    let level = read(&root, "assets/levels/main.level.ron");
    for name in ["Ground", "Crate", "Pillar", "Sun", "Lamp"] {
        assert!(level.contains(name), "{name} is in the saved level:\n{level}");
    }
    assert!(
        level.contains("DirectionalLight") && level.contains("PointLight"),
        "{level}"
    );
}

#[test]
fn the_first_material_tutorial_runs_through_the_agent() {
    let (_ws, root, answered) = run_page("editor-first-material.md", "book-tutorial-first-material");
    let compiled = answer(&answered, "editor.matgraph_compile");
    assert_eq!(compiled["ok"], true, "{compiled}");
    let saved = answer(&answered, "editor.matgraph_save");
    assert_eq!(saved["ok"], true, "{saved}");
    let graph = read(&root, "assets/materials/painted_metal.matgraph.ron");
    assert!(graph.contains("tint") && graph.contains("base_color"), "{graph}");
}

#[test]
fn the_package_tutorial_editor_steps_run_through_the_agent() {
    let (_ws, root, answered) = run_page("package-a-build.md", "book-tutorial-package");
    let order = methods(&answered);
    assert_eq!(order.last(), Some(&"agent.shutdown"), "{order:?}");
    let manifest = read(&root, "Regia.toml");
    assert!(manifest.contains("version = \"1.0.0\""), "{manifest}");
    assert!(manifest.contains("build_number = 2"), "{manifest}");
}
