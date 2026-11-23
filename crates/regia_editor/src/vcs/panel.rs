// SPDX-License-Identifier: MPL-2.0
//! The Revision Control window (View ▸ Revision Control, palette `view.toggle_revision_control`,
//! the status bar's revision slot; ADR-7837): the Staged and Changes lists with checkboxes, Stage,
//! Unstage and Discard (confirmed), the diff of a file, the commit message and Commit (validated
//! first), the History tab (the project's or one file's), and the branch picker with Create branch.
//!
//! Every control is a focusable egui widget (keyboard: Tab, Space/Enter), sized by the editor's
//! `appearance.ui_scale`, colored from theme tokens. Controls queue an [`Op`]; the ops run once
//! per frame after the draw ([`draw_and_apply`] guards with `cumulative_frame_nr`, since egui may
//! run a panel's UI twice in one frame), through the same functions the agent methods call.

use std::collections::BTreeSet;

use regia_ecs::prelude::*;

use super::{
    Branches, FileDiff, FileStatus, LogEntry, RepoStatus, RevisionControl as _, VcsState, VcsStatus,
};

/// The window's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VcsTab {
    /// Staged and unstaged lists, diff, commit.
    #[default]
    Changes,
    /// The log.
    History,
}

/// What a control asked for; applied after the draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Poll now.
    Refresh,
    /// Stage these paths.
    Stage(Vec<String>),
    /// Unstage these paths.
    Unstage(Vec<String>),
    /// Discard these paths (after the confirmation).
    Discard(Vec<String>),
    /// Commit the staged changes (`true`: warnings accepted).
    Commit(bool),
    /// Show the diff of a path (`true`: staged).
    Diff(String, bool),
    /// Switch to a branch.
    Switch(String),
    /// Create a branch from the name field and switch to it.
    CreateBranch,
    /// Load the history of the project (`None`) or a path.
    History(Option<String>),
}

/// The window's state (an editor-world resource; view state only, never on the undo stack).
#[derive(Resource, Debug, Clone, Default)]
pub struct VcsPanel {
    /// The window is shown.
    pub open: bool,
    /// The tab shown.
    pub tab: VcsTab,
    /// Checked rows: `s:<path>` (Staged list) and `u:<path>` (Changes list).
    pub checked: BTreeSet<String>,
    /// The diff shown.
    pub diff: Option<FileDiff>,
    /// The structural, side by side diff of the shown RON asset (`None`: the file is not a RON
    /// asset, or a side is missing: see [`VcsPanel::structural_note`]).
    pub structural: Option<crate::diff_view::DiffWindow>,
    /// Why the shown diff has no structural view.
    pub structural_note: String,
    /// Show the text diff although a structural one exists (the Text toggle).
    pub text_diff: bool,
    /// The commit message being written.
    pub message: String,
    /// The new-branch name field.
    pub new_branch: String,
    /// The history shown: whose (`None`: the project) and its rows.
    pub history_path: Option<String>,
    /// History rows.
    pub history: Vec<LogEntry>,
    /// The local branches (loaded when the window opens and after branch operations).
    pub branches: Branches,
    /// Paths waiting for the Discard confirmation.
    pub confirm_discard: Option<Vec<String>>,
    /// Validation warnings waiting for Commit anyway.
    pub confirm_warnings: Option<String>,
    /// The last result line (`true`: an error).
    pub notice: Option<(bool, String)>,
    /// Ops queued by this frame's draw.
    pub pending: Vec<Op>,
    /// Ops applied (counts each applied op once).
    pub ops_applied: u64,
    applied_frame: Option<u64>,
    loaded: bool,
}

/// Shows or hides the window (View ▸ Revision Control, the status bar slot, the palette).
pub fn toggle(world: &mut World) -> bool {
    let open = {
        let mut panel = world.get_resource_or_insert_with(VcsPanel::default);
        panel.open = !panel.open;
        if panel.open {
            panel.loaded = false;
        }
        panel.open
    };
    if open {
        super::request_refresh(world);
    }
    open
}

/// Opens the window on the History tab for `path` (`None`: the whole project): the Content
/// Browser's Show history and `editor.vcs_log { open: true }`.
pub fn show_history(world: &mut World, path: Option<&str>) {
    let mut panel = world.get_resource_or_insert_with(VcsPanel::default);
    panel.open = true;
    panel.tab = VcsTab::History;
    panel.pending.push(Op::History(path.map(str::to_string)));
}

/// The floating window, drawn over the dock while open.
pub fn window(ctx: &egui::Context, world: &mut World) {
    if !world.get_resource::<VcsPanel>().is_some_and(|p| p.open) {
        return;
    }
    let _span = tracing::info_span!("vcs_panel").entered();
    let mut open = true;
    egui::Window::new("Revision Control")
        .default_pos(crate::tool_windows::default_pos(ctx, "Revision Control"))
        .open(&mut open)
        .default_width(560.0)
        .show(ctx, |ui| draw_and_apply(ui, world));
    if !open && let Some(mut panel) = world.get_resource_mut::<VcsPanel>() {
        panel.open = false;
    }
}

/// Draws the panel into `ui` and applies the ops it queued (once per frame).
pub fn draw_and_apply(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<VcsPanel>().unwrap_or_default();
    if !panel.loaded {
        panel.loaded = true;
        panel.pending.push(Op::Refresh);
    }
    let state = world.get_resource::<VcsStatus>().map(|s| s.state.clone());
    panel_ui(ui, &mut panel, state.as_ref());
    let frame = ui.ctx().cumulative_frame_nr();
    if panel.applied_frame == Some(frame) {
        // A second UI pass in the same frame: its controls repeat the first pass's clicks.
        panel.pending.clear();
    } else if !panel.pending.is_empty() {
        panel.applied_frame = Some(frame);
        apply(world, &mut panel);
    }
    world.insert_resource(panel);
}

/// Runs the queued ops through the shared functions.
pub fn apply(world: &mut World, panel: &mut VcsPanel) {
    let ops = std::mem::take(&mut panel.pending);
    for op in ops {
        panel.ops_applied += 1;
        let result: Result<Option<String>, super::VcsError> = match op {
            Op::Refresh => {
                super::refresh_now(world);
                if let Ok(driver) = super::driver(world) {
                    panel.branches = driver.branches().unwrap_or_default();
                }
                Ok(None)
            }
            Op::Stage(paths) => super::agent::stage(world, &paths).map(|()| {
                panel
                    .checked
                    .retain(|k| !paths.iter().any(|p| k == &format!("u:{p}")));
                Some(format!("Staged {} file(s)", paths.len()))
            }),
            Op::Unstage(paths) => super::agent::unstage(world, &paths).map(|()| {
                panel
                    .checked
                    .retain(|k| !paths.iter().any(|p| k == &format!("s:{p}")));
                Some(format!("Unstaged {} file(s)", paths.len()))
            }),
            Op::Discard(paths) => {
                panel.confirm_discard = None;
                super::agent::discard(world, &paths).map(|done| {
                    panel
                        .checked
                        .retain(|k| !paths.iter().any(|p| k == &format!("u:{p}")));
                    panel.diff = None;
                    panel.structural = None;
                    Some(format!("Discarded changes to {} file(s)", done.len()))
                })
            }
            Op::Commit(allow_warnings) => {
                panel.confirm_warnings = None;
                match super::agent::commit(world, &panel.message, allow_warnings) {
                    Ok((id, _)) => {
                        panel.message.clear();
                        panel.checked.clear();
                        panel.diff = None;
                        panel.structural = None;
                        Ok(Some(format!("Committed {}", short(&id))))
                    }
                    Err(error) if error.code == "validation_warnings" => {
                        panel.confirm_warnings = Some(error.message);
                        Ok(None)
                    }
                    Err(error) => Err(error),
                }
            }
            Op::Diff(path, staged) => super::driver(world)
                .and_then(|d| d.diff(&path, staged))
                .map(|diff| {
                    panel.diff = Some(diff);
                    match structural_diff(world, &path, staged) {
                        Ok(window) => {
                            panel.structural = window;
                            panel.structural_note.clear();
                        }
                        Err(note) => {
                            panel.structural = None;
                            panel.structural_note = note;
                        }
                    }
                    None
                }),
            Op::Switch(name) => super::agent::branch(world, "switch", Some(&name), true).map(|b| {
                panel.branches = b;
                Some(format!("Switched to {name}"))
            }),
            Op::CreateBranch => {
                let name = panel.new_branch.trim().to_string();
                super::agent::branch(world, "create", Some(&name), true).map(|b| {
                    panel.branches = b;
                    panel.new_branch.clear();
                    Some(format!("Created and switched to {name}"))
                })
            }
            Op::History(path) => super::driver(world)
                .and_then(|d| d.log(path.as_deref(), super::agent::DEFAULT_LOG_LIMIT))
                .map(|entries| {
                    panel.history = entries;
                    panel.history_path = path;
                    panel.tab = VcsTab::History;
                    None
                }),
        };
        match result {
            Ok(Some(text)) => panel.notice = Some((false, text)),
            Ok(None) => {}
            Err(error) => panel.notice = Some((true, error.message)),
        }
    }
}

/// Where one side of a structural diff reads its text.
#[derive(Clone, Copy)]
enum Source {
    /// The last commit.
    Head,
    /// The index.
    Index,
    /// The working tree.
    Work,
}

/// The structural diff of a RON asset row: the Staged row compares `HEAD` with the index, the
/// Changes row the index with the working tree (what the text diff beside it shows), through
/// [`crate::asset_diff::diff_texts`]. `Ok(None)` for a file that is not a RON asset (the text diff
/// is all there is); `Err` carries why a RON asset has no structural view (a side is missing, or
/// is not valid RON).
pub(super) fn structural_diff(
    world: &World,
    path: &str,
    staged: bool,
) -> Result<Option<crate::diff_view::DiffWindow>, String> {
    use crate::asset_diff::{self, AssetDiff, DiffSide, Git};
    let Some(kind) = asset_diff::AssetKind::of_path(path) else {
        return Ok(None);
    };
    let root = super::project_root(world).ok_or_else(|| "no project is open".to_string())?;
    let git = Git {
        program: super::git_program(world).0.into_os_string(),
        ceiling: None,
    };
    let note = |e: asset_diff::DiffError| e.message;
    let prefix = git.prefix(&root).map_err(note)?;
    let head = git.resolve(&root, "HEAD").map_err(note)?;
    let layout = asset_diff::layout_sidecar(path);
    let side = |source: Source| -> Result<(DiffSide, String, Option<String>), String> {
        let (rev, hash) = match source {
            Source::Head => (Some("HEAD".to_string()), Some(head.clone())),
            Source::Index => (Some("index".to_string()), None),
            Source::Work => (None, None),
        };
        let show = |rel: &str| -> Result<Option<String>, String> {
            match source {
                Source::Work => Ok(std::fs::read_to_string(root.join(rel)).ok()),
                Source::Head => git.show(&root, &head, &format!("{prefix}{rel}")).map_err(note),
                // `git show :<path>` reads the index.
                Source::Index => git.show(&root, "", &format!("{prefix}{rel}")).map_err(note),
            }
        };
        let text = show(path)?.ok_or_else(|| {
            "No structural view: the file is missing on one side (added, deleted or untracked)".to_string()
        })?;
        let layout = if kind.has_layout_sidecar() {
            show(&layout)?
        } else {
            None
        };
        Ok((
            DiffSide {
                rev,
                hash,
                path: path.to_string(),
            },
            text,
            layout,
        ))
    };
    let (from, to) = if staged {
        (Source::Head, Source::Index)
    } else {
        (Source::Index, Source::Work)
    };
    let (old, old_text, old_layout) = side(from)?;
    let (new, new_text, new_layout) = side(to)?;
    let body = asset_diff::diff_texts(
        kind,
        &old_text,
        old_layout.as_deref(),
        &new_text,
        new_layout.as_deref(),
    )
    .map_err(|e| format!("No structural view: {}", e.message))?;
    Ok(Some(crate::diff_view::DiffWindow::for_result(
        &root,
        AssetDiff {
            path: path.to_string(),
            kind,
            old,
            new,
            body,
        },
    )))
}

fn short(id: &str) -> &str {
    id.get(..10).unwrap_or(id)
}

/// The panel's controls; they only queue ops.
pub fn panel_ui(ui: &mut egui::Ui, panel: &mut VcsPanel, state: Option<&VcsState>) {
    let status = match state {
        Some(VcsState::Ready(status)) => status.clone(),
        other => {
            let text = match other {
                Some(VcsState::NoRevisionControl) => {
                    "No revision control: the project is not in a git repository.".to_string()
                }
                Some(VcsState::NoProject) => "No project is open.".to_string(),
                Some(VcsState::Unavailable(error)) => {
                    format!("Revision control is unavailable: {}", error.message)
                }
                _ => "Reading the revision-control status…".to_string(),
            };
            ui.label(text);
            if ui.button("Refresh").clicked() {
                panel.pending.push(Op::Refresh);
            }
            return;
        }
    };
    branch_row(ui, panel, &status);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut panel.tab, VcsTab::Changes, "Changes");
        if ui
            .selectable_value(&mut panel.tab, VcsTab::History, "History")
            .clicked()
        {
            panel.pending.push(Op::History(panel.history_path.clone()));
        }
    });
    ui.separator();
    match panel.tab {
        VcsTab::Changes => changes_tab(ui, panel, &status),
        VcsTab::History => history_tab(ui, panel),
    }
    if let Some((error, text)) = &panel.notice {
        ui.separator();
        if *error {
            ui.colored_label(
                crate::theme::ui_tokens(ui.ctx()).accents.error_text,
                text.as_str(),
            );
        } else {
            ui.weak(text.as_str());
        }
    }
}

fn branch_row(ui: &mut egui::Ui, panel: &mut VcsPanel, status: &RepoStatus) {
    ui.horizontal(|ui| {
        let current = status.branch.clone().unwrap_or_else(|| "(detached)".to_string());
        let mut chosen = current.clone();
        egui::ComboBox::from_label("Branch")
            .selected_text(current.clone())
            .show_ui(ui, |ui| {
                for name in &panel.branches.list {
                    ui.selectable_value(&mut chosen, name.clone(), name.as_str());
                }
            });
        if chosen != current {
            panel.pending.push(Op::Switch(chosen));
        }
        let label = ui.label("New branch");
        ui.add(egui::TextEdit::singleline(&mut panel.new_branch).desired_width(120.0))
            .labelled_by(label.id);
        let can_create = !panel.new_branch.trim().is_empty();
        if ui
            .add_enabled(can_create, egui::Button::new("Create branch"))
            .clicked()
        {
            panel.pending.push(Op::CreateBranch);
        }
        if ui.button("Refresh").clicked() {
            panel.pending.push(Op::Refresh);
        }
    });
}

/// One list row: a checkbox named by the path, the badge letter, and a Diff toggle.
fn file_row(ui: &mut egui::Ui, panel: &mut VcsPanel, file: &FileStatus, staged: bool) {
    let key = format!("{}:{}", if staged { "s" } else { "u" }, file.path);
    let state = if staged {
        super::RevisionState::from_change(file.staged)
    } else {
        file.state()
    };
    ui.horizontal(|ui| {
        let mut on = panel.checked.contains(&key);
        if ui.checkbox(&mut on, file.path.as_str()).changed() {
            if on {
                panel.checked.insert(key.clone());
            } else {
                panel.checked.remove(&key);
            }
        }
        ui.colored_label(state.color(ui.ctx()), state.letter())
            .on_hover_text(state.describe());
        let shown = panel
            .diff
            .as_ref()
            .is_some_and(|d| d.path == file.path && d.staged == staged);
        if ui
            .selectable_label(shown, "Diff")
            .on_hover_text(format!("Show the diff of {}", file.path))
            .clicked()
        {
            panel.pending.push(Op::Diff(file.path.clone(), staged));
        }
    });
}

fn checked_in(panel: &VcsPanel, prefix: &str) -> Vec<String> {
    panel
        .checked
        .iter()
        .filter_map(|k| k.strip_prefix(prefix).map(str::to_string))
        .collect()
}

fn changes_tab(ui: &mut egui::Ui, panel: &mut VcsPanel, status: &RepoStatus) {
    // Checks of rows that left a list are dropped.
    panel.checked.retain(|k| match k.split_once(':') {
        Some(("s", path)) => status.file(path).is_some_and(FileStatus::is_staged),
        Some(("u", path)) => status.file(path).is_some_and(FileStatus::is_unstaged),
        _ => false,
    });
    let staged: Vec<FileStatus> = status.staged().cloned().collect();
    let unstaged: Vec<FileStatus> = status.unstaged().cloned().collect();
    let list_height = ui.spacing().interact_size.y * 8.0;
    ui.strong(format!("Staged ({})", staged.len()));
    egui::ScrollArea::vertical()
        .id_salt("vcs-staged")
        .max_height(list_height)
        .show(ui, |ui| {
            for file in &staged {
                file_row(ui, panel, file, true);
            }
        });
    let picked_staged = checked_in(panel, "s:");
    if ui
        .add_enabled(!picked_staged.is_empty(), egui::Button::new("Unstage"))
        .clicked()
    {
        panel.pending.push(Op::Unstage(picked_staged));
    }
    ui.separator();
    ui.strong(format!("Changes ({})", unstaged.len()));
    egui::ScrollArea::vertical()
        .id_salt("vcs-unstaged")
        .max_height(list_height)
        .show(ui, |ui| {
            for file in &unstaged {
                file_row(ui, panel, file, false);
            }
        });
    let picked = checked_in(panel, "u:");
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!picked.is_empty(), egui::Button::new("Stage"))
            .clicked()
        {
            panel.pending.push(Op::Stage(picked.clone()));
        }
        if ui
            .add_enabled(!picked.is_empty(), egui::Button::new("Discard…"))
            .clicked()
        {
            panel.confirm_discard = Some(picked.clone());
        }
    });
    if let Some(paths) = panel.confirm_discard.clone() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(format!(
                "Discard the unstaged changes to {} file(s)? Untracked files are deleted. This cannot be undone.",
                paths.len()
            ));
            ui.horizontal(|ui| {
                if ui.button("Discard").clicked() {
                    panel.pending.push(Op::Discard(paths.clone()));
                }
                if ui.button("Cancel").clicked() {
                    panel.confirm_discard = None;
                }
            });
        });
    }
    ui.separator();
    diff_view(ui, panel);
    ui.separator();
    let label = ui.label("Commit message");
    ui.add(
        egui::TextEdit::multiline(&mut panel.message)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("What changed and why"),
    )
    .labelled_by(label.id);
    let can_commit = !staged.is_empty() && !panel.message.trim().is_empty();
    if ui.add_enabled(can_commit, egui::Button::new("Commit")).clicked() {
        panel.pending.push(Op::Commit(false));
    }
    if let Some(warnings) = panel.confirm_warnings.clone() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.alert, warnings);
            ui.horizontal(|ui| {
                if ui.button("Commit anyway").clicked() {
                    panel.pending.push(Op::Commit(true));
                }
                if ui.button("Cancel").clicked() {
                    panel.confirm_warnings = None;
                }
            });
        });
    }
}

fn diff_view(ui: &mut egui::Ui, panel: &mut VcsPanel) {
    let Some(diff) = &panel.diff else {
        ui.weak("Choose Diff on a row to see its changes.");
        return;
    };
    let side = if diff.staged { "staged" } else { "unstaged" };
    ui.strong(format!("{} ({side})", diff.path));
    if panel.structural.is_some() {
        // A RON asset: the structural view is the default, with a toggle back to the text.
        let structural = !panel.text_diff;
        ui.horizontal(|ui| {
            if ui
                .selectable_label(structural, "Structural")
                .on_hover_text("Side by side: what changed in the asset's fields or graph")
                .clicked()
            {
                panel.text_diff = false;
            }
            if ui
                .selectable_label(!structural, "Text")
                .on_hover_text("The unified text diff")
                .clicked()
            {
                panel.text_diff = true;
            }
        });
        if structural && let Some(window) = panel.structural.as_mut() {
            let height = ui.spacing().interact_size.y * 16.0;
            ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                crate::diff_view::window_ui(ui, window);
            });
            return;
        }
    } else if !panel.structural_note.is_empty() {
        ui.weak(panel.structural_note.as_str());
    }
    match diff.kind {
        "binary" => {
            let size = |s: Option<u64>| s.map_or_else(|| "none".to_string(), |n| format!("{n} bytes"));
            ui.label(format!(
                "Binary file changed: {} before, {} after.",
                size(diff.old_size),
                size(diff.new_size)
            ));
        }
        "text" => {
            let tokens = crate::theme::ui_tokens(ui.ctx());
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            egui::ScrollArea::both()
                .id_salt("vcs-diff")
                .max_height(ui.spacing().interact_size.y * 12.0)
                .show(ui, |ui| {
                    for line in diff.text.lines() {
                        let color = if line.starts_with('+') && !line.starts_with("+++") {
                            tokens.accents.ok
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            tokens.accents.error_text
                        } else if line.starts_with("@@") {
                            tokens.accents.log_info
                        } else {
                            tokens.text
                        };
                        ui.label(egui::RichText::new(line).font(font.clone()).color(color));
                    }
                    if diff.truncated {
                        ui.weak("(diff cut: too long to show)");
                    }
                });
        }
        _ => {
            ui.weak("No changes.");
        }
    }
}

fn history_tab(ui: &mut egui::Ui, panel: &mut VcsPanel) {
    ui.horizontal(|ui| {
        match &panel.history_path {
            Some(path) => ui.strong(format!("History of {path}")),
            None => ui.strong("History of the project"),
        };
        if panel.history_path.is_some() && ui.button("All history").clicked() {
            panel.pending.push(Op::History(None));
        }
    });
    if panel.history.is_empty() {
        ui.weak("No commits.");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("vcs-history")
        .max_height(ui.spacing().interact_size.y * 16.0)
        .show(ui, |ui| {
            egui::Grid::new("vcs-history-grid").striped(true).show(ui, |ui| {
                for entry in &panel.history {
                    ui.monospace(short(&entry.id));
                    ui.label(entry.date.get(..10).unwrap_or(&entry.date));
                    ui.label(entry.author.as_str());
                    ui.label(entry.summary.as_str());
                    ui.end_row();
                }
            });
        });
}
