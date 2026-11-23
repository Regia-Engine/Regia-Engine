// SPDX-License-Identifier: MPL-2.0
//! The console line (ADR-7754): one input line under the Output Log (its drawer and its tab) that
//! runs an editor command by id (`edit.undo`), an agent method with JSON parameters
//! (`editor.focus {"entity": 12}`) or a project console variable (`name` reads it, `name value`
//! sets it), with history (Up/Down, kept per project in [`HISTORY_FILE`]) and Tab completion over
//! command ids, method names and variable names, with each method's parameters from the schema.
//!
//! One code path: a method line is [`regia_agent::call_local`], the dispatcher route a client
//! request takes (permission classes, parameter gate, rate limits, audit and provenance); a command
//! id is `editor.command { run }` through the same call, so the console, the palette and an agent
//! cannot drift apart. The result is printed to the log (target [`LOG_TARGET`]) and kept in
//! [`ConsoleState::last`].
//!
//! Agent surface: `editor.console_run { line }` and `editor.console_complete { prefix }`
//! (`book/agents/editor-console.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_agent::{LocalReply, call_local};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The per-project history file, relative to the project root (per-user view state, `.regia/`).
pub const HISTORY_FILE: &str = ".regia/console_history.txt";
/// Lines the history keeps (oldest drop first).
pub const HISTORY_CAP: usize = 200;
/// The log target console lines and their results are printed under.
pub const LOG_TARGET: &str = "regia_editor::console";
/// Completions a reply or the hint row lists at most.
pub const COMPLETION_LIMIT: usize = 50;
/// Longest result text printed to the log (the full reply stays in [`ConsoleState::last`]).
const PRINT_CAP: usize = 2000;
/// Methods the console refuses to run (they would run the console inside itself).
const NESTED: &[&str] = &["editor.console_run", "editor.console_complete"];

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

// ---------------------------------------------------------------------------------------------
// Console variables

/// One project console variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleVar {
    /// The current value, as typed.
    pub value: String,
    /// What the variable does (shown by completion).
    pub help: String,
}

/// The console variables a project registers (ADR-7754). Empty unless something registers one:
/// an editor extension or the project's editor-side code calls [`ConsoleVars::register`]. A
/// variable is read with `name` and set with `name value` on the console line.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsoleVars {
    vars: BTreeMap<String, ConsoleVar>,
}

impl ConsoleVars {
    /// Registers (or replaces) `name` with its starting value and help text. A name that is a
    /// command id or a method name is shadowed by those.
    pub fn register(&mut self, name: &str, value: &str, help: &str) {
        self.vars.insert(
            name.to_string(),
            ConsoleVar {
                value: value.to_string(),
                help: help.to_string(),
            },
        );
    }

    /// The variable `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ConsoleVar> {
        self.vars.get(name)
    }

    /// Sets a registered variable; the previous value. `None` for an unknown name.
    pub fn set(&mut self, name: &str, value: &str) -> Option<String> {
        self.vars
            .get_mut(name)
            .map(|var| std::mem::replace(&mut var.value, value.to_string()))
    }

    /// Every variable, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ConsoleVar)> {
        self.vars.iter().map(|(name, var)| (name.as_str(), var))
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing

/// What a console line asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum ConsoleLine {
    /// An editor command id (`edit.undo`), run as `editor.command { run }`.
    Command(&'static str),
    /// An agent method and its parameters (`{}` when none were typed).
    Method {
        /// The method name.
        name: String,
        /// The parameters (a JSON object).
        params: Value,
    },
    /// A console variable: read it (`value: None`) or set it.
    Var {
        /// The variable name.
        name: String,
        /// The value to set.
        value: Option<String>,
    },
}

/// The schema of method `name`: the protocol table or an extension (the editor's methods).
fn method_schema(world: &World, name: &str) -> Option<MethodSchema> {
    regia_agent::schema::method(name).cloned().or_else(|| {
        world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .and_then(|ext| ext.method_schema(name).cloned())
    })
}

/// Parses one console line.
///
/// # Errors
/// `console_empty_line`, `console_unknown_name` (with the closest names), `console_bad_params`
/// (the text after a method name is not a JSON object), `console_nested` (the console's own
/// methods), `console_command_args` (a command id takes no arguments).
pub fn parse(world: &World, line: &str) -> Result<ConsoleLine, RpcError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(bad(
            "console_empty_line",
            "the line is empty",
            "type a command id, a method name or a variable; Tab completes",
        ));
    }
    let (head, rest) = match line.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (line, ""),
    };
    if let Some(def) = crate::command_palette::find(head) {
        if !rest.is_empty() {
            return Err(bad(
                "console_command_args",
                format!("{head} is a command id and takes no arguments"),
                "run the command alone, or call its agent method with parameters",
            ));
        }
        return Ok(ConsoleLine::Command(def.id));
    }
    if NESTED.contains(&head) {
        return Err(bad(
            "console_nested",
            format!("{head} cannot run from the console line"),
            "type the line itself instead",
        ));
    }
    if method_schema(world, head).is_some() {
        let params = if rest.is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(rest) {
                Ok(value @ Value::Object(_)) => value,
                Ok(_) => {
                    return Err(bad(
                        "console_bad_params",
                        format!("the parameters of {head} must be a JSON object"),
                        "for example {\"entity\": 12}",
                    ));
                }
                Err(err) => {
                    return Err(bad(
                        "console_bad_params",
                        format!("the parameters of {head} are not JSON: {err}"),
                        "for example {\"entity\": 12}",
                    ));
                }
            }
        };
        return Ok(ConsoleLine::Method {
            name: head.to_string(),
            params,
        });
    }
    if world
        .get_resource::<ConsoleVars>()
        .is_some_and(|vars| vars.get(head).is_some())
    {
        return Ok(ConsoleLine::Var {
            name: head.to_string(),
            value: (!rest.is_empty()).then(|| rest.to_string()),
        });
    }
    let near: Vec<String> = complete(world, head.get(..head.len().min(6)).unwrap_or(head))
        .into_iter()
        .take(5)
        .map(|c| c.text)
        .collect();
    Err(bad(
        "console_unknown_name",
        format!("{head} is not a command id, an agent method or a console variable"),
        if near.is_empty() {
            "Tab lists the names that start with what you typed".to_string()
        } else {
            format!("did you mean: {}", near.join(", "))
        },
    ))
}

// ---------------------------------------------------------------------------------------------
// Completion

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The full name.
    pub text: String,
    /// `command`, `method` or `var`.
    pub kind: &'static str,
    /// What it takes or does: a method's parameters, a command's title, a variable's value.
    pub hint: String,
}

/// A method's parameter hint: `entity: integer, frame?: boolean` (`?` = optional).
#[must_use]
pub fn param_hint(params: &[ParamSchema]) -> String {
    if params.is_empty() {
        return "no parameters".to_string();
    }
    params
        .iter()
        .map(|p| format!("{}{}: {}", p.name, if p.required { "" } else { "?" }, p.kind))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The names that start with `prefix`: command ids, agent methods (the protocol and the editor's
/// extension) and console variables, sorted by name, at most [`COMPLETION_LIMIT`].
#[must_use]
pub fn complete(world: &World, prefix: &str) -> Vec<Completion> {
    let mut out: BTreeMap<String, Completion> = BTreeMap::new();
    for def in crate::command_palette::COMMANDS.iter() {
        if def.id.starts_with(prefix) {
            out.insert(
                def.id.to_string(),
                Completion {
                    text: def.id.to_string(),
                    kind: "command",
                    hint: format!("{}: {}", def.category, def.title),
                },
            );
        }
    }
    let mut methods: Vec<MethodSchema> = regia_agent::schema::METHODS
        .iter()
        .filter(|m| m.name.starts_with(prefix))
        .cloned()
        .collect();
    if let Some(ext) = world.get_resource::<regia_agent::extension::AgentExtension>() {
        for name in ext.method_names() {
            if name.starts_with(prefix)
                && let Some(schema) = ext.method_schema(name)
            {
                methods.push(schema.clone());
            }
        }
    }
    for method in methods {
        if NESTED.contains(&method.name) {
            continue;
        }
        out.entry(method.name.to_string()).or_insert_with(|| Completion {
            text: method.name.to_string(),
            kind: "method",
            hint: param_hint(method.params),
        });
    }
    if let Some(vars) = world.get_resource::<ConsoleVars>() {
        for (name, var) in vars.iter() {
            if name.starts_with(prefix) {
                out.entry(name.to_string()).or_insert_with(|| Completion {
                    text: name.to_string(),
                    kind: "var",
                    hint: format!("= {} ({})", var.value, var.help),
                });
            }
        }
    }
    out.into_values().take(COMPLETION_LIMIT).collect()
}

/// The longest common prefix of `names`.
fn common_prefix<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut names = names.into_iter();
    let Some(first) = names.next() else {
        return String::new();
    };
    let mut len = first.len();
    for name in names {
        len = first
            .char_indices()
            .zip(name.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
            .min(len);
    }
    first[..len].to_string()
}

/// What Tab does to `input`: the line it becomes and the candidates to show. One candidate
/// completes the name and adds a space; several extend it to their common prefix; a line that
/// already has a name and a space shows that name's hint and stays as it is.
#[must_use]
pub fn tab_complete(world: &World, input: &str) -> (String, Vec<Completion>) {
    if let Some((head, _)) = input.split_once(char::is_whitespace) {
        let exact: Vec<Completion> = complete(world, head)
            .into_iter()
            .filter(|c| c.text == head)
            .collect();
        return (input.to_string(), exact);
    }
    let candidates = complete(world, input);
    match candidates.as_slice() {
        [] => (input.to_string(), Vec::new()),
        [only] => (format!("{} ", only.text), candidates.clone()),
        many => {
            let prefix = common_prefix(many.iter().map(|c| c.text.as_str()));
            let line = if prefix.len() > input.len() {
                prefix
            } else {
                input.to_string()
            };
            (line, candidates)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Running

/// A line the console ran and its reply.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleEntry {
    /// The line as typed.
    pub line: String,
    /// The reply: the agent call's reply for a command or a method, `{ var, value }` for a
    /// variable.
    pub result: Result<Value, RpcError>,
}

/// The client a line the person types is audited and stamped under (`agent.changes`).
pub const CONSOLE_CLIENT: &str = "editor:console";

/// Runs one console line (see the module docs). A method whose handler answers in a later frame
/// comes back [`LocalReply::Pending`]. `client` is [`regia_agent::call_local`]'s: [`CONSOLE_CLIENT`]
/// for a typed line, `None` for one an agent sent through `editor.console_run` (it stays the
/// agent's, and a scoped tool's call keeps its scope).
pub fn run_line(world: &mut World, line: &str, client: Option<&str>) -> LocalReply {
    let _span = tracing::info_span!(target: "regia_editor", "console_run").entered();
    let parsed = match parse(world, line) {
        Ok(parsed) => parsed,
        Err(err) => return LocalReply::Done(Err(err)),
    };
    match parsed {
        ConsoleLine::Command(id) => {
            call_local(world, "editor.command", json!({ "run": id, "limit": 1 }), client)
        }
        ConsoleLine::Method { name, params } => call_local(world, &name, params, client),
        ConsoleLine::Var { name, value } => {
            let mut vars = world.get_resource_or_insert_with(ConsoleVars::default);
            let result = match value {
                None => vars
                    .get(&name)
                    .map(|var| json!({ "var": name, "value": var.value })),
                Some(value) => vars
                    .set(&name, &value)
                    .map(|previous| json!({ "var": name, "value": value, "previous": previous })),
            };
            LocalReply::Done(result.ok_or_else(|| {
                bad(
                    "console_unknown_name",
                    format!("{name} is not a console variable"),
                    "",
                )
            }))
        }
    }
}

/// The short text the log shows for a reply.
#[must_use]
pub fn summary(line: &str, result: &Result<Value, RpcError>) -> String {
    let text = match result {
        Ok(value) => match value.pointer("/ran/message").and_then(Value::as_str) {
            // A command id: the palette's status line.
            Some(message) if crate::command_palette::find(line.trim()).is_some() => message.to_string(),
            _ => value.to_string(),
        },
        Err(err) => {
            let code = err
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            format!("error {code}: {}", err.message)
        }
    };
    if text.len() <= PRINT_CAP {
        return text;
    }
    let mut end = PRINT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

fn print_entry(entry: &ConsoleEntry) {
    let text = summary(&entry.line, &entry.result);
    match &entry.result {
        Ok(_) => tracing::info!(target: "regia_editor::console", "{text}"),
        Err(_) => tracing::warn!(target: "regia_editor::console", "{text}"),
    }
}

/// The console line's state (a world resource shared by the drawer, the Output Log tab and the
/// agent methods).
#[derive(Resource, Debug, Default)]
pub struct ConsoleState {
    /// The text in the line.
    pub input: String,
    /// The candidates Tab found last (shown under the line).
    pub completions: Vec<Completion>,
    /// The last line that finished and its reply.
    pub last: Option<ConsoleEntry>,
    /// Lines run in this session.
    pub ran: u64,
    history: Vec<String>,
    /// The history row Up/Down shows (`None`: the draft).
    cursor: Option<usize>,
    /// What was typed before Up started browsing.
    draft: String,
    /// The project root the history was loaded for (`Some(None)`: no project).
    loaded_for: Option<Option<PathBuf>>,
    /// Lines whose method answers in a later frame.
    /// (Each is a [`LocalReply::Pending`].)
    pending: Vec<(String, LocalReply)>,
    /// The frame whose keys the line handled (egui may run a frame's UI twice).
    keys_frame: Option<u64>,
    /// Focus a line on its next draw: `Some(None)` whichever draws first, `Some(Some(salt))` the
    /// line drawn with that salt (the log drawer's, while the docked tab's line also draws).
    want_focus: Option<Option<String>>,
}

impl ConsoleState {
    /// The history, oldest first.
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Whether a line still waits for its reply.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Asks the line to take the keyboard focus when it is next drawn.
    pub fn focus(&mut self) {
        self.want_focus = Some(None);
    }

    /// Asks the line drawn with `salt` (see [`console_line`]) to take the keyboard focus when it is
    /// next drawn; another line drawn first leaves the request alone.
    pub fn focus_line(&mut self, salt: &str) {
        self.want_focus = Some(Some(salt.to_string()));
    }

    /// Up (`-1`) or Down (`+1`) through the history: Up from the draft shows the newest line,
    /// Down past the newest returns to the draft.
    pub fn step_history(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        let newest = self.history.len() - 1;
        let next = match (self.cursor, delta < 0) {
            (None, true) => {
                self.draft = self.input.clone();
                Some(newest)
            }
            (None, false) => None,
            (Some(row), true) => Some(row.saturating_sub(1)),
            (Some(row), false) if row >= newest => None,
            (Some(row), false) => Some(row + 1),
        };
        self.cursor = next;
        self.input = match next {
            Some(row) => self.history[row].clone(),
            None => self.draft.clone(),
        };
    }
}

/// The history file of `root`.
#[must_use]
pub fn history_path(root: &Path) -> PathBuf {
    root.join(HISTORY_FILE)
}

/// Reads a history file: one line per entry, oldest first, at most [`HISTORY_CAP`]. A missing or
/// unreadable file is an empty history.
#[must_use]
pub fn read_history(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let skip = lines.len().saturating_sub(HISTORY_CAP);
    lines.into_iter().skip(skip).collect()
}

fn write_history(path: &Path, history: &[String]) {
    let mut text = history.join("\n");
    text.push('\n');
    if let Err(err) = crate::atomic_write::write_atomic(path, text.as_bytes()) {
        tracing::warn!(target: "regia_editor", "console history not saved: {err}");
    }
}

/// Loads the open project's history when the project changed since the last load.
fn sync_history(world: &World, state: &mut ConsoleState) {
    let root = crate::matgraph_editor::project_root(world);
    if state.loaded_for.as_ref() == Some(&root) {
        return;
    }
    state.history = root
        .as_deref()
        .map(history_path)
        .map(|p| read_history(&p))
        .unwrap_or_default();
    state.cursor = None;
    state.loaded_for = Some(root);
}

/// Records `line` in the history (a repeat of the newest line is not added again) and writes
/// the project's history file.
fn remember(world: &World, state: &mut ConsoleState, line: &str) {
    sync_history(world, state);
    if state.history.last().map(String::as_str) != Some(line) {
        state.history.push(line.to_string());
        let over = state.history.len().saturating_sub(HISTORY_CAP);
        state.history.drain(..over);
        if let Some(Some(root)) = &state.loaded_for {
            write_history(&history_path(root), &state.history);
        }
    }
    state.cursor = None;
    state.draft.clear();
}

/// Runs `line` as the person at the editor typed it: history, the run, the log line. Answers the
/// entry when the reply came now (`None` when it comes in a later frame, see [`poll`]). The call
/// is audited and stamped as [`CONSOLE_CLIENT`].
pub fn submit(world: &mut World, line: &str) -> Option<ConsoleEntry> {
    submit_as(world, line, Some(CONSOLE_CLIENT))
}

/// [`submit`] for a line that came from `client` (`None`: the caller of the current agent call).
fn submit_as(world: &mut World, line: &str, client: Option<&str>) -> Option<ConsoleEntry> {
    let line = line.trim().to_string();
    let mut state = world.remove_resource::<ConsoleState>().unwrap_or_default();
    if !line.is_empty() {
        remember(world, &mut state, &line);
    }
    world.insert_resource(state);
    tracing::info!(target: "regia_editor::console", "> {line}");
    let reply = run_line(world, &line, client);
    let mut state = world.remove_resource::<ConsoleState>().unwrap_or_default();
    state.ran += 1;
    state.completions.clear();
    let entry = match reply {
        LocalReply::Done(result) => {
            let entry = ConsoleEntry { line, result };
            print_entry(&entry);
            state.last = Some(entry.clone());
            Some(entry)
        }
        pending @ LocalReply::Pending(_) => {
            state.pending.push((line, pending));
            None
        }
    };
    world.insert_resource(state);
    entry
}

/// Prints the replies of lines whose method answered since the last call. Cheap when nothing
/// waits (the shell calls it every frame).
pub fn poll(world: &mut World) {
    let Some(mut state) = world.get_resource_mut::<ConsoleState>() else {
        return;
    };
    if state.pending.is_empty() {
        return;
    }
    let mut done = Vec::new();
    state.pending.retain(|(line, reply)| {
        let LocalReply::Pending(receiver) = reply else {
            return false;
        };
        match receiver.try_recv() {
            Ok(response) => {
                let result = match response.error {
                    Some(error) => Err(error),
                    None => Ok(response.result.unwrap_or(Value::Null)),
                };
                done.push(ConsoleEntry {
                    line: line.clone(),
                    result,
                });
                false
            }
            Err(err) if err.is_empty() => true,
            Err(_) => {
                done.push(ConsoleEntry {
                    line: line.clone(),
                    result: Err(bad("console_no_reply", "the method ended without a reply", "")),
                });
                false
            }
        }
    });
    for entry in done {
        print_entry(&entry);
        state.last = Some(entry);
    }
}

// ---------------------------------------------------------------------------------------------
// The line

/// The keys the line handles this frame.
#[derive(Debug, Default, Clone, Copy)]
struct LineKeys {
    enter: bool,
    tab: bool,
    /// Shift+Tab: leave the line backwards (the focus lock keeps Tab for completion).
    back: bool,
    up: bool,
    down: bool,
}

/// Draws the console line (and the completion hints under it). `salt` tells the drawer's line
/// from the docked tab's. Keyboard: Enter runs, Tab completes, Up/Down walk the history.
pub fn console_line(ui: &mut egui::Ui, world: &mut World, salt: &str) {
    let id = egui::Id::new(("regia-console-line", salt));
    let mut state = world.remove_resource::<ConsoleState>().unwrap_or_default();
    sync_history(world, &mut state);
    let frame = ui.ctx().cumulative_frame_nr();
    let focused = ui.memory(|m| m.has_focus(id));
    let mut keys = LineKeys::default();
    if focused && state.keys_frame != Some(frame) {
        state.keys_frame = Some(frame);
        keys = ui.input_mut(|i| {
            // `consume_key(NONE, Tab)` would also take Shift+Tab (a NONE pattern matches extra
            // Shift), so Shift+Tab is taken first.
            let back = i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab);
            LineKeys {
                enter: i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                tab: !back && i.consume_key(egui::Modifiers::NONE, egui::Key::Tab),
                back,
                up: i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                down: i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            }
        });
    }
    if keys.back {
        ui.memory_mut(|m| m.move_focus(egui::FocusDirection::Previous));
    }
    let mut move_to_end = false;
    if keys.up {
        state.step_history(-1);
        move_to_end = true;
    }
    if keys.down {
        state.step_history(1);
        move_to_end = true;
    }
    if keys.tab {
        let (line, candidates) = tab_complete(world, &state.input);
        state.input = line;
        state.completions = candidates;
        move_to_end = true;
    }
    let mut run = None;
    if keys.enter && !state.input.trim().is_empty() {
        run = Some(std::mem::take(&mut state.input));
    }
    let mut response = None;
    ui.horizontal(|ui| {
        let label = ui.label("Console");
        let edit = egui::TextEdit::singleline(&mut state.input)
            .id(id)
            .lock_focus(true)
            .hint_text("command id, method {json params} or variable (Tab completes)")
            .desired_width(f32::INFINITY);
        let r = ui.add(edit).labelled_by(label.id);
        if r.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: false,
                    },
                );
            });
        }
        response = Some(r);
    });
    let wanted = state
        .want_focus
        .as_ref()
        .is_some_and(|line| line.as_deref().is_none_or(|line| line == salt));
    if let Some(r) = &response
        && (wanted || keys.enter)
    {
        r.request_focus();
        if wanted {
            state.want_focus = None;
        }
    }
    if move_to_end && let Some(mut edit) = egui::text_edit::TextEditState::load(ui.ctx(), id) {
        let end = egui::text::CCursor::new(state.input.chars().count());
        edit.cursor
            .set_char_range(Some(egui::text::CCursorRange::one(end)));
        edit.store(ui.ctx(), id);
    }
    for completion in state.completions.iter().take(6) {
        ui.weak(format!("{}  {}", completion.text, completion.hint));
    }
    if state.completions.len() > 6 {
        ui.weak(format!("… {} more", state.completions.len() - 6));
    }
    world.insert_resource(state);
    if let Some(line) = run {
        let _ = submit(world, &line);
    }
}

// ---------------------------------------------------------------------------------------------
// Agent methods

/// `editor.console_run { line }`.
fn console_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let line = params
        .get("line")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("console_empty_line", "`line` (a string) is required", "edit.undo"))?;
    match submit_as(world, line, None) {
        Some(entry) => {
            let summary = summary(&entry.line, &entry.result);
            match entry.result {
                Ok(reply) => {
                    Ok(json!({ "line": entry.line, "reply": reply, "printed": summary, "pending": false }))
                }
                Err(err) => Err(err),
            }
        }
        None => Ok(json!({ "line": line.trim(), "reply": Value::Null, "printed": "", "pending": true })),
    }
}

/// `editor.console_complete { prefix }`.
fn console_complete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let prefix = match params.get("prefix") {
        None | Some(Value::Null) => "",
        Some(Value::String(prefix)) => prefix.as_str(),
        Some(_) => {
            return Err(bad(
                "console_bad_params",
                "`prefix` must be a string",
                "\"editor.fo\"",
            ));
        }
    };
    let (line, candidates) = tab_complete(world, prefix);
    Ok(json!({
        "prefix": prefix,
        "line": line,
        "count": candidates.len(),
        "completions": candidates
            .iter()
            .map(|c| json!({ "text": c.text, "kind": c.kind, "hint": c.hint }))
            .collect::<Vec<_>>(),
    }))
}

/// The console's agent methods, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.console_run",
                tool: "editor_console_run",
                description: "Runs one console line as typed under the Output Log: an editor command id (edit.undo), an agent method with JSON params (editor.focus {\"entity\": 12}) or a project console variable (name, or name value). Commands and methods go through the same dispatcher as an agent call (permissions apply); the line joins the project's console history and its result is printed to the log.",
                params: &[ParamSchema {
                    name: "line",
                    kind: "string",
                    required: true,
                    description: "The console line.",
                }],
                result: "{ line, reply (the called method's own reply, or { var, value, previous? }), printed, pending }",
                mutating: true,
                permission: Permission::EditScene,
            },
            console_run as Handler,
        ),
        (
            MethodSchema {
                name: "editor.console_complete",
                tool: "editor_console_complete",
                description: "What Tab does on the console line: the command ids, agent methods (with their parameters from the schema) and console variables that start with prefix, and the line Tab makes of it.",
                params: &[ParamSchema {
                    name: "prefix",
                    kind: "string",
                    required: false,
                    description: "The text typed so far (empty lists the first names).",
                }],
                result: "{ prefix, line, count, completions: [{ text, kind (command | method | var), hint }] }",
                mutating: false,
                permission: Permission::Read,
            },
            console_complete as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        world.init_resource::<regia_app::prelude::Time>();
        world.init_resource::<crate::command::UndoStack>();
        world.init_resource::<crate::selection::Selection>();
        world
    }

    #[test]
    fn a_line_parses_as_a_command_a_method_or_a_variable() {
        let mut world = world();
        world
            .get_resource_or_insert_with(ConsoleVars::default)
            .register("r.fog", "1", "fog on or off");
        assert_eq!(
            parse(&world, " edit.undo ").unwrap(),
            ConsoleLine::Command("edit.undo")
        );
        assert_eq!(
            parse(&world, "editor.focus {\"entity\": 3}").unwrap(),
            ConsoleLine::Method {
                name: "editor.focus".into(),
                params: json!({ "entity": 3 })
            }
        );
        assert_eq!(
            parse(&world, "editor.get_selection").unwrap(),
            ConsoleLine::Method {
                name: "editor.get_selection".into(),
                params: json!({})
            }
        );
        assert_eq!(
            parse(&world, "r.fog 0").unwrap(),
            ConsoleLine::Var {
                name: "r.fog".into(),
                value: Some("0".into())
            }
        );
        let code = |line: &str| {
            parse(&world, line).unwrap_err().data.unwrap()["code"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(code(""), "console_empty_line");
        assert_eq!(code("nope.nothing"), "console_unknown_name");
        assert_eq!(code("editor.focus [1]"), "console_bad_params");
        assert_eq!(code("editor.focus {oops"), "console_bad_params");
        assert_eq!(code("edit.undo now"), "console_command_args");
        assert_eq!(code("editor.console_run {}"), "console_nested");
    }

    #[test]
    fn tab_completes_a_method_and_shows_its_parameters() {
        let world = world();
        let (line, candidates) = tab_complete(&world, "editor.get_selectio");
        assert_eq!(line, "editor.get_selection ");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, "method");
        assert_eq!(candidates[0].hint, "no parameters");
        // Several candidates extend to their common prefix and are all listed.
        let (line, candidates) = tab_complete(&world, "edit.re");
        assert_eq!(line, "edit.redo ", "{candidates:?}");
        let (line, candidates) = tab_complete(&world, "view.toggle_");
        assert_eq!(line, "view.toggle_");
        assert!(candidates.len() > 1);
        assert!(candidates.iter().all(|c| c.text.starts_with("view.toggle_")));
        // A line with params shows the method's hint from the schema.
        let (line, hint) = tab_complete(&world, "editor.select {\"entities\": []}");
        assert_eq!(line, "editor.select {\"entities\": []}");
        assert_eq!(hint.len(), 1);
        assert!(hint[0].hint.contains("entities"), "{hint:?}");
        // The console's own methods are not offered.
        assert!(complete(&world, "editor.console").is_empty());
    }

    #[test]
    fn a_method_line_answers_what_the_agent_call_answers() {
        let mut world = world();
        let entity = world.spawn_empty().id();
        world
            .resource_mut::<crate::selection::Selection>()
            .select(entity, crate::selection::SelectMode::Replace);
        let direct = match call_local(&mut world, "editor.get_selection", json!({}), None) {
            LocalReply::Done(result) => result.unwrap(),
            LocalReply::Pending(_) => panic!("synchronous"),
        };
        let entry = submit(&mut world, "editor.get_selection").unwrap();
        assert_eq!(entry.result.unwrap(), direct);
        let via_agent = console_run(&mut world, &json!({ "line": "editor.get_selection {}" })).unwrap();
        assert_eq!(via_agent["reply"], direct);
    }

    #[test]
    fn a_command_line_runs_through_editor_command() {
        let mut world = world();
        world.init_resource::<crate::viewport::ViewportSettings>();
        let before = world.resource::<crate::viewport::ViewportSettings>().grid;
        let entry = submit(&mut world, "view.toggle_grid").unwrap();
        let reply = entry.result.unwrap();
        assert_eq!(reply["ran"]["id"], "view.toggle_grid");
        assert_ne!(world.resource::<crate::viewport::ViewportSettings>().grid, before);
        assert_eq!(
            summary("view.toggle_grid", &Ok(reply)),
            format!("Grid {}", if before { "off" } else { "on" })
        );
    }

    #[test]
    fn the_dispatcher_permission_applies_to_a_console_line() {
        let mut world = world();
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.read_only = true;
        world.insert_resource(permissions);
        let entry = submit(&mut world, "edit.undo").unwrap();
        let err = entry.result.unwrap_err();
        assert_eq!(err.code, regia_agent::protocol::PERMISSION_DENIED, "{err:?}");
        // A read method still runs.
        assert!(submit(&mut world, "editor.get_selection").unwrap().result.is_ok());
    }

    #[test]
    fn variables_read_and_set() {
        let mut world = world();
        world
            .get_resource_or_insert_with(ConsoleVars::default)
            .register("game.speed", "1.0", "time scale");
        let read = submit(&mut world, "game.speed").unwrap().result.unwrap();
        assert_eq!(read, json!({ "var": "game.speed", "value": "1.0" }));
        let set = submit(&mut world, "game.speed 2.5").unwrap().result.unwrap();
        assert_eq!(set["previous"], "1.0");
        assert_eq!(
            world.resource::<ConsoleVars>().get("game.speed").unwrap().value,
            "2.5"
        );
        // Completions are sorted by name, and `game.*` agent methods (`game.pause`, ...) sort
        // before the variable: find its entry.
        let completions = complete(&world, "game.");
        let speed = completions.iter().find(|c| c.text == "game.speed");
        assert_eq!(speed.map(|c| c.kind), Some("var"), "{completions:?}");
    }

    #[test]
    fn history_steps_and_survives_a_restart() {
        let root = regia_test_utils::unique_temp_dir("console-history");
        let mut world = world();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        for line in ["edit.undo", "edit.redo", "edit.redo", "editor.get_selection"] {
            let _ = submit(&mut world, line);
        }
        let state = world.resource::<ConsoleState>();
        assert_eq!(
            state.history(),
            ["edit.undo", "edit.redo", "editor.get_selection"]
        );
        assert_eq!(
            read_history(&history_path(&root)),
            ["edit.undo", "edit.redo", "editor.get_selection"]
        );
        // A new session (a restart) reads the file.
        let mut world = self::world();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let mut state = ConsoleState {
            input: "draft".into(),
            ..ConsoleState::default()
        };
        sync_history(&world, &mut state);
        state.step_history(-1);
        assert_eq!(state.input, "editor.get_selection");
        state.step_history(-1);
        state.step_history(-1);
        state.step_history(-1);
        assert_eq!(state.input, "edit.undo", "Up stops at the oldest line");
        state.step_history(1);
        assert_eq!(state.input, "edit.redo");
        state.step_history(1);
        state.step_history(1);
        assert_eq!(state.input, "draft", "Down past the newest returns to the draft");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_history_keeps_the_newest_lines() {
        let root = regia_test_utils::unique_temp_dir("console-history-cap");
        let path = history_path(&root);
        let lines: Vec<String> = (0..HISTORY_CAP + 7).map(|n| format!("line {n}")).collect();
        write_history(&path, &lines);
        let read = read_history(&path);
        assert_eq!(read.len(), HISTORY_CAP);
        assert_eq!(read[0], "line 7");
        let _ = std::fs::remove_dir_all(root);
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One headless frame of the console line with `events`.
    fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 300.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| console_line(ui, world, "test"));
        });
        output.textures_delta.clear();
    }

    /// The keyboard path: focus, type, Tab completes, Enter runs, Up recalls.
    #[test]
    fn the_line_works_from_the_keyboard() {
        let mut world = world();
        world.get_resource_or_insert_with(ConsoleState::default).focus();
        let ctx = egui::Context::default();
        frame(&ctx, &mut world, vec![]);
        frame(&ctx, &mut world, vec![]);
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::Text("editor.get_selec".into())],
        );
        assert_eq!(world.resource::<ConsoleState>().input, "editor.get_selec");
        frame(&ctx, &mut world, vec![key(egui::Key::Tab)]);
        assert_eq!(world.resource::<ConsoleState>().input, "editor.get_selection ");
        frame(&ctx, &mut world, vec![key(egui::Key::Enter)]);
        let state = world.resource::<ConsoleState>();
        assert_eq!(state.ran, 1);
        assert!(state.input.is_empty());
        assert_eq!(state.last.as_ref().unwrap().line, "editor.get_selection");
        assert!(state.last.as_ref().unwrap().result.is_ok());
        frame(&ctx, &mut world, vec![key(egui::Key::ArrowUp)]);
        assert_eq!(world.resource::<ConsoleState>().input, "editor.get_selection");
    }

    /// A line the person types is stamped with the console as its client, not with the connected
    /// agent session (review w12-rev-early-k row 24).
    #[test]
    fn a_typed_line_is_stamped_as_the_console() {
        let mut world = world();
        world.init_resource::<crate::viewport::ViewportSettings>();
        world.get_resource_or_insert_with(ConsoleState::default).focus();
        let ctx = egui::Context::default();
        frame(&ctx, &mut world, vec![]);
        frame(&ctx, &mut world, vec![]);
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::Text("view.toggle_grid".into())],
        );
        frame(&ctx, &mut world, vec![key(egui::Key::Enter)]);
        assert_eq!(world.resource::<ConsoleState>().ran, 1);
        let last_client = |world: &mut World| {
            let LocalReply::Done(Ok(page)) = call_local(world, "agent.changes", json!({}), None) else {
                panic!("agent.changes answers");
            };
            let last = page["records"].as_array().and_then(|r| r.last()).cloned();
            let last = last.expect("the command is recorded");
            assert_eq!(last["method"], "editor.command");
            last["client"].as_str().unwrap_or_default().to_string()
        };
        assert_eq!(last_client(&mut world), CONSOLE_CLIENT);
        // The same line from an agent's editor.console_run stays the caller's.
        console_run(&mut world, &json!({ "line": "view.toggle_grid" })).unwrap();
        assert_ne!(last_client(&mut world), CONSOLE_CLIENT);
    }

    /// Shift+Tab leaves the line (focus moves back) instead of completing (review
    /// w12-rev-early-k row 20).
    #[test]
    fn shift_tab_leaves_the_line() {
        let mut world = world();
        world.get_resource_or_insert_with(ConsoleState::default).focus();
        let ctx = egui::Context::default();
        let mut before = egui::Id::NULL;
        let mut run = |world: &mut World, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 300.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    before = ui.button("before").id;
                    console_line(ui, world, "test");
                });
            });
            output.textures_delta.clear();
        };
        run(&mut world, vec![]);
        run(&mut world, vec![]);
        run(&mut world, vec![egui::Event::Text("editor.get_selec".into())]);
        let line = egui::Id::new(("regia-console-line", "test"));
        assert!(ctx.memory(|m| m.has_focus(line)));
        run(
            &mut world,
            vec![egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::SHIFT,
            }],
        );
        run(&mut world, vec![]);
        assert_eq!(
            world.resource::<ConsoleState>().input,
            "editor.get_selec",
            "Shift+Tab does not complete"
        );
        assert!(
            !ctx.memory(|m| m.has_focus(line)),
            "Shift+Tab moves the focus off the line"
        );
        assert!(ctx.memory(|m| m.has_focus(before)), "to the widget before it");
    }

    #[test]
    fn the_agent_methods_answer() {
        let mut world = world();
        let out = console_complete(&mut world, &json!({ "prefix": "editor.get_selectio" })).unwrap();
        assert_eq!(out["line"], "editor.get_selection ");
        assert_eq!(out["completions"][0]["hint"], "no parameters");
        let err = console_run(&mut world, &json!({})).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "console_empty_line");
        let err = console_complete(&mut world, &json!({ "prefix": 3 })).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "console_bad_params");
    }
}
