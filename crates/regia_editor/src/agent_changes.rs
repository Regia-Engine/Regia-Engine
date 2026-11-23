// SPDX-License-Identifier: MPL-2.0
//! The Agent Changes panel (`docs/specs/23-agent-native.md` § 5, ADR-2346): the staged changes
//! agents proposed through `review.propose`, each with its title, reason, proposing client, the
//! unified diff and the attached before/after screenshots, and Accept / Reject for one change or
//! its whole set. The panel decides in-process (the human at the editor is the owner), through the
//! same `regia_agent::review::decide_selected` as `review.accept`, so a click and an accepted call
//! write, check conflicts and stamp `.meta` provenance alike.
//!
//! Also here: the editor's `agent.open_in_editor` hook ([`open_target`]): select an entity, open a
//! prefab for editing, or open a project file at a line in the owner's code editor.
//!
//! Agent surface: `editor.agent_changes_state`, `editor.agent_changes_select`
//! (`book/agents/staged-review.md`).

use regia_agent::open_in_editor::OpenTarget;
use regia_agent::protocol::RpcError;
use regia_agent::review::{ChangeStatus, StagedChange};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The panel's own state.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentChangesPanel {
    /// The change whose diff is shown.
    pub selected: Option<u64>,
    /// What the panel last had to say.
    pub status: String,
    /// The reviewer's note for the next decision.
    pub note: String,
}

/// The pending changes, oldest first.
fn pending(world: &mut World) -> Vec<StagedChange> {
    regia_agent::review::queue(world).pending().cloned().collect()
}

/// One pending change as the panel lists it (no texts: the panel draws every frame).
struct Row {
    id: u64,
    set: u64,
    title: String,
    client: String,
    model: String,
    label: String,
}

/// The pending rows, oldest first, with the line counts stored at proposal.
fn rows(world: &mut World) -> Vec<Row> {
    regia_agent::review::queue(world)
        .pending()
        .map(|c| {
            let (added, removed) = c.counts();
            Row {
                id: c.id,
                set: c.set,
                title: c.title.clone(),
                client: c.client.clone(),
                model: c.model.clone(),
                label: format!("{} {}  +{added} -{removed}", c.kind(), c.path),
            }
        })
        .collect()
}

/// The selected change's details: path, reason, screenshots and its diff. A pending change's
/// texts never change, so its diff is computed once and kept until another change is selected.
#[derive(Resource, Clone, Debug, Default)]
struct SelectedDetail {
    id: u64,
    path: String,
    reason: String,
    images: Vec<(String, String)>,
    diff: Vec<String>,
}

fn selected_detail(world: &mut World, id: u64) -> Option<SelectedDetail> {
    if let Some(cached) = world.get_resource::<SelectedDetail>()
        && cached.id == id
    {
        return Some(cached.clone());
    }
    let detail = {
        let queue = regia_agent::review::queue(world);
        let change = queue.pending().find(|c| c.id == id)?;
        SelectedDetail {
            id,
            path: change.path.clone(),
            reason: change.reason.clone(),
            images: change
                .images
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            diff: regia_agent::review::change_diff(change)
                .lines()
                .map(str::to_string)
                .collect(),
        }
    };
    world.insert_resource(detail.clone());
    Some(detail)
}

/// Decides `ids` (or the change's whole `set`) as the owner, and records the outcome in the panel.
pub fn decide(world: &mut World, selection: &Value, accept: bool) -> Result<Value, RpcError> {
    let note = world
        .get_resource::<AgentChangesPanel>()
        .map(|p| p.note.clone())
        .filter(|n| !n.is_empty());
    let mut params = selection.clone();
    if let (Some(note), Some(object)) = (note, params.as_object_mut()) {
        object.insert("note".into(), json!(note));
    }
    let answer = regia_agent::review::decide_selected(world, &params, accept, "owner")?;
    if accept {
        push_undo_entries(world, &answer);
    }
    let applied = answer["applied"].as_u64().unwrap_or(0);
    let decided = answer["decided"].as_u64().unwrap_or(0);
    let conflicts = answer["results"].as_array().map_or(0, |r| {
        r.iter().filter(|x| x["reason"] == "review_conflict").count()
    });
    // ADR-2821: a golden proposal stays pending; say how the owner accepts it.
    let goldens: Vec<String> = answer["results"].as_array().map_or_else(Vec::new, |r| {
        r.iter()
            .filter(|x| x["reason"] == "golden_owner_only")
            .map(|x| x["id"].to_string())
            .collect()
    });
    let mut panel = world.get_resource_or_insert_with::<AgentChangesPanel>(Default::default);
    panel.status = if accept {
        format!(
            "accepted {applied} change(s){}{}",
            if conflicts > 0 {
                format!(", {conflicts} conflict(s) left pending")
            } else {
                String::new()
            },
            if goldens.is_empty() {
                String::new()
            } else {
                format!(
                    "; golden proposals are accepted at a terminal: regia golden accept {}",
                    goldens.join(" ")
                )
            }
        )
    } else {
        format!("rejected {decided} change(s)")
    };
    panel.note.clear();
    Ok(answer)
}

/// An accepted staged change as an undo entry (specs/23 § 5: accepted changes become normal undo
/// entries): undo puts the file's proposal-time text back (or removes a created file), redo writes
/// the accepted text again.
pub struct AcceptedFileChange {
    /// The absolute file path.
    pub path: std::path::PathBuf,
    /// The project-relative path, for the description.
    pub rel: String,
    /// The text before the change (`None`: the change created the file).
    pub before: Option<String>,
    /// The accepted text.
    pub after: String,
}

impl AcceptedFileChange {
    fn write(&self, text: Option<&str>) {
        let result = match text {
            Some(text) => std::fs::write(&self.path, text),
            None => match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        if let Err(e) = result {
            tracing::warn!(target: "regia_editor", "agent change {}: {e}", self.rel);
        }
    }
}

impl crate::command::EditorCommand for AcceptedFileChange {
    fn apply(&mut self, _world: &mut World) {
        self.write(Some(self.after.as_str()));
    }

    fn revert(&mut self, _world: &mut World) {
        self.write(self.before.as_deref());
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Accept agent change to {}", self.rel)
    }
}

/// Pushes one undo entry per change `answer` applied (when the world has an undo stack).
fn push_undo_entries(world: &mut World, answer: &Value) {
    if !world.contains_resource::<crate::command::UndoStack>() {
        return;
    }
    let applied: Vec<u64> = answer["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["applied"] == true)
        .filter_map(|r| r["id"].as_u64())
        .collect();
    let root = regia_agent::req_path::project_root(world);
    let entries: Vec<AcceptedFileChange> = {
        let queue = regia_agent::review::queue(world);
        queue
            .file
            .changes
            .iter()
            .filter(|c| applied.contains(&c.id))
            .filter_map(|c| {
                let path = regia_core::project_path::resolve_write(&root, &c.path).ok()?;
                Some(AcceptedFileChange {
                    path,
                    rel: c.path.clone(),
                    before: c.before.clone(),
                    after: c.after.clone(),
                })
            })
            .collect()
    };
    let mut stack = world.resource_mut::<crate::command::UndoStack>();
    for entry in entries {
        stack.push(Box::new(entry));
        stack.seal();
    }
}

/// Draws the panel.
pub fn agent_changes_panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!("agent_changes_panel").entered();
    let changes = rows(world);
    let mut panel = world
        .get_resource::<AgentChangesPanel>()
        .cloned()
        .unwrap_or_default();
    if panel
        .selected
        .is_none_or(|id| !changes.iter().any(|c| c.id == id))
    {
        panel.selected = changes.first().map(|c| c.id);
    }
    let detail = panel.selected.and_then(|id| selected_detail(world, id));
    let mut action: Option<(Value, bool)> = None;
    ui.horizontal(|ui| {
        ui.label(format!("{} pending", changes.len()));
        ui.separator();
        ui.label(&panel.status);
    });
    if changes.is_empty() {
        ui.label("No agent changes wait for review.");
        world.insert_resource(panel);
        return;
    }
    ui.columns(2, |columns| {
        egui::ScrollArea::vertical()
            .id_salt("agent-changes-list")
            .show(&mut columns[0], |ui| {
                let mut last_set = None;
                for change in &changes {
                    if last_set != Some(change.set) {
                        last_set = Some(change.set);
                        ui.separator();
                        ui.strong(format!("Set {}: {}", change.set, change.title));
                        ui.label(format!(
                            "by {}{}",
                            change.client,
                            if change.model.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", change.model)
                            }
                        ));
                        ui.horizontal(|ui| {
                            if ui.button("Accept set").clicked() {
                                action = Some((json!({ "set": change.set }), true));
                            }
                            if ui.button("Reject set").clicked() {
                                action = Some((json!({ "set": change.set }), false));
                            }
                        });
                    }
                    if ui
                        .selectable_label(panel.selected == Some(change.id), change.label.as_str())
                        .clicked()
                    {
                        panel.selected = Some(change.id);
                    }
                }
            });
        let ui = &mut columns[1];
        if let Some(change) = &detail {
            ui.strong(&change.path);
            ui.label(format!("Reason: {}", change.reason));
            for (which, path) in &change.images {
                ui.label(format!("{which} screenshot: {path}"));
            }
            ui.horizontal(|ui| {
                ui.label("Note:");
                ui.text_edit_singleline(&mut panel.note);
            });
            ui.horizontal(|ui| {
                if ui.button("Accept").clicked() {
                    action = Some((json!({ "ids": [change.id] }), true));
                }
                if ui.button("Reject").clicked() {
                    action = Some((json!({ "ids": [change.id] }), false));
                }
            });
            // Only the lines in view are laid out (ADR-7306): an agent's new or rewritten file can be
            // tens of thousands of diff lines.
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both().id_salt("agent-changes-diff").show_rows(
                ui,
                row_height,
                change.diff.len(),
                |ui, range| {
                    for line in &change.diff[range] {
                        let color = match line.as_bytes().first() {
                            Some(b'+') if !line.starts_with("+++") => egui::Color32::from_rgb(110, 200, 120),
                            Some(b'-') if !line.starts_with("---") => egui::Color32::from_rgb(230, 110, 110),
                            Some(b'@') => egui::Color32::from_rgb(120, 170, 230),
                            _ => ui.visuals().text_color(),
                        };
                        ui.label(egui::RichText::new(line.as_str()).monospace().color(color));
                    }
                },
            );
        }
    });
    world.insert_resource(panel);
    if let Some((selection, accept)) = action
        && let Err(error) = decide(world, &selection, accept)
    {
        world
            .get_resource_or_insert_with::<AgentChangesPanel>(Default::default)
            .status = error.message;
    }
}

/// `editor.agent_changes_state`: what the panel shows.
fn panel_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let changes = pending(world);
    let panel = world
        .get_resource::<AgentChangesPanel>()
        .cloned()
        .unwrap_or_default();
    let selected = panel
        .selected
        .filter(|id| changes.iter().any(|c| c.id == *id))
        .or_else(|| changes.first().map(|c| c.id));
    let rows: Vec<Value> = changes.iter().map(StagedChange::summary).collect();
    let diff = selected
        .and_then(|id| changes.iter().find(|c| c.id == id))
        .map(regia_agent::review::change_diff);
    Ok(json!({ "pending": rows, "selected": selected, "diff": diff, "status": panel.status }))
}

/// `editor.agent_changes_select`: show one change's diff, as a click on its row does.
fn panel_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(id) = params.get("id").and_then(Value::as_u64) else {
        return Err(RpcError::invalid_params("`id` (a pending change id) is required"));
    };
    let found = regia_agent::review::queue(world)
        .file
        .changes
        .iter()
        .any(|c| c.id == id && c.status == ChangeStatus::Pending);
    if !found {
        let mut error = RpcError::not_found(
            format!("no pending change {id}"),
            "editor.agent_changes_state lists them",
        );
        if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
            data.insert("reason".into(), json!("review_change_not_found"));
        }
        return Err(error);
    }
    world
        .get_resource_or_insert_with::<AgentChangesPanel>(Default::default)
        .selected = Some(id);
    panel_state(world, &json!({}))
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.agent_changes_state",
                tool: "agent_changes_state",
                description: "The Agent Changes panel: pending staged changes (review.propose) with their summaries, the selected change and its unified diff, and the panel's last status. Decisions are the owner's (the panel's buttons, or review.accept with the review permission).",
                params: &[],
                result: "{ pending: [{ id, set, path, kind, status, title, reason, client, model, created_ms, added, removed, images }], selected, diff, status }",
                mutating: false,
                permission: Permission::Read,
            },
            panel_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.agent_changes_select",
                tool: "agent_changes_select",
                description: "Show one pending change's diff in the Agent Changes panel, as a click on its row does.",
                params: &[ParamSchema {
                    name: "id",
                    kind: "integer",
                    required: true,
                    description: "A pending change id.",
                }],
                result: "the panel state, as editor.agent_changes_state",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_select as Handler,
        ),
    ]
}

fn unsupported(message: String) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("reason".into(), json!("open_in_editor_unsupported"));
        data.insert(
            "hint".into(),
            json!("see book/agents/staged-review.md (agent.open_in_editor)"),
        );
    }
    error
}

/// Starts the code editor `command` in `root`, detached and never through a shell.
///
/// # Errors
/// The command is empty or the program does not start.
pub(crate) fn spawn_code_editor(root: &std::path::Path, command: &[String]) -> Result<(), RpcError> {
    spawn_code_editor_child(root, command).map(drop)
}

/// [`spawn_code_editor`] that hands back the started process, so a caller (a test) can wait for
/// it to finish instead of racing its output.
///
/// # Errors
/// The command is empty or the program does not start.
pub(crate) fn spawn_code_editor_child(
    root: &std::path::Path,
    command: &[String],
) -> Result<std::process::Child, RpcError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| RpcError::internal("empty code editor command"))?;
    std::process::Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| RpcError::internal(format!("cannot start the code editor {program}: {e}")))
}

/// The editor's `agent.open_in_editor` hook (`regia_agent::open_in_editor::OpenInEditorHook`).
///
/// # Errors
/// The selection or prefab errors of `editor.select` / `editor.open_prefab`; `open_in_editor_unsupported`
/// for another asset kind; `open_in_editor_no_code_editor`; `F0003` for a missing file.
pub fn open_target(world: &mut World, target: &OpenTarget, dry_run: bool) -> Result<Value, RpcError> {
    match target {
        OpenTarget::Entity(bits) => {
            if dry_run {
                return Ok(json!({ "opened": "entity", "entity": bits }));
            }
            let selection = crate::agent_ext::test_support::select(world, &json!({ "entities": [bits] }))?;
            Ok(json!({ "opened": "entity", "entity": bits, "selection": selection }))
        }
        OpenTarget::Asset(path) if path.ends_with(".prefab.ron") => {
            if !dry_run {
                crate::agent_ext::test_support::open_prefab(world, &json!({ "path": path }))?;
            }
            Ok(json!({ "opened": "prefab", "path": path }))
        }
        OpenTarget::Asset(path) => Err(unsupported(format!(
            "the editor opens prefabs (.prefab.ron) from agent.open_in_editor; {path} is another kind (open it as a file)"
        ))),
        OpenTarget::File { path, line } => {
            let root = regia_agent::req_path::project_root(world);
            let full = regia_agent::req_path::project_path_in(&root, "file", path)?;
            if !full.is_file() {
                return Err(RpcError::not_found(
                    format!("no file {path}"),
                    "a project-relative file path",
                ));
            }
            let line_number = u32::try_from(*line).unwrap_or(u32::MAX);
            let command = crate::log_links::code_editor_command(world, &root, &full, line_number, None)?;
            if !dry_run {
                spawn_code_editor(&root, &command)?;
            }
            Ok(json!({ "opened": "file", "path": path, "line": line, "command": command }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_at(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    fn stage(world: &mut World, root: &std::path::Path) {
        std::fs::write(root.join("notes.txt"), "one\ntwo\n").expect("file");
        let mut queue = regia_agent::review::queue(world);
        let change = |id: u64, path: &str, before: Option<&str>| StagedChange {
            golden: None,
            id,
            set: 1,
            path: path.into(),
            before: before.map(str::to_string),
            after: "one\nTWO\n".into(),
            reason: "capitalize".into(),
            title: "caps".into(),
            client: "claude".into(),
            session: "s".into(),
            model: String::new(),
            images: Default::default(),
            created_ms: 0,
            status: ChangeStatus::Pending,
            decided_ms: None,
            decided_by: None,
            note: None,
            added: 0,
            removed: 0,
            texts_dropped: false,
        };
        queue
            .file
            .changes
            .push(change(1, "notes.txt", Some("one\ntwo\n")));
        queue.file.changes.push(change(2, "new.txt", None));
        queue.file.next_id = 2;
        queue.file.next_set = 1;
    }

    /// A golden proposal stays pending through the panel's Accept, and the status says how it is
    /// accepted (ADR-2821).
    #[test]
    fn the_panel_does_not_accept_a_golden_proposal() {
        let root = regia_test_utils::unique_temp_dir("agent-changes-golden");
        let mut world = world_at(&root);
        world.insert_resource(crate::command::UndoStack::new());
        std::fs::create_dir_all(root.join("crates/x/goldens")).expect("goldens");
        std::fs::write(root.join("crates/x/goldens/a.png"), b"image").expect("golden");
        {
            let mut queue = regia_agent::review::queue(&mut world);
            let golden = StagedChange {
                golden: Some(regia_agent::review::GoldenProposal {
                    name: "x:a".into(),
                    job: "golden:1".into(),
                    dir: ".regia/agent/review/golden_proposals/1".into(),
                    before_hash: "b".into(),
                    after_hash: "a".into(),
                    stats: "{}".into(),
                }),
                id: 1,
                set: 1,
                path: "crates/x/goldens/a.png".into(),
                before: Some("golden: x:a\n".into()),
                after: "golden: x:a\nssim: 0.9\n".into(),
                reason: "intended".into(),
                title: "bless".into(),
                client: "claude".into(),
                session: "s".into(),
                model: String::new(),
                images: Default::default(),
                created_ms: 0,
                status: ChangeStatus::Pending,
                decided_ms: None,
                decided_by: None,
                note: None,
                added: 1,
                removed: 0,
                texts_dropped: false,
            };
            queue.file.changes.push(golden);
            queue.file.next_id = 1;
            queue.file.next_set = 1;
        }
        let answer = decide(&mut world, &json!({ "ids": [1] }), true).expect("decide");
        assert_eq!(answer["applied"], 0, "{answer}");
        assert_eq!(answer["results"][0]["reason"], "golden_owner_only");
        assert_eq!(
            std::fs::read(root.join("crates/x/goldens/a.png")).expect("golden"),
            b"image"
        );
        let status = &world.resource::<AgentChangesPanel>().status;
        assert!(status.contains("regia golden accept 1"), "{status}");
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            0,
            "no undo entry"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_panel_lists_selects_and_decides_as_the_owner() {
        let root = regia_test_utils::unique_temp_dir("agent-changes-panel");
        let mut world = world_at(&root);
        world.insert_resource(crate::command::UndoStack::new());
        stage(&mut world, &root);
        let state = panel_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["pending"].as_array().map(Vec::len), Some(2));
        assert_eq!(state["selected"], 1);
        assert!(
            state["diff"].as_str().unwrap_or_default().contains("-two\n+TWO"),
            "{state}"
        );
        let selected = panel_select(&mut world, &json!({ "id": 2 })).expect("select");
        assert_eq!(selected["selected"], 2);
        assert!(
            selected["diff"]
                .as_str()
                .unwrap_or_default()
                .starts_with("--- /dev/null")
        );
        assert!(panel_select(&mut world, &json!({ "id": 9 })).is_err());

        world
            .get_resource_or_insert_with::<AgentChangesPanel>(Default::default)
            .note = "ok".into();
        let accepted = decide(&mut world, &json!({ "ids": [1] }), true).expect("accept");
        assert_eq!(accepted["applied"], 1);
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).expect("read"),
            "one\nTWO\n"
        );
        // The accepted change is an undo entry: undo restores the old text, redo the new.
        {
            let mut stack = world
                .remove_resource::<crate::command::UndoStack>()
                .expect("stack");
            assert_eq!(stack.undo_depth(), 1);
            assert!(stack.undo(&mut world));
            assert_eq!(
                std::fs::read_to_string(root.join("notes.txt")).expect("read"),
                "one\ntwo\n"
            );
            assert!(stack.redo(&mut world));
            assert_eq!(
                std::fs::read_to_string(root.join("notes.txt")).expect("read"),
                "one\nTWO\n"
            );
            world.insert_resource(stack);
        }
        let rejected = decide(&mut world, &json!({ "set": 1 }), false).expect("reject");
        assert_eq!(rejected["decided"], 1);
        assert!(!root.join("new.txt").exists());
        let queue = regia_agent::review::queue(&mut world);
        let first = queue.file.changes.iter().find(|c| c.id == 1).expect("kept");
        assert_eq!(first.decided_by.as_deref(), Some("owner"));
        assert_eq!(first.note.as_deref(), Some("ok"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both panel methods are registered with the editor surface and answer by name.
    #[test]
    fn the_panel_methods_answer_by_name() {
        let root = regia_test_utils::unique_temp_dir("agent-changes-methods");
        let mut world = world_at(&root);
        stage(&mut world, &root);
        let methods = crate::agent_ext::all_methods();
        let call = |world: &mut World, name: &str, params: Value| {
            let (_, handler) = methods
                .iter()
                .find(|(schema, _)| schema.name == name)
                .expect("registered");
            handler(world, &params)
        };
        let state = call(&mut world, "editor.agent_changes_state", json!({})).expect("state");
        assert_eq!(state["selected"], 1);
        let selected = call(&mut world, "editor.agent_changes_select", json!({ "id": 2 })).expect("select");
        assert_eq!(selected["selected"], 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_target_handles_prefabs_files_and_refuses_other_kinds() {
        let root = regia_test_utils::unique_temp_dir("open-target");
        let mut world = world_at(&root);
        let dry = open_target(
            &mut world,
            &OpenTarget::Asset("prefabs/door.prefab.ron".into()),
            true,
        )
        .expect("prefab");
        assert_eq!(dry["opened"], "prefab");
        let other = open_target(
            &mut world,
            &OpenTarget::Asset("materials/a.material.ron".into()),
            true,
        )
        .expect_err("unsupported");
        assert_eq!(
            other.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "open_in_editor_unsupported"
        );
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("src/door.rs"), "fn main() {}\n").expect("file");
        let file = OpenTarget::File {
            path: "src/door.rs".into(),
            line: 1,
        };
        let unconfigured = open_target(&mut world, &file, true).expect_err("no code editor");
        assert_eq!(
            unconfigured.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "open_in_editor_no_code_editor"
        );
        let state = regia_core::brand::project_state_dir(&root);
        std::fs::create_dir_all(&state).expect("state");
        std::fs::write(
            state.join("agent.toml"),
            "[editor]\ncode_editor = [\"ed\", \"+{line}\", \"{file}\"]\n",
        )
        .expect("cfg");
        let planned = open_target(&mut world, &file, true).expect("dry run");
        assert_eq!(planned["command"][0], "ed");
        assert_eq!(planned["command"][1], "+1");
        assert!(
            open_target(
                &mut world,
                &OpenTarget::File {
                    path: "src/missing.rs".into(),
                    line: 1
                },
                true
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
