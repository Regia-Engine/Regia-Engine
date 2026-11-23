// SPDX-License-Identifier: MPL-2.0
//! Utility panels (ADR-8189, `docs/specs/19-editor.md` § 10.17): editor panels built without code.
//! A `.editorpanel.ron` sidecar ([`format`](crate::utility_panel::format)) names a UI-designer
//! document (`.ui.ron`) and a driver (a logic graph or a Lua script,
//! [`driver`](crate::utility_panel::driver)); Window ▸ Utility Panels lists every sidecar under the
//! project's `assets/` plus the starter sample, and opening one docks it as a tab.
//!
//! The document runs in its own runtime UI app (the UI designer's interactive preview,
//! `crate::ui_designer_interactive`), is drawn by the runtime UI renderer into an egui texture, and
//! gets the tab's pointer and keyboard input while the tab's canvas has focus (Tab moves through
//! the document's controls, Enter or Space presses the focused one, Escape gives focus back to the
//! editor). Each UI command (`command: "rename"` on a button) goes to the driver with the panel's
//! named values and the selection; the driver answers editor calls, which run through
//! `regia_agent::call_local` (the console's route: permission classes, parameter gate, audit) as
//! one undo step.
//!
//! **The safe subset.** A call runs only when it is an `editor.*` method of permission class
//! `read` or `edit_scene` (the classes a Tools scope may hold, without `play` and `assets`), or
//! `editor.command { run }` / `editor.extension_action { id }` naming a live action of the editor
//! actions registry; `editor.utility_panels`, `editor.console_run` and `editor.console_complete` are
//! never run. Anything else refuses the whole command with `utility_panel_method_refused` before
//! any call runs.
//!
//! Agent surface: `editor.utility_panels { op: list | open | input | state }`
//! (`book/agents/utility-panels.md`).

pub mod driver;
pub mod format;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_agent::{LocalReply, call_local};
use regia_ecs::prelude::*;
use regia_scene::prelude::Name;
use serde_json::{Value, json};

use crate::shell::Panel;
use crate::viewport_agent::Handler;
use driver::{PanelCall, PanelContext, PanelDriver};
use format::{EDITOR_PANEL_SUFFIX, EditorPanelDef, PanelDriverDef};

/// The id of the starter sample ("Rename selection with prefix").
pub const STARTER_ID: &str = "starter/rename_with_prefix";
/// The starter sample's files, as the starter content installs them under
/// `assets/starter/utility_panels/` (`crates/regia_editor/starter/utility_panels/`).
pub const STARTER_FILES: &[(&str, &str)] = &[
    (
        "rename_with_prefix.editorpanel.ron",
        include_str!("../starter/utility_panels/rename_with_prefix.editorpanel.ron"),
    ),
    (
        "rename_with_prefix.ui.ron",
        include_str!("../starter/utility_panels/rename_with_prefix.ui.ron"),
    ),
    (
        "rename_with_prefix.lua",
        include_str!("../starter/utility_panels/rename_with_prefix.lua"),
    ),
];
/// The audit client of a utility panel's calls.
pub const UTILITY_PANEL_CLIENT: &str = "editor:utility_panel";
/// Most sidecars a scan lists.
pub const MAX_PANELS: usize = 256;
/// Most directory entries a scan visits.
const MAX_SCAN_ENTRIES: usize = 20_000;
/// Deepest folder a scan enters below `assets/`.
const MAX_SCAN_DEPTH: usize = 12;
/// Methods a panel never runs (they would run a panel or a console inside itself).
const NEVER: &[&str] = &[
    "editor.utility_panels",
    "editor.console_run",
    "editor.console_complete",
];

fn bad(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "")
}

/// The Regia error code of `error` (its `data.code`).
fn reason(error: &RpcError) -> &str {
    error
        .data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Interns a panel id or title for the `Copy` [`Panel`] tab key (one leak per distinct string).
fn intern(text: &str) -> &'static str {
    static NAMES: std::sync::OnceLock<std::sync::Mutex<BTreeSet<&'static str>>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(Default::default);
    let mut names = names.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = names.get(text) {
        return found;
    }
    let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

// ---------------------------------------------------------------------------------------------
// Discovery

/// Where a listed panel's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelSource {
    /// Compiled into the editor ([`STARTER_FILES`]).
    Starter,
    /// A sidecar in the project (absolute path).
    Project(PathBuf),
}

/// One listed utility panel.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelEntry {
    /// The id: [`STARTER_ID`], or the sidecar's project-relative path without the suffix
    /// (`assets/tools/rename`).
    pub id: String,
    /// Where its files live.
    pub source: PanelSource,
    /// The parsed sidecar, or why it was refused.
    pub def: Result<EditorPanelDef, format::PanelFileError>,
}

impl PanelEntry {
    /// The menu title (the sidecar's, or the id for a refused sidecar).
    #[must_use]
    pub fn title(&self) -> &str {
        self.def.as_ref().map_or(self.id.as_str(), |d| d.title.as_str())
    }

    /// Reads one of the panel's files, `rel` relative to the sidecar.
    fn read(&self, rel: &str) -> Result<String, RpcError> {
        match &self.source {
            PanelSource::Starter => STARTER_FILES
                .iter()
                .find(|(name, _)| *name == rel)
                .map(|(_, text)| (*text).to_owned())
                .ok_or_else(|| {
                    bad(
                        "utility_panel_read_failed",
                        format!("the starter sample has no {rel}"),
                    )
                }),
            PanelSource::Project(sidecar) => {
                let path = sidecar.parent().unwrap_or(Path::new(".")).join(rel);
                std::fs::read_to_string(&path)
                    .map_err(|e| bad("utility_panel_read_failed", format!("{}: {e}", path.display())))
            }
        }
    }
}

fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

/// Every utility panel: the starter sample first, then the project's sidecars under `assets/`
/// by id (at most [`MAX_PANELS`]).
#[must_use]
pub fn scan(project: Option<&Path>) -> Vec<PanelEntry> {
    let _span = tracing::info_span!(target: "regia_editor", "utility_panel_scan").entered();
    let mut out = vec![PanelEntry {
        id: STARTER_ID.to_owned(),
        source: PanelSource::Starter,
        def: EditorPanelDef::from_text(STARTER_FILES[0].1),
    }];
    let Some(root) = project else {
        return out;
    };
    let mut found = Vec::new();
    let mut stack = vec![(root.join("assets"), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_SCAN_ENTRIES || found.len() >= MAX_PANELS {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() && depth < MAX_SCAN_DEPTH => stack.push((path, depth + 1)),
                Ok(kind) if kind.is_file() && name.ends_with(EDITOR_PANEL_SUFFIX) => found.push(path),
                _ => {}
            }
        }
        if visited > MAX_SCAN_ENTRIES || found.len() >= MAX_PANELS {
            break;
        }
    }
    let mut entries: Vec<PanelEntry> = found
        .into_iter()
        .filter_map(|path| {
            let rel = path.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
            let id = rel.strip_suffix(EDITOR_PANEL_SUFFIX)?.to_owned();
            let def = std::fs::read_to_string(&path)
                .map_err(|e| format::PanelFileError {
                    code: "utility_panel_read_failed",
                    message: e.to_string(),
                })
                .and_then(|text| EditorPanelDef::from_text(&text));
            Some(PanelEntry {
                id,
                source: PanelSource::Project(path),
                def,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    out.extend(entries);
    out
}

// ---------------------------------------------------------------------------------------------
// Open panels

/// The result of the last command a panel ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandStatus {
    /// The UI command.
    pub command: String,
    /// The calls the driver asked for.
    pub calls: Vec<String>,
    /// Whether every call ran (and the edits were kept).
    pub ok: bool,
    /// The error code when not `ok`.
    pub code: Option<String>,
    /// What happened, for the status line.
    pub message: String,
}

impl CommandStatus {
    fn json(&self) -> Value {
        json!({
            "command": self.command,
            "calls": self.calls,
            "ok": self.ok,
            "code": self.code,
            "message": self.message,
        })
    }
}

/// One open utility panel: its runtime UI, its driver and its last result.
pub struct OpenPanel {
    /// The panel id.
    pub id: &'static str,
    /// The tab title.
    pub title: &'static str,
    def: EditorPanelDef,
    document: regia_ui::UiFile,
    preview: crate::ui_designer_interactive::InteractivePreview,
    driver: PanelDriver,
    /// The UI scale (egui points per pixel) the runtime UI was laid out at.
    scale: f32,
    /// The newest UI event sequence already handled.
    seen: u64,
    /// The egui frame whose input was forwarded (egui may run a panel twice in one frame).
    frame: Option<u64>,
    /// Frames left to re-render after an input or a command.
    redraw: u8,
    texture: Option<egui::TextureHandle>,
    /// Why the panel has no picture (no GPU for the runtime UI renderer); not retried each frame.
    no_picture: Option<String>,
    /// The commands run so far.
    pub runs: u64,
    /// The last command's result.
    pub status: Option<CommandStatus>,
}

/// The open utility panels (a non-send editor resource: each holds a private runtime `App`).
#[derive(Default)]
pub struct UtilityPanels {
    /// Open panels by id.
    pub open: BTreeMap<String, OpenPanel>,
}

fn preview_settings(
    world: &World,
    def: &EditorPanelDef,
    scale: f32,
) -> crate::ui_designer_preview::PreviewSettings {
    crate::ui_designer_preview::PreviewSettings {
        size: [(def.size[0] * scale).round(), (def.size[1] * scale).round()],
        safe_area: [0.0; 4],
        scale: Some(scale),
        assets_root: project_root(world).map(|root| root.join("assets")),
        ..Default::default()
    }
}

fn theme_choice(world: &World) -> crate::ui_designer_gpu::ThemeChoice {
    crate::ui_designer_gpu::ThemeChoice {
        themes: world.get_resource::<regia_ui::UiThemes>().cloned(),
        active: world.get_resource::<regia_ui::ActiveTheme>().cloned(),
        files: Vec::new(),
        theme: None,
        project: project_root(world),
    }
}

fn spawn_preview(
    world: &World,
    document: &regia_ui::UiFile,
    def: &EditorPanelDef,
    scale: f32,
) -> Result<crate::ui_designer_interactive::InteractivePreview, RpcError> {
    crate::ui_designer_interactive::InteractivePreview::new(
        document,
        &preview_settings(world, def, scale),
        &theme_choice(world),
        None,
    )
    .map_err(|e| bad("utility_panel_layout", format!("{}: {}", e.code, e.message)))
}

/// Loads panel `id` (its document and its driver) unless it is open already; returns its tab.
///
/// # Errors
/// `utility_panel_not_found`, the sidecar's code (`utility_panel_parse`, `utility_panel_newer`,
/// `utility_panel_invalid`, `utility_panel_read_failed`), `utility_panel_layout` (the `.ui.ron`
/// does not parse or spawn) or `utility_panel_driver_load`.
pub fn open(world: &mut World, id: &str) -> Result<Panel, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "utility_panel_open", id).entered();
    if let Some(panels) = world.get_non_send::<UtilityPanels>()
        && let Some(panel) = panels.open.get(id)
    {
        return Ok(Panel::UtilityPanel(panel.id, panel.title));
    }
    let entry = scan(project_root(world).as_deref())
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| bad("utility_panel_not_found", format!("no utility panel {id:?}")))?;
    let def = entry
        .def
        .clone()
        .map_err(|e| bad(e.code, format!("{id}: {}", e.message)))?;
    let layout_text = entry.read(&def.layout)?;
    let document = regia_ui::authoring::parse_ui_ron(
        &layout_text,
        &def.layout,
        &regia_ui::authoring::UiMigrators::default(),
    )
    .map_err(|e| bad("utility_panel_layout", format!("{}: {e}", def.layout)))?;
    let driver_text = entry.read(def.driver.path())?;
    let driver = match &def.driver {
        PanelDriverDef::Logic(_) => PanelDriver::logic(&driver_text),
        PanelDriverDef::Lua(_) => PanelDriver::lua(&def.title, &driver_text),
    }
    .map_err(|e| bad(e.code, e.message))?;
    let preview = spawn_preview(world, &document, &def, 1.0)?;
    let panel = OpenPanel {
        id: intern(id),
        title: intern(&def.title),
        def,
        document,
        preview,
        driver,
        scale: 1.0,
        seen: 0,
        frame: None,
        redraw: 2,
        texture: None,
        no_picture: None,
        runs: 0,
        status: None,
    };
    let tab = Panel::UtilityPanel(panel.id, panel.title);
    let mut panels = world.remove_non_send::<UtilityPanels>().unwrap_or_default();
    panels.open.insert(id.to_owned(), panel);
    world.insert_non_send(panels);
    Ok(tab)
}

/// Whether a panel may run `call` (the module docs' safe subset); the refusal message otherwise.
///
/// # Errors
/// The reason, for `utility_panel_method_refused`.
pub fn check_call(world: &World, call: &PanelCall) -> Result<(), String> {
    let method = call.method.as_str();
    if !method.starts_with("editor.") {
        return Err(format!("{method} is not an editor.* method"));
    }
    if NEVER.contains(&method) {
        return Err(format!("{method} cannot run from a utility panel"));
    }
    let live = |id: &str| {
        crate::extension::actions::actions(world)
            .iter()
            .any(|(_, action)| action.id == id)
    };
    match method {
        "editor.command" => {
            if call.params.get("bind").is_some() || call.params.get("pin").is_some() {
                return Err("editor.command may not bind or pin from a utility panel".into());
            }
            return match call.params.get("run").and_then(Value::as_str) {
                Some(id) if live(id) => Ok(()),
                Some(id) => Err(format!("{id} is not an action of the editor actions registry")),
                None => Err("editor.command needs `run` naming a registered action".into()),
            };
        }
        "editor.extension_action" => {
            return match call.params.get("id").and_then(Value::as_str) {
                Some(id) if live(id) => Ok(()),
                _ => Err("editor.extension_action needs `id` naming a registered action".into()),
            };
        }
        _ => {}
    }
    let schema = regia_agent::schema::method(method).cloned().or_else(|| {
        world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .and_then(|ext| ext.method_schema(method).cloned())
    });
    match schema.map(|s| s.permission) {
        Some(Permission::Read | Permission::EditScene) => Ok(()),
        Some(other) => Err(format!(
            "{method} needs the {} permission (a utility panel runs read and edit_scene methods)",
            other.name()
        )),
        None => Err(format!("{method} is not a known method")),
    }
}

fn context(world: &mut World, panel: &mut OpenPanel, command: &str) -> PanelContext {
    let values = panel
        .preview
        .frame()
        .nodes
        .into_iter()
        .filter_map(|node| Some((node.name?, node.text?)))
        .collect();
    let selection = world
        .get_resource::<crate::selection::Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entity| {
            let name = world.get::<Name>(entity)?.as_str().to_owned();
            Some((entity.to_bits(), name))
        })
        .collect();
    PanelContext {
        command: command.to_owned(),
        values,
        selection,
    }
}

fn abort(world: &mut World) {
    if world.contains_resource::<crate::command::UndoStack>() {
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            let _ = stack.transaction_abort(world);
        });
    }
}

/// Runs one UI command through the driver and the safe subset, as one undo step.
fn run_command(world: &mut World, panel: &mut OpenPanel, command: &str) -> CommandStatus {
    let _span = tracing::info_span!(target: "regia_editor", "utility_panel_command", command).entered();
    panel.runs += 1;
    let ctx = context(world, panel, command);
    let fail = |calls: Vec<String>, code: &str, message: String| CommandStatus {
        command: command.to_owned(),
        calls,
        ok: false,
        code: Some(code.to_owned()),
        message,
    };
    let calls = match panel.driver.run(&ctx) {
        Ok(calls) => calls,
        Err(e) => return fail(Vec::new(), e.code, e.message),
    };
    let names: Vec<String> = calls.iter().map(|c| c.method.clone()).collect();
    for call in &calls {
        if let Err(message) = check_call(world, call) {
            return fail(names, "utility_panel_method_refused", message);
        }
    }
    if calls.is_empty() {
        return CommandStatus {
            command: command.to_owned(),
            calls: names,
            ok: true,
            code: None,
            message: "Nothing to do".to_owned(),
        };
    }
    let own = world
        .get_resource_or_insert_with(crate::command::UndoStack::new)
        .transaction_begin(format!("{}: {command}", panel.title))
        .is_ok();
    for call in &calls {
        let failure = match call_local(
            world,
            &call.method,
            call.params.clone(),
            Some(UTILITY_PANEL_CLIENT),
        ) {
            LocalReply::Done(Ok(_)) => None,
            LocalReply::Done(Err(e)) => {
                Some(format!("{} failed: {} ({})", call.method, e.message, reason(&e)))
            }
            LocalReply::Pending(_) => Some(format!(
                "{} answers in a later frame; a utility panel runs only immediate methods",
                call.method
            )),
        };
        if let Some(message) = failure {
            if own {
                abort(world);
            }
            return fail(names, "utility_panel_call_failed", message);
        }
    }
    if own && let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        let _ = stack.transaction_commit();
    }
    CommandStatus {
        command: command.to_owned(),
        message: format!(
            "Ran {} call{}",
            calls.len(),
            if calls.len() == 1 { "" } else { "s" }
        ),
        calls: names,
        ok: true,
        code: None,
    }
}

/// Runs every UI command the panel's document sent since the last pump; returns their results.
fn pump(world: &mut World, panel: &mut OpenPanel) -> Vec<CommandStatus> {
    let mut out = Vec::new();
    let commands = panel.preview.commands();
    for record in commands {
        let sequence = record["sequence"].as_u64().unwrap_or(0);
        if sequence <= panel.seen {
            continue;
        }
        panel.seen = sequence;
        let Some(name) = record["value"].as_str() else {
            continue;
        };
        let status = run_command(world, panel, name);
        panel.status = Some(status.clone());
        panel.redraw = panel.redraw.max(2);
        out.push(status);
    }
    out
}

/// Runs `f` on open panel `id` with the panel taken out of the world (so its calls see the world).
fn with_panel<R>(world: &mut World, id: &str, f: impl FnOnce(&mut World, &mut OpenPanel) -> R) -> Option<R> {
    let mut panels = world.remove_non_send::<UtilityPanels>()?;
    let result = match panels.open.remove(id) {
        Some(mut panel) => {
            let result = f(world, &mut panel);
            panels.open.insert(id.to_owned(), panel);
            Some(result)
        }
        None => None,
    };
    // A call may have opened another panel meanwhile (never: `editor.utility_panels` is refused),
    // so the taken map simply goes back.
    world.insert_non_send(panels);
    result
}

// ---------------------------------------------------------------------------------------------
// The tab

fn forward_input(ui: &egui::Ui, panel: &mut OpenPanel, response: &egui::Response) -> bool {
    let rect = response.rect;
    let focused = response.has_focus();
    let scale = 1.0 / panel.scale;
    let events = ui.input(|i| i.events.clone());
    let mut any = false;
    for event in &events {
        let accept = match event {
            egui::Event::PointerMoved(_) | egui::Event::PointerGone => true,
            egui::Event::PointerButton { pos, pressed, .. } => rect.contains(*pos) || (!pressed && focused),
            egui::Event::MouseWheel { .. } => response.hovered(),
            // Escape gives focus back to the editor; it never reaches the document.
            egui::Event::Key {
                key: egui::Key::Escape,
                ..
            } => false,
            _ => focused,
        };
        if !accept {
            continue;
        }
        if matches!(event, egui::Event::PointerMoved(pos) if !rect.contains(*pos)) {
            panel
                .preview
                .egui_event(&egui::Event::PointerGone, rect.min, scale);
        } else {
            panel.preview.egui_event(event, rect.min, scale);
            any = true;
        }
    }
    any
}

/// Draws the Utility panel tab of panel `id` (Window ▸ Utility Panels).
pub fn utility_panel_tab(ui: &mut egui::Ui, world: &mut World, id: &str) {
    let _span = tracing::info_span!(target: "regia_editor", "utility_panel_tab", id).entered();
    if world
        .get_non_send::<UtilityPanels>()
        .is_none_or(|p| !p.open.contains_key(id))
        && let Err(e) = open(world, id)
    {
        let tokens = crate::theme::ui_tokens(ui.ctx());
        ui.colored_label(tokens.error, crate::data_panels::sentence(&e.message))
            .on_hover_text(format!("Agent error code: {}", reason(&e)));
        return;
    }
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let ppp = ui.ctx().pixels_per_point();
    with_panel(world, id, |world, panel| {
        // The UI scale changed (appearance.ui_scale or a monitor move): lay the document out again.
        if (panel.scale - ppp).abs() > f32::EPSILON
            && let Ok(preview) = spawn_preview(world, &panel.document, &panel.def, ppp)
        {
            panel.preview = preview;
            panel.scale = ppp;
            panel.seen = 0;
            panel.redraw = 2;
        }
        let size = egui::vec2(panel.def.size[0], panel.def.size[1]);
        let response = ui.allocate_response(size, egui::Sense::click());
        let canvas_id = response.id;
        crate::canvas_keys::name_canvas(
            ui.ctx(),
            canvas_id,
            &format!(
                "{}: utility panel. Tab moves between its controls, Enter or Space presses one; Escape returns to the editor.",
                panel.title
            ),
        );
        if response.clicked() {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused {
            // Tab and the arrows move through the document's controls, not the editor's; Escape
            // gives focus back to the editor.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    canvas_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: false,
                    },
                );
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                response.surrender_focus();
            }
        }
        // egui may run this panel twice in one frame: input, a step and commands once per frame.
        if panel.frame != Some(frame_nr) {
            panel.frame = Some(frame_nr);
            if forward_input(ui, panel, &response) {
                panel.redraw = panel.redraw.max(3);
            }
            panel.preview.step();
            pump(world, panel);
        }
        let tokens = crate::theme::ui_tokens(ui.ctx());
        if panel.no_picture.is_none() && (panel.redraw > 0 || panel.texture.is_none()) {
            panel.redraw = panel.redraw.saturating_sub(1);
            let frame = crate::ui_designer_gpu::preview_device(world)
                .and_then(|device| panel.preview.render(&device))
                .map_err(|e| format!("{} ({})", e.message, e.code));
            if let Err(message) = &frame {
                panel.no_picture = Some(message.clone());
            }
            if let Ok(frame) = frame {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.rgba,
                );
                match &mut panel.texture {
                    Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        panel.texture = Some(ui.ctx().load_texture(
                            format!("utility_panel:{}", panel.id),
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }
        }
        let painter = ui.painter_at(response.rect);
        match &panel.texture {
            Some(texture) => {
                painter.image(
                    texture.id(),
                    response.rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                painter.rect_filled(response.rect, 0.0, tokens.well);
                if let Some(message) = &panel.no_picture {
                    painter.text(
                        response.rect.center(),
                        egui::Align2::CENTER_CENTER,
                        message,
                        egui::TextStyle::Body.resolve(ui.style()),
                        tokens.muted,
                    );
                }
            }
        }
        if focused {
            painter.rect_stroke(
                response.rect,
                0.0,
                egui::Stroke::new(2.0, tokens.signal),
                egui::StrokeKind::Inside,
            );
        }
        match &panel.status {
            Some(status) if status.ok => {
                ui.colored_label(tokens.muted, format!("{}: {}", status.command, status.message));
            }
            Some(status) => {
                ui.colored_label(tokens.error, format!("{}: {}", status.command, status.message))
                    .on_hover_text(format!(
                        "Agent error code: {}",
                        status.code.as_deref().unwrap_or("")
                    ));
            }
            None => {
                ui.colored_label(
                    tokens.muted,
                    "Click the panel or press Tab to reach it; Escape returns to the editor.",
                );
            }
        }
    });
}

/// Window ▸ Utility Panels: one entry per listed panel. Picking one opens it and queues its tab
/// (an [`crate::editor_layout::LayoutRequest::Open`]); returns the error to show when it cannot open.
pub fn window_menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let mut failed = None;
    crate::agent_parity::menu(ui, "Utility Panels", |ui| {
        let entries = scan(project_root(world).as_deref());
        for entry in entries {
            let enabled = entry.def.is_ok();
            let title = entry.title().to_owned();
            let clicked = ui
                .add_enabled_ui(enabled, |ui| {
                    crate::agent_parity::action_button_text(
                        ui,
                        &crate::agent_parity::WINDOW_UTILITY_PANEL,
                        title,
                    )
                })
                .inner
                .clicked();
            if clicked {
                match open(world, &entry.id) {
                    Ok(tab) => crate::editor_layout::LayoutRequests::push(
                        world,
                        crate::editor_layout::LayoutRequest::Open(tab),
                    ),
                    Err(e) => failed = Some(format!("{} ({})", e.message, reason(&e))),
                }
                ui.close();
            }
        }
    });
    failed
}

// ---------------------------------------------------------------------------------------------
// Agent surface

fn entry_json(world: &World, entry: &PanelEntry) -> Value {
    let open = world
        .get_non_send::<UtilityPanels>()
        .is_some_and(|p| p.open.contains_key(&entry.id));
    let mut out = json!({
        "id": entry.id,
        "title": entry.title(),
        "source": match &entry.source {
            PanelSource::Starter => "starter",
            PanelSource::Project(_) => "project",
        },
        "open": open,
    });
    match &entry.def {
        Ok(def) => {
            out["layout"] = json!(def.layout);
            out["driver"] = json!({ "kind": def.driver.kind(), "path": def.driver.path() });
        }
        Err(e) => out["error"] = json!({ "code": e.code, "message": e.message }),
    }
    out
}

fn state_json(panel: &mut OpenPanel) -> Value {
    let values: BTreeMap<String, String> = panel
        .preview
        .frame()
        .nodes
        .into_iter()
        .filter_map(|node| Some((node.name?, node.text?)))
        .collect();
    json!({
        "id": panel.id,
        "title": panel.title,
        "runs": panel.runs,
        "values": values,
        "status": panel.status.as_ref().map(CommandStatus::json),
    })
}

/// `editor.utility_panels { op, id?, input? }`.
fn utility_panels(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or("list");
    let id = || {
        params
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("utility_panel_bad_params", format!("op {op} needs `id`")))
    };
    let not_open = |id: &str| {
        bad(
            "utility_panel_not_open",
            format!("utility panel {id:?} is not open"),
        )
    };
    match op {
        "list" => {
            let entries = scan(project_root(world).as_deref());
            let panels: Vec<Value> = entries.iter().map(|e| entry_json(world, e)).collect();
            Ok(json!({ "panels": panels }))
        }
        "open" => {
            let id = id()?;
            let tab = open(world, id)?;
            crate::editor_layout::LayoutRequests::push(world, crate::editor_layout::LayoutRequest::Open(tab));
            with_panel(world, id, |_, panel| state_json(panel)).ok_or_else(|| not_open(id))
        }
        "input" => {
            let id = id()?;
            let input = params
                .get("input")
                .filter(|v| v.is_object())
                .ok_or_else(|| bad("utility_panel_bad_params", "op input needs an `input` object"))?;
            with_panel(world, id, |world, panel| {
                panel
                    .preview
                    .input(input)
                    .map_err(|e| bad("utility_panel_bad_params", format!("{}: {}", e.code, e.message)))?;
                let ran: Vec<Value> = pump(world, panel).iter().map(CommandStatus::json).collect();
                let mut out = state_json(panel);
                out["ran"] = json!(ran);
                Ok(out)
            })
            .ok_or_else(|| not_open(id))?
        }
        "state" => {
            let id = id()?;
            with_panel(world, id, |_, panel| state_json(panel)).ok_or_else(|| not_open(id))
        }
        other => Err(bad(
            "utility_panel_bad_params",
            format!("op {other:?} is not list, open, input or state"),
        )),
    }
}

/// The utility panels' agent methods, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.utility_panels",
            tool: "editor_utility_panels",
            description: "Utility panels (ADR-8189): editor panels built from a UI-designer document and a logic graph or Lua driver, named by `.editorpanel.ron` sidecars under assets/ (plus the starter sample). list them, open one as a dock tab (Window > Utility Panels), drive its document with input (click, focus, type, set_value) and read its state; a command's editor calls run as one undo step and only within the safe subset (read and edit_scene editor.* methods and registered editor actions; anything else is utility_panel_method_refused).",
            params: &[
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: false,
                    description: "list (default), open, input or state.",
                },
                ParamSchema {
                    name: "id",
                    kind: "string",
                    required: false,
                    description: "The panel id from list (starter/rename_with_prefix or the sidecar's project path without .editorpanel.ron); open, input and state need it.",
                },
                ParamSchema {
                    name: "input",
                    kind: "object",
                    required: false,
                    description: "op input: { action: click | focus | type | set_value | list | scroll_to, node: \"#name\" or a node path, text?, value?, index? } (the UI designer's interactive input).",
                },
            ],
            result: "list: { panels: [{ id, title, source, open, layout?, driver?: { kind, path }, error?: { code, message } }] }; open, state: { id, title, runs, values, status }; input: the same plus ran: [{ command, calls, ok, code, message }]",
            mutating: true,
            permission: Permission::EditScene,
        },
        utility_panels as Handler,
    )]
}

#[cfg(test)]
mod tests;
