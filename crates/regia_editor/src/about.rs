// SPDX-License-Identifier: MPL-2.0
//! Help ▸ Documentation, Help ▸ Agent guide and Help ▸ About (`docs/specs/19-editor.md` § 2,
//! ADR-7475; the About box and the local book search are ported from the unpushed
//! `lane/sprint-editor-menus-package-help`, ADR-3469, without its online fallback).
//!
//! Documentation and Agent guide open a local file only: the rendered book
//! (`book/build/index.html`, `book/build/agents/protocol.html`), else the generated manual
//! (`docs/manual/manual.html`, Documentation only), else the book's Markdown sources
//! (`book/introduction.md`, `book/agents/protocol.md`). The search starts at `$REGIA_BOOK` (a
//! directory holding `book/` or the book itself), then walks up to eight parents from the editor
//! executable and from the working directory. No local copy answers `help_docs_missing`, with the
//! places searched.
//!
//! About shows the editor version, the git revision baked at build time, the engine's license
//! (MPL-2.0, with a link to its text; the title is the first line of `LICENSE`, ADR-10225) and the
//! entry point to `THIRD_PARTY_NOTICES.md`.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// File name of the engine licence (the verbatim MPL-2.0 text, ADR-10225).
pub const LICENSE_FILE: &str = "LICENSE";
/// The engine licence's SPDX id.
pub const LICENSE_SPDX: &str = "MPL-2.0";
/// The engine licence's name as About shows it.
pub const LICENSE_NAME: &str = "Mozilla Public License 2.0";
/// The canonical public copy of the engine licence.
pub const LICENSE_URL: &str = "https://mozilla.org/MPL/2.0/";
/// File name of the third-party notices list.
pub const NOTICES_FILE: &str = "THIRD_PARTY_NOTICES.md";
/// The environment variable naming a local book (or a directory that holds `book/`).
pub const BOOK_ENV: &str = "REGIA_BOOK";
/// How many parents the search walks up from each start.
const MAX_PARENTS: usize = 8;

/// What a Help item opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    /// Help ▸ Documentation: the manual.
    Documentation,
    /// Help ▸ Agent guide: the agent protocol pages.
    AgentGuide,
}

impl HelpTopic {
    /// The protocol name (`documentation`, `agent_guide`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Documentation => "documentation",
            Self::AgentGuide => "agent_guide",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "documentation" => Some(Self::Documentation),
            "agent_guide" => Some(Self::AgentGuide),
            _ => None,
        }
    }

    /// The files tried under each search root, best first.
    #[must_use]
    pub fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Documentation => &[
                "book/build/index.html",
                "docs/manual/manual.html",
                "book/introduction.md",
            ],
            Self::AgentGuide => &["book/build/agents/protocol.html", "book/agents/protocol.md"],
        }
    }
}

/// The directories the search starts from: `$REGIA_BOOK` (and its parent, when it is the book
/// itself), then every parent (up to eight) of the executable's directory and the working
/// directory.
#[must_use]
pub fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(book) = regia_core::brand::env_var_os(BOOK_ENV).filter(|v| !v.is_empty()) {
        let book = PathBuf::from(book);
        if book.file_name().is_some_and(|name| name == "book")
            && let Some(parent) = book.parent()
        {
            roots.push(parent.to_path_buf());
        }
        roots.push(book);
    }
    let mut starts = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        starts.push(dir.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    for start in starts {
        let mut dir = Some(start.as_path());
        for _ in 0..=MAX_PARENTS {
            let Some(here) = dir else { break };
            if !roots.iter().any(|r| r == here) {
                roots.push(here.to_path_buf());
            }
            dir = here.parent();
        }
    }
    roots
}

/// The first candidate of `topic` that exists under one of `roots` (in root order).
#[must_use]
pub fn find_help(topic: HelpTopic, roots: &[PathBuf]) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        topic
            .candidates()
            .iter()
            .map(|rel| root.join(rel))
            .find(|path| path.is_file())
    })
}

/// Why a Help item opened nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpError {
    /// `help_docs_missing` or `help_open_failed`.
    pub reason: &'static str,
    /// What to show.
    pub message: String,
}

impl HelpError {
    /// The protocol error.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let mut error = RpcError::new(
            NOT_FOUND,
            "F0003",
            self.message.clone(),
            "see book/agents/editor-menus.md",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("reason".into(), Value::String(self.reason.to_string()));
        }
        error
    }
}

/// The file a Help item opens, from the default [`search_roots`].
///
/// # Errors
/// `help_docs_missing` when no local copy exists.
pub fn help_path(topic: HelpTopic) -> Result<PathBuf, HelpError> {
    let roots = search_roots();
    find_help(topic, &roots).ok_or_else(|| HelpError {
        reason: "help_docs_missing",
        message: format!(
            "no local {} found (looked for {} under {} directories; set {BOOK_ENV} to the book)",
            topic.name(),
            topic.candidates().join(", "),
            roots.len()
        ),
    })
}

/// Opens a Help item in the OS viewer, unless `dry_run`. Answers the file.
///
/// # Errors
/// `help_docs_missing`, or `help_open_failed` when the viewer cannot start.
pub fn open_help(topic: HelpTopic, dry_run: bool) -> Result<PathBuf, HelpError> {
    let path = help_path(topic)?;
    if !dry_run {
        crate::package_job::open_path(&path).map_err(|message| HelpError {
            reason: "help_open_failed",
            message,
        })?;
    }
    Ok(path)
}

/// What About shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AboutInfo {
    /// The editor crate's version.
    pub version: String,
    /// `REGIA_GIT_HASH` at build time, or `unknown`.
    pub git_revision: String,
    /// The first line of `LICENSE` (its title), or `None` when no license file was found.
    pub license_title: Option<String>,
    /// `LICENSE` beside the book, when found.
    pub license_path: Option<PathBuf>,
    /// `THIRD_PARTY_NOTICES.md` beside the book, when found.
    pub notices_path: Option<PathBuf>,
}

impl AboutInfo {
    /// The protocol object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        json!({
            "name": "Regia Engine",
            "version": self.version,
            "git_revision": self.git_revision,
            "license_title": self.license_title,
            "license_spdx": LICENSE_SPDX,
            "license_url": LICENSE_URL,
            "license_path": path(&self.license_path),
            "notices_path": path(&self.notices_path),
        })
    }
}

/// The first `# ` heading of a Markdown text.
#[must_use]
pub fn first_heading(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("# "))
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
}

/// The title of a licence text: its first `# ` heading when it is Markdown, else its first
/// non-empty line (the plain-text MPL starts with its name).
#[must_use]
pub fn license_title(text: &str) -> Option<String> {
    first_heading(text).or_else(|| {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    })
}

/// The About fields, with the license and notices looked up under `roots`.
#[must_use]
pub fn about_info_in(roots: &[PathBuf]) -> AboutInfo {
    let find = |name: &str| roots.iter().map(|r| r.join(name)).find(|p| p.is_file());
    let license_path = find(LICENSE_FILE);
    let license_title = license_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| license_title(&text));
    AboutInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_revision: option_env!("REGIA_GIT_HASH").unwrap_or("unknown").to_string(),
        license_title,
        license_path,
        notices_path: find(NOTICES_FILE),
    }
}

/// The About fields from the default [`search_roots`].
#[must_use]
pub fn about_info() -> AboutInfo {
    about_info_in(&search_roots())
}

/// The About window.
#[derive(Resource, Debug, Clone, Default)]
pub struct AboutDialog {
    /// Whether the window is open.
    pub open: bool,
    /// The fields gathered when it opened.
    pub info: Option<AboutInfo>,
}

/// Opens the About window (gathers its fields once).
pub fn open_about(world: &mut World) {
    world.insert_resource(AboutDialog {
        open: true,
        info: Some(about_info()),
    });
}

/// Draws the About window when it is open.
pub fn show(ctx: &egui::Context, world: &mut World) {
    // F1 help queued by `help.context` opens here, outside the command (ADR-7767).
    open_pending_help(ctx, world);
    let Some(dialog) = world.get_resource::<AboutDialog>().cloned() else {
        return;
    };
    let (true, Some(info)) = (dialog.open, dialog.info) else {
        return;
    };
    let mut open = true;
    egui::Window::new("About Regia Engine")
        .default_pos(crate::tool_windows::default_pos(ctx, "About Regia Engine"))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.strong(format!("Regia Engine {}", info.version));
            ui.label(format!("Git revision {}", info.git_revision));
            ui.horizontal(|ui| {
                ui.label(format!("License: {LICENSE_NAME} ({LICENSE_SPDX})"));
                ui.hyperlink_to("mozilla.org/MPL/2.0", LICENSE_URL);
            });
            ui.weak("Open source. Your game's own code and content stay yours.");
            if info.license_path.is_none() {
                ui.weak("(LICENSE not found beside this build)");
            }
            ui.horizontal(|ui| {
                if let Some(path) = &info.license_path
                    && crate::agent_parity::action_button(ui, &crate::agent_parity::HELP_ABOUT_LICENSE)
                        .clicked()
                    && let Err(error) = crate::package_job::open_path(path)
                {
                    tracing::warn!(target: "regia_editor", "{error}");
                }
                if let Some(path) = &info.notices_path
                    && crate::agent_parity::action_button(ui, &crate::agent_parity::HELP_ABOUT_NOTICES)
                        .clicked()
                    && let Err(error) = crate::package_job::open_path(path)
                {
                    tracing::warn!(target: "regia_editor", "{error}");
                }
            });
            if info.notices_path.is_none() {
                ui.weak("Third-party notices: THIRD_PARTY_NOTICES.md ships beside the license.");
            }
        });
    if let Some(mut dialog) = world.get_resource_mut::<AboutDialog>() {
        dialog.open = open;
    }
}

// ---------------------------------------------------------------------------------------------
// Contextual help (F1, `help.context`, ADR-7767)

/// The book pages of a panel: the user-guide page and the agent page (paths under `book/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelHelp {
    /// The user-guide page (`user-guide/editor-guide/viewport.md`).
    pub guide: &'static str,
    /// The agent page (`agents/viewports.md`).
    pub agent: &'static str,
}

/// The book pages of `panel`, or why it has none (an extension's panel documents itself). The match
/// is exhaustive, so a new panel does not compile without a page or a reason; a test checks every
/// page exists.
///
/// # Errors
///
/// The allow-list reason of a panel without a book page.
pub fn panel_help(panel: crate::shell::Panel) -> Result<PanelHelp, &'static str> {
    use crate::shell::Panel as P;
    let (guide, agent) = match panel {
        P::Viewport => ("user-guide/editor-guide/viewport.md", "agents/viewports.md"),
        P::Outliner => (
            "user-guide/editor-guide/outliner.md",
            "agents/editor-hierarchy.md",
        ),
        P::Inspector => ("user-guide/editor-guide/inspector.md", "agents/inspector.md"),
        P::Content => (
            "user-guide/editor-guide/content-browser.md",
            "agents/content-browser.md",
        ),
        P::OutputLog => ("user-guide/editor.md", "agents/editor-diagnostics.md"),
        P::Material => ("user-guide/materials.md", "agents/editor.md"),
        P::ProjectSettings => (
            "user-guide/editor-guide/project-settings.md",
            "agents/editor-preferences.md",
        ),
        P::AssetViewer => ("user-guide/editor.md", "agents/editor-asset-browser.md"),
        P::AnimGraph => ("user-guide/animation.md", "agents/animation-editor.md"),
        P::AnimClip => ("user-guide/animation.md", "agents/animation-editor.md"),
        P::AnimSkeleton => ("user-guide/animation.md", "agents/animation-editor.md"),
        P::Kit => ("user-guide/open-kit.md", "agents/open-kit.md"),
        P::Bake => ("user-guide/bake-panel.md", "agents/editor-bake.md"),
        P::BakeRunner => ("user-guide/bake-panel.md", "agents/baked-gi-editor.md"),
        P::Lightmaps => ("user-guide/bake-panel.md", "agents/lightmaps.md"),
        P::Theme => ("user-guide/ui.md", "agents/ui-theme-panel.md"),
        P::Shots => ("user-guide/shot-sequences.md", "agents/shot-panel.md"),
        P::Net => ("user-guide/networking.md", "agents/pie-net.md"),
        P::Tool => ("user-guide/editor.md", "agents/editor-tools.md"),
        P::Mode2d => ("user-guide/2d.md", "agents/editor-2d.md"),
        P::DccFeed => ("user-guide/editor.md", "agents/dcc-feed.md"),
        P::ProcGraph => ("user-guide/editor-guide/procgen.md", "agents/procgen-editor.md"),
        P::ContentChunks => ("user-guide/cooked-packs.md", "agents/content-chunks-panel.md"),
        P::Extensions => ("user-guide/editor.md", "agents/plugins.md"),
        P::SeqCurves => (
            "user-guide/editor-guide/sequencer.md",
            "agents/seq-curve-panel.md",
        ),
        P::UiDesigner => ("user-guide/editor-guide/ui-designer.md", "agents/ui-designer.md"),
        P::UiAnim => ("user-guide/ui.md", "agents/ui-anim-designer.md"),
        P::Fracture => ("user-guide/editor-guide/fracture.md", "agents/fracture.md"),
        P::Tables => ("user-guide/data-tables.md", "agents/editor-data-panels.md"),
        P::Abilities => ("user-guide/gameplay.md", "agents/editor-gameplay-panels.md"),
        P::Montage => ("user-guide/montages.md", "agents/montages.md"),
        P::Ragdoll => ("user-guide/physics.md", "agents/editor-gameplay-panels.md"),
        P::Problems => ("user-guide/editor.md", "agents/asset-validators.md"),
        P::ReferenceGraph => ("user-guide/asset-registry.md", "agents/asset-registry.md"),
        P::SizeMap => ("user-guide/asset-registry.md", "agents/asset-registry.md"),
        P::Layers => ("user-guide/scenes.md", "agents/levels-layers-instances.md"),
        P::Tests => ("user-guide/editor.md", "agents/functional-tests.md"),
        P::StateTrees => ("user-guide/gameplay.md", "agents/state-tree.md"),
        P::SmartObjects => ("user-guide/gameplay.md", "agents/smart-objects.md"),
        P::Cameras => ("user-guide/gameplay.md", "agents/camera-framework.md"),
        P::MaterialGraph => (
            "user-guide/editor-guide/material-graph.md",
            "agents/material-graph-editor.md",
        ),
        P::AudioMixer => ("user-guide/audio.md", "agents/audio-mixer.md"),
        P::CueEditor => ("user-guide/audio.md", "agents/audio.md"),
        P::AgentChanges => ("user-guide/agent-protocol.md", "agents/staged-review.md"),
        P::Preferences => ("user-guide/editor.md", "agents/editor-preferences.md"),
        P::Terrain => ("user-guide/terrain-data.md", "agents/level-terrain.md"),
        P::BehaviorTree => ("user-guide/navigation-ai.md", "agents/behavior-tree-editor.md"),
        P::MotionMatching => ("user-guide/motion-matching.md", "agents/motion-matching.md"),
        P::UiInspector => ("user-guide/ui.md", "agents/ui-inspector.md"),
        P::WorldSettings => ("user-guide/editor.md", "agents/editor.md"),
        P::VfxEditor => ("user-guide/terrain-vfx.md", "agents/vfx.md"),
        P::StateTreeEditor => ("user-guide/gameplay.md", "agents/state-tree-editor.md"),
        P::Video => ("user-guide/editor.md", "agents/video-panel.md"),
        P::Conversation => ("user-guide/dialogue.md", "agents/conversation-editor.md"),
        P::Localization => ("user-guide/localization.md", "agents/editor-localization.md"),
        P::Cloth => ("user-guide/cloth.md", "agents/cloth.md"),
        P::LookDev => ("user-guide/lookdev.md", "agents/editor-lookdev.md"),
        P::DataAsset => ("user-guide/data-tables.md", "agents/editor-data-panels.md"),
        P::VehicleTuning => ("user-guide/editor-guide/vehicle-tuning.md", "agents/vehicles.md"),
        // Panels merged after 3206's table (integ-v06k).
        P::LogicGraph => ("user-guide/editor-guide/logic-graph.md", "agents/logic-graph.md"),
        P::SmartObjectDef => (
            "user-guide/editor-guide/index.md",
            "agents/smart-object-editor.md",
        ),
        P::Biome => ("user-guide/editor-guide/procgen.md", "agents/biome-editor.md"),
        P::BulkEdit => ("user-guide/editor-guide/bulk-edit.md", "agents/bulk-edit.md"),
        P::ProjectHealth => ("user-guide/editor-guide/problems.md", "agents/project-health.md"),
        P::Retarget => ("user-guide/animation.md", "agents/retargeting.md"),
        P::WorldMap => (
            "user-guide/world-streaming.md",
            "agents/world-streaming-editor.md",
        ),
        P::UndoHistory => ("user-guide/editor.md", "agents/undo.md"),
        P::SelectedDebug | P::VisualLog => (
            "user-guide/editor-guide/play-in-editor.md",
            "agents/visual-log.md",
        ),
        P::Notes | P::Activity => ("user-guide/editor.md", "agents/review-notes.md"),
        P::MatchRules => ("user-guide/game-modes.md", "agents/editor-match-rules.md"),
        P::GameView(..) => ("user-guide/editor-guide/play-in-editor.md", "agents/pie-net.md"),
        P::UtilityPanel(..) => ("user-guide/editor.md", "agents/utility-panels.md"),
        P::Agent => ("user-guide/editor.md", "agents/agent-panel.md"),
        P::ExtensionPanel(..) => {
            return Err("an extension's panel is documented by the extension, not the engine book");
        }
        // A further Outliner, Content Browser or Inspector (P-003) has its first instance's pages.
        P::Instance(kind, _) => return panel_help(kind.base()),
    };
    Ok(PanelHelp { guide, agent })
}

/// The Inspector field under the pointer, for F1 over a field. The Inspector's field rows set it
/// from label hit targets before F1 dispatch; keyboard-focused labels work as well.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct HoveredField {
    /// The component's short type name (`Transform`).
    pub component: String,
    /// The field path (`translation.x`).
    pub path: String,
}

#[derive(Resource, Default)]
struct HelpUiFrame {
    current: u64,
    queued: Option<u64>,
}

// The shell dispatches F1 before drawing the dock. Retain the last frame's label hit targets,
// then resolve against the current pointer/focus before dispatch; never retain a stale hover.
#[derive(Clone, Default)]
struct FieldHelpTargets {
    frame: u64,
    rows: Vec<(egui::Rect, egui::Id, egui::LayerId, HoveredField)>,
}

impl HoveredField {
    /// Records one Inspector label's hit target without borrowing the editor world.
    pub(crate) fn record(response: &egui::Response, component: &str, path: &str) {
        let ctx = &response.ctx;
        let frame = ctx.cumulative_frame_nr();
        ctx.data_mut(|data| {
            let targets =
                data.get_temp_mut_or_default::<FieldHelpTargets>(egui::Id::new("field_help_targets"));
            if targets.frame != frame {
                targets.rows.clear();
                targets.frame = frame;
            }
            let field = Self {
                component: component.rsplit("::").next().unwrap_or(component).to_owned(),
                path: path.to_owned(),
            };
            targets
                .rows
                .push((response.rect, response.id, response.layer_id, field));
        });
    }

    /// Resolves the current pointer or keyboard-focused label and clears expired field help.
    #[tracing::instrument(target = "regia_editor", skip_all, name = "field_help_sync")]
    pub(crate) fn sync(ctx: &egui::Context, world: &mut World) {
        world.get_resource_or_insert_with(HelpUiFrame::default).current = ctx.cumulative_frame_nr();
        let targets =
            ctx.data_mut(|data| data.get_temp::<FieldHelpTargets>(egui::Id::new("field_help_targets")));
        let pointer = ctx.pointer_hover_pos();
        let focused = ctx.memory(|memory| memory.focused());
        let field = targets
            .filter(|t| ctx.cumulative_frame_nr().saturating_sub(t.frame) <= 1)
            .and_then(|t| {
                t.rows
                    .iter()
                    .rev()
                    .find(|(rect, _, layer, _)| {
                        pointer.is_some_and(|p| rect.contains(p) && ctx.layer_id_at(p) == Some(*layer))
                    })
                    .or_else(|| t.rows.iter().rev().find(|(_, id, _, _)| Some(*id) == focused))
                    .map(|(_, _, _, field)| field.clone())
            });
        if let Some(field) = field {
            world.insert_resource(field);
        } else {
            world.remove_resource::<Self>();
        }
    }
}

/// What F1 opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextHelp {
    /// The panel id the help is for, when a panel answered.
    pub panel: Option<&'static str>,
    /// The page under `book/` (`agents/inspector.md`).
    pub page: String,
    /// The heading anchor on the page (a component's section), when one matched.
    pub anchor: Option<String>,
    /// The local file to open (the rendered page, else its Markdown), `None` without a local book.
    pub path: Option<PathBuf>,
}

impl ContextHelp {
    /// The protocol object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "panel": self.panel,
            "page": self.page,
            "anchor": self.anchor,
            "path": self.path.as_ref().map(|p| p.display().to_string()),
        })
    }
}

/// An mdBook heading anchor: lowercase, spaces to `-`, other punctuation dropped.
#[must_use]
pub fn heading_anchor(heading: &str) -> String {
    heading
        .trim()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                Some(c.to_ascii_lowercase())
            } else if c == ' ' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// The first agent page section (in file-name order) whose heading names `component` as a whole
/// word, under `book/agents` of `book_root`: the page (`agents/<file>.md`) and the heading anchor.
#[must_use]
pub fn component_section(book_root: &Path, component: &str) -> Option<(String, String)> {
    if component.is_empty() {
        return None;
    }
    let dir = book_root.join("book/agents");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    let names_component = |line: &str| {
        line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|word| word == component)
    };
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            let Some(heading) = line.strip_prefix('#').map(|h| h.trim_start_matches('#')) else {
                continue;
            };
            if line.starts_with("##") && names_component(heading) {
                let name = file.file_name()?.to_str()?;
                return Some((format!("agents/{name}"), heading_anchor(heading)));
            }
        }
    }
    None
}

/// The local file of book page `page` under one of `roots`: the rendered page
/// (`book/build/<page>.html`), else its Markdown source.
#[must_use]
pub fn book_page_path(roots: &[PathBuf], page: &str) -> Option<PathBuf> {
    let html = format!("book/build/{}", page.trim_end_matches(".md")) + ".html";
    let md = format!("book/{page}");
    roots.iter().find_map(|root| {
        [root.join(&html), root.join(&md)]
            .into_iter()
            .find(|p| p.is_file())
    })
}

/// What F1 opens for `panel` and the hovered `field`, searching the book under `roots`: the agent
/// page section of the field's component when one exists, else the panel's user-guide page, else
/// the editor guide index.
#[must_use]
pub fn context_help_in(
    roots: &[PathBuf],
    panel: Option<crate::shell::Panel>,
    field: Option<&HoveredField>,
) -> ContextHelp {
    if let Some(field) = field {
        for root in roots {
            if let Some((page, anchor)) = component_section(root, &field.component) {
                return ContextHelp {
                    panel: panel.map(crate::shell::Panel::id),
                    path: book_page_path(std::slice::from_ref(root), &page),
                    page,
                    anchor: Some(anchor),
                };
            }
        }
    }
    let page = panel
        .and_then(|p| panel_help(p).ok())
        .map_or("user-guide/editor-guide/index.md", |h| h.guide);
    ContextHelp {
        panel: panel.map(crate::shell::Panel::id),
        page: page.to_owned(),
        anchor: None,
        path: book_page_path(roots, page),
    }
}

/// Help the next frame opens (queued by `help.context`, opened by [`show`]).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct PendingHelp(pub ContextHelp);

/// The `help.context` command (F1): queues the help of the focused panel (or hovered field) for the
/// next frame and answers a status line.
pub fn help_context_command(world: &mut World) -> String {
    let panel = world
        .get_resource::<crate::editor_keymap::FocusedPanel>()
        .and_then(|f| f.0);
    let field = world.get_resource::<HoveredField>().cloned();
    let help = context_help_in(&search_roots(), panel, field.as_ref());
    let line = match (&help.path, &help.anchor) {
        (Some(_), Some(anchor)) => format!("Help: book/{} § {anchor}", help.page),
        (Some(_), None) => format!("Help: book/{}", help.page),
        (None, _) => format!(
            "help_docs_missing: no local book holds {} (set {BOOK_ENV} to the book)",
            help.page
        ),
    };
    if help.path.is_some() {
        let first_pass = world.get_resource_mut::<HelpUiFrame>().is_none_or(|mut frame| {
            if frame.queued == Some(frame.current) {
                return false;
            }
            frame.queued = Some(frame.current);
            true
        });
        if first_pass {
            world.insert_resource(PendingHelp(help));
        }
    }
    line
}

/// Opens queued contextual help in the OS viewer (called by [`show`] every frame).
fn open_pending_help(ctx: &egui::Context, world: &mut World) {
    if let Some(PendingHelp(help)) = world.remove_resource::<PendingHelp>()
        && let Some(path) = &help.path
    {
        if path.extension().is_some_and(|ext| ext == "html") {
            match path.canonicalize() {
                Ok(path) => ctx.open_url(egui::OpenUrl::new_tab(help_url(&path, help.anchor.as_deref()))),
                Err(error) => tracing::warn!(target: "regia_editor", "help: {}: {error}", path.display()),
            }
        } else if let Err(error) = crate::package_job::open_path(path) {
            tracing::warn!(target: "regia_editor", "help: {error}");
        }
    }
}

// Encode a local path as a file URI; a fragment belongs to the URI, never to the filename.
fn help_url(path: &Path, anchor: Option<&str>) -> String {
    use std::fmt::Write as _;
    let path = path.to_string_lossy().replace('\\', "/");
    let path = if let Some(unc) = path.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        path.strip_prefix("//?/").unwrap_or(&path).to_owned()
    };
    let mut url = if path.starts_with("//") {
        "file:"
    } else if path.starts_with('/') {
        "file://"
    } else {
        "file:///"
    }
    .to_owned();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            url.push(char::from(byte));
        } else {
            let _ = write!(url, "%{byte:02X}");
        }
    }
    if let Some(anchor) = anchor {
        url.push('#');
        for byte in anchor.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                url.push(char::from(byte));
            } else {
                let _ = write!(url, "%{byte:02X}");
            }
        }
    }
    url
}

/// `editor.help_context { panel?, field?, open? }`.
fn help_context(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad = |code: &str, message: String| {
        RpcError::new(
            regia_agent::protocol::INVALID_PARAMS,
            code,
            message,
            "see book/agents/editor-preferences.md",
        )
    };
    let panel = match params.get("panel") {
        None | Some(Value::Null) => world
            .get_resource::<crate::editor_keymap::FocusedPanel>()
            .and_then(|f| f.0),
        Some(v) => {
            let id = v.as_str().unwrap_or_default();
            let panel = crate::shell::Panel::from_id(id)
                .ok_or_else(|| bad("unknown_panel", format!("no panel has the id {id:?}")))?;
            Some(panel)
        }
    };
    if let Some(p) = panel
        && let Err(reason) = panel_help(p)
    {
        return Err(bad("help_no_page", format!("{}: {reason}", p.id())));
    }
    let field = match params.get("field") {
        None | Some(Value::Null) => world.get_resource::<HoveredField>().cloned(),
        Some(v) => Some(HoveredField {
            component: v
                .get("component")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("bad_field", "`field.component` must be a component name".into()))?
                .to_owned(),
            path: v
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
    };
    let help = context_help_in(&search_roots(), panel, field.as_ref());
    let open = params.get("open").and_then(Value::as_bool).unwrap_or(false);
    let mut out = help.to_json();
    out["agent_page"] = json!(panel.and_then(|p| panel_help(p).ok()).map(|h| h.agent));
    out["queued"] = json!(open && help.path.is_some());
    if open && help.path.is_some() {
        world.insert_resource(PendingHelp(help));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Agent methods

/// `editor.about`.
fn about(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let info = about_info();
    let open = params.get("open").and_then(Value::as_bool).unwrap_or(false);
    if open {
        world.insert_resource(AboutDialog {
            open: true,
            info: Some(info.clone()),
        });
    }
    let mut out = info.to_json();
    out["open"] = json!(world.get_resource::<AboutDialog>().is_some_and(|d| d.open));
    Ok(out)
}

/// `editor.help_open`.
fn help_open(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("topic")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`topic` is required: documentation or agent_guide"))?;
    let topic = HelpTopic::parse(name).ok_or_else(|| {
        RpcError::invalid_params(format!("unknown topic {name:?} (documentation or agent_guide)"))
    })?;
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let path = open_help(topic, dry_run).map_err(|e| e.rpc())?;
    Ok(json!({ "topic": topic.name(), "path": path.display().to_string(), "opened": !dry_run }))
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.about",
                tool: "about",
                description: "Help ▸ About: the editor version, git revision, the license (title from LICENSE, \
                    SPDX id MPL-2.0 and its URL) and the paths of LICENSE and THIRD_PARTY_NOTICES.md (null when \
                    not found). `open: true` opens the About window too.",
                params: &[ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "Open the About window (default false: read only).",
                }],
                result: "{ name, version, git_revision, license_title, license_spdx, license_url, license_path, notices_path, open }",
                // `open: true` changes editor state (the About window), so the method is declared
                // mutating (review w8 finding 10).
                mutating: true,
                permission: Permission::EditScene,
            },
            about as Handler,
        ),
        (
            MethodSchema {
                name: "editor.help_open",
                tool: "help_open",
                description: "Help ▸ Documentation or Help ▸ Agent guide: open the local book page (the rendered \
                    book, else the generated manual, else the Markdown sources; $REGIA_BOOK first) in the OS \
                    viewer. help_docs_missing when no local copy exists; help_open_failed when the viewer \
                    cannot start. `dry_run` answers the file without opening it.",
                params: &[
                    ParamSchema {
                        name: "topic",
                        kind: "string",
                        required: true,
                        description: "documentation or agent_guide.",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Find the file, open nothing.",
                    },
                ],
                result: "{ topic, path, opened }",
                // Starts an OS viewer (review w8 finding 10).
                mutating: true,
                permission: Permission::EditScene,
            },
            help_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.help_context",
                tool: "help_context",
                description: "Contextual help (F1, the help.context command): the book page for a panel (the focused one by default) and, for an Inspector field, the agent page section of its component when one exists. `open: true` opens it in the OS viewer on the next frame. unknown_panel, help_no_page (an extension's panel), bad_field.",
                params: &[
                    ParamSchema {
                        name: "panel",
                        kind: "string",
                        required: false,
                        description: "A panel id (viewport, inspector, content, ...); default the focused panel.",
                    },
                    ParamSchema {
                        name: "field",
                        kind: "object",
                        required: false,
                        description: "{ component, path }: an Inspector field; its component's agent page section wins when one exists.",
                    },
                    ParamSchema {
                        name: "open",
                        kind: "boolean",
                        required: false,
                        description: "Open the page (default false: answer only).",
                    },
                ],
                result: "{ panel, page, anchor, path, agent_page, queued }",
                mutating: true,
                permission: Permission::EditScene,
            },
            help_context as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_prefers_the_rendered_book_then_the_manual_then_sources() {
        let root = regia_test_utils::unique_temp_dir("help_find");
        let other = regia_test_utils::unique_temp_dir("help_find_other");
        std::fs::create_dir_all(root.join("book/agents")).expect("book");
        std::fs::create_dir_all(root.join("docs/manual")).expect("manual");
        std::fs::write(root.join("book/introduction.md"), "# Intro").expect("intro");
        std::fs::write(root.join("book/agents/protocol.md"), "# P").expect("protocol");
        let roots = vec![other.clone(), root.clone()];
        assert_eq!(
            find_help(HelpTopic::Documentation, &roots),
            Some(root.join("book/introduction.md"))
        );
        std::fs::write(root.join("docs/manual/manual.html"), "<html>").expect("manual");
        assert_eq!(
            find_help(HelpTopic::Documentation, &roots),
            Some(root.join("docs/manual/manual.html"))
        );
        assert_eq!(
            find_help(HelpTopic::AgentGuide, &roots),
            Some(root.join("book/agents/protocol.md"))
        );
        assert_eq!(
            find_help(HelpTopic::AgentGuide, std::slice::from_ref(&other)),
            None
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn about_reads_the_license_heading_and_the_notices_path() {
        let root = regia_test_utils::unique_temp_dir("about_info");
        std::fs::create_dir_all(&root).expect("root");
        let info = about_info_in(std::slice::from_ref(&root));
        assert_eq!((info.license_title, info.notices_path), (None, None));
        std::fs::write(
            root.join(LICENSE_FILE),
            "\nMozilla Public License Version 2.0\n==================================\n\nText",
        )
        .expect("licence");
        std::fs::write(root.join(NOTICES_FILE), "# Notices").expect("notices");
        let info = about_info_in(std::slice::from_ref(&root));
        assert_eq!(
            info.license_title.as_deref(),
            Some("Mozilla Public License Version 2.0")
        );
        assert_eq!(info.license_path, Some(root.join(LICENSE_FILE)));
        let json = info.to_json();
        assert_eq!(
            (json["license_spdx"].clone(), json["license_url"].clone()),
            (json!("MPL-2.0"), json!(LICENSE_URL))
        );
        // A Markdown licence still answers its heading.
        assert_eq!(license_title("\n# Title\nbody").as_deref(), Some("Title"));
        assert_eq!(license_title("\n  \n"), None);
        assert_eq!(info.notices_path, Some(root.join(NOTICES_FILE)));
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn every_panel_has_a_book_page_or_a_reason() {
        let book = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book");
        for panel in crate::editor_layout::BUILT_IN_PANELS {
            let help = panel_help(*panel).unwrap_or_else(|r| panic!("{panel:?}: {r}"));
            for page in [help.guide, help.agent] {
                assert!(book.join(page).is_file(), "{panel:?}: book/{page} is missing");
            }
        }
        assert!(panel_help(crate::shell::Panel::ExtensionPanel("x", "X")).is_err());
    }

    #[test]
    fn f1_over_a_hovered_field_opens_its_section() {
        use crate::canvas_keys::test_input::{key, raw};
        use crate::inspector::reflect_ui::{InspectorContext, InspectorUi};
        #[derive(Reflect)]
        struct HelpFixture {
            score: f32,
        }
        let ctx = egui::Context::default();
        let mut registry = TypeRegistry::default();
        registry.register::<HelpFixture>();
        let mut value = HelpFixture { score: 1.0 };
        let mut world = World::new();
        let mut frame = |events| {
            ctx.run_ui(raw([800.0, 600.0], events), |ui| {
                crate::command_palette::show(ui.ctx(), &mut world, false);
                // Use a component with a shipped manual section; no global environment mutation.
                let mut context = InspectorContext::new("UtilityReasoner");
                InspectorUi::new(&registry, &mut context).ui_for_reflect(&mut value, ui);
            })
            .textures_delta
            .clear()
        };
        frame(vec![]);
        let targets = ctx
            .data_mut(|data| data.get_temp::<FieldHelpTargets>(egui::Id::new("field_help_targets")))
            .expect("Inspector recorded its labels");
        let (rect, id, _, field) = targets
            .rows
            .iter()
            .find(|(_, _, _, field)| field.path == "score")
            .expect("score label");
        assert_eq!(field.component, "UtilityReasoner");
        frame(vec![
            egui::Event::PointerMoved(rect.center()),
            key(egui::Key::F1, egui::Modifiers::NONE),
        ]);
        let pending = world.resource::<PendingHelp>();
        assert_eq!(pending.0.page, "agents/utility-ai.md");
        assert_eq!(pending.0.anchor.as_deref(), Some("on-entities-utilityreasoner"));
        world.remove_resource::<PendingHelp>();
        help_context_command(&mut world);
        assert!(
            !world.contains_resource::<PendingHelp>(),
            "a repeated UI pass queues only once"
        );
        ctx.run_ui(raw([800.0, 600.0], vec![egui::Event::PointerGone]), |ui| {
            HoveredField::sync(ui.ctx(), &mut world);
        })
        .textures_delta
        .clear();
        assert!(
            !world.contains_resource::<HoveredField>(),
            "moving away clears help"
        );
        ctx.memory_mut(|memory| memory.request_focus(*id));
        // Record a current keyboard focus target after the intervening frame.
        ctx.run_ui(raw([800.0, 600.0], vec![]), |ui| {
            let mut context = InspectorContext::new("UtilityReasoner");
            InspectorUi::new(&registry, &mut context).ui_for_reflect(&mut value, ui);
            HoveredField::sync(ui.ctx(), &mut world);
        })
        .textures_delta
        .clear();
        assert_eq!(world.resource::<HoveredField>().path, "score");
    }

    #[test]
    fn help_urls_encode_paths_and_keep_windows_drive_and_share_roots() {
        assert_eq!(
            help_url(Path::new(r"C:\docs\a #.html"), Some("séction")),
            "file:///C:/docs/a%20%23.html#s%C3%A9ction"
        );
        assert_eq!(
            help_url(Path::new(r"\\?\UNC\server\share\a.html"), None),
            "file://server/share/a.html"
        );
    }

    #[test]
    fn rendered_help_keeps_its_section_in_the_open_url() {
        let root = regia_test_utils::unique_temp_dir("help-url");
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("a page.html");
        std::fs::write(&path, "<h2 id=component>Component</h2>").expect("page");
        let mut world = World::new();
        world.insert_resource(PendingHelp(ContextHelp {
            panel: None,
            page: "a page.md".into(),
            anchor: Some("component".into()),
            path: Some(path),
        }));
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            open_pending_help(ui.ctx(), &mut world)
        });
        output.textures_delta.clear();
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::OpenUrl(url) if url.url.ends_with("a%20page.html#component"))
        }));
        assert!(!world.contains_resource::<PendingHelp>());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn f1_over_a_field_opens_its_component_section_else_the_panel_page() {
        let root = regia_test_utils::unique_temp_dir("help_context");
        std::fs::create_dir_all(root.join("book/agents")).expect("agents");
        std::fs::create_dir_all(root.join("book/user-guide/editor-guide")).expect("guide");
        std::fs::write(
            root.join("book/agents/lamps.md"),
            "# Lamps\n\n## The `Lamp` component\n",
        )
        .expect("page");
        std::fs::write(
            root.join("book/user-guide/editor-guide/inspector.md"),
            "# Inspector",
        )
        .expect("guide");
        std::fs::write(root.join("book/user-guide/editor-guide/index.md"), "# Guide").expect("index");
        let roots = vec![root.clone()];
        let field = HoveredField {
            component: "Lamp".into(),
            path: "brightness".into(),
        };
        let help = context_help_in(&roots, Some(crate::shell::Panel::Inspector), Some(&field));
        assert_eq!(help.page, "agents/lamps.md");
        assert_eq!(help.anchor.as_deref(), Some("the-lamp-component"));
        assert_eq!(help.path, Some(root.join("book/agents/lamps.md")));
        // No section for the component: the focused panel's page.
        let other = HoveredField {
            component: "Lamps2".into(),
            path: String::new(),
        };
        let help = context_help_in(&roots, Some(crate::shell::Panel::Inspector), Some(&other));
        assert_eq!(help.page, "user-guide/editor-guide/inspector.md");
        assert_eq!(help.anchor, None);
        // No panel: the guide index.
        assert_eq!(
            context_help_in(&roots, None, None).page,
            "user-guide/editor-guide/index.md"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn help_context_method_answers_the_page_and_refuses_unknown_panels() {
        let mut world = World::new();
        world.insert_resource(crate::editor_keymap::FocusedPanel(Some(
            crate::shell::Panel::Viewport,
        )));
        let out = help_context(&mut world, &json!({})).expect("focused panel");
        assert_eq!(out["panel"], "viewport");
        assert_eq!(out["page"], "user-guide/editor-guide/viewport.md");
        assert_eq!(out["agent_page"], "agents/viewports.md");
        assert_eq!(out["queued"], false);
        let code = |e: RpcError| {
            e.data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        assert_eq!(
            code(help_context(&mut world, &json!({ "panel": "nope" })).expect_err("unknown")),
            "unknown_panel"
        );
        assert_eq!(
            code(help_context(&mut world, &json!({ "field": { "path": "x" } })).expect_err("no component")),
            "bad_field"
        );
        // The command queues nothing without a local copy of the page and never opens a viewer itself.
        let line = help_context_command(&mut world);
        assert!(
            line.starts_with("Help: ") || line.starts_with("help_docs_missing"),
            "{line}"
        );
    }
}
