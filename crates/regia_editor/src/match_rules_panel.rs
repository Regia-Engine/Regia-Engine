// SPDX-License-Identifier: MPL-2.0
//! The Match Rules panel (ADR-8738, `book/agents/editor-match-rules.md`): rule sets and playlists
//! authored in the editor, and the Game Rules picker of World Settings.
//!
//! - **Rule set.** `editor.rules_open` opens a project `.rules.ron` (or starts a new one). The form
//!   is generated from the format's fields: the file's own fields (`id`, `name_key`, `skeleton`,
//!   `extends`, `teams`, `win_conditions`, `options`) and every field of every session section
//!   (`FlowPatch::FIELDS`, `PhasePatch::FIELDS`, ...). `editor.rules_set_field` writes one value at a
//!   field path (`session.objectives.capture_limit`, `options[0].default`; `null` clears a field so
//!   the parent's value applies). Each edit is one [`EditRuleSet`] on the editor's undo stack, and
//!   after each edit the rule set is checked and resolved against the project's library, so the
//!   problems show inline with their `ruleset_*` codes. `editor.rules_save` refuses while a problem
//!   is listed (`match_rules_save_blocked`) and otherwise writes the file atomically (temp file and
//!   rename), then reloads the project library so the Game Rules picker lists the new id.
//! - **Playlist.** `editor.playlist_edit` opens a `.playlist.ron` and edits its rows (add, remove,
//!   move, set, looping), each an [`EditPlaylist`] undo step. Every row's options are validated
//!   against the rule set its `mode` names; a row option the schema does not declare, or a value
//!   out of range, blocks the save. A mode that names no rule set and no skeleton is a game's own
//!   mode: it is listed as a warning, not a blocker (S-14 behavior).
//! - **Game Rules picker.** World Settings draws `LevelSettings.game_rules` as a combo of the names
//!   `GameRulesRegistry::registered_names` lists (the edit world's registry, the play world's last
//!   seen registry and the engine's built-ins) plus the project's rule sets. A name in neither list
//!   is kept as free text and marked "custom" with the `game_rules_unregistered` warning chip.
//!
//! Play-in-editor match controls, the live state and the overlays are in `crate::match_rules_play`.
//!
//! Rule sets and playlists are files, not level state: their undo steps do not mark the level dirty.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::rules::playlist::PlaylistEntry;
use regia_framework::rules::ruleset::{
    FlowPatch, ObjectivePatch, PhasePatch, RespawnPatch, RuleSet, RuleSetError, TeamsPatch,
};
use regia_framework::ruleset_file::{MAX_RULES_RON_BYTES, RuleSetRegistry, parse_rules_ron, rules_ron_text};
use regia_net::formats::{PLAYLIST_RON_FORMAT_VERSION, PlaylistFile};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::viewport_agent::Handler;

/// Error and warning codes of the Match Rules panel (`book/agents/editor-match-rules.md`). Rule-set
/// problems keep their own `ruleset_*` codes; refused match inputs keep the rules' `match_*` codes.
pub mod codes {
    /// The editor has no project directory.
    pub const NO_PROJECT: &str = "match_rules_no_project";
    /// The path is not a project-relative `.rules.ron` / `.playlist.ron` path.
    pub const BAD_PATH: &str = "match_rules_bad_path";
    /// The file could not be read (missing without `create`, or an IO error).
    pub const READ_FAILED: &str = "match_rules_read_failed";
    /// No rule set is open in the panel.
    pub const NOT_OPEN: &str = "match_rules_not_open";
    /// The field path names no field, or the value does not fit the field.
    pub const BAD_FIELD: &str = "match_rules_bad_field";
    /// The section holds a value the form cannot keep (a `Some(None)` "clear the parent's value");
    /// edit that file as text.
    pub const FIELD_UNSUPPORTED: &str = "match_rules_field_unsupported";
    /// Save refused: the rule set has problems (`data.errors` lists them with their codes).
    pub const SAVE_BLOCKED: &str = "match_rules_save_blocked";
    /// The atomic write failed.
    pub const WRITE_FAILED: &str = "match_rules_write_failed";
    /// No playlist is open in the panel.
    pub const PLAYLIST_NOT_OPEN: &str = "playlist_not_open";
    /// `op` is not one of the playlist operations, or a parameter it needs is missing.
    pub const PLAYLIST_BAD_OP: &str = "playlist_bad_op";
    /// A row index out of range, or a row that does not read (`map_id`, `mode`, `options`).
    pub const PLAYLIST_BAD_ROW: &str = "playlist_bad_row";
    /// Save refused: a row has a blocking problem, or the playlist has no row.
    pub const PLAYLIST_SAVE_BLOCKED: &str = "playlist_save_blocked";
    /// Warning chip: `LevelSettings.game_rules` names neither a registered rules type nor a rule set.
    pub const GAME_RULES_UNREGISTERED: &str = "game_rules_unregistered";
    /// Warning on a playlist row: its mode names no rule set and no skeleton (a game's own mode).
    pub const PLAYLIST_MODE_UNKNOWN: &str = "playlist_mode_unknown";
    /// `editor.match_control` while no play session runs.
    pub const MATCH_NOT_PLAYING: &str = "match_control_not_playing";
    /// `editor.match_control` with an unknown `action` or a missing parameter.
    pub const MATCH_BAD_ACTION: &str = "match_control_bad_action";
    /// A match control that needs a session found none in the play world (`start` creates one).
    pub const MATCH_NO_SESSION: &str = "match_control_no_session";
    /// `step` while the match is not paused.
    pub const MATCH_NOT_PAUSED: &str = "match_control_not_paused";
    /// `add_bot` on a team with no free seat.
    pub const MATCH_NO_SEAT: &str = "match_control_no_seat";
    /// `remove_bot` on a team with no bot seat.
    pub const MATCH_NO_BOT: &str = "match_control_no_bot";
    /// `next_row` with a row index past the playlist.
    pub const MATCH_BAD_ROW: &str = "match_control_bad_row";
    /// An open would replace a rule set or playlist with unsaved edits: save it, or pass
    /// `discard: true`.
    pub const DIRTY: &str = "match_rules_dirty";

    /// Every code, sorted by its text.
    pub const ALL: [&str; 22] = [
        GAME_RULES_UNREGISTERED,
        MATCH_BAD_ACTION,
        MATCH_BAD_ROW,
        MATCH_NO_BOT,
        MATCH_NO_SEAT,
        MATCH_NO_SESSION,
        MATCH_NOT_PAUSED,
        MATCH_NOT_PLAYING,
        BAD_FIELD,
        BAD_PATH,
        DIRTY,
        FIELD_UNSUPPORTED,
        NO_PROJECT,
        NOT_OPEN,
        READ_FAILED,
        SAVE_BLOCKED,
        WRITE_FAILED,
        PLAYLIST_BAD_OP,
        PLAYLIST_BAD_ROW,
        PLAYLIST_MODE_UNKNOWN,
        PLAYLIST_NOT_OPEN,
        PLAYLIST_SAVE_BLOCKED,
    ];
}

/// File suffix of a rule set.
pub const RULES_SUFFIX: &str = ".rules.ron";
/// File suffix of a playlist.
pub const PLAYLIST_SUFFIX: &str = ".playlist.ron";
/// The largest `.playlist.ron` the panel reads, in bytes.
pub const MAX_PLAYLIST_BYTES: usize = 256 * 1024;
/// The most rows one playlist holds in the editor.
pub const MAX_PLAYLIST_ROWS: usize = 1024;

/// The file's own fields the form lists before the session sections.
pub const TOP_FIELDS: [&str; 7] = [
    "id",
    "name_key",
    "skeleton",
    "extends",
    "teams",
    "win_conditions",
    "options",
];

/// The session sections and their fields, in the format's order (`session.<section>.<field>`).
#[must_use]
pub fn session_sections() -> [(&'static str, &'static [&'static str]); 6] {
    [
        ("flow", FlowPatch::FIELDS),
        ("phases", PhasePatch::FIELDS),
        ("objectives", ObjectivePatch::FIELDS),
        ("teams", TeamsPatch::FIELDS),
        ("respawn", RespawnPatch::FIELDS),
        ("zones", &["mode", "score_limit"]),
    ]
}

/// The open rule set.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleSetDoc {
    /// Project-relative path (`assets/rules/ctf_long.rules.ron`).
    pub path: String,
    /// The rule set as edited.
    pub set: RuleSet,
    /// The rule set as last read or saved (`None` for a new file).
    pub saved: Option<RuleSet>,
    /// The problems of [`RuleSetDoc::set`]: its own check, then the resolution against the project.
    pub errors: Vec<RuleSetError>,
}

impl RuleSetDoc {
    /// Whether the edited set differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.set)
    }
}

/// One problem of a playlist row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowProblem {
    /// The row index.
    pub row: usize,
    /// A `ruleset_*` code, [`codes::PLAYLIST_BAD_ROW`] or [`codes::PLAYLIST_MODE_UNKNOWN`].
    pub code: String,
    /// The field path.
    pub path: String,
    /// What is wrong.
    pub message: String,
    /// Whether it blocks the save (a warning does not).
    pub blocking: bool,
}

/// The open playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistDoc {
    /// Project-relative path (`assets/rotation.playlist.ron`).
    pub path: String,
    /// The rows and the loop switch as edited.
    pub file: PlaylistFile,
    /// As last read or saved (`None` for a new file).
    pub saved: Option<PlaylistFile>,
    /// Row problems, row order.
    pub problems: Vec<RowProblem>,
}

impl PlaylistDoc {
    /// Whether the edited playlist differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.file)
    }
}

/// The panel's state (edit-world resource; view state, never saved with the level).
#[derive(Resource, Debug, Clone, Default)]
pub struct MatchRulesPanel {
    /// The open rule set.
    pub rules: Option<RuleSetDoc>,
    /// The open playlist.
    pub playlist: Option<PlaylistDoc>,
    /// The last status line.
    pub status: String,
    /// The form's text buffers by field path (UI only).
    pub drafts: BTreeMap<String, String>,
    /// The path typed into the panel's rule-set box (UI only).
    pub rules_path_input: String,
    /// The path typed into the panel's playlist box (UI only).
    pub playlist_path_input: String,
    /// Names the play world's `GameRulesRegistry` listed when a session last ran (a game crate's
    /// own rules types).
    pub play_rule_names: Vec<String>,
    /// The egui frame the panel last refreshed its library on (egui may run a pass twice).
    pub last_frame: Option<u64>,
}

/// The project's rule-set library as the panel and the picker see it: the shipped defaults plus
/// `assets/rules/*.rules.ron`, loaded once per project and again after each save.
#[derive(Resource, Debug, Clone)]
pub struct ProjectRules {
    /// The library.
    pub registry: RuleSetRegistry,
    /// The project root it was loaded from.
    pub root: Option<PathBuf>,
    /// Counts loads, so caches built from the library know when it changed.
    pub generation: u64,
}

fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

/// The project's rule sets, loaded when the project changed (or `reload`).
pub fn project_rules(world: &mut World, reload: bool) -> &RuleSetRegistry {
    let root = project_root(world);
    let stale = reload
        || world
            .get_resource::<ProjectRules>()
            .is_none_or(|cached| cached.root != root);
    if stale {
        let generation = world.get_resource::<ProjectRules>().map_or(0, |p| p.generation) + 1;
        let mut registry = RuleSetRegistry::with_defaults();
        if let Some(root) = &root {
            registry.load_project(root);
        }
        world.insert_resource(ProjectRules {
            registry,
            root,
            generation,
        });
    }
    &world.resource::<ProjectRules>().registry
}

// ---------------------------------------------------------------------------------------------
// Game Rules picker
// ---------------------------------------------------------------------------------------------

/// What the Game Rules combo offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameRulesChoices {
    /// `GameRulesRegistry::registered_names`: the edit world's registry, the play world's last
    /// seen one and the engine's built-ins, sorted and without duplicates.
    pub registered: Vec<String>,
    /// The project's rule-set ids (shipped defaults included), sorted.
    pub rule_sets: Vec<String>,
}

impl GameRulesChoices {
    /// `none` (no name), `registered`, `rule_set` or `custom` (neither: the warning chip).
    #[must_use]
    pub fn status(&self, name: Option<&str>) -> &'static str {
        match name {
            None => "none",
            Some(name) if self.registered.iter().any(|n| n == name) => "registered",
            Some(name) if self.rule_sets.iter().any(|n| n == name) => "rule_set",
            Some(_) => "custom",
        }
    }
}

fn builtin_rule_names() -> Vec<String> {
    let mut registry = regia_framework::rules::GameRulesRegistry::default();
    registry.register_default::<regia_framework::rules::DefaultRules>();
    registry.register_default::<regia_framework::rules::MatchSessionRules>();
    registry
        .registered_names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The names the Game Rules combo lists.
pub fn game_rules_choices(world: &mut World) -> GameRulesChoices {
    let mut registered = builtin_rule_names();
    if let Some(registry) = world.get_resource::<regia_framework::rules::GameRulesRegistry>() {
        registered.extend(registry.registered_names().into_iter().map(str::to_owned));
    }
    if let Some(panel) = world.get_resource::<MatchRulesPanel>() {
        registered.extend(panel.play_rule_names.iter().cloned());
    }
    registered.sort();
    registered.dedup();
    let rule_sets = project_rules(world, false)
        .library()
        .iter()
        .map(|set| set.id.clone())
        .collect();
    GameRulesChoices {
        registered,
        rule_sets,
    }
}

/// `game_rules` of `editor.world_settings_get`: the current name, its status and the choices.
pub fn game_rules_json(world: &mut World) -> Value {
    let current = regia_scene::level::current_settings(world).game_rules;
    let choices = game_rules_choices(world);
    let status = choices.status(current.as_deref());
    json!({
        "current": current,
        "status": status,
        "warning": (status == "custom").then_some(codes::GAME_RULES_UNREGISTERED),
        "registered": choices.registered,
        "rule_sets": choices.rule_sets,
    })
}

/// Sets `LevelSettings.game_rules` as one undo step (the combo's pick). Answers whether it changed.
pub fn pick_game_rules(world: &mut World, name: Option<String>) -> bool {
    let mut next = regia_scene::level::current_settings(world);
    next.game_rules = name;
    crate::world_settings::apply_settings(world, next, "game_rules", false)
}

/// Warning-chip color.
const WARN: egui::Color32 = egui::Color32::from_rgb(232, 170, 60);
/// Error-row color.
const ERROR: egui::Color32 = egui::Color32::from_rgb(230, 90, 80);

/// The World Settings panel's Game Rules row: a combo of the registered names and the project's
/// rule sets, with the custom-name warning chip.
pub fn game_rules_row(ui: &mut egui::Ui, world: &mut World) {
    let current = regia_scene::level::current_settings(world).game_rules;
    let choices = game_rules_choices(world);
    let status = choices.status(current.as_deref());
    let mut picked: Option<Option<String>> = None;
    ui.horizontal(|ui| {
        ui.label("Game Rules");
        let shown = current
            .clone()
            .unwrap_or_else(|| "(the game's own rules)".to_owned());
        egui::ComboBox::from_id_salt("world-settings-game-rules")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(current.is_none(), "(the game's own rules)")
                    .clicked()
                {
                    picked = Some(None);
                }
                ui.separator();
                ui.weak("Registered rules");
                for name in &choices.registered {
                    let on = current.as_deref() == Some(name.as_str());
                    if ui.selectable_label(on, name.as_str()).clicked() {
                        picked = Some(Some(name.clone()));
                    }
                }
                ui.separator();
                ui.weak("Rule sets");
                for id in &choices.rule_sets {
                    let on = current.as_deref() == Some(id.as_str());
                    if ui.selectable_label(on, id.as_str()).clicked() {
                        picked = Some(Some(id.clone()));
                    }
                }
            });
        if status == "custom" {
            ui.colored_label(WARN, "⚠ custom").on_hover_text(format!(
                "{}: no registered rules type and no rule set has this name; the level falls back to \
                     DefaultRules. Edit the text below to keep a custom name.",
                codes::GAME_RULES_UNREGISTERED
            ));
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::MATCH_RULES_OPEN_PANEL)
            .on_hover_text("Edit rule sets and playlists")
            .clicked()
        {
            crate::editor_layout::LayoutRequests::push(
                world,
                crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::MatchRules),
            );
        }
    });
    if let Some(name) = picked {
        pick_game_rules(world, name);
    }
}

// ---------------------------------------------------------------------------------------------
// Rule-set documents and field edits
// ---------------------------------------------------------------------------------------------

/// The problems of `set` in the project's library: its own check first, then the resolution.
pub fn validate_set(world: &mut World, set: &RuleSet) -> Vec<RuleSetError> {
    if let Err(error) = set.check() {
        return vec![error];
    }
    let mut library = project_rules(world, false).library().clone();
    if let Err(error) = library.replace(set.clone()) {
        return vec![error];
    }
    match library.resolve(&set.id, &BTreeMap::new()) {
        Ok(_) => Vec::new(),
        Err(error) => vec![error],
    }
}

/// One edit of the open rule set: the whole set before and after.
pub struct EditRuleSet {
    /// The document's path (an edit applies only to the document it was made on).
    pub path: String,
    /// The set before.
    pub old: RuleSet,
    /// The set after.
    pub new: RuleSet,
    /// The edited field path.
    pub field: String,
}

fn set_doc_set(world: &mut World, path: &str, set: &RuleSet) {
    let errors = validate_set(world, set);
    let mut panel = world.get_resource_or_insert_with(MatchRulesPanel::default);
    if let Some(doc) = panel.rules.as_mut()
        && doc.path == path
    {
        doc.set = set.clone();
        doc.errors = errors;
    }
    panel.drafts.clear();
}

impl EditorCommand for EditRuleSet {
    fn apply(&mut self, world: &mut World) {
        set_doc_set(world, &self.path, &self.new);
    }

    fn revert(&mut self, world: &mut World) {
        set_doc_set(world, &self.path, &self.old);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        // Each committed field is its own step.
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Rule set {}: {}", self.new.id, self.field)
    }

    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (Vec::new(), vec![self.path.clone()])
    }
}

/// One segment of a field path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

fn parse_path(path: &str) -> Option<Vec<Seg>> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let (name, rest) = part.split_once('[').unwrap_or((part, ""));
        if name.is_empty() && out.is_empty() {
            return None;
        }
        if !name.is_empty() {
            if !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return None;
            }
            out.push(Seg::Key(name.to_owned()));
        }
        if !rest.is_empty() {
            for index in format!("[{rest}").split('[').skip(1) {
                let digits = index.strip_suffix(']')?;
                out.push(Seg::Index(digits.parse().ok()?));
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Writes `value` at `path` inside `node`. An object key must exist (a misspelled field is an
/// error); an index may name the next element of an array (an append).
fn put(node: &mut Value, path: &[Seg], value: Value) -> Result<(), String> {
    let Some((first, rest)) = path.split_first() else {
        *node = value;
        return Ok(());
    };
    match (first, node) {
        (Seg::Key(key), Value::Object(map)) => {
            let slot = map.get_mut(key).ok_or_else(|| format!("no field `{key}`"))?;
            put(slot, rest, value)
        }
        (Seg::Index(index), Value::Array(list)) => {
            if *index == list.len() && rest.is_empty() {
                list.push(value);
                return Ok(());
            }
            let len = list.len();
            let slot = list
                .get_mut(*index)
                .ok_or_else(|| format!("index {index} is past the {len} entries"))?;
            put(slot, rest, value)
        }
        (Seg::Index(_), Value::Null) => Err("the field is not set: set the whole list first".to_owned()),
        (Seg::Key(key), _) => Err(format!("`{key}` is not inside an object")),
        (Seg::Index(index), _) => Err(format!("[{index}] is not inside a list")),
    }
}

/// `value` with `new` at `path`, through JSON: refused when `value` itself does not survive the
/// round trip (a `Some(None)` the form cannot keep).
fn patch_json<T: Serialize + DeserializeOwned + PartialEq>(
    value: &T,
    path: &[Seg],
    new: Value,
) -> Result<T, (&'static str, String)> {
    let mut tree =
        serde_json::to_value(value).map_err(|e| (codes::BAD_FIELD, format!("the value as JSON: {e}")))?;
    let back: T = serde_json::from_value(tree.clone()).map_err(|e| {
        (
            codes::FIELD_UNSUPPORTED,
            format!("the section does not read back: {e}"),
        )
    })?;
    if &back != value {
        return Err((
            codes::FIELD_UNSUPPORTED,
            "this section holds a cleared value (`Some(None)`) the form cannot keep; edit the file as text"
                .to_owned(),
        ));
    }
    put(&mut tree, path, new).map_err(|e| (codes::BAD_FIELD, e))?;
    serde_json::from_value(tree).map_err(|e| (codes::BAD_FIELD, format!("the value does not fit: {e}")))
}

/// `set` with `value` written at field `path` (see the module docs).
///
/// # Errors
///
/// `(code, message)`: [`codes::BAD_FIELD`] or [`codes::FIELD_UNSUPPORTED`].
pub fn set_field(set: &RuleSet, path: &str, value: Value) -> Result<RuleSet, (&'static str, String)> {
    let segs = parse_path(path).ok_or((codes::BAD_FIELD, format!("`{path}` is not a field path")))?;
    let Some(Seg::Key(head)) = segs.first() else {
        return Err((codes::BAD_FIELD, format!("`{path}` is not a field path")));
    };
    let mut out = set.clone();
    if head == "session" {
        let Some(Seg::Key(section)) = segs.get(1) else {
            return Err((
                codes::BAD_FIELD,
                "name a session section (session.objectives.capture_limit)".to_owned(),
            ));
        };
        let rest = &segs[2..];
        if rest.is_empty() {
            return Err((codes::BAD_FIELD, "name a field of the section".to_owned()));
        }
        let patch = &mut out.session;
        match section.as_str() {
            "flow" => patch.flow = patch_json(&patch.flow, rest, value)?,
            "phases" => patch.phases = patch_json(&patch.phases, rest, value)?,
            "objectives" => patch.objectives = patch_json(&patch.objectives, rest, value)?,
            "teams" => patch.teams = patch_json(&patch.teams, rest, value)?,
            "respawn" => patch.respawn = patch_json(&patch.respawn, rest, value)?,
            "zones" => patch.zones = patch_json(&patch.zones, rest, value)?,
            other => return Err((codes::BAD_FIELD, format!("no session section `{other}`"))),
        }
        return Ok(out);
    }
    if head == "format_version" {
        return Err((
            codes::BAD_FIELD,
            "format_version is the file's, not a field to edit".to_owned(),
        ));
    }
    if !TOP_FIELDS.contains(&head.as_str()) {
        return Err((codes::BAD_FIELD, format!("no field `{head}`")));
    }
    // The file without its session: a `Some(None)` there never blocks a top-level edit.
    let session = std::mem::take(&mut out.session);
    let mut patched = patch_json(&out, &segs, value)?;
    patched.session = session;
    Ok(patched)
}

/// The form rows: `(path, value)` for every field the format has, in the format's order. An unset
/// field is `null` (the parent's value, or the Rust default at the root, applies).
pub fn form_rows(set: &RuleSet) -> Vec<(String, Value)> {
    let mut rows = Vec::new();
    let top = serde_json::to_value(set).unwrap_or(Value::Null);
    for field in TOP_FIELDS {
        rows.push((field.to_owned(), top.get(field).cloned().unwrap_or(Value::Null)));
    }
    let session = top.get("session").cloned().unwrap_or(Value::Null);
    for (section, fields) in session_sections() {
        let values = session.get(section).cloned().unwrap_or(Value::Null);
        for field in fields {
            rows.push((
                format!("session.{section}.{field}"),
                values.get(*field).cloned().unwrap_or(Value::Null),
            ));
        }
    }
    rows
}

fn refused(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, code, message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(code));
    }
    error
}

fn error_json(error: &RuleSetError) -> Value {
    json!({ "code": error.code, "path": error.path, "message": error.message })
}

/// A project-relative path with `suffix`, resolved under the project root.
fn project_file(world: &World, path: &str, suffix: &str) -> Result<PathBuf, RpcError> {
    let root = project_root(world).ok_or_else(|| {
        refused(
            codes::NO_PROJECT,
            "the editor has no project directory",
            "open a project first",
        )
    })?;
    let rel = Path::new(path);
    let clean = !path.is_empty()
        && path.to_ascii_lowercase().ends_with(suffix)
        && rel.is_relative()
        && rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !clean {
        return Err(refused(
            codes::BAD_PATH,
            format!("`{path}` is not a project-relative `{suffix}` path"),
            format!("e.g. assets/rules/ctf_long{suffix}"),
        ));
    }
    Ok(root.join(rel))
}

/// Reads at most `cap` bytes of text. The length the file system reports is not trusted (a link to
/// a device reports 0 and never ends): the read stops one byte past the cap.
fn read_capped(file: &Path, cap: usize) -> Result<String, String> {
    regia_framework::ruleset_file::read_text_capped(file, cap)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(RpcError::wrong_type(key, "a string")),
    }
}

fn bool_param(params: &Value, key: &str) -> Result<bool, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(RpcError::wrong_type(key, "a boolean")),
    }
}

fn usize_param(params: &Value, key: &str) -> Result<Option<usize>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| RpcError::wrong_type(key, "a whole number")),
    }
}

fn open_panel(world: &mut World) {
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::MatchRules),
    );
}

/// Opens (or, with `create`, starts) the rule set at `path`. Replaces the open document; not an
/// undo step, so an open document with unsaved edits is refused with [`codes::DIRTY`] unless
/// `discard` (its edits stay on the undo stack and apply only to that document).
///
/// # Errors
///
/// [`codes::DIRTY`], [`codes::BAD_PATH`], [`codes::NO_PROJECT`], [`codes::READ_FAILED`] or the
/// file's `ruleset_*` code.
pub fn open_rules(
    world: &mut World,
    path: &str,
    create: bool,
    skeleton: Option<&str>,
    extends: Option<&str>,
    discard: bool,
) -> Result<(), RpcError> {
    if !discard
        && let Some(open) = world
            .get_resource::<MatchRulesPanel>()
            .and_then(|p| p.rules.as_ref())
            .filter(|doc| doc.dirty())
    {
        return Err(refused(
            codes::DIRTY,
            format!("{} has unsaved edits; opening {path} would drop them", open.path),
            "save it with editor.rules_save, or pass discard: true",
        ));
    }
    let file = project_file(world, path, RULES_SUFFIX)?;
    let (set, saved) = if file.is_file() {
        let text = read_capped(&file, MAX_RULES_RON_BYTES)
            .map_err(|e| refused(codes::READ_FAILED, format!("{path}: {e}"), "check the file"))?;
        let set = parse_rules_ron(&text).map_err(|e| {
            refused(
                &e.code,
                format!("{path}: {e}"),
                "fix the file as text; book/agents/editor-match-rules.md lists the codes",
            )
        })?;
        (set.clone(), Some(set))
    } else if create {
        let stem = file
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(RULES_SUFFIX))
            .unwrap_or("custom")
            .to_owned();
        let mode = skeleton
            .and_then(regia_framework::rules::mode::MatchMode::from_lobby_str)
            .unwrap_or(regia_framework::rules::mode::MatchMode::Custom);
        let mut set = RuleSet::new(stem, mode);
        if let Some(parent) = extends {
            set.extends = Some(parent.to_owned());
            if skeleton.is_none() {
                set.skeleton = None;
            }
        }
        (set, None)
    } else {
        return Err(refused(
            codes::READ_FAILED,
            format!("{path} does not exist"),
            "pass create: true to start a new rule set",
        ));
    };
    let errors = validate_set(world, &set);
    let mut panel = world.get_resource_or_insert_with(MatchRulesPanel::default);
    panel.rules = Some(RuleSetDoc {
        path: path.to_owned(),
        set,
        saved,
        errors,
    });
    panel.drafts.clear();
    panel.rules_path_input = path.to_owned();
    panel.status = format!("Opened {path}");
    open_panel(world);
    Ok(())
}

/// Writes `value` at `field` of the open rule set as one undo step. Answers whether it changed.
///
/// # Errors
///
/// [`codes::NOT_OPEN`], [`codes::BAD_FIELD`] or [`codes::FIELD_UNSUPPORTED`].
pub fn edit_rules_field(world: &mut World, field: &str, value: Value) -> Result<bool, RpcError> {
    let doc = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.rules.clone())
        .ok_or_else(|| {
            refused(
                codes::NOT_OPEN,
                "no rule set is open",
                "call editor.rules_open first",
            )
        })?;
    let next = set_field(&doc.set, field, value).map_err(|(code, message)| {
        refused(
            code,
            format!("{field}: {message}"),
            "editor.match_rules_state lists the form's field paths",
        )
    })?;
    if next == doc.set {
        return Ok(false);
    }
    let mut command = EditRuleSet {
        path: doc.path.clone(),
        old: doc.set,
        new: next,
        field: field.to_owned(),
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

/// Saves the open rule set atomically. Answers the written path.
///
/// # Errors
///
/// [`codes::NOT_OPEN`], [`codes::SAVE_BLOCKED`] (with `data.errors`) or [`codes::WRITE_FAILED`].
pub fn save_rules(world: &mut World) -> Result<String, RpcError> {
    let doc = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.rules.clone())
        .ok_or_else(|| {
            refused(
                codes::NOT_OPEN,
                "no rule set is open",
                "call editor.rules_open first",
            )
        })?;
    let errors = validate_set(world, &doc.set);
    if !errors.is_empty() {
        let mut error = refused(
            codes::SAVE_BLOCKED,
            format!(
                "{} is not saved: {}",
                doc.path,
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            "fix the listed fields (their codes are in data.errors)",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("errors".into(), errors.iter().map(error_json).collect());
        }
        let count = errors.len();
        if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
            if let Some(open) = panel.rules.as_mut() {
                open.errors = errors;
            }
            panel.status = format!("Not saved: fix the {count} listed problem(s)");
        }
        return Err(error);
    }
    let file = project_file(world, &doc.path, RULES_SUFFIX)?;
    if !doc.dirty() && file.is_file() {
        // Nothing changed since the file was read or saved: its bytes (comments, layout and the
        // fields it leaves out) stay as they are.
        if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
            panel.status = format!("{} is unchanged", doc.path);
        }
        return Ok(doc.path);
    }
    let text = rules_ron_text(&doc.set)
        .map_err(|e| refused(codes::WRITE_FAILED, format!("serializing: {e}"), "report this"))?;
    let text = if doc.saved.is_none() {
        format!(
            "// A rule set (`.rules.ron` v1, book/user-guide/game-modes.md), saved by the editor's Match Rules panel.\n{}",
            with_newline(text)
        )
    } else {
        with_newline(text)
    };
    crate::atomic_write::write_atomic(&file, text.as_bytes()).map_err(|e| {
        refused(
            codes::WRITE_FAILED,
            e,
            "check the folder's permissions and free space",
        )
    })?;
    project_rules(world, true);
    if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
        if let Some(open) = panel.rules.as_mut() {
            open.saved = Some(open.set.clone());
        }
        panel.status = format!("Saved {}", doc.path);
    }
    Ok(doc.path)
}

// ---------------------------------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------------------------------

/// The problems of `file`'s rows against the project's rule sets.
pub fn validate_playlist(world: &mut World, file: &PlaylistFile) -> Vec<RowProblem> {
    let library = project_rules(world, false).library().clone();
    let mut out = Vec::new();
    for (row, entry) in file.entries.iter().enumerate() {
        if entry.map_id.trim().is_empty() {
            out.push(RowProblem {
                row,
                code: codes::PLAYLIST_BAD_ROW.to_owned(),
                path: "map_id".to_owned(),
                message: "a row names its map".to_owned(),
                blocking: true,
            });
        }
        // The refusals of `PlaylistFile::from_ron` (ADR-8897), whatever the mode: the editor never
        // writes a file it cannot read back.
        let max_weight = regia_framework::rules::playlist::MAX_ROW_WEIGHT;
        if entry.weight > max_weight {
            out.push(RowProblem {
                row,
                code: codes::PLAYLIST_BAD_ROW.to_owned(),
                path: "weight".to_owned(),
                message: format!("weight {} is above {max_weight}", entry.weight),
                blocking: true,
            });
        }
        if let (Some(min), Some(max)) = (entry.min_players_bound(), entry.max_players_bound())
            && min > max
        {
            out.push(RowProblem {
                row,
                code: codes::PLAYLIST_BAD_ROW.to_owned(),
                path: "max_players".to_owned(),
                message: format!(
                    "min_players {min} is above max_players {max} (field or option): no player count fits"
                ),
                blocking: true,
            });
        }
        if entry.mode.is_empty() {
            // The level's own rules.
            continue;
        }
        if library.get(&entry.mode).is_none()
            && regia_framework::rules::mode::MatchMode::from_lobby_str(&entry.mode).is_none()
        {
            out.push(RowProblem {
                row,
                code: codes::PLAYLIST_MODE_UNKNOWN.to_owned(),
                path: "mode".to_owned(),
                message: format!(
                    "`{}` names no rule set and no skeleton: the game must know it",
                    entry.mode
                ),
                blocking: false,
            });
            continue;
        }
        if let Err(error) = library.resolve_row(entry) {
            out.push(RowProblem {
                row,
                code: error.code.clone(),
                path: error.path.clone(),
                message: error.message.clone(),
                // The mode is a rule set or a skeleton here: any problem blocks.
                blocking: true,
            });
        }
    }
    out
}

/// One edit of the open playlist: the whole file before and after.
pub struct EditPlaylist {
    /// The document's path.
    pub path: String,
    /// Before.
    pub old: PlaylistFile,
    /// After.
    pub new: PlaylistFile,
    /// The operation (`add`, `move`, ...).
    pub op: String,
}

fn set_doc_playlist(world: &mut World, path: &str, file: &PlaylistFile) {
    let problems = validate_playlist(world, file);
    let mut panel = world.get_resource_or_insert_with(MatchRulesPanel::default);
    if let Some(doc) = panel.playlist.as_mut()
        && doc.path == path
    {
        doc.file = file.clone();
        doc.problems = problems;
    }
    clear_playlist_drafts(&mut panel.drafts);
}

/// Drops the playlist grid's cell drafts (after the rows changed, they show the new values).
fn clear_playlist_drafts(drafts: &mut BTreeMap<String, String>) {
    drafts.retain(|key, _| !key.starts_with("playlist["));
}

impl EditorCommand for EditPlaylist {
    fn apply(&mut self, world: &mut World) {
        set_doc_playlist(world, &self.path, &self.new);
    }

    fn revert(&mut self, world: &mut World) {
        set_doc_playlist(world, &self.path, &self.old);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Playlist {}: {}", self.path, self.op)
    }

    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (Vec::new(), vec![self.path.clone()])
    }
}

/// Opens (or, with `create`, starts) the playlist at `path`. Refused with [`codes::DIRTY`] while
/// the open playlist has unsaved edits, unless `discard`.
fn open_playlist(world: &mut World, path: &str, create: bool, discard: bool) -> Result<(), RpcError> {
    if !discard
        && let Some(open) = world
            .get_resource::<MatchRulesPanel>()
            .and_then(|p| p.playlist.as_ref())
            .filter(|doc| doc.dirty())
    {
        return Err(refused(
            codes::DIRTY,
            format!("{} has unsaved edits; opening {path} would drop them", open.path),
            "save it with editor.playlist_edit { op: \"save\" }, or pass discard: true",
        ));
    }
    let file = project_file(world, path, PLAYLIST_SUFFIX)?;
    let (doc, saved) = if file.is_file() {
        let text = read_capped(&file, MAX_PLAYLIST_BYTES)
            .map_err(|e| refused(codes::READ_FAILED, format!("{path}: {e}"), "check the file"))?;
        let parsed = PlaylistFile::from_ron(&text)
            .map_err(|e| refused(codes::READ_FAILED, format!("{path}: {e}"), "fix the file as text"))?;
        (parsed.clone(), Some(parsed))
    } else if create {
        (
            PlaylistFile {
                format_version: PLAYLIST_RON_FORMAT_VERSION,
                looping: false,
                order: Default::default(),
                no_repeat_last: 0,
                entries: Vec::new(),
            },
            None,
        )
    } else {
        return Err(refused(
            codes::READ_FAILED,
            format!("{path} does not exist"),
            "pass create: true to start a new playlist",
        ));
    };
    let problems = validate_playlist(world, &doc);
    let mut panel = world.get_resource_or_insert_with(MatchRulesPanel::default);
    panel.playlist = Some(PlaylistDoc {
        path: path.to_owned(),
        file: doc,
        saved,
        problems,
    });
    panel.playlist_path_input = path.to_owned();
    clear_playlist_drafts(&mut panel.drafts);
    panel.status = format!("Opened {path}");
    open_panel(world);
    Ok(())
}

fn row_from(value: &Value, base: Option<&PlaylistEntry>) -> Result<PlaylistEntry, RpcError> {
    let bad = |message: String| {
        refused(
            codes::PLAYLIST_BAD_ROW,
            message,
            "a row is { map_id, mode, options }",
        )
    };
    let mut row = base.cloned().unwrap_or_default();
    let Value::Object(map) = value else {
        return Err(bad("`row` must be an object".to_owned()));
    };
    for (key, value) in map {
        match key.as_str() {
            "map_id" => {
                row.map_id = value
                    .as_str()
                    .ok_or_else(|| bad("map_id must be a string".into()))?
                    .to_owned()
            }
            "mode" => {
                row.mode = value
                    .as_str()
                    .ok_or_else(|| bad("mode must be a string".into()))?
                    .to_owned()
            }
            "options" => {
                row.options = serde_json::from_value(value.clone())
                    .map_err(|e| bad(format!("options must map keys to text values: {e}")))?;
            }
            other => return Err(bad(format!("unknown row field `{other}`"))),
        }
    }
    Ok(row)
}

fn row_json(row: &PlaylistEntry) -> Value {
    json!({ "map_id": row.map_id, "mode": row.mode, "options": row.options })
}

/// The playlist after `op` on `current` (see `editor.playlist_edit`).
fn playlist_op(current: &PlaylistFile, op: &str, params: &Value) -> Result<PlaylistFile, RpcError> {
    let mut next = current.clone();
    let len = next.entries.len();
    let index_of = |key: &str| -> Result<usize, RpcError> {
        usize_param(params, key)?.ok_or_else(|| {
            refused(
                codes::PLAYLIST_BAD_OP,
                format!("`{op}` needs `{key}`"),
                "see book/agents/editor-match-rules.md",
            )
        })
    };
    let in_range = |index: usize, key: &str| -> Result<usize, RpcError> {
        if index < len {
            Ok(index)
        } else {
            Err(refused(
                codes::PLAYLIST_BAD_ROW,
                format!("`{key}` {index} is past the {len} rows"),
                "editor.match_rules_state lists the rows",
            ))
        }
    };
    match op {
        "add" => {
            if len >= MAX_PLAYLIST_ROWS {
                return Err(refused(
                    codes::PLAYLIST_BAD_ROW,
                    format!("at most {MAX_PLAYLIST_ROWS} rows"),
                    "remove a row first",
                ));
            }
            let row = row_from(params.get("row").unwrap_or(&Value::Null), None)?;
            let at = usize_param(params, "at")?.unwrap_or(len).min(len);
            next.entries.insert(at, row);
        }
        "remove" => {
            let index = in_range(index_of("index")?, "index")?;
            next.entries.remove(index);
        }
        "move" => {
            let from = in_range(index_of("from")?, "from")?;
            let to = in_range(index_of("to")?, "to")?;
            let row = next.entries.remove(from);
            next.entries.insert(to, row);
        }
        "set_row" => {
            let index = in_range(index_of("index")?, "index")?;
            let row = row_from(params.get("row").unwrap_or(&Value::Null), next.entries.get(index))?;
            next.entries[index] = row;
        }
        "set_looping" => {
            next.looping = match params.get("looping") {
                Some(Value::Bool(b)) => *b,
                _ => {
                    return Err(refused(
                        codes::PLAYLIST_BAD_OP,
                        "`set_looping` needs `looping: true|false`",
                        "see book/agents/editor-match-rules.md",
                    ));
                }
            };
        }
        other => {
            return Err(refused(
                codes::PLAYLIST_BAD_OP,
                format!("unknown op `{other}`"),
                "open, add, remove, move, set_row, set_looping or save",
            ));
        }
    }
    Ok(next)
}

/// Applies one playlist edit (not `open`/`save`) as an undo step. Answers whether it changed.
///
/// # Errors
///
/// [`codes::PLAYLIST_NOT_OPEN`], [`codes::PLAYLIST_BAD_OP`] or [`codes::PLAYLIST_BAD_ROW`].
pub fn edit_playlist(world: &mut World, op: &str, params: &Value) -> Result<bool, RpcError> {
    let doc = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.playlist.clone())
        .ok_or_else(|| {
            refused(
                codes::PLAYLIST_NOT_OPEN,
                "no playlist is open",
                "call editor.playlist_edit { op: \"open\", path }",
            )
        })?;
    let next = playlist_op(&doc.file, op, params)?;
    if next == doc.file {
        return Ok(false);
    }
    let mut command = EditPlaylist {
        path: doc.path.clone(),
        old: doc.file,
        new: next,
        op: op.to_owned(),
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

/// Saves the open playlist atomically. Answers the written path.
///
/// # Errors
///
/// [`codes::PLAYLIST_NOT_OPEN`], [`codes::PLAYLIST_SAVE_BLOCKED`] or [`codes::WRITE_FAILED`].
pub fn save_playlist(world: &mut World) -> Result<String, RpcError> {
    let doc = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.playlist.clone())
        .ok_or_else(|| refused(codes::PLAYLIST_NOT_OPEN, "no playlist is open", "open one first"))?;
    let problems = validate_playlist(world, &doc.file);
    let blocking: Vec<&RowProblem> = problems.iter().filter(|p| p.blocking).collect();
    if doc.file.entries.is_empty() || !blocking.is_empty() {
        let mut error = refused(
            codes::PLAYLIST_SAVE_BLOCKED,
            if doc.file.entries.is_empty() {
                format!("{} has no row", doc.path)
            } else {
                format!("{} has {} blocking problem(s)", doc.path, blocking.len())
            },
            "fix the rows listed in data.problems",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert(
                "problems".into(),
                blocking.iter().map(|p| problem_json(p)).collect(),
            );
        }
        return Err(error);
    }
    let file = project_file(world, &doc.path, PLAYLIST_SUFFIX)?;
    if !doc.dirty() && file.is_file() {
        // Nothing changed since the file was read or saved: keep its bytes.
        if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
            panel.status = format!("{} is unchanged", doc.path);
        }
        return Ok(doc.path);
    }
    let text = doc
        .file
        .to_ron()
        .map_err(|e| refused(codes::WRITE_FAILED, format!("serializing: {e}"), "report this"))?;
    crate::atomic_write::write_atomic(&file, with_newline(text).as_bytes()).map_err(|e| {
        refused(
            codes::WRITE_FAILED,
            e,
            "check the folder's permissions and free space",
        )
    })?;
    if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
        if let Some(open) = panel.playlist.as_mut() {
            open.saved = Some(open.file.clone());
        }
        panel.status = format!("Saved {}", doc.path);
    }
    Ok(doc.path)
}

/// `text` ending in exactly one newline.
fn with_newline(mut text: String) -> String {
    let kept = text.trim_end_matches('\n').len();
    text.truncate(kept);
    text.push('\n');
    text
}

fn problem_json(problem: &RowProblem) -> Value {
    json!({
        "row": problem.row,
        "code": problem.code,
        "path": problem.path,
        "message": problem.message,
        "blocking": problem.blocking,
    })
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

/// The panel's state as JSON (`editor.match_rules_state`, and every method's answer).
pub fn state_json(world: &mut World) -> Value {
    let game_rules = game_rules_json(world);
    let panel = world
        .get_resource::<MatchRulesPanel>()
        .cloned()
        .unwrap_or_default();
    let rules = panel.rules.as_ref().map(|doc| {
        let fields: Vec<Value> = form_rows(&doc.set)
            .into_iter()
            .map(|(path, value)| json!({ "path": path, "value": value, "inherited": value.is_null() }))
            .collect();
        json!({
            "path": doc.path,
            "id": doc.set.id,
            "dirty": doc.dirty(),
            "new": doc.saved.is_none(),
            "fields": fields,
            "errors": doc.errors.iter().map(error_json).collect::<Vec<_>>(),
            "can_save": doc.errors.is_empty(),
        })
    });
    let playlist = panel.playlist.as_ref().map(|doc| {
        json!({
            "path": doc.path,
            "looping": doc.file.looping,
            "rows": doc.file.entries.iter().map(row_json).collect::<Vec<_>>(),
            "dirty": doc.dirty(),
            "problems": doc.problems.iter().map(problem_json).collect::<Vec<_>>(),
            "can_save": !doc.file.entries.is_empty() && doc.problems.iter().all(|p| !p.blocking),
        })
    });
    json!({
        "rules": rules,
        "playlist": playlist,
        "game_rules": game_rules,
        "load_errors": load_errors_json(world),
        "match": crate::match_rules_play::match_json(world),
        "status": panel.status,
    })
}

/// The files of the project's `assets/rules/` folder that did not load, with their `ruleset_*`
/// codes (the library leaves them out, so the picker does not list them).
pub fn load_errors(world: &mut World) -> Vec<(String, RuleSetError)> {
    project_rules(world, false);
    let Some(rules) = world.get_resource::<ProjectRules>() else {
        return Vec::new();
    };
    rules
        .registry
        .errors()
        .iter()
        .map(|(file, error)| {
            // Project-relative when the file is under the project, as every other path here.
            let shown = rules
                .root
                .as_deref()
                .and_then(|root| Path::new(file).strip_prefix(root).ok())
                .map_or_else(|| file.clone(), |rel| rel.to_string_lossy().replace('\\', "/"));
            (shown, error.clone())
        })
        .collect()
}

fn load_errors_json(world: &mut World) -> Vec<Value> {
    load_errors(world)
        .iter()
        .map(|(file, error)| {
            json!({ "file": file, "code": error.code, "path": error.path, "message": error.message })
        })
        .collect()
}

fn rules_open_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(path) = str_param(params, "path")? else {
        // No path: bring the panel forward with what it holds.
        open_panel(world);
        return Ok(state_json(world));
    };
    open_rules(
        world,
        path,
        bool_param(params, "create")?,
        str_param(params, "skeleton")?,
        str_param(params, "extends")?,
        bool_param(params, "discard")?,
    )?;
    Ok(state_json(world))
}

fn rules_set_field_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let field = str_param(params, "field")?.ok_or_else(|| RpcError::wrong_type("field", "a string"))?;
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    let changed = edit_rules_field(world, field, value)?;
    let mut out = state_json(world);
    out["changed"] = json!(changed);
    Ok(out)
}

fn rules_save_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let written = save_rules(world)?;
    let mut out = state_json(world);
    out["written"] = json!(written);
    Ok(out)
}

fn playlist_edit_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op")?.ok_or_else(|| {
        refused(
            codes::PLAYLIST_BAD_OP,
            "`op` is required",
            "open, add, remove, move, set_row, set_looping or save",
        )
    })?;
    let mut extra = Map::new();
    match op {
        "open" => {
            let path = str_param(params, "path")?.ok_or_else(|| {
                refused(
                    codes::PLAYLIST_BAD_OP,
                    "`open` needs `path`",
                    "e.g. assets/rotation.playlist.ron",
                )
            })?;
            open_playlist(
                world,
                path,
                bool_param(params, "create")?,
                bool_param(params, "discard")?,
            )?;
        }
        "save" => {
            extra.insert("written".into(), json!(save_playlist(world)?));
        }
        other => {
            extra.insert("changed".into(), json!(edit_playlist(world, other, params)?));
        }
    }
    let mut out = state_json(world);
    if let Value::Object(map) = &mut out {
        map.extend(extra);
    }
    Ok(out)
}

fn state_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state_json(world))
}

const STATE: &str = "{ rules: { path, id, dirty, new, fields: [{ path, value, inherited }], errors: [{ code, path, \
    message }], can_save } | null, playlist: { path, looping, rows: [{ map_id, mode, options }], dirty, problems: \
    [{ row, code, path, message, blocking }], can_save } | null, game_rules: { current, status, warning, registered, \
    rule_sets }, load_errors: [{ file, code, path, message }], match: { playing, session, live, bots, \
    last_control, queued }, status }";

/// The Match Rules panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.match_rules_state",
                tool: "match_rules_state",
                description: "The Match Rules panel: the open rule set's form rows and problems, the open playlist's \
                    rows and problems, the Game Rules choices and the play-in-editor match state.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            state_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.rules_open",
                tool: "rules_open",
                description: "Open a project .rules.ron in the Match Rules panel (create: true starts a new one), \
                    replacing the open rule set, and bring the panel forward. Refused with match_rules_dirty while \
                    the open rule set has unsaved edits (save it, or pass discard: true). Errors: match_rules_dirty, \
                    match_rules_bad_path, match_rules_read_failed or the file's ruleset_* code.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Project-relative path ending in .rules.ron (assets/rules/ctf_long.rules.ron); \
                            absent, the panel only comes forward.",
                    },
                    ParamSchema {
                        name: "create",
                        kind: "boolean",
                        required: false,
                        description: "Start a new rule set when the file does not exist (id = the file stem).",
                    },
                    ParamSchema {
                        name: "skeleton",
                        kind: "string",
                        required: false,
                        description: "A new rule set's skeleton (tdm, ffa, ctf, koth, dom, custom).",
                    },
                    ParamSchema {
                        name: "extends",
                        kind: "string",
                        required: false,
                        description: "A new rule set's parent id.",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "Drop the open rule set's unsaved edits (else match_rules_dirty).",
                    },
                ],
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            rules_open_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.rules_set_field",
                tool: "rules_set_field",
                description: "Write one field of the open rule set as one undo step, then check and resolve it \
                    (problems answer with their ruleset_* codes in rules.errors). null clears a field so the \
                    parent's value applies.",
                params: &[
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: true,
                        description: "Field path: id, name_key, skeleton, extends, teams, win_conditions, options[0].default, \
                            session.objectives.capture_limit, ...",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "The new value (JSON of the field's type); null or absent clears it.",
                    },
                ],
                result: "{ ...state, changed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            rules_set_field_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.rules_save",
                tool: "rules_save",
                description: "Save the open rule set atomically. Refused with match_rules_save_blocked (data.errors \
                    lists the ruleset_* problems) while it has a problem.",
                params: &[],
                result: "{ ...state, written }",
                mutating: true,
                permission: Permission::EditScene,
            },
            rules_save_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.playlist_edit",
                tool: "playlist_edit",
                description: "Open, edit or save a .playlist.ron in the Match Rules panel. op: open { path, create, \
                    discard }, add { row, at }, remove { index }, move { from, to }, set_row { index, row }, \
                    set_looping { looping }, save. Each edit is one undo step; rows are validated against their rule \
                    sets. open is refused with match_rules_dirty while the open playlist has unsaved edits.",
                params: &[
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: true,
                        description: "open, add, remove, move, set_row, set_looping or save.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "open: project-relative path ending in .playlist.ron.",
                    },
                    ParamSchema {
                        name: "create",
                        kind: "boolean",
                        required: false,
                        description: "open: start a new playlist when the file does not exist.",
                    },
                    ParamSchema {
                        name: "row",
                        kind: "object",
                        required: false,
                        description: "add / set_row: { map_id, mode, options: { key: \"text\" } } (set_row merges).",
                    },
                    ParamSchema {
                        name: "at",
                        kind: "integer",
                        required: false,
                        description: "add: the index to insert at (default: the end).",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "integer",
                        required: false,
                        description: "remove / set_row: the row.",
                    },
                    ParamSchema {
                        name: "from",
                        kind: "integer",
                        required: false,
                        description: "move: the row to move.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "integer",
                        required: false,
                        description: "move: where it ends up.",
                    },
                    ParamSchema {
                        name: "looping",
                        kind: "boolean",
                        required: false,
                        description: "set_looping: start over after the last row.",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "open: drop the open playlist's unsaved edits (else match_rules_dirty).",
                    },
                ],
                result: "{ ...state, changed | written }",
                mutating: true,
                permission: Permission::EditScene,
            },
            playlist_edit_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.match_control",
                tool: "match_control",
                description: "Drive the play-in-editor match, as the Match strip does: start, pause, resume, end, \
                    step { steps } (while paused), add_bot { team }, remove_bot { team }, next_row { row }. Queued \
                    for the editor's next frame; editor.match_rules_state's match.last_control has the outcome.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "start, pause, resume, end, step, add_bot, remove_bot or next_row.",
                    },
                    ParamSchema {
                        name: "steps",
                        kind: "integer",
                        required: false,
                        description: "step: match ticks to run (1..=100000).",
                    },
                    ParamSchema {
                        name: "team",
                        kind: "integer",
                        required: false,
                        description: "add_bot / remove_bot: the fighting team (1..=max_teams).",
                    },
                    ParamSchema {
                        name: "row",
                        kind: "integer",
                        required: false,
                        description: "next_row: the playlist row the next launch starts.",
                    },
                ],
                result: "{ queued, id }",
                mutating: true,
                permission: Permission::EditScene,
            },
            crate::match_rules_play::match_control_method as Handler,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------------------------

fn value_text(value: &Value) -> String {
    if value.is_null() {
        String::new()
    } else {
        value.to_string()
    }
}

fn show_status(world: &mut World, result: Result<(), RpcError>) {
    if let Err(error) = result
        && let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>()
    {
        panel.status = error.message;
    }
}

/// Draws the Match Rules panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "match_rules_panel")]
pub fn match_rules_panel(ui: &mut egui::Ui, world: &mut World) {
    let frame = ui.ctx().cumulative_frame_nr();
    let first_pass = world
        .get_resource::<MatchRulesPanel>()
        .is_none_or(|p| p.last_frame != Some(frame));
    let mut panel = world.remove_resource::<MatchRulesPanel>().unwrap_or_default();
    panel.last_frame = Some(frame);
    world.insert_resource(panel);
    if first_pass {
        // The library follows the project (once per frame: egui may run a pass twice).
        project_rules(world, false);
    }
    ui.horizontal(|ui| {
        ui.strong("Match Rules");
        let status = world
            .get_resource::<MatchRulesPanel>()
            .map(|p| p.status.clone())
            .unwrap_or_default();
        ui.weak(status);
    });
    egui::ScrollArea::vertical().show(ui, |ui| {
        crate::match_rules_play::match_strip(ui, world);
        ui.separator();
        rules_section(ui, world);
        ui.separator();
        playlist_section(ui, world);
    });
}

fn rules_section(ui: &mut egui::Ui, world: &mut World) {
    ui.strong("Rule set");
    for (file, error) in load_errors(world) {
        ui.colored_label(
            ERROR,
            format!(
                "not loaded: {file}: {} at {}: {}",
                error.code, error.path, error.message
            ),
        );
    }
    let mut input = world
        .get_resource::<MatchRulesPanel>()
        .map(|p| p.rules_path_input.clone())
        .unwrap_or_default();
    let dirty = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.rules.as_ref())
        .is_some_and(RuleSetDoc::dirty);
    let mut open = false;
    let mut discard = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut input).hint_text("assets/rules/ctf_long.rules.ron"));
        open = ui.button("Open or create").clicked();
        if dirty {
            discard = ui
                .button("Discard and open")
                .on_hover_text("Drop the open rule set's unsaved edits")
                .clicked();
        }
    });
    if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
        panel.rules_path_input.clone_from(&input);
    }
    if open || discard {
        let result = open_rules(world, &input, true, None, None, discard);
        show_status(world, result);
    }
    let Some(doc) = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.rules.clone())
    else {
        ui.weak("Open a .rules.ron, or name a new one, to edit a mode.");
        return;
    };
    for error in &doc.errors {
        ui.colored_label(
            ERROR,
            format!("error: {} at {}: {}", error.code, error.path, error.message),
        );
    }
    ui.horizontal(|ui| {
        let label = if doc.dirty() { "Save *" } else { "Save" };
        let save = ui
            .add_enabled(doc.errors.is_empty(), egui::Button::new(label))
            .on_disabled_hover_text("Fix the listed problems first");
        if save.clicked() {
            let result = save_rules(world).map(|_| ());
            show_status(world, result);
        }
        ui.weak("Empty field = the parent's value. Values are JSON.");
    });
    let mut drafts = world
        .get_resource::<MatchRulesPanel>()
        .map(|p| p.drafts.clone())
        .unwrap_or_default();
    let mut commit: Option<(String, String)> = None;
    egui::Grid::new("match-rules-form")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for (path, value) in form_rows(&doc.set) {
                let flagged = doc
                    .errors
                    .iter()
                    .any(|e| e.path.contains(path.rsplit('.').next().unwrap_or("")));
                if flagged {
                    ui.colored_label(ERROR, &path);
                } else {
                    ui.label(&path);
                }
                let text = drafts.entry(path.clone()).or_insert_with(|| value_text(&value));
                let response = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text("inherited")
                        .desired_width(260.0),
                );
                if response.lost_focus() && *text != value_text(&value) {
                    commit = Some((path.clone(), text.clone()));
                }
                ui.end_row();
            }
        });
    if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
        panel.drafts = drafts;
    }
    if let Some((path, text)) = commit {
        // JSON when it reads as JSON, else the text as a string (`mode.ctf_long` needs no quotes).
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text.clone()))
        };
        let result = edit_rules_field(world, &path, value).map(|_| ());
        show_status(world, result);
    }
}

fn playlist_section(ui: &mut egui::Ui, world: &mut World) {
    ui.strong("Playlist");
    let mut input = world
        .get_resource::<MatchRulesPanel>()
        .map(|p| p.playlist_path_input.clone())
        .unwrap_or_default();
    let dirty = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.playlist.as_ref())
        .is_some_and(PlaylistDoc::dirty);
    let mut open = false;
    let mut discard = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut input).hint_text("assets/rotation.playlist.ron"));
        open = ui.button("Open or create").clicked();
        if dirty {
            discard = ui
                .button("Discard and open")
                .on_hover_text("Drop the open playlist's unsaved edits")
                .clicked();
        }
    });
    if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
        panel.playlist_path_input.clone_from(&input);
    }
    if open || discard {
        let result = open_playlist(world, &input, true, discard);
        show_status(world, result);
    }
    let Some(doc) = world
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.playlist.clone())
    else {
        ui.weak("Open a .playlist.ron, or name a new one, to edit a rotation.");
        return;
    };
    let mut op: Option<(String, Value)> = None;
    ui.horizontal(|ui| {
        let mut looping = doc.file.looping;
        if ui.checkbox(&mut looping, "Loop").changed() {
            op = Some(("set_looping".into(), json!({ "looping": looping })));
        }
        if ui.button("Add row").clicked() {
            op = Some((
                "add".into(),
                json!({ "row": { "map_id": "levels/new.level.ron", "mode": "" } }),
            ));
        }
        let label = if doc.dirty() { "Save *" } else { "Save" };
        if ui.button(label).clicked() {
            op = Some(("save".into(), json!({})));
        }
    });
    let last = doc.file.entries.len().saturating_sub(1);
    let mut drafts = world
        .get_resource::<MatchRulesPanel>()
        .map(|p| p.drafts.clone())
        .unwrap_or_default();
    egui::Grid::new("match-rules-playlist")
        .num_columns(5)
        .striped(true)
        .show(ui, |ui| {
            for (index, row) in doc.file.entries.iter().enumerate() {
                ui.label(format!("{index}"));
                // Each cell commits on focus loss as one `set_row` (the agent's op): map, mode and
                // the options as JSON text.
                let cells = [
                    ("map_id", row.map_id.clone(), "levels/arena.level.ron"),
                    ("mode", row.mode.clone(), "(level rules)"),
                    (
                        "options",
                        serde_json::to_string(&row.options).unwrap_or_default(),
                        "{\"key\": \"text\"}",
                    ),
                ];
                for (field, value, hint) in cells {
                    let text = drafts
                        .entry(format!("playlist[{index}].{field}"))
                        .or_insert_with(|| value.clone());
                    let response = ui.add(
                        egui::TextEdit::singleline(text)
                            .hint_text(hint)
                            .desired_width(if field == "mode" { 110.0 } else { 180.0 }),
                    );
                    if response.lost_focus() && *text != value {
                        op = Some(("set_row".into(), set_row_params(index, field, text)));
                    }
                }
                ui.horizontal(|ui| {
                    if index > 0 && ui.small_button("Up").clicked() {
                        op = Some(("move".into(), json!({ "from": index, "to": index - 1 })));
                    }
                    if index < last && ui.small_button("Down").clicked() {
                        op = Some(("move".into(), json!({ "from": index, "to": index + 1 })));
                    }
                    if ui.small_button("Remove").clicked() {
                        op = Some(("remove".into(), json!({ "index": index })));
                    }
                });
                ui.end_row();
            }
        });
    if let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>() {
        panel.drafts = drafts;
    }
    for problem in &doc.problems {
        let color = if problem.blocking { ERROR } else { WARN };
        ui.colored_label(
            color,
            format!(
                "row {}: {} at {}: {}",
                problem.row, problem.code, problem.path, problem.message
            ),
        );
    }
    if let Some((name, mut params)) = op {
        params["op"] = json!(name);
        let result = playlist_edit_method(world, &params).map(|_| ());
        show_status(world, result);
    }
}

/// The `set_row` parameters for one edited playlist cell. Options text that is not a JSON object
/// is passed as typed, so the op refuses it with `playlist_bad_row` and the status line says why.
fn set_row_params(index: usize, field: &str, text: &str) -> Value {
    let value = if field == "options" {
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_owned()))
        }
    } else {
        Value::String(text.trim().to_owned())
    };
    json!({ "index": index, "row": { field: value } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_paths_parse_keys_and_indices() {
        assert_eq!(
            parse_path("options[2].default"),
            Some(vec![
                Seg::Key("options".into()),
                Seg::Index(2),
                Seg::Key("default".into())
            ])
        );
        assert_eq!(parse_path("teams[0].color[3]").map(|p| p.len()), Some(4));
        assert_eq!(parse_path(""), None);
        assert_eq!(parse_path("Bad.Field"), None);
        assert_eq!(parse_path("[0]"), None);
    }

    #[test]
    fn set_field_writes_one_value_and_refuses_unknown_fields() {
        let set = RuleSet::new(
            "ctf_long",
            regia_framework::rules::mode::MatchMode::CaptureTheFlag,
        );
        let next = set_field(&set, "session.objectives.capture_limit", json!(5)).expect("set");
        assert_eq!(next.session.objectives.capture_limit, Some(5));
        let cleared = set_field(&next, "session.objectives.capture_limit", Value::Null).expect("clear");
        assert_eq!(cleared, set);
        assert_eq!(
            set_field(&set, "session.objectives.capture_limt", json!(5)).map(|_| ()),
            Err((codes::BAD_FIELD, "no field `capture_limt`".to_owned()))
        );
        assert!(set_field(&set, "session.objectives.capture_limit", json!("lots")).is_err());
        assert!(set_field(&set, "format_version", json!(2)).is_err());
        let option =
            json!({ "key": "laps", "kind": "int", "default": "3", "min": "1", "max": "9", "choices": [] });
        let with = set_field(&set, "options[0]", option).expect("append");
        assert_eq!(with.options.len(), 1);
        let edited = set_field(&with, "options[0].default", json!("4")).expect("default");
        assert_eq!(edited.options[0].default, "4");
        assert!(
            set_field(&with, "options[3].default", json!("4")).is_err(),
            "past the end"
        );
    }

    #[test]
    fn form_rows_list_every_format_field() {
        let set = RuleSet::new("x", regia_framework::rules::mode::MatchMode::TeamDeathmatch);
        let rows = form_rows(&set);
        let paths: Vec<&str> = rows.iter().map(|(p, _)| p.as_str()).collect();
        for field in TOP_FIELDS {
            assert!(paths.contains(&field), "{field}");
        }
        for (section, fields) in session_sections() {
            for field in fields {
                assert!(paths.contains(&format!("session.{section}.{field}").as_str()));
            }
        }
        assert!(
            rows.iter()
                .any(|(p, v)| p == "session.objectives.capture_limit" && v.is_null())
        );
    }

    #[test]
    fn codes_all_is_sorted_by_text_and_names_the_dirty_code() {
        let mut sorted = codes::ALL.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, codes::ALL.to_vec());
        assert!(codes::ALL.contains(&codes::DIRTY));
        assert!(codes::ALL.contains(&codes::PLAYLIST_NOT_OPEN));
    }

    #[test]
    fn with_newline_ends_in_exactly_one() {
        assert_eq!(with_newline("(a: 1)".into()), "(a: 1)\n");
        assert_eq!(with_newline("(a: 1)\n".into()), "(a: 1)\n");
        assert_eq!(with_newline("(a: 1)\n\n".into()), "(a: 1)\n");
    }

    #[test]
    fn a_playlist_cell_commits_as_one_set_row() {
        let file = PlaylistFile {
            format_version: PLAYLIST_RON_FORMAT_VERSION,
            looping: false,
            order: Default::default(),
            no_repeat_last: 0,
            entries: vec![PlaylistEntry::new("levels/a.level.ron", "tdm")],
        };
        let map = set_row_params(0, "map_id", " levels/b.level.ron ");
        let next = playlist_op(&file, "set_row", &map).expect("map");
        assert_eq!(next.entries[0].map_id, "levels/b.level.ron");
        assert_eq!(next.entries[0].mode, "tdm", "set_row merges: the mode stays");
        let mode = set_row_params(0, "mode", "ctf");
        assert_eq!(
            playlist_op(&file, "set_row", &mode).expect("mode").entries[0].mode,
            "ctf"
        );
        let options = set_row_params(0, "options", r#"{"capture_limit": "3"}"#);
        let next = playlist_op(&file, "set_row", &options).expect("options");
        assert_eq!(
            next.entries[0].options.get("capture_limit").map(String::as_str),
            Some("3")
        );
        let cleared = playlist_op(&next, "set_row", &set_row_params(0, "options", "  ")).expect("clear");
        assert!(cleared.entries[0].options.is_empty());
        let bad = playlist_op(&file, "set_row", &set_row_params(0, "options", "capture_limit=3"));
        let refused = bad.expect_err("options text that is not an object");
        assert_eq!(
            refused.data.as_ref().and_then(|d| d["code"].as_str()),
            Some(codes::PLAYLIST_BAD_ROW)
        );
    }

    #[test]
    fn codes_are_sorted_lowercase_and_unique() {
        let mut sorted = codes::ALL.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes::ALL.len());
        for code in codes::ALL {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }
}
