// SPDX-License-Identifier: MPL-2.0
//! The samples browser (`docs/specs/19-editor.md` § 10.19, `book/agents/samples.md`).
//!
//! **Help ▸ Samples…** lists three kinds of starting points, offline:
//!
//! * **Project templates** the engine scaffolds (`blank`, `first_person`, `third_person`, `top_down`,
//!   `net_game`), built from the sources bundled in `regia_templates`;
//! * **Samples** of the engine checkout (`samples/<name>/`, described by their `Cargo.toml`);
//! * **Extension examples** of the engine checkout (`extensions/<id>/`, described by their
//!   `regia-plugin.toml`).
//!
//! An extension may also bundle samples of its own: every folder under `<extension>/samples/` is a
//! sample the Extensions panel lists under the extension
//! ([`extension_samples`]). The convention needs no
//! manifest field: the description is the first paragraph of the folder's `README.md`.
//!
//! **Open** never edits a sample in place: it copies the sample into a folder the person chooses
//! ([`create`]) and, when the copy is a project (it has a project
//! manifest), opens the copy.
//! `editor.samples { action: "list" | "create" }` does the same for agents.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::extension::ExtensionRegistry;
use serde_json::{Value, json};

/// What kind of starting point a [`Sample`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    /// A project template the engine scaffolds.
    Template,
    /// A sample project of the engine checkout (`samples/<name>/`).
    Sample,
    /// An example extension of the engine checkout (`extensions/<id>/`).
    Extension,
    /// A sample an extension bundles (`<extension>/samples/<name>/`).
    ExtensionSample,
}

impl SampleKind {
    /// The name the agent protocol and the window use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Sample => "sample",
            Self::Extension => "extension",
            Self::ExtensionSample => "extension_sample",
        }
    }
}

/// One entry of the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The id `editor.samples` takes: `template/<id>`, `sample/<dir>`, `extension/<id>` or
    /// `extension_sample/<extension>/<dir>`.
    pub id: String,
    /// What kind of entry it is.
    pub kind: SampleKind,
    /// The display name.
    pub name: String,
    /// One or two sentences about it.
    pub description: String,
    /// The folder a copy is made from (`None` for a template, which is scaffolded).
    pub source: Option<PathBuf>,
    /// The picture the browser shows (`preview.png` in the sample folder), when there is one.
    pub preview: Option<PathBuf>,
    /// Whether the copy is a project the editor can open (it has a project manifest, or it is a template).
    pub project: bool,
}

/// Where the engine checkout is, for the samples and extension examples it holds.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    /// The engine checkout (`samples/` and `extensions/` live under it). `None` lists the templates only.
    pub engine: Option<PathBuf>,
}

impl Roots {
    /// The checkout this editor was built from (the same one the project scaffold uses). An editor that
    /// is not next to a checkout lists the templates only.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            engine: crate::scaffold::engine_root().ok(),
        }
    }
}

/// The project templates: `(id, display name, description)`. The ids are the ones `regia new` and
/// `editor.new_project` take; `quick` is a single-crate layout the editor's scaffold does not offer.
const TEMPLATES: &[(&str, &str, &str)] = &[
    (
        "blank",
        "Blank",
        "An empty project: one level, one camera, one light and a game crate that builds.",
    ),
    (
        "first_person",
        "First Person",
        "A first-person starter: a character that walks and looks, a level to move through.",
    ),
    (
        "third_person",
        "Third Person",
        "A third-person starter: a character with a follow camera and a level to move through.",
    ),
    (
        "top_down",
        "Top Down",
        "A top-down starter: a character on a fixed camera and a level to move through.",
    ),
    (
        "net_game",
        "Net Game",
        "A client, server and shared-rules workspace for a small networked game.",
    ),
];

/// Why a sample could not be listed or copied. [`Self::code`] is the agent error reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleError {
    /// The agent error code (`unknown_sample`, `sample_destination_exists`, ...).
    pub code: &'static str,
    /// What went wrong, for a person.
    pub message: String,
}

impl SampleError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The agent error.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let status = if self.code == "unknown_sample" {
            NOT_FOUND
        } else {
            INVALID_PARAMS
        };
        RpcError::new(
            status,
            self.code,
            self.message.clone(),
            "list the samples with editor.samples { action: \"list\" }",
        )
    }
}

/// A copy the browser made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// The new folder.
    pub root: PathBuf,
    /// Whether it is a project (it has a project manifest).
    pub project: bool,
}

// ---------------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------------

/// A table value of a TOML file, when the file parses.
fn toml_file(path: &Path) -> Option<toml::Value> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn preview_of(dir: &Path) -> Option<PathBuf> {
    let file = dir.join("preview.png");
    file.is_file().then_some(file)
}

/// The sub-folders of `dir` that are real folders (a link is skipped), sorted by name.
fn child_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut rows: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .filter(|(name, _)| !name.starts_with(['.', '_']))
        .collect();
    rows.sort();
    rows
}

/// The first paragraph line of a `README.md` that is not a heading.
fn readme_summary(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("README.md")).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

/// Every starting point the browser offers: the templates, then the samples and extension examples of
/// the checkout `roots` names, each group sorted by name.
#[must_use]
pub fn list(roots: &Roots) -> Vec<Sample> {
    let mut rows: Vec<Sample> = TEMPLATES
        .iter()
        .map(|(id, name, description)| Sample {
            id: format!("template/{id}"),
            kind: SampleKind::Template,
            name: (*name).to_owned(),
            description: (*description).to_owned(),
            source: None,
            preview: None,
            project: true,
        })
        .collect();
    let Some(engine) = &roots.engine else {
        return rows;
    };
    for (dir_name, dir) in child_dirs(&engine.join("samples")) {
        let Some(manifest) = toml_file(&dir.join("Cargo.toml")) else {
            continue;
        };
        let description = manifest
            .get("package")
            .and_then(|package| package.get("description"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .or_else(|| readme_summary(&dir))
            .unwrap_or_else(|| format!("The {dir_name} sample."));
        rows.push(Sample {
            id: format!("sample/{dir_name}"),
            kind: SampleKind::Sample,
            name: dir_name,
            description,
            preview: preview_of(&dir),
            project: regia_core::brand::is_project_root(&dir),
            source: Some(dir),
        });
    }
    for (dir_name, dir) in child_dirs(&engine.join("extensions")) {
        let Some(manifest) = toml_file(&dir.join("regia-plugin.toml")) else {
            continue;
        };
        let text = |key: &str| {
            manifest
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let id = text("id").unwrap_or_else(|| dir_name.clone());
        rows.push(Sample {
            id: format!("extension/{id}"),
            kind: SampleKind::Extension,
            name: text("name").unwrap_or(dir_name),
            description: text("description").unwrap_or_else(|| format!("The {id} example extension.")),
            preview: preview_of(&dir),
            project: false,
            source: Some(dir),
        });
    }
    rows
}

/// The samples an extension bundles: one per folder under `<extension_dir>/samples/`. `extension` is the
/// extension's id (it is part of each sample's id).
#[must_use]
pub fn extension_samples(extension: &str, extension_dir: &Path) -> Vec<Sample> {
    child_dirs(&extension_dir.join("samples"))
        .into_iter()
        .map(|(dir_name, dir)| Sample {
            id: format!("extension_sample/{extension}/{dir_name}"),
            kind: SampleKind::ExtensionSample,
            name: dir_name,
            description: readme_summary(&dir)
                .unwrap_or_else(|| format!("A sample bundled with {extension}.")),
            preview: preview_of(&dir),
            project: regia_core::brand::is_project_root(&dir),
            source: Some(dir),
        })
        .collect()
}

/// The folder an extension was found in, from its `plugin.info` answer (`path` is the manifest file).
fn extension_dir(info: &Value) -> Option<PathBuf> {
    Path::new(info["path"].as_str()?).parent().map(Path::to_path_buf)
}

/// The samples the loaded extensions bundle, with the extension that owns each.
#[must_use]
pub fn loaded_extension_samples(world: &World) -> Vec<(String, Sample)> {
    let Some(registry) = world.get_resource::<ExtensionRegistry>() else {
        return Vec::new();
    };
    let ids: Vec<String> = registry.list_json(None)["extensions"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ids.into_iter()
        .filter_map(|id| {
            let dir = extension_dir(&registry.info_json(&id)?)?;
            Some(
                extension_samples(&id, &dir)
                    .into_iter()
                    .map(|sample| (id.clone(), sample))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

/// Finds `id` among the checkout's entries and the loaded extensions' bundled samples.
fn find(world: Option<&World>, roots: &Roots, id: &str) -> Result<Sample, SampleError> {
    let mut all = list(roots);
    if let Some(world) = world {
        all.extend(
            loaded_extension_samples(world)
                .into_iter()
                .map(|(_, sample)| sample),
        );
    }
    all.into_iter().find(|sample| sample.id == id).ok_or_else(|| {
        SampleError::new(
            "unknown_sample",
            format!("`{id}` is not a sample, template or extension example"),
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Copying
// ---------------------------------------------------------------------------------------------

/// Folder names a copy leaves out: build output, version control and the editor's per-project state.
fn skipped(name: &str) -> bool {
    matches!(name, "target" | ".git" | "Saved")
        || name == regia_core::brand::PROJECT_STATE_DIR
        || name == regia_core::brand::LEGACY_PROJECT_STATE_DIR
}

/// A folder name must be one plain path component.
fn valid_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed == name
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'])
        && name != ".."
}

fn copy_tree(from: &Path, to: &Path, depth: u32) -> Result<(), String> {
    if depth > 32 {
        return Err(format!("{} is nested too deeply", from.display()));
    }
    std::fs::create_dir_all(to).map_err(|err| format!("{}: {err}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|err| format!("{}: {err}", from.display()))? {
        let entry = entry.map_err(|err| format!("{}: {err}", from.display()))?;
        let name = entry.file_name();
        let kind = entry
            .file_type()
            .map_err(|err| format!("{}: {err}", entry.path().display()))?;
        let target = to.join(&name);
        if kind.is_dir() {
            if !skipped(&name.to_string_lossy()) {
                copy_tree(&entry.path(), &target, depth + 1)?;
            }
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .map_err(|err| format!("{}: {err}", entry.path().display()))?;
        }
        // A symbolic link is not followed and not copied: it could point outside the sample.
    }
    Ok(())
}

/// Copies `sample` into a new folder under `parent` (or scaffolds it, for a template) and returns the
/// new folder. The sample itself is never written to. `name` is the folder name (the project name, for
/// a template); it defaults to the sample's own name.
///
/// # Errors
///
/// `sample_name_invalid`, `sample_destination_exists` (the target folder exists and is not empty),
/// `sample_destination_inside_source`, `sample_source_missing`, `sample_copy_failed` or
/// `sample_template_failed`. A failed copy removes what it wrote.
pub fn create(sample: &Sample, parent: &Path, name: Option<&str>) -> Result<Created, SampleError> {
    let tail = sample.id.rsplit('/').next().unwrap_or(&sample.name);
    let name = name.unwrap_or(tail);
    if sample.kind == SampleKind::Template {
        let created = crate::scaffold::create(&crate::scaffold::NewProjectRequest {
            name: name.to_owned(),
            template: tail.to_owned(),
            parent: parent.to_path_buf(),
        })
        .map_err(|err| match err {
            crate::scaffold::NewProjectError::BadName(_) => {
                SampleError::new("sample_name_invalid", err.to_string())
            }
            crate::scaffold::NewProjectError::Exists(_) => {
                SampleError::new("sample_destination_exists", err.to_string())
            }
            _ => SampleError::new("sample_template_failed", err.to_string()),
        })?;
        return Ok(Created {
            root: created.root,
            project: true,
        });
    }
    if !valid_name(name) {
        return Err(SampleError::new(
            "sample_name_invalid",
            format!("`{name}` is not a folder name (one plain name, no separators)"),
        ));
    }
    let source = sample
        .source
        .as_deref()
        .filter(|dir| dir.is_dir())
        .ok_or_else(|| {
            SampleError::new(
                "sample_source_missing",
                format!("the folder of `{}` is gone", sample.id),
            )
        })?;
    let source = source
        .canonicalize()
        .map_err(|err| SampleError::new("sample_source_missing", format!("{}: {err}", source.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|err| SampleError::new("sample_copy_failed", format!("{}: {err}", parent.display())))?;
    let parent = parent
        .canonicalize()
        .map_err(|err| SampleError::new("sample_copy_failed", format!("{}: {err}", parent.display())))?;
    if parent.starts_with(&source) {
        return Err(SampleError::new(
            "sample_destination_inside_source",
            format!(
                "{} is inside the sample; choose another folder (a sample is never edited in place)",
                parent.display()
            ),
        ));
    }
    let root = parent.join(name);
    if !regia_templates::project_dir_is_free(&root) {
        return Err(SampleError::new(
            "sample_destination_exists",
            format!("{} already exists and is not empty", root.display()),
        ));
    }
    if let Err(message) = copy_tree(&source, &root, 0) {
        let _ = std::fs::remove_dir_all(&root);
        return Err(SampleError::new("sample_copy_failed", message));
    }
    tracing::info!(target: "regia_editor", "copied sample {} to {}", sample.id, root.display());
    let project = regia_core::brand::is_project_root(&root);
    Ok(Created { root, project })
}

/// Copies `id` (see [`create`]) and, when `open` is set and the copy is a project, opens it in the editor.
/// Returns the copy and the warning opening it gave, if any.
///
/// # Errors
///
/// As [`create`], plus `sample_open_failed` when the copy was made but did not open (the copy stays).
pub fn create_and_open(
    world: &mut World,
    roots: &Roots,
    id: &str,
    parent: &Path,
    name: Option<&str>,
    open: bool,
) -> Result<(Created, Option<String>), SampleError> {
    let sample = find(Some(world), roots, id)?;
    let created = create(&sample, parent, name)?;
    let mut warning = None;
    if open && created.project {
        warning = crate::project::open_project(world, &created.root)
            .map_err(|err| {
                SampleError::new(
                    "sample_open_failed",
                    format!("copied to {}, but {err}", created.root.display()),
                )
            })?
            .warning;
    }
    Ok((created, warning))
}

// ---------------------------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------------------------

/// The window's state.
#[derive(Resource, Debug, Clone)]
pub struct SamplesWindow {
    /// Whether the window is showing.
    pub open: bool,
    /// The selected entry's id.
    pub selected: Option<String>,
    /// The folder a copy goes into.
    pub folder: String,
    /// The name of the copy (empty: the sample's own name).
    pub name: String,
    /// The last outcome, shown under the buttons.
    pub status: String,
    /// The frame whose Open click was already applied (egui can run a panel twice a frame).
    handled_frame: u64,
}

impl Default for SamplesWindow {
    fn default() -> Self {
        Self {
            open: false,
            selected: None,
            folder: std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            name: String::new(),
            status: String::new(),
            handled_frame: 0,
        }
    }
}

/// Opens the window (Help ▸ Samples…), selecting `id` when given.
pub fn open_window(world: &mut World, id: Option<&str>) {
    let mut state = world.get_resource_or_insert_with(SamplesWindow::default);
    state.open = true;
    if let Some(id) = id {
        state.selected = Some(id.to_owned());
        state.name.clear();
    }
}

/// The picture of a sample, decoded once and kept in egui's memory; `None` when it did not decode.
#[derive(Clone)]
struct PreviewTexture(Option<egui::TextureHandle>);

const PREVIEW_POINTS: f32 = 64.0;

fn preview_texture(ctx: &egui::Context, source: &Path) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("sample-preview", source));
    if let Some(PreviewTexture(texture)) = ctx.data(|d| d.get_temp::<PreviewTexture>(id)) {
        return texture;
    }
    let texture = crate::thumb_cache::make_texture_thumb(source, 128)
        .ok()
        .map(|rgba| {
            ctx.load_texture(
                format!("sample-preview:{}", source.display()),
                egui::ColorImage::from_rgba_unmultiplied([128, 128], &rgba),
                egui::TextureOptions::LINEAR,
            )
        });
    ctx.data_mut(|d| d.insert_temp(id, PreviewTexture(texture.clone())));
    texture
}

/// Draws a sample's picture, or its kind letter when it has none.
fn draw_preview(ui: &mut egui::Ui, sample: &Sample) {
    match sample
        .preview
        .as_deref()
        .and_then(|src| preview_texture(ui.ctx(), src))
    {
        Some(texture) => {
            ui.add(
                egui::Image::new(&texture)
                    .fit_to_exact_size(egui::vec2(PREVIEW_POINTS, PREVIEW_POINTS))
                    .alt_text(format!("Preview of {}", sample.name)),
            );
        }
        None => {
            let letter = sample
                .name
                .chars()
                .next()
                .unwrap_or('?')
                .to_uppercase()
                .to_string();
            ui.add_sized(
                [PREVIEW_POINTS, PREVIEW_POINTS],
                egui::Label::new(egui::RichText::new(letter).heading().weak()),
            );
        }
    }
}

/// The Open button's outcome as a line for the status area.
pub(crate) fn outcome_line(result: &Result<(Created, Option<String>), SampleError>) -> String {
    match result {
        Ok((created, warning)) => {
            let mut line = format!("Copied to {}", created.root.display());
            if created.project {
                line.push_str(" and opened it");
            }
            if let Some(warning) = warning {
                line = format!("{line} ({warning})");
            }
            line
        }
        Err(err) => err.message.clone(),
    }
}

/// Draws the Samples window when it is open. The shell calls it every frame.
///
/// Keys: Tab reaches every control; Up and Down move the selection; Enter in the folder or name field
/// is Open; the window's close button (or Escape on it) closes it.
#[tracing::instrument(target = "regia_editor", skip_all, name = "samples_window")]
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(mut state) = world.get_resource::<SamplesWindow>().cloned().filter(|s| s.open) else {
        return;
    };
    let roots = Roots::detect();
    let mut entries = list(&roots);
    entries.extend(
        loaded_extension_samples(world)
            .into_iter()
            .map(|(_, sample)| sample),
    );
    if state.selected.is_none() {
        state.selected = entries.first().map(|sample| sample.id.clone());
    }
    let frame = ctx.cumulative_frame_nr();
    let mut open = true;
    let mut pressed_open = false;
    egui::Window::new("Samples")
        .open(&mut open)
        .default_width(560.0)
        .show(ctx, |ui| {
            ui.label(
                "Copies of templates, samples and extension examples. A sample is never edited in place.",
            );
            let steps = ui.input_mut(crate::level_templates::arrow_steps);
            let at = entries
                .iter()
                .position(|sample| state.selected.as_deref() == Some(sample.id.as_str()));
            if steps != 0 && !entries.is_empty() {
                let last = entries.len() as i64 - 1;
                let from = at.map_or(if steps > 0 { -1 } else { last + 1 }, |i| i as i64);
                let next = (from + i64::from(steps)).clamp(0, last) as usize;
                state.selected = Some(entries[next].id.clone());
                state.name.clear();
            }
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for sample in &entries {
                    let selected = state.selected.as_deref() == Some(sample.id.as_str());
                    let mut frame_style = egui::Frame::group(ui.style());
                    if selected {
                        frame_style = frame_style.stroke(ui.visuals().selection.stroke);
                    }
                    frame_style.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            draw_preview(ui, sample);
                            ui.vertical(|ui| {
                                let title =
                                    format!("{} ({})", sample.name, sample.kind.name().replace('_', " "));
                                let row = ui.selectable_label(selected, title);
                                if row.clicked() {
                                    state.selected = Some(sample.id.clone());
                                    state.name.clear();
                                }
                                ui.label(&sample.description);
                            });
                        });
                    });
                }
                if entries.is_empty() {
                    ui.weak("(nothing to show)");
                }
            });
            if roots.engine.is_none() {
                ui.weak("No engine checkout was found: only the project templates are listed.");
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Folder");
                let field = ui.text_edit_singleline(&mut state.folder);
                crate::a11y_audit::name(
                    field.clone(),
                    egui::WidgetType::TextEdit,
                    "Samples destination folder",
                );
                if field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    pressed_open = true;
                }
                if ui.button("Browse…").clicked()
                    && let Some(dir) = crate::file_dialog::FileDialog::new().pick_folder()
                {
                    state.folder = dir.display().to_string();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Name");
                let field =
                    ui.add(egui::TextEdit::singleline(&mut state.name).hint_text("the sample's own name"));
                crate::a11y_audit::name(field.clone(), egui::WidgetType::TextEdit, "Name of the copy");
                if field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    pressed_open = true;
                }
            });
            if crate::agent_parity::action_button(ui, &crate::agent_parity::SAMPLES_OPEN).clicked() {
                pressed_open = true;
            }
            if !state.status.is_empty() {
                ui.label(&state.status);
            }
        });
    if pressed_open && state.handled_frame != frame {
        state.handled_frame = frame;
        state.status = match state.selected.clone() {
            Some(id) => {
                let name = Some(state.name.trim())
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                let result = create_and_open(
                    world,
                    &roots,
                    &id,
                    Path::new(&state.folder),
                    name.as_deref(),
                    true,
                );
                outcome_line(&result)
            }
            None => "Select a sample first.".to_owned(),
        };
    }
    state.open = open;
    world.insert_resource(state);
}

/// What the Extensions panel draws under an extension: its bundled samples, each with an Open that
/// copies it into the Samples window's folder. Returns the id of the sample whose Open was pressed.
pub fn extension_samples_section(
    ui: &mut egui::Ui,
    folder: &mut String,
    samples: &[Sample],
) -> Option<String> {
    if samples.is_empty() {
        return None;
    }
    let mut pressed = None;
    ui.separator();
    ui.strong("Samples");
    ui.horizontal(|ui| {
        ui.label("Copy into");
        let field = ui.text_edit_singleline(folder);
        crate::a11y_audit::name(
            field,
            egui::WidgetType::TextEdit,
            "Extension samples destination folder",
        );
    });
    for sample in samples {
        ui.horizontal(|ui| {
            draw_preview(ui, sample);
            ui.vertical(|ui| {
                ui.strong(&sample.name);
                ui.label(&sample.description);
                if crate::agent_parity::action_button(ui, &crate::agent_parity::EXTENSIONS_SAMPLE_OPEN)
                    .clicked()
                {
                    pressed = Some(sample.id.clone());
                }
            });
        });
    }
    pressed
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn sample_json(sample: &Sample, owner: Option<&str>) -> Value {
    json!({
        "id": sample.id,
        "kind": sample.kind.name(),
        "name": sample.name,
        "description": sample.description,
        "project": sample.project,
        "preview": sample.preview.as_ref().map(|p| p.display().to_string()),
        "extension": owner,
    })
}

fn text_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

/// `editor.samples { action, id?, parent?, name?, open?, extension? }`.
fn editor_samples(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let roots = Roots::detect();
    match text_param(params, "action")? {
        Some("list") => {
            let only = text_param(params, "extension")?;
            let mut rows = Vec::new();
            if only.is_none() {
                rows.extend(list(&roots).iter().map(|sample| sample_json(sample, None)));
            }
            rows.extend(
                loaded_extension_samples(world)
                    .iter()
                    .filter(|(owner, _)| only.is_none_or(|id| id == owner))
                    .map(|(owner, sample)| sample_json(sample, Some(owner))),
            );
            Ok(json!({ "samples": rows, "engine_checkout": roots.engine.is_some() }))
        }
        Some("create") => {
            let id = text_param(params, "id")?
                .ok_or_else(|| RpcError::invalid_params("`id` (from action \"list\") is required"))?;
            let parent = text_param(params, "parent")?
                .filter(|parent| !parent.is_empty())
                .ok_or_else(|| {
                    RpcError::invalid_params("`parent` (the folder the copy goes into) is required")
                })?;
            let open = match params.get("open") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(flag)) => *flag,
                Some(_) => return Err(RpcError::invalid_params("`open` must be a boolean")),
            };
            let name = text_param(params, "name")?;
            let (created, warning) =
                create_and_open(world, &roots, id, Path::new(parent), name, open).map_err(|err| err.rpc())?;
            Ok(json!({
                "root": created.root.display().to_string(),
                "project": created.project,
                "opened": open && created.project,
                "warning": warning,
            }))
        }
        Some(other) => Err(RpcError::invalid_params(format!(
            "`action` is \"list\" or \"create\", not `{other}`"
        ))),
        None => Err(RpcError::invalid_params(
            "`action` (\"list\" or \"create\") is required",
        )),
    }
}

/// The samples agent method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.samples",
            tool: "samples",
            description: "List the templates, samples and extension examples (Help > Samples), or copy one into a new folder and optionally open the copy. A sample is never edited in place.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: true,
                    description: "`list` or `create`.",
                },
                ParamSchema {
                    name: "id",
                    kind: "string",
                    required: false,
                    description: "create: an id from list (`template/<id>`, `sample/<dir>`, `extension/<id>`, `extension_sample/<extension>/<dir>`).",
                },
                ParamSchema {
                    name: "parent",
                    kind: "string",
                    required: false,
                    description: "create: the folder the copy goes into (made when missing).",
                },
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "create: the new folder's name (the project name, for a template); default the sample's own name.",
                },
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "create: open the copy as the editor's project when it is one.",
                },
                ParamSchema {
                    name: "extension",
                    kind: "string",
                    required: false,
                    description: "list: only the samples this loaded extension bundles.",
                },
            ],
            result: "{ samples: [{ id, kind, name, description, project, preview, extension }], engine_checkout } or { root, project, opened, warning }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_samples as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("write");
    }

    /// A tiny engine checkout: one project sample, one library sample, one example extension.
    fn fake_engine() -> (PathBuf, Roots) {
        let engine = temp("samples-engine");
        write(
            &engine.join("samples/demo/Cargo.toml"),
            "[package]\nname = \"demo\"\ndescription = \"A demo sample.\"\n",
        );
        write(
            &engine.join("samples/demo/Regia.toml"),
            "[project]\nname = \"Demo\"\nid = \"com.example.demo\"\n",
        );
        write(&engine.join("samples/demo/assets/level.ron"), "()\n");
        write(&engine.join("samples/demo/target/junk.bin"), "x");
        write(&engine.join("samples/demo/.git/config"), "x");
        write(
            &engine.join("samples/lib_only/Cargo.toml"),
            "[package]\nname = \"lib_only\"\n",
        );
        write(
            &engine.join("samples/lib_only/README.md"),
            "# Lib\n\nA library sample.\n",
        );
        write(
            &engine.join("extensions/hello/regia-plugin.toml"),
            "id = \"hello\"\nname = \"Hello example\"\ndescription = \"An example extension.\"\n",
        );
        let roots = Roots {
            engine: Some(engine.clone()),
        };
        (engine, roots)
    }

    fn tree(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(path.strip_prefix(base).expect("under base").display().to_string());
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn the_browser_lists_templates_samples_and_extension_examples() {
        let (_engine, roots) = fake_engine();
        let rows = list(&roots);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert!(
            ids.contains(&"template/blank") && ids.contains(&"template/net_game"),
            "{ids:?}"
        );
        assert!(
            ids.contains(&"sample/demo") && ids.contains(&"sample/lib_only"),
            "{ids:?}"
        );
        assert!(ids.contains(&"extension/hello"), "{ids:?}");
        let demo = rows.iter().find(|row| row.id == "sample/demo").expect("demo");
        assert_eq!(demo.description, "A demo sample.");
        assert!(demo.project);
        let lib = rows.iter().find(|row| row.id == "sample/lib_only").expect("lib");
        assert_eq!(lib.description, "A library sample.", "the README is the fallback");
        assert!(!lib.project);
        // Without a checkout the templates still list (offline, installed editor).
        let templates_only = list(&Roots::default());
        assert!(templates_only.iter().all(|row| row.kind == SampleKind::Template));
        assert!(templates_only.len() >= 4);
        assert!(templates_only.iter().all(|row| !row.description.is_empty()));
    }

    #[test]
    fn the_real_checkout_lists_its_samples() {
        let roots = Roots::detect();
        let rows = list(&roots);
        assert!(
            rows.iter().any(|row| row.id == "sample/arena"),
            "samples/arena is listed"
        );
        assert!(
            rows.iter().any(|row| row.id == "extension/hello_panel"),
            "extensions/hello_panel is listed"
        );
        assert!(rows.iter().all(|row| !row.description.is_empty()));
    }

    #[test]
    fn the_samples_browser_creates_a_copy_that_opens() {
        let (engine, roots) = fake_engine();
        let dest = temp("samples-dest");
        let before = tree(&engine.join("samples/demo"));
        let mut world = World::new();
        let (created, _warning) =
            create_and_open(&mut world, &roots, "sample/demo", &dest, Some("my_demo"), true)
                .expect("the copy is made and opened");
        assert_eq!(created.root, dest.canonicalize().expect("dest").join("my_demo"));
        assert!(created.project);
        // The copy opened as the editor's project.
        let project = world
            .get_resource::<crate::agent_ext::EditorProject>()
            .expect("project");
        assert_eq!(
            project.root.as_deref(),
            created.root.canonicalize().ok().as_deref()
        );
        // Build output and version control stayed behind; the assets came along.
        let copied = tree(&created.root);
        assert!(copied.iter().any(|f| f.ends_with("level.ron")), "{copied:?}");
        assert!(
            copied
                .iter()
                .all(|f| !f.starts_with("target") && !f.starts_with(".git")),
            "{copied:?}"
        );
        // The sample was not touched.
        assert_eq!(tree(&engine.join("samples/demo")), before);
    }

    #[test]
    fn a_copy_never_lands_in_the_sample_or_over_a_folder() {
        let (engine, roots) = fake_engine();
        let dest = temp("samples-refuse");
        let mut world = World::new();
        let inside = create_and_open(
            &mut world,
            &roots,
            "sample/demo",
            &engine.join("samples/demo/assets"),
            Some("copy"),
            false,
        )
        .expect_err("inside the sample");
        assert_eq!(inside.code, "sample_destination_inside_source");
        assert!(!engine.join("samples/demo/assets/copy").exists());
        create_and_open(&mut world, &roots, "sample/demo", &dest, Some("a"), false).expect("first copy");
        let again = create_and_open(&mut world, &roots, "sample/demo", &dest, Some("a"), false)
            .expect_err("the folder exists");
        assert_eq!(again.code, "sample_destination_exists");
        for bad in ["", "..", "a/b", ".hidden", " pad"] {
            let err = create_and_open(&mut world, &roots, "sample/demo", &dest, Some(bad), false)
                .expect_err("bad name");
            assert_eq!(err.code, "sample_name_invalid", "{bad:?}");
        }
        let missing =
            create_and_open(&mut world, &roots, "sample/nope", &dest, None, false).expect_err("unknown");
        assert_eq!(missing.code, "unknown_sample");
    }

    #[test]
    fn a_template_is_scaffolded_and_opens() {
        let (_engine, roots) = fake_engine();
        let dest = temp("samples-template");
        let mut world = World::new();
        let (created, _) =
            create_and_open(&mut world, &roots, "template/blank", &dest, Some("My Game"), true)
                .expect("scaffolded");
        assert!(regia_core::brand::is_project_root(&created.root));
        assert!(world.get_resource::<crate::agent_ext::EditorProject>().is_some());
    }

    #[test]
    fn an_extension_sample_is_listed() {
        let ext = temp("samples-extension");
        write(&ext.join("regia-plugin.toml"), "id = \"fancy\"\n");
        write(
            &ext.join("samples/starter/README.md"),
            "# Starter\n\nA starter scene for fancy.\n",
        );
        write(&ext.join("samples/starter/scene.ron"), "()\n");
        write(&ext.join("samples/starter/preview.png"), "not a real png");
        write(&ext.join("samples/second/data.ron"), "()\n");
        let rows = extension_samples("fancy", &ext);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            ["extension_sample/fancy/second", "extension_sample/fancy/starter"]
        );
        assert_eq!(rows[1].description, "A starter scene for fancy.");
        assert!(rows[1].preview.is_some() && rows[0].preview.is_none());
        assert_eq!(rows[0].description, "A sample bundled with fancy.");
        // The same Open copies it.
        let dest = temp("samples-extension-dest");
        let created = create(&rows[1], &dest, None).expect("copy");
        assert!(created.root.join("scene.ron").is_file());
        assert!(!created.project);
        // No samples folder, nothing listed.
        assert!(extension_samples("fancy", &temp("samples-extension-none")).is_empty());
    }

    #[test]
    fn the_agent_method_lists_and_creates() {
        let mut world = World::new();
        let listed = editor_samples(&mut world, &json!({ "action": "list" })).expect("list");
        let rows = listed["samples"].as_array().expect("rows");
        assert!(rows.iter().any(|row| row["id"] == "template/blank"), "{listed}");
        let dest = temp("samples-agent");
        let made = editor_samples(
            &mut world,
            &json!({ "action": "create", "id": "template/blank", "parent": dest.display().to_string(), "name": "agent_game" }),
        )
        .expect("create");
        assert_eq!(made["project"], true);
        assert_eq!(made["opened"], false, "open was not asked for");
        assert!(
            Path::new(made["root"].as_str().expect("root"))
                .join("Regia.toml")
                .is_file()
        );
        let err = editor_samples(
            &mut world,
            &json!({ "action": "create", "id": "x/y", "parent": "/tmp" }),
        )
        .expect_err("unknown");
        let reason = err
            .data
            .as_ref()
            .and_then(|d| d.get("reason").or_else(|| d.get("code")))
            .and_then(Value::as_str);
        assert_eq!(reason, Some("unknown_sample"), "{err:?}");
        assert!(editor_samples(&mut world, &json!({})).is_err());
        assert!(editor_samples(&mut world, &json!({ "action": "nope" })).is_err());
    }

    /// Runs the window for a few frames with raw input and returns the output shapes' text.
    fn run_frames(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| window(ui.ctx(), world));
        output.textures_delta.clear();
    }

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn the_samples_window_is_operable_from_the_keyboard() {
        // Down moves the selection one row.
        let ctx = egui::Context::default();
        let mut world = World::new();
        open_window(&mut world, None);
        run_frames(&ctx, &mut world, vec![]);
        let first = world.resource::<SamplesWindow>().selected.clone();
        assert_eq!(first.as_deref(), Some("template/blank"));
        run_frames(&ctx, &mut world, vec![key_event(egui::Key::ArrowDown)]);
        assert_ne!(
            world.resource::<SamplesWindow>().selected,
            first,
            "Down selects the next row"
        );

        // Tab then Enter alone reach and press Open (or Enter in a field): walk the Tab count until the
        // copy exists. Every control is a focusable widget, so some count lands on it.
        let dest = temp("samples-keyboard");
        let mut made = false;
        for tabs in 1..=120 {
            let ctx = egui::Context::default();
            let mut world = World::new();
            open_window(&mut world, Some("template/blank"));
            {
                let mut state = world.resource_mut::<SamplesWindow>();
                state.folder = dest.display().to_string();
                state.name = "kbd_game".into();
            }
            run_frames(&ctx, &mut world, vec![]);
            run_frames(&ctx, &mut world, vec![]);
            for _ in 0..tabs {
                run_frames(&ctx, &mut world, vec![key_event(egui::Key::Tab)]);
            }
            run_frames(&ctx, &mut world, vec![key_event(egui::Key::Enter)]);
            if dest.join("kbd_game/Regia.toml").is_file() {
                made = true;
                break;
            }
        }
        assert!(made, "Tab and Enter alone created the copy");
    }

    #[test]
    fn an_extension_samples_open_is_reachable_from_the_keyboard() {
        let ext = temp("samples-extension-keys");
        write(
            &ext.join("samples/starter/README.md"),
            "# Starter\n\nA starter.\n",
        );
        let samples = extension_samples("fancy", &ext);
        let mut pressed = None;
        for tabs in 1..=10 {
            let ctx = egui::Context::default();
            let mut folder = String::from("somewhere");
            let mut frame = |events: Vec<egui::Event>, pressed: &mut Option<String>| {
                let input = egui::RawInput {
                    events,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    if let Some(id) = extension_samples_section(ui, &mut folder, &samples) {
                        *pressed = Some(id);
                    }
                });
                output.textures_delta.clear();
            };
            frame(vec![], &mut pressed);
            for _ in 0..tabs {
                frame(vec![key_event(egui::Key::Tab)], &mut pressed);
            }
            frame(vec![key_event(egui::Key::Enter)], &mut pressed);
            if pressed.is_some() {
                break;
            }
        }
        assert_eq!(pressed.as_deref(), Some("extension_sample/fancy/starter"));
    }
}
