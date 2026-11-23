// SPDX-License-Identifier: MPL-2.0
//! The book's getting-started walkthroughs, run as written (B07): `book/start/first-agent-check.md`
//! (a First Person project checked through the agent protocol) and steps 1 to 5 of
//! `book/getting-started.md` (a blank project, the page's own `lib.rs`, the agent calls).
//!
//! The page is the script. Its `console` fences, and the file edits it tells the reader to make, run in
//! page order through a small interpreter, so the commands a reader copies are exactly the ones this
//! test runs.
//!
//! - `cd DIR` changes the working directory of the lines after it.
//! - `regia ...` runs this checkout's `regia` binary; `cargo ...` runs cargo. Each must exit 0.
//! - `regia run` is the reader's first terminal: it starts in the background and must exit 0 after the
//!   page's `quit.request`. When a page runs the game again (getting-started step 3, "Close the window
//!   and run it again"), the harness first closes the running one the way the reader does, through
//!   `quit.request`, and requires exit 0.
//! - `regia agent call agent.hello` is the page's "if it cannot connect, run it again": it is retried
//!   (a counted hang guard, while `regia run` is alive) until the game answers.
//! - Every other agent call runs once, and its printed result is checked against what the page tells
//!   the reader to look for: `"satisfied": true`, `"pass": true`, `"will_exit": true`, a found entity,
//!   and a PNG of the printed size (not one flat color; for getting-started, an orange box under a blue
//!   sky).
//! - A `$ ` line in any other fence fails the test (the reader would type it; the walk would not).
//! - A `toml` fence whose paragraph names `game/Cargo.toml` adds its crates next to the existing `regia`
//!   line with that line's path prefix, as the page says; a `rust` fence introduced by "replace
//!   `game/src/lib.rs` with" becomes that file. Other `rust` fences are excerpts the reader merges by
//!   hand (getting-started step 4's `Spin`); `cargo xtask book-check` compiles them, the walk does not.
//!
//! What the harness adds that the page does not show: a fresh temp directory to start in, and one
//! cached `CARGO_TARGET_DIR` so a second run does not rebuild the engine. Neither changes a command.
//! `regia new` seeds the project's `Cargo.lock` from this checkout itself (spec 94 § 3.2), so the
//! reader's build and this one resolve the same dependency versions; the test checks it did.
//!
//! `page_*` tests run in every `cargo test` and need no build: they hold each page to its walkthrough
//! (each step present, in order, every line runnable by the interpreter, every JSON parameter
//! valid). The full runs are ignored by default (an engine build, and a window: run them under
//! `xvfb-run -a` with lavapipe or a GPU):
//! `xvfb-run -a cargo test -p regia_cli --test getting_started_walkthrough -- --ignored --test-threads=1`.
#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// The agent walkthrough page, relative to the repository root.
const PAGE: &str = "book/start/first-agent-check.md";

/// The first-project page, relative to the repository root; its walkthrough is steps 1 to 5.
const GETTING_STARTED: &str = "book/getting-started.md";

/// The heading the getting-started walkthrough stops at (the editor and packaging steps need a
/// desktop session and a package build; they are not agent steps).
const GETTING_STARTED_STOP: &str = "## 6.";

/// Root of this repo (the test lives in `crates/regia_cli/tests`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// What a page asks the reader to do, in order, up to the heading that starts with `stop`: each `$ `
/// line of a `console` fence, a `toml` fence whose paragraph names `game/Cargo.toml` (crates to add),
/// and a `rust` fence introduced by "replace `game/src/lib.rs` with" (the file's new text).
fn page_script(text: &str, stop: Option<&str>) -> Vec<(String, Result<Step, String>)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    // The last paragraph of prose before a fence (a blank line ends it; the next prose line starts
    // a new one).
    let mut paragraph = String::new();
    let mut ended = false;
    while let Some(line) = lines.next() {
        if stop.is_some_and(|stop| line.starts_with(stop)) {
            break;
        }
        let trimmed = line.trim_start();
        let Some(info) = trimmed.strip_prefix("```") else {
            if trimmed.is_empty() {
                ended = true;
            } else {
                if ended {
                    paragraph.clear();
                    ended = false;
                }
                paragraph.push_str(trimmed);
                paragraph.push(' ');
            }
            continue;
        };
        let info = info.trim().to_owned();
        let mut body = String::new();
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                break;
            }
            body.push_str(inner);
            body.push('\n');
        }
        if info == "console" {
            for command in body.lines().filter_map(|l| l.trim_start().strip_prefix("$ ")) {
                let command = command.trim().to_owned();
                out.push((command.clone(), step(&command)));
            }
        } else if is_walk_fence(&info) {
            // `cargo xtask walkthrough`'s fences (ADR-2744): every line is a command the reader
            // types. The default-template fence is this walk's; other templates are examples.
            for command in body
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
            {
                let command = command.to_owned();
                out.push((command.clone(), step(&command)));
            }
        } else if info == "toml" && paragraph.contains("`game/Cargo.toml`") {
            out.push((
                "(add the crates to game/Cargo.toml)".into(),
                Ok(Step::AddDeps(body)),
            ));
        } else if info.split(',').next() == Some("rust")
            && paragraph.contains("replace `game/src/lib.rs` with")
        {
            out.push(("(replace game/src/lib.rs)".into(), Ok(Step::ReplaceLib(body))));
        } else if let Some(command) = body.lines().find_map(|l| l.trim_start().strip_prefix("$ ")) {
            // A command the reader would type, in a fence the walkthrough does not run.
            let command = command.trim().to_owned();
            let error = format!("`{command}` is in a ```{info} fence: mark it ```console");
            out.push((command, Err(error)));
        }
        paragraph.clear();
        ended = false;
    }
    out
}

/// A `sh,walkthrough=<stage>` fence this walk runs: one without a `template=` key or with
/// `template=blank` (integ-v049a: step 1 moved to this form with the game-developer walkthrough).
fn is_walk_fence(info: &str) -> bool {
    let mut keys = info.split(',');
    keys.next() == Some("sh")
        && info.split(',').any(|k| k.starts_with("walkthrough="))
        && keys.all(|k| !k.starts_with("template=") || k == "template=blank")
}

/// Splits a command line the way a POSIX shell does for the forms the book uses: words separated
/// by blanks, `'...'` taken literally, `"..."` with `\"` and `\\` escapes. Anything a plain run of
/// a program cannot mean (pipes, redirections, `&&`, variables, globs) is refused, so a page that
/// grows one fails here instead of running something else than it shows.
fn shell_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => word.push(inner),
                        None => return Err(format!("unclosed ' in `{line}`")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(escaped @ ('"' | '\\' | '$' | '`')) => word.push(escaped),
                            Some(other) => {
                                word.push('\\');
                                word.push(other);
                            }
                            None => return Err(format!("unclosed \" in `{line}`")),
                        },
                        Some('$' | '`') => return Err(format!("expansion inside \"...\" in `{line}`")),
                        Some(inner) => word.push(inner),
                        None => return Err(format!("unclosed \" in `{line}`")),
                    }
                }
            }
            '|' | '&' | ';' | '<' | '>' | '$' | '`' | '*' | '?' | '#' | '(' | ')' | '\\' | '~' => {
                return Err(format!(
                    "`{c}` in `{line}`: the walkthrough runs plain commands only"
                ));
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        return Err("an empty command".into());
    }
    Ok(words)
}

/// One page line, as the interpreter runs it.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// `cd DIR`.
    Cd(String),
    /// `regia new ...`: runs, and must leave a project with a seeded `Cargo.lock`.
    New(Vec<String>),
    /// `cargo ...`: runs to completion.
    Cargo(Vec<String>),
    /// `regia run ...`: the first terminal, in the background.
    Run(Vec<String>),
    /// A `toml` fence for `game/Cargo.toml`: crates to add next to the existing `regia` line.
    AddDeps(String),
    /// A `rust` fence that replaces `game/src/lib.rs`.
    ReplaceLib(String),
    /// `regia agent call <method> [params] [--out FILE]`.
    AgentCall {
        /// Every argument after `regia`.
        args: Vec<String>,
        /// The protocol method.
        method: String,
        /// The JSON parameters (`{}` when the line has none).
        params: Value,
        /// `--out FILE`, for a screenshot.
        out: Option<String>,
    },
}

/// Classifies one page line. Anything the interpreter does not know is an error.
fn step(line: &str) -> Result<Step, String> {
    let words = shell_words(line)?;
    match words[0].as_str() {
        "cd" if words.len() == 2 => Ok(Step::Cd(words[1].clone())),
        "cargo" => Ok(Step::Cargo(words[1..].to_vec())),
        "regia" => {
            let args = words[1..].to_vec();
            match args.first().map(String::as_str) {
                Some("new") => Ok(Step::New(args)),
                Some("run") => Ok(Step::Run(args)),
                Some("agent") if args.get(1).map(String::as_str) == Some("call") => {
                    let method = args
                        .get(2)
                        .cloned()
                        .ok_or_else(|| format!("`{line}`: no method"))?;
                    let mut params = Value::Object(serde_json::Map::new());
                    let mut out = None;
                    let mut rest = args[3..].iter();
                    while let Some(arg) = rest.next() {
                        if arg == "--out" {
                            out = Some(
                                rest.next()
                                    .cloned()
                                    .ok_or_else(|| format!("`{line}`: --out needs a file"))?,
                            );
                        } else if arg.starts_with("--") {
                            return Err(format!("`{line}`: the walkthrough does not know `{arg}`"));
                        } else {
                            params = serde_json::from_str(arg)
                                .map_err(|e| format!("`{line}`: the parameters are not JSON: {e}"))?;
                        }
                    }
                    Ok(Step::AgentCall {
                        args,
                        method,
                        params,
                        out,
                    })
                }
                _ => Err(format!(
                    "`{line}`: the walkthrough does not run this `regia` command"
                )),
            }
        }
        other => Err(format!("`{line}`: the walkthrough does not run `{other}`")),
    }
}

/// A page's steps, in order, with the text each one shows; panics on a line the interpreter cannot run.
fn page_steps_of(page: &str, stop: Option<&str>) -> Vec<(String, Step)> {
    let path = repo_root().join(page);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let script = page_script(&text, stop);
    assert!(!script.is_empty(), "{page} has no walkthrough steps");
    script
        .into_iter()
        .map(|(shown, step)| (shown, step.unwrap_or_else(|e| panic!("{page}: {e}"))))
        .collect()
}

/// The agent walkthrough page's steps, in order.
fn page_steps() -> Vec<Step> {
    page_steps_of(PAGE, None)
        .into_iter()
        .map(|(_, step)| step)
        .collect()
}

/// Getting-started steps 1 to 5, in order.
fn getting_started_steps() -> Vec<Step> {
    page_steps_of(GETTING_STARTED, Some(GETTING_STARTED_STOP))
        .into_iter()
        .map(|(_, step)| step)
        .collect()
}

/// The method of an agent-call step.
fn method_of(step: &Step) -> Option<&str> {
    match step {
        Step::AgentCall { method, .. } => Some(method),
        _ => None,
    }
}

#[test]
fn page_shell_words_follow_the_shell_for_the_forms_the_book_uses() {
    assert_eq!(
        shell_words(r#"regia new "My Game" --template first_person"#).expect("splits"),
        ["regia", "new", "My Game", "--template", "first_person"]
    );
    assert_eq!(
        shell_words(r#"regia agent call play.expect '{"a": "b c"}'"#).expect("splits"),
        ["regia", "agent", "call", "play.expect", r#"{"a": "b c"}"#]
    );
    assert_eq!(shell_words(r#"x "a\"b""#).expect("splits"), ["x", "a\"b"]);
    assert_eq!(shell_words("a'b'c").expect("splits"), ["abc"]);
    for refused in ["a | b", "a && b", "a > f", "echo $?", "ls *.png", "a 'b", "cd ~"] {
        assert!(shell_words(refused).is_err(), "{refused} must be refused");
    }
}

#[test]
fn page_console_lines_are_only_prompted_lines_of_console_fences() {
    let text =
        "```console\n$ a\noutput\n$ b c\n```\n\n```toml\nnot = 1\n```\n$ prose\n```console\n  $ d\n```\n";
    let shown: Vec<String> = page_script(text, None)
        .into_iter()
        .map(|(shown, _)| shown)
        .collect();
    assert_eq!(shown, ["a", "b c", "d"]);
}

#[test]
fn page_every_line_is_one_the_walkthrough_runs() {
    let steps = page_steps();
    for unknown in [
        "ls",
        "regia package --platform linux",
        "regia agent call x --project .",
    ] {
        assert!(
            step(unknown).is_err(),
            "`{unknown}` must not be accepted silently"
        );
    }
    // `page_steps` panics on any line the interpreter cannot run.
    assert!(steps.len() >= 9, "{steps:?}");
}

#[test]
fn page_walks_new_build_run_wait_expect_screenshot_quit_in_order() {
    let steps = page_steps();
    let position = |what: &str, pred: &dyn Fn(&Step) -> bool| {
        steps
            .iter()
            .position(pred)
            .unwrap_or_else(|| panic!("{PAGE} has no {what} step: {steps:?}"))
    };
    let new = position(
        "`regia new --template first_person`",
        &|s| matches!(s, Step::New(a) if a.windows(2).any(|w| w[0] == "--template" && w[1] == "first_person")),
    );
    let cd = position("`cd`", &|s| matches!(s, Step::Cd(_)));
    let build = position(
        "`cargo build`",
        &|s| matches!(s, Step::Cargo(a) if a.first().is_some_and(|c| c == "build")),
    );
    let run = position("`regia run`", &|s| matches!(s, Step::Run(_)));
    let hello = position("`agent.hello`", &|s| method_of(s) == Some("agent.hello"));
    let wait = position("`play.wait_until`", &|s| method_of(s) == Some("play.wait_until"));
    let expect = position("`play.expect`", &|s| method_of(s) == Some("play.expect"));
    let shot = position(
        "`seeing.screenshot --out`",
        &|s| matches!(s, Step::AgentCall { method, out: Some(_), .. } if method == "seeing.screenshot"),
    );
    let quit = position("`quit.request`", &|s| method_of(s) == Some("quit.request"));
    let order = [new, cd, build, run, hello, wait, expect, shot, quit];
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "steps out of order: {order:?} in {steps:?}"
    );
    assert_eq!(quit, steps.len() - 1, "quit.request is the last step: {steps:?}");
    assert_eq!(
        steps.iter().filter(|s| matches!(s, Step::Run(_))).count(),
        1,
        "one background `regia run`"
    );
    // The conditions are the shapes the schema documents.
    if let Step::AgentCall { params, .. } = &steps[wait] {
        assert!(params["condition"].is_object(), "{params}");
    }
    if let Step::AgentCall { params, .. } = &steps[expect] {
        let assertions = params["assertions"].as_array().expect("assertions array");
        assert!(!assertions.is_empty());
        assert!(
            assertions
                .iter()
                .all(|a| a["condition"].is_object() && a["name"].is_string()),
            "{params}"
        );
    }
}

/// Kills the background `regia run` when dropped (an assertion failed while it ran), with its
/// whole process group: `regia run` starts `cargo run`, which starts the game, and killing `regia`
/// alone would leave both behind (holding the target dir's lock, outliving the temp project).
struct KillOnDrop(Option<Child>);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            kill_group(child);
        }
    }
}

/// Kills `child`'s process group (it was spawned as a group leader) and reaps it.
fn kill_group(child: &mut Child) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The target dir the generated project builds into, kept under this test's own target dir between
/// runs (`<target>/debug/deps/<test>` -> `<target>/getting-started-walkthrough`).
fn walkthrough_target_dir() -> PathBuf {
    std::env::current_exe()
        .expect("test binary path")
        .ancestors()
        .nth(3)
        .expect("the test binary is under a target dir")
        .join("getting-started-walkthrough")
}

/// The last lines of a log file, for a failure message.
fn tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(60)..].join("\n")
}

/// A command for a page line: `program args`, in `cwd`, with the harness environment.
fn command(program: &Path, args: &[String], cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env("CARGO_TARGET_DIR", walkthrough_target_dir())
        .stdin(Stdio::null());
    cmd
}

/// What a finished page line printed.
struct Output {
    /// Exit status.
    status: std::process::ExitStatus,
    /// Standard output.
    stdout: String,
    /// Standard error.
    stderr: String,
}

/// Runs a command to completion with its output in files (no pipe can fill up), under a hang guard:
/// `guard` checks of 100 ms each, then the command is killed and the test fails. A guard, not a
/// budget: it only stops a command that would otherwise hang the test (a game that accepts the agent
/// connection and never answers).
fn run_guarded(line: &str, mut cmd: Command, guard: u32) -> Output {
    let dir = regia_test_utils::unique_temp_dir("walkthrough_cmd");
    let (out_path, err_path) = (dir.join("stdout"), dir.join("stderr"));
    let out = std::fs::File::create(&out_path).expect("stdout file");
    let err = std::fs::File::create(&err_path).expect("stderr file");
    let mut child = cmd
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("`{line}`: cannot start: {e}"));
    let mut status = None;
    for _ in 0..guard {
        if let Some(done) = child.try_wait().expect("wait") {
            status = Some(done);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let Some(status) = status else {
        kill_group(&mut child);
        panic!("`{line}` did not finish:\n{}", read(&err_path));
    };
    let output = Output {
        status,
        stdout: read(&out_path),
        stderr: read(&err_path),
    };
    let _ = std::fs::remove_dir_all(&dir);
    output
}

/// The hang guard of one agent call or project command, in 100 ms checks (half an hour: long enough
/// for a cold `cargo build` of the engine on a loaded host).
const COMMAND_GUARD: u32 = 18_000;

/// Runs a page line to completion; panics with its output unless it exits 0. Returns stdout.
fn run_to_end(line: &str, cmd: Command) -> String {
    let output = run_guarded(line, cmd, COMMAND_GUARD);
    assert!(
        output.status.success(),
        "`{line}` exited {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        output.stdout,
        output.stderr
    );
    output.stdout
}

/// Loads the PNG `regia agent call seeing.screenshot --out` wrote and checks it against its printed
/// size and that it shows something (a window that never drew is one flat color).
fn load_screenshot(path: &Path, printed: &str) -> image::RgbaImage {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("screenshot {}: {e}", path.display()));
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .unwrap_or_else(|e| panic!("screenshot {} is not a PNG: {e}", path.display()))
        .to_rgba8();
    let size = format!("{}x{}", image.width(), image.height());
    assert!(image.width() > 0 && image.height() > 0, "an empty screenshot");
    assert!(
        printed.split_whitespace().next() == Some(size.as_str()),
        "the command printed `{}`, the PNG is {size}",
        printed.trim()
    );
    let first = image.pixels().next().copied().expect("a pixel");
    let differing = image.pixels().filter(|p| **p != first).count();
    assert!(
        differing * 100 >= image.pixels().len(),
        "the screenshot is almost one flat color ({differing} of {} pixels differ from the first)",
        image.pixels().len()
    );
    image
}

/// The share (0 to 1) of `image`'s pixels for which `pred` holds.
fn share(image: &image::RgbaImage, pred: impl Fn([u8; 4]) -> bool) -> f64 {
    let hits = image.pixels().filter(|p| pred(p.0)).count();
    hits as f64 / image.pixels().len().max(1) as f64
}

/// Adds the crates of a page's `toml` fence to `game/Cargo.toml` "next to the existing `regia` line,
/// with the same path prefix", as getting-started step 3 says.
fn add_deps(project: &Path, fence: &str) {
    let manifest = project.join("game/Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("game/Cargo.toml");
    let regia_line = text
        .lines()
        .find(|l| l.starts_with("regia = { path = \""))
        .unwrap_or_else(|| panic!("game/Cargo.toml has no `regia = {{ path = ... }}` line:\n{text}"));
    let prefix = regia_line
        .split_once("path = \"")
        .and_then(|(_, rest)| rest.split_once("/crates/regia\""))
        .map(|(prefix, _)| prefix)
        .unwrap_or_else(|| panic!("cannot read the engine path of `{regia_line}`"));
    let mut added = String::new();
    for line in fence.lines().map(str::trim) {
        if line.is_empty() || line == "[dependencies]" || line.starts_with("regia = ") {
            continue;
        }
        let (page_prefix, rest) = line
            .split_once("path = \"")
            .and_then(|(_, rest)| rest.split_once("/crates/"))
            .unwrap_or_else(|| panic!("the page's dependency line `{line}` has no `.../crates/<name>` path"));
        let name = line.split(' ').next().unwrap_or_default();
        assert!(
            rest.starts_with(&format!("{name}\"")),
            "`{line}`: the path does not name the crate `{name}`"
        );
        added.push_str(&line.replacen(page_prefix, prefix, 1));
        added.push('\n');
    }
    assert!(!added.is_empty(), "the page's toml fence adds no crate");
    let edited = text.replacen(&format!("{regia_line}\n"), &format!("{regia_line}\n{added}"), 1);
    assert_ne!(edited, text, "the crates were not added after `{regia_line}`");
    std::fs::write(&manifest, edited).expect("write game/Cargo.toml");
}

/// Waits for `regia run`'s game to answer `agent.hello` and returns what the call printed. The page's
/// "if it cannot connect, the game is still starting: run it again": the call is retried only while it
/// reports that it cannot connect and `regia run` is alive. While `regia run` is still cooking and
/// building (its log grows, and cargo has not printed `Running`), the wait has no cap: a cold engine
/// build takes as long as it takes. After the game starts, a counted hang guard applies.
fn wait_for_game(regia: &Path, args: &[String], cwd: &Path, game: &mut KillOnDrop, run_log: &Path) -> String {
    let mut after_start = 0u32;
    let mut last_err = String::new();
    // A backstop of about three hours for a build that never finishes.
    let mut total = 0u32;
    while after_start < 6_000 && total < 100_000 {
        total += 1;
        if let Some(status) = game.0.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            panic!(
                "`regia run` exited {status} before the game answered:\n{}",
                tail(run_log)
            );
        }
        let output = run_guarded(
            "regia agent call agent.hello",
            command(regia, args, cwd),
            COMMAND_GUARD,
        );
        if output.status.success() {
            return output.stdout;
        }
        assert!(
            output
                .stderr
                .contains("cannot connect to a running Regia Engine app"),
            "`regia agent call agent.hello` failed for another reason than a game still starting:\n{}",
            output.stderr
        );
        last_err = output.stderr;
        let started = std::fs::read_to_string(run_log).is_ok_and(|log| log.contains("Running `"));
        if started {
            after_start += 1;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "the game started but never answered `agent.hello`:\n{last_err}\n{}",
        tail(run_log)
    );
}

/// Waits for the background `regia run` to exit after a quit (a counted hang guard) and requires 0.
fn expect_clean_exit(game: &mut KillOnDrop, run_log: &Path) {
    let mut child = game.0.take().expect("a `regia run` is running");
    for _ in 0..1_200 {
        if let Some(status) = child.try_wait().expect("wait for regia run") {
            assert!(
                status.success(),
                "`regia run` exited {status}:\n{}",
                tail(run_log)
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    kill_group(&mut child);
    panic!("`regia run` did not exit after quit.request:\n{}", tail(run_log));
}

/// Runs a page's steps as the reader does (see the module docs). `check_shot` gets every screenshot.
fn walk(page: &str, stop: Option<&str>, check_shot: &dyn Fn(&image::RgbaImage)) {
    assert!(
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some(),
        "the walkthrough opens a window: run it under `xvfb-run -a` (or in a desktop session)"
    );
    let steps = page_steps_of(page, stop);
    let regia = PathBuf::from(env!("CARGO_BIN_EXE_regia"));
    let work = regia_test_utils::unique_temp_dir("getting_started_walkthrough");
    let _cleanup = Cleanup(work.clone());
    let mut cwd = work.clone();
    let mut game = KillOnDrop(None);
    let mut runs = 0;
    let mut run_log = work.join("regia-run-0.log");
    let hello: Vec<String> = ["agent", "call", "agent.hello"].map(String::from).to_vec();
    let quit: Vec<String> = ["agent", "call", "quit.request"].map(String::from).to_vec();

    for (line, step) in &steps {
        eprintln!("walkthrough ({page}): $ {line}");
        match step {
            Step::Cd(dir) => {
                cwd = cwd.join(dir);
                assert!(cwd.is_dir(), "`{line}`: {} is not a directory", cwd.display());
            }
            Step::New(args) => {
                let stdout = run_to_end(line, command(&regia, args, &cwd));
                let created = PathBuf::from(stdout.lines().last().unwrap_or_default().trim());
                assert!(
                    created.join("Regia.toml").is_file(),
                    "`{line}` printed `{}`",
                    stdout.trim()
                );
                assert!(
                    created.join("Cargo.lock").is_file(),
                    "`{line}` did not seed Cargo.lock: the build would resolve untested versions"
                );
            }
            Step::Cargo(args) => {
                run_to_end(line, command(Path::new("cargo"), args, &cwd));
            }
            Step::AddDeps(fence) => add_deps(&cwd, fence),
            Step::ReplaceLib(code) => {
                assert!(
                    !code.lines().any(|l| {
                        let t = l.trim_start();
                        t == "#" || t.starts_with("# ")
                    }),
                    "the page's lib.rs has hidden lines the reader does not see"
                );
                std::fs::write(cwd.join("game/src/lib.rs"), code).expect("write game/src/lib.rs");
            }
            Step::Run(args) => {
                if game.0.is_some() {
                    // The reader closes the window before running again: the same quit the page uses.
                    wait_for_game(&regia, &hello, &cwd, &mut game, &run_log);
                    let closed = run_to_end("regia agent call quit.request", command(&regia, &quit, &cwd));
                    let closed: Value = serde_json::from_str(&closed).expect("quit.request prints JSON");
                    assert_eq!(closed["will_exit"], true, "{closed}");
                    expect_clean_exit(&mut game, &run_log);
                }
                runs += 1;
                run_log = work.join(format!("regia-run-{runs}.log"));
                let log = std::fs::File::create(&run_log).expect("run log");
                let err = log.try_clone().expect("run log");
                let child = command(&regia, args, &cwd)
                    .stdout(log)
                    .stderr(err)
                    .process_group(0)
                    .spawn()
                    .unwrap_or_else(|e| panic!("`{line}`: cannot start: {e}"));
                game.0 = Some(child);
                // The reader sees the window before doing anything else: the game this line starts
                // must come up before the next step (getting-started edits `lib.rs` next).
                wait_for_game(&regia, &hello, &cwd, &mut game, &run_log);
            }
            Step::AgentCall {
                args,
                method,
                params,
                out,
            } => {
                let stdout = if method == "agent.hello" {
                    wait_for_game(&regia, args, &cwd, &mut game, &run_log)
                } else {
                    run_to_end(line, command(&regia, args, &cwd))
                };
                if let Some(file) = out {
                    let shot = cwd.join(file);
                    check_shot(&load_screenshot(&shot, &stdout));
                    // Evidence for a report: `REGIA_WALKTHROUGH_SHOTS=<dir>` keeps a copy.
                    if let Some(dir) = std::env::var_os("REGIA_WALKTHROUGH_SHOTS") {
                        let name = page.replace(['/', '.'], "_");
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::copy(&shot, Path::new(&dir).join(format!("{name}.png")));
                    }
                    continue;
                }
                let result: Value = serde_json::from_str(&stdout)
                    .unwrap_or_else(|e| panic!("`{line}` did not print JSON ({e}): {stdout}"));
                match method.as_str() {
                    "play.wait_until" => assert_eq!(result["satisfied"], true, "`{line}`: {result}"),
                    "play.expect" => {
                        assert_eq!(result["pass"], true, "`{line}`: {result}");
                        let asked = params["assertions"].as_array().map_or(0, Vec::len);
                        let results = result["results"].as_array().expect("results");
                        assert_eq!(results.len(), asked, "one result per assertion: {result}");
                        assert!(results.iter().all(|r| r["pass"] == true), "{result}");
                    }
                    "quit.request" => assert_eq!(result["will_exit"], true, "`{line}`: {result}"),
                    "scene.query_entities" => {
                        let rows = result["entities"].as_array().expect("entities");
                        assert!(!rows.is_empty(), "`{line}` found nothing: {result}");
                        if let Some(name) = params["name"].as_str() {
                            assert!(
                                rows.iter().all(|row| row["name"] == name),
                                "`{line}` returned entities not named {name}: {result}"
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Back in the first terminal: `regia run` returns with exit status 0.
    expect_clean_exit(&mut game, &run_log);
}

#[test]
#[ignore = "builds the engine for a generated project and opens a window: run under xvfb-run with --ignored"]
fn the_first_agent_check_page_runs_as_written() {
    walk(PAGE, None, &|_| {});
}

#[test]
#[ignore = "builds the engine for a generated project and opens a window: run under xvfb-run with --ignored"]
fn the_getting_started_page_runs_as_written() {
    walk(GETTING_STARTED, Some(GETTING_STARTED_STOP), &|image| {
        // "You see an orange box under a blue procedural sky."
        let orange = share(image, |[r, g, b, _]| {
            r > 80 && i32::from(r) > i32::from(g) + 20 && i32::from(g) > i32::from(b) + 10
        });
        let sky = share(image, |[r, _, b, _]| b > 60 && i32::from(b) > i32::from(r) + 10);
        eprintln!("walkthrough: orange {orange:.4}, sky {sky:.4} of the pixels");
        assert!(
            orange >= 0.002,
            "no orange box in the screenshot ({orange:.4} of the pixels are orange)"
        );
        assert!(
            sky >= 0.05,
            "no blue sky in the screenshot ({sky:.4} of the pixels are sky blue)"
        );
    });
}

#[test]
fn page_getting_started_steps_one_to_five_are_a_walkthrough() {
    let steps = getting_started_steps();
    let kinds: Vec<&str> = steps
        .iter()
        .map(|s| match s {
            Step::Cd(_) => "cd",
            Step::New(_) => "new",
            Step::Cargo(_) => "cargo",
            Step::Run(_) => "run",
            Step::AddDeps(_) => "deps",
            Step::ReplaceLib(_) => "lib",
            Step::AgentCall { .. } => method_of(s).unwrap_or_default(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "new",
            "cd",
            "run",
            "deps",
            "lib",
            "run",
            "agent.hello",
            "scene.query_entities",
            "seeing.screenshot",
            "quit.request"
        ],
        "{GETTING_STARTED} steps 1 to 5 changed: update the walkthrough with the page"
    );
}

#[test]
fn page_script_reads_edits_only_where_the_prose_asks_for_them() {
    let text = "Add them to `game/Cargo.toml`:\n\n```toml\na = 1\n```\n\nThen replace `game/src/lib.rs` with:\n\n```rust\nfn x() {}\n```\n\nSome code:\n\n```rust\nfn y() {}\n```\n\n```toml\nb = 2\n```\n\n## 6. Later\n\n```console\n$ regia package\n```\n";
    let script = page_script(text, Some("## 6."));
    let steps: Vec<Step> = script.into_iter().map(|(_, s)| s.expect("known")).collect();
    assert_eq!(
        steps,
        [
            Step::AddDeps("a = 1\n".into()),
            Step::ReplaceLib("fn x() {}\n".into())
        ]
    );
}

#[test]
fn page_script_refuses_commands_in_fences_it_does_not_run() {
    let script = page_script("```sh\n$ regia run\n```\n\n```text\nplain output\n```\n", None);
    assert_eq!(script.len(), 1, "only the prompted line is reported");
    let (shown, step) = &script[0];
    assert_eq!(shown, "regia run");
    assert!(
        step.as_ref().is_err_and(|e| e.contains("```sh")),
        "a `$ ` line outside a console fence is an error: {step:?}"
    );
}
